//! The operations you can submit to an [`EventLoop`].
//!
//! Each method builds an [`Op`] for the driver and wraps the user callback
//! so it receives the matching typed result.

use std::io;
use std::net::SocketAddr;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use iorca_driver::{Domain, Metadata, Op, OpenOptions, Output, Protocol, SockAddr, Type};

use crate::event_loop::{Callback, MultiCallback};
use crate::{EventLoop, OpId};

impl EventLoop {
    /// Submits a no-op. Its callback runs on the next loop turn, which makes
    /// it handy for deferring work.
    pub fn nop<F>(&mut self, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(Op::Nop, unit_cb(cb))
    }

    /// Fires `cb` with `Ok(())` once `dur` has elapsed.
    ///
    /// If the timer is cancelled, `cb` gets an `ECANCELED` error.
    pub fn timeout<F>(&mut self, dur: Duration, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(Op::Timeout(dur), unit_cb(cb))
    }

    /// Kernel-side periodic timer (`IORING_TIMEOUT_MULTISHOT`): one
    /// submission that fires `cb` every `period`. Opt-in; needs Linux ≥ 6.4.
    ///
    /// `cb` gets `Ok(more)` on each expiry, where `more` is false on the last
    /// call. `count` is the number of expiries, or 0 to repeat until the op
    /// is [`cancel`](Self::cancel)led, which ends it with a final
    /// `Err(ECANCELED)`. `cb` is never called again after `Ok(false)` or an
    /// error. Errors: `InvalidInput` for a zero `period`; `EINVAL` from
    /// kernels without multishot timeouts.
    ///
    /// Compared with [`Timer::interval`](crate::Timer::interval), this saves a
    /// submission per tick, but the kernel re-arms the timer relative to each
    /// expiry, so ticks are not anchored to the start time and can drift.
    /// [`deadline`](Self::deadline) does not apply to it.
    pub fn timeout_multishot<F>(&mut self, period: Duration, count: u32, mut cb: F) -> OpId
    where
        F: FnMut(&mut EventLoop, io::Result<bool>) + 'static,
    {
        let handler: MultiCallback = Box::new(
            move |el: &mut EventLoop, out: Output, more: bool| match out {
                Output::Unit(res) => cb(el, res.map(|(): ()| -> bool { more })),
                other => mismatch(other),
            },
        );
        self.push_multi(Op::TimeoutMultishot { period, count }, handler)
    }

    /// Reads from `fd` at `offset` into `buf`.
    ///
    /// Reads up to `buf.capacity()` bytes into the start of `buf`. On success
    /// the callback gets the byte count and `buf` with `len()` set to that
    /// count. `Ok(0)` means end of file.
    ///
    /// Use `offset = u64::MAX` to read from the file's current position (needed
    /// for non-seekable files such as pipes).
    pub fn read<F>(&mut self, fd: RawFd, buf: Vec<u8>, offset: u64, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        self.push(Op::Read { fd, buf, offset }, bytes_cb(cb))
    }

    /// Writes `buf[..buf.len()]` to `fd` at `offset`.
    ///
    /// The callback gets the number of bytes written (which may be less than
    /// `buf.len()`) and the buffer back. Use `offset = u64::MAX` to write at the
    /// file's current position.
    pub fn write<F>(&mut self, fd: RawFd, buf: Vec<u8>, offset: u64, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        self.push(Op::Write { fd, buf, offset }, bytes_cb(cb))
    }

    /// Accepts a connection on the listening socket `listener`.
    ///
    /// The accepted socket is close-on-exec.
    pub fn accept<F>(&mut self, listener: RawFd, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<OwnedFd>) + 'static,
    {
        self.push(Op::Accept { listener }, fd_cb(cb))
    }

    /// Creates a close-on-exec socket, e.g. `(Domain::IPV4, Type::DGRAM, None)`
    /// for UDP or `(Domain::UNIX, Type::DGRAM, None)` for a Unix datagram
    /// socket.
    pub fn socket<F>(&mut self, domain: Domain, ty: Type, protocol: Option<Protocol>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<OwnedFd>) + 'static,
    {
        self.push(
            Op::Socket {
                domain,
                ty,
                protocol,
            },
            fd_cb(cb),
        )
    }

    /// Connects socket `fd` to `addr`: a [`SocketAddr`] or a [`SockAddr`]
    /// (e.g. `SockAddr::unix(path)?` for a Unix socket).
    pub fn connect<F>(&mut self, fd: RawFd, addr: impl Into<SockAddr>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(
            Op::Connect {
                fd,
                addr: Into::<SockAddr>::into(addr),
            },
            unit_cb(cb),
        )
    }

    /// Opens a TCP connection to `addr`: [`socket`](Self::socket) followed by
    /// [`connect`](Self::connect). The callback gets the connected socket.
    ///
    /// The returned [`OpId`] is that of the `socket` step only.
    pub fn tcp_connect<F>(&mut self, addr: SocketAddr, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<OwnedFd>) + 'static,
    {
        self.socket(
            Domain::for_address(addr),
            Type::STREAM,
            None,
            move |el: &mut EventLoop, res: io::Result<OwnedFd>| match res {
                Ok(socket) => {
                    el.connect(
                        socket.as_raw_fd(),
                        addr,
                        move |el: &mut EventLoop, res: io::Result<()>| {
                            cb(el, res.map(|(): ()| -> OwnedFd { socket }))
                        },
                    );
                }
                Err(e) => cb(el, Err(e)),
            },
        )
    }

