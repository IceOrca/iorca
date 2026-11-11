//! Shell state and the event-driven plumbing around it: ordered console
//! output, line input, the prompt, signals, history persistence and shutdown.
//!
//! Everything runs on one iorca [`EventLoop`]. The shell reads stdin only
//! while it waits for a line, so a running command that reads the terminal
//! gets its input. On a terminal, lines are read with the [`Editor`] in
//! non-canonical mode (arrow keys, history); the terminal is put back in
//! canonical mode before any command runs. Signals arrive as bytes on socket pairs (signal-hook's
//! self-pipe trick) that the loop reads like any other input.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, ErrorKind};
use std::os::unix::net;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use iorca::{EventLoop, File, OpenOptions, UnixStream};
use rustix::termios::{LocalModes, OptionalActions, SpecialCodeIndex, Termios};
use signal_hook::consts::{SIGCHLD, SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP};

use crate::edit::{Editor, Fed, Finish};
use crate::exec::{self, Job};
use crate::history::History;
use crate::parse::{self, Env, List};

/// The shell, shared by every callback.
pub type Sh = Rc<RefCell<Shell>>;
/// A continuation with no result.
pub type Done = Box<dyn FnOnce(&mut EventLoop)>;
/// Receives the next input line, or `None` at end of input.
type LineCont = Box<dyn FnOnce(&mut EventLoop, Option<String>)>;
/// Receives here-document bodies, in order.
type BodiesCont = Box<dyn FnOnce(&mut EventLoop, Vec<String>)>;
/// Receives one here-document body.
type BodyCont = Box<dyn FnOnce(&mut EventLoop, String)>;
/// Receives a whole file's contents.
type BytesCont = Box<dyn FnOnce(&mut EventLoop, Vec<u8>)>;

pub struct Shell {
    /// Reading commands from a terminal: show prompts, survive Ctrl+C.
    pub interactive: bool,
    /// Exit status of the last pipeline (`$?`).
    pub status: i32,
    /// `export`ed (`Some`) and `unset` (`None`) variables, layered over the
    /// process environment and passed to children.
    pub vars: HashMap<String, Option<String>>,
    pub history: History,
    /// Where history is loaded from and saved to, if anywhere.
    hist_path: Option<PathBuf>,
    input: Input,
    console: Console,
    /// The terminal's settings at startup (canonical mode), when stdin is
    /// an interactive terminal: enables the line editor.
    term: Option<Termios>,
    /// The terminal is in the editor's non-canonical mode.
    raw: bool,
    /// The line being edited, while the prompt is up.
    edit: Option<Editor>,
    /// The pipeline that is running, if any.
    pub job: Option<Job>,
    /// Ctrl+C arrived while a job was running: abort the rest of the line.
    pub interrupted: bool,
    /// A job just died of a Ctrl+C whose SIGINT the shell hasn't seen yet
    /// (SIGCHLD won the race); ignore that SIGINT when it arrives.
    pub sigint_expected: bool,
    /// `shutdown` has started.
    pub exiting: bool,
    /// The process exit code once the loop stops.
    pub exit_code: i32,
}

/// Line-oriented stdin (or `-c` script) input.
#[derive(Default)]
struct Input {
    /// Complete lines not consumed yet.
    lines: VecDeque<String>,
    /// Bytes after the last newline.
    partial: Vec<u8>,
    eof: bool,
    /// A stdin read is in flight.
    reading: bool,
    /// Who gets the next line.
    want: Option<LineCont>,
    /// Key bytes typed past the end of the last edited line.
    keys: Vec<u8>,
}

impl Input {
    fn push_bytes(&mut self, bytes: &[u8]) {
        self.partial.extend_from_slice(bytes);
        while let Some(pos) = self
            .partial
            .iter()
            .position(|b: &u8| -> bool { *b == b'\n' })
        {
            let line: Vec<u8> = self.partial.drain(..=pos).collect::<Vec<u8>>();
            self.lines
                .push_back(String::from_utf8_lossy(&line[..pos]).into_owned());
        }
    }
}

/// Serializes stdout/stderr writes so they reach the terminal in order
/// (io_uring may complete concurrently submitted writes in any order).
#[derive(Default)]
struct Console {
    busy: bool,
    queue: VecDeque<(Out, Vec<u8>, Option<Done>)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    Stdout,
    Stderr,
}

