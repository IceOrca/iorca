use std::cell::{Cell, RefCell};
use std::io::{self, ErrorKind};
use std::net::UdpSocket;
use std::os::fd::{AsRawFd, RawFd};
use std::rc::Rc;
use std::time::{Duration, Instant};

use iorca::{EventLoop, OpId, Timer};

/// Result slot filled in by a callback and checked after `run()`.
type Slot<T> = Rc<RefCell<Option<T>>>;

/// A multishot tick as recorded by a test: `Ok(more)` or the raw errno.
type Tick = Result<bool, Option<i32>>;

#[test]
fn after_fires_once() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let fired: Rc<Cell<Option<Duration>>> = Rc::new(Cell::new(None));
    let f: Rc<Cell<Option<Duration>>> = fired.clone();
    let timer: Timer = Timer::after(
        &mut el,
        Duration::from_millis(30),
        move |_: &mut EventLoop| {
            f.set(Some(start.elapsed()));
        },
    );
    assert!(timer.is_active());
    el.run().unwrap();
    assert!(fired.get().expect("timer did not fire") >= Duration::from_millis(30));
    assert!(!timer.is_active());
    // Cancelling a fired timer is a no-op.
    assert!(!timer.cancel(&mut el));
    el.run().unwrap();
}

#[test]
fn at_fires_at_deadline() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let deadline: Instant = Instant::now() + Duration::from_millis(30);
    let fired_at: Rc<Cell<Option<Instant>>> = Rc::new(Cell::new(None));
    let f: Rc<Cell<Option<Instant>>> = fired_at.clone();
    Timer::at(&mut el, deadline, move |_: &mut EventLoop| {
        f.set(Some(Instant::now()))
    });
    // A deadline in the past fires right away.
    let past: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let p: Rc<Cell<bool>> = past.clone();
    Timer::at(&mut el, Instant::now(), move |_: &mut EventLoop| {
        p.set(true)
    });
    el.run().unwrap();
    assert!(fired_at.get().expect("timer did not fire") >= deadline);
    assert!(past.get());
}

#[test]
fn cancel_before_firing() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let fired: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let f: Rc<Cell<bool>> = fired.clone();
    let timer: Timer = Timer::after(
        &mut el,
        Duration::from_secs(10),
        move |_: &mut EventLoop| f.set(true),
    );
    assert!(timer.cancel(&mut el));
    assert!(!timer.cancel(&mut el));
    el.run().unwrap();
    assert!(!fired.get());
    assert!(!timer.is_active());
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn interval_ticks_until_cancelled_from_callback() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let ticks: Rc<RefCell<Vec<Duration>>> = Rc::new(RefCell::new(Vec::new()));
    let t: Rc<RefCell<Vec<Duration>>> = ticks.clone();
    let timer: Timer = Timer::interval(
        &mut el,
        Duration::from_millis(20),
        move |el: &mut EventLoop, timer: &Timer| {
            t.borrow_mut().push(start.elapsed());
            if t.borrow().len() == 5 {
                assert!(timer.cancel(el));
            }
        },
    );
    el.run().unwrap();
    assert!(!timer.is_active());
    let ticks: Vec<Duration> = ticks.take();
    assert_eq!(ticks.len(), 5);
    // Tick k is due at k * period from the start.
    for (k, at) in ticks.iter().enumerate() {
        let due: Duration = Duration::from_millis(20) * u32::try_from(k + 1).unwrap();
        assert!(*at >= due, "tick {k} at {at:?}, due {due:?}");
    }
}

#[test]
fn interval_cancelled_from_outside() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let ticks: Rc<Cell<u32>> = Rc::new(Cell::new(0));
    let t: Rc<Cell<u32>> = ticks.clone();
    let interval: Timer = Timer::interval(
        &mut el,
        Duration::from_millis(10),
        move |_: &mut EventLoop, _: &Timer| t.set(t.get() + 1),
    );
    let i: Timer = interval.clone();
    Timer::after(
        &mut el,
        Duration::from_millis(55),
        move |el: &mut EventLoop| {
            assert!(i.cancel(el));
        },
    );
    el.run().unwrap();
    assert!(!interval.is_active());
    assert!(ticks.get() >= 1);
    assert!(ticks.get() <= 5);
}

#[test]
fn interval_skips_missed_ticks() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let ticks: Rc<RefCell<Vec<Duration>>> = Rc::new(RefCell::new(Vec::new()));
    let t: Rc<RefCell<Vec<Duration>>> = ticks.clone();
    Timer::interval(
        &mut el,
        Duration::from_millis(10),
        move |el: &mut EventLoop, timer: &Timer| {
            t.borrow_mut().push(start.elapsed());
            if t.borrow().len() == 1 {
                // Block the loop through the ticks due at 20, 30 and 40ms.
                std::thread::sleep(Duration::from_millis(35));
            } else {
                timer.cancel(el);
            }
        },
    );
    el.run().unwrap();
    let ticks: Vec<Duration> = ticks.take();
    assert_eq!(ticks.len(), 2);
    // No catch-up burst: the second tick waits for the next slot (>= 50ms).
    assert!(
        ticks[1] >= Duration::from_millis(50),
        "second tick at {:?}",
        ticks[1]
    );
}

