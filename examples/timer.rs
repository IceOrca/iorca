//! Timers: an interval that ticks every 500ms and stops itself after five
//! ticks, a one-shot timer, and a deadline on a UDP receive nobody answers.

use std::io::{self, ErrorKind};
use std::time::Duration;

use iorca::{EventLoop, OpId, Timer, UdpSocket};

fn main() -> io::Result<()> {
    let mut el: EventLoop = EventLoop::new()?;

    let mut n: u32 = 0;
    Timer::interval(
        &mut el,
        Duration::from_millis(500),
        move |el: &mut EventLoop, timer: &Timer| {
            n += 1;
            println!("tick {n}");
            if n == 5 {
                timer.cancel(el);
            }
        },
    );

    Timer::after(&mut el, Duration::from_millis(1200), |_: &mut EventLoop| {
        println!("one-shot fired after 1.2s");
    });

    let sock: UdpSocket = UdpSocket::bind("127.0.0.1:0")?;
    let recv: OpId = sock.recv_from(
        &mut el,
        Vec::with_capacity(1500),
        |_: &mut EventLoop, res: io::Result<(usize, std::net::SocketAddr)>, _: Vec<u8>| match res {
            Err(e) if e.kind() == ErrorKind::TimedOut => println!("recv timed out after 2s"),
            other => println!("recv: {other:?}"),
        },
    );
    el.deadline(recv, Duration::from_secs(2));

    el.run()
}
