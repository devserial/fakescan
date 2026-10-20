//! Tiny HTTP/1.1 client for the control API. No TLS: a lab scanner speaks plain HTTP.
//! Hosts on the LAN are allowed. This client is how the server attaches; it never dials
//! anywhere except the configured base URL.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use crate::error::ToolError;

const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseUrl {
    host: String,
    port: u16,
    /// `Host` header value, with brackets around an IPv6 address.
    host_header: String,
}

impl BaseUrl {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let raw = raw.trim();
        if raw.is_empty() || raw.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
            return Err(format!("base url {raw:?} is not an http URL"));
        }
        if raw.contains('@') {
            return Err("base url must not carry userinfo".into());
        }
        let rest = if let Some(rest) = raw.strip_prefix("http://") {
            rest
        } else if raw.starts_with("https://") {
            return Err("base url https is not supported; fakescan's control API is http".into());
        } else {
            return Err(format!("base url must be http:// (got {raw})"));
        };
        let (authority, path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        if !path.is_empty() && path != "/" {
            return Err("base url must not include a path".into());
        }
        if authority.is_empty() {
            return Err("base url host missing".into());
        }
        let (host, port) = split_authority(authority)?;
        if host.is_empty() {
            return Err("base url host missing".into());
        }
        let host_header = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        Ok(Self {
            host: host.to_string(),
            port,
            host_header,
        })
    }
}

fn split_authority(authority: &str) -> Result<(&str, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| format!("bad IPv6 host in {authority}"))?;
        let host = &rest[..end];
        if host.is_empty() || host.contains('%') {
            return Err(format!("bad IPv6 host in {authority}"));
        }
        let after = &rest[end + 1..];
        let port = if after.is_empty() {
            80
        } else {
            let digits = after
                .strip_prefix(':')
                .ok_or_else(|| format!("bad port in {authority}"))?;
            digits
                .parse()
                .map_err(|_| format!("bad port in {authority}"))?
        };
        if port == 0 {
            return Err("base url port 0 is not allowed".into());
        }
        return Ok((host, port));
    }
    if let Some((host, digits)) = authority.rsplit_once(':') {
        if !host.is_empty() && digits.chars().all(|c| c.is_ascii_digit()) {
            let port: u16 = digits
                .parse()
                .map_err(|_| format!("bad port in {authority}"))?;
            if port == 0 {
                return Err("base url port 0 is not allowed".into());
            }
            return Ok((host, port));
        }
    }
    Ok((authority, 80))
}

#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    pub body: Vec<u8>,
}

#[derive(Debug)]
pub enum ClientError {
    Unreachable(String),
    Protocol(String),
}

impl ClientError {
    pub fn into_tool(self) -> ToolError {
        match self {
            ClientError::Unreachable(message) => ToolError::new(
                message,
                "device_unreachable",
                Some(
                    "Start fakescan yourself and point --base-url at it. This server does not start or stop the device."
                        .into(),
                ),
            ),
            ClientError::Protocol(message) => ToolError::new(message, "protocol", None),
        }
    }
}

pub trait DeviceClient {
    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<RawResponse, ClientError>;
}

pub struct HttpDevice {
    base: BaseUrl,
    timeout: Duration,
}

impl HttpDevice {
    pub fn new(base: BaseUrl, timeout: Duration) -> Self {
        Self { base, timeout }
    }
}

