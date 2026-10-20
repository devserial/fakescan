use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::discovery::DiscoveryStats;
use crate::effects::{ColorMode, EffectsBody, EffectsSummary, ImageEffects};
use crate::image_gen::{self, PageSide, PatternId};
use crate::profile::Profile;
use crate::scenarios::{self, Scenario};
use crate::wsd::soap::Envelope;
use crate::wsd::xml::{self, Element};

mod faultplan;
pub use faultplan::{
    Cut, FaultKind, LiveCut, PlanBody, PlanEntryView, PlanIn, Trigger, WhenIn, WhenObj,
};

/// Sheet index order for RetrieveImage (manual duplex flip pass uses `Reverse`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PageOrder {
    #[default]
    Forward,
    Reverse,
}

impl PageOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            PageOrder::Forward => "forward",
            PageOrder::Reverse => "reverse",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "forward" | "fwd" | "" => Ok(PageOrder::Forward),
            "reverse" | "rev" | "flipped" => Ok(PageOrder::Reverse),
            other => bail!("unknown order {other:?}; expected forward or reverse"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ScanJob {
    pub id: i32,
    pub token: String,
    pub format: String,
    pub input_source: String,
    pub dpi: u32,
    /// ColorProcessing from the ScanTicket (`RGB24` / `Gray8`).
    pub color: ColorMode,
    pub width_px: u32,
    pub height_px: u32,
    pub pages_left: u32,
    pub page_index: u32,
    /// Feeder sheets already consumed by earlier jobs when this one was created
    /// (first sheet index this job reads; 0 on a freshly loaded stack and for glass).
    pub sheet_base: u32,
    /// Sheets in the loaded stack at Create (for `order=reverse`); equals the job's own
    /// sheet count when the job takes the whole stack.
    pub stack_total: u32,
    /// Feeder sheets this job took from the tray at Create (0 for glass). A cancel or a timeout puts the ones it
    /// never fed back ([`Self::unfed_sheets`]): a real feeder keeps the sheets it did not pull.
    pub sheets_taken: u32,
    /// Hardware ADF duplex: each sheet yields front then back (2 faces).
    pub duplex: bool,
    /// When CreateScanJob accepted the ticket (JobTimedOut clock).
    pub created_at: Instant,
    /// Last successful Create or RetrieveImage; idle past [`World::job_timeout`] → JobTimedOut.
    pub last_activity: Instant,
    /// WKV `JobStateReason` (`None`, `JobScanningAndTransfering`, `JobTimedOut`, …).
    pub state_reason: String,
    /// True after abort (timeout tombstone until lazy sweep).
    pub aborted: bool,
    /// DocumentFinalParameters: client omitted Format → UsedDefault.
    pub format_used_default: bool,
    /// DocumentFinalParameters: device replaced requested Format → Override.
    pub format_overridden: bool,
    /// DocumentFinalParameters: client omitted Resolution → UsedDefault.
    pub dpi_used_default: bool,
    /// DocumentFinalParameters: device replaced requested dpi → Override.
    pub dpi_overridden: bool,
}

/// Default Create→Retrieve / between-Retrieve idle limit (MS CreateScanJobResponse).
pub const DEFAULT_JOB_TIMEOUT: Duration = Duration::from_secs(60);
/// Default delay before `empty_feeder_brother` refuses a feeder job (measured ~10 s on the Brother).
pub const DEFAULT_EMPTY_FEEDER_DELAY_MS: u64 = 10_000;

/// Longest control-plane sleep (`/v1/slow` `ms`, lamp warmup, empty-feeder delay).
///
/// Those sleeps run on a request thread, and `/v1/slow` holds the device lock for the whole
/// wait. The control API is reachable from every interface the scan service binds, so an
/// unbounded value would stall the device indefinitely.
pub const MAX_CONTROL_DELAY_MS: u64 = 120_000;

/// `Err` when a control-plane sleep exceeds [`MAX_CONTROL_DELAY_MS`].
pub fn reject_overlong_delay(field: &str, ms: u64) -> Result<()> {
    if ms > MAX_CONTROL_DELAY_MS {
        bail!("{field}: {ms} ms exceeds {MAX_CONTROL_DELAY_MS}");
    }
    Ok(())
}

fn bounded_ms(ms: u64) -> u64 {
    ms.min(MAX_CONTROL_DELAY_MS)
}

/// Delays on a [`FaultBody`] that would block a request thread past [`MAX_CONTROL_DELAY_MS`].
pub fn check_fault_delays(body: &FaultBody) -> Result<()> {
    if let Some(ms) = body.ms {
        reject_overlong_delay("ms", ms)?;
    }
    if let Some(ms) = body.lamp_warmup_ms {
        reject_overlong_delay("lamp_warmup_ms", ms)?;
    }
    if let Some(ms) = body.empty_feeder_delay_ms {
        reject_overlong_delay("empty_feeder_delay_ms", ms)?;
    }
    Ok(())
}

/// `POST /v1/slow` `ms` past [`MAX_CONTROL_DELAY_MS`].
pub fn check_slow(body: &SlowBody) -> Result<()> {
    reject_overlong_delay("ms", body.ms)
}

impl ScanJob {
    /// Face position within the loaded feeder stack (faces consumed by earlier jobs of the
    /// same stack plus this job's delivered pages). Equals `page_index` for the first job on a
    /// stack and for glass, so fault-plan `page` / `after_pages` keep their per-scan meaning
    /// there while a stack scanned as several one-page jobs still counts 1, 2, 3.
    pub fn stack_position(&self) -> u32 {
        self.sheet_base * if self.duplex { 2 } else { 1 } + self.page_index
    }

    /// Sheets taken at Create that were never fed: a duplex sheet whose front was delivered is fed.
    pub fn unfed_sheets(&self) -> u32 {
        let fed = if self.duplex {
            self.page_index.div_ceil(2)
        } else {
            self.page_index
        };
        self.sheets_taken.saturating_sub(fed)
    }

    pub fn job_state(&self) -> &'static str {
        if self.aborted {
            "Aborted"
        } else if self.pages_left == 0 {
            "Completed"
        } else if self.page_index == 0 {
            "Pending"
        } else {
            "Processing"
        }
    }
}

/// Resolve idle job timeout: `FAKESCAN_JOB_TIMEOUT_SECS` (default 60).
fn job_timeout_from_env() -> Duration {
    match std::env::var("FAKESCAN_JOB_TIMEOUT_SECS") {
        Ok(s) => {
            let s = s.trim();
            if s.is_empty() {
                return DEFAULT_JOB_TIMEOUT;
            }
            match s.parse::<u64>() {
                Ok(secs) => Duration::from_secs(secs),
                Err(_) => DEFAULT_JOB_TIMEOUT,
            }
        }
        Err(_) => DEFAULT_JOB_TIMEOUT,
    }
}

#[derive(Debug)]
pub struct World {
    profiles_dir: PathBuf,
    profile: Profile,
    feeder_ready: bool,
    /// Lab instance name from `--lab-name` / `FAKESCAN_NAME` (for `save` + `GET /v1/ctl`).
    lab_name: Option<String>,
    /// Bound listen address (set after `bind_listener`; used by `save` + `GET /v1/ctl`).
    lab_listen: Option<std::net::SocketAddr>,
    /// Synthetic pages available in the feeder (0 = empty / glass-only).
    pages_remaining: u32,
    /// Per-device override of `profile.feeder.initial_pages` (`DeviceConfig` / daemon flag).
    /// Survives reset and profile reloads; `None` = use the profile's value.
    initial_pages_override: Option<u32>,
    /// Sheets in the stack at the last load (`set_pages`); consumed so far =
    /// `feeder_loaded - pages_remaining`. A job takes only the sheets it asked for.
    feeder_loaded: u32,
    /// Paper jam fault (wired via `/v1/fault` / `/v1/condition`).
    jam: bool,
    /// Cover / interlock open fault (wired via `/v1/fault` / `/v1/condition`).
    cover_open: bool,
    /// Double-feed / multi-pick (ScannerStateReason `MultipleFeedError`).
    multi_feed: bool,
    /// Lamp failure (ScannerStateReason `LampError`; WIA often `0x80210011`).
    lamp_error: bool,
    /// Device busy — refuse Subscribe/Create/Retrieve; Status stays `Processing`.
    busy: bool,
    /// Sticky offline (Subscribe + Create/Retrieve refuse; WIA Connect → 0x80210005).
    offline: bool,
    /// Lab sticky: ScannerStateReason `Calibrating` + DeviceCondition Id=5.
    calibrating: bool,
    /// Lab sticky: `InterlockOpen` (Id=6, MediaPath, Critical).
    interlock_open: bool,
    /// Lab sticky: `InternalStorageFull` (Id=7, Platen, Critical).
    storage_full: bool,
    /// Lab sticky: ScannerStateReason `Paused` only (no ConditionName); state Processing.
    paused: bool,
    /// Lab sticky: Condition `LampWarning` (Id=8) + reason `AttentionRequired`.
    lamp_warning: bool,
    /// Transient / lab sticky: ScannerStateReason `LampWarming` (set during warmup sleep).
    lamp_warming: bool,
    /// Lab: Create/Retrieve refuse with `ServerErrorTemporaryError` (retry later).
    temporary_error: bool,
    /// Lab: Create/Retrieve refuse with `ServerErrorInternalError` (permanent).
    internal_error: bool,
    /// Artificial delay inside `retrieve_image` (STOP_FEEDER / cancel races).
    slow_retrieve_ms: u64,
    /// Cap RetrieveImage MTOM body send rate (bytes/sec). `0` = off (send at once).
    /// Lab: progressive WIA / pdfcraft buffer fill. Keeps `Content-Length` (chunked
    /// framing without length hangs wsdscdrv ~30s).
    retrieve_bps: u64,
    /// After this many images transferred on a job, next RetrieveImage sets jam and fails (0 = off).
    jam_after_n: u32,
    /// After this many images transferred, RetrieveImage returns NoImagesAvailable early (0 = off).
    empty_mid_job: u32,
    /// After this many images, next RetrieveImage sets offline and fails (0 = off).
    offline_after_n: u32,
    /// After this many images, next RetrieveImage sets multi_feed and fails (0 = off).
    multi_feed_after_n: u32,
    /// General fault plan (`POST /v1/faults/plan`).
    plan: faultplan::PlanState,
    /// Cut the next RetrieveImage MTOM body mid-send (TCP stream error). Sticky until reset.
    abort_retrieve: bool,
    /// After this many images already transferred, next RetrieveImage aborts mid-MTOM (0 = off).
    abort_retrieve_after_n: u32,
    /// Bytes of the MTOM body to send before abort (`0` = half the body).
    abort_retrieve_bytes: u64,
    /// Extra delay on the first RetrieveImage of each job (lamp warm-up), ms (0 = off).
    lamp_warmup_ms: u64,
    /// Brother empty-feeder emulation armed (refuse feeder jobs as busy after a delay).
    empty_feeder_brother: bool,
    empty_feeder_delay_ms: u64,
    /// Set by `create_job` when it refused for the empty feeder; the WSD handler takes it,
    /// drops the world lock and sleeps this many ms before answering.
    empty_feeder_refusal_ms: Option<u64>,
    /// Synthetic JPEG layout for RetrieveImage (`POST /v1/pattern`).
    image_pattern: PatternId,
    /// Post-pattern defects (`POST /v1/effects`).
    effects: ImageEffects,
    /// Active tester progression step id (`POST /v1/scenario`), if any.
    scenario_id: Option<String>,
    /// Forced front/back tagging for the loaded feeder stack (`POST /v1/pages` `side`).
    page_side: Option<PageSide>,
    /// Sheet index order for RetrieveImage (`POST /v1/pages` `order`).
    page_order: PageOrder,
    /// Inject JPEG COM markers on JFIF RetrieveImage (`POST /v1/jpeg-com`).
    jpeg_com: bool,
    /// Hard loopback page store: uploaded blobs echoed on RetrieveImage (`POST /v1/loopback`).
    /// When non-empty, synthetic patterns/effects are skipped for those sheets.
    loopback_pages: Vec<Vec<u8>>,
    next_job_id: i32,
    jobs: HashMap<i32, ScanJob>,
    /// Idle limit after CreateScanJob / between RetrieveImage calls (default 60s).
    job_timeout: Duration,
    /// Device metadata URL advertised in WS-Discovery XAddrs (WS-Transfer Get).
    device_endpoint: String,
    /// Hosted Scan Service URL in Relationship metadata / scan SOAP.
    service_endpoint: String,
    device_uuid: Uuid,
    discovery: Arc<DiscoveryStats>,
    /// Recent WSD calls (`GET /v1/trace`). Connect that never talks to us leaves this still.
    trace_seq: u64,
    trace: VecDeque<TraceEvent>,
    /// Set by the binary. `shutdown` asks the process to leave the port.
    exit_tx: Option<tokio::sync::watch::Sender<bool>>,
    /// DestinationToken → ClientContext from WS-Eventing Subscribe (device-push).
    destinations: HashMap<String, String>,
    /// Most recently registered DestinationToken (ctl `push_scan` default).
    last_destination_token: Option<String>,
    /// NotifyTo Address from the last Subscribe (ScanAvailableEvent sink).
    notify_to: Option<String>,
    /// TCP peer that registered [`Self::notify_to`]. Events are posted only back to that host.
    notify_peer: Option<std::net::IpAddr>,
    /// Last granted `wse:Expires` from Subscribe / Renew (WS-Eventing GetStatus).
    subscription_expires: Option<String>,
    /// `wse:Identifier` from the last Subscribe (`uuid:…`).
    subscription_id: Option<String>,
    /// When `subscription_expires` was a duration (`PT…` / `P1D` / `P1DT…`), wall-clock end.
    subscription_deadline: Option<Instant>,
    /// ScanIdentifier → DestinationToken for a pending ScanAvailableEvent (lab/ctl).
    pending_push: HashMap<String, String>,
    /// Completed / aborted / cancelled jobs for GetJobHistory (newest last; capped).
    job_history: Vec<JobHistoryEntry>,
    /// Cleared sticky DeviceCondition rows for GetScannerElements ConditionHistory (newest last; capped).
    condition_history: Vec<ConditionHistoryEntry>,
    /// Raise-time ISO for active sticky conditions (Ids 1–4), keyed by Id.
    condition_raised_at: HashMap<u32, String>,
    /// Drained by the SOAP layer after RetrieveImage for NotifyTo (mid-job faults).
    pending_condition_changes: Vec<ConditionChange>,
    /// Ring of every Validate/Create ticket seen (`GET /v1/tickets`).
    tickets: crate::tickets::TicketLog,
}

/// One finished job row for `GetJobHistory`.
#[derive(Debug, Clone, Serialize)]
pub struct JobHistoryEntry {
    pub id: i32,
    pub state: String,
    pub reason: String,
    pub format: String,
    pub dpi: u32,
}

/// Lab view of a pending device-push scan (after `ctl push_scan` / `mint_push_scan`).
#[derive(Debug, Clone, Serialize)]
pub struct PendingPushScan {
    pub scan_identifier: String,
    pub destination_token: String,
    pub client_context: String,
    /// Subscribe NotifyTo URL, when present (ctl POSTs ScanAvailableEvent best-effort).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_to: Option<String>,
}

const JOB_HISTORY_MAX: usize = 32;
const CONDITION_HISTORY_MAX: usize = 16;

/// Cleared sticky fault for GetScannerElements `ConditionHistory` (DeviceCondition + ClearTime).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionHistoryEntry {
    pub id: u32,
    pub name: &'static str,
    pub component: &'static str,
    pub severity: &'static str,
    /// When the condition was raised (`wscn:Time`).
    pub time_iso: String,
    /// When the condition cleared (`wscn:ClearTime`).
    pub clear_time_iso: String,
}

/// UTC wall-clock for ConditionHistory Time / ClearTime (matches elements Status Time).
fn condition_now_iso() -> String {
    crate::timeutil::utc_now_iso()
}

/// One recent call. `kind` is `wsd` (SOAP), `job` (scan task), or `ctl` (control plane).
#[derive(Debug, Clone, Serialize)]
pub struct TraceEvent {
    pub seq: u64,
    pub kind: String,
    pub peer: String,
    pub action: String,
    /// `ok` or `fault`.
    pub outcome: String,
    pub ms: u128,
    pub detail: String,
}

/// A scan job without its token.
#[derive(Debug, Clone, Serialize)]
pub struct JobView {
    pub id: i32,
    /// `Pending`, `Processing`, or `Completed`. Same words as GetActiveJobs.
    pub state: String,
    pub format: String,
    pub source: String,
    pub dpi: u32,
    pub color: String,
    pub pages_left: u32,
    pub page_index: u32,
    pub width_px: u32,
    pub height_px: u32,
}

#[derive(Debug, Clone, Serialize)]
pub struct JobsView {
    pub jobs: Vec<JobView>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TraceView {
    /// Highest seq assigned. Pass this as `?after=` to see only newer calls.
    pub seq: u64,
    pub events: Vec<TraceEvent>,
}

/// Feeder load refused by the profile's tray limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PagesError {
    OverCapacity { requested: u32, capacity: u32 },
}

impl std::fmt::Display for PagesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PagesError::OverCapacity {
                requested,
                capacity,
            } => write!(
                f,
                "cannot load {requested} sheets: feeder tray holds at most {capacity}"
            ),
        }
    }
}

impl std::error::Error for PagesError {}

#[derive(Debug, Clone, Serialize)]
pub struct Status {
    pub profile_id: String,
    pub connection: String,
    pub feeder_ready: bool,
    pub sticky_ready: bool,
    /// Profile advertises hardware ADF duplex (`ADFSupportsDuplex`).
    pub duplex: bool,
    pub pages_remaining: u32,
    /// Tray capacity in sheets (`feeder.capacity`); null = no tray limit.
    pub feeder_capacity: Option<u32>,
    /// Sheets a fresh device, `POST /v1/reset` and a profile reload load into the tray
    /// (device/daemon override, else `feeder.initial_pages`; clamped to capacity). 0 = empty.
    pub initial_pages: u32,
    pub jam: bool,
    pub cover_open: bool,
    /// Double-feed / multi-pick sticky flag (`MultipleFeedError`).
    pub multi_feed: bool,
    /// Lamp failure (`LampError`).
    pub lamp_error: bool,
    /// Device busy (Status `Processing`; job ops refuse).
    pub busy: bool,
    /// Sticky offline: Subscribe / CreateScanJob / RetrieveImage refuse (WIA Connect → 0x80210005).
    pub offline: bool,
    /// Lab sticky: `Calibrating` reason + DeviceCondition Id=5.
    pub calibrating: bool,
    /// Lab sticky: `InterlockOpen` (Id=6).
    pub interlock_open: bool,
    /// Lab sticky: `InternalStorageFull` (Id=7).
    pub storage_full: bool,
    /// Lab sticky: `Paused` reason only (Processing).
    pub paused: bool,
    /// Lab sticky: Condition `LampWarning` + reason `AttentionRequired`.
    pub lamp_warning: bool,
    /// Transient / lab: `LampWarming` reason (warmup sleep or `/v1/fault`).
    pub lamp_warming: bool,
    /// Lab: Create/Retrieve refuse `ServerErrorTemporaryError`.
    pub temporary: bool,
    /// Lab: Create/Retrieve refuse `ServerErrorInternalError`.
    pub internal: bool,
    pub slow_retrieve_ms: u64,
    /// RetrieveImage body throttle (bytes/sec). `0` = off.
    pub retrieve_bps: u64,
    /// 0 = off; jam on RetrieveImage after this many images already transferred.
    pub jam_after_n: u32,
    /// 0 = off; NoImagesAvailable after this many images already transferred.
    pub empty_mid_job: u32,
    /// 0 = off; go offline on RetrieveImage after this many images already transferred.
    pub offline_after_n: u32,
    /// 0 = off; multi-feed on RetrieveImage after this many images already transferred.
    pub multi_feed_after_n: u32,
    /// 0 = off; cover opens on RetrieveImage after this many images (plan sugar).
    pub cover_open_after_n: u32,
    /// Fault plan entries (pending and fired) plus read-only legacy `*_after_n` rows.
    pub fault_plan: Vec<PlanEntryView>,
    /// Cut RetrieveImage MTOM mid-send while armed.
    pub abort_retrieve: bool,
    /// 0 = off; abort MTOM after this many images already transferred.
    pub abort_retrieve_after_n: u32,
    /// Bytes to send before abort (`0` = half the assembled MTOM body).
    pub abort_retrieve_bytes: u64,
    /// Extra ms on first RetrieveImage of each job (0 = off).
    pub lamp_warmup_ms: u64,
    /// `empty_feeder_brother` armed (see [`FaultBody::empty_feeder_brother`]).
    pub empty_feeder_brother: bool,
    /// Delay before the empty-feeder refusal, ms.
    pub empty_feeder_delay_ms: u64,
    /// Active synthetic image pattern id (`page_id`, `wedges`, `duplex_pair`, …).
    pub image_pattern: String,
    /// Active image effects (skew, speckles, streaks, hole punches).
    pub effects: EffectsSummary,
    /// Active progression scenario id (`order`, `qr`, …), if any.
    pub scenario: Option<String>,
    /// 1-based step when a scenario is active.
    pub scenario_step: Option<u8>,
    pub scenario_of: Option<u8>,
    /// What the tester should verify for the active scenario.
    pub scenario_check: Option<String>,
    /// Next scenario id in [`crate::scenarios::PROGRESSION`], if any.
    pub scenario_next: Option<String>,
    /// Forced duplex side for the feeder stack (`front` / `back`), if any.
    pub page_side: Option<String>,
    /// RetrieveImage sheet order (`forward` / `reverse`).
    pub page_order: String,
    /// When true, JFIF payloads carry `fakescan-page:` COM markers.
    pub jpeg_com: bool,
    /// Number of hard-loopback pages loaded (`POST /v1/loopback`).
    pub loopback_pages: u32,
    /// Short fingerprints (`len:first8hex`) for each loopback page (ground-truth expect).
    pub loopback_digests: Vec<String>,
    pub refuse_unknown: bool,
    pub active_jobs: usize,
    /// Idle job timeout in milliseconds (Create→Retrieve / between Retrieves).
    pub job_timeout_ms: u64,
    /// Registered push-scan DestinationToken count (from Subscribe).
    pub destination_tokens: usize,
    /// Last granted WS-Eventing `Expires` (Subscribe / Renew), if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription_expires: Option<String>,
    /// Pending ScanIdentifier count (awaiting CreateScanJob).
    pub pending_push_scans: usize,
    /// Finished jobs retained for GetJobHistory.
    pub job_history_len: usize,
    pub optical: Option<crate::profile::Optical>,
    pub dpi: Vec<u32>,
    pub formats: Vec<String>,
    pub available_profiles: Vec<String>,
    /// WS-Discovery XAddrs / WS-Transfer Get target (Device endpoint).
    pub device_endpoint: String,
    /// Hosted ScannerService URL.
    pub service_endpoint: String,
    pub device_uuid: String,
    /// WS-Discovery Hellos successfully sent (UDP 3702 multicast).
    pub wsd_hellos_sent: u64,
    /// ProbeMatch replies sent (0 on Windows Hello-only unless `--probe-listen`).
    pub wsd_probes_answered: u64,
    /// `AppSequence/@InstanceId` for this process (boot-time seconds; 0 if WSD off).
    pub wsd_instance_id: u64,
    /// Discovery `MetadataVersion` on Hello / ProbeMatch / ResolveMatch.
    pub wsd_metadata_version: u64,
    pub note: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct FeederBody {
    pub ready: bool,
}

#[derive(Debug, Deserialize)]
pub struct PagesBody {
    pub count: u32,
    /// Optional duplex face: `front` or `back`. Omitting clears forced side.
    #[serde(default)]
    pub side: Option<String>,
    /// Optional sheet order: `forward` (default) or `reverse` (flip-stack backs).
    #[serde(default)]
    pub order: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SlowBody {
    /// Sleep before building the RetrieveImage body (cancel / busy labs). Default `0`.
    #[serde(default)]
    pub ms: u64,
    /// Throttle MTOM TCP send (bytes/sec) so WIA can report mid-page progress.
    /// Omit or `0` leaves the previous rate unchanged when only `ms` is set; send
    /// `"bps": 0` to clear. When the JSON includes `bps`, it always applies.
    #[serde(default)]
    pub bps: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct PatternBody {
    pub id: String,
}

/// Body for `POST /v1/scenario` — apply one progression step by id.
#[derive(Debug, Deserialize)]
pub struct ScenarioBody {
    pub id: String,
}

/// Body for `POST /v1/jpeg-com` — toggle JFIF COM markers on RetrieveImage.
#[derive(Debug, Deserialize)]
pub struct JpegComBody {
    /// `true` injects `fakescan-page:` / `fakescan-side:`; `false` omits them.
    pub enabled: bool,
}

/// Body for `POST /v1/loopback` — replace the hard page store with uploaded blobs.
///
/// Each entry is base64 of a complete image file (prefer JFIF). RetrieveImage
/// echoes bytes as-is for `jfif`/`exif`; other formats re-encode after decode.
#[derive(Debug, Deserialize)]
pub struct LoopbackBody {
    /// Base64-encoded page blobs in feeder order (empty clears the store).
    pub pages: Vec<String>,
}

/// Summary of the hard loopback store (`GET /v1/loopback`).
#[derive(Debug, Clone, Serialize)]
pub struct LoopbackStatus {
    pub count: u32,
    pub digests: Vec<String>,
    /// True when RetrieveImage will echo store bytes instead of synthesizing.
    pub active: bool,
}

/// Raised or cleared scanner condition for NotifyTo ScannerStatusCondition* events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionChange {
    /// Stable DeviceCondition Id (1–9). `0` = no DeviceCondition (e.g. offline
    /// `AttentionRequired`, `Paused`, `LampWarming` — StatusSummary only; skip Condition* NotifyTo).
    pub id: u32,
    /// WSD ConditionName / ScannerStateReason token (`MediaJam`, `CoverOpen`, …).
    pub name: &'static str,
    /// ConditionComponent (`ADF`, `Platen`, `MediaPath`). Empty when `id == 0`.
    pub component: &'static str,
    /// XSD `ConditionSeverity`: Critical / Warning / Informational. Empty when `id == 0`.
    pub severity: &'static str,
    /// `true` = raised (ConditionEvent); `false` = cleared.
    pub raised: bool,
}

/// Jam / cover (and optional retrieve delay) for `/v1/fault`, `/v1/faults`, `/v1/condition`.
#[derive(Debug, Deserialize, Default)]
pub struct FaultBody {
    pub jam: Option<bool>,
    pub cover_open: Option<bool>,
    /// Double-feed / multi-pick sticky (`MultipleFeedError`). Cleared by `reset` / `false`.
    pub multi_feed: Option<bool>,
    /// Lamp failure sticky (`LampError`). Cleared by `reset` / `false`.
    pub lamp_error: Option<bool>,
    /// Device busy sticky. Cleared by `reset` / `false`.
    pub busy: Option<bool>,
    /// Sticky offline (Subscribe + job ops refuse). Cleared by `reset` / `offline: false`.
    pub offline: Option<bool>,
    /// Sticky calibrating (`Calibrating` reason + DeviceCondition Id=5).
    pub calibrating: Option<bool>,
    /// Sticky interlock (`InterlockOpen` Id=6). Alias `interlock_open`.
    #[serde(alias = "interlock_open")]
    pub interlock: Option<bool>,
    /// Sticky internal storage full (`InternalStorageFull` Id=7).
    pub storage_full: Option<bool>,
    /// Sticky paused (`Paused` reason only; Processing). Cleared by `reset` / `false`.
    pub paused: Option<bool>,
    /// Sticky lamp warning (Condition `LampWarning` Id=8 + reason `AttentionRequired`).
    pub lamp_warning: Option<bool>,
    /// Sticky / lab lamp warming (`LampWarming` reason only). Also set during warmup sleep.
    pub lamp_warming: Option<bool>,
    /// Refuse Create/Retrieve with `ServerErrorTemporaryError`. Cleared by `reset` / `false`.
    pub temporary: Option<bool>,
    /// Refuse Create/Retrieve with `ServerErrorInternalError`. Cleared by `reset` / `false`.
    pub internal: Option<bool>,
    /// When set, also updates `slow_retrieve_ms` (same as `/v1/slow`).
    pub ms: Option<u64>,
    /// Jam after N successful image transfers on a job (`0` clears).
    pub jam_after_n: Option<u32>,
    /// End job early after N transfers (`0` clears).
    pub empty_mid_job: Option<u32>,
    /// Go offline after N successful transfers (`0` clears). Mid-scan drop.
    pub offline_after_n: Option<u32>,
    /// Multi-feed after N successful transfers (`0` clears).
    pub multi_feed_after_n: Option<u32>,
    /// Cover opens after N successful transfers (`0` clears). Sugar for a fault-plan entry.
    pub cover_open_after_n: Option<u32>,
    /// Cut RetrieveImage MTOM mid-send (sticky). Cleared by `reset` / `false`.
    pub abort_retrieve: Option<bool>,
    /// Abort MTOM after N successful transfers (`0` clears).
    pub abort_retrieve_after_n: Option<u32>,
    /// Bytes of MTOM body to send before the stream errors (`0` = half).
    pub abort_retrieve_bytes: Option<u64>,
    /// Lamp warm-up delay on first image of each job (`0` clears).
    pub lamp_warmup_ms: Option<u64>,
    /// Reproduce the real Brother with an EMPTY feeder: published status stays Idle with no
    /// conditions, a feeder job (0 sheets loaded) is refused with the `busy` fault
    /// (`ServerErrorNotAcceptingJobs`) after `empty_feeder_delay_ms`. Cleared by `reset` / `false`.
    pub empty_feeder_brother: Option<bool>,
    /// Delay before that refusal, ms (default 10000; `reset` restores the default).
    pub empty_feeder_delay_ms: Option<u64>,
}

/// Result of ValidateScanTicket against the loaded profile.
#[derive(Debug, Clone)]
pub struct TicketValidation {
    pub valid: bool,
    pub dpi: u32,
    pub format: String,
    pub color: ColorMode,
    pub reason: Option<String>,
}

impl World {
    pub fn new(profiles_dir: PathBuf, profile: Profile) -> Self {
        Self::with_discovery(profiles_dir, profile, Arc::new(DiscoveryStats::default()))
    }

