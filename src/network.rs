//! Socket handles: TCP, UDP, and Unix stream/datagram sockets.
//!
//! Binding and listening are synchronous (via std): they don't block, and
//! io_uring only has bind/listen ops from Linux 6.11. Everything that waits
//! (connect, accept, send, recv) goes through the event loop.

use std::io;
use std::net::{self, SocketAddr, ToSocketAddrs};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net as unix;
use std::path::Path;
use std::rc::Rc;

use crate::io::{self as shared, WriteMode};
use crate::{Domain, EventLoop, OpId, SockAddr, Type};

/// Defines a socket handle type wrapping `Rc<$std>`, with `close`, `as_std`
/// and conversions.
macro_rules! socket_handle {
    ($(#[$doc:meta])* $name:ident, $std:ty) => {
        $(#[$doc])*
        ///
        /// Cloning is cheap and shares the same socket. The fd closes when the
        /// last clone and the last in-flight operation using it are gone, or
        /// earlier through `close`.
        #[derive(Clone, Debug)]
        pub struct $name {
            inner: Rc<$std>,
        }

        impl $name {
            /// Closes the socket asynchronously. If clones or in-flight
            /// operations still use it, the fd closes when the last of them is
            /// gone instead.
            pub fn close<F>(self, el: &mut EventLoop, cb: F) -> OpId
            where
                F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
            {
                shared::close(el, self.inner, cb)
            }

            /// The underlying std socket, for synchronous calls such as
            /// addresses and socket options. Its blocking I/O methods would
            /// stall the event loop.
            pub fn as_std(&self) -> &$std {
                &self.inner
            }
        }

        impl From<$std> for $name {
            fn from(socket: $std) -> Self {
                Self { inner: Rc::new(socket) }
            }
        }

        impl From<OwnedFd> for $name {
            fn from(fd: OwnedFd) -> Self {
                Self::from(<$std>::from(fd))
            }
        }

        impl AsRawFd for $name {
            fn as_raw_fd(&self) -> RawFd {
                self.inner.as_raw_fd()
            }
        }
    };
}

/// `recv` / `send` for connected sockets.
macro_rules! connected_io {
    ($name:ident) => {
        impl $name {
            /// Receives into `buf` (up to `buf.capacity()` bytes). For a stream,
            /// `Ok(0)` means the peer closed the connection.
            pub fn recv<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
            where
                F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
            {
                shared::recv(el, &self.inner, buf, cb)
            }

            /// Sends `buf[..buf.len()]` to the connected peer. On a stream it
            /// may be short; see `write_all`.
            pub fn send<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
            where
                F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
            {
                shared::send(el, &self.inner, buf, cb)
            }
        }
    };
}

/// `write_all` for stream sockets.
macro_rules! stream_io {
    ($name:ident) => {
        impl $name {
            /// Sends all of `buf`, resubmitting after short sends. On success
            /// the buffer comes back cleared.
            pub fn write_all<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F)
            where
                F: FnOnce(&mut EventLoop, io::Result<()>, Vec<u8>) + 'static,
            {
                shared::write_all(el, Rc::clone(&self.inner), WriteMode::Send, buf, cb)
            }
        }
    };
}

// ---------------------------------------------------------------- TCP

socket_handle!(
    /// A listening TCP socket.
    TcpListener,
    net::TcpListener
);

socket_handle!(
    /// A connected TCP stream.
    TcpStream,
    net::TcpStream
);
connected_io!(TcpStream);
stream_io!(TcpStream);

impl TcpListener {
    /// Binds and listens on `addr` (synchronously).
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        net::TcpListener::bind(addr).map(Self::from)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Accepts one connection. Call again (e.g. from the callback) to keep
    /// accepting. The peer address is available via `stream.as_std().peer_addr()`.
    pub fn accept<F>(&self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<TcpStream>) + 'static,
    {
        accept(el, &self.inner, cb)
    }
}

impl TcpStream {
    /// Connects to `addr`.
    pub fn connect<F>(el: &mut EventLoop, addr: SocketAddr, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<TcpStream>) + 'static,
    {
        el.tcp_connect(addr, move |el, res| cb(el, res.map(TcpStream::from)))
    }
}

// ---------------------------------------------------------------- UDP

socket_handle!(
    /// A UDP socket.
    UdpSocket,
    net::UdpSocket
);
connected_io!(UdpSocket);

impl UdpSocket {
    /// Binds to `addr` (synchronously). Use port 0 for any free port.
    pub fn bind(addr: impl ToSocketAddrs) -> io::Result<Self> {
        net::UdpSocket::bind(addr).map(Self::from)
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    /// Sets the default peer, so [`send`](Self::send) / [`recv`](Self::recv)
    /// can be used, and filters incoming datagrams to that peer.
    pub fn connect<F>(&self, el: &mut EventLoop, addr: SocketAddr, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        connect(el, &self.inner, addr.into(), cb)
    }

    /// Sends one datagram, `buf[..buf.len()]`, to `addr`.
    pub fn send_to<F>(&self, el: &mut EventLoop, buf: Vec<u8>, addr: SocketAddr, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        send_to(el, &self.inner, buf, addr.into(), cb)
    }

    /// Receives one datagram into `buf` (up to `buf.capacity()` bytes; the
    /// rest of a longer datagram is discarded). The callback gets the byte
    /// count and the sender's address.
    pub fn recv_from<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<(usize, SocketAddr)>, Vec<u8>) + 'static,
    {
        recv_from(el, &self.inner, buf, move |el, res, buf| {
            let res = res.and_then(|(n, from)| {
                from.as_socket().map(|addr| (n, addr)).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "sender is not an IP address")
                })
            });
            cb(el, res, buf)
        })
    }
}

