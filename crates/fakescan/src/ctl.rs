//! Unified `GET /v1/ctl` (caps + identity) and `POST /v1/ctl` (command dispatch).
//!
//! The same [`dispatch`] function is shared by the HTTP handler (`main.rs`) and
//! the Windows named-pipe server ([`crate::pipe`]), so both surfaces accept
//! identical JSONL.
//!
//! **Registry-level commands** (list, add, remove, ensure, autostart) live in
//! `scripts/fakescan.ps1` and `fakescan-ctl`, not inside the device process.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::effects::EffectsBody;
use crate::manager::DeviceCtx;
use crate::registry::WiaInfo;
use crate::state::{FaultBody, LoopbackBody, PagesBody, World};

/// Commands that shut the process down, write the registry, or change install state.
///
/// The control API shares the scan-service port, which listens on every interface by default.
/// These commands are accepted from a loopback HTTP peer only. The named pipe calls
/// [`dispatch`] directly and stays a local surface. Scan knobs (`pages`, `fault`, …) stay
/// reachable from the LAN so a lab client can arm a device it did not start.
const LOOPBACK_CMDS: &[&str] = &[
    "shutdown",
    "save",
    "installed",
    "mark_added",
    "forget",
    "reconcile",
];

/// True when `cmd` must come from a loopback HTTP peer.
pub fn host_cmd_is_loopback_only(cmd: &str) -> bool {
    LOOPBACK_CMDS.contains(&cmd)
}

/// Every cmd accepted by `POST /v1/ctl`.
pub const CTL_CMDS: &[&str] = &[
    "status",
    "health",
    "jobs",
    "activity",
    "tickets",
    "profile",
    "pattern",
    "pages",
    "feeder",
    "jpeg_com",
    "effects",
    "scenario",
    "fault",
    "fault_plan",
    "slow",
    "loopback",
    "push_scan",
    "reset",
    "save",
    "installed",
    "mark_added",
    "forget",
    "reconcile",
    "shutdown",
    "open_settings",
];

/// Body for `POST /v1/ctl`.
#[derive(Debug, Deserialize)]
pub struct CtlRequest {
    pub cmd: String,
    #[serde(default)]
    pub args: Value,
}

/// Response body for `GET /v1/ctl` — capabilities + running identity.
#[derive(Debug, Serialize)]
pub struct CtlInfo {
    pub cmds: &'static [&'static str],
    pub port: u16,
    pub uuid: String,
    pub base_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `GET /v1/ctl` — return available commands and this instance's identity.
pub async fn get_ctl(State(world): State<Arc<RwLock<World>>>) -> Json<CtlInfo> {
    let w = world.read().await;
    Json(CtlInfo {
        cmds: CTL_CMDS,
        port: w.lab_port(),
        uuid: w.device_uuid().to_string(),
        base_url: w.lab_base_url(),
        name: w.lab_name().map(str::to_owned),
    })
}

/// `POST /v1/ctl` — dispatch a named command (JSON body: `{cmd, args}`).
pub async fn post_ctl(
    State(world): State<Arc<RwLock<World>>>,
    ctx: Option<axum::Extension<DeviceCtx>>,
    peer: Option<axum::Extension<axum::extract::ConnectInfo<std::net::SocketAddr>>>,
    Json(raw): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    let ctx = ctx.map(|axum::Extension(c)| c);
    let req: CtlRequest = serde_json::from_value(raw.clone())
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    if host_cmd_is_loopback_only(&req.cmd) {
        let ip = peer
            .as_ref()
            .map(|axum::Extension(axum::extract::ConnectInfo(a))| a.ip());
        if !ip.is_some_and(crate::ui::is_loopback_peer) {
            return Err((
                StatusCode::FORBIDDEN,
                format!("{}: loopback peers only", req.cmd),
            ));
        }
    }
    if req.cmd == "open_settings" {
        // `page` may sit at the top level (`{cmd, page}`) or in `args`.
        let args = match raw.get("page") {
            Some(p) => json!({ "page": p }),
            None => req.args.clone(),
        };
        // Needs the peer address, which the shared (HTTP + pipe) dispatch does not have.
        let peer_ip = peer.map(|axum::Extension(axum::extract::ConnectInfo(a))| a.ip());
        return open_settings(ctx.as_ref(), peer_ip, &args)
            .await
            .map(Json)
            .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()));
    }
    dispatch_ctx(ctx.as_ref(), &world, req)
        .await
        .map(Json)
        .map_err(|e| (StatusCode::BAD_REQUEST, e.to_string()))
}

