//! The sandbox policy file (TOML) and its built-in defaults.
//!
//! A user file only needs the keys it changes: it is merged over
//! [`DEFAULT_CONFIG`] table by table.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The default policy, printed by `shino --sandbox-defaults`.
pub const DEFAULT_CONFIG: &str = r#"# shino sandbox policy. Start shino with `shino --sandbox [FILE]`
# (default FILE: ~/.config/shino/sandbox.toml, if it exists). FILE only needs
# the keys it changes: its tables are merged over these defaults.
# Paths may start with `~` or `$VAR`; relative paths are taken from the
# directory shino starts in. Missing paths and programs are skipped.

# "allowlist" (or "whitelist"): only commands.allow may run.
# "denylist" (or "blacklist"): anything may run except commands.deny.
mode = "allowlist"

# The sandbox directory: shino starts here, `cd` stays inside it, and
# commands may write here (and, in allowlist mode, run programs built here).
root = "."

[commands]
# Allowlist mode: program names, looked up in $PATH at startup. Checked by
# shino and enforced by the kernel (Landlock), so programs started
# indirectly (sh -c, xargs, find -exec, scripts, ...) must be allowed too.
allow = [
    # shells and basics
    "sh", "bash", "dash", "env", "echo", "printf", "true", "false", "test", "[",
    "sleep", "which", "whoami", "id", "date", "uname", "hostname", "clear",
    "tput", "tty", "stty",
    # files
    "cat", "less", "more", "head", "tail", "ls", "tree", "file", "stat", "wc",
    "du", "df", "find", "realpath", "readlink", "basename", "dirname", "pwd",
    "cp", "mv", "mkdir", "rmdir", "rm", "touch", "ln", "chmod", "tee",
    "tar", "gzip", "gunzip", "zcat", "xz", "unxz", "bzip2", "zip", "unzip",
    # text
    "grep", "egrep", "fgrep", "rg", "sed", "awk", "gawk", "mawk", "cut", "tr",
    "sort", "uniq", "diff", "cmp", "comm", "paste", "join", "column", "fmt",
    "fold", "nl", "rev", "tac", "xargs", "jq", "seq", "yes", "base64", "od",
    "xxd", "hexdump", "strings", "md5sum", "sha1sum", "sha256sum",
    # network
    "curl", "wget", "ssh", "scp", "git",
    # development
    "gcc", "g++", "cc", "c++", "cpp", "clang", "clang++", "ld", "as", "ar",
    "nm", "objdump", "strip", "make", "cmake", "pkg-config", "cargo", "rustc",
    "rustup", "rustfmt", "rustdoc", "python3", "python", "pip", "pip3",
    "node", "npm", "go",
    # editors, help, processes
    "vi", "vim", "nano", "man", "ps", "top", "htop", "kill", "nproc", "free",
    "uptime", "time", "timeout", "nice", "nohup",
]
# Appended to `allow` (handy in a small override file).
allow_more = []
# Both modes: "command [flags...]" rules. A rule matches when the program
# name matches (a trailing `*` matches a prefix) and every flag is present;
# short flags also match inside clusters ("-f" matches "-rf"). Checked by
# shino, also inside sh -c, env, xargs, nice, timeout, find -exec, ...
deny = [
    "rm -f", "rm --force", "sudo", "su", "doas", "pkexec", "rmmod", "insmod",
    "modprobe", "mkfs*", "shutdown", "reboot", "poweroff", "halt",
]
# Appended to `deny`.
deny_more = []
# Allowlist mode: directories whose programs allowed commands may run as
# helpers (git-core, the gcc backend, the dynamic loader, Rust toolchains).
exec_dirs = [
    "/usr/lib", "/usr/libexec", "/usr/local/lib", "/usr/local/libexec", "/lib",
    "/lib64", "~/.rustup/toolchains", "$RUSTUP_HOME/toolchains",
]
# Allowlist mode: programs under `root` may run (build scripts, ./a.out).
exec_in_root = true

[files]
# Readable paths. Everything by default; narrow it to hide parts of the system.
read = ["/"]
# Writable paths besides `root` and the history file.
write = [
    "/tmp", "/dev/null", "/dev/zero", "/dev/tty", "/dev/pts", "/dev/shm",
    "~/.cache", "~/.cargo", "$CARGO_HOME",
]
# Let `cd` leave `root` (writes stay restricted either way).
cd_outside_root = false

[network]
# TCP ports commands may connect to: a list, "any" or "none".
# (Landlock covers TCP only: UDP, e.g. plain DNS, is not restricted.)
connect = [22, 53, 80, 443, 9418]
# TCP ports commands may listen on: a list, "any" or "none".
bind = "none"

[limits]
# Per process, for shino and everything it runs. 0 leaves a limit unchanged.
cpu_seconds = 3600
memory_mb = 8192
file_size_mb = 4096
open_files = 4096
# Per user (the kernel does not apply it to root).
processes = 4096
"#;

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub mode: Mode,
    pub root: String,
    pub commands: Commands,
    pub files: Files,
    pub network: Network,
    pub limits: Limits,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[serde(alias = "whitelist")]
    Allowlist,
    #[serde(alias = "blacklist")]
    Denylist,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Commands {
    pub allow: Vec<String>,
    pub allow_more: Vec<String>,
    pub deny: Vec<String>,
    pub deny_more: Vec<String>,
    pub exec_dirs: Vec<String>,
    pub exec_in_root: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    pub read: Vec<String>,
    pub write: Vec<String>,
    pub cd_outside_root: bool,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub connect: Ports,
    pub bind: Ports,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub cpu_seconds: u64,
    pub memory_mb: u64,
    pub file_size_mb: u64,
    pub open_files: u64,
    pub processes: u64,
}

