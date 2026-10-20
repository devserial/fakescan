//! The fakescan server as a library: control-plane router, WSD Scan Service, WS-Discovery.
//!
//! `main.rs` is argument parsing around [`serve`]; embedders (the Python bindings) call the same
//! function with their own [`ServerConfig`] and a stop channel, so a test can run the exact
//! server the lab runs, in process, on an ephemeral port.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, Query, State};
use axum::http::Request;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, RwLock};
use tower_http::trace::TraceLayer;
use tracing::{info, info_span};
use uuid::Uuid;

use crate::ctl::{get_ctl, post_ctl};
use crate::effects::EffectsBody;
use crate::manager::{valid_id, DeviceCtx, DeviceManager, ManagerConfig};
use crate::profile::Profile;
use crate::registry::{ConfigInfo, InstallInfo, RegistryStore, WiaInfo};
use crate::scenarios;
use crate::state::{
    FaultBody, FeederBody, JpegComBody, LoopbackBody, LoopbackStatus, PagesBody, PatternBody,
    ScenarioBody, SlowBody, Status, World,
};
use crate::{lab, wsd};

/// One emulated scanner: its own profile, UUID, endpoints and `World`.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceConfig {
    /// URL-safe slug, unique per server (`[A-Za-z0-9_-]+`). The first device is the "default"
    /// device: it also answers the legacy un-prefixed routes and keeps the legacy WSD paths.
    pub id: String,
    /// Profile id (`profiles/<id>.json`).
    pub profile: String,
    pub device_uuid: Uuid,
    /// Shown in Windows; defaults to `fakescan <id>` (`fakescan` for the default device).
    pub friendly_name: Option<String>,
    /// Sheets loaded into this device's tray at creation and on reset / profile reload.
    /// `None` = the daemon default ([`ServerConfig::initial_pages`]), else the profile's
    /// `feeder.initial_pages` (0 unless a profile opts in). Runtime state, not a capability.
    pub initial_pages: Option<u32>,
    /// Lab-safety override, default `false`: accept a device UUID outside
    /// [`lab::FAKE_UUID_PREFIX`]. Never reachable from the control plane (`POST /v1/devices`).
    #[serde(skip)]
    pub allow_foreign_uuid: bool,
}

impl DeviceConfig {
    pub fn new(id: impl Into<String>, profile: impl Into<String>, device_uuid: Uuid) -> Self {
        Self {
            id: id.into(),
            profile: profile.into(),
            device_uuid,
            friendly_name: None,
            initial_pages: None,
            allow_foreign_uuid: false,
        }
    }

    /// Like [`DeviceConfig::new`] with a stable UUID derived from the id, so a restarted
    /// server presents the same identity to Windows.
    pub fn derived(id: impl Into<String>, profile: impl Into<String>) -> Self {
        let id = id.into();
        let uuid = derived_uuid(&id);
        Self::new(id, profile, uuid)
    }
}

/// `00000000-0000-4000-8000-<48-bit FNV-1a of id>`: stable, v4-shaped, lab-looking.
fn derived_uuid(id: &str) -> Uuid {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in id.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    let tail = h & 0xffff_ffff_ffff;
    Uuid::parse_str(&format!("00000000-0000-4000-8000-{tail:012x}")).expect("well-formed uuid")
}

