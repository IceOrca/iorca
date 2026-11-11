//! The resolved sandbox policy and shino's own checks (level 1): commands
//! against the allow/deny rules, redirection targets, and `cd`.
//!
//! These checks give clear messages; the kernel layer ([`super::enforce`])
//! is what makes the policy hold for everything else (programs started by
//! other programs, scripts, compiled code).

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use super::config::{Config, Limits, Mode, Ports, expand_path};
use crate::builtins::valid_name;
use crate::parse::{self, Connector, Env, Pipeline};

/// How deep `sh -c '... sh -c ...'` and wrapper nesting is followed.
const MAX_DEPTH: usize = 4;

/// The sandbox policy, with paths resolved at startup.
#[derive(Debug)]
pub struct Policy {
    pub mode: Mode,
    /// Canonical sandbox directory.
    pub root: PathBuf,
    /// Allowlist: program names.
    allow: HashSet<String>,
    /// Allowlist: the programs `allow` resolved to (canonical), for the
    /// kernel rules.
    pub allowed_programs: Vec<PathBuf>,
    deny: Vec<DenyRule>,
    pub exec_dirs: Vec<PathBuf>,
    pub exec_in_root: bool,
    pub read: Vec<PathBuf>,
    /// Writable paths besides `root` (canonical, existing).
    pub write: Vec<PathBuf>,
    pub cd_outside_root: bool,
    pub connect: Ports,
    pub bind: Ports,
    pub limits: Limits,
    /// How the kernel layer turned out (set by `enforce`).
    pub enforcement: String,
}

impl Policy {
    /// Resolves `config` relative to `start` (the startup directory).
    pub fn new(config: Config, start: &Path) -> Result<Policy, String> {
        let root_path: PathBuf = expand_path(&config.root, start)
            .ok_or_else(|| -> String { format!("root: cannot expand {:?}", config.root) })?;
        let root: PathBuf = root_path
            .canonicalize()
            .map_err(|e: std::io::Error| -> String {
                format!("root: {}: {e}", root_path.display())
            })?;
        let path_var: Option<String> = std::env::var("PATH").ok();
        let names: Vec<String> = config
            .commands
            .allow
            .iter()
            .chain(config.commands.allow_more.iter())
            .cloned()
            .collect::<Vec<String>>();
        let mut programs: Vec<PathBuf> = names
            .iter()
            .filter_map(|name: &String| -> Option<PathBuf> { resolve(name, path_var.as_deref()) })
            .filter_map(|p: PathBuf| -> Option<PathBuf> { p.canonicalize().ok() })
            .collect::<Vec<PathBuf>>();
        programs.sort();
        programs.dedup();
        let deny: Vec<DenyRule> = config
            .commands
            .deny
            .iter()
            .chain(config.commands.deny_more.iter())
            .filter_map(|text: &String| -> Option<DenyRule> { DenyRule::parse(text) })
            .collect::<Vec<DenyRule>>();
        let existing = |paths: &[String]| -> Vec<PathBuf> {
            paths
                .iter()
                .filter_map(|raw: &String| -> Option<PathBuf> {
                    expand_path(raw, start)?.canonicalize().ok()
                })
                .collect::<Vec<PathBuf>>()
        };
        Ok(Policy {
            mode: config.mode,
            root,
            allow: names.into_iter().collect::<HashSet<String>>(),
            allowed_programs: programs,
            deny,
            exec_dirs: existing(&config.commands.exec_dirs),
            exec_in_root: config.commands.exec_in_root,
            read: existing(&config.files.read),
            write: existing(&config.files.write),
            cd_outside_root: config.files.cd_outside_root,
            connect: config.network.connect,
            bind: config.network.bind,
            limits: config.limits,
            enforcement: String::from("not enforced yet"),
        })
    }

    /// Checks an external command (and the commands it obviously wraps).
    pub fn check_command(&self, argv: &[String], env: &dyn Env) -> Result<(), String> {
        self.check(argv, env, 0)
    }

