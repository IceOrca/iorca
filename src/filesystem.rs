//! Filesystem handles.

use std::fs;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;

use crate::io::{self as shared, WriteMode};
use crate::{EventLoop, Metadata, OpId, OpenOptions};

/// An open file. Reads and writes are positional (`read_at` / `write_at`).
///
/// Cloning is cheap and shares the same fd. The fd closes when the last clone
/// and the last in-flight operation using it are gone, or earlier through
/// [`close`](Self::close).
#[derive(Clone, Debug)]
pub struct File {
    inner: Rc<fs::File>,
}

impl File {
    /// Opens `path` read-only.
    pub fn open<F>(el: &mut EventLoop, path: impl Into<PathBuf>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<File>) + 'static,
    {
        Self::open_with(el, path, OpenOptions::new().read(true), cb)
    }

    /// Opens `path` write-only, creating it or truncating an existing file.
    pub fn create<F>(el: &mut EventLoop, path: impl Into<PathBuf>, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<File>) + 'static,
    {
        let mut options = OpenOptions::new();
        options.write(true).create(true).truncate(true);
        Self::open_with(el, path, &options, cb)
    }

    /// Opens `path` with explicit `options`.
    pub fn open_with<F>(
        el: &mut EventLoop,
        path: impl Into<PathBuf>,
        options: &OpenOptions,
        cb: F,
    ) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<File>) + 'static,
    {
        el.open(path, options, move |el, res| cb(el, res.map(File::from)))
    }

    /// Reads up to `buf.capacity()` bytes at `offset` into the start of `buf`.
    /// `Ok(0)` means end of file.
    pub fn read_at<F>(&self, el: &mut EventLoop, buf: Vec<u8>, offset: u64, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        shared::read(el, &self.inner, buf, offset, cb)
    }

    /// Writes `buf[..buf.len()]` at `offset`, possibly short.
    pub fn write_at<F>(&self, el: &mut EventLoop, buf: Vec<u8>, offset: u64, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<usize>, Vec<u8>) + 'static,
    {
        shared::write(el, &self.inner, buf, offset, cb)
    }

    /// Writes all of `buf` starting at `offset`. On success the buffer comes
    /// back cleared.
    pub fn write_all_at<F>(&self, el: &mut EventLoop, buf: Vec<u8>, offset: u64, cb: F)
    where
        F: FnOnce(&mut EventLoop, io::Result<()>, Vec<u8>) + 'static,
    {
        shared::write_all(el, Rc::clone(&self.inner), WriteMode::At(offset), buf, cb)
    }

    /// Flushes data and metadata to disk (`fsync`).
    pub fn sync_all<F>(&self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        let keep = Rc::clone(&self.inner);
        el.fsync(self.as_raw_fd(), move |el, res| {
            drop(keep);
            cb(el, res)
        })
    }

    /// Flushes data to disk, skipping metadata not needed to read it back
    /// (`fdatasync`).
    pub fn sync_data<F>(&self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        let keep = Rc::clone(&self.inner);
        el.fdatasync(self.as_raw_fd(), move |el, res| {
            drop(keep);
            cb(el, res)
        })
    }

    /// Gets this file's metadata.
    pub fn metadata<F>(&self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<Metadata>) + 'static,
    {
        let keep = Rc::clone(&self.inner);
        el.statx(self.as_raw_fd(), move |el, res| {
            drop(keep);
            cb(el, res)
        })
    }

    /// Closes the file asynchronously. If clones or in-flight operations still
    /// use it, the fd closes when the last of them is gone instead.
    pub fn close<F>(self, el: &mut EventLoop, cb: F) -> OpId
    where
        F: FnOnce(&mut EventLoop, io::Result<()>) + 'static,
    {
        shared::close(el, self.inner, cb)
    }

    /// The underlying std file, for synchronous calls (e.g. `set_len`). Its
    /// blocking I/O methods would stall the event loop.
    pub fn as_std(&self) -> &fs::File {
        &self.inner
    }
}

/// Gets metadata for `path`, following symlinks.
pub fn metadata<F>(el: &mut EventLoop, path: impl Into<PathBuf>, cb: F) -> OpId
where
    F: FnOnce(&mut EventLoop, io::Result<Metadata>) + 'static,
{
    el.statx_path(path, cb)
}

impl From<fs::File> for File {
    fn from(file: fs::File) -> Self {
        Self {
            inner: Rc::new(file),
        }
    }
}

impl From<OwnedFd> for File {
    fn from(fd: OwnedFd) -> Self {
        Self::from(fs::File::from(fd))
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}
