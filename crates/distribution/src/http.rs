//! Minimal blocking HTTP/1.1 client for the registry protocol.
//!
//! Only what the distribution client needs: one request per connection
//! (`Connection: close`), request bodies from memory or a file (streamed),
//! response bodies framed by Content-Length or chunked encoding (or EOF),
//! redirect following with credentials stripped on cross-origin hops, and
//! TLS through the platform backend (`native-tls`: SChannel on Windows,
//! OpenSSL elsewhere).
//!
//! Explicitly out of scope (reported, not silently degraded): HTTP/2,
//! proxies, cookies, connection reuse, and calling `https` URLs on a host
//! whose TLS backend fails to initialize.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

use docker_errors::{Error, Result};

use crate::url::{Scheme, Url};

/// Maximum total size of a response head (status line + headers).
const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Maximum number of response header fields.
const MAX_HEADERS: usize = 128;
/// Maximum size of one chunk-size/trailer line.
const MAX_CHUNK_LINE_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Head => "HEAD",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Patch => "PATCH",
            Method::Delete => "DELETE",
        }
    }
}

/// Order-preserving, case-insensitive header collection. Repeated headers
/// (`WWW-Authenticate`, `Warning`, `Link`) are preserved for `get_all`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Headers {
    entries: Vec<(String, String)>,
}

impl Headers {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn get_all(&self, name: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Replace all existing values for `name`.
    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        let name = name.into();
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(&name));
        self.entries.push((name, value.into()));
    }

    pub fn append(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries.push((name.into(), value.into()));
    }

    pub fn remove(&mut self, name: &str) {
        self.entries.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().map(|(n, v)| (n.as_str(), v.as_str()))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    Empty,
    Bytes(Vec<u8>),
    /// Streamed at send time from disk; the length must be known upfront.
    File {
        path: PathBuf,
        len: u64,
    },
}

impl Body {
    pub fn len(&self) -> Option<u64> {
        match self {
            Body::Empty => Some(0),
            Body::Bytes(bytes) => Some(bytes.len() as u64),
            Body::File { len, .. } => Some(*len),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Body::Empty)
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub url: Url,
    pub headers: Headers,
    pub body: Body,
}

impl Request {
    pub fn new(method: Method, url: Url) -> Self {
        Self {
            method,
            url,
            headers: Headers::new(),
            body: Body::Empty,
        }
    }

    pub fn with_body(mut self, body: Body) -> Self {
        self.body = body;
        self
    }
}

/// A response head plus a (possibly streaming) body reader.
pub struct Response {
    pub status: u16,
    pub headers: Headers,
    pub body: Box<dyn Read + Send>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body", &"<stream>")
            .finish()
    }
}

impl Response {
    /// Read the rest of the body, refusing to exceed `limit` bytes.
    pub fn body_bytes_limited(mut self, limit: usize) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        self.body
            .by_ref()
            .take(limit as u64 + 1)
            .read_to_end(&mut out)?;
        if out.len() > limit {
            return Err(Error::invalid_argument(format!(
                "response body exceeds the {limit}-byte limit"
            )));
        }
        Ok(out)
    }
}

/// One logical request (redirects included). Tests and later subsystems
/// (daemon, CLI) can substitute their own transport.
pub trait Transport {
    fn execute(&self, request: &Request) -> Result<Response>;
}

/// Blocking HTTP/1.1 transport over `std::net` + `native-tls`.
#[derive(Debug, Clone)]
pub struct StdTransport {
    connect_timeout: Duration,
    io_timeout: Duration,
    max_redirects: usize,
}

impl Default for StdTransport {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(30),
            io_timeout: Duration::from_secs(60),
            max_redirects: 5,
        }
    }
}

