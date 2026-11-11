//! Typed handles: File, stdio, TCP, UDP and Unix sockets.

use std::cell::RefCell;
use std::io::{self, ErrorKind, Read, Write};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::rc::Rc;
use std::thread::{self, JoinHandle};

use iorca::{
    EventLoop, File, Metadata, OpenOptions, SockAddr, TcpListener, TcpStream, UdpSocket,
    UnixDatagram, UnixListener, UnixStream, metadata, stdout,
};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

/// One received datagram: payload, sender path, sender unnamed?
type Datagram = (Vec<u8>, Option<PathBuf>, bool);

/// Datagrams collected by callbacks.
type Received = Rc<RefCell<Vec<Datagram>>>;

fn temp_path(name: &str) -> PathBuf {
    let path: PathBuf = std::env::temp_dir().join(format!("iorca-h-{}-{name}", std::process::id()));
    let _: io::Result<()> = std::fs::remove_file(&path);
    path
}

#[test]
fn file_write_all_sync_metadata_read() {
    let path: PathBuf = temp_path("file");
    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<(u64, Vec<u8>, u64)> = Rc::default();
    let g: Slot<(u64, Vec<u8>, u64)> = got.clone();
    let p: PathBuf = path.clone();

    File::create(
        &mut el,
        &path,
        move |el: &mut EventLoop, res: io::Result<File>| {
            let file: File = res.unwrap();
            let f: File = file.clone();
            file.write_all_at(
                el,
                b"0123456789".to_vec(),
                0,
                move |el: &mut EventLoop, res: io::Result<()>, buf: Vec<u8>| {
                    res.unwrap();
                    assert!(buf.is_empty(), "write_all returns the buffer cleared");
                    let f2: File = f.clone();
                    f.sync_all(el, move |el: &mut EventLoop, res: io::Result<()>| {
                        res.unwrap();
                        let f3: File = f2.clone();
                        f2.metadata(el, move |el: &mut EventLoop, res: io::Result<Metadata>| {
                            let len: u64 = res.unwrap().len();
                            f3.close(el, move |el: &mut EventLoop, res: io::Result<()>| {
                                res.unwrap();
                                File::open(
                                    el,
                                    p.clone(),
                                    move |el: &mut EventLoop, res: io::Result<File>| {
                                        let file: File = res.unwrap();
                                        file.read_at(
                                            el,
                                            Vec::with_capacity(4),
                                            3,
                                            move |el: &mut EventLoop,
                                                  res: io::Result<usize>,
                                                  buf: Vec<u8>| {
                                                res.unwrap();
                                                metadata(
                                                    el,
                                                    p,
                                                    move |_: &mut EventLoop,
                                                          res: io::Result<Metadata>| {
                                                        *g.borrow_mut() =
                                                            Some((len, buf, res.unwrap().len()));
                                                    },
                                                );
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

    let (len, buf, path_len): (u64, Vec<u8>, u64) = got.take().unwrap();
    assert_eq!(len, 10);
    assert_eq!(buf, b"3456");
    assert_eq!(path_len, 10);
}

#[test]
fn file_open_with_append() {
    let path: PathBuf = temp_path("append");
    std::fs::write(&path, b"abc").unwrap();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let mut options: OpenOptions = OpenOptions::new();
    options.append(true);
    File::open_with(
        &mut el,
        &path,
        &options,
        |el: &mut EventLoop, res: io::Result<File>| {
            let file: File = res.unwrap();
            // With O_APPEND the kernel writes at the end, whatever the offset.
            file.write_all_at(
                el,
                b"def".to_vec(),
                0,
                |_: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| res.unwrap(),
            );
        },
    );
    el.run().unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"abcdef");
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn file_open_missing_reports_not_found() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let kind: Slot<ErrorKind> = Rc::new(RefCell::new(None));
    let k: Slot<ErrorKind> = kind.clone();
    File::open(
        &mut el,
        temp_path("missing"),
        move |_: &mut EventLoop, res: io::Result<File>| {
            *k.borrow_mut() = Some(res.unwrap_err().kind());
        },
    );
    el.run().unwrap();
    assert_eq!(*kind.borrow(), Some(ErrorKind::NotFound));
}

#[test]
fn stdout_write_all_empty_and_short() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    stdout().write_all(
        &mut el,
        Vec::new(),
        |_: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| res.unwrap(),
    );
    stdout().write(
        &mut el,
        Vec::new(),
        |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| assert_eq!(res.unwrap(), 0),
    );
    el.run().unwrap();
}

#[test]
fn tcp_echo_with_handles() {
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let client: JoinHandle<[u8; 7]> = thread::spawn(move || -> [u8; 7] {
        let mut s: std::net::TcpStream = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(b"handles").unwrap();
        let mut reply: [u8; 7] = [0u8; 7];
        s.read_exact(&mut reply).unwrap();
        reply
    });

    let mut el: EventLoop = EventLoop::new().unwrap();
    listener.accept(&mut el, |el: &mut EventLoop, res: io::Result<TcpStream>| {
        let stream: TcpStream = res.unwrap();
        let s: TcpStream = stream.clone();
        stream.recv(
            el,
            Vec::with_capacity(64),
            move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
                assert_eq!(res.unwrap(), 7);
                let s2: TcpStream = s.clone();
                s.write_all(
                    el,
                    buf,
                    move |el: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| {
                        res.unwrap();
                        s2.close(el, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
                    },
                );
            },
        );
    });
    // The listener handle can be dropped: the pending accept keeps it alive.
    drop(listener);
    el.run().unwrap();
    assert_eq!(&client.join().unwrap(), b"handles");
}

#[test]
fn tcp_stream_connect() {
    let listener: std::net::TcpListener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    let server: JoinHandle<Vec<u8>> = thread::spawn(move || -> Vec<u8> {
        let (mut s, _): (std::net::TcpStream, SocketAddr) = listener.accept().unwrap();
        let mut got: Vec<u8> = Vec::new();
        s.read_to_end(&mut got).unwrap();
        got
    });

    let mut el: EventLoop = EventLoop::new().unwrap();
    TcpStream::connect(
        &mut el,
        addr,
        |el: &mut EventLoop, res: io::Result<TcpStream>| {
            let stream: TcpStream = res.unwrap();
            assert!(stream.as_std().peer_addr().is_ok());
            let s: TcpStream = stream.clone();
            stream.write_all(
                el,
                b"connected".to_vec(),
                move |el: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| {
                    res.unwrap();
                    s.close(el, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
                },
            );
        },
    );
    el.run().unwrap();
    assert_eq!(server.join().unwrap(), b"connected");
}

#[test]
fn udp_ping_pong() {
    let server: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let client: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let server_addr: SocketAddr = server.local_addr().unwrap();
    let client_addr: SocketAddr = client.local_addr().unwrap();

    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<(SocketAddr, Vec<u8>)> = Rc::default();
    let g: Slot<(SocketAddr, Vec<u8>)> = got.clone();
    let s: UdpSocket = server.clone();
    server.recv_from(
        &mut el,
        Vec::with_capacity(64),
        move |el: &mut EventLoop, res: io::Result<(usize, SocketAddr)>, buf: Vec<u8>| {
            let (_, peer): (usize, SocketAddr) = res.unwrap();
            s.send_to(
                el,
                buf,
                peer,
                |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| assert_eq!(res.unwrap(), 4),
            );
        },
    );
    let c: UdpSocket = client.clone();
    client.send_to(
        &mut el,
        b"ping".to_vec(),
        server_addr,
        move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            res.unwrap();
            c.recv_from(
                el,
                Vec::with_capacity(64),
                move |_: &mut EventLoop, res: io::Result<(usize, SocketAddr)>, buf: Vec<u8>| {
                    let (_, from): (usize, SocketAddr) = res.unwrap();
                    *g.borrow_mut() = Some((from, buf));
                },
            );
        },
    );
    el.run().unwrap();

    let (from, buf): (SocketAddr, Vec<u8>) = got.take().unwrap();
    assert_eq!(from, server_addr);
    assert_eq!(buf, b"ping");
    assert_ne!(client_addr.port(), 0);
}

#[test]
fn udp_connect_send_recv() {
    let a: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let b_addr: SocketAddr = b.local_addr().unwrap();

    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let g: Slot<Vec<u8>> = got.clone();
    let a2: UdpSocket = a.clone();
    a.connect(
        &mut el,
        b_addr,
        move |el: &mut EventLoop, res: io::Result<()>| {
            res.unwrap();
            a2.send(
                el,
                b"connected udp".to_vec(),
                move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
                    res.unwrap();
                    b.recv(
                        el,
                        Vec::with_capacity(64),
                        move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
                            res.unwrap();
                            *g.borrow_mut() = Some(buf);
                        },
                    );
                },
            );
        },
    );
    el.run().unwrap();
    assert_eq!(got.take().unwrap(), b"connected udp");
}

#[test]
fn dropped_handle_stays_open_until_op_completes() {
    let socket: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr: SocketAddr = socket.local_addr().unwrap();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let g: Slot<Vec<u8>> = got.clone();
    socket.recv_from(
        &mut el,
        Vec::with_capacity(64),
        move |_: &mut EventLoop, res: io::Result<(usize, SocketAddr)>, buf: Vec<u8>| {
            res.unwrap();
            *g.borrow_mut() = Some(buf);
        },
    );
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
    let server_path: PathBuf = temp_path("srv.sock");
    let client_path: PathBuf = temp_path("cli.sock");
    let server: UnixDatagram = UnixDatagram::bind(&server_path).unwrap();
    let named: UnixDatagram = UnixDatagram::bind(&client_path).unwrap();
    let unnamed: UnixDatagram = UnixDatagram::unbound().unwrap();

    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Received = Rc::default();
    for _ in 0..2 {
        let g: Received = got.clone();
        server.recv_from(
            &mut el,
            Vec::with_capacity(64),
            move |_: &mut EventLoop, res: io::Result<(usize, SockAddr)>, buf: Vec<u8>| {
                let (_, from): (usize, SockAddr) = res.unwrap();
                let path: Option<PathBuf> = from.as_pathname().map(PathBuf::from);
                g.borrow_mut().push((buf, path, from.is_unnamed()));
            },
        );
    }
    named.send_to(
        &mut el,
        b"named".to_vec(),
        &server_path,
        |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            res.unwrap();
        },
    );
    unnamed.send_to(
        &mut el,
        b"unnamed".to_vec(),
        &server_path,
        |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            res.unwrap();
        },
    );
    el.run().unwrap();
    std::fs::remove_file(&server_path).unwrap();
    std::fs::remove_file(&client_path).unwrap();

    let mut got: Vec<Datagram> = got.take();
    got.sort();
    assert_eq!(got[0], (b"named".to_vec(), Some(client_path), false));
    assert_eq!(got[1], (b"unnamed".to_vec(), None, true));
}

#[test]
fn unix_datagram_pair_and_connect() {
    let (a, b): (UnixDatagram, UnixDatagram) = UnixDatagram::pair().unwrap();
    let path: PathBuf = temp_path("conn.sock");
    let target: UnixDatagram = UnixDatagram::bind(&path).unwrap();
    let sender: UnixDatagram = UnixDatagram::unbound().unwrap();

    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Rc<RefCell<Vec<Vec<u8>>>> = Rc::default();
    a.send(
        &mut el,
        b"pair".to_vec(),
        |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            res.unwrap();
        },
    );
    let g: Rc<RefCell<Vec<Vec<u8>>>> = got.clone();
    b.recv(
        &mut el,
        Vec::with_capacity(64),
        move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            res.unwrap();
            g.borrow_mut().push(buf);
        },
    );
    let s: UnixDatagram = sender.clone();
    sender.connect(
        &mut el,
        &path,
        move |el: &mut EventLoop, res: io::Result<()>| {
            res.unwrap();
            s.send(
                el,
                b"connected".to_vec(),
                |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
                    res.unwrap();
                },
            );
        },
    );
    let g: Rc<RefCell<Vec<Vec<u8>>>> = got.clone();
    target.recv(
        &mut el,
        Vec::with_capacity(64),
        move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            res.unwrap();
            g.borrow_mut().push(buf);
        },
    );
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();

    let mut got: Vec<Vec<u8>> = got.take();
    got.sort();
    assert_eq!(got, vec![b"connected".to_vec(), b"pair".to_vec()]);
}

#[test]
fn unix_stream_listener_and_connect() {
    let path: PathBuf = temp_path("stream.sock");
    let listener: UnixListener = UnixListener::bind(&path).unwrap();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();

    let g: Slot<Vec<u8>> = got.clone();
    listener.accept(
        &mut el,
        move |el: &mut EventLoop, res: io::Result<UnixStream>| {
            let conn: UnixStream = res.unwrap();
            conn.recv(
                el,
                Vec::with_capacity(64),
                move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
                    res.unwrap();
                    *g.borrow_mut() = Some(buf);
                },
            );
        },
    );
    UnixStream::connect(
        &mut el,
        &path,
        |el: &mut EventLoop, res: io::Result<UnixStream>| {
            let stream: UnixStream = res.unwrap();
            let s: UnixStream = stream.clone();
            stream.write_all(
                el,
                b"unix stream".to_vec(),
                move |el: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| {
                    res.unwrap();
                    s.close(el, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
                },
            );
        },
    );
    el.run().unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(got.take().unwrap(), b"unix stream");
}

#[test]
fn unix_stream_pair_and_connect_error() {
    let (a, b): (UnixStream, UnixStream) = UnixStream::pair().unwrap();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let got: Slot<Vec<u8>> = Rc::default();
    let err: Slot<ErrorKind> = Rc::new(RefCell::new(None));

    a.write_all(
        &mut el,
        b"pair".to_vec(),
        |_: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| res.unwrap(),
    );
    let g: Slot<Vec<u8>> = got.clone();
    b.recv(
        &mut el,
        Vec::with_capacity(64),
        move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            res.unwrap();
            *g.borrow_mut() = Some(buf);
        },
    );
    let e: Slot<ErrorKind> = err.clone();
    UnixStream::connect(
        &mut el,
        temp_path("nobody.sock"),
        move |_: &mut EventLoop, res: io::Result<UnixStream>| {
            *e.borrow_mut() = Some(res.unwrap_err().kind());
        },
    );
    el.run().unwrap();
    assert_eq!(got.take().unwrap(), b"pair");
    assert_eq!(*err.borrow(), Some(ErrorKind::NotFound));
}
