//! Lab stubs: optional JobStatus / Condition / ElementsChange push to WS-Eventing NotifyTo.
//!
//! When no NotifyTo is registered on [`World`], every helper is a no-op (client-pull
//! WIA path unchanged). Delivery reuses [`super::notify::post_soap_event`].

use tracing::{info, warn};

use crate::profile::Profile;
use crate::state::{ConditionChange, ScanJob, World};
use crate::wsd::notify;
use crate::wsd::soap;
use crate::wsd::xml::Element;

/// Snapshot carried into the async NotifyTo POST (no World lock held).
#[derive(Debug, Clone)]
pub struct JobEventSnap {
    pub job_id: i32,
    pub state: String,
    pub reason: String,
    pub images_completed: u32,
    pub images_to_transfer: u32,
}

impl JobEventSnap {
    pub fn from_job(job: &ScanJob) -> Self {
        Self {
            job_id: job.id,
            state: job.job_state().to_string(),
            reason: job.state_reason.clone(),
            images_completed: job.page_index,
            images_to_transfer: job.pages_left,
        }
    }

    /// CancelJob end-state (job removed from World before emit).
    pub fn canceled(job: &ScanJob) -> Self {
        Self {
            job_id: job.id,
            state: "Canceled".into(),
            reason: "JobCanceledAtDevice".into(),
            images_completed: job.page_index,
            images_to_transfer: job.pages_left,
        }
    }
}

/// Fire-and-forget JobStatusEvent on every successful RetrieveImage progress.
pub fn maybe_emit_job_status(world: &World, scan_ns: &str, job: &ScanJob) {
    maybe_emit(
        world,
        scan_ns,
        "JobStatusEvent",
        &job_status_event_body(&JobEventSnap::from_job(job)),
    );
}

/// Fire-and-forget JobEndStateEvent when the job reaches Completed / Canceled / Aborted.
pub fn maybe_emit_job_end(world: &World, scan_ns: &str, job: &ScanJob) {
    maybe_emit(
        world,
        scan_ns,
        "JobEndStateEvent",
        &job_end_state_event_body(&JobEventSnap::from_job(job)),
    );
}

/// JobEndStateEvent from a pre-built snap (Cancel removes the job first).
pub fn maybe_emit_job_end_snap(world: &World, scan_ns: &str, snap: &JobEventSnap) {
    maybe_emit(
        world,
        scan_ns,
        "JobEndStateEvent",
        &job_end_state_event_body(snap),
    );
}

/// Emit ScannerStatusConditionEvent / ClearedEvent for each fault delta (NotifyTo only).
///
/// Skips changes with `id == 0` (e.g. offline `AttentionRequired`): those are
/// ScannerStateReasons only, not DeviceCondition rows in the WSD schema.
pub fn maybe_emit_condition_changes(world: &World, scan_ns: &str, changes: &[ConditionChange]) {
    for ch in changes {
        if ch.id == 0 {
            continue;
        }
        let sev = if ch.severity.is_empty() {
            "Critical"
        } else {
            ch.severity
        };
        let (action, body) = if ch.raised {
            (
                "ScannerStatusConditionEvent",
                condition_event_body(ch.id, ch.name, ch.component, sev, true),
            )
        } else {
            (
                "ScannerStatusConditionClearedEvent",
                condition_event_body(ch.id, ch.name, ch.component, sev, false),
            )
        };
        maybe_emit(world, scan_ns, action, &body);
    }
}