impl StdTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_timeouts(mut self, connect: Duration, io: Duration) -> Self {
        self.connect_timeout = connect;
        self.io_timeout = io;
        self
    }

    pub fn with_max_redirects(mut self, max: usize) -> Self {
        self.max_redirects = max;
        self
    }

    fn connect(&self, url: &Url) -> Result<Connection> {
        let host = url.host();
        let port = url.port();
        let addrs: Vec<SocketAddr> = (host, port)
            .to_socket_addrs()
            .map_err(|e| Error::unavailable(format!("cannot resolve registry host `{host}`: {e}")))?
            .collect();
        if addrs.is_empty() {
            return Err(Error::unavailable(format!(
                "cannot resolve registry host `{host}`: no addresses"
            )));
        }
        let mut stream = None;
        let mut last_err = None;
        for addr in addrs {
            match TcpStream::connect_timeout(&addr, self.connect_timeout) {
                Ok(s) => {
                    stream = Some(s);
                    break;
                }
                Err(e) => last_err = Some(e),
            }
        }
        let stream = stream.ok_or_else(|| {
            Error::unavailable(format!(
                "cannot connect to `{host}:{port}`: {}",
                last_err
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "no attempt made".to_string())
            ))
        })?;
        stream.set_read_timeout(Some(self.io_timeout))?;
        stream.set_write_timeout(Some(self.io_timeout))?;
        let _ = stream.set_nodelay(true);
        match url.scheme() {
            Scheme::Http => Ok(Connection::Plain(stream)),
            Scheme::Https => {
                let connector = native_tls::TlsConnector::new()
                    .map_err(|e| Error::unsupported(format!("TLS backend unavailable: {e}")))?;
                let tls = connector.connect(host, stream).map_err(|e| {
                    Error::unavailable(format!("TLS handshake with `{host}` failed: {e}"))
                })?;
                Ok(Connection::Tls(Box::new(tls)))
            }
        }
    }

    fn send_once(&self, request: &Request) -> Result<Response> {
        let mut connection = self.connect(&request.url)?;
        write_request(&mut connection, request)?;
        read_response(BufReader::new(connection), request.method == Method::Head)
    }
}

impl Transport for StdTransport {
    fn execute(&self, request: &Request) -> Result<Response> {
        let mut current = request.clone();
        let mut hops = 0usize;
        loop {
            let response = self.send_once(&current)?;
            let location = response.headers.get("location").map(str::to_string);
            let Some(location) = location else {
                return Ok(response);
            };
            if !is_redirect(response.status) {
                return Ok(response);
            }
            if hops >= self.max_redirects {
                return Err(Error::unavailable(format!(
                    "registry redirect limited exceeded ({hops} hops; last Location `{location}`)"
                )));
            }
            hops += 1;
            let next = current.url.join(&location)?;
            if current.url.is_secure() && !next.is_secure() {
                return Err(Error::unsupported(format!(
                    "refusing HTTPS→HTTP redirect to `{next}`"
                )));
            }
            let mut headers = current.headers.clone();
            let mut method = current.method;
            let mut body = current.body.clone();
            if matches!(response.status, 301..=303) && method != Method::Head {
                method = Method::Get;
                body = Body::Empty;
                headers.remove("content-type");
                headers.remove("content-range");
            }
            headers.remove("content-length");
            if next.origin() != current.url.origin() {
                // Never leak registry credentials to a redirect target.
                headers.remove("authorization");
                headers.remove("cookie");
                headers.remove("proxy-authorization");
            }
            current = Request {
                method,
                url: next,
                headers,
                body,
            };
        }
    }
}

fn is_redirect(status: u16) -> bool {
    matches!(status, 301 | 302 | 303 | 307 | 308)
}

enum Connection {
    Plain(TcpStream),
    Tls(Box<native_tls::TlsStream<TcpStream>>),
}

impl Read for Connection {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Connection::Plain(s) => s.read(buf),
            Connection::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Connection {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Connection::Plain(s) => s.write(buf),
            Connection::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Connection::Plain(s) => s.flush(),
            Connection::Tls(s) => s.flush(),
        }
    }
}

fn validate_header(name: &str, value: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
    {
        return Err(Error::invalid_argument(format!(
            "invalid HTTP header name `{name}`"
        )));
    }
    if value.contains('\r') || value.contains('\n') {
        return Err(Error::invalid_argument(format!(
            "HTTP header `{name}` value contains a line break"
        )));
    }
    Ok(())
}

