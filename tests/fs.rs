use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;

use iorca::{EventLoop, Metadata, OpenOptions};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

fn temp_path(name: &str) -> PathBuf {
    let path: PathBuf = std::env::temp_dir().join(format!("iorca-{}-{name}", std::process::id()));
    let _: io::Result<()> = std::fs::remove_file(&path);
    path
}

#[test]
fn open_write_fsync_statx_read() {
    let path: PathBuf = temp_path("file.txt");
    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<(u64, bool, u32, Vec<u8>)> = Rc::default();
    let g: Slot<(u64, bool, u32, Vec<u8>)> = got.clone();
    let p: PathBuf = path.clone();

    let mut create: OpenOptions = OpenOptions::new();
    create.write(true).create(true).truncate(true).mode(0o640);
    el.open(
        &path,
        &create,
        move |el: &mut EventLoop, res: io::Result<OwnedFd>| {
            let file: OwnedFd = res.unwrap();
            let fd: RawFd = file.as_raw_fd();
            el.write(
                fd,
                b"hello fs".to_vec(),
                0,
                move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
                    assert_eq!(res.unwrap(), 8);
                    el.fsync(fd, move |el: &mut EventLoop, res: io::Result<()>| {
                        res.unwrap();
                        el.fdatasync(fd, move |el: &mut EventLoop, res: io::Result<()>| {
                            res.unwrap();
                            el.statx(fd, move |el: &mut EventLoop, res: io::Result<Metadata>| {
                                let meta: Metadata = res.unwrap();
                                el.close(file, |_: &mut EventLoop, res: io::Result<()>| {
                                    res.unwrap()
                                });
                                // Re-open read-only and read it back.
                                el.open(
                                    p,
                                    OpenOptions::new().read(true),
                                    move |el: &mut EventLoop, res: io::Result<OwnedFd>| {
                                        let file: OwnedFd = res.unwrap();
                                        let fd: RawFd = file.as_raw_fd();
                                        el.read(
                                            fd,
                                            Vec::with_capacity(64),
                                            0,
                                            move |_: &mut EventLoop,
                                                  res: io::Result<usize>,
                                                  buf: Vec<u8>| {
                                                res.unwrap();
                                                drop(file);
                                                *g.borrow_mut() = Some((
                                                    meta.len(),
                                                    meta.is_file(),
                                                    meta.permissions(),
                                                    buf,
                                                ));
                                            },
                                        );
                                    },
                                );
                            });
                        });
                    });
                },
            );
        },
    );
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let (len, is_file, perms, buf): (u64, bool, u32, Vec<u8>) = got.take().unwrap();
    assert_eq!(len, 8);
    assert!(is_file);
    // The process umask may clear bits, but never add them.
    assert_eq!(perms & !0o640, 0);
    assert_eq!(buf, b"hello fs");
}

#[test]
fn statx_path_on_directory() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let is_dir: Slot<bool> = Rc::new(RefCell::new(None));
    let d: Slot<bool> = is_dir.clone();
    el.statx_path(
        std::env::temp_dir(),
        move |_: &mut EventLoop, res: io::Result<Metadata>| {
            *d.borrow_mut() = Some(res.unwrap().is_dir());
        },
    );
    el.run().unwrap();
    assert_eq!(*is_dir.borrow(), Some(true));
}

#[test]
fn open_errors_are_reported_through_callbacks() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let errs: Rc<RefCell<Vec<ErrorKind>>> = Rc::new(RefCell::new(Vec::new()));

    let e: Rc<RefCell<Vec<ErrorKind>>> = errs.clone();
    el.open(
        temp_path("missing"),
        OpenOptions::new().read(true),
        move |_: &mut EventLoop, res: io::Result<OwnedFd>| {
            e.borrow_mut().push(res.unwrap_err().kind());
        },
    );
    let e: Rc<RefCell<Vec<ErrorKind>>> = errs.clone();
    el.open(
        "bad\0path",
        OpenOptions::new().read(true),
        move |_: &mut EventLoop, res: io::Result<OwnedFd>| {
            e.borrow_mut().push(res.unwrap_err().kind());
        },
    );
    let e: Rc<RefCell<Vec<ErrorKind>>> = errs.clone();
    // No access mode selected: rejected like std::fs::OpenOptions.
    el.open(
        temp_path("x"),
        &OpenOptions::new(),
        move |_: &mut EventLoop, res: io::Result<OwnedFd>| {
            e.borrow_mut().push(res.unwrap_err().kind());
        },
    );
    el.run().unwrap();

    assert_eq!(
        *errs.borrow(),
        vec![
            ErrorKind::NotFound,
            ErrorKind::InvalidInput,
            ErrorKind::InvalidInput
        ]
    );
}
