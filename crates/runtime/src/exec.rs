//! Linux exec helper.
//!
//! This module is deliberately small: the helper joins the namespaces of an
//! already-running OCI task, changes root to that task's rootfs, and spawns a
//! single process. `setns(CLONE_NEWPID)` only affects children, so the helper
//! itself stays in the host PID namespace and waits for the child. This is the
//! same split used by the container init helper and avoids `unsafe` forks.
//!
//! Exec is not a second container. It inherits the target task's mount, UTS,
//! IPC, network and (for children) PID namespaces, but it does not get a new
//! log store, cgroup, or lifecycle/restart policy. Those are daemon concerns.

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use docker_errors::{Error, Result};
use nix::sched::{setns, CloneFlags};
use nix::sys::prctl::set_pdeathsig;
use nix::sys::signal::Signal;
use nix::unistd::{chdir, chroot, fchdir, setgid, setgroups, setpgid, setuid, Gid, Pid, Uid};
use serde::{Deserialize, Serialize};

use crate::bundle::DEFAULT_PATH_ENV;

/// Upper bound for `/etc/passwd` and `/etc/group` lookups inside a rootfs.
/// Identity files are expected to be tiny; refusing an unusually large file
/// keeps a hostile image from turning one exec into an unbounded allocation.
const MAX_IDENTITY_FILE_BYTES: u64 = 1024 * 1024;

/// Serialized payload accepted by the `docker-runtime-exec` helper.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecSpec {
    /// Host PID of the container's PID-namespace init (stage 2, PID 1 inside
    /// the container). Namespace files must be read from this process.
    pub pid: i32,
    /// Host PID of the stage-1 helper, which is the parent of `pid` and stays
    /// alive until the container exits. Re-checking this parentage after the
    /// namespace descriptors are opened rejects a recycled PID.
    pub parent_pid: i32,
    /// Host path of the container rootfs. It is opened before joining the
    /// mount namespace so the descriptor remains valid even if the target
    /// namespace has a different mount view.
    pub rootfs: PathBuf,
    pub argv: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub user: Option<String>,
}

impl ExecSpec {
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|error| Error::internal(format!("encoding exec spec: {error}")))
    }

    pub fn from_json(value: &str) -> Result<Self> {
        serde_json::from_str(value)
            .map_err(|error| Error::invalid_argument(format!("invalid exec spec: {error}")))
    }
}

