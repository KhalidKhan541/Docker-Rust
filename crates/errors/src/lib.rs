//! Shared error taxonomy for the docker-rs ecosystem.
//!
//! This is the Rust-native equivalent of Moby's `api/types/errdefs` package
//! ([moby v28.5.2](https://github.com/moby/moby), `api/types/errdefs/errors.go`),
//! which defines the daemon-wide error classes that the HTTP API layer maps
//! to status codes.
//!
//! Design rules established after inspecting the original code:
//! * The API layer returns plain message strings plus an HTTP status code;
//!   the message text never contains the status code. `Display` therefore
//!   renders only the message.
//! * Every daemon error is classified by a coarse kind, exactly like the
//!   original sentinel errors (`ErrNotFound`, `ErrInvalidArgument`, ...).
//!
//! `unsafe_code` is forbidden workspace-wide; see each crate's manifest.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Coarse classification of an error, mirroring Moby's `errdefs` sentinel
/// error set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// The requested object does not exist (HTTP 404).
    NotFound,
    /// The request is malformed or semantically invalid (HTTP 400).
    InvalidArgument,
    /// The operation conflicts with current state (HTTP 409).
    Conflict,
    /// Another operation holds the object and prevents this one (HTTP 409).
    Busy,
    /// The server does not implement the operation (HTTP 501).
    NotImplemented,
    /// The feature exists upstream but is intentionally unsupported here
    /// (HTTP 501).
    Unsupported,
    /// An external service or resource is temporarily unavailable (HTTP 503).
    Unavailable,
    /// The request exceeded its deadline (HTTP 504).
    DeadlineExceeded,
    /// The request was cancelled before completion (HTTP 499, client-closed;
    /// mapped to 500 for completeness of the public table).
    Cancelled,
    /// Too many requests (HTTP 429).
    Throttle,
    /// Authentication required (HTTP 401).
    Unauthorized,
    /// Authenticated but not permitted (HTTP 403).
    Forbidden,
    /// Host-side privilege missing (HTTP 403).
    PermissionDenied,
    /// Unclassified internal failure (HTTP 500). This is the default class.
    Internal,
}

impl ErrorKind {
    /// Brief, stable, machine-readable name (`errdefs`-style).
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "not_found",
            ErrorKind::InvalidArgument => "invalid_argument",
            ErrorKind::Conflict => "conflict",
            ErrorKind::Busy => "busy",
            ErrorKind::NotImplemented => "not_implemented",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::Unavailable => "unavailable",
            ErrorKind::DeadlineExceeded => "deadline_exceeded",
            ErrorKind::Cancelled => "cancelled",
            ErrorKind::Throttle => "throttle",
            ErrorKind::Unauthorized => "unauthorized",
            ErrorKind::Forbidden => "forbidden",
            ErrorKind::PermissionDenied => "permission_denied",
            ErrorKind::Internal => "internal",
        }
    }

    /// HTTP status the Engine API layer must return for this kind.
    ///
    /// Mapping matches Moby `errdefs.GetHTTPErrorStatusCode`
    /// (api/types/errdefs/errors.go, same revision).
    pub fn http_status(self) -> u16 {
        match self {
            ErrorKind::NotFound => 404,
            ErrorKind::InvalidArgument => 400,
            ErrorKind::Conflict | ErrorKind::Busy => 409,
            ErrorKind::Throttle => 429,
            ErrorKind::Unauthorized => 401,
            ErrorKind::Forbidden | ErrorKind::PermissionDenied => 403,
            ErrorKind::NotImplemented | ErrorKind::Unsupported => 501,
            ErrorKind::Unavailable => 503,
            ErrorKind::DeadlineExceeded => 504,
            // Client-closed connections have no standard code; Moby emits the
            // nonstandard 499. The serde-independent table maps it to 500 so
            // that callers can never send an invalid status.
            ErrorKind::Cancelled | ErrorKind::Internal => 500,
        }
    }
}

/// Engine error: a [`ErrorKind`] plus a human-readable message and an
/// optional underlying cause.
#[derive(Debug)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    cause: Option<Box<dyn std::error::Error + Send + Sync + 'static>>,
}

