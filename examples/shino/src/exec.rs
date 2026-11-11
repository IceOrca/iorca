//! Running parsed lines: `;` / `&&` / `||` sequencing, pipelines of external
//! commands and builtins, redirections and here-documents.
//!
//! A pipeline becomes a [`Job`]. External commands are spawned with
//! `std::process` (pipes between them are plain OS pipes the shell never
//! touches). Everything the shell itself writes (builtin output and
//! here-document bodies) goes through iorca. The job finishes once every
//! child has been reaped (on SIGCHLD) and every shell-side write completed.

use std::collections::VecDeque;
use std::fs;
use std::io::{self, ErrorKind};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net;
use std::os::unix::process::ExitStatusExt;
use std::process::{self, Child, ExitStatus, Stdio};
use std::rc::Rc;

use iorca::{EventLoop, File, UnixStream};
use rustix::process::{Pid, Signal};
use signal_hook::consts::{
    SIGABRT, SIGBUS, SIGFPE, SIGHUP, SIGILL, SIGINT, SIGKILL, SIGPIPE, SIGQUIT, SIGSEGV, SIGTERM,
};

use crate::builtins::{self, Outcome};
use crate::parse::{self, Connector, Env, Pipeline, Redirect, RedirectKind};
use crate::shell::{self, Done, Out, Sh, os_msg};

/// Receives a finished pipeline's status and whether it was interrupted
/// (a command died of SIGINT).
type JobDone = Box<dyn FnOnce(&mut EventLoop, i32, bool)>;

/// A running pipeline.
pub struct Job {
    /// Children not reaped yet.
    children: Vec<Child>,
    /// The last command's pid, if it is an external command.
    last_pid: Option<u32>,
    /// The last command's status, once known.
    status: Option<i32>,
    /// Shell-side writes (builtin output, here-documents) in flight.
    pending: usize,
    /// Reports for commands killed by a signal ("Aborted (core dumped)").
    messages: String,
    /// Some command died of SIGINT.
    sigint: bool,
    done: Option<JobDone>,
}

impl Job {
    fn new(done: JobDone) -> Self {
        Self {
            children: Vec::new(),
            last_pid: None,
            status: None,
            pending: 0,
            messages: String::new(),
            sigint: false,
            done: Some(done),
        }
    }

    fn record(&mut self, pid: u32, status: ExitStatus) {
        let (code, message): (i32, Option<String>) = describe(status);
        if status.signal() == Some(SIGINT) {
            self.sigint = true;
        }
        if let Some(message) = message {
            self.messages.push_str(&message);
            self.messages.push('\n');
        }
        if self.last_pid == Some(pid) {
            self.status = Some(code);
        }
    }
}

/// Exit code (128 + signal for signal deaths) and, for signals other than
/// SIGINT and SIGPIPE, a bash-style report.
fn describe(status: ExitStatus) -> (i32, Option<String>) {
    if let Some(code) = status.code() {
        return (code, None);
    }
    let Some(sig) = status.signal() else {
        return (1, None);
    };
    let name: String = match sig {
        SIGINT | SIGPIPE => return (128 + sig, None),
        SIGTERM => String::from("Terminated"),
        SIGKILL => String::from("Killed"),
        SIGABRT => String::from("Aborted"),
        SIGQUIT => String::from("Quit"),
        SIGSEGV => String::from("Segmentation fault"),
        SIGHUP => String::from("Hangup"),
        SIGBUS => String::from("Bus error"),
        SIGFPE => String::from("Floating point exception"),
        SIGILL => String::from("Illegal instruction"),
        other => format!("Killed by signal {other}"),
    };
    let core: &str = if status.core_dumped() {
        " (core dumped)"
    } else {
        ""
    };
    (128 + sig, Some(format!("{name}{core}")))
}

// ---- lists ----

