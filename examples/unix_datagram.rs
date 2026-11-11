//! Unix datagrams: an unbound client sends three messages to a server socket
//! file, and the server prints each one with its sender.

use std::io;

use iorca::{EventLoop, UnixDatagram};

fn main() -> io::Result<()> {
    let path = std::env::temp_dir().join(format!("iorca-example-{}.sock", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let server = UnixDatagram::bind(&path)?;
    let client = UnixDatagram::unbound()?;

    let mut el = EventLoop::new()?;
    receive(&mut el, server, 3);
    for i in 1..=3 {
        let msg = format!("hello {i}").into_bytes();
        client.send_to(&mut el, msg, &path, |_, res, _| {
            res.expect("send_to failed");
        });
    }
    el.run()?;
    std::fs::remove_file(&path)
}

fn receive(el: &mut EventLoop, server: UnixDatagram, remaining: u32) {
    if remaining == 0 {
        return;
    }
    let next = server.clone();
    server.recv_from(el, Vec::with_capacity(1024), move |el, res, buf| {
        let (_, from) = res.expect("recv_from failed");
        let sender = if from.is_unnamed() {
            "an unnamed socket".to_string()
        } else {
            format!("{:?}", from.as_pathname())
        };
        println!("got {:?} from {sender}", String::from_utf8_lossy(&buf));
        receive(el, next, remaining - 1);
    });
}