#[test]
fn deadline_times_out_pending_op() {
    // Nobody ever sends to this socket.
    let sock: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let fd: RawFd = sock.as_raw_fd();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let got: Slot<(ErrorKind, Option<i32>, usize)> = Rc::default();
    let g: Slot<(ErrorKind, Option<i32>, usize)> = got.clone();
    let id: OpId = el.recv(
        fd,
        Vec::with_capacity(64),
        move |_: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            let err: io::Error = res.unwrap_err();
            *g.borrow_mut() = Some((err.kind(), err.raw_os_error(), buf.capacity()));
        },
    );
    assert!(el.deadline(id, Duration::from_millis(30)));
    el.run().unwrap();
    let (kind, raw, cap): (ErrorKind, Option<i32>, usize) = got.take().unwrap();
    assert_eq!(kind, ErrorKind::TimedOut);
    assert_eq!(raw, Some(libc::ETIMEDOUT));
    assert!(cap >= 64, "buffer handed back");
    assert!(start.elapsed() >= Duration::from_millis(30));
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn deadline_is_disarmed_when_op_completes_first() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let done: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let d: Rc<Cell<bool>> = done.clone();
    let id: OpId = el.nop(move |_: &mut EventLoop, res: io::Result<()>| {
        res.unwrap();
        d.set(true);
    });
    assert!(el.deadline(id, Duration::from_secs(10)));
    el.run().unwrap();
    assert!(done.get());
    // `run` didn't wait for the 10s deadline timer.
    assert!(start.elapsed() < Duration::from_secs(5));
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn deadline_on_completed_op_is_rejected() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let id: OpId = el.nop(|_: &mut EventLoop, _: io::Result<()>| {});
    el.run().unwrap();
    assert!(!el.deadline(id, Duration::from_millis(10)));
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn user_cancel_still_reports_ecanceled() {
    // Cancelling an op yourself is not a timeout, even with a deadline set.
    let sock: UdpSocket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let mut el: EventLoop = EventLoop::new().unwrap();
    let err: Rc<Cell<Option<i32>>> = Rc::new(Cell::new(None));
    let e: Rc<Cell<Option<i32>>> = err.clone();
    let id: OpId = el.recv(
        sock.as_raw_fd(),
        Vec::with_capacity(8),
        move |_: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            e.set(res.unwrap_err().raw_os_error());
        },
    );
    assert!(el.deadline(id, Duration::from_secs(10)));
    el.cancel(id, |_: &mut EventLoop, res: io::Result<()>| res.unwrap());
    el.run().unwrap();
    assert_eq!(err.get(), Some(libc::ECANCELED));
}

#[test]
fn multishot_fires_count_times() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let start: Instant = Instant::now();
    let ticks: Rc<RefCell<Vec<bool>>> = Rc::new(RefCell::new(Vec::new()));
    let t: Rc<RefCell<Vec<bool>>> = ticks.clone();
    el.timeout_multishot(
        Duration::from_millis(10),
        3,
        move |_: &mut EventLoop, res: io::Result<bool>| t.borrow_mut().push(res.unwrap()),
    );
    el.run().unwrap();
    assert_eq!(*ticks.borrow(), vec![true, true, false]);
    assert!(start.elapsed() >= Duration::from_millis(30));
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn multishot_repeats_until_cancelled() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let results: Rc<RefCell<Vec<Tick>>> = Rc::new(RefCell::new(Vec::new()));
    let r: Rc<RefCell<Vec<Tick>>> = results.clone();
    let id: Rc<Cell<Option<OpId>>> = Rc::new(Cell::new(None));
    let i: Rc<Cell<Option<OpId>>> = id.clone();
    let op: OpId = el.timeout_multishot(
        Duration::from_millis(5),
        0,
        move |el: &mut EventLoop, res: io::Result<bool>| {
            r.borrow_mut()
                .push(res.map_err(|e: io::Error| -> Option<i32> { e.raw_os_error() }));
            if r.borrow().len() == 4 {
                el.cancel(
                    i.get().unwrap(),
                    |_: &mut EventLoop, res: io::Result<()>| res.unwrap(),
                );
            }
        },
    );
    id.set(Some(op));
    // A multishot op can't take a deadline.
    assert!(!el.deadline(op, Duration::from_secs(1)));
    el.run().unwrap();
    // The cancel may race one more tick; the op always ends with ECANCELED.
    let results: Vec<Tick> = results.take();
    let (last, ticks): (&Tick, &[Tick]) = results.split_last().unwrap();
    assert_eq!(*last, Err(Some(libc::ECANCELED)));
    assert!(ticks.len() >= 4);
    assert!(ticks.iter().all(|t: &Tick| -> bool { *t == Ok(true) }));
    assert_eq!(el.in_flight(), 0);
}

#[test]
fn multishot_zero_period_is_rejected() {
    let mut el: EventLoop = EventLoop::new().unwrap();
    let kinds: Rc<RefCell<Vec<ErrorKind>>> = Rc::new(RefCell::new(Vec::new()));
    let k: Rc<RefCell<Vec<ErrorKind>>> = kinds.clone();
    el.timeout_multishot(
        Duration::ZERO,
        0,
        move |_: &mut EventLoop, res: io::Result<bool>| {
            k.borrow_mut().push(res.unwrap_err().kind())
        },
    );
    el.run().unwrap();
    assert_eq!(*kinds.borrow(), vec![ErrorKind::InvalidInput]);
}