    fn check(&self, argv: &[String], env: &dyn Env, depth: usize) -> Result<(), String> {
        let Some(name) = argv.first() else {
            return Ok(());
        };
        let resolved: Option<PathBuf> = resolve(name, env.var("PATH").as_deref());
        // The name the program runs as (`/bin/rm` -> `rm`), not its symlink
        // target: multi-call binaries share one file.
        let program: &str = resolved
            .as_deref()
            .map(base_name)
            .unwrap_or_else(|| -> &str { base_name(Path::new(name)) });
        if let Some(rule) = self
            .deny
            .iter()
            .find(|rule: &&DenyRule| -> bool { rule.matches(program, &argv[1..]) })
        {
            return Err(format!(
                "{name}: blocked by the sandbox (rule `{}`)",
                rule.text
            ));
        }
        if self.mode == Mode::Allowlist
            && let Some(path) = &resolved
            && !self.allow.contains(program)
            && !(self.exec_in_root
                && path
                    .canonicalize()
                    .is_ok_and(|p: PathBuf| -> bool { p.starts_with(&self.root) }))
        {
            return Err(format!("{name}: not in the sandbox allowlist"));
        }
        if depth < MAX_DEPTH {
            for inner in wrapped_commands(program, argv, env) {
                self.check(&inner, env, depth + 1)?;
            }
        }
        Ok(())
    }

    /// Checks that a redirection may write `path`.
    pub fn check_write(&self, path: &Path) -> Result<(), String> {
        let absolute: PathBuf = absolute(path);
        if self.writable(&absolute) {
            Ok(())
        } else {
            Err(format!(
                "{}: outside the sandbox's writable paths",
                path.display()
            ))
        }
    }

    fn writable(&self, absolute: &Path) -> bool {
        // The file may not exist yet: resolve its directory instead.
        let resolved: Option<PathBuf> =
            absolute.canonicalize().ok().or_else(|| -> Option<PathBuf> {
                let parent: PathBuf = absolute.parent()?.canonicalize().ok()?;
                Some(parent.join(absolute.file_name()?))
            });
        resolved.is_some_and(|p: PathBuf| -> bool {
            p.starts_with(&self.root)
                || self
                    .write
                    .iter()
                    .any(|w: &PathBuf| -> bool { p.starts_with(w) })
        })
    }

    /// Checks that `cd` may enter `dir`.
    pub fn check_cd(&self, dir: &Path) -> Result<(), String> {
        if self.cd_outside_root {
            return Ok(());
        }
        let inside: bool = absolute(dir)
            .canonicalize()
            .is_ok_and(|p: PathBuf| -> bool { p.starts_with(&self.root) });
        if inside {
            Ok(())
        } else {
            Err(format!(
                "cd: {}: outside the sandbox root {}",
                dir.display(),
                self.root.display()
            ))
        }
    }

    /// One line for the interactive banner.
    pub fn banner(&self) -> String {
        format!(
            "shino: sandbox on ({}, root {}; {}). Type `sandbox` for details.\n",
            self.mode_text(),
            self.root.display(),
            self.enforcement
        )
    }

    /// The `sandbox` builtin's report.
    pub fn summary(&self) -> String {
        let paths = |list: &[PathBuf]| -> String {
            list.iter()
                .map(|p: &PathBuf| -> String { p.display().to_string() })
                .collect::<Vec<String>>()
                .join(", ")
        };
        let rules: Vec<&str> = self
            .deny
            .iter()
            .map(|r: &DenyRule| -> &str { r.text.as_str() })
            .collect::<Vec<&str>>();
        let mut allowed: Vec<&String> = self.allow.iter().collect::<Vec<&String>>();
        allowed.sort();
        let limit = |value: u64, unit: &str| -> String {
            if value == 0 {
                String::from("unchanged")
            } else {
                format!("{value}{unit}")
            }
        };
        let l: &Limits = &self.limits;
        let mut out: String = format!("sandbox: {}\n", self.mode_text());
        out.push_str(&format!("  root:      {}\n", self.root.display()));
        if self.mode == Mode::Allowlist {
            let names: Vec<&str> = allowed
                .iter()
                .map(|s: &&String| -> &str { s.as_str() })
                .collect::<Vec<&str>>();
            out.push_str(&wrap("  allowed:   ", &names, 78));
        }
        let last: usize = rules.len().saturating_sub(1);
        let rules: Vec<String> = rules
            .iter()
            .enumerate()
            .map(|(i, r): (usize, &&str)| -> String {
                if i < last {
                    format!("{r},")
                } else {
                    String::from(*r)
                }
            })
            .collect::<Vec<String>>();
        out.push_str(&wrap(
            "  denied:    ",
            &rules.iter().map(String::as_str).collect::<Vec<&str>>(),
            78,
        ));
        out.push_str(&format!("  readable:  {}\n", paths(&self.read)));
        out.push_str(&format!("  writable:  root, {}\n", paths(&self.write)));
        out.push_str(&format!(
            "  network:   TCP connect {}; listen {}\n",
            self.connect, self.bind
        ));
        out.push_str(&format!(
            "  limits:    cpu {}, memory {}, file size {}, open files {}, processes {}\n",
            limit(l.cpu_seconds, "s"),
            limit(l.memory_mb, " MB"),
            limit(l.file_size_mb, " MB"),
            limit(l.open_files, ""),
            limit(l.processes, "")
        ));
        out.push_str(&format!("  kernel:    {}\n", self.enforcement));
        out
    }

