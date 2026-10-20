//! Device registry + install tracking (docs/multi-device.md sections 8-10).
//!
//! SAFETY: every test uses an explicit temp registry path (and `FAKESCAN_REGISTRY` is pointed at
//! a temp file for the whole process). Nothing here may read or write the real devices.json.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;

use axum::extract::ConnectInfo;
use fakescan::manager::{DeviceManager, ManagerConfig};
use fakescan::registry::{InstallState, Registry, RegistryStore, WiaInfo};
use fakescan::server::{manager_router, serve, DeviceConfig, Ready, ServerConfig};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::oneshot;
use tower::ServiceExt;
use uuid::Uuid;

static N: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(tag: &str) -> PathBuf {
    static ONCE: Once = Once::new();
    let d = std::env::temp_dir().join(format!(
        "fakescan-it-{}-{}-{}",
        std::process::id(),
        tag,
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&d).unwrap();
    ONCE.call_once(|| {
        // Belt and braces: anything that falls back to registry_path() hits a temp file.
        std::env::set_var("FAKESCAN_REGISTRY", d.join("env-guard.json"));
    });
    d
}

fn reg_path(tag: &str) -> PathBuf {
    tmp_dir(tag).join("devices.json")
}

fn profiles_src() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

/// A copy of the shipped profiles plus `tray5-cap9`: tray5 with a bigger tray (unpublished change).
fn profiles_copy(tag: &str) -> PathBuf {
    let dst = tmp_dir(tag).join("profiles");
    std::fs::create_dir_all(&dst).unwrap();
    for e in std::fs::read_dir(profiles_src()).unwrap() {
        let p = e.unwrap().path();
        if p.extension().and_then(|x| x.to_str()) == Some("json") {
            std::fs::copy(&p, dst.join(p.file_name().unwrap())).unwrap();
        }
    }
    let mut v: Value =
        serde_json::from_str(&std::fs::read_to_string(dst.join("feeder-tray5.json")).unwrap())
            .unwrap();
    v["id"] = "tray5-cap9".into();
    v["feeder"]["capacity"] = 9.into();
    std::fs::write(dst.join("tray5-cap9.json"), v.to_string()).unwrap();
    dst
}

fn manager_cfg(profiles: &Path, store: Option<Arc<RegistryStore>>) -> ManagerConfig {
    ManagerConfig {
        profiles: profiles.to_path_buf(),
        wsd: true,
        discovery: false,
        probe_listen: false,
        advertise: "127.0.0.1".parse().unwrap(),
        port: 3920,
        lab_name: None,
        local_addr: "127.0.0.1:3920".parse().unwrap(),
        listen_key: "127.0.0.1:3920".into(),
        registry: store,
        initial_pages: None,
    }
}

fn wia(uuid: &Uuid, id: &str) -> WiaInfo {
    WiaInfo {
        id: id.into(),
        name: "fakescan".into(),
        port: format!("urn:uuid:{uuid}/uri:{uuid}/ScannerService"),
    }
}

async fn row_state(m: &DeviceManager, id: &str) -> InstallState {
    m.registry_doc().unwrap().find(id).unwrap().install.state
}

// ── manager-level state machine ────────────────────────────────────────────────

#[tokio::test]
async fn new_to_probed_on_soap_get_with_fake_peer_through_dispatcher() {
    let store = RegistryStore::file(reg_path("probe"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store.clone())));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.add(DeviceConfig::derived("tray5", "feeder-tray5"))
        .await
        .unwrap();
    assert_eq!(row_state(&m, "tray5").await, InstallState::New);

    let app = manager_router(Arc::clone(&m));
    let soap = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wscn="http://schemas.microsoft.com/windows/2006/01/wdp/scan">
  <soap:Header>
    <wsa:Action>http://schemas.microsoft.com/windows/2006/01/wdp/scan/GetScannerElements</wsa:Action>
    <wsa:MessageID>uuid:11111111-1111-1111-1111-111111111111</wsa:MessageID>
  </soap:Header>
  <soap:Body><wscn:GetScannerElementsRequest><wscn:RequestedElements>
    <wscn:Name>wscn:ScannerConfiguration</wscn:Name></wscn:RequestedElements>
  </wscn:GetScannerElementsRequest></soap:Body>
</soap:Envelope>"#;
    let mut req = axum::http::Request::post("/d/tray5/ScannerService")
        .header("content-type", "application/soap+xml")
        .body(axum::body::Body::from(soap))
        .unwrap();
    let peer: SocketAddr = "10.1.2.3:5555".parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(peer));
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), 200);

    // File IO happens off the request path: poll.
    let mut got = None;
    for _ in 0..100 {
        let reg = m.registry_doc().unwrap();
        let r = reg.find("tray5").unwrap();
        if r.install.state == InstallState::Probed {
            got = Some(r.install.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let i = got.expect("tray5 became probed");
    assert_eq!(i.last_peer.as_deref(), Some("10.1.2.3"));
    assert!(i.first_probed_at.is_some() && i.last_seen_at.is_some());
    // The other device saw nothing.
    assert_eq!(row_state(&m, "default").await, InstallState::New);

    // probed -> installed only via mark_installed (UUID must be in the port string).
    let uuid = m
        .list()
        .await
        .iter()
        .find(|d| d.id == "tray5")
        .unwrap()
        .uuid;
    let bad = WiaInfo {
        id: "{X}".into(),
        name: "fakescan tray5".into(),
        port: "urn:uuid:nope".into(),
    };
    assert!(
        m.mark_installed("tray5", bad).await.is_err(),
        "name/other port must not install"
    );
    assert_eq!(row_state(&m, "tray5").await, InstallState::Probed);
    m.mark_installed("tray5", wia(&uuid, "{6BDD1FC6}\\0003"))
        .await
        .unwrap();
    let reg = m.registry_doc().unwrap();
    let r = reg.find("tray5").unwrap();
    assert_eq!(r.install.state, InstallState::Installed);
    assert!(r.added.pnp, "added.pnp kept in sync");
    assert_eq!(r.install.wia.as_ref().unwrap().id, "{6BDD1FC6}\\0003");
    assert!(r
        .install
        .published_fingerprint_at_install
        .as_deref()
        .unwrap()
        .starts_with("sha256:"));
    assert!(r.install.installed_at.is_some() && r.install.verified_at.is_some());
}

#[tokio::test]
async fn probe_writes_are_throttled_and_first_contact_always_writes() {
    let store = RegistryStore::file(reg_path("throttle"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store.clone())));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.set_probe_interval(Duration::from_secs(3600));
    let before = store.writes();
    assert!(
        m.mark_probed("default", "10.0.0.1").await.unwrap(),
        "first contact writes"
    );
    assert_eq!(row_state(&m, "default").await, InstallState::Probed);
    for _ in 0..5 {
        assert!(!m.mark_probed("default", "10.0.0.2").await.unwrap());
    }
    assert_eq!(store.writes(), before + 1);
    assert_eq!(
        m.registry_doc()
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .last_peer
            .as_deref(),
        Some("10.0.0.1")
    );
    m.set_probe_interval(Duration::ZERO);
    assert!(m.mark_probed("default", "10.0.0.3").await.unwrap());
    assert_eq!(
        m.registry_doc()
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .last_peer
            .as_deref(),
        Some("10.0.0.3")
    );
}

#[tokio::test]
async fn reconfigure_goes_stale_only_when_published_capabilities_change() {
    let profiles = profiles_copy("reconf");
    let store = RegistryStore::file(reg_path("reconf"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles, Some(store)));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.add(DeviceConfig::derived("tray", "feeder-tray5"))
        .await
        .unwrap();
    let uuid = |id: &'static str| {
        let m = &m;
        async move { m.list().await.iter().find(|d| d.id == id).unwrap().uuid }
    };
    let (u_tray, u_def) = (uuid("tray").await, uuid("default").await);
    m.mark_installed("tray", wia(&u_tray, "{A}")).await.unwrap();
    m.mark_installed("default", wia(&u_def, "{B}"))
        .await
        .unwrap();

    // Capacity only (unpublished): stays installed, fingerprint (full) changes, published not.
    let before = m
        .registry_doc()
        .unwrap()
        .find("tray")
        .unwrap()
        .config
        .clone()
        .unwrap();
    let info = m.reconfigure("tray", "tray5-cap9").await.unwrap();
    assert!(!info.needs_readd, "{info:?}");
    assert_eq!(row_state(&m, "tray").await, InstallState::Installed);
    let after = m
        .registry_doc()
        .unwrap()
        .find("tray")
        .unwrap()
        .config
        .clone()
        .unwrap();
    assert_ne!(before.fingerprint, after.fingerprint);
    assert_eq!(before.published_fingerprint, after.published_fingerprint);
    assert_eq!(after.feeder.capacity, Some(9));
    assert_eq!(
        m.registry_doc().unwrap().find("tray").unwrap().profile,
        "tray5-cap9"
    );

    // tray5 -> tray50 changes ADFMaximumSize (letter -> letter/a4/legal => 8500x14000): published.
    let info = m.reconfigure("tray", "feeder-tray50").await.unwrap();
    assert!(info.needs_readd);
    assert_eq!(
        info.needs_readd_reason.as_deref(),
        Some("profile changed; remove and re-Add in Imaging")
    );
    assert_eq!(row_state(&m, "tray").await, InstallState::Stale);
    assert!(!m.registry_doc().unwrap().find("tray").unwrap().added.pnp);

    // brother-wsd -> glass-wsd (feeder -> glass only): stale.
    let info = m.reconfigure("default", "glass-wsd").await.unwrap();
    assert!(info.needs_readd);
    assert_eq!(row_state(&m, "default").await, InstallState::Stale);

    // The live device really serves the new profile.
    let h = m.get("default").await.unwrap();
    assert_eq!(h.world.read().await.profile().id, "glass-wsd");

    // A device that was never installed is not marked stale by a reconfigure.
    m.add(DeviceConfig::derived("fresh", "feeder-tray5"))
        .await
        .unwrap();
    m.reconfigure("fresh", "glass-wsd").await.unwrap();
    assert_eq!(row_state(&m, "fresh").await, InstallState::New);
    assert!(m.reconfigure("nope", "glass-wsd").await.is_err());
    assert!(m.reconfigure("fresh", "no-such-profile").await.is_err());
}

