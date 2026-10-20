//! GetScannerElements response builders from a capability profile.

use crate::image_gen::{LETTER_H_THOU, LETTER_W_THOU};
use crate::profile::Profile;
use crate::state::{encodable_formats, ConditionHistoryEntry, ScanJob};
use crate::wsd::soap::Envelope;
use crate::wsd::xml::{self, Element};

pub fn requested_names(body: &str) -> Vec<String> {
    let tree = Envelope::parse(body)
        .body_tree()
        .cloned()
        .or_else(|| xml::parse(body.trim()));
    let Some(tree) = tree else {
        return Vec::new();
    };
    let mut names = Vec::new();
    tree.walk(&mut |el, _| {
        if el.local == "ElementData" {
            if let Some(n) = el.attr("Name").or_else(|| el.attr("wscn:Name")) {
                names.push(n.to_string());
            }
        }
        if el.local == "Name" {
            let v = el.text.trim();
            if !v.is_empty() && !v.contains('=') {
                names.push(v.to_string());
            }
        }
    });
    names
}

pub struct ScannerView<'a> {
    pub profile: &'a Profile,
    pub jam: bool,
    pub cover_open: bool,
    /// Double-feed / multi-pick (`MultipleFeedError`).
    pub multi_feed: bool,
    /// Lamp failure (`LampError`).
    pub lamp_error: bool,
    /// Device busy — Status `Processing` even with no job.
    pub busy: bool,
    /// Sticky offline (AttentionRequired + Create/Retrieve refuse).
    pub offline: bool,
    /// Lab sticky: `Calibrating` reason + DeviceCondition Id=5.
    pub calibrating: bool,
    /// Lab sticky: `InterlockOpen` Id=6.
    pub interlock_open: bool,
    /// Lab sticky: `InternalStorageFull` Id=7.
    pub storage_full: bool,
    /// Lab sticky: `Paused` reason only (Processing).
    pub paused: bool,
    /// Lab sticky: Condition `LampWarning` Id=8 + reason `AttentionRequired`.
    pub lamp_warning: bool,
    /// Transient / lab: `LampWarming` reason.
    pub lamp_warming: bool,
    /// Feeder sensor (for derived `InputTrayEmpty` when ADF is configured).
    pub feeder_ready: bool,
    pub pages_remaining: u32,
    /// True while a CreateScanJob is waiting for RetrieveImage.
    pub scanning: bool,
    /// Cleared sticky faults for optional `ConditionHistory` (omit when empty).
    pub condition_history: &'a [ConditionHistoryEntry],
}

pub fn get_scanner_elements(view: ScannerView<'_>, body: &str) -> String {
    let names = requested_names(body);
    let want_all = names.is_empty();
    let mut parts: Vec<Element> = Vec::new();

    // Echo the client's QName (e.g. sca:ScannerDescription) on ElementData/@Name —
    // WIA asks with the 2006/08 prefix and rejects a mismatched Name.
    let client_name = |suffix: &str| -> String {
        names
            .iter()
            .find(|n| n.ends_with(suffix) || *n == suffix || n.ends_with(&format!(":{suffix}")))
            .cloned()
            .unwrap_or_else(|| format!("wscn:{suffix}"))
    };
    let want = |suffix: &str| {
        want_all
            || names
                .iter()
                .any(|n| n.ends_with(suffix) || n == suffix || n.ends_with(&format!(":{suffix}")))
    };

    if want("ScannerDescription") {
        parts.push(element_data_from_element(
            &client_name("ScannerDescription"),
            scanner_description(view.profile),
        ));
    }
    if want("ScannerStatus") {
        parts.push(element_data_from_element(
            &client_name("ScannerStatus"),
            scanner_status(&view),
        ));
    }
    if want("ScannerConfiguration") {
        parts.push(element_data_from_element(
            &client_name("ScannerConfiguration"),
            scanner_configuration(view.profile),
        ));
    }
    if want("DefaultScanTicket") {
        parts.push(element_data_from_element(
            &client_name("DefaultScanTicket"),
            default_scan_ticket(view.profile),
        ));
    }

    if parts.is_empty() {
        // Unknown names → Valid=false stubs + full config so clients still progress.
        for n in &names {
            parts.push(element_data_invalid(n));
        }
        parts.push(element_data_from_element(
            "wscn:ScannerConfiguration",
            scanner_configuration(view.profile),
        ));
    }

    let mut scanner_elements = Element::new("wscn:ScannerElements");
    for part in parts {
        scanner_elements = scanner_elements.with(part);
    }
    Element::new("wscn:GetScannerElementsResponse")
        .with(scanner_elements)
        .to_xml()
}

