//! Container lifecycle states and legal transitions.
//!
//! Translated from moby's `daemon/container/state.go` (pinned revision):
//! the daemon keeps `State` with `status ∈ {created, running, paused,
//! restarting, removing, exited, dead}` and each operation (start/stop/
//! pause/unpause/restart/kill) checks the current state, refusing
//! contradictory transitions with `invalid parameter`/`conflict`-class
//! errors. This model encodes each legal transition explicitly so that the
//! engine layer cannot bypass the guards.

use std::fmt;
use std::str::FromStr;

use docker_errors::{Error, Result};
use serde::{Deserialize, Serialize};

/// Container lifecycle state, matching the status strings the Engine API
/// reports in `State.Status` (and `docker ps`). `Paused` is a projection of
/// `Running` upstream; here it is a distinct state, with the running flavor
/// recoverable because pause/unpause never dies the container process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LifecycleState {
    Created,
    Running,
    Paused,
    Restarting,
    Removing,
    Exited,
    Dead,
}

impl LifecycleState {
    /// Engine-API `State.Status` string.
    pub fn as_api_status(self) -> &'static str {
        match self {
            LifecycleState::Created => "created",
            LifecycleState::Running => "running",
            LifecycleState::Paused => "paused",
            LifecycleState::Restarting => "restarting",
            LifecycleState::Removing => "removing",
            LifecycleState::Exited => "exited",
            LifecycleState::Dead => "dead",
        }
    }

    /// Is a process expected to exist for this state? Mirrors
    /// `daemon/container/state.go`'s `IsRunning` semantic
    /// (running OR paused OR restarting).
    pub fn is_alive(self) -> bool {
        matches!(
            self,
            LifecycleState::Running | LifecycleState::Paused | LifecycleState::Restarting
        )
    }

    /// Legal transitions graph.
    ///
    /// Mapping (source state -> set of allowed targets) follows the daemon
    /// lifecycle:
    /// * created -> running (start), removing, dead
    /// * running -> exited (die/stop), paused, dead, removing, restarting
    /// * paused -> running (unpause), exited, dead, removing
    /// * restarting -> running (andon restart success), exited, dead
    /// * removing -> (terminal; removal finishing)
    /// * exited -> running (start), removing, dead
    /// * dead -> removing, terminal
    pub fn can_transition_to(self, next: LifecycleState) -> bool {
        use LifecycleState::*;
        matches!(
            (self, next),
            (Created, Running)
                | (Created, Removing)
                | (Created, Dead)
                | (Running, Exited)
                | (Running, Paused)
                | (Running, Dead)
                | (Running, Removing)
                | (Running, Restarting)
                | (Paused, Running)
                | (Paused, Exited)
                | (Paused, Dead)
                | (Paused, Removing)
                | (Restarting, Running)
                | (Restarting, Exited)
                | (Restarting, Dead)
                | (Exited, Running)
                | (Exited, Removing)
                | (Exited, Dead)
                | (Removing, Dead)
                | (Dead, Removing)
        )
    }
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_api_status())
    }
}

impl FromStr for LifecycleState {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "created" => Ok(LifecycleState::Created),
            "running" => Ok(LifecycleState::Running),
            "paused" => Ok(LifecycleState::Paused),
            "restarting" => Ok(LifecycleState::Restarting),
            "removing" => Ok(LifecycleState::Removing),
            "exited" => Ok(LifecycleState::Exited),
            "dead" => Ok(LifecycleState::Dead),
            other => Err(Error::invalid_argument(format!(
                "unknown lifecycle state `{other}`"
            ))),
        }
    }
}

/// Strongly-typed container state wrapper that owns its current lifecycle
/// value plus observable runtime fields (exit status, started-at, finished
/// reasons) the Engine API inspects. Fills in exactly what `State` carries
/// that docker-rs will populate in Milestone D.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub status: LifecycleState,
    /// Process exit code once the container has exited.
    pub exit_code: Option<i32>,
    /// Signal that terminated the process, if the process did not exit
    /// normally (fatal signals reported by the runtime).
    pub error_signal: Option<String>,
}