    pub fn mode_text(&self) -> String {
        match self.mode {
            Mode::Allowlist => format!(
                "allowlist mode, {} allowed programs installed",
                self.allowed_programs.len()
            ),
            Mode::Denylist => format!("denylist mode, {} rules", self.deny.len()),
        }
    }
}

/// A `deny` entry: "command [flags...]".
#[derive(Debug)]
struct DenyRule {
    text: String,
    command: String,
    /// `command*`: prefix match.
    prefix: bool,
    flags: Vec<String>,
}

impl DenyRule {
    fn parse(text: &str) -> Option<DenyRule> {
        let mut words: std::str::SplitWhitespace<'_> = text.split_whitespace();
        let first: &str = words.next()?;
        let (command, prefix): (&str, bool) = match first.strip_suffix('*') {
            Some(stem) => (stem, true),
            None => (first, false),
        };
        Some(DenyRule {
            text: String::from(text.trim()),
            command: String::from(command),
            prefix,
            flags: words.map(String::from).collect::<Vec<String>>(),
        })
    }

    fn matches(&self, program: &str, args: &[String]) -> bool {
        let named: bool = if self.prefix {
            program.starts_with(&self.command)
        } else {
            program == self.command
        };
        named
            && self
                .flags
                .iter()
                .all(|flag: &String| -> bool { has_flag(flag, args) })
    }
}

/// Whether `args` contain `flag`: `--long` (or `--long=..`), a short `-x`
/// alone or in a cluster (`-rx`), or any other word exactly.
fn has_flag(flag: &str, args: &[String]) -> bool {
    let operands_from: usize = args
        .iter()
        .position(|a: &String| -> bool { a == "--" })
        .unwrap_or(args.len());
    let options: &[String] = &args[..operands_from];
    if flag.starts_with("--") {
        let with_value: String = format!("{flag}=");
        return options
            .iter()
            .any(|a: &String| -> bool { a == flag || a.starts_with(&with_value) });
    }
    let mut chars: std::str::Chars<'_> = flag.chars();
    if let (Some('-'), Some(short), None) = (chars.next(), chars.next(), chars.next()) {
        return options.iter().any(|a: &String| -> bool {
            a.starts_with('-') && !a.starts_with("--") && a[1..].contains(short)
        });
    }
    args.iter().any(|a: &String| -> bool { a == flag })
}

/// Commands that `argv` runs on its behalf: the script of `sh -c`, the
/// command of `env`/`nice`/`xargs`/..., and `find -exec` commands.
fn wrapped_commands(program: &str, argv: &[String], env: &dyn Env) -> Vec<Vec<String>> {
    let args: &[String] = &argv[1..];
    match program {
        "sh" | "bash" | "dash" | "zsh" | "ksh" => shell_script(args, env),
        "find" => find_exec(args),
        _ => match wrapper_options(program) {
            Some((value_options, positionals)) => {
                let mut i: usize = 0;
                while i < args.len() && args[i].starts_with('-') && args[i] != "-" {
                    let takes_value: bool = value_options.contains(&args[i].as_str());
                    i += if takes_value { 2 } else { 1 };
                    if args
                        .get(i - 1)
                        .is_some_and(|a: &String| -> bool { a == "--" })
                    {
                        break;
                    }
                }
                let rest: Vec<String> =
                    strip_assignments(args.get(i + positionals..).unwrap_or(&[]).to_vec());
                if rest.is_empty() {
                    Vec::new()
                } else {
                    vec![rest]
                }
            }
            None => Vec::new(),
        },
    }
}

/// Options taking a value, and positional arguments before the command,
/// of programs that run another command.
fn wrapper_options(program: &str) -> Option<(&'static [&'static str], usize)> {
    match program {
        "env" => Some((&["-u", "-C", "--unset", "--chdir"], 0)),
        "nice" => Some((&["-n", "--adjustment"], 0)),
        "nohup" | "setsid" | "command" | "exec" | "builtin" => Some((&[], 0)),
        "time" => Some((&["-f", "-o", "--format", "--output"], 0)),
        "timeout" => Some((&["-s", "-k", "--signal", "--kill-after"], 1)),
        "stdbuf" => Some((&["-i", "-o", "-e"], 0)),
        "xargs" => Some((
            &[
                "-n",
                "-I",
                "-d",
                "-P",
                "-L",
                "-s",
                "-a",
                "-E",
                "--max-args",
                "--delimiter",
            ],
            0,
        )),
        _ => None,
    }
}