fn write_request<W: Write>(writer: &mut W, request: &Request) -> Result<()> {
    let target = request.url.path_and_query();
    // A request target must be visible ASCII; spaces, tabs, and other
    // controls would let a hostile `Location`/`Link` value forge the request
    // line.
    if !target.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        return Err(Error::invalid_argument(format!(
            "request target `{target}` contains characters that must be percent-encoded"
        )));
    }
    let mut head = String::with_capacity(256);
    head.push_str(request.method.as_str());
    head.push(' ');
    head.push_str(&target);
    head.push_str(" HTTP/1.1\r\n");
    head.push_str("Host: ");
    head.push_str(&request.url.authority());
    head.push_str("\r\n");
    for (name, value) in request.headers.iter() {
        validate_header(name, value)?;
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    }
    if request.headers.get("content-length").is_none() {
        // GET/HEAD/DELETE without a body must not advertise a zero-length
        // body; some servers treat that as a malformed request.
        let sends_body = !matches!(request.method, Method::Get | Method::Head | Method::Delete)
            || !request.body.is_empty();
        if sends_body {
            if let Some(len) = request.body.len() {
                head.push_str(&format!("Content-Length: {len}\r\n"));
            }
        }
    }
    if request.headers.get("connection").is_none() {
        head.push_str("Connection: close\r\n");
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    match &request.body {
        Body::Empty => {}
        Body::Bytes(bytes) => {
            check_declared_length(request, bytes.len() as u64)?;
            writer.write_all(bytes)?;
        }
        Body::File { path, len } => {
            check_declared_length(request, *len)?;
            let mut file = std::fs::File::open(path)?;
            let mut source = Read::by_ref(&mut file).take(*len);
            let copied = std::io::copy(&mut source, writer)?;
            if copied != *len {
                return Err(Error::invalid_argument(format!(
                    "blob file `{}` is shorter than its recorded length {len}",
                    path.display()
                )));
            }
        }
    }
    writer.flush()?;
    Ok(())
}

fn check_declared_length(request: &Request, actual: u64) -> Result<()> {
    if let Some(raw) = request.headers.get("content-length") {
        let declared: u64 = raw.trim().parse().map_err(|_| {
            Error::invalid_argument(format!("invalid request Content-Length `{raw}`"))
        })?;
        if declared != actual {
            return Err(Error::invalid_argument(format!(
                "request Content-Length {declared} does not match body length {actual}"
            )));
        }
    }
    Ok(())
}

fn read_line_limited<R: BufRead>(reader: &mut R, budget: &mut usize) -> Result<String> {
    let mut raw = Vec::new();
    let read = reader.read_until(b'\n', &mut raw)?;
    if read == 0 {
        return Err(Error::unavailable(
            "registry closed the connection before a complete response head arrived",
        ));
    }
    if raw.len() > *budget {
        return Err(Error::unavailable(format!(
            "response head exceeds the {MAX_HEADER_BYTES}-byte limit"
        )));
    }
    *budget -= raw.len();
    while matches!(raw.last(), Some(b'\n') | Some(b'\r')) {
        raw.pop();
    }
    String::from_utf8(raw)
        .map_err(|_| Error::unavailable("response head contains non-UTF-8 header text"))
}

