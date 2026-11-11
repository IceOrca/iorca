//! Public operation and result types.

use std::io;
use std::os::fd::{OwnedFd, RawFd};
use std::path::PathBuf;
use std::time::Duration;

use socket2::{Domain, Protocol, SockAddr, Type};

use crate::fs::{Metadata, OpenOptions};

/// An operation to submit. Owns every resource the kernel will access.
///
/// File descriptors given as [`RawFd`] must stay open until the operation
/// completes. A closed fd causes an error result, never memory unsafety.
///
/// Each variant documents which [`Output`] variant it completes with.
#[derive(Debug)]
pub enum Op {
    /// Does nothing. Completes with [`Output::Unit`].
    Nop,
    /// Completes with `Output::Unit(Ok(()))` after the duration elapses.
    Timeout(Duration),
    /// Cancels the operation with id `target`. Completes with [`Output::Unit`].
    /// The target itself completes with `ECANCELED`.
    Cancel { target: u64 },

    // ---- generic fd I/O (files, pipes, stdio, sockets) ----
    /// Reads up to `buf.capacity()` bytes at `offset` (`u64::MAX` = current
    /// position) into the start of `buf`. Completes with [`Output::Bytes`],
    /// with `buf.len()` set to the bytes read.
    Read {
        fd: RawFd,
        buf: Vec<u8>,
        offset: u64,
    },
    /// Writes `buf[..buf.len()]` at `offset` (`u64::MAX` = current position).
    /// Completes with [`Output::Bytes`] (possibly a short count).
    Write {
        fd: RawFd,
        buf: Vec<u8>,
        offset: u64,
    },
    /// Closes the fd. Completes with [`Output::Unit`].
    Close { fd: OwnedFd },

    // ---- sockets ----
    /// Creates a close-on-exec socket. Completes with [`Output::Fd`].
    Socket {
        domain: Domain,
        ty: Type,
        protocol: Option<Protocol>,
    },
    /// Connects `fd` to `addr` (IP or Unix). Completes with [`Output::Unit`].
    Connect { fd: RawFd, addr: SockAddr },
    /// Accepts a connection (close-on-exec). Completes with [`Output::Fd`].
    Accept { listener: RawFd },
    /// Like [`Op::Read`] for a connected socket, without an offset.
    Recv { fd: RawFd, buf: Vec<u8> },
    /// Like [`Op::Write`] for a connected socket, with `MSG_NOSIGNAL`.
    Send { fd: RawFd, buf: Vec<u8> },
    /// Sends one datagram from `buf[..buf.len()]` to `addr` (`sendmsg`), with
    /// `MSG_NOSIGNAL`. Completes with [`Output::Bytes`].
    SendTo {
        fd: RawFd,
        buf: Vec<u8>,
        addr: SockAddr,
    },
    /// Receives one datagram into `buf` (filled like [`Op::Read`]) and reports
    /// the sender's address (`recvmsg`). Completes with [`Output::Datagram`].
    RecvFrom { fd: RawFd, buf: Vec<u8> },

    // ---- filesystem ----
    /// Opens `path` (relative paths are resolved against the current
    /// directory) with close-on-exec. Completes with [`Output::Fd`].
    Open { path: PathBuf, options: OpenOptions },
    /// Flushes file data and metadata (`fsync`), or only data when
    /// `data_only` (`fdatasync`). Completes with [`Output::Unit`].
    Fsync { fd: RawFd, data_only: bool },
    /// Gets metadata for an open fd. Completes with [`Output::Metadata`].
    Statx { fd: RawFd },
    /// Gets metadata for `path`, following symlinks. Completes with
    /// [`Output::Metadata`].
    StatxPath { path: PathBuf },
}

/// The result of an operation, with its resources handed back.
#[derive(Debug)]
pub enum Output {
    /// Nop, Timeout, Cancel, Close, Connect, Fsync.
    Unit(io::Result<()>),
    /// Read, Write, Recv, Send, SendTo: byte count and the buffer.
    Bytes(io::Result<usize>, Vec<u8>),
    /// RecvFrom: byte count, sender address and the buffer.
    Datagram(io::Result<(usize, SockAddr)>, Vec<u8>),
    /// Socket, Accept, Open: the new fd.
    Fd(io::Result<OwnedFd>),
    /// Statx, StatxPath.
    Metadata(io::Result<Metadata>),
}

/// A finished operation.
#[derive(Debug)]
pub struct Completion {
    /// The id returned by [`Ring::push`](crate::Ring::push).
    pub id: u64,
    pub output: Output,
}
