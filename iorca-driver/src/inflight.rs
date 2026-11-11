//! SQE construction ([`prepare`]) and the resources each in-flight operation
//! keeps alive ([`InFlight`]).
//!
//! Invariant: every pointer placed in an SQE targets heap memory (`Vec`
//! contents, a `CString`, or a `Box` allocated *before* the pointer is taken)
//! owned by the `InFlight` returned alongside it. Moving the `InFlight` (into
//! the ring's map) does not move that memory.
//!
//! C structs passed to the kernel come from [`crate::abi`], so nothing here
//! needs `unsafe` except [`adopt_fd`].

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::time::Duration;

use io_uring::{opcode, squeue, types};
use socket2::SockAddr;

use crate::abi::{AddrBuf, MsgHdr, StatxBuf};
use crate::fs::Metadata;
use crate::op::{Op, Output};

/// Resources kept alive while the kernel may use them.
pub(crate) enum InFlight {
    Unit,
    Timeout(Box<types::Timespec>),
    /// Kernel fills `buf` (read, recv).
    Fill {
        buf: Vec<u8>,
        orig_len: usize,
    },
    /// Kernel reads `buf` (write, send).
    Drain {
        buf: Vec<u8>,
    },
    /// Produces a new fd (socket, accept, open). `path` is kept for `open`.
    NewFd {
        path: Option<CString>,
    },
    Connect(Box<SockAddr>),
    SendTo {
        buf: Vec<u8>,
        msg: Box<MsgState<SockAddr>>,
    },
    RecvFrom {
        buf: Vec<u8>,
        orig_len: usize,
        msg: Box<MsgState<AddrBuf>>,
    },
    Statx {
        statx: Box<StatxBuf>,
        path: Option<CString>,
    },
    /// Failed before submission (sent as a `Nop`); this is the final output.
    Ready(Output),
}

/// A `msghdr` plus the iovec and address (`A`) it points to, boxed together
/// so the internal pointers stay valid.
pub(crate) struct MsgState<A> {
    hdr: MsgHdr,
    iov: libc::iovec,
    addr: A,
}

impl InFlight {
    /// Converts a CQE flagged `IORING_CQE_F_MORE`. The op stays in flight and
    /// keeps its resources, which the kernel is still using. Only multishot
    /// timeouts post such CQEs.
    pub(crate) fn more(&self, res: i32) -> Output {
        match self {
            InFlight::Timeout(_) => Output::Unit(timeout_result(res)),
            _ => Output::Unit(cvt(res).map(drop)),
        }
    }

    /// Converts the CQE result into the typed output, handing resources back.
    pub(crate) fn finish(self, res: i32) -> Output {
        match self {
            InFlight::Unit => Output::Unit(cvt(res).map(drop)),
            InFlight::Timeout(ts) => {
                // The kernel is done with the timespec now.
                drop(ts);
                Output::Unit(timeout_result(res))
            }
            InFlight::Fill { mut buf, orig_len } => {
                let res: io::Result<usize> = cvt(res).map(|n: u32| -> usize { n as usize });
                // On error the kernel wrote nothing; restore the original length.
                buf.truncate(*res.as_ref().unwrap_or(&orig_len));
                Output::Bytes(res, buf)
            }
            InFlight::Drain { buf } => {
                Output::Bytes(cvt(res).map(|n: u32| -> usize { n as usize }), buf)
            }
            InFlight::NewFd { path } => {
                drop(path);
                Output::Fd(adopt_fd(res))
            }
            InFlight::Connect(addr) => {
                drop(addr);
                Output::Unit(cvt(res).map(drop))
            }
            InFlight::SendTo { buf, msg } => {
                drop(msg);
                Output::Bytes(cvt(res).map(|n: u32| -> usize { n as usize }), buf)
            }
            InFlight::RecvFrom {
                mut buf,
                orig_len,
                msg,
            } => {
                // The kernel updated `namelen` to the sender address's length.
                let res: io::Result<(usize, SockAddr)> =
                    cvt(res).and_then(|n: u32| -> io::Result<(usize, SockAddr)> {
                        Ok((n as usize, msg.addr.decode(msg.hdr.namelen)?))
                    });
                let n: usize = match &res {
                    Ok((n, _)) => *n,
                    Err(_) => orig_len,
                };
                buf.truncate(n);
                Output::Datagram(res, buf)
            }
            InFlight::Statx { statx, path } => {
                drop(path);
                Output::Metadata(
                    cvt(res).map(|_: u32| -> Metadata { Metadata::from_statx(&statx) }),
                )
            }
            InFlight::Ready(output) => output,
        }
    }
}