fn read_response<R: BufRead + Send + 'static>(
    mut reader: R,
    head_request: bool,
) -> Result<Response> {
    let mut budget = MAX_HEADER_BYTES;
    let status_line = read_line_limited(&mut reader, &mut budget)?;
    let mut parts = status_line.splitn(3, ' ');
    let version = parts.next().unwrap_or("");
    if version != "HTTP/1.1" && version != "HTTP/1.0" {
        return Err(Error::unavailable(format!(
            "invalid HTTP status line `{status_line}`"
        )));
    }
    let status: u16 = parts
        .next()
        .ok_or_else(|| Error::unavailable(format!("invalid HTTP status line `{status_line}`")))?
        .parse()
        .map_err(|_| Error::unavailable(format!("invalid HTTP status code in `{status_line}`")))?;
    let mut headers = Headers::new();
    loop {
        let line = read_line_limited(&mut reader, &mut budget)?;
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| Error::unavailable(format!("invalid HTTP header line `{line}`")))?;
        let name = name.trim();
        if headers.len() >= MAX_HEADERS {
            return Err(Error::unavailable("too many response headers"));
        }
        validate_header(name, value.trim())
            .map_err(|e| Error::unavailable(format!("invalid response header: {}", e.message())))?;
        headers.append(name, value.trim());
    }
    let no_body = head_request || status == 204 || status == 304 || (100..200).contains(&status);
    let body: Box<dyn Read + Send> = if no_body {
        Box::new(std::io::empty())
    } else if headers
        .get("transfer-encoding")
        .map(|v| v.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        Box::new(ChunkedReader::new(reader))
    } else if let Some(raw) = headers.get("content-length") {
        let len: u64 = raw
            .trim()
            .parse()
            .map_err(|_| Error::unavailable(format!("invalid response Content-Length `{raw}`")))?;
        Box::new(reader.take(len))
    } else {
        Box::new(reader)
    };
    Ok(Response {
        status,
        headers,
        body,
    })
}

struct ChunkedReader<R: Read> {
    inner: R,
    remaining: u64,
    done: bool,
}

