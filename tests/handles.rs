//! Typed handles: File, stdio, TCP, UDP and Unix sockets.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;

use iorca::{
    EventLoop, File, OpenOptions, TcpListener, TcpStream, UdpSocket, UnixDatagram, UnixListener,
    UnixStream, metadata, stdout,
};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

/// Datagrams received: payload, sender path, sender unnamed?
type Received = Rc<RefCell<Vec<(Vec<u8>, Option<PathBuf>, bool)>>>;

fn temp_path(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("iorca-h-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    path
}

#[test]
fn file_write_all_sync_metadata_read() {
    let path = temp_path("file");
    let mut el = EventLoop::new().unwrap();
    let got: Slot<(u64, Vec<u8>, u64)> = Rc::default();
    let g = got.clone();
    let p = path.clone();

    File::create(&mut el, &path, move |el, res| {
        let file = res.unwrap();
        let f = file.clone();
        file.write_all_at(el, b"0123456789".to_vec(), 0, move |el, res, buf| {
            res.unwrap();
            assert!(buf.is_empty(), "write_all returns the buffer cleared");
            let f2 = f.clone();
            f.sync_all(el, move |el, res| {
                res.unwrap();
                let f3 = f2.clone();
                f2.metadata(el, move |el, res| {
                    let len = res.unwrap().len();
                    f3.close(el, move |el, res| {
                        res.unwrap();
                        File::open(el, p.clone(), move |el, res| {
                            let file = res.unwrap();
                            file.read_at(el, Vec::with_capacity(4), 3, move |el, res, buf| {
                                res.unwrap();
                                metadata(el, p, move |_, res| {
                                    *g.borrow_mut() = Some((len, buf, res.unwrap().len()));
                                });
                            });
                        });
                    });
                });
            });
        });
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let (len, buf, path_len) = got.take().unwrap();
    assert_eq!(len, 10);
    assert_eq!(buf, b"3456");
    assert_eq!(path_len, 10);
}

#[test]
fn file_open_with_append() {
    let path = temp_path("append");
    std::fs::write(&path, b"abc").unwrap();
    let mut el = EventLoop::new().unwrap();
    let mut options = OpenOptions::new();
    options.append(true);
    File::open_with(&mut el, &path, &options, |el, res| {
        let file = res.unwrap();
        // With O_APPEND the kernel writes at the end, whatever the offset.
        file.write_all_at(el, b"def".to_vec(), 0, |_, res, _| res.unwrap());
    });
    el.run().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn file_open_missing_reports_not_found() {
    let mut el = EventLoop::new().unwrap();
    let kind = Rc::new(RefCell::new(None));
    let k = kind.clone();
    File::open(&mut el, temp_path("missing"), move |_, res| {
        *k.borrow_mut() = Some(res.unwrap_err().kind());
    });
    el.run().unwrap();
    assert_eq!(*kind.borrow(), Some(std::io::ErrorKind::NotFound));
}

#[test]
fn stdout_write_all_empty_and_short() {
    let mut el = EventLoop::new().unwrap();
    stdout().write_all(&mut el, Vec::new(), |_, res, _| res.unwrap());
    stdout().write(&mut el, Vec::new(), |_, res, _| assert_eq!(res.unwrap(), 0));
    el.run().unwrap();
}

#[test]
fn tcp_echo_with_handles() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let client = thread::spawn(move || {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(b"handles").unwrap();
        let mut reply = [0u8; 7];
        s.read_exact(&mut reply).unwrap();
        reply
    });

    let mut el = EventLoop::new().unwrap();
    listener.accept(&mut el, |el, res| {
        let stream = res.unwrap();
        let s = stream.clone();
        stream.recv(el, Vec::with_capacity(64), move |el, res, buf| {
            assert_eq!(res.unwrap(), 7);
            let s2 = s.clone();
            s.write_all(el, buf, move |el, res, _| {
                res.unwrap();
                s2.close(el, |_, res| res.unwrap());
            });
        });
    });
    // The listener handle can be dropped: the pending accept keeps it alive.
    drop(listener);
    el.run().unwrap();
    assert_eq!(&client.join().unwrap(), b"handles");
}

#[test]
fn tcp_stream_connect() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    });

    let mut el = EventLoop::new().unwrap();
    TcpStream::connect(&mut el, addr, |el, res| {
        let stream = res.unwrap();
        assert!(stream.as_std().peer_addr().is_ok());
        let s = stream.clone();
        stream.write_all(el, b"connected".to_vec(), move |el, res, _| {
            res.unwrap();
            s.close(el, |_, res| res.unwrap());
        });
    });
    el.run().unwrap();
    assert_eq!(server.join().unwrap(), b"connected");
}

#[test]
fn udp_ping_pong() {
    let server = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr = server.local_addr().unwrap();
    let client_addr = client.local_addr().unwrap();

    let mut el = EventLoop::new().unwrap();
    let got: Slot<(SocketAddr, Vec<u8>)> = Rc::default();
    let g = got.clone();
    let s = server.clone();
    server.recv_from(&mut el, Vec::with_capacity(64), move |el, res, buf| {
        let (_, peer) = res.unwrap();
        s.send_to(el, buf, peer, |_, res, _| assert_eq!(res.unwrap(), 4));
    });
    let c = client.clone();
    client.send_to(&mut el, b"ping".to_vec(), server_addr, move |el, res, _| {
        res.unwrap();
        c.recv_from(el, Vec::with_capacity(64), move |_, res, buf| {
            let (_, from) = res.unwrap();
            *g.borrow_mut() = Some((from, buf));
        });
    });
    el.run().unwrap();

    let (from, buf) = got.take().unwrap();
    assert_eq!(from, server_addr);
    assert_eq!(buf, b"ping");
    assert_ne!(client_addr.port(), 0);
}

