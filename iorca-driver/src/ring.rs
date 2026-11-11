//! The ring: owns the `IoUring` and every in-flight operation's resources.

use std::collections::{HashMap, VecDeque};
use std::io;

use io_uring::{IoUring, cqueue, squeue};

use crate::inflight::{InFlight, prepare};
use crate::op::{Completion, Op};

/// One `io_uring` instance plus the resources of its in-flight operations.
pub struct Ring {
    ring: IoUring,
    in_flight: HashMap<u64, InFlight>,
    /// Entries waiting to be pushed into the submission queue.
    backlog: VecDeque<squeue::Entry>,
    next_id: u64,
}

impl Ring {
    /// Creates a ring whose submission queue has `entries` slots. This does not
    /// limit in-flight operations: extra submissions wait in a backlog.
    pub fn new(entries: u32) -> io::Result<Self> {
        Ok(Self {
            ring: IoUring::new(entries)?,
            in_flight: HashMap::new(),
            backlog: VecDeque::new(),
            next_id: 0,
        })
    }

    /// Number of operations pushed but not yet reaped.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Queues `op` and returns its id. Ids are never reused.
    ///
    /// Never fails: an error before submission (e.g. a path containing a NUL
    /// byte) is reported in the operation's [`Completion`].
    pub fn push(&mut self, op: Op) -> u64 {
        let id: u64 = self.next_id;
        self.next_id += 1;
        let (entry, resources): (squeue::Entry, InFlight) = prepare(op);
        self.in_flight.insert(id, resources);
        self.backlog.push_back(entry.user_data(id));
        id
    }

    /// Submits queued operations, then waits for at least one completion.
    /// It doesn't wait if operations are still queued for SQ space, or if
    /// nothing is in flight. A signal interruption returns `Ok`.
    pub fn submit_and_wait(&mut self) -> io::Result<()> {
        if self.in_flight.is_empty() {
            return Ok(());
        }
        self.flush_backlog();
        let want: usize = if self.backlog.is_empty() { 1 } else { 0 };
        match self.ring.submit_and_wait(want) {
            Ok(_) => Ok(()),
            // EINTR: interrupted by a signal. EBUSY: the completion queue
            // overflowed; reaping frees it up. Both are retried next call.
            Err(e) if matches!(e.raw_os_error(), Some(libc::EINTR | libc::EBUSY)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Appends every available completion to `out` without blocking.
    pub fn reap(&mut self, out: &mut Vec<Completion>) {
        for cqe in self.ring.completion() {
            let id: u64 = cqe.user_data();
            let res: i32 = cqe.result();
            if cqueue::more(cqe.flags()) {
                // Multishot: the kernel keeps using this op's resources.
                if let Some(resources) = self.in_flight.get(&id) {
                    out.push(Completion {
                        id,
                        output: resources.more(res),
                        more: true,
                    });
                }
            } else if let Some(resources) = self.in_flight.remove(&id) {
                out.push(Completion {
                    id,
                    output: resources.finish(res),
                    more: false,
                });
            }
        }
    }

    /// Moves queued entries, in order, into the submission queue until it is
    /// full. This is the only place pointers are handed to the kernel.
    fn flush_backlog(&mut self) {
        let mut sq: squeue::SubmissionQueue<'_> = self.ring.submission();
        while let Some(entry) = self.backlog.front() {
            // One of the driver's two `unsafe` sites: handing an SQE to the
            // kernel cannot be checked by the compiler.
            #[allow(unsafe_code)]
            // SAFETY: every pointer in `entry` was built by `prepare` and refers
            // to heap memory owned by the matching `InFlight` in
            // `self.in_flight`. That entry is removed only in `reap`, after the
            // kernel posted the op's final CQE (one without
            // `IORING_CQE_F_MORE`), and is leaked by `Drop` if the ring dies
            // first. So the memory outlives the kernel's use of it.
            let pushed: bool = unsafe { sq.push(entry) }.is_ok();
            if !pushed {
                break;
            }
            self.backlog.pop_front();
        }
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        if !self.in_flight.is_empty() {
            // The kernel may still read or write these buffers. Leak them
            // instead of freeing memory it might touch.
            std::mem::forget(std::mem::take(&mut self.in_flight));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::op::Output;
    use std::time::Duration;

    #[test]
    fn push_submit_reap() {
        let mut ring: Ring = Ring::new(4).unwrap();
        let id: u64 = ring.push(Op::Nop);
        assert_eq!(ring.in_flight(), 1);
        let mut out: Vec<Completion> = Vec::new();
        while out.is_empty() {
            ring.submit_and_wait().unwrap();
            ring.reap(&mut out);
        }
        assert_eq!(out[0].id, id);
        assert!(matches!(out[0].output, Output::Unit(Ok(()))));
        assert_eq!(ring.in_flight(), 0);
    }

    #[test]
    fn multishot_timeout_stays_in_flight_until_last_tick() {
        let mut ring: Ring = Ring::new(4).unwrap();
        let id: u64 = ring.push(Op::TimeoutMultishot {
            period: Duration::from_millis(1),
            count: 3,
        });
        let mut out: Vec<Completion> = Vec::new();
        while out.len() < 3 {
            ring.submit_and_wait().unwrap();
            ring.reap(&mut out);
            let last_more: bool = out.last().is_some_and(|c: &Completion| -> bool { c.more });
            assert_eq!(ring.in_flight(), usize::from(out.is_empty() || last_more));
        }
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(|c: &Completion| -> bool {
            c.id == id && matches!(c.output, Output::Unit(Ok(())))
        }));
        let more: Vec<bool> = out
            .iter()
            .map(|c: &Completion| -> bool { c.more })
            .collect::<Vec<bool>>();
        assert_eq!(more, vec![true, true, false]);
    }

    #[test]
    fn multishot_zero_period_is_rejected() {
        let mut ring: Ring = Ring::new(4).unwrap();
        ring.push(Op::TimeoutMultishot {
            period: Duration::ZERO,
            count: 0,
        });
        let mut out: Vec<Completion> = Vec::new();
        while out.is_empty() {
            ring.submit_and_wait().unwrap();
            ring.reap(&mut out);
        }
        assert!(!out[0].more);
        assert!(
            matches!(&out[0].output, Output::Unit(Err(e)) if e.kind() == io::ErrorKind::InvalidInput)
        );
        assert_eq!(ring.in_flight(), 0);
    }

    #[test]
    fn drop_with_pending_op_leaks_safely() {
        let mut ring: Ring = Ring::new(4).unwrap();
        ring.push(Op::Timeout(Duration::from_secs(60)));
        // The nop lets `submit_and_wait` return while the timer is in flight.
        ring.push(Op::Nop);
        ring.submit_and_wait().unwrap();
        drop(ring);
    }
}