/// Run one exec request and return the command's conventional exit code.
pub fn run(spec_json: &str) -> Result<i32> {
    let spec = ExecSpec::from_json(spec_json)?;
    if spec.argv.is_empty() {
        return Err(Error::invalid_argument("exec argv must not be empty"));
    }
    if !spec.rootfs.is_dir() {
        return Err(Error::invalid_argument(format!(
            "exec rootfs `{}` is not a directory",
            spec.rootfs.display()
        )));
    }
    if !Uid::effective().is_root() {
        return Err(Error::permission_denied(
            "the native exec helper must start as root to join container namespaces",
        ));
    }
    if spec.cwd.is_empty() || !spec.cwd.starts_with('/') {
        return Err(Error::invalid_argument(format!(
            "exec working directory `{}` must be absolute",
            spec.cwd
        )));
    }
    ensure_child_of(spec.pid, spec.parent_pid)?;
    if target_user_namespace_differs(spec.pid)? {
        return Err(Error::unsupported(
            "exec into a user namespace is not implemented; the native runtime does not create one",
        ));
    }

    // Open the rootfs before switching mount namespaces. The descriptor keeps
    // the directory alive and lets `fchdir` use the object even if the target
    // namespace has a different view of `/`.
    let rootfs = File::open(&spec.rootfs).map_err(|error| {
        Error::internal(format!(
            "opening exec rootfs `{}`: {error}",
            spec.rootfs.display()
        ))
    })?;

    // Open every namespace descriptor before the first `setns`. The container
    // mount namespace typically has `/proc` mounted for the container PID
    // namespace, so after joining it `/proc/<host-pid>/ns/*` for the target
    // no longer exists. Descriptors opened here remain valid across the
    // namespace switches.
    let namespaces = open_namespaces(spec.pid)?;
    // `/proc/<pid>` can disappear or the PID can be recycled between the
    // daemon's resolution and this open. Stage 2 is always a direct child of
    // stage 1, so verify parentage both before and after taking the namespace
    // descriptors. The descriptors themselves remain safe if the process
    // exits after this check.
    ensure_child_of(spec.pid, spec.parent_pid)?;
    join_namespaces(&namespaces)?;

    fchdir(&rootfs).map_err(|error| Error::internal(format!("entering exec rootfs: {error}")))?;
    chroot(".").map_err(|error| Error::internal(format!("chrooting exec rootfs: {error}")))?;
    chdir(spec.cwd.as_str())
        .map_err(|error| Error::internal(format!("chdir to `{}`: {error}", spec.cwd)))?;

    let credentials = resolve_credentials(spec.user.as_deref())?;
    setpgid(Pid::from_raw(0), Pid::from_raw(0))
        .map_err(|error| Error::internal(format!("setpgid in exec helper: {error}")))?;
    let supplementary: Vec<Gid> = credentials
        .supplementary_gids
        .iter()
        .copied()
        .map(Gid::from_raw)
        .collect();
    setgroups(&supplementary)
        .map_err(|error| Error::internal(format!("setting exec groups: {error}")))?;
    setgid(Gid::from_raw(credentials.gid)).map_err(|error| {
        Error::internal(format!("setting exec gid {}: {error}", credentials.gid))
    })?;
    setuid(Uid::from_raw(credentials.uid)).map_err(|error| {
        Error::internal(format!("setting exec uid {}: {error}", credentials.uid))
    })?;
    // The native container task is created with `no_new_privileges: false`
    // (Moby's default), so exec must not force NNP either. Setuid binaries and
    // file capabilities behave the same in the main process and exec.
    // A credential change clears PDEATHSIG. Re-arm it after dropping to the
    // container user so a daemon crash still tears down the helper.
    set_pdeathsig(Signal::SIGKILL).map_err(|error| {
        Error::internal(format!(
            "arming parent-death signal in exec helper: {error}"
        ))
    })?;

    let executable = resolve_executable(&spec.argv[0], &spec.env)?;
    let mut command = Command::new(&executable);
    command
        .args(&spec.argv[1..])
        .env_clear()
        .envs(parse_env(&spec.env)?)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = command.spawn().map_err(|error| {
        Error::unavailable(format!("spawning exec command `{}`: {error}", spec.argv[0]))
    })?;
    let status = child
        .wait()
        .map_err(|error| Error::internal(format!("waiting for exec command: {error}")))?;
    Ok(status
        .code()
        .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
}

fn open_namespace(pid: i32, name: &str) -> Result<File> {
    let path = format!("/proc/{pid}/ns/{name}");
    File::open(&path)
        .map_err(|error| Error::internal(format!("opening container namespace `{path}`: {error}")))
}

struct NamespaceFd {
    name: &'static str,
    flag: CloneFlags,
    /// `None` when the target process already shares this namespace.
    file: Option<File>,
}

/// Open all target namespace files while `/proc` still belongs to the host
/// PID namespace. The list is ordered so the mount namespace is entered first.
fn open_namespaces(pid: i32) -> Result<Vec<NamespaceFd>> {
    let targets: [(&'static str, CloneFlags); 6] = [
        ("mnt", CloneFlags::CLONE_NEWNS),
        ("uts", CloneFlags::CLONE_NEWUTS),
        ("ipc", CloneFlags::CLONE_NEWIPC),
        ("net", CloneFlags::CLONE_NEWNET),
        ("cgroup", CloneFlags::CLONE_NEWCGROUP),
        ("pid", CloneFlags::CLONE_NEWPID),
    ];
    let mut namespaces = Vec::with_capacity(targets.len());
    for (name, flag) in targets {
        let target = std::fs::read_link(format!("/proc/{pid}/ns/{name}")).map_err(|error| {
            Error::internal(format!("reading container {name} namespace: {error}"))
        })?;
        let current = std::fs::read_link(format!("/proc/self/ns/{name}")).map_err(|error| {
            Error::internal(format!("reading current {name} namespace: {error}"))
        })?;
        let file = if target == current {
            None
        } else {
            Some(open_namespace(pid, name)?)
        };
        namespaces.push(NamespaceFd { name, flag, file });
    }
    Ok(namespaces)
}

fn join_namespaces(namespaces: &[NamespaceFd]) -> Result<()> {
    for namespace in namespaces {
        if let Some(file) = &namespace.file {
            setns(file, namespace.flag).map_err(|error| {
                Error::internal(format!(
                    "joining container {} namespace: {error}",
                    namespace.name
                ))
            })?;
        }
    }
    Ok(())
}

fn target_user_namespace_differs(pid: i32) -> Result<bool> {
    let target = std::fs::read_link(format!("/proc/{pid}/ns/user"))
        .map_err(|error| Error::internal(format!("reading target user namespace: {error}")))?;
    let current = std::fs::read_link("/proc/self/ns/user")
        .map_err(|error| Error::internal(format!("reading current user namespace: {error}")))?;
    Ok(target != current)
}

/// Stage 2 is a direct child of the still-running stage-1 helper. Verify that
/// relationship around namespace-FD acquisition so a recycled host PID cannot
/// make exec join an unrelated process's namespaces.
fn ensure_child_of(pid: i32, parent_pid: i32) -> Result<()> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).map_err(|error| {
        Error::unavailable(format!(
            "container init process {pid} disappeared before exec: {error}"
        ))
    })?;
    let actual_parent = status
        .lines()
        .find_map(|line| line.strip_prefix("PPid:"))
        .and_then(|value| value.trim().parse::<i32>().ok())
        .ok_or_else(|| Error::internal("cannot read PPid from container init status"))?;
    if actual_parent != parent_pid {
        return Err(Error::conflict(format!(
            "container init process {pid} is no longer a child of {parent_pid}; refusing exec into a recycled PID"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Credentials {
    uid: u32,
    gid: u32,
    supplementary_gids: Vec<u32>,
}

fn parse_numeric_user(value: Option<&str>) -> Result<(u32, Option<u32>)> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok((0, None));
    };
    let (uid_text, gid_text) = match value.split_once(':') {
        Some((uid, gid)) => (uid, Some(gid)),
        None => (value, None),
    };
    let uid = uid_text.parse::<u32>().map_err(|_| {
        Error::unsupported(format!(
            "exec user `{value}` requires numeric UID[:GID]; name lookup is not implemented"
        ))
    })?;
    let gid = match gid_text {
        Some(gid) => Some(gid.parse::<u32>().map_err(|_| {
            Error::unsupported(format!(
                "exec user `{value}` requires numeric UID[:GID]; name lookup is not implemented"
            ))
        })?),
        None => None,
    };
    Ok((uid, gid))
}

fn resolve_credentials(value: Option<&str>) -> Result<Credentials> {
    let (uid, explicit_gid) = parse_numeric_user(value)?;
    let passwd = read_identity_file("/etc/passwd")?;
    let groups = read_identity_file("/etc/group")?;
    Ok(credentials_from(
        uid,
        explicit_gid,
        passwd.as_deref(),
        groups.as_deref(),
    ))
}

fn credentials_from(
    uid: u32,
    explicit_gid: Option<u32>,
    passwd: Option<&str>,
    group: Option<&str>,
) -> Credentials {
    let entry = passwd.and_then(|text| passwd_entry(text, uid));
    let gid = explicit_gid
        .or_else(|| entry.as_ref().map(|entry| entry.1))
        .unwrap_or(0);
    // Moby only computes supplementary groups for the implicit `UID` form.
    // `UID:GID` sets the primary group and leaves Sgids empty.
    let mut supplementary_gids = if explicit_gid.is_none() {
        entry
            .as_ref()
            .map(|entry| group_gids(group.unwrap_or_default(), &entry.0, gid))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    supplementary_gids.sort_unstable();
    supplementary_gids.dedup();
    Credentials {
        uid,
        gid,
        supplementary_gids,
    }
}

/// Return `(name, primary gid)` for the first passwd entry with `uid`.
fn passwd_entry(text: &str, uid: u32) -> Option<(String, u32)> {
    text.lines().find_map(|line| {
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let entry_uid = fields.next()?.parse::<u32>().ok()?;
        let gid = fields.next()?.parse::<u32>().ok()?;
        (entry_uid == uid && !name.is_empty()).then(|| (name.to_string(), gid))
    })
}

/// Supplementary groups for `user`, excluding the primary GID. Group member
/// lists are comma-separated names, matching `/etc/group`.
fn group_gids(text: &str, user: &str, primary_gid: u32) -> Vec<u32> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(':');
            let _name = fields.next()?;
            let _password = fields.next()?;
            let gid = fields.next()?.parse::<u32>().ok()?;
            let members = fields.next().unwrap_or_default();
            if gid == primary_gid {
                return None;
            }
            members
                .split(',')
                .any(|member| member == user)
                .then_some(gid)
        })
        .collect()
}

fn read_identity_file(path: &str) -> Result<Option<String>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Error::internal(format!(
                "reading exec identity metadata `{path}`: {error}"
            )))
        }
    };
    if !metadata.is_file() {
        return Err(Error::unsupported(format!(
            "exec identity path `{path}` is not a regular file"
        )));
    }
    if metadata.len() > MAX_IDENTITY_FILE_BYTES {
        return Err(Error::unsupported(format!(
            "exec identity file `{path}` exceeds {MAX_IDENTITY_FILE_BYTES} bytes"
        )));
    }
    std::fs::read_to_string(path)
        .map(Some)
        .map_err(|error| Error::internal(format!("reading exec identity file `{path}`: {error}")))
}

