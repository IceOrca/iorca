use std::cell::RefCell;
use std::io::Read;
use std::net::{SocketAddr, UdpSocket};
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixDatagram, UnixListener};
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;

use iorca::{Domain, EventLoop, SockAddr, Type};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

fn temp_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("iorca-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn udp_send_to_and_recv_from() {
    // Unconnected sockets on both sides: the address travels with each datagram.
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client_addr = client.local_addr().unwrap();
    let (sfd, cfd) = (server.as_raw_fd(), client.as_raw_fd());

    let mut el = EventLoop::new().unwrap();
    let got: Slot<(Vec<u8>, SocketAddr, Vec<u8>)> = Rc::default();
    let g = got.clone();
    el.send_to(cfd, b"ping".to_vec(), server_addr, move |el, res, _| {
        assert_eq!(res.unwrap(), 4);
        el.recv_from(sfd, Vec::with_capacity(64), move |el, res, req| {
            let (n, from) = res.unwrap();
            assert_eq!(n, 4);
            let from = from.as_socket().unwrap();
            // Reply to whoever sent it.
            el.send_to(sfd, b"pong".to_vec(), from, move |el, res, _| {
                res.unwrap();
                el.recv_from(cfd, Vec::with_capacity(64), move |_, res, reply| {
                    res.unwrap();
                    *g.borrow_mut() = Some((req, from, reply));
                });
            });
        });
    });
    el.run().unwrap();

    let (req, from, reply) = got.take().unwrap();
    assert_eq!(req, b"ping");
    assert_eq!(from, client_addr);
    assert_eq!(reply, b"pong");
}

#[test]
fn udp_socket_created_by_iorca() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();

    let mut el = EventLoop::new().unwrap();
    el.socket(Domain::IPV4, Type::DGRAM, None, move |el, res| {
        let sock = res.unwrap();
        let fd = sock.as_raw_fd();
        el.send_to(fd, b"hi".to_vec(), server_addr, move |el, res, _| {
            assert_eq!(res.unwrap(), 2);
            el.close(sock, |_, res| res.unwrap());
        });
    });
    el.run().unwrap();

    let mut buf = [0u8; 8];
    let (n, _) = server.recv_from(&mut buf).unwrap();
    assert_eq!(&buf[..n], b"hi");
}

#[test]
fn unix_datagram_send_to_path() {
    let path = temp_path("dgram.sock");
    let server = UnixDatagram::bind(&path).unwrap();
    let sfd = server.as_raw_fd();
    let target = SockAddr::unix(&path).unwrap();

    let mut el = EventLoop::new().unwrap();
    let got: Slot<(Vec<u8>, bool)> = Rc::default();
    let g = got.clone();
    el.socket(Domain::UNIX, Type::DGRAM, None, move |el, res| {
        let client = res.unwrap();
        let cfd = client.as_raw_fd();
        el.send_to(cfd, b"over unix".to_vec(), target, move |el, res, _| {
            assert_eq!(res.unwrap(), 9);
            drop(client);
            el.recv_from(sfd, Vec::with_capacity(64), move |_, res, buf| {
                let (_, from) = res.unwrap();
                // The client never bound a path, so the sender is unnamed.
                *g.borrow_mut() = Some((buf, from.is_unnamed()));
            });
        });
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let (buf, unnamed) = got.take().unwrap();
    assert_eq!(buf, b"over unix");
    assert!(unnamed);
}

#[test]
fn unix_stream_connect() {
    let path = temp_path("stream.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    });

    let mut el = EventLoop::new().unwrap();
    let addr = SockAddr::unix(&path).unwrap();
    el.socket(Domain::UNIX, Type::STREAM, None, move |el, res| {
        let sock = res.unwrap();
        let fd = sock.as_raw_fd();
        el.connect(fd, addr, move |el, res| {
            res.unwrap();
            el.send(fd, b"stream".to_vec(), move |el, res, _| {
                assert_eq!(res.unwrap(), 6);
                el.close(sock, |_, res| res.unwrap());
            });
        });
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(server.join().unwrap(), b"stream");
}

#[test]
fn connect_refused_is_reported() {
    let path = temp_path("nobody.sock");
    let mut el = EventLoop::new().unwrap();
    let err = Rc::new(RefCell::new(None));
    let e = err.clone();
    el.socket(Domain::UNIX, Type::STREAM, None, move |el, res| {
        let sock = res.unwrap();
        el.connect(
            sock.as_raw_fd(),
            SockAddr::unix(&path).unwrap(),
            move |_, res| {
                drop(sock);
                *e.borrow_mut() = res.unwrap_err().raw_os_error();
            },
        );
    });
    el.run().unwrap();
    assert_eq!(*err.borrow(), Some(libc::ENOENT));
}
