//! Sandbox mode (`shino --sandbox [FILE]`), in two layers:
//!
//! 1. **shino's checks** ([`Policy`]): every external command is checked
//!    against the allowlist or denylist, including the commands it wraps
//!    (`sh -c`, `env`, `xargs`, `find -exec`, ...); redirections may only
//!    write to allowed paths; `cd` stays inside the sandbox root. These give
//!    clear errors but can't see inside programs.
//! 2. **the kernel** ([`enforce`]): before the first prompt shino restricts
//!    itself with Landlock (which programs may run, which paths may be read
//!    or written, which TCP ports may be used) and resource limits, and sets
//!    no_new_privs (so setuid tools like sudo can't gain privileges).
//!    Children inherit all of it and cannot undo it.

mod config;
mod enforce;
mod policy;

use std::path::{Path, PathBuf};

pub use config::DEFAULT_CONFIG;
pub use policy::Policy;

/// Loads the policy (from `file`, else `~/.config/shino/sandbox.toml` if it
/// exists, else the defaults), moves into its root and locks the process
/// down. Irreversible.
pub fn setup(file: Option<&Path>, history: Option<&Path>) -> Result<Policy, String> {
    let file: Option<PathBuf> = file.map(Path::to_path_buf).or_else(|| -> Option<PathBuf> {
        let default: PathBuf =
            PathBuf::from(std::env::var_os("HOME")?).join(".config/shino/sandbox.toml");
        default.is_file().then_some(default)
    });
    let config: config::Config = config::load(file.as_deref())?;
    let start: PathBuf =
        std::env::current_dir().map_err(|e: std::io::Error| -> String { e.to_string() })?;
    let mut policy: Policy = Policy::new(config, &start)?;
    std::env::set_current_dir(&policy.root).map_err(|e: std::io::Error| -> String {
        format!("root: {}: {e}", policy.root.display())
    })?;
    // The kernel rule for the history file needs the file to exist.
    if let Some(path) = history {
        let _: std::io::Result<std::fs::File> = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path);
    }
    policy.enforcement = enforce::enforce(&policy, history)?;
    Ok(policy)
}
