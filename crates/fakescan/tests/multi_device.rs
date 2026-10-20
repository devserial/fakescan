//! Multi-device server: several scanners in one process, each with its own World and endpoints.

use std::net::SocketAddr;
use std::path::PathBuf;

use fakescan::server::{serve, DeviceConfig, Ready, ServerConfig};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use uuid::Uuid;

struct Srv {
    addr: SocketAddr,
    ready: Ready,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Srv {
    async fn start(devices: Vec<DeviceConfig>) -> Srv {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut cfg = ServerConfig::lab(dir, "brother-wsd");
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.discovery = false;
        cfg.probe_listen = false;
        cfg.instance_lock = false;
        cfg.named_pipe = false;
        cfg.ctrl_c = false;
        cfg.devices = devices;
        let (rtx, rrx) = oneshot::channel();
        let (stx, srx) = oneshot::channel();
        let task = tokio::spawn(serve(cfg, Some(rtx), Some(srx)));
        let ready = rrx.await.expect("server ready");
        Srv {
            addr: ready.local_addr,
            ready,
            stop: Some(stx),
            task,
        }
    }

    async fn stop(mut self) {
        let _ = self.stop.take().unwrap().send(());
        self.task.await.unwrap().unwrap();
    }

    /// Minimal HTTP/1.1 client (Connection: close). Returns (status, body).
    async fn http(&self, method: &str, path: &str, ctype: &str, body: &str) -> (u16, Vec<u8>) {
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\n\r\n{body}",
            self.addr,
            body.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let split = raw
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .expect("header end");
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut body = raw[split + 4..].to_vec();
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body = dechunk(&body);
        }
        (status, body)
    }

