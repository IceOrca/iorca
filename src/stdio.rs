//! Standard I/O handles, plus helpers shared by every typed handle.
//!
//! Every handle wraps its std type in an `Rc`. Each operation clones that `Rc`
//! into its callback, so the fd stays open (and its number can't be reused)
//! until the kernel has completed the operation, even if the user drops the
//! handle in the meantime.

use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::rc::Rc;

use crate::{EventLoop, OpId};

/// How [`write_all`] submits each chunk.
#[derive(Clone, Copy, Debug)]
pub(crate) enum WriteMode {
    /// `write` at an explicit offset that advances after each chunk (files).
    At(u64),
    /// `write` at the current position (pipes, ttys, stdio).
    Current,
    /// `send` (connected sockets).
    Send,
}

impl WriteMode {
    fn advance(self, n: usize) -> Self {
        match self {
            WriteMode::At(offset) => WriteMode::At(offset + n as u64),
            other => other,
        }
    }
}

/// Writes all of `buf`, resubmitting after short writes.
///
/// On success the callback gets the buffer back cleared (capacity kept). On
/// error it holds the bytes that were not written.
pub(crate) fn write_all<T, F>(el: &mut EventLoop, fd: Rc<T>, mode: WriteMode, buf: Vec<u8>, cb: F)
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<()>, Vec<u8>) + 'static,
{
    if buf.is_empty() {
        // Still complete asynchronously, never inside the caller.
        el.nop(move |el: &mut EventLoop, _: io::Result<()>| cb(el, Ok(()), buf));
        return;
    }
    let raw: RawFd = fd.as_raw_fd();
    // Boxed so the continuation has a nameable type.
    type Next = Box<dyn FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>)>;
    let next: Next = Box::new(
        move |el: &mut EventLoop, res: io::Result<usize>, mut buf: Vec<u8>| match res {
            Ok(0) => cb(el, Err(io::Error::from(io::ErrorKind::WriteZero)), buf),
            Ok(n) => {
                buf.drain(..n);
                if buf.is_empty() {
                    cb(el, Ok(()), buf)
                } else {
                    write_all(el, fd, mode.advance(n), buf, cb)
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => write_all(el, fd, mode, buf, cb),
            Err(e) => cb(el, Err(e), buf),
        },
    );
    match mode {
        WriteMode::At(offset) => el.write(raw, buf, offset, next),
        WriteMode::Current => el.write(raw, buf, u64::MAX, next),
        WriteMode::Send => el.send(raw, buf, next),
    };
}

/// `recv` on `fd`, keeping it alive until completion.
pub(crate) fn recv<T, F>(el: &mut EventLoop, fd: &Rc<T>, buf: Vec<u8>, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    let keep: Rc<T> = Rc::clone(fd);
    el.recv(
        fd.as_raw_fd(),
        buf,
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            drop(keep);
            cb(el, res, buf)
        },
    )
}

/// `send` on `fd`, keeping it alive until completion.
pub(crate) fn send<T, F>(el: &mut EventLoop, fd: &Rc<T>, buf: Vec<u8>, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    let keep: Rc<T> = Rc::clone(fd);
    el.send(
        fd.as_raw_fd(),
        buf,
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            drop(keep);
            cb(el, res, buf)
        },
    )
}

/// `read` on `fd` at `offset`, keeping it alive until completion.
pub(crate) fn read<T, F>(el: &mut EventLoop, fd: &Rc<T>, buf: Vec<u8>, offset: u64, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    let keep: Rc<T> = Rc::clone(fd);
    el.read(
        fd.as_raw_fd(),
        buf,
        offset,
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            drop(keep);
            cb(el, res, buf)
        },
    )
}

/// `write` on `fd` at `offset`, keeping it alive until completion.
pub(crate) fn write<T, F>(el: &mut EventLoop, fd: &Rc<T>, buf: Vec<u8>, offset: u64, cb: F) -> OpId
where
    T: AsRawFd + 'static,
    F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
{
    let keep: Rc<T> = Rc::clone(fd);
    el.write(
        fd.as_raw_fd(),
        buf,
        offset,
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            drop(keep);
            cb(el, res, buf)
        },
    )
}

/// Closes the handle's fd asynchronously if this is the last reference.
///
/// If clones or in-flight operations still hold it, this reference is just
/// dropped and the fd closes when the last one goes away; the callback still
/// gets `Ok(())`.
pub(crate) fn close<T, F>(el: &mut EventLoop, fd: Rc<T>, cb: F) -> OpId
where
    T: 'static,
    OwnedFd: From<T>,
    F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
{
    match Rc::try_unwrap(fd) {
        Ok(inner) => el.close(OwnedFd::from(inner), cb),
        Err(shared) => {
            drop(shared);
            el.nop(move |el: &mut EventLoop, _: io::Result<()>| cb(el, Ok(())))
        }
    }
}

/// Delivers an error that happened before submission through the callback,
/// on the next loop turn.
pub(crate) fn fail<T, F>(el: &mut EventLoop, err: io::Error, cb: F) -> OpId
where
    T: 'static,
    F: FnOnce(&mut EventLoop, io::Result<T>) + 'static,
{
    el.nop(move |el: &mut EventLoop, _: io::Result<()>| cb(el, Err(err)))
}

/// Handle to the process's standard input. Created by [`stdin`].
#[derive(Clone, Debug)]
pub struct Stdin(Rc<io::Stdin>);

/// Handle to the process's standard output. Created by [`stdout`].
///
/// Writes go straight to fd 1, bypassing the buffer behind `println!`, so
/// mixing the two can reorder output.
#[derive(Clone, Debug)]
pub struct Stdout(Rc<io::Stdout>);

/// Handle to the process's standard error. Created by [`stderr`].
#[derive(Clone, Debug)]
pub struct Stderr(Rc<io::Stderr>);

/// Returns a handle to standard input.
pub fn stdin() -> Stdin {
    Stdin(Rc::new(io::stdin()))
}

/// Returns a handle to standard output.
pub fn stdout() -> Stdout {
    Stdout(Rc::new(io::stdout()))
}

/// Returns a handle to standard error.
pub fn stderr() -> Stderr {
    Stderr(Rc::new(io::stderr()))
}

impl Stdin {
    /// Reads up to `buf.capacity()` bytes. `Ok(0)` means end of input.
    pub fn read<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        read(el, &self.0, buf, u64::MAX, cb)
    }
}

macro_rules! output_handle {
    ($name:ident) => {
        impl $name {
            /// Writes `buf[..buf.len()]`, possibly short. The callback gets the
            /// byte count and the buffer back.
            pub fn write<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F) -> OpId
            where
                F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
            {
                write(el, &self.0, buf, u64::MAX, cb)
            }

            /// Writes all of `buf`. On success the buffer comes back cleared.
            pub fn write_all<F>(&self, el: &mut EventLoop, buf: Vec<u8>, cb: F)
            where
                F: FnOnce(&mut EventLoop, io::Result<()>, Vec<u8>) + 'static,
            {
                write_all(el, Rc::clone(&self.0), WriteMode::Current, buf, cb)
            }
        }
    };
}

output_handle!(Stdout);
output_handle!(Stderr);
