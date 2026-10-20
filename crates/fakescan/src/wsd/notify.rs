//! ScanAvailableEvent delivery to a WS-Eventing NotifyTo sink.

use std::net::IpAddr;
use std::time::Duration;

use super::soap;
use super::xml::Element;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tracing::warn;

/// WSDL ScanAvailableEvent Action namespace (`2006/08`).
pub const SCAN_AVAILABLE_NS: &str = soap::SCAN_NS_AIRSCAN;

/// Default HTTP timeout for NotifyTo POSTs (lab sinks may be down).
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(3);

/// Build a SOAP 1.2 ScanAvailableEvent envelope (WSDL Action under `scan_ns`).
pub fn scan_available_event_xml(
    scan_ns: &str,
    notify_to: &str,
    scan_identifier: &str,
    client_context: &str,
) -> String {
    let body = Element::new("wscn:ScanAvailableEvent")
        .with(Element::text_el("wscn:ClientContext", client_context))
        .with(Element::text_el("wscn:ScanIdentifier", scan_identifier));
    soap::Response::new()
        .scan_ns(scan_ns)
        .action(&soap::scan_action(scan_ns, "ScanAvailableEvent"))
        .to(notify_to.trim())
        .body(body)
        .to_xml()
}

/// POST ScanAvailableEvent to `notify_to`. Best-effort: logs and returns Err on failure.
///
/// `subscriber` is the TCP peer that registered the sink. Delivery is refused unless the URL's
/// host is that address (a scanner must not become an open HTTP client).
pub async fn deliver_scan_available(
    notify_to: &str,
    subscriber: Option<IpAddr>,
    scan_ns: &str,
    scan_identifier: &str,
    client_context: &str,
) -> Result<(), String> {
    let target = accept_notify_target(notify_to, subscriber)?;
    let xml = scan_available_event_xml(scan_ns, &target.url(), scan_identifier, client_context);
    match post_soap_event(&target, "ScanAvailableEvent", &xml).await {
        Ok(()) => Ok(()),
        Err(e) => {
            warn!(
                %notify_to,
                %scan_identifier,
                error = %e,
                "ScanAvailableEvent NotifyTo delivery failed"
            );
            Err(e)
        }
    }
}

/// Generic NotifyTo SOAP POST (ScanAvailableEvent, JobStatusEvent, JobEndStateEvent, …).
///
/// The request line is built from [`NotifyTarget`] fields, never from the raw URL, so a
/// `NotifyTo` value cannot inject headers.
pub async fn post_soap_event(
    target: &NotifyTarget,
    _action: &str,
    body: &str,
) -> Result<(), String> {
    let path = &target.path;
    let host_header = target.host_header();
    let req = format!(
        "POST {path} HTTP/1.1\r\n\
         Host: {host_header}\r\n\
         Content-Type: application/soap+xml; charset=utf-8\r\n\
         Content-Length: {len}\r\n\
         Connection: close\r\n\
         \r\n\
         {body}",
        len = body.len(),
    );
    let connect = TcpStream::connect((target.host, target.port));
    let mut stream = timeout(NOTIFY_TIMEOUT, connect)
        .await
        .map_err(|_| format!("connect timeout to {host_header}"))?
        .map_err(|e| format!("connect {host_header}: {e}"))?;
    timeout(NOTIFY_TIMEOUT, stream.write_all(req.as_bytes()))
        .await
        .map_err(|_| "write timeout".to_string())?
        .map_err(|e| format!("write: {e}"))?;
    let mut buf = vec![0u8; 1024];
    let n = timeout(NOTIFY_TIMEOUT, stream.read(&mut buf))
        .await
        .map_err(|_| "read timeout".to_string())?
        .map_err(|e| format!("read: {e}"))?;
    if n == 0 {
        return Ok(());
    }
    let head = String::from_utf8_lossy(&buf[..n]);
    let status_line = head.lines().next().unwrap_or("");
    // Sinks may close without a status line; treat as delivered.
    if status_line.is_empty() {
        return Ok(());
    }
    match http_status_code(status_line) {
        Some(code) if (200..300).contains(&code) => Ok(()),
        Some(code) => Err(format!("NotifyTo HTTP {code}: {status_line}")),
        None => Ok(()),
    }
}

fn http_status_code(status_line: &str) -> Option<u16> {
    // HTTP/1.1 200 OK
    let mut parts = status_line.split_whitespace();
    let _proto = parts.next()?;
    parts.next()?.parse().ok()
}

/// A NotifyTo target that is safe to connect to: an IP literal, no userinfo, no controls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyTarget {
    pub host: IpAddr,
    pub port: u16,
    pub path: String,
}