    pub fn with_discovery(
        profiles_dir: PathBuf,
        profile: Profile,
        discovery: Arc<DiscoveryStats>,
    ) -> Self {
        let sticky = profile.feeder.sticky_ready;
        let jpeg_com = jpeg_com_initial(profile.jpeg_com);
        Self {
            profiles_dir,
            profile,
            lab_name: None,
            lab_listen: None,
            feeder_ready: sticky,
            pages_remaining: 0,
            initial_pages_override: None,
            feeder_loaded: 0,
            jam: false,
            cover_open: false,
            multi_feed: false,
            lamp_error: false,
            busy: false,
            offline: false,
            calibrating: false,
            interlock_open: false,
            storage_full: false,
            paused: false,
            lamp_warning: false,
            lamp_warming: false,
            temporary_error: false,
            internal_error: false,
            slow_retrieve_ms: 0,
            retrieve_bps: 0,
            jam_after_n: 0,
            empty_mid_job: 0,
            offline_after_n: 0,
            multi_feed_after_n: 0,
            plan: Default::default(),
            abort_retrieve: false,
            abort_retrieve_after_n: 0,
            abort_retrieve_bytes: 0,
            lamp_warmup_ms: 0,
            empty_feeder_brother: false,
            empty_feeder_delay_ms: DEFAULT_EMPTY_FEEDER_DELAY_MS,
            empty_feeder_refusal_ms: None,
            image_pattern: PatternId::default(),
            effects: ImageEffects::default(),
            scenario_id: None,
            page_side: None,
            page_order: PageOrder::Forward,
            jpeg_com,
            loopback_pages: Vec::new(),
            next_job_id: 1,
            jobs: HashMap::new(),
            job_timeout: job_timeout_from_env(),
            device_endpoint: "http://127.0.0.1:3920/WebServices/Device".into(),
            service_endpoint: "http://127.0.0.1:3920/ScannerService".into(),
            device_uuid: crate::lab::random_fake_uuid(),
            discovery,
            trace_seq: 0,
            trace: VecDeque::new(),
            exit_tx: None,
            destinations: HashMap::new(),
            last_destination_token: None,
            notify_to: None,
            notify_peer: None,
            subscription_expires: None,
            subscription_id: None,
            subscription_deadline: None,
            pending_push: HashMap::new(),
            job_history: Vec::new(),
            condition_history: Vec::new(),
            condition_raised_at: HashMap::new(),
            pending_condition_changes: Vec::new(),
            tickets: crate::tickets::TicketLog::default(),
        }
    }

    /// The scan-ticket ring (interior-locked; recording needs only `&World`).
    pub fn tickets(&self) -> &crate::tickets::TicketLog {
        &self.tickets
    }

    /// Record a DestinationToken issued in SubscribeResponse (device-push).
    pub fn register_destination(&mut self, token: &str, client_context: &str) {
        if token.is_empty() {
            return;
        }
        self.destinations
            .insert(token.to_string(), client_context.to_string());
        self.last_destination_token = Some(token.to_string());
        self.note(
            "wsd",
            "DestinationToken",
            "ok",
            &format!("token={token} context={client_context}"),
        );
    }

    /// Store the Subscribe `NotifyTo` Address for ScanAvailableEvent delivery.
    ///
    /// An empty URL clears the sink and the subscriber peer. Callers that accept a URL must
    /// also call [`Self::set_notify_subscriber`] with the TCP peer the event may be posted to.
    pub fn set_notify_to(&mut self, url: Option<&str>) {
        self.notify_to = url
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if self.notify_to.is_none() {
            self.notify_peer = None;
        }
        if let Some(u) = self.notify_to.clone() {
            self.note("wsd", "NotifyTo", "ok", &u);
        }
    }

    /// TCP address of the client that subscribed. Event delivery refuses any other host.
    pub fn set_notify_subscriber(&mut self, peer: Option<std::net::IpAddr>) {
        self.notify_peer = peer;
    }

    /// NotifyTo Address from the last Subscribe, if any.
    pub fn notify_to(&self) -> Option<&str> {
        self.notify_to.as_deref()
    }

    /// Subscriber address stored with [`Self::set_notify_subscriber`].
    pub fn notify_peer(&self) -> Option<std::net::IpAddr> {
        self.notify_peer
    }

    /// Alias for event emitters (`job_events` / NotifyTo POST).
    pub fn event_notify_to(&self) -> Option<&str> {
        self.notify_to()
    }

    /// Store the granted `wse:Expires` from Subscribe / Renew.
    pub fn set_subscription_expires(&mut self, expires: Option<&str>) {
        self.subscription_expires = expires
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(e) = self.subscription_expires.clone() {
            self.note("wsd", "Expires", "ok", &e);
        }
    }

    /// Last granted `wse:Expires`, if any.
    pub fn subscription_expires(&self) -> Option<&str> {
        self.subscription_expires.as_deref()
    }

    /// Store the Subscribe `wse:Identifier` (`uuid:…`).
    pub fn set_subscription_id(&mut self, id: Option<&str>) {
        self.subscription_id = id
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        if let Some(i) = self.subscription_id.clone() {
            self.note("wsd", "Identifier", "ok", &i);
        }
    }

    /// Last Subscribe `wse:Identifier`, if any.
    pub fn subscription_id(&self) -> Option<&str> {
        self.subscription_id.as_deref()
    }

    /// Store a duration-based subscription end (`None` = no Instant enforcement).
    pub fn set_subscription_deadline(&mut self, deadline: Option<Instant>) {
        self.subscription_deadline = deadline;
    }

    /// Duration-based end Instant from the last Subscribe / Renew, if any.
    pub fn subscription_deadline(&self) -> Option<Instant> {
        self.subscription_deadline
    }

    /// True when a duration deadline was set and `now` is at or past it.
    pub fn subscription_deadline_elapsed(&self, now: Instant) -> bool {
        self.subscription_deadline
            .map(|d| now >= d)
            .unwrap_or(false)
    }

    /// Drop DestinationTokens, pending push ids, NotifyTo, Expires, and Identifier.
    pub fn clear_subscriptions(&mut self) {
        self.destinations.clear();
        self.last_destination_token = None;
        self.pending_push.clear();
        self.notify_to = None;
        self.notify_peer = None;
        self.subscription_expires = None;
        self.subscription_id = None;
        self.subscription_deadline = None;
        self.note(
            "wsd",
            "Unsubscribe",
            "ok",
            "cleared destinations and NotifyTo",
        );
    }

    /// Mint a ScanIdentifier for an existing DestinationToken (lab stand-in for ScanAvailableEvent).
    ///
    /// When `destination_token` is `None`, uses the most recently registered destination.
    pub fn mint_push_scan(&mut self, destination_token: Option<&str>) -> Result<PendingPushScan> {
        let token = match destination_token {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => self.last_destination_token.clone().ok_or_else(|| {
                anyhow!("no DestinationToken registered; Subscribe with ScanDestinations first")
            })?,
        };
        let context = self.destinations.get(&token).cloned().ok_or_else(|| {
            anyhow!("ClientErrorInvalidDestinationToken: destination token not registered")
        })?;
        let scan_identifier = format!("uuid:{}", Uuid::new_v4());
        self.pending_push
            .insert(scan_identifier.clone(), token.clone());
        self.note(
            "ctl",
            "push_scan",
            "ok",
            &format!("scan_id={scan_identifier} token={token}"),
        );
        Ok(PendingPushScan {
            scan_identifier,
            destination_token: token,
            client_context: context,
            notify_to: self.notify_to.clone(),
        })
    }

    /// Validate device-push CreateScanJob credentials and consume the ScanIdentifier.
    pub fn consume_push_credentials(
        &mut self,
        scan_identifier: &str,
        destination_token: &str,
    ) -> Result<()> {
        if scan_identifier.is_empty() || destination_token.is_empty() {
            bail!("InvalidArgs: ScanIdentifier and DestinationToken required together");
        }
        if !self.destinations.contains_key(destination_token) {
            bail!("ClientErrorInvalidDestinationToken: destination token not registered");
        }
        match self.pending_push.get(scan_identifier) {
            None => bail!("ClientErrorInvalidScanIdentifier: unknown or spent ScanIdentifier"),
            Some(expected) if expected != destination_token => {
                bail!("ClientErrorInvalidDestinationToken: token does not match ScanIdentifier");
            }
            Some(_) => {
                self.pending_push.remove(scan_identifier);
                Ok(())
            }
        }
    }

    pub fn job_history(&self) -> &[JobHistoryEntry] {
        &self.job_history
    }

    /// Cleared sticky conditions for GetScannerElements `ConditionHistory` (newest last).
    pub fn condition_history(&self) -> &[ConditionHistoryEntry] {
        &self.condition_history
    }

    fn push_job_history(&mut self, job: &ScanJob, state: &str, reason: &str) {
        self.job_history.push(JobHistoryEntry {
            id: job.id,
            state: state.into(),
            reason: reason.into(),
            format: job.format.clone(),
            dpi: job.dpi,
        });
        if self.job_history.len() > JOB_HISTORY_MAX {
            let drop = self.job_history.len() - JOB_HISTORY_MAX;
            self.job_history.drain(0..drop);
        }
    }

    fn push_condition_history(&mut self, entry: ConditionHistoryEntry) {
        self.condition_history.push(entry);
        if self.condition_history.len() > CONDITION_HISTORY_MAX {
            let drop = self.condition_history.len() - CONDITION_HISTORY_MAX;
            self.condition_history.drain(0..drop);
        }
    }

    /// Record raise time or append ConditionHistory on clear (Ids 1–9 with DeviceCondition).
    fn record_condition_transition(
        &mut self,
        id: u32,
        name: &'static str,
        component: &'static str,
        severity: &'static str,
        raised: bool,
    ) {
        if raised {
            self.condition_raised_at.insert(id, condition_now_iso());
            return;
        }
        let time_iso = self
            .condition_raised_at
            .remove(&id)
            .unwrap_or_else(condition_now_iso);
        self.push_condition_history(ConditionHistoryEntry {
            id,
            name,
            component,
            severity,
            time_iso,
            clear_time_iso: condition_now_iso(),
        });
    }

    /// Idle Create→Retrieve / between-Retrieve timeout (MS ~60s).
    pub fn job_timeout(&self) -> Duration {
        self.job_timeout
    }

    /// Override idle job timeout (tests / lab).
    pub fn set_job_timeout(&mut self, timeout: Duration) {
        self.job_timeout = timeout;
    }

    /// Backdate a job's activity clock (unit tests; avoids sleeping the real timeout).
    pub fn age_job_activity(&mut self, job_id: i32, age: Duration) -> Result<()> {
        let job = self
            .jobs
            .get_mut(&job_id)
            .ok_or_else(|| anyhow!("job {job_id} not found"))?;
        let at = Instant::now()
            .checked_sub(age)
            .ok_or_else(|| anyhow!("age {age:?} exceeds Instant clock"))?;
        job.last_activity = at;
        if job.created_at > at {
            job.created_at = at;
        }
        Ok(())
    }

    /// True when the job is still open and idle past [`Self::job_timeout`].
    pub fn job_is_timed_out(&self, job: &ScanJob, now: Instant) -> bool {
        if job.aborted || job.pages_left == 0 {
            return false;
        }
        now.duration_since(job.last_activity) >= self.job_timeout
    }

    /// Mark a job Aborted with `JobStateReason` = `JobTimedOut`.
    pub fn mark_job_timed_out(&mut self, job_id: i32) -> Result<()> {
        let job = self
            .jobs
            .get_mut(&job_id)
            .ok_or_else(|| anyhow!("job {job_id} not found"))?;
        let unfed = if job.aborted { 0 } else { job.unfed_sheets() };
        job.aborted = true;
        job.state_reason = "JobTimedOut".into();
        job.pages_left = 0;
        let snap = job.clone();
        self.return_sheets(unfed);
        self.push_job_history(&snap, "Aborted", "JobTimedOut");
        self.note(
            "job",
            "JobTimedOut",
            "fault",
            &format!("id={job_id} state=Aborted reason=JobTimedOut"),
        );
        Ok(())
    }

    /// Abort open jobs past the idle timeout and drop timed-out tombstones.
    pub fn sweep_timed_out_jobs(&mut self) -> usize {
        let now = Instant::now();
        let stale: Vec<i32> = self
            .jobs
            .iter()
            .filter(|(_, j)| self.job_is_timed_out(j, now))
            .map(|(id, _)| *id)
            .collect();
        let n = stale.len();
        for id in stale {
            let _ = self.mark_job_timed_out(id);
        }
        self.jobs
            .retain(|_, j| !(j.aborted && j.state_reason == "JobTimedOut"));
        n
    }

    /// Alias used by older call sites.
    pub fn expire_timed_out_jobs(&mut self) -> usize {
        self.sweep_timed_out_jobs()
    }

    /// If `job_id` is past the idle timeout, abort it and return OperationFailed.
    pub fn ensure_job_not_timed_out(&mut self, job_id: i32) -> Result<()> {
        let now = Instant::now();
        let timed_out = self
            .jobs
            .get(&job_id)
            .map(|j| {
                if j.aborted && j.state_reason == "JobTimedOut" {
                    true
                } else {
                    self.job_is_timed_out(j, now)
                }
            })
            .unwrap_or(false);
        if !timed_out {
            return Ok(());
        }
        if let Some(j) = self.jobs.get(&job_id) {
            if !(j.aborted && j.state_reason == "JobTimedOut") {
                self.mark_job_timed_out(job_id)?;
            }
        }
        bail!("OperationFailed: JobTimedOut on job {job_id}");
    }

    /// Arm the graceful exit used by `POST /v1/ctl` `shutdown`.
    pub fn arm_exit(&mut self, tx: tokio::sync::watch::Sender<bool>) {
        self.exit_tx = Some(tx);
    }

    /// Ask this process to stop: discovery Bye, then the listen port is released.
    pub fn request_exit(&self) -> Result<()> {
        let Some(tx) = &self.exit_tx else {
            bail!("this process cannot shut down from the control plane");
        };
        tx.send(true)
            .map_err(|_| anyhow!("shutdown already finished"))?;
        Ok(())
    }

    /// Record one activity line. The ring keeps the last 64.
    pub fn note(&mut self, kind: &str, action: &str, outcome: &str, detail: &str) {
        self.note_wsd(kind, "-", action, outcome, 0, detail);
    }

    /// Record one WSD HTTP call.
    pub fn note_wsd(
        &mut self,
        kind: &str,
        peer: &str,
        action: &str,
        outcome: &str,
        ms: u128,
        detail: &str,
    ) {
        self.trace_seq = self.trace_seq.saturating_add(1);
        self.trace.push_back(TraceEvent {
            seq: self.trace_seq,
            kind: kind.to_string(),
            peer: peer.to_string(),
            action: action.to_string(),
            outcome: outcome.to_string(),
            ms,
            detail: detail.chars().take(160).collect(),
        });
        while self.trace.len() > 64 {
            self.trace.pop_front();
        }
    }

    pub fn trace_since(&self, after: u64) -> TraceView {
        TraceView {
            seq: self.trace_seq,
            events: self
                .trace
                .iter()
                .filter(|event| event.seq > after)
                .cloned()
                .collect(),
        }
    }

    pub fn trace_seq(&self) -> u64 {
        self.trace_seq
    }

    /// Fill peer and elapsed time on the one event this call just recorded.
    ///
    /// CreateScanJob / RetrieveImage / CancelJob already append a `job` line.
    /// A second `wsd` line for the same call burns the 64-deep ring twice as
    /// fast. Returns false when this call did not record exactly that action,
    /// so the caller appends its own line.
    pub fn absorb_call(
        &mut self,
        seq_before: u64,
        action: &str,
        outcome: &str,
        peer: &str,
        ms: u128,
    ) -> bool {
        let mut found = None;
        for (index, event) in self.trace.iter().enumerate() {
            if event.seq <= seq_before {
                continue;
            }
            if found.is_some() {
                return false;
            }
            found = Some(index);
        }
        let Some(index) = found else {
            return false;
        };
        if self.trace[index].action != action || self.trace[index].outcome != outcome {
            return false;
        }
        let event = &mut self.trace[index];
        event.peer = peer.to_string();
        event.ms = ms;
        true
    }

    /// Plain-text activity, one line per call, for `GET /v1/log`.
    ///
    /// Fields are written as data. A detail that contains `{` must not be
    /// treated as a format string.
    pub fn log_since(&self, after: u64) -> String {
        let mut out = String::new();
        for event in self.trace_since(after).events {
            out.push_str(&event.seq.to_string());
            out.push(' ');
            out.push_str(&event.kind);
            out.push(' ');
            out.push_str(&event.action);
            out.push(' ');
            out.push_str(&event.outcome);
            out.push(' ');
            out.push_str(&event.ms.to_string());
            out.push_str("ms ");
            out.push_str(&event.detail);
            out.push('\n');
        }
        out
    }

    pub fn jobs_view(&self) -> JobsView {
        JobsView {
            jobs: self
                .jobs_snapshot()
                .into_iter()
                .map(|job| JobView {
                    id: job.id,
                    state: job.job_state().to_string(),
                    format: job.format.clone(),
                    source: job.input_source.clone(),
                    dpi: job.dpi,
                    color: job.color.wsd_token().to_string(),
                    pages_left: job.pages_left,
                    page_index: job.page_index,
                    width_px: job.width_px,
                    height_px: job.height_px,
                })
                .collect(),
        }
    }

    pub fn set_identity(
        &mut self,
        device_endpoint: String,
        service_endpoint: String,
        device_uuid: Uuid,
    ) {
        self.device_endpoint = device_endpoint;
        self.service_endpoint = service_endpoint;
        self.device_uuid = device_uuid;
    }

    /// Set lab instance name and bound listen address.
    ///
    /// Called from `main` once the port is known.  Required by `GET /v1/ctl`
    /// and `POST /v1/ctl save`.
    pub fn set_lab_identity(&mut self, name: Option<String>, listen: std::net::SocketAddr) {
        self.lab_name = name;
        self.lab_listen = Some(listen);
    }

    /// Lab instance name (`--lab-name` / `FAKESCAN_NAME`), if set.
    pub fn lab_name(&self) -> Option<&str> {
        self.lab_name.as_deref()
    }

    /// Bound listen address, if set via [`set_lab_identity`].
    pub fn lab_listen(&self) -> Option<std::net::SocketAddr> {
        self.lab_listen
    }

    /// TCP port this instance is listening on (falls back to `LAB_PORT`).
    pub fn lab_port(&self) -> u16 {
        self.lab_listen
            .map(|a| a.port())
            .unwrap_or(crate::lab::LAB_PORT)
    }

    /// Base URL for the control plane (`http://127.0.0.1:<port>`).
    pub fn lab_base_url(&self) -> String {
        if let Some(addr) = self.lab_listen {
            let ip = if addr.ip().is_unspecified() {
                "127.0.0.1".to_string()
            } else {
                addr.ip().to_string()
            };
            format!("http://{}:{}", ip, addr.port())
        } else {
            format!("http://127.0.0.1:{}", crate::lab::LAB_PORT)
        }
    }

    pub fn discovery_stats(&self) -> Arc<DiscoveryStats> {
        Arc::clone(&self.discovery)
    }

    pub fn device_endpoint(&self) -> &str {
        &self.device_endpoint
    }

    pub fn service_endpoint(&self) -> &str {
        &self.service_endpoint
    }

