//! Restart-policy engine, translated from Moby's
//! `restartmanager/restartmanager.go` (pinned v28.5.2).
//!
//! The manager is pure scheduling state: it decides whether an exit should be
//! followed by a restart and how long the daemon should wait. The actor owns
//! the timer; only one restart may be in flight per container, which is what
//! the upstream `active` guard enforces.

use std::time::Duration;

use docker_container_model::config::RestartPolicy;
use docker_errors::{Error, Result};

/// First restart delay (upstream `defaultTimeout`).
pub const DEFAULT_RESTART_TIMEOUT: Duration = Duration::from_millis(100);
/// Maximum backoff (upstream `maxRestartTimeout`).
pub const MAX_RESTART_TIMEOUT: Duration = Duration::from_secs(60);
/// Running for this long resets the backoff to the default (upstream 10s).
pub const BACKOFF_RESET_AFTER: Duration = Duration::from_secs(10);

/// Result of one exit evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartDecision {
    pub restart: bool,
    /// Present exactly when `restart` is true.
    pub delay: Option<Duration>,
}

impl RestartDecision {
    fn no() -> Self {
        Self {
            restart: false,
            delay: None,
        }
    }
}

/// Per-container restart scheduler.
#[derive(Debug, Clone)]
pub struct RestartManager {
    policy: RestartPolicy,
    max_retry_count: u16,
    restart_count: u32,
    timeout: Duration,
    active: bool,
    canceled: bool,
}

impl RestartManager {
    /// `restart_count` is the count persisted for the container; a manual
    /// start resets it to zero, automatic restarts carry it forward.
    pub fn new(policy: RestartPolicy, max_retry_count: u16, restart_count: u32) -> Self {
        Self {
            policy,
            max_retry_count,
            restart_count,
            timeout: Duration::ZERO,
            active: false,
            canceled: false,
        }
    }

    /// Evaluate an exit. Equivalent to upstream `ShouldRestart`; the caller
    /// must invoke [`Self::complete_active`] when the returned delay elapses
    /// and [`Self::cancel`] when the user stops the container.
    pub fn should_restart(
        &mut self,
        exit_code: i32,
        manually_stopped: bool,
        execution_duration: Duration,
    ) -> Result<RestartDecision> {
        if self.policy == RestartPolicy::No {
            return Ok(RestartDecision::no());
        }
        if self.canceled {
            return Err(Error::conflict("restart canceled"));
        }
        if self.active {
            return Err(Error::conflict("invalid call on an active restart manager"));
        }

        // A container that ran for at least ten seconds gets a fresh backoff
        // regardless of policy or exit status.
        if execution_duration >= BACKOFF_RESET_AFTER {
            self.timeout = Duration::ZERO;
        }
        if self.timeout.is_zero() {
            self.timeout = DEFAULT_RESTART_TIMEOUT;
        } else if self.timeout < MAX_RESTART_TIMEOUT {
            self.timeout = self.timeout.saturating_mul(2).min(MAX_RESTART_TIMEOUT);
        }

        let restart = match self.policy {
            RestartPolicy::No => false,
            RestartPolicy::Always => true,
            RestartPolicy::UnlessStopped => !manually_stopped,
            RestartPolicy::OnFailure => {
                let under_limit = self.max_retry_count == 0
                    || self.restart_count < u32::from(self.max_retry_count);
                under_limit && exit_code != 0
            }
        };

        if !restart {
            self.active = false;
            return Ok(RestartDecision::no());
        }

        self.restart_count += 1;
        self.active = true;
        Ok(RestartDecision {
            restart: true,
            delay: Some(self.timeout),
        })
    }

    /// Timer elapsed (or start was attempted): the next exit may be evaluated.
    pub fn complete_active(&mut self) {
        self.active = false;
    }

    /// Permanently disable automatic restarts. A manual start replaces the
    /// manager, mirroring `ResetRestartManager`.
    pub fn cancel(&mut self) {
        self.canceled = true;
        self.active = false;
    }

