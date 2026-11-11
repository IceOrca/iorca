//! Prints a tick every 500ms, five times, by re-arming a timeout from its own
//! callback.

use std::io;
use std::time::Duration;

use iorca::EventLoop;

fn main() -> io::Result<()> {
    let mut el = EventLoop::new()?;
    tick(&mut el, 1);
    el.run()
}

fn tick(el: &mut EventLoop, n: u32) {
    el.timeout(Duration::from_millis(500), move |el, res| {
        res.expect("timeout failed");
        println!("tick {n}");
        if n < 5 {
            tick(el, n + 1);
        }
    });
}
