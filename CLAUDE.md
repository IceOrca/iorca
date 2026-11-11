# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

iorca is a Linux-only Rust library for completion-based, event-driven I/O on top of `io_uring` (via the `io-uring` crate), modelled loosely on tokio-uring but **without async/await**: each operation takes a callback that the event loop invokes on completion. Requires Linux ≥ 5.19 (`IORING_OP_SOCKET`).

## Commands

There is no host Rust toolchain; build and test inside the `dev` service in `compose.yaml` (image built from the `builder` stage of `Dockerfile`: `rust:1.98.1-slim-trixie` + clippy, rustfmt, cargo-sweep; project bind-mounted at `/workspace`, `target/` and the cargo registry in named volumes, seccomp unconfined because Docker's default profile blocks io_uring syscalls):

```sh
docker compose run --rm dev cargo test --workspace
docker compose run --rm dev cargo build --workspace --all-targets
docker compose run --rm dev cargo test --test net udp_       # one test file (ops, net, fs, handles) / name filter
docker compose run --rm dev cargo test -p iorca-driver       # driver unit tests only
docker compose run --rm dev cargo run --example timer        # Timer + deadline demo; also: cat -- <file>, echo_server [addr], udp_echo [addr], unix_datagram
docker compose run --rm dev cargo clippy --workspace --all-targets -- -D warnings
docker compose run --rm dev cargo fmt --all --check
```

## Coding rules

- **Explicit types everywhere.** Annotate every `let` (`let n: usize = ...`, `let _: T = ...`), every closure parameter (`|el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>|`), closure return types unless `()`, turbofish otherwise-inferred generics (`.collect::<Vec<u8>>()`), and `T::from(x)` instead of `x.into()`. Only skip where Rust's grammar forbids it: `for` loop variables, `match` / `if let` / `while let` bindings and destructured parts. No lint enforces this; check new code by eye.

## Architecture

Cargo workspace with two crates. All `unsafe` code is confined to `iorca-driver`, which is `#![deny(unsafe_code)]` with exactly two `#[allow(unsafe_code)]` sites: the SQ push in `Ring::flush_backlog` and `adopt_fd` (raw fd → `OwnedFd`). Both are inherent to io_uring. `iorca` sets `unsafe_code = "forbid"` in its `Cargo.toml`.

- **`iorca-driver/`**: a safe, ownership-based io_uring wrapper, analogous to tokio-uring's `driver` module.
  - `op.rs`: public `Op` enum (taken **by value**: `Vec<u8>` buffers, `OwnedFd`s, `SockAddr`, `PathBuf`) and `Output` (`Unit`, `Bytes(res, buf)`, `Datagram(res<(n, SockAddr)>, buf)`, `Fd(res)`, `Metadata(res)`). Each `Op` variant documents its `Output` variant.
  - `inflight.rs`: `prepare()` turns an `Op` into an SQE plus an `InFlight` holding every resource the SQE points into; `InFlight::finish()` converts the CQE result and hands resources back. `sendmsg`/`recvmsg` use a boxed `MsgState<A>` (msghdr + iovec + address `A`: the `SockAddr` for send, an `AddrBuf` for recv). Pre-submission failures (NUL in path, bad `OpenOptions`) become `InFlight::Ready(output)` sent as a `Nop`.
  - `ring.rs`: `Ring` (`push`, `submit_and_wait`, `reap`). SQEs wait in a `backlog` and reach the kernel only in `flush_backlog`. Multishot: a CQE flagged `IORING_CQE_F_MORE` yields a `Completion { more: true }` via `InFlight::more(&self)` and leaves the `InFlight` in the map; only the final CQE removes it. `Op::TimeoutMultishot` (Linux ≥ 6.4) is currently the only multishot op.
  - `abi.rs`: C structs shared with the kernel, built and decoded **without unsafe**: `MsgHdr` (own `#[repr(C)]` mirror of the kernel's msghdr), `StatxBuf` and `AddrBuf` (aligned zeroed byte buffers read at `offset_of!` positions; `AddrBuf::decode` handles IPv4/IPv6/Unix and returns `Unsupported` for other families). Compile-time asserts check sizes, alignment and offsets against `libc`, and `statx_getters!` uses a `const` fn-pointer closure so a wrong field type fails to compile.
  - `fs.rs`: `OpenOptions` (std-compatible flag validation, always `O_CLOEXEC`) and `Metadata` (from `statx`). No unsafe.
  - Re-exports socket2's `Domain`, `Type`, `Protocol`, `SockAddr`; `iorca` re-exports these plus `OpenOptions`/`Metadata`.
- **`src/event_loop.rs`**: `EventLoop` = `Ring` + `HashMap<u64, Handler>`, where `Handler` is `Once(Callback)` (`Box<dyn FnOnce(&mut EventLoop, Output)>`) or `Multi(MultiCallback)` (`Box<dyn FnMut(&mut EventLoop, Output, bool)>`, re-inserted after each call while `more` is true; registered via `push_multi`). `run()` loops `submit_and_wait → dispatch` until no callbacks remain or `stop()` is called. `dispatch` reaps into a scratch vec before invoking callbacks, because callbacks get `&mut EventLoop` and may submit more ops. Op ids are monotonic and never reused, so `OpId`s are safe to cancel.
- **`EventLoop::deadline(op, after)`** (in `event_loop.rs`): swaps `op`'s callback for a wrapper and arms a timeout. If the timeout fires first it cancels `op`, and the wrapper turns the resulting `ECANCELED` into `ETIMEDOUT` (`ErrorKind::TimedOut`). If `op` completes first, the wrapper cancels the timeout so `run()` doesn't wait on it.
- **`src/ops.rs`**: one raw, fd-based method per op: `nop`, `timeout`, `timeout_multishot` (opt-in, callback gets `io::Result<bool>` = more), `cancel`, `read`, `write`, `close`, `socket`, `connect(fd, addr)`, `accept`, `recv`, `send`, `send_to`, `recv_from`, `open`, `fsync`, `fdatasync`, `statx`, `statx_path`, plus `tcp_connect(addr)` (socket + connect chained). Each wraps the user callback in a helper (`unit_cb`, `bytes_cb`, `fd_cb`, `metadata_cb`) that unpacks the expected `Output` variant.
- **Typed handles** (the recommended API, re-exported at the crate root), built only on the raw ops above:
  - `src/io.rs`: `Stdin`/`Stdout`/`Stderr` (`stdin()`, ...) plus the `pub(crate)` helpers every handle uses: `read`/`write`/`recv`/`send` (keep-alive wrappers), `write_all` (resubmits short writes; `WriteMode::{At, Current, Send}`), `close`, `fail`.
  - `src/filesystem.rs`: `File` (`open`, `create`, `open_with`, `read_at`, `write_at`, `write_all_at`, `sync_all`, `sync_data`, `metadata`, `close`) and the free fn `metadata(path)`.
  - `src/timer.rs`: `Timer` (`after`, `at`, `interval`, `cancel`, `is_active`), built on the raw `timeout` op. `Rc<State>` holds the pending timeout's `OpId` and an `active` flag. Intervals schedule tick k at `start + k*period` (no drift) and skip missed ticks instead of bursting. Dropping a `Timer` does not cancel it.
  - `src/network.rs`: `TcpListener`, `TcpStream`, `UdpSocket`, `UnixListener`, `UnixStream`, `UnixDatagram`, generated by the `socket_handle!` / `connected_io!` / `stream_io!` macros plus per-type methods. `bind`/`pair`/`unbound` are synchronous via std (io_uring has no bind before 6.11); connect, accept and all I/O go through the loop.
  - **Fd lifetime:** each handle wraps `Rc<std type>`, and every op clones that `Rc` into its callback (`let keep = Rc::clone(..)`, dropped in the callback). So the fd stays open until the kernel completes, even if the user drops the handle. `close(self)` closes asynchronously only when it holds the last reference; otherwise it just drops its reference. `as_std()` exposes the std type for synchronous getters/setters. New handle methods must follow this keep-alive pattern.

### Adding an operation

Add a variant to `Op` (`op.rs`) and `InFlight` if it needs new resources, build its SQE in `prepare()` (`inflight.rs`), convert the result in `InFlight::finish()`, then add the `EventLoop` method in `src/ops.rs`. Invariant for the driver: every pointer in an SQE must target heap memory (`Vec` contents, a `CString`, or a `Box` created *before* taking the pointer) owned by that op's `InFlight`, which stays in `Ring::in_flight` until its CQE is reaped. `Drop for Ring` deliberately leaks pending `InFlight`s because the kernel may still access them. Prefer safe workarounds over new `unsafe`: e.g. `fill()` zero-fills read buffers and truncates instead of using `set_len`. Don't add `unsafe` for C structs: add a layout-checked type to `abi.rs` instead (own `#[repr(C)]` mirror or byte buffer + `offset_of!`, with `const` asserts against the `libc` type). Any `unsafe` needs `#[allow(unsafe_code)]` and a `SAFETY:` comment (clippy's `undocumented_unsafe_blocks` is denied in the driver).

Buffer conventions: read-like ops (`read`, `recv`) fill from the start up to `capacity()`, then the buffer is truncated to `n` (restored to its original length on error); write-like ops (`write`, `send`) use `buf[..len()]` and may complete short. Both return the `Vec`. Raw ops take `RawFd`, and callers keep the fd open themselves (typed handles do this for them). Errors before submission come back through the normal completion (via a `Nop` SQE), never synchronously.