#[tokio::test]
async fn reconcile_matches_by_uuid_only_and_never_touches_foreign() {
    let store = RegistryStore::file(reg_path("recon"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store.clone())));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.add(DeviceConfig::derived("a", "feeder-tray5"))
        .await
        .unwrap();
    m.add(DeviceConfig::derived("b", "glass-wsd"))
        .await
        .unwrap();
    let list = m.list().await;
    let ua = list.iter().find(|d| d.id == "a").unwrap().uuid;
    let ub = list.iter().find(|d| d.id == "b").unwrap().uuid;
    m.mark_installed("b", wia(&ub, "{OLD-B}")).await.unwrap();

    let foreign_by_name = WiaInfo {
        id: "{N}".into(),
        name: "fakescan b".into(), // a name guess must not install b
        port: "usb:vid_04f9".into(),
    };
    let unknown_uuid = WiaInfo {
        id: "{U}".into(),
        name: "fakescan lookalike".into(),
        port: "urn:uuid:11111111-2222-4333-8444-555555555555/uri:x".into(),
    };
    // a: braces + upper case + no urn prefix still matches.
    let a_entry = WiaInfo {
        id: "{A-ID}\\0001".into(),
        name: "whatever the user renamed it".into(),
        port: format!("{{{}}}", ua.to_string().to_uppercase()),
    };
    let before = std::fs::read_to_string(store.path().unwrap()).unwrap();
    let r = m
        .reconcile_with_windows(&[
            a_entry.clone(),
            foreign_by_name.clone(),
            unknown_uuid.clone(),
        ])
        .unwrap();
    assert_eq!(r.installed, vec!["a".to_string()]);
    assert_eq!(
        r.stale,
        vec!["b".to_string()],
        "installed b is no longer listed"
    );
    assert_eq!(r.foreign.len(), 2);
    assert!(r.foreign.contains(&foreign_by_name) && r.foreign.contains(&unknown_uuid));

    let reg = m.registry_doc().unwrap();
    let a = reg.find("a").unwrap();
    assert_eq!(a.install.state, InstallState::Installed);
    assert_eq!(a.install.wia.as_ref().unwrap(), &a_entry);
    let b = reg.find("b").unwrap();
    assert_eq!(b.install.state, InstallState::Stale);
    assert_eq!(b.install.reason.as_deref(), Some("not listed by Windows"));
    assert_eq!(
        reg.find("default").unwrap().install.state,
        InstallState::New
    );
    // Foreign entries left no trace anywhere in the document.
    let after = std::fs::read_to_string(store.path().unwrap()).unwrap();
    assert_ne!(before, after);
    assert!(!after.contains("fakescan lookalike") && !after.contains("vid_04f9"));
    assert_eq!(reg.devices.len(), 3);

    // Idempotent: a second identical reconcile changes no state and keeps a installed.
    let r2 = m.reconcile_with_windows(&[a_entry]).unwrap();
    assert_eq!(r2.installed, vec!["a".to_string()]);
    assert!(r2.stale.is_empty());
}

#[tokio::test]
async fn reconcile_flags_changed_published_caps_but_not_after_a_re_add() {
    let profiles = profiles_copy("reconcile2");
    let store = RegistryStore::file(reg_path("reconcile2"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles, Some(store.clone())));
    m.add(DeviceConfig::derived("t", "feeder-tray5"))
        .await
        .unwrap();
    let u = m.list().await[0].uuid;
    m.mark_installed("t", wia(&u, "{W1}")).await.unwrap();
    // Simulate: profile file changed on disk behind an installed row (e.g. edited while down).
    store
        .update(|r| {
            let row = r.find_mut("t").unwrap();
            row.install.published_fingerprint_at_install = Some("sha256:old".into());
        })
        .unwrap();
    let r = m.reconcile_with_windows(&[wia(&u, "{W1}")]).unwrap();
    assert_eq!(r.stale, vec!["t".to_string()]);
    assert_eq!(row_state(&m, "t").await, InstallState::Stale);
    // Still the same WIA id: still stale. A new WIA id means Windows re-Added it.
    m.reconcile_with_windows(&[wia(&u, "{W1}")]).unwrap();
    assert_eq!(row_state(&m, "t").await, InstallState::Stale);
    let r = m.reconcile_with_windows(&[wia(&u, "{W2}")]).unwrap();
    assert_eq!(r.installed, vec!["t".to_string()]);
    assert_eq!(row_state(&m, "t").await, InstallState::Installed);
}