/// Runs the pipelines of a line in order, honouring `;`, `&&` and `||`.
/// Ctrl+C (or a command dying of SIGINT) aborts the rest of the line.
pub fn run_list(
    el: &mut EventLoop,
    sh: &Sh,
    mut items: VecDeque<(Connector, Pipeline)>,
    then: Done,
) {
    while let Some((connector, pipeline)) = items.pop_front() {
        let status: i32 = sh.borrow().status;
        let run: bool = match connector {
            Connector::Seq => true,
            Connector::And => status == 0,
            Connector::Or => status != 0,
        };
        if !run {
            continue;
        }
        let sh2: Sh = Rc::clone(sh);
        let on_done: JobDone = Box::new(move |el: &mut EventLoop, status: i32, sigint: bool| {
            let (seen, exiting, interactive): (bool, bool, bool) = {
                let mut s: std::cell::RefMut<'_, shell::Shell> = sh2.borrow_mut();
                s.status = status;
                (std::mem::take(&mut s.interrupted), s.exiting, s.interactive)
            };
            let interrupted: bool = seen || sigint;
            if sigint && !seen {
                shell::expect_sigint(el, &sh2);
            }
            if interrupted && !interactive {
                // A script stops on Ctrl+C, like sh.
                shell::shutdown(el, &sh2, 130);
            } else if interrupted || exiting {
                then(el);
            } else {
                run_list(el, &sh2, items, then);
            }
        });
        let sh3: Sh = Rc::clone(sh);
        shell::after_output(
            el,
            sh,
            Box::new(move |el: &mut EventLoop| run_pipeline(el, &sh3, pipeline, on_done)),
        );
        return;
    }
    then(el);
}

// ---- pipelines ----

/// Where one of a command's fds 0-2 points.
enum Slot {
    /// The shell's own fd N: inherited by children, the console for builtins.
    Shell(u8),
    /// A file or pipe end the shell opened.
    Fd(OwnedFd, Kind),
    /// A here-document body, fed to the command through a socket pair.
    Heredoc(String),
}

/// How the shell writes to an [`Slot::Fd`] (for builtin output).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// A regular file: `File::write_all_at` (appends if opened with `>>`).
    File,
    /// A socket: `UnixStream::write_all`.
    Socket,
    /// An OS pipe between two external commands; the shell never writes it.
    Pipe,
}

/// A command's fds 0, 1 and 2 after redirection.
struct Fds {
    slots: [Slot; 3],
    /// `dup_of[n] == Some(m)`: fd n was last set by `n>&m`.
    dup_of: [Option<u8>; 3],
}

impl Fds {
    fn new() -> Self {
        Self {
            slots: [Slot::Shell(0), Slot::Shell(1), Slot::Shell(2)],
            dup_of: [None; 3],
        }
    }

    /// Applies redirections left to right, opening files synchronously (the
    /// fds go to a child or to iorca writes).
    fn apply(&mut self, redirects: &[Redirect], env: &dyn Env) -> Result<(), String> {
        for redirect in redirects {
            let fd: usize = usize::from(redirect.fd);
            let slot: Slot = match &redirect.kind {
                RedirectKind::Read(word) => {
                    let path: String = target_path(word, env)?;
                    let file: fs::File = fs::File::open(&path)
                        .map_err(|e: io::Error| -> String { format!("{path}: {}", os_msg(&e)) })?;
                    Slot::Fd(OwnedFd::from(file), Kind::File)
                }
                RedirectKind::Write(word) | RedirectKind::Append(word) => {
                    let append: bool = matches!(redirect.kind, RedirectKind::Append(_));
                    let path: String = target_path(word, env)?;
                    let file: fs::File = fs::OpenOptions::new()
                        .write(true)
                        .create(true)
                        .append(append)
                        .truncate(!append)
                        .open(&path)
                        .map_err(|e: io::Error| -> String { format!("{path}: {}", os_msg(&e)) })?;
                    Slot::Fd(OwnedFd::from(file), Kind::File)
                }
                RedirectKind::Heredoc(heredoc) => Slot::Heredoc(if heredoc.expand {
                    parse::expand_text(&heredoc.body, env)
                } else {
                    heredoc.body.clone()
                }),
                RedirectKind::Dup(to) => {
                    let copy: Slot = match &self.slots[usize::from(*to)] {
                        Slot::Shell(n) => Slot::Shell(*n),
                        Slot::Fd(owned, kind) => Slot::Fd(
                            owned.try_clone().map_err(|e: io::Error| -> String {
                                format!("{fd}>&{to}: {}", os_msg(&e))
                            })?,
                            *kind,
                        ),
                        Slot::Heredoc(body) => Slot::Heredoc(body.clone()),
                    };
                    self.slots[fd] = copy;
                    self.dup_of[fd] = Some(*to);
                    continue;
                }
            };
            self.slots[fd] = slot;
            self.dup_of[fd] = None;
        }
        Ok(())
    }
}

