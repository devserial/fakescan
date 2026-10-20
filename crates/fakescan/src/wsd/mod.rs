//! WSD Scan Service HTTP endpoint (SOAP + MTOM RetrieveImage).

mod elements;
mod envelope;
pub mod job_events;
pub mod notify;
mod responses;
mod scan_schema;
pub mod soap;
pub mod status_matrix;
pub(crate) mod xml;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::state::World;
use crate::wsd::responses::jfif_wire_color;
use crate::wsd::xml::Element;

/// See `elements::published_capability_xml`.
pub fn published_capability_xml(profile: &crate::profile::Profile) -> String {
    elements::published_capability_xml(profile)
}

pub async fn scanner_service(
    State(world): State<Arc<RwLock<World>>>,
    // `Option<ConnectInfo<_>>` does not implement the axum 0.8 extractor.
    // `Extension<ConnectInfo<_>>` does, and the connect-info service inserts that.
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    ctx: Option<Extension<crate::manager::DeviceCtx>>,
    uri: Uri,
    body: Bytes,
) -> Response {
    let started = Instant::now();
    // Record the peer's first/last contact in the device registry (throttled, off-thread).
    if let (Some(Extension(ConnectInfo(addr))), Some(Extension(ctx))) = (&peer, &ctx) {
        ctx.probe(addr.ip());
    }
    let peer = peer
        .map(|Extension(ConnectInfo(addr))| addr.to_string())
        .unwrap_or_else(|| "-".into());
    let path = uri.path();
    let text = String::from_utf8_lossy(&body);
    let req = soap::Request::parse(&text);
    let action = &req.action;
    let short = &req.short_action;
    let relates = req.relates.as_deref();
    if let Some(finding) = &req.rejected {
        warn!(field = finding.field, "WSD header rejected");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
            soap::fault_with_code(Some("soap:MustUnderstand"), finding.note, relates),
        )
            .into_response();
    }
    let scan_ns = req.scan_ns;
    info!(%path, %short, %peer, scan_ns, bytes = body.len(), "WSD request");
    debug!(%path, %action, "WSD Action URI");

    let seq_before = world.read().await.trace_seq();
    let device_id = match &ctx {
        Some(Extension(c)) => c.id.clone(),
        None => world
            .read()
            .await
            .lab_name()
            .map(str::to_owned)
            .unwrap_or_else(|| "default".into()),
    };
    let result = dispatch(&world, &req, &text, &peer, &device_id).await;
    let ms = started.elapsed().as_millis();
    let (outcome, detail) = match &result {
        Ok(SoapOut::Xml(_)) => ("ok".to_string(), "soap".to_string()),
        Ok(SoapOut::Mtom {
            image,
            abort_after: Some(_),
            ..
        }) => (
            "abort".to_string(),
            format!("mtom abort bytes={}", image.len()),
        ),
        Ok(SoapOut::Mtom {
            image,
            cut: Some(_),
            ..
        }) => (
            "abort".to_string(),
            format!("mtom cut mid page bytes={}", image.len()),
        ),
        Ok(SoapOut::Mtom { image, .. }) => {
            ("ok".to_string(), format!("mtom bytes={}", image.len()))
        }
        Err(FaultOut { reason, .. }) => ("fault".to_string(), reason.clone()),
    };
    {
        let mut w = world.write().await;
        if !w.absorb_call(seq_before, short, &outcome, &peer, ms) {
            w.note_wsd("wsd", &peer, short, &outcome, ms, &detail);
        }
    }
    match result {
        Ok(SoapOut::Xml(xml)) => {
            info!(%path, %short, ms, out_bytes = xml.len(), kind = "soap", "WSD ok");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
                xml,
            )
                .into_response()
        }
        Ok(SoapOut::Mtom {
            soap_xml,
            image,
            content_type,
            retrieve_bps,
            abort_after,
            cut,
            live,
        }) => {
            info!(
                %path,
                %short,
                ms,
                image_bytes = image.len(),
                %content_type,
                retrieve_bps,
                ?abort_after,
                kind = "mtom",
                "WSD ok"
            );
            mtom_response(
                &soap_xml,
                &image,
                &content_type,
                retrieve_bps,
                abort_after,
                cut,
                live,
            )
        }
        Err(FaultOut { subcode, reason }) => {
            warn!(%path, %short, ms, %reason, "WSD fault");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "application/soap+xml; charset=utf-8")],
                soap::fault_with_code(subcode.as_deref(), &reason, relates),
            )
                .into_response()
        }
    }
}

#[derive(Debug)]
enum SoapOut {
    Xml(String),
    Mtom {
        soap_xml: String,
        image: Vec<u8>,
        content_type: String,
        /// Body send throttle (bytes/sec). `0` = one-shot.
        retrieve_bps: u64,
        /// When set, send this many body bytes then error the stream (mid-MTOM abort).
        abort_after: Option<usize>,
        /// Fault-plan mid-page cut (wins over `abort_after`).
        cut: Option<crate::state::Cut>,
        /// Live-fault / timed-deadline handle checked between stream chunks.
        live: Option<crate::state::LiveCut>,
    },
}

/// All XSD-authorized fault subcodes + common real-driver extensions that
/// drivers like Brother and Canon emit during actual scanning.
#[allow(dead_code)] // reference list of the fault subcodes; not consulted by the server yet
const ALL_FAULT_SUBCODES: &[&str] = &[
    "wscn:InvalidArgs",
    "wscn:OperationFailed",
    "wscn:ServerErrorTemporaryError",
    "wscn:ServerErrorInternalError",
    "wscn:ServerErrorNotAcceptingJobs",
    "wscn:ClientErrorJobCancelled",
    "wscn:ClientErrorJobIdNotFound",
    "wscn:ClientErrorFormatNotSupported",
    "wscn:ClientErrorInvalidScanIdentifier",
    "wscn:ClientErrorInvalidDestinationToken",
    "wscn:ClientErrorNoImagesAvailable",
    "wscn:ClientErrorInvalidJobToken",
    "wscn:ClientErrorConflictingRequiredParameters",
    // Common real-driver extensions (not in WDPScan.xsd but emitted by Brother/Canon):
    "wscn:ClientErrorMediaJam",
    "wscn:ClientErrorCoverOpen",
    "wscn:ClientErrorMultipleFeed",
    "wscn:ClientErrorInterlockOpen",
    "wscn:ClientErrorInternalStorageFull",
    // WS-Eventing fault codes used by Windows WSDScan:
    "wse:UnableToRenew",
    "wse:InvalidMessage",
];

fn final_params_json(color: &str, bytes_per_line: u32, format: &str) -> serde_json::Value {
    serde_json::json!({
        "color_processing": color,
        "bits": crate::tickets::bits_for_token(color),
        "bytes_per_line": bytes_per_line,
        "format": format,
    })
}

