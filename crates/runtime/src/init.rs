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
//! This is intentionally smaller than runc: unsupported OCI fields are
//! rejected by [`crate::oci::ensure_supported`] before the helper starts.

use std::collections::VecDeque;
use std::fs;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Command, ExitStatus};
use std::sync::{Arc, Mutex};

use docker_errors::{Error, Result};
use nix::mount::{mount, MsFlags};
use nix::sched::{unshare, CloneFlags};
use nix::sys::prctl::set_no_new_privs;
use nix::sys::signal::{kill, Signal};
use nix::unistd::{
    chdir, chroot, getpid, setgid, setgroups, sethostname, setpgid, setuid, Gid, Pid, Uid,
};
use signal_hook::consts::{SIGALRM, SIGHUP, SIGINT, SIGPIPE, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2};
use signal_hook::iterator::Signals;

use crate::bundle::Bundle;
use crate::oci::ensure_supported;
use crate::spec::{LinuxNamespaceType, Process, Spec};

const FORWARDED_SIGNALS: [i32; 8] = [
    SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGUSR1, SIGUSR2, SIGALRM, SIGPIPE,
];

/// Stage 1 entry point: create namespaces, fork PID 1, wait, and propagate the
/// container exit code.
pub fn run_stage1(bundle_path: &Path) -> Result<i32> {
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

    // Signal handlers must be installed before spawning stage 2 so a stop
    // racing with startup is not lost. Handlers are reset to default by exec,
    // so the container process itself receives normal signals.
    let pending = Arc::new(Mutex::new(VecDeque::<i32>::new()));
    let child_pid = Arc::new(Mutex::new(None::<i32>));
    let mut signals = Signals::new(FORWARDED_SIGNALS)
        .map_err(|err| Error::internal(format!("installing signal handlers: {err}")))?;
    let signal_handle = signals.handle();
    let thread_pending = Arc::clone(&pending);
    let thread_pid = Arc::clone(&child_pid);
    let signal_thread = std::thread::spawn(move || {
        for signal in signals.forever() {
            let pid = *thread_pid
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            match pid {
                Some(pid) => forward(pid, signal),
                None => thread_pending
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push_back(signal),
            }
        }
    });

    let stage2 = std::env::current_exe()
        .map_err(|err| Error::internal(format!("locating stage 2 executable: {err}")))?;
    let mut child = Command::new(stage2)
        .arg("--stage2")
        .arg("--bundle")
        .arg(bundle_path)
        .spawn()
        .map_err(|err| Error::internal(format!("starting stage 2 init: {err}")))?;

    {
        let mut slot = child_pid
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *slot = Some(child.id() as i32);
    }
    let queued: Vec<i32> = {
        let mut pending = pending.lock().unwrap_or_else(|poison| poison.into_inner());
        pending.drain(..).collect()
    };
    for signal in queued {
        forward(child.id() as i32, signal);
    }

    let status = child.wait();
    {
        let mut slot = child_pid
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *slot = None;
    }
    signal_handle.close();
    let _ = signal_thread.join();

    let status =
        status.map_err(|err| Error::internal(format!("waiting for container process: {err}")))?;
    Ok(exit_code(status))
}

/// Stage 2 entry point. This process must already be PID 1 in the container's
/// PID namespace and share stage 1's mount namespace.
pub fn run_stage2(bundle_path: &Path) -> Result<i32> {
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

fn forward(pid: i32, signal: i32) {
    if let Ok(signal) = Signal::try_from(signal) {
        let _ = kill(Pid::from_raw(pid), signal);
    }
}

fn exit_code(status: ExitStatus) -> i32 {
    if let Some(code) = status.code() {
        code
    } else if let Some(signal) = status.signal() {
        128 + signal
    } else {
        255
    }
}