    pub fn device_uuid(&self) -> Uuid {
        self.device_uuid
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn feeder_ready(&self) -> bool {
        self.feeder_ready
    }

    pub fn pages_remaining(&self) -> u32 {
        self.pages_remaining
    }

    pub fn jam(&self) -> bool {
        self.jam
    }

    pub fn cover_open(&self) -> bool {
        self.cover_open
    }

    pub fn offline(&self) -> bool {
        self.offline
    }

    pub fn set_offline(&mut self, offline: bool) {
        self.offline = offline;
    }

    pub fn set_jam(&mut self, jam: bool) {
        self.jam = jam;
    }

    pub fn set_cover_open(&mut self, cover_open: bool) {
        self.cover_open = cover_open;
    }

    pub fn multi_feed(&self) -> bool {
        self.multi_feed
    }

    pub fn set_multi_feed(&mut self, multi_feed: bool) {
        self.multi_feed = multi_feed;
    }

    pub fn lamp_error(&self) -> bool {
        self.lamp_error
    }

    pub fn set_lamp_error(&mut self, lamp_error: bool) {
        self.lamp_error = lamp_error;
    }

    pub fn busy(&self) -> bool {
        self.busy
    }

    /// `empty_feeder_brother` armed.
    pub fn empty_feeder_brother(&self) -> bool {
        self.empty_feeder_brother
    }

    /// Delay (ms) the last `create_job` refusal owes for the empty-feeder emulation, if any.
    pub fn take_empty_feeder_refusal_ms(&mut self) -> Option<u64> {
        self.empty_feeder_refusal_ms.take()
    }

    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    pub fn calibrating(&self) -> bool {
        self.calibrating
    }

    pub fn interlock_open(&self) -> bool {
        self.interlock_open
    }

    pub fn storage_full(&self) -> bool {
        self.storage_full
    }

    pub fn paused(&self) -> bool {
        self.paused
    }

    pub fn lamp_warning(&self) -> bool {
        self.lamp_warning
    }

    pub fn lamp_warming(&self) -> bool {
        self.lamp_warming
    }

    /// Reason string when Subscribe / Create / Retrieve must refuse (`None` = accepting).
    pub fn not_accepting_reason(&self) -> Option<&'static str> {
        // Temporary / Internal first so FaultOut maps the XSD subcodes (not NotAcceptingJobs).
        // Path/media stop conditions (jam/cover/…) use ClientError* via [`Self::path_stopped_reason`].
        if self.temporary_error {
            Some("ServerErrorTemporaryError: temporary")
        } else if self.internal_error {
            Some("ServerErrorInternalError: internal")
        } else if self.offline {
            Some("ServerErrorNotAcceptingJobs: device offline")
        } else if self.busy {
            Some("ServerErrorNotAcceptingJobs: device busy")
        } else if self.lamp_error {
            Some("ServerErrorNotAcceptingJobs: LampError")
        } else if let Some(r) = self.path_stopped_reason() {
            Some(r)
        } else {
            None
        }
    }

    /// Sticky Stopped path/media faults → ClientError* (Create/Retrieve refuse).
    fn path_stopped_reason(&self) -> Option<&'static str> {
        if self.jam {
            Some("ClientErrorMediaJam: MediaJam")
        } else if self.cover_open {
            Some("ClientErrorCoverOpen: CoverOpen")
        } else if self.multi_feed {
            Some("ClientErrorMultipleFeed: MultipleFeedError")
        } else if self.interlock_open {
            Some("ClientErrorInterlockOpen: InterlockOpen")
        } else if self.storage_full {
            Some("ClientErrorInternalStorageFull: InternalStorageFull")
        } else {
            None
        }
    }

    /// Mark open jobs `ScannerStopped` when a Stopped scanner condition raises.
    fn mark_jobs_scanner_stopped(&mut self) {
        for job in self.jobs.values_mut() {
            if !job.aborted && job.pages_left > 0 {
                job.state_reason = "ScannerStopped".into();
            }
        }
    }

    /// NotifyTo deltas queued during RetrieveImage (mid-job `*_after_n` trips).
    pub fn drain_pending_condition_changes(&mut self) -> Vec<ConditionChange> {
        if self.pending_condition_changes.is_empty() {
            return Vec::new();
        }
        let out = self.pending_condition_changes.clone();
        self.pending_condition_changes.clear();
        out
    }

    fn enqueue_condition_change(&mut self, change: ConditionChange) {
        self.pending_condition_changes.push(change);
    }

    /// Same deltas as `apply_faults` jam=true — used when `jam_after_n` trips mid RetrieveImage.
    fn raise_jam_sticky(&mut self) {
        let before = self.jam;
        if before {
            return;
        }
        self.record_condition_transition(1, "MediaJam", "ADF", "Critical", true);
        self.jam = true;
        self.mark_jobs_scanner_stopped();
        self.enqueue_condition_change(ConditionChange {
            id: 1,
            name: "MediaJam",
            component: "ADF",
            severity: "Critical",
            raised: true,
        });
    }

    fn raise_multi_feed_sticky(&mut self) {
        let before = self.multi_feed;
        if before {
            return;
        }
        self.record_condition_transition(3, "MultipleFeedError", "ADF", "Critical", true);
        self.multi_feed = true;
        self.mark_jobs_scanner_stopped();
        self.enqueue_condition_change(ConditionChange {
            id: 3,
            name: "MultipleFeedError",
            component: "ADF",
            severity: "Critical",
            raised: true,
        });
    }

    /// AttentionRequired only (id=0) — StatusSummary + Create/Retrieve refuse.
    fn raise_offline_sticky(&mut self) {
        let before = self.offline;
        if before {
            return;
        }
        self.offline = true;
        self.mark_jobs_scanner_stopped();
        self.enqueue_condition_change(ConditionChange {
            id: 0,
            name: "AttentionRequired",
            component: "",
            severity: "",
            raised: true,
        });
    }

    pub fn slow_retrieve_ms(&self) -> u64 {
        self.slow_retrieve_ms
    }

    pub fn set_slow_retrieve_ms(&mut self, ms: u64) {
        self.slow_retrieve_ms = bounded_ms(ms);
    }

    pub fn retrieve_bps(&self) -> u64 {
        self.retrieve_bps
    }

    pub fn set_retrieve_bps(&mut self, bps: u64) {
        self.retrieve_bps = bps;
    }

    /// Apply `POST /v1/slow` — pre-delay always; `bps` only when the field is present.
    ///
    /// `ms` is stored capped at [`MAX_CONTROL_DELAY_MS`]. The HTTP and ctl handlers reject an
    /// over-limit value before calling this, so a client sees an error instead of a silent clamp.
    pub fn apply_slow(&mut self, body: &SlowBody) {
        self.slow_retrieve_ms = bounded_ms(body.ms);
        if let Some(bps) = body.bps {
            self.retrieve_bps = bps;
        }
    }

    pub fn image_pattern(&self) -> PatternId {
        self.image_pattern
    }

    pub fn set_image_pattern(&mut self, pattern: PatternId) {
        self.scenario_id = None;
        self.clear_loopback();
        self.image_pattern = pattern;
    }

    /// Parse and set the image pattern from a control-plane id string.
    ///
    /// Selecting a synthetic pattern clears the hard loopback store (mutually exclusive).
    pub fn set_image_pattern_id(&mut self, id: &str) -> Result<()> {
        self.scenario_id = None;
        self.clear_loopback();
        self.image_pattern = PatternId::parse(id)?;
        Ok(())
    }

    /// Apply a named tester progression step (pattern + feeder + check text).
    pub fn apply_scenario(&mut self, id: &str) -> Result<&'static Scenario> {
        let s = scenarios::by_id(id).ok_or_else(|| {
            anyhow!(
                "unknown scenario {id:?}; see GET /v1/scenarios (order…duplex) or lab id sticky_empty"
            )
        })?;
        self.scenario_id = Some(s.id.to_string());
        self.clear_loopback();
        self.image_pattern = s.pattern;
        self.page_side = match s.side {
            Some(side) => Some(PageSide::parse(side)?),
            None => None,
        };
        self.page_order = match s.order {
            Some(order) => PageOrder::parse(order)?,
            None => PageOrder::Forward,
        };
        self.set_pages(s.pages);
        // Platen steps leave ADF empty; honor the scenario's ready flag.
        if s.pages == 0 {
            self.feeder_ready = s.feeder_ready;
        }
        Ok(s)
    }

    pub fn scenario_id(&self) -> Option<&str> {
        self.scenario_id.as_deref()
    }

    pub fn page_side(&self) -> Option<PageSide> {
        self.page_side
    }

    pub fn page_order(&self) -> PageOrder {
        self.page_order
    }

    pub fn jpeg_com(&self) -> bool {
        self.jpeg_com
    }

    /// Toggle JFIF COM markers for RetrieveImage (`POST /v1/jpeg-com`).
    pub fn set_jpeg_com(&mut self, enabled: bool) {
        self.jpeg_com = enabled;
    }

    /// Whether the hard loopback store has pages (RetrieveImage echoes them).
    pub fn loopback_active(&self) -> bool {
        !self.loopback_pages.is_empty()
    }

    pub fn loopback_status(&self) -> LoopbackStatus {
        LoopbackStatus {
            count: self.loopback_pages.len() as u32,
            digests: self
                .loopback_pages
                .iter()
                .map(|p| loopback_digest(p))
                .collect(),
            active: self.loopback_active(),
        }
    }