impl DeviceClient for HttpDevice {
    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<RawResponse, ClientError> {
        if !path_is_safe(path) {
            return Err(ClientError::Protocol(format!("refusing path {path}")));
        }
        let host = &self.base.host;
        let stream = TcpStream::connect((host.as_str(), self.base.port)).map_err(|e| {
            ClientError::Unreachable(format!(
                "cannot reach http://{} ({e})",
                self.base.host_header
            ))
        })?;
        stream
            .set_read_timeout(Some(self.timeout))
            .map_err(|e| ClientError::Unreachable(format!("timeout: {e}")))?;
        stream
            .set_write_timeout(Some(self.timeout))
            .map_err(|e| ClientError::Unreachable(format!("timeout: {e}")))?;
        let mut stream = stream;
        let body = body.unwrap_or(b"");
        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nAccept: application/json, text/plain\r\nConnection: close\r\n",
            self.base.host_header
        );
        if method != "GET" && method != "DELETE" {
            req.push_str("Content-Type: application/json\r\n");
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        req.push_str("\r\n");
        stream
            .write_all(req.as_bytes())
            .and_then(|_| {
                if method != "GET" && method != "DELETE" {
                    stream.write_all(body)?;
                }
                stream.flush()
            })
            .map_err(|e| {
                ClientError::Unreachable(format!("write to {}: {e}", self.base.host_header))
            })?;
        read_response(&mut stream)
    }
}

fn path_is_safe(path: &str) -> bool {
    if !path.starts_with('/') || path.contains("://") {
        return false;
    }
    !path
        .chars()
        .any(|c| c.is_control() || c == ' ' || c == '\\')
}

fn read_response(stream: &mut TcpStream) -> Result<RawResponse, ClientError> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if buf.len() > MAX_BODY + 64 * 1024 {
            return Err(ClientError::Protocol("response exceeded 16 MiB".into()));
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if buf.is_empty() {
                    return Err(ClientError::Unreachable(format!("read timeout: {e}")));
                }
                break;
            }
            Err(e) => return Err(ClientError::Unreachable(format!("read: {e}"))),
        }
        if let Some(header_end) = find_header_end(&buf) {
            if response_complete(&buf, header_end) {
                break;
            }
        }
    }
    if buf.is_empty() {
        return Err(ClientError::Unreachable(
            "device closed the connection without a response".into(),
        ));
    }
    parse_response(&buf)
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

fn response_complete(buf: &[u8], header_end: usize) -> bool {
    let head = String::from_utf8_lossy(&buf[..header_end]);
    let mut content_length = None;
    let mut chunked = false;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse::<usize>().ok();
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let body = &buf[header_end..];
    if let Some(n) = content_length {
        return body.len() >= n;
    }
    if chunked {
        return body.windows(5).any(|w| w == b"0\r\n\r\n") || body.ends_with(b"0\r\n\r\n");
    }
    false
}

fn parse_response(buf: &[u8]) -> Result<RawResponse, ClientError> {
    let header_end = find_header_end(buf)
        .ok_or_else(|| ClientError::Protocol("response has no header terminator".into()))?;
    let head = std::str::from_utf8(&buf[..header_end])
        .map_err(|_| ClientError::Protocol("response headers are not utf-8".into()))?;
    let status_line = head.lines().next().unwrap_or("");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| ClientError::Protocol(format!("bad status line {status_line:?}")))?;
    let mut content_length = None;
    let mut chunked = false;
    for line in head.lines().skip(1) {
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = Some(
                rest.trim()
                    .parse::<usize>()
                    .map_err(|_| ClientError::Protocol("bad Content-Length".into()))?,
            );
        }
        if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
            chunked = true;
        }
    }
    let raw_body = &buf[header_end..];
    let body = if chunked {
        decode_chunks(raw_body)?
    } else if let Some(n) = content_length {
        if raw_body.len() < n {
            return Err(ClientError::Protocol("short response body".into()));
        }
        raw_body[..n].to_vec()
    } else {
        raw_body.to_vec()
    };
    if body.len() > MAX_BODY {
        return Err(ClientError::Protocol(
            "response body exceeded 16 MiB".into(),
        ));
    }
    Ok(RawResponse { status, body })
}