pub fn get_job_elements(job: &ScanJob, body: &str) -> String {
    let names = requested_names(body);
    let want_all = names.is_empty();
    let want = |suffix: &str| {
        want_all
            || names
                .iter()
                .any(|n| n.ends_with(suffix) || n == suffix || n.ends_with(&format!(":{suffix}")))
    };

    let mut parts: Vec<Element> = Vec::new();
    if want("JobStatus") || want_all {
        parts.push(element_data_from_element(
            "wscn:JobStatus",
            job_status_element(job),
        ));
    }
    if want("JobToken") || want_all {
        // JobElementsEntry is JobStatus, ScanTicket, or Document. JobToken is not one of them.
        parts.push(
            Element::new("wscn:ElementData")
                .with_attr("Name", "wscn:JobToken")
                .with_attr("Valid", "false"),
        );
    }
    // Documents / DocumentFinalParameters: Format, dpi, pages_left from ScanJob.
    // Documents is the WKV name; DocumentFinalParameters is accepted as a lab alias.
    if want("Documents") || want("DocumentFinalParameters") || want_all {
        let name = if want("Documents") && !want_all {
            "wscn:Documents"
        } else if want("DocumentFinalParameters") && !want("Documents") && !want_all {
            "wscn:DocumentFinalParameters"
        } else {
            "wscn:Documents"
        };
        let child = if name.ends_with("DocumentFinalParameters") {
            document_final_parameters(job)
        } else {
            Element::new("wscn:Documents").with(document_final_parameters(job))
        };
        parts.push(element_data_from_element(name, child));
    }

    if parts.is_empty() {
        for n in &names {
            parts.push(element_data_invalid(n));
        }
        // Always include status so clients can progress.
        parts.push(element_data_from_element(
            "wscn:JobStatus",
            job_status_element(job),
        ));
    }

    let mut job_elements = Element::new("wscn:JobElements");
    for part in parts {
        job_elements = job_elements.with(part);
    }
    Element::new("wscn:GetJobElementsResponse")
        .with(job_elements)
        .to_xml()
}

fn job_status_element(job: &ScanJob) -> Element {
    // JobStatusType: JobId, JobState, JobStateReasons, ScansCompleted. `xs:any` is another namespace.
    Element::new("wscn:JobStatus")
        .with(Element::text_el("wscn:JobId", job.id.to_string()))
        .with(Element::text_el("wscn:JobState", job.job_state()))
        .with(Element::new("wscn:JobStateReasons").with(Element::text_el(
            "wscn:JobStateReason",
            job.state_reason.clone(),
        )))
        .with(Element::text_el(
            "wscn:ScansCompleted",
            job.page_index.to_string(),
        ))
}

fn document_final_parameters(job: &ScanJob) -> Element {
    let media_front = Element::new("wscn:MediaFront").with(
        Element::new("wscn:Resolution")
            .with(Element::text_el("wscn:Width", job.dpi.to_string()))
            .with(Element::text_el("wscn:Height", job.dpi.to_string())),
    );
    Element::new("wscn:DocumentFinalParameters")
        .with(Element::text_el("wscn:Format", job.format.clone()))
        .with(Element::text_el(
            "wscn:ImagesToTransfer",
            job.pages_left.to_string(),
        ))
        .with(Element::text_el(
            "wscn:InputSource",
            job.input_source.clone(),
        ))
        .with(Element::new("wscn:MediaSides").with(media_front))
}

fn scanner_status(view: &ScannerView<'_>) -> Element {
    let jam = view.jam;
    let cover_open = view.cover_open;
    let multi_feed = view.multi_feed;
    let lamp_error = view.lamp_error;
    let busy = view.busy;
    let offline = view.offline;
    let calibrating = view.calibrating;
    let interlock_open = view.interlock_open;
    let storage_full = view.storage_full;
    let paused = view.paused;
    let lamp_warning = view.lamp_warning;
    let lamp_warming = view.lamp_warming;
    let scanning = view.scanning;
    let condition_history = view.condition_history;

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
    if calibrating {
        reasons.push("Calibrating");
    }
    if interlock_open {
        reasons.push("InterlockOpen");
    }
    if storage_full {
        reasons.push("InternalStorageFull");
    }
    if lamp_warming {
        reasons.push("LampWarming");
    }
    if paused {
        reasons.push("Paused");
    }
    if offline || lamp_warning {
        reasons.push("AttentionRequired");
    }
    // Empty ADF is Idle/None — AttentionRequired means human intervention and
    // must not be used for feeder_ready=false (glass acquire stays online).
    // Busy alone keeps reason None with state Processing.
    if reasons.is_empty() {
        reasons.push("None");
    }
    // After CreateScanJob, WIA polls Status — Idle here aborts before RetrieveImage.
    let state = if jam
        || cover_open
        || multi_feed
        || lamp_error
        || offline
        || interlock_open
        || storage_full
        || lamp_warning
    {
        "Stopped"
    } else if scanning || busy || calibrating || paused || lamp_warming {
        "Processing"
    } else {
        "Idle"
    };
    let mut reason_el = Element::new("wscn:ScannerStateReasons");
    for r in &reasons {
        reason_el = reason_el.with(Element::text_el("wscn:ScannerStateReason", *r));
    }
    // ConditionName enum has no AttentionRequired / Paused / LampWarming — those stay reasons only.
    // Stable Ids: 1 MediaJam, 2 CoverOpen, 3 MultipleFeedError, 4 LampError, 5 Calibrating,
    // 6 InterlockOpen, 7 InternalStorageFull, 8 LampWarning, 9 InputTrayEmpty (derived).
    let time = now_iso();
    let mut condition_els: Vec<Element> = Vec::new();
    if jam {
        condition_els.push(device_condition(1, &time, "MediaJam", "ADF", "Critical"));
    }
    if cover_open {
        condition_els.push(device_condition(
            2,
            &time,
            "CoverOpen",
            "Platen",
            "Critical",
        ));
    }
    if multi_feed {
        condition_els.push(device_condition(
            3,
            &time,
            "MultipleFeedError",
            "ADF",
            "Critical",
        ));
    }
    if lamp_error {
        condition_els.push(device_condition(
            4,
            &time,
            "LampError",
            "Platen",
            "Critical",
        ));
    }
    if calibrating {
        condition_els.push(device_condition(
            5,
            &time,
            "Calibrating",
            "Platen",
            "Informational",
        ));
    }
    if interlock_open {
        condition_els.push(device_condition(
            6,
            &time,
            "InterlockOpen",
            "MediaPath",
            "Critical",
        ));
    }
    if storage_full {
        condition_els.push(device_condition(
            7,
            &time,
            "InternalStorageFull",
            "Platen",
            "Critical",
        ));
    }
    if lamp_warning {
        condition_els.push(device_condition(
            8,
            &time,
            "LampWarning",
            "Platen",
            "Warning",
        ));
    }
    let has_feeder = view.profile.sources.iter().any(|s| s == "feeder");
    let input_tray_empty = has_feeder && !view.feeder_ready && view.pages_remaining == 0;
    if input_tray_empty {
        // Derived only — do not emit AttentionRequired for empty ADF (Brother WIA).
        condition_els.push(device_condition(
            9,
            &time,
            "InputTrayEmpty",
            "ADF",
            "Warning",
        ));
    }
    let mut active = Element::new("wscn:ActiveConditions");
    for c in condition_els {
        active = active.with(c);
    }
    let mut status = Element::new("wscn:ScannerStatus")
        .with(Element::text_el("wscn:ScannerCurrentTime", time))
        .with(Element::text_el("wscn:ScannerState", state))
        .with(reason_el)
        .with(active);
    // ConditionHistory is minOccurs=0 — omit the element when the ring is empty.
    if !condition_history.is_empty() {
        let mut history = Element::new("wscn:ConditionHistory");
        for e in condition_history {
            history = history.with(condition_history_entry(e));
        }
        status = status.with(history);
    }
    status
}