    /// Replace the hard loopback store from base64 page blobs.
    ///
    /// Empty `pages` clears the store (synthetic patterns resume). Non-empty
    /// loads the feeder to match and clears the active scenario.
    pub fn set_loopback(&mut self, body: &LoopbackBody) -> Result<()> {
        use base64::Engine;
        if body.pages.is_empty() {
            self.clear_loopback();
            return Ok(());
        }
        let mut decoded = Vec::with_capacity(body.pages.len());
        for (i, b64) in body.pages.iter().enumerate() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(b64.trim())
                .map_err(|e| anyhow!("loopback page {i}: base64 decode: {e}"))?;
            if bytes.is_empty() {
                bail!("loopback page {i}: empty blob");
            }
            decoded.push(bytes);
        }
        let n = decoded.len() as u32;
        self.loopback_pages = decoded;
        // Loopback and synthetic pattern/side are mutually exclusive.
        self.scenario_id = None;
        self.image_pattern = PatternId::default();
        self.page_side = None;
        self.page_order = PageOrder::Forward;
        self.set_pages(n);
        Ok(())
    }

    pub fn clear_loopback(&mut self) {
        self.loopback_pages.clear();
    }

    /// Apply optional fault / condition fields (unset keys leave prior values).
    ///
    /// Returns condition deltas for NotifyTo `ScannerStatusConditionEvent` /
    /// `ScannerStatusConditionClearedEvent` (only sticky status flags).
    pub fn apply_faults(&mut self, body: &FaultBody) -> Vec<ConditionChange> {
        let mut changes = Vec::new();
        let mut track = |id: u32,
                         name: &'static str,
                         component: &'static str,
                         severity: &'static str,
                         before: bool,
                         after: bool| {
            if before != after {
                changes.push(ConditionChange {
                    id,
                    name,
                    component,
                    severity,
                    raised: after,
                });
            }
        };
        if let Some(jam) = body.jam {
            let before = self.jam;
            track(1, "MediaJam", "ADF", "Critical", before, jam);
            if before != jam {
                self.record_condition_transition(1, "MediaJam", "ADF", "Critical", jam);
            }
            self.jam = jam;
            if jam && !before {
                self.mark_jobs_scanner_stopped();
            }
        }
        if let Some(cover_open) = body.cover_open {
            let before = self.cover_open;
            track(2, "CoverOpen", "Platen", "Critical", before, cover_open);
            if before != cover_open {
                self.record_condition_transition(2, "CoverOpen", "Platen", "Critical", cover_open);
            }
            self.cover_open = cover_open;
            if cover_open && !before {
                self.mark_jobs_scanner_stopped();
            }
        }
        if let Some(multi_feed) = body.multi_feed {
            let before = self.multi_feed;
            track(
                3,
                "MultipleFeedError",
                "ADF",
                "Critical",
                before,
                multi_feed,
            );
            if before != multi_feed {
                self.record_condition_transition(
                    3,
                    "MultipleFeedError",
                    "ADF",
                    "Critical",
                    multi_feed,
                );
            }
            self.multi_feed = multi_feed;
            if multi_feed && !before {
                self.mark_jobs_scanner_stopped();
            }
        }
        if let Some(lamp_error) = body.lamp_error {
            let before = self.lamp_error;
            track(4, "LampError", "Platen", "Critical", before, lamp_error);
            if before != lamp_error {
                self.record_condition_transition(4, "LampError", "Platen", "Critical", lamp_error);
            }
            self.lamp_error = lamp_error;
        }
        if let Some(busy) = body.busy {
            self.busy = busy;
        }
        if let Some(offline) = body.offline {
            // AttentionRequired is a ScannerStateReason only — no DeviceCondition Id / history.
            // id=0: still returned for StatusSummary; maybe_emit_condition_changes skips it.
            track(0, "AttentionRequired", "", "", self.offline, offline);
            self.offline = offline;
        }
        if let Some(calibrating) = body.calibrating {
            let before = self.calibrating;
            track(
                5,
                "Calibrating",
                "Platen",
                "Informational",
                before,
                calibrating,
            );
            if before != calibrating {
                self.record_condition_transition(
                    5,
                    "Calibrating",
                    "Platen",
                    "Informational",
                    calibrating,
                );
            }
            self.calibrating = calibrating;
        }
        if let Some(interlock) = body.interlock {
            let before = self.interlock_open;
            track(
                6,
                "InterlockOpen",
                "MediaPath",
                "Critical",
                before,
                interlock,
            );
            if before != interlock {
                self.record_condition_transition(
                    6,
                    "InterlockOpen",
                    "MediaPath",
                    "Critical",
                    interlock,
                );
            }
            self.interlock_open = interlock;
            if interlock && !before {
                self.mark_jobs_scanner_stopped();
            }
        }
        if let Some(storage_full) = body.storage_full {
            let before = self.storage_full;
            track(
                7,
                "InternalStorageFull",
                "Platen",
                "Critical",
                before,
                storage_full,
            );
            if before != storage_full {
                self.record_condition_transition(
                    7,
                    "InternalStorageFull",
                    "Platen",
                    "Critical",
                    storage_full,
                );
            }
            self.storage_full = storage_full;
            if storage_full && !before {
                self.mark_jobs_scanner_stopped();
            }
        }
        if let Some(paused) = body.paused {
            // Paused is a ScannerStateReason only — no ConditionName / DeviceCondition.
            track(0, "Paused", "", "", self.paused, paused);
            self.paused = paused;
        }
        if let Some(lamp_warning) = body.lamp_warning {
            let before = self.lamp_warning;
            track(8, "LampWarning", "Platen", "Warning", before, lamp_warning);
            if before != lamp_warning {
                self.record_condition_transition(
                    8,
                    "LampWarning",
                    "Platen",
                    "Warning",
                    lamp_warning,
                );
            }
            self.lamp_warning = lamp_warning;
        }
        if let Some(lamp_warming) = body.lamp_warming {
            // LampWarming is a ScannerStateReason only — no ConditionName.
            track(0, "LampWarming", "", "", self.lamp_warming, lamp_warming);
            self.lamp_warming = lamp_warming;
        }
        if let Some(temporary) = body.temporary {
            self.temporary_error = temporary;
        }
        if let Some(internal) = body.internal {
            self.internal_error = internal;
        }
        if let Some(ms) = body.ms {
            self.slow_retrieve_ms = bounded_ms(ms);
        }
        if let Some(n) = body.jam_after_n {
            self.jam_after_n = n;
        }
        if let Some(n) = body.empty_mid_job {
            self.empty_mid_job = n;
        }
        if let Some(n) = body.offline_after_n {
            self.offline_after_n = n;
        }
        if let Some(n) = body.multi_feed_after_n {
            self.multi_feed_after_n = n;
        }
        if let Some(n) = body.cover_open_after_n {
            self.set_cover_open_after_n(n);
        }
        if let Some(abort) = body.abort_retrieve {
            self.abort_retrieve = abort;
        }
        if let Some(n) = body.abort_retrieve_after_n {
            self.abort_retrieve_after_n = n;
        }
        if let Some(n) = body.abort_retrieve_bytes {
            self.abort_retrieve_bytes = n;
        }
        if let Some(ms) = body.lamp_warmup_ms {
            self.lamp_warmup_ms = bounded_ms(ms);
        }
        if let Some(on) = body.empty_feeder_brother {
            self.empty_feeder_brother = on;
        }
        if let Some(ms) = body.empty_feeder_delay_ms {
            self.empty_feeder_delay_ms = bounded_ms(ms);
        }
        self.note(
            "ctl",
            "fault",
            "ok",
            &format!(
                "jam={} cover_open={} multi_feed={} lamp_error={} busy={} offline={} calibrating={} interlock={} storage_full={} paused={} lamp_warning={} lamp_warming={} temporary={} internal={} jam_after_n={} empty_mid_job={} offline_after_n={} multi_feed_after_n={} abort_retrieve={} abort_retrieve_after_n={} abort_retrieve_bytes={}",
                self.jam,
                self.cover_open,
                self.multi_feed,
                self.lamp_error,
                self.busy,
                self.offline,
                self.calibrating,
                self.interlock_open,
                self.storage_full,
                self.paused,
                self.lamp_warning,
                self.lamp_warming,
                self.temporary_error,
                self.internal_error,
                self.jam_after_n,
                self.empty_mid_job,
                self.offline_after_n,
                self.multi_feed_after_n,
                self.abort_retrieve,
                self.abort_retrieve_after_n,
                self.abort_retrieve_bytes
            ),
        );
        changes
    }

    pub fn jam_after_n(&self) -> u32 {
        self.jam_after_n
    }

    pub fn empty_mid_job(&self) -> u32 {
        self.empty_mid_job
    }

    pub fn offline_after_n(&self) -> u32 {
        self.offline_after_n
    }

    pub fn multi_feed_after_n(&self) -> u32 {
        self.multi_feed_after_n
    }

    pub fn abort_retrieve(&self) -> bool {
        self.abort_retrieve
    }

    pub fn abort_retrieve_after_n(&self) -> u32 {
        self.abort_retrieve_after_n
    }

    pub fn abort_retrieve_bytes(&self) -> u64 {
        self.abort_retrieve_bytes
    }

    /// Whether the next RetrieveImage (at `page_index` images already transferred) should
    /// cut the MTOM body mid-send. When `abort_retrieve_after_n` trips, arms sticky
    /// `abort_retrieve` so later pages keep failing the same way until `reset`.
    pub fn plan_abort_retrieve(&mut self, page_index: u32) -> bool {
        if self.abort_retrieve {
            return true;
        }
        let after = self.abort_retrieve_after_n;
        if after > 0 && page_index >= after {
            self.abort_retrieve = true;
            return true;
        }
        false
    }

    /// Bytes of the assembled MTOM body to send before the stream errors.
    /// `abort_retrieve_bytes == 0` → half of `body_len` (at least 1, less than full).
    pub fn abort_cut_bytes(&self, body_len: usize) -> usize {
        if body_len <= 1 {
            return body_len.saturating_sub(1);
        }
        let configured = self.abort_retrieve_bytes as usize;
        let cut = if configured == 0 {
            body_len / 2
        } else {
            configured.min(body_len.saturating_sub(1))
        };
        cut.max(1)
    }

    pub fn lamp_warmup_ms(&self) -> u64 {
        self.lamp_warmup_ms
    }

    /// WSD `ScannerStateReason` tokens (handy for control-plane tests / Agent B).
    pub fn scanner_state_reasons(&self) -> Vec<&'static str> {
        let mut reasons = Vec::new();
        if self.jam {
            reasons.push("MediaJam");
        }
        if self.cover_open {
            reasons.push("CoverOpen");
        }
        if self.multi_feed {
            reasons.push("MultipleFeedError");
        }
        if self.lamp_error {
            reasons.push("LampError");
        }
        if self.calibrating {
            reasons.push("Calibrating");
        }
        if self.interlock_open {
            reasons.push("InterlockOpen");
        }
        if self.storage_full {
            reasons.push("InternalStorageFull");
        }
        if self.lamp_warming {
            reasons.push("LampWarming");
        }
        if self.paused {
            reasons.push("Paused");
        }
        if self.offline || self.lamp_warning {
            // No Offline / LampWarning reason tokens — AttentionRequired is the
            // sticky “needs human / not ready” signal (lamp_warning pairs with
            // ConditionName LampWarning in ActiveConditions).
            reasons.push("AttentionRequired");
        }
        // Busy alone keeps reason None with ScannerState=Processing (see elements).
        // Empty feeder is Idle/None; AttentionRequired is for real faults (not InputTrayEmpty).
        if reasons.is_empty() {
            reasons.push("None");
        }
        reasons
    }

    pub fn status(&self) -> Status {
        let available_profiles = Profile::list(&self.profiles_dir).unwrap_or_default();
        let (wsd_hellos_sent, wsd_probes_answered) = self.discovery.snapshot();
        let (wsd_instance_id, wsd_metadata_version) = self.discovery.sequence();
        Status {
            profile_id: self.profile.id.clone(),
            connection: self.profile.connection.clone(),
            feeder_ready: self.feeder_ready,
            sticky_ready: self.profile.feeder.sticky_ready,
            duplex: self.profile.feeder.duplex,
            pages_remaining: self.pages_remaining,
            feeder_capacity: self.profile.feeder.capacity,
            initial_pages: self.initial_pages(),
            jam: self.jam,
            cover_open: self.cover_open,
            multi_feed: self.multi_feed,
            lamp_error: self.lamp_error,
            busy: self.busy,
            offline: self.offline,
            calibrating: self.calibrating,
            interlock_open: self.interlock_open,
            storage_full: self.storage_full,
            paused: self.paused,
            lamp_warning: self.lamp_warning,
            lamp_warming: self.lamp_warming,
            temporary: self.temporary_error,
            internal: self.internal_error,
            slow_retrieve_ms: self.slow_retrieve_ms,
            retrieve_bps: self.retrieve_bps,
            jam_after_n: self.jam_after_n,
            empty_mid_job: self.empty_mid_job,
            offline_after_n: self.offline_after_n,
            multi_feed_after_n: self.multi_feed_after_n,
            cover_open_after_n: self.cover_open_after_n(),
            fault_plan: self.fault_plan_view(),
            abort_retrieve: self.abort_retrieve,
            abort_retrieve_after_n: self.abort_retrieve_after_n,
            abort_retrieve_bytes: self.abort_retrieve_bytes,
            lamp_warmup_ms: self.lamp_warmup_ms,
            empty_feeder_brother: self.empty_feeder_brother,
            empty_feeder_delay_ms: self.empty_feeder_delay_ms,
            image_pattern: self.image_pattern.as_str().to_string(),
            effects: self.effects.summary(),
            scenario: self.scenario_id.clone(),
            scenario_step: self
                .scenario_id
                .as_deref()
                .and_then(|id| scenarios::by_id(id).map(|s| s.step)),
            scenario_of: self
                .scenario_id
                .as_deref()
                .and_then(|id| scenarios::by_id(id).map(|s| s.of)),
            scenario_check: self
                .scenario_id
                .as_deref()
                .and_then(|id| scenarios::by_id(id).map(|s| s.check.to_string())),
            scenario_next: self
                .scenario_id
                .as_deref()
                .and_then(|id| scenarios::next_after(id).map(|s| s.id.to_string())),
            page_side: self.page_side.map(|s| s.as_str().to_string()),
            page_order: self.page_order.as_str().to_string(),
            jpeg_com: self.jpeg_com,
            loopback_pages: self.loopback_pages.len() as u32,
            loopback_digests: self
                .loopback_pages
                .iter()
                .map(|p| loopback_digest(p))
                .collect(),
            refuse_unknown: self.profile.refuse_unknown,
            active_jobs: self.jobs.len(),
            job_timeout_ms: self.job_timeout.as_millis() as u64,
            destination_tokens: self.destinations.len(),
            subscription_expires: self.subscription_expires.clone(),
            pending_push_scans: self.pending_push.len(),
            job_history_len: self.job_history.len(),
            optical: self.profile.optical.clone(),
            dpi: self.profile.dpi.clone(),
            formats: self.profile.formats.clone(),
            available_profiles,
            device_endpoint: self.device_endpoint.clone(),
            service_endpoint: self.service_endpoint.clone(),
            device_uuid: self.device_uuid.to_string(),
            wsd_hellos_sent,
            wsd_probes_answered,
            wsd_instance_id,
            wsd_metadata_version,
            note: "WS-Scan GetScannerElements / CreateScanJob / RetrieveImage / push-scan lab",
        }
    }

    pub fn load_profile(&mut self, id: &str) -> Result<()> {
        let profile = Profile::load(&self.profiles_dir, id)?;
        let sticky = profile.feeder.sticky_ready;
        self.jpeg_com = jpeg_com_initial(profile.jpeg_com);
        self.profile = profile;
        self.feeder_ready = sticky;
        self.pages_remaining = 0;
        self.feeder_loaded = 0;
        self.jam = false;
        self.cover_open = false;
        self.multi_feed = false;
        self.lamp_error = false;
        self.busy = false;
        self.offline = false;
        self.calibrating = false;
        self.interlock_open = false;
        self.storage_full = false;
        self.paused = false;
        self.lamp_warning = false;
        self.lamp_warming = false;
        self.temporary_error = false;
        self.internal_error = false;
        self.slow_retrieve_ms = 0;
        self.retrieve_bps = 0;
        self.jam_after_n = 0;
        self.empty_mid_job = 0;
        self.offline_after_n = 0;
        self.multi_feed_after_n = 0;
        self.plan.clear();
        self.abort_retrieve = false;
        self.abort_retrieve_after_n = 0;
        self.abort_retrieve_bytes = 0;
        self.lamp_warmup_ms = 0;
        self.empty_feeder_brother = false;
        self.empty_feeder_delay_ms = DEFAULT_EMPTY_FEEDER_DELAY_MS;
        self.empty_feeder_refusal_ms = None;
        self.image_pattern = PatternId::default();
        self.effects = ImageEffects::default();
        self.scenario_id = None;
        self.page_side = None;
        self.page_order = PageOrder::Forward;
        self.loopback_pages.clear();
        self.jobs.clear();
        self.destinations.clear();
        self.last_destination_token = None;
        self.notify_to = None;
        self.notify_peer = None;
        self.subscription_expires = None;
        self.subscription_id = None;
        self.subscription_deadline = None;
        self.pending_push.clear();
        self.job_history.clear();
        self.condition_history.clear();
        self.condition_raised_at.clear();
        self.pending_condition_changes.clear();
        self.load_initial_pages();
        Ok(())
    }

    pub fn set_feeder(&mut self, ready: bool) {
        self.feeder_ready = ready;
    }

    /// Sheets loaded at creation, reset and profile reload: the per-device override if set,
    /// else the profile's `feeder.initial_pages`, clamped to the tray `capacity` and
    /// `pages_max`. Runtime state only; never part of a capability fingerprint.
    pub fn initial_pages(&self) -> u32 {
        let f = &self.profile.feeder;
        let n = self.initial_pages_override.unwrap_or(f.initial_pages);
        n.min(f.capacity.unwrap_or(u32::MAX)).min(f.pages_max)
    }

    /// Set (or clear with `None`) the per-device initial-pages override and load the tray
    /// accordingly right away. Used when a device is created.
    pub fn set_initial_pages_override(&mut self, n: Option<u32>) {
        self.initial_pages_override = n;
        self.load_initial_pages();
    }

    /// The per-device override, if any.
    pub fn initial_pages_override(&self) -> Option<u32> {
        self.initial_pages_override
    }

    /// Load the initial stack (as `set_pages` + `set_feeder(true)` would). A no-op for 0, so
    /// an empty-by-default device is untouched.
    fn load_initial_pages(&mut self) {
        let n = self.initial_pages();
        if n > 0 {
            self.set_pages(n);
            self.feeder_ready = true;
        }
    }

    /// Set feeder page count only (keeps current side/order). Prefer [`Self::apply_pages`]
    /// from the control plane so side/order travel with the stack.
    pub fn try_set_pages(&mut self, count: u32) -> Result<(), PagesError> {
        self.check_pages_capacity(count)?;
        self.set_pages(count);
        Ok(())
    }

    /// `Err` when `count` exceeds the profile's tray `capacity` (state untouched).
    pub fn check_pages_capacity(&self, count: u32) -> Result<(), PagesError> {
        match self.profile.feeder.capacity {
            Some(capacity) if count > capacity => Err(PagesError::OverCapacity {
                requested: count,
                capacity,
            }),
            _ => Ok(()),
        }
    }

    pub fn set_pages(&mut self, count: u32) {
        let max = self.profile.feeder.pages_max;
        self.pages_remaining = count.min(max);
        self.feeder_loaded = self.pages_remaining;
        if self.pages_remaining > 0 {
            self.feeder_ready = true;
        } else if !self.profile.feeder.sticky_ready {
            self.feeder_ready = false;
        }
        self.note(
            "ctl",
            "pages",
            "ok",
            &format!(
                "count={} feeder_ready={}",
                self.pages_remaining, self.feeder_ready
            ),
        );
    }

    /// Load a feeder stack with optional duplex side tagging and reverse order.
    ///
    /// Setting `side` to front/back also selects the `duplex_pair` pattern so
    /// RetrieveImage emits blue/front or red/back faces. Omitting `side` clears
    /// forced tagging (parity-based duplex_pair still works if that pattern is active).
    ///
    /// Synthetic pages clear the hard loopback store (mutually exclusive).
    pub fn apply_pages(&mut self, body: &PagesBody) -> Result<()> {
        // Over-capacity is refused before any state changes (handler maps PagesError to 409).
        self.check_pages_capacity(body.count)?;
        self.clear_loopback();
        self.set_pages(body.count);
        self.page_side = match body.side.as_deref() {
            None | Some("") => None,
            Some(s) => Some(PageSide::parse(s)?),
        };
        self.page_order = match body.order.as_deref() {
            None | Some("") => PageOrder::Forward,
            Some(s) => PageOrder::parse(s)?,
        };
        if self.page_side.is_some() {
            self.image_pattern = PatternId::DuplexPair;
        }
        Ok(())
    }

    pub fn reset(&mut self) {
        self.tickets.clear();
        self.feeder_ready = self.profile.feeder.sticky_ready;
        self.pages_remaining = 0;
        self.feeder_loaded = 0;
        self.jam = false;
        self.cover_open = false;
        self.multi_feed = false;
        self.lamp_error = false;
        self.busy = false;
        self.offline = false;
        self.calibrating = false;
        self.interlock_open = false;
        self.storage_full = false;
        self.paused = false;
        self.lamp_warning = false;
        self.lamp_warming = false;
        self.temporary_error = false;
        self.internal_error = false;
        self.slow_retrieve_ms = 0;
        self.retrieve_bps = 0;
        self.jam_after_n = 0;
        self.empty_mid_job = 0;
        self.offline_after_n = 0;
        self.multi_feed_after_n = 0;
        self.plan.clear();
        self.abort_retrieve = false;
        self.abort_retrieve_after_n = 0;
        self.abort_retrieve_bytes = 0;
        self.lamp_warmup_ms = 0;
        self.empty_feeder_brother = false;
        self.empty_feeder_delay_ms = DEFAULT_EMPTY_FEEDER_DELAY_MS;
        self.empty_feeder_refusal_ms = None;
        self.image_pattern = PatternId::default();
        self.effects = ImageEffects::default();
        self.scenario_id = None;
        self.page_side = None;
        self.page_order = PageOrder::Forward;
        self.jpeg_com = jpeg_com_initial(self.profile.jpeg_com);
        self.loopback_pages.clear();
        self.jobs.clear();
        self.destinations.clear();
        self.last_destination_token = None;
        self.notify_to = None;
        self.notify_peer = None;
        self.subscription_expires = None;
        self.subscription_id = None;
        self.subscription_deadline = None;
        self.pending_push.clear();
        self.job_history.clear();
        self.condition_history.clear();
        self.condition_raised_at.clear();
        self.pending_condition_changes.clear();
        self.load_initial_pages();
        self.note("ctl", "reset", "ok", "cleared");
    }

    /// Apply `POST /v1/effects` body (merge / preset / clear).
    pub fn apply_effects(&mut self, body: &EffectsBody) -> Result<()> {
        self.effects
            .apply_body(body)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(())
    }

    pub fn effects(&self) -> &ImageEffects {
        &self.effects
    }

    /// `jfif_gray=gray8_tiff`: the Format a Grayscale8 JFIF/JPEG/EXIF ticket is
    /// answered with (`tiff-single-uncompressed`), if the mode is on and the
    /// profile offers it. `None` = keep the requested format.
    pub fn jfif_gray_format(&self, format: &str, color: ColorMode) -> Option<String> {
        const TIFF: &str = "tiff-single-uncompressed";
        let jfif = matches!(
            format.to_ascii_lowercase().as_str(),
            "jfif" | "jpeg" | "jpg" | "exif"
        );
        (self.effects.jfif_gray == crate::effects::JfifGray::Gray8Tiff
            && color == ColorMode::Gray8
            && jfif
            && self.profile.formats.iter().any(|f| f == TIFF))
        .then(|| TIFF.to_string())
    }

    /// `jfif_bw=reject`: Some(fault reason) when a BlackAndWhite1 ticket asks for a
    /// JPEG-family Format (JPEG cannot carry 1-bit; the driver then falls back).
    pub fn jfif_bw_reject(&self, format: &str, color: ColorMode) -> Option<String> {
        let jfif = matches!(
            format.to_ascii_lowercase().as_str(),
            "jfif" | "jpeg" | "jpg" | "exif"
        );
        (self.effects.jfif_bw == crate::effects::JfifBw::Reject && color == ColorMode::Bw1 && jfif)
            .then(|| {
                format!(
                    "ClientErrorDocumentFormatNotSupported: format {format} cannot carry BlackAndWhite1"
                )
            })
    }

    /// `refuse_tickets`: Some(fault reason) when a rule matches the ticket (all fields
    /// present in the rule must match; absent = wildcard). Format matches either the
    /// requested or the resolved wire token; colour / dpi are the resolved values.
    pub fn refuse_ticket(&self, body: &str, check: &TicketValidation) -> Option<String> {
        if self.effects.refuse_tickets.is_empty() {
            return None;
        }
        let norm = |s: &str| {
            normalize_format(s)
                .map(|n| n.to_string())
                .unwrap_or_else(|_| s.trim().to_ascii_lowercase())
        };
        let requested_format = extract_tag(body, "Format").map(|f| norm(&f));
        let resolved_format = norm(&check.format);
        let source = extract_tag(body, "InputSource").map(|s| {
            if s.eq_ignore_ascii_case("Feeder") {
                "ADF".to_string()
            } else {
                s
            }
        });
        for rule in &self.effects.refuse_tickets {
            if let Some(f) = rule.format.as_deref() {
                let f = norm(f);
                if requested_format.as_deref() != Some(f.as_str()) && resolved_format != f {
                    continue;
                }
            }
            if let Some(c) = rule.color.as_deref() {
                if !check.color.wsd_token().eq_ignore_ascii_case(c) {
                    continue;
                }
            }
            if let Some(s) = rule.input_source.as_deref() {
                if !source.as_deref().is_some_and(|t| t.eq_ignore_ascii_case(s)) {
                    continue;
                }
            }
            if let Some(r) = rule.resolution {
                if check.dpi != r {
                    continue;
                }
            }
            let fault = rule.fault_name();
            return Some(format!(
                "{fault}: ticket refused by refuse_tickets (format {}, color {}, dpi {})",
                check.format,
                check.color.wsd_token(),
                check.dpi
            ));
        }
        None
    }

    /// Validate a ScanTicket body (ValidateScanTicket / CreateScanJob).
    ///
    /// With `refuse_unknown`, unsupported Resolution / Format / ColorProcessing /
    /// InputSource / ScanRegion follow MustHonor (docs/reference/must-honor.md):
    /// true → reject; false/absent → substitute nearest dpi / preferred_format /
    /// preferred color / ignore region / drop unsupported source. Empty profile
    /// `colors` keeps ColorProcessing soft-map via [`ColorMode::parse`].
    /// Format MustHonor=true keeps the `ClientErrorDocumentFormatNotSupported`
    /// reason phrase (Create hard-faults; Validate may soft-accept).
    pub fn validate_ticket(&self, body: &str) -> TicketValidation {
        let mut dpi = extract_resolution(body)
            .or_else(|| self.profile.dpi.first().copied())
            .unwrap_or(200);
        let color_tag = extract_tag(body, "ColorProcessing");
        let mut color = color_tag
            .as_deref()
            .map(ColorMode::parse)
            .unwrap_or_default();
        let requested_format =
            extract_tag(body, "Format").unwrap_or_else(|| preferred_format(&self.profile));
        let format = match normalize_format(&requested_format) {
            Ok(f) => f,
            Err(reason) => {
                return TicketValidation {
                    valid: false,
                    dpi,
                    format: requested_format,
                    color,
                    reason: Some(reason.to_string()),
                };
            }
        };

        let dpi_must_honor = extract_must_honor(body, "Resolution");
        let format_must_honor = extract_must_honor(body, "Format");
        let source_must_honor = extract_must_honor(body, "InputSource");
        let region_must_honor = extract_must_honor(body, "ScanRegionWidth")
            || extract_must_honor(body, "ScanRegionHeight")
            || extract_must_honor(body, "ScanRegionXOffset")
            || extract_must_honor(body, "ScanRegionYOffset");
        let color_must_honor = extract_must_honor(body, "ColorProcessing");

        if !self.profile.refuse_unknown {
            return TicketValidation {
                valid: true,
                dpi,
                format,
                color,
                reason: None,
            };
        }

        let dpi_ok = self.profile.dpi.contains(&dpi);
        let format_ok = format_in_profile(&format, &self.profile);
        let color_ok = color_tag
            .as_deref()
            .map(|s| color_in_profile(s, &self.profile))
            .unwrap_or(true);
        let source_tag = extract_tag(body, "InputSource");
        let source_ok = source_tag
            .as_deref()
            .map(|s| input_source_in_profile(s, &self.profile))
            .unwrap_or(true);
        let region_ok = scan_region_in_bounds(body);
        // ADF tray media sizes (glass / Platen unaffected).
        let adf_source = match source_tag.as_deref() {
            Some(src) => {
                src.eq_ignore_ascii_case("ADF")
                    || src.eq_ignore_ascii_case("Feeder")
                    || src.eq_ignore_ascii_case("ADFDuplex")
            }
            None => self.feeder_ready && self.pages_remaining > 0,
        };
        let size_must_honor =
            extract_must_honor(body, "InputMediaSize") || extract_must_honor(body, "InputSize");
        let size_ok = !adf_source
            || match input_media_size(body) {
                Some((w, h)) => self.profile.feeder.accepts_media(w, h),
                None => true,
            };
        // Two+ MustHonor=true values the device cannot satisfy → named Validate/Create fault
        // (Learn CreateScanJob / ValidateScanTicket: ConflictingRequiredParameters).
        let mut honor_conflicts: Vec<&str> = Vec::new();
        if !dpi_ok && dpi_must_honor {
            honor_conflicts.push("Resolution");
        }
        if !format_ok && format_must_honor {
            honor_conflicts.push("Format");
        }
        if !color_ok && color_must_honor {
            honor_conflicts.push("ColorProcessing");
        }
        if !source_ok && source_must_honor {
            honor_conflicts.push("InputSource");
        }
        if !region_ok && region_must_honor {
            honor_conflicts.push("ScanRegion");
        }
        if !size_ok && size_must_honor {
            honor_conflicts.push("InputSize");
        }
        if honor_conflicts.len() >= 2 {
            return TicketValidation {
                valid: false,
                dpi,
                format,
                color,
                reason: Some(format!(
                    "ClientErrorConflictingRequiredParameters: MustHonor conflict on {}",
                    honor_conflicts.join("+")
                )),
            };
        }

        if !dpi_ok {
            if dpi_must_honor {
                return TicketValidation {
                    valid: false,
                    dpi,
                    format,
                    color,
                    reason: Some(format!("dpi {dpi} not in profile list")),
                };
            }
            dpi = nearest_dpi(dpi, &self.profile);
        }

        let format = if format_ok {
            format
        } else if format_must_honor {
            return TicketValidation {
                valid: false,
                dpi,
                format: format.clone(),
                color,
                // Phrase triggers inbox WSD Format fallback. Soft-accept on Validate
                // keys off this string; Create hard-faults.
                reason: Some(format!(
                    "ClientErrorDocumentFormatNotSupported: format {format} not in profile list"
                )),
            };
        } else {
            preferred_format(&self.profile)
        };

        if !color_ok {
            if color_must_honor {
                let c = color_tag.as_deref().unwrap_or("?");
                return TicketValidation {
                    valid: false,
                    dpi,
                    format,
                    color,
                    reason: Some(format!("ColorProcessing {c} not in profile colors")),
                };
            }
            // MustHonor false/absent: substitute first profile color (or RGB24).
            color = preferred_color(&self.profile);
        }

        if !source_ok && source_must_honor {
            let src = source_tag.as_deref().unwrap_or("?");
            return TicketValidation {
                valid: false,
                dpi,
                format,
                color,
                reason: Some(format!("InputSource {src} not in profile sources")),
            };
        }
        // MustHonor false/absent: ignore unsupported source (Create may still pick Platen).

        if !size_ok && size_must_honor {
            let (w, h) = input_media_size(body).unwrap_or((0, 0));
            return TicketValidation {
                valid: false,
                dpi,
                format,
                color,
                reason: Some(format!(
                    "InputMediaSize {w}x{h} not in profile feeder media_sizes"
                )),
            };
        }
        // MustHonor false/absent: unsupported ADF size is ignored (device uses its own).

        if !region_ok && region_must_honor {
            return TicketValidation {
                valid: false,
                dpi,
                format,
                color,
                reason: Some("ScanRegion outside platen bounds".into()),
            };
        }
        // MustHonor false/absent: ignore / clamp to full platen at Create.

        TicketValidation {
            valid: true,
            dpi,
            format,
            color,
            reason: None,
        }
    }

    /// Corrected dpi / format for a Learn-style `ValidScanTicket` rewrite.
    pub fn corrected_ticket_params(&self, check: &TicketValidation) -> (u32, String) {
        (
            nearest_dpi(check.dpi, &self.profile),
            preferred_format(&self.profile),
        )
    }

    /// Create a scan job from a CreateScanJobRequest body (raw SOAP XML).
    pub fn create_job(&mut self, body: &str) -> Result<ScanJob> {
        self.sweep_timed_out_jobs();
        self.empty_feeder_refusal_ms = None;
        if let Some(reason) = self.not_accepting_reason() {
            bail!("{reason}");
        }
        // Device-push CreateScanJob: ScanIdentifier + DestinationToken (Learn).
        let scan_id = extract_tag(body, "ScanIdentifier");
        let dest_tok = extract_tag(body, "DestinationToken");
        match (scan_id.as_deref(), dest_tok.as_deref()) {
            (None, None) => {}
            (Some(id), Some(tok)) => self.consume_push_credentials(id, tok)?,
            _ => bail!("InvalidArgs: ScanIdentifier and DestinationToken required together"),
        }
        let check = self.validate_ticket(body);
        if let Some(reason) = self.refuse_ticket(body, &check) {
            bail!(reason);
        }
        if !check.valid {
            // Prefer the ticket reason verbatim when it already names a WSD ClientError*
            // (FormatNotSupported → driver lossless fallback). Otherwise keep a prefix.
            let why = check.reason.unwrap_or_else(|| "rejected".into());
            if why.contains("ClientError") {
                bail!("{why}");
            }
            bail!("invalid scan ticket: {why}");
        }

        let mut input_source = extract_tag(body, "InputSource").unwrap_or_else(|| {
            if self.feeder_ready && self.pages_remaining > 0 {
                "ADF".into()
            } else {
                "Platen".into()
            }
        });
        let wants_duplex = input_source.eq_ignore_ascii_case("ADFDuplex");
        if wants_duplex && !self.profile.feeder.duplex {
            bail!("ClientErrorFormatNotSupported: ADFDuplex not offered by this profile");
        }
        // Keep ADFDuplex on the job when hardware duplex is on; else collapse aliases to ADF.
        if !wants_duplex
            && (input_source.eq_ignore_ascii_case("Feeder")
                || input_source.eq_ignore_ascii_case("ADF"))
        {
            input_source = "ADF".into();
        }
        let dpi = check.dpi;
        let color = check.color;
        if let Some(reason) = self.jfif_bw_reject(&check.format, color) {
            bail!(reason);
        }
        let format = self
            .jfif_gray_format(&check.format, color)
            .unwrap_or_else(|| check.format.clone());
        let requested_format = extract_tag(body, "Format");
        let format_used_default = requested_format.is_none();
        let format_overridden = requested_format
            .as_deref()
            .and_then(|r| normalize_format(r).ok())
            .map(|n| n != format)
            .unwrap_or(false);
        let requested_dpi = extract_resolution(body);
        let dpi_used_default = requested_dpi.is_none();
        let dpi_overridden = requested_dpi.map(|d| d != dpi).unwrap_or(false);
        // Spec: 0 = unknown / until ADF empty; Platen clients send 1.
        let images_to_transfer =
            extract_tag(body, "ImagesToTransfer").and_then(|s| s.parse::<u32>().ok());

        let is_adf = input_source.eq_ignore_ascii_case("ADF")
            || input_source.eq_ignore_ascii_case("Feeder")
            || wants_duplex;
        let duplex = wants_duplex && self.profile.feeder.duplex;
        let mut sheet_base = 0;
        let mut stack_total = 0;
        let mut sheets_taken = 0;
        let pages = if is_adf {
            // Sticky FEED_READY can stay true with an empty stack (Brother quirk).
            // ADF Create still fails so Auto can fall back to glass.
            if self.pages_remaining == 0 {
                if self.empty_feeder_brother {
                    // Real Brother: empty feeder looks Idle; the job is refused as busy,
                    // late (the WSD handler sleeps `empty_feeder_refusal_ms` first).
                    self.empty_feeder_refusal_ms = Some(self.empty_feeder_delay_ms);
                    bail!("ServerErrorNotAcceptingJobs: device busy");
                }
                bail!("ADF selected but feeder empty");
            }
            let sheets = self.pages_remaining;
            // Hardware duplex: each sheet yields two faces (front then back).
            let available_faces = if duplex {
                sheets.saturating_mul(2)
            } else {
                sheets
            };
            let n = match images_to_transfer {
                None | Some(0) => available_faces,
                Some(n) => n.min(available_faces).clamp(
                    1,
                    self.profile
                        .feeder
                        .pages_max
                        .saturating_mul(if duplex { 2 } else { 1 }),
                ),
            };
            // A job consumes only the sheets it asked for (a real feeder keeps the rest
            // loaded for the next CreateScanJob); 0 / absent takes everything left.
            sheets_taken = if duplex { n.div_ceil(2) } else { n }.min(sheets);
            sheet_base = self.feeder_loaded.saturating_sub(self.pages_remaining);
            stack_total = self.feeder_loaded.max(sheet_base + sheets);
            self.pages_remaining -= sheets_taken;
            if self.pages_remaining == 0 && !self.profile.feeder.sticky_ready {
                self.feeder_ready = false;
            }
            n
        } else {
            // Platen is one image; clients (airscan) send ImagesToTransfer=1.
            let _ = images_to_transfer;
            1
        };

        let (width_px, height_px) = image_gen::job_page_pixels(dpi);
        let id = self.next_job_id;
        self.next_job_id += 1;
        let now = Instant::now();
        let job = ScanJob {
            id,
            // Brother live JobToken is `urn:uuid:…` (not `job-…`).
            token: format!("urn:uuid:{}", Uuid::new_v4()),
            format,
            input_source,
            dpi,
            color,
            width_px,
            height_px,
            pages_left: pages,
            page_index: 0,
            sheet_base,
            stack_total,
            sheets_taken,
            duplex,
            created_at: now,
            last_activity: now,
            // Pending + JobScanning until first Retrieve → JobScanningAndTransfering.
            state_reason: "JobScanning".into(),
            aborted: false,
            format_used_default,
            format_overridden,
            dpi_used_default,
            dpi_overridden,
        };
        self.jobs.insert(id, job.clone());
        self.note(
            "job",
            "CreateScanJob",
            "ok",
            &format!(
                "id={id} state=Pending pages={} format={} source={} dpi={}",
                job.pages_left, job.format, job.input_source, job.dpi
            ),
        );
        Ok(job)
    }

    pub fn cancel_job(&mut self, job_id: i32) -> Result<()> {
        // Idempotent: WIA CancelJob after a finished RetrieveImage often races
        // the completed job; faulting here surfaces as COM 0x80004005 noise.
        let removed = if let Some(mut job) = self.jobs.remove(&job_id) {
            if !job.aborted && job.pages_left > 0 {
                job.state_reason = "JobCanceledAtDevice".into();
                self.push_job_history(&job, "Canceled", "JobCanceledAtDevice");
                self.return_sheets(job.unfed_sheets());
            } else if job.pages_left == 0 && !job.aborted {
                // Already completed via Retrieve — history may already hold it.
            }
            true
        } else {
            false
        };
        self.note(
            "job",
            "CancelJob",
            "ok",
            &format!("id={job_id} removed={removed}"),
        );
        Ok(())
    }

    /// Sheets a cancelled or timed-out job never fed go back on the tray, so the next job starts at the first of them.
    fn return_sheets(&mut self, sheets: u32) {
        if sheets == 0 {
            return;
        }
        self.pages_remaining = self.pages_remaining.saturating_add(sheets);
        self.feeder_ready = true;
    }

    pub fn jobs_snapshot(&self) -> Vec<ScanJob> {
        let mut v: Vec<_> = self.jobs.values().cloned().collect();
        v.sort_by_key(|j| j.id);
        v
    }

    pub fn get_job(&self, job_id: i32) -> Option<&ScanJob> {
        self.jobs.get(&job_id)
    }

    /// True when `job_id` appears in history as a canceled job (active map entry gone).
    fn job_was_canceled(&self, job_id: i32) -> bool {
        self.job_history
            .iter()
            .rev()
            .any(|e| e.id == job_id && (e.state == "Canceled" || e.reason == "JobCanceledAtDevice"))
    }

    /// Fault text when RetrieveImage / GetJobElements cannot find an active job.
    /// Canceled JobIds (history only) map to `ClientErrorJobCancelled` (XSD spelling).
    pub fn missing_job_fault(&self, job_id: i32) -> String {
        if self.job_was_canceled(job_id) {
            format!("ClientErrorJobCancelled: job {job_id} was cancelled")
        } else {
            format!("job {job_id} not found")
        }
    }

    /// Pull one page image for RetrieveImage.
    ///
    /// After the last page, the job stays alive with `pages_left == 0` so a
    /// further RetrieveImage returns `ClientErrorNoImagesAvailable` (ADF end)
    /// instead of `JobIdNotFound`.
    /// Retrieve an image from the job, synthesising or echoing a loopback blob.
    ///
    /// Equivalent to `retrieve_image_with_peer(job_id, token, None)`.
    pub fn retrieve_image(&mut self, job_id: i32, token: &str) -> Result<(ScanJob, Vec<u8>)> {
        self.retrieve_image_with_peer(job_id, token, None)
    }

    /// Like [`retrieve_image`] but records the WSD caller's peer address so that
    /// diagnostic patterns ([`crate::image_gen::PatternId::Info`],
    /// [`crate::image_gen::PatternId::Anac`]) can embed it on the page.
    pub fn retrieve_image_with_peer(
        &mut self,
        job_id: i32,
        token: &str,
        peer: Option<String>,
    ) -> Result<(ScanJob, Vec<u8>)> {
        let delay = bounded_ms(self.slow_retrieve_ms);
        let warmup = bounded_ms(self.lamp_warmup_ms);
        // Fault plan: live / timed / between-pages entries fire here; a mid-page entry
        // is returned for the page about to stream.
        let gate = self.plan_gate_retrieve(job_id, token);
        if let Some(reason) = gate.fault {
            bail!("{reason}");
        }
        let mid_cut = gate.mid;
        if let Some(reason) = self.not_accepting_reason() {
            bail!("{reason}");
        }
        self.ensure_job_not_timed_out(job_id)?;
        let jam_after = self.jam_after_n;
        let empty_after = self.empty_mid_job;
        let offline_after = self.offline_after_n;
        let multi_after = self.multi_feed_after_n;
        let mut pattern = self.image_pattern;
        let forced_side = self.page_side;
        let page_order = self.page_order;
        let jpeg_com = self.jpeg_com;
        let effects = self.effects.clone();
        let loopback = self.loopback_pages.clone();
        // Mid-job jam: trip after N images already transferred.
        if jam_after > 0 {
            if let Some(job) = self.jobs.get(&job_id) {
                if job.token == token && job.page_index >= jam_after {
                    self.raise_jam_sticky();
                    bail!("ClientErrorMediaJam: jam_after_n={jam_after} tripped on job {job_id}");
                }
            }
        }
        // Mid-job multi-feed: double-pick after N images.
        if multi_after > 0 {
            if let Some(job) = self.jobs.get(&job_id) {
                if job.token == token && job.page_index >= multi_after {
                    self.raise_multi_feed_sticky();
                    bail!(
                        "ClientErrorMultipleFeed: multi_feed_after_n={multi_after} tripped on job {job_id}"
                    );
                }
            }
        }
        // Mid-job offline: device drops after N images (WIA often surfaces Unspecified / offline).
        if offline_after > 0 {
            if let Some(job) = self.jobs.get(&job_id) {
                if job.token == token && job.page_index >= offline_after {
                    self.raise_offline_sticky();
                    bail!(
                        "ServerErrorNotAcceptingJobs: offline_after_n={offline_after} tripped on job {job_id}"
                    );
                }
            }
        }
        let first_image = self
            .jobs
            .get(&job_id)
            .map(|j| j.token == token && j.page_index == 0)
            .unwrap_or(false);
        if first_image && warmup > 0 {
            // Expose LampWarming on Status while the first-page warm-up sleep runs.
            self.lamp_warming = true;
            std::thread::sleep(std::time::Duration::from_millis(warmup));
            self.lamp_warming = false;
        }
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        if !self.jobs.contains_key(&job_id) {
            bail!("{}", self.missing_job_fault(job_id));
        }
        let job = self
            .jobs
            .get_mut(&job_id)
            .expect("job present after contains_key");
        if job.token != token {
            bail!("ClientErrorInvalidJobToken: job token mismatch on job {job_id}");
        }
        if job.pages_left == 0 {
            bail!("ClientErrorNoImagesAvailable: no images left on job {job_id}");
        }
        // Empty mid-job: pretend the feeder ran out early.
        if empty_after > 0 && job.page_index >= empty_after {
            // Job ends early: Completed + JobCompletedWithErrors (history + NotifyTo JobEnd
            // see a terminal state instead of a stale JobScanningAndTransfering).
            self.end_job_empty(job_id);
            bail!("ClientErrorNoImagesAvailable: empty_mid_job={empty_after} on job {job_id}");
        }
        // First/mid Retrieve while still Processing: XSD WKV spelling (one `r` in Transfering).
        job.state_reason = "JobScanningAndTransfering".into();
        let format = job.format.clone();
        let duplex_job = job.duplex;
        let take = image_gen::pages_per_retrieve(&format, job.pages_left);
        // Face indices → sheet indices. Hardware duplex: 2 faces per sheet.
        // Reverse order is for a flip-stack second pass (manual duplex).
        let sheet_base = job.sheet_base;
        let sheet_total = if job.stack_total > 0 {
            job.stack_total
        } else {
            let face_total = job.page_index + job.pages_left;
            if duplex_job {
                face_total.div_ceil(2)
            } else {
                face_total
            }
        };
        let sheets: Vec<u32> = (0..take)
            .map(|i| {
                let face = job.page_index + i;
                let logical = sheet_base + if duplex_job { face / 2 } else { face };
                match page_order {
                    PageOrder::Forward => logical,
                    PageOrder::Reverse => sheet_total.saturating_sub(1).saturating_sub(logical),
                }
            })
            .collect();
        // Manual duplex (`POST /v1/pages` side) wins; else hardware duplex tags faces.
        let page_side = if forced_side.is_some() {
            forced_side
        } else if duplex_job {
            let face = job.page_index;
            Some(if face.is_multiple_of(2) {
                PageSide::Front
            } else {
                PageSide::Back
            })
        } else {
            None
        };
        // Prefer duplex_pair art when emitting hardware-duplex faces without an explicit pattern.
        if duplex_job
            && forced_side.is_none()
            && matches!(pattern, PatternId::PageId | PatternId::Wedges)
        {
            pattern = PatternId::DuplexPair;
        }
        let color = job.color;
        let dpi = job.dpi;
        // Lab accept-gate A/B: serve a captured Brother (or other) body as-is.
        // `FAKESCAN_RETRIEVE_FILE` = JPEG path; ImageInformation must match that SOF.
        let override_path = std::env::var("FAKESCAN_RETRIEVE_FILE")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let bytes = if let Some(path) = override_path {
            std::fs::read(&path)
                .map_err(|e| anyhow::anyhow!("FAKESCAN_RETRIEVE_FILE read {path}: {e}"))?
        } else if !loopback.is_empty() {
            match emit_loopback(&loopback, &format, &sheets) {
                Ok(b) => b,
                Err(e) => {
                    // Loopback re-encode miss → DocumentFormatError (not Compression*).
                    job.state_reason = "DocumentFormatError".into();
                    return Err(e);
                }
            }
        } else {
            let opts = image_gen::encode_opts_for(pattern, jpeg_com);
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let ctx = image_gen::PageContext {
                profile_id: self.profile.id.clone(),
                device_uuid: self.device_uuid.to_string(),
                pattern_id: pattern.as_str().to_string(),
                dpi,
                format: format.clone(),
                job_id,
                peer: peer.filter(|p| !p.is_empty() && p != "-"),
                timestamp_secs: ts,
            };
            match image_gen::encode_pattern_pages_ctx(
                &format,
                pattern,
                dpi,
                &sheets,
                page_side,
                Some(&effects),
                color,
                opts,
                Some(&ctx),
            ) {
                Ok(b) => b,
                Err(e) => {
                    job.state_reason = "CompressionError".into();
                    return Err(e);
                }
            }
        };
        // Mid-page fault: the stream is cut part-way, so this page is not delivered.
        if let Some((entry_id, cut)) = mid_cut {
            job.last_activity = Instant::now();
            let snapshot = job.clone();
            self.plan_fire_mid(entry_id, job_id, snapshot.stack_position() + 1, cut);
            return Ok((snapshot, bytes));
        }
        job.page_index += take;
        job.pages_left -= take;
        job.last_activity = Instant::now();
        if job.pages_left == 0 {
            // Mid-MTOM abort armed → completed with errors; LampWarning → warnings; else success.
            let done_reason = if self.abort_retrieve {
                "JobCompletedWithErrors"
            } else if self.lamp_warning {
                "JobCompletedWithWarnings"
            } else {
                "JobCompletedSuccessfully"
            };
            job.state_reason = done_reason.into();
        } else {
            // Throttled Retrieve (`retrieve_bps`) is transfer-bound; else scan+transfer.
            job.state_reason = if self.retrieve_bps > 0 {
                "JobTransfering".into()
            } else {
                "JobScanningAndTransfering".into()
            };
        }
        let snapshot = job.clone();
        if snapshot.pages_left == 0 {
            self.push_job_history(&snapshot, "Completed", &snapshot.state_reason);
        }
        self.note(
            "job",
            "RetrieveImage",
            "ok",
            &format!(
                "id={} state={} page={} left={} bytes={}",
                snapshot.id,
                snapshot.job_state(),
                snapshot.page_index,
                snapshot.pages_left,
                bytes.len()
            ),
        );
        Ok((snapshot, bytes))
    }
}

