//! End-to-end tests: run the real `iosh` binary non-interactively.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdout, Command, Output, Stdio};
use std::time::{Duration, Instant};

use std::os::fd::{AsFd, OwnedFd};
use std::sync::{Arc, Mutex};

use rustix::process::{Pid, Signal};

const IOSH: &str = env!("CARGO_BIN_EXE_iosh");

/// A fresh scratch directory, used as the shell's working directory.
fn scratch(name: &str) -> PathBuf {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("iosh-test-{}-{name}", std::process::id()));
    let _: std::io::Result<()> = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Runs `iosh -c script` in `dir`; returns (exit code, stdout, stderr).
fn run_c(dir: &PathBuf, script: &str) -> (i32, String, String) {
    let out: Output = Command::new(IOSH)
        .args(["-c", script])
        .current_dir(dir)
        .env_remove("IOSH_HISTFILE")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Feeds `input` to iosh on stdin; returns (exit code, stdout, stderr).
fn run_stdin(dir: &PathBuf, input: &str, histfile: Option<&PathBuf>) -> (i32, String, String) {
    let mut cmd: Command = Command::new(IOSH);
    cmd.current_dir(dir)
        .env_remove("IOSH_HISTFILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(path) = histfile {
        cmd.env("IOSH_HISTFILE", path);
    }
    let mut child: Child = cmd.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out: Output = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn sequences_and_conditionals() {
    let dir: PathBuf = scratch("seq");
    let (code, out, _): (i32, String, String) = run_c(
        &dir,
        "echo a; false && echo no || echo b; true || echo no; true && echo c; false; echo $?",
    );
    assert_eq!(out, "a\nb\nc\n1\n");
    assert_eq!(code, 0);
}

#[test]
fn pipes_between_commands_and_builtins() {
    let dir: PathBuf = scratch("pipe");
    let (_, out, _): (i32, String, String) = run_c(
        &dir,
        "echo hello world | tr a-z A-Z | tr -d O; printf 'b\\na\\n' | sort | head -1",
    );
    assert_eq!(out, "HELL WRLD\na\n");
}

#[test]
fn redirections() {
    let dir: PathBuf = scratch("redir");
    let (_, out, err): (i32, String, String) = run_c(
        &dir,
        "echo one > f; echo two >> f; cat < f; wc -l < f > n; cat n; \
         ls /nope 2> e; wc -l < e; ls /nope 2>&1 | grep -c nope; echo to-err >&2; > empty; cat empty",
    );
    assert_eq!(out, "one\ntwo\n2\n1\n1\n");
    assert_eq!(err, "to-err\n");
    assert_eq!(
        std::fs::read_to_string(dir.join("f")).unwrap(),
        "one\ntwo\n"
    );
    let (code, _, err): (i32, String, String) = run_c(&dir, "cat < missing");
    assert_eq!(code, 1);
    assert_eq!(err, "iosh: missing: No such file or directory\n");
}

#[test]
fn here_documents() {
    let dir: PathBuf = scratch("heredoc");
    let input: &str = "export X=world\n\
                       cat <<EOF\nhello $X\n\\$X $?\nEOF\n\
                       cat <<'END' | tr a-z A-Z\nraw $X\nEND\n\
                       cat <<-EOF\n\t\ttabs\n\tEOF\n\
                       echo after\n";
    let (code, out, _): (i32, String, String) = run_stdin(&dir, input, None);
    assert_eq!(out, "hello world\n$X 0\nRAW $X\ntabs\nafter\n");
    assert_eq!(code, 0);
    // End of input inside a here-document: a warning, then the body so far.
    let (_, out, err): (i32, String, String) = run_stdin(&dir, "cat <<EOF\npartial\n", None);
    assert_eq!(out, "partial\n");
    assert!(
        err.contains("here-document delimited by end-of-file"),
        "{err}"
    );
}

#[test]
fn variables_and_builtins() {
    let dir: PathBuf = scratch("vars");
    let (_, out, _): (i32, String, String) = run_c(
        &dir,
        "X=1; echo $X ${X}x '$X' \"[$X]\"; export Y=2; sh -c 'echo $Y'; Z=3 sh -c 'echo $Z'; echo [$Z]; \
         unset Y; sh -c 'echo [$Y]'; mkdir d; cd d; pwd | grep -c /d$; cd ..; echo -n no-newline; echo",
    );
    assert_eq!(out, "1 1x $X [1]\n2\n3\n[]\n[]\n1\nno-newline\n");
}

#[test]
fn history_expansion_and_file() {
    let dir: PathBuf = scratch("history");
    let histfile: PathBuf = dir.join("hist");
    std::fs::write(&histfile, "echo from-file\n").unwrap();
    let (_, out, err): (i32, String, String) = run_stdin(
        &dir,
        "echo one\n!!\n!-2\n!ec | tr a-z A-Z\nhistory 2\n!nope\n",
        Some(&histfile),
    );
    assert_eq!(
        out,
        "one\necho one\none\necho from-file\nfrom-file\necho from-file | tr a-z A-Z\nFROM-FILE\n\
         \x20   4  echo from-file | tr a-z A-Z\n    5  history 2\n"
    );
    assert_eq!(err, "iosh: !nope: event not found\n");
    // Saved on exit (the failed expansion is not recorded).
    assert_eq!(
        std::fs::read_to_string(&histfile).unwrap(),
        "echo from-file\necho one\necho from-file\necho from-file | tr a-z A-Z\nhistory 2\n"
    );
}

#[test]
fn exit_codes_and_errors() {
    let dir: PathBuf = scratch("exit");
    assert_eq!(run_c(&dir, "exit 7; echo unreachable").0, 7);
    assert_eq!(run_c(&dir, "false").0, 1);
    assert_eq!(run_c(&dir, "sh -c 'exit 3' | true").0, 0);
    let (code, _, err): (i32, String, String) = run_c(&dir, "no-such-command-x");
    assert_eq!(
        (code, err.as_str()),
        (127, "iosh: no-such-command-x: command not found\n")
    );
    let (code, _, err): (i32, String, String) = run_c(&dir, "echo a |");
    assert_eq!(
        (code, err.as_str()),
        (2, "iosh: syntax error: unexpected end of line\n")
    );
    let (code, out, err): (i32, String, String) = run_c(&dir, "sh -c 'kill -ABRT $$'; echo st=$?");
    assert_eq!(code, 0);
    assert_eq!(out, "st=134\n");
    assert!(err.starts_with("Aborted"), "{err}");
    // Ctrl+D (end of input) exits with the last status.
    assert_eq!(run_stdin(&dir, "echo x\nfalse\n", None).0, 1);
}

/// Spawns iosh in its own process group (like a terminal's foreground job)
/// running `input`, and waits until it has printed "ready".
fn spawn_ready(input: &str) -> (Child, Instant) {
    let mut child: Child = Command::new(IOSH)
        .env_remove("IOSH_HISTFILE")
        .process_group(0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let mut stdout: BufReader<ChildStdout> = BufReader::new(child.stdout.take().unwrap());
    let mut line: String = String::new();
    stdout.read_line(&mut line).unwrap();
    assert_eq!(line, "ready\n");
    child.stdout = Some(stdout.into_inner());
    (child, Instant::now())
}

fn finish(mut child: Child) -> (i32, String) {
    let mut rest: String = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut rest)
        .unwrap();
    (child.wait().unwrap().code().unwrap_or(-1), rest)
}

#[test]
fn ctrl_c_interrupts_the_line() {
    // SIGINT to the whole process group, as the terminal does on Ctrl+C: the
    // command dies, the rest of the line is skipped, and a script exits.
    let (child, start): (Child, Instant) =
        spawn_ready("echo ready; sleep 5; echo skipped\necho never\n");
    std::thread::sleep(Duration::from_millis(200));
    rustix::process::kill_process_group(Pid::from_child(&child), Signal::INT).unwrap();
    let (code, rest): (i32, String) = finish(child);
    assert_eq!(code, 130);
    assert_eq!(rest, "");
    assert!(start.elapsed() < Duration::from_secs(3));
}

#[test]
fn sigterm_is_forwarded_and_exits() {
    let (child, start): (Child, Instant) = spawn_ready("echo ready; sleep 5; echo skipped\n");
    std::thread::sleep(Duration::from_millis(200));
    rustix::process::kill_process(Pid::from_child(&child), Signal::TERM).unwrap();
    let (code, rest): (i32, String) = finish(child);
    assert_eq!(code, 143);
    assert_eq!(rest, "");
    assert!(start.elapsed() < Duration::from_secs(3));
}

/// iosh on a pseudo-terminal, driven like a user at the keyboard. (Not its
/// controlling terminal, so keys don't raise signals here.)
struct Terminal {
    master: OwnedFd,
    output: Arc<Mutex<Vec<u8>>>,
    child: Child,
}

impl Terminal {
    fn spawn(dir: &PathBuf, histfile: &PathBuf) -> Terminal {
        use rustix::fs::{Mode, OFlags};
        use rustix::pty::OpenptFlags;
        let master: OwnedFd = rustix::pty::openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        rustix::pty::grantpt(&master).unwrap();
        rustix::pty::unlockpt(&master).unwrap();
        let name: std::ffi::CString = rustix::pty::ptsname(&master, Vec::new()).unwrap();
        let slave: OwnedFd = rustix::fs::open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let child: Child = Command::new(IOSH)
            .current_dir(dir)
            .env("HOME", dir)
            .env("IOSH_HISTFILE", histfile)
            .stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave))
            .spawn()
            .unwrap();
        let output: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let sink: Arc<Mutex<Vec<u8>>> = Arc::clone(&output);
        let mut reader: std::fs::File = std::fs::File::from(master.try_clone().unwrap());
        std::thread::spawn(move || {
            let mut buf: [u8; 4096] = [0u8; 4096];
            // Ends with EIO once the shell (the last slave holder) exits.
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        Terminal {
            master,
            output,
            child,
        }
    }

    fn send(&self, keys: &[u8]) {
        let written: usize = rustix::io::write(&self.master, keys).unwrap();
        assert_eq!(written, keys.len());
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output.lock().unwrap()).into_owned()
    }

    /// The shell is at a prompt: the terminal is in the editor's mode.
    fn editing(&self) -> bool {
        let termios: rustix::termios::Termios =
            rustix::termios::tcgetattr(self.master.as_fd()).unwrap();
        !termios
            .local_modes
            .contains(rustix::termios::LocalModes::ICANON)
    }

    /// Waits until the output contains `needle` and the next prompt is up.
    fn wait_for(&self, needle: &str) {
        let deadline: Instant = Instant::now() + Duration::from_secs(10);
        while !(self.text().contains(needle) && self.editing()) {
            assert!(
                Instant::now() < deadline,
                "no {needle:?} in {:?}",
                self.text()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

#[test]
fn line_editor_history_and_keys() {
    let dir: PathBuf = scratch("editor");
    let histfile: PathBuf = dir.join("hist");
    std::fs::write(&histfile, "echo first\necho second\n").unwrap();
    let mut term: Terminal = Terminal::spawn(&dir, &histfile);
    term.wait_for("iosh:~$ ");

    // Up twice: the older entry. (Command output gets \r\n from the tty.)
    term.send(b"\x1b[A\x1b[A\r");
    term.wait_for("first\r\n");
    // Fix a typo mid-line: Ctrl+A, Ctrl+Right (word), Right, insert.
    term.send(b"echo hllo\x01\x1b[1;5C\x1b[C\x1b[Ce\r");
    term.wait_for("hello\r\n");
    // Up, Down returns to the draft; Up again, edit the recalled line.
    term.send(b"draft\x1b[A\x1b[B\x15\x1b[A\x7f\x7f\x7f\x7f\x7fbye\r");
    term.wait_for("bye\r\n");
    // Ctrl+D on an empty line exits.
    term.send(b"\x04");
    let code: Option<i32> = term.child.wait().unwrap().code();
    assert_eq!(code, Some(0));
    assert!(term.text().contains("exit"), "{:?}", term.text());
    assert_eq!(
        std::fs::read_to_string(&histfile).unwrap(),
        "echo first\necho second\necho first\necho hello\necho bye\n"
    );
}