/// SOAP body for raised (`true`) or cleared (`false`) scanner condition.
///
/// Raise → `DeviceCondition` (@Id, Time, Name, Component, Severity).
/// Clear → `DeviceConditionCleared` (ConditionId + ConditionClearTime).
/// `severity` is XSD Critical / Warning / Informational (ignored on clear).
pub fn condition_event_body(
    id: u32,
    name: &str,
    component: &str,
    severity: &str,
    raised: bool,
) -> String {
    let time = crate::timeutil::utc_now_iso();
    if raised {
        let cond = Element::new("wscn:DeviceCondition")
            .with_attr("Id", id.to_string())
            .with(Element::text_el("wscn:Time", time))
            .with(Element::text_el("wscn:Name", name))
            .with(Element::text_el("wscn:Component", component))
            .with(Element::text_el("wscn:Severity", severity));
        Element::new("wscn:ScannerStatusConditionEvent")
            .with(cond)
            .to_xml()
    } else {
        let cleared = Element::new("wscn:DeviceConditionCleared")
            .with(Element::text_el("wscn:ConditionId", id.to_string()))
            .with(Element::text_el("wscn:ConditionClearTime", time));
        Element::new("wscn:ScannerStatusConditionClearedEvent")
            .with(cleared)
            .to_xml()
    }
}

/// Fire-and-forget ScannerElementsChangeEvent when lab ctl changes advertised elements.
pub fn maybe_emit_elements_change(world: &World, scan_ns: &str, profile: &Profile) {
    maybe_emit(
        world,
        scan_ns,
        "ScannerElementsChangeEvent",
        &elements_change_event_body(profile),
    );
}

/// `wscn:ScannerElementsChangeEvent` with full ScannerConfiguration + DefaultScanTicket.
pub fn elements_change_event_body(profile: &Profile) -> String {
    let mut changes = Element::new("wscn:ElementChanges");
    for child in super::elements::element_changes_elements(profile) {
        changes = changes.with(child);
    }
    Element::new("wscn:ScannerElementsChangeEvent")
        .with(changes)
        .to_xml()
}

/// Fire-and-forget ScannerStatusSummaryEvent from current World fault/busy/scan flags.
///
/// Call after condition deltas and job lifecycle transitions (CreateScanJob,
/// RetrieveImage, CancelJob / JobEnd) so NotifyTo clients see State + Reasons
/// without re-polling GetScannerElements. No-op when no NotifyTo is registered.
pub fn maybe_emit_status_summary(world: &World, scan_ns: &str) {
    let (state, reasons) = status_summary_from_world(world);
    maybe_emit(
        world,
        scan_ns,
        "ScannerStatusSummaryEvent",
        &status_summary_event_body(&state, &reasons),
    );
}

/// Mirror `elements::scanner_status` state/reason logic from World getters.
pub fn status_summary_from_world(world: &World) -> (String, Vec<&'static str>) {
    let scanning = world.status().active_jobs > 0;
    status_summary_state(
        world.jam(),
        world.cover_open(),
        world.multi_feed(),
        world.lamp_error(),
        world.busy(),
        world.offline(),
        scanning,
    )
}

/// Stopped if jam/cover/multi/lamp/offline; Processing if scanning||busy; else Idle.
/// Reasons: MediaJam, CoverOpen, MultipleFeedError, LampError, AttentionRequired, else None.
pub fn status_summary_state(
    jam: bool,
    cover_open: bool,
    multi_feed: bool,
    lamp_error: bool,
    busy: bool,
    offline: bool,
    scanning: bool,
) -> (String, Vec<&'static str>) {
    let mut reasons = Vec::new();
    if jam {
        reasons.push("MediaJam");
    }
    if cover_open {
        reasons.push("CoverOpen");
    }
    if multi_feed {
        reasons.push("MultipleFeedError");
    }
    if lamp_error {
        reasons.push("LampError");
    }
    if offline {
        reasons.push("AttentionRequired");
    }
    if reasons.is_empty() {
        reasons.push("None");
    }
    let state = if jam || cover_open || multi_feed || lamp_error || offline {
        "Stopped"
    } else if scanning || busy {
        "Processing"
    } else {
        "Idle"
    };
    (state.to_string(), reasons)
}