fn decode_chunks(mut raw: &[u8]) -> Result<Vec<u8>, ClientError> {
    let mut out = Vec::new();
    loop {
        let line_end = raw
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or_else(|| ClientError::Protocol("truncated chunk size".into()))?;
        let line = std::str::from_utf8(&raw[..line_end])
            .map_err(|_| ClientError::Protocol("chunk size is not utf-8".into()))?;
        let size_hex = line.split(';').next().unwrap_or("").trim();
        let size = usize::from_str_radix(size_hex, 16)
            .map_err(|_| ClientError::Protocol(format!("bad chunk size {line:?}")))?;
        raw = &raw[line_end + 2..];
        if size == 0 {
            break;
        }
        if raw.len() < size + 2 {
            return Err(ClientError::Protocol("truncated chunk".into()));
        }
        out.extend_from_slice(&raw[..size]);
        if &raw[size..size + 2] != b"\r\n" {
            return Err(ClientError::Protocol("chunk missing CRLF".into()));
        }
        raw = &raw[size + 2..];
        if out.len() > MAX_BODY {
            return Err(ClientError::Protocol("chunked body exceeded 16 MiB".into()));
        }
    }
    Ok(out)
}

/// Records calls and returns a scripted body. Used by offline tests.
#[cfg(test)]
#[derive(Debug, Default)]
pub struct ScriptedClient {
    pub calls: Vec<RecordedCall>,
    pub status: u16,
    pub body: Vec<u8>,
    pub fail: Option<String>,
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedCall {
    pub method: String,
    pub path: String,
    pub body: Option<Vec<u8>>,
}

#[cfg(test)]
impl DeviceClient for ScriptedClient {
    fn exchange(
        &mut self,
        method: &str,
        path: &str,
        body: Option<&[u8]>,
    ) -> Result<RawResponse, ClientError> {
        self.calls.push(RecordedCall {
            method: method.to_string(),
            path: path.to_string(),
            body: body.map(|b| b.to_vec()),
        });
        if let Some(message) = &self.fail {
            return Err(ClientError::Unreachable(message.clone()));
        }
        let status = if self.status == 0 { 200 } else { self.status };
        Ok(RawResponse {
            status,
            body: self.body.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn base_url_rejects_userinfo_https_and_paths() {
        assert!(BaseUrl::parse("http://127.0.0.1:3920").is_ok());
        assert!(BaseUrl::parse("http://[::1]:3920").is_ok());
        assert!(BaseUrl::parse("https://127.0.0.1:3920").is_err());
        assert!(BaseUrl::parse("http://user:pass@127.0.0.1:3920").is_err());
        assert!(BaseUrl::parse("http://127.0.0.1:3920/v1").is_err());
        assert!(BaseUrl::parse("http://127.0.0.1:3920/\r\nX").is_err());
    }

    #[test]
    fn http_get_and_post_round_trip_on_loopback() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 512];
            loop {
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0, "client closed before the body");
                buf.extend_from_slice(&chunk[..n]);
                if buf.windows(4).any(|w| w == b"\r\n\r\n")
                    && String::from_utf8_lossy(&buf).contains("{\"count\":2}")
                {
                    break;
                }
            }
            let req = String::from_utf8_lossy(&buf).into_owned();
            assert!(req.starts_with("POST /v1/pages HTTP/1.1"), "{req}");
            assert!(req.contains("\r\nHost: 127.0.0.1:"), "{req}");
            assert!(req.contains("{\"count\":2}"), "{req}");
            let body = b"{\"pages_remaining\":2}";
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            sock.write_all(resp.as_bytes()).unwrap();
            sock.write_all(body).unwrap();
        });
        let base = BaseUrl::parse(&format!("http://{addr}")).unwrap();
        let mut device = HttpDevice::new(base, Duration::from_secs(2));
        let raw = device
            .exchange("POST", "/v1/pages", Some(br#"{"count":2}"#))
            .unwrap();
        assert_eq!(raw.status, 200);
        assert_eq!(raw.body, b"{\"pages_remaining\":2}");
        server.join().unwrap();
    }
}
