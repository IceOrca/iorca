//! End-to-end tests for `shino --sandbox`: shino's own checks (level 1)
//! and what the kernel enforces on top (level 2: Landlock and rlimits).
//! The kernel-level assertions are skipped where Landlock is unavailable.

use std::io::Read;
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::Duration;

const SHINO: &str = env!("CARGO_BIN_EXE_shino");

/// A fresh scratch directory holding `sb/` (the sandbox root) and the
/// policy file.
fn scratch(name: &str) -> PathBuf {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("shino-sandbox-{}-{name}", std::process::id()));
    let _: std::io::Result<()> = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("sb")).unwrap();
    dir
}

/// Runs `shino --sandbox <dir>/policy.toml -c script` in `<dir>/sb` with
/// `policy` as the policy file; returns (exit code, stdout, stderr).
fn run(dir: &Path, policy: &str, script: &str) -> (i32, String, String) {
    let file: PathBuf = dir.join("policy.toml");
    std::fs::write(&file, policy).unwrap();
    let out: Output = Command::new(SHINO)
        .arg("--sandbox")
        .arg(&file)
        .args(["-c", script])
        .current_dir(dir.join("sb"))
        .env_remove("SHINO_HISTFILE")
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// Whether this kernel enforces Landlock (filesystem and TCP).
fn kernel_enforced(dir: &Path) -> bool {
    let (_, out, _): (i32, String, String) = run(dir, "", "sandbox");
    let enforced: bool = out.contains("kernel:    kernel-enforced by Landlock");
    if !enforced {
        eprintln!("Landlock unavailable, skipping kernel checks:\n{out}");
    }
    enforced
}

#[test]
fn allowlist_blocks_unlisted_programs() {
    let dir: PathBuf = scratch("allow");
    std::fs::write(dir.join("sb/a"), "hi\n").unwrap();
    let (code, out, err): (i32, String, String) = run(
        &dir,
        "",
        "cat a; printenv HOME; echo st=$?; /usr/bin/printenv HOME; env printenv HOME",
    );
    assert_eq!(out, "hi\nst=126\n");
    assert!(
        err.contains("shino: printenv: not in the sandbox allowlist"),
        "{err}"
    );
    assert!(
        err.contains("shino: /usr/bin/printenv: not in the sandbox allowlist"),
        "{err}"
    );
    assert_eq!(
        err.matches("not in the sandbox allowlist").count(),
        3,
        "{err}"
    );
    assert_eq!(code, 126);

    // Hidden from shino's checks, so the kernel stops it.
    if kernel_enforced(&dir) {
        let (_, out, err): (i32, String, String) =
            run(&dir, "", "sh -c 'c=printenv; $c HOME'; echo st=$?");
        assert_eq!(out, "st=126\n");
        assert!(err.contains("Permission denied"), "{err}");
    }
}

#[test]
fn denylist_blocks_only_the_rules() {
    let dir: PathBuf = scratch("deny");
    for name in ["a", "b"] {
        std::fs::write(dir.join("sb").join(name), "").unwrap();
    }
    let (_, out, err): (i32, String, String) = run(
        &dir,
        "mode = \"blacklist\"\n",
        "printenv SHINO_NOT_SET || echo printenv-ran; rm a; rm -rf b; rm --force b; sh -c 'rm -f b'; \
         nice -n 1 rm -f b; find . -name b -exec rm -f {} +; sudo true; echo st=$?; ls",
    );
    assert_eq!(out, "printenv-ran\nst=126\nb\n");
    assert_eq!(
        err.matches("blocked by the sandbox (rule `rm -f`)").count(),
        4,
        "{err}"
    );
    assert!(
        err.contains("blocked by the sandbox (rule `rm --force`)"),
        "{err}"
    );
    assert!(
        err.contains("shino: sudo: blocked by the sandbox (rule `sudo`)"),
        "{err}"
    );
}

#[test]
fn writes_and_cd_stay_inside() {
    let dir: PathBuf = scratch("files");
    std::fs::create_dir(dir.join("sb/sub")).unwrap();
    std::fs::create_dir(dir.join("outside")).unwrap();
    let policy: &str = "[files]\nwrite = [\"/dev/null\"]\n";
    let outside: String = dir.join("outside/f").display().to_string();
    let (_, out, err): (i32, String, String) = run(
        &dir,
        policy,
        &format!(
            "echo x > {outside}; echo st=$?; echo y >> ../outside/f; echo ok > sub/f; cat sub/f; cd /; cd sub; pwd"
        ),
    );
    assert_eq!(out, format!("st=1\nok\n{}\n", dir.join("sb/sub").display()));
    assert!(
        err.contains(&format!(
            "shino: {outside}: outside the sandbox's writable paths"
        )),
        "{err}"
    );
    assert!(
        err.contains("shino: ../outside/f: outside the sandbox's writable paths"),
        "{err}"
    );
    assert!(err.contains("cd: /: outside the sandbox root"), "{err}");
    assert!(!dir.join("outside/f").exists());

    if kernel_enforced(&dir) {
        let (_, out, err): (i32, String, String) = run(
            &dir,
            policy,
            &format!("sh -c 'echo x > {outside}'; echo st=$?; cp sub/f {outside}"),
        );
        assert_eq!(out, "st=2\n");
        assert_eq!(err.matches("Permission denied").count(), 2, "{err}");
        assert!(!dir.join("outside/f").exists());
    }
}

#[test]
fn tcp_connect_follows_the_port_list() {
    let dir: PathBuf = scratch("net");
    if !kernel_enforced(&dir) {
        return;
    }
    let listener: TcpListener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port: u16 = listener.local_addr().unwrap().port();
    let script: String = format!("bash -c 'echo hello > /dev/tcp/127.0.0.1/{port}' && echo sent");

    let (_, out, _): (i32, String, String) =
        run(&dir, &format!("[network]\nconnect = [{port}]\n"), &script);
    assert_eq!(out, "sent\n");
    let (mut stream, _): (TcpStream, std::net::SocketAddr) = listener.accept().unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut got: String = String::new();
    stream.read_to_string(&mut got).unwrap();
    assert_eq!(got, "hello\n");

    let (_, out, err): (i32, String, String) =
        run(&dir, "[network]\nconnect = \"none\"\n", &script);
    assert_eq!(out, "");
    assert!(err.contains("Permission denied"), "{err}");
}

#[test]
fn resource_limits_apply_to_children() {
    let dir: PathBuf = scratch("limits");
    let (_, out, _): (i32, String, String) = run(
        &dir,
        "[limits]\nopen_files = 64\ncpu_seconds = 50\nfile_size_mb = 1\n",
        "bash -c 'ulimit -n; ulimit -t; ulimit -f'",
    );
    // bash's `ulimit -f` counts 1024-byte blocks.
    assert_eq!(out, "64\n50\n1024\n");
}

#[test]
fn bad_policies_are_rejected() {
    let dir: PathBuf = scratch("bad");
    let (code, out, err): (i32, String, String) = run(&dir, "[files]\nwrtie = []\n", "echo never");
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.starts_with("shino: sandbox: ") && err.contains("wrtie"),
        "{err}"
    );
    let (code, _, err): (i32, String, String) = run(&dir, "mode = \"greylist\"\n", "echo never");
    assert_eq!(code, 2);
    assert!(err.contains("greylist"), "{err}");
    let (code, _, err): (i32, String, String) =
        run(&dir, "[network]\nbind = \"some\"\n", "echo never");
    assert_eq!(code, 2);
    assert!(err.contains("\"any\" or \"none\""), "{err}");
}

#[test]
fn printed_defaults_round_trip() {
    let dir: PathBuf = scratch("defaults");
    let out: Output = Command::new(SHINO)
        .arg("--sandbox-defaults")
        .output()
        .unwrap();
    assert!(out.status.success());
    let defaults: String = String::from_utf8(out.stdout).unwrap();
    assert!(defaults.contains("mode = \"allowlist\""));
    let (code, out, _): (i32, String, String) = run(&dir, &defaults, "sandbox");
    assert_eq!(code, 0);
    assert!(out.starts_with("sandbox: allowlist mode"), "{out}");
    assert!(
        out.contains("network:   TCP connect ports 22,53,80,443,9418; listen none"),
        "{out}"
    );
}

#[test]
fn sandbox_builtin_reports_off() {
    let out: Output = Command::new(SHINO)
        .args(["-c", "sandbox"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).starts_with("sandbox: off"));
}
