//! Control-plane guards: profile ids stay inside the profiles directory, host commands and the
//! registry are loopback-only, and scan knobs stay reachable from another address.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Request, StatusCode};
use fakescan::manager::{DeviceManager, ManagerConfig};
use fakescan::profile::Profile;
use fakescan::server::{manager_router, router, DeviceConfig};
use fakescan::state::World;
use tokio::sync::RwLock;
use tower::ServiceExt;

fn profiles() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

fn world() -> Arc<RwLock<World>> {
    let dir = profiles();
    let profile = Profile::load(&dir, "brother-wsd").expect("profile");
    Arc::new(RwLock::new(World::new(dir, profile)))
}

fn with_peer(method: &str, uri: &str, body: &'static str, peer: &str) -> Request<Body> {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    let addr: SocketAddr = peer.parse().unwrap();
    req.extensions_mut().insert(ConnectInfo(addr));
    req
}

async fn manager() -> Arc<DeviceManager> {
    let addr: SocketAddr = "127.0.0.1:9".parse().unwrap();
    let (m, _exit) = DeviceManager::new(ManagerConfig {
        profiles: profiles(),
        wsd: false,
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
    m.add(DeviceConfig::derived("default", "brother-wsd"))
        .await
        .unwrap();
    m
}

#[tokio::test]
async fn lan_peer_can_load_pages_but_cannot_shut_down_or_read_the_registry() {
    let w = world();
    let (tx, mut rx) = tokio::sync::watch::channel(false);
    w.write().await.arm_exit(tx);
    let app = router(Arc::clone(&w), false);
    let res = app
        .oneshot(with_peer(
            "POST",
            "/v1/pages",
            r#"{"count":2}"#,
            "192.168.1.20:40000",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(w.read().await.status().pages_remaining, 2);

    let app = router(Arc::clone(&w), false);
    let res = app
        .oneshot(with_peer(
            "POST",
            "/v1/ctl",
            r#"{"cmd":"shutdown","args":{"yes":true}}"#,
            "192.168.1.20:40000",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(!*rx.borrow());

    let app = router(w, false);
    let res = app
        .oneshot(with_peer(
            "POST",
            "/v1/ctl",
            r#"{"cmd":"shutdown","args":{"yes":true}}"#,
            "127.0.0.1:40000",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    rx.changed().await.expect("exit signal");
    assert!(*rx.borrow());

    let app = manager_router(manager().await);
    let res = app
        .oneshot(with_peer("GET", "/v1/registry", "", "192.168.1.20:40000"))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);

    let app = manager_router(manager().await);
    let res = app
        .oneshot(with_peer(
            "POST",
            "/v1/devices",
            r#"{"id":"late","profile":"brother-wsd"}"#,
            "10.1.2.3:9",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn profile_post_rejects_a_path() {
    let w = world();
    let app = router(Arc::clone(&w), false);
    let res = app
        .oneshot(with_peer(
            "POST",
            "/v1/profile",
            r#"{"id":"../brother-wsd"}"#,
            "127.0.0.1:1",
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    assert_eq!(w.read().await.profile().id, "brother-wsd");
}