/// Splits leading `NAME=value` words off a command's argv.
fn split_assignments(words: Vec<String>) -> (Vec<(String, String)>, Vec<String>) {
    let mut assigns: Vec<(String, String)> = Vec::new();
    let mut rest: std::vec::IntoIter<String> = words.into_iter();
    for word in rest.by_ref() {
        match word.split_once('=') {
            Some((name, value)) if builtins::valid_name(name) => {
                assigns.push((String::from(name), String::from(value)));
            }
            _ => {
                return (
                    assigns,
                    std::iter::once(word).chain(rest).collect::<Vec<String>>(),
                );
            }
        }
    }
    (assigns, Vec::new())
}

fn target_path(word: &parse::Word, env: &dyn Env) -> Result<String, String> {
    let path: String = parse::expand(word, env);
    if path.is_empty() {
        return Err(String::from("ambiguous redirect"));
    }
    Ok(path)
}

/// A pipe from one command to the next: a socket pair when the writer is a
/// builtin (the shell writes it through iorca), an OS pipe otherwise.
fn pipe(writer_is_builtin: bool) -> io::Result<(Slot, Slot)> {
    if writer_is_builtin {
        let (read, write): (net::UnixStream, net::UnixStream) = net::UnixStream::pair()?;
        Ok((
            Slot::Fd(OwnedFd::from(read), Kind::Socket),
            Slot::Fd(OwnedFd::from(write), Kind::Socket),
        ))
    } else {
        let (read, write): (io::PipeReader, io::PipeWriter) = io::pipe()?;
        Ok((
            Slot::Fd(OwnedFd::from(read), Kind::Pipe),
            Slot::Fd(OwnedFd::from(write), Kind::Pipe),
        ))
    }
}

/// Starts a pipeline; `done` runs when all of it has finished.
fn run_pipeline(el: &mut EventLoop, sh: &Sh, pipeline: Pipeline, done: JobDone) {
    let count: usize = pipeline.commands.len();
    let mut job: Job = Job::new(done);
    let mut next_stdin: Option<Slot> = None;
    for (i, command) in pipeline.commands.into_iter().enumerate() {
        let last: bool = i + 1 == count;
        let words: Vec<String> = parse::expand_argv(&command.words, &*sh.borrow());
        let (assigns, argv): (Vec<(String, String)>, Vec<String>) = split_assignments(words);
        let builtin: bool = argv
            .first()
            .is_some_and(|name: &String| -> bool { builtins::is_builtin(name) });
        let mut fds: Fds = Fds::new();
        if let Some(slot) = next_stdin.take() {
            fds.slots[0] = slot;
        }
        if !last {
            match pipe(builtin) {
                Ok((read, write)) => {
                    fds.slots[1] = write;
                    next_stdin = Some(read);
                }
                Err(e) => {
                    shell::error(el, sh, &format!("pipe: {}", os_msg(&e)));
                    job.status = Some(1);
                    break;
                }
            }
        }
        let applied: Result<(), String> = fds.apply(&command.redirects, &*sh.borrow());
        let status: Option<i32> = match applied {
            Err(msg) => {
                shell::error(el, sh, &msg);
                Some(1)
            }
            // Builtins that change the shell (cd, exit, export, ...) only
            // take effect when they are the whole pipeline.
            // `NAME=value` alone sets a shell variable (exported to children).
            Ok(()) if argv.is_empty() && !assigns.is_empty() => {
                if count == 1 {
                    let mut s: std::cell::RefMut<'_, shell::Shell> = sh.borrow_mut();
                    for (name, value) in assigns {
                        s.vars.insert(name, Some(value));
                    }
                }
                Some(0)
            }
            Ok(()) if builtin => Some(run_builtin(el, sh, &argv, fds, count == 1, &mut job)),
            Ok(()) => spawn(el, sh, &argv, &assigns, fds, last, &mut job),
        };
        if last {
            job.status = status;
        }
    }
    sh.borrow_mut().job = Some(job);
    // Finishes right away if nothing is running (builtins without output).
    let sh2: Sh = Rc::clone(sh);
    el.nop(move |el: &mut EventLoop, _: io::Result<()>| maybe_finish(el, &sh2));
}