/// `open_settings {page:"printers"}`: loopback peers only, `printers` only, launched through the
/// manager's injectable [`crate::manager::SettingsLauncher`]. Returns `{"opened": bool}`.
async fn open_settings(
    ctx: Option<&DeviceCtx>,
    peer: Option<std::net::IpAddr>,
    args: &Value,
) -> anyhow::Result<Value> {
    let page = args.get("page").and_then(Value::as_str).unwrap_or("");
    crate::ui::check_open_settings(peer, page).map_err(|e| anyhow::anyhow!(e))?;
    let m = ctx
        .and_then(|c| c.manager.upgrade())
        .ok_or_else(|| anyhow::anyhow!("open_settings: needs the device manager"))?;
    let launcher = m.settings_launcher();
    let page = page.to_string();
    let opened = tokio::task::spawn_blocking(move || launcher.open(&page))
        .await
        .map_err(|e| anyhow::anyhow!("open_settings: {e}"))??;
    Ok(json!({ "opened": opened }))
}

/// Core dispatch — shared by the HTTP handler and the Windows named-pipe server.
///
/// Returns the JSON response value on success, or an error message.
pub async fn dispatch(world: &Arc<RwLock<World>>, req: CtlRequest) -> anyhow::Result<Value> {
    dispatch_ctx(None, world, req).await
}

/// Switch a device's profile. With a manager context this goes through
/// [`crate::manager::DeviceManager::reconfigure`], so the registry row's `config` and install state
/// stay correct (an installed row goes `stale` only if the PUBLISHED fingerprint changed). Without
/// one (bare `router()` / pipe without a manager) the world is updated directly, as before.
/// Must be called without holding the world lock.
pub async fn apply_profile(
    ctx: Option<&DeviceCtx>,
    world: &Arc<RwLock<World>>,
    profile_id: &str,
) -> anyhow::Result<crate::state::Status> {
    if let Some(m) = ctx.and_then(|c| c.manager.upgrade().map(|m| (m, c.id.clone()))) {
        let (m, id) = m;
        m.reconfigure(&id, profile_id).await?;
        return Ok(world.read().await.status());
    }
    let mut w = world.write().await;
    w.load_profile(profile_id)?;
    crate::wsd::job_events::maybe_emit_elements_change(
        &w,
        crate::wsd::soap::SCAN_NS_AIRSCAN,
        w.profile(),
    );
    Ok(w.status())
}