#[tokio::test]
async fn concurrent_probe_and_install_on_different_devices_lose_nothing() {
    let path = reg_path("conc");
    let store = RegistryStore::file(&path);
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store.clone())));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.add(DeviceConfig::derived("x", "feeder-tray5"))
        .await
        .unwrap();
    m.set_probe_interval(Duration::ZERO);
    let ux = m.list().await[1].uuid;

    // A second "process" (separate store handle on the same file) hammers its own device.
    let other = RegistryStore::file(&path);
    let t = std::thread::spawn(move || {
        for i in 0..40 {
            other
                .update(|r| {
                    let row = r.find_mut("default").unwrap();
                    row.touch_probed(&format!("10.9.9.{i}"));
                })
                .unwrap();
        }
    });
    let m2 = Arc::clone(&m);
    let a = tokio::spawn(async move {
        for i in 0..40 {
            m2.mark_probed("x", &format!("10.0.0.{i}")).await.unwrap();
            m2.mark_installed("x", wia(&ux, &format!("{{I{i}}}")))
                .await
                .unwrap();
        }
    });
    a.await.unwrap();
    t.join().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    let v: Value = serde_json::from_str(&text).expect("valid JSON");
    assert_eq!(v["devices"].as_array().unwrap().len(), 2);
    let reg = Registry::load_from(&path).unwrap();
    assert_eq!(
        reg.find("default").unwrap().install.last_peer.as_deref(),
        Some("10.9.9.39")
    );
    let x = reg.find("x").unwrap();
    assert_eq!(x.install.state, InstallState::Installed);
    assert_eq!(x.install.wia.as_ref().unwrap().id, "{I39}");
    assert_eq!(x.install.last_peer.as_deref(), Some("10.0.0.39"));
}

// ── HTTP surface, persistence, --manage ────────────────────────────────────────

struct Srv {
    addr: SocketAddr,
    ready: Ready,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

fn base_cfg(devices: Vec<DeviceConfig>, registry: Option<PathBuf>) -> ServerConfig {
    let mut cfg = ServerConfig::lab(profiles_src(), "brother-wsd");
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.discovery = false;
    cfg.probe_listen = false;
    cfg.instance_lock = false;
    cfg.named_pipe = false;
    cfg.ctrl_c = false;
    cfg.devices = devices;
    cfg.registry = registry;
    cfg
}

#[test]
fn fake_uuid_prefix_rule() {
    use fakescan::lab::{is_fake_uuid, uuid_is_fake, FAKE_UUID_PREFIX};
    assert_eq!(FAKE_UUID_PREFIX, "00000000-0000-4000-8000-");
    assert!(is_fake_uuid(fakescan::lab::LAB_DEVICE_UUID));
    assert!(is_fake_uuid("{00000000-0000-4000-8000-0000000000F2}"));
    assert!(is_fake_uuid(
        "urn:uuid:00000000-0000-4000-8000-0000000000f2"
    ));
    assert!(!is_fake_uuid("3f2b9c1e-7d44-4a55-9e0b-1c2d3e4f5a6b"));
    assert!(!is_fake_uuid("00000000-0000-0000-0000-0000000000f2"));
    assert!(!is_fake_uuid("nope"));
    assert!(uuid_is_fake(&fakescan::lab::random_fake_uuid()));
    assert!(uuid_is_fake(&DeviceConfig::derived("x", "p").device_uuid));
}

impl Srv {
    async fn start(cfg: ServerConfig) -> Srv {
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

    async fn json(&self, method: &str, path: &str, body: Value) -> (u16, Value) {
        let b = if body.is_null() {
            String::new()
        } else {
            body.to_string()
        };
        let mut s = TcpStream::connect(self.addr).await.unwrap();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{b}",
            self.addr,
            b.len()
        );
        s.write_all(req.as_bytes()).await.unwrap();
        let mut raw = Vec::new();
        s.read_to_end(&mut raw).await.unwrap();
        let split = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        let head = String::from_utf8_lossy(&raw[..split]).to_string();
        let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut body = raw[split + 4..].to_vec();
        if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            body = dechunk(&body);
        }
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
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

#[tokio::test]
async fn runtime_add_persists_remove_keeps_forget_drops_install() {
    let path = reg_path("crud");
    let srv = Srv::start(base_cfg(
        vec![DeviceConfig::new(
            "default",
            "brother-wsd",
            fakescan::lab::lab_device_uuid(),
        )],
        Some(path.clone()),
    ))
    .await;
    // Start-up already wrote the default device's row.
    assert!(Registry::load_from(&path)
        .unwrap()
        .find("default")
        .is_some());

    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late", "profile": "feeder-tray5"}),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["install"]["state"], "new");
    assert_eq!(v["needs_readd"], false);
    assert!(v["config"]["fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert!(v["config"]["published_fingerprint"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));
    assert_eq!(v["config"]["feeder"]["capacity"], 5);
    let uuid = v["uuid"].as_str().unwrap().to_string();
    let reg = Registry::load_from(&path).unwrap();
    let row = reg.find("late").expect("runtime add persisted a row");
    assert_eq!(row.device_uuid, uuid);
    assert_eq!(row.profile, "feeder-tray5");
    assert_eq!(row.listen, "127.0.0.1:0");

    // Install it (HTTP), then remove: the row and its install state stay.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices/late/installed",
            json!({"wia_id":"{W}","wia_name":"fakescan late","wia_port":format!("urn:uuid:{uuid}/uri:{uuid}/ScannerService")}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "installed");
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices/late/installed",
            json!({"wia_id":"{W}","wia_port":"urn:uuid:nope"}),
        )
        .await;
    assert_eq!(st, 400, "port without the UUID is refused");
    let (st, _) = srv
        .json("POST", "/v1/devices/zzz/installed", json!({}))
        .await;
    assert_eq!(st, 404);

    let (st, _) = srv.json("DELETE", "/v1/devices/late", Value::Null).await;
    assert_eq!(st, 200);
    let reg = Registry::load_from(&path).unwrap();
    let row = reg.find("late").expect("remove keeps the row");
    assert_eq!(row.install.state, InstallState::Installed);

    // Re-add with no UUID: the stored UUID comes back, install state intact.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late", "profile": "feeder-tray5"}),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["uuid"].as_str().unwrap(), uuid);
    assert_eq!(v["install"]["state"], "installed");

    // Reconfigure over HTTP: tray5 -> tray50 publishes a bigger ADF envelope => needs_readd.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices/late/reconfigure",
            json!({"profile": "feeder-tray50"}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["needs_readd"], true);
    assert_eq!(v["install"]["state"], "stale");
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let late = list
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "late")
        .unwrap();
    assert_eq!(
        late["needs_readd_reason"],
        "profile changed; remove and re-Add in Imaging"
    );

    // POST forget: install info dropped, row (identity/config) kept.
    let (st, v) = srv
        .json("POST", "/v1/devices/late/forget", Value::Null)
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "new");
    assert!(v["install"].get("wia").is_none());
    let reg = Registry::load_from(&path).unwrap();
    let row = reg.find("late").unwrap();
    assert_eq!(row.install.state, InstallState::New);
    assert!(row.install.installed_at.is_none() && !row.added.pnp);
    assert_eq!(row.device_uuid, uuid);