/// SOAP body for `wscn:ScannerStatusSummaryEvent` (StatusSummary + optional reasons).
pub fn status_summary_event_body(state: &str, reasons: &[&str]) -> String {
    let mut summary =
        Element::new("wscn:StatusSummary").with(Element::text_el("wscn:ScannerState", state));
    if !reasons.is_empty() {
        let mut block = Element::new("wscn:ScannerStateReasons");
        for r in reasons {
            block = block.with(Element::text_el("wscn:ScannerStateReason", *r));
        }
        summary = summary.with(block);
    }
    Element::new("wscn:ScannerStatusSummaryEvent")
        .with(summary)
        .to_xml()
}

fn maybe_emit(world: &World, scan_ns: &str, short_action: &str, body_inner: &str) {
    let Some(notify_to) = world.event_notify_to().map(str::to_owned) else {
        return;
    };
    let subscriber = world.notify_peer();
    let Ok(target) = notify::accept_notify_target(&notify_to, subscriber) else {
        warn!(%notify_to, "job event NotifyTo refused");
        return;
    };
    let service_ep = world.service_endpoint().to_string();
    let scan_ns = scan_ns.to_string();
    let action_short = short_action.to_string();
    let body_inner = body_inner.to_string();
    let to = target.url();
    tokio::spawn(async move {
        let action = soap::scan_action(&scan_ns, &action_short);
        let envelope = event_envelope(&scan_ns, &action, &to, &service_ep, &body_inner);
        match notify::post_soap_event(&target, &action_short, &envelope).await {
            Ok(()) => info!(notify_to = %to, action = %action_short, "job event notification sent"),
            Err(e) => {
                warn!(notify_to = %to, action = %action_short, error = %e, "job event notify failed")
            }
        }
    });
}

/// Build `wscn:JobStatusEvent` body (schema JobStatus + lab Images* counters).
pub fn job_status_event_body(job: &JobEventSnap) -> String {
    let status = Element::new("wscn:JobStatus")
        .with(Element::text_el("wscn:JobId", job.job_id.to_string()))
        .with(Element::text_el("wscn:JobState", &job.state))
        .with(
            Element::new("wscn:JobStateReasons")
                .with(Element::text_el("wscn:JobStateReason", &job.reason)),
        )
        .with(Element::text_el(
            "wscn:ScansCompleted",
            job.images_completed.to_string(),
        ))
        .with(Element::text_el(
            "wscn:ImagesCompleted",
            job.images_completed.to_string(),
        ))
        .with(Element::text_el(
            "wscn:ImagesToTransfer",
            job.images_to_transfer.to_string(),
        ));
    Element::new("wscn:JobStatusEvent").with(status).to_xml()
}

/// Build `wscn:JobEndStateEvent` body (WDPScan.xsd JobEndStateType).
pub fn job_end_state_event_body(job: &JobEventSnap) -> String {
    let end = Element::new("wscn:JobEndState")
        .with(Element::text_el("wscn:JobId", job.job_id.to_string()))
        .with(Element::text_el("wscn:JobCompletedState", &job.state))
        .with(
            Element::new("wscn:JobCompletedStateReasons")
                .with(Element::text_el("wscn:JobStateReason", &job.reason)),
        )
        .with(Element::text_el("wscn:JobName", "fakescan"))
        .with(Element::text_el("wscn:JobOriginatingUserName", "fakescan"))
        .with(Element::text_el(
            "wscn:ScansCompleted",
            job.images_completed.to_string(),
        ));
    Element::new("wscn:JobEndStateEvent").with(end).to_xml()
}

