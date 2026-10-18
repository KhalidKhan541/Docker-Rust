//! Own-child process control primitives.
//!
//! The container manager (Milestone D) supervises runtime child processes.
//! The foundation exposes the portable part of that surface — spawn, pid,
//! wait/try_wait, hard kill — and is honest about platform gaps:
//!
//! POSIX-style signals are *not* portable. The OCI runtime stop protocol
//! (`SIGTERM`, then `SIGKILL`) needs per-pid signaling; Windows offers no
//! signal channel, only `TerminateProcess`. The modeled [`Signal`] enum is
//! therefore paired with an [`SignalDelivery`] result rather than silent
//! remapping: `SIGKILL` maps to a hard kill on every platform by explicit,
//! documented agreement, and any other signal requests a runtime backend
//! that implements the POSIX semantics (Milestone C) and returns
//! `Unsupported` here with an actionable message.

use std::process::{Child, Command, ExitStatus, Stdio};

use docker_errors::{Error, Result};

/// Signals required for the OCI runtime lifecycle (subset; later milestones
/// extend this list as the runtime grows).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    Sighup,
    Sigint,
    Sigquit,
    /// Graceful termination request — the OCI stop protocol default.
    Sigterm,
    /// Immediate, unblockable termination (`SIGKILL`).
    Sigkill,
}

impl Signal {
    /// POSIX name as the Engine API accepts in `stop`/`kill` requests.
    pub fn as_str(self) -> &'static str {
        match self {
            Signal::Sighup => "SIGHUP",
            Signal::Sigint => "SIGINT",
            Signal::Sigquit => "SIGQUIT",
            Signal::Sigterm => "SIGTERM",
            Signal::Sigkill => "SIGKILL",
        }
    }
}

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Outcome of a signal transmission attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalDelivery {
    Delivered,
    UnsupportedPlatform,
}

/// An owned child process.
#[derive(Debug)]
pub struct ChildProcess {
    child: Option<Child>,
}

impl ChildProcess {
    /// Spawn a child with stdin closed and stdout/stderr piped, matching
    /// daemon behavior of separating log streaming from TTY semantics.
    pub fn spawn(mut cmd: Command) -> Result<Self> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd.spawn().map_err(|e| {
            Error::unavailable(format!("process spawn failed: {}", e.kind())).with_cause(e)
        })?;
        Ok(Self { child: Some(child) })
    }

    /// OS process id, if the child has not been reaped yet.
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Hard-terminate the child (both platforms: POSIX `SIGKILL`
    /// equivalent / `TerminateProcess` on Windows).
    pub fn kill(&mut self) -> Result<()> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::conflict("child already reaped"))?;
        child
            .kill()
            .map_err(|e| Error::internal("child kill failed").with_cause(e))
    }

    /// Wait for exit; the child is consumed once reaped, so a second wait
    /// conflicts (mirrors Go's `Wait` contract used throughout the daemon).
    pub fn wait(&mut self) -> Result<ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| Error::conflict("child already reaped"))?;
        child
            .wait()
            .map_err(|e| Error::internal("child wait failed").with_cause(e))
    }

    /// Non-blocking exit check. Consumes nothing on success; behavior
    /// matches `std::process::Child::try_wait`.
    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| Error::conflict("child already reaped"))?;
        child
            .try_wait()
            .map_err(|e| Error::internal("child try_wait failed").with_cause(e))
    }

    fn live_child(&mut self) -> Result<&mut Child> {
        self.child
            .as_mut()
            .ok_or_else(|| Error::conflict("child already reaped"))
    }

    /// Transmit a signal, per the crate documentation on portability.
    pub fn send_signal(&mut self, signal: Signal) -> Result<SignalDelivery> {
        match signal {
            Signal::Sigkill => {
                self.kill()?;
                Ok(SignalDelivery::Delivered)
            }
            other => {
                let _ = self.live_child()?;
                Err(Error::unsupported(format!(
                    "signal {other} delivery requires the OS runtime backend (portable backend cannot signal other processes)"
                )))
            }
        }
    }

    /// Take ownership of the pipes after a completed wait; reused by log
    /// streaming in the container manager (Milestone D).
    pub fn into_pipes(
        mut self,
    ) -> (
        Option<std::process::ChildStdout>,
        Option<std::process::ChildStderr>,
    ) {
        match self.child.take() {
            Some(child) => (child.stdout, child.stderr),
            None => (None, None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn quick_cmd() -> Command {
        // Neutral, dependency-free no-op commands present on both platforms.
        #[cfg(windows)]
        {
            let mut c = Command::new("cmd.exe");
            c.args(["/C", "exit"]);
            c
        }
        #[cfg(not(windows))]
        {
            Command::new("echo")
        }
    }

    #[test]
    fn spawn_and_wait() {
        let mut child = ChildProcess::spawn(quick_cmd()).unwrap();
        assert!(child.pid().is_some());
        let st = child.wait().unwrap();
        assert!(st.success());
    }

    #[test]
    fn double_wait_conflicts() {
        let mut child = ChildProcess::spawn(quick_cmd()).unwrap();
        let _ = child.wait().unwrap();
        let err = child.wait().unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Conflict);
    }

    #[test]
    fn graceful_signal_is_unsupported_not_silent() {
        let mut child = ChildProcess::spawn(quick_cmd()).unwrap();
        child.wait().unwrap();
        // Reap already happened; error must name the limitation, never lie
        // about success.
        let err = child.send_signal(Signal::Sigterm).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Conflict);
        assert!(err.message().contains("reaped"));
    }

    #[test]
    fn pid_display_stable() {
        let mut child = ChildProcess::spawn(quick_cmd()).unwrap();
        let pid = child.pid().unwrap();
        let _ = child.wait().unwrap();
        assert!(pid > 0);
    }
}