/// Like [`dispatch`], with the manager context so `installed`, `mark_added`, `forget`,
/// `reconcile` and `save` can reach the registry. Without a context those commands error
/// (`save` falls back to the registry file).
pub async fn dispatch_ctx(
    ctx: Option<&DeviceCtx>,
    world: &Arc<RwLock<World>>,
    req: CtlRequest,
) -> anyhow::Result<Value> {
    match req.cmd.as_str() {
        "installed" | "mark_added" | "forget" | "reconcile" => manager_cmd(ctx, &req).await,

        "open_settings" => anyhow::bail!(
            "open_settings is only accepted over HTTP from a loopback peer (POST /v1/ctl)"
        ),

        "status" | "health" => {
            let mut w = world.write().await;
            w.sweep_timed_out_jobs();
            Ok(serde_json::to_value(w.status())?)
        }

        "jobs" => {
            let w = world.read().await;
            Ok(serde_json::to_value(w.jobs_view())?)
        }

        "tickets" => {
            // args: {since, op, limit, clear}. `clear: true` empties the ring after reading.
            let arg = |k: &str| {
                req.args.get(k).and_then(|v| match v {
                    Value::String(s) => Some(s.clone()),
                    Value::Number(n) => Some(n.to_string()),
                    _ => None,
                })
            };
            let q = crate::tickets::TicketsQuery {
                since: arg("since"),
                op: arg("op"),
                limit: arg("limit"),
            };
            let w = world.read().await;
            let view = q.run(w.tickets());
            if req.args.get("clear").and_then(Value::as_bool) == Some(true) {
                w.tickets().clear();
            }
            Ok(serde_json::to_value(view)?)
        }

        "activity" => {
            let after = args_after(&req.args);
            let w = world.read().await;
            Ok(serde_json::to_value(w.trace_since(after))?)
        }

        "profile" => {
            let id = req.args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("profile: missing args.id (string)"))?
                .to_owned();
            Ok(serde_json::to_value(apply_profile(ctx, world, &id).await?)?)
        }

        "pattern" => {
            let id = req.args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("pattern: missing args.id (string)"))?
                .to_owned();
            let mut w = world.write().await;
            w.set_image_pattern_id(&id)?;
            crate::wsd::job_events::maybe_emit_elements_change(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                w.profile(),
            );
            Ok(serde_json::to_value(w.status())?)
        }

        "pages" => {
            let body: PagesBody =
                serde_json::from_value(req.args).map_err(|e| anyhow::anyhow!("pages: {e}"))?;
            let mut w = world.write().await;
            w.apply_pages(&body)?;
            crate::wsd::job_events::maybe_emit_elements_change(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                w.profile(),
            );
            Ok(serde_json::to_value(w.status())?)
        }

        "feeder" => {
            let ready = req.args["ready"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("feeder: missing args.ready (bool)"))?;
            let mut w = world.write().await;
            w.set_feeder(ready);
            crate::wsd::job_events::maybe_emit_elements_change(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                w.profile(),
            );
            Ok(serde_json::to_value(w.status())?)
        }

        "jpeg_com" => {
            let enabled = req.args["enabled"]
                .as_bool()
                .ok_or_else(|| anyhow::anyhow!("jpeg_com: missing args.enabled (bool)"))?;
            let mut w = world.write().await;
            w.set_jpeg_com(enabled);
            Ok(serde_json::to_value(w.status())?)
        }

        "effects" => {
            let body: EffectsBody =
                serde_json::from_value(req.args).map_err(|e| anyhow::anyhow!("effects: {e}"))?;
            let mut w = world.write().await;
            w.apply_effects(&body)?;
            crate::wsd::job_events::maybe_emit_elements_change(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                w.profile(),
            );
            Ok(serde_json::to_value(w.status())?)
        }

        "scenario" => {
            let id = req.args["id"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("scenario: missing args.id (string)"))?
                .to_owned();
            let mut w = world.write().await;
            w.apply_scenario(&id)?;
            crate::wsd::job_events::maybe_emit_elements_change(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                w.profile(),
            );
            Ok(serde_json::to_value(w.status())?)
        }

        "fault" => {
            // A missing `args` is an empty object (no flags change). A present but unusable
            // value is an error — `unwrap_or_default` used to report success and change nothing.
            let args = if req.args.is_null() {
                serde_json::json!({})
            } else {
                req.args
            };
            let body: FaultBody =
                serde_json::from_value(args).map_err(|e| anyhow::anyhow!("fault: {e}"))?;
            crate::state::check_fault_delays(&body)?;
            let mut w = world.write().await;
            let changes = w.apply_faults(&body);
            // Best-effort NotifyTo for raised/cleared ScannerStatusCondition*.
            crate::wsd::job_events::maybe_emit_condition_changes(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                &changes,
            );
            if !changes.is_empty() {
                crate::wsd::job_events::maybe_emit_status_summary(
                    &w,
                    crate::wsd::soap::SCAN_NS_AIRSCAN,
                );
            }
            Ok(serde_json::to_value(w.status())?)
        }

        "fault_plan" => {
            let mut w = world.write().await;
            if req
                .args
                .get("clear")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                w.clear_fault_plan();
            }
            let has_entries = req.args.get("faults").is_some() || req.args.get("kind").is_some();
            if has_entries {
                let body: crate::state::PlanBody = serde_json::from_value(req.args.clone())
                    .map_err(|e| anyhow::anyhow!("fault_plan: {e}"))?;
                w.add_fault_plan(body)?;
            }
            w.tick_fault_plan();
            let changes = w.drain_pending_condition_changes();
            crate::wsd::job_events::maybe_emit_condition_changes(
                &w,
                crate::wsd::soap::SCAN_NS_AIRSCAN,
                &changes,
            );
            if !changes.is_empty() {
                crate::wsd::job_events::maybe_emit_status_summary(
                    &w,
                    crate::wsd::soap::SCAN_NS_AIRSCAN,
                );
            }
            Ok(serde_json::to_value(w.status())?)
        }

        "slow" => {
            let body: crate::state::SlowBody = serde_json::from_value(req.args.clone())
                .map_err(|e| anyhow::anyhow!("slow: {e}"))?;
            if req.args.get("ms").is_none() && req.args.get("bps").is_none() {
                anyhow::bail!("slow: need args.ms and/or args.bps");
            }
            crate::state::check_slow(&body)?;
            let mut w = world.write().await;
            w.apply_slow(&body);
            Ok(serde_json::to_value(w.status())?)
        }

        "loopback" => {
            let body: LoopbackBody =
                serde_json::from_value(req.args).map_err(|e| anyhow::anyhow!("loopback: {e}"))?;
            let mut w = world.write().await;
            w.set_loopback(&body)?;
            Ok(serde_json::to_value(w.status())?)
        }

        "push_scan" => {
            // Lab stand-in for ScanAvailableEvent: mint ScanIdentifier for a registered
            // DestinationToken (from Subscribe). Optional args.destination_token.
            // When Subscribe stored a NotifyTo URL, POST the event best-effort (ctl
            // still returns the ids if the sink is down).
            let token = req
                .args
                .get("destination_token")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);
            let pending = {
                let mut w = world.write().await;
                w.mint_push_scan(token.as_deref())?
            };
            if let Some(ref url) = pending.notify_to {
                let subscriber = world.read().await.notify_peer();
                let _ = crate::wsd::notify::deliver_scan_available(
                    url,
                    subscriber,
                    crate::wsd::notify::SCAN_AVAILABLE_NS,
                    &pending.scan_identifier,
                    &pending.client_context,
                )
                .await;
            }
            Ok(serde_json::to_value(pending)?)
        }

        "reset" => {
            let mut w = world.write().await;
            w.reset();
            Ok(serde_json::to_value(w.status())?)
        }

        "save" => {
            // Write the running state to the devices.json row for this lab_name.
            let (lab_name, lab_listen, uuid_str, profile_id) = {
                let w = world.read().await;
                (
                    w.lab_name().map(str::to_owned),
                    w.lab_listen(),
                    w.device_uuid().to_string(),
                    w.profile().id.clone(),
                )
            };
            let name = lab_name.ok_or_else(|| {
                anyhow::anyhow!(
                    "save: no --lab-name set; cannot identify registry row.\n\
                     Start with --lab-name default (or FAKESCAN_NAME=default)."
                )
            })?;
            let listen_str = lab_listen
                .map(|a| a.to_string())
                .unwrap_or_else(|| format!("0.0.0.0:{}", crate::lab::LAB_PORT));

            use crate::registry::{registry_path, DeviceRow, RegistryStore};
            let store = match ctx.and_then(|c| c.manager.upgrade()) {
                Some(m) => Arc::clone(m.store()),
                None => RegistryStore::file(registry_path()),
            };
            store.update(|reg| {
                if let Some(row) = reg.find_mut(&name) {
                    row.profile = profile_id.clone();
                    row.listen = listen_str.clone();
                    row.device_uuid = uuid_str.clone();
                } else {
                    reg.devices.push(DeviceRow::new(
                        name.clone(),
                        listen_str.clone(),
                        uuid_str.clone(),
                        profile_id.clone(),
                    ));
                }
            })?;
            Ok(json!({
                "saved": true,
                "name": name,
                "listen": listen_str,
                "device_uuid": uuid_str,
                "profile": profile_id,
                "path": store.path().map(|p| p.display().to_string()),
            }))
        }

        "shutdown" => {
            // A stray POST must not take the scanner offline. `yes` is required.
            let yes = req
                .args
                .get("yes")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !yes {
                anyhow::bail!("shutdown needs yes: true");
            }
            world.read().await.request_exit()?;
            Ok(json!({ "shutting_down": true }))
        }

        other => anyhow::bail!(
            "unknown ctl cmd {other:?}; see GET /v1/ctl for the list of supported commands"
        ),
    }
}