impl NotifyTarget {
    /// URL rebuilt from the parsed parts (the form stored and placed in `wsa:To`).
    pub fn url(&self) -> String {
        match self.host {
            IpAddr::V4(v4) => format!("http://{v4}:{}{}", self.port, self.path),
            IpAddr::V6(v6) => format!("http://[{v6}]:{}{}", self.port, self.path),
        }
    }

    fn host_header(&self) -> String {
        match self.host {
            IpAddr::V4(v4) => format!("{v4}:{}", self.port),
            IpAddr::V6(v6) => format!("[{v6}]:{}", self.port),
        }
    }
}

/// Accept `url` as a NotifyTo sink for `subscriber`.
///
/// WS-Eventing asks the device to POST events to whatever Address the client supplies. On a
/// scanner bound to every interface that is an open proxy: a Subscribe could name another host,
/// a link-local metadata address, or a URL with CR/LF that rewrites the request. Delivery is
/// limited to the TCP peer that subscribed (IPv4-mapped forms compare equal). Subscribe itself
/// still succeeds when this returns `Err` — eventing is on the WIA "device online" path — but
/// nothing is stored and nothing is posted.
pub fn accept_notify_target(url: &str, subscriber: Option<IpAddr>) -> Result<NotifyTarget, String> {
    let target = parse_http_url(url)?;
    if is_metadata_host(target.host) || target.host.is_unspecified() || target.host.is_multicast() {
        return Err(format!("NotifyTo host {} is not allowed", target.host));
    }
    let Some(peer) = subscriber else {
        return Err("NotifyTo has no subscriber address to match".into());
    };
    if !same_host(target.host, peer) {
        return Err(format!(
            "NotifyTo host {} is not the subscriber {peer}",
            target.host
        ));
    }
    Ok(target)
}

fn same_host(a: IpAddr, b: IpAddr) -> bool {
    if a == b {
        return true;
    }
    match (a, b) {
        (IpAddr::V4(v4), IpAddr::V6(v6)) | (IpAddr::V6(v6), IpAddr::V4(v4)) => {
            v6.to_ipv4_mapped() == Some(v4)
        }
        _ => false,
    }
}

fn is_metadata_host(host: IpAddr) -> bool {
    // Cloud instance metadata (IPv4 and the IPv6 form). A lab scanner should not call these.
    match host {
        IpAddr::V4(v4) => v4.octets() == [169, 254, 169, 254],
        IpAddr::V6(v6) => v6.segments() == [0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254],
    }
}

fn parse_http_url(url: &str) -> Result<NotifyTarget, String> {
    let url = url.trim();
    if url.chars().any(|c| c.is_control() || c == ' ' || c == '\\') {
        return Err("NotifyTo contains a control character or space".into());
    }
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("NotifyTo must be http:// (got {url})"))?;
    if rest.contains('@') {
        return Err("NotifyTo must not carry userinfo".into());
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    if authority.is_empty() {
        return Err("NotifyTo host missing".into());
    }
    let (host, port) = split_authority(authority)?;
    if port == 0 {
        return Err("NotifyTo port 0 is not allowed".into());
    }
    let path = path.split('#').next().unwrap_or("/");
    if path.is_empty() || !path.starts_with('/') || !path_is_safe(path) {
        return Err("NotifyTo path is not allowed".into());
    }
    let host: IpAddr = host
        .parse()
        .map_err(|_| format!("NotifyTo host must be an IP address (got {host})"))?;
    Ok(NotifyTarget {
        host,
        port,
        path: path.to_string(),
    })
}

fn split_authority(authority: &str) -> Result<(&str, u16), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let end = rest
            .find(']')
            .ok_or_else(|| format!("bad NotifyTo IPv6 host in {authority}"))?;
        let host = &rest[..end];
        if host.is_empty() || host.contains('%') {
            return Err(format!("bad NotifyTo IPv6 host in {authority}"));
        }
        let after = &rest[end + 1..];
        let port = if after.is_empty() {
            80
        } else {
            let digits = after
                .strip_prefix(':')
                .ok_or_else(|| format!("bad NotifyTo port in {authority}"))?;
            digits
                .parse()
                .map_err(|_| format!("bad NotifyTo port in {authority}"))?
        };
        return Ok((host, port));
    }
    if authority.contains(':') {
        let (host, digits) = authority
            .rsplit_once(':')
            .ok_or_else(|| format!("bad NotifyTo port in {authority}"))?;
        if host.is_empty() {
            return Err("NotifyTo host missing".into());
        }
        let port = digits
            .parse()
            .map_err(|_| format!("bad NotifyTo port in {authority}"))?;
        return Ok((host, port));
    }
    Ok((authority, 80))
}