/// Short fingerprint for ground-truth expect (`len:first8hex`).
/// `?after=` / ctl `args.after`. Blank and non-numeric text mean 0.
pub fn parse_cursor(text: &str) -> u64 {
    let text = text.trim();
    if text.is_empty() {
        return 0;
    }
    if let Ok(n) = text.parse::<u64>() {
        return n;
    }
    if let Ok(n) = text.parse::<i64>() {
        return if n < 0 { 0 } else { n as u64 };
    }
    if let Ok(n) = text.parse::<f64>() {
        if n.is_finite() && n >= 0.0 && n.fract() == 0.0 && n <= u64::MAX as f64 {
            return n as u64;
        }
    }
    0
}

fn loopback_digest(bytes: &[u8]) -> String {
    let n = bytes.len().min(8);
    let mut hex = String::with_capacity(n * 2);
    for b in &bytes[..n] {
        hex.push_str(&format!("{b:02x}"));
    }
    format!("{}:{hex}", bytes.len())
}

/// Echo loopback store pages for RetrieveImage.
///
/// JFIF/EXIF single-page: exact bytes (hard ground truth). Other formats decode
/// and re-encode. Multi-page Formats that consume several sheets in one retrieve
/// are refused — arm with `jfif` for byte-identical compare.
fn emit_loopback(store: &[Vec<u8>], format: &str, sheets: &[u32]) -> Result<Vec<u8>> {
    if sheets.is_empty() {
        bail!("loopback: no sheets");
    }
    for &s in sheets {
        if s as usize >= store.len() {
            bail!(
                "loopback: sheet {s} out of range (store has {} pages)",
                store.len()
            );
        }
    }
    let fmt = format.to_ascii_lowercase();
    let exact = matches!(fmt.as_str(), "jfif" | "jpeg" | "jpg" | "exif");
    if exact && sheets.len() == 1 {
        return Ok(store[sheets[0] as usize].clone());
    }
    if sheets.len() > 1 {
        bail!("loopback: multi-page format {format:?} not supported; use jfif for exact echo");
    }
    let raw = &store[sheets[0] as usize];
    let img = image::load_from_memory(raw)
        .map_err(|e| anyhow!("loopback: decode page {}: {e}", sheets[0]))?
        .to_rgb8();
    // Re-encode without COM so the blob stays close to the uploaded pixels, at the resolution the upload carries: inferring
    // it from a letter-wide page labelled an A4 page at 100 dpi as 97 dpi (and an A5 page at 200 dpi as 137).
    image_gen::encode_image_at(&fmt, img, sheets[0], None, image_gen::image_dpi(raw))
}

/// Resolve JPEG COM enablement: `FAKESCAN_JPEG_COM` overrides the profile default.
///
/// Env values: `0` / `false` / `off` / `no` → disabled; `1` / `true` / `on` / `yes` → enabled.
fn jpeg_com_initial(profile_default: bool) -> bool {
    match std::env::var("FAKESCAN_JPEG_COM") {
        Ok(v) => match v.trim().to_ascii_lowercase().as_str() {
            "0" | "false" | "off" | "no" => false,
            "1" | "true" | "on" | "yes" => true,
            _ => profile_default,
        },
        Err(_) => profile_default,
    }
}

/// WSD `FormatValue` tokens we can encode for this profile (order preserved).
///
/// Profile JSON may say `jpeg` / `tiff` / `bmp`; wire values are
/// `jfif` / `tiff-single-uncompressed` / `dib` / `png`.
pub fn encodable_formats(profile: &Profile) -> Vec<String> {
    let mut out = Vec::new();
    for f in &profile.formats {
        if let Some(wsd) = profile_format_to_wsd(f) {
            if !out.iter().any(|x| x == &wsd) {
                out.push(wsd);
            }
        }
    }
    out
}

/// Map a profile or ticket format name to a WSD FormatValue we can encode.
pub fn profile_format_to_wsd(name: &str) -> Option<String> {
    match name.to_ascii_lowercase().as_str() {
        "jfif" | "jpeg" | "jpg" => Some("jfif".into()),
        "exif" => Some("exif".into()),
        "tiff-single-uncompressed" | "tiff" | "tif" => Some("tiff-single-uncompressed".into()),
        "tiff-single-g4" | "tiff-g4" | "g4" => Some("tiff-single-g4".into()),
        "tiff-single-g3mh" | "tiff-g3mh" | "g3mh" => Some("tiff-single-g3mh".into()),
        "tiff-single-jpeg-tn2" | "tiff-jpeg" | "jpeg-tn2" => Some("tiff-single-jpeg-tn2".into()),
        "tiff-multi-uncompressed" | "tiff-multi" => Some("tiff-multi-uncompressed".into()),
        "tiff-multi-g4" => Some("tiff-multi-g4".into()),
        "tiff-multi-g3mh" => Some("tiff-multi-g3mh".into()),
        "tiff-multi-jpeg-tn2" => Some("tiff-multi-jpeg-tn2".into()),
        "dib" | "bmp" => Some("dib".into()),
        "png" => Some("png".into()),
        "jpeg2k" | "jp2" | "j2k" => Some("jpeg2k".into()),
        "pdf-a" | "pdf" => Some("pdf-a".into()),
        "xps" => Some("xps".into()),
        // jbig (JBIG1 / ISO 11544): not shipped — JBIG-KIT is GPL; no MIT/Apache encoder.
        _ => None,
    }
}

/// Every WSD FormatValue fakescan can encode (order stable for tests / docs).
pub const SHIPPED_WSD_FORMATS: &[&str] = &[
    "jfif",
    "exif",
    "png",
    "dib",
    "tiff-single-uncompressed",
    "tiff-single-g4",
    "tiff-single-g3mh",
    "tiff-single-jpeg-tn2",
    "tiff-multi-uncompressed",
    "tiff-multi-g4",
    "tiff-multi-g3mh",
    "tiff-multi-jpeg-tn2",
    "jpeg2k",
    "pdf-a",
    "xps",
];

fn preferred_format(profile: &Profile) -> String {
    encodable_formats(profile)
        .into_iter()
        .next()
        .unwrap_or_else(|| "jfif".into())
}

/// Closest profile dpi to `requested` (absolute distance); empty list → 200.
fn nearest_dpi(requested: u32, profile: &Profile) -> u32 {
    profile
        .dpi
        .iter()
        .copied()
        .min_by_key(|&d| (d as i64 - requested as i64).unsigned_abs())
        .or_else(|| profile.dpi.first().copied())
        .unwrap_or(200)
}

fn normalize_format(requested: &str) -> Result<String> {
    let requested_lc = requested.to_ascii_lowercase();
    if requested_lc.is_empty() {
        bail!("empty format");
    }
    // Keep unknown tokens so refuse_unknown can reject them against the profile.
    Ok(profile_format_to_wsd(&requested_lc).unwrap_or(requested_lc))
}

/// Map a WSD `InputSource` token to a profile `sources` entry (`glass` / `feeder`).
fn input_source_profile_key(wsd: &str) -> Option<&'static str> {
    match wsd.trim().to_ascii_lowercase().as_str() {
        "platen" | "glass" | "flatbed" => Some("glass"),
        "adf" | "feeder" => Some("feeder"),
        "adfduplex" => Some("feeder"),
        _ => None,
    }
}

fn input_source_in_profile(wsd: &str, profile: &Profile) -> bool {
    let Some(key) = input_source_profile_key(wsd) else {
        return false;
    };
    if !profile.sources.iter().any(|s| s.eq_ignore_ascii_case(key)) {
        return false;
    }
    // ADFDuplex needs hardware duplex even when feeder is listed.
    if wsd.eq_ignore_ascii_case("ADFDuplex") && !profile.feeder.duplex {
        return false;
    }
    true
}

/// `true` when no ScanRegion is present, or X/Y/W/H fit the selected source (1/1000").
///
/// Platen tickets are letter-bounded. ADF / Feeder / ADFDuplex may use the taller
/// [`image_gen::ADF_MAX_H_THOU`] window that `ADFMaximumSize` advertises; WIA feeder
/// items often send that full sheet region with MustHonor=true.
fn scan_region_in_bounds(body: &str) -> bool {
    let tree = ticket_tree(body);
    let has_region = tree.as_ref().is_some_and(|t| {
        t.descendant("ScanRegionWidth").is_some()
            || t.descendant("ScanRegionHeight").is_some()
            || t.descendant("ScanRegionXOffset").is_some()
            || t.descendant("ScanRegionYOffset").is_some()
    });
    if !has_region {
        return true;
    }
    let x = extract_tag(body, "ScanRegionXOffset")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let y = extract_tag(body, "ScanRegionYOffset")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let w = extract_tag(body, "ScanRegionWidth")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(image_gen::LETTER_W_THOU);
    let h = extract_tag(body, "ScanRegionHeight")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(image_gen::LETTER_H_THOU);
    if w == 0 || h == 0 {
        return false;
    }
    let source = extract_tag(body, "InputSource").unwrap_or_default();
    let max_h = if source.eq_ignore_ascii_case("ADF")
        || source.eq_ignore_ascii_case("Feeder")
        || source.eq_ignore_ascii_case("ADFDuplex")
    {
        image_gen::ADF_MAX_H_THOU
    } else {
        image_gen::LETTER_H_THOU
    };
    x.saturating_add(w) <= image_gen::LETTER_W_THOU && y.saturating_add(h) <= max_h
}

fn format_in_profile(wsd_format: &str, profile: &Profile) -> bool {
    let aliases: &[&str] = match wsd_format {
        "jfif" => &["jfif", "jpeg", "jpg"],
        "exif" => &["exif"],
        "tiff-single-uncompressed" => &["tiff-single-uncompressed", "tiff", "tif"],
        "tiff-single-g4" => &["tiff-single-g4", "tiff-g4", "g4"],
        "tiff-single-g3mh" => &["tiff-single-g3mh", "tiff-g3mh", "g3mh"],
        "tiff-single-jpeg-tn2" => &["tiff-single-jpeg-tn2", "tiff-jpeg", "jpeg-tn2"],
        "tiff-multi-uncompressed" => &["tiff-multi-uncompressed", "tiff-multi"],
        "tiff-multi-g4" => &["tiff-multi-g4"],
        "tiff-multi-g3mh" => &["tiff-multi-g3mh"],
        "tiff-multi-jpeg-tn2" => &["tiff-multi-jpeg-tn2"],
        "dib" => &["dib", "bmp"],
        "png" => &["png"],
        "jpeg2k" => &["jpeg2k", "jp2", "j2k"],
        "pdf-a" => &["pdf-a", "pdf"],
        "xps" => &["xps"],
        other => {
            return profile
                .formats
                .iter()
                .any(|f| f.eq_ignore_ascii_case(other));
        }
    };
    profile
        .formats
        .iter()
        .any(|f| aliases.iter().any(|a| f.eq_ignore_ascii_case(a)))
}

/// First profile color, or `RGB24` when the allow-list is empty.
fn preferred_color(profile: &Profile) -> ColorMode {
    profile
        .colors
        .first()
        .map(|s| ColorMode::parse(s))
        .unwrap_or(ColorMode::Rgb24)
}

/// `true` when `colors` is empty (no allow-list) or the token matches a listed
/// entry via the same aliases as [`ColorMode::parse`]. Unknown tokens that only
/// soft-map to RGB24 are not treated as in-profile.
fn color_in_profile(wsd_color: &str, profile: &Profile) -> bool {
    if profile.colors.is_empty() {
        return true;
    }
    let mode = ColorMode::parse(wsd_color);
    if mode == ColorMode::Rgb24 && !wsd_color.trim().eq_ignore_ascii_case("RGB24") {
        return false;
    }
    profile.colors.iter().any(|c| ColorMode::parse(c) == mode)
}

fn ticket_tree(body: &str) -> Option<Element> {
    let t = body.trim();
    if t.is_empty() {
        return None;
    }
    let env = Envelope::parse(body);
    if env.root.as_deref() == Some("Envelope") {
        if let Some(bt) = env.body_tree() {
            return Some(bt.clone());
        }
        let b = env.body.trim();
        if !b.is_empty() {
            return wrap_ticket_fields(b);
        }
        return None;
    }
    wrap_ticket_fields(t)
}

fn wrap_ticket_fields(ticket: &str) -> Option<Element> {
    const SCAN_NS: &str = "http://schemas.microsoft.com/windows/2006/01/wdp/scan";
    xml::parse(&format!(
        r#"<ticket xmlns:wscn="{SCAN_NS}" xmlns:sca="{SCAN_NS}">{ticket}</ticket>"#
    ))
    .or_else(|| xml::parse(ticket))
}

pub(crate) fn extract_tag(xml: &str, local: &str) -> Option<String> {
    let tree = ticket_tree(xml)?;
    tree.descendant(local).and_then(|el| {
        let value = el.text.trim();
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    })
}

/// `MustHonor` on the named element's start tag. Absent → **false** (Common.xsd default).
fn extract_must_honor(xml: &str, local: &str) -> bool {
    let Some(tree) = ticket_tree(xml) else {
        return false;
    };
    let Some(el) = tree.descendant(local) else {
        return false;
    };
    el.attr("MustHonor")
        .map(|v| matches!(v.trim(), "true" | "1" | "TRUE"))
        .unwrap_or(false)
}

/// `InputMediaSize` Width/Height (1/1000 inch) from a ticket, when present.
fn input_media_size(xml: &str) -> Option<(u32, u32)> {
    let tree = ticket_tree(xml)?;
    let el = tree.descendant("InputMediaSize")?;
    let w = el.find_text("Width")?.trim().parse().ok()?;
    let h = el.find_text("Height")?.trim().parse().ok()?;
    Some((w, h))
}

/// Ticket fields the parser finds, as JSON (only the ones present).
pub(crate) fn ticket_parsed(xml: &str) -> serde_json::Map<String, serde_json::Value> {
    use serde_json::Value;
    let mut m = serde_json::Map::new();
    let val = |s: String| match s.parse::<i64>() {
        Ok(n) => Value::from(n),
        Err(_) => Value::from(s),
    };
    for (key, tag) in [
        ("input_source", "InputSource"),
        ("format", "Format"),
        ("color_processing", "ColorProcessing"),
        ("images_to_transfer", "ImagesToTransfer"),
        ("content_type", "ContentType"),
        ("compression_quality", "CompressionQualityFactor"),
        ("scan_region_width", "ScanRegionWidth"),
        ("scan_region_height", "ScanRegionHeight"),
    ] {
        if let Some(v) = extract_tag(xml, tag) {
            m.insert(key.into(), val(v));
        }
    }
    if let Some(tree) = ticket_tree(xml) {
        if let Some(res) = tree.descendant("Resolution") {
            if let Some(w) = res.find_text("Width") {
                m.insert("resolution_x".into(), val(w.trim().to_string()));
            }
            if let Some(h) = res.find_text("Height") {
                m.insert("resolution_y".into(), val(h.trim().to_string()));
            }
        }
    }
    if let Some((w, h)) = input_media_size(xml) {
        m.insert("width".into(), w.into());
        m.insert("height".into(), h.into());
    }
    m
}