/// Everything [`serve`] needs, already resolved (CLI / registry / defaults are the caller's job).
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Listen address. Port `0` picks a free port (see [`Ready::local_addr`]).
    pub listen: SocketAddr,
    /// Host advertised in WS-Discovery XAddrs and the scan endpoints; defaults to the listen IP.
    pub advertise: Option<IpAddr>,
    /// Serve the WSD Scan Service. Off = control API only.
    pub wsd: bool,
    /// Announce on WS-Discovery (Hello/Bye, Probe answers). Needs `wsd`. An embedded test server
    /// leaves this off so it does not advertise a second device on the LAN.
    pub discovery: bool,
    pub profiles: PathBuf,
    /// The emulated scanners; the first is the default (legacy-route) device. Must be non-empty.
    pub devices: Vec<DeviceConfig>,
    /// Lab instance name; applies to the default device (`/v1/ctl save`, named pipe).
    pub lab_name: Option<String>,
    /// Bind UDP :3702 to answer WS-Discovery Probes.
    pub probe_listen: bool,
    /// Hold the per-port single-instance lock (skipped for ephemeral ports).
    pub instance_lock: bool,
    /// Serve the Windows named pipe control surface.
    pub named_pipe: bool,
    /// Treat Ctrl-C as a shutdown request (a library host usually owns signals itself).
    pub ctrl_c: bool,
    /// Daemon mode: persist device rows and install state to this registry file. `None` (the
    /// default, and what embedded in-process servers use) never touches a registry.
    pub registry: Option<PathBuf>,
    /// Daemon `--manage`: with no `devices`, bring up the registry rows whose `listen` equals
    /// [`ServerConfig::listen`], with their stored UUIDs and profiles. Needs `registry`.
    pub manage: bool,
    /// Lab-safety override (default `false`): devices restored from the registry (`manage`) may
    /// use UUIDs outside [`lab::FAKE_UUID_PREFIX`].
    pub allow_foreign_uuid: bool,
    /// Daemon-wide default for devices that set no `initial_pages` of their own
    /// (`--initial-pages N`). `None` = each profile's `feeder.initial_pages` (0 by default).
    pub initial_pages: Option<u32>,
}

impl ServerConfig {
    /// Lab defaults for the given profile: `0.0.0.0:3920`, one device `default` with the stable
    /// lab UUID, WSD on.
    pub fn lab(profiles: impl Into<PathBuf>, profile: impl Into<String>) -> Self {
        Self {
            listen: lab::default_listen(),
            advertise: None,
            wsd: true,
            discovery: true,
            profiles: profiles.into(),
            devices: vec![DeviceConfig::new(
                "default",
                profile,
                lab::lab_device_uuid(),
            )],
            lab_name: None,
            probe_listen: true, // Hello-only is never found by Windows (see main.rs --probe-listen)
            instance_lock: true,
            named_pipe: true,
            ctrl_c: true,
            registry: None,
            manage: false,
            allow_foreign_uuid: false,
            initial_pages: None,
        }
    }
}

/// One row of `GET /v1/devices` (the stable per-device handle; later extended with
/// `install` / `config`).
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub id: String,
    pub uuid: Uuid,
    pub profile: String,
    pub friendly_name: String,
    /// Sheets a reset loads into the tray (device/daemon override, else the profile's
    /// `feeder.initial_pages`, clamped to capacity). 0 = starts empty. Runtime state.
    pub initial_pages: u32,
    /// `null` when WSD is off.
    pub scan_endpoint: Option<String>,
    pub device_endpoint: Option<String>,
    /// Install lifecycle from the registry (`null` only if the row is unavailable).
    pub install: Option<InstallInfo>,
    pub config: Option<ConfigInfo>,
    /// True when the device was installed and a profile change makes the capabilities Windows
    /// cached wrong: remove and re-Add in Imaging.
    pub needs_readd: bool,
    pub needs_readd_reason: Option<String>,
}

/// Sent once the listener is bound and the router is about to serve.
#[derive(Debug, Clone)]
pub struct Ready {
    pub local_addr: SocketAddr,
    /// UUID of the default (first) device.
    pub device_uuid: Uuid,
    pub devices: Vec<DeviceInfo>,
}