fn device_condition(id: u32, time: &str, name: &str, component: &str, severity: &str) -> Element {
    Element::new("wscn:DeviceCondition")
        .with_attr("Id", id.to_string())
        .with(Element::text_el("wscn:Time", time))
        .with(Element::text_el("wscn:Name", name))
        .with(Element::text_el("wscn:Component", component))
        .with(Element::text_el("wscn:Severity", severity))
}

fn condition_history_entry(e: &ConditionHistoryEntry) -> Element {
    Element::new("wscn:ConditionHistoryEntry")
        .with_attr("Id", e.id.to_string())
        .with(Element::text_el("wscn:Time", e.time_iso.clone()))
        .with(Element::text_el("wscn:Name", e.name))
        .with(Element::text_el("wscn:Component", e.component))
        .with(Element::text_el("wscn:Severity", e.severity))
        .with(Element::text_el("wscn:ClearTime", e.clear_time_iso.clone()))
}

/// Live wsdscdrv: unprefixed `Name`/`Valid` + client QName (sca:…).
fn element_data_from_element(name: &str, child: Element) -> Element {
    Element::new("wscn:ElementData")
        .with_attr("Name", name)
        .with_attr("Valid", "true")
        .with(child)
}

fn element_data_invalid(name: &str) -> Element {
    Element::new("wscn:ElementData")
        .with_attr("wscn:Name", name)
        .with_attr("wscn:Valid", "false")
}

/// WSD `ColorEntry` list from profile `colors` (canonical wsd_token order).
/// Empty allow-list → Brother-shaped BW → Gray → RGB.
fn color_entries(profile: &Profile) -> Vec<Element> {
    use crate::effects::ColorMode;
    let tokens: Vec<String> = if profile.colors.is_empty() {
        vec![
            ColorMode::Bw1.wsd_token().to_string(),
            ColorMode::Gray8.wsd_token().to_string(),
            ColorMode::Rgb24.wsd_token().to_string(),
        ]
    } else {
        // Preserve profile order; canonicalize via ColorMode so Gray8 → Grayscale8.
        let mut out = Vec::new();
        for raw in &profile.colors {
            let tok = ColorMode::parse(raw).wsd_token().to_string();
            if !out.iter().any(|t| t == &tok) {
                out.push(tok);
            }
        }
        out
    };
    tokens
        .into_iter()
        .map(|t| Element::text_el("wscn:ColorEntry", t))
        .collect()
}

fn color_container(name: &str, entries: Vec<Element>) -> Element {
    let mut el = Element::new(name);
    for entry in entries {
        el = el.with(entry);
    }
    el
}

fn default_color_processing(profile: &Profile) -> &'static str {
    use crate::effects::ColorMode;
    if let Some(c) = profile.colors.first() {
        ColorMode::parse(c).wsd_token()
    } else {
        ColorMode::Rgb24.wsd_token()
    }
}

fn dpi_widths_heights(profile: &Profile) -> (Element, Element) {
    let mut widths = Element::new("wscn:Widths");
    let mut heights = Element::new("wscn:Heights");
    for d in &profile.dpi {
        widths = widths.with(Element::text_el("wscn:Width", d.to_string()));
        heights = heights.with(Element::text_el("wscn:Height", d.to_string()));
    }
    (widths, heights)
}

