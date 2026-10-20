//! Device manager UI: `/ui`, `/v1/profiles`, ctl `open_settings`, and embedded-asset hygiene.
//! Everything runs in process on port 0 with no registry; the settings launcher is a recorder.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use fakescan::manager::{DeviceManager, ManagerConfig, SettingsLauncher};
use fakescan::server::{manager_router, serve, DeviceConfig, ServerConfig};
use fakescan::ui::{check_open_settings, is_loopback_peer, INDEX_HTML};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

/// Minimal HTTP/1.1 client. Returns (status, lowercased head, body).
async fn http(addr: SocketAddr, method: &str, path: &str, body: &str) -> (u16, String, Vec<u8>) {
    let mut s = TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("header end");
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut body = raw[split + 4..].to_vec();
    if head.contains("transfer-encoding: chunked") {
        body = dechunk(&body);
    }
    (status, head, body)
}

fn dechunk(mut b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(i) = b.windows(2).position(|w| w == b"\r\n") {
        let n = usize::from_str_radix(String::from_utf8_lossy(&b[..i]).trim(), 16).unwrap_or(0);
        if n == 0 {
            break;
        }
        out.extend_from_slice(&b[i + 2..i + 2 + n]);
        b = &b[i + 2 + n + 2..];
    }
    out
}