impl<R: Read> ChunkedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            remaining: 0,
            done: false,
        }
    }

    fn read_line(&mut self) -> std::io::Result<Vec<u8>> {
        let mut line = Vec::new();
        loop {
            let mut byte = [0u8; 1];
            if self.inner.read(&mut byte)? == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "truncated chunked transfer",
                ));
            }
            if byte[0] == b'\n' {
                break;
            }
            if byte[0] != b'\r' {
                line.push(byte[0]);
            }
            if line.len() > MAX_CHUNK_LINE_BYTES {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "chunk header line too long",
                ));
            }
        }
        Ok(line)
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() || self.done {
            return Ok(0);
        }
        if self.remaining == 0 {
            let line = self.read_line()?;
            let size_part = line.split(|&b| b == b';').next().unwrap_or(&[]);
            let text = std::str::from_utf8(size_part)
                .map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "invalid chunk-size encoding",
                    )
                })?
                .trim();
            let size = u64::from_str_radix(text, 16).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("invalid chunk size `{text}`"),
                )
            })?;
            if size == 0 {
                // Consume optional trailers up to the terminating blank line.
                loop {
                    let trailer = self.read_line()?;
                    if trailer.is_empty() {
                        break;
                    }
                }
                self.done = true;
                return Ok(0);
            }
            self.remaining = size;
        }
        let want = buf
            .len()
            .min(self.remaining.min(usize::MAX as u64) as usize);
        let read = self.inner.read(&mut buf[..want])?;
        if read == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "truncated chunk data",
            ));
        }
        self.remaining -= read as u64;
        if self.remaining == 0 {
            let mut crlf = [0u8; 2];
            self.inner.read_exact(&mut crlf)?;
            if &crlf != b"\r\n" {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "missing chunk terminator",
                ));
            }
        }
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse_url(raw: &str) -> Url {
        Url::parse(raw).unwrap()
    }

    #[test]
    fn headers_are_case_insensitive_and_ordered() {
        let mut headers = Headers::new();
        headers.append("WWW-Authenticate", "Basic realm=\"a\"");
        headers.append("www-authenticate", "Bearer realm=\"b\"");
        assert_eq!(
            headers.get_all("www-authenticate"),
            vec!["Basic realm=\"a\"", "Bearer realm=\"b\""]
        );
        headers.insert("Content-Type", "text/plain");
        assert_eq!(headers.get("content-type"), Some("text/plain"));
        headers.insert("content-type", "application/json");
        assert_eq!(headers.get_all("Content-Type"), vec!["application/json"]);
        headers.remove("CONTENT-TYPE");
        assert!(headers.get("content-type").is_none());
    }

    #[test]
    fn writes_request_head_and_body() {
        let mut req = Request::new(
            Method::Put,
            parse_url("http://127.0.0.1:5000/v2/a/manifests/v1"),
        );
        req.headers
            .insert("content-type", "application/vnd.oci.image.manifest.v1+json");
        req.body = Body::Bytes(b"{}".to_vec());
        let mut out = Vec::new();
        write_request(&mut out, &req).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("PUT /v2/a/manifests/v1 HTTP/1.1\r\n"));
        assert!(text.contains("Host: 127.0.0.1:5000\r\n"));
        assert!(text.contains("content-type: application/vnd.oci.image.manifest.v1+json\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }

    #[test]
    fn rejects_header_injection() {
        let mut req = Request::new(Method::Get, parse_url("http://example.test/"));
        req.headers.insert("x-evil", "a\r\nInjected: yes");
        let err = write_request(&mut Vec::new(), &req).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
    }

    #[test]
    fn rejects_body_length_mismatch() {
        let mut req = Request::new(Method::Post, parse_url("http://example.test/"));
        req.headers.insert("content-length", "5");
        req.body = Body::Bytes(b"abc".to_vec());
        let err = write_request(&mut Vec::new(), &req).unwrap_err();
        assert!(err.message().contains("does not match"));
    }

    #[test]
    fn rejects_request_targets_needing_percent_encoding() {
        let req = Request::new(Method::Get, parse_url("http://example.test/a b"));
        let err = write_request(&mut Vec::new(), &req).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::InvalidArgument);
        assert!(err.message().contains("percent-encoded"));
    }

    #[test]
    fn bodyless_requests_do_not_advertise_content_length() {
        let req = Request::new(Method::Get, parse_url("http://example.test/v2/"));
        let mut out = Vec::new();
        write_request(&mut out, &req).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.to_ascii_lowercase().contains("content-length"));
        assert!(text.contains("Connection: close\r\n"));
    }

    #[test]
    fn reads_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 5\r\n\r\nhelloEXTRA"
            .to_vec();
        let response = read_response(BufReader::new(Cursor::new(raw)), false).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(
            response.headers.get("content-type"),
            Some("application/json")
        );
        assert_eq!(
            response.body_bytes_limited(16).unwrap().as_slice(),
            b"hello"
        );
    }

    #[test]
    fn reads_chunked_body_with_trailers() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5;ext=1\r\npedia\r\n0\r\nX-Trailer: y\r\n\r\n"
            .to_vec();
        let response = read_response(BufReader::new(Cursor::new(raw)), false).unwrap();
        assert_eq!(
            response.body_bytes_limited(64).unwrap().as_slice(),
            b"Wikipedia"
        );
    }

    #[test]
    fn head_response_body_is_empty() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".to_vec();
        let response = read_response(BufReader::new(Cursor::new(raw)), true).unwrap();
        assert!(response.body_bytes_limited(16).unwrap().is_empty());
    }

    #[test]
    fn rejects_oversized_head() {
        let mut raw = b"HTTP/1.1 200 OK\r\n".to_vec();
        for _ in 0..200 {
            raw.extend_from_slice(
                b"X-Pad: aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
            );
        }
        raw.extend_from_slice(b"\r\n");
        let err = read_response(BufReader::new(Cursor::new(raw)), false).unwrap_err();
        assert_eq!(err.kind(), docker_errors::ErrorKind::Unavailable);
    }

    #[test]
    fn rejects_malformed_status_line() {
        let raw = b"garbage\r\n\r\n".to_vec();
        assert!(read_response(BufReader::new(Cursor::new(raw)), false).is_err());
    }

    #[test]
    fn body_byte_limit_is_enforced() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n0123456789".to_vec();
        let response = read_response(BufReader::new(Cursor::new(raw)), false).unwrap();
        let err = response.body_bytes_limited(4).unwrap_err();
        assert!(err.message().contains("limit"));
    }
}