/// Run the server until `stop` fires, `POST /v1/ctl` asks it to exit, or (if enabled) Ctrl-C.
pub async fn serve(
    cfg: ServerConfig,
    ready: Option<oneshot::Sender<Ready>>,
    stop: Option<oneshot::Receiver<()>>,
) -> anyhow::Result<()> {
    let store = cfg.registry.as_ref().map(RegistryStore::file);
    let mut devices = cfg.devices.clone();
    if cfg.manage && devices.is_empty() {
        let st = store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("--manage needs a registry path"))?;
        let reg = st.snapshot()?;
        for row in reg.rows_for_listen(&cfg.listen.to_string()) {
            let uuid = match Uuid::parse_str(&row.device_uuid) {
                Ok(u) => u,
                Err(e) => {
                    tracing::warn!(row = %row.name, error = %e, "registry row has a bad UUID; skipped");
                    continue;
                }
            };
            if !valid_id(&row.name) || Profile::load(&cfg.profiles, &row.profile).is_err() {
                tracing::warn!(row = %row.name, profile = %row.profile, "registry row not startable; skipped");
                continue;
            }
            if !cfg.allow_foreign_uuid && !lab::uuid_is_fake(&uuid) {
                tracing::warn!(row = %row.name, "registry row UUID is outside the fake namespace; skipped");
                continue;
            }
            let mut dc = DeviceConfig::new(row.name.clone(), row.profile.clone(), uuid);
            dc.allow_foreign_uuid = cfg.allow_foreign_uuid;
            dc.friendly_name = row.friendly_name.clone();
            dc.initial_pages = row.initial_pages;
            devices.push(dc);
        }
        info!(count = devices.len(), listen = %cfg.listen, "--manage: brought up registry rows");
    }
    anyhow::ensure!(
        !devices.is_empty() || cfg.manage,
        "at least one device is required"
    );
    let mut seen = std::collections::HashSet::new();
    for d in &devices {
        anyhow::ensure!(
            valid_id(&d.id),
            "invalid device id {:?} (use [A-Za-z0-9_-]+)",
            d.id
        );
        anyhow::ensure!(seen.insert(d.id.clone()), "duplicate device id {:?}", d.id);
    }
    let uuids: std::collections::HashSet<_> = devices.iter().map(|d| d.device_uuid).collect();
    anyhow::ensure!(
        uuids.len() == devices.len(),
        "device UUIDs must be distinct"
    );
    // Fail fast on a typo before binding anything.
    for d in &devices {
        Profile::load(&cfg.profiles, &d.profile)?;
    }

    // Stale locks (dead PIDs) are reclaimed automatically.
    let _lock = if cfg.instance_lock && cfg.listen.port() != 0 {
        Some(lab::acquire_instance_lock(cfg.listen.port())?)
    } else {
        None
    };

    // Bind first so an ephemeral port is known before identity and endpoints are built.
    let listener = lab::bind_listener(cfg.listen).await?;
    let local_addr = listener.local_addr()?;

    let (manager, exit_rx) = DeviceManager::new(ManagerConfig {
        profiles: cfg.profiles.clone(),
        wsd: cfg.wsd,
        discovery: cfg.discovery,
        probe_listen: cfg.probe_listen,
        advertise: cfg.advertise.unwrap_or_else(|| local_addr.ip()),
        port: local_addr.port(),
        lab_name: cfg.lab_name.clone(),
        local_addr,
        listen_key: cfg.listen.to_string(),
        registry: store,
        initial_pages: cfg.initial_pages,
    });
    for d in &devices {
        manager.add(d.clone()).await?;
    }
    let app = manager_router(Arc::clone(&manager));

    if cfg.named_pipe {
        if let Some(def) = manager.default_device().await {
            let pipe_name = cfg
                .lab_name
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let ctx = DeviceCtx {
                manager: Arc::downgrade(&manager),
                id: def.info.id.clone(),
            };
            tokio::spawn(async move {
                crate::pipe::serve(&pipe_name, def.world, Some(ctx)).await;
            });
        }
    }

    let default_uuid = devices
        .first()
        .map(|d| d.device_uuid)
        .unwrap_or_else(Uuid::nil);
    info!(
        listen = %local_addr,
        uuid = %default_uuid,
        devices = devices.len(),
        lab_name = ?cfg.lab_name,
        "fakescan listening — attach: http://127.0.0.1:{}/v1/ctl", local_addr.port()
    );
    if let Some(tx) = ready {
        let _ = tx.send(Ready {
            local_addr,
            device_uuid: default_uuid,
            devices: manager.list().await,
        });
    }

    let ctrl_c = cfg.ctrl_c;
    let served = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal(exit_rx, stop, ctrl_c).await;
    })
    .await;

    // Let discovery broadcast a WSD Bye per device before exit.
    manager.shutdown().await;
    served?;
    Ok(())
}

/// Routes for one device: the control plane plus (with `wsd_on`) its WSD endpoints.
/// [`manager_router`] dispatches to one of these per request.
pub fn device_router(world: Arc<RwLock<World>>, wsd_on: bool) -> Router {
    device_router_ctx(world, wsd_on, None)
}