fn event_envelope(
    scan_ns: &str,
    action_uri: &str,
    notify_to: &str,
    service_ep: &str,
    body_xml: &str,
) -> String {
    let mut resp = soap::Response::new()
        .scan_ns(scan_ns)
        .action(action_uri)
        .to(notify_to.trim())
        .from_address(service_ep)
        .eventing_xmlns(true)
        .message_id(format!("urn:uuid:{}", uuid::Uuid::new_v4()));
    for el in soap::parse_body_children(body_xml) {
        resp = resp.body(el);
    }
    resp.to_xml()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_processing() -> JobEventSnap {
        JobEventSnap {
            job_id: 7,
            state: "Processing".into(),
            reason: "None".into(),
            images_completed: 1,
            images_to_transfer: 2,
        }
    }

    fn sample_completed() -> JobEventSnap {
        JobEventSnap {
            job_id: 7,
            state: "Completed".into(),
            reason: "JobCompletedSuccessfully".into(),
            images_completed: 3,
            images_to_transfer: 0,
        }
    }

    #[test]
    fn job_status_event_xml_shape() {
        let xml = job_status_event_body(&sample_processing());
        assert!(xml.contains("<wscn:JobStatusEvent>"), "{xml}");
        assert!(xml.contains("<wscn:JobId>7</wscn:JobId>"), "{xml}");
        assert!(
            xml.contains("<wscn:JobState>Processing</wscn:JobState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScansCompleted>1</wscn:ScansCompleted>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ImagesCompleted>1</wscn:ImagesCompleted>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>"),
            "{xml}"
        );
    }

    #[test]
    fn job_end_state_event_xml_shape() {
        let xml = job_end_state_event_body(&sample_completed());
        assert!(xml.contains("<wscn:JobEndStateEvent>"), "{xml}");
        assert!(xml.contains("<wscn:JobEndState>"), "{xml}");
        assert!(
            xml.contains("<wscn:JobCompletedState>Completed</wscn:JobCompletedState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:JobStateReason>JobCompletedSuccessfully</wscn:JobStateReason>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScansCompleted>3</wscn:ScansCompleted>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:JobName>fakescan</wscn:JobName>"),
            "{xml}"
        );
    }

    #[test]
    fn job_end_canceled_and_aborted_reasons() {
        let canceled = JobEventSnap {
            job_id: 2,
            state: "Canceled".into(),
            reason: "JobCanceledAtDevice".into(),
            images_completed: 0,
            images_to_transfer: 1,
        };
        let xml = job_end_state_event_body(&canceled);
        assert!(xml.contains("Canceled"), "{xml}");
        assert!(xml.contains("JobCanceledAtDevice"), "{xml}");

        let aborted = JobEventSnap {
            job_id: 3,
            state: "Aborted".into(),
            reason: "JobTimedOut".into(),
            images_completed: 0,
            images_to_transfer: 0,
        };
        let xml = job_end_state_event_body(&aborted);
        assert!(xml.contains("Aborted"), "{xml}");
        assert!(xml.contains("JobTimedOut"), "{xml}");
    }

    #[test]
    fn condition_event_bodies_raise_and_clear() {
        let raised = condition_event_body(1, "MediaJam", "ADF", "Critical", true);
        assert!(raised.contains("ScannerStatusConditionEvent"), "{raised}");
        assert!(
            raised.contains(r#"<wscn:DeviceCondition Id="1">"#),
            "{raised}"
        );
        let time = raised
            .split_once("<wscn:Time>")
            .and_then(|(_, rest)| rest.split_once("</wscn:Time>"))
            .map(|(t, _)| t)
            .expect("Time element");
        assert!(
            crate::timeutil::is_utc_iso(time),
            "Time should be YYYY-MM-DDTHH:MM:SSZ, got {time:?} in {raised}"
        );
        assert!(
            raised.contains("<wscn:Name>MediaJam</wscn:Name>"),
            "{raised}"
        );
        assert!(
            raised.contains("<wscn:Component>ADF</wscn:Component>"),
            "{raised}"
        );
        assert!(
            raised.contains("<wscn:Severity>Critical</wscn:Severity>"),
            "{raised}"
        );
        assert!(!raised.contains("Cleared"), "{raised}");
        assert!(!raised.contains("<wscn:Condition>"), "{raised}");

        let cleared = condition_event_body(2, "CoverOpen", "Platen", "Critical", false);
        let warn = condition_event_body(8, "LampWarning", "Platen", "Warning", true);
        assert!(
            warn.contains("<wscn:Severity>Warning</wscn:Severity>"),
            "{warn}"
        );
        let info = condition_event_body(5, "Calibrating", "Platen", "Informational", true);
        assert!(
            info.contains("<wscn:Severity>Informational</wscn:Severity>"),
            "{info}"
        );
        assert!(
            cleared.contains("ScannerStatusConditionClearedEvent"),
            "{cleared}"
        );
        assert!(
            cleared.contains("<wscn:DeviceConditionCleared>"),
            "{cleared}"
        );
        assert!(
            cleared.contains("<wscn:ConditionId>2</wscn:ConditionId>"),
            "{cleared}"
        );
        let clear_time = cleared
            .split_once("<wscn:ConditionClearTime>")
            .and_then(|(_, rest)| rest.split_once("</wscn:ConditionClearTime>"))
            .map(|(t, _)| t)
            .expect("ConditionClearTime element");
        assert!(
            crate::timeutil::is_utc_iso(clear_time),
            "ConditionClearTime should be YYYY-MM-DDTHH:MM:SSZ, got {clear_time:?} in {cleared}"
        );
        assert!(!cleared.contains("<wscn:Name>"), "{cleared}");
        assert!(!cleared.contains("<wscn:Condition>"), "{cleared}");
    }

    #[test]
    fn elements_change_event_body_shape() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-wsd").expect("profile");
        let xml = elements_change_event_body(&profile);
        assert!(xml.contains("<wscn:ScannerElementsChangeEvent>"), "{xml}");
        assert!(xml.contains("<wscn:ElementChanges>"), "{xml}");
        assert!(xml.contains("<wscn:ScannerDescription>"), "{xml}");
        assert!(xml.contains("<wscn:ScannerName"), "{xml}");
        assert!(xml.contains("<wscn:ScannerConfiguration>"), "{xml}");
        assert!(xml.contains("</wscn:ScannerConfiguration>"), "{xml}");
        assert!(xml.contains("<wscn:DefaultScanTicket>"), "{xml}");
        assert!(xml.contains("</wscn:DefaultScanTicket>"), "{xml}");
        assert!(xml.contains("<wscn:FormatValue>"), "{xml}");
        assert!(
            xml.contains("<wscn:ColorProcessing>"),
            "expected DefaultScanTicket ColorProcessing: {xml}"
        );
        // Full config (not the old FormatsSupported+PlatenColor subset).
        assert!(
            xml.contains("<wscn:PlatenOpticalResolution>")
                || xml.contains("<wscn:ADFFront>")
                || xml.contains("<wscn:BrightnessSupported>"),
            "expected full ScannerConfiguration markers: {xml}"
        );
        // ElementChanges child order: Description → Configuration → DefaultScanTicket.
        let desc = xml
            .find("<wscn:ScannerDescription>")
            .unwrap_or_else(|| panic!("{xml}"));
        let config = xml
            .find("<wscn:ScannerConfiguration>")
            .unwrap_or_else(|| panic!("{xml}"));
        let ticket = xml
            .find("<wscn:DefaultScanTicket>")
            .unwrap_or_else(|| panic!("{xml}"));
        assert!(desc < config && config < ticket, "{xml}");
        assert!(xml.contains("</wscn:ElementChanges>"), "{xml}");
        assert!(xml.contains("</wscn:ScannerElementsChangeEvent>"), "{xml}");
    }

    #[test]
    fn status_summary_event_body_idle_none() {
        let xml = status_summary_event_body("Idle", &["None"]);
        assert!(xml.contains("<wscn:ScannerStatusSummaryEvent>"), "{xml}");
        assert!(xml.contains("<wscn:StatusSummary>"), "{xml}");
        assert!(
            xml.contains("<wscn:ScannerState>Idle</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(xml.contains("<wscn:ScannerStateReasons>"), "{xml}");
        assert!(
            xml.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(xml.contains("</wscn:StatusSummary>"), "{xml}");
        assert!(xml.contains("</wscn:ScannerStatusSummaryEvent>"), "{xml}");
    }

    #[test]
    fn status_summary_event_body_stopped_jam() {
        let xml = status_summary_event_body("Stopped", &["MediaJam"]);
        assert!(
            xml.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>MediaJam</wscn:ScannerStateReason>"),
            "{xml}"
        );
    }

    #[test]
    fn status_summary_state_mirrors_elements_logic() {
        let (state, reasons) = status_summary_state(true, false, false, false, false, false, false);
        assert_eq!(state, "Stopped");
        assert_eq!(reasons, vec!["MediaJam"]);

        let (state, reasons) = status_summary_state(false, false, false, false, true, false, false);
        assert_eq!(state, "Processing");
        assert_eq!(reasons, vec!["None"]);

        let (state, reasons) = status_summary_state(false, false, false, false, false, true, false);
        assert_eq!(state, "Stopped");
        assert_eq!(reasons, vec!["AttentionRequired"]);

        let (state, reasons) =
            status_summary_state(false, false, false, false, false, false, false);
        assert_eq!(state, "Idle");
        assert_eq!(reasons, vec!["None"]);
    }

    #[test]
    fn status_summary_from_world_processing_with_active_job() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-wsd").expect("profile");
        let mut w = World::new(dir, profile);
        w.set_pages(1);
        let _job = w
            .create_job(
                r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create");
        assert!(w.status().active_jobs > 0);
        let (state, reasons) = status_summary_from_world(&w);
        assert_eq!(state, "Processing");
        assert_eq!(reasons, vec!["None"]);
    }

    #[test]
    fn event_envelope_targets_notify_to() {
        let body = job_status_event_body(&sample_processing());
        let env = event_envelope(
            soap::SCAN_NS,
            &soap::scan_action(soap::SCAN_NS, "JobStatusEvent"),
            "http://127.0.0.1:5357/sink",
            "http://127.0.0.1:3920/ScannerService",
            &body,
        );
        assert!(
            env.contains("<wsa:To>http://127.0.0.1:5357/sink</wsa:To>"),
            "{env}"
        );
        assert!(env.contains("/wdp/scan/JobStatusEvent"), "{env}");
        assert!(env.contains("JobStatusEvent"), "{env}");
        assert!(
            super::super::envelope::Envelope::parse(&env)
                .reply_problems(super::super::envelope::ReplyRole::Event)
                .is_empty(),
            "{env}"
        );
    }

    #[test]
    fn from_job_maps_scan_job_fields() {
        let job = ScanJob {
            id: 9,
            token: "urn:uuid:x".into(),
            format: "jfif".into(),
            input_source: "Platen".into(),
            dpi: 300,
            color: crate::effects::ColorMode::Rgb24,
            width_px: 100,
            height_px: 100,
            pages_left: 0,
            page_index: 1,
            sheet_base: 0,
            stack_total: 0,
            sheets_taken: 0,
            duplex: false,
            created_at: std::time::Instant::now(),
            last_activity: std::time::Instant::now(),
            state_reason: "JobCompletedSuccessfully".into(),
            aborted: false,
            format_used_default: false,
            format_overridden: false,
            dpi_used_default: false,
            dpi_overridden: false,
        };
        let snap = JobEventSnap::from_job(&job);
        assert_eq!(snap.job_id, 9);
        assert_eq!(snap.state, "Completed");
        assert_eq!(snap.images_completed, 1);
        assert_eq!(snap.images_to_transfer, 0);
        let canceled = JobEventSnap::canceled(&job);
        assert_eq!(canceled.state, "Canceled");
        assert_eq!(canceled.reason, "JobCanceledAtDevice");
    }
}