/// `sh [-opts] -c 'script' ...`: the script's commands.
fn shell_script(args: &[String], env: &dyn Env) -> Vec<Vec<String>> {
    let mut i: usize = 0;
    let mut has_c: bool = false;
    while i < args.len() && (args[i].starts_with('-') || args[i].starts_with('+')) && args[i] != "-"
    {
        if args[i] == "--" {
            i += 1;
            break;
        }
        if !args[i].starts_with("--") && args[i].contains('c') {
            has_c = true;
        }
        // `-o option` / `+o option` take a value.
        i += if args[i] == "-o" || args[i] == "+o" {
            2
        } else {
            1
        };
    }
    let Some(script) = args.get(i).filter(|_: &&String| -> bool { has_c }) else {
        // A script file: only the kernel layer can see inside.
        return Vec::new();
    };
    let Ok(list) = parse::parse(script) else {
        return Vec::new();
    };
    list.items
        .iter()
        .flat_map(|(_, pipeline): &(Connector, Pipeline)| pipeline.commands.iter())
        .map(|command: &parse::Command| -> Vec<String> {
            strip_assignments(sh_words(&command.words, env))
        })
        .filter(|argv: &Vec<String>| -> bool { !argv.is_empty() })
        .collect::<Vec<Vec<String>>>()
}

/// Expands words the way `sh` would for checking: unquoted expansions are
/// split on whitespace (shino itself never splits).
fn sh_words(words: &[parse::Word], env: &dyn Env) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in words {
        let text: String = parse::expand(word, env);
        if word.quoted {
            out.push(text);
        } else {
            out.extend(text.split_whitespace().map(String::from));
        }
    }
    out
}

/// `find ... -exec cmd args ;` (also `-execdir`, `-ok`, `-okdir`).
fn find_exec(args: &[String]) -> Vec<Vec<String>> {
    let mut commands: Vec<Vec<String>> = Vec::new();
    let mut i: usize = 0;
    while i < args.len() {
        if matches!(args[i].as_str(), "-exec" | "-execdir" | "-ok" | "-okdir") {
            let start: usize = i + 1;
            let end: usize = args[start..]
                .iter()
                .position(|a: &String| -> bool { a == ";" || a == "+" })
                .map_or(args.len(), |p: usize| -> usize { start + p });
            commands.push(args[start..end].to_vec());
            i = end;
        }
        i += 1;
    }
    commands
}

/// Drops leading `NAME=value` words.
fn strip_assignments(argv: Vec<String>) -> Vec<String> {
    let skip: usize = argv
        .iter()
        .take_while(|w: &&String| -> bool {
            w.split_once('=')
                .is_some_and(|(n, _): (&str, &str)| -> bool { valid_name(n) })
        })
        .count();
    argv[skip..].to_vec()
}

/// Finds the program `name` would run: a path as given, else the first
/// executable match in `$PATH`. Not canonicalized.
fn resolve(name: &str, path_var: Option<&str>) -> Option<PathBuf> {
    if name.contains('/') {
        let path: PathBuf = absolute(Path::new(name));
        return is_executable(&path).then_some(path);
    }
    path_var?
        .split(':')
        .map(|dir: &str| -> PathBuf {
            absolute(&Path::new(if dir.is_empty() { "." } else { dir }).join(name))
        })
        .find(|p: &PathBuf| -> bool { is_executable(p) })
}

fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m: std::fs::Metadata| -> bool {
        m.is_file() && m.permissions().mode() & 0o111 != 0
    })
}

fn absolute(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(
            |_: std::io::Error| -> PathBuf { path.to_path_buf() },
            |cwd: PathBuf| -> PathBuf { cwd.join(path) },
        )
    }
}

fn base_name(path: &Path) -> &str {
    path.file_name()
        .and_then(|n: &std::ffi::OsStr| -> Option<&str> { n.to_str() })
        .unwrap_or("")
}