/// [`device_router`] plus the manager context handlers use for peer tracking and manager commands.
pub fn device_router_ctx(
    world: Arc<RwLock<World>>,
    wsd_on: bool,
    ctx: Option<DeviceCtx>,
) -> Router {
    let mut app = Router::new()
        // Unified ctl surface.
        .route("/v1/ctl", get(get_ctl).post(post_ctl))
        // Fine-grained routes (kept for backward compat).
        .route("/v1/status", get(status))
        .route("/v1/jobs", get(jobs))
        .route("/v1/activity", get(trace))
        .route("/v1/trace", get(trace))
        .route("/v1/log", get(activity_log))
        .route("/v1/tickets", get(get_tickets).delete(delete_tickets))
        .route("/v1/profile", post(set_profile))
        .route("/v1/feeder", post(set_feeder))
        .route("/v1/pages", post(set_pages))
        .route("/v1/slow", post(set_slow))
        .route("/v1/pattern", post(set_pattern))
        .route("/v1/jpeg-com", post(set_jpeg_com))
        .route("/v1/effects", post(set_effects))
        .route("/v1/scenarios", get(list_scenarios))
        .route("/v1/scenario", post(set_scenario))
        .route(
            "/v1/faults/plan",
            get(get_fault_plan)
                .post(post_fault_plan)
                .delete(delete_fault_plan),
        )
        .route("/v1/fault", post(set_fault))
        .route("/v1/faults", post(set_fault))
        .route("/v1/condition", post(set_fault))
        .route(
            "/v1/loopback",
            get(get_loopback).post(set_loopback).delete(clear_loopback),
        )
        .route("/v1/reset", post(reset));

    if wsd_on {
        app = app
            .route("/WebServices/Device", post(wsd::scanner_service))
            .route("/WebServices/Device/", post(wsd::scanner_service))
            .route("/ScannerService", post(wsd::scanner_service))
            .route("/ScannerService/", post(wsd::scanner_service))
            .route("/WebServices/ScannerService", post(wsd::scanner_service))
            .route("/WebServices/ScannerService/", post(wsd::scanner_service));
    }
    let app = app.with_state(world);
    match ctx {
        Some(c) => app.layer(Extension(c)),
        None => app,
    }
}

fn with_trace(app: Router) -> Router {
    app.layer(
        TraceLayer::new_for_http().make_span_with(|req: &Request<_>| {
            let peer = req
                .extensions()
                .get::<ConnectInfo<SocketAddr>>()
                .map(|ConnectInfo(a)| a.to_string())
                .unwrap_or_else(|| "-".into());
            info_span!("http", method = %req.method(), uri = %req.uri(), peer = %peer)
        }),
    )
}

/// Single-device router (legacy paths only): `/health` plus [`device_router`].
pub fn router(world: Arc<RwLock<World>>, wsd_on: bool) -> Router {
    with_trace(
        Router::new()
            .route("/health", get(health))
            .merge(device_router(world, wsd_on)),
    )
}

/// The server router. Device-set routes (`/v1/devices`) are explicit; everything else goes to
/// [`dispatch`], which resolves the device per request: `/d/{id}/...` by id, any other path
/// (the legacy `/v1/*`, `/ScannerService`, `/WebServices/Device`) to the default device.
pub fn manager_router(manager: Arc<DeviceManager>) -> Router {
    with_trace(
        Router::new()
            .route("/health", get(health))
            .route("/ui", get(crate::ui::ui_page))
            .route("/ui/", get(crate::ui::ui_page))
            .route("/v1/profiles", get(crate::ui::list_profiles))
            .route("/v1/devices", get(list_devices).post(add_device))
            .route("/v1/devices/{id}", axum::routing::delete(remove_device))
            .route("/v1/devices/{id}/reconfigure", post(reconfigure_device))
            .route("/v1/devices/{id}/installed", post(installed_device))
            .route("/v1/devices/{id}/forget", post(forget_device))
            .route("/v1/registry", get(get_registry))
            .route("/v1/reconcile", post(reconcile))
            .fallback(dispatch)
            .with_state(manager),
    )
}