/// Builds the SQE for `op` plus the resources it points into.
pub(crate) fn prepare(op: Op) -> (squeue::Entry, InFlight) {
    match op {
        Op::Nop => (opcode::Nop::new().build(), InFlight::Unit),
        Op::Timeout(dur) => {
            let ts: Box<types::Timespec> = timespec(dur);
            (opcode::Timeout::new(&*ts).build(), InFlight::Timeout(ts))
        }
        Op::TimeoutMultishot { period, count } => {
            // A zero period would make the kernel fire continuously.
            if period.is_zero() {
                return ready(Output::Unit(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "multishot timeout period must be non-zero",
                ))));
            }
            let ts: Box<types::Timespec> = timespec(period);
            let entry: squeue::Entry = opcode::Timeout::new(&*ts)
                .count(count)
                .flags(types::TimeoutFlags::MULTISHOT)
                .build();
            (entry, InFlight::Timeout(ts))
        }
        Op::Cancel { target } => (opcode::AsyncCancel::new(target).build(), InFlight::Unit),
        Op::Read { fd, buf, offset } => {
            let (ptr, len, buf, orig_len): (*mut u8, u32, Vec<u8>, usize) = fill(buf);
            let entry: squeue::Entry = opcode::Read::new(types::Fd(fd), ptr, len)
                .offset(offset)
                .build();
            (entry, InFlight::Fill { buf, orig_len })
        }
        Op::Write { fd, buf, offset } => {
            let entry: squeue::Entry =
                opcode::Write::new(types::Fd(fd), buf.as_ptr(), io_len(buf.len()))
                    .offset(offset)
                    .build();
            (entry, InFlight::Drain { buf })
        }
        Op::Close { fd } => (
            opcode::Close::new(types::Fd(fd.into_raw_fd())).build(),
            InFlight::Unit,
        ),
        Op::Socket {
            domain,
            ty,
            protocol,
        } => {
            let entry: squeue::Entry = opcode::Socket::new(
                libc::c_int::from(domain),
                libc::c_int::from(ty) | libc::SOCK_CLOEXEC,
                protocol.map_or(0, libc::c_int::from),
            )
            .build();
            (entry, InFlight::NewFd { path: None })
        }
        Op::Connect { fd, addr } => {
            let addr: Box<SockAddr> = Box::new(addr);
            let entry: squeue::Entry =
                opcode::Connect::new(types::Fd(fd), addr.as_ptr().cast(), addr.len()).build();
            (entry, InFlight::Connect(addr))
        }
        Op::Accept { listener } => {
            let entry: squeue::Entry = opcode::Accept::new(
                types::Fd(listener),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
            .flags(libc::SOCK_CLOEXEC)
            .build();
            (entry, InFlight::NewFd { path: None })
        }
        Op::Recv { fd, buf } => {
            let (ptr, len, buf, orig_len): (*mut u8, u32, Vec<u8>, usize) = fill(buf);
            let entry: squeue::Entry = opcode::Recv::new(types::Fd(fd), ptr, len).build();
            (entry, InFlight::Fill { buf, orig_len })
        }
        Op::Send { fd, buf } => {
            let entry: squeue::Entry =
                opcode::Send::new(types::Fd(fd), buf.as_ptr(), io_len(buf.len()))
                    .flags(libc::MSG_NOSIGNAL)
                    .build();
            (entry, InFlight::Drain { buf })
        }
        Op::SendTo { fd, mut buf, addr } => {
            // The kernel only reads the address for sendmsg.
            let msg: Box<MsgState<SockAddr>> = msg_state(
                buf.as_mut_ptr(),
                buf.len(),
                addr,
                |a: &mut SockAddr| -> (*mut libc::c_void, libc::socklen_t) {
                    (a.as_ptr().cast_mut().cast(), a.len())
                },
            );
            let entry: squeue::Entry = opcode::SendMsg::new(types::Fd(fd), msg.hdr.as_ptr())
                .flags(libc::MSG_NOSIGNAL as u32)
                .build();
            (entry, InFlight::SendTo { buf, msg })
        }
        Op::RecvFrom { fd, buf } => {
            let (ptr, len, buf, orig_len): (*mut u8, u32, Vec<u8>, usize) = fill(buf);
            let mut msg: Box<MsgState<AddrBuf>> = msg_state(
                ptr,
                len as usize,
                AddrBuf::zeroed(),
                |a: &mut AddrBuf| -> (*mut libc::c_void, libc::socklen_t) {
                    (a.as_mut_ptr(), a.capacity())
                },
            );
            let entry: squeue::Entry =
                opcode::RecvMsg::new(types::Fd(fd), msg.hdr.as_mut_ptr()).build();
            (entry, InFlight::RecvFrom { buf, orig_len, msg })
        }
        Op::Open { path, options } => {
            let opened: io::Result<(i32, CString)> = options
                .flags()
                .and_then(|f: i32| -> io::Result<(i32, CString)> { Ok((f, c_path(path)?)) });
            let (flags, path): (i32, CString) = match opened {
                Ok(v) => v,
                Err(e) => return ready(Output::Fd(Err(e))),
            };
            let entry: squeue::Entry =
                opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path.as_ptr())
                    .flags(flags)
                    .mode(options.creation_mode())
                    .build();
            (entry, InFlight::NewFd { path: Some(path) })
        }
        Op::Fsync { fd, data_only } => {
            let flags: types::FsyncFlags = if data_only {
                types::FsyncFlags::DATASYNC
            } else {
                types::FsyncFlags::empty()
            };
            (
                opcode::Fsync::new(types::Fd(fd)).flags(flags).build(),
                InFlight::Unit,
            )
        }
        Op::Statx { fd } => statx(fd, None, libc::AT_EMPTY_PATH),
        Op::StatxPath { path } => match c_path(path) {
            Ok(path) => statx(libc::AT_FDCWD, Some(path), 0),
            Err(e) => ready(Output::Metadata(Err(e))),
        },
    }
}

