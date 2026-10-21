//! Registry authentication primitives.
//!
//! Parses `WWW-Authenticate` challenges (RFC 7235) as registries emit them:
//!
//! ```text
//! Bearer realm="https://auth.docker.io/token",service="registry.docker.io",
//!        scope="repository:library/alpine:pull"
//! ```
//!
//! and builds the `Basic` credential used on the token endpoint. Token
//! *acquisition* lives in [`crate::client`] because it needs the transport.
//!
//! The parser handles the two cases real registries exercise and that naive
//! `split(',')` parsers get wrong: commas inside quoted parameter values
//! (multi-action scopes) and multiple challenges in one header value
//! (`Basic realm="...", Bearer realm="..."`).

use base64::Engine as _;

/// Bearer challenge parameters relevant to the distribution auth flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BearerChallenge {
    /// Token endpoint (`realm`), required.
    pub realm: String,
    /// Service name the token is scoped to (`service`).
    pub service: Option<String>,
    /// Requested scope (`scope`), e.g. `repository:library/alpine:pull`.
    pub scope: Option<String>,
}

/// One parsed authentication challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Challenge {
    Bearer(BearerChallenge),
    Basic {
        /// Optional `realm` (used only in diagnostics).
        realm: Option<String>,
    },
}

/// Parse all challenges from one or more `WWW-Authenticate` header values.
///
/// Unknown schemes are ignored; malformed parameter tails end the current
/// challenge without discarding earlier valid ones.
pub fn parse_challenges(values: &[&str]) -> Vec<Challenge> {
    let mut out = Vec::new();
    for raw in values {
        parse_header(raw, &mut out);
    }
    out
}

/// Build a `Basic` `Authorization` header value. Credentials are encoded per
/// RFC 7617 (`base64(user ":" password)`).
pub fn basic_authorization(username: &str, password: &str) -> String {
    format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"))
    )
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Scanner<'a> {
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t')) {
            self.pos += 1;
        }
    }

    /// Consume a `token` per RFC 7230. Returns `""` when no token starts here.
    fn token(&mut self) -> &'a str {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if is_token_char(b) {
                self.pos += 1;
            } else {
                break;
            }
        }
        // Token characters are ASCII by construction.
        std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("")
    }

    /// Consume either a quoted-string or a token parameter value.
    fn param_value(&mut self) -> Option<String> {
        if self.peek()? == b'"' {
            self.pos += 1;
            let mut out = String::new();
            while let Some(b) = self.peek() {
                self.pos += 1;
                match b {
                    b'"' => return Some(out),
                    b'\\' => {
                        let escaped = self.peek()?;
                        self.pos += 1;
                        out.push(escaped as char);
                    }
                    other => out.push(other as char),
                }
            }
            None // unterminated quoted-string
        } else {
            let token = self.token();
            if token.is_empty() {
                None
            } else {
                Some(token.to_string())
            }
        }
    }
}

fn is_token_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

