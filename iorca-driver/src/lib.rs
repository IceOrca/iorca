//! # iorca-driver
//!
//! A safe, ownership-based wrapper around one `io_uring` instance. This crate
//! holds **all** of iorca's `unsafe` code, so that `iorca` itself can be
//! `forbid(unsafe_code)`.
//!
//! ## Why the API is safe
//!
//! With io_uring the kernel reads and writes user memory *after* the submitting
//! call has returned. This crate keeps that sound without the borrow checker's
//! help:
//!
//! * [`Ring::push`] takes an [`Op`] **by value**, including its buffer, path,
//!   address and owned fds. The resources move into the ring and stay there
//!   while the kernel may use them.
//! * Resources are handed back only in a [`Completion`] from [`Ring::reap`],
//!   i.e. after the kernel has posted the CQE and is done with them.
//! * If the [`Ring`] is dropped with operations still in flight, their resources
//!   are leaked rather than freed, because the kernel might still touch them.
//!
//! Nothing in the public API exposes a raw pointer to the kernel.
//!
//! ## Unsafe code
//!
//! The crate is `#![deny(unsafe_code)]`. Exactly two statements opt out,
//! because io_uring cannot work without them:
//!
//! * pushing an SQE to the kernel (`Ring::flush_backlog` in `ring.rs`);
//! * taking ownership of a new fd the kernel returns (`adopt_fd` in
//!   `inflight.rs`).
//!
//! C structs shared with the kernel (`msghdr`, `statx`, socket addresses) are
//! built and decoded safely in `abi.rs`, with compile-time layout checks
//! against `libc`.
//!
//! Requires Linux 5.19 or newer (for `IORING_OP_SOCKET`).

#![deny(unsafe_code)]

mod abi;
mod fs;
mod inflight;
mod op;
mod ring;

pub use fs::{Metadata, OpenOptions};
pub use op::{Completion, Op, Output};
pub use ring::Ring;
pub use socket2::{Domain, Protocol, SockAddr, Type};
