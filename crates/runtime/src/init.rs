//! Linux init helper.
//!
//! The daemon launches this helper with `--bundle <path>`. It runs in two
//! stages:
//!
//! 1. **Stage 1** (outside the new PID namespace) creates the namespaces,
//!    applies private mount propagation, bind-mounts the rootfs, sets the
//!    hostname, and forks stage 2. It then waits and forwards signals.
//! 2. **Stage 2** is the first process in the new PID namespace (PID 1). It
//!    chroots, mounts `/proc` in the correct PID namespace, applies identity
//!    and `no_new_privs`, and `exec`s the container process.
//!
//! The split is required because procfs binds to the mounter's *active* PID
//! namespace, not the `pid_ns_for_children` pointer set by `unshare`.
//! Mounting before the fork would expose host PIDs inside the container.
//!
//! Stage 1 stays **single-threaded**. After `unshare(CLONE_NEWPID)`,
//! `pid_ns_for_children` differs from the active PID namespace, and the kernel
//! rejects every `clone(CLONE_THREAD)` with `EINVAL` because a thread cannot
//! join another PID namespace; `unshare` itself also requires a single-threaded
//! caller. Signal forwarding therefore uses a blocked signal mask plus a
//! `signalfd`, with `SIGCHLD` driving the child reap instead of a helper thread.
//!
//! This is intentionally smaller than runc: unsupported OCI fields are
//! rejected by [`crate::oci::ensure_supported`] before the helper starts.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use docker_errors::{Error, Result};
use nix::errno::Errno;
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::prctl::{set_no_new_privs, set_pdeathsig};
use nix::sys::signal::{kill, sigprocmask, SigSet, SigmaskHow, Signal};
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{
    chdir, chroot, getpid, getppid, setgid, setgroups, sethostname, setpgid, setuid, Gid, Pid, Uid,
};

use crate::bundle::Bundle;
use crate::oci::ensure_supported;
use crate::spec::{LinuxNamespaceType, Process, Spec};

const FORWARDED_SIGNALS: [Signal; 8] = [
    Signal::SIGHUP,
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTERM,
    Signal::SIGUSR1,
    Signal::SIGUSR2,
    Signal::SIGALRM,
    Signal::SIGPIPE,
];

/// Stage 1 entry point: create namespaces, fork PID 1, wait, and propagate the
/// container exit code.
pub fn run_stage1(bundle_path: &Path) -> Result<i32> {
    arm_parent_death_signal("stage 1")?;
    let bundle = Bundle::open(bundle_path)?;
    let spec = bundle.spec();
    ensure_supported(spec)?;

    let root = spec
        .root
        .as_ref()
        .ok_or_else(|| Error::invalid_argument("OCI config has no root"))?;
    if !Uid::effective().is_root() {
        return Err(Error::permission_denied(
            "native OCI execution currently requires root (user-namespace/rootless setup is not implemented)",
        ));
    }

    let rootfs = Path::new(&root.path);
    if !rootfs.is_dir() {
        return Err(Error::invalid_argument(format!(
            "rootfs `{}` is not a directory",
            rootfs.display()
        )));
    }

    setpgid(Pid::from_raw(0), Pid::from_raw(0))
        .map_err(|err| Error::internal(format!("setpgid in runtime init: {err}")))?;

    let clone_flags = clone_flags(spec)?;
    unshare(clone_flags)
        .map_err(|err| Error::internal(format!("unshare({clone_flags:?}): {err}")))?;

    mount(
        None::<&str>,
        "/",
        None::<&str>,
        MsFlags::MS_REC | MsFlags::MS_PRIVATE,
        None::<&str>,
    )
    .map_err(|err| Error::internal(format!("making mount propagation private: {err}")))?;

    mount(
        Some(rootfs),
        rootfs,
        None::<&str>,
        MsFlags::MS_BIND | MsFlags::MS_REC,
        None::<&str>,
    )
    .map_err(|err| {
        Error::internal(format!(
            "bind-mounting rootfs `{}`: {err}",
            rootfs.display()
        ))
    })?;

    if !spec.hostname.is_empty() {
        sethostname(&spec.hostname)
            .map_err(|err| Error::internal(format!("setting hostname: {err}")))?;
    }

    // Block the signals stage 1 forwards plus SIGCHLD and consume them from a
    // signalfd. See the module docs for why a forwarding thread is impossible
    // after `unshare(CLONE_NEWPID)`.
    let mut mask = SigSet::empty();
    for signal in FORWARDED_SIGNALS {
        mask.add(signal);
    }
    mask.add(Signal::SIGCHLD);
    sigprocmask(SigmaskHow::SIG_BLOCK, Some(&mask), None)
        .map_err(|err| Error::internal(format!("blocking signals in runtime init: {err}")))?;
    let signal_fd = SignalFd::with_flags(&mask, SfdFlags::SFD_CLOEXEC)
        .map_err(|err| Error::internal(format!("creating the runtime init signalfd: {err}")))?;

    let stage2 = std::env::current_exe()
        .map_err(|err| Error::internal(format!("locating stage 2 executable: {err}")))?;
    // Signals sent while stage 2 is being spawned stay pending on the blocked
    // mask and are consumed by the loop below, so a stop racing with startup
    // is not lost.
    let child = Command::new(stage2)
        .arg("--stage2")
        .arg("--bundle")
        .arg(bundle_path)
        .spawn()
        .map_err(|err| Error::internal(format!("starting stage 2 init: {err}")))?;
    let child_pid = Pid::from_raw(child.id() as i32);

    loop {
        let info = signal_fd
            .read_signal()
            .map_err(|err| Error::internal(format!("reading runtime init signals: {err}")))?;
        let Some(info) = info else { continue };
        let Ok(number) = i32::try_from(info.ssi_signo) else {
            continue;
        };
        let Ok(signal) = Signal::try_from(number) else {
            continue;
        };
        if signal != Signal::SIGCHLD {
            forward(child_pid, signal);
            continue;
        }
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) => continue,
            Ok(WaitStatus::Exited(_, code)) => return Ok(code),
            Ok(WaitStatus::Signaled(_, signal, _)) => return Ok(128 + signal as i32),
            // SIGCHLD is also generated for stop/continue notifications; the
            // child has not exited yet, so keep waiting.
            Ok(_) => continue,
            Err(Errno::ECHILD) => {
                return Err(Error::internal(
                    "stage 2 exited but could not be reaped (ECHILD)",
                ))
            }
            Err(err) => {
                return Err(Error::internal(format!(
                    "waiting for the container process: {err}"
                )))
            }
        }
    }
}