impl Error {
    /// Internal constructor.
    fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            cause: None,
        }
    }

    /// Create an error of an explicit kind. Prefer the named constructors.
    pub fn of_kind(kind: ErrorKind, message: impl Into<String>) -> Self {
        Error::new(kind, message)
    }

    /// Attach a cause chain to this error (builder style). The cause is
    /// reported by the `std::error::Error` source chain, never embedded in
    /// the message text.
    pub fn with_cause(
        mut self,
        cause: impl Into<Box<dyn std::error::Error + Send + Sync + 'static>>,
    ) -> Self {
        self.cause = Some(cause.into());
        self
    }

    /// Coarse classification.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// Human-readable message (no kind prefix, matching the Engine API
    /// contract where the message is returned verbatim to clients).
    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn is_not_found(&self) -> bool {
        self.kind == ErrorKind::NotFound
    }

    pub fn is_conflict(&self) -> bool {
        self.kind == ErrorKind::Conflict || self.kind == ErrorKind::Busy
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::NotFound, message)
    }

    pub fn invalid_argument(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::InvalidArgument, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Conflict, message)
    }

    pub fn busy(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Busy, message)
    }

    pub fn not_implemented(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::NotImplemented, message)
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Unsupported, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Unavailable, message)
    }

    pub fn deadline_exceeded(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::DeadlineExceeded, message)
    }

    pub fn cancelled(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Cancelled, message)
    }

    pub fn throttle(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Throttle, message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Unauthorized, message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Forbidden, message)
    }

    pub fn permission_denied(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::PermissionDenied, message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Error::new(ErrorKind::Internal, message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.cause
            .as_deref()
            .map(|c| c as &(dyn std::error::Error + 'static))
    }
}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        // Mirror the file-is-missing special case that daemon callers
        // repeatedly convert to a 404; everything else stays internal.
        if value.kind() == std::io::ErrorKind::NotFound {
            Error::not_found(value.to_string()).with_cause(value)
        } else {
            Error::internal(value.to_string()).with_cause(value)
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        // Serialization failures of our own values are always internal;
        // malformed *input* JSON is mapped explicitly by the API layer
        // before reaching serde.
        Error::internal("serialization failure").with_cause(value)
    }
}

/// Crate-wide (and workspace-wide) result alias.
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error as _StdError;

    #[test]
    fn display_is_message_only() {
        let err = Error::not_found("no such container: deadbeef");
        assert_eq!(err.to_string(), "no such container: deadbeef");
        assert_eq!(err.message(), "no such container: deadbeef");
    }

    #[test]
    fn http_status_matches_errdefs_mapping() {
        assert_eq!(ErrorKind::NotFound.http_status(), 404);
        assert_eq!(ErrorKind::InvalidArgument.http_status(), 400);
        assert_eq!(ErrorKind::Conflict.http_status(), 409);
        assert_eq!(ErrorKind::Busy.http_status(), 409);
        assert_eq!(ErrorKind::Throttle.http_status(), 429);
        assert_eq!(ErrorKind::Unauthorized.http_status(), 401);
        assert_eq!(ErrorKind::Forbidden.http_status(), 403);
        assert_eq!(ErrorKind::NotImplemented.http_status(), 501);
        assert_eq!(ErrorKind::Unavailable.http_status(), 503);
        assert_eq!(ErrorKind::Internal.http_status(), 500);
    }

    #[test]
    fn io_not_found_maps_to_404() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "missing file");
        let err: Error = io_err.into();
        assert_eq!(err.kind(), ErrorKind::NotFound);

        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let err: Error = io_err.into();
        assert_eq!(err.kind(), ErrorKind::Internal);
        assert!(err.source().is_some(), "cause must be preserved");
    }

    #[test]
    fn serde_round_trip() {
        let err = Error::conflict("container already exists");
        let json = serde_json::to_string(&err.kind()).unwrap();
        let kind: ErrorKind = serde_json::from_str(&json).unwrap();
        assert_eq!(kind, ErrorKind::Conflict);
    }

    #[test]
    fn cause_chain_is_source_not_message() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "inner");
        let err = Error::unavailable("runtime unavailable").with_cause(io_err);
        assert_eq!(err.to_string(), "runtime unavailable");
        assert_eq!(err.source().unwrap().to_string(), "inner");
    }
}
