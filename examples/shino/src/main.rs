//! shino: a small interactive shell whose I/O runs on the iorca event loop.
//!
//! ```text
//! shino                 interactive (prompt, history file, Ctrl+C handling)
//! shino -c 'commands'  run commands, then exit with the last status
//! shino --sandbox [F]  restricted mode (policy file F; see --sandbox-defaults)
//! cmds | shino          run commands from stdin
//! ```
//!
//! On a terminal, the prompt has line editing: ↑/↓ browse history, ←/→,
//! Home/End and the usual Ctrl keys edit. History is kept in
//! `$SHINO_HISTFILE`, or `~/.shino_history` when interactive. Type `help` for
//! the syntax.

mod builtins;
mod edit;
mod exec;
mod history;
mod parse;
mod sandbox;
mod shell;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::rc::Rc;

use iorca::EventLoop;

use crate::sandbox::Policy;
use crate::shell::Sh;

const USAGE: &str = "usage: shino [--sandbox [FILE]] [-c COMMAND]\n       shino --sandbox-defaults";

/// Command-line options.
struct Options {
    script: Option<String>,
    /// `--sandbox [FILE]`: `Some(None)` uses the default policy file.
    sandbox: Option<Option<PathBuf>>,
}

fn main() {
    let options: Options = parse_args(std::env::args().skip(1).collect::<Vec<String>>())
        .unwrap_or_else(|msg: String| -> Options {
            eprintln!("{msg}");
            std::process::exit(2);
        });
    match run(options) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("shino: {e}");
            std::process::exit(1);
        }
    }
}

fn parse_args(args: Vec<String>) -> Result<Options, String> {
    let mut options: Options = Options {
        script: None,
        sandbox: None,
    };
    let mut iter: std::iter::Peekable<std::vec::IntoIter<String>> = args.into_iter().peekable();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-c" => {
                options.script = Some(
                    iter.next()
                        .ok_or_else(|| -> String { String::from(USAGE) })?,
                )
            }
            "--sandbox" => {
                let file: Option<PathBuf> = iter
                    .next_if(|next: &String| -> bool { !next.starts_with('-') })
                    .map(PathBuf::from);
                options.sandbox = Some(file);
            }
            "--sandbox-defaults" => {
                print!("{}", sandbox::DEFAULT_CONFIG);
                std::process::exit(0);
            }
            _ => return Err(String::from(USAGE)),
        }
    }
    Ok(options)
}

fn run(options: Options) -> io::Result<i32> {
    let Options {
        script,
        sandbox: sandbox_file,
    } = options;
    let interactive: bool = script.is_none() && io::stdin().is_terminal();
    let hist_path: Option<PathBuf> = std::env::var_os("SHINO_HISTFILE")
        .map(PathBuf::from)
        .or_else(|| -> Option<PathBuf> {
            let home: PathBuf = PathBuf::from(std::env::var_os("HOME")?);
            interactive.then(|| -> PathBuf { home.join(".shino_history") })
        });
    // Locks the process down before anything runs (irreversible).
    let policy: Option<Rc<Policy>> = match sandbox_file {
        None => None,
        Some(file) => match sandbox::setup(file.as_deref(), hist_path.as_deref()) {
            Ok(policy) => Some(Rc::new(policy)),
            Err(msg) => {
                eprintln!("shino: sandbox: {msg}");
                std::process::exit(2);
            }
        },
    };
    let mut el: EventLoop = EventLoop::new()?;
    let sh: Sh = shell::Shell::new(interactive, script, hist_path, policy);
    shell::start(&mut el, &sh)?;
    // Signal watchers are always in flight, so this returns only once
    // `shutdown` stops the loop.
    el.run()?;
    let code: i32 = sh.borrow().exit_code;
    Ok(code)
}
