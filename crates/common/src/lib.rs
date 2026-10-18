//! Platform-independent building blocks shared across docker-rs.
//!
//! Every subsystem the Replacement covers — the engine API layer, container
//! manager, OCI runtime, content store, storage manager, network and volume
//! managers — needs the same primitives: content-addressed identifiers,
//! validated object names, image references, and host capability detection.
//! This crate centralizes them so that each subsystem crate has exactly one
//! source of truth for these types.

pub mod digest;
pub mod ids;
pub mod names;
pub mod paths;
pub mod platform;
pub mod process;
pub mod reference;
pub mod rng;

pub use digest::Digest;
pub use ids::ContainerId;
pub use names::Name;
pub use paths::SafePath;
pub use reference::ImageRef;
