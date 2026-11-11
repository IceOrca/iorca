use std::cell::RefCell;
use std::io::ErrorKind;
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::rc::Rc;

use iorca::{EventLoop, OpenOptions};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

fn temp_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("iorca-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn open_write_fsync_statx_read() {
    let path = temp_path("file.txt");
    let mut el = EventLoop::new().unwrap();
    let got: Slot<(u64, bool, u32, Vec<u8>)> = Rc::default();
    let g = got.clone();
    let p = path.clone();

    let mut create = OpenOptions::new();
    create.write(true).create(true).truncate(true).mode(0o640);
    el.open(&path, &create, move |el, res| {
        let file = res.unwrap();
        let fd = file.as_raw_fd();
        el.write(fd, b"hello fs".to_vec(), 0, move |el, res, _| {
            assert_eq!(res.unwrap(), 8);
            el.fsync(fd, move |el, res| {
                res.unwrap();
                el.fdatasync(fd, move |el, res| {
                    res.unwrap();
                    el.statx(fd, move |el, res| {
                        let meta = res.unwrap();
                        el.close(file, |_, res| res.unwrap());
                        // Re-open read-only and read it back.
                        el.open(p, OpenOptions::new().read(true), move |el, res| {
                            let file = res.unwrap();
                            let fd = file.as_raw_fd();
                            el.read(fd, Vec::with_capacity(64), 0, move |_, res, buf| {
                                res.unwrap();
                                drop(file);
                                *g.borrow_mut() =
                                    Some((meta.len(), meta.is_file(), meta.permissions(), buf));
                            });
                        });
                    });
                });
            });
        });
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let (len, is_file, perms, buf) = got.take().unwrap();
    assert_eq!(len, 8);
    assert!(is_file);
    // The process umask may clear bits, but never add them.
    assert_eq!(perms & !0o640, 0);
    assert_eq!(buf, b"hello fs");
}

#[test]
fn statx_path_on_directory() {
    let mut el = EventLoop::new().unwrap();
    let is_dir = Rc::new(RefCell::new(None));
    let d = is_dir.clone();
    el.statx_path(std::env::temp_dir(), move |_, res| {
        *d.borrow_mut() = Some(res.unwrap().is_dir());
    });
    el.run().unwrap();
    assert_eq!(*is_dir.borrow(), Some(true));
}

#[test]
fn open_errors_are_reported_through_callbacks() {
    let mut el = EventLoop::new().unwrap();
    let errs = Rc::new(RefCell::new(Vec::new()));

    let e = errs.clone();
    el.open(
        temp_path("missing"),
        OpenOptions::new().read(true),
        move |_, res| {
            e.borrow_mut().push(res.unwrap_err().kind());
        },
    );
    let e = errs.clone();
    el.open("bad\0path", OpenOptions::new().read(true), move |_, res| {
        e.borrow_mut().push(res.unwrap_err().kind());
    });
    let e = errs.clone();
    // No access mode selected: rejected like std::fs::OpenOptions.
    el.open(temp_path("x"), &OpenOptions::new(), move |_, res| {
        e.borrow_mut().push(res.unwrap_err().kind());
    });
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