fn scanner_configuration(profile: &Profile) -> Element {
    // Shape matches a live GetScannerElements capture of a real device
    // (`D:\temp\brother-gse-config.xml`): 0/1 booleans, Auto-only content type,
    // CompressionQuality 100–100, Scaling 100–100, color order BW→Gray→RGB,
    // min size 556, platen max height LETTER_H_THOU (11700).
    let mut formats_supported = Element::new("wscn:FormatsSupported");
    for v in encodable_formats(profile) {
        formats_supported = formats_supported.with(Element::text_el("wscn:FormatValue", v));
    }

    let (widths, heights) = dpi_widths_heights(profile);
    let platen_resolutions = Element::new("wscn:PlatenResolutions")
        .with(widths.clone())
        .with(heights.clone());
    let adf_resolutions = Element::new("wscn:ADFResolutions")
        .with(widths.clone())
        .with(heights.clone());

    let (ox, oy) = match &profile.optical {
        Some(o) => (o.x_dpi, o.y_dpi),
        None => (
            *profile.dpi.last().unwrap_or(&300),
            *profile.dpi.last().unwrap_or(&300),
        ),
    };

    let has_feeder = profile.sources.iter().any(|s| s == "feeder");
    let has_glass = profile.sources.iter().any(|s| s == "glass");
    let color_list = color_entries(profile);

    let device_settings = Element::new("wscn:DeviceSettings")
        .with(formats_supported)
        .with(
            Element::new("wscn:CompressionQualityFactorSupported")
                .with(Element::text_el("wscn:MinValue", "100"))
                .with(Element::text_el("wscn:MaxValue", "100")),
        )
        .with(
            Element::new("wscn:ContentTypesSupported")
                .with(Element::text_el("wscn:ContentTypeValue", "Auto")),
        )
        .with(Element::text_el(
            "wscn:DocumentSizeAutoDetectSupported",
            "0",
        ))
        .with(Element::text_el("wscn:AutoExposureSupported", "0"))
        .with(Element::text_el("wscn:BrightnessSupported", "1"))
        .with(Element::text_el("wscn:ContrastSupported", "1"))
        .with(
            Element::new("wscn:ScalingRangeSupported")
                .with(
                    Element::new("wscn:ScalingWidth")
                        .with(Element::text_el("wscn:MinValue", "100"))
                        .with(Element::text_el("wscn:MaxValue", "100")),
                )
                .with(
                    Element::new("wscn:ScalingHeight")
                        .with(Element::text_el("wscn:MinValue", "100"))
                        .with(Element::text_el("wscn:MaxValue", "100")),
                ),
        )
        .with(
            Element::new("wscn:RotationsSupported")
                .with(Element::text_el("wscn:RotationValue", "0")),
        );

    let mut config = Element::new("wscn:ScannerConfiguration").with(device_settings);

    if has_glass {
        let platen = Element::new("wscn:Platen")
            .with(
                Element::new("wscn:PlatenOpticalResolution")
                    .with(Element::text_el("wscn:Width", ox.to_string()))
                    .with(Element::text_el("wscn:Height", oy.to_string())),
            )
            .with(platen_resolutions)
            .with(color_container("wscn:PlatenColor", color_list.clone()))
            .with(
                Element::new("wscn:PlatenMinimumSize")
                    .with(Element::text_el("wscn:Width", "556"))
                    .with(Element::text_el("wscn:Height", "556")),
            )
            .with(
                Element::new("wscn:PlatenMaximumSize")
                    .with(Element::text_el("wscn:Width", LETTER_W_THOU.to_string()))
                    .with(Element::text_el("wscn:Height", LETTER_H_THOU.to_string())),
            );
        config = config.with(platen);
    }

    if has_feeder {
        let duplex = if profile.feeder.duplex { "1" } else { "0" };
        // ADFBack mirrors ADFFront when duplex is offered (WSD: back optional but
        // clients treat Front+Back as duplex even if SupportsDuplex is wrong).
        // WS-Scan has no media-size list element; the ADF envelope is the only standard way
        // to publish the tray's sizes. Unrestricted profiles keep the legacy 8500x14000.
        let (adf_max_w, adf_max_h) = match profile.feeder.media_size_list() {
            Some(list) if !list.is_empty() => list.iter().fold((0, 0), |(w, h), m| {
                let (mw, mh) = m.dims_thou();
                (w.max(mw), h.max(mh))
            }),
            _ => (LETTER_W_THOU, 14000),
        };
        let adf_side = |tag: &str| {
            Element::new(tag)
                .with(
                    Element::new("wscn:ADFOpticalResolution")
                        .with(Element::text_el("wscn:Width", ox.to_string()))
                        .with(Element::text_el("wscn:Height", oy.to_string())),
                )
                .with(adf_resolutions.clone())
                .with(color_container("wscn:ADFColor", color_list.clone()))
                .with(
                    Element::new("wscn:ADFMinimumSize")
                        .with(Element::text_el("wscn:Width", "556"))
                        .with(Element::text_el("wscn:Height", "556")),
                )
                .with(
                    Element::new("wscn:ADFMaximumSize")
                        .with(Element::text_el("wscn:Width", adf_max_w.to_string()))
                        .with(Element::text_el("wscn:Height", adf_max_h.to_string())),
                )
        };
        let mut adf = Element::new("wscn:ADF")
            .with(Element::text_el("wscn:ADFSupportsDuplex", duplex))
            .with(adf_side("wscn:ADFFront"));
        if profile.feeder.duplex {
            adf = adf.with(adf_side("wscn:ADFBack"));
        }
        config = config.with(adf);
    }

    config
}

/// The capability XML Windows caches at Add time: `ScannerConfiguration` + `DefaultScanTicket`
/// (profile-only; no runtime status, no tray capacity). Hashed by the registry to decide when a
/// reconfigured device needs a re-Add.
pub(crate) fn published_capability_xml(profile: &Profile) -> String {
    format!(
        "{}
{}",
        scanner_configuration(profile).to_xml(),
        default_scan_ticket(profile).to_xml()
    )
}

fn now_iso() -> String {
    crate::timeutil::utc_now_iso()
}

fn lang_text_el(name: &str, s: impl Into<String>) -> Element {
    Element::new(name)
        .with_attr("xml:lang", "en-US")
        .with_text(s)
}

