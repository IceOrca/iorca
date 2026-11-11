//! Timers: an interval that ticks every 500ms and stops itself after five
//! ticks, a one-shot timer, a kernel multishot timeout (Linux ≥ 6.4) that
//! fires three times, and a deadline on a UDP receive nobody answers.

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

    let mut kernel_ticks: u32 = 0;
    el.timeout_multishot(
        Duration::from_millis(700),
        3,
        move |_: &mut EventLoop, res: io::Result<bool>| match res {
            Ok(more) => {
                kernel_ticks += 1;
                println!("multishot tick {kernel_ticks} (more: {more})");
            }
            Err(e) => println!("multishot timeout unavailable: {e}"),
        },
    );

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