impl Env for Shell {
    fn var(&self, name: &str) -> Option<String> {
        match self.vars.get(name) {
            Some(value) => value.clone(),
            None => std::env::var(name).ok(),
        }
    }

    fn status(&self) -> i32 {
        self.status
    }
}

impl Shell {
    /// `script` (from `-c`) replaces stdin as the source of lines.
    pub fn new(interactive: bool, script: Option<String>, hist_path: Option<PathBuf>) -> Sh {
        let mut input: Input = Input::default();
        if let Some(script) = script {
            input.lines = script
                .lines()
                .map(String::from)
                .collect::<VecDeque<String>>();
            input.eof = true;
        }
        Rc::new(RefCell::new(Shell {
            interactive,
            status: 0,
            vars: HashMap::new(),
            history: History::default(),
            hist_path,
            input,
            console: Console::default(),
            term: if interactive {
                rustix::termios::tcgetattr(io::stdin()).ok()
            } else {
                None
            },
            raw: false,
            edit: None,
            job: None,
            interrupted: false,
            sigint_expected: false,
            exiting: false,
            exit_code: 0,
        }))
    }
}

/// Installs signal watchers, loads history, then shows the first prompt.
pub fn start(el: &mut EventLoop, sh: &Sh) -> io::Result<()> {
    for sig in [SIGINT, SIGTERM, SIGHUP, SIGQUIT, SIGTSTP, SIGCHLD] {
        watch_signal(el, sh, sig)?;
    }
    let path: Option<PathBuf> = sh.borrow().hist_path.clone();
    match path {
        Some(path) => load_history(el, sh, path),
        None => prompt(el, sh),
    }
    Ok(())
}

// ---- console ----

/// Queues `bytes` for stdout or stderr; `done` runs once they are written.
pub fn print(el: &mut EventLoop, sh: &Sh, out: Out, bytes: Vec<u8>, done: Option<Done>) {
    {
        let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
        s.console.queue.push_back((out, bytes, done));
        if s.console.busy {
            return;
        }
        s.console.busy = true;
    }
    console_next(el, sh);
}

/// Runs `then` once all queued output is written: children write to the
/// terminal directly, so they must not overtake the shell's own messages.
pub fn after_output(el: &mut EventLoop, sh: &Sh, then: Done) {
    if sh.borrow().console.busy {
        print(el, sh, Out::Stdout, Vec::new(), Some(then));
    } else {
        then(el);
    }
}

/// Prints `shino: msg` on stderr.
pub fn error(el: &mut EventLoop, sh: &Sh, msg: &str) {
    print(
        el,
        sh,
        Out::Stderr,
        format!("shino: {msg}\n").into_bytes(),
        None,
    );
}

fn console_next(el: &mut EventLoop, sh: &Sh) {
    let next: Option<(Out, Vec<u8>, Option<Done>)> = sh.borrow_mut().console.queue.pop_front();
    let Some((out, bytes, done)) = next else {
        sh.borrow_mut().console.busy = false;
        return;
    };
    let sh2: Sh = Rc::clone(sh);
    // Write errors (e.g. a closed terminal) are ignored: there is nowhere
    // left to report them.
    let cb = move |el: &mut EventLoop, _: io::Result<()>, _: Vec<u8>| {
        if let Some(done) = done {
            done(el);
        }
        console_next(el, &sh2);
    };
    match out {
        Out::Stdout => iorca::stdout().write_all(el, bytes, cb),
        Out::Stderr => iorca::stderr().write_all(el, bytes, cb),
    }
}

// ---- input ----

/// Shows `prompt` and hands the next input line to `cont`: through the line
/// editor on a terminal, otherwise from buffered stdin (or the `-c` script).
fn request_line(el: &mut EventLoop, sh: &Sh, prompt: Option<String>, cont: LineCont) {
    if sh.borrow().term.is_some() {
        start_edit(el, sh, prompt.unwrap_or_default(), cont);
        return;
    }
    if let Some(prompt) = prompt {
        print(el, sh, Out::Stdout, prompt.into_bytes(), None);
    }
    next_line(el, sh, cont);
}