/// Stage 2 entry point. This process must already be PID 1 in the container's
/// PID namespace and share stage 1's mount namespace.
pub fn run_stage2(bundle_path: &Path) -> Result<i32> {
    // Stage 2 is PID 1 of the container's PID namespace. If stage 1 (and with
    // it the daemon) dies, the kernel must tear the namespace down instead of
    // leaving a process the restarted daemon cannot adopt. The setting is
    // dropped when credentials change and preserved by a non-setuid `exec`, so
    // it is armed here for setup and re-armed after `apply_identity` below.
    arm_parent_death_signal("stage 2")?;
    // Stage 1 blocks the signals it forwards (and SIGCHLD) for its signalfd,
    // and `exec` preserves the signal mask. Restore the default before the
    // container process starts so it can be stopped and killed normally.
    sigprocmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
        .map_err(|err| Error::internal(format!("restoring the signal mask in stage 2: {err}")))?;

    let bundle = Bundle::open(bundle_path)?;
    let spec = bundle.spec();
    ensure_supported(spec)?;

    if getpid() != Pid::from_raw(1) {
        return Err(Error::internal(format!(
            "stage 2 is PID {}, not PID 1; refusing to mount /proc outside the container PID namespace",
            getpid()
        )));
    }

    let process = spec
        .process
        .as_ref()
        .ok_or_else(|| Error::invalid_argument("OCI config has no process"))?;
    let root = spec
        .root
        .as_ref()
        .ok_or_else(|| Error::invalid_argument("OCI config has no root"))?;
    let rootfs = Path::new(&root.path);
    if !Uid::effective().is_root() {
        return Err(Error::permission_denied(
            "stage 2 must run as root before dropping to the container user",
        ));
    }

    chroot(rootfs)
        .map_err(|err| Error::internal(format!("chroot to `{}`: {err}", rootfs.display())))?;
    chdir("/").map_err(|err| Error::internal(format!("chdir after chroot: {err}")))?;

    // Now that this process is in the new PID namespace, procfs will be bound
    // to that namespace and cannot reveal host PIDs. Create the mountpoint if
    // the image rootfs omitted it, before a read-only remount.
    let proc_path = Path::new("/proc");
    if !proc_path.is_dir() {
        if proc_path.exists() {
            return Err(Error::invalid_argument(
                "rootfs contains `/proc` but it is not a directory",
            ));
        }
        fs::create_dir(proc_path)
            .map_err(|err| Error::internal(format!("creating `/proc` mountpoint: {err}")))?;
    }
    mount(
        Some("proc"),
        proc_path,
        Some("proc"),
        MsFlags::MS_NOSUID | MsFlags::MS_NOEXEC | MsFlags::MS_NODEV,
        None::<&str>,
    )
    .map_err(|err| Error::internal(format!("mounting /proc: {err}")))?;

    if root.readonly {
        mount(
            None::<&str>,
            "/",
            None::<&str>,
            MsFlags::MS_BIND | MsFlags::MS_REMOUNT | MsFlags::MS_RDONLY,
            None::<&str>,
        )
        .map_err(|err| Error::internal(format!("remounting rootfs read-only: {err}")))?;
    }

    apply_identity(process)?;

    // Changing the effective/filesystem UID or GID clears PR_SET_PDEATHSIG
    // (prctl(2)), so restore it after dropping to the container user and
    // before exec. A non-setuid exec preserves the setting.
    arm_parent_death_signal("stage 2 after the identity change")?;

    let mut command = Command::new(&process.args[0]);
    command
        .args(&process.args[1..])
        .env_clear()
        .envs(environment(process)?)
        .current_dir(&process.cwd);

    // `exec` replaces PID 1 with the container process, preserving the PID
    // namespace's init semantics and signal delivery.
    let error = command.exec();
    Err(Error::internal(format!(
        "executing `{}`: {error}",
        process.args[0]
    )))
}