// ---------------------------------------------------------------- Unix

socket_handle!(
    /// A listening Unix stream socket.
    UnixListener,
    unix::UnixListener
);

socket_handle!(
    /// A connected Unix stream socket.
    UnixStream,
    unix::UnixStream
);
connected_io!(UnixStream);
stream_io!(UnixStream);

socket_handle!(
    /// A Unix datagram socket.
    UnixDatagram,
    unix::UnixDatagram
);
connected_io!(UnixDatagram);

impl UnixListener {
    /// Binds and listens on the socket file `path` (synchronously).
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        unix::UnixListener::bind(path).map(Self::from)
    }

    /// Accepts one connection. Call again (e.g. from the callback) to keep
    /// accepting.
    pub fn accept<F>(&self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<UnixStream>) + 'static,
    {
        accept(el, &self.inner, cb)
    }
}

impl UnixStream {
    /// Connects to the socket file `path`.
    pub fn connect<F>(el: &mut EventLoop, path: impl AsRef<Path>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<UnixStream>) + 'static,
    {
        let addr = match SockAddr::unix(path) {
            Ok(addr) => addr,
            Err(e) => return shared::fail(el, e, cb),
        };
        el.socket(Domain::UNIX, Type::STREAM, None, move |el, res| match res {
            Ok(fd) => {
                el.connect(fd.as_raw_fd(), addr, move |el, res| {
                    cb(el, res.map(|()| UnixStream::from(fd)))
                });
            }
            Err(e) => cb(el, Err(e)),
        })
    }

    /// Creates a connected pair of sockets (synchronously).
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = unix::UnixStream::pair()?;
        Ok((a.into(), b.into()))
    }
}

impl UnixDatagram {
    /// Binds to the socket file `path` (synchronously).
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        unix::UnixDatagram::bind(path).map(Self::from)
    }

    /// Creates a socket without an address (synchronously). Receivers see it
    /// as an unnamed sender.
    pub fn unbound() -> io::Result<Self> {
        unix::UnixDatagram::unbound().map(Self::from)
    }

    /// Creates a connected pair of sockets (synchronously).
    pub fn pair() -> io::Result<(Self, Self)> {
        let (a, b) = unix::UnixDatagram::pair()?;
        Ok((a.into(), b.into()))
    }

    /// Sets the default peer to the socket file `path`, so
    /// [`send`](Self::send) / [`recv`](Self::recv) can be used.
    pub fn connect<F>(&self, el: &mut EventLoop, path: impl AsRef<Path>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        match SockAddr::unix(path) {
            Ok(addr) => connect(el, &self.inner, addr, cb),
            Err(e) => shared::fail(el, e, cb),
        }
    }

    /// Sends one datagram, `buf[..buf.len()]`, to the socket file `path`.
    pub fn send_to<F>(
        &self,
        el: &mut EventLoop,
        buf: Vec<u8>,
        path: impl AsRef<Path>,
        cb: F,
    ) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        match SockAddr::unix(path) {
            Ok(addr) => send_to(el, &self.inner, buf, addr, cb),
            Err(e) => el.nop(move |el, _| cb(el, Err(e), buf)),
        }
    }

    /// Receives one datagram into `buf`. The callback gets the byte count and
    /// the sender's address: use [`SockAddr::as_pathname`] for a bound sender,
    /// or [`SockAddr::is_unnamed`] for an unbound one.
    pub fn recv_from<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<(usize, SockAddr)>, Vec<u8>) + 'static,
    {
        recv_from(el, &self.inner, buf, cb)
    }
}

// ---------------------------------------------------------------- helpers

fn accept<T, S, F>(el: &mut EventLoop, listener: &Rc<T>, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    S: From<OwnedFd>,
    F: FnOnce(&mut EventLoop, io::Result<S>) + 'static,
{
    let keep = Rc::clone(listener);
    el.accept(listener.as_raw_fd(), move |el, res| {
        drop(keep);
        cb(el, res.map(S::from))
    })
}

fn connect<T, F>(el: &mut EventLoop, socket: &Rc<T>, addr: SockAddr, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
{
    let keep = Rc::clone(socket);
    el.connect(socket.as_raw_fd(), addr, move |el, res| {
        drop(keep);
        cb(el, res)
    })
}

fn send_to<T, F>(el: &mut EventLoop, socket: &Rc<T>, buf: Vec<u8>, addr: SockAddr, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    let keep = Rc::clone(socket);
    el.send_to(socket.as_raw_fd(), buf, addr, move |el, res, buf| {
        drop(keep);
        cb(el, res, buf)
    })
}

fn recv_from<T, F>(el: &mut EventLoop, socket: &Rc<T>, buf: Vec<u8>, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<(usize, SockAddr)>, Vec<u8>) + 'static,
{
    let keep = Rc::clone(socket);
    el.recv_from(socket.as_raw_fd(), buf, move |el, res, buf| {
        drop(keep);
        cb(el, res, buf)
    })
}