/// Hands the next buffered line to `cont`, reading stdin if needed.
fn next_line(el: &mut EventLoop, sh: &Sh, cont: LineCont) {
    let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
    if let Some(line) = s.input.lines.pop_front() {
        drop(s);
        // Deferred a turn so a long run of buffered lines can't recurse.
        el.nop(move |el: &mut EventLoop, _: io::Result<()>| cont(el, Some(line)));
        return;
    }
    if s.input.eof {
        let rest: Vec<u8> = std::mem::take(&mut s.input.partial);
        drop(s);
        let line: Option<String> =
            (!rest.is_empty()).then(|| -> String { String::from_utf8_lossy(&rest).into_owned() });
        el.nop(move |el: &mut EventLoop, _: io::Result<()>| cont(el, line));
        return;
    }
    s.input.want = Some(cont);
    drop(s);
    ensure_reading(el, sh);
}

/// Submits a stdin read unless one is in flight.
fn ensure_reading(el: &mut EventLoop, sh: &Sh) {
    let start: bool = !std::mem::replace(&mut sh.borrow_mut().input.reading, true);
    if start {
        read_stdin(el, sh);
    }
}

fn read_stdin(el: &mut EventLoop, sh: &Sh) {
    let sh2: Sh = Rc::clone(sh);
    iorca::stdin().read(
        el,
        Vec::with_capacity(4096),
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| {
            sh2.borrow_mut().input.reading = false;
            let retry: Option<Duration> = match &res {
                Err(e) if e.kind() == ErrorKind::Interrupted => Some(Duration::ZERO),
                // A non-blocking stdin (set by some parent): poll gently.
                Err(e) if e.kind() == ErrorKind::WouldBlock => Some(Duration::from_millis(10)),
                _ => None,
            };
            if let Some(delay) = retry {
                let sh3: Sh = Rc::clone(&sh2);
                el.timeout(delay, move |el: &mut EventLoop, _: io::Result<()>| {
                    resume_input(el, &sh3)
                });
                return;
            }
            // `None`: end of input (or an unrecoverable error).
            let data: Option<Vec<u8>> = match res {
                Ok(n) if n > 0 => Some(buf),
                _ => None,
            };
            if sh2.borrow().edit.is_some() {
                match data {
                    Some(bytes) => feed_editor(el, &sh2, &bytes),
                    None => finish_edit(el, &sh2, Finish::Eof),
                }
                return;
            }
            {
                let mut s: std::cell::RefMut<'_, Shell> = sh2.borrow_mut();
                match data {
                    Some(bytes) => s.input.push_bytes(&bytes),
                    None => s.input.eof = true,
                }
            }
            resume_input(el, &sh2);
        },
    );
}

/// Serves a pending line request after a read completed.
fn resume_input(el: &mut EventLoop, sh: &Sh) {
    if sh.borrow().edit.is_some() {
        ensure_reading(el, sh);
        return;
    }
    let want: Option<LineCont> = sh.borrow_mut().input.want.take();
    if let Some(cont) = want {
        next_line(el, sh, cont);
    }
}

// ---- line editing ----

/// Starts editing a line: non-canonical mode, prompt, then key input.
fn start_edit(el: &mut EventLoop, sh: &Sh, prompt: String, cont: LineCont) {
    let editor: Editor = Editor::new(prompt);
    let screen: Vec<u8> = editor.start();
    let keys: Vec<u8> = {
        let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
        set_raw(&mut s, true);
        s.edit = Some(editor);
        s.input.want = Some(cont);
        std::mem::take(&mut s.input.keys)
    };
    print(el, sh, Out::Stdout, screen, None);
    if keys.is_empty() {
        ensure_reading(el, sh);
    } else {
        // Typed ahead (or pasted) past the previous line.
        let sh2: Sh = Rc::clone(sh);
        el.nop(move |el: &mut EventLoop, _: io::Result<()>| feed_editor(el, &sh2, &keys));
    }
}

/// Passes key bytes to the editor and echoes its redraw.
fn feed_editor(el: &mut EventLoop, sh: &Sh, bytes: &[u8]) {
    let fed: Option<Fed> = {
        let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
        let Shell {
            edit,
            history,
            input,
            ..
        } = &mut *s;
        match edit.as_mut() {
            Some(editor) => {
                let mut fed: Fed = editor.feed(bytes, history.entries());
                if fed.finish.is_some() {
                    input.keys = std::mem::take(&mut fed.rest);
                }
                Some(fed)
            }
            None => {
                input.keys.extend_from_slice(bytes);
                None
            }
        }
    };
    let Some(fed) = fed else {
        return;
    };
    if !fed.output.is_empty() {
        print(el, sh, Out::Stdout, fed.output, None);
    }
    match fed.finish {
        Some(finish) => finish_edit(el, sh, finish),
        None => ensure_reading(el, sh),
    }
}