fn not_found(msg: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

async fn dispatch(
    State(manager): State<Arc<DeviceManager>>,
    mut req: Request<axum::body::Body>,
) -> axum::response::Response {
    use tower::ServiceExt;
    let path = req.uri().path().to_string();
    let (handle, inner_path) = if let Some(rest) = path.strip_prefix("/d/") {
        let (id, tail) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, "/"),
        };
        match manager.get(id).await {
            Some(h) => (h, tail.to_string()),
            None => return not_found("unknown device"),
        }
    } else {
        match manager.default_device().await {
            Some(h) => (h, path.clone()),
            None => return not_found("unknown device"),
        }
    };
    let pq = match req.uri().query() {
        Some(q) => format!("{inner_path}?{q}"),
        None => inner_path,
    };
    let mut parts = req.uri().clone().into_parts();
    match pq.parse() {
        Ok(p) => parts.path_and_query = Some(p),
        Err(_) => return not_found("bad path"),
    }
    match axum::http::Uri::from_parts(parts) {
        Ok(u) => *req.uri_mut() = u,
        Err(_) => return not_found("bad path"),
    }
    match handle.router.oneshot(req).await {
        Ok(r) => r,
        Err(e) => match e {},
    }
}

async fn list_devices(State(manager): State<Arc<DeviceManager>>) -> Json<Vec<DeviceInfo>> {
    Json(manager.list().await)
}

#[derive(Debug, Deserialize)]
struct AddDeviceBody {
    id: String,
    profile: String,
    device_uuid: Option<Uuid>,
    friendly_name: Option<String>,
    /// Sheets to load at creation and on reset; omitted = the daemon default / the profile's.
    initial_pages: Option<u32>,
}

fn loopback_denied(
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    what: &str,
) -> Option<axum::response::Response> {
    let ip = peer.map(|Extension(ConnectInfo(addr))| addr.ip());
    if ip.is_some_and(crate::ui::is_loopback_peer) {
        None
    } else {
        Some(
            (
                axum::http::StatusCode::FORBIDDEN,
                Json(serde_json::json!({ "error": format!("{what}: loopback peers only") })),
            )
                .into_response(),
        )
    }
}

