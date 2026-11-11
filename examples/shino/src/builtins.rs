//! Builtin commands. Each produces its output as bytes, which the executor
//! writes through iorca to wherever the command's stdout/stderr point.

use std::path::{Path, PathBuf};

use crate::parse::Env;
use crate::shell::{Sh, Shell, os_msg};

const NAMES: [&str; 8] = [
    "cd", "pwd", "echo", "exit", "history", "export", "unset", "help",
];

const HELP: &str = "\
shino: a small shell on the iorca io_uring event loop

builtins:
  cd [dir|-]          change directory (default $HOME)
  pwd                 print the working directory
  echo [-n] args...   print arguments
  exit [n]            leave the shell (also Ctrl+D)
  history [-c | n]    list (last n) commands, or clear them
  export [NAME=val]   set an environment variable (no args: list them)
  NAME=val [cmd]      set a variable (exported), or set it for cmd only
  unset NAME...       remove environment variables
  help                this text

syntax:
  a | b               pipe            a ; b       run in sequence
  a && b              b if a succeeds a || b      b if a fails
  < f  > f  >> f      redirect stdin / stdout (truncate / append)
  2> f  2>> f  2>&1   redirect stderr
  << EOF  <<- EOF     here-document ('EOF' or \"EOF\": no $ expansion)
  'x'  \"x $V\"  \\x    quoting         $V ${V} $?  variables, last status
  !!  !n  !-n  !abc   history expansion
  # comment

keys and signals:
  Up / Down           browse history (also Ctrl+P / Ctrl+N)
  Left / Right        move (Ctrl+B / Ctrl+F; Ctrl+Left / Ctrl+Right by word)
  Home / End          start / end of line (also Ctrl+A / Ctrl+E)
  Ctrl+U / K / W      delete to start / to end / the previous word
  Ctrl+L              clear the screen
  Ctrl+C              cancel the line, or interrupt the running command
  Ctrl+D              exit at an empty prompt
  Ctrl+\\              quit (abort) the running command
  SIGTERM / SIGHUP    forwarded to the running command; the shell exits
";

/// The result of a builtin.
#[derive(Debug, Default)]
pub struct Outcome {
    pub out: Vec<u8>,
    pub err: Vec<u8>,
    pub status: i32,
    /// `exit`: leave the shell with this code.
    pub exit: Option<i32>,
}

impl Outcome {
    fn ok(out: String) -> Self {
        Self {
            out: out.into_bytes(),
            ..Self::default()
        }
    }

    fn fail(status: i32, msg: String) -> Self {
        Self {
            err: format!("shino: {msg}\n").into_bytes(),
            status,
            ..Self::default()
        }
    }
}

/// A shell variable name: `[A-Za-z_][A-Za-z0-9_]*`.
pub fn valid_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c: char| -> bool { c.is_ascii_alphabetic() || c == '_' })
        && name
            .chars()
            .all(|c: char| -> bool { c.is_ascii_alphanumeric() || c == '_' })
}

pub fn is_builtin(name: &str) -> bool {
    NAMES.contains(&name)
}

/// Runs builtin `argv[0]`. With `effects` false (inside a pipeline), it
/// does not change the shell's state, like a bash subshell.
pub fn run(sh: &Sh, argv: &[String], effects: bool) -> Outcome {
    let mut s: std::cell::RefMut<'_, Shell> = sh.borrow_mut();
    let args: &[String] = &argv[1..];
    match argv[0].as_str() {
        "cd" => cd(&mut s, args, effects),
        "pwd" => match std::env::current_dir() {
            Ok(dir) => Outcome::ok(format!("{}\n", dir.display())),
            Err(e) => Outcome::fail(1, format!("pwd: {}", os_msg(&e))),
        },
        "echo" => echo(args),
        "exit" => exit(&s, args, effects),
        "history" => history(&mut s, args, effects),
        "export" => export(&mut s, args, effects),
        "unset" => {
            if effects {
                for name in args {
                    s.vars.insert(name.clone(), None);
                }
            }
            Outcome::default()
        }
        _ => Outcome::ok(String::from(HELP)),
    }
}