/// `label` then `words` separated by spaces, wrapped at `width` columns with
/// continuation lines indented under the first word.
fn wrap(label: &str, words: &[&str], width: usize) -> String {
    let indent: String = " ".repeat(label.len());
    let mut out: String = String::from(label);
    let mut column: usize = label.len();
    for (i, word) in words.iter().enumerate() {
        if i > 0 && column + 1 + word.len() > width {
            out.push('\n');
            out.push_str(&indent);
            column = indent.len();
        } else if i > 0 {
            out.push(' ');
            column += 1;
        }
        out.push_str(word);
        column += word.len();
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    struct TestEnv(HashMap<&'static str, String>);

    impl Env for TestEnv {
        fn var(&self, name: &str) -> Option<String> {
            self.0.get(name).cloned()
        }
        fn status(&self) -> i32 {
            0
        }
    }

    fn argv(line: &str) -> Vec<String> {
        line.split_whitespace()
            .map(String::from)
            .collect::<Vec<String>>()
    }

    fn policy(mode: Mode, allow: &[&str]) -> Policy {
        let mut config: Config = super::super::config::load(None).unwrap();
        config.mode = mode;
        config.commands.allow = allow
            .iter()
            .map(|s: &&str| -> String { String::from(*s) })
            .collect::<Vec<String>>();
        config.root = String::from("/tmp");
        Policy::new(config, Path::new("/")).unwrap()
    }

    fn env() -> TestEnv {
        TestEnv(HashMap::from([
            ("PATH", std::env::var("PATH").unwrap()),
            ("C", String::from("rm -f x")),
        ]))
    }

    #[test]
    fn flags_match_alone_in_clusters_and_long() {
        let rule: DenyRule = DenyRule::parse("rm -f").unwrap();
        assert!(rule.matches("rm", &argv("-f x")));
        assert!(rule.matches("rm", &argv("-rf x")));
        assert!(rule.matches("rm", &argv("x -vf")));
        assert!(!rule.matches("rm", &argv("-r x")));
        assert!(!rule.matches("rm", &argv("-- -f")));
        assert!(!rule.matches("rmdir", &argv("-f x")));
        let long: DenyRule = DenyRule::parse("rm --force").unwrap();
        assert!(long.matches("rm", &argv("--force x")));
        let prefix: DenyRule = DenyRule::parse("mkfs*").unwrap();
        assert!(prefix.matches("mkfs.ext4", &argv("/dev/x")));
        assert!(
            DenyRule::parse("sudo")
                .unwrap()
                .matches("sudo", &argv("ls"))
        );
    }

    #[test]
    fn deny_rules_see_through_wrappers() {
        let p: Policy = policy(Mode::Denylist, &[]);
        let e: TestEnv = env();
        for ok in [
            "rm x",
            "ls -f",
            "sh -c 'ls; rm x'",
            "find . -name x -exec rm {} ;",
            "env A=1 ls",
        ] {
            let words: Vec<String> =
                parse::expand_argv(&parse::parse(ok).unwrap().items[0].1.commands[0].words, &e);
            assert_eq!(p.check_command(&words, &e), Ok(()), "{ok}");
        }
        for bad in [
            "rm -rf x",
            "/bin/rm -f x",
            "sudo ls",
            "sh -c 'ls && rm -f x'",
            "bash -ec 'sudo id'",
            "env A=1 rm -f x",
            "nice -n 5 rm -f x",
            "timeout 5 sudo ls",
            "xargs -n 1 rm -f",
            "find . -exec rm -f {} ;",
            "sh -c \"sh -c 'rm -f x'\"",
            "sh -c '$C'",
        ] {
            let words: Vec<String> =
                parse::expand_argv(&parse::parse(bad).unwrap().items[0].1.commands[0].words, &e);
            assert!(
                p.check_command(&words, &e).is_err(),
                "{bad} should be blocked"
            );
        }
    }

    #[test]
    fn allowlist_by_program_name() {
        let p: Policy = policy(Mode::Allowlist, &["ls", "sh"]);
        let e: TestEnv = env();
        assert_eq!(p.check_command(&argv("ls -l"), &e), Ok(()));
        assert_eq!(p.check_command(&argv("/bin/ls"), &e), Ok(()));
        assert_eq!(
            p.check_command(&argv("cat x"), &e).unwrap_err(),
            "cat: not in the sandbox allowlist"
        );
        assert!(p.check_command(&argv("sh -c cat"), &e).is_err());
        // Unknown commands are left to fail as "command not found".
        assert_eq!(p.check_command(&argv("no-such-program-x"), &e), Ok(()));
    }

    #[test]
    fn writes_and_cd_stay_inside() {
        let p: Policy = policy(Mode::Allowlist, &[]);
        assert_eq!(p.check_write(Path::new("/tmp/new-file")), Ok(()));
        assert_eq!(p.check_write(Path::new("/dev/null")), Ok(()));
        assert!(p.check_write(Path::new("/etc/passwd")).is_err());
        assert!(p.check_write(Path::new("/tmp/../etc/x")).is_err());
        assert_eq!(p.check_cd(Path::new("/tmp")), Ok(()));
        assert!(p.check_cd(Path::new("/")).is_err());
    }
}