fn clone_flags(spec: &Spec) -> Result<CloneFlags> {
    let mut flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWUTS | CloneFlags::CLONE_NEWIPC;
    let mut has_pid_namespace = false;
    let namespaces = spec
        .linux
        .as_ref()
        .map(|linux| linux.namespaces.as_slice())
        .unwrap_or(&[]);
    for namespace in namespaces {
        match namespace.typ {
            LinuxNamespaceType::Network => flags |= CloneFlags::CLONE_NEWNET,
            LinuxNamespaceType::Pid => has_pid_namespace = true,
            LinuxNamespaceType::Mount | LinuxNamespaceType::Ipc | LinuxNamespaceType::Uts => {}
            LinuxNamespaceType::User | LinuxNamespaceType::Cgroup | LinuxNamespaceType::Time => {
                return Err(Error::unsupported(format!(
                    "{} namespaces are not implemented by the native runtime",
                    namespace.typ
                )));
            }
        }
    }
    if !has_pid_namespace {
        return Err(Error::unsupported(
            "native runtime requires a PID namespace; refusing to mount /proc with host PID visibility",
        ));
    }
    Ok(flags | CloneFlags::CLONE_NEWPID)
}

fn apply_identity(process: &Process) -> Result<()> {
    let groups = process
        .user
        .additional_gids
        .iter()
        .map(|gid| Gid::from_raw(*gid))
        .collect::<Vec<_>>();
    setgroups(&groups).map_err(|err| Error::internal(format!("setgroups: {err}")))?;
    setgid(Gid::from_raw(process.user.gid))
        .map_err(|err| Error::internal(format!("setgid({}): {err}", process.user.gid)))?;
    setuid(Uid::from_raw(process.user.uid))
        .map_err(|err| Error::internal(format!("setuid({}): {err}", process.user.uid)))?;
    if process.no_new_privileges {
        set_no_new_privs()
            .map_err(|err| Error::internal(format!("setting no_new_privs: {err}")))?;
    }
    Ok(())
}

fn environment(process: &Process) -> Result<Vec<(&str, &str)>> {
    let mut environment = Vec::with_capacity(process.env.len());
    for entry in &process.env {
        let (key, value) = entry.split_once('=').ok_or_else(|| {
            Error::invalid_argument(format!(
                "process environment entry `{entry}` is not KEY=value"
            ))
        })?;
        environment.push((key, value));
    }
    Ok(environment)
}

/// Tie this helper to the process/thread that spawned it. The daemon actor
/// thread owns the task, so an abrupt daemon death (SIGKILL, panic, OOM)
/// delivers SIGKILL here instead of leaving an orphaned container process the
/// restarted daemon cannot adopt. When stage 2 (PID 1 of the container's PID
/// namespace) dies, the kernel kills the rest of the namespace.
///
/// The parent is sampled before and after `prctl` so the check also works when
/// the daemon itself is PID 1 of its own namespace.
fn arm_parent_death_signal(stage: &str) -> Result<()> {
    let parent = getppid();
    set_pdeathsig(Signal::SIGKILL).map_err(|err| {
        Error::internal(format!(
            "setting the parent-death signal for {stage}: {err}"
        ))
    })?;
    if getppid() != parent {
        return Err(Error::internal(format!(
            "parent exited before {stage} initialized"
        )));
    }
    Ok(())
}

fn forward(pid: Pid, signal: Signal) {
    let _ = kill(pid, signal);
}
