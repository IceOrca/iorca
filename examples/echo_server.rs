//! TCP echo server: accept → recv → write_all → recv ... built from callbacks.
//!
//! Run it, then try `nc 127.0.0.1 8080`.

use std::io;

use iorca::{EventLoop, TcpListener, TcpStream};

fn main() -> io::Result<()> {
    let addr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "127.0.0.1:8080".into());
    let listener = TcpListener::bind(addr)?;
    println!("listening on {}", listener.local_addr()?);

    let mut el = EventLoop::new()?;
    accept_loop(&mut el, listener);
    el.run()
}

fn accept_loop(el: &mut EventLoop, listener: TcpListener) {
    let next = listener.clone();
    listener.accept(el, move |el, res| {
        match res {
            Ok(stream) => echo(el, stream, Vec::with_capacity(4096)),
            Err(e) => eprintln!("accept error: {e}"),
        }
        // Re-arm so we keep accepting.
        accept_loop(el, next);
    });
}

fn echo(el: &mut EventLoop, stream: TcpStream, buf: Vec<u8>) {
    let s = stream.clone();
    stream.recv(el, buf, move |el, res, buf| match res {
        Ok(0) => {
            // Peer closed the connection.
            s.close(el, |_, _| {});
        }
        Ok(_) => {
            let next = s.clone();
            s.write_all(el, buf, move |el, res, buf| match res {
                Ok(()) => echo(el, next, buf),
                Err(e) => eprintln!("send error: {e}"),
            });
        }
        Err(e) => eprintln!("recv error: {e}"),
    });
}
