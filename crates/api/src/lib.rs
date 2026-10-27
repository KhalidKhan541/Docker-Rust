//! Docker Engine HTTP API layer.
//!
//! Milestone F/G exposes the single-owner [`docker_daemon::Daemon`] actor over
//! the Engine API wire contract. The transport is a small synchronous
//! `tiny_http` server so blocking daemon calls do not need an async bridge.

#![forbid(unsafe_code)]

mod static_files;

pub mod dto;
pub mod image_dto;
pub mod query;
pub mod server;
pub mod version;

pub use server::{ApiServer, RunningServer, ServerConfig};
