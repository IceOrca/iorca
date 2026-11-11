//! The kernel layer (level 2): resource limits and a Landlock ruleset that
//! shino applies to itself. Children inherit both, and neither can be
//! lifted again, so they hold for every program the sandbox runs, however
//! it was started.

use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, NetPort, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetError, RulesetStatus,
};
use rustix::process::{Resource, Rlimit};

use super::config::{Limits, Mode, Ports};
use super::policy::Policy;

/// The Landlock ABI shino asks for: filesystem, TCP (v4) and device ioctl
/// (v5) rules. On older kernels the crate drops what is unsupported.
const ABI_VERSION: ABI = ABI::V5;

/// Applies the limits and the ruleset. Returns a description of what the
/// kernel enforces.
pub fn enforce(policy: &Policy, history: Option<&Path>) -> Result<String, String> {
    apply_limits(&policy.limits)?;
    landlock(policy, history).map_err(|e: RulesetError| -> String { format!("landlock: {e}") })
}

fn apply_limits(limits: &Limits) -> Result<(), String> {
    const MB: u64 = 1024 * 1024;
    let wanted: [(Resource, u64, &str); 5] = [
        (Resource::Cpu, limits.cpu_seconds, "cpu_seconds"),
        // RLIMIT_DATA (heap and private mappings) rather than RLIMIT_AS:
        // runtimes that reserve large address ranges keep working.
        (
            Resource::Data,
            limits.memory_mb.saturating_mul(MB),
            "memory_mb",
        ),
        (
            Resource::Fsize,
            limits.file_size_mb.saturating_mul(MB),
            "file_size_mb",
        ),
        (Resource::Nofile, limits.open_files, "open_files"),
        (Resource::Nproc, limits.processes, "processes"),
    ];
    for (resource, value, name) in wanted {
        if value == 0 {
            continue;
        }
        // Never above the current hard limit (lowering it is irreversible,
        // which is the point).
        let current: Rlimit = rustix::process::getrlimit(resource);
        let value: u64 = current
            .maximum
            .map_or(value, |max: u64| -> u64 { value.min(max) });
        rustix::process::setrlimit(
            resource,
            Rlimit {
                current: Some(value),
                maximum: Some(value),
            },
        )
        .map_err(|e: rustix::io::Errno| -> String { format!("limits.{name}: {e}") })?;
    }
    Ok(())
}

fn landlock(policy: &Policy, history: Option<&Path>) -> Result<String, RulesetError> {
    let read: BitFlags<AccessFs> = AccessFs::ReadFile | AccessFs::ReadDir;
    let write: BitFlags<AccessFs> = read | AccessFs::from_write(ABI_VERSION);
    let exec: BitFlags<AccessFs> = read | AccessFs::Execute;

    let mut ruleset: Ruleset = Ruleset::default().handle_access(AccessFs::from_all(ABI_VERSION))?;
    if policy.connect != Ports::Any {
        ruleset = ruleset.handle_access(AccessNet::ConnectTcp)?;
    }
    if policy.bind != Ports::Any {
        ruleset = ruleset.handle_access(AccessNet::BindTcp)?;
    }
    let mut created: RulesetCreated = ruleset.create()?;

    let mut writable: Vec<PathBuf> = vec![policy.root.clone()];
    writable.extend(policy.write.iter().cloned());
    writable.extend(history.map(Path::to_path_buf));
    let executable: Vec<PathBuf> = match policy.mode {
        // Anything may run; files and network stay restricted.
        Mode::Denylist => vec![PathBuf::from("/")],
        Mode::Allowlist => {
            let mut paths: Vec<PathBuf> = policy.allowed_programs.clone();
            paths.extend(policy.exec_dirs.iter().cloned());
            if policy.exec_in_root {
                paths.push(policy.root.clone());
            }
            paths
        }
    };
    for (paths, access) in [
        (&policy.read, read),
        (&writable, write),
        (&executable, exec),
    ] {
        for path in paths {
            // Skips paths that vanished since the policy was resolved.
            let Ok(fd) = PathFd::new(path) else {
                continue;
            };
            // Non-directories only take file rights.
            let access: BitFlags<AccessFs> = if path.is_dir() {
                access
            } else {
                access & AccessFs::from_file(ABI_VERSION)
            };
            created = created.add_rule(PathBeneath::new(fd, access))?;
        }
    }
    for (ports, access) in [
        (&policy.connect, AccessNet::ConnectTcp),
        (&policy.bind, AccessNet::BindTcp),
    ] {
        if let Ports::List(list) = ports {
            for port in list {
                created = created.add_rule(NetPort::new(*port, access))?;
            }
        }
    }

    let status: landlock::RestrictionStatus = created.restrict_self()?;
    let kernel: &str = match status.ruleset {
        RulesetStatus::FullyEnforced => "kernel-enforced by Landlock",
        RulesetStatus::PartiallyEnforced => "partly kernel-enforced (older Landlock)",
        RulesetStatus::NotEnforced => {
            "NOT kernel-enforced (no Landlock): only shino's checks apply"
        }
    };
    let privileges: &str = if status.no_new_privs {
        ", no privilege gain"
    } else {
        ""
    };
    Ok(format!("{kernel}{privileges}"))
}