async fn json(addr: SocketAddr, method: &str, path: &str, body: Value) -> (u16, Value) {
    let b = if body.is_null() {
        String::new()
    } else {
        body.to_string()
    };
    let (st, _, bytes) = http(addr, method, path, &b).await;
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// A real `serve()` on port 0 (no registry, no discovery).
async fn with_server<F, Fut>(f: F)
where
    F: FnOnce(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mut cfg = ServerConfig::lab(profiles_dir(), "brother-wsd");
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.discovery = false;
    cfg.probe_listen = false;
    cfg.instance_lock = false;
    cfg.named_pipe = false;
    cfg.ctrl_c = false;
    let (rtx, rrx) = oneshot::channel();
    let (stx, srx) = oneshot::channel();
    let task = tokio::spawn(serve(cfg, Some(rtx), Some(srx)));
    let ready = rrx.await.expect("server ready");
    f(ready.local_addr).await;
    let _ = stx.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ui_page_is_served_with_landmarks() {
    with_server(|addr| async move {
        for path in ["/ui", "/ui/"] {
            let (st, head, body) = http(addr, "GET", path, "").await;
            assert_eq!(st, 200, "{path}");
            assert!(
                head.contains("content-type: text/html; charset=utf-8"),
                "{head}"
            );
            assert!(head.contains("cache-control: no-store"), "{head}");
            let text = String::from_utf8(body).unwrap();
            assert!(text.contains("<title>fakescan device manager</title>"));
            assert!(text.contains("Add device"));
            assert_eq!(text, INDEX_HTML, "served bytes are the embedded asset");
        }
        // The device dispatcher still works next to it.
        let (st, v) = json(addr, "GET", "/v1/devices", Value::Null).await;
        assert_eq!(st, 200);
        assert_eq!(v[0]["id"], "default");
    })
    .await;
}

#[test]
fn loopback_check() {
    for ok in [
        "127.0.0.1",
        "127.0.0.2",
        "127.255.255.254",
        "::1",
        "::ffff:127.0.0.1",
    ] {
        assert!(
            is_loopback_peer(ok.parse().unwrap()),
            "{ok} must be allowed"
        );
    }
    for bad in [
        "192.168.1.5",
        "10.0.0.1",
        "0.0.0.0",
        "::ffff:192.168.1.5",
        "fe80::1",
        "2001:db8::1",
    ] {
        assert!(
            !is_loopback_peer(bad.parse().unwrap()),
            "{bad} must be denied"
        );
    }
}

#[tokio::test]
async fn profiles_lists_shipped_scenarios_sorted() {
    with_server(|addr| async move {
        let (st, v) = json(addr, "GET", "/v1/profiles", Value::Null).await;
        assert_eq!(st, 200);
        let list = v.as_array().expect("array");
        let ids: Vec<&str> = list.iter().map(|p| p["id"].as_str().unwrap()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted, "sorted by id");
        let by = |id: &str| {
            list.iter()
                .find(|p| p["id"] == id)
                .unwrap_or_else(|| panic!("{id} missing: {ids:?}"))
        };
        let t5 = by("feeder-tray5");
        assert_eq!(t5["feeder"]["capacity"], 5);
        assert_eq!(t5["feeder"]["duplex"], false);
        assert_eq!(t5["feeder"]["media_sizes"], json!(["letter"]));
        assert_eq!(t5["connection"], "wsd");
        assert!(t5["formats"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f == "jfif"));
        let bw = by("brother-wsd");
        assert!(bw["feeder"]["capacity"].is_null());
        assert!(bw["description"].is_string());
    })
    .await;
}

/// A manager over a temp profiles dir (one good copy, one invalid) behind the real router.
struct Harness {
    addr: SocketAddr,
    manager: Arc<DeviceManager>,
    dir: PathBuf,
    task: tokio::task::JoinHandle<()>,
}

#[derive(Default)]
struct Recorder(Mutex<Vec<String>>);
impl SettingsLauncher for Recorder {
    fn open(&self, page: &str) -> anyhow::Result<bool> {
        self.0.lock().unwrap().push(page.to_string());
        Ok(true)
    }
}

async fn harness(tag: &str) -> Harness {
    let dir = std::env::temp_dir().join(format!("fakescan-ui-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for id in ["brother-wsd", "feeder-tray5"] {
        std::fs::copy(
            profiles_dir().join(format!("{id}.json")),
            dir.join(format!("{id}.json")),
        )
        .unwrap();
    }
    std::fs::write(dir.join("broken.json"), "{ not json").unwrap();
    // Parses, but the declared id does not match the file name.
    let mut v: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("brother-wsd.json")).unwrap())
            .unwrap();
    v["id"] = "someone-else".into();
    std::fs::write(dir.join("mismatch.json"), v.to_string()).unwrap();
    // Valid JSON, invalid feeder (capacity below pages_max).
    let mut v: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("brother-wsd.json")).unwrap())
            .unwrap();
    v["id"] = "badcap".into();
    v["feeder"]["capacity"] = 1.into();
    v["feeder"]["pages_max"] = 9.into();
    std::fs::write(dir.join("badcap.json"), v.to_string()).unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (manager, _exit) = DeviceManager::new(ManagerConfig {
        profiles: dir.clone(),
        wsd: true,
        discovery: false,
        probe_listen: false,
        advertise: addr.ip(),
        port: addr.port(),
        lab_name: None,
        local_addr: addr,
        listen_key: addr.to_string(),
        registry: None,
        initial_pages: None,
    });
    manager
        .add(DeviceConfig::derived("default", "brother-wsd"))
        .await
        .unwrap();
    let app = manager_router(Arc::clone(&manager));
    let task = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await;
    });
    Harness {
        addr,
        manager,
        dir,
        task,
    }
}

impl Harness {
    fn finish(self) {
        self.task.abort();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn profiles_skips_invalid_files() {
    let h = harness("profiles").await;
    let (st, v) = json(h.addr, "GET", "/v1/profiles", Value::Null).await;
    assert_eq!(st, 200, "{v}");
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["brother-wsd", "feeder-tray5"],
        "invalid files skipped, not a 500"
    );
    h.finish();
}

#[tokio::test]
async fn open_settings_records_one_launch_and_rejects_other_pages() {
    let h = harness("settings").await;
    let rec = Arc::new(Recorder::default());
    h.manager.set_settings_launcher(rec.clone());

    let (st, v) = json(
        h.addr,
        "POST",
        "/v1/ctl",
        json!({"cmd": "open_settings", "page": "printers"}),
    )
    .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v, json!({"opened": true}));
    assert_eq!(*rec.0.lock().unwrap(), ["printers"], "exactly one launch");