/// Record a faulted/refused ticket request: outcome `<prefix>:<subcode>`.
fn record_ticket(
    w: &World,
    device_id: &str,
    op: &str,
    body: &str,
    f: &FaultOut,
    prefix: &str,
    final_p: Option<serde_json::Value>,
) {
    w.tickets().record(
        device_id,
        op,
        format!("{prefix}:{}", f.subcode.as_deref().unwrap_or("none")),
        Some(f.reason.clone()),
        crate::state::ticket_parsed(body),
        final_p,
        body,
    );
}

#[derive(Debug)]
struct FaultOut {
    subcode: Option<String>,
    reason: String,
}

/// `wscn:Override` / `wscn:UsedDefault` attribute fragment for FinalParameters.
impl From<String> for FaultOut {
    fn from(reason: String) -> Self {
        // Format refusal: XSD / WSDL subcode is wscn:ClientErrorFormatNotSupported.
        // Inbound still matches Document* (MS Format remarks — WIA fallback) and the
        // legacy "format … not in profile" phrase so CreateScanJob keeps driving
        // PNG → EXIF → G4 TIFF → tiff-single-uncompressed.
        let subcode = if reason.contains("ClientErrorNoImagesAvailable") {
            Some("wscn:ClientErrorNoImagesAvailable".into())
        } else if reason.contains("ClientErrorMediaJam") {
            Some("wscn:ClientErrorMediaJam".into())
        } else if reason.contains("ClientErrorCoverOpen") {
            Some("wscn:ClientErrorCoverOpen".into())
        } else if reason.contains("ClientErrorMultipleFeed") {
            // Lab convention: maps to ScannerStateReason MultipleFeedError / WIA MULTI_FEED.
            Some("wscn:ClientErrorMultipleFeed".into())
        } else if reason.contains("ClientErrorInterlockOpen") {
            Some("wscn:ClientErrorInterlockOpen".into())
        } else if reason.contains("ClientErrorInternalStorageFull") {
            Some("wscn:ClientErrorInternalStorageFull".into())
        } else if reason.contains("ClientErrorDocumentFormatNotSupported")
            || reason.contains("ClientErrorFormatNotSupported")
            || (reason.contains("format ") && reason.contains("not in profile"))
        {
            Some("wscn:ClientErrorFormatNotSupported".into())
        } else if reason.contains("ClientErrorConflictingRequiredParameters") {
            Some("wscn:ClientErrorConflictingRequiredParameters".into())
        } else if reason.contains("ClientErrorInvalidScanIdentifier") {
            Some("wscn:ClientErrorInvalidScanIdentifier".into())
        } else if reason.contains("ClientErrorInvalidDestinationToken") {
            Some("wscn:ClientErrorInvalidDestinationToken".into())
        } else if reason.contains("ClientErrorInvalidJobToken") {
            Some("wscn:ClientErrorInvalidJobToken".into())
        } else if reason.contains("InvalidArgs") {
            Some("wscn:InvalidArgs".into())
        } else if reason.contains("ServerErrorTemporaryError") || reason.contains("TemporaryError")
        {
            Some("wscn:ServerErrorTemporaryError".into())
        } else if reason.contains("ServerErrorInternalError") || reason.contains("InternalError") {
            Some("wscn:ServerErrorInternalError".into())
        } else if reason.contains("ServerErrorNotAcceptingJobs")
            || reason.contains("device offline")
        {
            Some("wscn:ServerErrorNotAcceptingJobs".into())
        } else if reason.contains("JobTimedOut") {
            // JobStateReason is JobTimedOut; SOAP fault is OperationFailed (no dedicated subcode).
            Some("wscn:OperationFailed".into())
        } else if reason.contains("ClientErrorJobCancelled") || reason.contains("JobCancelled") {
            // XSD British spelling (Cancelled); history reason stays JobCanceledAtDevice (one L).
            Some("wscn:ClientErrorJobCancelled".into())
        } else if reason.contains("UnableToRenew") {
            Some("wse:UnableToRenew".into())
        } else if reason.contains("InvalidMessage") {
            // WS-Eventing: unknown / mismatched Identifier or expired subscription.
            Some("wse:InvalidMessage".into())
        } else if reason.contains("not found") {
            Some("wscn:ClientErrorJobIdNotFound".into())
        } else {
            None
        };
        Self { subcode, reason }
    }
}