/// Manager-level commands: thin wrappers over [`crate::manager::DeviceManager`].
async fn manager_cmd(ctx: Option<&DeviceCtx>, req: &CtlRequest) -> anyhow::Result<Value> {
    let cmd = req.cmd.as_str();
    let ctx = ctx.ok_or_else(|| {
        anyhow::anyhow!("{cmd}: needs the device manager; use POST /v1/ctl on a running server")
    })?;
    let m = ctx
        .manager
        .upgrade()
        .ok_or_else(|| anyhow::anyhow!("{cmd}: device manager is shutting down"))?;
    let device = req
        .args
        .get("device")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| ctx.id.clone());
    match cmd {
        "installed" => {
            let s = |k: &str| {
                req.args
                    .get(k)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string()
            };
            let wia = WiaInfo {
                id: s("wia_id"),
                name: s("wia_name"),
                port: s("wia_port"),
            };
            m.mark_installed(&device, wia).await?;
            Ok(device_json(&m, &device).await?)
        }
        "mark_added" => {
            m.mark_installed_legacy(&device).await?;
            Ok(device_json(&m, &device).await?)
        }
        "forget" => {
            m.forget(&device).await?;
            Ok(device_json(&m, &device).await?)
        }
        "reconcile" => {
            let scanners: Vec<WiaInfo> = serde_json::from_value(
                req.args
                    .get("scanners")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )
            .map_err(|e| anyhow::anyhow!("reconcile: args.scanners: {e}"))?;
            Ok(serde_json::to_value(m.reconcile_with_windows(&scanners)?)?)
        }
        _ => unreachable!("manager_cmd called for {cmd}"),
    }
}

