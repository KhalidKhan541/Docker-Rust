//! API version parsing and the Engine's min/default gates.
//!
//! Moby's `api/server/middleware/version.go` sets `Api-Version`,
//! `Server`, and `Ostype` on every response, accepts unversioned requests as
//! the default version, and rejects requests outside `min..=default` with a
//! 400-class `versionUnsupportedError`. The comparison follows
//! `api/types/versions/compare.go`: dotted components compare numerically,
//! and missing components compare as zero.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

/// Default Engine API version supported by this server.
pub const DEFAULT_VERSION: &str = "1.51";
/// Oldest Engine API version accepted by this server.
pub const MIN_VERSION: &str = "1.24";

/// A parsed dotted API version such as `1.51`.
#[derive(Debug, Clone)]
pub struct ApiVersion {
    components: Vec<u32>,
}

impl PartialEq for ApiVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for ApiVersion {}

impl ApiVersion {
    /// Parse a version without a leading `v`.
    pub fn parse(value: &str) -> Result<Self, VersionParseError> {
        if value.is_empty() {
            return Err(VersionParseError::Empty);
        }
        let mut components = Vec::new();
        for component in value.split('.') {
            if component.is_empty() {
                return Err(VersionParseError::Invalid(value.to_string()));
            }
            let parsed = component
                .parse::<u32>()
                .map_err(|_| VersionParseError::Invalid(value.to_string()))?;
            components.push(parsed);
        }
        Ok(Self { components })
    }

    /// Numeric component at `index`, or zero when absent.
    fn component(&self, index: usize) -> u32 {
        self.components.get(index).copied().unwrap_or(0)
    }

    /// The dotted form originally accepted by [`FromStr`].
    pub fn as_str(&self) -> String {
        self.components
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_str())
    }
}

impl FromStr for ApiVersion {
    type Err = VersionParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl PartialOrd for ApiVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ApiVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        let length = self.components.len().max(other.components.len());
        for index in 0..length {
            match self.component(index).cmp(&other.component(index)) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    }
}

/// Failure to parse a dotted version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionParseError {
    Empty,
    Invalid(String),
}

impl fmt::Display for VersionParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("API version is empty"),
            Self::Invalid(value) => write!(f, "invalid API version {value:?}"),
        }
    }
}

impl std::error::Error for VersionParseError {}

/// Result of applying the version gate to a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionDecision {
    /// Request falls within the supported range.
    Accepted(ApiVersion),
    /// Request is older than the configured minimum.
    TooOld { requested: String, minimum: String },
    /// Request is newer than the configured default.
    TooNew { requested: String, maximum: String },
}

impl VersionDecision {
    /// True when the request may continue to routing.
    pub fn is_accepted(&self) -> bool {
        matches!(self, Self::Accepted(_))
    }
}

/// Min/default version pair used by the middleware.
#[derive(Debug, Clone)]
pub struct VersionGate {
    pub minimum: ApiVersion,
    pub default: ApiVersion,
}

impl VersionGate {
    /// Build a gate, rejecting configurations outside Moby's supported range.
    pub fn new(minimum: &str, default: &str) -> Result<Self, VersionConfigError> {
        let minimum = ApiVersion::parse(minimum)
            .map_err(|error| VersionConfigError::InvalidMinimum(error.to_string()))?;
        let default = ApiVersion::parse(default)
            .map_err(|error| VersionConfigError::InvalidDefault(error.to_string()))?;
        let supported_min = ApiVersion::parse(MIN_VERSION).expect("MIN_VERSION is valid");
        let supported_default =
            ApiVersion::parse(DEFAULT_VERSION).expect("DEFAULT_VERSION is valid");
        if minimum < supported_min || minimum > supported_default {
            return Err(VersionConfigError::MinimumOutOfRange {
                minimum: minimum.to_string(),
                supported_min: supported_min.to_string(),
                supported_default: supported_default.to_string(),
            });
        }
        if minimum > default {
            return Err(VersionConfigError::MinimumAboveDefault {
                minimum: minimum.to_string(),
                default: default.to_string(),
            });
        }
        Ok(Self { minimum, default })
    }

    /// The gate for this build's supported API range.
    pub fn current() -> Self {
        Self {
            minimum: ApiVersion::parse(MIN_VERSION).expect("MIN_VERSION is valid"),
            default: ApiVersion::parse(DEFAULT_VERSION).expect("DEFAULT_VERSION is valid"),
        }
    }

    /// Apply the gate. `requested` is the raw version without a leading `v`.
    pub fn check(&self, requested: Option<&str>) -> VersionDecision {
        let default = self.default.to_string();
        let requested = requested.unwrap_or(&default);
        let parsed = match ApiVersion::parse(requested) {
            Ok(version) => version,
            Err(_) => {
                // Upstream's regex only permits digits and dots, so this path
                // is reachable only through a hand-built request. Treat an
                // unparseable version as too old rather than silently
                // accepting it.
                return VersionDecision::TooOld {
                    requested: requested.to_string(),
                    minimum: self.minimum.to_string(),
                };
            }
        };
        if parsed < self.minimum {
            VersionDecision::TooOld {
                requested: requested.to_string(),
                minimum: self.minimum.to_string(),
            }
        } else if parsed > self.default {
            VersionDecision::TooNew {
                requested: requested.to_string(),
                maximum: self.default.to_string(),
            }
        } else {
            VersionDecision::Accepted(parsed)
        }
    }
}

