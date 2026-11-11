//! One-shot and repeating timers, built on the one-shot
//! [`EventLoop::timeout`] op.

use std::cell::Cell;
use std::io;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::event_loop::{EventLoop, OpId};

/// Tick handler of an interval timer.
type Tick = Box<dyn FnMut(&mut EventLoop, &Timer)>;

/// A one-shot or repeating timer driven by an [`EventLoop`].
///
/// Clones refer to the same timer. Dropping the handle does **not** stop the
/// timer; call [`cancel`](Self::cancel) for that. A cancelled timer never
/// invokes its callback again.
///
/// ```no_run
/// use std::time::Duration;
/// use iorca::{EventLoop, Timer};
///
/// let mut el: EventLoop = EventLoop::new().unwrap();
/// let mut ticks: u32 = 0;
/// Timer::interval(&mut el, Duration::from_millis(100), move |el: &mut EventLoop, timer: &Timer| {
///     ticks += 1;
///     if ticks == 3 {
///         timer.cancel(el);
///     }
/// });
/// el.run().unwrap();
/// ```
#[derive(Clone, Debug)]
pub struct Timer {
    state: Rc<State>,
}

#[derive(Debug)]
struct State {
    /// The armed timeout op, if any.
    pending: Cell<Option<OpId>>,
    /// False once a one-shot timer fired or any timer was cancelled.
    active: Cell<bool>,
}

impl Timer {
    fn new() -> Self {
        Self {
            state: Rc::new(State {
                pending: Cell::new(None),
                active: Cell::new(true),
            }),
        }
    }

    /// Calls `cb` once, after `delay` has elapsed.
    pub fn after<F>(el: &mut EventLoop, delay: Duration, cb: F) -> Timer
    where
        F: FnOnce(&mut EventLoop) + 'static,
    {
        let timer: Timer = Timer::new();
        let t: Timer = timer.clone();
        let id: OpId = el.timeout(delay, move |el: &mut EventLoop, res: io::Result<()>| {
            t.state.pending.set(None);
            let was_active: bool = t.state.active.replace(false);
            if res.is_ok() && was_active {
                cb(el);
            }
        });
        timer.state.pending.set(Some(id));
        timer
    }

    /// Calls `cb` once, at `deadline`. A deadline in the past fires on the
    /// next loop turn.
    pub fn at<F>(el: &mut EventLoop, deadline: Instant, cb: F) -> Timer
    where
        F: FnOnce(&mut EventLoop) + 'static,
    {
        Timer::after(el, deadline.saturating_duration_since(Instant::now()), cb)
    }

    /// Calls `cb` every `period`, starting one `period` from now, until the
    /// timer is cancelled (from `cb` via its `&Timer` argument, or from
    /// anywhere else).
    ///
    /// Ticks are scheduled from the start time, not from the previous tick, so
    /// they don't drift. If ticks are missed because the loop was busy, the
    /// timer fires once and skips ahead to the next future tick instead of
    /// firing a burst.
    ///
    /// # Panics
    ///
    /// Panics if `period` is zero.
    pub fn interval<F>(el: &mut EventLoop, period: Duration, cb: F) -> Timer
    where
        F: FnMut(&mut EventLoop, &Timer) + 'static,
    {
        assert!(!period.is_zero(), "interval period must be non-zero");
        let timer: Timer = Timer::new();
        let first: Instant = Instant::now() + period;
        arm_tick(el, timer.clone(), first, period, Box::new(cb));
        timer
    }

    /// Stops the timer. Its callback will not run again.
    ///
    /// Returns `true` if the timer was still active.
    pub fn cancel(&self, el: &mut EventLoop) -> bool {
        let was_active: bool = self.state.active.replace(false);
        if let Some(id) = self.state.pending.take() {
            // The timeout's own callback sees ECANCELED and does nothing. If it
            // fired in the same batch, this cancel finds nothing: also fine.
            el.cancel(id, |_: &mut EventLoop, _: io::Result<()>| {});
        }
        was_active
    }

    /// True until a one-shot timer has fired or the timer is cancelled.
    pub fn is_active(&self) -> bool {
        self.state.active.get()
    }
}

/// Arms one tick of an interval timer, due at `deadline`.
fn arm_tick(el: &mut EventLoop, timer: Timer, deadline: Instant, period: Duration, mut cb: Tick) {
    let delay: Duration = deadline.saturating_duration_since(Instant::now());
    let t: Timer = timer.clone();
    let id: OpId = el.timeout(delay, move |el: &mut EventLoop, res: io::Result<()>| {
        t.state.pending.set(None);
        if res.is_err() || !t.state.active.get() {
            t.state.active.set(false);
            return;
        }
        cb(el, &t);
        if t.state.active.get() {
            let next: Instant = next_deadline(deadline, period, Instant::now());
            arm_tick(el, t, next, period, cb);
        }
    });
    timer.state.pending.set(Some(id));
}

/// The first tick `deadline + k * period` (k ≥ 1) that is after `now`.
fn next_deadline(deadline: Instant, period: Duration, now: Instant) -> Instant {
    let next: Instant = deadline + period;
    if next > now {
        return next;
    }
    let behind: Duration = now - deadline;
    let missed: u32 = u32::try_from(behind.as_nanos() / period.as_nanos()).unwrap_or(u32::MAX);
    deadline
        .checked_add(period.saturating_mul(missed.saturating_add(1)))
        .unwrap_or(now + period)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_deadline_skips_missed_ticks() {
        let t0: Instant = Instant::now();
        fn ms(n: u64) -> Duration {
            Duration::from_millis(n)
        }
        let p: Duration = ms(10);
        // On time: the next tick.
        assert_eq!(next_deadline(t0, p, t0 + ms(3)), t0 + ms(10));
        // Exactly on the next tick: that tick is not in the future, skip it.
        assert_eq!(next_deadline(t0, p, t0 + ms(10)), t0 + ms(20));
        // 35ms late: ticks at 10, 20 and 30 are skipped.
        assert_eq!(next_deadline(t0, p, t0 + ms(35)), t0 + ms(40));
    }
}