/// TCP ports a rule allows.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(try_from = "RawPorts")]
pub enum Ports {
    Any,
    Nothing,
    List(Vec<u16>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawPorts {
    Word(String),
    List(Vec<u16>),
}

impl TryFrom<RawPorts> for Ports {
    type Error = String;

    fn try_from(raw: RawPorts) -> Result<Self, Self::Error> {
        match raw {
            RawPorts::List(ports) => Ok(Ports::List(ports)),
            RawPorts::Word(word) if word == "any" => Ok(Ports::Any),
            RawPorts::Word(word) if word == "none" => Ok(Ports::Nothing),
            RawPorts::Word(word) => Err(format!(
                "expected a port list, \"any\" or \"none\", not {word:?}"
            )),
        }
    }
}

impl std::fmt::Display for Ports {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ports::Any => write!(f, "any port"),
            Ports::Nothing => write!(f, "none"),
            Ports::List(ports) => {
                let list: Vec<String> = ports.iter().map(u16::to_string).collect::<Vec<String>>();
                write!(f, "ports {}", list.join(","))
            }
        }
    }
}

/// Loads the defaults, merged with `file` if given.
pub fn load(file: Option<&Path>) -> Result<Config, String> {
    let mut table: toml::Table = toml::from_str::<toml::Table>(DEFAULT_CONFIG)
        .map_err(|e: toml::de::Error| -> String { e.to_string() })?;
    let origin: String = match file {
        Some(path) => {
            let text: String = std::fs::read_to_string(path)
                .map_err(|e: std::io::Error| -> String { format!("{}: {e}", path.display()) })?;
            let user: toml::Table =
                toml::from_str::<toml::Table>(&text).map_err(|e: toml::de::Error| -> String {
                    format!("{}: {}", path.display(), e.to_string().trim_end())
                })?;
            merge(&mut table, user);
            path.display().to_string()
        }
        None => String::from("default policy"),
    };
    toml::Value::Table(table)
        .try_into::<Config>()
        .map_err(|e: toml::de::Error| -> String {
            format!("{origin}: {}", e.to_string().trim_end())
        })
}

/// Merges `over` into `base`: nested tables recursively, other values
/// replaced.
fn merge(base: &mut toml::Table, over: toml::Table) {
    for (key, value) in over {
        let replace: Option<toml::Value> = match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(inner)), toml::Value::Table(over_inner)) => {
                merge(inner, over_inner);
                None
            }
            (_, value) => Some(value),
        };
        if let Some(value) = replace {
            base.insert(key, value);
        }
    }
}

/// Expands a leading `~` or `$VAR` and makes `raw` absolute against `base`.
/// `None` if it names an unset variable.
pub fn expand_path(raw: &str, base: &Path) -> Option<PathBuf> {
    let expanded: String = if raw == "~" || raw.starts_with("~/") {
        format!("{}{}", std::env::var("HOME").ok()?, &raw[1..])
    } else if let Some(rest) = raw.strip_prefix('$') {
        let end: usize = rest.find('/').unwrap_or(rest.len());
        let value: String = std::env::var(&rest[..end])
            .ok()
            .filter(|v: &String| -> bool { !v.is_empty() })?;
        format!("{value}{}", &rest[end..])
    } else {
        String::from(raw)
    };
    let path: PathBuf = PathBuf::from(expanded);
    Some(if path.is_relative() {
        base.join(path)
    } else {
        path
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_parse() {
        let config: Config = load(None).unwrap();
        assert_eq!(config.mode, Mode::Allowlist);
        assert!(
            config
                .commands
                .allow
                .iter()
                .any(|c: &String| -> bool { c == "git" })
        );
        assert_eq!(config.network.bind, Ports::Nothing);
        assert_eq!(
            config.network.connect,
            Ports::List(vec![22, 53, 80, 443, 9418])
        );
    }

    #[test]
    fn user_file_merges_over_defaults() {
        let path: PathBuf =
            std::env::temp_dir().join(format!("shino-config-{}.toml", std::process::id()));
        std::fs::write(
            &path,
            "mode = \"blacklist\"\n[network]\nconnect = \"any\"\n[limits]\nopen_files = 64\n",
        )
        .unwrap();
        let config: Config = load(Some(&path)).unwrap();
        assert_eq!(config.mode, Mode::Denylist);
        assert_eq!(config.network.connect, Ports::Any);
        // Untouched keys keep their defaults.
        assert_eq!(config.network.bind, Ports::Nothing);
        assert_eq!(config.limits.open_files, 64);
        assert_eq!(config.limits.cpu_seconds, 3600);
        std::fs::write(&path, "[files]\nwrtie = []\n").unwrap();
        assert!(load(Some(&path)).unwrap_err().contains("wrtie"));
        std::fs::write(&path, "[network]\nbind = \"some\"\n").unwrap();
        assert!(load(Some(&path)).is_err());
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn path_expansion() {
        let base: &Path = Path::new("/start");
        let home: String = std::env::var("HOME").unwrap();
        assert_eq!(
            expand_path("~/x", base),
            Some(PathBuf::from(format!("{home}/x")))
        );
        assert_eq!(expand_path("rel", base), Some(PathBuf::from("/start/rel")));
        assert_eq!(expand_path("$SHINO_SURELY_UNSET/x", base), None);
    }
}