fn parse_header(raw: &str, out: &mut Vec<Challenge>) {
    let mut s = Scanner {
        bytes: raw.as_bytes(),
        pos: 0,
    };
    loop {
        s.skip_ws();
        while s.peek() == Some(b',') {
            s.pos += 1;
            s.skip_ws();
        }
        if s.peek().is_none() {
            return;
        }
        let scheme = s.token();
        if scheme.is_empty() {
            // Not a challenge start (stray punctuation); skip one byte.
            s.pos += 1;
            continue;
        }
        let scheme = scheme.to_ascii_lowercase();
        let mut params: Vec<(String, String)> = Vec::new();
        loop {
            s.skip_ws();
            let Some(_) = s.peek() else { break };
            if s.peek() == Some(b',') {
                // A comma separates either two parameters of this challenge
                // or two challenges. Look ahead: `name =` means parameter.
                s.pos += 1;
                s.skip_ws();
                let name_start = s.pos;
                let name = s.token();
                s.skip_ws();
                if !name.is_empty() && s.peek() == Some(b'=') {
                    s.pos = name_start;
                    // Fall through to the parameter parser below.
                } else {
                    // Another challenge begins at `name_start`.
                    s.pos = name_start;
                    break;
                }
            }
            let name_start = s.pos;
            let name = s.token().to_string();
            if name.is_empty() {
                s.pos = name_start;
                break;
            }
            s.skip_ws();
            if s.peek() != Some(b'=') {
                // Not `name=value`; treat the token as the next challenge.
                s.pos = name_start;
                break;
            }
            s.pos += 1;
            s.skip_ws();
            let Some(value) = s.param_value() else {
                // Unterminated/missing value: abandon the rest of the header.
                s.pos = s.bytes.len();
                break;
            };
            params.push((name.to_ascii_lowercase(), value));
            s.skip_ws();
            if s.peek().is_some() && s.peek() != Some(b',') {
                // Garbage between parameters; abandon this header.
                s.pos = s.bytes.len();
                break;
            }
        }
        let get = |key: &str| {
            params
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        match scheme.as_str() {
            "bearer" => {
                if let Some(realm) = get("realm").filter(|realm| !realm.is_empty()) {
                    out.push(Challenge::Bearer(BearerChallenge {
                        realm,
                        service: get("service"),
                        scope: get("scope"),
                    }));
                }
            }
            "basic" => out.push(Challenge::Basic {
                realm: get("realm"),
            }),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_single_bearer_challenge() {
        let values = [
            r#"Bearer realm="https://auth.docker.io/token",service="registry.docker.io",scope="repository:library/alpine:pull""#,
        ];
        let parsed = parse_challenges(&values);
        assert_eq!(
            parsed,
            vec![Challenge::Bearer(BearerChallenge {
                realm: "https://auth.docker.io/token".into(),
                service: Some("registry.docker.io".into()),
                scope: Some("repository:library/alpine:pull".into()),
            })]
        );
    }

    #[test]
    fn keeps_commas_inside_quoted_scope() {
        let values =
            [r#"Bearer realm="https://auth.example/token",scope="repository:a/b:pull,push""#];
        let parsed = parse_challenges(&values);
        let Challenge::Bearer(bearer) = &parsed[0] else {
            panic!("expected bearer");
        };
        assert_eq!(bearer.scope.as_deref(), Some("repository:a/b:pull,push"));
        assert_eq!(bearer.service, None);
    }

    #[test]
    fn splits_multiple_challenges_in_one_header() {
        let values = [
            r#"Basic realm="registry", Bearer realm="https://auth.example/token",service="example",scope="repository:x:pull""#,
        ];
        let parsed = parse_challenges(&values);
        assert_eq!(parsed.len(), 2);
        assert!(
            matches!(&parsed[0], Challenge::Basic { realm } if realm.as_deref() == Some("registry"))
        );
        assert!(
            matches!(&parsed[1], Challenge::Bearer(b) if b.realm == "https://auth.example/token")
        );
    }

    #[test]
    fn splits_challenges_across_header_values() {
        let values = [
            r#"Basic realm="a""#,
            r#"Bearer realm="https://b/token",scope="repository:c/d:pull""#,
        ];
        let parsed = parse_challenges(&values);
        assert_eq!(parsed.len(), 2);
        assert!(matches!(&parsed[1], Challenge::Bearer(_)));
    }

    #[test]
    fn supports_escaped_quotes() {
        let values = [r#"Bearer realm="https://example/token",error="a \"quoted\" phrase""#];
        let parsed = parse_challenges(&values);
        assert!(matches!(&parsed[0], Challenge::Bearer(b) if b.realm == "https://example/token"));
    }

    #[test]
    fn ignores_unknown_schemes_and_realmless_bearer() {
        let values = [
            "Negotiate",
            r#"Bearer service="no-realm""#,
            r#"Digest realm="x",nonce="y""#,
        ];
        assert!(parse_challenges(&values).is_empty());
    }

    #[test]
    fn basic_header_matches_rfc7617_example() {
        // Aladdin:open sesame -> QWxhZGRpbjpvcGVuIHNlc2FtZQ==
        assert_eq!(
            basic_authorization("Aladdin", "open sesame"),
            "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ=="
        );
    }

    #[test]
    fn lowercase_scheme_and_parameter_names_are_accepted() {
        let values = [r#"bearer REALM="https://x/token",SERVICE="svc""#];
        let parsed = parse_challenges(&values);
        assert!(
            matches!(&parsed[0], Challenge::Bearer(b) if b.realm == "https://x/token" && b.service.as_deref() == Some("svc"))
        );
    }
}