#[test]
fn udp_connect_send_recv() {
    let a = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b_addr = b.local_addr().unwrap();

    let mut el = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let g = got.clone();
    let a2 = a.clone();
    a.connect(&mut el, b_addr, move |el, res| {
        res.unwrap();
        a2.send(el, b"connected udp".to_vec(), move |el, res, _| {
            res.unwrap();
            b.recv(el, Vec::with_capacity(64), move |_, res, buf| {
                res.unwrap();
                *g.borrow_mut() = Some(buf);
            });
        });
    });
    el.run().unwrap();
    assert_eq!(got.take().unwrap(), b"connected udp");
}

#[test]
fn dropped_handle_stays_open_until_op_completes() {
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let mut el = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let g = got.clone();
    socket.recv_from(&mut el, Vec::with_capacity(64), move |_, res, buf| {
        res.unwrap();
        *g.borrow_mut() = Some(buf);
    });
    // Drop the only user-visible handle while the receive is pending.
    drop(socket);
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .send_to(b"still open", addr)
        .unwrap();
    el.run().unwrap();
    assert_eq!(got.take().unwrap(), b"still open");
}

#[test]
fn unix_datagram_bound_and_unbound() {
    let server_path = temp_path("srv.sock");
    let client_path = temp_path("cli.sock");
    let server = UnixDatagram::bind(&server_path).unwrap();
    let named = UnixDatagram::bind(&client_path).unwrap();
    let unnamed = UnixDatagram::unbound().unwrap();

    let mut el = EventLoop::new().unwrap();
    let got: Received = Rc::default();
    for _ in 0..2 {
        let g = got.clone();
        server.recv_from(&mut el, Vec::with_capacity(64), move |_, res, buf| {
            let (_, from) = res.unwrap();
            let path = from.as_pathname().map(PathBuf::from);
            g.borrow_mut().push((buf, path, from.is_unnamed()));
        });
    }
    named.send_to(&mut el, b"named".to_vec(), &server_path, |_, res, _| {
        res.unwrap();
    });
    unnamed.send_to(&mut el, b"unnamed".to_vec(), &server_path, |_, res, _| {
        res.unwrap();
    });
    el.run().unwrap();
    std::fs::remove_file(&server_path).unwrap();
    std::fs::remove_file(&client_path).unwrap();

    let mut got = got.take();
    got.sort();
    assert_eq!(got[0], (b"named".to_vec(), Some(client_path), false));
    assert_eq!(got[1], (b"unnamed".to_vec(), None, true));
}

#[test]
fn unix_datagram_pair_and_connect() {
    let (a, b) = UnixDatagram::pair().unwrap();
    let path = temp_path("conn.sock");
    let target = UnixDatagram::bind(&path).unwrap();
    let sender = UnixDatagram::unbound().unwrap();

    let mut el = EventLoop::new().unwrap();
    let got: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
    a.send(&mut el, b"pair".to_vec(), |_, res, _| {
        res.unwrap();
    });
    let g = got.clone();
    b.recv(&mut el, Vec::with_capacity(64), move |_, res, buf| {
        res.unwrap();
        g.borrow_mut().push(buf);
    });
    let s = sender.clone();
    sender.connect(&mut el, &path, move |el, res| {
        res.unwrap();
        s.send(el, b"connected".to_vec(), |_, res, _| {
            res.unwrap();
        });
    });
    let g = got.clone();
    target.recv(&mut el, Vec::with_capacity(64), move |_, res, buf| {
        res.unwrap();
        g.borrow_mut().push(buf);
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut got = got.take();
    got.sort();
    assert_eq!(got, vec![b"connected".to_vec(), b"pair".to_vec()]);
}

#[test]
fn unix_stream_listener_and_connect() {
    let path = temp_path("stream.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let mut el = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();

    let g = got.clone();
    listener.accept(&mut el, move |el, res| {
        let conn = res.unwrap();
        conn.recv(el, Vec::with_capacity(64), move |_, res, buf| {
            res.unwrap();
            *g.borrow_mut() = Some(buf);
        });
    });
    UnixStream::connect(&mut el, &path, |el, res| {
        let stream = res.unwrap();
        let s = stream.clone();
        stream.write_all(el, b"unix stream".to_vec(), move |el, res, _| {
            res.unwrap();
            s.close(el, |_, res| res.unwrap());
        });
    });
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(got.take().unwrap(), b"unix stream");
}

#[test]
fn unix_stream_pair_and_connect_error() {
    let (a, b) = UnixStream::pair().unwrap();
    let mut el = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let err = Rc::new(RefCell::new(None));

    a.write_all(&mut el, b"pair".to_vec(), |_, res, _| res.unwrap());
    let g = got.clone();
    b.recv(&mut el, Vec::with_capacity(64), move |_, res, buf| {
        res.unwrap();
        *g.borrow_mut() = Some(buf);
    });
    let e = err.clone();
    UnixStream::connect(&mut el, temp_path("nobody.sock"), move |_, res| {
        *e.borrow_mut() = Some(res.unwrap_err().kind());
    });
    el.run().unwrap();
    assert_eq!(got.take().unwrap(), b"pair");
    assert_eq!(*err.borrow(), Some(std::io::ErrorKind::NotFound));
}