fn run_builtin(
    el: &mut EventLoop,
    sh: &Sh,
    argv: &[String],
    fds: Fds,
    effects: bool,
    job: &mut Job,
) -> i32 {
    let outcome: Outcome = builtins::run(sh, argv, effects);
    let Fds { slots, dup_of } = fds;
    let [_stdin, out_slot, err_slot]: [Slot; 3] = slots;
    let mut out: Vec<u8> = outcome.out;
    let mut err: Vec<u8> = outcome.err;
    if dup_of[2] == Some(1) {
        out.append(&mut err);
    } else if dup_of[1] == Some(2) {
        out.append(&mut err);
        err = std::mem::take(&mut out);
    }
    write_slot(el, sh, out_slot, out, job);
    write_slot(el, sh, err_slot, err, job);
    if let Some(code) = outcome.exit {
        shell::shutdown(el, sh, code);
    }
    outcome.status
}

/// Writes `bytes` to `slot` through iorca; the job waits for it. Dropping the
/// slot afterwards closes it, so a reader sees end of file.
fn write_slot(el: &mut EventLoop, sh: &Sh, slot: Slot, bytes: Vec<u8>, job: &mut Job) {
    if bytes.is_empty() {
        return;
    }
    let sh2: Sh = Rc::clone(sh);
    let finished = move |el: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| {
        // A reader that exits early (`history | head -1`) is not an error.
        if let Err(e) = res
            && e.kind() != ErrorKind::BrokenPipe
        {
            shell::error(el, &sh2, &format!("write error: {}", os_msg(&e)));
        }
        write_done(el, &sh2);
    };
    match slot {
        Slot::Shell(fd) => {
            let out: Out = if fd == 2 { Out::Stderr } else { Out::Stdout };
            let sh3: Sh = Rc::clone(sh);
            shell::print(
                el,
                sh,
                out,
                bytes,
                Some(Box::new(move |el: &mut EventLoop| write_done(el, &sh3))),
            );
        }
        Slot::Fd(owned, Kind::File) => File::from(owned).write_all_at(el, bytes, 0, finished),
        Slot::Fd(owned, Kind::Socket) => UnixStream::from(owned).write_all(el, bytes, finished),
        Slot::Fd(_, Kind::Pipe) | Slot::Heredoc(_) => return,
    }
    job.pending += 1;
}

fn write_done(el: &mut EventLoop, sh: &Sh) {
    if let Some(job) = sh.borrow_mut().job.as_mut() {
        job.pending -= 1;
    }
    maybe_finish(el, sh);
}

