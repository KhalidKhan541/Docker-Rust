//! Narrow absolute-URL handling for the registry client.
//!
//! Scope is deliberately small: HTTP(S) URLs as they appear in registry base
//! URLs and in `Location`/`Link` headers. No userinfo, no fragments, no
//! percent-*decoding*. Relative references are resolved per RFC 3986 §5.2
//! (path merge + dot-segment removal), which is what registries actually
//! exercise with `Location: /v2/...` and `Link: </v2/...>; rel="next"`.

use std::fmt;

use docker_errors::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scheme {
    Http,
    Https,
}

impl Scheme {
    pub fn default_port(self) -> u16 {
        match self {
            Scheme::Http => 80,
            Scheme::Https => 443,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scheme::Http => "http",
            Scheme::Https => "https",
        }
    }
}

/// An absolute `scheme://host[:port]/path[?query]` URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    scheme: Scheme,
    /// Lowercased host, without IPv6 brackets.
    host: String,
    /// Explicit non-default port; `None` means the scheme default.
    port: Option<u16>,
    /// Always starts with `/`.
    path: String,
    query: Option<String>,
}

impl Url {
    pub fn parse(input: &str) -> Result<Self> {
        if input.contains('#') {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: fragments are not part of HTTP requests"
            )));
        }
        let (scheme_str, rest) = input.split_once("://").ok_or_else(|| {
            Error::invalid_argument(format!("invalid URL `{input}`: missing scheme"))
        })?;
        let scheme = match scheme_str.to_ascii_lowercase().as_str() {
            "http" => Scheme::Http,
            "https" => Scheme::Https,
            other => {
                return Err(Error::unsupported(format!(
                    "unsupported URL scheme `{other}` in `{input}`; supported: http, https"
                )))
            }
        };
        if rest.is_empty() {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: empty authority"
            )));
        }
        let (authority, tail) = match rest.find(['/', '?']) {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, ""),
        };
        if authority.contains('@') {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: embedded credentials are not supported"
            )));
        }
        let (host, port) = parse_authority(authority, input)?;
        let (path, query) = split_path_query(tail);
        let mut port = port;
        if port == Some(scheme.default_port()) {
            port = None;
        }
        Ok(Self {
            scheme,
            host,
            port,
            path,
            query,
        })
    }

    pub fn scheme(&self) -> Scheme {
        self.scheme
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    /// Effective port (explicit or scheme default).
    pub fn port(&self) -> u16 {
        self.port.unwrap_or(self.scheme.default_port())
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }

    /// `scheme://host[:port]` — used for origin comparisons.
    pub fn origin(&self) -> String {
        format!("{}://{}", self.scheme.as_str(), self.authority())
    }

    /// `host[:port]` with IPv6 brackets — used for the `Host` header.
    pub fn authority(&self) -> String {
        let mut out = String::new();
        if self.host.contains(':') {
            out.push('[');
            out.push_str(&self.host);
            out.push(']');
        } else {
            out.push_str(&self.host);
        }
        if let Some(port) = self.port {
            out.push(':');
            out.push_str(&port.to_string());
        }
        out
    }

    pub fn path_and_query(&self) -> String {
        match &self.query {
            Some(q) => format!("{}?{}", self.path, q),
            None => self.path.clone(),
        }
    }

    pub fn is_secure(&self) -> bool {
        matches!(self.scheme, Scheme::Https)
    }

    /// Same URL with the query replaced (`None` removes it).
    pub fn with_query(&self, query: Option<String>) -> Self {
        Self {
            scheme: self.scheme,
            host: self.host.clone(),
            port: self.port,
            path: self.path.clone(),
            query: query.filter(|q| !q.is_empty()),
        }
    }

    /// Resolve a `Location`/`Link` reference against this URL.
    pub fn join(&self, reference: &str) -> Result<Self> {
        let reference = reference.trim();
        if reference.is_empty() {
            return Err(Error::invalid_argument("empty URL reference"));
        }
        if reference.contains("://") {
            return Url::parse(reference);
        }
        if let Some(rest) = reference.strip_prefix("//") {
            // Scheme-relative reference.
            return Url::parse(&format!("{}://{}", self.scheme.as_str(), rest));
        }
        if reference.contains('#') {
            return Err(Error::invalid_argument(format!(
                "invalid URL reference `{reference}`: fragments are not supported"
            )));
        }
        let (ref_path, ref_query) = match reference.split_once('?') {
            Some((path, query)) => (path, Some(query.to_string())),
            None => (reference, None),
        };
        if ref_path.is_empty() {
            // Query-only reference keeps the current path.
            return Ok(Self {
                scheme: self.scheme,
                host: self.host.clone(),
                port: self.port,
                path: self.path.clone(),
                query: ref_query.filter(|q| !q.is_empty()),
            });
        }
        let combined = if ref_path.starts_with('/') {
            ref_path.to_string()
        } else {
            let base_dir = match self.path.rfind('/') {
                Some(idx) => &self.path[..=idx],
                None => "/",
            };
            format!("{base_dir}{ref_path}")
        };
        Ok(Self {
            scheme: self.scheme,
            host: self.host.clone(),
            port: self.port,
            path: remove_dot_segments(&combined),
            query: ref_query.filter(|q| !q.is_empty()),
        })
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}://{}{}",
            self.scheme.as_str(),
            self.authority(),
            self.path_and_query()
        )
    }
}