fn path_is_safe(path: &str) -> bool {
    path.chars().all(|c| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                '/' | '-' | '.' | '_' | '~' | '%' | '?' | '&' | '=' | '+' | ':' | '@'
            )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wsd::soap::SCAN_NS_AIRSCAN;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;

    #[test]
    fn builds_scan_available_event_shape() {
        let xml = scan_available_event_xml(
            SCAN_NS_AIRSCAN,
            "http://127.0.0.1:9/sink",
            "uuid:scan-1",
            "ctx-lab",
        );
        assert!(
            xml.contains(&format!(
                "<wsa:Action>{}/ScanAvailableEvent</wsa:Action>",
                SCAN_NS_AIRSCAN
            )),
            "{xml}"
        );
        assert!(
            xml.contains("<wsa:To>http://127.0.0.1:9/sink</wsa:To>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ClientContext>ctx-lab</wscn:ClientContext>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScanIdentifier>uuid:scan-1</wscn:ScanIdentifier>"),
            "{xml}"
        );
        assert!(xml.contains("<wscn:ScanAvailableEvent>"), "{xml}");
    }

    #[test]
    fn echoes_empty_client_context_element() {
        let xml = scan_available_event_xml(SCAN_NS_AIRSCAN, "http://x/s", "uuid:1", "");
        assert!(
            xml.contains("<wscn:ClientContext></wscn:ClientContext>")
                || xml.contains("<wscn:ClientContext/>"),
            "{xml}"
        );
        assert!(xml.contains("ScanIdentifier"), "{xml}");
    }

    #[test]
    fn parse_http_url_splits_host_port_path() {
        let v4 = parse_http_url("http://127.0.0.1:9999/evt").unwrap();
        assert_eq!(v4.host, "127.0.0.1".parse::<IpAddr>().unwrap());
        assert_eq!(v4.port, 9999);
        assert_eq!(v4.path, "/evt");
        let v6 = parse_http_url("http://[::1]:5357/sink").unwrap();
        assert_eq!(v6.host, "::1".parse::<IpAddr>().unwrap());
        assert_eq!(v6.port, 5357);
        assert_eq!(v6.path, "/sink");
        assert!(parse_http_url("http://example.com/a")
            .unwrap_err()
            .contains("IP address"));
        assert!(parse_http_url("https://x/y").is_err());
        assert!(parse_http_url("http://127.0.0.1:9/x\r\nHost: evil").is_err());
        assert!(parse_http_url("http://user:pass@127.0.0.1/x").is_err());
    }

    #[test]
    fn notify_target_must_be_the_subscriber() {
        let lo: IpAddr = "127.0.0.1".parse().unwrap();
        let lan: IpAddr = "192.168.1.20".parse().unwrap();
        let mapped: IpAddr = "::ffff:192.168.1.20".parse().unwrap();
        assert!(accept_notify_target("http://192.168.1.20:5357/sink", Some(lan)).is_ok());
        assert!(
            accept_notify_target("http://192.168.1.20:5357/sink", Some(mapped)).is_ok(),
            "IPv4-mapped subscriber matches the IPv4 NotifyTo"
        );
        let denied = accept_notify_target("http://127.0.0.1:9/sink", Some(lan)).unwrap_err();
        assert!(denied.contains("not the subscriber"), "{denied}");
        assert!(accept_notify_target("http://169.254.169.254/latest", Some(lo)).is_err());
        assert!(accept_notify_target("http://127.0.0.1:9/sink", None).is_err());
    }

    #[tokio::test]
    async fn deliver_posts_soap_to_local_sink() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().unwrap();
        let got = Arc::new(Mutex::new(None::<String>));
        let got2 = got.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.expect("accept");
            let mut buf = vec![0u8; 8192];
            let n = sock.read(&mut buf).await.expect("read");
            let req = String::from_utf8_lossy(&buf[..n]).to_string();
            *got2.lock().await = Some(req);
            let _ = sock
                .write_all(
                    b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
        });

        let url = format!("http://{addr}/sink");
        deliver_scan_available(
            &url,
            Some(addr.ip()),
            SCAN_NS_AIRSCAN,
            "uuid:push-1",
            "ctx-a",
        )
        .await
        .expect("deliver");

        let req = got.lock().await.clone().expect("request captured");
        assert!(req.starts_with("POST /sink HTTP/1.1"), "method/path: {req}");
        assert!(
            req.contains("Content-Type: application/soap+xml"),
            "content-type: {req}"
        );
        assert!(
            req.contains("<wscn:ScanIdentifier>uuid:push-1</wscn:ScanIdentifier>"),
            "{req}"
        );
        assert!(
            req.contains("<wscn:ClientContext>ctx-a</wscn:ClientContext>"),
            "{req}"
        );
        assert!(
            req.contains(&format!("{SCAN_NS_AIRSCAN}/ScanAvailableEvent")),
            "{req}"
        );
    }
}