/// Builds a statx SQE. With `path == None` it stats `dirfd` itself
/// (`AT_EMPTY_PATH` with an empty path).
fn statx(dirfd: RawFd, path: Option<CString>, flags: i32) -> (squeue::Entry, InFlight) {
    let mut statx: Box<StatxBuf> = StatxBuf::zeroed();
    let path_ptr: *const libc::c_char = path
        .as_ref()
        .map_or(c"".as_ptr(), |p: &CString| -> *const libc::c_char {
            p.as_ptr()
        });
    let entry: squeue::Entry =
        opcode::Statx::new(types::Fd(dirfd), path_ptr, statx.as_mut_ptr().cast())
            .flags(flags)
            .mask(libc::STATX_BASIC_STATS)
            .build();
    (entry, InFlight::Statx { statx, path })
}

/// Builds a boxed `msghdr` with one iovec over `buf_ptr[..buf_len]`. `name`
/// returns the pointer and length of the address inside the box.
fn msg_state<A>(
    buf_ptr: *mut u8,
    buf_len: usize,
    addr: A,
    name: impl FnOnce(&mut A) -> (*mut libc::c_void, libc::socklen_t),
) -> Box<MsgState<A>> {
    let mut msg: Box<MsgState<A>> = Box::new(MsgState {
        hdr: MsgHdr::empty(),
        iov: libc::iovec {
            iov_base: buf_ptr.cast(),
            iov_len: buf_len,
        },
        addr,
    });
    let (name_ptr, name_len): (*mut libc::c_void, libc::socklen_t) = name(&mut msg.addr);
    let iov_ptr: *mut libc::iovec = &mut msg.iov;
    msg.hdr.name = name_ptr;
    msg.hdr.namelen = name_len;
    msg.hdr.iov = iov_ptr;
    msg.hdr.iovlen = 1;
    msg
}

/// Zero-fills `buf` to its capacity so the kernel writes into initialised
/// memory (no `set_len` needed). Returns the SQE pointer/length, the buffer
/// and its original length (restored if the operation fails).
fn fill(mut buf: Vec<u8>) -> (*mut u8, u32, Vec<u8>, usize) {
    let orig_len: usize = buf.len();
    buf.resize(buf.capacity(), 0);
    (buf.as_mut_ptr(), io_len(buf.len()), buf, orig_len)
}

/// An operation that failed before submission: send a `Nop` and deliver
/// `output` when it completes, so errors are never reported synchronously.
/// Boxed so the pointer handed to the kernel stays valid when the `InFlight`
/// moves.
fn timespec(dur: Duration) -> Box<types::Timespec> {
    Box::new(
        types::Timespec::new()
            .sec(dur.as_secs())
            .nsec(dur.subsec_nanos()),
    )
}

/// A pure timer completes with `-ETIME` when it expires.
fn timeout_result(res: i32) -> io::Result<()> {
    if res == -libc::ETIME {
        Ok(())
    } else {
        cvt(res).map(drop)
    }
}

fn ready(output: Output) -> (squeue::Entry, InFlight) {
    (opcode::Nop::new().build(), InFlight::Ready(output))
}

fn c_path(path: PathBuf) -> io::Result<CString> {
    CString::new(path.into_os_string().into_vec()).map_err(|_: std::ffi::NulError| -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        )
    })
}

/// Takes ownership of a new fd returned by the kernel. One of the driver's
/// two `unsafe` sites: no safe API can turn a raw fd into an `OwnedFd`.
#[allow(unsafe_code)]
fn adopt_fd(res: i32) -> io::Result<OwnedFd> {
    cvt(res).map(|fd: u32| -> OwnedFd {
        // SAFETY: a successful socket/accept/openat result is a new fd that
        // nothing else owns, so taking ownership of it is sound.
        unsafe { OwnedFd::from_raw_fd(fd as RawFd) }
    })
}

/// Converts a CQE result (`-errno` on failure) into an `io::Result`.
fn cvt(res: i32) -> io::Result<u32> {
    if res < 0 {
        Err(io::Error::from_raw_os_error(-res))
    } else {
        Ok(res as u32)
    }
}

/// Clamps a buffer length to the `u32` that SQEs carry.
fn io_len(len: usize) -> u32 {
    len.min(u32::MAX as usize) as u32
}