async fn dispatch(
    world: &Arc<RwLock<World>>,
    req: &soap::Request,
    body: &str,
    peer: &str,
    device_id: &str,
) -> Result<SoapOut, FaultOut> {
    let scan_ns = req.scan_ns;
    let relates = req.relates.as_deref();
    let short = req.short_action.as_str();
    // Normalize Action URI short name even when client used 08 vs 01.
    let short = match short {
        "GetResponse" => "Get",
        other => other,
    };
    match short {
        "GetScannerElements" => {
            // Timed / now fault-plan entries fire on status polls too.
            {
                let mut wm = world.write().await;
                wm.tick_fault_plan();
                let changes = wm.drain_pending_condition_changes();
                job_events::maybe_emit_condition_changes(&wm, scan_ns, &changes);
                if !changes.is_empty() {
                    job_events::maybe_emit_status_summary(&wm, scan_ns);
                }
            }
            let w = world.read().await;
            let scanning = w.status().active_jobs > 0;
            let inner = elements::get_scanner_elements(
                elements::ScannerView {
                    profile: w.profile(),
                    jam: w.jam(),
                    cover_open: w.cover_open(),
                    multi_feed: w.multi_feed(),
                    lamp_error: w.lamp_error(),
                    busy: w.busy(),
                    offline: w.offline(),
                    calibrating: w.calibrating(),
                    interlock_open: w.interlock_open(),
                    storage_full: w.storage_full(),
                    paused: w.paused(),
                    lamp_warning: w.lamp_warning(),
                    lamp_warming: w.lamp_warming(),
                    // empty_feeder_brother: the real Brother never publishes InputTrayEmpty.
                    feeder_ready: w.feeder_ready() || w.empty_feeder_brother(),
                    pages_remaining: w.pages_remaining(),
                    scanning,
                    condition_history: w.condition_history(),
                },
                body,
            );
            info!(
                names = ?elements::requested_names(body),
                "GetScannerElements request"
            );
            let body_el = xml::parse(&inner).expect("GetScannerElementsResponse");
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "GetScannerElementsResponse"),
                relates,
                body_el,
            )))
        }
        "ValidateScanTicket" => {
            let w = world.read().await;
            let check = w.validate_ticket(body);
            if let Some(reason) = w
                .refuse_ticket(body, &check)
                .or_else(|| w.jfif_bw_reject(&check.format, check.color))
            {
                let f = FaultOut::from(reason);
                record_ticket(&w, device_id, "ValidateScanTicket", body, &f, "fault", None);
                return Err(f);
            }
            // Named Validate fault (Learn): conflicting MustHonor=true parameters.
            if check
                .reason
                .as_deref()
                .is_some_and(|r| r.contains("ClientErrorConflictingRequiredParameters"))
            {
                let f = FaultOut::from(
                    check
                        .reason
                        .clone()
                        .unwrap_or_else(|| "ClientErrorConflictingRequiredParameters".into()),
                );
                record_ticket(&w, device_id, "ValidateScanTicket", body, &f, "fault", None);
                return Err(f);
            }
            let final_p: Option<serde_json::Value>;
            // WIA Connect requires ImageInformation in ValidationInfo (MS sample);
            // ValidTicket alone → 0x80210001 after Status.
            //
            // Soft-accept DocumentFormatNotSupported here: inbox WSD still CreateScanJobs
            // a MustHonor jfif even when RetrievableDocumentTypes omit it. If Validate
            // returns ValidTicket=false, that probe often stops without the MS Format
            // fallback (PNG→…→uncompressed TIFF / dib). CreateScanJob still faults
            // wscn:ClientErrorFormatNotSupported (XSD; Document* matched inbound) so the
            // retry loop can run.
            let format_soft = check.reason.as_deref().is_some_and(|r| {
                r.contains("ClientErrorDocumentFormatNotSupported")
                    || (r.contains("format ") && r.contains("not in profile"))
            });
            let valid_ticket = check.valid || format_soft;
            if format_soft && !check.valid {
                info!(
                    format = %check.format,
                    "ValidateScanTicket soft-accept format (CreateScanJob will fault)"
                );
            }
            // Learn invalid example: zero ImageInformation + corrected ValidScanTicket.
            // Soft-accepted format miss stays ValidTicket=true (no rewrite).
            let (width_px, height_px, bpl, valid_scan_ticket_el) = if valid_ticket {
                let (width_px, height_px) = crate::image_gen::job_page_pixels(check.dpi);
                // Brother live jfif uses RGB stride for BytesPerLine (not MS compressed=0)
                // unless profile.bytes_per_line_ms is set.
                let mode = w.effects().jfif_gray;
                let eff_format = w
                    .jfif_gray_format(&check.format, check.color)
                    .unwrap_or_else(|| check.format.clone());
                let bpl = crate::image_gen::bytes_per_line_fx(
                    &eff_format,
                    width_px,
                    check.color,
                    w.profile().bytes_per_line_ms,
                    mode,
                    w.effects().jfif_bw,
                );
                let wire = jfif_wire_color(
                    &eff_format,
                    check.color.wsd_token(),
                    w.effects().jfif_gray,
                    w.effects().jfif_bw,
                );
                final_p = Some(final_params_json(wire, bpl, &eff_format));
                (width_px, height_px, bpl, None)
            } else {
                let (corr_dpi, corr_format) = w.corrected_ticket_params(&check);
                final_p = Some(serde_json::json!({
                    "format": corr_format,
                    "resolution": corr_dpi,
                }));
                (
                    0,
                    0,
                    0,
                    Some(responses::valid_scan_ticket_element(
                        body,
                        &check,
                        corr_dpi,
                        &corr_format,
                        w.effects().jfif_gray,
                        w.effects().jfif_bw,
                    )),
                )
            };
            info!(
                valid = check.valid,
                valid_ticket,
                dpi = check.dpi,
                format = %check.format,
                color = %check.color.wsd_token(),
                width_px,
                height_px,
                bytes_per_line = bpl,
                "ValidateScanTicket"
            );
            w.tickets().record(
                device_id,
                "ValidateScanTicket",
                if valid_ticket { "valid" } else { "invalid" }.to_string(),
                check.reason.clone(),
                crate::state::ticket_parsed(body),
                final_p,
                body,
            );
            let body_el = responses::validate_scan_ticket_response(
                valid_ticket,
                width_px,
                height_px,
                bpl,
                valid_scan_ticket_el,
            );
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "ValidateScanTicketResponse"),
                relates,
                body_el,
            )))
        }
        "CreateScanJob" => {
            // Lab: dump ticket Format / size tags so Create→no-RetrieveImage is diagnosable.
            info!(%body, "CreateScanJob request body");
            {
                let path = std::env::temp_dir().join("fakescan-last-create-body.xml");
                if let Err(e) = std::fs::write(&path, body.as_bytes()) {
                    warn!(path = %path.display(), error = %e, "failed to dump CreateScanJob body");
                }
            }
            let mut w = world.write().await;
            let created = w.create_job(body);
            // empty_feeder_brother: refuse as busy only after the delay, without holding the
            // world lock (other requests, incl. status polls, keep answering Idle meanwhile).
            if let Some(ms) = w.take_empty_feeder_refusal_ms() {
                drop(w);
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                w = world.write().await;
            }
            let job = match created {
                Ok(j) => j,
                Err(e) => {
                    let f = FaultOut::from(e.to_string());
                    record_ticket(&w, device_id, "CreateScanJob", body, &f, "refused", None);
                    return Err(f);
                }
            };
            info!(
                job_id = job.id,
                dpi = job.dpi,
                source = %job.input_source,
                format = %job.format,
                color = %job.color.wsd_token(),
                pages = job.pages_left,
                width_px = job.width_px,
                height_px = job.height_px,
                "CreateScanJob"
            );
            // Idle→Processing (active_jobs); NotifyTo no-op when unset.
            job_events::maybe_emit_status_summary(&w, scan_ns);
            // RGB jfif: BytesPerLine = width×3 (Brother capture 2480→7440); gray jfif =
            // width (default). profile.bytes_per_line_ms overrides both.
            let jfif_gray = w.effects().jfif_gray;
            let jfif_bw = w.effects().jfif_bw;
            let bytes_per_line = crate::image_gen::bytes_per_line_fx(
                &job.format,
                job.width_px,
                job.color,
                w.profile().bytes_per_line_ms,
                jfif_gray,
                jfif_bw,
            );
            // Live 2026-10-12 (Windows 11): answering a Grayscale8 ticket with RGB24
            // FinalParams (old coercion) → CreateScanJob then no RetrieveImage. Default
            // is now consistent gray; `jfif_gray=rgb24` restores the coercion.
            let final_color =
                jfif_wire_color(&job.format, job.color.wsd_token(), jfif_gray, jfif_bw);
            // DocumentFinalParameters must fill known params (MS remarks) —
            // bare Format/Resolution alone → WIA CreateScanJob OK then no RetrieveImage.
            // Override / UsedDefault: Learn DocumentFinalParameters — device replaced
            // or filled a default (MustHonor=false substitution sets Override).
            w.tickets().record(
                device_id,
                "CreateScanJob",
                "accepted".to_string(),
                None,
                crate::state::ticket_parsed(body),
                Some(final_params_json(final_color, bytes_per_line, &job.format)),
                body,
            );
            let body_el = responses::create_scan_job_response(&job, bytes_per_line, final_color);
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "CreateScanJobResponse"),
                relates,
                body_el,
            )))
        }
        "RetrieveImage" => {
            let job_id: i32 = req
                .field("JobId")
                .ok_or_else(|| FaultOut::from("JobId missing".to_string()))?
                .parse()
                .map_err(|_| FaultOut::from("JobId invalid".to_string()))?;
            let token = req
                .field("JobToken")
                .ok_or_else(|| FaultOut::from("JobToken missing".to_string()))?;
            let mut w = world.write().await;
            // page_index before this transfer — abort_after_n trips like jam_after_n.
            let page_index_before = w
                .get_job(job_id)
                .filter(|j| j.token == token)
                .map(|j| j.page_index)
                .unwrap_or(0);
            let abort_planned = w.plan_abort_retrieve(page_index_before);
            let (job, image) =
                match w.retrieve_image_with_peer(job_id, &token, Some(peer.to_string())) {
                    Ok(v) => v,
                    Err(e) => {
                        let err = e.to_string();
                        let changes = w.drain_pending_condition_changes();
                        job_events::maybe_emit_condition_changes(&w, scan_ns, &changes);
                        // Idle timeout: job already Aborted + JobTimedOut in World.
                        if err.contains("JobTimedOut") {
                            if let Some(j) = w.get_job(job_id) {
                                job_events::maybe_emit_job_end(&w, scan_ns, j);
                            }
                        }
                        // Mid-job jam/offline and other Retrieve faults: push Stopped +
                        // MediaJam / AttentionRequired so inbox WIA updates without
                        // re-polling GetScannerElements (maps to DOCUMENT_HANDLING_STATUS).
                        if !changes.is_empty() || err.contains("JobTimedOut") {
                            job_events::maybe_emit_status_summary(&w, scan_ns);
                        }
                        return Err(FaultOut::from(err));
                    }
                };
            // Fault-plan mid-page cut: the page is not delivered; push the raised condition.
            let plan_cut = w.take_pending_cut();
            if plan_cut.is_some() {
                let changes = w.drain_pending_condition_changes();
                job_events::maybe_emit_condition_changes(&w, scan_ns, &changes);
                job_events::maybe_emit_status_summary(&w, scan_ns);
            } else {
                // Optional NotifyTo: every Retrieve → JobStatusEvent; last page → JobEndStateEvent.
                job_events::maybe_emit_job_status(&w, scan_ns, &job);
                if job.pages_left == 0 {
                    job_events::maybe_emit_job_end(&w, scan_ns, &job);
                }
                // Often Processing while pages remain; re-emit after JobEnd path too.
                job_events::maybe_emit_status_summary(&w, scan_ns);
            }
            info!(
                job_id,
                image_bytes = image.len(),
                pages_left = job.pages_left,
                format = %job.format,
                width_px = job.width_px,
                height_px = job.height_px,
                abort_planned,
                "RetrieveImage"
            );
            // Lab dump: last RetrieveImage body for SOF / accept-gate diffs.
            {
                let ext = if job.format.eq_ignore_ascii_case("jfif")
                    || job.format.eq_ignore_ascii_case("image/jpeg")
                {
                    "jpg"
                } else {
                    "bin"
                };
                let path = std::env::temp_dir().join(format!("fakescan-last-retrieve.{ext}"));
                if let Err(e) = std::fs::write(&path, &image) {
                    warn!(path = %path.display(), error = %e, "failed to dump RetrieveImage body");
                } else {
                    info!(
                        path = %path.display(),
                        bytes = image.len(),
                        "dumped RetrieveImage body"
                    );
                }
            }
            // Content-ID shape matches MS RetrieveImageResponse sample (`…@uuid`).
            // xmlns:xop is added on the Envelope in mtom_response (not on Include).
            let cid = format!("{}@uuid", Uuid::new_v4());
            let body_el = responses::retrieve_image_response(&cid);
            // Action …/RetrieveImageResponse (tried bare …/RetrieveImage per MS
            // sample; same CancelJob-after-~30s outcome).
            let soap_xml = responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "RetrieveImageResponse"),
                relates,
                body_el,
            );
            let content_type = crate::image_gen::mime_for_format(&job.format).to_string();
            let retrieve_bps = w.retrieve_bps();
            // `Some(n)` arms abort: `n == 0` → cut at half the assembled MTOM body.
            let abort_after = abort_planned.then_some(w.abort_retrieve_bytes() as usize);
            let live = (retrieve_bps > 0 || plan_cut.is_some() || w.plan_has_pending())
                .then(|| w.live_cut_handle(job_id));
            Ok(SoapOut::Mtom {
                soap_xml,
                image,
                content_type,
                retrieve_bps,
                abort_after,
                cut: plan_cut,
                live,
            })
        }
        "CancelJob" => {
            let job_id: i32 = req
                .field("JobId")
                .ok_or_else(|| FaultOut::from("JobId missing".to_string()))?
                .parse()
                .map_err(|_| FaultOut::from("JobId invalid".to_string()))?;
            let mut w = world.write().await;
            // Snapshot before remove — only emit when cancel ends an active job.
            let end_snap = w
                .get_job(job_id)
                .filter(|j| !j.aborted && j.pages_left > 0)
                .map(job_events::JobEventSnap::canceled);
            w.cancel_job(job_id)
                .map_err(|e| FaultOut::from(e.to_string()))?;
            if let Some(snap) = end_snap {
                job_events::maybe_emit_job_end_snap(&w, scan_ns, &snap);
            }
            // Processing→Idle when the canceled job was the last active one.
            job_events::maybe_emit_status_summary(&w, scan_ns);
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "CancelJobResponse"),
                relates,
                responses::cancel_job_response(),
            )))
        }
        "GetActiveJobs" => {
            let w = world.read().await;
            let jobs = w.jobs_snapshot();
            let body_el = responses::get_active_jobs_response(&jobs);
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "GetActiveJobsResponse"),
                relates,
                body_el,
            )))
        }
        "GetJobElements" => {
            let job_id: i32 = req
                .field("JobId")
                .ok_or_else(|| FaultOut::from("JobId missing".to_string()))?
                .parse()
                .map_err(|_| FaultOut::from("JobId invalid".to_string()))?;
            let mut w = world.write().await;
            w.ensure_job_not_timed_out(job_id)
                .map_err(|e| FaultOut::from(e.to_string()))?;
            let job = w.get_job(job_id).ok_or_else(|| {
                // Canceled JobId lives only in history → ClientErrorJobCancelled.
                FaultOut::from(w.missing_job_fault(job_id))
            })?;
            let inner = elements::get_job_elements(job, body);
            let body_el = xml::parse(&inner).expect("GetJobElementsResponse");
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "GetJobElementsResponse"),
                relates,
                body_el,
            )))
        }
        "GetJobHistory" => {
            let w = world.read().await;
            let summaries = w
                .job_history()
                .iter()
                .map(|entry| responses::job_history_summary(entry.id, &entry.state, &entry.reason))
                .collect();
            let body_el = responses::get_job_history_response(summaries);
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::scan_action(scan_ns, "GetJobHistoryResponse"),
                relates,
                body_el,
            )))
        }
        // WS-Eventing: WIA/WSDScan Subscribes before CreateScanJob; a fault here
        // surfaces as WIA_ERROR_OFFLINE even after GetScannerElements succeeds.
        "Subscribe" => {
            let mut w = world.write().await;
            if let Some(reason) = w.not_accepting_reason() {
                return Err(FaultOut::from(reason.to_string()));
            }
            let mgr = w.service_endpoint().to_string();
            let sub_id = Uuid::new_v4();
            let identifier = format!("uuid:{sub_id}");
            let expires = req.expires();
            let contexts = req.destination_contexts();
            let notify_url = req.notify_to();
            let filter_dialect = req.filter_dialect();
            let subscriber = peer.parse::<SocketAddr>().ok().map(|a| a.ip());
            // A bad NotifyTo must not fail Subscribe: WIA treats a Subscribe fault as offline.
            // It is also not stored, so the process will not POST to an arbitrary host.
            match notify_url.as_deref() {
                Some(url) => match notify::accept_notify_target(url, subscriber) {
                    Ok(target) => {
                        w.set_notify_to(Some(&target.url()));
                        w.set_notify_subscriber(subscriber);
                    }
                    Err(e) => {
                        warn!(error = %e, "NotifyTo rejected; subscription kept without a sink");
                        w.set_notify_to(None);
                    }
                },
                None => w.set_notify_to(None),
            }
            w.set_subscription_id(Some(&identifier));
            w.set_subscription_expires(Some(&expires));
            w.set_subscription_deadline(soap::expires_deadline_now(&expires));
            // Windows Subscribe often carries wscn:ScanDestinations; the service
            // must return matching DestinationResponses / DestinationToken or
            // WIA may leave the device "not online".
            let dest_blocks: Vec<Element> = contexts
                .iter()
                .map(|ctx| {
                    let token = format!("fakescan-dest-{}", Uuid::new_v4());
                    w.register_destination(&token, ctx);
                    responses::destination_response(ctx, &token)
                })
                .collect();
            info!(
                %sub_id,
                %expires,
                destinations = contexts.len(),
                notify_to = notify_url.as_deref().unwrap_or("-"),
                filter_dialect = filter_dialect.as_deref().unwrap_or("-"),
                "WS-Eventing Subscribe accepted (stub)"
            );
            // Lab: WIA open may Subscribe then immediately Unsubscribe + offline —
            // keep the request at info while diagnosing.
            info!(%body, "Subscribe request body");
            let body_el = responses::subscribe_response(&mgr, &identifier, &expires, dest_blocks);
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::eventing_action("SubscribeResponse"),
                relates,
                body_el,
            )))
        }
        "Renew" => {
            let expires = req.expires();
            let req_id = req.identifier().map(str::to_string);
            {
                let mut w = world.write().await;
                if w.subscription_deadline_elapsed(Instant::now()) {
                    w.clear_subscriptions();
                    return Err(FaultOut::from(
                        "InvalidMessage: subscription expired".to_string(),
                    ));
                }
                // Identifier present: must match an active subscription (WSE InvalidMessage).
                // Identifier absent: lab-accept / WIA quirk — still refresh Expires.
                if let Some(ref id) = req_id {
                    match w.subscription_id() {
                        Some(stored) if stored == id.as_str() => {}
                        _ => {
                            return Err(FaultOut::from(
                                "InvalidMessage: unknown or mismatched Identifier".to_string(),
                            ));
                        }
                    }
                }
                w.set_subscription_expires(Some(&expires));
                w.set_subscription_deadline(soap::expires_deadline_now(&expires));
            }
            info!(%expires, "WS-Eventing Renew accepted");
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::eventing_action("RenewResponse"),
                relates,
                responses::renew_response(&expires),
            )))
        }
        "Unsubscribe" => {
            let req_id = req.identifier().map(str::to_string);
            {
                let mut w = world.write().await;
                // Identifier present: must match an active subscription (WSE InvalidMessage).
                // Identifier absent: lab-accept / WIA quirk — clear all.
                if let Some(ref id) = req_id {
                    match w.subscription_id() {
                        Some(stored) if stored == id.as_str() => {}
                        _ => {
                            return Err(FaultOut::from(
                                "InvalidMessage: unknown or mismatched Identifier".to_string(),
                            ));
                        }
                    }
                }
                w.clear_subscriptions();
            }
            info!("WS-Eventing Unsubscribe cleared destinations / NotifyTo");
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::eventing_action("UnsubscribeResponse"),
                relates,
                responses::unsubscribe_response(),
            )))
        }
        "GetStatus" => {
            let req_id = req.identifier().map(str::to_string);
            let stored = {
                let mut w = world.write().await;
                if w.subscription_deadline_elapsed(Instant::now()) {
                    w.clear_subscriptions();
                    return Err(FaultOut::from(
                        "InvalidMessage: subscription expired".to_string(),
                    ));
                }
                if let Some(ref id) = req_id {
                    match w.subscription_id() {
                        Some(stored) if stored == id.as_str() => {}
                        _ => {
                            return Err(FaultOut::from(
                                "InvalidMessage: unknown or mismatched Identifier".to_string(),
                            ));
                        }
                    }
                }
                w.subscription_expires().map(str::to_owned)
            };
            let expires = stored.unwrap_or_else(|| req.expires());
            Ok(SoapOut::Xml(responses::envelope_body(
                scan_ns,
                &soap::eventing_action("GetStatusResponse"),
                relates,
                responses::get_status_response(&expires),
            )))
        }
        "Get" => {
            let w = world.read().await;
            let friendly = friendly_name_for_listing(&w.profile().label);
            let device_uuid = w.device_uuid().to_string();
            let metadata =
                responses::transfer_get_metadata(friendly, &device_uuid, w.service_endpoint());
            Ok(SoapOut::Xml(responses::transfer_get_response(
                relates, metadata,
            )))
        }
        other => Err(FaultOut::from(format!("action not implemented: {other}"))),
    }
}

