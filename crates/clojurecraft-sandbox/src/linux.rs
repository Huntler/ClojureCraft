//! Linux self-restriction: rlimits, Landlock, and seccomp. Applied by the helper to itself just
//! before it executes the target program.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use landlock::{
    ABI, Access, AccessFs, AccessNet, CompatLevel, Compatible, PathBeneath, PathFd, Ruleset, RulesetAttr,
    RulesetCreatedAttr, RulesetStatus, Scope,
};
use rustix::process::{Resource, Rlimit, setrlimit};
use seccompiler::{
    BpfProgram, SeccompAction, SeccompCmpArgLen, SeccompCmpOp, SeccompCondition, SeccompFilter, SeccompRule,
    TargetArch,
};

use crate::Policy;

#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("setting resource limit {0}: {1}")]
    Rlimit(&'static str, rustix::io::Errno),
    #[error("setting no_new_privs: {0}")]
    NoNewPrivs(rustix::io::Errno),
    #[error("Landlock: {0}")]
    Landlock(String),
    #[error("Landlock is not available on this kernel; refusing to run untrusted code without it")]
    LandlockUnavailable,
    #[error("seccomp: {0}")]
    Seccomp(String),
}

/// What the kernel actually enforced, for diagnostics.
#[derive(Debug, Clone)]
pub struct Applied {
    pub landlock: &'static str,
}

/// The newest Landlock ABI this build knows. Older kernels enforce a subset (best effort), but
/// filesystem restriction itself (ABI 1, Linux 5.13) is required.
const LANDLOCK_ABI: ABI = ABI::V6;

pub fn apply(policy: &Policy) -> Result<Applied, SetupError> {
    set_rlimits(policy)?;
    rustix::thread::set_no_new_privs(true).map_err(SetupError::NoNewPrivs)?;
    let landlock = apply_landlock(policy)?;
    apply_seccomp()?;
    Ok(Applied { landlock })
}

fn limit(resource: Resource, name: &'static str, value: u64) -> Result<(), SetupError> {
    setrlimit(
        resource,
        Rlimit {
            current: Some(value),
            maximum: Some(value),
        },
    )
    .map_err(|e| SetupError::Rlimit(name, e))
}

fn set_rlimits(policy: &Policy) -> Result<(), SetupError> {
    // The hard CPU limit sits one second above the soft limit so the kernel's SIGXCPU is
    // followed by SIGKILL if the process ignores it.
    setrlimit(
        Resource::Cpu,
        Rlimit {
            current: Some(policy.cpu_seconds),
            maximum: Some(policy.cpu_seconds + 1),
        },
    )
    .map_err(|e| SetupError::Rlimit("CPU", e))?;
    limit(Resource::Core, "CORE", 0)?;
    // No regular file may be written or grown, even if a path were writable.
    limit(Resource::Fsize, "FSIZE", 0)?;
    limit(Resource::Nofile, "NOFILE", policy.max_open_files)?;
    if let Some(bytes) = policy.address_space_bytes {
        limit(Resource::As, "AS", bytes)?;
    }
    Ok(())
}

fn existing(paths: &[PathBuf]) -> impl Iterator<Item = &Path> {
    paths.iter().map(PathBuf::as_path).filter(|p| p.exists())
}