    // GET /v1/registry returns the document.
    let (st, doc) = srv.json("GET", "/v1/registry", Value::Null).await;
    assert_eq!(st, 200);
    assert_eq!(doc["devices"].as_array().unwrap().len(), 2);

    // DELETE ?forget=true drops the row.
    let (st, _) = srv
        .json("DELETE", "/v1/devices/late?forget=true", Value::Null)
        .await;
    assert_eq!(st, 200);
    assert!(Registry::load_from(&path).unwrap().find("late").is_none());
    srv.stop().await;
}

#[tokio::test]
async fn http_reconcile_and_ctl_commands() {
    let path = reg_path("httprecon");
    let srv = Srv::start(base_cfg(
        vec![
            DeviceConfig::new("default", "brother-wsd", fakescan::lab::lab_device_uuid()),
            DeviceConfig::derived("g", "glass-wsd"),
        ],
        Some(path.clone()),
    ))
    .await;
    let g = srv.ready.devices.iter().find(|d| d.id == "g").unwrap().uuid;
    let (st, r) = srv
        .json(
            "POST",
            "/v1/reconcile",
            json!({"scanners": [
                {"id": "{1}", "name": "fakescan g", "port": format!("urn:uuid:{g}/uri:{g}")},
                {"id": "{2}", "name": "Some Real Brother", "port": "192.168.1.9"}
            ]}),
        )
        .await;
    assert_eq!(st, 200, "{r}");
    assert_eq!(r["installed"], json!(["g"]));
    assert_eq!(r["stale"], json!([]));
    assert_eq!(r["foreign"][0]["name"], "Some Real Brother");

    // ctl: forget g on its own device scope, then installed via ctl, then mark_added is legacy.
    let (st, v) = srv
        .json("POST", "/d/g/v1/ctl", json!({"cmd": "forget"}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "new");
    let (st, v) = srv
        .json(
            "POST",
            "/d/g/v1/ctl",
            json!({"cmd": "installed", "args": {"wia_id": "{3}", "wia_name": "x", "wia_port": format!("urn:uuid:{g}/")}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "installed");
    let (st, v) = srv
        .json("POST", "/v1/ctl", json!({"cmd": "mark_added"}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "installed");
    assert_eq!(v["install"]["note"], "legacy mark-added");
    assert!(
        v["install"].get("wia").is_none(),
        "legacy mark-added claims no WIA details"
    );
    let (st, v) = srv
        .json(
            "POST",
            "/v1/ctl",
            json!({"cmd": "reconcile", "args": {"scanners": []}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(
        v["stale"].as_array().unwrap().len(),
        2,
        "both installed rows unlisted: {v}"
    );
    // ctl save still works and keeps install info (uses the attached registry, not the real one).
    let (st, v) = srv.json("POST", "/v1/ctl", json!({"cmd": "save"})).await;
    assert_eq!(st, 400, "no --lab-name: {v}");
    srv.stop().await;
}

#[tokio::test]
async fn manage_startup_brings_up_rows_for_this_listen_with_stored_uuids() {
    let path = reg_path("manage");
    let doc = json!({
        "devices": [
            {"name": "tray5", "listen": "127.0.0.1:0", "advertise": "",
             "device_uuid": "00000000-0000-4000-8000-0000000000a5", "profile": "feeder-tray5",
             "added": {"pnp": true}},
            {"name": "duplex", "listen": "127.0.0.1:0", "advertise": "",
             "device_uuid": "00000000-0000-4000-8000-0000000000d2", "profile": "feeder-duplex30",
             "friendly_name": "my duplex",
             "install": {"state": "installed", "installed_at": "2026-10-01T00:00:00Z",
                         "published_fingerprint_at_install": "sha256:zzz"}},
            {"name": "elsewhere", "listen": "0.0.0.0:3922", "advertise": "",
             "device_uuid": "00000000-0000-4000-8000-0000000000e1", "profile": "glass-wsd"}
        ]
    });
    std::fs::write(&path, doc.to_string()).unwrap();
    let mut cfg = base_cfg(vec![], Some(path.clone()));
    cfg.manage = true;
    let srv = Srv::start(cfg).await;
    let ids: Vec<_> = srv.ready.devices.iter().map(|d| d.id.clone()).collect();
    assert_eq!(ids, vec!["tray5", "duplex"], "only rows for this listen");
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let list = list.as_array().unwrap();
    assert_eq!(list[0]["uuid"], "00000000-0000-4000-8000-0000000000a5");
    assert_eq!(list[1]["uuid"], "00000000-0000-4000-8000-0000000000d2");
    assert_eq!(list[1]["friendly_name"], "my duplex");
    assert_eq!(
        list[0]["install"]["state"], "installed",
        "legacy added.pnp row"
    );
    // duplex was installed against a different published fingerprint: flagged at start-up.
    assert_eq!(list[1]["install"]["state"], "stale");
    assert_eq!(list[1]["needs_readd"], true);
    // tray5 is the default (first) device: legacy routes reach it.
    let (st, s) = srv.json("GET", "/v1/status", Value::Null).await;
    assert_eq!(st, 200);
    assert_eq!(s["profile_id"], "feeder-tray5");
    srv.stop().await;

    let reg = Registry::load_from(&path).unwrap();
    assert_eq!(reg.devices.len(), 3);
    assert_eq!(
        reg.find("tray5").unwrap().device_uuid,
        "00000000-0000-4000-8000-0000000000a5"
    );
    assert_eq!(
        reg.find("duplex").unwrap().device_uuid,
        "00000000-0000-4000-8000-0000000000d2"
    );
    let elsewhere = reg.find("elsewhere").unwrap();
    assert_eq!(elsewhere.listen, "0.0.0.0:3922");
    assert_eq!(
        elsewhere.install.state,
        InstallState::New,
        "other listens untouched"
    );
}

#[tokio::test]
async fn embedded_server_without_registry_path_never_writes_a_registry() {
    let _ = tmp_dir("emb");
    let guard = std::env::var_os("FAKESCAN_REGISTRY").map(PathBuf::from);
    let srv = Srv::start(base_cfg(
        vec![DeviceConfig::new(
            "default",
            "brother-wsd",
            fakescan::lab::lab_device_uuid(),
        )],
        None,
    ))
    .await;
    let (st, v) = srv.json("GET", "/v1/devices", Value::Null).await;
    assert_eq!(st, 200);
    // Install tracking still works in memory for the embedded server.
    assert_eq!(v[0]["install"]["state"], "new");
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "t", "profile": "feeder-tray5"}),
        )
        .await;
    assert_eq!(st, 201);
    let (st, r) = srv.json("GET", "/v1/registry", Value::Null).await;
    assert_eq!(st, 200);
    assert_eq!(r["devices"].as_array().unwrap().len(), 2);
    srv.stop().await;
    if let Some(g) = guard {
        assert!(
            !g.exists(),
            "embedded servers must not create any registry file"
        );
    }
}

// ── profile changes through the plain control surface use the manager's reconfigure path ──

async fn install_via_http(srv: &Srv, id: &str) -> String {
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let uuid = list
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == id)
        .unwrap()["uuid"]
        .as_str()
        .unwrap()
        .to_string();
    let (st, v) = srv
        .json(
            "POST",
            &format!("/v1/devices/{id}/installed"),
            json!({"wia_id":"{W}","wia_name":"x","wia_port":format!("urn:uuid:{uuid}/uri:{uuid}/ScannerService")}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    uuid
}

#[tokio::test]
async fn post_v1_profile_and_ctl_profile_keep_install_state_correct() {
    let path = reg_path("viaprofile");
    let mut cfg = base_cfg(
        vec![
            DeviceConfig::new("default", "brother-wsd", fakescan::lab::lab_device_uuid()),
            DeviceConfig::derived("tray", "feeder-tray5"),
        ],
        Some(path.clone()),
    );
    cfg.profiles = profiles_copy("viaprofile");
    let srv = Srv::start(cfg).await;
    install_via_http(&srv, "tray").await;
    install_via_http(&srv, "default").await;

    // Capacity-only change through POST /d/tray/v1/profile: still installed, config updated.
    let (st, v) = srv
        .json("POST", "/d/tray/v1/profile", json!({"id": "tray5-cap9"}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["profile_id"], "tray5-cap9");
    let reg = Registry::load_from(&path).unwrap();
    let row = reg.find("tray").unwrap();
    assert_eq!(
        row.install.state,
        InstallState::Installed,
        "capacity is not published"
    );
    assert_eq!(row.profile, "tray5-cap9");
    assert_eq!(row.config.as_ref().unwrap().feeder.capacity, Some(9));

    // tray5 -> tray50 through POST /v1/profile scoped to the device: stale.
    let (st, v) = srv
        .json("POST", "/d/tray/v1/profile", json!({"id": "feeder-tray50"}))
        .await;
    assert_eq!(st, 200, "{v}");
    let reg = Registry::load_from(&path).unwrap();
    assert_eq!(reg.find("tray").unwrap().install.state, InstallState::Stale);
    assert_eq!(reg.find("tray").unwrap().profile, "feeder-tray50");
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let tray = list
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["id"] == "tray")
        .unwrap();
    assert_eq!(tray["needs_readd"], true);
    // The other device is untouched.
    assert_eq!(
        reg.find("default").unwrap().install.state,
        InstallState::Installed
    );

    // Legacy alias (default device) through ctl `profile`: published change => stale.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/ctl",
            json!({"cmd": "profile", "args": {"id": "glass-wsd"}}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["profile_id"], "glass-wsd");
    let reg = Registry::load_from(&path).unwrap();
    assert_eq!(
        reg.find("default").unwrap().install.state,
        InstallState::Stale
    );
    assert_eq!(reg.find("default").unwrap().profile, "glass-wsd");

    // Errors stay 400 and change nothing.
    let (st, _) = srv
        .json("POST", "/d/tray/v1/profile", json!({"id": "no-such"}))
        .await;
    assert_eq!(st, 400);
    let (st, _) = srv
        .json(
            "POST",
            "/v1/ctl",
            json!({"cmd": "profile", "args": {"id": "no-such"}}),
        )
        .await;
    assert_eq!(st, 400);
    assert_eq!(
        Registry::load_from(&path)
            .unwrap()
            .find("tray")
            .unwrap()
            .profile,
        "feeder-tray50"
    );
    srv.stop().await;
}

#[tokio::test]
async fn v1_profile_without_a_registry_row_still_works() {
    let srv = Srv::start(base_cfg(
        vec![DeviceConfig::new(
            "default",
            "brother-wsd",
            fakescan::lab::lab_device_uuid(),
        )],
        None,
    ))
    .await;
    let (st, v) = srv
        .json("POST", "/v1/profile", json!({"id": "feeder-tray5"}))
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["profile_id"], "feeder-tray5");
    srv.stop().await;
}

// ── WS-Discovery Probe/Resolve records peer contact (message handler driven directly) ──

#[tokio::test]
async fn discovery_probe_and_resolve_mark_only_the_devices_that_answer() {
    use fakescan::discovery::{handle_datagram, DiscoveryConfig, DiscoveryStats};
    use fakescan::manager::DeviceCtx;

    let store = RegistryStore::file(reg_path("wsdprobe"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store)));
    m.set_probe_interval(Duration::ZERO);
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    m.add(DeviceConfig::derived("tray5", "feeder-tray5"))
        .await
        .unwrap();
    m.add(DeviceConfig::derived("tray50", "feeder-tray50"))
        .await
        .unwrap();
    let list = m.list().await;
    let dcfg = |id: &str| {
        let uuid = list.iter().find(|d| d.id == id).unwrap().uuid;
        let ctx = DeviceCtx {
            manager: Arc::downgrade(&m),
            id: id.to_string(),
        };
        DiscoveryConfig {
            endpoint: format!("http://127.0.0.1:3920/d/{id}/WebServices/Device"),
            device_uuid: uuid,
            friendly_name: id.into(),
            multicast_iface: "0.0.0.0".parse().unwrap(),
            listen_probes: false,
            stats: Arc::new(DiscoveryStats::default()),
            instance_id: 1,
            metadata_version: 1,
            on_peer: Some(Arc::new(move |ip| ctx.probe(ip))),
        }
    };
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap(); // plain loopback
    let peer: SocketAddr = "10.9.8.7:3702".parse().unwrap();
    let mut n = 0u64;
    let resolve = |uuid: Uuid| {
        format!(
            "<s:Envelope><s:Body><d:Resolve><a:EndpointReference><a:Address>urn:uuid:{uuid}</a:Address></a:EndpointReference></d:Resolve></s:Body></s:Envelope>"
        )
    };

    // Resolve addressed to tray5: only tray5 becomes probed.
    let t5 = list.iter().find(|d| d.id == "tray5").unwrap().uuid;
    for id in ["default", "tray5", "tray50"] {
        handle_datagram(&sock, &dcfg(id), resolve(t5).as_bytes(), peer, &mut n).await;
    }
    let mut ok = false;
    for _ in 0..100 {
        if row_state(&m, "tray5").await == InstallState::Probed {
            ok = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(ok, "tray5 probed by Resolve");
    let reg = m.registry_doc().unwrap();
    assert_eq!(
        reg.find("tray5").unwrap().install.last_peer.as_deref(),
        Some("10.9.8.7")
    );
    assert_eq!(row_state(&m, "default").await, InstallState::New);
    assert_eq!(row_state(&m, "tray50").await, InstallState::New);

    // A Types-matching Probe is answered by every device, so all of them record the peer.
    let probe = "<s:Envelope><s:Body><d:Probe><d:Types>wsdp:Device</d:Types></d:Probe></s:Body></s:Envelope>";
    for id in ["default", "tray5", "tray50"] {
        handle_datagram(&sock, &dcfg(id), probe.as_bytes(), peer, &mut n).await;
    }
    for _ in 0..100 {
        if row_state(&m, "default").await == InstallState::Probed
            && row_state(&m, "tray50").await == InstallState::Probed
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("Probe did not mark every answering device");
}

#[tokio::test]
async fn discovery_peer_contact_is_throttled_like_the_http_path() {
    use fakescan::discovery::{handle_datagram, DiscoveryConfig, DiscoveryStats};
    use fakescan::manager::DeviceCtx;

    let store = RegistryStore::file(reg_path("wsdthrottle"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles_src(), Some(store.clone())));
    m.set_probe_interval(Duration::from_secs(3600));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    let uuid = m.list().await[0].uuid;
    let ctx = DeviceCtx {
        manager: Arc::downgrade(&m),
        id: "default".into(),
    };
    let cfg = DiscoveryConfig {
        endpoint: "http://127.0.0.1:3920/WebServices/Device".into(),
        device_uuid: uuid,
        friendly_name: "x".into(),
        multicast_iface: "0.0.0.0".parse().unwrap(),
        listen_probes: false,
        stats: Arc::new(DiscoveryStats::default()),
        instance_id: 1,
        metadata_version: 1,
        on_peer: Some(Arc::new(move |ip| ctx.probe(ip))),
    };
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer: SocketAddr = "10.9.8.7:3702".parse().unwrap();
    let probe = "<s:Envelope><s:Body><d:Probe><d:Types>wsdp:Device</d:Types></d:Probe></s:Body></s:Envelope>";
    let before = store.writes();
    let mut n = 0u64;
    for _ in 0..20 {
        handle_datagram(&sock, &cfg, probe.as_bytes(), peer, &mut n).await;
    }
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        store.writes(),
        before + 1,
        "20 probes => one registry write"
    );
    assert_eq!(row_state(&m, "default").await, InstallState::Probed);
}

// ── a port naming more than one UUID is foreign ──

#[test]
fn port_has_uuid_requires_exactly_one_distinct_uuid() {
    use fakescan::manager::port_has_uuid;
    let u = "00000000-0000-4000-8000-0000000000a5";
    let brother = "11111111-2222-4333-8444-555555555555"; // stands in for a real, foreign scanner
    assert!(port_has_uuid(
        &format!("urn:uuid:{u}/uri:{u}/ScannerService"),
        u
    ));
    assert!(port_has_uuid(&format!("{{{}}}", u.to_uppercase()), u));
    assert!(!port_has_uuid(
        &format!("urn:uuid:{brother}/uri:{brother}/ScannerService"),
        u
    ));
    assert!(!port_has_uuid(
        &format!("urn:uuid:{u}/uri:{brother}/ScannerService"),
        u
    ));
    assert!(!port_has_uuid(&format!("{brother} {u}"), u));
    assert!(!port_has_uuid("urn:uuid:nope", u));
    assert!(!port_has_uuid(u, "not-a-uuid"));
}

#[tokio::test]
async fn two_uuid_port_is_refused_by_installed_and_foreign_in_reconcile() {
    let path = reg_path("twouuid");
    let srv = Srv::start(base_cfg(
        vec![DeviceConfig::new(
            "default",
            "brother-wsd",
            fakescan::lab::lab_device_uuid(),
        )],
        Some(path.clone()),
    ))
    .await;
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let uuid = list[0]["uuid"].as_str().unwrap().to_string();
    let brother = "11111111-2222-4333-8444-555555555555"; // stands in for a real, foreign scanner
    let two = format!("urn:uuid:{uuid}/uri:{brother}/ScannerService");
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices/default/installed",
            json!({"wia_id":"{W}","wia_port":two}),
        )
        .await;
    assert_eq!(st, 400);
    assert_eq!(
        Registry::load_from(&path)
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .state,
        InstallState::New
    );

    let real_brother = format!("urn:uuid:{brother}/uri:{brother}/ScannerService");
    let (st, v) = srv
        .json(
            "POST",
            "/v1/reconcile",
            json!({"scanners":[
                {"id":"{1}","name":"x","port":two},
                {"id":"{2}","name":"Brother","port":real_brother}
            ]}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["foreign"].as_array().unwrap().len(), 2, "{v}");
    assert!(v["installed"].as_array().unwrap().is_empty(), "{v}");
    assert_eq!(
        Registry::load_from(&path)
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .state,
        InstallState::New
    );
    srv.stop().await;
}

// ── stale -> installed heal when a temporary profile change is undone ──────────────────────

#[tokio::test]
async fn switching_profile_away_and_back_restores_installed_with_identical_wia() {
    let path = reg_path("heal");
    let profiles = profiles_copy("heal");
    let store = RegistryStore::file(path.clone());
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles, Some(store)));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    let u = m.list().await[0].uuid;
    m.mark_installed("default", wia(&u, "{HEAL}"))
        .await
        .unwrap();
    let installed = m
        .registry_doc()
        .unwrap()
        .find("default")
        .unwrap()
        .install
        .clone();

    // Away: stale, remembering where it came from.
    let info = m.reconfigure("default", "glass-wsd").await.unwrap();
    assert!(info.needs_readd);
    let stale = m
        .registry_doc()
        .unwrap()
        .find("default")
        .unwrap()
        .install
        .clone();
    assert_eq!(stale.state, InstallState::Stale);
    assert_eq!(stale.stale_from, Some(InstallState::Installed));
    assert!(stale.stale_at.is_some());
    assert_eq!(stale.wia, installed.wia, "wia info kept while stale");
    // Staying on the same different profile keeps it stale (and does not reset stale_at).
    m.reconfigure("default", "glass-wsd").await.unwrap();
    assert_eq!(
        m.registry_doc()
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .stale_at,
        stale.stale_at
    );
    assert_eq!(row_state(&m, "default").await, InstallState::Stale);
    // A third profile with yet another published set is still not the install-time one.
    m.reconfigure("default", "accept-dib").await.unwrap();
    assert_eq!(row_state(&m, "default").await, InstallState::Stale);

    // Back: healed, nothing else changed.
    let info = m.reconfigure("default", "brother-wsd").await.unwrap();
    assert!(!info.needs_readd, "{info:?}");
    assert_eq!(info.needs_readd_reason, None);
    let row = m.registry_doc().unwrap().find("default").unwrap().clone();
    assert_eq!(row.install.state, InstallState::Installed);
    assert_eq!(row.install.reason, None);
    assert_eq!(row.install.stale_from, None);
    assert_eq!(row.install.stale_at, None);
    assert_eq!(row.install.wia, installed.wia);
    assert_eq!(row.install.installed_at, installed.installed_at);
    assert_eq!(row.install.verified_at, installed.verified_at);
    assert_eq!(
        row.install.published_fingerprint_at_install,
        installed.published_fingerprint_at_install
    );
    assert!(row.added.pnp, "legacy mirror restored");

    // The healed state survives a reload from disk.
    let reloaded = Registry::load_from(&path).unwrap();
    assert_eq!(
        reloaded.find("default").unwrap().install.state,
        InstallState::Installed
    );
    assert_eq!(reloaded.find("default").unwrap().install.wia, installed.wia);
}

#[tokio::test]
async fn persistent_profile_change_stays_stale_and_survives_reload() {
    let path = reg_path("stays");
    let profiles = profiles_copy("stays");
    let (m, _rx) = DeviceManager::new(manager_cfg(
        &profiles,
        Some(RegistryStore::file(path.clone())),
    ));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    let u = m.list().await[0].uuid;
    m.mark_installed("default", wia(&u, "{S}")).await.unwrap();
    m.reconfigure("default", "glass-wsd").await.unwrap();
    let reg = Registry::load_from(&path).unwrap();
    let i = &reg.find("default").unwrap().install;
    assert_eq!(i.state, InstallState::Stale);
    assert_eq!(i.stale_from, Some(InstallState::Installed));
    assert!(fakescan::timeutil::is_utc_iso(
        i.stale_at.as_deref().unwrap()
    ));
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("stale_from") && text.contains("stale_at"));
    // A fresh process (new manager, same file, still the changed profile) keeps it stale.
    let (m2, _rx2) = DeviceManager::new(manager_cfg(
        &profiles,
        Some(RegistryStore::file(path.clone())),
    ));
    m2.add(DeviceConfig::new(
        "default",
        "glass-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    assert_eq!(row_state(&m2, "default").await, InstallState::Stale);
}

#[tokio::test]
async fn stale_not_listed_by_windows_is_never_healed_by_a_profile_change() {
    let profiles = profiles_copy("notlisted");
    let store = RegistryStore::file(reg_path("notlisted"));
    let (m, _rx) = DeviceManager::new(manager_cfg(&profiles, Some(store)));
    m.add(DeviceConfig::new(
        "default",
        "brother-wsd",
        fakescan::lab::lab_device_uuid(),
    ))
    .await
    .unwrap();
    let u = m.list().await[0].uuid;
    m.mark_installed("default", wia(&u, "{N}")).await.unwrap();
    m.reconcile_with_windows(&[]).unwrap();
    let r = m.registry_doc().unwrap();
    assert_eq!(
        r.find("default").unwrap().install.reason.as_deref(),
        Some("not listed by Windows")
    );
    // Re-applying the install-time profile, and a round trip, change nothing.
    m.reconfigure("default", "brother-wsd").await.unwrap();
    m.reconfigure("default", "glass-wsd").await.unwrap();
    m.reconfigure("default", "brother-wsd").await.unwrap();
    let r = m.registry_doc().unwrap();
    let i = &r.find("default").unwrap().install;
    assert_eq!(i.state, InstallState::Stale);
    assert!(i.reason.as_deref().unwrap().starts_with("not listed"));
    // Only a reconcile that sees it again brings it back.
    m.reconcile_with_windows(&[wia(&u, "{N}")]).unwrap();
    assert_eq!(row_state(&m, "default").await, InstallState::Installed);
}

#[tokio::test]
async fn http_and_ctl_profile_round_trip_heals() {
    let path = reg_path("httpheal");
    let mut cfg = base_cfg(
        vec![DeviceConfig::new(
            "default",
            "brother-wsd",
            fakescan::lab::lab_device_uuid(),
        )],
        Some(path.clone()),
    );
    cfg.profiles = profiles_copy("httpheal");
    let srv = Srv::start(cfg).await;
    install_via_http(&srv, "default").await;
    let wia_before = Registry::load_from(&path)
        .unwrap()
        .find("default")
        .unwrap()
        .install
        .wia
        .clone();
    let state = |p: &PathBuf| {
        Registry::load_from(p)
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .state
    };

    let (st, _) = srv
        .json("POST", "/v1/profile", json!({"id": "glass-wsd"}))
        .await;
    assert_eq!(st, 200);
    assert_eq!(state(&path), InstallState::Stale);
    let (st, _) = srv
        .json("POST", "/v1/profile", json!({"id": "brother-wsd"}))
        .await;
    assert_eq!(st, 200);
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    assert_eq!(list[0]["install"]["state"], "installed");
    assert_eq!(list[0]["needs_readd"], false);

    // ctl `profile` and /v1/devices/{id}/reconfigure take the same path.
    srv.json(
        "POST",
        "/v1/ctl",
        json!({"cmd": "profile", "args": {"id": "glass-wsd"}}),
    )
    .await;
    assert_eq!(state(&path), InstallState::Stale);
    srv.json(
        "POST",
        "/v1/ctl",
        json!({"cmd": "profile", "args": {"id": "brother-wsd"}}),
    )
    .await;
    assert_eq!(state(&path), InstallState::Installed);
    srv.json(
        "POST",
        "/v1/devices/default/reconfigure",
        json!({"profile": "glass-wsd"}),
    )
    .await;
    assert_eq!(state(&path), InstallState::Stale);
    srv.json(
        "POST",
        "/v1/devices/default/reconfigure",
        json!({"profile": "brother-wsd"}),
    )
    .await;
    assert_eq!(state(&path), InstallState::Installed);
    assert_eq!(
        Registry::load_from(&path)
            .unwrap()
            .find("default")
            .unwrap()
            .install
            .wia,
        wia_before
    );
    // POST /v1/reset does not re-apply a profile: a stale row stays as it is.
    srv.json("POST", "/v1/profile", json!({"id": "glass-wsd"}))
        .await;
    srv.json("POST", "/v1/reset", Value::Null).await;
    assert_eq!(state(&path), InstallState::Stale);
    srv.stop().await;
}

#[tokio::test]
async fn manage_startup_heals_a_stale_profile_changed_row_whose_profile_matches_again() {
    let path = reg_path("manageheal");
    let src = profiles_src();
    let fp = fakescan::registry::published_fingerprint(
        &fakescan::profile::Profile::load(&src, "feeder-tray5").unwrap(),
    );
    // Rows as left by a daemon that died while stale (OLD rows: no stale_from/stale_at).
    let row = |name: &str, last: u8, reason: &str, at: &str| {
        json!({"name": name, "listen": "127.0.0.1:0", "advertise": "",
               "device_uuid": format!("00000000-0000-4000-8000-0000000000{last:02x}"),
               "profile": "feeder-tray5", "future_field": 7,
               "install": {"state": "stale", "reason": reason, "installed_at": "2026-10-01T00:00:00Z",
                           "wia": {"id": "{K}", "name": "x", "port": "p"},
                           "published_fingerprint_at_install": at}})
    };
    let doc = json!({"devices": [
        row("healme", 0xa1, "profile changed; remove and re-Add in Imaging", &fp),
        row("offline", 0xa2, "not listed by Windows", &fp),
        row("differs", 0xa3, "profile changed; remove and re-Add in Imaging", "sha256:other"),
    ]});
    std::fs::write(&path, doc.to_string()).unwrap();
    let mut cfg = base_cfg(vec![], Some(path.clone()));
    cfg.manage = true;
    let srv = Srv::start(cfg).await;
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let by = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by("healme")["install"]["state"], "installed");
    assert_eq!(by("healme")["needs_readd"], false);
    assert_eq!(by("offline")["install"]["state"], "stale");
    assert_eq!(by("differs")["install"]["state"], "stale");
    srv.stop().await;
    let reg = Registry::load_from(&path).unwrap();
    let h = reg.find("healme").unwrap();
    assert_eq!(h.install.state, InstallState::Installed);
    assert_eq!(h.install.wia.as_ref().unwrap().id, "{K}");
    assert_eq!(
        h.install.installed_at.as_deref(),
        Some("2026-10-01T00:00:00Z")
    );
    assert!(h.added.pnp);
    assert_eq!(
        h.extra.get("future_field"),
        Some(&json!(7)),
        "unknown fields preserved"
    );
}

// ── initial_pages: tray preload on create / reset (runtime state, not a capability) ──────────

/// Profiles copy with `tray5-init3`: feeder-tray5 (capacity 5) that starts with 3 sheets.
fn profiles_with_initial(tag: &str) -> PathBuf {
    let dst = profiles_copy(tag);
    let mut v: Value =
        serde_json::from_str(&std::fs::read_to_string(dst.join("feeder-tray5.json")).unwrap())
            .unwrap();
    v["id"] = "tray5-init3".into();
    v["feeder"]["initial_pages"] = 3.into();
    std::fs::write(dst.join("tray5-init3.json"), v.to_string()).unwrap();
    dst
}

async fn pages_of(srv: &Srv, prefix: &str) -> (u64, bool, u64) {
    let (st, v) = srv
        .json("GET", &format!("{prefix}/v1/status"), Value::Null)
        .await;
    assert_eq!(st, 200, "{v}");
    (
        v["pages_remaining"].as_u64().unwrap(),
        v["feeder_ready"].as_bool().unwrap(),
        v["initial_pages"].as_u64().unwrap(),
    )
}

#[tokio::test]
async fn initial_pages_default_is_empty_and_profile_value_applies_after_reset() {
    let mut cfg = base_cfg(
        vec![
            DeviceConfig::derived("default", "brother-wsd"),
            DeviceConfig::derived("plain", "feeder-tray5"),
            DeviceConfig::derived("loaded", "tray5-init3"),
        ],
        None,
    );
    cfg.profiles = profiles_with_initial("init-profile");
    let srv = Srv::start(cfg).await;
    // Nothing opts in: every existing profile still starts empty.
    assert_eq!(pages_of(&srv, "").await, (0, false, 0));
    assert_eq!(pages_of(&srv, "/d/plain").await, (0, false, 0));
    // The opting-in profile starts loaded and ready.
    assert_eq!(pages_of(&srv, "/d/loaded").await, (3, true, 3));
    // A run-time POST /v1/pages overrides it ...
    let (st, _) = srv
        .json("POST", "/d/loaded/v1/pages", json!({"count": 1}))
        .await;
    assert_eq!(st, 200);
    assert_eq!(pages_of(&srv, "/d/loaded").await.0, 1);
    // ... and reset restores the initial stack; the other device is still empty after reset.
    let (st, _) = srv.json("POST", "/d/loaded/v1/reset", Value::Null).await;
    assert_eq!(st, 200);
    assert_eq!(pages_of(&srv, "/d/loaded").await, (3, true, 3));
    srv.json("POST", "/d/plain/v1/reset", Value::Null).await;
    assert_eq!(pages_of(&srv, "/d/plain").await, (0, false, 0));
    // The device listing carries it too.
    let (_, list) = srv.json("GET", "/v1/devices", Value::Null).await;
    let by = |id: &str| {
        list.as_array()
            .unwrap()
            .iter()
            .find(|d| d["id"] == id)
            .unwrap()
            .clone()
    };
    assert_eq!(by("plain")["initial_pages"], 0);
    assert_eq!(by("loaded")["initial_pages"], 3);
    srv.stop().await;
}

#[tokio::test]
async fn initial_pages_device_override_daemon_default_and_capacity_clamp() {
    let mut own = DeviceConfig::derived("own", "tray5-init3");
    own.initial_pages = Some(2);
    let mut zero = DeviceConfig::derived("zero", "tray5-init3");
    zero.initial_pages = Some(0);
    let mut cfg = base_cfg(
        vec![
            DeviceConfig::derived("default", "feeder-tray5"),
            own,
            zero,
            DeviceConfig::derived("prof", "tray5-init3"),
        ],
        None,
    );
    cfg.profiles = profiles_with_initial("init-override");
    cfg.initial_pages = Some(9); // daemon-wide default; the tray holds 5
    let srv = Srv::start(cfg).await;
    // No own value: the daemon default, clamped to the 5-sheet tray.
    assert_eq!(pages_of(&srv, "").await, (5, true, 5));
    assert_eq!(pages_of(&srv, "/d/prof").await, (5, true, 5));
    // Own values win over the daemon default and the profile, including an explicit 0.
    assert_eq!(pages_of(&srv, "/d/own").await, (2, true, 2));
    assert_eq!(pages_of(&srv, "/d/zero").await, (0, false, 0));
    srv.json("POST", "/d/own/v1/pages", json!({"count": 0}))
        .await;
    srv.json("POST", "/d/own/v1/reset", Value::Null).await;
    assert_eq!(pages_of(&srv, "/d/own").await, (2, true, 2));

    // POST /v1/devices takes initial_pages; omitting it uses the daemon default.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late", "profile": "feeder-tray50", "initial_pages": 6}),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(v["initial_pages"], 6);
    assert_eq!(pages_of(&srv, "/d/late").await, (6, true, 6));
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "late2", "profile": "feeder-tray50"}),
        )
        .await;
    assert_eq!(st, 201);
    assert_eq!(pages_of(&srv, "/d/late2").await, (9, true, 9));
    srv.stop().await;
}

#[tokio::test]
async fn initial_pages_is_persisted_and_does_not_touch_fingerprints_or_install_state() {
    let path = reg_path("init-registry");
    let mut cfg = base_cfg(
        vec![DeviceConfig::derived("default", "feeder-tray5")],
        Some(path.clone()),
    );
    cfg.profiles = profiles_with_initial("init-registry-profiles");
    let srv = Srv::start(cfg.clone()).await;
    let (_, base) = srv.json("GET", "/v1/devices", Value::Null).await;
    let base_cfg_info = base[0]["config"].clone();
    let uuid = base[0]["uuid"].as_str().unwrap().to_string();
    // Install the device, then add a loaded sibling with the same effective capabilities.
    let (st, _) = srv
        .json(
            "POST",
            "/v1/devices/default/installed",
            json!({"wia_id":"{W}","wia_name":"x","wia_port":format!("urn:uuid:{uuid}/uri:{uuid}/ScannerService")}),
        )
        .await;
    assert_eq!(st, 200);
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices",
            json!({"id": "loaded", "profile": "feeder-tray5", "initial_pages": 4}),
        )
        .await;
    assert_eq!(st, 201, "{v}");
    assert_eq!(
        v["config"], base_cfg_info,
        "same fingerprints as the empty twin"
    );
    // Switching the installed device to a profile that differs only in initial_pages is not
    // a capability change: it stays installed, not stale.
    let (st, v) = srv
        .json(
            "POST",
            "/v1/devices/default/reconfigure",
            json!({"profile": "tray5-init3"}),
        )
        .await;
    assert_eq!(st, 200, "{v}");
    assert_eq!(v["install"]["state"], "installed");
    assert_eq!(v["needs_readd"], false);
    assert_eq!(v["config"]["fingerprint"], base_cfg_info["fingerprint"]);
    assert_eq!(v["initial_pages"], 3);
    assert_eq!(pages_of(&srv, "").await, (3, true, 3), "reload applies it");
    srv.stop().await;

    let reg = Registry::load_from(&path).unwrap();
    assert_eq!(reg.find("loaded").unwrap().initial_pages, Some(4));
    assert_eq!(reg.find("default").unwrap().initial_pages, None);

    // Restart from the registry alone: the row's own value comes back.
    let mut cfg2 = base_cfg(vec![], Some(path.clone()));
    cfg2.manage = true;
    cfg2.profiles = cfg.profiles.clone();
    let srv2 = Srv::start(cfg2).await;
    assert_eq!(pages_of(&srv2, "/d/loaded").await, (4, true, 4));
    srv2.stop().await;
}