/// Name shown in Add device / WIA; must match `Test-FakescanLooksLike` hints.
fn friendly_name_for_listing(profile_label: &str) -> &str {
    let lower = profile_label.to_ascii_lowercase();
    if lower.contains("fakescan") || lower.contains("fake wsd scanner") {
        profile_label
    } else {
        "fakescan"
    }
}

/// XOP Include NS accepted by inbox wsdscdrv (Brother live). `2003/12` → CancelJob.
const XOP_INCLUDE_NS: &str = "http://www.w3.org/2004/08/xop/include";

/// Pure MTOM wire pieces (no HTTP). Unit-tested; [`mtom_response`] wraps this.
#[allow(dead_code)] // boundary / cids asserted in unit tests; HTTP path uses CT + body.
struct MtomWire {
    boundary: String,
    soap_cid: String,
    image_cid: String,
    outer_content_type: String,
    body: Vec<u8>,
}

/// Force `xmlns:xop` on the Envelope to [`XOP_INCLUDE_NS`] (replace wrong URI if present).
fn ensure_xop_xmlns_2004_08(soap_xml: &str) -> String {
    if let Some(start) = soap_xml.find("xmlns:xop=\"") {
        let after = start + "xmlns:xop=\"".len();
        if let Some(end_rel) = soap_xml[after..].find('"') {
            let mut out = soap_xml.to_string();
            out.replace_range(after..after + end_rel, XOP_INCLUDE_NS);
            return out;
        }
    }
    soap_xml.replacen(
        "<soap:Envelope ",
        &format!("<soap:Envelope xmlns:xop=\"{XOP_INCLUDE_NS}\" "),
        1,
    )
}