/// Ends the edit: back to canonical mode, then hands over the line.
fn finish_edit(el: &mut EventLoop, sh: &Sh, finish: Finish) {
    let cont: Option<LineCont> = {
        let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
        s.edit = None;
        set_raw(&mut s, false);
        s.input.want.take()
    };
    if let Some(cont) = cont {
        let line: Option<String> = match finish {
            Finish::Line(line) => Some(line),
            Finish::Eof => None,
        };
        cont(el, line);
    }
}

/// Switches the terminal between the editor's mode (no line buffering, no
/// echo; signal keys still work) and the startup settings.
fn set_raw(s: &mut Shell, raw: bool) {
    if s.raw == raw {
        return;
    }
    let Some(saved) = s.term.as_ref() else {
        return;
    };
    let mut termios: Termios = saved.clone();
    if raw {
        termios
            .local_modes
            .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::IEXTEN);
        termios.special_codes[SpecialCodeIndex::VMIN] = 1;
        termios.special_codes[SpecialCodeIndex::VTIME] = 0;
    }
    if rustix::termios::tcsetattr(io::stdin(), OptionalActions::Now, &termios).is_ok() {
        s.raw = raw;
    }
}

// ---- the read-eval loop ----

/// Expects the shell's own SIGINT for a Ctrl+C that already killed a
/// command, for a moment (a SIGINT sent to the command alone never comes).
pub fn expect_sigint(el: &mut EventLoop, sh: &Sh) {
    sh.borrow_mut().sigint_expected = true;
    let sh2: Sh = Rc::clone(sh);
    el.timeout(
        Duration::from_millis(250),
        move |_: &mut EventLoop, _: io::Result<()>| {
            sh2.borrow_mut().sigint_expected = false;
        },
    );
}

/// Shows the prompt (when interactive) and waits for a command line.
pub fn prompt(el: &mut EventLoop, sh: &Sh) {
    let text: Option<String> = {
        let s: std::cell::Ref<'_, Shell> = sh.borrow();
        if s.exiting {
            return;
        }
        s.interactive.then(|| -> String { prompt_text(&s) })
    };
    let sh2: Sh = Rc::clone(sh);
    request_line(
        el,
        sh,
        text,
        Box::new(move |el: &mut EventLoop, line: Option<String>| on_line(el, &sh2, line)),
    );
}

fn prompt_text(s: &Shell) -> String {
    let cwd: String = std::env::current_dir()
        .map(|p: PathBuf| -> String { p.display().to_string() })
        .unwrap_or_else(|_: io::Error| -> String { String::from("?") });
    let cwd: String = match s.var("HOME") {
        Some(home) if !home.is_empty() && cwd.starts_with(&home) => {
            format!("~{}", &cwd[home.len()..])
        }
        _ => cwd,
    };
    match s.status {
        0 => format!("shino:{cwd}$ "),
        code => format!("shino:{cwd} [{code}]$ "),
    }
}

fn on_line(el: &mut EventLoop, sh: &Sh, line: Option<String>) {
    let Some(line) = line else {
        // Ctrl+D / end of input.
        let (interactive, status): (bool, i32) = {
            let s: std::cell::Ref<'_, Shell> = sh.borrow();
            (s.interactive, s.status)
        };
        if interactive {
            print(el, sh, Out::Stdout, b"exit\n".to_vec(), None);
        }
        shutdown(el, sh, status);
        return;
    };
    let expanded: Result<Option<String>, String> = sh.borrow().history.expand(&line);
    let line: String = match expanded {
        Ok(None) => line,
        Ok(Some(new)) => {
            // Like bash: show what `!` expanded to.
            print(el, sh, Out::Stdout, format!("{new}\n").into_bytes(), None);
            new
        }
        Err(msg) => {
            error(el, sh, &msg);
            sh.borrow_mut().status = 1;
            prompt(el, sh);
            return;
        }
    };
    sh.borrow_mut().history.push(&line);
    let mut list: List = match parse::parse(&line) {
        Ok(list) => list,
        Err(msg) => {
            error(el, sh, &format!("syntax error: {msg}"));
            sh.borrow_mut().status = 2;
            prompt(el, sh);
            return;
        }
    };
    let specs: VecDeque<(String, bool)> = list
        .heredocs_mut()
        .into_iter()
        .map(|h: &mut parse::Heredoc| -> (String, bool) { (h.delimiter.clone(), h.strip_tabs) })
        .collect::<VecDeque<(String, bool)>>();
    let sh2: Sh = Rc::clone(sh);
    read_heredocs(
        el,
        sh,
        specs,
        Vec::new(),
        Box::new(move |el: &mut EventLoop, bodies: Vec<String>| {
            for (heredoc, body) in list.heredocs_mut().into_iter().zip(bodies) {
                heredoc.body = body;
            }
            let sh3: Sh = Rc::clone(&sh2);
            exec::run_list(
                el,
                &sh2,
                list.items,
                Box::new(move |el: &mut EventLoop| prompt(el, &sh3)),
            );
        }),
    );
}