fn apply_landlock(policy: &Policy) -> Result<&'static str, SetupError> {
    let ll = |e: landlock::RulesetError| SetupError::Landlock(e.to_string());
    let abi = LANDLOCK_ABI;
    let read = AccessFs::ReadFile | AccessFs::ReadDir;
    let exec = read | AccessFs::Execute;
    let mut ruleset = Ruleset::default()
        // Filesystem restriction is mandatory; newer features are applied when available.
        .set_compatibility(CompatLevel::HardRequirement)
        .handle_access(AccessFs::from_all(ABI::V1))
        .map_err(ll)?
        .set_compatibility(CompatLevel::BestEffort)
        .handle_access(AccessFs::from_all(abi))
        .map_err(ll)?
        .handle_access(AccessNet::from_all(abi))
        .map_err(ll)?
        .scope(Scope::from_all(abi))
        .map_err(ll)?
        .create()
        .map_err(ll)?;
    let open = |p: &Path| PathFd::new(p).map_err(|e| SetupError::Landlock(format!("{}: {e}", p.display())));
    for p in existing(&policy.exec) {
        ruleset = ruleset.add_rule(PathBeneath::new(open(p)?, exec)).map_err(ll)?;
    }
    for p in existing(&policy.read) {
        // Rules on regular files may only carry file rights.
        let access = if p.is_dir() {
            read
        } else {
            AccessFs::ReadFile.into()
        };
        ruleset = ruleset.add_rule(PathBeneath::new(open(p)?, access)).map_err(ll)?;
    }
    let dev_null = Path::new("/dev/null");
    ruleset = ruleset
        .add_rule(PathBeneath::new(
            open(dev_null)?,
            AccessFs::ReadFile | AccessFs::WriteFile,
        ))
        .map_err(ll)?;
    let status = ruleset.restrict_self().map_err(ll)?;
    match status.ruleset {
        RulesetStatus::FullyEnforced => Ok("fully enforced"),
        RulesetStatus::PartiallyEnforced => Ok("partially enforced (older kernel ABI)"),
        RulesetStatus::NotEnforced => Err(SetupError::LandlockUnavailable),
    }
}

fn target_arch() -> Result<TargetArch, SetupError> {
    std::env::consts::ARCH
        .try_into()
        .map_err(|e| SetupError::Seccomp(format!("unsupported architecture: {e:?}")))
}

/// Builds the filter. Matching calls fail with `ENOSYS`, which glibc treats as "use the older
/// interface" for `clone3`; thread creation then proceeds through `clone(CLONE_THREAD)`.
pub fn seccomp_program() -> Result<BpfProgram, SetupError> {
    fn se(e: impl std::fmt::Display) -> SetupError {
        SetupError::Seccomp(e.to_string())
    }
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    let deny_always = [
        libc::SYS_socket,
        libc::SYS_socketpair,
        libc::SYS_clone3,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
        libc::SYS_mount,
        libc::SYS_umount2,
        libc::SYS_pivot_root,
        libc::SYS_chroot,
        libc::SYS_unshare,
        libc::SYS_setns,
        libc::SYS_keyctl,
        libc::SYS_add_key,
        libc::SYS_request_key,
        libc::SYS_bpf,
        libc::SYS_perf_event_open,
        libc::SYS_userfaultfd,
        libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter,
        libc::SYS_io_uring_register,
        libc::SYS_kexec_load,
        libc::SYS_init_module,
        libc::SYS_finit_module,
        libc::SYS_delete_module,
        libc::SYS_reboot,
        libc::SYS_swapon,
        libc::SYS_swapoff,
        libc::SYS_acct,
        libc::SYS_quotactl,
        libc::SYS_open_by_handle_at,
        libc::SYS_name_to_handle_at,
    ];
    for sys in deny_always {
        rules.insert(sys, vec![]);
    }
    #[cfg(target_arch = "x86_64")]
    for sys in [libc::SYS_fork, libc::SYS_vfork] {
        rules.insert(sys, vec![]);
    }
    // clone without CLONE_THREAD creates a process; with it, a thread (which the JVM needs).
    #[allow(clippy::cast_sign_loss)]
    let clone_thread = libc::CLONE_THREAD as u64;
    let new_process = SeccompCondition::new(
        0,
        SeccompCmpArgLen::Qword,
        SeccompCmpOp::MaskedEq(clone_thread),
        0,
    )
    .map_err(se)?;
    rules.insert(
        libc::SYS_clone,
        vec![SeccompRule::new(vec![new_process]).map_err(se)?],
    );

    #[allow(clippy::cast_sign_loss)]
    let enosys = libc::ENOSYS as u32;
    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow,
        SeccompAction::Errno(enosys),
        target_arch()?,
    )
    .map_err(se)?;
    filter.try_into().map_err(se)
}

fn apply_seccomp() -> Result<(), SetupError> {
    let program = seccomp_program()?;
    seccompiler::apply_filter(&program).map_err(|e| SetupError::Seccomp(e.to_string()))
}