fn parse_authority(authority: &str, input: &str) -> Result<(String, Option<u16>)> {
    if authority.is_empty() {
        return Err(Error::invalid_argument(format!(
            "invalid URL `{input}`: empty host"
        )));
    }
    if let Some(rest) = authority.strip_prefix('[') {
        // IPv6 literal.
        let (host, after) = rest.split_once(']').ok_or_else(|| {
            Error::invalid_argument(format!("invalid URL `{input}`: unclosed IPv6 bracket"))
        })?;
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
        {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: bad IPv6 host"
            )));
        }
        let port = if after.is_empty() {
            None
        } else if let Some(p) = after.strip_prefix(':') {
            Some(parse_port(p, input)?)
        } else {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: trailing authority characters"
            )));
        };
        return Ok((host.to_ascii_lowercase(), port));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => {
            (h, Some(parse_port(p, input)?))
        }
        Some((_, "")) => {
            return Err(Error::invalid_argument(format!(
                "invalid URL `{input}`: empty port"
            )))
        }
        _ => (authority, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
    {
        return Err(Error::invalid_argument(format!(
            "invalid URL `{input}`: bad host `{host}`"
        )));
    }
    Ok((host.to_ascii_lowercase(), port))
}

fn parse_port(digits: &str, input: &str) -> Result<u16> {
    let n: u32 = digits.parse().map_err(|_| {
        Error::invalid_argument(format!("invalid URL `{input}`: bad port `{digits}`"))
    })?;
    if n == 0 || n > u16::MAX as u32 {
        return Err(Error::invalid_argument(format!(
            "invalid URL `{input}`: port out of range `{digits}`"
        )));
    }
    Ok(n as u16)
}

fn split_path_query(tail: &str) -> (String, Option<String>) {
    let (path, query) = match tail.split_once('?') {
        Some((path, query)) => (path, Some(query.to_string())),
        None => (tail, None),
    };
    let path = if path.is_empty() {
        "/".to_string()
    } else if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    (path, query.filter(|q| !q.is_empty()))
}

/// RFC 3986 §5.2.4 dot-segment removal for absolute paths.
fn remove_dot_segments(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "" | "." => {}
            ".." => {
                segments.pop();
            }
            other => segments.push(other),
        }
    }
    let mut out = String::new();
    if absolute {
        out.push('/');
    }
    out.push_str(&segments.join("/"));
    if path.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Percent-encode a query-string value, keeping RFC 3986 unreserved
