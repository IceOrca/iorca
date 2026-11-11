//! SQE construction ([`prepare`]) and the resources each in-flight operation
//! keeps alive ([`InFlight`]).
//!
//! Invariant: every pointer placed in an SQE targets heap memory (`Vec`
//! contents, a `CString`, or a `Box` allocated *before* the pointer is taken)
//! owned by the `InFlight` returned alongside it. Moving the `InFlight` (into
//! the ring's map) does not move that memory.

use std::ffi::CString;
use std::io;
use std::mem;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

use io_uring::{opcode, squeue, types};
use socket2::{SockAddr, SockAddrStorage};

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
        msg: Box<MsgState>,
    },
    RecvFrom {
        buf: Vec<u8>,
        orig_len: usize,
        msg: Box<MsgState>,
    },
    Statx {
        statx: Box<libc::statx>,
        path: Option<CString>,
    },
    /// Failed before submission (sent as a `Nop`); this is the final output.
    Ready(Output),
}

/// A `msghdr` plus the iovec and address it points to, boxed together so the
/// internal pointers stay valid.
pub(crate) struct MsgState {
    hdr: libc::msghdr,
    iov: libc::iovec,
    addr: SockAddrStorage,
}

impl InFlight {
    /// Converts the CQE result into the typed output, handing resources back.
    pub(crate) fn finish(self, res: i32) -> Output {
        match self {
            InFlight::Unit => Output::Unit(cvt(res).map(drop)),
            InFlight::Timeout(ts) => {
                // The kernel is done with the timespec now.
                drop(ts);
                // A pure timer completes with -ETIME when it expires.
                if res == -libc::ETIME {
                    Output::Unit(Ok(()))
                } else {
                    Output::Unit(cvt(res).map(drop))
                }
            }
            InFlight::Fill { mut buf, orig_len } => {
                let res = cvt(res).map(|n| n as usize);
                // On error the kernel wrote nothing; restore the original length.
                buf.truncate(*res.as_ref().unwrap_or(&orig_len));
                Output::Bytes(res, buf)
            }
            InFlight::Drain { buf } => Output::Bytes(cvt(res).map(|n| n as usize), buf),
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
                Output::Bytes(cvt(res).map(|n| n as usize), buf)
            }
            InFlight::RecvFrom {
                mut buf,
                orig_len,
                msg,
            } => {
                let MsgState { hdr, addr, .. } = *msg;
                let res = cvt(res).map(|n| (n as usize, sender_addr(addr, hdr.msg_namelen)));
                let n = match &res {
                    Ok((n, _)) => *n,
                    Err(_) => orig_len,
                };
                buf.truncate(n);
                Output::Datagram(res, buf)
            }
            InFlight::Statx { statx, path } => {
                drop(path);
                Output::Metadata(cvt(res).map(|_| Metadata::from_statx(&statx)))
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
            let ts = Box::new(
                types::Timespec::new()
                    .sec(dur.as_secs())
                    .nsec(dur.subsec_nanos()),
            );
            (opcode::Timeout::new(&*ts).build(), InFlight::Timeout(ts))
        }
        Op::Cancel { target } => (opcode::AsyncCancel::new(target).build(), InFlight::Unit),
        Op::Read { fd, buf, offset } => {
            let (ptr, len, buf, orig_len) = fill(buf);
            let entry = opcode::Read::new(types::Fd(fd), ptr, len)
                .offset(offset)
                .build();
            (entry, InFlight::Fill { buf, orig_len })
        }
        Op::Write { fd, buf, offset } => {
            let entry = opcode::Write::new(types::Fd(fd), buf.as_ptr(), io_len(buf.len()))
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
            let entry = opcode::Socket::new(
                domain.into(),
                libc::c_int::from(ty) | libc::SOCK_CLOEXEC,
                protocol.map_or(0, Into::into),
            )
            .build();
            (entry, InFlight::NewFd { path: None })
        }
        Op::Connect { fd, addr } => {
            let addr = Box::new(addr);
            let entry =
                opcode::Connect::new(types::Fd(fd), addr.as_ptr().cast(), addr.len()).build();
            (entry, InFlight::Connect(addr))
        }
        Op::Accept { listener } => {
            let entry = opcode::Accept::new(
                types::Fd(listener),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
            .flags(libc::SOCK_CLOEXEC)
            .build();
            (entry, InFlight::NewFd { path: None })
        }
        Op::Recv { fd, buf } => {
            let (ptr, len, buf, orig_len) = fill(buf);
            let entry = opcode::Recv::new(types::Fd(fd), ptr, len).build();
            (entry, InFlight::Fill { buf, orig_len })
        }
        Op::Send { fd, buf } => {
            let entry = opcode::Send::new(types::Fd(fd), buf.as_ptr(), io_len(buf.len()))
                .flags(libc::MSG_NOSIGNAL)
                .build();
            (entry, InFlight::Drain { buf })
        }
        Op::SendTo { fd, mut buf, addr } => {
            let len = addr.len();
            let msg = msg_state(buf.as_mut_ptr(), buf.len(), addr.as_storage(), len);
            let entry = opcode::SendMsg::new(types::Fd(fd), &msg.hdr)
                .flags(libc::MSG_NOSIGNAL as u32)
                .build();
            (entry, InFlight::SendTo { buf, msg })
        }
        Op::RecvFrom { fd, buf } => {
            let (ptr, len, buf, orig_len) = fill(buf);
            let storage = SockAddrStorage::zeroed();
            let storage_len = storage.size_of();
            let mut msg = msg_state(ptr, len as usize, storage, storage_len);
            let entry = opcode::RecvMsg::new(types::Fd(fd), &mut msg.hdr).build();
            (entry, InFlight::RecvFrom { buf, orig_len, msg })
        }
        Op::Open { path, options } => {
            let (flags, path) = match options.flags().and_then(|f| Ok((f, c_path(path)?))) {
                Ok(v) => v,
                Err(e) => return ready(Output::Fd(Err(e))),
            };
            let entry = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), path.as_ptr())
                .flags(flags)
                .mode(options.creation_mode())
                .build();
            (entry, InFlight::NewFd { path: Some(path) })
        }
        Op::Fsync { fd, data_only } => {
            let flags = if data_only {
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

/// Turns the address `recvmsg` wrote into a `SockAddr`.
fn sender_addr(addr: SockAddrStorage, len: libc::socklen_t) -> SockAddr {
    if len == 0 {
        // Linux reports a datagram from an unbound Unix socket with a
        // zero-length address. Return the canonical unnamed Unix address
        // instead, as `std::os::unix::net::UnixDatagram::recv_from` does.
        return SockAddr::unix("").expect("empty Unix path is always valid");
    }
    // SAFETY: on success the kernel wrote a valid socket address of the
    // socket's family into `addr` and set `len` to its length (≤ the storage
    // size).
    unsafe { SockAddr::new(addr, len) }
}

/// Builds a statx SQE. With `path == None` it stats `dirfd` itself
/// (`AT_EMPTY_PATH` with an empty path).
fn statx(dirfd: RawFd, path: Option<CString>, flags: i32) -> (squeue::Entry, InFlight) {
    // SAFETY: `statx` is a plain C struct of integers; all-zero is valid.
    let mut statx: Box<libc::statx> = Box::new(unsafe { mem::zeroed() });
    let path_ptr = path.as_ref().map_or(c"".as_ptr(), |p| p.as_ptr());
    let entry = opcode::Statx::new(
        types::Fd(dirfd),
        path_ptr,
        (&mut *statx as *mut libc::statx).cast(),
    )
    .flags(flags)
    .mask(libc::STATX_BASIC_STATS)
    .build();
    (entry, InFlight::Statx { statx, path })
}

/// Builds a boxed `msghdr` with one iovec over `buf_ptr[..buf_len]` and the
/// given address storage.
fn msg_state(
    buf_ptr: *mut u8,
    buf_len: usize,
    addr: SockAddrStorage,
    addr_len: libc::socklen_t,
) -> Box<MsgState> {
    let mut msg = Box::new(MsgState {
        // SAFETY: an all-zero `msghdr` is valid (null pointers, zero lengths).
        // It is zeroed rather than built literally because some libc targets
        // have private padding fields.
        hdr: unsafe { mem::zeroed() },
        iov: libc::iovec {
            iov_base: buf_ptr.cast(),
            iov_len: buf_len,
        },
        addr,
    });
    let addr_ptr: *mut SockAddrStorage = &mut msg.addr;
    let iov_ptr: *mut libc::iovec = &mut msg.iov;
    msg.hdr.msg_name = addr_ptr.cast();
    msg.hdr.msg_namelen = addr_len;
    msg.hdr.msg_iov = iov_ptr;
    msg.hdr.msg_iovlen = 1;
    msg
}

/// Zero-fills `buf` to its capacity so the kernel writes into initialised
/// memory (no `set_len` needed). Returns the SQE pointer/length, the buffer
/// and its original length (restored if the operation fails).
fn fill(mut buf: Vec<u8>) -> (*mut u8, u32, Vec<u8>, usize) {
    let orig_len = buf.len();
    buf.resize(buf.capacity(), 0);
    (buf.as_mut_ptr(), io_len(buf.len()), buf, orig_len)
}

/// An operation that failed before submission: send a `Nop` and deliver
/// `output` when it completes, so errors are never reported synchronously.
fn ready(output: Output) -> (squeue::Entry, InFlight) {
    (opcode::Nop::new().build(), InFlight::Ready(output))
}

fn c_path(path: PathBuf) -> io::Result<CString> {
    CString::new(path.into_os_string().into_vec()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "path contains an interior NUL byte",
        )
    })
}

/// Takes ownership of a new fd returned by the kernel.
fn adopt_fd(res: i32) -> io::Result<OwnedFd> {
    cvt(res).map(|fd| {
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
