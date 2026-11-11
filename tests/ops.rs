use std::cell::{Cell, RefCell};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::rc::Rc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use iorca::{EventLoop, OpId};

#[test]
fn nop_completes() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let done: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let d: Rc<Cell<bool>> = done.clone();
    el.nop(move |_: &mut EventLoop, res: io::Result<()>| {
        res.unwrap();
        d.set(true);
    });
    assert_eq!(el.in_flight(), 1);
    el.run().unwrap();
    assert!(done.get());
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn callbacks_can_chain_operations() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let order: Rc<RefCell<Vec<i32>>> = Rc::new(RefCell::new(Vec::new()));
    let o: Rc<RefCell<Vec<i32>>> = order.clone();
    el.nop(move |el: &mut EventLoop, _: io::Result<()>| {
        o.borrow_mut().push(1);
        el.nop(move |_: &mut EventLoop, _: io::Result<()>| o.borrow_mut().push(2));
    });
    el.run().unwrap();
    assert_eq!(*order.borrow(), vec![1, 2]);
}

#[test]
fn more_ops_than_queue_entries() {
    // A 4-entry SQ must still accept 100 submissions via the backlog.
    let mut el: EventLoop = EventLoop::with_entries(4).unwrap();
    let count: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    for _ in 0..100 {
        let c: Rc<Cell<u32>> = count.clone();
        el.nop(move |_: &mut EventLoop, res: io::Result<()>| {
            res.unwrap();
            c.set(c.get() + 1);
        });
    }
    el.run().unwrap();
    assert_eq!(count.get(), 100);
}

#[test]
fn timeout_fires_after_duration() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let fired: Rc<Cell<Option<Duration>>> = Rc::new(Cell::new(None));
    let f: Rc<Cell<Option<Duration>>> = fired.clone();
    el.timeout(
        Duration::from_millis(50),
        move |_: &mut EventLoop, res: io::Result<()>| {
            res.unwrap();
            f.set(Some(start.elapsed()));
        },
    );
    el.run().unwrap();
    assert!(fired.get().expect("timer did not fire") >= Duration::from_millis(50));
}

#[test]
fn cancel_timeout() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let target_err: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let t: Rc<Cell<Option<i32>>> = target_err.clone();
    let id: OpId = el.timeout(
        Duration::from_secs(10),
        move |_: &mut EventLoop, res: io::Result<()>| {
            t.set(res.unwrap_err().raw_os_error());
        },
    );
    el.cancel(id, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
    el.run().unwrap();
    assert_eq!(target_err.get(), Some(libc::ECANCELED));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn stop_returns_with_ops_pending() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    el.timeout(
        Duration::from_secs(10),
        |_: &mut EventLoop, _: io::Result<()>| {},
    );
    el.nop(|el: &mut EventLoop, _: io::Result<()>| el.stop());
    el.run().unwrap();
    assert_eq!(el.in_flight(), 1);
}

#[test]
fn write_then_read_file() {
    let path: PathBuf = std::env::temp_dir().join(format!("iorca-test-{}", std::process::id()));
    let file: fs::File = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    let fd: RawFd = file.as_raw_fd();

    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Rc<RefCell<Vec<u8>>> = Rc::new(RefCell::new(Vec::new()));
    let g: Rc<RefCell<Vec<u8>>> = got.clone();
    el.write(
        fd,
        b"hello iorca".to_vec(),
        0,
        move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            assert_eq!(res.unwrap(), 11);
            el.read(
                fd,
                Vec::with_capacity(64),
                0,
                move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
                    assert_eq!(res.unwrap(), 11);
                    *g.borrow_mut() = buf;
                },
            );
        },
    );
    el.run().unwrap();
    drop(file);
    fs::remove_file(&path).unwrap();
    assert_eq!(&*got.borrow(), b"hello iorca");
}

#[test]
fn read_reports_errors() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let err: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let e: Rc<Cell<Option<i32>>> = err.clone();
    el.read(
        -1,
        Vec::with_capacity(8),
        0,
        move |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            e.set(res.unwrap_err().raw_os_error());
        },
    );
    el.run().unwrap();
    assert_eq!(err.get(), Some(libc::EBADF));
}

#[test]
fn tcp_accept_recv_send() {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let client: JoinHandle<[u8; 4]> = thread::spawn(move || -> [u8; 4] {
        let mut s: TcpStream = TcpStream::connect(addr).unwrap();
        s.write_all(b"ping").unwrap();
        let mut reply: [u8; 4] = [0u8; 4];
        s.read_exact(&mut reply).unwrap();
        reply
    });

    let mut el: EventLoop = EventLoop::new().unwrap();
    el.accept(
        listener.as_raw_fd(),
        |el: &mut EventLoop, res: io::Result<OwnedFd>| {
            let conn: OwnedFd = res.unwrap();
            let fd: RawFd = conn.as_raw_fd();
            el.recv(
                fd,
                Vec::with_capacity(16),
                move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
                    assert_eq!(res.unwrap(), 4);
                    el.send(
                        fd,
                        buf,
                        move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
                            assert_eq!(res.unwrap(), 4);
                            el.close(conn, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
                        },
                    );
                },
            );
        },
    );
    el.run().unwrap();
    assert_eq!(&client.join().unwrap(), b"ping");
}

#[test]
fn tcp_connect_and_send() {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let server: JoinHandle<Vec<u8>> = thread::spawn(move || -> Vec<u8> {
        let (mut s, _): (TcpStream, SocketAddr) = listener.accept().unwrap();
        let mut got: Vec<u8> = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    });

    let mut el: EventLoop = EventLoop::new().unwrap();
    el.tcp_connect(addr, |el: &mut EventLoop, res: io::Result<OwnedFd>| {
        let conn: OwnedFd = res.unwrap();
        let fd: RawFd = conn.as_raw_fd();
        el.send(
            fd,
            b"hi".to_vec(),
            move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
                assert_eq!(res.unwrap(), 2);
                el.close(conn, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
            },
        );
    });
    el.run().unwrap();
    assert_eq!(server.join().unwrap(), b"hi");
}