fn extract_resolution(xml: &str) -> Option<u32> {
    // Only `Resolution/Width`. A ticket that omits Resolution still carries `InputMediaSize`
    // Width in thousandths of an inch (letter is 8500). Reading the first Width in the tree
    // treats that media size as a dpi and then "substitutes" the nearest profile dpi.
    let tree = ticket_tree(xml)?;
    let res = tree.descendant("Resolution")?;
    res.find_text("Width")?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::{boot_sequence_id, DiscoveryStats};

    fn brother_wsd() -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-wsd").expect("profile");
        World::new(dir, profile)
    }

    /// brother-wsd with the hardware ADF duplex bit on (the shipped lab profile is single-sided).
    fn brother_wsd_duplex() -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut profile = Profile::load(&dir, "brother-wsd").expect("profile");
        profile.feeder.duplex = true;
        World::new(dir, profile)
    }

    fn glass_wsd() -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "glass-wsd").expect("profile");
        World::new(dir, profile)
    }

    #[test]
    fn status_exposes_wsd_discovery_sequence() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-wsd").expect("profile");
        let stats = Arc::new(DiscoveryStats::default());
        let id = boot_sequence_id();
        stats.set_sequence(id, id);
        let w = World::with_discovery(dir, profile, stats);
        let st = w.status();
        assert!(
            st.wsd_instance_id >= 1,
            "boot_sequence_id style InstanceId must be nonzero"
        );
        assert_eq!(st.wsd_instance_id, id);
        assert_eq!(st.wsd_metadata_version, id);
    }

    fn brother_usb() -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "brother-usb").expect("profile");
        World::new(dir, profile)
    }

    #[test]
    fn trace_ring_returns_only_calls_after_a_cursor() {
        let mut w = brother_wsd();
        assert!(w.trace_since(0).events.is_empty());
        w.note_wsd("wsd", "192.168.1.70:1", "Get", "ok", 0, "soap");
        w.note_wsd(
            "wsd",
            "192.168.1.70:2",
            "ValidateScanTicket",
            "ok",
            1,
            "soap",
        );
        let cursor = w.trace_since(0).seq;
        w.note_wsd(
            "wsd",
            "192.168.1.70:3",
            "CreateScanJob",
            "fault",
            4,
            "invalid scan ticket",
        );
        let newer = w.trace_since(cursor);
        assert_eq!(newer.events.len(), 1);
        assert_eq!(newer.events[0].action, "CreateScanJob");
        assert_eq!(newer.events[0].outcome, "fault");
        assert!(w.log_since(cursor).contains("CreateScanJob"));
        for i in 0..80 {
            w.note_wsd("wsd", "peer", "Get", "ok", i, "soap");
        }
        assert_eq!(w.trace_since(0).events.len(), 64);
        assert!(w.jobs_view().jobs.is_empty());
    }

    #[test]
    fn encodable_maps_profile_aliases_to_wsd() {
        let wsd = brother_wsd();
        // Hardware-parity lite caps: JFIF + single TIFF only (no DIB on the WSD Brother).
        assert_eq!(
            encodable_formats(wsd.profile()),
            vec!["jfif".to_string(), "tiff-single-uncompressed".to_string(),]
        );
        let usb = brother_usb();
        assert_eq!(encodable_formats(usb.profile()), vec!["dib".to_string()]);

        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let rich = Profile::load(&dir, "wia-rich").expect("wia-rich");
        assert_eq!(
            encodable_formats(&rich),
            vec![
                "jfif".to_string(),
                "exif".to_string(),
                "png".to_string(),
                "dib".to_string(),
                "tiff-single-uncompressed".to_string(),
                "tiff-single-g4".to_string(),
                "tiff-single-g3mh".to_string(),
                "tiff-single-jpeg-tn2".to_string(),
                "tiff-multi-uncompressed".to_string(),
                "tiff-multi-g4".to_string(),
                "tiff-multi-g3mh".to_string(),
                "tiff-multi-jpeg-tn2".to_string(),
                "jpeg2k".to_string(),
                "pdf-a".to_string(),
                "xps".to_string(),
            ]
        );
        assert_eq!(
            encodable_formats(&rich),
            SHIPPED_WSD_FORMATS
                .iter()
                .map(|s| (*s).to_string())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_shipped_format_encodes() {
        for fmt in SHIPPED_WSD_FORMATS {
            let sheets: &[u32] = if image_gen::is_multipage_format(fmt) {
                &[0, 1]
            } else {
                &[0]
            };
            let bytes = image_gen::encode_pattern_pages(
                fmt,
                crate::image_gen::PatternId::PageId,
                50,
                sheets,
                None,
                None,
                crate::effects::ColorMode::Rgb24,
                image_gen::EncodeOpts::WITH_COM,
            )
            .unwrap_or_else(|e| panic!("{fmt}: {e}"));
            assert!(!bytes.is_empty(), "{fmt} empty");
            assert_ne!(
                image_gen::mime_for_format(fmt),
                "application/octet-stream",
                "{fmt} mime"
            );
        }
    }

    #[test]
    fn multipage_tiff_consumes_all_pages_in_one_retrieve() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "wia-rich").expect("wia-rich");
        let mut w = World::new(dir, profile);
        w.set_pages(3);
        let job = w
            .create_job(
                r#"
          <wscn:Format>tiff-multi-uncompressed</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create");
        assert_eq!(job.pages_left, 3);
        let (snap, bytes) = w.retrieve_image(job.id, &job.token).expect("multi");
        assert_eq!(snap.pages_left, 0);
        assert!(bytes[..4] == [0x49, 0x49, 0x2A, 0x00] || bytes[..4] == [0x4D, 0x4D, 0x00, 0x2A]);
        let err = w
            .retrieve_image(job.id, &job.token)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ClientErrorNoImagesAvailable"));
    }

    #[test]
    fn retrieve_pdf_a_starts_with_pdf_header() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "wia-rich").expect("wia-rich");
        let mut w = World::new(dir, profile);
        w.set_pages(1);
        let job = w
            .create_job(
                r#"
          <wscn:Format>pdf-a</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create pdf");
        assert_eq!(job.format, "pdf-a");
        let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
        assert!(bytes.starts_with(b"%PDF-1.4"));
    }

    #[test]
    fn omitted_resolution_is_the_profile_default_not_media_width() {
        let w = brother_wsd();
        // Letter is 8500 x 11000 thousandths of an inch. That Width is not a dpi.
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:InputSize><wscn:InputMediaSize>
            <wscn:Width>8500</wscn:Width><wscn:Height>11000</wscn:Height>
          </wscn:InputMediaSize></wscn:InputSize>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.dpi, 100, "first profile dpi, not nearest to 8500");
        let mut w = brother_wsd();
        let job = w.create_job(body).expect("platen create");
        assert!(job.dpi_used_default, "omitted Resolution is UsedDefault");
        assert!(
            !job.dpi_overridden,
            "media width must not look like a substitution"
        );
        assert_eq!(job.dpi, 100);
    }

    #[test]
    fn extract_tag_reads_must_honor_format() {
        let xml = r#"<sca:Format sca:MustHonor="true">jfif</sca:Format>"#;
        assert_eq!(extract_tag(xml, "Format").as_deref(), Some("jfif"));
        assert_eq!(
            extract_tag(r#"<sca:FormatValue>dib</sca:FormatValue>"#, "Format"),
            None,
            "Format must not match FormatValue"
        );
    }

    #[test]
    fn accept_dib_refuses_jfif_must_honor_ticket() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, "accept-dib").expect("accept-dib");
        assert!(profile.refuse_unknown);
        assert_eq!(encodable_formats(&profile), vec!["dib".to_string()]);
        let w = World::new(dir, profile);
        let body = r#"
          <sca:Format sca:MustHonor="true">jfif</sca:Format>
          <sca:ImagesToTransfer sca:MustHonor="true">1</sca:ImagesToTransfer>
          <sca:InputSource sca:MustHonor="true">Platen</sca:InputSource>
          <sca:ColorProcessing sca:MustHonor="true">RGB24</sca:ColorProcessing>
          <sca:Resolution sca:MustHonor="true"><sca:Width>200</sca:Width><sca:Height>200</sca:Height></sca:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid, "jfif must be refused on dib-only profile: {v:?}");
        assert_eq!(v.format, "jfif");
        assert!(
            v.reason
                .as_deref()
                .is_some_and(|r| r.contains("ClientErrorDocumentFormatNotSupported")),
            "{v:?}"
        );
        let dib = r#"
          <sca:Format sca:MustHonor="true">dib</sca:Format>
          <sca:ImagesToTransfer sca:MustHonor="true">1</sca:ImagesToTransfer>
          <sca:InputSource sca:MustHonor="true">Platen</sca:InputSource>
          <sca:Resolution sca:MustHonor="true"><sca:Width>200</sca:Width><sca:Height>200</sca:Height></sca:Resolution>
        "#;
        assert!(w.validate_ticket(dib).valid);
    }

    #[test]
    fn accept_tiff_on_wsd_and_dib_on_usb() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>tiff-single-uncompressed</wscn:Format>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        assert!(w.validate_ticket(body).valid);
        // brother-wsd does not advertise DIB (lite WSD caps); MustHonor=true refuses.
        let body = r#"
          <wscn:Format wscn:MustHonor="true">dib</wscn:Format>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        assert!(!w.validate_ticket(body).valid);

        let mut u = brother_usb();
        let body = r#"
          <wscn:Format>dib</wscn:Format>
          <wscn:Resolution><wscn:Width>300</wscn:Width><wscn:Height>300</wscn:Height></wscn:Resolution>
        "#;
        assert!(u.validate_ticket(body).valid);
        let job = u
            .create_job(
                r#"
          <wscn:Format>bmp</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>300</wscn:Width><wscn:Height>300</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create dib");
        assert_eq!(job.format, "dib");
    }

    #[test]
    fn brother_wsd_retrieves_jfif_and_tiff() {
        let mut w = brother_wsd();
        for (fmt, magic) in [
            ("jfif", &b"\xff\xd8"[..]),
            ("tiff-single-uncompressed", &[0x49, 0x49, 0x2A, 0x00][..]),
        ] {
            let job = w
                .create_job(&format!(
                    r#"
          <wscn:Format>{fmt}</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#
                ))
                .unwrap_or_else(|e| panic!("create {fmt}: {e}"));
            let (_, bytes) = w.retrieve_image(job.id, &job.token).expect(fmt);
            assert!(
                bytes.starts_with(magic),
                "{fmt} magic {:?}",
                &bytes[..4.min(bytes.len())]
            );
        }
    }

    #[test]
    fn jpeg_com_toggle_omits_markers_on_retrieve() {
        let mut w = brother_wsd();
        assert!(w.jpeg_com());
        w.set_jpeg_com(false);
        assert!(!w.jpeg_com());
        let job = w
            .create_job(
                r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create jfif");
        let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert_eq!(image_gen::read_page_marker(&bytes), None);

        w.set_jpeg_com(true);
        let job = w
            .create_job(
                r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create jfif com");
        let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("page com");
        assert_eq!(image_gen::read_page_marker(&bytes), Some(0));
    }

    #[test]
    fn wia_plain_pattern_retrieve_omits_com_even_when_jpeg_com_on() {
        let mut w = brother_wsd();
        assert!(w.jpeg_com());
        w.set_image_pattern_id("wia_plain").unwrap();
        let job = w
            .create_job(
                r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#,
            )
            .expect("create jfif");
        let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert!(!image_gen::jpeg_has_com(&bytes));
        assert!(!image_gen::jpeg_has_app1(&bytes));
        assert!(image_gen::jpeg_is_baseline(&bytes));
        assert_eq!(image_gen::read_page_marker(&bytes), None);
        assert_eq!(w.status().image_pattern, "wia_plain");
    }

    #[test]
    fn extract_input_source() {
        let xml = r#"<wscn:InputSource>Platen</wscn:InputSource>"#;
        assert_eq!(extract_tag(xml, "InputSource").as_deref(), Some("Platen"));
    }

    #[test]
    fn must_honor_false_unsupported_dpi_substitutes() {
        let w = brother_wsd();
        assert!(w.profile().refuse_unknown);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:Resolution wscn:MustHonor="false"><wscn:Width>999</wscn:Width><wscn:Height>999</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.dpi, 300, "999 → nearest profile dpi");
        assert_eq!(v.format, "jfif");
    }

    #[test]
    fn create_job_marks_final_param_override_and_used_default() {
        let mut w = brother_wsd();
        w.set_pages(1);
        let overridden = r#"
          <wscn:Format wscn:MustHonor="false">png</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution wscn:MustHonor="false"><wscn:Width>999</wscn:Width><wscn:Height>999</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(overridden).expect("create");
        assert_eq!(job.format, "jfif");
        assert_eq!(job.dpi, 300);
        assert!(job.format_overridden, "png → preferred jfif");
        assert!(!job.format_used_default);
        assert!(job.dpi_overridden, "999 → nearest 300");
        assert!(!job.dpi_used_default);

        let defaults = r#"
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
        "#;
        let job2 = w.create_job(defaults).expect("create defaults");
        assert!(job2.format_used_default);
        assert!(!job2.format_overridden);
        assert!(job2.dpi_used_default);
        assert!(!job2.dpi_overridden);
    }

    #[test]
    fn must_honor_absent_unsupported_dpi_substitutes() {
        // Common.xsd default MustHonor=false.
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:Resolution><wscn:Width>999</wscn:Width><wscn:Height>999</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.dpi, 300);
        let (dpi, format) = w.corrected_ticket_params(&v);
        assert_eq!(dpi, 300);
        assert_eq!(format, "jfif");
    }

    #[test]
    fn must_honor_true_unsupported_dpi_refuses() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:Resolution wscn:MustHonor="true"><wscn:Width>999</wscn:Width><wscn:Height>999</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid);
        assert!(v.reason.as_deref().unwrap().contains("dpi"));
        assert_eq!(v.dpi, 999);
    }

    #[test]
    fn must_honor_false_unsupported_format_substitutes() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format wscn:MustHonor="false">png</wscn:Format>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.format, "jfif", "preferred_format on brother-wsd");
    }

    #[test]
    fn must_honor_true_unsupported_format_fault_phrase() {
        let mut w = brother_wsd();
        let body = r#"
          <wscn:Format wscn:MustHonor="true">png</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid);
        assert!(
            v.reason
                .as_deref()
                .is_some_and(|r| r.contains("ClientErrorDocumentFormatNotSupported")),
            "{v:?}"
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("ClientErrorDocumentFormatNotSupported"),
            "Create must hard-fault MustHonor format: {err}"
        );
    }

    #[test]
    fn must_honor_true_dpi_and_format_conflict() {
        let mut w = brother_wsd();
        let body = r#"
          <wscn:Format wscn:MustHonor="true">png</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution wscn:MustHonor="true"><wscn:Width>999</wscn:Width><wscn:Height>999</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid);
        let reason = v.reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("ClientErrorConflictingRequiredParameters"),
            "expected ConflictingRequiredParameters, got {v:?}"
        );
        assert!(
            reason.contains("Resolution") && reason.contains("Format"),
            "{reason}"
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("ClientErrorConflictingRequiredParameters"),
            "Create must hard-fault conflict: {err}"
        );
    }

    #[test]
    fn must_honor_true_unsupported_input_source_refuses() {
        let w = glass_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource wscn:MustHonor="true">ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid, "{v:?}");
        assert!(
            v.reason
                .as_deref()
                .is_some_and(|r| r.contains("InputSource")),
            "{v:?}"
        );
    }

    #[test]
    fn must_honor_false_unsupported_input_source_substitutes() {
        let w = glass_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource wscn:MustHonor="false">ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
    }

    #[test]
    fn must_honor_true_scan_region_out_of_bounds_refuses() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
          <wscn:ScanRegion>
            <wscn:ScanRegionXOffset>0</wscn:ScanRegionXOffset>
            <wscn:ScanRegionYOffset>0</wscn:ScanRegionYOffset>
            <wscn:ScanRegionWidth wscn:MustHonor="true">20000</wscn:ScanRegionWidth>
            <wscn:ScanRegionHeight>11700</wscn:ScanRegionHeight>
          </wscn:ScanRegion>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid, "{v:?}");
        assert!(
            v.reason
                .as_deref()
                .is_some_and(|r| r.contains("ScanRegion")),
            "{v:?}"
        );
    }

    #[test]
    fn adf_legal_scan_region_is_in_bounds() {
        // WIA feeder items advertise ADF max height 14000; MustHonor region tickets
        // must not be platen-bounded or Validate faults before CreateScanJob.
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
          <wscn:ScanRegion>
            <wscn:ScanRegionXOffset>0</wscn:ScanRegionXOffset>
            <wscn:ScanRegionYOffset>0</wscn:ScanRegionYOffset>
            <wscn:ScanRegionWidth wscn:MustHonor="true">8500</wscn:ScanRegionWidth>
            <wscn:ScanRegionHeight wscn:MustHonor="true">14000</wscn:ScanRegionHeight>
          </wscn:ScanRegion>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
    }

    #[test]
    fn must_honor_false_scan_region_out_of_bounds_ignored() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
          <wscn:ScanRegion>
            <wscn:ScanRegionXOffset>0</wscn:ScanRegionXOffset>
            <wscn:ScanRegionYOffset>0</wscn:ScanRegionYOffset>
            <wscn:ScanRegionWidth wscn:MustHonor="false">20000</wscn:ScanRegionWidth>
            <wscn:ScanRegionHeight>11700</wscn:ScanRegionHeight>
          </wscn:ScanRegion>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
    }

    #[test]
    fn must_honor_true_source_and_format_conflict() {
        let mut w = glass_wsd();
        let body = r#"
          <wscn:Format wscn:MustHonor="true">png</wscn:Format>
          <wscn:InputSource wscn:MustHonor="true">ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid);
        let reason = v.reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("ClientErrorConflictingRequiredParameters"),
            "{v:?}"
        );
        assert!(
            reason.contains("InputSource") && reason.contains("Format"),
            "{reason}"
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("ClientErrorConflictingRequiredParameters"),
            "{err}"
        );
    }

    /// brother-wsd with RGB24+Grayscale8 only (no BlackAndWhite1).
    fn brother_wsd_no_bw() -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut profile = Profile::load(&dir, "brother-wsd").expect("profile");
        profile.colors = vec!["RGB24".into(), "Grayscale8".into()];
        World::new(dir, profile)
    }

    #[test]
    fn must_honor_true_unsupported_color_refuses() {
        let w = brother_wsd_no_bw();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ColorProcessing wscn:MustHonor="true">BlackAndWhite1</wscn:ColorProcessing>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid, "{v:?}");
        assert!(
            v.reason
                .as_deref()
                .is_some_and(|r| r.contains("ColorProcessing")),
            "{v:?}"
        );
        assert_eq!(v.color, ColorMode::Bw1);
    }

    #[test]
    fn must_honor_false_unsupported_color_substitutes() {
        let w = brother_wsd_no_bw();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ColorProcessing wscn:MustHonor="false">BlackAndWhite1</wscn:ColorProcessing>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.color, ColorMode::Rgb24, "first profile color");
    }

    #[test]
    fn must_honor_absent_unsupported_color_substitutes() {
        let w = brother_wsd_no_bw();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ColorProcessing>BlackAndWhite1</wscn:ColorProcessing>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.color, ColorMode::Rgb24);
    }

    #[test]
    fn must_honor_true_format_and_color_conflict() {
        let mut w = brother_wsd_no_bw();
        let body = r#"
          <wscn:Format wscn:MustHonor="true">png</wscn:Format>
          <wscn:ColorProcessing wscn:MustHonor="true">BlackAndWhite1</wscn:ColorProcessing>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(!v.valid);
        let reason = v.reason.as_deref().unwrap_or("");
        assert!(
            reason.contains("ClientErrorConflictingRequiredParameters"),
            "{v:?}"
        );
        assert!(
            reason.contains("Format") && reason.contains("ColorProcessing"),
            "{reason}"
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("ClientErrorConflictingRequiredParameters"),
            "Create must hard-fault conflict: {err}"
        );
    }

    #[test]
    fn empty_colors_allow_list_soft_maps() {
        // glass-wsd omits `colors` → empty allow-list → any token soft-maps.
        let w = glass_wsd();
        assert!(w.profile().colors.is_empty());
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ColorProcessing>BlackAndWhite1</wscn:ColorProcessing>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let v = w.validate_ticket(body);
        assert!(v.valid, "{v:?}");
        assert_eq!(v.color, ColorMode::Bw1);
    }

    #[test]
    fn extract_must_honor_reads_prefixed_attr() {
        assert!(extract_must_honor(
            r#"<sca:Format sca:MustHonor="true">jfif</sca:Format>"#,
            "Format"
        ));
        assert!(!extract_must_honor(
            r#"<wscn:Format wscn:MustHonor="false">jfif</wscn:Format>"#,
            "Format"
        ));
        assert!(!extract_must_honor(
            r#"<wscn:Format>jfif</wscn:Format>"#,
            "Format"
        ));
        assert!(extract_must_honor(
            r#"<wscn:Resolution wscn:MustHonor="1"><wscn:Width>200</wscn:Width></wscn:Resolution>"#,
            "Resolution"
        ));
        assert!(extract_must_honor(
            r#"<wscn:ColorProcessing wscn:MustHonor="true">RGB24</wscn:ColorProcessing>"#,
            "ColorProcessing"
        ));
    }

    #[test]
    fn accept_known_ticket() {
        let w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        assert!(w.validate_ticket(body).valid);
    }

    #[test]
    fn retrieve_keeps_job_for_no_images() {
        let mut w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        let (snap, bytes) = w.retrieve_image(id, &token).expect("page");
        assert_eq!(snap.page_index, 1);
        assert_eq!(image_gen::read_page_marker(&bytes), Some(0));
        assert!(w.get_job(id).is_some());
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorNoImagesAvailable"));
    }

    #[test]
    fn retrieve_sets_scanning_and_transfering_mid_job() {
        let mut w = brother_wsd();
        w.set_pages(3);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>3</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        assert_eq!(job.state_reason, "JobScanning");
        assert_eq!(job.job_state(), "Pending");
        let id = job.id;
        let token = job.token.clone();

        let (snap, _) = w.retrieve_image(id, &token).expect("first page");
        assert!(snap.pages_left > 0, "need mid-transfer");
        assert_eq!(snap.job_state(), "Processing");
        assert_eq!(snap.state_reason, "JobScanningAndTransfering");
        assert_eq!(
            w.get_job(id).expect("active").state_reason,
            "JobScanningAndTransfering"
        );

        w.cancel_job(id).expect("cancel");
        assert!(w.get_job(id).is_none());
        let hist = w.job_history().last().expect("canceled history");
        assert_eq!(hist.state, "Canceled");
        assert_eq!(hist.reason, "JobCanceledAtDevice");
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(
            err.contains("ClientErrorJobCancelled"),
            "retrieve after cancel: {err}"
        );
        assert_eq!(
            w.missing_job_fault(id),
            format!("ClientErrorJobCancelled: job {id} was cancelled")
        );
    }

    #[test]
    fn status_exposes_fault_and_slow_knobs() {
        let mut w = brother_wsd();
        w.apply_faults(&FaultBody {
            jam: Some(true),
            cover_open: Some(true),
            multi_feed: Some(true),
            lamp_error: Some(true),
            busy: Some(false),
            offline: Some(false),
            temporary: Some(false),
            internal: Some(false),
            ms: Some(250),
            jam_after_n: Some(2),
            empty_mid_job: Some(3),
            offline_after_n: Some(4),
            multi_feed_after_n: Some(5),
            abort_retrieve: Some(true),
            abort_retrieve_after_n: Some(6),
            abort_retrieve_bytes: Some(1024),
            lamp_warmup_ms: Some(15),
            ..Default::default()
        });
        let st = w.status();
        assert_eq!(st.slow_retrieve_ms, 250);
        assert!(st.jam);
        assert!(st.cover_open);
        assert!(st.multi_feed);
        assert!(st.lamp_error);
        assert!(!st.busy);
        assert!(!st.offline);
        assert!(!st.temporary);
        assert!(!st.internal);
        assert_eq!(st.jam_after_n, 2);
        assert_eq!(st.empty_mid_job, 3);
        assert_eq!(st.offline_after_n, 4);
        assert_eq!(st.multi_feed_after_n, 5);
        assert!(st.abort_retrieve);
        assert_eq!(st.abort_retrieve_after_n, 6);
        assert_eq!(st.abort_retrieve_bytes, 1024);
        assert_eq!(st.lamp_warmup_ms, 15);
        assert!(st.refuse_unknown);
        assert_eq!(
            w.scanner_state_reasons(),
            ["MediaJam", "CoverOpen", "MultipleFeedError", "LampError"]
        );
    }

    #[test]
    fn offline_after_n_trips_and_sticky_offline_blocks_create() {
        let mut w = brother_wsd();
        w.set_pages(3);
        w.apply_faults(&FaultBody {
            offline_after_n: Some(1),
            ..FaultBody::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        let _ = w.retrieve_image(id, &token).expect("first page");
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(
            err.contains("NotAcceptingJobs") || err.contains("offline"),
            "{err}"
        );
        assert!(w.offline());
        assert!(w.scanner_state_reasons().contains(&"AttentionRequired"));
        let blocked = w.create_job(body).unwrap_err().to_string();
        assert!(
            blocked.contains("offline") || blocked.contains("NotAcceptingJobs"),
            "{blocked}"
        );
    }

    #[test]
    fn scanner_state_reasons_none_when_idle_even_if_feeder_empty() {
        let mut w = brother_wsd();
        w.set_feeder(false);
        w.set_jam(false);
        w.set_cover_open(false);
        assert_eq!(w.scanner_state_reasons(), ["None"]);
        assert!(!w.scanner_state_reasons().contains(&"AttentionRequired"));
    }

    #[test]
    fn scanner_state_reasons_jam_is_media_jam() {
        let mut w = brother_wsd();
        w.set_jam(true);
        w.set_cover_open(false);
        assert_eq!(w.scanner_state_reasons(), ["MediaJam"]);
    }

    #[test]
    fn scanner_state_reasons_cover_open() {
        let mut w = brother_wsd();
        w.set_jam(false);
        w.set_cover_open(true);
        assert_eq!(w.scanner_state_reasons(), ["CoverOpen"]);
    }

    #[test]
    fn scanner_state_reasons_calibrating() {
        let mut w = brother_wsd();
        w.apply_faults(&FaultBody {
            calibrating: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["Calibrating"]);
        assert!(w.status().calibrating);
        let changes = w.apply_faults(&FaultBody {
            calibrating: Some(true),
            ..Default::default()
        });
        assert!(changes.is_empty(), "noop: {changes:?}");
        let raised = w.apply_faults(&FaultBody {
            calibrating: Some(false),
            ..Default::default()
        });
        // Clear emits ConditionChange; raise was already true from first apply.
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].id, 5);
        assert_eq!(raised[0].name, "Calibrating");
        assert!(!raised[0].raised);
    }

    #[test]
    fn scanner_state_reasons_interlock_open() {
        let mut w = brother_wsd();
        let raised = w.apply_faults(&FaultBody {
            interlock: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["InterlockOpen"]);
        assert!(w.status().interlock_open);
        assert!(
            raised.iter().any(|c| {
                c.id == 6 && c.name == "InterlockOpen" && c.component == "MediaPath" && c.raised
            }),
            "{raised:?}"
        );
        // Alias interlock_open
        let mut w2 = brother_wsd();
        let body: FaultBody = serde_json::from_str(r#"{"interlock_open":true}"#).expect("alias");
        w2.apply_faults(&body);
        assert!(w2.interlock_open());
    }

    #[test]
    fn scanner_state_reasons_storage_full() {
        let mut w = brother_wsd();
        let raised = w.apply_faults(&FaultBody {
            storage_full: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["InternalStorageFull"]);
        assert!(w.status().storage_full);
        assert!(
            raised.iter().any(|c| {
                c.id == 7 && c.name == "InternalStorageFull" && c.component == "Platen" && c.raised
            }),
            "{raised:?}"
        );
    }

    #[test]
    fn scanner_state_reasons_paused() {
        let mut w = brother_wsd();
        let raised = w.apply_faults(&FaultBody {
            paused: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["Paused"]);
        assert!(w.status().paused);
        assert_eq!(raised.len(), 1);
        assert_eq!(raised[0].id, 0);
        assert_eq!(raised[0].name, "Paused");
        assert!(raised[0].raised);
    }

    #[test]
    fn scanner_state_reasons_lamp_warning_is_attention_required() {
        let mut w = brother_wsd();
        let raised = w.apply_faults(&FaultBody {
            lamp_warning: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["AttentionRequired"]);
        assert!(w.status().lamp_warning);
        assert!(
            raised.iter().any(|c| {
                c.id == 8 && c.name == "LampWarning" && c.component == "Platen" && c.raised
            }),
            "{raised:?}"
        );
        assert!(!w.scanner_state_reasons().contains(&"LampError"));
    }

    #[test]
    fn scanner_state_reasons_lamp_warming() {
        let mut w = brother_wsd();
        w.apply_faults(&FaultBody {
            lamp_warming: Some(true),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["LampWarming"]);
        assert!(w.status().lamp_warming);
        w.apply_faults(&FaultBody {
            lamp_warming: Some(false),
            ..Default::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["None"]);
    }

    #[test]
    fn retrieve_image_honors_slow_retrieve_ms() {
        use std::time::Instant;
        let mut w = brother_wsd();
        w.set_slow_retrieve_ms(40);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let t0 = Instant::now();
        let _ = w.retrieve_image(job.id, &job.token).expect("page");
        assert!(t0.elapsed().as_millis() >= 35);
    }

    /// feeder-tray5 (capacity 5, pages_max 5) with `feeder.initial_pages` set.
    fn tray5_with_initial(initial: u32) -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut profile = Profile::load(&dir, "feeder-tray5").expect("profile");
        profile.feeder.initial_pages = initial;
        World::new(dir, profile)
    }

    #[test]
    fn initial_pages_default_is_an_empty_feeder() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        for id in [
            "brother-wsd",
            "feeder-tray5",
            "feeder-tray50",
            "feeder-duplex30",
            "feeder-a4only",
        ] {
            let mut w = World::new(dir.clone(), Profile::load(&dir, id).unwrap());
            assert_eq!(w.initial_pages(), 0, "{id}");
            assert_eq!(w.pages_remaining(), 0, "{id}");
            w.set_initial_pages_override(None);
            w.reset();
            assert_eq!(w.pages_remaining(), 0, "{id}");
            assert_eq!(w.status().initial_pages, 0, "{id}");
        }
    }

    #[test]
    fn initial_pages_loads_on_override_reset_and_profile_reload() {
        let mut w = tray5_with_initial(0);
        w.set_initial_pages_override(Some(4));
        assert_eq!(w.pages_remaining(), 4);
        assert!(w.feeder_ready());
        assert_eq!(w.status().initial_pages, 4);
        // A run-time set_pages still overrides ...
        w.set_pages(1);
        assert_eq!(w.pages_remaining(), 1);
        // ... until the next reset, which restores the initial stack.
        w.reset();
        assert_eq!(w.pages_remaining(), 4);
        assert!(w.feeder_ready());
        w.set_pages(0);
        w.set_feeder(false);
        w.load_profile("feeder-tray5").unwrap();
        assert_eq!(w.pages_remaining(), 4, "profile reload reapplies it");
        assert!(w.feeder_ready());
        // Clearing the override falls back to the profile's value (0).
        w.set_initial_pages_override(None);
        w.reset();
        assert_eq!(w.pages_remaining(), 0);
        assert!(!w.feeder_ready());
    }

    #[test]
    fn profile_initial_pages_applies_and_override_wins() {
        let mut w = tray5_with_initial(3);
        assert_eq!(w.initial_pages(), 3);
        w.reset();
        assert_eq!(w.pages_remaining(), 3);
        assert!(w.feeder_ready());
        w.set_initial_pages_override(Some(2));
        assert_eq!(w.pages_remaining(), 2);
        w.set_initial_pages_override(Some(0));
        w.reset();
        assert_eq!(
            w.pages_remaining(),
            0,
            "an explicit 0 beats the profile's 3"
        );
    }

    #[test]
    fn initial_pages_clamps_to_capacity() {
        let mut w = tray5_with_initial(99);
        assert_eq!(w.initial_pages(), 5);
        w.reset();
        assert_eq!(w.pages_remaining(), 5);
        w.set_initial_pages_override(Some(6));
        assert_eq!(w.pages_remaining(), 5);
        assert_eq!(w.status().initial_pages, 5);
        assert_eq!(w.status().feeder_capacity, Some(5));
    }

    #[test]
    fn reset_clears_faults_and_slow() {
        let mut w = brother_wsd();
        w.apply_faults(&FaultBody {
            jam: Some(true),
            cover_open: Some(true),
            multi_feed: Some(true),
            lamp_error: Some(true),
            busy: Some(true),
            offline: Some(true),
            calibrating: Some(true),
            interlock: Some(true),
            storage_full: Some(true),
            paused: Some(true),
            lamp_warning: Some(true),
            lamp_warming: Some(true),
            temporary: Some(true),
            internal: Some(true),
            ms: Some(100),
            jam_after_n: Some(1),
            empty_mid_job: Some(2),
            offline_after_n: Some(3),
            multi_feed_after_n: Some(4),
            cover_open_after_n: Some(2),
            abort_retrieve: Some(true),
            abort_retrieve_after_n: Some(5),
            abort_retrieve_bytes: Some(512),
            lamp_warmup_ms: Some(50),
            empty_feeder_brother: Some(true),
            empty_feeder_delay_ms: Some(5),
        });
        w.reset();
        assert!(!w.empty_feeder_brother());
        assert!(!w.offline());
        assert_eq!(w.offline_after_n(), 0);
        assert!(!w.jam());
        assert!(!w.cover_open());
        assert!(!w.multi_feed());
        assert!(!w.lamp_error());
        assert!(!w.busy());
        assert!(!w.calibrating());
        assert!(!w.interlock_open());
        assert!(!w.storage_full());
        assert!(!w.paused());
        assert!(!w.lamp_warning());
        assert!(!w.lamp_warming());
        assert_eq!(w.multi_feed_after_n(), 0);
        assert!(!w.abort_retrieve());
        assert_eq!(w.abort_retrieve_after_n(), 0);
        assert_eq!(w.abort_retrieve_bytes(), 0);
        assert_eq!(w.slow_retrieve_ms(), 0);
        assert_eq!(w.retrieve_bps(), 0);
        assert_eq!(w.jam_after_n(), 0);
        assert_eq!(w.empty_mid_job(), 0);
        assert_eq!(w.lamp_warmup_ms(), 0);
    }

    #[test]
    fn lamp_error_and_busy_refuse_create() {
        let mut w = brother_wsd();
        w.apply_faults(&FaultBody {
            lamp_error: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["LampError"]);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("LampError") || err.contains("NotAcceptingJobs"),
            "{err}"
        );
        w.reset();
        w.apply_faults(&FaultBody {
            busy: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(w.scanner_state_reasons(), ["None"]);
        assert_eq!(
            w.not_accepting_reason(),
            Some("ServerErrorNotAcceptingJobs: device busy")
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("busy") || err.contains("NotAcceptingJobs"),
            "{err}"
        );
    }

    #[test]
    fn temporary_and_internal_refuse_create() {
        let mut w = brother_wsd();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        w.apply_faults(&FaultBody {
            temporary: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(
            w.not_accepting_reason(),
            Some("ServerErrorTemporaryError: temporary")
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(err.contains("Temporary"), "{err}");
        w.reset();
        w.apply_faults(&FaultBody {
            internal: Some(true),
            ..FaultBody::default()
        });
        assert_eq!(
            w.not_accepting_reason(),
            Some("ServerErrorInternalError: internal")
        );
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(err.contains("Internal"), "{err}");
        assert!(w.status().internal);
    }

    const ADF_ONE: &str = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;

    /// A one-page job takes one sheet; the rest stay loaded for the next CreateScanJob.
    #[test]
    fn single_page_adf_jobs_consume_one_sheet_each() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut w = World::new(
            dir.clone(),
            Profile::load(&dir, "wia-rich").expect("wia-rich"),
        );
        w.set_jpeg_com(true);
        w.set_pages(3);
        for want_base in 0..3u32 {
            let job = w.create_job(ADF_ONE).expect("create");
            assert_eq!(job.pages_left, 1);
            assert_eq!(job.sheet_base, want_base, "job reads sheet {want_base}");
            assert_eq!(w.pages_remaining(), 2 - want_base);
            let (j, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
            assert_eq!(j.pages_left, 0);
            // COM marker carries the sheet index the page was rendered from.
            assert_eq!(
                image_gen::read_page_marker(&bytes),
                Some(want_base),
                "job {want_base} must scan sheet {want_base}"
            );
            w.cancel_job(job.id).unwrap();
        }
        let err = w.create_job(ADF_ONE).unwrap_err().to_string();
        assert!(err.contains("feeder empty"), "4th job: {err}");
    }

    /// ImagesToTransfer 0 / absent still drains whatever is left; N>left clamps to what is left.
    #[test]
    fn adf_all_and_overask_take_only_what_is_left() {
        let mut w = brother_usb();
        w.set_pages(3);
        let one = w.create_job(ADF_ONE).expect("one");
        assert_eq!(w.pages_remaining(), 2);
        let all = w
            .create_job(&ADF_ONE.replace("ImagesToTransfer>1<", "ImagesToTransfer>0<"))
            .expect("rest");
        assert_eq!((all.pages_left, all.sheet_base), (2, 1));
        assert_eq!(w.pages_remaining(), 0);
        w.set_pages(2);
        let over = w
            .create_job(&ADF_ONE.replace("ImagesToTransfer>1<", "ImagesToTransfer>9<"))
            .expect("over-ask");
        assert_eq!((over.pages_left, over.sheet_base), (2, 0));
        assert_eq!(w.pages_remaining(), 0);
        let _ = one;
    }

    /// Fault-plan page numbers count along the stack, so `page 2` fires on the 2nd one-page job.
    #[test]
    fn fault_plan_page_counts_across_one_page_jobs() {
        let mut w = brother_usb();
        w.set_pages(3);
        let b: crate::state::faultplan::PlanBody = serde_json::from_value(serde_json::json!(
            {"faults":[{"kind":"jam","when":{"after_pages":1}}]}
        ))
        .unwrap();
        w.add_fault_plan(b).unwrap();
        let j1 = w.create_job(ADF_ONE).unwrap();
        w.retrieve_image(j1.id, &j1.token)
            .expect("page 1 delivered");
        let j2 = w.create_job(ADF_ONE).unwrap();
        let err = w.retrieve_image(j2.id, &j2.token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorMediaJam"), "{err}");
    }

    #[test]
    fn sticky_empty_adf_fails_platen_leaves_tray() {
        let mut w = brother_usb();
        w.apply_scenario("sticky_empty").unwrap();
        let st = w.status();
        assert!(st.sticky_ready);
        assert!(st.feeder_ready);
        assert_eq!(st.pages_remaining, 0);

        let adf = r#"
          <wscn:Format>bmp</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let err = w.create_job(adf).unwrap_err().to_string();
        assert!(err.contains("feeder empty"), "{err}");

        let platen = r#"
          <wscn:Format>bmp</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(platen).expect("platen");
        let _ = w.retrieve_image(job.id, &job.token).expect("page");
        let after = w.status();
        assert_eq!(after.pages_remaining, 0);
        assert!(after.feeder_ready);
        assert!(after.sticky_ready);
    }

    /// A cancel mid-stack leaves the sheets the feeder never pulled in the tray (measured on Windows' own client: cancel
    /// at 3 of 20 sheets, 5 fetched, then CancelJob); the next job starts at the first of them.
    #[test]
    fn a_cancelled_job_leaves_the_sheets_it_never_fed_in_the_tray() {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut w = World::new(
            dir.clone(),
            Profile::load(&dir, "wia-rich").expect("wia-rich"),
        );
        w.set_jpeg_com(true);
        w.set_pages(20);
        let all = ADF_ONE.replace("ImagesToTransfer>1<", "ImagesToTransfer>0<");
        let job = w.create_job(&all).expect("create");
        assert_eq!(
            (job.pages_left, w.pages_remaining()),
            (20, 0),
            "the whole stack is the job's"
        );
        for _ in 0..5 {
            w.retrieve_image(job.id, &job.token).expect("page");
        }
        w.cancel_job(job.id).unwrap();
        assert_eq!(w.pages_remaining(), 15);
        assert!(w.status().feeder_ready);
        let next = w.create_job(ADF_ONE).expect("next job");
        assert_eq!(next.sheet_base, 5);
        let (_, bytes) = w.retrieve_image(next.id, &next.token).expect("page");
        assert_eq!(
            image_gen::read_page_marker(&bytes),
            Some(5),
            "the next job scans the sixth sheet"
        );
        // A cancel after the job completed returns nothing.
        w.cancel_job(next.id).unwrap();
        assert_eq!(w.pages_remaining(), 14);
    }

    /// A duplex sheet whose front was delivered has been fed; only untouched sheets go back. A timed-out job returns
    /// its unfed sheets the same way.
    #[test]
    fn a_cancelled_or_timed_out_duplex_job_returns_only_untouched_sheets() {
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADFDuplex</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let mut w = brother_wsd_duplex();
        w.set_pages(4);
        let job = w.create_job(body).expect("create duplex");
        for _ in 0..3 {
            w.retrieve_image(job.id, &job.token).expect("face"); // sheet 1 front and back, sheet 2 front
        }
        w.cancel_job(job.id).unwrap();
        assert_eq!(w.pages_remaining(), 2, "sheets 3 and 4");
        assert_eq!(w.create_job(body).expect("next").sheet_base, 2);

        let mut w = brother_wsd_duplex();
        w.set_pages(4);
        let job = w.create_job(body).expect("create duplex");
        w.retrieve_image(job.id, &job.token).expect("face");
        w.mark_job_timed_out(job.id).unwrap();
        assert_eq!(w.pages_remaining(), 3);
        w.mark_job_timed_out(job.id).unwrap();
        assert_eq!(w.pages_remaining(), 3, "a second timeout returns nothing");
        w.cancel_job(job.id).unwrap();
        assert_eq!(
            w.pages_remaining(),
            3,
            "nor does a cancel of the aborted job"
        );
    }

    #[test]
    fn hardware_adfduplex_emits_front_then_back_per_sheet() {
        let mut w = brother_wsd_duplex();
        assert!(w.status().duplex);
        w.set_pages(2);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADFDuplex</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create duplex");
        assert!(job.duplex);
        assert_eq!(job.pages_left, 4, "2 sheets × 2 faces");
        assert_eq!(job.input_source, "ADFDuplex");
        let id = job.id;
        let token = job.token.clone();
        let mut sides = Vec::new();
        for _ in 0..4 {
            let (_, bytes) = w.retrieve_image(id, &token).expect("face");
            sides.push(image_gen::read_side_marker(&bytes));
        }
        assert_eq!(
            sides,
            [
                Some(PageSide::Front),
                Some(PageSide::Back),
                Some(PageSide::Front),
                Some(PageSide::Back)
            ]
        );
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("NoImagesAvailable"));
    }

    #[test]
    fn adfduplex_refused_when_profile_has_no_duplex() {
        let mut w = glass_wsd();
        w.set_pages(1);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>ADFDuplex</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let err = w.create_job(body).unwrap_err().to_string();
        assert!(
            err.contains("ADFDuplex") || err.contains("not offered"),
            "{err}"
        );
    }

    #[test]
    fn plan_abort_retrieve_arms_sticky_and_cut_bytes() {
        let mut w = brother_wsd();
        assert!(!w.plan_abort_retrieve(0));
        w.apply_faults(&FaultBody {
            abort_retrieve_after_n: Some(1),
            ..FaultBody::default()
        });
        assert!(!w.plan_abort_retrieve(0));
        assert!(w.plan_abort_retrieve(1));
        assert!(w.abort_retrieve());
        assert!(w.plan_abort_retrieve(99));
        assert_eq!(w.abort_cut_bytes(100), 50);
        w.apply_faults(&FaultBody {
            abort_retrieve_bytes: Some(10),
            ..FaultBody::default()
        });
        assert_eq!(w.abort_cut_bytes(100), 10);
        assert_eq!(w.abort_cut_bytes(8), 7);
    }

    #[test]
    fn apply_slow_sets_ms_and_optional_bps() {
        let mut w = brother_wsd();
        w.apply_slow(&SlowBody {
            ms: 10,
            bps: Some(32_000),
        });
        assert_eq!(w.slow_retrieve_ms(), 10);
        assert_eq!(w.retrieve_bps(), 32_000);
        // ms-only leaves bps alone.
        w.apply_slow(&SlowBody { ms: 5, bps: None });
        assert_eq!(w.slow_retrieve_ms(), 5);
        assert_eq!(w.retrieve_bps(), 32_000);
        w.apply_slow(&SlowBody {
            ms: 0,
            bps: Some(0),
        });
        assert_eq!(w.retrieve_bps(), 0);
        w.reset();
        assert_eq!(w.retrieve_bps(), 0);
    }

    #[test]
    fn multi_feed_after_n_trips_and_sticky_reason() {
        let mut w = brother_wsd();
        w.set_pages(3);
        w.apply_faults(&FaultBody {
            multi_feed_after_n: Some(1),
            ..FaultBody::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        let _ = w.retrieve_image(id, &token).expect("first page");
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorMultipleFeed"), "{err}");
        assert!(w.multi_feed());
        assert_eq!(w.scanner_state_reasons(), ["MultipleFeedError"]);
    }

    #[test]
    fn lamp_warmup_delays_first_image_only() {
        use std::time::Instant;
        // Wall-clock test: the old 40 ms warm-up was comparable to a (debug-build) page encode
        // under parallel test load, so `second <= first` flaked whenever the second page was
        // scheduled late. A warm-up far above any encode makes the contrast unambiguous.
        const WARMUP_MS: u64 = 400;
        let mut w = brother_wsd();
        w.set_pages(2);
        w.apply_faults(&FaultBody {
            lamp_warmup_ms: Some(WARMUP_MS),
            ..Default::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        let t0 = Instant::now();
        w.retrieve_image(id, &token).expect("first");
        let first_ms = t0.elapsed().as_millis();
        assert!(first_ms >= u128::from(WARMUP_MS) - 5, "first={first_ms}");
        let t1 = Instant::now();
        w.retrieve_image(id, &token).expect("second");
        let second_ms = t1.elapsed().as_millis();
        // Second image skips warm-up (page_index > 0): it must not pay the 400 ms again. Compare the
        // two rather than testing an absolute time: a debug-build encode on a loaded CI runner alone
        // can take several hundred ms, but the first image is always the warm-up slower.
        assert!(
            second_ms + u128::from(WARMUP_MS) / 2 < first_ms,
            "first={first_ms} second={second_ms}"
        );
    }

    #[test]
    fn jam_after_n_trips_on_next_retrieve() {
        let mut w = brother_wsd();
        w.set_pages(4);
        w.apply_faults(&FaultBody {
            jam_after_n: Some(2),
            ..Default::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>4</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        w.retrieve_image(id, &token).expect("page 0");
        w.retrieve_image(id, &token).expect("page 1");
        assert!(!w.jam());
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorMediaJam"), "{err}");
        assert!(w.jam());
        assert_eq!(w.scanner_state_reasons(), ["MediaJam"]);
        assert_eq!(
            w.get_job(id).expect("still active").state_reason,
            "ScannerStopped"
        );
        let changes = w.drain_pending_condition_changes();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].id, 1);
        assert_eq!(changes[0].name, "MediaJam");
        assert!(changes[0].raised);
    }

    #[test]
    fn offline_after_n_enqueues_attention_required_notify() {
        let mut w = brother_wsd();
        w.set_pages(2);
        w.apply_faults(&FaultBody {
            offline_after_n: Some(1),
            ..Default::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        w.retrieve_image(id, &token).expect("first page");
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ServerErrorNotAcceptingJobs"), "{err}");
        assert!(w.offline());
        let changes = w.drain_pending_condition_changes();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].id, 0);
        assert_eq!(changes[0].name, "AttentionRequired");
        assert!(changes[0].raised);
    }

    #[test]
    fn sticky_jam_marks_active_job_scanner_stopped() {
        let mut w = brother_wsd();
        w.set_pages(2);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        w.retrieve_image(id, &token).expect("first");
        w.apply_faults(&FaultBody {
            jam: Some(true),
            ..Default::default()
        });
        assert_eq!(
            w.get_job(id).expect("active").state_reason,
            "ScannerStopped"
        );
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorMediaJam"), "{err}");
    }

    #[test]
    fn throttled_retrieve_uses_job_transfering_mid_job() {
        let mut w = brother_wsd();
        w.set_pages(3);
        w.apply_slow(&SlowBody {
            ms: 0,
            bps: Some(1_000_000),
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>3</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let (snap, _) = w.retrieve_image(job.id, &job.token).expect("first");
        assert!(snap.pages_left > 0);
        assert_eq!(snap.state_reason, "JobTransfering");
    }

    #[test]
    fn abort_retrieve_last_page_is_job_completed_with_errors() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.apply_faults(&FaultBody {
            abort_retrieve: Some(true),
            ..Default::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let (snap, _) = w
            .retrieve_image(job.id, &job.token)
            .expect("retrieve ok before MTOM cut");
        assert_eq!(snap.state_reason, "JobCompletedWithErrors");
        assert_eq!(
            w.job_history().last().expect("hist").reason,
            "JobCompletedWithErrors"
        );
    }

    #[test]
    fn complete_with_lamp_warning_is_job_completed_with_warnings() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.apply_faults(&FaultBody {
            lamp_warning: Some(true),
            ..Default::default()
        });
        // LampWarning stops Create via AttentionRequired path? No — only path_stopped.
        // But Stopped Status + AttentionRequired; Create still allowed unless offline.
        // path_stopped does not include lamp_warning — Create OK, Retrieve OK.
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        // lamp_warning → Stopped scanner state; Create still accepted (warning, not path refuse).
        let job = w.create_job(body).expect("create");
        let (snap, _) = w.retrieve_image(job.id, &job.token).expect("retrieve");
        assert_eq!(snap.state_reason, "JobCompletedWithWarnings");
        let hist = w.job_history().last().expect("history");
        assert_eq!(hist.reason, "JobCompletedWithWarnings");
    }

    #[test]
    fn empty_mid_job_ends_early() {
        let mut w = brother_wsd();
        w.set_pages(5);
        w.apply_faults(&FaultBody {
            empty_mid_job: Some(2),
            ..Default::default()
        });
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>5</wscn:ImagesToTransfer>
          <wscn:InputSource>Feeder</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let id = job.id;
        let token = job.token.clone();
        w.retrieve_image(id, &token).expect("page 0");
        w.retrieve_image(id, &token).expect("page 1");
        let err = w.retrieve_image(id, &token).unwrap_err().to_string();
        assert!(err.contains("ClientErrorNoImagesAvailable"), "{err}");
        assert!(!w.jam());
        let left = w.get_job(id).expect("job kept").pages_left;
        assert_eq!(left, 0);
    }

    #[test]
    fn effects_preset_and_reset() {
        let mut w = brother_wsd();
        assert!(w.status().effects.clean);
        w.apply_effects(&EffectsBody {
            preset: Some("dirty_adf".into()),
            seed: Some(7),
            ..Default::default()
        })
        .unwrap();
        let st = w.status().effects;
        assert!(!st.clean);
        assert!(st.streaks > 0);
        assert_eq!(st.seed, 7);
        w.reset();
        assert!(w.status().effects.clean);
    }

    #[test]
    fn effects_same_seed_stable_jpeg() {
        let mut w = brother_wsd();
        w.apply_effects(&EffectsBody {
            seed: Some(123),
            speckles: Some(0.04),
            ..Default::default()
        })
        .unwrap();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let j1 = w.create_job(body).unwrap();
        let (_, a) = w.retrieve_image(j1.id, &j1.token).unwrap();
        let j2 = w.create_job(body).unwrap();
        let (_, b) = w.retrieve_image(j2.id, &j2.token).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn gray8_ticket_collapses_patch_chroma() {
        use image::load_from_memory;
        let mut w = brother_wsd();
        w.set_image_pattern_id("patches").unwrap();
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ColorProcessing>Grayscale8</wscn:ColorProcessing>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).unwrap();
        assert_eq!(job.color, ColorMode::Gray8);
        let (_, bytes) = w.retrieve_image(job.id, &job.token).unwrap();
        let img = load_from_memory(&bytes).unwrap().to_rgb8();
        let mut chroma = 0u32;
        for p in img.pixels() {
            let d = p[0].abs_diff(p[1]) as u32 + p[1].abs_diff(p[2]) as u32;
            chroma = chroma.max(d);
        }
        assert!(
            chroma <= 2,
            "expected near-zero chroma after Gray8, max delta {chroma}"
        );
    }

    #[test]
    fn apply_scenario_order_loads_pages_and_status_check() {
        let mut w = brother_wsd();
        w.apply_scenario("order").unwrap();
        assert_eq!(w.image_pattern(), PatternId::PageId);
        assert_eq!(w.pages_remaining(), 3);
        assert!(w.feeder_ready());
        let st = w.status();
        assert_eq!(st.scenario.as_deref(), Some("order"));
        assert_eq!(st.scenario_step, Some(1));
        assert_eq!(st.scenario_of, Some(7));
        assert_eq!(st.scenario_next.as_deref(), Some("qr"));
        assert!(st.scenario_check.as_ref().unwrap().contains("Multi-page"));
        w.set_image_pattern_id("wedges").unwrap();
        assert!(w.status().scenario.is_none());
    }

    #[test]
    fn apply_scenario_duplex_primes_front_pass() {
        let mut w = brother_wsd();
        w.apply_scenario("duplex").unwrap();
        assert_eq!(w.image_pattern(), PatternId::DuplexPair);
        assert_eq!(w.page_side(), Some(PageSide::Front));
        assert_eq!(w.pages_remaining(), 3);
        assert_eq!(w.status().scenario_next, None);
    }

    #[test]
    fn pattern_selection_affects_retrieve_and_status() {
        let mut w = brother_wsd();
        assert_eq!(w.status().image_pattern, "diag");
        w.set_image_pattern_id("wedges").unwrap();
        assert_eq!(w.image_pattern(), PatternId::Wedges);
        assert_eq!(w.status().image_pattern, "wedges");
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
        assert_eq!(image_gen::read_page_marker(&bytes), Some(0));
        let expected = image_gen::jpeg_pattern(PatternId::Wedges, 100, 0).unwrap();
        assert_eq!(bytes, expected);
        w.reset();
        assert_eq!(w.status().image_pattern, "diag");
    }

    #[test]
    fn set_pages_count_alone_preserves_feeder_counting() {
        let mut w = brother_wsd();
        w.set_pages(4);
        assert_eq!(w.pages_remaining(), 4);
        assert!(w.feeder_ready());
        assert_eq!(w.page_side(), None);
        assert_eq!(w.page_order(), PageOrder::Forward);
        let st = w.status();
        assert_eq!(st.pages_remaining, 4);
        assert!(st.page_side.is_none());
        assert_eq!(st.page_order, "forward");
    }

    #[test]
    fn loopback_echoes_exact_jfif_bytes() {
        let mut w = brother_wsd();
        let page0 = image_gen::jpeg_pattern(PatternId::PageId, 50, 0).unwrap();
        let page1 = image_gen::jpeg_pattern(PatternId::Wedges, 50, 1).unwrap();
        let b64 = |b: &[u8]| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(b)
        };
        w.set_loopback(&LoopbackBody {
            pages: vec![b64(&page0), b64(&page1)],
        })
        .unwrap();
        assert_eq!(w.pages_remaining(), 2);
        assert!(w.loopback_active());
        let st = w.loopback_status();
        assert_eq!(st.count, 2);
        assert_eq!(st.digests.len(), 2);
        assert_eq!(w.status().loopback_pages, 2);

        let ticket = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(ticket).expect("create");
        let (_, a) = w.retrieve_image(job.id, &job.token).expect("p0");
        let (_, b) = w.retrieve_image(job.id, &job.token).expect("p1");
        assert_eq!(a, page0);
        assert_eq!(b, page1);

        // Pattern selection clears loopback.
        w.set_image_pattern_id("wedges").unwrap();
        assert!(!w.loopback_active());
        assert_eq!(w.status().loopback_pages, 0);
    }

    #[test]
    fn loopback_cleared_by_reset_and_empty_body() {
        let mut w = brother_wsd();
        let page = image_gen::jpeg_pattern(PatternId::PageId, 50, 0).unwrap();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&page);
        w.set_loopback(&LoopbackBody { pages: vec![b64] }).unwrap();
        assert!(w.loopback_active());
        w.set_loopback(&LoopbackBody { pages: vec![] }).unwrap();
        assert!(!w.loopback_active());
        w.set_loopback(&LoopbackBody {
            pages: vec![base64::engine::general_purpose::STANDARD.encode(&page)],
        })
        .unwrap();
        w.reset();
        assert!(!w.loopback_active());
    }

    #[test]
    fn apply_pages_clears_loopback_and_loopback_clears_side() {
        let mut w = brother_wsd();
        let page = image_gen::jpeg_pattern(PatternId::PageId, 50, 0).unwrap();
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&page);
        w.set_loopback(&LoopbackBody {
            pages: vec![b64.clone()],
        })
        .unwrap();
        assert!(w.loopback_active());
        w.apply_pages(&PagesBody {
            count: 2,
            side: Some("front".into()),
            order: None,
        })
        .unwrap();
        assert!(!w.loopback_active());
        assert_eq!(w.page_side(), Some(PageSide::Front));

        w.set_loopback(&LoopbackBody { pages: vec![b64] }).unwrap();
        assert!(w.loopback_active());
        assert_eq!(w.page_side(), None);
        assert_eq!(w.image_pattern(), PatternId::Diag);

        w.apply_scenario("order").unwrap();
        assert!(!w.loopback_active());
    }

    #[test]
    fn apply_pages_front_then_reverse_backs_for_manual_duplex() {
        let mut w = brother_wsd();
        // Pass 1: fronts in order → sheets 0,1,2 tagged front.
        w.apply_pages(&PagesBody {
            count: 3,
            side: Some("front".into()),
            order: None,
        })
        .unwrap();
        assert_eq!(w.image_pattern(), PatternId::DuplexPair);
        assert_eq!(w.page_side(), Some(image_gen::PageSide::Front));
        assert_eq!(w.status().page_side.as_deref(), Some("front"));

        let ticket = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>0</wscn:ImagesToTransfer>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:Resolution><wscn:Width>100</wscn:Width><wscn:Height>100</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(ticket).expect("fronts job");
        let mut fronts = Vec::new();
        for expect_sheet in 0u32..3 {
            let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("front");
            assert_eq!(image_gen::read_page_marker(&bytes), Some(expect_sheet));
            assert_eq!(
                image_gen::read_side_marker(&bytes),
                Some(image_gen::PageSide::Front)
            );
            fronts.push(bytes);
        }

        // Pass 2: flip stack — backs arrive last-sheet-first (2,1,0).
        w.apply_pages(&PagesBody {
            count: 3,
            side: Some("back".into()),
            order: Some("reverse".into()),
        })
        .unwrap();
        assert_eq!(w.page_order(), PageOrder::Reverse);
        assert_eq!(w.status().page_order, "reverse");
        assert_eq!(w.status().page_side.as_deref(), Some("back"));

        let job = w.create_job(ticket).expect("backs job");
        let mut backs_as_scanned = Vec::new();
        for expect_sheet in [2u32, 1, 0] {
            let (_, bytes) = w.retrieve_image(job.id, &job.token).expect("back");
            assert_eq!(image_gen::read_page_marker(&bytes), Some(expect_sheet));
            assert_eq!(
                image_gen::read_side_marker(&bytes),
                Some(image_gen::PageSide::Back)
            );
            backs_as_scanned.push(bytes);
        }

        // pdfcraft interleave_manual_duplex: reverse backs, zip with fronts.
        let mut backs = backs_as_scanned;
        backs.reverse();
        let interleaved: Vec<_> = fronts
            .into_iter()
            .zip(backs)
            .flat_map(|(f, b)| [f, b])
            .collect();
        assert_eq!(interleaved.len(), 6);
        for (i, bytes) in interleaved.iter().enumerate() {
            let sheet = (i / 2) as u32;
            let side = if i % 2 == 0 {
                image_gen::PageSide::Front
            } else {
                image_gen::PageSide::Back
            };
            assert_eq!(image_gen::read_page_marker(bytes), Some(sheet));
            assert_eq!(image_gen::read_side_marker(bytes), Some(side));
        }
    }

    #[test]
    fn retrieve_after_idle_timeout_faults_job_timed_out() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.set_job_timeout(Duration::from_millis(1));
        let body = r#"
            <wscn:ScanTicket>
              <wscn:DocumentParameters>
                <wscn:Format>jfif</wscn:Format>
                <wscn:InputSource>Platen</wscn:InputSource>
              </wscn:DocumentParameters>
            </wscn:ScanTicket>
        "#;
        let job = w.create_job(body).expect("create");
        w.age_job_activity(job.id, Duration::from_secs(2))
            .expect("age");
        let err = w
            .retrieve_image(job.id, &job.token)
            .expect_err("should JobTimedOut")
            .to_string();
        assert!(
            err.contains("JobTimedOut"),
            "expected JobTimedOut in fault, got {err}"
        );
        assert!(err.contains("OperationFailed"), "got {err}");
        let tomb = w.get_job(job.id).expect("tombstone until sweep");
        assert!(tomb.aborted);
        assert_eq!(tomb.state_reason, "JobTimedOut");
        assert_eq!(tomb.job_state(), "Aborted");
    }

    #[test]
    fn get_job_elements_path_times_out_via_ensure() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.set_job_timeout(Duration::from_millis(1));
        let body = r#"
            <wscn:ScanTicket>
              <wscn:DocumentParameters>
                <wscn:Format>jfif</wscn:Format>
                <wscn:InputSource>Platen</wscn:InputSource>
              </wscn:DocumentParameters>
            </wscn:ScanTicket>
        "#;
        let job = w.create_job(body).expect("create");
        w.age_job_activity(job.id, Duration::from_secs(2))
            .expect("age");
        let err = w
            .ensure_job_not_timed_out(job.id)
            .expect_err("timed out")
            .to_string();
        assert!(err.contains("JobTimedOut"), "{err}");
    }

    #[test]
    fn retrieve_within_timeout_still_ok_and_resets_activity() {
        let mut w = brother_wsd();
        w.set_pages(2);
        w.set_job_timeout(Duration::from_secs(60));
        let body = r#"
            <wscn:ScanTicket>
              <wscn:DocumentParameters>
                <wscn:Format>jfif</wscn:Format>
                <wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>
                <wscn:InputSource>ADF</wscn:InputSource>
              </wscn:DocumentParameters>
            </wscn:ScanTicket>
        "#;
        let job = w.create_job(body).expect("create");
        let (j1, bytes) = w.retrieve_image(job.id, &job.token).expect("first");
        assert!(!bytes.is_empty());
        assert_eq!(j1.page_index, 1);
        // Age past timeout from create, but last_activity was reset on retrieve.
        w.age_job_activity(job.id, Duration::from_millis(1)).ok();
        let (_j2, bytes2) = w
            .retrieve_image(job.id, &job.token)
            .expect("second still ok");
        assert!(!bytes2.is_empty());
    }

    #[test]
    fn sweep_timed_out_jobs_clears_abandoned_on_create() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.set_job_timeout(Duration::from_millis(1));
        let body = r#"
            <wscn:ScanTicket>
              <wscn:DocumentParameters>
                <wscn:Format>jfif</wscn:Format>
                <wscn:InputSource>Platen</wscn:InputSource>
              </wscn:DocumentParameters>
            </wscn:ScanTicket>
        "#;
        let stale = w.create_job(body).expect("stale");
        w.age_job_activity(stale.id, Duration::from_secs(2))
            .expect("age");
        w.set_pages(1);
        let _fresh = w.create_job(body).expect("fresh create sweeps");
        assert!(
            w.get_job(stale.id).is_none(),
            "stale JobTimedOut tombstone should be swept on CreateScanJob"
        );
    }

    #[test]
    fn push_scan_create_requires_matching_identifier_and_token() {
        let mut w = brother_wsd();
        w.set_pages(1);
        w.register_destination("tok-lab", "ctx-lab");
        let pending = w.mint_push_scan(Some("tok-lab")).expect("mint");
        assert_eq!(w.status().pending_push_scans, 1);

        let bad_id = r#"
          <wscn:ScanIdentifier>uuid:dead</wscn:ScanIdentifier>
          <wscn:DestinationToken>tok-lab</wscn:DestinationToken>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#.to_string();
        let err = w.create_job(&bad_id).unwrap_err().to_string();
        assert!(err.contains("ClientErrorInvalidScanIdentifier"), "{err}");

        let bad_tok = format!(
            r#"
          <wscn:ScanIdentifier>{}</wscn:ScanIdentifier>
          <wscn:DestinationToken>tok-wrong</wscn:DestinationToken>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#,
            pending.scan_identifier
        );
        let err = w.create_job(&bad_tok).unwrap_err().to_string();
        assert!(err.contains("ClientErrorInvalidDestinationToken"), "{err}");

        let ok = format!(
            r#"
          <wscn:ScanIdentifier>{}</wscn:ScanIdentifier>
          <wscn:DestinationToken>{}</wscn:DestinationToken>
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#,
            pending.scan_identifier, pending.destination_token
        );
        let job = w.create_job(&ok).expect("push create");
        assert_eq!(w.status().pending_push_scans, 0);
        // Identifier is one-shot.
        let err = w.create_job(&ok).unwrap_err().to_string();
        assert!(err.contains("ClientErrorInvalidScanIdentifier"), "{err}");
        let (_snap, bytes) = w.retrieve_image(job.id, &job.token).expect("page");
        assert!(!bytes.is_empty());
        assert_eq!(w.job_history().len(), 1);
        assert_eq!(w.job_history()[0].state, "Completed");
    }

    #[test]
    fn retrieve_bad_job_token_faults_invalid_job_token() {
        let mut w = brother_wsd();
        w.set_pages(1);
        let body = r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:ImagesToTransfer>1</wscn:ImagesToTransfer>
          <wscn:InputSource>Platen</wscn:InputSource>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>
        "#;
        let job = w.create_job(body).expect("create");
        let err = w
            .retrieve_image(job.id, "urn:uuid:not-the-token")
            .unwrap_err()
            .to_string();
        assert!(err.contains("ClientErrorInvalidJobToken"), "{err}");
    }

    #[test]
    fn push_scan_partial_credentials_invalid_args() {
        let mut w = brother_wsd();
        w.register_destination("tok-lab", "ctx");
        let only_id = r#"
          <wscn:ScanIdentifier>uuid:x</wscn:ScanIdentifier>
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>Platen</wscn:InputSource>
        "#;
        let err = w.create_job(only_id).unwrap_err().to_string();
        assert!(err.contains("InvalidArgs"), "{err}");
    }

    #[test]
    fn register_stores_notify_to_for_mint() {
        let mut w = brother_wsd();
        w.register_destination("tok-lab", "ctx-lab");
        w.set_notify_to(Some("http://127.0.0.1:9/sink"));
        assert_eq!(w.notify_to(), Some("http://127.0.0.1:9/sink"));
        let pending = w.mint_push_scan(Some("tok-lab")).expect("mint");
        assert_eq!(
            pending.notify_to.as_deref(),
            Some("http://127.0.0.1:9/sink")
        );
        assert_eq!(pending.client_context, "ctx-lab");
        w.set_notify_to(None);
        assert!(w.notify_to().is_none());
    }

    #[test]
    fn clear_subscriptions_drops_tokens_and_notify_to() {
        let mut w = brother_wsd();
        w.register_destination("tok-lab", "ctx");
        w.set_notify_to(Some("http://127.0.0.1:9/sink"));
        w.set_subscription_expires(Some("PT1H"));
        w.set_subscription_id(Some("uuid:lab-sub"));
        w.set_subscription_deadline(Some(Instant::now() + Duration::from_secs(3600)));
        w.mint_push_scan(None).expect("mint");
        assert_eq!(w.status().destination_tokens, 1);
        assert_eq!(w.status().pending_push_scans, 1);
        assert_eq!(w.subscription_expires(), Some("PT1H"));
        assert_eq!(w.status().subscription_expires.as_deref(), Some("PT1H"));
        assert_eq!(w.subscription_id(), Some("uuid:lab-sub"));
        assert!(w.subscription_deadline().is_some());
        w.clear_subscriptions();
        assert_eq!(w.status().destination_tokens, 0);
        assert_eq!(w.status().pending_push_scans, 0);
        assert!(w.notify_to().is_none());
        assert!(w.subscription_expires().is_none());
        assert!(w.status().subscription_expires.is_none());
        assert!(w.subscription_id().is_none());
        assert!(w.subscription_deadline().is_none());
    }

    #[test]
    fn subscription_expires_set_get_clear() {
        let mut w = brother_wsd();
        assert!(w.subscription_expires().is_none());
        w.set_subscription_expires(Some("PT2H"));
        assert_eq!(w.subscription_expires(), Some("PT2H"));
        w.set_subscription_expires(Some("  PT30M  "));
        assert_eq!(w.subscription_expires(), Some("PT30M"));
        w.set_subscription_expires(Some(""));
        assert!(w.subscription_expires().is_none());
        w.set_subscription_expires(Some("PT8H"));
        w.clear_subscriptions();
        assert!(w.subscription_expires().is_none());
    }

    #[test]
    fn subscription_id_set_get_clear() {
        let mut w = brother_wsd();
        assert!(w.subscription_id().is_none());
        w.set_subscription_id(Some("uuid:abc"));
        assert_eq!(w.subscription_id(), Some("uuid:abc"));
        w.set_subscription_id(Some("  uuid:def  "));
        assert_eq!(w.subscription_id(), Some("uuid:def"));
        w.set_subscription_id(Some(""));
        assert!(w.subscription_id().is_none());
        w.set_subscription_id(Some("uuid:keep"));
        w.set_subscription_deadline(Some(Instant::now()));
        assert!(w.subscription_deadline_elapsed(Instant::now() + Duration::from_secs(1)));
        w.clear_subscriptions();
        assert!(w.subscription_id().is_none());
        assert!(w.subscription_deadline().is_none());
    }

    #[test]
    fn apply_faults_reports_condition_deltas() {
        let mut w = brother_wsd();
        let raised = w.apply_faults(&FaultBody {
            jam: Some(true),
            cover_open: Some(true),
            ..Default::default()
        });
        assert!(
            raised
                .iter()
                .any(|c| { c.id == 1 && c.name == "MediaJam" && c.component == "ADF" && c.raised }),
            "{raised:?}"
        );
        assert!(
            raised.iter().any(|c| {
                c.id == 2 && c.name == "CoverOpen" && c.component == "Platen" && c.raised
            }),
            "{raised:?}"
        );
        let cleared = w.apply_faults(&FaultBody {
            jam: Some(false),
            ..Default::default()
        });
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].id, 1);
        assert_eq!(cleared[0].name, "MediaJam");
        assert_eq!(cleared[0].component, "ADF");
        assert!(!cleared[0].raised);
        let offline = w.apply_faults(&FaultBody {
            offline: Some(true),
            ..Default::default()
        });
        assert_eq!(offline.len(), 1);
        assert_eq!(offline[0].id, 0);
        assert_eq!(offline[0].name, "AttentionRequired");
        assert!(offline[0].raised);
        let noop = w.apply_faults(&FaultBody {
            jam: Some(false),
            ..Default::default()
        });
        assert!(noop.is_empty(), "same value → no delta: {noop:?}");
        let extended = w.apply_faults(&FaultBody {
            calibrating: Some(true),
            interlock: Some(true),
            storage_full: Some(true),
            lamp_warning: Some(true),
            paused: Some(true),
            ..Default::default()
        });
        assert!(
            extended
                .iter()
                .any(|c| c.id == 5 && c.name == "Calibrating" && c.raised),
            "{extended:?}"
        );
        assert!(
            extended
                .iter()
                .any(|c| c.id == 6 && c.name == "InterlockOpen" && c.raised),
            "{extended:?}"
        );
        assert!(
            extended
                .iter()
                .any(|c| c.id == 7 && c.name == "InternalStorageFull" && c.raised),
            "{extended:?}"
        );
        assert!(
            extended
                .iter()
                .any(|c| c.id == 8 && c.name == "LampWarning" && c.raised),
            "{extended:?}"
        );
        assert!(
            extended
                .iter()
                .any(|c| c.id == 0 && c.name == "Paused" && c.raised),
            "{extended:?}"
        );
    }

    #[test]
    fn apply_faults_clear_appends_condition_history() {
        let mut w = brother_wsd();
        assert!(w.condition_history().is_empty());
        w.apply_faults(&FaultBody {
            jam: Some(true),
            ..Default::default()
        });
        assert!(
            w.condition_history().is_empty(),
            "raise alone does not append history"
        );
        w.apply_faults(&FaultBody {
            jam: Some(false),
            ..Default::default()
        });
        let hist = w.condition_history();
        assert_eq!(hist.len(), 1, "{hist:?}");
        assert_eq!(hist[0].id, 1);
        assert_eq!(hist[0].name, "MediaJam");
        assert_eq!(hist[0].component, "ADF");
        assert_eq!(hist[0].severity, "Critical");
        assert!(crate::timeutil::is_utc_iso(&hist[0].time_iso), "{hist:?}");
        assert!(
            crate::timeutil::is_utc_iso(&hist[0].clear_time_iso),
            "{hist:?}"
        );
        assert!(
            hist[0].time_iso <= hist[0].clear_time_iso,
            "Time ≤ ClearTime: {hist:?}"
        );
        w.reset();
        assert!(
            w.condition_history().is_empty(),
            "reset clears history: {:?}",
            w.condition_history()
        );
        w.apply_faults(&FaultBody {
            calibrating: Some(true),
            lamp_warning: Some(true),
            ..Default::default()
        });
        w.apply_faults(&FaultBody {
            calibrating: Some(false),
            lamp_warning: Some(false),
            ..Default::default()
        });
        let hist = w.condition_history();
        assert_eq!(hist.len(), 2, "{hist:?}");
        let cal = hist.iter().find(|e| e.id == 5).expect("Calibrating hist");
        assert_eq!(cal.name, "Calibrating");
        assert_eq!(cal.severity, "Informational");
        let warn = hist.iter().find(|e| e.id == 8).expect("LampWarning hist");
        assert_eq!(warn.name, "LampWarning");
        assert_eq!(warn.severity, "Warning");
    }

    fn profile_world(id: &str) -> World {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let profile = Profile::load(&dir, id).expect("profile");
        World::new(dir, profile)
    }

    fn adf_ticket(w: u32, h: u32, must_honor: bool) -> String {
        format!(
            r#"
          <wscn:Format>jfif</wscn:Format>
          <wscn:InputSource>ADF</wscn:InputSource>
          <wscn:InputSize><wscn:InputMediaSize MustHonor="{must_honor}"><wscn:Width>{w}</wscn:Width><wscn:Height>{h}</wscn:Height></wscn:InputMediaSize></wscn:InputSize>
          <wscn:Resolution><wscn:Width>200</wscn:Width><wscn:Height>200</wscn:Height></wscn:Resolution>"#
        )
    }

    #[test]
    fn tray_capacity_refuses_capacity_plus_one_and_accepts_capacity() {
        let mut w = profile_world("feeder-tray5");
        assert_eq!(w.status().feeder_capacity, Some(5));
        w.try_set_pages(5).expect("5 fits");
        assert_eq!(w.status().pages_remaining, 5);
        let err = w.try_set_pages(6).expect_err("6 does not fit");
        assert_eq!(
            err,
            PagesError::OverCapacity {
                requested: 6,
                capacity: 5
            }
        );
        assert!(err.to_string().contains("at most 5"), "{err}");
        assert_eq!(w.status().pages_remaining, 5, "status unchanged on refusal");
        let body = PagesBody {
            count: 6,
            side: None,
            order: None,
        };
        let e = w.apply_pages(&body).expect_err("apply refused");
        assert!(e.downcast_ref::<PagesError>().is_some(), "{e}");
        assert_eq!(w.status().pages_remaining, 5);
        w.apply_pages(&PagesBody {
            count: 5,
            side: None,
            order: None,
        })
        .expect("ok");
    }

    #[test]
    fn no_capacity_means_no_limit_for_brother_wsd() {
        let mut w = brother_wsd();
        assert_eq!(w.status().feeder_capacity, None);
        w.try_set_pages(30).expect("30");
        assert_eq!(w.status().pages_remaining, 30);
        w.try_set_pages(500)
            .expect("no capacity: not refused (clamped to pages_max)");
        assert_eq!(w.status().pages_remaining, 30);
    }

    #[test]
    fn tray50_and_duplex30_capacity_in_status() {
        assert_eq!(
            profile_world("feeder-tray50").status().feeder_capacity,
            Some(50)
        );
        let d = profile_world("feeder-duplex30");
        assert_eq!(d.status().feeder_capacity, Some(30));
        assert!(d.status().duplex);
        assert_eq!(
            profile_world("feeder-a4only").status().feeder_capacity,
            Some(20)
        );
    }

    #[test]
    fn a4_ticket_refused_on_tray5_accepted_on_tray50() {
        let a4 = adf_ticket(8267, 11692, true);
        let tray5 = profile_world("feeder-tray5");
        let v = tray5.validate_ticket(&a4);
        assert!(!v.valid, "{v:?}");
        assert!(v.reason.unwrap().contains("InputMediaSize"));
        assert!(tray5.validate_ticket(&adf_ticket(8500, 11700, true)).valid);
        assert!(profile_world("feeder-tray50").validate_ticket(&a4).valid);
        assert!(profile_world("feeder-a4only").validate_ticket(&a4).valid);
        assert!(
            !profile_world("feeder-a4only")
                .validate_ticket(&adf_ticket(8500, 11700, true))
                .valid
        );
        // MustHonor false: soft-accepted like other unsupported values.
        assert!(tray5.validate_ticket(&adf_ticket(8267, 11692, false)).valid);
        // Platen is unaffected.
        let platen = a4.replace("ADF", "Platen");
        assert!(tray5.validate_ticket(&platen).valid);
        // Unrestricted profile accepts anything.
        assert!(brother_wsd().validate_ticket(&a4).valid);
    }

    #[test]
    fn a4_create_job_refused_on_tray5() {
        let mut w = profile_world("feeder-tray5");
        w.try_set_pages(2).unwrap();
        let err = w
            .create_job(&adf_ticket(8267, 11692, true))
            .expect_err("refused");
        assert!(err.to_string().contains("InputMediaSize"), "{err}");
        assert!(w.create_job(&adf_ticket(8500, 11700, true)).is_ok());
    }
}
