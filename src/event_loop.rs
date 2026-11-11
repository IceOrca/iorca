use std::cell::Cell;
use std::collections::HashMap;
use std::io;
use std::os::fd::OwnedFd;
use std::rc::Rc;
use std::time::Duration;

use iorca_driver::{Completion, Metadata, Op, Output, Ring, SockAddr};

/// Internal completion handler, invoked with the driver's typed [`Output`].
pub(crate) type Callback = Box<dyn FnOnce(&mut EventLoop, Output)>;

/// Identifies a submitted operation. Pass it to [`EventLoop::cancel`] to
/// cancel the operation.
///
/// Ids are never reused within one `EventLoop`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OpId(pub(crate) u64);

/// A single-threaded, callback-driven event loop backed by one `io_uring`.
pub struct EventLoop {
    /// Owns the ring and every resource the kernel can touch.
    ring: Ring,
    /// User callbacks for in-flight operations, keyed by op id.
    callbacks: HashMap<u64, Callback>,
    /// Scratch buffer for reaped completions, reused across turns.
    completed: Vec<Completion>,
    stopped: bool,
}

impl EventLoop {
    /// Submission queue size used by [`EventLoop::new`].
    pub const DEFAULT_ENTRIES: u32 = 256;

    /// Creates an event loop with [`Self::DEFAULT_ENTRIES`] submission entries.
    pub fn new() -> io::Result<Self> {
        Self::with_entries(Self::DEFAULT_ENTRIES)
    }

    /// Creates an event loop whose submission queue has `entries` slots
    /// (rounded up to a power of two by the kernel).
    ///
    /// This is not a limit on in-flight operations: when the queue is full,
    /// further submissions are buffered and pushed on the next turn.
    pub fn with_entries(entries: u32) -> io::Result<Self> {
        Ok(Self {
            ring: Ring::new(entries)?,
            callbacks: HashMap::new(),
            completed: Vec::new(),
            stopped: false,
        })
    }

    /// Number of submitted operations whose callback has not run yet.
    pub fn in_flight(&self) -> usize {
        self.callbacks.len()
    }

    /// Makes [`run`](Self::run) return after the current batch of completions
    /// has been dispatched. Pending operations stay pending; calling `run`
    /// again resumes them.
    pub fn stop(&mut self) {
        self.stopped = true;
    }

    /// Runs the loop until there are no operations in flight or
    /// [`stop`](Self::stop) is called.
    pub fn run(&mut self) -> io::Result<()> {
        self.stopped = false;
        while !self.stopped && !self.callbacks.is_empty() {
            self.ring.submit_and_wait()?;
            self.dispatch();
        }
        Ok(())
    }

    /// Cancels `op` if it has not completed within `after`.
    ///
    /// When the deadline passes first, `op`'s callback gets an error of kind
    /// [`ErrorKind::TimedOut`](io::ErrorKind::TimedOut) (`ETIMEDOUT`), with any
    /// buffer handed back as usual. When `op` completes first, the deadline
    /// timer is cancelled, so it does not keep [`run`](Self::run) waiting.
    ///
    /// Returns `false`, and does nothing, if `op` has already completed.
    ///
    /// The deadline applies to that one op: for chained helpers such as
    /// `write_all` or `tcp_connect`, only the first step is covered. An op the
    /// kernel cannot interrupt (e.g. regular file I/O already in progress)
    /// completes normally despite the deadline.
    pub fn deadline(&mut self, op: OpId, after: Duration) -> bool {
        let Some(cb): Option<Callback> = self.callbacks.remove(&op.0) else {
            return false;
        };
        let expired: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let e: Rc<Cell<bool>> = Rc::clone(&expired);
        let timer: OpId = self.timeout(after, move |el: &mut EventLoop, res: io::Result<()>| {
            if res.is_ok() {
                e.set(true);
                el.cancel(op, |_: &mut EventLoop, _: io::Result<()>| {});
            }
        });
        let wrapped: Callback = Box::new(move |el: &mut EventLoop, output: Output| {
            if expired.get() {
                cb(el, timed_out(output));
            } else {
                el.cancel(timer, |_: &mut EventLoop, _: io::Result<()>| {});
                cb(el, output);
            }
        });
        self.callbacks.insert(op.0, wrapped);
        true
    }

    fn dispatch(&mut self) {
        // Reap everything first: callbacks need `&mut self`, so they cannot run
        // while the ring is borrowed.
        let mut completed: Vec<Completion> = std::mem::take(&mut self.completed);
        self.ring.reap(&mut completed);
        // Every reaped completion must be delivered, even if a callback calls
        // `stop()`: the CQEs are already consumed.
        for completion in completed.drain(..) {
            if let Some(cb) = self.callbacks.remove(&completion.id) {
                cb(self, completion.output);
            }
        }
        self.completed = completed;
    }

    /// Hands `op` to the driver and registers `cb` for its completion.
    pub(crate) fn push(&mut self, op: Op, cb: Callback) -> OpId {
        let id: u64 = self.ring.push(op);
        self.callbacks.insert(id, cb);
        OpId(id)
    }
}

/// Turns the `ECANCELED` caused by an expired deadline into `ETIMEDOUT`.
/// Other results (the op won the race, or failed on its own) pass through.
fn timed_out(output: Output) -> Output {
    fn map<T>(res: io::Result<T>) -> io::Result<T> {
        match res {
            Err(e) if e.raw_os_error() == Some(libc::ECANCELED) => {
                Err(io::Error::from_raw_os_error(libc::ETIMEDOUT))
            }
            other => other,
        }
    }
    match output {
        Output::Unit(res) => Output::Unit(map::<()>(res)),
        Output::Bytes(res, buf) => Output::Bytes(map::<usize>(res), buf),
        Output::Datagram(res, buf) => Output::Datagram(map::<(usize, SockAddr)>(res), buf),
        Output::Fd(res) => Output::Fd(map::<OwnedFd>(res)),
        Output::Metadata(res) => Output::Metadata(map::<Metadata>(res)),
    }
}