/// Reads the bodies of the line's here-documents from the following lines.
fn read_heredocs(
    el: &mut EventLoop,
    sh: &Sh,
    mut specs: VecDeque<(String, bool)>,
    bodies: Vec<String>,
    then: BodiesCont,
) {
    let Some((delimiter, strip_tabs)) = specs.pop_front() else {
        then(el, bodies);
        return;
    };
    let sh2: Sh = Rc::clone(sh);
    read_heredoc_body(
        el,
        sh,
        delimiter,
        strip_tabs,
        String::new(),
        Box::new(move |el: &mut EventLoop, body: String| {
            let mut bodies: Vec<String> = bodies;
            bodies.push(body);
            read_heredocs(el, &sh2, specs, bodies, then);
        }),
    );
}

fn read_heredoc_body(
    el: &mut EventLoop,
    sh: &Sh,
    delimiter: String,
    strip_tabs: bool,
    mut body: String,
    then: BodyCont,
) {
    let prompt: Option<String> = sh
        .borrow()
        .interactive
        .then(|| -> String { String::from("> ") });
    let sh2: Sh = Rc::clone(sh);
    request_line(
        el,
        sh,
        prompt,
        Box::new(move |el: &mut EventLoop, line: Option<String>| match line {
            None => {
                error(
                    el,
                    &sh2,
                    &format!(
                        "warning: here-document delimited by end-of-file (wanted `{delimiter}`)"
                    ),
                );
                then(el, body);
            }
            Some(line) => {
                let line: &str = if strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if line == delimiter {
                    then(el, body);
                } else {
                    body.push_str(line);
                    body.push('\n');
                    read_heredoc_body(el, &sh2, delimiter, strip_tabs, body, then);
                }
            }
        }),
    );
}

// ---- signals ----

/// Forwards `sig` to a socket pair and reads it on the loop.
fn watch_signal(el: &mut EventLoop, sh: &Sh, sig: i32) -> io::Result<()> {
    let (rx, tx): (net::UnixStream, net::UnixStream) = net::UnixStream::pair()?;
    signal_hook::low_level::pipe::register(sig, tx)?;
    arm_signal(el, Rc::clone(sh), UnixStream::from(rx), sig);
    Ok(())
}

fn arm_signal(el: &mut EventLoop, sh: Sh, rx: UnixStream, sig: i32) {
    let next: UnixStream = rx.clone();
    rx.recv(
        el,
        Vec::with_capacity(64),
        move |el: &mut EventLoop, res: io::Result<usize>, _: Vec<u8>| {
            if !matches!(res, Ok(n) if n > 0) {
                return;
            }
            // Several deliveries may arrive as one read: handled once.
            arm_signal(el, Rc::clone(&sh), next, sig);
            on_signal(el, &sh, sig);
        },
    );
}

