//! iosh: a small interactive shell whose I/O runs on the iorca event loop.
//!
//! ```text
//! iosh                 interactive (prompt, history file, Ctrl+C handling)
//! iosh -c 'commands'   run commands, then exit with the last status
//! cmds | iosh          run commands from stdin
//! ```
//!
//! On a terminal, the prompt has line editing: ↑/↓ browse history, ←/→,
//! Home/End and the usual Ctrl keys edit. History is kept in
//! `$IOSH_HISTFILE`, or `~/.iosh_history` when interactive. Type `help` for
//! the syntax.

mod builtins;
mod edit;
mod exec;
mod history;
mod parse;
mod shell;

use std::io::{self, IsTerminal};
use std::path::PathBuf;

use iorca::EventLoop;

use crate::shell::Sh;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect::<Vec<String>>();
    let script: Option<String> = match args.as_slice() {
        [] => None,
        [flag, command] if flag == "-c" => Some(command.clone()),
        _ => {
            eprintln!("usage: iosh [-c COMMAND]");
            std::process::exit(2);
        }
    };
    match run(script) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("iosh: {e}");
            std::process::exit(1);
        }
    }
}

fn run(script: Option<String>) -> io::Result<i32> {
    let interactive: bool = script.is_none() && io::stdin().is_terminal();
    let hist_path: Option<PathBuf> = std::env::var_os("IOSH_HISTFILE")
        .map(PathBuf::from)
        .or_else(|| -> Option<PathBuf> {
            let home: PathBuf = PathBuf::from(std::env::var_os("HOME")?);
            interactive.then(|| -> PathBuf { home.join(".iosh_history") })
        });
    let mut el: EventLoop = EventLoop::new()?;
    let sh: Sh = shell::Shell::new(interactive, script, hist_path);
    shell::start(&mut el, &sh)?;
    // Signal watchers are always in flight, so this returns only once
    // `shutdown` stops the loop.
    el.run()?;
    let code: i32 = sh.borrow().exit_code;
    Ok(code)
}