fn scanner_info_text(profile: &Profile) -> String {
    let mut s = String::from("fakescan profile ");
    s.push_str(&profile.id);
    s
}

/// Same Description block as GetScannerElements (`ScannerName` / Info / Location).
fn scanner_description(profile: &Profile) -> Element {
    Element::new("wscn:ScannerDescription")
        .with(lang_text_el("wscn:ScannerName", profile.label.clone()))
        .with(lang_text_el("wscn:ScannerInfo", scanner_info_text(profile)))
        .with(lang_text_el("wscn:ScannerLocation", "lab"))
}

/// ElementChanges children for ScannerElementsChangeEvent.
///
/// Order: [`scanner_description`], full [`scanner_configuration`], then
/// [`default_scan_ticket`] — same shapes as GetScannerElements — so a NotifyTo
/// subscriber sees the advertised identity and capability set without re-polling.
#[allow(dead_code)] // for the NotifyTo ScannerElementsChangeEvent body; not sent yet
pub(crate) fn element_changes_xml(profile: &Profile) -> String {
    element_changes_elements(profile)
        .into_iter()
        .map(|el| el.to_xml())
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn element_changes_elements(profile: &Profile) -> Vec<Element> {
    vec![
        scanner_description(profile),
        scanner_configuration(profile),
        default_scan_ticket(profile),
    ]
}

fn default_scan_ticket(profile: &Profile) -> Element {
    // Brother default ticket (live): jfif @ 300, InputMediaSize 8500×11700,
    // Exposure/Scaling/Rotation/ScanRegion filled — not DocumentSizeAutoDetect.
    let dpi = profile
        .dpi
        .iter()
        .find(|&&d| d == 300)
        .copied()
        .or_else(|| profile.dpi.iter().find(|&&d| d == 200).copied())
        .unwrap_or(profile.dpi.first().copied().unwrap_or(200));
    let formats = encodable_formats(profile);
    let format = formats
        .iter()
        .find(|f| *f == "jfif")
        .cloned()
        .or_else(|| formats.into_iter().next())
        .unwrap_or_else(|| "jfif".into());
    let color = default_color_processing(profile);
    let media_front = Element::new("wscn:MediaFront")
        .with(
            Element::new("wscn:ScanRegion")
                .with(Element::text_el("wscn:ScanRegionXOffset", "0"))
                .with(Element::text_el("wscn:ScanRegionYOffset", "0"))
                .with(Element::text_el(
                    "wscn:ScanRegionWidth",
                    LETTER_W_THOU.to_string(),
                ))
                .with(Element::text_el(
                    "wscn:ScanRegionHeight",
                    LETTER_H_THOU.to_string(),
                )),
        )
        .with(Element::text_el("wscn:ColorProcessing", color))
        .with(
            Element::new("wscn:Resolution")
                .with(Element::text_el("wscn:Width", dpi.to_string()))
                .with(Element::text_el("wscn:Height", dpi.to_string())),
        );
    Element::new("wscn:DefaultScanTicket")
        .with(
            Element::new("wscn:JobDescription")
                .with(Element::text_el("wscn:JobName", "fakescan"))
                .with(Element::text_el("wscn:JobOriginatingUserName", "fakescan"))
                .with(Element::text_el("wscn:JobInformation", "fakescan")),
        )
        .with(
            Element::new("wscn:DocumentParameters")
                .with(Element::text_el("wscn:Format", format))
                .with(Element::text_el("wscn:CompressionQualityFactor", "100"))
                .with(Element::text_el("wscn:ImagesToTransfer", "1"))
                .with(Element::text_el("wscn:InputSource", "Platen"))
                .with(Element::text_el("wscn:ContentType", "Auto"))
                .with(
                    Element::new("wscn:InputSize").with(
                        Element::new("wscn:InputMediaSize")
                            .with(Element::text_el("wscn:Width", LETTER_W_THOU.to_string()))
                            .with(Element::text_el("wscn:Height", LETTER_H_THOU.to_string())),
                    ),
                )
                .with(
                    Element::new("wscn:Exposure").with(
                        Element::new("wscn:ExposureSettings")
                            .with(Element::text_el("wscn:Contrast", "0"))
                            .with(Element::text_el("wscn:Brightness", "0"))
                            .with(Element::text_el("wscn:Sharpness", "0")),
                    ),
                )
                .with(
                    Element::new("wscn:Scaling")
                        .with(Element::text_el("wscn:ScalingWidth", "100"))
                        .with(Element::text_el("wscn:ScalingHeight", "100")),
                )
                .with(Element::text_el("wscn:Rotation", "0"))
                .with(Element::new("wscn:MediaSides").with(media_front)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::Profile;
    use crate::wsd::soap::{SCAN_NS_AIRSCAN, SOAP_NS};
    use std::path::PathBuf;

    fn soap_wrap_for_schema(body_xml: &str) -> String {
        let body = xml::parse(body_xml).expect("parse scan body");
        Element::new("soap:Envelope")
            .with_attr("xmlns:soap", SOAP_NS)
            .with_attr("xmlns:wscn", SCAN_NS_AIRSCAN)
            .with(Element::new("soap:Body").with(body))
            .to_xml()
    }

    fn profile() -> Profile {
        load_profile("brother-wsd")
    }

    /// brother-wsd with the hardware ADF duplex bit on (the shipped lab profile is single-sided).
    fn duplex_profile() -> Profile {
        let mut profile = profile();
        profile.feeder.duplex = true;
        profile
    }

    fn load_profile(id: &str) -> Profile {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        Profile::load(&dir, id).expect("profile")
    }

    #[test]
    fn scanner_capabilities_follow_the_profile_and_validate() {
        let brother = load_profile("brother-wsd");
        let dib = load_profile("accept-dib");
        let ask = r#"<wscn:Name>wscn:ScannerConfiguration</wscn:Name>"#;
        let brother_xml = get_scanner_elements(idle_view(&brother), ask);
        let dib_xml = get_scanner_elements(idle_view(&dib), ask);
        assert!(
            brother_xml.contains("<wscn:FormatValue>jfif</wscn:FormatValue>"),
            "{brother_xml}"
        );
        assert!(
            brother_xml.contains("tiff-single-uncompressed"),
            "{brother_xml}"
        );
        assert!(
            !brother_xml.contains("<wscn:FormatValue>dib</wscn:FormatValue>"),
            "{brother_xml}"
        );
        assert!(
            dib_xml.contains("<wscn:FormatValue>dib</wscn:FormatValue>"),
            "{dib_xml}"
        );
        assert!(
            !dib_xml.contains("<wscn:FormatValue>jfif</wscn:FormatValue>"),
            "{dib_xml}"
        );
        for xml in [&brother_xml, &dib_xml] {
            let wrapped = soap_wrap_for_schema(xml);
            let problems = crate::wsd::scan_schema::body_problems(&wrapped);
            assert!(problems.is_empty(), "{problems:?}\n{xml}");
        }
    }

    fn idle_view<'a>(profile: &'a Profile) -> ScannerView<'a> {
        ScannerView {
            profile,
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
            feeder_ready: true,
            pages_remaining: 0,
            scanning: false,
            condition_history: &[],
        }
    }

    const STATUS_REQ: &str = r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#;

    fn status_xml(jam: bool, cover_open: bool) -> String {
        let p = profile();
        let mut v = idle_view(&p);
        v.jam = jam;
        v.cover_open = cover_open;
        get_scanner_elements(v, STATUS_REQ)
    }

    #[test]
    fn scanner_status_processing_while_scanning() {
        let xml = get_scanner_elements(
            ScannerView {
                profile: &profile(),
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: true,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            xml.contains("<wscn:ScannerState>Processing</wscn:ScannerState>"),
            "{xml}"
        );
    }

    #[test]
    fn scanner_status_idle_reason_none_not_attention() {
        let xml = status_xml(false, false);
        assert!(
            xml.contains("<wscn:ScannerState>Idle</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("AttentionRequired"), "{xml}");
    }

    #[test]
    fn scanner_status_jam_is_media_jam() {
        let xml = status_xml(true, false);
        assert!(
            xml.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>MediaJam</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("CoverOpen"), "{xml}");
    }

    #[test]
    fn scanner_status_jam_emits_active_condition() {
        let xml = status_xml(true, false);
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="1">"#), "{xml}");
        assert!(xml.contains("<wscn:Name>MediaJam</wscn:Name>"), "{xml}");
        assert!(
            xml.contains("<wscn:Component>ADF</wscn:Component>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:Severity>Critical</wscn:Severity>"),
            "{xml}"
        );
        let time = xml
            .split_once("<wscn:Time>")
            .and_then(|(_, rest)| rest.split_once("</wscn:Time>"))
            .map(|(t, _)| t)
            .expect("Time element");
        assert!(
            crate::timeutil::is_utc_iso(time),
            "Time should be YYYY-MM-DDTHH:MM:SSZ, got {time:?} in {xml}"
        );
        assert!(!xml.contains(r#"Id="2""#), "{xml}");
        let idle = status_xml(false, false);
        assert!(
            idle.contains("<wscn:ActiveConditions/>"),
            "empty faults stay self-closing: {idle}"
        );
        assert!(!idle.contains("DeviceCondition"), "{idle}");
        assert!(
            !idle.contains("ConditionHistory"),
            "empty history omits ConditionHistory: {idle}"
        );
    }

    #[test]
    fn scanner_status_emits_condition_history_when_present() {
        let hist = [ConditionHistoryEntry {
            id: 1,
            name: "MediaJam",
            component: "ADF",
            severity: "Critical",
            time_iso: "2026-03-15T12:30:00Z".into(),
            clear_time_iso: "2026-03-15T12:31:05Z".into(),
        }];
        let xml = get_scanner_elements(
            ScannerView {
                profile: &profile(),
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &hist,
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(xml.contains("<wscn:ConditionHistory>"), "{xml}");
        assert!(
            xml.contains(r#"<wscn:ConditionHistoryEntry Id="1">"#),
            "{xml}"
        );
        assert!(xml.contains("<wscn:Name>MediaJam</wscn:Name>"), "{xml}");
        let clear = xml
            .split_once("<wscn:ClearTime>")
            .and_then(|(_, rest)| rest.split_once("</wscn:ClearTime>"))
            .map(|(t, _)| t)
            .expect("ClearTime element");
        assert!(
            crate::timeutil::is_utc_iso(clear),
            "ClearTime should be YYYY-MM-DDTHH:MM:SSZ, got {clear:?} in {xml}"
        );
        assert_eq!(clear, hist[0].clear_time_iso);
    }

    #[test]
    fn scanner_status_cover_open() {
        let xml = status_xml(false, true);
        assert!(
            xml.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>CoverOpen</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("MediaJam"), "{xml}");
    }

    #[test]
    fn scanner_status_multi_feed_is_multiple_feed_error() {
        let xml = get_scanner_elements(
            ScannerView {
                profile: &profile(),
                jam: false,
                cover_open: false,
                multi_feed: true,
                lamp_error: false,
                busy: false,
                offline: false,
                calibrating: false,
                interlock_open: false,
                storage_full: false,
                paused: false,
                lamp_warning: false,
                lamp_warming: false,
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            xml.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>MultipleFeedError</wscn:ScannerStateReason>"),
            "{xml}"
        );
    }

    #[test]
    fn scanner_status_lamp_error_and_busy_processing() {
        let lamp = get_scanner_elements(
            ScannerView {
                profile: &profile(),
                jam: false,
                cover_open: false,
                multi_feed: false,
                lamp_error: true,
                busy: false,
                offline: false,
                calibrating: false,
                interlock_open: false,
                storage_full: false,
                paused: false,
                lamp_warning: false,
                lamp_warming: false,
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            lamp.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{lamp}"
        );
        assert!(
            lamp.contains("<wscn:ScannerStateReason>LampError</wscn:ScannerStateReason>"),
            "{lamp}"
        );
        let busy = get_scanner_elements(
            ScannerView {
                profile: &profile(),
                jam: false,
                cover_open: false,
                multi_feed: false,
                lamp_error: false,
                busy: true,
                offline: false,
                calibrating: false,
                interlock_open: false,
                storage_full: false,
                paused: false,
                lamp_warning: false,
                lamp_warming: false,
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            busy.contains("<wscn:ScannerState>Processing</wscn:ScannerState>"),
            "{busy}"
        );
        assert!(
            busy.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
            "{busy}"
        );
    }

    #[test]
    fn scanner_status_calibrating_active_condition() {
        let p = profile();
        let mut v = idle_view(&p);
        v.calibrating = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerState>Processing</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>Calibrating</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="5">"#), "{xml}");
        assert!(xml.contains("<wscn:Name>Calibrating</wscn:Name>"), "{xml}");
        assert!(
            xml.contains("<wscn:Severity>Informational</wscn:Severity>"),
            "{xml}"
        );
    }

    #[test]
    fn scanner_status_interlock_and_storage_full() {
        let p = profile();
        let mut v = idle_view(&p);
        v.interlock_open = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerStateReason>InterlockOpen</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="6">"#), "{xml}");
        assert!(
            xml.contains("<wscn:Component>MediaPath</wscn:Component>"),
            "{xml}"
        );
        let mut v = idle_view(&p);
        v.storage_full = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerStateReason>InternalStorageFull</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="7">"#), "{xml}");
    }

    #[test]
    fn scanner_status_paused_and_lamp_warming_reasons_only() {
        let p = profile();
        let mut v = idle_view(&p);
        v.paused = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerState>Processing</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>Paused</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("DeviceCondition"), "{xml}");
        let mut v = idle_view(&p);
        v.lamp_warming = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerStateReason>LampWarming</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("DeviceCondition"), "{xml}");
    }

    #[test]
    fn scanner_status_lamp_warning_attention_and_condition() {
        let p = profile();
        let mut v = idle_view(&p);
        v.lamp_warning = true;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerState>Stopped</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>AttentionRequired</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="8">"#), "{xml}");
        assert!(xml.contains("<wscn:Name>LampWarning</wscn:Name>"), "{xml}");
        assert!(
            xml.contains("<wscn:Severity>Warning</wscn:Severity>"),
            "{xml}"
        );
        assert!(!xml.contains("LampError"), "{xml}");
    }

    #[test]
    fn scanner_status_input_tray_empty_without_attention() {
        let p = profile();
        assert!(
            p.sources.iter().any(|s| s == "feeder"),
            "brother-wsd should advertise feeder"
        );
        let mut v = idle_view(&p);
        v.feeder_ready = false;
        v.pages_remaining = 0;
        let xml = get_scanner_elements(v, STATUS_REQ);
        assert!(
            xml.contains("<wscn:ScannerState>Idle</wscn:ScannerState>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScannerStateReason>None</wscn:ScannerStateReason>"),
            "{xml}"
        );
        assert!(!xml.contains("AttentionRequired"), "{xml}");
        assert!(xml.contains(r#"<wscn:DeviceCondition Id="9">"#), "{xml}");
        assert!(
            xml.contains("<wscn:Name>InputTrayEmpty</wscn:Name>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:Component>ADF</wscn:Component>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:Severity>Warning</wscn:Severity>"),
            "{xml}"
        );
    }

    #[test]
    fn default_ticket_matches_brother_shape() {
        let xml = get_scanner_elements(
            ScannerView {
                profile: &profile(),
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:DefaultScanTicket</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(xml.contains("<wscn:InputMediaSize>"), "{xml}");
        assert!(xml.contains("<wscn:Height>11700</wscn:Height>"), "{xml}");
        assert!(
            xml.contains("<wscn:ScanRegionWidth>8500</wscn:ScanRegionWidth>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:CompressionQualityFactor>100</wscn:CompressionQualityFactor>"),
            "{xml}"
        );
        assert!(!xml.contains("DocumentSizeAutoDetect>"), "{xml}");
    }

    #[test]
    fn configuration_matches_brother_device_settings() {
        let xml = get_scanner_elements(
            ScannerView {
                profile: &duplex_profile(),
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerConfiguration</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            xml.contains(
                "<wscn:DocumentSizeAutoDetectSupported>0</wscn:DocumentSizeAutoDetectSupported>"
            ),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:BrightnessSupported>1</wscn:BrightnessSupported>"),
            "{xml}"
        );
        assert!(xml.contains("<wscn:Height>11700</wscn:Height>"), "{xml}");
        assert!(xml.contains("<wscn:MinValue>100</wscn:MinValue>"), "{xml}");
        assert!(
            xml.contains("<wscn:ColorEntry>BlackAndWhite1</wscn:ColorEntry>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ColorEntry>Grayscale8</wscn:ColorEntry>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ColorEntry>RGB24</wscn:ColorEntry>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ADFSupportsDuplex>1</wscn:ADFSupportsDuplex>"),
            "{xml}"
        );
        assert!(xml.contains("<wscn:ADFFront>"), "{xml}");
        assert!(xml.contains("<wscn:ADFBack>"), "{xml}");
    }

    #[test]
    fn configuration_color_entries_follow_profile_colors() {
        let mut p = profile();
        p.colors = vec!["RGB24".into()];
        let xml = get_scanner_elements(
            ScannerView {
                profile: &p,
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:ScannerConfiguration</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            xml.contains("<wscn:ColorEntry>RGB24</wscn:ColorEntry>"),
            "{xml}"
        );
        assert!(
            !xml.contains("<wscn:ColorEntry>BlackAndWhite1</wscn:ColorEntry>"),
            "BW omitted when not in profile colors: {xml}"
        );
        assert!(
            !xml.contains("<wscn:ColorEntry>Grayscale8</wscn:ColorEntry>"),
            "Gray omitted when not in profile colors: {xml}"
        );
        let ticket = get_scanner_elements(
            ScannerView {
                profile: &p,
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
                feeder_ready: true,
                pages_remaining: 0,
                scanning: false,
                condition_history: &[],
            },
            r#"<wscn:RequestedElements><wscn:Name>wscn:DefaultScanTicket</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            ticket.contains("<wscn:ColorProcessing>RGB24</wscn:ColorProcessing>"),
            "{ticket}"
        );
    }

    fn sample_job() -> ScanJob {
        use std::time::Instant;
        ScanJob {
            id: 7,
            token: "tok-7".into(),
            format: "jfif".into(),
            input_source: "Platen".into(),
            dpi: 200,
            color: crate::effects::ColorMode::Rgb24,
            width_px: 1700,
            height_px: 2200,
            pages_left: 2,
            page_index: 1,
            sheet_base: 0,
            stack_total: 0,
            sheets_taken: 0,
            duplex: false,
            created_at: Instant::now(),
            last_activity: Instant::now(),
            state_reason: "JobScanning".into(),
            aborted: false,
            format_used_default: false,
            format_overridden: false,
            dpi_used_default: false,
            dpi_overridden: false,
        }
    }

    #[test]
    fn job_elements_status_includes_reason_and_scans() {
        let xml = get_job_elements(
            &sample_job(),
            r#"<wscn:RequestedElements><wscn:Name>wscn:JobStatus</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(
            xml.contains("<wscn:JobStateReason>JobScanning</wscn:JobStateReason>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:ScansCompleted>1</wscn:ScansCompleted>"),
            "{xml}"
        );
        assert!(
            xml.contains("<wscn:JobState>Processing</wscn:JobState>"),
            "{xml}"
        );
    }

    #[test]
    fn job_elements_document_final_parameters() {
        let xml = get_job_elements(
            &sample_job(),
            r#"<wscn:RequestedElements><wscn:Name>wscn:DocumentFinalParameters</wscn:Name></wscn:RequestedElements>"#,
        );
        assert!(xml.contains("<wscn:Format>jfif</wscn:Format>"), "{xml}");
        assert!(
            xml.contains("<wscn:InputSource>Platen</wscn:InputSource>"),
            "{xml}"
        );
        assert!(xml.contains("<wscn:Width>200</wscn:Width>"), "{xml}");
        assert!(
            xml.contains("<wscn:ImagesToTransfer>2</wscn:ImagesToTransfer>"),
            "{xml}"
        );
        assert!(!xml.contains("JobStatus"), "{xml}");
    }

    fn adf_max(profile: &Profile) -> (String, String) {
        let ask = r#"<wscn:Name>wscn:ScannerConfiguration</wscn:Name>"#;
        let xml = get_scanner_elements(idle_view(profile), ask);
        let wrapped = soap_wrap_for_schema(&xml);
        let problems = crate::wsd::scan_schema::body_problems(&wrapped);
        assert!(problems.is_empty(), "{problems:?}\n{xml}");
        let tree = xml::parse(&xml).expect("parse");
        let max = tree.descendant("ADFMaximumSize").expect("ADFMaximumSize");
        (
            max.find_text("Width").unwrap().to_string(),
            max.find_text("Height").unwrap().to_string(),
        )
    }

    #[test]
    fn adf_max_size_follows_media_sizes_per_profile() {
        assert_eq!(
            adf_max(&load_profile("brother-wsd")),
            ("8500".into(), "14000".into())
        );
        assert_eq!(
            adf_max(&load_profile("feeder-tray5")),
            ("8500".into(), "11700".into())
        );
        assert_eq!(
            adf_max(&load_profile("feeder-tray50")),
            ("8500".into(), "14000".into())
        );
        assert_eq!(
            adf_max(&load_profile("feeder-duplex30")),
            ("8500".into(), "14000".into())
        );
        assert_eq!(
            adf_max(&load_profile("feeder-a4only")),
            ("8267".into(), "11692".into())
        );
    }

    #[test]
    fn capacity_is_not_published_in_wsd_xml() {
        let p = load_profile("feeder-tray5");
        let ask = r#"<wscn:Name>wscn:ScannerConfiguration</wscn:Name>"#;
        let xml = get_scanner_elements(idle_view(&p), ask);
        assert!(!xml.to_ascii_lowercase().contains("capacity"), "{xml}");
    }
}