async fn device_json(m: &crate::manager::DeviceManager, id: &str) -> anyhow::Result<Value> {
    match m.list().await.into_iter().find(|d| d.id == id) {
        Some(d) => Ok(serde_json::to_value(d)?),
        None => Ok(Value::Null),
    }
}

/// `args.after` from a probe: a JSON number, a decimal string, or omitted.
///
/// `as_u64` alone drops `"12"` and `12.0` (PowerShell's JSON numbers are
/// doubles). That cursor has to be honored, not turned into 0 or a 400.
fn args_after(args: &Value) -> u64 {
    let Some(value) = args.get("after") else {
        return 0;
    };
    if let Some(n) = value.as_u64() {
        return n;
    }
    if let Some(n) = value.as_i64() {
        return if n < 0 { 0 } else { n as u64 };
    }
    if let Some(n) = value.as_f64() {
        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64 {
            return n as u64;
        }
        return 0;
    }
    value.as_str().map(crate::state::parse_cursor).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tokio::sync::RwLock;

    fn test_world() -> Arc<RwLock<World>> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = crate::profile::Profile::load(&dir, "brother-wsd").expect("profile");
        Arc::new(RwLock::new(World::new(dir, profile)))
    }

    #[tokio::test]
    async fn ctl_shutdown_waits_for_yes_then_signals() {
        let w = test_world();
        let (tx, mut rx) = tokio::sync::watch::channel(false);
        w.write().await.arm_exit(tx);
        let missing = dispatch(
            &w,
            CtlRequest {
                cmd: "shutdown".into(),
                args: json!({}),
            },
        )
        .await
        .expect_err("yes is required");
        assert!(missing.to_string().contains("yes"), "{missing}");
        assert!(!*rx.borrow());
        let v = dispatch(
            &w,
            CtlRequest {
                cmd: "shutdown".into(),
                args: json!({ "yes": true }),
            },
        )
        .await
        .expect("shutdown");
        assert_eq!(v["shutting_down"], true);
        rx.changed().await.expect("exit signal");
        assert!(*rx.borrow());
    }

    #[tokio::test]
    async fn ctl_status_returns_profile_id() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "status".into(),
            args: Value::Null,
        };
        let v = dispatch(&w, req).await.unwrap();
        assert!(
            v["profile_id"].as_str().is_some(),
            "profile_id missing: {v}"
        );
    }

    #[tokio::test]
    async fn ctl_health_returns_status() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "health".into(),
            args: Value::Null,
        };
        let v = dispatch(&w, req).await.unwrap();
        assert!(v.is_object());
    }

    #[tokio::test]
    async fn ctl_reset_clears_pages() {
        let w = test_world();
        {
            let mut g = w.write().await;
            g.set_pages(5);
        }
        let req = CtlRequest {
            cmd: "reset".into(),
            args: Value::Null,
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["pages_remaining"], 0, "reset should clear pages");
    }

    #[tokio::test]
    async fn ctl_pages_sets_count() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "pages".into(),
            args: json!({ "count": 3 }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["pages_remaining"], 3);
    }

    #[tokio::test]
    async fn ctl_feeder_toggles() {
        let w = test_world();
        // brother-wsd has sticky_ready=true, feeder starts ready.
        let req = CtlRequest {
            cmd: "feeder".into(),
            args: json!({ "ready": false }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["feeder_ready"], false);
    }

    #[tokio::test]
    async fn ctl_pattern_changes_image_pattern() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "pattern".into(),
            args: json!({ "id": "wedges" }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["image_pattern"], "wedges");
    }

    #[tokio::test]
    async fn ctl_fault_sets_jam() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "fault".into(),
            args: json!({ "jam": true }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["jam"], true);
    }

    #[tokio::test]
    async fn ctl_fault_empty_feeder_brother_round_trip() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "fault".into(),
            args: json!({ "empty_feeder_brother": true, "empty_feeder_delay_ms": 7 }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["empty_feeder_brother"], true);
        assert_eq!(v["empty_feeder_delay_ms"], 7);
        let v = dispatch(
            &w,
            CtlRequest {
                cmd: "reset".into(),
                args: json!({}),
            },
        )
        .await
        .unwrap();
        assert_eq!(v["empty_feeder_brother"], false);
    }

    #[tokio::test]
    async fn ctl_slow_sets_ms() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "slow".into(),
            args: json!({ "ms": 100 }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["slow_retrieve_ms"], 100);
    }

    #[tokio::test]
    async fn ctl_slow_sets_bps() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "slow".into(),
            args: json!({ "bps": 50000 }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["retrieve_bps"], 50000);
        assert_eq!(v["slow_retrieve_ms"], 0);
    }

    #[tokio::test]
    async fn ctl_jpeg_com_toggle() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "jpeg_com".into(),
            args: json!({ "enabled": false }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["jpeg_com"], false);
    }

    #[tokio::test]
    async fn ctl_scenario_applies() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "scenario".into(),
            args: json!({ "id": "order" }),
        };
        let v = dispatch(&w, req).await.unwrap();
        assert_eq!(v["scenario"], "order");
        assert_eq!(v["pages_remaining"], 3);
    }

    #[tokio::test]
    async fn ctl_unknown_cmd_errors() {
        let w = test_world();
        let req = CtlRequest {
            cmd: "doesnotexist".into(),
            args: Value::Null,
        };
        let err = dispatch(&w, req).await.unwrap_err();
        assert!(err.to_string().contains("unknown ctl cmd"));
    }

    #[tokio::test]
    async fn ctl_jobs_and_activity_shape() {
        let w = test_world();
        let jobs = dispatch(
            &w,
            CtlRequest {
                cmd: "jobs".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap();
        assert_eq!(jobs["jobs"].as_array().map(|a| a.len()), Some(0), "{jobs}");

        let activity = dispatch(
            &w,
            CtlRequest {
                cmd: "activity".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap();
        assert!(activity["seq"].as_u64().is_some(), "{activity}");
        assert!(
            activity["events"].as_array().unwrap().is_empty(),
            "{activity}"
        );

        {
            let mut g = w.write().await;
            g.note("wsd", "Get", "ok", "soap {peer}");
        }
        let seq = w.read().await.trace_seq();
        let text = w.read().await.log_since(0);
        assert!(text.contains("soap {peer}"), "{text}");

        let string_after = dispatch(
            &w,
            CtlRequest {
                cmd: "activity".into(),
                args: json!({ "after": seq.to_string() }),
            },
        )
        .await
        .expect("string after must not reject the probe");
        assert!(string_after["seq"].as_u64().is_some(), "{string_after}");
        assert!(
            string_after["events"].as_array().unwrap().is_empty(),
            "{string_after}"
        );

        let float_after = Value::Object({
            let mut map = serde_json::Map::new();
            map.insert(
                "after".into(),
                Value::Number(serde_json::Number::from_f64(seq as f64).unwrap()),
            );
            map
        });
        let float_view = dispatch(
            &w,
            CtlRequest {
                cmd: "activity".into(),
                args: float_after,
            },
        )
        .await
        .expect("decimal after must not reject the probe");
        assert!(float_view["seq"].as_u64().is_some(), "{float_view}");
        assert!(float_view["events"].is_array(), "{float_view}");
        assert!(
            float_view["events"].as_array().unwrap().is_empty(),
            "{float_view}"
        );

        let token = {
            let mut g = w.write().await;
            g.create_job(
                r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#,
            )
            .unwrap()
            .token
        };
        let jobs = dispatch(
            &w,
            CtlRequest {
                cmd: "jobs".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap();
        assert!(jobs["jobs"][0].get("token").is_none(), "{jobs}");
        assert!(!jobs.to_string().contains(&token), "{jobs}");

        dispatch(
            &w,
            CtlRequest {
                cmd: "reset".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap();
        let activity = dispatch(
            &w,
            CtlRequest {
                cmd: "activity".into(),
                args: json!({ "after": 0 }),
            },
        )
        .await
        .unwrap();
        let actions: Vec<_> = activity["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|event| event["action"].as_str().unwrap())
            .collect();
        assert!(
            actions.contains(&"Get"),
            "reset wiped the trace: {actions:?}"
        );
        assert!(actions.contains(&"reset"), "{actions:?}");
    }

    #[tokio::test]
    async fn ctl_save_errors_without_lab_name() {
        let w = test_world();
        // World has no lab_name by default → save should fail gracefully.
        let req = CtlRequest {
            cmd: "save".into(),
            args: Value::Null,
        };
        let err = dispatch(&w, req).await.unwrap_err();
        assert!(
            err.to_string().contains("lab-name"),
            "expected lab-name error: {err}"
        );
    }

    #[tokio::test]
    async fn ctl_fault_rejects_a_mistyped_flag() {
        let w = test_world();
        let err = dispatch(
            &w,
            CtlRequest {
                cmd: "fault".into(),
                args: json!({ "jam": "yes" }),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("fault:"), "{err}");
        assert!(!w.read().await.status().jam);
        // No args is still a no-op success.
        dispatch(
            &w,
            CtlRequest {
                cmd: "fault".into(),
                args: Value::Null,
            },
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn ctl_slow_rejects_an_unbounded_delay() {
        let w = test_world();
        let err = dispatch(
            &w,
            CtlRequest {
                cmd: "slow".into(),
                args: json!({ "ms": crate::state::MAX_CONTROL_DELAY_MS + 1 }),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("exceeds"), "{err}");
        assert_eq!(w.read().await.slow_retrieve_ms(), 0);
    }
}