fn on_signal(el: &mut EventLoop, sh: &Sh, sig: i32) {
    let (running, interactive): (bool, bool) = {
        let s: std::cell::Ref<'_, Shell> = sh.borrow();
        (s.job.is_some(), s.interactive)
    };
    match sig {
        SIGCHLD => exec::reap(el, sh),
        // Ctrl+C: the terminal also sent it to the running command.
        SIGINT if running => sh.borrow_mut().interrupted = true,
        // The same Ctrl+C that already ended the last command.
        SIGINT if std::mem::take(&mut sh.borrow_mut().sigint_expected) => {}
        SIGINT if interactive => {
            // At a prompt (or inside a here-document): drop the line.
            let editing: bool = {
                let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
                s.input.want = None;
                s.input.partial.clear();
                s.input.lines.clear();
                s.input.keys.clear();
                s.status = 130;
                s.edit.take().is_some()
            };
            // Without echo the terminal did not show ^C itself.
            let shown: &[u8] = if editing { b"^C\n" } else { b"\n" };
            print(el, sh, Out::Stdout, shown.to_vec(), None);
            prompt(el, sh);
        }
        SIGINT => shutdown(el, sh, 130),
        SIGTERM | SIGHUP => {
            let signal: rustix::process::Signal = if sig == SIGTERM {
                rustix::process::Signal::TERM
            } else {
                rustix::process::Signal::HUP
            };
            exec::signal_children(sh, signal);
            shutdown(el, sh, 128 + sig);
        }
        // Ctrl+Z: the command stopped, but there is no job control to
        // resume it later, so continue it right away.
        SIGTSTP if running => {
            exec::signal_children(sh, rustix::process::Signal::CONT);
            error(
                el,
                sh,
                "job control (Ctrl+Z) is not supported; command resumed",
            );
        }
        // Ctrl+\ (SIGQUIT) is ignored by the shell, like bash; the running
        // command quits (and dumps core).
        _ => {}
    }
}

// ---- history file and shutdown ----

fn load_history(el: &mut EventLoop, sh: &Sh, path: PathBuf) {
    let sh2: Sh = Rc::clone(sh);
    File::open(
        el,
        path,
        move |el: &mut EventLoop, res: io::Result<File>| match res {
            Ok(file) => read_all(
                el,
                file,
                Vec::new(),
                Box::new(move |el: &mut EventLoop, data: Vec<u8>| {
                    sh2.borrow_mut()
                        .history
                        .load(&String::from_utf8_lossy(&data));
                    prompt(el, &sh2);
                }),
            ),
            // No history yet.
            Err(_) => prompt(el, &sh2),
        },
    );
}

fn read_all(el: &mut EventLoop, file: File, data: Vec<u8>, then: BytesCont) {
    let offset: u64 = u64::try_from(data.len()).unwrap_or(u64::MAX);
    let next: File = file.clone();
    file.read_at(
        el,
        Vec::with_capacity(64 * 1024),
        offset,
        move |el: &mut EventLoop, res: io::Result<usize>, buf: Vec<u8>| match res {
            Ok(n) if n > 0 => {
                let mut data: Vec<u8> = data;
                data.extend_from_slice(&buf);
                read_all(el, next, data, then);
            }
            _ => then(el, data),
        },
    );
}

/// Saves history, flushes queued output, then stops the loop. The process
/// exits with `code`.
pub fn shutdown(el: &mut EventLoop, sh: &Sh, code: i32) {
    let (path, text): (Option<PathBuf>, Vec<u8>) = {
        let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
        if s.exiting {
            return;
        }
        s.exiting = true;
        s.exit_code = code;
        s.input.want = None;
        s.edit = None;
        set_raw(&mut s, false);
        (s.hist_path.clone(), s.history.to_text().into_bytes())
    };
    let sh2: Sh = Rc::clone(sh);
    // Queued behind all earlier output, so the loop stops once it is out.
    let finish: Done = Box::new(move |el: &mut EventLoop| {
        print(
            el,
            &sh2,
            Out::Stdout,
            Vec::new(),
            Some(Box::new(|el: &mut EventLoop| el.stop())),
        );
    });
    let Some(path) = path else {
        finish(el);
        return;
    };
    let mut options: OpenOptions = OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(0o600);
    let sh3: Sh = Rc::clone(sh);
    File::open_with(
        el,
        path,
        &options,
        move |el: &mut EventLoop, res: io::Result<File>| match res {
            Ok(file) => file.write_all_at(
                el,
                text,
                0,
                move |el: &mut EventLoop, res: io::Result<()>, _: Vec<u8>| {
                    if let Err(e) = res {
                        error(el, &sh3, &format!("cannot save history: {}", os_msg(&e)));
                    }
                    finish(el);
                },
            ),
            Err(e) => {
                error(el, &sh3, &format!("cannot save history: {}", os_msg(&e)));
                finish(el);
            }
        },
    );
}

/// An I/O error's message without the " (os error N)" suffix.
pub fn os_msg(e: &io::Error) -> String {
    let msg: String = e.to_string();
    match msg.find(" (os error") {
        Some(end) => String::from(&msg[..end]),
        None => msg,
    }
}
