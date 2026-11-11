//! Filesystem helper types: open flags and file metadata. No `unsafe` here.

use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::abi::StatxBuf;

/// Options for [`Op::Open`](crate::Op::Open), mirroring
/// [`std::fs::OpenOptions`]. The file is always opened close-on-exec.
#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
    mode: u32,
    custom_flags: i32,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenOptions {
    /// All options off, creation mode `0o666` (before umask).
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
            mode: 0o666,
            custom_flags: 0,
        }
    }

    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Permission bits used when a file is created.
    pub fn mode(&mut self, mode: u32) -> &mut Self {
        self.mode = mode;
        self
    }

    /// Extra `open(2)` flags (e.g. `O_DIRECT`). Access-mode bits are ignored.
    pub fn custom_flags(&mut self, flags: i32) -> &mut Self {
        self.custom_flags = flags;
        self
    }

    pub(crate) fn creation_mode(&self) -> u32 {
        self.mode
    }

    /// The `open(2)` flags, with the same validation rules as `std`.
    pub(crate) fn flags(&self) -> io::Result<i32> {
        let einval: fn() -> io::Result<i32> =
            || -> io::Result<i32> { Err(io::Error::from_raw_os_error(libc::EINVAL)) };
        let access: i32 = match (self.read, self.write, self.append) {
            (true, false, false) => libc::O_RDONLY,
            (false, true, false) => libc::O_WRONLY,
            (true, true, false) => libc::O_RDWR,
            (false, _, true) => libc::O_WRONLY | libc::O_APPEND,
            (true, _, true) => libc::O_RDWR | libc::O_APPEND,
            (false, false, false) => return einval(),
        };
        match (self.write, self.append) {
            (true, false) => {}
            (false, false) if self.truncate || self.create || self.create_new => return einval(),
            (_, true) if self.truncate && !self.create_new => return einval(),
            _ => {}
        }
        let creation: i32 = match (self.create, self.truncate, self.create_new) {
            (false, false, false) => 0,
            (true, false, false) => libc::O_CREAT,
            (false, true, false) => libc::O_TRUNC,
            (true, true, false) => libc::O_CREAT | libc::O_TRUNC,
            (_, _, true) => libc::O_CREAT | libc::O_EXCL,
        };
        Ok(libc::O_CLOEXEC | access | creation | (self.custom_flags & !libc::O_ACCMODE))
    }
}

/// File metadata from `statx(2)`.
#[derive(Clone, Debug)]
pub struct Metadata {
    len: u64,
    mode: u32,
    uid: u32,
    gid: u32,
    nlink: u32,
    ino: u64,
    blksize: u32,
    blocks: u64,
    accessed: SystemTime,
    modified: SystemTime,
    changed: SystemTime,
}

impl Metadata {
    pub(crate) fn from_statx(st: &StatxBuf) -> Self {
        Self {
            len: st.size(),
            mode: u32::from(st.mode()),
            uid: st.uid(),
            gid: st.gid(),
            nlink: st.nlink(),
            ino: st.ino(),
            blksize: st.blksize(),
            blocks: st.blocks(),
            accessed: to_system_time(st.atime_sec(), st.atime_nsec()),
            modified: to_system_time(st.mtime_sec(), st.mtime_nsec()),
            changed: to_system_time(st.ctime_sec(), st.ctime_nsec()),
        }
    }

    /// Size in bytes.
    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// File type and permission bits (`st_mode`).
    pub fn mode(&self) -> u32 {
        self.mode
    }

    /// Permission bits only (`mode & 0o7777`).
    pub fn permissions(&self) -> u32 {
        self.mode & 0o7777
    }

    pub fn is_file(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFREG
    }

    pub fn is_dir(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFDIR
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & libc::S_IFMT == libc::S_IFLNK
    }

    pub fn uid(&self) -> u32 {
        self.uid
    }

    pub fn gid(&self) -> u32 {
        self.gid
    }

    pub fn nlink(&self) -> u32 {
        self.nlink
    }

    pub fn ino(&self) -> u64 {
        self.ino
    }

    pub fn blksize(&self) -> u32 {
        self.blksize
    }

    /// Number of 512-byte blocks allocated.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    pub fn accessed(&self) -> SystemTime {
        self.accessed
    }

    pub fn modified(&self) -> SystemTime {
        self.modified
    }

    /// Last status change (`ctime`).
    pub fn changed(&self) -> SystemTime {
        self.changed
    }
}

fn to_system_time(sec: i64, nsec: u32) -> SystemTime {
    let nanos: Duration = Duration::from_nanos(u64::from(nsec));
    if sec >= 0 {
        UNIX_EPOCH + Duration::from_secs(sec as u64) + nanos
    } else {
        UNIX_EPOCH - Duration::from_secs(sec.unsigned_abs()) + nanos
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_flags_follow_std_rules() {
        assert!(OpenOptions::new().flags().is_err());
        assert!(OpenOptions::new().read(true).create(true).flags().is_err());
        let f: i32 = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .flags()
            .unwrap();
        assert_eq!(f & libc::O_ACCMODE, libc::O_WRONLY);
        assert_ne!(f & libc::O_CREAT, 0);
        assert_ne!(f & libc::O_TRUNC, 0);
        assert_ne!(f & libc::O_CLOEXEC, 0);
        let f: i32 = OpenOptions::new()
            .append(true)
            .create_new(true)
            .flags()
            .unwrap();
        assert_ne!(f & libc::O_APPEND, 0);
        assert_ne!(f & libc::O_EXCL, 0);
    }
}