/// Build RetrieveImage MTOM body + outer Content-Type (MS sample + Brother accept deltas).
///
/// https://learn.microsoft.com/en-us/windows-hardware/drivers/image/retrieveimageresponse
fn build_mtom(soap_xml: &str, image: &[u8], image_ct: &str) -> MtomWire {
    // bare UUID boundary, `@uuid` Content-IDs, MIME-Version, xmlns:xop on Envelope,
    // `startinfo` (MS) + `start-info` (RFC), type-before-charset on root part,
    // image CT without trailing `;`, CTE binary, CRLF throughout.
    let boundary = Uuid::new_v4().to_string();
    let soap_cid = format!("{}@uuid", Uuid::new_v4());
    let image_cid = soap_xml
        .split("cid:")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .unwrap_or("image@uuid")
        .to_string();

    let soap_xml = ensure_xop_xmlns_2004_08(soap_xml);
    // Multipart parts are CRLF-delimited; keep the SOAP Infoset on CRLF too.
    let soap_xml = soap_xml.replace("\r\n", "\n").replace('\n', "\r\n");

    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    // Brother live (2026-10-12): SOAP part CTE binary (not 8bit); image
    // `Content-Type: image/jpeg` with no trailing `;`; Content-ID `<…@uuid>`
    // with no space before `>`.
    body.extend_from_slice(
        format!(
            "Content-Type: application/xop+xml; type=\"application/soap+xml\"; charset=utf-8\r\n\
Content-Transfer-Encoding: binary\r\n\
Content-ID: <{soap_cid}>\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(soap_xml.as_bytes());
    body.extend_from_slice(b"\r\n");
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    let image_ct_header = image_ct.trim_end_matches(';');
    body.extend_from_slice(
        format!(
            "Content-Type: {image_ct_header}\r\n\
Content-Transfer-Encoding: binary\r\n\
Content-ID: <{image_cid}>\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(image);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());

    // Emit both MS `startinfo` and W3C/RFC `start-info`. Boundary/start unquoted.
    let outer_content_type = format!(
        "multipart/related; type=application/xop+xml; boundary={boundary}; start=\"<{soap_cid}>\"; start-info=application/soap+xml; startinfo=application/soap+xml"
    );

    MtomWire {
        boundary,
        soap_cid,
        image_cid,
        outer_content_type,
        body,
    }
}

fn mtom_response(
    soap_xml: &str,
    image: &[u8],
    image_ct: &str,
    retrieve_bps: u64,
    abort_after: Option<usize>,
    cut: Option<crate::state::Cut>,
    live: Option<crate::state::LiveCut>,
) -> Response {
    let wire = build_mtom(soap_xml, image, image_ct);

    let mut headers = HeaderMap::new();
    headers.insert(
        header::HeaderName::from_static("mime-version"),
        header::HeaderValue::from_static("1.0"),
    );
    headers.insert(
        header::CONTENT_TYPE,
        wire.outer_content_type.parse().expect("content-type"),
    );
    // Explicit length so wsdscdrv does not hang on chunked framing (~30s then CancelJob).
    // Throttle paces the TCP send of that known-length body (not Transfer-Encoding: chunked).
    // Abort still advertises the full length, then errors the stream mid-body (lab: drop).
    headers.insert(
        header::CONTENT_LENGTH,
        header::HeaderValue::from_str(&wire.body.len().to_string()).expect("content-length"),
    );

    let body = wire.body;
    let cut = match cut {
        Some(c) => Some(c.len(body.len())),
        None => abort_after.map(|cfg| abort_cut_len(body.len(), cfg)),
    };
    if let Some(n) = cut {
        warn!(
            abort_after = n,
            body_len = body.len(),
            "RetrieveImage MTOM abort mid-send"
        );
        return (
            StatusCode::OK,
            headers,
            aborted_body(body, n, retrieve_bps, live),
        )
            .into_response();
    }
    if retrieve_bps == 0 && live.is_none() {
        return (StatusCode::OK, headers, body).into_response();
    }
    (
        StatusCode::OK,
        headers,
        throttled_body(body, retrieve_bps, live),
    )
        .into_response()
}

/// `cfg == 0` → half of `body_len`; otherwise min(cfg, body_len-1), at least 1.
fn abort_cut_len(body_len: usize, cfg: usize) -> usize {
    if body_len <= 1 {
        return body_len.saturating_sub(1);
    }
    let cut = if cfg == 0 {
        body_len / 2
    } else {
        cfg.min(body_len.saturating_sub(1))
    };
    cut.max(1)
}

/// Pace a known-length body at about `bps` bytes/sec (8 KiB chunks).
fn throttled_body(
    body: Vec<u8>,
    bps: u64,
    live: Option<crate::state::LiveCut>,
) -> axum::body::Body {
    use bytes::Bytes;
    use futures_util::stream::unfold;
    use std::time::Duration;

    // ~8 KiB chunks; delay = chunk / bps (bps 0 = unpaced, still cuttable via `live`).
    let chunk = 8 * 1024usize;
    let delay = if bps == 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(
            (u128::from(chunk as u64) * 1_000_000_000u128 / u128::from(bps))
                .try_into()
                .unwrap_or(u64::MAX),
        )
    };
    let stream = unfold(
        (body, 0usize, true, false),
        move |(data, off, first, failed)| {
            let live = live.clone();
            async move {
                if failed || off >= data.len() {
                    return None;
                }
                if !first {
                    if delay.is_zero() {
                        tokio::task::yield_now().await;
                    } else {
                        tokio::time::sleep(delay).await;
                    }
                }
                if live.as_ref().is_some_and(|l| l.tripped()) {
                    let err = std::io::Error::new(
                        std::io::ErrorKind::ConnectionReset,
                        "fakescan fault plan: stream cut live",
                    );
                    return Some((Err(err), (data, off, false, true)));
                }
                let end = (off + chunk).min(data.len());
                let piece = Bytes::copy_from_slice(&data[off..end]);
                Some((Ok::<_, std::io::Error>(piece), (data, end, false, false)))
            }
        },
    );
    axum::body::Body::from_stream(stream)
}

/// Send up to `abort_after` bytes (optionally paced), then fail the stream so the
/// client sees a mid-transfer drop while `Content-Length` still claimed the full body.
fn aborted_body(
    body: Vec<u8>,
    abort_after: usize,
    bps: u64,
    live: Option<crate::state::LiveCut>,
) -> axum::body::Body {
    use bytes::Bytes;
    use futures_util::stream::unfold;
    use std::io::{Error, ErrorKind};
    use std::time::Duration;

    let abort_after = abort_after
        .min(body.len().saturating_sub(1))
        .max(1.min(body.len()));
    let chunk = 8 * 1024usize;
    let delay = if bps == 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(
            (u128::from(chunk as u64) * 1_000_000_000u128 / u128::from(bps.max(1)))
                .try_into()
                .unwrap_or(u64::MAX),
        )
    };
    let stream = unfold(
        (body, 0usize, true, false),
        move |(data, off, first, failed)| {
            let live = live.clone();
            async move {
                if failed {
                    return None;
                }
                if live.as_ref().is_some_and(|l| l.tripped()) {
                    let err = Error::new(
                        ErrorKind::ConnectionReset,
                        "fakescan fault plan: stream cut live",
                    );
                    return Some((Err(err), (data, off, false, true)));
                }
                if off >= abort_after {
                    let err = Error::new(
                        ErrorKind::ConnectionReset,
                        format!("fakescan abort_retrieve after {abort_after} bytes"),
                    );
                    return Some((Err(err), (data, off, false, true)));
                }
                if !first && !delay.is_zero() {
                    tokio::time::sleep(delay).await;
                }
                let end = (off + chunk).min(abort_after);
                let piece = Bytes::copy_from_slice(&data[off..end]);
                Some((Ok::<_, Error>(piece), (data, end, false, false)))
            }
        },
    );
    axum::body::Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use std::path::PathBuf;

    fn test_world() -> Arc<RwLock<World>> {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-wsd").expect("profile");
        Arc::new(RwLock::new(World::new(dir, profile)))
    }

    #[test]
    fn ensure_xop_xmlns_injects_2004_08() {
        let xml = "<soap:Envelope xmlns:wscn=\"urn:scan\">\n  <soap:Body/>\n</soap:Envelope>";
        let out = ensure_xop_xmlns_2004_08(xml);
        assert!(
            out.contains(&format!("xmlns:xop=\"{XOP_INCLUDE_NS}\"")),
            "{out}"
        );
        assert!(!out.contains("2003/12/xop/include"), "{out}");
    }

    #[test]
    fn ensure_xop_xmlns_replaces_2003_12() {
        let xml = concat!(
            "<soap:Envelope xmlns:xop=\"http://www.w3.org/2003/12/xop/include\" ",
            "xmlns:wscn=\"urn:scan\"><soap:Body/></soap:Envelope>"
        );
        let out = ensure_xop_xmlns_2004_08(xml);
        assert!(out.contains(XOP_INCLUDE_NS), "{out}");
        assert!(!out.contains("2003/12/xop/include"), "{out}");
    }

    #[test]
    fn build_mtom_locks_wsdscdrv_accept_shape() {
        // Packaging invariants from docs/specs/mtom-retrieveimage.md (no live WIA).
        let image_cid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee@uuid";
        let soap = format!(
            "<soap:Envelope xmlns:wscn=\"urn:scan\">\n\
             <soap:Body><wscn:RetrieveImageResponse>\
             <wscn:ScanData><xop:Include href=\"cid:{image_cid}\" /></wscn:ScanData>\
             </wscn:RetrieveImageResponse></soap:Body></soap:Envelope>"
        );
        let jpeg = b"\xff\xd8\xff\xd9";
        let wire = build_mtom(&soap, jpeg, "image/jpeg;");

        // Outer CT: MS startinfo + RFC start-info; bare UUID boundary; start → SOAP CID.
        assert!(
            !wire.boundary.contains(':'),
            "boundary must be bare UUID, not uuid:…: {}",
            wire.boundary
        );
        assert!(wire.outer_content_type.contains("multipart/related"));
        assert!(wire.outer_content_type.contains("type=application/xop+xml"));
        assert!(wire
            .outer_content_type
            .contains(&format!("boundary={}", wire.boundary)));
        let start_needle = format!(r#"start="<{}>""#, wire.soap_cid);
        assert!(
            wire.outer_content_type.contains(&start_needle),
            "missing {start_needle:?} in {}",
            wire.outer_content_type
        );
        assert!(wire
            .outer_content_type
            .contains("start-info=application/soap+xml"));
        assert!(wire
            .outer_content_type
            .contains("startinfo=application/soap+xml"));

        // CID linkage: xop:Include cid: equals image part Content-ID (no <> in href).
        assert_eq!(wire.image_cid, image_cid);
        let body = String::from_utf8_lossy(&wire.body);
        assert!(
            body.contains(&format!("href=\"cid:{image_cid}\"")),
            "Include href must stay cid: without brackets"
        );
        assert!(
            body.contains(&format!("Content-ID: <{image_cid}>\r\n")),
            "image Content-ID must be <cid> with no space before >: {}",
            body.lines().take(20).collect::<Vec<_>>().join(" | ")
        );
        assert!(body.contains(&format!("Content-ID: <{}>\r\n", wire.soap_cid)));

        // XOP NS on Envelope; never 2003/12.
        assert!(body.contains(XOP_INCLUDE_NS));
        assert!(!body.contains("2003/12/xop/include"));

        // Part headers: type-before-charset; CTE binary; image CT strips trailing `;`.
        assert!(body.contains(
            "Content-Type: application/xop+xml; type=\"application/soap+xml\"; charset=utf-8\r\n"
        ));
        assert!(body.contains("Content-Transfer-Encoding: binary\r\n"));
        assert!(body.contains("Content-Type: image/jpeg\r\n"));
        assert!(!body.contains("Content-Type: image/jpeg;\r\n"));

        // CRLF separators + closing boundary; JPEG present; SOAP Infoset uses CRLF.
        assert!(wire.body.windows(4).any(|w| w == b"\xff\xd8\xff\xd9"));
        assert!(body.contains(&format!("\r\n--{}--\r\n", wire.boundary)));
        assert!(
            body.contains("\r\n<soap:Envelope"),
            "SOAP Infoset must use CRLF after part headers"
        );
        // HTTP chunked framing is not part of the multipart body (only CTE headers).
        assert!(!body.contains("\nTransfer-Encoding:"));
        assert!(!body.contains("\rTransfer-Encoding:"));
    }

    #[tokio::test]
    async fn unsubscribe_wrong_identifier_faults() {
        let world = test_world();
        {
            let mut w = world.write().await;
            w.set_subscription_id(Some("uuid:active-sub"));
            w.set_subscription_expires(Some("PT1H"));
        }
        let body = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Unsubscribe</wsa:Action>
    <wsa:MessageID>uuid:unsub-bad-0000-0000-000000000001</wsa:MessageID>
    <wsa:ReferenceParameters>
      <wse:Identifier>uuid:deadbeef</wse:Identifier>
    </wsa:ReferenceParameters>
  </soap:Header>
  <soap:Body>
    <wse:Unsubscribe/>
  </soap:Body>
</soap:Envelope>"#;
        let req = soap::Request::parse(body);
        let result = dispatch(&world, &req, body, "test", "test-dev").await;
        let Err(err) = result else {
            panic!("mismatched Identifier must fault");
        };
        assert_eq!(err.subcode.as_deref(), Some("wse:InvalidMessage"));
        assert!(
            err.reason.contains("mismatched") || err.reason.contains("unknown"),
            "reason={}",
            err.reason
        );
        // Active subscription must remain (do not clear on mismatch).
        assert_eq!(
            world.read().await.subscription_id(),
            Some("uuid:active-sub")
        );
    }

    #[tokio::test]
    async fn unsubscribe_without_identifier_clears() {
        let world = test_world();
        {
            let mut w = world.write().await;
            w.set_subscription_id(Some("uuid:active-sub"));
            w.set_subscription_expires(Some("PT1H"));
        }
        let body = r#"<?xml version="1.0"?>
<soap:Envelope xmlns:soap="http://www.w3.org/2003/05/soap-envelope"
  xmlns:wsa="http://schemas.xmlsoap.org/ws/2004/08/addressing"
  xmlns:wse="http://schemas.xmlsoap.org/ws/2004/08/eventing">
  <soap:Header>
    <wsa:Action>http://schemas.xmlsoap.org/ws/2004/08/eventing/Unsubscribe</wsa:Action>
    <wsa:MessageID>uuid:unsub-ok-0000-0000-000000000001</wsa:MessageID>
  </soap:Header>
  <soap:Body>
    <wse:Unsubscribe/>
  </soap:Body>
</soap:Envelope>"#;
        let req = soap::Request::parse(body);
        let result = dispatch(&world, &req, body, "test", "test-dev").await;
        assert!(result.is_ok(), "absent Identifier clears");
        assert!(world.read().await.subscription_id().is_none());
    }

    #[test]
    fn job_timed_out_maps_operation_failed() {
        let f = FaultOut::from("OperationFailed: JobTimedOut on job 3".to_string());
        assert_eq!(f.subcode.as_deref(), Some("wscn:OperationFailed"));
    }

    #[test]
    fn format_not_supported_maps_xsd_format_fault() {
        // Document* inbound (WIA / Learn Format remarks) → XSD outbound name.
        let f = FaultOut::from(
            "ClientErrorDocumentFormatNotSupported: format jfif not in profile list".to_string(),
        );
        assert_eq!(
            f.subcode.as_deref(),
            Some("wscn:ClientErrorFormatNotSupported")
        );
        let xsd = FaultOut::from(
            "ClientErrorFormatNotSupported: format jfif not in profile list".to_string(),
        );
        assert_eq!(
            xsd.subcode.as_deref(),
            Some("wscn:ClientErrorFormatNotSupported")
        );
        let legacy = FaultOut::from("format jfif not in profile list".to_string());
        assert_eq!(
            legacy.subcode.as_deref(),
            Some("wscn:ClientErrorFormatNotSupported")
        );
    }

    #[test]
    fn conflicting_must_honor_maps_named_fault() {
        let f = FaultOut::from(
            "ClientErrorConflictingRequiredParameters: MustHonor conflict on Resolution+Format"
                .to_string(),
        );
        assert_eq!(
            f.subcode.as_deref(),
            Some("wscn:ClientErrorConflictingRequiredParameters")
        );
    }

    #[test]
    fn job_cancelled_phrase_maps_xsd() {
        assert_eq!(
            FaultOut::from("ClientErrorJobCancelled: job 7 was cancelled".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorJobCancelled")
        );
        assert_eq!(
            FaultOut::from("JobCancelled: job already canceled".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorJobCancelled")
        );
        // Unknown JobId stays JobIdNotFound (must not steal Cancelled mapping).
        assert_eq!(
            FaultOut::from("job 99 not found".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorJobIdNotFound")
        );
    }

    #[test]
    fn push_and_job_token_faults_map_xsd() {
        assert_eq!(
            FaultOut::from("ClientErrorInvalidScanIdentifier: unknown".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorInvalidScanIdentifier")
        );
        assert_eq!(
            FaultOut::from("ClientErrorInvalidDestinationToken: bad".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorInvalidDestinationToken")
        );
        assert_eq!(
            FaultOut::from("ClientErrorInvalidJobToken: mismatch".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ClientErrorInvalidJobToken")
        );
        assert_eq!(
            FaultOut::from(
                "InvalidArgs: ScanIdentifier and DestinationToken required together".to_string()
            )
            .subcode
            .as_deref(),
            Some("wscn:InvalidArgs")
        );
    }

    #[test]
    fn temporary_and_internal_faults_map_xsd() {
        assert_eq!(
            FaultOut::from("ServerErrorTemporaryError: temporary".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ServerErrorTemporaryError")
        );
        assert_eq!(
            FaultOut::from("TemporaryError: retry later".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ServerErrorTemporaryError")
        );
        assert_eq!(
            FaultOut::from("ServerErrorInternalError: internal".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ServerErrorInternalError")
        );
        assert_eq!(
            FaultOut::from("InternalError: permanent failure".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ServerErrorInternalError")
        );
        // Busy / offline stay NotAcceptingJobs (no TemporaryError/InternalError phrase).
        assert_eq!(
            FaultOut::from("ServerErrorNotAcceptingJobs: device busy".to_string())
                .subcode
                .as_deref(),
            Some("wscn:ServerErrorNotAcceptingJobs")
        );
    }

    #[test]
    fn eventing_identifier_faults_map_wse() {
        assert_eq!(
            FaultOut::from("InvalidMessage: unknown or mismatched Identifier".to_string())
                .subcode
                .as_deref(),
            Some("wse:InvalidMessage")
        );
        assert_eq!(
            FaultOut::from("InvalidMessage: subscription expired".to_string())
                .subcode
                .as_deref(),
            Some("wse:InvalidMessage")
        );
        assert_eq!(
            FaultOut::from("UnableToRenew: refused".to_string())
                .subcode
                .as_deref(),
            Some("wse:UnableToRenew")
        );
    }
}