async fn add_device(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<AddDeviceBody>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "add device") {
        return resp;
    }
    // No explicit UUID: reuse the one the registry already holds for this id (Windows knows it).
    let uuid = body.device_uuid.or_else(|| manager.stored_uuid(&body.id));
    let mut dc = match uuid {
        Some(u) => DeviceConfig::new(body.id, body.profile, u),
        None => DeviceConfig::derived(body.id, body.profile),
    };
    dc.friendly_name = body.friendly_name;
    dc.initial_pages = body.initial_pages;
    match manager.add(dc).await {
        Ok(info) => (axum::http::StatusCode::CREATED, Json(info)).into_response(),
        Err(e) => {
            let msg = e.to_string();
            let code = if msg.contains("duplicate") || msg.contains("already in use") {
                axum::http::StatusCode::CONFLICT
            } else {
                axum::http::StatusCode::BAD_REQUEST
            };
            (code, Json(serde_json::json!({ "error": msg }))).into_response()
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct RemoveQuery {
    /// Drop the registry row too (default: keep it and its install state).
    forget: Option<bool>,
}

async fn remove_device(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Query(q): Query<RemoveQuery>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "remove device") {
        return resp;
    }
    match manager.remove_row(&id, q.forget.unwrap_or(false)).await {
        Ok(()) => Json(serde_json::json!({ "removed": id })).into_response(),
        Err(_) => not_found("unknown device"),
    }
}

fn bad_request(msg: String) -> axum::response::Response {
    (
        axum::http::StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": msg })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ReconfigureBody {
    profile: String,
}

async fn reconfigure_device(
    State(manager): State<Arc<DeviceManager>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<ReconfigureBody>,
) -> axum::response::Response {
    if manager.get(&id).await.is_none() {
        return not_found("unknown device");
    }
    match manager.reconfigure(&id, &body.profile).await {
        Ok(info) => Json(info).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

#[derive(Debug, Deserialize)]
struct InstalledBody {
    #[serde(default)]
    wia_id: String,
    #[serde(default)]
    wia_name: String,
    #[serde(default)]
    wia_port: String,
}

async fn installed_device(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    Json(body): Json<InstalledBody>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "installed") {
        return resp;
    }
    if manager.get(&id).await.is_none() {
        return not_found("unknown device");
    }
    let wia = WiaInfo {
        id: body.wia_id,
        name: body.wia_name,
        port: body.wia_port,
    };
    match manager.mark_installed(&id, wia).await {
        Ok(()) => Json(device_json(&manager, &id).await).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

async fn forget_device(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "forget") {
        return resp;
    }
    if manager.get(&id).await.is_none() {
        return not_found("unknown device");
    }
    match manager.forget(&id).await {
        Ok(()) => Json(device_json(&manager, &id).await).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

async fn device_json(manager: &DeviceManager, id: &str) -> serde_json::Value {
    manager
        .list()
        .await
        .into_iter()
        .find(|d| d.id == id)
        .and_then(|d| serde_json::to_value(d).ok())
        .unwrap_or(serde_json::Value::Null)
}

async fn get_registry(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "registry") {
        return resp;
    }
    match manager.registry_doc() {
        Ok(r) => Json(r).into_response(),
        Err(e) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Debug, Deserialize)]
struct ReconcileBody {
    #[serde(default)]
    scanners: Vec<WiaInfo>,
}

async fn reconcile(
    State(manager): State<Arc<DeviceManager>>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    Json(body): Json<ReconcileBody>,
) -> axum::response::Response {
    if let Some(resp) = loopback_denied(peer, "reconcile") {
        return resp;
    }
    match manager.reconcile_with_windows(&body.scanners) {
        Ok(r) => Json(r).into_response(),
        Err(e) => bad_request(e.to_string()),
    }
}

// ── HTTP handlers ─────────────────────────────────────────────────────────────

async fn health() -> &'static str {
    "ok"
}

async fn status(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    // Timed / now fault-plan entries are evaluated on every poll.
    let mut w = world.write().await;
    plan_tick(&mut w);
    Json(w.status())
}

/// Evaluate timed plan entries and push any raised conditions to NotifyTo.
fn plan_tick(w: &mut World) {
    w.tick_fault_plan();
    let changes = w.drain_pending_condition_changes();
    if !changes.is_empty() {
        wsd::job_events::maybe_emit_condition_changes(w, wsd::soap::SCAN_NS_AIRSCAN, &changes);
        wsd::job_events::maybe_emit_status_summary(w, wsd::soap::SCAN_NS_AIRSCAN);
    }
}

async fn get_fault_plan(State(world): State<Arc<RwLock<World>>>) -> Json<serde_json::Value> {
    let mut w = world.write().await;
    plan_tick(&mut w);
    Json(serde_json::json!({ "fault_plan": w.fault_plan_view() }))
}

async fn post_fault_plan(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<crate::state::PlanBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.add_fault_plan(body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    plan_tick(&mut w);
    Ok(Json(w.status()))
}

async fn delete_fault_plan(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    let mut w = world.write().await;
    w.clear_fault_plan();
    Json(w.status())
}

#[derive(Debug, Deserialize)]
struct TraceQuery {
    /// Return events with `seq` greater than this. Omit for the whole ring.
    /// A string so `?after=5.0` and `?after=` do not 400 the probe.
    after: Option<String>,
}

fn query_after(after: Option<&str>) -> u64 {
    after.map(crate::state::parse_cursor).unwrap_or(0)
}

async fn trace(
    State(world): State<Arc<RwLock<World>>>,
    Query(query): Query<TraceQuery>,
) -> Json<crate::state::TraceView> {
    Json(
        world
            .read()
            .await
            .trace_since(query_after(query.after.as_deref())),
    )
}

async fn jobs(State(world): State<Arc<RwLock<World>>>) -> Json<crate::state::JobsView> {
    Json(world.read().await.jobs_view())
}

async fn activity_log(
    State(world): State<Arc<RwLock<World>>>,
    Query(query): Query<TraceQuery>,
) -> axum::response::Response {
    let text = world
        .read()
        .await
        .log_since(query_after(query.after.as_deref()));
    (
        axum::http::StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/plain; charset=utf-8",
        )],
        text,
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
struct ProfileBody {
    id: String,
}

async fn set_profile(
    State(world): State<Arc<RwLock<World>>>,
    ctx: Option<Extension<DeviceCtx>>,
    Json(body): Json<ProfileBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let ctx = ctx.map(|Extension(c)| c);
    crate::ctl::apply_profile(ctx.as_ref(), &world, &body.id)
        .await
        .map(Json)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))
}

async fn set_feeder(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<FeederBody>,
) -> Json<Status> {
    let mut w = world.write().await;
    w.set_feeder(body.ready);
    wsd::job_events::maybe_emit_elements_change(&w, wsd::soap::SCAN_NS_AIRSCAN, w.profile());
    Json(w.status())
}

async fn set_pages(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<PagesBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.apply_pages(&body).map_err(|e| {
        let code = if e.downcast_ref::<crate::state::PagesError>().is_some() {
            axum::http::StatusCode::CONFLICT
        } else {
            axum::http::StatusCode::BAD_REQUEST
        };
        (code, e.to_string())
    })?;
    wsd::job_events::maybe_emit_elements_change(&w, wsd::soap::SCAN_NS_AIRSCAN, w.profile());
    Ok(Json(w.status()))
}

async fn set_slow(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<SlowBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    crate::state::check_slow(&body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut w = world.write().await;
    w.apply_slow(&body);
    Ok(Json(w.status()))
}

async fn set_pattern(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<PatternBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.set_image_pattern_id(&body.id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    wsd::job_events::maybe_emit_elements_change(&w, wsd::soap::SCAN_NS_AIRSCAN, w.profile());
    Ok(Json(w.status()))
}

async fn set_jpeg_com(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<JpegComBody>,
) -> Json<Status> {
    let mut w = world.write().await;
    w.set_jpeg_com(body.enabled);
    Json(w.status())
}

async fn set_effects(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<EffectsBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.apply_effects(&body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    wsd::job_events::maybe_emit_elements_change(&w, wsd::soap::SCAN_NS_AIRSCAN, w.profile());
    Ok(Json(w.status()))
}

async fn list_scenarios() -> Json<&'static [scenarios::Scenario]> {
    Json(scenarios::PROGRESSION)
}

async fn set_scenario(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<ScenarioBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.apply_scenario(&body.id)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    wsd::job_events::maybe_emit_elements_change(&w, wsd::soap::SCAN_NS_AIRSCAN, w.profile());
    Ok(Json(w.status()))
}

async fn set_fault(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<FaultBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    crate::state::check_fault_delays(&body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    let mut w = world.write().await;
    let changes = w.apply_faults(&body);
    wsd::job_events::maybe_emit_condition_changes(&w, wsd::soap::SCAN_NS_AIRSCAN, &changes);
    if !changes.is_empty() {
        wsd::job_events::maybe_emit_status_summary(&w, wsd::soap::SCAN_NS_AIRSCAN);
    }
    Ok(Json(w.status()))
}

async fn get_loopback(State(world): State<Arc<RwLock<World>>>) -> Json<LoopbackStatus> {
    Json(world.read().await.loopback_status())
}

async fn set_loopback(
    State(world): State<Arc<RwLock<World>>>,
    Json(body): Json<LoopbackBody>,
) -> Result<Json<Status>, (axum::http::StatusCode, String)> {
    let mut w = world.write().await;
    w.set_loopback(&body)
        .map_err(|e| (axum::http::StatusCode::BAD_REQUEST, e.to_string()))?;
    Ok(Json(w.status()))
}

async fn clear_loopback(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    let mut w = world.write().await;
    w.clear_loopback();
    Json(w.status())
}

async fn reset(State(world): State<Arc<RwLock<World>>>) -> Json<Status> {
    let mut w = world.write().await;
    w.reset();
    Json(w.status())
}

async fn shutdown_signal(
    mut exit_rx: tokio::sync::watch::Receiver<bool>,
    stop: Option<oneshot::Receiver<()>>,
    ctrl_c: bool,
) {
    let ctrl = async {
        if ctrl_c {
            let _ = tokio::signal::ctrl_c().await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    let external = async {
        match stop {
            // Only an actual `send(())` is a stop request. A dropped sender (e.g. the tray never
            // started, so nothing holds it) must not read as one, or the server exits at once.
            Some(rx) => {
                if rx.await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        _ = ctrl => {}
        _ = external => {}
        result = exit_rx.changed() => {
            let _ = result;
        }
    }
    info!("shutting down");
}

/// `GET /v1/tickets?since=SEQ&op=ValidateScanTicket\|CreateScanJob&limit=N`.
async fn get_tickets(
    State(world): State<Arc<RwLock<World>>>,
    Query(query): Query<crate::tickets::TicketsQuery>,
) -> Json<crate::tickets::TicketsView> {
    Json(query.run(world.read().await.tickets()))
}

/// `DELETE /v1/tickets` empties the ring (`POST /v1/reset` does too).
async fn delete_tickets(
    State(world): State<Arc<RwLock<World>>>,
) -> Json<crate::tickets::TicketsView> {
    let w = world.read().await;
    w.tickets().clear();
    Json(w.tickets().view(0, None, None))
}