    pub fn restart_count(&self) -> u32 {
        self.restart_count
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn is_canceled(&self) -> bool {
        self.canceled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn always_restarts_with_default_timeout() {
        let mut rm = RestartManager::new(RestartPolicy::Always, 0, 0);
        let d = rm.should_restart(0, false, Duration::from_secs(1)).unwrap();
        assert!(d.restart);
        assert_eq!(d.delay, Some(DEFAULT_RESTART_TIMEOUT));
        assert_eq!(rm.timeout(), DEFAULT_RESTART_TIMEOUT);
        assert_eq!(rm.restart_count(), 1);
    }

    #[test]
    fn backoff_doubles_and_resets_after_ten_seconds() {
        let mut rm = RestartManager::new(RestartPolicy::Always, 0, 0);

        let first = rm.should_restart(0, false, Duration::from_secs(1)).unwrap();
        assert_eq!(first.delay, Some(Duration::from_millis(100)));
        rm.complete_active();

        let second = rm.should_restart(0, false, Duration::from_secs(1)).unwrap();
        assert_eq!(second.delay, Some(Duration::from_millis(200)));
        rm.complete_active();

        let reset = rm
            .should_restart(0, false, Duration::from_secs(10))
            .unwrap();
        assert_eq!(reset.delay, Some(Duration::from_millis(100)));
    }

    #[test]
    fn backoff_caps_at_one_minute() {
        let mut rm = RestartManager::new(RestartPolicy::Always, 0, 0);
        for _ in 0..12 {
            let d = rm.should_restart(0, false, Duration::from_secs(1)).unwrap();
            assert!(d.restart);
            rm.complete_active();
        }
        assert_eq!(rm.timeout(), MAX_RESTART_TIMEOUT);
    }

    #[test]
    fn on_failure_honors_max_retry_count() {
        let mut rm = RestartManager::new(RestartPolicy::OnFailure, 2, 0);

        assert!(
            rm.should_restart(1, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        rm.complete_active();
        assert!(
            rm.should_restart(1, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        rm.complete_active();
        assert!(
            !rm.should_restart(1, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        assert_eq!(rm.restart_count(), 2);
    }

    #[test]
    fn on_failure_does_not_restart_clean_exit() {
        let mut rm = RestartManager::new(RestartPolicy::OnFailure, 0, 0);
        let d = rm.should_restart(0, false, Duration::from_secs(1)).unwrap();
        assert!(!d.restart);
        assert_eq!(d.delay, None);
    }

    #[test]
    fn unless_stopped_respects_manual_stop() {
        let mut rm = RestartManager::new(RestartPolicy::UnlessStopped, 0, 0);
        assert!(
            !rm.should_restart(0, true, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        rm.complete_active();
        assert!(
            rm.should_restart(0, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
    }

    #[test]
    fn always_ignores_manual_flag_unless_canceled() {
        let mut rm = RestartManager::new(RestartPolicy::Always, 0, 0);
        assert!(
            rm.should_restart(0, true, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        rm.complete_active();
        rm.cancel();
        assert!(rm.should_restart(0, false, Duration::from_secs(1)).is_err());
    }

    #[test]
    fn no_policy_does_not_touch_backoff() {
        let mut rm = RestartManager::new(RestartPolicy::No, 0, 0);
        for _ in 0..3 {
            assert!(
                !rm.should_restart(1, false, Duration::from_secs(1))
                    .unwrap()
                    .restart
            );
        }
        assert_eq!(rm.timeout(), Duration::ZERO);
    }

    #[test]
    fn active_manager_rejects_double_evaluation() {
        let mut rm = RestartManager::new(RestartPolicy::Always, 0, 0);
        assert!(
            rm.should_restart(0, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
        assert!(rm.should_restart(0, false, Duration::from_secs(1)).is_err());
        rm.complete_active();
        assert!(
            rm.should_restart(0, false, Duration::from_secs(1))
                .unwrap()
                .restart
        );
    }
}