    // `args.page` form is accepted too (still one more call, same page).
    let (st, _) = json(
        h.addr,
        "POST",
        "/v1/ctl",
        json!({"cmd": "open_settings", "args": {"page": "printers"}}),
    )
    .await;
    assert_eq!(st, 200);
    assert_eq!(rec.0.lock().unwrap().len(), 2);

    for bad in [
        json!({"cmd": "open_settings", "page": "network"}),
        json!({"cmd": "open_settings"}),
        json!({"cmd": "open_settings", "page": "printers & calc"}),
    ] {
        let (st, _) = json(h.addr, "POST", "/v1/ctl", bad).await;
        assert_eq!(st, 400);
    }
    assert_eq!(
        rec.0.lock().unwrap().len(),
        2,
        "rejected requests never reach the launcher"
    );
    h.finish();
}

#[test]
fn open_settings_check_is_loopback_and_printers_only() {
    let lo: Option<IpAddr> = Some("127.0.0.1".parse().unwrap());
    assert!(check_open_settings(lo, "printers").is_ok());
    assert!(check_open_settings(lo, "network").is_err());
    assert!(check_open_settings(lo, "").is_err());
    assert!(check_open_settings(Some("192.168.1.5".parse().unwrap()), "printers").is_err());
    assert!(check_open_settings(Some("::ffff:192.168.1.5".parse().unwrap()), "printers").is_err());
    assert!(
        check_open_settings(None, "printers").is_err(),
        "unknown peer is not loopback"
    );
}

/// Asset hygiene (the page is an operator console served to a browser, so it must stay inert):
///  - no external scripts (`<script src`) and no absolute http(s) `src=`/`href=`;
///  - no network URL other than relative ones (`fetch("/...")`);
///  - no `innerHTML`/`outerHTML`/`insertAdjacentHTML`/`document.write` at all, so server-provided
///    strings can only reach the DOM as text (textContent / createTextNode / fixed attributes).
#[test]
fn embedded_asset_hygiene() {
    let t = INDEX_HTML;
    assert!(!t.contains("<script src"), "external script");
    for needle in [
        "src=\"http",
        "href=\"http",
        "src='http",
        "href='http",
        "src=\"//",
        "href=\"//",
        "@import",
        "url(http",
    ] {
        assert!(!t.contains(needle), "external reference: {needle}");
    }
    for needle in [
        "innerHTML",
        "outerHTML",
        "insertAdjacentHTML",
        "document.write",
        "eval(",
        "new Function",
    ] {
        assert!(!t.contains(needle), "unsafe sink: {needle}");
    }
    for cmd in [
        "fetch(\"http",
        "fetch('http",
        "XMLHttpRequest",
        "WebSocket(",
        "EventSource(",
    ] {
        assert!(!t.contains(cmd), "non-relative network use: {cmd}");
    }
}

#[test]
fn embedded_script_parses_with_node() {
    let Ok(node) = std::process::Command::new("node").arg("--version").output() else {
        eprintln!("node not on PATH; skipping script syntax check");
        return;
    };
    if !node.status.success() {
        return;
    }
    let start = INDEX_HTML.find("<script>").expect("script open") + "<script>".len();
    let end = INDEX_HTML.rfind("</script>").expect("script close");
    let path = std::env::temp_dir().join(format!("fakescan-ui-script-{}.js", std::process::id()));
    std::fs::write(&path, &INDEX_HTML[start..end]).unwrap();
    let out = std::process::Command::new("node")
        .arg("--check")
        .arg(&path)
        .output()
        .unwrap();
    let _ = std::fs::remove_file(&path);
    assert!(
        out.status.success(),
        "node --check failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