fn parse_env(values: &[String]) -> Result<Vec<(String, String)>> {
    values
        .iter()
        .map(|value| {
            let Some((key, value)) = value.split_once('=') else {
                return Err(Error::invalid_argument(format!(
                    "exec environment entry `{value}` is missing `=`"
                )));
            };
            if key.is_empty() {
                return Err(Error::invalid_argument(
                    "exec environment variable name must not be empty",
                ));
            }
            Ok((key.to_string(), value.to_string()))
        })
        .collect()
}

fn resolve_executable(program: &str, env: &[String]) -> Result<PathBuf> {
    if program.contains('/') {
        let path = Path::new(program);
        if is_executable(path) {
            return Ok(path.to_path_buf());
        }
        return Err(Error::not_found(format!(
            "exec executable `{program}` does not exist or is not executable"
        )));
    }
    let path_value = env
        .iter()
        .find_map(|entry| entry.strip_prefix("PATH="))
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_PATH_ENV);
    for directory in path_value.split(':') {
        if directory.is_empty() {
            continue;
        }
        let candidate = Path::new(directory).join(program);
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    Err(Error::not_found(format!(
        "exec executable `{program}` was not found in PATH `{path_value}`"
    )))
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_spec_round_trips() {
        let spec = ExecSpec {
            pid: 42,
            parent_pid: 41,
            rootfs: PathBuf::from("/rootfs"),
            argv: vec!["/bin/echo".to_string(), "hi".to_string()],
            env: vec!["PATH=/bin".to_string()],
            cwd: "/work".to_string(),
            user: Some("1000:1000".to_string()),
        };
        let json = spec.to_json().unwrap();
        assert_eq!(ExecSpec::from_json(&json).unwrap(), spec);
    }

    #[test]
    fn resolves_numeric_exec_users_and_groups() {
        assert_eq!(
            parse_numeric_user(Some("1000:1001")).unwrap(),
            (1000, Some(1001))
        );
        assert_eq!(parse_numeric_user(Some("1000")).unwrap(), (1000, None));
        assert_eq!(parse_numeric_user(None).unwrap(), (0, None));
        let error = parse_numeric_user(Some("nobody")).unwrap_err();
        assert_eq!(error.kind(), docker_errors::ErrorKind::Unsupported);

        let passwd = "root:x:0:0:root:/root:/bin/sh\napp:x:1000:1001:App:/home/app:/bin/sh\n";
        let group = "root:x:0:\napp:x:1001:\nextra:x:2000:app,other\n";
        assert_eq!(
            credentials_from(1000, None, Some(passwd), Some(group)),
            Credentials {
                uid: 1000,
                gid: 1001,
                supplementary_gids: vec![2000],
            }
        );
        assert_eq!(
            credentials_from(1000, Some(3000), Some(passwd), Some(group)),
            Credentials {
                uid: 1000,
                gid: 3000,
                supplementary_gids: Vec::new(),
            }
        );
        assert_eq!(
            credentials_from(1000, None, None, None),
            Credentials {
                uid: 1000,
                gid: 0,
                supplementary_gids: Vec::new(),
            }
        );
    }
}
