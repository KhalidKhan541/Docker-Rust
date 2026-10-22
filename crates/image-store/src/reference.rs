//! Image-reference normalization and platform parsing.
//!
//! Docker's local store uses canonical references internally (`docker.io/
//! library/alpine:latest`) while the CLI displays familiar names (`alpine:
//! latest`). Keeping the two forms separate avoids the classic bug where a
//! pull by one spelling cannot be resolved by another.

use docker_common::{Digest, ImageRef};
use docker_errors::{Error, Result};
use docker_image::Platform;

/// Canonical registry/repository pair, with Docker Hub implied defaults made
/// explicit and the `library/` namespace inserted exactly where Docker does.
pub fn canonical_registry_repo(image: &ImageRef) -> (String, String) {
    let registry = match image.registry().map(str::to_ascii_lowercase) {
        None => "docker.io".to_string(),
        Some(registry) if matches!(registry.as_str(), "docker.io" | "index.docker.io") => {
            "docker.io".to_string()
        }
        Some(registry) => registry,
    };
    let mut repo = image.repo();
    if registry == "docker.io" && repo.split('/').count() == 1 {
        repo = format!("library/{repo}");
    }
    (registry, repo)
}

/// Canonical local tag key (`registry/repo:tag`). References with an
/// explicit digest are repository digests, not tags.
pub fn canonical_tag(image: &ImageRef) -> Result<String> {
    if image.digest().is_some() {
        return Err(Error::invalid_argument(
            "a digest-pinned reference cannot be stored as a tag",
        ));
    }
    let (registry, repo) = canonical_registry_repo(image);
    Ok(format!("{registry}/{repo}:{}", image.effective_tag()))
}

/// Canonical tag key for an API `repo` + `tag` pair. `repo` may contain a
/// registry and port but must not already include a tag or digest.
pub fn canonical_tag_from_parts(repo: &str, tag: &str) -> Result<String> {
    let repo = repo.trim();
    if repo.is_empty() {
        return Err(Error::invalid_argument("repository name must not be empty"));
    }
    if repo.contains('@') {
        return Err(Error::invalid_argument(
            "repository name must not include a digest",
        ));
    }
    let last = repo.rsplit('/').next().unwrap_or(repo);
    if last.contains(':') {
        return Err(Error::invalid_argument(
            "repository name must not include a tag; pass the tag separately",
        ));
    }
    let tag = if tag.is_empty() { "latest" } else { tag };
    let parsed = ImageRef::parse(&format!("{repo}:{tag}"))?;
    canonical_tag(&parsed)
}

/// Canonical repository digest (`registry/repo@sha256:...`).
pub fn canonical_repo_digest(image: &ImageRef, digest: &Digest) -> String {
    let (registry, repo) = canonical_registry_repo(image);
    format!("{registry}/{repo}@{}", digest.full())
}

/// Turn a canonical tag/digest string into the name Docker's CLI displays.
pub fn familiar_reference(canonical: &str) -> String {
    if let Some((repo, digest)) = canonical.split_once('@') {
        return format!("{}@{digest}", familiar_repo(repo));
    }
    let slash = canonical.rfind('/');
    let colon = canonical.rfind(':');
    match (slash, colon) {
        (Some(slash), Some(colon)) if colon > slash => {
            format!(
                "{}:{}",
                familiar_repo(&canonical[..colon]),
                &canonical[colon + 1..]
            )
        }
        _ => familiar_repo(canonical),
    }
}

fn familiar_repo(repo: &str) -> String {
    let Some(rest) = repo.strip_prefix("docker.io/") else {
        return repo.to_string();
    };
    if let Some(single) = rest.strip_prefix("library/") {
        if !single.contains('/') {
            return single.to_string();
        }
    }
    rest.to_string()
}

/// Parse a Docker API `platform` value (`os/arch[/variant]`).
pub fn parse_platform(value: &str) -> Result<Platform> {
    let value = value.trim();
    if value.is_empty() {
        return Err(Error::invalid_argument("platform must not be empty"));
    }
    let parts: Vec<&str> = value.split('/').collect();
    if parts.len() < 2 || parts.len() > 3 || parts.iter().any(|part| part.is_empty()) {
        return Err(Error::invalid_argument(format!(
            "invalid platform {value:?}; expected os/arch[/variant]"
        )));
    }
    Ok(Platform {
        os: parts[0].to_string(),
        architecture: parts[1].to_string(),
        variant: parts.get(2).map(|variant| variant.to_string()),
        os_version: None,
        os_features: None,
    })
}