    /// Receives from socket `fd` into `buf`. The buffer works as in
    /// [`read`](Self::read). `Ok(0)` means the peer closed the connection.
    pub fn recv<F>(&mut self, fd: RawFd, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        self.push(Op::Recv { fd, buf }, bytes_cb(cb))
    }

    /// Sends `buf[..buf.len()]` on socket `fd`. The callback gets the number of
    /// bytes sent (which may be short) and the buffer back.
    ///
    /// Uses `MSG_NOSIGNAL`, so a closed peer gives `EPIPE` rather than `SIGPIPE`.
    pub fn send<F>(&mut self, fd: RawFd, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        self.push(Op::Send { fd, buf }, bytes_cb(cb))
    }

    /// Sends one datagram, `buf[..buf.len()]`, from socket `fd` to `addr` (a
    /// [`SocketAddr`] or a [`SockAddr`]). The callback gets the number of bytes
    /// sent and the buffer back.
    pub fn send_to<F>(&mut self, fd: RawFd, buf: Vec<u8>, addr: impl Into<SockAddr>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        self.push(
            Op::SendTo {
                fd,
                buf,
                addr: Into::<SockAddr>::into(addr),
            },
            bytes_cb(cb),
        )
    }

    /// Receives one datagram on socket `fd` into `buf` (filled as in
    /// [`read`](Self::read)). The callback gets the byte count, the sender's
    /// address and the buffer. Use [`SockAddr::as_socket`] for IP senders, or
    /// [`SockAddr::as_pathname`] / [`SockAddr::is_unnamed`] for Unix ones.
    pub fn recv_from<F>(&mut self, fd: RawFd, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<(usize, SockAddr)>, Vec<u8>) + 'static,
    {
        self.push(
            Op::RecvFrom { fd, buf },
            Box::new(move |el: &mut EventLoop, out: Output| match out {
                Output::Datagram(res, buf) => cb(el, res, buf),
                other => mismatch(other),
            }),
        )
    }

    /// Opens `path` with `options` (close-on-exec). The callback gets the fd.
    pub fn open<F>(&mut self, path: impl Into<PathBuf>, options: &OpenOptions, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<OwnedFd>) + 'static,
    {
        self.push(
            Op::Open {
                path: Into::<PathBuf>::into(path),
                options: options.clone(),
            },
            fd_cb(cb),
        )
    }

    /// Flushes file data and metadata to disk (`fsync`).
    pub fn fsync<F>(&mut self, fd: RawFd, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(
            Op::Fsync {
                fd,
                data_only: false,
            },
            unit_cb(cb),
        )
    }

    /// Flushes file data to disk, skipping metadata that isn't needed to read
    /// it back (`fdatasync`).
    pub fn fdatasync<F>(&mut self, fd: RawFd, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(
            Op::Fsync {
                fd,
                data_only: true,
            },
            unit_cb(cb),
        )
    }

    /// Gets metadata for the open file `fd`.
    pub fn statx<F>(&mut self, fd: RawFd, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<Metadata>) + 'static,
    {
        self.push(Op::Statx { fd }, metadata_cb(cb))
    }

    /// Gets metadata for `path`, following symlinks.
    pub fn statx_path<F>(&mut self, path: impl Into<PathBuf>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<Metadata>) + 'static,
    {
        self.push(
            Op::StatxPath {
                path: Into::<PathBuf>::into(path),
            },
            metadata_cb(cb),
        )
    }

    /// Closes `fd` asynchronously.
    pub fn close<F>(&mut self, fd: OwnedFd, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(Op::Close { fd }, unit_cb(cb))
    }

    /// Asks the kernel to cancel `target`.
    ///
    /// If cancellation succeeds, `target`'s callback runs with `ECANCELED`, and
    /// this callback gets `Ok(())`. It gets `ENOENT` if `target` already
    /// completed and `EALREADY` if `target` was already running.
    pub fn cancel<F>(&mut self, target: OpId, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        self.push(Op::Cancel { target: target.0 }, unit_cb(cb))
    }
}

fn unit_cb<F>(cb: F) -> Callback
where
    F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
{
    Box::new(move |el: &mut EventLoop, out: Output| match out {
        Output::Unit(res) => cb(el, res),
        other => mismatch(other),
    })
}

fn bytes_cb<F>(cb: F) -> Callback
where
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    Box::new(move |el: &mut EventLoop, out: Output| match out {
        Output::Bytes(res, buf) => cb(el, res, buf),
        other => mismatch(other),
    })
}

fn fd_cb<F>(cb: F) -> Callback
where
    F: FnOnce(&mut EventLoop, io::Result<OwnedFd>) + 'static,
{
    Box::new(move |el: &mut EventLoop, out: Output| match out {
        Output::Fd(res) => cb(el, res),
        other => mismatch(other),
    })
}

fn metadata_cb<F>(cb: F) -> Callback
where
    F: FnOnce(&mut EventLoop, io::Result<Metadata>) + 'static,
{
    Box::new(move |el: &mut EventLoop, out: Output| match out {
        Output::Metadata(res) => cb(el, res),
        other => mismatch(other),
    })
}

/// The driver always returns the `Output` variant documented for each `Op`.
fn mismatch(out: Output) -> ! {
    unreachable!("iorca-driver returned an unexpected output kind: {out:?}")
}