    async fn json(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let b = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let (st, bytes) = self.http(method, path, "application/json", &b).await;
        (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn soap(&self, path: &str, action: &str, body: &str) -> (u16, String) {
        let xml = format!(
            r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/{action}</wsa:Action>
    <wsa:MessageID>uuid:11111111-1111-1111-1111-111111111111</wsa:MessageID>
  </soap:Header>
  <soap:Body>{body}</soap:Body>
</soap:Envelope>"#
        );
        let (st, b) = self.http("POST", path, "application/soap+xml", &xml).await;
        (st, String::from_utf8_lossy(&b).into_owned())
    }

    async fn elements(&self, scan_path: &str) -> String {
        let (st, body) = self
            .soap(
                scan_path,
                "GetScannerElements",
                "<wscn:GetScannerElementsRequest><wscn:RequestedElements>\
                 <wscn:Name>wscn:ScannerConfiguration</wscn:Name>\
                 </wscn:RequestedElements></wscn:GetScannerElementsRequest>",
            )
            .await;
        assert_eq!(st, 200, "{body}");
        body
    }
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

fn three() -> Vec<DeviceConfig> {
    vec![
        DeviceConfig::new(
            "default",
            "brother-wsd",
            Uuid::parse_str("00000000-0000-4000-8000-0000000000f2").unwrap(),
        ),
        DeviceConfig::derived("glass", "glass-wsd"),
        DeviceConfig::derived("dib", "accept-dib"),
    ]
}

#[tokio::test]
async fn devices_listing_has_distinct_uuids_and_endpoints() {
    let srv = Srv::start(three()).await;
    assert_eq!(srv.ready.devices.len(), 3);
    let (st, v) = srv.json("GET", "/v1/devices", Value::Null).await;
    assert_eq!(st, 200);
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 3);
    let uuids: std::collections::HashSet<_> =
        rows.iter().map(|r| r["uuid"].as_str().unwrap()).collect();
    let eps: std::collections::HashSet<_> = rows
        .iter()
        .map(|r| r["scan_endpoint"].as_str().unwrap())
        .collect();
    assert_eq!(uuids.len(), 3);
    assert_eq!(eps.len(), 3);
    let port = srv.addr.port();
    assert_eq!(
        rows[0]["scan_endpoint"],
        format!("http://127.0.0.1:{port}/ScannerService")
    );
    assert_eq!(
        rows[1]["scan_endpoint"],
        format!("http://127.0.0.1:{port}/d/glass/ScannerService")
    );
    assert_eq!(
        rows[1]["device_endpoint"],
        format!("http://127.0.0.1:{port}/d/glass/WebServices/Device")
    );
    assert_eq!(rows[2]["profile"], "accept-dib");
    srv.stop().await;
}

#[tokio::test]
async fn state_is_per_device_and_legacy_aliases_default() {
    let srv = Srv::start(three()).await;
    let (st, _) = srv
        .json("POST", "/d/dib/v1/pages", json!({"count": 3}))
        .await;
    assert_eq!(st, 200);
    let pages = |v: &Value| v["pages_remaining"].as_u64().unwrap();
    let (_, dib) = srv.json("GET", "/d/dib/v1/status", Value::Null).await;
    let (_, glass) = srv.json("GET", "/d/glass/v1/status", Value::Null).await;
    let (_, def) = srv.json("GET", "/d/default/v1/status", Value::Null).await;
    let (_, legacy) = srv.json("GET", "/v1/status", Value::Null).await;
    assert_eq!(pages(&dib), 3);
    assert_eq!(pages(&glass), 0);
    assert_eq!(pages(&def), 0);
    assert_eq!(legacy, def);
    assert_eq!(legacy["profile_id"], "brother-wsd");
    assert_eq!(glass["profile_id"], "glass-wsd");

    // Writing through the legacy route hits the default device only.
    srv.json("POST", "/v1/pages", json!({"count": 2})).await;
    let (_, def) = srv.json("GET", "/d/default/v1/status", Value::Null).await;
    let (_, dib) = srv.json("GET", "/d/dib/v1/status", Value::Null).await;
    assert_eq!(pages(&def), 2);
    assert_eq!(pages(&dib), 3);
    srv.stop().await;
}

#[tokio::test]
async fn scanner_elements_reflect_each_devices_profile() {
    let srv = Srv::start(three()).await;
    let brother = srv.elements("/d/default/ScannerService").await;
    let legacy = srv.elements("/ScannerService").await;
    let glass = srv.elements("/d/glass/ScannerService").await;
    let glass_slash = srv.elements("/d/glass/WebServices/ScannerService/").await;
    assert!(brother.contains("<wscn:ADF>"), "brother has an ADF");
    assert!(legacy.contains("<wscn:ADF>"));
    assert!(brother.contains("<wscn:Platen>"));
    assert!(glass.contains("<wscn:Platen>"));
    assert!(
        !glass.contains("<wscn:ADF>"),
        "glass-wsd has no ADF: {glass}"
    );
    assert!(!glass_slash.contains("<wscn:ADF>"));
    srv.stop().await;
}

#[tokio::test]
async fn fault_plan_is_per_device_over_http_and_ctl() {
    let srv = Srv::start(three()).await;
    // POST on glass only.
    let (st, v) = srv
        .json(
            "POST",
            "/d/glass/v1/faults/plan",
            json!({"faults":[{"kind":"cover_open","when":{"page":2,"fraction":0.5}}]}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["fault_plan"][0]["kind"], "cover_open");
    assert_eq!(v["fault_plan"][0]["fired"], false);
    let (_, dib) = srv.json("GET", "/d/dib/v1/faults/plan", Value::Null).await;
    assert_eq!(dib["fault_plan"].as_array().unwrap().len(), 0);
    let (_, def) = srv.json("GET", "/v1/status", Value::Null).await;
    assert_eq!(def["fault_plan"].as_array().unwrap().len(), 0);
    // Bad plan is a 400.
    let (st, _) = srv
        .json(
            "POST",
            "/d/glass/v1/faults/plan",
            json!({"kind":"jam","when":{"page":1}}),
        )
        .await;
    assert_eq!(st, 400);
    // ctl on dib: now-fault sets the condition immediately and shows up in status.
    let (st, v) = srv
        .json(
            "POST",
            "/d/dib/v1/ctl",
            json!({"cmd":"fault_plan","args":{"kind":"cover_open","when":"now"}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["cover_open"], true);
    assert_eq!(v["fault_plan"][0]["fired"], true);
    let (_, glass) = srv.json("GET", "/d/glass/v1/status", Value::Null).await;
    assert_eq!(glass["cover_open"], false);
    // DELETE clears the plan only; the sticky condition stays.
    let (st, v) = srv
        .json("DELETE", "/d/dib/v1/faults/plan", Value::Null)
        .await;
    assert_eq!(st, 200);
    assert_eq!(v["fault_plan"].as_array().unwrap().len(), 0);
    assert_eq!(v["cover_open"], true);
    srv.json("POST", "/d/dib/v1/reset", Value::Null).await;
    let (_, dib) = srv.json("GET", "/d/dib/v1/status", Value::Null).await;
    assert_eq!(dib["cover_open"], false);
    srv.stop().await;
}

#[tokio::test]
async fn unknown_device_is_404_json() {
    let srv = Srv::start(three()).await;
    for path in [
        "/d/nope/v1/status",
        "/d/nope/ScannerService",
        "/d/nope",
        "/d/nope/",
    ] {
        let (st, v) = srv.json("GET", path, Value::Null).await;
        assert_eq!(st, 404, "{path}");
        assert_eq!(v["error"], "unknown device", "{path}");
    }
    srv.stop().await;
}

async fn platen_scan(srv: &Srv, scan_path: &str) {
    let (st, body) = srv
        .soap(
            scan_path,
            "CreateScanJob",
            "<wscn:CreateScanJobRequest><wscn:ScanTicket><wscn:DocumentParameters>\
             <wscn:Format>jfif</wscn:Format><wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>\
             <wscn:InputSource>Platen</wscn:InputSource><wscn:MediaSides><wscn:MediaFront>\
             <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height>\
             </wscn:Resolution></wscn:MediaFront></wscn:MediaSides></wscn:DocumentParameters>\
             </wscn:ScanTicket></wscn:CreateScanJobRequest>",
        )
        .await;
    assert_eq!(st, 200, "{body}");
    let pick = |tag: &str| {
        body.split(&format!("<wscn:{tag}>"))
            .nth(1)
            .unwrap()
            .split('<')
            .next()
            .unwrap()
            .to_string()
    };
    let (job, token) = (pick("JobId"), pick("JobToken"));
    let (st, body) = srv
        .soap(
            scan_path,
            "RetrieveImage",
            &format!(
                "<wscn:RetrieveImageRequest><wscn:JobId>{job}</wscn:JobId>\
                 <wscn:JobToken>{token}</wscn:JobToken></wscn:RetrieveImageRequest>"
            ),
        )
        .await;
    assert_eq!(st, 200, "{body}");
}

#[tokio::test]
async fn scan_on_one_device_leaves_other_jobs_empty() {
    let srv = Srv::start(three()).await;
    platen_scan(&srv, "/d/dib/ScannerService").await;
    let (_, a) = srv.json("GET", "/d/dib/v1/jobs", Value::Null).await;
    let (_, b) = srv.json("GET", "/d/glass/v1/jobs", Value::Null).await;
    let (_, c) = srv.json("GET", "/v1/jobs", Value::Null).await;
    assert_eq!(a["jobs"].as_array().unwrap().len(), 1, "{a}");
    assert!(b["jobs"].as_array().unwrap().is_empty(), "{b}");
    assert!(c["jobs"].as_array().unwrap().is_empty(), "{c}");
    srv.stop().await;
}

#[tokio::test]
async fn tray_capacity_rejects_overload_with_409() {
    let srv = Srv::start(vec![
        DeviceConfig::new(
            "default",
            "brother-wsd",
            Uuid::parse_str("00000000-0000-4000-8000-0000000000f2").unwrap(),
        ),
        DeviceConfig::derived("tray5", "feeder-tray5"),
    ])
    .await;
    let (st, _) = srv
        .json("POST", "/d/tray5/v1/pages", json!({"count": 6}))
        .await;
    assert_eq!(st, 409);
    let (_, s) = srv.json("GET", "/d/tray5/v1/status", Value::Null).await;
    assert_eq!(s["pages_remaining"], 0);
    assert_eq!(s["feeder_capacity"], 5);
    let (st, _) = srv
        .json("POST", "/d/tray5/v1/pages", json!({"count": 5}))
        .await;
    assert_eq!(st, 200);
    srv.stop().await;
}

#[tokio::test]
async fn serve_refuses_a_foreign_uuid_unless_allowed() {
    let foreign = Uuid::parse_str("3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b").unwrap();
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let mk = |allow: bool| {
        let mut cfg = ServerConfig::lab(dir.clone(), "brother-wsd");
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.discovery = false;
        cfg.probe_listen = false;
        cfg.instance_lock = false;
        cfg.named_pipe = false;
        cfg.ctrl_c = false;
        let mut d = DeviceConfig::new("default", "brother-wsd", foreign);
        d.allow_foreign_uuid = allow;
        cfg.devices = vec![d];
        cfg
    };
    let e = serve(mk(false), None, None).await.unwrap_err();
    assert!(e.to_string().contains("fake namespace"), "{e:#}");
    let (rtx, rrx) = oneshot::channel();
    let (stx, srx) = oneshot::channel();
    let task = tokio::spawn(serve(mk(true), Some(rtx), Some(srx)));
    rrx.await.expect("allowed foreign UUID starts");
    let _ = stx.send(());
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn devices_can_be_added_and_removed_at_runtime() {
    let srv = Srv::start(three()).await;
    let (st, _) = srv.json("GET", "/d/late/v1/status", Value::Null).await;
    assert_eq!(st, 404);

    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late", "profile": "feeder-tray5"}),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["id"], "late");
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late", "profile": "feeder-tray5"}),
        )
        .await;
    assert_eq!(st, 409);
    let (st, _) = srv
        .json("POST", "/v1/devices", json!({"id": "x", "profile": "nope"}))
        .await;
    assert_eq!(st, 400);
    // identity is the UUID prefix: a foreign UUID is a 400 (no override over HTTP)
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "foreign", "profile": "feeder-tray5",
                   "device_uuid": "3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b"}),
        )
        .await;
    assert_eq!(st, 400, "{v}");
    assert!(
        v["error"].as_str().unwrap().contains("fake namespace"),
        "{v}"
    );

    let (st, s) = srv.json("GET", "/d/late/v1/status", Value::Null).await;
    assert_eq!(st, 200);
    assert_eq!(s["profile_id"], "feeder-tray5");
    let body = srv.elements("/d/late/ScannerService").await;
    assert!(body.contains("<wscn:Platen>"));
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    assert_eq!(list.as_array().unwrap().len(), 4);

    let (st, _) = srv.json("DELETE", "/v1/devices/late", Value::Null).await;
    assert_eq!(st, 200);
    let (st, v) = srv.json("GET", "/d/late/v1/status", Value::Null).await;
    assert_eq!(st, 404);
    assert_eq!(v["error"], "unknown device");
    let (st, _) = srv.json("DELETE", "/v1/devices/late", Value::Null).await;
    assert_eq!(st, 404);
    // The others are untouched.
    let (st, _) = srv.json("GET", "/d/glass/v1/status", Value::Null).await;
    assert_eq!(st, 200);
    srv.stop().await;
}

#[tokio::test]
async fn ctl_exit_on_any_device_stops_the_server() {
    let srv = Srv::start(three()).await;
    let (st, v) = srv
        .json(
            "POST",
            "/d/glass/v1/ctl",
            json!({"cmd": "shutdown", "args": {"yes": true}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    tokio::time::timeout(std::time::Duration::from_secs(5), srv.task)
        .await
        .expect("server exits")
        .unwrap()
        .unwrap();
}

/// Regression: `main.rs` hands `serve()` a stop channel whose sender only the tray holds. With no tray
/// (`--no-tray`, `--no-wsd`, non-Windows) that sender is dropped at once, and a dropped sender must
/// not read as "stop": the daemon exited right after start-up.
#[tokio::test]
async fn a_dropped_stop_sender_does_not_stop_the_server() {
    let mut srv = Srv::start(three()).await;
    drop(srv.stop.take());
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    assert!(
        !srv.task.is_finished(),
        "the server exited after its stop sender was dropped"
    );
    let (status, _) = srv.http("GET", "/health", "text/plain", "").await;
    assert_eq!(status, 200);
    srv.task.abort();
}
