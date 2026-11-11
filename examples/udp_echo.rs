//! UDP echo server: recv_from → send_to back to the sender → recv_from ...
//!
//! Run it, then try `nc -u 127.0.0.1 9000`.

use std::io;

use iorca::{EventLoop, UdpSocket};

fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:9000".into());
    let socket = UdpSocket::bind(addr)?;
    println!("udp echo on {}", socket.local_addr()?);

    let mut el = EventLoop::new()?;
    serve(&mut el, socket, Vec::with_capacity(64 * 1024));
    el.run()
}

fn serve(el: &mut EventLoop, socket: UdpSocket, buf: Vec<u8>) {
    let s = socket.clone();
    socket.recv_from(el, buf, move |el, res, buf| match res {
        Ok((_, peer)) => {
            let next = s.clone();
            s.send_to(el, buf, peer, move |el, res, mut buf| {
                if let Err(e) = res {
                    eprintln!("send_to {peer} failed: {e}");
                }
                buf.clear();
                serve(el, next, buf);
            });
        }
        Err(e) => {
            eprintln!("recv_from error: {e}");
            serve(el, s, buf);
        }
    });
}