impl Default for VersionGate {
    fn default() -> Self {
        Self::current()
    }
}

/// A Server version gate could not be constructed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VersionConfigError {
    InvalidMinimum(String),
    InvalidDefault(String),
    MinimumOutOfRange {
        minimum: String,
        supported_min: String,
        supported_default: String,
    },
    MinimumAboveDefault {
        minimum: String,
        default: String,
    },
}

impl fmt::Display for VersionConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMinimum(error) => write!(f, "invalid minimum API version: {error}"),
            Self::InvalidDefault(error) => write!(f, "invalid default API version: {error}"),
            Self::MinimumOutOfRange {
                minimum,
                supported_min,
                supported_default,
            } => write!(
                f,
                "invalid minimum API version ({minimum}): must be between {supported_min} and {supported_default}"
            ),
            Self::MinimumAboveDefault { minimum, default } => write!(
                f,
                "invalid API version: the minimum API version ({minimum}) is higher than the default version ({default})"
            ),
        }
    }
}

impl std::error::Error for VersionConfigError {}

/// Version gate rejection rendered exactly like Moby's middleware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionUnsupported {
    pub requested: String,
    pub minimum: Option<String>,
    pub maximum: Option<String>,
}

impl fmt::Display for VersionUnsupported {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(minimum) = &self.minimum {
            write!(
                f,
                "client version {} is too old. Minimum supported API version is {minimum}, please upgrade your client to a newer version",
                self.requested
            )
        } else {
            write!(
                f,
                "client version {} is too new. Maximum supported API version is {}",
                self.requested,
                self.maximum.as_deref().unwrap_or(DEFAULT_VERSION)
            )
        }
    }
}

/// Split an optional `/v<version>` path prefix.
///
/// Returns the raw version (without `v`) and the remaining path beginning
/// with `/`. A bare `/v1.51` yields an empty remaining path.
pub fn split_version(path: &str) -> (Option<&str>, &str) {
    let Some(rest) = path.strip_prefix('/') else {
        return (None, path);
    };
    let (first, _remainder) = match rest.split_once('/') {
        Some((first, remainder)) => (first, remainder),
        None => (rest, ""),
    };
    let Some(version) = first.strip_prefix('v') else {
        return (None, path);
    };
    if version.is_empty()
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return (None, path);
    }
    let consumed = 1 + first.len();
    (Some(version), &path[consumed..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_dotted_versions_numerically() {
        let one_nine = ApiVersion::parse("1.9").unwrap();
        let one_ten = ApiVersion::parse("1.10").unwrap();
        assert!(one_nine < one_ten);
        assert_eq!(
            ApiVersion::parse("1.51").unwrap(),
            ApiVersion::parse("1.51.0").unwrap()
        );
    }

    #[test]
    fn gate_accepts_supported_range_and_defaults() {
        let gate = VersionGate::current();
        assert!(gate.check(None).is_accepted());
        assert!(gate.check(Some("1.24")).is_accepted());
        assert!(gate.check(Some("1.51")).is_accepted());
        assert_eq!(
            gate.check(Some("1.23")),
            VersionDecision::TooOld {
                requested: "1.23".into(),
                minimum: "1.24".into()
            }
        );
        assert_eq!(
            gate.check(Some("1.52")),
            VersionDecision::TooNew {
                requested: "1.52".into(),
                maximum: "1.51".into()
            }
        );
    }

    #[test]
    fn unsupported_messages_match_upstream_wording() {
        let old = VersionUnsupported {
            requested: "1.20".into(),
            minimum: Some("1.24".into()),
            maximum: None,
        };
        assert_eq!(
            old.to_string(),
            "client version 1.20 is too old. Minimum supported API version is 1.24, please upgrade your client to a newer version"
        );
        let new = VersionUnsupported {
            requested: "1.99".into(),
            minimum: None,
            maximum: Some("1.51".into()),
        };
        assert_eq!(
            new.to_string(),
            "client version 1.99 is too new. Maximum supported API version is 1.51"
        );
    }

    #[test]
    fn splits_only_valid_version_prefixes() {
        assert_eq!(
            split_version("/v1.51/containers/json"),
            (Some("1.51"), "/containers/json")
        );
        assert_eq!(split_version("/v1.24"), (Some("1.24"), ""));
        assert_eq!(
            split_version("/containers/json"),
            (None, "/containers/json")
        );
        assert_eq!(
            split_version("/vfoo/containers"),
            (None, "/vfoo/containers")
        );
    }
}
