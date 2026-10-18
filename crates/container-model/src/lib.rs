//! Container configuration values and lifecycle state transitions.
//!
//! Mirrors two pinned Moby source areas:
//!
//! * `api/types/container/config.go` + `hostconfig.go` — request payload
//!   structure accepted by `POST /containers/create`.
//! * `daemon/container/state.go` — the container state machine
//!   (`created → running → stopped → paused ...`) with the transition
//!   rules guards checked by the daemon at each operation.

pub mod config;
pub mod state;
pub mod store;