/// Spawns an external command. Returns its status if it is already known
/// (it failed to start), `None` if it is running.
fn spawn(
    el: &mut EventLoop,
    sh: &Sh,
    argv: &[String],
    assigns: &[(String, String)],
    fds: Fds,
    last: bool,
    job: &mut Job,
) -> Option<i32> {
    // Only redirections (`> file`): the files are already created.
    let Some(program) = argv.first() else {
        return Some(0);
    };
    let mut command: process::Command = process::Command::new(program);
    command.args(&argv[1..]);
    for (name, value) in &sh.borrow().vars {
        match value {
            Some(value) => command.env(name, value),
            None => command.env_remove(name),
        };
    }
    // `NAME=value cmd`: for this command only.
    command.envs(
        assigns
            .iter()
            .map(|(n, v): &(String, String)| -> (&String, &String) { (n, v) }),
    );
    for (fd, slot) in fds.slots.into_iter().enumerate() {
        let stdio: Stdio = match slot {
            Slot::Shell(n) if usize::from(n) == fd => Stdio::inherit(),
            Slot::Shell(n) => match shell_fd(n) {
                Ok(owned) => Stdio::from(owned),
                Err(e) => {
                    shell::error(el, sh, &format!("{fd}>&{n}: {}", os_msg(&e)));
                    return Some(1);
                }
            },
            Slot::Fd(owned, _) => Stdio::from(owned),
            Slot::Heredoc(body) => match net::UnixStream::pair() {
                Ok((ours, theirs)) => {
                    write_slot(
                        el,
                        sh,
                        Slot::Fd(OwnedFd::from(ours), Kind::Socket),
                        body.into_bytes(),
                        job,
                    );
                    Stdio::from(OwnedFd::from(theirs))
                }
                Err(e) => {
                    shell::error(el, sh, &format!("here-document: {}", os_msg(&e)));
                    return Some(1);
                }
            },
        };
        match fd {
            0 => command.stdin(stdio),
            1 => command.stdout(stdio),
            _ => command.stderr(stdio),
        };
    }
    match command.spawn() {
        Ok(child) => {
            if last {
                job.last_pid = Some(child.id());
            }
            job.children.push(child);
            None
        }
        Err(e) => {
            let (msg, code): (String, i32) = match e.kind() {
                ErrorKind::NotFound => (String::from("command not found"), 127),
                _ => (os_msg(&e), 126),
            };
            shell::error(el, sh, &format!("{program}: {msg}"));
            Some(code)
        }
    }
}

/// A new fd for the shell's own fd `n` (for `2>&1` on an inherited stdout).
fn shell_fd(n: u8) -> io::Result<OwnedFd> {
    match n {
        0 => io::stdin().as_fd().try_clone_to_owned(),
        1 => io::stdout().as_fd().try_clone_to_owned(),
        _ => io::stderr().as_fd().try_clone_to_owned(),
    }
}

// ---- completion ----

/// SIGCHLD: collects every child of the job that has exited.
pub fn reap(el: &mut EventLoop, sh: &Sh) {
    {
        let mut s: std::cell::RefMut<'_, shell::Shell> = sh.borrow_mut();
        let Some(job) = s.job.as_mut() else {
            return;
        };
        for mut child in std::mem::take(&mut job.children) {
            match child.try_wait() {
                Ok(None) => job.children.push(child),
                Ok(Some(status)) => job.record(child.id(), status),
                // Not waitable (already reaped): nothing left to wait for.
                Err(_) => {}
            }
        }
    }
    maybe_finish(el, sh);
}

/// Completes the job once nothing in it is running.
fn maybe_finish(el: &mut EventLoop, sh: &Sh) {
    let (job, interactive): (Job, bool) = {
        let mut s: std::cell::RefMut<'_, shell::Shell> = sh.borrow_mut();
        let ready: bool = s
            .job
            .as_ref()
            .is_some_and(|job: &Job| -> bool { job.children.is_empty() && job.pending == 0 });
        if !ready {
            return;
        }
        let Some(job) = s.job.take() else {
            return;
        };
        (job, s.interactive)
    };
    let mut job: Job = job;
    let status: i32 = job.status.unwrap_or(0);
    let sigint: bool = job.sigint;
    let Some(done) = job.done.take() else {
        return;
    };
    let mut messages: String = std::mem::take(&mut job.messages);
    if sigint && interactive {
        // The terminal echoed ^C without a newline.
        messages.insert(0, '\n');
    }
    if messages.is_empty() {
        done(el, status, sigint);
    } else {
        // Report signal deaths before anything that follows.
        shell::print(
            el,
            sh,
            Out::Stderr,
            messages.into_bytes(),
            Some(Box::new(move |el: &mut EventLoop| done(el, status, sigint))),
        );
    }
}

/// Sends `signal` to every running child of the job.
pub fn signal_children(sh: &Sh, signal: Signal) {
    if let Some(job) = sh.borrow().job.as_ref() {
        for child in &job.children {
            let _: rustix::io::Result<()> =
                rustix::process::kill_process(Pid::from_child(child), signal);
        }
    }
}