fn cd(s: &mut Shell, args: &[String], effects: bool) -> Outcome {
    let dash: bool = args.first().is_some_and(|a: &String| -> bool { a == "-" });
    let target: Option<String> = match args.first() {
        None => s.var("HOME"),
        Some(_) if dash => s.var("OLDPWD"),
        Some(dir) => Some(dir.clone()),
    };
    let Some(target) = target else {
        let which: &str = if dash { "OLDPWD" } else { "HOME" };
        return Outcome::fail(1, format!("cd: {which} not set"));
    };
    if !effects {
        return if Path::new(&target).is_dir() {
            Outcome::default()
        } else {
            Outcome::fail(1, format!("cd: {target}: No such directory"))
        };
    }
    let old: Option<PathBuf> = std::env::current_dir().ok();
    if let Err(e) = std::env::set_current_dir(&target) {
        return Outcome::fail(1, format!("cd: {target}: {}", os_msg(&e)));
    }
    if let Some(old) = old {
        s.vars
            .insert(String::from("OLDPWD"), Some(old.display().to_string()));
    }
    let new: String = std::env::current_dir()
        .map(|p: PathBuf| -> String { p.display().to_string() })
        .unwrap_or(target);
    s.vars.insert(String::from("PWD"), Some(new.clone()));
    if dash {
        Outcome::ok(format!("{new}\n"))
    } else {
        Outcome::default()
    }
}

fn echo(args: &[String]) -> Outcome {
    let (newline, words): (bool, &[String]) = match args.first() {
        Some(flag) if flag == "-n" => (false, &args[1..]),
        _ => (true, args),
    };
    let mut out: String = words.join(" ");
    if newline {
        out.push('\n');
    }
    Outcome::ok(out)
}

fn exit(s: &Shell, args: &[String], effects: bool) -> Outcome {
    let code: Result<i32, String> = match args {
        [] => Ok(s.status),
        [n] => n
            .parse::<i32>()
            .map(|n: i32| -> i32 { n.rem_euclid(256) })
            .map_err(|_: std::num::ParseIntError| -> String {
                format!("exit: {n}: numeric argument required")
            }),
        _ => return Outcome::fail(1, String::from("exit: too many arguments")),
    };
    match code {
        Ok(code) => Outcome {
            status: code,
            exit: effects.then_some(code),
            ..Outcome::default()
        },
        Err(msg) => Outcome {
            exit: effects.then_some(2),
            ..Outcome::fail(2, msg)
        },
    }
}

fn history(s: &mut Shell, args: &[String], effects: bool) -> Outcome {
    match args.first().map(String::as_str) {
        None => Outcome::ok(s.history.listing()),
        Some("-c") => {
            if effects {
                s.history.clear();
            }
            Outcome::default()
        }
        Some(n) => match n.parse::<usize>() {
            Ok(n) => {
                let listing: String = s.history.listing();
                let lines: Vec<&str> = listing.lines().collect::<Vec<&str>>();
                let tail: &[&str] = &lines[lines.len().saturating_sub(n)..];
                Outcome::ok(
                    tail.iter()
                        .map(|l: &&str| -> String { format!("{l}\n") })
                        .collect::<String>(),
                )
            }
            Err(_) => Outcome::fail(2, format!("history: {n}: numeric argument required")),
        },
    }
}

fn export(s: &mut Shell, args: &[String], effects: bool) -> Outcome {
    if args.is_empty() {
        let mut vars: Vec<(String, String)> = std::env::vars()
            .filter(|(name, _): &(String, String)| -> bool { !s.vars.contains_key(name) })
            .collect::<Vec<(String, String)>>();
        vars.extend(s.vars.iter().filter_map(
            |(name, value): (&String, &Option<String>)| -> Option<(String, String)> {
                value
                    .as_ref()
                    .map(|v: &String| -> (String, String) { (name.clone(), v.clone()) })
            },
        ));
        vars.sort();
        return Outcome::ok(
            vars.iter()
                .map(|(name, value): &(String, String)| -> String { format!("{name}={value}\n") })
                .collect::<String>(),
        );
    }
    let mut outcome: Outcome = Outcome::default();
    for arg in args {
        let (name, value): (&str, Option<&str>) = match arg.split_once('=') {
            Some((name, value)) => (name, Some(value)),
            None => (arg.as_str(), None),
        };
        if !valid_name(name) {
            outcome = Outcome::fail(1, format!("export: `{arg}`: not a valid identifier"));
            continue;
        }
        if effects && let Some(value) = value {
            s.vars.insert(String::from(name), Some(String::from(value)));
        }
    }
    outcome
}