/// characters plus the separators registries conventionally leave readable
/// (`:`, `/`, `+`, `.`, `_`, `-`, `~`).
pub fn encode_query_value(value: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        let keep = b.is_ascii_alphanumeric()
            || matches!(b, b':' | b'/' | b'+' | b'.' | b'_' | b'-' | b'~');
        if keep {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(HEX[(b >> 4) as usize] as char);
            out.push(HEX[(b & 0x0f) as usize] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_https() {
        let u = Url::parse("https://registry.example.com").unwrap();
        assert_eq!(u.scheme(), Scheme::Https);
        assert_eq!(u.host(), "registry.example.com");
        assert_eq!(u.port(), 443);
        assert_eq!(u.path(), "/");
        assert_eq!(u.to_string(), "https://registry.example.com/");
    }

    #[test]
    fn parses_port_and_path_query() {
        let u = Url::parse("http://127.0.0.1:5000/v2/lib/app/blobs/uploads/?x=1&y=2").unwrap();
        assert_eq!(u.port(), 5000);
        assert_eq!(u.authority(), "127.0.0.1:5000");
        assert_eq!(u.path(), "/v2/lib/app/blobs/uploads/");
        assert_eq!(u.path_and_query(), "/v2/lib/app/blobs/uploads/?x=1&y=2");
        assert_eq!(u.query(), Some("x=1&y=2"));
    }

    #[test]
    fn normalizes_default_port_and_host_case() {
        let u = Url::parse("HTTPS://Registry.Example.com:443/x").unwrap();
        assert_eq!(u.host(), "registry.example.com");
        assert_eq!(u.port(), 443);
        assert_eq!(u.authority(), "registry.example.com");
    }

    #[test]
    fn parses_ipv6() {
        let u = Url::parse("http://[::1]:5000/v2/").unwrap();
        assert_eq!(u.host(), "::1");
        assert_eq!(u.authority(), "[::1]:5000");
    }

    #[test]
    fn rejects_bad_urls() {
        assert!(Url::parse("ftp://example.com").is_err());
        assert!(Url::parse("https://user:pass@example.com").is_err());
        assert!(Url::parse("https://example.com/x#frag").is_err());
        assert!(Url::parse("example.com").is_err());
        assert!(Url::parse("http://example.com:0/").is_err());
        assert!(Url::parse("http://example.com:99999/").is_err());
        assert!(Url::parse("http://exa mple.com/").is_err());
    }

    #[test]
    fn joins_absolute_paths_and_queries() {
        let base = Url::parse("https://registry.example.com/v2/team/app").unwrap();
        let abs = base.join("/v2/team/app/manifests/latest").unwrap();
        assert_eq!(
            abs.to_string(),
            "https://registry.example.com/v2/team/app/manifests/latest"
        );

        let q = base.join("?digest=sha256:abc").unwrap();
        assert_eq!(q.path(), "/v2/team/app");
        assert_eq!(q.query(), Some("digest=sha256:abc"));

        let rel = base.join("manifests/latest").unwrap();
        assert_eq!(rel.path(), "/v2/team/manifests/latest");

        let dotted = base.join("../other/manifests/x").unwrap();
        assert_eq!(dotted.path(), "/v2/other/manifests/x");

        let scheme_rel = base.join("//cdn.example.com/blob").unwrap();
        assert_eq!(scheme_rel.host(), "cdn.example.com");
        assert_eq!(scheme_rel.scheme(), Scheme::Https);
    }

    #[test]
    fn encodes_query_values() {
        assert_eq!(encode_query_value("sha256:abc"), "sha256:abc");
        assert_eq!(
            encode_query_value("application/vnd.example+json"),
            "application/vnd.example+json"
        );
        assert_eq!(encode_query_value("a b&c=d"), "a%20b%26c%3Dd");
    }

    #[test]
    fn same_url_with_new_query() {
        let u = Url::parse("https://h.example/p?x=1").unwrap();
        let v = u.with_query(Some("y=2".into()));
        assert_eq!(v.query(), Some("y=2"));
        assert_eq!(v.path(), "/p");
        assert!(v.with_query(None).query().is_none());
    }
}
