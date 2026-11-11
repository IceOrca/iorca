//! # iorca
//!
//! A small, completion-based, event-driven I/O library built directly on
//! Linux `io_uring`. There is no `async`/`await` and no executor: you submit an
//! operation together with a callback, and the [`EventLoop`] invokes that
//! callback when the kernel reports completion.
//!
//! ## Model
//!
//! * Every operation is a method on [`EventLoop`] that takes a callback and
//!   returns an [`OpId`].
//! * Callbacks receive `&mut EventLoop`, so they can submit follow-up
//!   operations (this is how you build loops: accept → recv → send → recv ...).
//! * Buffers are passed **by value** (`Vec<u8>`) and handed back in the
//!   callback. The loop owns the buffer while the kernel may access it, which
//!   is what makes completion-based I/O memory safe (same idea as
//!   `tokio-uring`'s buffer ownership model).
//! * Typed handles keep their fd open while operations are in flight (see
//!   below). With the raw ops you pass a [`RawFd`](std::os::fd::RawFd) and
//!   must keep it open until the operation completes yourself.
//! * [`EventLoop::run`] returns when no operations are in flight or when
//!   [`EventLoop::stop`] is called.
//!
//! ## Two API levels
//!
//! * **Typed handles** (recommended): [`File`], [`TcpListener`], [`TcpStream`],
//!   [`UdpSocket`], [`UnixListener`], [`UnixStream`], [`UnixDatagram`] and
//!   [`stdin`]/[`stdout`]/[`stderr`]. Each wraps its std type in an `Rc`, and
//!   every operation keeps a clone until it completes. So the fd can't close
//!   under an in-flight operation, and dropping a handle early is safe. Methods
//!   take `&mut EventLoop` plus a callback. Binding and listening are
//!   synchronous; `as_std()` exposes the std type for socket options and
//!   addresses.
//! * **Raw ops** on [`EventLoop`] (`read`, `send_to`, `open`, `statx`, ...)
//!   take a [`RawFd`](std::os::fd::RawFd). You keep the fd open yourself.
//!
//! ## Example
//!
//! ```no_run
//! use iorca::EventLoop;
//! use std::time::Duration;
//!
//! let mut el: EventLoop = EventLoop::new().unwrap();
//! el.timeout(Duration::from_millis(10), |el: &mut EventLoop, res: std::io::Result<()>| {
//!     res.unwrap();
//!     println!("timer fired");
//!     el.nop(|_: &mut EventLoop, _: std::io::Result<()>| println!("nop completed"));
//! });
//! el.run().unwrap();
//! ```
//!
//! ## Unsafe code
//!
//! This crate contains no `unsafe` code; `Cargo.toml` sets
//! `unsafe_code = "forbid"`. Everything io_uring requires lives in the
//! `iorca-driver` crate, whose safe API takes ownership of buffers and fds
//! until the kernel completes each operation. User code never needs `unsafe`.

mod event_loop;
mod filesystem;
mod io;
mod network;
mod ops;

pub use event_loop::{EventLoop, OpId};
pub use filesystem::{File, metadata};
pub use io::{Stderr, Stdin, Stdout, stderr, stdin, stdout};
pub use iorca_driver::{Domain, Metadata, OpenOptions, Protocol, SockAddr, Type};
pub use network::{TcpListener, TcpStream, UdpSocket, UnixDatagram, UnixListener, UnixStream};