/// Platform used when a pull request does not specify one. The native
/// runtime is Linux-only, but on Windows we still default to the host
/// platform: silently pulling a Linux image for a Windows engine would be a
/// lie. Callers can pass `platform=linux/amd64` explicitly.
pub fn host_platform() -> Platform {
    let os = if cfg!(windows) { "windows" } else { "linux" };
    let architecture = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "x86" => "386",
        "aarch64" => "arm64",
        "arm" => "arm",
        "powerpc64" => "ppc64",
        "powerpc64le" => "ppc64le",
        "s390x" => "s390x",
        other => other,
    };
    Platform {
        os: os.to_string(),
        architecture: architecture.to_string(),
        ..Platform::default()
    }
}

pub(crate) fn platform_matches(candidate: &Platform, wanted: &Platform) -> bool {
    if candidate.os != wanted.os || candidate.architecture != wanted.architecture {
        return false;
    }
    match (&wanted.variant, &candidate.variant) {
        (Some(wanted), Some(candidate)) => wanted == candidate,
        (Some(_), None) => false,
        (None, _) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(value: &str) -> ImageRef {
        ImageRef::parse(value).unwrap()
    }

    #[test]
    fn canonicalizes_docker_hub_names() {
        assert_eq!(
            canonical_tag(&image("alpine")).unwrap(),
            "docker.io/library/alpine:latest"
        );
        assert_eq!(
            canonical_tag(&image("alpine:3.20")).unwrap(),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            canonical_tag(&image("user/app:edge")).unwrap(),
            "docker.io/user/app:edge"
        );
        assert_eq!(
            canonical_tag(&image("localhost:5000/team/app:dev")).unwrap(),
            "localhost:5000/team/app:dev"
        );
    }

    #[test]
    fn repository_digest_is_canonical_and_familiarizes() {
        let digest = Digest::parse(
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let canonical = canonical_repo_digest(&image("alpine"), &digest);
        assert_eq!(
            canonical,
            format!("docker.io/library/alpine@{}", digest.full())
        );
        assert_eq!(
            familiar_reference(&canonical),
            format!("alpine@{}", digest.full())
        );
        assert_eq!(
            familiar_reference("docker.io/library/alpine:3.20"),
            "alpine:3.20"
        );
        assert_eq!(
            familiar_reference("docker.io/user/app:edge"),
            "user/app:edge"
        );
        assert_eq!(
            familiar_reference("localhost:5000/team/app:dev"),
            "localhost:5000/team/app:dev"
        );
    }

    #[test]
    fn builds_and_validates_tag_pairs() {
        assert_eq!(
            canonical_tag_from_parts("alpine", "3.20").unwrap(),
            "docker.io/library/alpine:3.20"
        );
        assert_eq!(
            canonical_tag_from_parts("localhost:5000/team/app", "").unwrap(),
            "localhost:5000/team/app:latest"
        );
        assert!(canonical_tag_from_parts("alpine:3.20", "latest").is_err());
        assert!(canonical_tag_from_parts("", "latest").is_err());
        assert!(canonical_tag_from_parts("alpine@sha256:abc", "latest").is_err());
    }

    #[test]
    fn parses_platforms_and_host_default() {
        let platform = parse_platform("linux/arm64/v8").unwrap();
        assert_eq!(platform.os, "linux");
        assert_eq!(platform.architecture, "arm64");
        assert_eq!(platform.variant.as_deref(), Some("v8"));
        assert!(parse_platform("linux").is_err());
        assert!(parse_platform("linux/amd64/extra/parts").is_err());
        assert!(!host_platform().architecture.is_empty());
    }

    #[test]
    fn matches_requested_variant() {
        let wanted = parse_platform("linux/arm64/v8").unwrap();
        let mut candidate = wanted.clone();
        assert!(platform_matches(&candidate, &wanted));
        candidate.variant = Some("v9".into());
        assert!(!platform_matches(&candidate, &wanted));
        candidate.variant = None;
        assert!(!platform_matches(&candidate, &wanted));
    }
}