/// Validate a state transition request and return the target state, or an
/// error naming the illegal move.
pub fn validate_transition(
    current: LifecycleState,
    next: LifecycleState,
) -> Result<LifecycleState> {
    if current == next {
        // Idempotent no-ops are allowed and mapped as-is; callers that need
        // strictness must compare states themselves.
        return Ok(current);
    }
    if current.can_transition_to(next) {
        Ok(next)
    } else {
        Err(Error::conflict(format!(
            "invalid lifecycle transition {current} -> {next}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_status_strings_match_engine_api() {
        assert_eq!(LifecycleState::Created.as_api_status(), "created");
        assert_eq!(LifecycleState::Restarting.as_api_status(), "restarting");
        assert_eq!(LifecycleState::Removing.as_api_status(), "removing");
        assert_eq!(LifecycleState::Exited.as_api_status(), "exited");
    }

    #[test]
    fn parse_roundtrip_all_states() {
        for raw in [
            "created",
            "running",
            "paused",
            "restarting",
            "removing",
            "exited",
            "dead",
        ] {
            let parsed = LifecycleState::from_str(raw).unwrap();
            assert_eq!(parsed.as_api_status(), raw);
        }
        assert!(LifecycleState::from_str("newly-invented").is_err());
    }

    #[test]
    fn legal_create_run_exit_chain() {
        let mut s = LifecycleState::Created;
        s = validate_transition(s, LifecycleState::Running).unwrap();
        s = validate_transition(s, LifecycleState::Exited).unwrap();
        s = validate_transition(s, LifecycleState::Removing).unwrap();
        assert_eq!(s, LifecycleState::Removing);
    }

    #[test]
    fn pause_unpause_restart() {
        assert_eq!(
            validate_transition(LifecycleState::Running, LifecycleState::Paused).unwrap(),
            LifecycleState::Paused
        );
        assert_eq!(
            validate_transition(LifecycleState::Paused, LifecycleState::Running).unwrap(),
            LifecycleState::Running
        );
        assert_eq!(
            validate_transition(LifecycleState::Running, LifecycleState::Restarting).unwrap(),
            LifecycleState::Restarting
        );
        assert_eq!(
            validate_transition(LifecycleState::Restarting, LifecycleState::Running).unwrap(),
            LifecycleState::Running
        );
    }

    #[test]
    fn illegal_transitions_named() {
        // created -> paused is illegal; must start first (upstream parity).
        let err = validate_transition(LifecycleState::Created, LifecycleState::Paused).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Conflict);
        assert!(err.message().contains("invalid lifecycle transition"));
        // removing -> anything except dead is illegal.
        assert!(validate_transition(LifecycleState::Removing, LifecycleState::Running).is_err());
        // exited -> paused illegal.
        assert!(validate_transition(LifecycleState::Exited, LifecycleState::Paused).is_err());
    }

    #[test]
    fn is_alive_matches_is_running_semantics() {
        assert!(LifecycleState::Running.is_alive());
        assert!(LifecycleState::Paused.is_alive());
        assert!(LifecycleState::Restarting.is_alive());
        assert!(!LifecycleState::Created.is_alive());
        assert!(!LifecycleState::Exited.is_alive());
        assert!(!LifecycleState::Dead.is_alive());
    }

    #[test]
    fn serde_lowercase() {
        let json = serde_json::to_string(&LifecycleState::Running).unwrap();
        assert_eq!(json, "\"running\"");
        let back: LifecycleState = serde_json::from_str(&json).unwrap();
        assert_eq!(back, LifecycleState::Running);
    }

    #[test]
    fn dead_is_terminal_for_run_chains() {
        assert!(validate_transition(LifecycleState::Dead, LifecycleState::Running).is_err());
        assert!(validate_transition(LifecycleState::Dead, LifecycleState::Removing).is_ok());
    }
}
