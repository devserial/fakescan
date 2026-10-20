//! Synthetic page bitmaps for RetrieveImage.

use anyhow::{bail, Result};
use image::{ImageBuffer, Rgb, RgbImage};
use qrcode::{EcLevel, QrCode};
use serde::{Deserialize, Serialize};

/// Letter width in 1/1000 inch (WSD InputMediaSize units).
pub const LETTER_W_THOU: u32 = 8500;
/// Platen / ticket height in 1/1000 inch. Live Brother MFC GSE uses **11700**
/// (not 11000). WIA item extent uses this; CreateScanJob ImageInformation uses
/// the smaller [`optical_page_pixels`] crop (Brother live behavior).
pub const LETTER_H_THOU: u32 = 11700;
/// ADF maximum height in 1/1000 inch (`ADFMaximumSize` / Feeder page height).
/// Legal-length feeder windows use this; platen validation stays on [`LETTER_H_THOU`].
pub const ADF_MAX_H_THOU: u32 = 14000;

/// JPEG COM payload prefix so multi-page order is machine-readable without decoding pixels.
pub const PAGE_MARKER_PREFIX: &[u8] = b"fakescan-page:";

/// JPEG COM payload prefix for manual-duplex front/back tagging.
pub const SIDE_MARKER_PREFIX: &[u8] = b"fakescan-side:";

/// QR / text payload scheme (no OCR required — decode with zbar / zxing / rqrr).
/// Example: `fakescan|v1|page=0|pat=page_id|side=front`
pub const QR_SCHEME: &str = "fakescan|v1";

/// Max RGB pixels before we drop to 150 dpi (CI / memory).
const MAX_PIXELS: u32 = 4_000_000;

/// JPEG quality when a QR is stamped (JPEG softens modules; keep this high).
const JPEG_QUALITY_WITH_QR: u8 = 95;

/// Reasonable baseline quality for WIA accept trials (no QR to protect).
const JPEG_QUALITY_PLAIN: u8 = 80;

/// Front vs back face for manual duplex / `--reverse` stacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PageSide {
    Front,
    Back,
}

impl PageSide {
    pub fn as_str(self) -> &'static str {
        match self {
            PageSide::Front => "front",
            PageSide::Back => "back",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "front" | "f" => Ok(PageSide::Front),
            "back" | "b" => Ok(PageSide::Back),
            other => bail!("unknown side {other:?}; expected front or back"),
        }
    }
}

/// Per-request page context for diagnostic patterns ([`PatternId::Info`], [`PatternId::Anac`]).
///
/// Built by [`crate::state::World::retrieve_image_with_peer`] from the live job and the
/// WSD TCP peer address.  All fields degrade gracefully when absent.
#[derive(Debug, Clone, Default)]
pub struct PageContext {
    /// Active profile id (`brother-wsd`, …).
    pub profile_id: String,
    /// Device UUID as a hyphenated string.
    pub device_uuid: String,
    /// Active pattern id string (as returned by [`PatternId::as_str`]).
    pub pattern_id: String,
    pub dpi: u32,
    /// WSD `Format` token (`jfif`, …).
    pub format: String,
    /// Scan-job id assigned by CreateScanJob.
    pub job_id: i32,
    /// WSD RetrieveImage TCP peer address (`ip:port`).
    /// `None` or `"-"` means the address was not available.
    pub peer: Option<String>,
    /// UNIX seconds at render time.
    pub timestamp_secs: u64,
}

static DEFAULT_PATTERN: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(u8::MAX);

impl Default for PatternId {
    fn default() -> Self {
        PatternId::default_pattern()
    }
}

/// Named synthetic layout for RetrieveImage JPEGs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatternId {
    /// Numbered bars + center block (multi-page order cue). The previous default; select it with
    /// `--default-pattern page_id` / `FAKESCAN_DEFAULT_PATTERN=page_id` or `POST /v1/pattern`.
    PageId,
    /// Gray step wedges for brightness / contrast / threshold tests.
    Wedges,
    /// R/G/B + gray patches for color-mode checks.
    Patches,
    /// Large asymmetric F in the top-left for orientation.
    SlantF,
    /// L-marks in four corners + center cross for crop / region.
    Corners,
    /// Even indices blue/front, odd red/back (manual duplex / `--reverse`).
    DuplexPair,
    /// Large centered QR only (decode robustness); still COM-tagged.
    Qr,
    /// WIA accept trial: solid + gray wedges only (no banner, no QR).
    /// Pair with [`EncodeOpts::WIA_PLAIN`] (forced by [`encode_opts_for`]).
    WiaPlain,
    /// ~5° slanted edge (ISO 16067-1 style visual; no MTF math).
    SlantEdge,
    /// Text size ladder: the same phrase rendered at decreasing glyph scales.
    TextLadder,
    /// All-in-one composite: wedges + slant edge + colour patches + text ladder + corner marks.
    Testpage,
    /// Server diagnostics: profile, UUID, DPI, format, job id, client IP, time.
    Info,
    /// ANAC-style client IP readback (alias: `whoami`).
    Anac,
    /// CMYK bars + tint ladders (RGB-approximated; C / M / Y / K labelled).
    Cmyk,
    /// Nozzle-check style horizontal and vertical band groups (7 colour channels).
    Nozzle,
    /// Alignment / registration crosses + grid tick marks along all four edges.
    Align,
    /// Margin frames + scale rules (inch / quarter-inch tick cues).
    Grid,
    /// Resolution line ladder: coarse-to-fine horizontal and vertical line groups.
    Lines,
    /// The default page: parametric diagnostic layout v3 (see `diag_v3`): print marks, rulers, colour
    /// bars, ColorChecker, ramps, flat fields, resolution targets, orientation / duplex / page-order
    /// markers and a QR carrying the page parameters. Scales to the sheet.
    Diag,
    /// Fixed-size layout v2 (v1 content plus print marks, resolution, colour; see `diag_v2`).
    DiagV2,
    /// The original layout-v1 diag page, byte-identical to what `Diag` rendered before layout v2.
    DiagV1,
}

impl PatternId {
    pub const ALL: &'static [PatternId] = &[
        PatternId::PageId,
        PatternId::Wedges,
        PatternId::Patches,
        PatternId::SlantF,
        PatternId::Corners,
        PatternId::DuplexPair,
        PatternId::Qr,
        PatternId::WiaPlain,
        PatternId::SlantEdge,
        PatternId::TextLadder,
        PatternId::Testpage,
        PatternId::Info,
        PatternId::Anac,
        PatternId::Cmyk,
        PatternId::Nozzle,
        PatternId::Align,
        PatternId::Grid,
        PatternId::Lines,
        PatternId::Diag,
        PatternId::DiagV2,
        PatternId::DiagV1,
    ];

    /// The pattern a fresh device serves: `diag` unless overridden by [`PatternId::set_default`]
    /// or the `FAKESCAN_DEFAULT_PATTERN` environment variable (read once).
    pub fn default_pattern() -> PatternId {
        let v = DEFAULT_PATTERN.load(std::sync::atomic::Ordering::Relaxed);
        if v != u8::MAX {
            return PatternId::ALL[v as usize];
        }
        let p = std::env::var("FAKESCAN_DEFAULT_PATTERN")
            .ok()
            .and_then(|s| PatternId::parse(&s).ok())
            .unwrap_or(PatternId::Diag);
        PatternId::set_default(p);
        p
    }

    /// Override the process-wide default pattern (daemon `--default-pattern`).
    pub fn set_default(p: PatternId) {
        let i = PatternId::ALL.iter().position(|x| *x == p).unwrap_or(0);
        DEFAULT_PATTERN.store(i as u8, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PatternId::PageId => "page_id",
            PatternId::Wedges => "wedges",
            PatternId::Patches => "patches",
            PatternId::SlantF => "slant_f",
            PatternId::Corners => "corners",
            PatternId::DuplexPair => "duplex_pair",
            PatternId::Qr => "qr",
            PatternId::WiaPlain => "wia_plain",
            PatternId::SlantEdge => "slant_edge",
            PatternId::TextLadder => "textladder",
            PatternId::Testpage => "testpage",
            PatternId::Info => "info",
            PatternId::Anac => "anac",
            PatternId::Cmyk => "cmyk",
            PatternId::Nozzle => "nozzle",
            PatternId::Align => "align",
            PatternId::Grid => "grid",
            PatternId::Lines => "lines",
            PatternId::Diag => "diag",
            PatternId::DiagV2 => "diag_v2",
            PatternId::DiagV1 => "diag_v1",
        }
    }

    /// Name carried on the wire (QR `pat=`, banner): `diag_v1` is still a `diag` page.
    pub fn wire_name(self) -> &'static str {
        match self {
            PatternId::DiagV1 | PatternId::DiagV2 => "diag",
            other => other.as_str(),
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "page_id" | "page-id" | "default" => Ok(PatternId::PageId),
            "wedges" => Ok(PatternId::Wedges),
            "patches" => Ok(PatternId::Patches),
            "slant_f" | "slant-f" | "slantf" => Ok(PatternId::SlantF),
            "corners" => Ok(PatternId::Corners),
            "duplex_pair" | "duplex-pair" | "duplex" => Ok(PatternId::DuplexPair),
            "qr" | "qrcode" => Ok(PatternId::Qr),
            "wia_plain" | "wia-plain" | "plain" => Ok(PatternId::WiaPlain),
            "slant_edge" | "slant-edge" | "slantedge" => Ok(PatternId::SlantEdge),
            "textladder" | "text_ladder" | "text-ladder" => Ok(PatternId::TextLadder),
            "testpage" | "test_page" | "test-page" => Ok(PatternId::Testpage),
            "info" => Ok(PatternId::Info),
            "anac" | "whoami" => Ok(PatternId::Anac),
            "cmyk" => Ok(PatternId::Cmyk),
            "nozzle" => Ok(PatternId::Nozzle),
            "align" => Ok(PatternId::Align),
            "grid" => Ok(PatternId::Grid),
            "lines" => Ok(PatternId::Lines),
            "diag" => Ok(PatternId::Diag),
            "diag_v2" | "diag-v2" => Ok(PatternId::DiagV2),
            "diag_v1" | "diag-v1" => Ok(PatternId::DiagV1),
            other => bail!(
                "unknown pattern {other:?}; expected one of: {}",
                PatternId::ALL
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    /// True when this pattern is the WIA minimal JPEG layout.
    pub fn is_wia_plain(self) -> bool {
        matches!(self, PatternId::WiaPlain)
    }
}

/// Machine-readable page identity carried in the QR (and documented for host decoders).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrPayload {
    pub page: u32,
    pub pattern: PatternId,
    pub side: Option<PageSide>,
    /// Page layout version (`lay=N`); `None` = absent = layout 1 (every page before layout v2).
    pub layout: Option<u32>,
}

/// Build the QR text payload for a page.
pub fn qr_payload(pattern: PatternId, page_index: u32, side: Option<PageSide>) -> String {
    let side = side.map(|s| s.as_str()).unwrap_or("none");
    format!(
        "{QR_SCHEME}|page={page_index}|pat={}|side={side}",
        pattern.wire_name()
    )
}

/// Parse a QR / text payload produced by [`qr_payload`].
pub fn parse_qr_payload(text: &str) -> Option<QrPayload> {
    let text = text.trim();
    if !text.starts_with(QR_SCHEME) {
        return None;
    }
    let mut page = None;
    let mut pattern = None;
    let mut side = None;
    let mut layout = None;
    for part in text.split('|').skip(2) {
        if let Some(v) = part.strip_prefix("page=") {
            page = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("pat=") {
            pattern = PatternId::parse(v).ok();
        } else if let Some(v) = part.strip_prefix("lay=") {
            layout = v.parse().ok();
        } else if let Some(v) = part.strip_prefix("side=") {
            side = match v {
                "none" | "" => None,
                other => PageSide::parse(other).ok(),
            };
        }
    }
    Some(QrPayload {
        page: page?,
        pattern: pattern?,
        side,
        layout,
    })
}

pub fn page_pixels(dpi: u32) -> (u32, u32) {
    let w = (LETTER_W_THOU * dpi) / 1000;
    let h = (LETTER_H_THOU * dpi) / 1000;
    (w.max(1), h.max(1))
}

/// A real device's live deliverable width in 1/1000 inch (Transfer @200 → 1648 px).
/// Platen / WIA item extent stays [`LETTER_W_THOU`]; ImageInformation + SOF use this crop.
const OPTICAL_W_THOU: u32 = 8240;
/// Brother live deliverable height in 1/1000 inch (Transfer @200 → 2289 px).
const OPTICAL_H_THOU: u32 = 11445;

/// Pixel size Brother WSD actually returns (not full letter).
///
/// Lab: WIA extent stays letter (`page_pixels`), but CreateScanJob ImageInformation
/// and RetrieveImage SOF are this crop. Transfer @200 accepted at **1648×2289**.
/// A fake full-letter **1700×2340** (same JFIF layout) gets CancelJob ~30s after RetrieveImage.
/// Exact triples match live Brother samples @100 / @200 / @300.
pub fn optical_page_pixels(dpi: u32) -> (u32, u32) {
    match dpi {
        100 => (816, 1144),
        200 => (1648, 2289),
        300 => (2480, 3440),
        d => {
            let w = (OPTICAL_W_THOU * d) / 1000;
            let h = (OPTICAL_H_THOU * d) / 1000;
            (w.max(1), h.max(1))
        }
    }
}

/// Pixel size for Validate/Create `ImageInformation` **and** RetrieveImage encode.
///
/// Uses [`optical_page_pixels`] (Brother crop), then caps to 150 dpi optical when
/// the product exceeds [`MAX_PIXELS`]. SOF must match declared PixelsPerLine /
/// NumberOfLines.
pub fn job_page_pixels(dpi: u32) -> (u32, u32) {
    capped_page_pixels(dpi)
}

fn capped_page_pixels(dpi: u32) -> (u32, u32) {
    let (w, h) = optical_page_pixels(dpi);
    if w.saturating_mul(h) > MAX_PIXELS {
        optical_page_pixels(150)
    } else {
        (w, h)
    }
}

/// Encode a JPEG with the default `page_id` pattern and COM `fakescan-page:N`.
pub fn jpeg_page(dpi: u32, page_index: u32) -> Result<Vec<u8>> {
    jpeg_pattern(PatternId::PageId, dpi, page_index)
}

/// Encode options for WSD Format payloads (JPEG COM, QR stamp, quality, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodeOpts {
    /// Inject `fakescan-page:` / `fakescan-side:` JPEG COM markers (default true).
    /// Airscan / multi-page order tests rely on COM; WIA experiments may disable.
    pub inject_jpeg_com: bool,
    /// Stamp a corner / focus QR into the RGB buffer before encode.
    pub stamp_qr: bool,
    /// Stamp the tester banner across the top margin.
    pub stamp_banner: bool,
    /// When set, overrides effects / default JPEG quality (1–100).
    pub jpeg_quality: Option<u8>,
}

impl Default for EncodeOpts {
    fn default() -> Self {
        Self {
            inject_jpeg_com: true,
            stamp_qr: true,
            stamp_banner: true,
            jpeg_quality: None,
        }
    }
}

impl EncodeOpts {
    pub const WITH_COM: Self = Self {
        inject_jpeg_com: true,
        stamp_qr: true,
        stamp_banner: true,
        jpeg_quality: None,
    };
    pub const NO_COM: Self = Self {
        inject_jpeg_com: false,
        stamp_qr: true,
        stamp_banner: true,
        jpeg_quality: None,
    };
    /// Baseline JFIF for WIA RetrieveImage accept: no COM, no QR, no banner, Q≈80.
    pub const WIA_PLAIN: Self = Self {
        inject_jpeg_com: false,
        stamp_qr: false,
        stamp_banner: false,
        jpeg_quality: Some(JPEG_QUALITY_PLAIN),
    };
}

/// Resolve encode knobs from the active pattern and `POST /v1/jpeg-com`.
///
/// [`PatternId::WiaPlain`] always forces [`EncodeOpts::WIA_PLAIN`] (COM-off is
/// not enough alone — QR/banner/high-Q remain on other patterns).
pub fn encode_opts_for(pattern: PatternId, jpeg_com: bool) -> EncodeOpts {
    if pattern.is_wia_plain() {
        EncodeOpts::WIA_PLAIN
    } else if jpeg_com {
        EncodeOpts::WITH_COM
    } else {
        EncodeOpts::NO_COM
    }
}

/// Encode a JPEG for the given pattern id (always injects `fakescan-page:N`
/// and a corner QR with [`qr_payload`]).
///
/// `duplex_pair` also injects `fakescan-side:front|back` from the page index
/// (even → front/blue, odd → back/red).
pub fn jpeg_pattern(pattern: PatternId, dpi: u32, page_index: u32) -> Result<Vec<u8>> {
    jpeg_pattern_with_side(pattern, dpi, page_index, None)
}

/// Like [`jpeg_pattern`], but an explicit `side` overrides duplex auto-tagging
/// (and tags non-duplex patterns for front/back stack tests).
pub fn jpeg_pattern_with_side(
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
) -> Result<Vec<u8>> {
    encode_pattern(
        "jfif",
        pattern,
        dpi,
        page_index,
        side,
        None,
        crate::effects::ColorMode::Rgb24,
        EncodeOpts::WITH_COM,
    )
}

/// True when one RetrieveImage should return every remaining job page in a single file.
pub fn is_multipage_format(wsd_format: &str) -> bool {
    matches!(
        wsd_format,
        "tiff-multi-uncompressed"
            | "tiff-multi-g4"
            | "tiff-multi-g3mh"
            | "tiff-multi-jpeg-tn2"
            | "pdf-a"
            | "pdf"
            | "xps"
    )
}

/// How many feeder pages one RetrieveImage consumes for this Format.
pub fn pages_per_retrieve(wsd_format: &str, pages_left: u32) -> u32 {
    if pages_left == 0 {
        0
    } else if is_multipage_format(wsd_format) {
        pages_left
    } else {
        1
    }
}

/// Render pattern (+ optional effects / color) and encode as the WSD Format token.
#[allow(clippy::too_many_arguments)] // format, pattern, geometry, and encode options stay positional
pub fn encode_pattern(
    wsd_format: &str,
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
    effects: Option<&crate::effects::ImageEffects>,
    color: crate::effects::ColorMode,
    opts: EncodeOpts,
) -> Result<Vec<u8>> {
    encode_pattern_pages(
        wsd_format,
        pattern,
        dpi,
        &[page_index],
        side,
        effects,
        color,
        opts,
    )
}

/// Encode one or more sheet indices into a (possibly multi-page) Format payload.
///
/// For diagnostic patterns ([`PatternId::Info`], [`PatternId::Anac`]) prefer
/// [`encode_pattern_pages_ctx`] so server / client context appears on the page.
#[allow(clippy::too_many_arguments)]
pub fn encode_pattern_pages(
    wsd_format: &str,
    pattern: PatternId,
    dpi: u32,
    sheet_indices: &[u32],
    side: Option<PageSide>,
    effects: Option<&crate::effects::ImageEffects>,
    color: crate::effects::ColorMode,
    opts: EncodeOpts,
) -> Result<Vec<u8>> {
    encode_pattern_pages_ctx(
        wsd_format,
        pattern,
        dpi,
        sheet_indices,
        side,
        effects,
        color,
        opts,
        None,
    )
}

/// Like [`encode_pattern_pages`] but threads a [`PageContext`] into diagnostic patterns.
#[allow(clippy::too_many_arguments)]
pub fn encode_pattern_pages_ctx(
    wsd_format: &str,
    pattern: PatternId,
    dpi: u32,
    sheet_indices: &[u32],
    side: Option<PageSide>,
    effects: Option<&crate::effects::ImageEffects>,
    color: crate::effects::ColorMode,
    opts: EncodeOpts,
    ctx: Option<&PageContext>,
) -> Result<Vec<u8>> {
    if sheet_indices.is_empty() {
        bail!("encode_pattern_pages: no sheets");
    }
    let mut pages = Vec::with_capacity(sheet_indices.len());
    let jpeg_q = opts
        .jpeg_quality
        .filter(|&q| q > 0)
        .or_else(|| effects.map(|fx| fx.effective_jpeg_quality()))
        .unwrap_or(JPEG_QUALITY_WITH_QR);
    for &sheet in sheet_indices {
        let (mut img, effective_side) =
            render_pattern_rgb_ctx(pattern, dpi, sheet, side, opts, ctx)?;
        if let Some(fx) = effects {
            crate::effects::apply_pipeline(&mut img, fx, sheet, color);
        } else if color != crate::effects::ColorMode::Rgb24 {
            crate::effects::apply_pipeline(
                &mut img,
                &crate::effects::ImageEffects::default(),
                sheet,
                color,
            );
        }
        pages.push((img, sheet, effective_side));
    }
    // APP0 density must match the pixels we actually rendered (cap may drop dpi).
    let effective_dpi = {
        let (cw, _) = capped_page_pixels(dpi);
        let (ow, _) = optical_page_pixels(dpi);
        if cw != ow {
            150
        } else {
            dpi
        }
    };
    let spec = PixelSpec {
        color,
        polarity: effects.map(|fx| fx.bilevel_polarity).unwrap_or_default(),
        jfif_gray: effects.map(|fx| fx.jfif_gray).unwrap_or_default(),
        jfif_bw: effects.map(|fx| fx.jfif_bw).unwrap_or_default(),
    };
    encode_images(wsd_format, &pages, jpeg_q, opts, effective_dpi, spec)
}

/// Back-compat alias: always JFIF with COM markers.
pub fn jpeg_pattern_processed(
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
    effects: Option<&crate::effects::ImageEffects>,
    color: crate::effects::ColorMode,
) -> Result<Vec<u8>> {
    encode_pattern(
        "jfif",
        pattern,
        dpi,
        page_index,
        side,
        effects,
        color,
        EncodeOpts::WITH_COM,
    )
}

/// Draw pattern, banner, and QR into an RGB buffer (no JPEG yet).
///
/// Uses default encode decorations (banner + QR on). Prefer
/// [`render_pattern_rgb_opts`] when applying [`EncodeOpts::WIA_PLAIN`].
pub fn render_pattern_rgb(
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
) -> Result<(RgbImage, Option<PageSide>)> {
    render_pattern_rgb_opts(pattern, dpi, page_index, side, EncodeOpts::default())
}

/// Like [`render_pattern_rgb`], honoring QR / banner encode knobs.
///
/// For diagnostic patterns [`PatternId::Info`] and [`PatternId::Anac`] the caller should
/// prefer [`render_pattern_rgb_ctx`] so that server / client context is embedded.
pub fn render_pattern_rgb_opts(
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
    opts: EncodeOpts,
) -> Result<(RgbImage, Option<PageSide>)> {
    render_pattern_rgb_ctx(pattern, dpi, page_index, side, opts, None)
}

/// Like [`render_pattern_rgb_opts`] but threads a [`PageContext`] into diagnostic patterns.
///
/// `ctx` is used by [`PatternId::Info`] and [`PatternId::Anac`] to embed server / client
/// diagnostics on the page and to append `client=` to the QR payload when the peer is known.
pub fn render_pattern_rgb_ctx(
    pattern: PatternId,
    dpi: u32,
    page_index: u32,
    side: Option<PageSide>,
    opts: EncodeOpts,
    ctx: Option<&PageContext>,
) -> Result<(RgbImage, Option<PageSide>)> {
    let (w, h) = capped_page_pixels(dpi);
    let effective_side = side.or_else(|| match pattern {
        PatternId::DuplexPair => Some(if page_index.is_multiple_of(2) {
            PageSide::Front
        } else {
            PageSide::Back
        }),
        _ => None,
    });
    let mut img = match pattern {
        PatternId::PageId => draw_page_id(w, h, page_index),
        PatternId::Wedges | PatternId::WiaPlain => draw_wedges(w, h, page_index),
        PatternId::Patches => draw_patches(w, h, page_index),
        PatternId::SlantF => draw_slant_f(w, h, page_index),
        PatternId::Corners => draw_corners(w, h, page_index),
        PatternId::DuplexPair => {
            draw_duplex_pair(w, h, page_index, effective_side.unwrap_or(PageSide::Front))
        }
        PatternId::Qr => draw_qr_focus(w, h, page_index),
        PatternId::SlantEdge => crate::image_iso::draw_slant_edge(w, h, page_index),
        PatternId::TextLadder => crate::image_iso::draw_text_ladder(w, h, page_index),
        PatternId::Testpage => crate::image_iso::draw_testpage(w, h, page_index),
        PatternId::Info => draw_info(w, h, page_index, ctx),
        PatternId::Anac => draw_anac(w, h, page_index, ctx),
        PatternId::Cmyk => crate::image_charts::draw_cmyk(w, h, page_index),
        PatternId::Nozzle => crate::image_charts::draw_nozzle(w, h, page_index),
        PatternId::Align => crate::image_charts::draw_align(w, h, page_index),
        PatternId::Grid => crate::image_charts::draw_grid(w, h, page_index),
        PatternId::Lines => crate::image_charts::draw_lines(w, h, page_index),
        PatternId::Diag => {
            let mut sp =
                crate::diag_v3::PageSpec::from_px("custom", w, h, diag::effective_dpi(dpi));
            sp.page = page_index;
            sp.side = effective_side;
            if opts.stamp_banner {
                sp.banner_px = crate::diag_v3::banner_height_px(w, h);
            }
            // No client address in the QR: the default page must be byte-stable across connections.
            let _ = ctx;
            crate::diag_v3::render(&sp).0
        }
        PatternId::DiagV2 => diag::draw_diag_v(w, h, diag::effective_dpi(dpi), 2),
        PatternId::DiagV1 => diag::draw_diag_v(w, h, diag::effective_dpi(dpi), 1),
    };
    if opts.stamp_banner {
        let banner = pattern_banner(pattern, page_index, effective_side);
        stamp_banner(&mut img, &banner);
    }

    if opts.stamp_qr {
        // Append `|client=<ip>` to the QR when a peer is available (Info/Anac or any pattern).
        let mut base = qr_payload(pattern, page_index, effective_side);
        if pattern == PatternId::DiagV2 {
            // Layout version rides in the payload; v1 pages (and older builds) omit it.
            base.push_str(&format!("|lay={}", crate::diag_v2::LAYOUT_VERSION));
        }
        let payload = match ctx
            .and_then(|c| c.peer.as_deref())
            .filter(|p| !p.is_empty() && *p != "-")
        {
            Some(peer) => format!("{base}|client={peer}"),
            None => base,
        };
        if pattern == PatternId::Diag {
            // layout v3 draws its own QR (it carries the page parameters)
        } else if matches!(pattern, PatternId::DiagV2 | PatternId::DiagV1) {
            let e = diag::effective_dpi(dpi);
            stamp_qr(
                &mut img,
                &payload,
                QrPlacement::AnchorBottomRight(
                    diag::px(diag::QR_ANCHOR_MM.0, e),
                    diag::px(diag::QR_ANCHOR_MM.1, e),
                ),
                diag::px(diag::QR_TARGET_MM, e),
            )?;
        } else if pattern != PatternId::Qr {
            stamp_qr(
                &mut img,
                &payload,
                QrPlacement::BottomRight,
                qr_corner_size(w, h),
            )?;
        } else {
            stamp_qr(&mut img, &payload, QrPlacement::Center, qr_focus_size(w, h))?;
        }
    }
    Ok((img, effective_side))
}

/// Banner drawn at the top of every page (tester-facing, no OCR needed to spot).
pub fn pattern_banner(pattern: PatternId, page_index: u32, side: Option<PageSide>) -> String {
    let step = crate::scenarios::PROGRESSION
        .iter()
        .find(|s| s.pattern == pattern)
        .map(|s| s.title)
        .unwrap_or(pattern.wire_name());
    let side = side.map(|s| s.as_str()).unwrap_or("-");
    format!("{step}  P{page_index}  {side}")
}

fn stamp_banner(img: &mut RgbImage, text: &str) {
    let (w, h) = img.dimensions();
    if w < 80 || h < 40 {
        return;
    }
    let scale = (w.min(h) / 180).clamp(2, 6);
    let pad = scale * 2;
    let glyph_h = 7 * scale;
    let band = glyph_h + pad * 2;
    fill_rect(img, 0, 0, w, band.min(h), Rgb([20, 24, 32]));
    let mut x = pad;
    let y = pad;
    let ink = Rgb([240, 240, 235]);
    for ch in text.chars().take(48) {
        let advance = draw_glyph(img, ch, x, y, scale, ink);
        x = x.saturating_add(advance + scale);
        if x + 5 * scale >= w {
            break;
        }
    }
}

/// Tiny 5×7 glyphs (uppercase / digits / a few punctuation). Unknown → blank.
/// Returns the advance width (5 × scale); caller adds an inter-character gap.
pub(crate) fn draw_glyph(
    img: &mut RgbImage,
    ch: char,
    x0: u32,
    y0: u32,
    scale: u32,
    color: Rgb<u8>,
) -> u32 {
    let rows = glyph_rows(ch);
    for (row, bits) in rows.iter().enumerate() {
        for col in 0..5u32 {
            if bits & (1 << (4 - col)) != 0 {
                let px = x0 + col * scale;
                let py = y0 + (row as u32) * scale;
                fill_rect(img, px, py, px + scale, py + scale, color);
            }
        }
    }
    5 * scale
}

pub(crate) fn glyph_rows(ch: char) -> [u8; 7] {
    // 5-bit rows, MSB = left pixel.
    match ch.to_ascii_uppercase() {
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        '-' => [0, 0, 0, 0b11111, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 0b00100, 0b00100],
        ':' => [0, 0b00100, 0b00100, 0, 0b00100, 0b00100, 0],
        '/' => [0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0, 0],
        '0' => [
            0b01110, 0b10001, 0b10011, 0b10101, 0b11001, 0b10001, 0b01110,
        ],
        '1' => [
            0b00100, 0b01100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        '2' => [
            0b01110, 0b10001, 0b00001, 0b00010, 0b00100, 0b01000, 0b11111,
        ],
        '3' => [
            0b11110, 0b00001, 0b00001, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        '4' => [
            0b00010, 0b00110, 0b01010, 0b10010, 0b11111, 0b00010, 0b00010,
        ],
        '5' => [
            0b11111, 0b10000, 0b11110, 0b00001, 0b00001, 0b10001, 0b01110,
        ],
        '6' => [
            0b01110, 0b10000, 0b10000, 0b11110, 0b10001, 0b10001, 0b01110,
        ],
        '7' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b01000, 0b01000,
        ],
        '8' => [
            0b01110, 0b10001, 0b10001, 0b01110, 0b10001, 0b10001, 0b01110,
        ],
        '9' => [
            0b01110, 0b10001, 0b10001, 0b01111, 0b00001, 0b00001, 0b01110,
        ],
        'A' => [
            0b01110, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'B' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10001, 0b10001, 0b11110,
        ],
        'C' => [
            0b01110, 0b10001, 0b10000, 0b10000, 0b10000, 0b10001, 0b01110,
        ],
        'D' => [
            0b11110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b11110,
        ],
        'E' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b11111,
        ],
        'F' => [
            0b11111, 0b10000, 0b10000, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'G' => [
            0b01110, 0b10001, 0b10000, 0b10111, 0b10001, 0b10001, 0b01110,
        ],
        'H' => [
            0b10001, 0b10001, 0b10001, 0b11111, 0b10001, 0b10001, 0b10001,
        ],
        'I' => [
            0b01110, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b01110,
        ],
        'K' => [
            0b10001, 0b10010, 0b10100, 0b11000, 0b10100, 0b10010, 0b10001,
        ],
        'L' => [
            0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b10000, 0b11111,
        ],
        'M' => [
            0b10001, 0b11011, 0b10101, 0b10001, 0b10001, 0b10001, 0b10001,
        ],
        'N' => [
            0b10001, 0b11001, 0b10101, 0b10011, 0b10001, 0b10001, 0b10001,
        ],
        'O' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'P' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10000, 0b10000, 0b10000,
        ],
        'Q' => [
            0b01110, 0b10001, 0b10001, 0b10001, 0b10101, 0b10010, 0b01101,
        ],
        'R' => [
            0b11110, 0b10001, 0b10001, 0b11110, 0b10100, 0b10010, 0b10001,
        ],
        'S' => [
            0b01111, 0b10000, 0b10000, 0b01110, 0b00001, 0b00001, 0b11110,
        ],
        'T' => [
            0b11111, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'U' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01110,
        ],
        'V' => [
            0b10001, 0b10001, 0b10001, 0b10001, 0b10001, 0b01010, 0b00100,
        ],
        'W' => [
            0b10001, 0b10001, 0b10001, 0b10101, 0b10101, 0b10101, 0b01010,
        ],
        'X' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b01010, 0b10001, 0b10001,
        ],
        'Y' => [
            0b10001, 0b10001, 0b01010, 0b00100, 0b00100, 0b00100, 0b00100,
        ],
        'Z' => [
            0b11111, 0b00001, 0b00010, 0b00100, 0b01000, 0b10000, 0b11111,
        ],
        _ => [0, 0, 0, 0b00100, 0, 0, 0],
    }
}

/// Encode RGB buffer as a WSD Scan Format payload.
pub fn encode_image(
    wsd_format: &str,
    img: RgbImage,
    page_index: u32,
    side: Option<PageSide>,
) -> Result<Vec<u8>> {
    encode_image_at(wsd_format, img, page_index, side, None)
}

/// [`encode_image`] at a known resolution. `None` falls back to inferring it from a letter-wide page, which is
/// only right for letter-wide images: an uploaded loopback page passes the density it carries ([`image_dpi`]).
pub fn encode_image_at(
    wsd_format: &str,
    img: RgbImage,
    page_index: u32,
    side: Option<PageSide>,
    dpi: Option<u32>,
) -> Result<Vec<u8>> {
    let dpi = dpi.unwrap_or_else(|| jfif_dpi_from_img(&img));
    encode_images(
        wsd_format,
        &[(img, page_index, side)],
        JPEG_QUALITY_WITH_QR,
        EncodeOpts::WITH_COM,
        dpi,
        PixelSpec::default(),
    )
}

/// Bit depth / polarity the stored pixels must have (from the ticket's
/// ColorProcessing and the `bilevel_polarity` effect). Pages arrive as RGB
/// buffers already converted by `effects::apply_pipeline` (equal channels for
/// Gray8 / BW1); this decides how they are *stored*.
#[derive(Debug, Clone, Copy, Default)]
pub struct PixelSpec {
    pub color: crate::effects::ColorMode,
    pub polarity: crate::effects::BilevelPolarity,
    /// `jfif_gray` effect: whether Gray8 JFIF/EXIF is a real 1-component JPEG.
    pub jfif_gray: crate::effects::JfifGray,
    /// `jfif_bw` effect: whether BW1 JFIF/EXIF is a real 1-component JPEG.
    pub jfif_bw: crate::effects::JfifBw,
}

impl PixelSpec {
    /// True when JFIF/EXIF output must be a genuine single-component gray JPEG.
    pub fn gray_jpeg(&self) -> bool {
        use crate::effects::{ColorMode, JfifBw};
        match self.color {
            ColorMode::Gray8 => self.jfif_gray.gray_jpeg(),
            ColorMode::Bw1 => self.jfif_bw == JfifBw::Gray8,
            ColorMode::Rgb24 => false,
        }
    }
}

/// Luma threshold for BW1 packing (below = black). WS-Scan has no Threshold
/// element in the ticket; Brightness/Contrast effects act before this.
pub const BW_THRESHOLD: u8 = 128;

/// Encode one or more RGB pages. Multi-page Formats pack every page into one blob.
fn encode_images(
    wsd_format: &str,
    pages: &[(RgbImage, u32, Option<PageSide>)],
    jpeg_quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    spec: PixelSpec,
) -> Result<Vec<u8>> {
    use crate::effects::ColorMode;
    if pages.is_empty() {
        bail!("no pages to encode");
    }
    match wsd_format {
        "jfif" | "jpeg" | "jpg" => {
            let (img, idx, side) = take_single(pages, wsd_format)?;
            encode_jpeg_ex(
                img.clone(),
                idx,
                side,
                jpeg_quality,
                opts,
                dpi,
                spec.gray_jpeg(),
            )
        }
        "exif" => {
            let (img, idx, side) = take_single(pages, wsd_format)?;
            encode_exif_jpeg(
                img.clone(),
                idx,
                side,
                jpeg_quality,
                opts,
                dpi,
                spec.gray_jpeg(),
            )
        }
        "dib" | "bmp" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            match spec.color {
                ColorMode::Rgb24 => encode_bmp(img.clone(), dpi),
                ColorMode::Gray8 => encode_bmp_indexed(img, 8, spec.polarity, dpi),
                ColorMode::Bw1 => encode_bmp_indexed(img, 1, spec.polarity, dpi),
            }
        }
        "png" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            match spec.color {
                ColorMode::Rgb24 => encode_png(img.clone(), dpi),
                ColorMode::Gray8 => encode_png_gray(img, 8, dpi),
                ColorMode::Bw1 => encode_png_gray(img, 1, dpi),
            }
        }
        "tiff-single-uncompressed" | "tiff" | "tif" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            match spec.color {
                ColorMode::Rgb24 => encode_tiff(img.clone(), dpi),
                _ => encode_tiff_indexed(&[img], spec, dpi),
            }
        }
        "tiff-single-g4" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            encode_tiff_g4(&[img], dpi)
        }
        "tiff-single-g3mh" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            encode_tiff_g3mh(&[img], dpi)
        }
        "tiff-single-jpeg-tn2" => encode_tiff_jpeg_tn2(
            pages,
            jpeg_quality,
            opts,
            dpi,
            spec.color != ColorMode::Rgb24,
        ),
        "tiff-multi-uncompressed" => {
            let imgs: Vec<&RgbImage> = pages.iter().map(|(i, _, _)| i).collect();
            match spec.color {
                ColorMode::Rgb24 => encode_tiff_multi_uncompressed(&imgs, dpi),
                _ => encode_tiff_indexed(&imgs, spec, dpi),
            }
        }
        "tiff-multi-g4" => {
            let imgs: Vec<&RgbImage> = pages.iter().map(|(i, _, _)| i).collect();
            encode_tiff_g4(&imgs, dpi)
        }
        "tiff-multi-g3mh" => {
            let imgs: Vec<&RgbImage> = pages.iter().map(|(i, _, _)| i).collect();
            encode_tiff_g3mh(&imgs, dpi)
        }
        "tiff-multi-jpeg-tn2" => encode_tiff_jpeg_tn2(
            pages,
            jpeg_quality,
            opts,
            dpi,
            spec.color != ColorMode::Rgb24,
        ),
        "jpeg2k" => {
            let (img, _, _) = take_single(pages, wsd_format)?;
            encode_jpeg2k(img, spec.color != ColorMode::Rgb24)
        }
        "pdf-a" | "pdf" => encode_pdf_pages(
            pages,
            jpeg_quality,
            opts,
            dpi,
            spec.color != ColorMode::Rgb24,
        ),
        "xps" => encode_xps_pages(
            pages,
            jpeg_quality,
            opts,
            dpi,
            spec.color != ColorMode::Rgb24,
        ),
        other => bail!("encode not implemented for format {other:?}"),
    }
}

fn take_single<'a>(
    pages: &'a [(RgbImage, u32, Option<PageSide>)],
    format: &str,
) -> Result<(&'a RgbImage, u32, Option<PageSide>)> {
    if pages.len() != 1 {
        bail!("{format} is single-page; got {} sheets", pages.len());
    }
    Ok((&pages[0].0, pages[0].1, pages[0].2))
}

/// The horizontal resolution an encoded image carries, in dots per inch: JFIF `APP0` (units 1 = inch, 2 = cm), PNG
/// `pHYs` (per metre) or BMP pels per metre. `None` when it carries none that can be used.
pub fn image_dpi(bytes: &[u8]) -> Option<u32> {
    let sane =
        |dpi: f64| (dpi.is_finite() && (10.0..=9600.0).contains(&dpi)).then(|| dpi.round() as u32);
    if bytes.starts_with(&[0xFF, 0xD8]) {
        // Walk the markers to APP0 "JFIF\0".
        let mut at = 2usize;
        while at + 4 <= bytes.len() && bytes[at] == 0xFF {
            let marker = bytes[at + 1];
            let len = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
            if marker == 0xE0 && bytes.get(at + 4..at + 9) == Some(b"JFIF\0".as_slice()) {
                let units = *bytes.get(at + 11)?;
                let x = u16::from_be_bytes([*bytes.get(at + 12)?, *bytes.get(at + 13)?]);
                return match units {
                    1 => sane(f64::from(x)),
                    2 => sane(f64::from(x) * 2.54),
                    _ => None,
                };
            }
            if marker == 0xDA || len < 2 {
                return None;
            }
            at += 2 + len;
        }
        return None;
    }
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut at = 8usize;
        while at + 8 <= bytes.len() {
            let len = u32::from_be_bytes(bytes[at..at + 4].try_into().ok()?) as usize;
            if &bytes[at + 4..at + 8] == b"pHYs" && len >= 9 {
                let x = u32::from_be_bytes(bytes.get(at + 8..at + 12)?.try_into().ok()?);
                return if *bytes.get(at + 16)? == 1 {
                    sane(f64::from(x) * 0.0254)
                } else {
                    None
                };
            }
            at += 12 + len;
        }
        return None;
    }
    if bytes.starts_with(b"BM") && bytes.len() >= 42 {
        let x = i32::from_le_bytes(bytes[38..42].try_into().ok()?);
        return if x > 0 {
            sane(f64::from(x) * 0.0254)
        } else {
            None
        };
    }
    None
}

/// Infer JFIF density from letter-sized RGB dimensions (fallback for encode_image).
fn jfif_dpi_from_img(img: &RgbImage) -> u32 {
    let inferred = (img.width().saturating_mul(1000) + LETTER_W_THOU / 2) / LETTER_W_THOU;
    inferred.clamp(72, 1200)
}

/// MCU columns for 4:2:0 (16×16 MCU) — Brother uses DRI = one MCU row.
fn restart_interval_mcu_row(width: u32) -> u16 {
    let cols = width.saturating_add(15) / 16;
    cols.clamp(1, u16::MAX as u32) as u16
}

/// Encode JFIF toward Brother WSD / wsdscdrv shape: baseline SOF0, 4:2:0,
/// APP0 units=dpi, DRI = one MCU row. (`image` crate emits 4:4:4 + aspect 1×1.)
/// Optionally a genuine single-component 8-bit gray scan
/// (`gray`; luma = the red channel of an already-gray RGB buffer, SOF0 Nf=1).
fn encode_jpeg_ex(
    img: RgbImage,
    page_index: u32,
    side: Option<PageSide>,
    quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    gray: bool,
) -> Result<Vec<u8>> {
    let q = if quality == 0 {
        JPEG_QUALITY_WITH_QR
    } else {
        quality.clamp(1, 100)
    };
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        bail!("empty image for jpeg");
    }
    if w > u16::MAX as u32 || h > u16::MAX as u32 {
        bail!("image {w}x{h} exceeds JPEG u16 dimensions");
    }
    let dens = dpi.clamp(1, u16::MAX as u32) as u16;
    let mut out = Vec::new();
    {
        let mut encoder = jpeg_encoder::Encoder::new(&mut out, q);
        encoder.set_density(jpeg_encoder::PixelDensity::dpi(dens));
        if gray {
            // One component: MCU = one 8x8 block, so a restart row is width/8 MCUs.
            encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
            encoder.set_restart_interval((w.div_ceil(8)).clamp(1, u16::MAX as u32) as u16);
            let luma: Vec<u8> = img
                .as_raw()
                .as_chunks::<3>()
                .0
                .iter()
                .map(|p| p[0])
                .collect();
            encoder
                .encode(&luma, w as u16, h as u16, jpeg_encoder::ColorType::Luma)
                .map_err(|e| anyhow::anyhow!("jpeg encode: {e}"))?;
        } else {
            // Force 4:2:0 even at Q≥90 (crate default would be 4:4:4).
            encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
            encoder.set_restart_interval(restart_interval_mcu_row(w));
            encoder
                .encode(
                    img.as_raw(),
                    w as u16,
                    h as u16,
                    jpeg_encoder::ColorType::Rgb,
                )
                .map_err(|e| anyhow::anyhow!("jpeg encode: {e}"))?;
        }
    }
    if out.is_empty() {
        bail!("empty jpeg");
    }
    // jpeg-encoder uses component ids 0/1/2; Brother / JFIF scanners use 1/2/3.
    remap_jpeg_component_ids_1_based(&mut out)?;
    // jpeg-encoder order is APP0→SOF→DQT×N→DHT×N→DRI→SOS; Brother / wsdscdrv
    // samples are APP0→DQT→DRI→SOF→DHT→SOS with tables packed into one segment each.
    out = reshape_jpeg_brother_layout(&out)?;
    if opts.inject_jpeg_com {
        inject_page_com(&mut out, page_index)?;
        if let Some(side) = side {
            inject_side_com(&mut out, side)?;
        }
    }
    Ok(out)
}

/// Rebuild header segments toward Brother WSD / WIA JPEG layout.
///
/// Target order: `SOI APP0 [APP*/COM…] DQT DRI SOF0 DHT SOS … EOI`, with all
/// quantization tables in one DQT and all Huffman tables in one DHT.
fn reshape_jpeg_brother_layout(jpeg: &[u8]) -> Result<Vec<u8>> {
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        bail!("not a JPEG (missing SOI)");
    }
    let mut app0: Option<Vec<u8>> = None;
    let mut misc: Vec<Vec<u8>> = Vec::new(); // APP1.. / COM / other APPn
    let mut dqt_payload = Vec::new();
    let mut dht_payload = Vec::new();
    let mut dri: Option<Vec<u8>> = None;
    let mut sof: Option<Vec<u8>> = None;
    let mut sos: Option<Vec<u8>> = None;
    let mut i = 2usize;
    while i + 1 < jpeg.len() {
        if jpeg[i] != 0xFF {
            bail!("jpeg desync before SOS at offset {i}");
        }
        let mut j = i + 1;
        while j < jpeg.len() && jpeg[j] == 0xFF {
            j += 1;
        }
        if j >= jpeg.len() {
            bail!("truncated JPEG marker");
        }
        let marker = jpeg[j];
        i = j + 1;
        if marker == 0xD9 {
            bail!("EOI before SOS");
        }
        if (0xD0..=0xD7).contains(&marker) || marker == 0x01 {
            continue;
        }
        if i + 2 > jpeg.len() {
            bail!("truncated JPEG segment length");
        }
        let len = u16::from_be_bytes([jpeg[i], jpeg[i + 1]]) as usize;
        if len < 2 || i + len > jpeg.len() {
            bail!("bad JPEG segment length {len} at marker {marker:#04x}");
        }
        let segment = jpeg[j - 1..i + len].to_vec(); // FF marker + len + payload
        let payload = &jpeg[i + 2..i + len];
        i += len;
        match marker {
            0xE0 => {
                if app0.is_none() {
                    app0 = Some(segment);
                } else {
                    misc.push(segment);
                }
            }
            0xDB => dqt_payload.extend_from_slice(payload),
            0xC4 => dht_payload.extend_from_slice(payload),
            0xDD => dri = Some(segment),
            0xC0 => sof = Some(segment),
            0xDA => {
                sos = Some(segment);
                break;
            }
            m if (0xC0..=0xCF).contains(&m) && m != 0xC4 && m != 0xC8 && m != 0xCC => {
                // Other SOF — keep position as frame header.
                sof = Some(segment);
            }
            _ => misc.push(segment),
        }
    }
    let app0 = app0.ok_or_else(|| anyhow::anyhow!("JPEG missing APP0/JFIF"))?;
    let sof = sof.ok_or_else(|| anyhow::anyhow!("JPEG missing SOF"))?;
    let sos = sos.ok_or_else(|| anyhow::anyhow!("JPEG missing SOS"))?;
    if dqt_payload.is_empty() || dht_payload.is_empty() {
        bail!("JPEG missing DQT or DHT");
    }
    let dqt = pack_jpeg_segment(0xDB, &dqt_payload)?;
    let dht = pack_jpeg_segment(0xC4, &dht_payload)?;
    let mut out = Vec::with_capacity(jpeg.len() + 32);
    out.extend_from_slice(&[0xFF, 0xD8]);
    out.extend_from_slice(&app0);
    for seg in misc {
        out.extend_from_slice(&seg);
    }
    out.extend_from_slice(&dqt);
    if let Some(dri) = dri {
        out.extend_from_slice(&dri);
    }
    out.extend_from_slice(&sof);
    out.extend_from_slice(&dht);
    out.extend_from_slice(&sos);
    out.extend_from_slice(&jpeg[i..]); // entropy + EOI
    Ok(out)
}

fn pack_jpeg_segment(marker: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let len = 2 + payload.len();
    if len > u16::MAX as usize {
        bail!("JPEG segment {marker:#04x} too long ({len})");
    }
    let mut out = Vec::with_capacity(2 + len);
    out.push(0xFF);
    out.push(marker);
    out.extend_from_slice(&(len as u16).to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// Rewrite SOF/SOS component selectors from 0-based (jpeg-encoder) to 1-based (Brother).
fn remap_jpeg_component_ids_1_based(jpeg: &mut [u8]) -> Result<()> {
    let mut i = 0;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 {
            break;
        }
        if (0xD0..=0xD7).contains(&marker) {
            i += 2;
            continue;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        let payload = &mut jpeg[i + 4..i + 2 + len];
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            // SOF: [P Y Y X X Nf] then Nf × [id HV Tq]
            if payload.len() >= 6 {
                let nf = payload[5] as usize;
                let mut off = 6;
                for _ in 0..nf {
                    if off + 3 > payload.len() {
                        break;
                    }
                    if payload[off] < 3 {
                        payload[off] = payload[off].saturating_add(1);
                    }
                    off += 3;
                }
            }
        } else if marker == 0xDA {
            // SOS: [Ns] then Ns × [Cs TdTa], then 3 spectral bytes
            if !payload.is_empty() {
                let ns = payload[0] as usize;
                let mut off = 1;
                for _ in 0..ns {
                    if off + 2 > payload.len() {
                        break;
                    }
                    if payload[off] < 3 {
                        payload[off] = payload[off].saturating_add(1);
                    }
                    off += 2;
                }
            }
        }
        if marker == 0xDA {
            break; // entropy follows
        }
        i += 2 + len;
    }
    Ok(())
}

/// Pixels per metre for a dpi (BMP biXPelsPerMeter / PNG pHYs), rounded to nearest.
fn pixels_per_metre(dpi: u32) -> u32 {
    ((u64::from(dpi.max(1)) * 10_000 + 127) / 254) as u32
}

/// 24-bpp BI_RGB bottom-up BMP (BITMAPINFOHEADER, BGR rows padded to 4) with real density.
fn encode_bmp(img: RgbImage, dpi: u32) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image for bmp");
    }
    let stride = stride_bytes(w, 24, 4) as usize;
    let pix_len = stride * h as usize;
    let off_bits = 14 + 40;
    let mut out = Vec::with_capacity(off_bits + pix_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((off_bits + pix_len) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(off_bits as u32).to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes()); // positive = bottom-up
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(pix_len as u32).to_le_bytes());
    let ppm = pixels_per_metre(dpi);
    out.extend_from_slice(&ppm.to_le_bytes());
    out.extend_from_slice(&ppm.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    for y in (0..h).rev() {
        let start = out.len();
        for x in 0..w {
            let p = img.get_pixel(x, y).0;
            out.extend_from_slice(&[p[2], p[1], p[0]]);
        }
        out.resize(start + stride, 0);
    }
    Ok(out)
}

/// BT.601-ish luma used by every gray / bilevel conversion here.
pub(crate) fn luma8(p: &image::Rgb<u8>) -> u8 {
    ((u32::from(p[0]) * 77 + u32::from(p[1]) * 150 + u32::from(p[2]) * 29) >> 8) as u8
}

/// Row stride in bytes for `bits` per pixel, padded to `align` bytes.
pub fn stride_bytes(width_px: u32, bits: u32, align: u32) -> u32 {
    let raw = (width_px.saturating_mul(bits)).div_ceil(8);
    let a = align.max(1);
    raw.div_ceil(a).saturating_mul(a)
}

/// Pack `img` top-down. `bits` 8: one luma byte per pixel. `bits` 1: MSB-first,
/// bit 1 = white when `polarity` is black-is-zero, bit 1 = black otherwise.
/// Rows are padded with zero bits/bytes to `align`.
fn pack_rows(
    img: &RgbImage,
    bits: u32,
    polarity: crate::effects::BilevelPolarity,
    align: u32,
) -> Vec<u8> {
    let (w, h) = img.dimensions();
    let stride = stride_bytes(w, bits, align) as usize;
    let mut out = vec![0u8; stride * h as usize];
    let white_is_one = polarity == crate::effects::BilevelPolarity::BlackIsZero;
    for y in 0..h {
        let row = &mut out[y as usize * stride..(y as usize + 1) * stride];
        for x in 0..w {
            let l = luma8(img.get_pixel(x, y));
            if bits == 8 {
                row[x as usize] = l;
            } else {
                let white = l >= BW_THRESHOLD;
                if white == white_is_one {
                    row[(x / 8) as usize] |= 0x80 >> (x % 8);
                }
            }
        }
    }
    out
}

/// Real 1-bpp / 8-bpp BMP (BITMAPINFOHEADER, palette, rows padded to 4). 1-bpp is
/// top-down (negative biHeight, as the Brother serves it), 8-bpp bottom-up.
///
/// 1-bpp palette is `[black, white]` (black-is-zero) or `[white, black]`
/// (white-is-zero); 8-bpp is a 256-entry gray ramp.
fn encode_bmp_indexed(
    img: &RgbImage,
    bits: u32,
    polarity: crate::effects::BilevelPolarity,
    dpi: u32,
) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        bail!("empty image for bmp");
    }
    let palette: Vec<[u8; 4]> = if bits == 1 {
        let (a, b) = match polarity {
            crate::effects::BilevelPolarity::BlackIsZero => (0u8, 255u8),
            crate::effects::BilevelPolarity::WhiteIsZero => (255u8, 0u8),
        };
        vec![[a, a, a, 0], [b, b, b, 0]]
    } else {
        (0..=255u8).map(|v| [v, v, v, 0]).collect()
    };
    let top_down = pack_rows(img, bits, polarity, 4);
    let stride = stride_bytes(w, bits, 4) as usize;
    let pix_len = top_down.len();
    let off_bits = 14 + 40 + palette.len() * 4;
    let mut out = Vec::with_capacity(off_bits + pix_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((off_bits + pix_len) as u32).to_le_bytes());
    out.extend_from_slice(&[0u8; 4]);
    out.extend_from_slice(&(off_bits as u32).to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    // Brother serves its 1-bpp BMPs top-down (negative biHeight, BI_RGB is legal there);
    // 8-bpp gray stays bottom-up.
    let top_down_file = bits == 1;
    let height_field = if top_down_file { -(h as i32) } else { h as i32 };
    out.extend_from_slice(&height_field.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&(bits as u16).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // BI_RGB
    out.extend_from_slice(&(pix_len as u32).to_le_bytes());
    let ppm = pixels_per_metre(dpi);
    out.extend_from_slice(&ppm.to_le_bytes());
    out.extend_from_slice(&ppm.to_le_bytes());
    out.extend_from_slice(&(palette.len() as u32).to_le_bytes());
    out.extend_from_slice(&(palette.len() as u32).to_le_bytes());
    for e in &palette {
        out.extend_from_slice(e);
    }
    if top_down_file {
        out.extend_from_slice(&top_down);
    } else {
        for row in top_down.chunks(stride).rev() {
            out.extend_from_slice(row);
        }
    }
    Ok(out)
}

/// Write a PNG with a `pHYs` chunk (pixels per metre) for `dpi`.
fn png_write(
    w: u32,
    h: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    dpi: u32,
    data: &[u8],
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        let ppm = pixels_per_metre(dpi);
        enc.set_pixel_dims(Some(png::PixelDimensions {
            xppu: ppm,
            yppu: ppm,
            unit: png::Unit::Meter,
        }));
        let mut wr = enc
            .write_header()
            .map_err(|e| anyhow::anyhow!("png header: {e}"))?;
        wr.write_image_data(data)
            .map_err(|e| anyhow::anyhow!("png data: {e}"))?;
    }
    if out.is_empty() {
        bail!("empty png");
    }
    Ok(out)
}

/// 1-bit or 8-bit grayscale PNG (no polarity: PNG gray 0 is always black).
fn encode_png_gray(img: &RgbImage, bits: u32, dpi: u32) -> Result<Vec<u8>> {
    let (w, h) = img.dimensions();
    let data = pack_rows(img, bits, crate::effects::BilevelPolarity::BlackIsZero, 1);
    let depth = if bits == 1 {
        png::BitDepth::One
    } else {
        png::BitDepth::Eight
    };
    png_write(w, h, png::ColorType::Grayscale, depth, dpi, &data)
}

/// Uncompressed 1-bit (Bw1) or 8-bit gray TIFF, one IFD per page, single strip.
///
/// 1-bit: `Photometric` = 1 (MinIsBlack) for black-is-zero, 0 (MinIsWhite) for
/// white-is-zero, with the bits stored to match so the visual is unchanged.
fn encode_tiff_indexed(imgs: &[&RgbImage], spec: PixelSpec, dpi: u32) -> Result<Vec<u8>> {
    use crate::effects::{BilevelPolarity, ColorMode};
    if imgs.is_empty() {
        bail!("no tiff pages");
    }
    let bits: u32 = if spec.color == ColorMode::Bw1 { 1 } else { 8 };
    let photometric: u32 = match (bits, spec.polarity) {
        (1, BilevelPolarity::WhiteIsZero) => 0,
        _ => 1,
    };
    let pages: Vec<(Vec<u8>, u32, u32)> = imgs
        .iter()
        .map(|img| {
            let (w, h) = img.dimensions();
            if w == 0 || h == 0 {
                bail!("empty image for tiff");
            }
            Ok((pack_rows(img, bits, spec.polarity, 1), w, h))
        })
        .collect::<Result<_>>()?;
    let n = pages.len();
    const ENTRIES: usize = 13;
    let ifd_bytes = 2 + 12 * ENTRIES + 4;
    let rat_bytes = 16;
    let header = 8usize;
    let mut cursor = header + n * (ifd_bytes + rat_bytes);
    let mut offsets = Vec::with_capacity(n);
    for (data, _, _) in &pages {
        offsets.push(cursor as u32);
        cursor += data.len();
    }
    let mut out = Vec::with_capacity(cursor);
    out.extend_from_slice(&[b'I', b'I', 42, 0]);
    out.extend_from_slice(&(header as u32).to_le_bytes());
    for (i, (data, w, h)) in pages.iter().enumerate() {
        let rat_at = out.len() + ifd_bytes;
        let next = if i + 1 < n {
            (rat_at + rat_bytes) as u32
        } else {
            0
        };
        out.extend_from_slice(&(ENTRIES as u16).to_le_bytes());
        let tags: [(u16, u16, u32, u32); ENTRIES] = [
            (256, 4, 1, *w),
            (257, 4, 1, *h),
            (258, 3, 1, bits),
            (259, 3, 1, 1), // uncompressed
            (262, 3, 1, photometric),
            (273, 4, 1, offsets[i]),
            (274, 3, 1, 1),
            (277, 3, 1, 1), // SamplesPerPixel
            (278, 4, 1, *h),
            (279, 4, 1, data.len() as u32),
            (282, 5, 1, rat_at as u32),
            (283, 5, 1, (rat_at + 8) as u32),
            (296, 3, 1, 2),
        ];
        for (tag, typ, count, val) in tags {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        }
        out.extend_from_slice(&next.to_le_bytes());
        for _ in 0..2 {
            out.extend_from_slice(&dpi.max(1).to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        }
    }
    for (data, _, _) in &pages {
        out.extend_from_slice(data);
    }
    Ok(out)
}

fn encode_png(img: RgbImage, dpi: u32) -> Result<Vec<u8>> {
    png_write(
        img.width(),
        img.height(),
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        dpi,
        img.as_raw(),
    )
}

fn encode_tiff(img: RgbImage, dpi: u32) -> Result<Vec<u8>> {
    encode_tiff_multi_uncompressed(&[&img], dpi)
}

fn encode_tiff_multi_uncompressed(imgs: &[&RgbImage], dpi: u32) -> Result<Vec<u8>> {
    use std::io::Cursor;
    let mut out = Vec::new();
    {
        let mut cursor = Cursor::new(&mut out);
        let mut enc = tiff::encoder::TiffEncoder::new(&mut cursor)
            .map_err(|e| anyhow::anyhow!("tiff: {e}"))?;
        for img in imgs {
            let mut page = enc
                .new_image::<tiff::encoder::colortype::RGB8>(img.width(), img.height())
                .map_err(|e| anyhow::anyhow!("tiff page: {e}"))?;
            page.resolution(
                tiff::tags::ResolutionUnit::Inch,
                tiff::encoder::Rational {
                    n: dpi.max(1),
                    d: 1,
                },
            );
            page.write_data(img.as_raw())
                .map_err(|e| anyhow::anyhow!("tiff page: {e}"))?;
        }
    }
    if out.is_empty() {
        bail!("empty tiff");
    }
    Ok(out)
}

/// CCITT Group 4 (WSD `tiff-*-g4`). Threshold RGB → bilevel, wrap with [`fax::tiff`].
fn encode_tiff_g4(imgs: &[&RgbImage], dpi: u32) -> Result<Vec<u8>> {
    if imgs.is_empty() {
        bail!("empty g4 tiff");
    }
    // Own IFD writer (not `fax::tiff::wrap`) so XResolution/YResolution carry the real dpi.
    wrap_g4_multipage(
        &imgs
            .iter()
            .map(|img| {
                let data = encode_g4_strip(img)?;
                Ok((data, img.width(), img.height()))
            })
            .collect::<Result<Vec<_>>>()?,
        dpi,
    )
}

fn encode_g4_strip(img: &RgbImage) -> Result<Vec<u8>> {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        bail!("empty image for g4");
    }
    if w > u16::MAX as u32 {
        bail!("image width {w} exceeds G4 u16 limit");
    }
    let width = w as u16;
    let writer = fax::VecWriter::with_capacity(((w * h) / 8 + 64) as usize);
    let mut enc = fax::encoder::Encoder::new(writer);
    for y in 0..h {
        let colors = (0..w).map(|x| {
            let p = img.get_pixel(x, y).0;
            let luma = (u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2])) / 3;
            if luma < 128 {
                fax::Color::Black
            } else {
                fax::Color::White
            }
        });
        enc.encode_line(colors, width)
            .map_err(|_| anyhow::anyhow!("g4 encode line {y}"))?;
    }
    let writer = enc.finish().map_err(|_| anyhow::anyhow!("g4 finish"))?;
    Ok(writer.finish())
}

/// CCITT T.4 Group 3 1-D Modified Huffman (WSD `tiff-single-g3mh`).
fn encode_tiff_g3mh(imgs: &[&RgbImage], dpi: u32) -> Result<Vec<u8>> {
    if imgs.is_empty() {
        bail!("empty g3mh tiff");
    }
    let pages: Vec<(Vec<u8>, u32, u32)> = imgs
        .iter()
        .map(|img| {
            let data = encode_g3mh_strip(img)?;
            Ok((data, img.width(), img.height()))
        })
        .collect::<Result<_>>()?;
    wrap_bilevel_tiff_multipage(
        &pages,
        /* compression */ 3,
        /* t4_options */ Some(0),
        dpi,
    )
}

fn encode_g3mh_strip(img: &RgbImage) -> Result<Vec<u8>> {
    use fax::maps::{black, white, EOL};
    use fax::{BitWriter, Color, VecWriter};

    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        bail!("empty image for g3mh");
    }
    if w > u16::MAX as u32 {
        bail!("image width {w} exceeds G3 u16 limit");
    }
    let width = w as u16;
    let mut writer = VecWriter::with_capacity(((w * h) / 8 + 64) as usize);

    let write_run = |writer: &mut VecWriter, color: Color, mut n: u16| -> Result<()> {
        let encode = |n: u16| match color {
            Color::White => white::encode(n),
            Color::Black => black::encode(n),
        };
        while n >= 2560 {
            let bits = encode(2560).ok_or_else(|| anyhow::anyhow!("g3 makeup"))?;
            writer
                .write(bits)
                .map_err(|_| anyhow::anyhow!("g3 write"))?;
            n -= 2560;
        }
        if n >= 64 {
            let d = n & !63;
            let bits = encode(d).ok_or_else(|| anyhow::anyhow!("g3 makeup"))?;
            writer
                .write(bits)
                .map_err(|_| anyhow::anyhow!("g3 write"))?;
            n -= d;
        }
        let bits = encode(n).ok_or_else(|| anyhow::anyhow!("g3 term"))?;
        writer
            .write(bits)
            .map_err(|_| anyhow::anyhow!("g3 write"))?;
        Ok(())
    };

    for y in 0..h {
        // Optional EOL before each line (common for T.4); start with white runs.
        writer.write(EOL).map_err(|_| anyhow::anyhow!("g3 eol"))?;
        let mut run_color = Color::White;
        let mut run_len: u16 = 0;
        for x in 0..width {
            let p = img.get_pixel(u32::from(x), y).0;
            let luma = (u16::from(p[0]) + u16::from(p[1]) + u16::from(p[2])) / 3;
            let c = if luma < 128 {
                Color::Black
            } else {
                Color::White
            };
            if c == run_color {
                run_len = run_len.saturating_add(1);
            } else {
                write_run(&mut writer, run_color, run_len)?;
                run_color = c;
                run_len = 1;
            }
        }
        write_run(&mut writer, run_color, run_len)?;
    }
    // RTC: six EOLs
    for _ in 0..6 {
        writer.write(EOL).map_err(|_| anyhow::anyhow!("g3 rtc"))?;
    }
    Ok(writer.finish())
}

/// TIFF JPEG Technical Note #2 (`Compression=7`): each strip is a full JFIF stream.
///
/// Colour pages are 4:2:0 YCbCr JPEG, so the IFD says `Photometric=YCbCr` (6) with
/// `YCbCrSubSampling=[2,2]` (TN2 forbids RGB photometric around a YCbCr stream; libtiff
/// rejects it and WIC decodes wrong colours). Gray / bilevel pages (`gray`) are a one-component
/// stream with `Photometric=BlackIsZero`, `SamplesPerPixel=1`.
fn encode_tiff_jpeg_tn2(
    pages: &[(RgbImage, u32, Option<PageSide>)],
    jpeg_quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    gray: bool,
) -> Result<Vec<u8>> {
    let mut strips = Vec::with_capacity(pages.len());
    for (img, idx, side) in pages {
        let jpeg = encode_jpeg_ex(img.clone(), *idx, *side, jpeg_quality, opts, dpi, gray)?;
        strips.push((jpeg, img.width(), img.height()));
    }
    wrap_jpeg_tiff_tn2(&strips, dpi, gray)
}

fn wrap_jpeg_tiff_tn2(pages: &[(Vec<u8>, u32, u32)], dpi: u32, gray: bool) -> Result<Vec<u8>> {
    if pages.is_empty() {
        bail!("empty jpeg-tn2 tiff");
    }
    // Per-IFD out-of-line block: [BitsPerSample 3xSHORT (colour only)] + X/Y resolution rationals.
    let n = pages.len();
    let entries_per = if gray { 14usize } else { 16usize };
    let ifd_bytes = 2 + 12 * entries_per + 4;
    let bps_bytes = if gray { 0 } else { 6 };
    let rat_bytes = 16;
    let block = bps_bytes + rat_bytes;
    let header = 8usize;
    let ifd_block = n * (ifd_bytes + block);
    let mut strip_offsets = Vec::with_capacity(n);
    let mut cursor = header + ifd_block;
    for (data, _, _) in pages {
        strip_offsets.push(cursor as u32);
        cursor += data.len();
    }

    let mut out = Vec::with_capacity(cursor);
    out.extend_from_slice(&[b'I', b'I', 42, 0]);
    let mut next_ifd = header as u32;
    out.extend_from_slice(&next_ifd.to_le_bytes());

    for (i, (data, width, height)) in pages.iter().enumerate() {
        let ifd_at = out.len();
        assert_eq!(ifd_at, next_ifd as usize);
        let bps_at = ifd_at + ifd_bytes;
        let rat_at = bps_at + bps_bytes;
        let next = if i + 1 < n {
            (rat_at + rat_bytes) as u32
        } else {
            0
        };
        out.extend_from_slice(&(entries_per as u16).to_le_bytes());
        let mut tags: Vec<(u16, u16, u32, u32)> = vec![
            (256, 4, 1, *width),  // ImageWidth
            (257, 4, 1, *height), // ImageLength
            if gray {
                (258, 3, 1, 8) // BitsPerSample 8 (inline)
            } else {
                (258, 3, 3, bps_at as u32) // BitsPerSample -> [8,8,8]
            },
            (259, 3, 1, 7),                        // Compression JPEG TN2
            (262, 3, 1, if gray { 1 } else { 6 }), // BlackIsZero / YCbCr
            (273, 4, 1, strip_offsets[i]),         // StripOffsets
            (274, 3, 1, 1),                        // Orientation
            (277, 3, 1, if gray { 1 } else { 3 }), // SamplesPerPixel
            (278, 4, 1, *height),                  // RowsPerStrip
            (279, 4, 1, data.len() as u32),        // StripByteCounts
            (282, 5, 1, rat_at as u32),            // XResolution
            (283, 5, 1, (rat_at + 8) as u32),      // YResolution
            (284, 3, 1, 1),                        // PlanarConfiguration chunky
            (296, 3, 1, 2),                        // ResolutionUnit inch
        ];
        if !gray {
            tags.push((530, 3, 2, 2 | (2 << 16))); // YCbCrSubSampling [2,2] = the 4:2:0 stream
            tags.push((531, 3, 1, 1)); // YCbCrPositioning centred (JPEG default)
        }
        debug_assert_eq!(tags.len(), entries_per);
        for (tag, typ, count, val) in tags {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        }
        out.extend_from_slice(&next.to_le_bytes());
        if !gray {
            for _ in 0..3 {
                out.extend_from_slice(&8u16.to_le_bytes());
            }
        }
        for _ in 0..2 {
            out.extend_from_slice(&dpi.max(1).to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        }
        next_ifd = next;
    }
    for (data, _, _) in pages {
        out.extend_from_slice(data);
    }
    Ok(out)
}

/// JP2; `gray` encodes one greyscale component (luma = red of the already-gray buffer).
fn encode_jpeg2k(img: &RgbImage, gray: bool) -> Result<Vec<u8>> {
    use openjp2::{ColorSpace, EncodeOptions, Encoder, Format, Image};

    let image = if gray {
        let luma: Vec<u8> = img
            .as_raw()
            .as_chunks::<3>()
            .0
            .iter()
            .map(|p| p[0])
            .collect();
        Image::from_interleaved_u8(img.width(), img.height(), ColorSpace::Greyscale, 1, &luma)
    } else {
        Image::from_interleaved_u8(img.width(), img.height(), ColorSpace::Srgb, 3, img.as_raw())
    }
    .map_err(|e| anyhow::anyhow!("jpeg2k image: {e:?}"))?;
    let opts = EncodeOptions {
        format: Format::Jp2,
        irreversible: true,
        use_mct: !gray,
        rates: vec![20.0],
        ..EncodeOptions::default()
    };
    Encoder::encode(&image, &opts).map_err(|e| anyhow::anyhow!("jpeg2k encode: {e:?}"))
}

/// Multi-page G4 TIFF (II* little-endian), Photometric WhiteIsZero, Compression=4.
fn wrap_g4_multipage(pages: &[(Vec<u8>, u32, u32)], dpi: u32) -> Result<Vec<u8>> {
    wrap_bilevel_tiff_multipage(pages, 4, None, dpi)
}

fn wrap_bilevel_tiff_multipage(
    pages: &[(Vec<u8>, u32, u32)],
    compression: u32,
    t4_options: Option<u32>,
    dpi: u32,
) -> Result<Vec<u8>> {
    if pages.is_empty() {
        bail!("no bilevel tiff pages");
    }
    let n = pages.len();
    let extra_t4 = usize::from(t4_options.is_some());
    let entries_per = 11 + extra_t4;
    let ifd_bytes = 2 + 12 * entries_per + 4;
    let rat_bytes = 16;
    let header = 8usize;
    let ifd_block = n * (ifd_bytes + rat_bytes);
    let mut strip_offsets = Vec::with_capacity(n);
    let mut cursor = header + ifd_block;
    for (data, _, _) in pages {
        strip_offsets.push(cursor as u32);
        cursor += data.len();
    }

    let mut out = Vec::with_capacity(cursor);
    out.extend_from_slice(&[b'I', b'I', 42, 0]);
    let mut next_ifd = header as u32;
    out.extend_from_slice(&next_ifd.to_le_bytes());

    for (i, (data, width, height)) in pages.iter().enumerate() {
        let ifd_at = out.len();
        assert_eq!(ifd_at, next_ifd as usize);
        let rat_at = ifd_at + ifd_bytes;
        let next = if i + 1 < n {
            (rat_at + rat_bytes) as u32
        } else {
            0
        };
        out.extend_from_slice(&(entries_per as u16).to_le_bytes());
        let mut tags: Vec<(u16, u16, u32, u32)> = vec![
            (256, 4, 1, *width),
            (257, 4, 1, *height),
            (259, 3, 1, compression),
            (262, 3, 1, 0), // WhiteIsZero
            (273, 4, 1, strip_offsets[i]),
            (274, 3, 1, 1),
            (278, 4, 1, *height),
            (279, 4, 1, data.len() as u32),
            (282, 5, 1, rat_at as u32),
            (283, 5, 1, (rat_at + 8) as u32),
            (296, 3, 1, 2),
        ];
        if let Some(opt) = t4_options {
            // T4Options must stay sorted by tag number (292) among entries.
            tags.push((292, 4, 1, opt));
            tags.sort_by_key(|(tag, _, _, _)| *tag);
        }
        for (tag, typ, count, val) in tags {
            out.extend_from_slice(&tag.to_le_bytes());
            out.extend_from_slice(&typ.to_le_bytes());
            out.extend_from_slice(&count.to_le_bytes());
            out.extend_from_slice(&val.to_le_bytes());
        }
        out.extend_from_slice(&next.to_le_bytes());
        for _ in 0..2 {
            out.extend_from_slice(&dpi.max(1).to_le_bytes());
            out.extend_from_slice(&1u32.to_le_bytes());
        }
        next_ifd = next;
    }
    for (data, _, _) in pages {
        out.extend_from_slice(data);
    }
    Ok(out)
}

/// JFIF with a minimal EXIF APP1 (WSD `exif` / WIA EXIF transfer).
fn encode_exif_jpeg(
    img: RgbImage,
    page_index: u32,
    side: Option<PageSide>,
    jpeg_quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    gray: bool,
) -> Result<Vec<u8>> {
    let mut jpeg = encode_jpeg_ex(img, page_index, side, jpeg_quality, opts, dpi, gray)?;
    inject_exif_app1(&mut jpeg)?;
    Ok(jpeg)
}

/// Minimal TIFF IFD APP1 so the blob is recognizably EXIF, not bare JFIF.
fn inject_exif_app1(jpeg: &mut Vec<u8>) -> Result<()> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        bail!("exif inject: not a jpeg");
    }
    // APP1: "Exif\0\0" + little-endian TIFF with empty IFD (count=0).
    let tiff: [u8; 8] = [
        b'I', b'I', // little-endian
        0x2A, 0x00, // magic 42
        0x08, 0x00, 0x00, 0x00, // IFD0 offset
    ];
    // IFD: entry count 0 + next-IFD offset 0.
    let ifd: [u8; 6] = [0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
    let mut payload = Vec::with_capacity(6 + tiff.len() + ifd.len());
    payload.extend_from_slice(b"Exif\0\0");
    payload.extend_from_slice(&tiff);
    payload.extend_from_slice(&ifd);
    let segment_len = (payload.len() + 2) as u16; // length includes itself
    let mut app1 = Vec::with_capacity(4 + payload.len());
    app1.extend_from_slice(&[0xFF, 0xE1]);
    app1.extend_from_slice(&segment_len.to_be_bytes());
    app1.extend_from_slice(&payload);
    jpeg.splice(2..2, app1);
    Ok(())
}

/// PDF with embedded JPEG page(s) (WSD `pdf-a`). Valid PDF; not certified PDF/A-1b.
fn encode_pdf_pages(
    pages: &[(RgbImage, u32, Option<PageSide>)],
    jpeg_quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    gray: bool,
) -> Result<Vec<u8>> {
    let mut jpegs = Vec::with_capacity(pages.len());
    for (img, idx, side) in pages {
        let w = img.width();
        let h = img.height();
        let jpeg = encode_jpeg_ex(img.clone(), *idx, *side, jpeg_quality, opts, dpi, gray)?;
        jpegs.push((jpeg, w, h));
    }
    wrap_jpegs_as_pdf(&jpegs, dpi, gray)
}

/// `value` rounded to 2 decimals with trailing zeros trimmed (`612`, `841.89`).
fn fmt_units(value: f64) -> String {
    let t = format!("{value:.2}");
    t.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// One DCT-coded JPEG image per page. MediaBox is the physical page in points
/// (`pixels * 72 / dpi`); `gray` pages use `/DeviceGray` (one-component stream).
fn wrap_jpegs_as_pdf(pages: &[(Vec<u8>, u32, u32)], dpi: u32, gray: bool) -> Result<Vec<u8>> {
    if pages.is_empty() {
        bail!("empty pdf");
    }
    // Obj 1 Catalog, 2 Pages, then per page: Page, Contents, Image (3 objs each).
    let n = pages.len();
    let total_objs = 2 + 3 * n;
    let mut body = Vec::new();
    let mut offsets = vec![0usize; total_objs + 1];

    let write_obj = |out: &mut Vec<u8>, offsets: &mut [usize], num: usize, dict: &[u8]| {
        offsets[num] = out.len();
        out.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
        out.extend_from_slice(dict);
        if !dict.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"endobj\n");
    };

    body.extend_from_slice(b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n");
    write_obj(
        &mut body,
        &mut offsets,
        1,
        b"<< /Type /Catalog /Pages 2 0 R >>",
    );

    let kids: String = (0..n).map(|i| format!("{} 0 R ", 3 + 3 * i)).collect();
    write_obj(
        &mut body,
        &mut offsets,
        2,
        format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>").as_bytes(),
    );

    let cs = if gray { "/DeviceGray" } else { "/DeviceRGB" };
    for (i, (jpeg, width, height)) in pages.iter().enumerate() {
        let pt_w = fmt_units(f64::from(*width) * 72.0 / f64::from(dpi.max(1)));
        let pt_h = fmt_units(f64::from(*height) * 72.0 / f64::from(dpi.max(1)));
        let page_obj = 3 + 3 * i;
        let content_obj = page_obj + 1;
        let image_obj = page_obj + 2;
        write_obj(
            &mut body,
            &mut offsets,
            page_obj,
            format!(
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 {pt_w} {pt_h}] \
                 /Contents {content_obj} 0 R /Resources << /XObject << /Im0 {image_obj} 0 R >> >> >>"
            )
            .as_bytes(),
        );
        let content = format!("q\n{pt_w} 0 0 {pt_h} 0 0 cm\n/Im0 Do\nQ\n");
        offsets[content_obj] = body.len();
        body.extend_from_slice(format!("{content_obj} 0 obj\n").as_bytes());
        body.extend_from_slice(format!("<< /Length {} >>\nstream\n", content.len()).as_bytes());
        body.extend_from_slice(content.as_bytes());
        body.extend_from_slice(b"endstream\nendobj\n");

        offsets[image_obj] = body.len();
        body.extend_from_slice(format!("{image_obj} 0 obj\n").as_bytes());
        body.extend_from_slice(
            format!(
                "<< /Type /XObject /Subtype /Image /Width {width} /Height {height} \
                 /ColorSpace {cs} /BitsPerComponent 8 /Filter /DCTDecode \
                 /Length {} >>\nstream\n",
                jpeg.len()
            )
            .as_bytes(),
        );
        body.extend_from_slice(jpeg);
        body.extend_from_slice(b"\nendstream\nendobj\n");
    }

    let xref_at = body.len();
    body.extend_from_slice(format!("xref\n0 {}\n", total_objs + 1).as_bytes());
    body.extend_from_slice(b"0000000000 65535 f \n");
    for offset in offsets.iter().take(total_objs + 1).skip(1) {
        body.extend_from_slice(format!("{:010} 00000 n \n", offset).as_bytes());
    }
    body.extend_from_slice(
        format!(
            "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
            total_objs + 1
        )
        .as_bytes(),
    );
    Ok(body)
}

/// Minimal OpenXPS package: one FixedDocument, JPEG resources per page.
fn encode_xps_pages(
    pages: &[(RgbImage, u32, Option<PageSide>)],
    jpeg_quality: u8,
    opts: EncodeOpts,
    dpi: u32,
    gray: bool,
) -> Result<Vec<u8>> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::CompressionMethod;

    let mut jpegs = Vec::with_capacity(pages.len());
    for (img, idx, side) in pages {
        jpegs.push(encode_jpeg_ex(
            img.clone(),
            *idx,
            *side,
            jpeg_quality,
            opts,
            dpi,
            gray,
        )?);
    }
    let n = jpegs.len();
    if n == 0 {
        bail!("empty xps");
    }

    let mut buf = Cursor::new(Vec::new());
    {
        let mut zip = zip::ZipWriter::new(&mut buf);
        let opts = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);

        zip.start_file("[Content_Types].xml", opts)
            .map_err(|e| anyhow::anyhow!("xps: {e}"))?;
        let mut types = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="fdseq" ContentType="application/vnd.ms-package.xps-fixeddocumentsequence+xml"/>
  <Default Extension="fdoc" ContentType="application/vnd.ms-package.xps-fixeddocument+xml"/>
  <Default Extension="fpage" ContentType="application/vnd.ms-package.xps-fixedpage+xml"/>
  <Default Extension="jpg" ContentType="image/jpeg"/>
"#,
        );
        types.push_str("</Types>");
        zip.write_all(types.as_bytes())?;

        zip.start_file("_rels/.rels", opts)?;
        zip.write_all(
            br#"<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/xps/2005/06/fixedrepresentation" Target="/FixedDocumentSequence.fdseq" Id="R0"/>
</Relationships>"#,
        )?;

        zip.start_file("FixedDocumentSequence.fdseq", opts)?;
        zip.write_all(
            br#"<?xml version="1.0" encoding="utf-8"?>
<FixedDocumentSequence xmlns="http://schemas.microsoft.com/xps/2005/06">
  <DocumentReference Source="Documents/1/FixedDocument.fdoc"/>
</FixedDocumentSequence>"#,
        )?;

        zip.start_file("Documents/1/FixedDocument.fdoc", opts)?;
        let mut fdoc = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<FixedDocument xmlns="http://schemas.microsoft.com/xps/2005/06">
"#,
        );
        for i in 1..=n {
            fdoc.push_str(&format!("  <PageContent Source=\"Pages/{i}.fpage\"/>\n"));
        }
        fdoc.push_str("</FixedDocument>");
        zip.write_all(fdoc.as_bytes())?;

        zip.start_file("Documents/1/_rels/FixedDocument.fdoc.rels", opts)?;
        let mut rels = String::from(
            r#"<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
"#,
        );
        for i in 1..=n {
            rels.push_str(&format!(
                r#"  <Relationship Type="http://schemas.microsoft.com/xps/2005/06/required-resource" Target="Pages/{i}.fpage" Id="P{i}"/>
"#
            ));
        }
        rels.push_str("</Relationships>");
        zip.write_all(rels.as_bytes())?;

        for (i, ((img, _, _), jpeg)) in pages.iter().zip(jpegs.iter()).enumerate() {
            let page = i + 1;
            // XPS user space is 1/96 inch. An image's natural size is pixels * 96 / dpi, so the
            // page, the geometry, Viewbox and Viewport are all that physical size (the JPEG
            // carries the same dpi in APP0).
            let w = fmt_units(f64::from(img.width()) * 96.0 / f64::from(dpi.max(1)));
            let h = fmt_units(f64::from(img.height()) * 96.0 / f64::from(dpi.max(1)));
            zip.start_file(format!("Documents/1/Pages/{page}.fpage"), opts)?;
            let fpage = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<FixedPage xmlns="http://schemas.microsoft.com/xps/2005/06" Width="{w}" Height="{h}" xml:lang="en-US">
  <Path Data="M 0,0 L {w},0 {w},{h} 0,{h} Z">
    <Path.Fill>
      <ImageBrush ImageSource="../Resources/{page}.jpg" Viewbox="0,0,{w},{h}" ViewboxUnits="Absolute" Viewport="0,0,{w},{h}" ViewportUnits="Absolute"/>
    </Path.Fill>
  </Path>
</FixedPage>"#
            );
            zip.write_all(fpage.as_bytes())?;

            zip.start_file(format!("Documents/1/Pages/_rels/{page}.fpage.rels"), opts)?;
            let prel = format!(
                r#"<?xml version="1.0" encoding="utf-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Type="http://schemas.microsoft.com/xps/2005/06/required-resource" Target="../Resources/{page}.jpg" Id="I{page}"/>
</Relationships>"#
            );
            zip.write_all(prel.as_bytes())?;

            zip.start_file(format!("Documents/1/Resources/{page}.jpg"), opts)?;
            zip.write_all(jpeg)?;
        }

        zip.finish()
            .map_err(|e| anyhow::anyhow!("xps finish: {e}"))?;
    }
    Ok(buf.into_inner())
}

/// MIME type for MTOM RetrieveImage binary part.
pub fn mime_for_format(wsd_format: &str) -> &'static str {
    match wsd_format {
        "jfif" | "jpeg" | "jpg" | "exif" => "image/jpeg",
        "dib" | "bmp" => "image/bmp",
        "png" => "image/png",
        "tiff-single-uncompressed"
        | "tiff"
        | "tif"
        | "tiff-single-g4"
        | "tiff-single-g3mh"
        | "tiff-single-jpeg-tn2"
        | "tiff-multi-uncompressed"
        | "tiff-multi-g4"
        | "tiff-multi-g3mh"
        | "tiff-multi-jpeg-tn2" => "image/tiff",
        "jpeg2k" => "image/jp2",
        "pdf-a" | "pdf" => "application/pdf",
        "xps" => "application/vnd.ms-xpsdocument",
        _ => "application/octet-stream",
    }
}

/// `true` when Format is a compressed WSD token (jfif, png, …).
///
/// MS [BytesPerLine remarks](https://learn.microsoft.com/en-us/windows-hardware/drivers/image/bytesperline)
/// say compressed formats should return 0. **Inbox wsdscdrv + Brother MFC do not:**
/// live Brother CreateScanJobResponse for `jfif` returns `BytesPerLine = PixelsPerLine × 3`
/// (lab 2026-10-12: 2480×3440 → 7440). Fake with BPL=0 still CancelJob after RetrieveImage;
/// match Brother's stride for accept experiments.
pub fn format_is_compressed(wsd_format: &str) -> bool {
    !matches!(
        wsd_format,
        "dib" | "bmp" | "tiff-single-uncompressed" | "tiff" | "tif" | "tiff-multi-uncompressed"
    )
}

/// ImageInformation `BytesPerLine` for Validate/CreateScanJob.
///
/// Decoded-bitmap stride for ImageInformation / ValidateScanTicket.
///
/// Brother live **jfif** reports `width×3` (RGB stride) for RGB24. For Grayscale8
/// the default is now the consistent `BPL=width` (live proof 2026-10-12: Windows
/// accepts Gray8 + `BPL=width` and Gray8 + `width×3`, but rejects the old RGB24
/// coercion; see `JfifGray`). Uncompressed formats keep the real color stride.
///
/// When `ms_strict` is true (profile `bytes_per_line_ms`), compressed formats
/// return 0 per MS remarks; default `false` keeps the Brother stride above.
pub fn bytes_per_line(
    wsd_format: &str,
    width_px: u32,
    color: crate::effects::ColorMode,
    ms_strict: bool,
) -> u32 {
    bytes_per_line_fx(
        wsd_format,
        width_px,
        color,
        ms_strict,
        crate::effects::JfifGray::default(),
        crate::effects::JfifBw::default(),
    )
}

/// [`bytes_per_line`] honouring the `jfif_gray` effect: in `gray8` mode (default) a
/// Gray8 JFIF/EXIF reports the true gray stride (`width`); the legacy `rgb24`,
/// `gray8_rgb_stride` and `gray8_tiff` modes keep the `width×3` stride.
pub fn bytes_per_line_fx(
    wsd_format: &str,
    width_px: u32,
    color: crate::effects::ColorMode,
    ms_strict: bool,
    jfif_gray: crate::effects::JfifGray,
    jfif_bw: crate::effects::JfifBw,
) -> u32 {
    if ms_strict && format_is_compressed(wsd_format) {
        return 0;
    }
    let compressed_jfif = matches!(
        wsd_format.to_ascii_lowercase().as_str(),
        "jfif" | "jpeg" | "exif"
    );
    if compressed_jfif {
        if color == crate::effects::ColorMode::Gray8 && jfif_gray == crate::effects::JfifGray::Gray8
        {
            return width_px;
        }
        if color == crate::effects::ColorMode::Bw1 && jfif_bw == crate::effects::JfifBw::Gray8 {
            return width_px;
        }
        // RGB JFIF/EXIF (and the legacy gray modes) use the RGB stride, see jfif_wire_color.
        return width_px.saturating_mul(3);
    }
    // BMP/DIB rows are DWORD-aligned; other uncompressed/decoded strides are byte-packed.
    let align = if matches!(wsd_format.to_ascii_lowercase().as_str(), "dib" | "bmp") {
        4
    } else {
        1
    };
    let bits = match color {
        crate::effects::ColorMode::Rgb24 => 24,
        crate::effects::ColorMode::Gray8 => 8,
        crate::effects::ColorMode::Bw1 => 1,
    };
    stride_bytes(width_px, bits, align)
}

#[derive(Clone, Copy)]
enum QrPlacement {
    BottomRight,
    Center,
    /// Bottom-right corner of the QR at this pixel (x, y).
    AnchorBottomRight(u32, u32),
}

fn qr_corner_size(w: u32, h: u32) -> u32 {
    // ~18% of the short edge, at least 96px so modules survive JPEG.
    let n = ((w.min(h) as f32) * 0.18).round() as u32;
    n.max(96).min(w.min(h) / 2)
}

fn qr_focus_size(w: u32, h: u32) -> u32 {
    let n = ((w.min(h) as f32) * 0.55).round() as u32;
    n.max(160).min(w.min(h) * 4 / 5)
}

fn stamp_qr(img: &mut RgbImage, payload: &str, place: QrPlacement, target: u32) -> Result<()> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)
        .map_err(|e| anyhow::anyhow!("qr encode: {e}"))?;
    let qr: RgbImage = code
        .render::<Rgb<u8>>()
        .dark_color(Rgb([0, 0, 0]))
        .light_color(Rgb([255, 255, 255]))
        .quiet_zone(true)
        .min_dimensions(target, target)
        .build();
    let (qw, qh) = qr.dimensions();
    let (w, h) = img.dimensions();
    if qw > w || qh > h {
        bail!("page too small for QR ({w}x{h}, need {qw}x{qh})");
    }
    let (x0, y0) = match place {
        QrPlacement::BottomRight => {
            let margin = (w.min(h) / 40).max(4);
            (w - qw - margin, h - qh - margin)
        }
        QrPlacement::Center => ((w - qw) / 2, (h - qh) / 2),
        QrPlacement::AnchorBottomRight(ax, ay) => (ax.saturating_sub(qw), ay.saturating_sub(qh)),
    };
    for y in 0..qh {
        for x in 0..qw {
            img.put_pixel(x0 + x, y0 + y, *qr.get_pixel(x, y));
        }
    }
    Ok(())
}

fn draw_qr_focus(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img = ImageBuffer::from_pixel(w, h, Rgb([250, 250, 248]));
    draw_border(&mut img, Rgb([40, 40, 40]));
    // Small page cue in the top margin (QR itself carries the index).
    let ink = Rgb([40, 40, 40]);
    let bars = (page_index + 1).min(12);
    let bar_w = (w / 50).max(3);
    for i in 0..bars {
        let x0 = w / 10 + i * (bar_w + bar_w);
        fill_rect(&mut img, x0, h / 40, x0 + bar_w, h / 40 + bar_w * 2, ink);
    }
    img
}

/// Render multiple lines of glyph text into an image buffer.
///
/// Each character is drawn with [`draw_glyph`] (5×7 pixels × `scale`).
/// Lines that would extend past the image bottom are silently clipped.
fn stamp_text_block(
    img: &mut RgbImage,
    lines: &[&str],
    x0: u32,
    y0: u32,
    scale: u32,
    ink: Rgb<u8>,
) {
    if scale == 0 {
        return;
    }
    let (w, h) = img.dimensions();
    let line_h = 7 * scale + scale; // glyph height + 1-scale gap between lines
    for (i, line) in lines.iter().enumerate() {
        let y = y0.saturating_add(i as u32 * line_h);
        if y + 7 * scale > h {
            break;
        }
        let mut x = x0;
        for ch in line.chars() {
            if x + 5 * scale >= w {
                break;
            }
            let advance = draw_glyph(img, ch, x, y, scale, ink);
            x = x.saturating_add(advance + scale);
        }
    }
}

/// Diagnostic page: server identity, job info, and client peer address.
///
/// Multi-line text is drawn with [`stamp_text_block`] using the existing 5×7 glyphs.
/// The top banner area is left clear for [`stamp_banner`] to overwrite later.
fn draw_info(w: u32, h: u32, page_index: u32, ctx: Option<&PageContext>) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([248, 250, 252]));
    draw_border(&mut img, Rgb([20, 40, 80]));

    // Accent bar below where stamp_banner will appear.
    let scale_b = (w.min(h) / 180).clamp(2, 6);
    let banner_h = 11 * scale_b; // matches stamp_banner band height
    let accent_y = banner_h + scale_b * 2;
    fill_rect(
        &mut img,
        w / 10,
        accent_y,
        w * 9 / 10,
        accent_y + scale_b * 2,
        Rgb([40, 80, 180]),
    );

    // Text scale (slightly smaller so more lines fit).
    let scale = (w.min(h) / 240).clamp(2, 5);
    let ink = Rgb([20, 40, 80]);
    let x0 = w / 20;
    let y0 = banner_h + scale * 6;

    let lines: Vec<String> = if let Some(c) = ctx {
        let peer = c
            .peer
            .as_deref()
            .filter(|p| !p.is_empty() && *p != "-")
            .unwrap_or("-");
        let uuid_short = if c.device_uuid.len() >= 8 {
            &c.device_uuid[..8]
        } else {
            c.device_uuid.as_str()
        };
        vec![
            format!("PROFILE: {}", c.profile_id.to_ascii_uppercase()),
            format!("UUID: {uuid_short}"),
            format!(
                "PATTERN: {}  DPI:{}",
                c.pattern_id.to_ascii_uppercase(),
                c.dpi
            ),
            format!(
                "FORMAT: {}  JOB:{}",
                c.format.to_ascii_uppercase(),
                c.job_id
            ),
            format!("PAGE: {page_index}"),
            format!("CLIENT: {peer}"),
            format!("TIME: {}", c.timestamp_secs),
        ]
    } else {
        vec![
            "FAKESCAN INFO".into(),
            "PROFILE: -".into(),
            "NO CONTEXT".into(),
        ]
    };
    let line_refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
    stamp_text_block(&mut img, &line_refs, x0, y0, scale, ink);
    img
}

/// ANAC-style page: displays the caller's IP address prominently.
///
/// The peer address is rendered in large glyphs centred on the page, suitable
/// for reading on a phone camera without OCR.  See `ANAC` (Automatic Number
/// Announcement Circuit) for the inspiration.
fn draw_anac(w: u32, h: u32, page_index: u32, ctx: Option<&PageContext>) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([245, 252, 248]));
    draw_border(&mut img, Rgb([20, 80, 40]));

    let scale_b = (w.min(h) / 180).clamp(2, 6);
    let banner_h = 11 * scale_b;
    let accent_y = banner_h + scale_b * 2;
    fill_rect(
        &mut img,
        w / 10,
        accent_y,
        w * 9 / 10,
        accent_y + scale_b * 2,
        Rgb([40, 140, 60]),
    );

    let ink_dark = Rgb([20, 60, 20]);
    let ink_mid = Rgb([60, 90, 60]);

    let peer = ctx
        .and_then(|c| c.peer.as_deref())
        .filter(|p| !p.is_empty() && *p != "-")
        .unwrap_or("-");

    // "YOUR IP" header in medium text, horizontally centred.
    let scale_med = (w.min(h) / 280).clamp(2, 4);
    let label = "YOUR IP";
    let label_px_w = label.len() as u32 * (5 * scale_med + scale_med);
    let label_x = w.saturating_sub(label_px_w) / 2;
    let label_y = banner_h + scale_med * 10;
    stamp_text_block(&mut img, &[label], label_x, label_y, scale_med, ink_mid);

    // IP address in large text, horizontally centred.
    let scale_lg = (w.min(h) / 110).clamp(3, 8);
    let ip_px_w = peer.len() as u32 * (5 * scale_lg + scale_lg);
    let ip_x = w.saturating_sub(ip_px_w) / 2;
    let ip_y = h / 3;
    stamp_text_block(&mut img, &[peer], ip_x, ip_y, scale_lg, ink_dark);

    // Page indicator at bottom in small text.
    let scale_sm = (w.min(h) / 320).clamp(1, 3);
    let pg_str = format!("PAGE: {page_index}");
    stamp_text_block(
        &mut img,
        &[pg_str.as_str()],
        w / 20,
        h * 4 / 5,
        scale_sm,
        ink_mid,
    );

    img
}

pub(crate) fn fill_rect(img: &mut RgbImage, x0: u32, y0: u32, x1: u32, y1: u32, color: Rgb<u8>) {
    let (w, h) = img.dimensions();
    let x0 = x0.min(w);
    let y0 = y0.min(h);
    let x1 = x1.min(w);
    let y1 = y1.min(h);
    for y in y0..y1 {
        for x in x0..x1 {
            img.put_pixel(x, y, color);
        }
    }
}

pub(crate) fn draw_border(img: &mut RgbImage, color: Rgb<u8>) {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return;
    }
    for x in 0..w {
        img.put_pixel(x, 0, color);
        img.put_pixel(x, h - 1, color);
    }
    for y in 0..h {
        img.put_pixel(0, y, color);
        img.put_pixel(w - 1, y, color);
    }
}

/// Current numbered bars + center block + border.
fn draw_page_id(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([245, 245, 240]));
    let ink = Rgb([20, 40, 80]);
    let accent = Rgb([180, 60, 40]);
    draw_border(&mut img, ink);

    let band = (h / 20).max(4);
    fill_rect(&mut img, w / 10, band * 2, w * 9 / 10, band * 3, accent);

    let bars = (page_index + 1).min(16);
    let bar_w = (w / 40).max(4);
    let gap = bar_w;
    let top = band * 4;
    let bar_h = band * 2;
    for i in 0..bars {
        let x0 = w / 10 + i * (bar_w + gap);
        let x1 = (x0 + bar_w).min(w.saturating_sub(1));
        fill_rect(&mut img, x0, top, x1, (top + bar_h).min(h), ink);
    }

    let mark = 8 + (page_index % 7) * 12;
    let cx = w / 2;
    let cy = h / 2;
    fill_rect(
        &mut img,
        cx.saturating_sub(mark),
        cy.saturating_sub(mark),
        (cx + mark).min(w),
        (cy + mark).min(h),
        ink,
    );
    img
}

/// Horizontal gray step wedges (black → white). Page index shifts the step count slightly.
fn draw_wedges(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([128, 128, 128]));
    let steps = (8 + (page_index % 5)).max(4);
    let step_h = (h / steps).max(1);
    for i in 0..steps {
        let t = if steps <= 1 {
            255u8
        } else {
            ((i * 255) / (steps - 1)) as u8
        };
        let y0 = i * step_h;
        let y1 = if i + 1 == steps {
            h
        } else {
            ((i + 1) * step_h).min(h)
        };
        fill_rect(&mut img, 0, y0, w, y1, Rgb([t, t, t]));
    }
    // Narrow ink border so the frame is visible on mid-gray steps.
    draw_border(&mut img, Rgb([0, 0, 0]));
    img
}

/// R / G / B / gray patches in a 2×2 grid (page index tints the gray cell).
fn draw_patches(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([240, 240, 240]));
    let mid_x = w / 2;
    let mid_y = h / 2;
    let gray = (40u32 + (page_index % 8) * 24).min(220) as u8;
    fill_rect(&mut img, 0, 0, mid_x, mid_y, Rgb([220, 40, 40])); // R
    fill_rect(&mut img, mid_x, 0, w, mid_y, Rgb([40, 180, 50])); // G
    fill_rect(&mut img, 0, mid_y, mid_x, h, Rgb([40, 80, 220])); // B
    fill_rect(&mut img, mid_x, mid_y, w, h, Rgb([gray, gray, gray]));
    draw_border(&mut img, Rgb([20, 20, 20]));
    // Thin crosshair so patch boundaries survive mild crop.
    fill_rect(
        &mut img,
        mid_x.saturating_sub(1),
        0,
        (mid_x + 1).min(w),
        h,
        Rgb([20, 20, 20]),
    );
    fill_rect(
        &mut img,
        0,
        mid_y.saturating_sub(1),
        w,
        (mid_y + 1).min(h),
        Rgb([20, 20, 20]),
    );
    img
}

/// Large asymmetric F anchored in the top-left corner (orientation cue).
fn draw_slant_f(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([250, 250, 245]));
    let ink = Rgb([15, 15, 15]);
    draw_border(&mut img, ink);

    // Margin grows slightly with page index so pages stay distinct.
    let margin = (w / 12).max(8) + (page_index % 4) * 4;
    let stem_w = (w / 10).max(12);
    let arm_h = (h / 14).max(10);
    let f_h = (h * 2 / 5).max(stem_w * 4);
    let top_arm = (w * 2 / 5).max(stem_w * 3);
    let mid_arm = (w * 3 / 10).max(stem_w * 2); // shorter → asymmetric

    // Vertical stem
    fill_rect(&mut img, margin, margin, margin + stem_w, margin + f_h, ink);
    // Top arm
    fill_rect(
        &mut img,
        margin,
        margin,
        margin + top_arm,
        margin + arm_h,
        ink,
    );
    // Middle arm (shorter, lower third of stem)
    let mid_y = margin + f_h / 3;
    fill_rect(
        &mut img,
        margin,
        mid_y,
        margin + mid_arm,
        mid_y + arm_h,
        ink,
    );
    img
}

/// Blue front / red back faces for manual duplex. Sheet index still drives bar count.
fn draw_duplex_pair(w: u32, h: u32, page_index: u32, side: PageSide) -> RgbImage {
    let (bg, accent, ink) = match side {
        PageSide::Front => (
            Rgb([220, 235, 250]), // cool / blue wash
            Rgb([40, 90, 200]),
            Rgb([15, 35, 90]),
        ),
        PageSide::Back => (
            Rgb([250, 225, 220]), // warm / red wash
            Rgb([200, 50, 40]),
            Rgb([90, 20, 15]),
        ),
    };
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, bg);
    draw_border(&mut img, ink);

    let band = (h / 20).max(4);
    fill_rect(&mut img, w / 10, band * 2, w * 9 / 10, band * 3, accent);

    // Side stripe along the left (front) or right (back) edge — orientation cue after flip.
    let stripe = (w / 25).max(6);
    match side {
        PageSide::Front => fill_rect(&mut img, 0, 0, stripe, h, accent),
        PageSide::Back => fill_rect(&mut img, w.saturating_sub(stripe), 0, w, h, accent),
    }

    let bars = (page_index + 1).min(16);
    let bar_w = (w / 40).max(4);
    let gap = bar_w;
    let top = band * 4;
    let bar_h = band * 2;
    for i in 0..bars {
        let x0 = w / 10 + i * (bar_w + gap);
        let x1 = (x0 + bar_w).min(w.saturating_sub(1));
        fill_rect(&mut img, x0, top, x1, (top + bar_h).min(h), ink);
    }

    let mark = 8 + (page_index % 7) * 12;
    let cx = w / 2;
    let cy = h / 2;
    fill_rect(
        &mut img,
        cx.saturating_sub(mark),
        cy.saturating_sub(mark),
        (cx + mark).min(w),
        (cy + mark).min(h),
        ink,
    );
    img
}

/// L-marks in four corners + center cross for crop / region tests.
fn draw_corners(w: u32, h: u32, page_index: u32) -> RgbImage {
    let mut img: RgbImage = ImageBuffer::from_pixel(w, h, Rgb([248, 248, 242]));
    let ink = Rgb([30, 30, 30]);
    let accent = Rgb([160, 50, 40]);
    let arm = ((w.min(h) / 12).max(16) + (page_index % 3) * 4).min(w.min(h) / 4);
    let thick = (arm / 6).max(3);

    let draw_l = |img: &mut RgbImage, ox: u32, oy: u32, right: bool, down: bool, color: Rgb<u8>| {
        let x_end = if right {
            (ox + arm).min(w)
        } else {
            ox.saturating_sub(arm)
        };
        let y_end = if down {
            (oy + arm).min(h)
        } else {
            oy.saturating_sub(arm)
        };
        let (hx0, hx1) = if right { (ox, x_end) } else { (x_end, ox) };
        let (vy0, vy1) = if down { (oy, y_end) } else { (y_end, oy) };
        // Horizontal leg
        fill_rect(
            img,
            hx0,
            oy.saturating_sub(thick / 2),
            hx1,
            (oy + thick / 2 + 1).min(h),
            color,
        );
        // Vertical leg
        fill_rect(
            img,
            ox.saturating_sub(thick / 2),
            vy0,
            (ox + thick / 2 + 1).min(w),
            vy1,
            color,
        );
    };

    let inset = (w.min(h) / 40).max(4);
    draw_l(&mut img, inset, inset, true, true, ink); // top-left
    draw_l(
        &mut img,
        w.saturating_sub(inset + 1),
        inset,
        false,
        true,
        ink,
    ); // top-right
    draw_l(
        &mut img,
        inset,
        h.saturating_sub(inset + 1),
        true,
        false,
        ink,
    ); // bottom-left
    draw_l(
        &mut img,
        w.saturating_sub(inset + 1),
        h.saturating_sub(inset + 1),
        false,
        false,
        ink,
    ); // bottom-right

    // Center cross
    let cx = w / 2;
    let cy = h / 2;
    let cross = arm;
    fill_rect(
        &mut img,
        cx.saturating_sub(cross),
        cy.saturating_sub(thick / 2),
        (cx + cross).min(w),
        (cy + thick / 2 + 1).min(h),
        accent,
    );
    fill_rect(
        &mut img,
        cx.saturating_sub(thick / 2),
        cy.saturating_sub(cross),
        (cx + thick / 2 + 1).min(w),
        (cy + cross).min(h),
        accent,
    );
    img
}

/// Insert a JPEG COM marker after SOI carrying `fakescan-page:N`.
fn inject_page_com(jpeg: &mut Vec<u8>, page_index: u32) -> Result<()> {
    let payload = format!(
        "{}{}",
        std::str::from_utf8(PAGE_MARKER_PREFIX).unwrap(),
        page_index
    );
    inject_com_payload(jpeg, payload.as_bytes())
}

fn inject_side_com(jpeg: &mut Vec<u8>, side: PageSide) -> Result<()> {
    let payload = format!(
        "{}{}",
        std::str::from_utf8(SIDE_MARKER_PREFIX).unwrap(),
        side.as_str()
    );
    inject_com_payload(jpeg, payload.as_bytes())
}

fn inject_com_payload(jpeg: &mut Vec<u8>, payload_bytes: &[u8]) -> Result<()> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        bail!("not a JPEG (missing SOI)");
    }
    let segment_len = 2 + payload_bytes.len(); // length field includes itself
    if segment_len > u16::MAX as usize {
        bail!("COM payload too long");
    }
    let mut segment = Vec::with_capacity(2 + segment_len);
    segment.push(0xFF);
    segment.push(0xFE); // COM
    segment.extend_from_slice(&(segment_len as u16).to_be_bytes());
    segment.extend_from_slice(payload_bytes);
    jpeg.splice(2..2, segment);
    Ok(())
}

fn for_each_com<F>(jpeg: &[u8], mut f: F)
where
    F: FnMut(&[u8]) -> bool,
{
    let mut i = 0;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xFE {
            let data = &jpeg[i + 4..i + 2 + len];
            if !f(data) {
                return;
            }
        }
        i += 2 + len;
    }
}

/// Collect JPEG marker bytes from segment headers up to (and including) SOS.
///
/// Useful for unit tests: COM=`0xFE`, APP0=`0xE0`, APP1=`0xE1`, SOF0=`0xC0`,
/// SOF2 progressive=`0xC2`. Does not walk entropy-coded scan data.
pub fn jpeg_segment_markers(jpeg: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return out;
    }
    out.push(0xD8); // SOI
    let mut i = 2usize;
    while i + 1 < jpeg.len() {
        if jpeg[i] != 0xFF {
            break;
        }
        let mut j = i + 1;
        while j < jpeg.len() && jpeg[j] == 0xFF {
            j += 1;
        }
        if j >= jpeg.len() {
            break;
        }
        let marker = jpeg[j];
        out.push(marker);
        i = j + 1;
        // Standalone markers (no length): RSTn, SOI, EOI, TEM.
        if matches!(marker, 0xD0..=0xD9 | 0x01) {
            if marker == 0xD9 {
                break;
            }
            continue;
        }
        if i + 2 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i], jpeg[i + 1]]) as usize;
        if len < 2 || i + len > jpeg.len() {
            break;
        }
        i += len;
        // Stop after SOS — remaining bytes are entropy-coded.
        if marker == 0xDA {
            break;
        }
    }
    out
}

/// True when any COM (`0xFE`) segment is present before SOS.
pub fn jpeg_has_com(jpeg: &[u8]) -> bool {
    jpeg_segment_markers(jpeg).contains(&0xFE)
}

/// True when APP1 (`0xE1`, typically EXIF) is present before SOS.
pub fn jpeg_has_app1(jpeg: &[u8]) -> bool {
    jpeg_segment_markers(jpeg).contains(&0xE1)
}

/// True when the frame is baseline SOF0 and not progressive SOF2.
pub fn jpeg_is_baseline(jpeg: &[u8]) -> bool {
    let m = jpeg_segment_markers(jpeg);
    m.contains(&0xC0) && !m.contains(&0xC2)
}

/// SOF0 `(width, height)` in pixels, or `None` if no baseline SOF0.
///
/// RetrieveImage SOF must match Create/Validate `PixelsPerLine` / `NumberOfLines`
/// ([docs/specs/mtom-retrieveimage.md](../../../docs/specs/mtom-retrieveimage.md)).
pub fn jpeg_sof0_dimensions(jpeg: &[u8]) -> Option<(u32, u32)> {
    let mut i = 0usize;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xC0 {
            let payload = &jpeg[i + 4..i + 2 + len];
            // precision(1) + height(2) + width(2) + nf(1) …
            if payload.len() < 5 {
                return None;
            }
            let height = u16::from_be_bytes([payload[1], payload[2]]) as u32;
            let width = u16::from_be_bytes([payload[3], payload[4]]) as u32;
            return Some((width, height));
        }
        i += 2 + len;
    }
    None
}

/// SOF0 component sampling: `(id, H, V)` triples, or empty if no SOF0.
pub fn jpeg_sof0_components(jpeg: &[u8]) -> Vec<(u8, u8, u8)> {
    let mut i = 0usize;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xC0 {
            let payload = &jpeg[i + 4..i + 2 + len];
            if payload.len() < 6 {
                return Vec::new();
            }
            let nf = payload[5] as usize;
            let mut comps = Vec::with_capacity(nf);
            let mut off = 6;
            for _ in 0..nf {
                if off + 3 > payload.len() {
                    break;
                }
                let id = payload[off];
                let hv = payload[off + 1];
                comps.push((id, hv >> 4, hv & 0xF));
                off += 3;
            }
            return comps;
        }
        i += 2 + len;
    }
    Vec::new()
}

/// APP0/JFIF `(units, xdensity, ydensity)` when present.
pub fn jpeg_jfif_density(jpeg: &[u8]) -> Option<(u8, u16, u16)> {
    let mut i = 0usize;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xE0 {
            let payload = &jpeg[i + 4..i + 2 + len];
            if payload.len() >= 14 && payload.starts_with(b"JFIF\0") {
                let units = payload[7];
                let x = u16::from_be_bytes([payload[8], payload[9]]);
                let y = u16::from_be_bytes([payload[10], payload[11]]);
                return Some((units, x, y));
            }
            return None;
        }
        i += 2 + len;
    }
    None
}

/// DRI restart interval, if a DRI segment is present before SOS.
pub fn jpeg_dri(jpeg: &[u8]) -> Option<u16> {
    let mut i = 0usize;
    while i + 4 < jpeg.len() {
        if jpeg[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = jpeg[i + 1];
        if marker == 0xD8 {
            i += 2;
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            break;
        }
        if i + 4 > jpeg.len() {
            break;
        }
        let len = u16::from_be_bytes([jpeg[i + 2], jpeg[i + 3]]) as usize;
        if len < 2 || i + 2 + len > jpeg.len() {
            break;
        }
        if marker == 0xDD {
            let payload = &jpeg[i + 4..i + 2 + len];
            if payload.len() >= 2 {
                return Some(u16::from_be_bytes([payload[0], payload[1]]));
            }
            return None;
        }
        i += 2 + len;
    }
    None
}

/// Read the page index from a fakescan JPEG COM marker, if present.
pub fn read_page_marker(jpeg: &[u8]) -> Option<u32> {
    let mut found = None;
    for_each_com(jpeg, |data| {
        if let Some(rest) = data.strip_prefix(PAGE_MARKER_PREFIX) {
            if let Ok(s) = std::str::from_utf8(rest) {
                found = s.parse().ok();
                return false;
            }
        }
        true
    });
    found
}

/// Read the duplex side from a fakescan JPEG COM marker, if present.
pub fn read_side_marker(jpeg: &[u8]) -> Option<PageSide> {
    let mut found = None;
    for_each_com(jpeg, |data| {
        if let Some(rest) = data.strip_prefix(SIDE_MARKER_PREFIX) {
            if let Ok(s) = std::str::from_utf8(rest) {
                found = PageSide::parse(s).ok();
                return false;
            }
        }
        true
    });
    found
}

// ───────────────────────────────────────────────────────────────────────────
// `diag` pattern: self-describing page for blind image-handling analysis.
// Layout is in millimetres (top-left anchored, px = round(mm * dpi / 25.4)).
// See docs/patterns.md ("diag") and pdfcraft-fakescan docs/pattern-analyzer.md.
// ───────────────────────────────────────────────────────────────────────────

/// Layout, sequences and renderer for [`PatternId::Diag`].
#[allow(clippy::needless_range_loop)]
pub mod diag {
    use super::*;
    use std::sync::OnceLock;

    /// Usable layout width / height in mm (the smallest optical page is 816x1144 px @100).
    pub const LAYOUT_W_MM: f64 = 206.0;
    pub const LAYOUT_H_MM: f64 = 288.0;
    /// Everything above this y is covered by the tester banner.
    pub const BANNER_RESERVE_MM: f64 = 12.0;

    /// Axis-aligned rectangle in mm: `(x0, y0, x1, y1)`.
    pub type RectMm = (f64, f64, f64, f64);

    // Fiducials: 8x8 unit cells of 2 mm = 16 mm squares.
    pub const FID_UNIT_MM: f64 = 2.0;
    pub const FID_UNITS: u32 = 8;
    pub const FID_TL: (f64, f64) = (4.0, 14.0);
    pub const FID_TR: (f64, f64) = (186.0, 14.0);
    pub const FID_BL: (f64, f64) = (4.0, 268.0);
    pub const FID_BR: (f64, f64) = (186.0, 268.0);
    /// Bar counts at TL, TR, BL, BR.
    pub const FID_BARS: [u32; 4] = [1, 2, 3, 4];

    // Correlation strips. Chip +1 = ink (black), -1 = paper (white).
    pub const BARKER13: [i8; 13] = [1, 1, 1, 1, 1, -1, -1, 1, 1, -1, 1, -1, 1];
    pub const BARKER_CHIP_MM: f64 = 4.0;
    pub const BARKER_REPEATS: usize = 3;
    pub const MSEQ_CHIP_MM: f64 = 2.5;
    pub const MSEQ_LEN: usize = 63;
    /// Horizontal strips (x0 mm, y0 mm); height 8 mm each.
    pub const HSTRIP_BARKER: (f64, f64) = (24.0, 14.0);
    pub const HSTRIP_MSEQ: (f64, f64) = (24.0, 24.0);
    pub const STRIP_THICK_MM: f64 = 8.0;
    /// Vertical strips (x0 mm, y0 mm); width 8 mm each, run downwards.
    pub const VSTRIP_BARKER: (f64, f64) = (4.0, 34.0);
    pub const VSTRIP_MSEQ: (f64, f64) = (13.0, 34.0);

    // Polarity wedge band: black outline 1 mm, [black ref 13][16 steps x 7][white ref 13].
    pub const WEDGE_BAND: RectMm = (26.0, 34.0, 166.0, 50.0);
    pub const WEDGE_OUTLINE_MM: f64 = 1.0;
    pub const WEDGE_REF_MM: f64 = 13.0;
    pub const WEDGE_STEPS: usize = 16;
    pub const WEDGE_STEP_MM: f64 = 7.0;
    /// Wedge step k has gray level `17 * k` (0 = ink black ... 255 = paper white).
    pub fn wedge_level(k: usize) -> u8 {
        (17 * k) as u8
    }

    // Colour patches R,G,B,C,M,Y (20 x 12 mm, adjacent).
    pub const PATCHES_Y: (f64, f64) = (52.0, 64.0);
    pub const PATCHES_X0: f64 = 26.0;
    pub const PATCH_W_MM: f64 = 20.0;
    pub const PATCH_COLORS: [[u8; 3]; 6] = [
        [255, 0, 0],
        [0, 255, 0],
        [0, 0, 255],
        [0, 255, 255],
        [255, 0, 255],
        [255, 255, 0],
    ];

    // Bit-order comb: pixel-exact. Byte-aligned start, 8-pixel motif (MSB = leftmost).
    pub const COMB_Y_MM: f64 = 67.0;
    pub const COMB_X0_MM: f64 = 26.0;
    pub const COMB_MOTIF: u8 = 0b1101_0000;
    pub const COMB_ROWS_PX: u32 = 8;
    pub const COMB_WIDTH_MM: f64 = 130.0;
    /// Extra pixels beyond a whole number of motifs (width not divisible by 8).
    pub const COMB_TAIL_PX: u32 = 5;

    // Vertical 1-px bars (pixel-exact, Golomb-ruler spacing x 4 px).
    pub const BARS_Y_MM: (f64, f64) = (76.0, 96.0);
    pub const BARS_X0_MM: f64 = 26.0;
    pub const BARS_GOLOMB: [u32; 10] = [0, 1, 6, 10, 23, 26, 34, 41, 53, 55];
    pub const BARS_PITCH_PX: u32 = 4;

    // Raw (non-ECC) bit strip: 128 chips of 1 mm: 13 Barker preamble + 115 PRBS.
    pub const RAW_Y_MM: (f64, f64) = (98.0, 105.0);
    pub const RAW_X0_MM: f64 = 26.0;
    pub const RAW_CHIP_MM: f64 = 1.0;
    pub const RAW_BITS: usize = 128;
    pub const RAW_SEED: u32 = 0xD1A6_0001;

    // 2-D position array: 36 x 36 cells of 2 mm.
    pub const POS_X0_MM: f64 = 26.0;
    pub const POS_Y0_MM: f64 = 108.0;
    pub const POS_CELL_MM: f64 = 2.0;
    pub const POS_N: usize = 36;
    pub const POS_SEED: u32 = 0xA55A_1234;

    // Zone plate: 40 mm square, centre (128, 128), f(r) = ZONE_FMAX * r / ZONE_R cycles/mm.
    pub const ZONE_BOX: RectMm = (108.0, 108.0, 148.0, 148.0);
    pub const ZONE_CENTER_MM: (f64, f64) = (128.0, 128.0);
    pub const ZONE_R_MM: f64 = 20.0;
    pub const ZONE_FMAX: f64 = 1.2;

    // QR: bottom-right corner of the QR (incl. quiet zone) sits at this point.
    pub const QR_ANCHOR_MM: (f64, f64) = (184.0, 264.0);
    pub const QR_TARGET_MM: f64 = 44.0;

    /// Effective render dpi for a requested dpi (mirrors the 4 MP cap in [`job_page_pixels`]).
    pub fn effective_dpi(dpi: u32) -> u32 {
        let (w, h) = optical_page_pixels(dpi);
        if w.saturating_mul(h) > MAX_PIXELS {
            150
        } else {
            dpi
        }
    }

    /// mm -> px at `dpi`.
    pub fn px(mm: f64, dpi: u32) -> u32 {
        (mm * dpi as f64 / 25.4).round().max(0.0) as u32
    }

    /// Periodic m-sequence of length 63 as +/-1 chips (+1 = ink).
    /// `vertical == false`: x^6+x+1; `true`: x^6+x^5+x^2+x+1.
    pub fn mseq63(vertical: bool) -> [i8; MSEQ_LEN] {
        let mut a = [0u8; MSEQ_LEN + 6];
        a[0] = 1;
        for n in 0..MSEQ_LEN {
            a[n + 6] = if vertical {
                a[n + 5] ^ a[n + 2] ^ a[n + 1] ^ a[n]
            } else {
                a[n + 1] ^ a[n]
            };
        }
        let mut out = [0i8; MSEQ_LEN];
        for n in 0..MSEQ_LEN {
            out[n] = if a[n] == 1 { 1 } else { -1 };
        }
        out
    }

    struct XorShift32(u32);
    impl XorShift32 {
        fn next(&mut self) -> u32 {
            let mut x = self.0;
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            self.0 = x;
            x
        }
        fn bit(&mut self) -> u8 {
            (self.next() >> 31) as u8
        }
    }

    /// The 128 raw strip bits (1 = ink): Barker-13 preamble then xorshift32 MSBs.
    pub fn raw_bits() -> [u8; RAW_BITS] {
        let mut out = [0u8; RAW_BITS];
        let mut rng = XorShift32(RAW_SEED);
        for (i, o) in out.iter_mut().enumerate() {
            let r = rng.bit();
            *o = if i < 13 { (BARKER13[i] == 1) as u8 } else { r };
        }
        out
    }

    /// 36x36 cell array (1 = ink) in which every 4x4 window is unique.
    pub fn position_array() -> &'static [[u8; POS_N]; POS_N] {
        static ARR: OnceLock<[[u8; POS_N]; POS_N]> = OnceLock::new();
        ARR.get_or_init(build_position_array)
    }

    fn window_at(bits: &[u8], r: usize, c: usize) -> usize {
        // window whose bottom-right cell is (r, c)
        let mut w = 0usize;
        for dr in 0..4 {
            for dc in 0..4 {
                w = (w << 1) | bits[(r - 3 + dr) * POS_N + (c - 3 + dc)] as usize;
            }
        }
        w
    }

    fn build_position_array() -> [[u8; POS_N]; POS_N] {
        let n = POS_N * POS_N;
        let mut bits = vec![0u8; n];
        let mut tried = vec![0u8; n];
        let mut first = vec![0u8; n];
        let mut used = vec![false; 1 << 16];
        let mut rng = XorShift32(POS_SEED);
        let mut k = 0usize;
        while k < n {
            let (r, c) = (k / POS_N, k % POS_N);
            if tried[k] == 0 {
                first[k] = rng.bit();
            }
            if tried[k] == 2 {
                tried[k] = 0;
                assert!(k > 0, "position array search exhausted");
                k -= 1;
                let (r, c) = (k / POS_N, k % POS_N);
                if r >= 3 && c >= 3 {
                    used[window_at(&bits, r, c)] = false;
                }
                continue;
            }
            let b = first[k] ^ tried[k];
            tried[k] += 1;
            bits[k] = b;
            if r >= 3 && c >= 3 {
                let w = window_at(&bits, r, c);
                if used[w] {
                    continue;
                }
                used[w] = true;
            }
            k += 1;
        }
        let mut out = [[0u8; POS_N]; POS_N];
        for r in 0..POS_N {
            for c in 0..POS_N {
                out[r][c] = bits[r * POS_N + c];
            }
        }
        out
    }

    /// Fiducial bitmap (8x8 units, true = ink): full-height spine on the left,
    /// `bars` teeth (rows 0,2,..,2*bars-2) running to the right.
    pub fn fiducial_cells(bars: u32) -> [[bool; 8]; 8] {
        let mut g = [[false; 8]; 8];
        for row in g.iter_mut() {
            row[0] = true;
        }
        for b in 0..bars as usize {
            for c in 0..8 {
                g[2 * b][c] = true;
            }
        }
        g
    }

    const INK: Rgb<u8> = Rgb([0, 0, 0]);
    const PAPER: Rgb<u8> = Rgb([255, 255, 255]);

    fn rect_mm(img: &mut RgbImage, dpi: u32, x0: f64, y0: f64, x1: f64, y1: f64, c: Rgb<u8>) {
        fill_rect(img, px(x0, dpi), px(y0, dpi), px(x1, dpi), px(y1, dpi), c);
    }

    fn draw_fiducial(img: &mut RgbImage, dpi: u32, org: (f64, f64), bars: u32) {
        let g = fiducial_cells(bars);
        for (r, row) in g.iter().enumerate() {
            for (c, &on) in row.iter().enumerate() {
                if on {
                    let x = org.0 + c as f64 * FID_UNIT_MM;
                    let y = org.1 + r as f64 * FID_UNIT_MM;
                    rect_mm(img, dpi, x, y, x + FID_UNIT_MM, y + FID_UNIT_MM, INK);
                }
            }
        }
    }

    fn strip_chips(
        img: &mut RgbImage,
        dpi: u32,
        org: (f64, f64),
        chips: &[i8],
        chip_mm: f64,
        vertical: bool,
    ) {
        for (i, &v) in chips.iter().enumerate() {
            if v != 1 {
                continue;
            }
            let a = i as f64 * chip_mm;
            let b = a + chip_mm;
            if vertical {
                rect_mm(
                    img,
                    dpi,
                    org.0,
                    org.1 + a,
                    org.0 + STRIP_THICK_MM,
                    org.1 + b,
                    INK,
                );
            } else {
                rect_mm(
                    img,
                    dpi,
                    org.0 + a,
                    org.1,
                    org.0 + b,
                    org.1 + STRIP_THICK_MM,
                    INK,
                );
            }
        }
    }

    /// Barker-13 repeated [`BARKER_REPEATS`] times.
    pub fn barker_run() -> Vec<i8> {
        (0..BARKER_REPEATS).flat_map(|_| BARKER13).collect()
    }

    /// Absolute pixel x of the comb start (byte aligned).
    pub fn comb_x0_px(dpi: u32) -> u32 {
        px(COMB_X0_MM, dpi).div_ceil(8) * 8
    }

    /// Comb length in pixels (whole motifs + [`COMB_TAIL_PX`]).
    pub fn comb_len_px(dpi: u32) -> u32 {
        (px(COMB_WIDTH_MM, dpi) / 8) * 8 + COMB_TAIL_PX
    }

    /// Render the diag page for a layout `version` (1 = original, 2 = v1 plus the
    /// `diag_v2` elements, which only occupy areas v1 leaves empty).
    pub fn draw_diag_v(w: u32, h: u32, dpi: u32, version: u32) -> RgbImage {
        let mut img = draw_diag(w, h, dpi);
        if version >= 2 {
            crate::diag_v2::draw(&mut img, dpi);
        }
        img
    }

    /// Render the layout-v1 diag page (no banner / QR; those are stamped by the caller).
    pub fn draw_diag(w: u32, h: u32, dpi: u32) -> RgbImage {
        let mut img: RgbImage = ImageBuffer::from_pixel(w, h, PAPER);

        // Fiducials.
        for (org, bars) in [FID_TL, FID_TR, FID_BL, FID_BR].into_iter().zip(FID_BARS) {
            draw_fiducial(&mut img, dpi, org, bars);
        }

        // Correlation strips.
        let barker = barker_run();
        strip_chips(&mut img, dpi, HSTRIP_BARKER, &barker, BARKER_CHIP_MM, false);
        strip_chips(
            &mut img,
            dpi,
            HSTRIP_MSEQ,
            &mseq63(false),
            MSEQ_CHIP_MM,
            false,
        );
        strip_chips(&mut img, dpi, VSTRIP_BARKER, &barker, BARKER_CHIP_MM, true);
        strip_chips(
            &mut img,
            dpi,
            VSTRIP_MSEQ,
            &mseq63(true),
            MSEQ_CHIP_MM,
            true,
        );

        // Polarity wedge band.
        let (bx0, by0, bx1, by1) = WEDGE_BAND;
        rect_mm(&mut img, dpi, bx0, by0, bx1, by1, INK);
        let (ix0, iy0, ix1, iy1) = (
            bx0 + WEDGE_OUTLINE_MM,
            by0 + WEDGE_OUTLINE_MM,
            bx1 - WEDGE_OUTLINE_MM,
            by1 - WEDGE_OUTLINE_MM,
        );
        rect_mm(&mut img, dpi, ix0, iy0, ix0 + WEDGE_REF_MM, iy1, INK);
        for k in 0..WEDGE_STEPS {
            let x = ix0 + WEDGE_REF_MM + k as f64 * WEDGE_STEP_MM;
            let l = wedge_level(k);
            rect_mm(
                &mut img,
                dpi,
                x,
                iy0,
                x + WEDGE_STEP_MM,
                iy1,
                Rgb([l, l, l]),
            );
        }
        rect_mm(&mut img, dpi, ix1 - WEDGE_REF_MM, iy0, ix1, iy1, PAPER);

        // Colour patches.
        for (i, c) in PATCH_COLORS.iter().enumerate() {
            let x = PATCHES_X0 + i as f64 * PATCH_W_MM;
            rect_mm(
                &mut img,
                dpi,
                x,
                PATCHES_Y.0,
                x + PATCH_W_MM,
                PATCHES_Y.1,
                Rgb(*c),
            );
        }

        // Bit-order comb (pixel-exact).
        let cx0 = comb_x0_px(dpi);
        let cy0 = px(COMB_Y_MM, dpi);
        for i in 0..comb_len_px(dpi) {
            if COMB_MOTIF & (0x80 >> (i % 8)) != 0 {
                fill_rect(&mut img, cx0 + i, cy0, cx0 + i + 1, cy0 + COMB_ROWS_PX, INK);
            }
        }

        // Vertical 1-px bars (pixel-exact).
        let bx = px(BARS_X0_MM, dpi);
        let (y0, y1) = (px(BARS_Y_MM.0, dpi), px(BARS_Y_MM.1, dpi));
        for g in BARS_GOLOMB {
            let x = bx + g * BARS_PITCH_PX;
            fill_rect(&mut img, x, y0, x + 1, y1, INK);
        }

        // Raw bit strip.
        for (i, b) in raw_bits().iter().enumerate() {
            if *b == 1 {
                let x = RAW_X0_MM + i as f64 * RAW_CHIP_MM;
                rect_mm(
                    &mut img,
                    dpi,
                    x,
                    RAW_Y_MM.0,
                    x + RAW_CHIP_MM,
                    RAW_Y_MM.1,
                    INK,
                );
            }
        }

        // Position array.
        let arr = position_array();
        for (r, row) in arr.iter().enumerate() {
            for (c, &b) in row.iter().enumerate() {
                if b == 1 {
                    let x = POS_X0_MM + c as f64 * POS_CELL_MM;
                    let y = POS_Y0_MM + r as f64 * POS_CELL_MM;
                    rect_mm(&mut img, dpi, x, y, x + POS_CELL_MM, y + POS_CELL_MM, INK);
                }
            }
        }

        // Zone plate.
        let (zx0, zy0, zx1, zy1) = ZONE_BOX;
        let pxmm = dpi as f64 / 25.4;
        for y in px(zy0, dpi)..px(zy1, dpi).min(h) {
            for x in px(zx0, dpi)..px(zx1, dpi).min(w) {
                let dx = (x as f64 + 0.5) / pxmm - ZONE_CENTER_MM.0;
                let dy = (y as f64 + 0.5) / pxmm - ZONE_CENTER_MM.1;
                let phase = std::f64::consts::PI * ZONE_FMAX * (dx * dx + dy * dy) / ZONE_R_MM;
                let l = (127.5 + 127.5 * phase.cos()).round() as u8;
                img.put_pixel(x, y, Rgb([l, l, l]));
            }
        }

        // Frame: 1-px border at the very edge (top row is covered by the banner).
        draw_border(&mut img, INK);
        img
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::collections::HashSet;

        fn corr(a: &[i8], b: &[i8], lag: usize) -> i32 {
            // circular cross-correlation at `lag`
            let n = a.len();
            (0..n).map(|i| a[i] as i32 * b[(i + lag) % n] as i32).sum()
        }

        #[test]
        fn layout_fits_smallest_page_and_scales_with_dpi() {
            for dpi in [100u32, 150, 200, 300] {
                let (w, h) = job_page_pixels(dpi);
                let e = effective_dpi(dpi);
                assert!(px(LAYOUT_W_MM, e) <= w, "dpi {dpi}: layout wider than page");
                assert!(
                    px(LAYOUT_H_MM, e) <= h,
                    "dpi {dpi}: layout taller than page"
                );
                let (img, _) = render_pattern_rgb(PatternId::DiagV2, dpi, 0, None).unwrap();
                assert_eq!(img.dimensions(), (w, h));
            }
            assert_eq!(
                effective_dpi(300),
                150,
                "300 dpi is capped to 150 by the 4 MP limit"
            );
            assert_eq!(effective_dpi(200), 200);
            // mm -> px scaling: 10 mm @ 254 dpi is exactly 100 px, and 2x dpi doubles it.
            assert_eq!(px(10.0, 254), 100);
            assert_eq!(px(25.4, 200), 200);
            assert_eq!(px(25.4, 100) * 2, px(25.4, 200));
        }

        #[test]
        fn every_feature_is_at_least_3px_at_100dpi_unless_pixel_exact() {
            let d = 100;
            for mm in [
                FID_UNIT_MM,
                BARKER_CHIP_MM,
                MSEQ_CHIP_MM,
                STRIP_THICK_MM,
                WEDGE_OUTLINE_MM,
                WEDGE_STEP_MM,
                PATCH_W_MM,
                RAW_CHIP_MM,
                POS_CELL_MM,
            ] {
                assert!(px(mm, d) >= 3, "{mm} mm is {} px at 100 dpi", px(mm, d));
            }
            // Regions below the banner.
            for y in [FID_TL.1, HSTRIP_BARKER.1] {
                assert!(y >= BANNER_RESERVE_MM);
            }
        }

        #[test]
        fn render_is_deterministic_and_page_dependent_only_via_qr() {
            let (a, _) = render_pattern_rgb(PatternId::DiagV2, 100, 0, None).unwrap();
            let (b, _) = render_pattern_rgb(PatternId::DiagV2, 100, 0, None).unwrap();
            assert_eq!(a.as_raw(), b.as_raw());
            let (c, _) = render_pattern_rgb(PatternId::DiagV2, 100, 1, None).unwrap();
            assert_ne!(a.as_raw(), c.as_raw());
            let j0 = jpeg_pattern(PatternId::DiagV2, 100, 0).unwrap();
            assert_eq!(j0, jpeg_pattern(PatternId::DiagV2, 100, 0).unwrap());
            assert_eq!(read_page_marker(&j0), Some(0));
        }

        #[test]
        fn qr_decodes_after_thresholding_at_200dpi() {
            let (img, _) = render_pattern_rgb(PatternId::DiagV2, 200, 7, None).unwrap();
            let mut g = image::GrayImage::new(img.width(), img.height());
            for (x, y, p) in img.enumerate_pixels() {
                g.put_pixel(
                    x,
                    y,
                    image::Luma([if luma8(p) >= BW_THRESHOLD { 255 } else { 0 }]),
                );
            }
            let mut prepared = rqrr::PreparedImage::prepare(g);
            let grids = prepared.detect_grids();
            assert!(!grids.is_empty(), "QR not found on thresholded diag page");
            let (_m, text) = grids[0].decode().expect("decode");
            assert_eq!(
                text,
                format!("{}|lay=2", qr_payload(PatternId::Diag, 7, None))
            );
            let p = parse_qr_payload(&text).unwrap();
            assert_eq!((p.page, p.pattern, p.layout), (7, PatternId::Diag, Some(2)));
        }

        #[test]
        fn qr_does_not_overlap_other_features() {
            // Reserve box (see QR_ANCHOR_MM); the QR must stay inside x>=128, y>=208 mm.
            for dpi in [100u32, 150, 200] {
                let code = QrCode::with_error_correction_level(
                    qr_payload(PatternId::Diag, 99, Some(PageSide::Back)).as_bytes(),
                    EcLevel::M,
                )
                .unwrap();
                let side = code
                    .render::<Rgb<u8>>()
                    .quiet_zone(true)
                    .min_dimensions(px(QR_TARGET_MM, dpi), px(QR_TARGET_MM, dpi))
                    .build()
                    .width();
                assert!(
                    px(QR_ANCHOR_MM.0, dpi) as i64 - side as i64 >= px(128.0, dpi) as i64,
                    "dpi {dpi}: QR {side}px too wide"
                );
                assert!(px(QR_ANCHOR_MM.1, dpi) as i64 - side as i64 >= px(208.0, dpi) as i64);
            }
        }

        #[test]
        fn position_array_windows_are_all_unique() {
            let a = position_array();
            let mut seen = HashSet::new();
            for r in 0..=POS_N - 4 {
                for c in 0..=POS_N - 4 {
                    let mut w = 0u32;
                    for dr in 0..4 {
                        for dc in 0..4 {
                            w = (w << 1) | a[r + dr][c + dc] as u32;
                        }
                    }
                    assert!(seen.insert(w), "duplicate 4x4 window at ({r},{c})");
                }
            }
            assert_eq!(seen.len(), (POS_N - 3) * (POS_N - 3));
            // Not degenerate: roughly balanced ink.
            let ink: usize = a.iter().flatten().map(|&b| b as usize).sum();
            let n = POS_N * POS_N;
            assert!(ink > n * 35 / 100 && ink < n * 65 / 100, "ink {ink}/{n}");
            // Deterministic.
            assert_eq!(build_position_array(), *a);
        }

        #[test]
        fn rendered_position_array_matches_cells_at_100dpi() {
            let (img, _) = render_pattern_rgb(PatternId::DiagV2, 100, 0, None).unwrap();
            let a = position_array();
            for r in 0..POS_N {
                for c in 0..POS_N {
                    let x = px(POS_X0_MM + (c as f64 + 0.5) * POS_CELL_MM, 100);
                    let y = px(POS_Y0_MM + (r as f64 + 0.5) * POS_CELL_MM, 100);
                    let ink = luma8(img.get_pixel(x, y)) < 128;
                    assert_eq!(ink, a[r][c] == 1, "cell ({r},{c})");
                }
            }
        }

        #[test]
        fn barker13_autocorrelation_sidelobes_are_at_most_one() {
            let b = BARKER13;
            assert_eq!(corr_aperiodic(&b, &b, 0), 13);
            for lag in 1..13 {
                assert!(corr_aperiodic(&b, &b, lag).abs() <= 1, "lag {lag}");
            }
            // Mirror: the reversed template does not correlate with the strip.
            let rev: Vec<i8> = b.iter().rev().copied().collect();
            let best = (0..13)
                .map(|l| corr_aperiodic(&b, &rev, l).abs())
                .max()
                .unwrap();
            assert!(best < 13, "Barker-13 palindromic?");
            // Polarity: negation flips the sign of the peak.
            let neg: Vec<i8> = b.iter().map(|v| -v).collect();
            assert_eq!(corr_aperiodic(&b, &neg, 0), -13);
            assert_eq!(barker_run().len(), 39);
        }

        fn corr_aperiodic(a: &[i8], b: &[i8], lag: usize) -> i32 {
            (0..a.len().saturating_sub(lag))
                .map(|i| a[i] as i32 * b[i + lag] as i32)
                .sum()
        }

        #[test]
        fn mseq63_periodic_autocorrelation_is_two_valued_for_both_polys() {
            for vertical in [false, true] {
                let m = mseq63(vertical);
                assert_eq!(m.iter().map(|&v| v as i32).sum::<i32>(), 1, "balance");
                assert_eq!(corr(&m, &m, 0), 63);
                for lag in 1..63 {
                    assert_eq!(corr(&m, &m, lag), -1, "poly vertical={vertical} lag {lag}");
                }
                // Reversed template (mirror) stays far below the peak.
                let rev: Vec<i8> = m.iter().rev().copied().collect();
                let worst = (0..63).map(|l| corr(&m, &rev, l).abs()).max().unwrap();
                assert!(worst <= 21, "mirror cross-correlation {worst} too high");
                // Negation gives -63 (polarity).
                let neg: Vec<i8> = m.iter().map(|v| -v).collect();
                assert_eq!(corr(&m, &neg, 0), -63);
            }
            // The two sequences are different families.
            let (h, v) = (mseq63(false), mseq63(true));
            let worst = (0..63).map(|l| corr(&h, &v, l).abs()).max().unwrap();
            assert!(worst <= 21, "H/V cross-correlation {worst}");
        }

        #[test]
        fn raw_bits_are_stable_and_start_with_barker() {
            let b = raw_bits();
            for i in 0..13 {
                assert_eq!(b[i] == 1, BARKER13[i] == 1);
            }
            assert_eq!(b, raw_bits());
            let ones: usize = b[13..].iter().map(|&v| v as usize).sum();
            assert!(ones > 30 && ones < 85, "ones={ones}");
        }

        type Grid = Vec<Vec<bool>>;

        fn composite() -> Grid {
            // 40x30 unit grid with the four fiducials in their corners (4 units in from edges 2).
            let (gw, gh) = (40usize, 30usize);
            let mut g = vec![vec![false; gw]; gh];
            let origins = [(1, 1), (gw - 9, 1), (1, gh - 9), (gw - 9, gh - 9)];
            for (o, bars) in origins.iter().zip(FID_BARS) {
                let f = fiducial_cells(bars);
                for r in 0..8 {
                    for c in 0..8 {
                        g[o.1 + r][o.0 + c] = f[r][c];
                    }
                }
            }
            g
        }

        fn mirror_x(g: &Grid) -> Grid {
            g.iter()
                .map(|r| r.iter().rev().copied().collect())
                .collect()
        }
        fn flip_y(g: &Grid) -> Grid {
            g.iter().rev().cloned().collect()
        }
        fn rot90(g: &Grid) -> Grid {
            let (h, w) = (g.len(), g[0].len());
            (0..w)
                .map(|c| (0..h).map(|r| g[h - 1 - r][c]).collect())
                .collect()
        }

        #[test]
        fn fiducials_are_distinct_under_all_8_symmetries() {
            let g0 = composite();
            let mut variants = vec![];
            let mut g = g0.clone();
            for _ in 0..4 {
                variants.push(g.clone());
                variants.push(mirror_x(&g));
                g = rot90(&g);
            }
            assert_eq!(variants.len(), 8);
            for i in 0..8 {
                for j in i + 1..8 {
                    assert_ne!(variants[i], variants[j], "symmetries {i} and {j} collide");
                }
            }
            // Sanity: flip_y equals mirror_x then rot180 -> in the set.
            assert!(variants.contains(&flip_y(&g0)));
            // Single fiducials are pairwise distinct and all asymmetric.
            let cells: Vec<_> = FID_BARS.iter().map(|&b| fiducial_cells(b)).collect();
            for i in 0..4 {
                for j in i + 1..4 {
                    assert_ne!(cells[i], cells[j]);
                }
            }
            assert_eq!(FID_BARS, [1, 2, 3, 4]);
        }

        #[test]
        fn comb_row_packs_to_motif_bytes_with_odd_tail() {
            for dpi in [100u32, 200] {
                let (img, _) = render_pattern_rgb(PatternId::DiagV2, dpi, 0, None).unwrap();
                let x0 = comb_x0_px(dpi);
                assert_eq!(x0 % 8, 0);
                let len = comb_len_px(dpi);
                assert_ne!(len % 8, 0, "comb width must not be a multiple of 8");
                let y = px(COMB_Y_MM, dpi) + 3;
                let mut bytes = vec![];
                for b in 0..len / 8 {
                    let mut v = 0u8;
                    for k in 0..8 {
                        if luma8(img.get_pixel(x0 + b * 8 + k, y)) < 128 {
                            v |= 0x80 >> k;
                        }
                    }
                    bytes.push(v);
                }
                assert!(bytes.iter().all(|&b| b == COMB_MOTIF));
                assert_ne!(COMB_MOTIF.reverse_bits(), COMB_MOTIF);
                // Rows: 8 identical rows then paper.
                assert!(luma8(img.get_pixel(x0, y + 8)) > 200);
            }
        }

        #[test]
        fn vertical_bars_are_one_px_and_golomb_distinct() {
            let mut d = HashSet::new();
            for i in 0..BARS_GOLOMB.len() {
                for j in i + 1..BARS_GOLOMB.len() {
                    assert!(d.insert(BARS_GOLOMB[j] - BARS_GOLOMB[i]), "repeated gap");
                }
            }
            let (img, _) = render_pattern_rgb(PatternId::DiagV2, 100, 0, None).unwrap();
            let x0 = px(BARS_X0_MM, 100);
            let ymid = (px(BARS_Y_MM.0, 100) + px(BARS_Y_MM.1, 100)) / 2;
            let mut ink_cols = vec![];
            for x in x0.saturating_sub(5)..x0 + 230 {
                if luma8(img.get_pixel(x, ymid)) < 128 {
                    ink_cols.push(x - x0);
                }
            }
            let want: Vec<u32> = BARS_GOLOMB.iter().map(|g| g * BARS_PITCH_PX).collect();
            assert_eq!(ink_cols, want);
            // Border: left/right/bottom columns are ink.
            let (w, h) = img.dimensions();
            assert!(luma8(img.get_pixel(0, h / 2)) < 10 && luma8(img.get_pixel(w - 1, h / 2)) < 10);
            assert!(luma8(img.get_pixel(w / 2, h - 1)) < 10);
        }

        #[test]
        fn wedge_patches_and_zone_plate_render() {
            let dpi = 200;
            let (img, _) = render_pattern_rgb(PatternId::DiagV2, dpi, 0, None).unwrap();
            let ymid = px((WEDGE_BAND.1 + WEDGE_BAND.3) / 2.0, dpi);
            let ix0 = WEDGE_BAND.0 + WEDGE_OUTLINE_MM;
            let at = |x_mm: f64, y: u32| luma8(img.get_pixel(px(x_mm, dpi), y));
            assert!(at(ix0 + 1.0, ymid) < 5, "black ref");
            for k in 0..WEDGE_STEPS {
                let x = ix0 + WEDGE_REF_MM + (k as f64 + 0.5) * WEDGE_STEP_MM;
                let got = at(x, ymid) as i32;
                assert!((got - wedge_level(k) as i32).abs() <= 1, "step {k}: {got}");
            }
            assert!(
                at(WEDGE_BAND.2 - WEDGE_OUTLINE_MM - 1.0, ymid) > 250,
                "white ref"
            );
            let py = px((PATCHES_Y.0 + PATCHES_Y.1) / 2.0, dpi);
            for (i, c) in PATCH_COLORS.iter().enumerate() {
                let p = img.get_pixel(px(PATCHES_X0 + (i as f64 + 0.5) * PATCH_W_MM, dpi), py);
                assert_eq!(p.0, *c, "patch {i}");
            }
            // Zone plate: centre is bright (cos 0 = 1) and the plate has a wide gray range.
            let zc = img.get_pixel(px(ZONE_CENTER_MM.0, dpi), px(ZONE_CENTER_MM.1, dpi));
            assert!(zc.0[0] > 240);
            let (mut lo, mut hi) = (255u8, 0u8);
            for y in px(ZONE_BOX.1, dpi)..px(ZONE_BOX.3, dpi) {
                for x in px(ZONE_BOX.0, dpi)..px(ZONE_BOX.2, dpi) {
                    let v = img.get_pixel(x, y).0[0];
                    lo = lo.min(v);
                    hi = hi.max(v);
                }
            }
            assert!(lo < 15 && hi > 240);
        }

        fn fnv64(bytes: &[u8]) -> u64 {
            let mut h: u64 = 0xcbf2_9ce4_8422_2325;
            for b in bytes {
                h ^= *b as u64;
                h = h.wrapping_mul(0x0000_0100_0000_01b3);
            }
            h
        }

        /// Layout v1 must stay byte-identical to what `diag` rendered before layout v2
        /// (hashes taken from the pre-v2 build; banner and QR included).
        #[test]
        fn diag_v1_is_byte_identical_to_the_pre_v2_page() {
            for (dpi, want) in [
                (100u32, 0x56b9_0782_c132_0453u64),
                (200, 0xa470_196f_51b5_cdd6),
            ] {
                let (a, _) = render_pattern_rgb(PatternId::DiagV1, dpi, 0, None).unwrap();
                assert_eq!(fnv64(a.as_raw()), want, "dpi {dpi}");
            }
            let t = parse_qr_payload(&qr_payload(PatternId::DiagV1, 0, None)).unwrap();
            assert_eq!((t.pattern, t.layout), (PatternId::Diag, None));
        }

        /// Layout v2 stays byte-identical (pinned; `diag` now renders layout v3).
        #[test]
        fn diag_v2_is_byte_identical_and_still_selectable() {
            for (dpi, want) in [
                (100u32, 0x80da_cf1b_230c_cf86u64),
                (200, 0x40ce_3a59_8334_9b2e),
            ] {
                let (a, _) = render_pattern_rgb(PatternId::DiagV2, dpi, 0, None).unwrap();
                assert_eq!(fnv64(a.as_raw()), want, "dpi {dpi}");
            }
            assert_eq!(PatternId::parse("diag_v2").unwrap(), PatternId::DiagV2);
            assert_eq!(PatternId::DiagV2.wire_name(), "diag");
        }

        #[test]
        fn diag_pattern_id_roundtrip_and_encoding_unchanged_for_others() {
            assert_eq!(PatternId::parse("diag").unwrap(), PatternId::Diag);
            assert_eq!(PatternId::Diag.as_str(), "diag");
            assert!(PatternId::ALL.contains(&PatternId::Diag));
            assert!(pattern_banner(PatternId::Diag, 2, None).contains("diag"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn page_markers_encode_distinct_indices() {
        let p0 = jpeg_page(100, 0).unwrap();
        let p1 = jpeg_page(100, 1).unwrap();
        let p2 = jpeg_page(100, 2).unwrap();
        assert_eq!(read_page_marker(&p0), Some(0));
        assert_eq!(read_page_marker(&p1), Some(1));
        assert_eq!(read_page_marker(&p2), Some(2));
        assert_ne!(p0, p1);
        assert_ne!(p1, p2);
    }

    #[test]
    fn each_pattern_id_produces_readable_com_and_distinct_bytes() {
        let mut digests = HashSet::new();
        for &pattern in PatternId::ALL {
            let bytes =
                jpeg_pattern(pattern, 100, 0).unwrap_or_else(|_| panic!("{}", pattern.as_str()));
            assert_eq!(
                read_page_marker(&bytes),
                Some(0),
                "COM missing for {}",
                pattern.as_str()
            );
            assert!(
                digests.insert(bytes.clone()),
                "pattern {} collided with another pattern's JPEG",
                pattern.as_str()
            );
            // Page index still differs within a pattern.
            let page1 = jpeg_pattern(pattern, 100, 1).unwrap();
            assert_eq!(read_page_marker(&page1), Some(1));
            assert_ne!(bytes, page1);
        }
        assert_eq!(digests.len(), PatternId::ALL.len());
    }

    #[test]
    fn pattern_id_parse_roundtrip() {
        for &p in PatternId::ALL {
            assert_eq!(PatternId::parse(p.as_str()).unwrap(), p);
        }
        assert!(PatternId::parse("nope").is_err());
        assert_eq!(PatternId::parse("default").unwrap(), PatternId::PageId);
        assert_eq!(PatternId::parse("qr").unwrap(), PatternId::Qr);
    }

    #[test]
    fn qr_payload_roundtrips_without_ocr() {
        let text = qr_payload(PatternId::Wedges, 3, Some(PageSide::Back));
        let parsed = parse_qr_payload(&text).expect("parse");
        assert_eq!(parsed.page, 3);
        assert_eq!(parsed.pattern, PatternId::Wedges);
        assert_eq!(parsed.side, Some(PageSide::Back));
        assert!(parse_qr_payload("not-a-fakescan-code").is_none());
    }

    #[test]
    fn qr_on_page_decodes_after_jpeg() {
        // Large `qr` pattern + quality 95 should survive rqrr after JPEG.
        let jpeg = jpeg_pattern(PatternId::Qr, 150, 2).unwrap();
        let img = image::load_from_memory(&jpeg).unwrap().to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "no QR detected in qr pattern JPEG");
        let (_meta, content) = grids[0].decode().expect("qr decode");
        let parsed = parse_qr_payload(&content).expect("payload");
        assert_eq!(parsed.page, 2);
        assert_eq!(parsed.pattern, PatternId::Qr);
        assert_eq!(read_page_marker(&jpeg), Some(2));
    }

    #[test]
    fn corner_qr_on_page_id_decodes() {
        let jpeg = jpeg_pattern(PatternId::PageId, 150, 0).unwrap();
        let img = image::load_from_memory(&jpeg).unwrap().to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "corner QR missing on page_id");
        let (_meta, content) = grids[0].decode().expect("decode");
        let parsed = parse_qr_payload(&content).unwrap();
        assert_eq!(parsed.page, 0);
        assert_eq!(parsed.pattern, PatternId::PageId);
    }

    #[test]
    fn jpeg_page_matches_page_id_pattern() {
        let a = jpeg_page(100, 3).unwrap();
        let b = jpeg_pattern(PatternId::PageId, 100, 3).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn duplex_pair_tags_even_front_odd_back() {
        let front = jpeg_pattern(PatternId::DuplexPair, 100, 0).unwrap();
        let back = jpeg_pattern(PatternId::DuplexPair, 100, 1).unwrap();
        assert_eq!(read_page_marker(&front), Some(0));
        assert_eq!(read_page_marker(&back), Some(1));
        assert_eq!(read_side_marker(&front), Some(PageSide::Front));
        assert_eq!(read_side_marker(&back), Some(PageSide::Back));
        assert_ne!(front, back);
    }

    #[test]
    fn duplex_pair_forced_side_overrides_parity() {
        // Odd sheet index but forced front → still front (blue stack for pass 1).
        let forced =
            jpeg_pattern_with_side(PatternId::DuplexPair, 100, 1, Some(PageSide::Front)).unwrap();
        assert_eq!(read_page_marker(&forced), Some(1));
        assert_eq!(read_side_marker(&forced), Some(PageSide::Front));
        let auto_back = jpeg_pattern(PatternId::DuplexPair, 100, 1).unwrap();
        assert_eq!(read_side_marker(&auto_back), Some(PageSide::Back));
        assert_ne!(forced, auto_back);
    }

    #[test]
    fn page_side_parse_roundtrip() {
        assert_eq!(PageSide::parse("front").unwrap(), PageSide::Front);
        assert_eq!(PageSide::parse("BACK").unwrap(), PageSide::Back);
        assert!(PageSide::parse("sideways").is_err());
    }

    #[test]
    fn bytes_per_line_zero_when_compressed() {
        use crate::effects::ColorMode;
        // Default / ms_strict=false: Brother/wsdscdrv jfif RGB stride; gray jfif = width.
        assert_eq!(bytes_per_line("jfif", 850, ColorMode::Rgb24, false), 2550);
        assert_eq!(bytes_per_line("jfif", 850, ColorMode::Gray8, false), 850);
        assert_eq!(bytes_per_line("png", 850, ColorMode::Gray8, false), 850);
        // BMP/DIB rows are DWORD-padded: 2550 -> 2552.
        assert_eq!(bytes_per_line("dib", 850, ColorMode::Rgb24, false), 2552);
        assert_eq!(
            bytes_per_line("tiff-single-uncompressed", 850, ColorMode::Gray8, false),
            850
        );
        assert_eq!(bytes_per_line("dib", 900, ColorMode::Bw1, false), 116);
        // ms_strict=true: compressed → 0; uncompressed stride unchanged.
        assert_eq!(bytes_per_line("jfif", 850, ColorMode::Rgb24, true), 0);
        assert_eq!(bytes_per_line("jfif", 850, ColorMode::Gray8, true), 0);
        assert_eq!(bytes_per_line("png", 850, ColorMode::Gray8, true), 0);
        assert_eq!(bytes_per_line("dib", 850, ColorMode::Rgb24, true), 2552);
    }

    #[test]
    fn encode_wsd_formats_have_magic() {
        let jfif = encode_pattern(
            "jfif",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&jfif[..2], &[0xFF, 0xD8]);
        assert_eq!(mime_for_format("jfif"), "image/jpeg");

        let dib = encode_pattern(
            "dib",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&dib[..2], b"BM");
        assert_eq!(mime_for_format("dib"), "image/bmp");

        let png = encode_pattern(
            "png",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
        assert_eq!(mime_for_format("png"), "image/png");

        let tiff = encode_pattern(
            "tiff-single-uncompressed",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert!(
            tiff[..4] == [0x49, 0x49, 0x2A, 0x00] || tiff[..4] == [0x4D, 0x4D, 0x00, 0x2A],
            "tiff magic {:?}",
            &tiff[..4]
        );
        assert_eq!(mime_for_format("tiff-single-uncompressed"), "image/tiff");

        let exif = encode_pattern(
            "exif",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&exif[..2], &[0xFF, 0xD8]);
        assert!(
            exif.windows(6).any(|w| w == b"Exif\0\0"),
            "missing EXIF APP1"
        );
        assert_eq!(mime_for_format("exif"), "image/jpeg");

        let pdf = encode_pattern(
            "pdf-a",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert!(pdf.starts_with(b"%PDF-1.4"), "pdf header {:?}", &pdf[..8]);
        assert!(pdf.windows(4).any(|w| w == b"%%EOF") || pdf.ends_with(b"%%EOF\n"));
        assert_eq!(mime_for_format("pdf-a"), "application/pdf");

        let g4 = encode_pattern(
            "tiff-single-g4",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&g4[..4], &[0x49, 0x49, 0x2A, 0x00]);
        assert_eq!(mime_for_format("tiff-single-g4"), "image/tiff");

        let multi = encode_pattern_pages(
            "tiff-multi-uncompressed",
            PatternId::PageId,
            50,
            &[0, 1],
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&multi[..4], &[0x49, 0x49, 0x2A, 0x00]);

        let xps = encode_pattern(
            "xps",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&xps[..2], b"PK", "xps is a zip");
        assert_eq!(mime_for_format("xps"), "application/vnd.ms-xpsdocument");

        let jpeg_tiff = encode_pattern(
            "tiff-single-jpeg-tn2",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&jpeg_tiff[..4], &[0x49, 0x49, 0x2A, 0x00]);
        assert!(
            jpeg_tiff.windows(2).any(|w| w == [0xFF, 0xD8]),
            "jpeg-tn2 strip should contain SOI"
        );

        let g3 = encode_pattern(
            "tiff-single-g3mh",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(&g3[..4], &[0x49, 0x49, 0x2A, 0x00]);

        let jp2 = encode_pattern(
            "jpeg2k",
            PatternId::PageId,
            50,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert!(
            jp2.len() > 32 && (jp2[4..8] == *b"jP  " || jp2.starts_with(&[0xFF, 0x4F])),
            "jp2/j2k magic {:?}",
            &jp2[..8.min(jp2.len())]
        );
        assert_eq!(mime_for_format("jpeg2k"), "image/jp2");
    }

    #[test]
    fn encode_opts_can_omit_jpeg_com() {
        let with_com = encode_pattern(
            "jfif",
            PatternId::PageId,
            50,
            3,
            Some(PageSide::Back),
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
        )
        .unwrap();
        assert_eq!(read_page_marker(&with_com), Some(3));
        assert_eq!(read_side_marker(&with_com), Some(PageSide::Back));

        let bare = encode_pattern(
            "jfif",
            PatternId::PageId,
            50,
            3,
            Some(PageSide::Back),
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::NO_COM,
        )
        .unwrap();
        assert_eq!(&bare[..2], &[0xFF, 0xD8]);
        assert_eq!(read_page_marker(&bare), None);
        assert_eq!(read_side_marker(&bare), None);
        // Still a distinct page (QR / pattern differ by index).
        assert_ne!(
            encode_pattern(
                "jfif",
                PatternId::PageId,
                50,
                0,
                None,
                None,
                crate::effects::ColorMode::Rgb24,
                EncodeOpts::NO_COM,
            )
            .unwrap(),
            bare
        );
    }

    #[test]
    fn encode_opts_for_wia_plain_forces_plain_knobs() {
        assert_eq!(
            encode_opts_for(PatternId::WiaPlain, true),
            EncodeOpts::WIA_PLAIN
        );
        assert_eq!(
            encode_opts_for(PatternId::WiaPlain, false),
            EncodeOpts::WIA_PLAIN
        );
        assert_eq!(
            encode_opts_for(PatternId::Wedges, false),
            EncodeOpts::NO_COM
        );
        assert_eq!(
            encode_opts_for(PatternId::Wedges, true),
            EncodeOpts::WITH_COM
        );
    }

    #[test]
    fn wia_plain_jfif_omits_com_app1_qr_and_is_baseline() {
        let opts = EncodeOpts::WIA_PLAIN;
        let bytes = encode_pattern(
            "jfif",
            PatternId::WiaPlain,
            100,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            opts,
        )
        .unwrap();
        assert_eq!(&bytes[..2], &[0xFF, 0xD8]);
        assert!(
            jpeg_is_baseline(&bytes),
            "markers={:?}",
            jpeg_segment_markers(&bytes)
        );
        assert!(
            !jpeg_has_com(&bytes),
            "COM present: markers={:?}",
            jpeg_segment_markers(&bytes)
        );
        assert!(
            !jpeg_has_app1(&bytes),
            "APP1 present: markers={:?}",
            jpeg_segment_markers(&bytes)
        );
        assert_eq!(read_page_marker(&bytes), None);
        assert_eq!(read_side_marker(&bytes), None);
        // JFIF APP0 is expected on baseline scanner-shaped encode.
        assert!(
            jpeg_segment_markers(&bytes).contains(&0xE0),
            "expected APP0/JFIF: {:?}",
            jpeg_segment_markers(&bytes)
        );
        // No QR modules to decode.
        let img = image::load_from_memory(&bytes).unwrap().to_luma8();
        let mut prepared = rqrr::PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(grids.is_empty(), "wia_plain must not stamp a QR");
        // Smaller / simpler than QR-stamped wedges at Q95.
        let decorated = encode_pattern(
            "jfif",
            PatternId::Wedges,
            100,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::NO_COM,
        )
        .unwrap();
        assert!(
            bytes.len() < decorated.len(),
            "plain {} should be smaller than wedges+QR {}",
            bytes.len(),
            decorated.len()
        );
    }

    #[test]
    fn job_page_pixels_caps_300_to_match_encode() {
        let (full_w, full_h) = optical_page_pixels(300);
        assert!(full_w.saturating_mul(full_h) > MAX_PIXELS);
        let (jw, jh) = job_page_pixels(300);
        let (cw, ch) = capped_page_pixels(300);
        assert_eq!((jw, jh), (cw, ch));
        assert_eq!((jw, jh), optical_page_pixels(150));
        // Brother crop @100/@200 stays under the cap — ImageInformation == SOF.
        assert_eq!(job_page_pixels(100), (816, 1144));
        assert_eq!(job_page_pixels(200), (1648, 2289));
        // Platen / WIA extent remains full letter.
        assert_eq!(page_pixels(200), (1700, 2340));
    }

    #[test]
    fn wia_plain_pattern_parse_aliases() {
        assert_eq!(PatternId::parse("wia_plain").unwrap(), PatternId::WiaPlain);
        assert_eq!(PatternId::parse("plain").unwrap(), PatternId::WiaPlain);
        assert_eq!(PatternId::parse("wia-plain").unwrap(), PatternId::WiaPlain);
    }

    #[test]
    fn jfif_encode_matches_brother_structure() {
        // A real device's WSD JPEG: SOF0 4:2:0 (Y 2×2), APP0 units=dpi,
        // DRI = MCU columns, component ids 1/2/3. Prior image-crate encode was
        // 4:4:4 + aspect density 1×1 + no DRI (CancelJob after RetrieveImage).
        let dpi = std::env::var("FAKESCAN_DUMP_DPI")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(100u32);
        let bytes = encode_pattern(
            "jfif",
            PatternId::WiaPlain,
            dpi,
            0,
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WIA_PLAIN,
        )
        .unwrap();
        let comps = jpeg_sof0_components(&bytes);
        assert_eq!(
            comps,
            vec![(1, 2, 2), (2, 1, 1), (3, 1, 1)],
            "expected 4:2:0 with 1-based ids, got {comps:?}"
        );
        assert_eq!(
            jpeg_jfif_density(&bytes),
            Some((1, dpi as u16, dpi as u16)),
            "APP0 units=inches + dpi"
        );
        let (w, _) = job_page_pixels(dpi);
        let expect_dri = restart_interval_mcu_row(w);
        assert_eq!(jpeg_dri(&bytes), Some(expect_dri), "DRI = MCU row width");
        assert!(jpeg_is_baseline(&bytes));
        assert!(!jpeg_has_com(&bytes));
        let expect_dims = job_page_pixels(dpi);
        assert_eq!(
            jpeg_sof0_dimensions(&bytes),
            Some(expect_dims),
            "SOF must match ImageInformation optical crop job_page_pixels({dpi})"
        );
        // Brother MFC WIA JPEG: APP0 → DQT → DRI → SOF0 → DHT → SOS (packed tables).
        let markers = jpeg_segment_markers(&bytes);
        assert_eq!(
            markers,
            vec![0xD8, 0xE0, 0xDB, 0xDD, 0xC0, 0xC4, 0xDA],
            "Brother segment order/packing; got {markers:?}"
        );
        assert_eq!(
            markers.iter().filter(|&&m| m == 0xDB).count(),
            1,
            "one packed DQT"
        );
        assert_eq!(
            markers.iter().filter(|&&m| m == 0xC4).count(),
            1,
            "one packed DHT"
        );
        if let Ok(path) = std::env::var("FAKESCAN_DUMP_WIA") {
            std::fs::write(&path, &bytes).expect("dump wia jpeg");
        }
    }

    #[test]
    fn reshape_jpeg_packs_dqt_dht_and_reorders() {
        // jpeg-encoder emits split tables + SOF-before-tables; reshape packs/reorders.
        let w = 32u16;
        let h = 32u16;
        let rgb = vec![0x80u8; (w as usize) * (h as usize) * 3];
        let mut raw = Vec::new();
        {
            let mut enc = jpeg_encoder::Encoder::new(&mut raw, 80);
            enc.set_density(jpeg_encoder::PixelDensity::dpi(100));
            enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
            enc.set_restart_interval(restart_interval_mcu_row(w as u32));
            enc.encode(&rgb, w, h, jpeg_encoder::ColorType::Rgb)
                .unwrap();
        }
        let before = jpeg_segment_markers(&raw);
        let dqt_before = before.iter().filter(|&&m| m == 0xDB).count();
        let dht_before = before.iter().filter(|&&m| m == 0xC4).count();
        eprintln!("reshape before markers={before:?} DQT={dqt_before} DHT={dht_before}");
        assert!(
            dqt_before >= 2
                || dht_before >= 2
                || before != vec![0xD8, 0xE0, 0xDB, 0xDD, 0xC0, 0xC4, 0xDA],
            "precondition: jpeg-encoder still split/unordered; got {before:?}"
        );
        let packed = reshape_jpeg_brother_layout(&raw).unwrap();
        let after = jpeg_segment_markers(&packed);
        eprintln!("reshape after  markers={after:?}");
        assert_eq!(
            after,
            vec![0xD8, 0xE0, 0xDB, 0xDD, 0xC0, 0xC4, 0xDA],
            "after reshape; before was {before:?}"
        );
        assert_eq!(after.iter().filter(|&&m| m == 0xDB).count(), 1);
        assert_eq!(after.iter().filter(|&&m| m == 0xC4).count(), 1);
        // Entropy + EOI preserved (length roughly unchanged).
        assert!(packed.len() >= raw.len().saturating_sub(32));
        assert_eq!(&packed[packed.len() - 2..], &[0xFF, 0xD9]);
    }

    // ── Diagnostic pattern tests ────────────────────────────────────────────

    #[test]
    fn info_and_anac_parse_aliases() {
        // Primary ids
        assert_eq!(PatternId::parse("info").unwrap(), PatternId::Info);
        assert_eq!(PatternId::parse("anac").unwrap(), PatternId::Anac);
        // Alias
        assert_eq!(PatternId::parse("whoami").unwrap(), PatternId::Anac);
        // Roundtrip via as_str
        assert_eq!(
            PatternId::parse(PatternId::Info.as_str()).unwrap(),
            PatternId::Info
        );
        assert_eq!(
            PatternId::parse(PatternId::Anac.as_str()).unwrap(),
            PatternId::Anac
        );
    }

    #[test]
    fn info_and_anac_in_all_slice() {
        assert!(PatternId::ALL.contains(&PatternId::Info));
        assert!(PatternId::ALL.contains(&PatternId::Anac));
    }

    #[test]
    fn render_pattern_rgb_info_non_blank_and_distinct() {
        let (info_img, _) = render_pattern_rgb(PatternId::Info, 50, 0, None).unwrap();
        let (anac_img, _) = render_pattern_rgb(PatternId::Anac, 50, 0, None).unwrap();
        let (page_img, _) = render_pattern_rgb(PatternId::PageId, 50, 0, None).unwrap();

        // Must be valid non-empty images.
        assert!(info_img.width() > 0 && info_img.height() > 0);
        assert!(anac_img.width() > 0 && anac_img.height() > 0);

        // Must differ from each other and from an unrelated pattern.
        assert_ne!(
            info_img.as_raw(),
            anac_img.as_raw(),
            "info vs anac should differ"
        );
        assert_ne!(
            info_img.as_raw(),
            page_img.as_raw(),
            "info should differ from page_id"
        );
        assert_ne!(
            anac_img.as_raw(),
            page_img.as_raw(),
            "anac should differ from page_id"
        );
    }

    #[test]
    fn render_pattern_rgb_info_with_ctx_non_blank() {
        let ctx = PageContext {
            profile_id: "test-profile".into(),
            device_uuid: "a1b2c3d4-0000-0000-0000-000000000000".into(),
            pattern_id: "info".into(),
            dpi: 50,
            format: "jfif".into(),
            job_id: 7,
            peer: Some("192.168.1.42:12345".into()),
            timestamp_secs: 1_760_000_000,
        };
        let (img, _) = render_pattern_rgb_ctx(
            PatternId::Info,
            50,
            0,
            None,
            EncodeOpts::default(),
            Some(&ctx),
        )
        .unwrap();
        assert!(img.width() > 0 && img.height() > 0);

        // Without context the image should differ (contains placeholder text).
        let (img_no_ctx, _) = render_pattern_rgb(PatternId::Info, 50, 0, None).unwrap();
        assert_ne!(
            img.as_raw(),
            img_no_ctx.as_raw(),
            "ctx vs no-ctx info should differ"
        );
    }

    #[test]
    fn render_pattern_rgb_anac_with_ctx_shows_peer() {
        let ctx = PageContext {
            profile_id: "test-profile".into(),
            device_uuid: "00000000-0000-0000-0000-000000000001".into(),
            pattern_id: "anac".into(),
            dpi: 50,
            format: "jfif".into(),
            job_id: 3,
            peer: Some("10.0.0.1:9999".into()),
            timestamp_secs: 1_760_000_001,
        };
        let (img, _) = render_pattern_rgb_ctx(
            PatternId::Anac,
            50,
            0,
            None,
            EncodeOpts::default(),
            Some(&ctx),
        )
        .unwrap();
        assert!(img.width() > 0 && img.height() > 0);

        // Different peer → different pixels.
        let ctx2 = PageContext {
            peer: Some("10.0.0.2:9999".into()),
            ..ctx.clone()
        };
        let (img2, _) = render_pattern_rgb_ctx(
            PatternId::Anac,
            50,
            0,
            None,
            EncodeOpts::default(),
            Some(&ctx2),
        )
        .unwrap();
        assert_ne!(
            img.as_raw(),
            img2.as_raw(),
            "different peer should produce different pixels"
        );
    }

    #[test]
    fn qr_contains_client_when_ctx_has_peer() {
        use rqrr::PreparedImage;

        let ctx = PageContext {
            profile_id: "test-profile".into(),
            device_uuid: "00000000-0000-0000-0000-000000000002".into(),
            pattern_id: "info".into(),
            dpi: 150,
            format: "jfif".into(),
            job_id: 1,
            peer: Some("192.168.99.1:55000".into()),
            timestamp_secs: 1_760_000_002,
        };
        let jpeg = encode_pattern_pages_ctx(
            "jfif",
            PatternId::Info,
            150,
            &[0],
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
            Some(&ctx),
        )
        .unwrap();

        let img = image::load_from_memory(&jpeg).unwrap().to_luma8();
        let mut prepared = PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(!grids.is_empty(), "info pattern must stamp a QR");
        let (_meta, content) = grids[0].decode().expect("qr decode");
        assert!(
            content.contains("client=192.168.99.1:55000"),
            "QR payload must contain client= when peer is set; got: {content}"
        );
    }

    #[test]
    fn qr_no_client_when_no_peer() {
        use rqrr::PreparedImage;

        let jpeg = encode_pattern_pages_ctx(
            "jfif",
            PatternId::Info,
            150,
            &[0],
            None,
            None,
            crate::effects::ColorMode::Rgb24,
            EncodeOpts::WITH_COM,
            None, // no ctx → no client=
        )
        .unwrap();
        let img = image::load_from_memory(&jpeg).unwrap().to_luma8();
        let mut prepared = PreparedImage::prepare(img);
        let grids = prepared.detect_grids();
        assert!(
            !grids.is_empty(),
            "info pattern must stamp a QR even without ctx"
        );
        let (_meta, content) = grids[0].decode().expect("qr decode");
        assert!(
            !content.contains("client="),
            "QR payload must NOT contain client= when peer absent; got: {content}"
        );
    }
}

#[cfg(test)]
mod bilevel_tests {
    use super::*;
    use crate::effects::{BilevelPolarity, ColorMode, ImageEffects};

    const DPI: u32 = 100;

    fn fx(p: BilevelPolarity) -> ImageEffects {
        ImageEffects {
            bilevel_polarity: p,
            ..ImageEffects::default()
        }
    }

    fn enc(fmt: &str, color: ColorMode, p: BilevelPolarity, dpi: u32, sheets: &[u32]) -> Vec<u8> {
        encode_pattern_pages(
            fmt,
            PatternId::PageId,
            dpi,
            sheets,
            None,
            Some(&fx(p)),
            color,
            EncodeOpts::WITH_COM,
        )
        .unwrap()
    }

    /// Reference: the RGB buffer the pipeline produces for `color` (before packing).
    fn reference(color: ColorMode, dpi: u32, sheet: u32) -> RgbImage {
        let (mut img, _) = render_pattern_rgb_ctx(
            PatternId::PageId,
            dpi,
            sheet,
            None,
            EncodeOpts::WITH_COM,
            None,
        )
        .unwrap();
        crate::effects::apply_pipeline(&mut img, &ImageEffects::default(), sheet, color);
        img
    }

    fn u16le(b: &[u8], o: usize) -> u32 {
        u32::from(u16::from_le_bytes([b[o], b[o + 1]]))
    }
    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    struct Bmp {
        /// File stores rows top-down (negative biHeight).
        top_down: bool,
        w: u32,
        h: u32,
        bpp: u32,
        palette: Vec<u8>, // gray level per entry (B channel)
        stride: usize,
        pixels: Vec<u8>, // top-down rows, stride bytes each
        size_image: u32,
        clr_used: u32,
    }

    fn parse_bmp(b: &[u8]) -> Bmp {
        assert_eq!(&b[0..2], b"BM");
        assert_eq!(u32le(b, 14), 40, "BITMAPINFOHEADER");
        let w = u32le(b, 18);
        let h_raw = u32le(b, 22) as i32;
        let top_down = h_raw < 0;
        let h = h_raw.unsigned_abs();
        let bpp = u16le(b, 28);
        assert_eq!(u32le(b, 30), 0, "BI_RGB");
        let size_image = u32le(b, 34);
        let clr_used = u32le(b, 46);
        let off = u32le(b, 10) as usize;
        let n_pal = (off - 54) / 4;
        let palette: Vec<u8> = (0..n_pal).map(|i| b[54 + i * 4]).collect();
        let stride = (w * bpp).div_ceil(32) as usize * 4;
        assert_eq!(b.len(), off + stride * h as usize, "file size");
        let mut rows: Vec<&[u8]> = b[off..].chunks(stride).collect();
        if !top_down {
            rows.reverse(); // bottom-up -> top-down
        }
        Bmp {
            top_down,
            w,
            h,
            bpp,
            palette,
            stride,
            pixels: rows.concat(),
            size_image,
            clr_used,
        }
    }

    /// Visual gray level (0..255) of pixel (x, y) after applying the palette.
    fn bmp_visual(bmp: &Bmp, x: u32, y: u32) -> u8 {
        let row = &bmp.pixels[y as usize * bmp.stride..];
        let idx = if bmp.bpp == 1 {
            usize::from((row[(x / 8) as usize] >> (7 - x % 8)) & 1)
        } else {
            usize::from(row[x as usize])
        };
        bmp.palette[idx]
    }

    fn assert_matches_ref(bmp: &Bmp, r: &RgbImage) {
        assert_eq!((bmp.w, bmp.h), r.dimensions());
        for y in 0..bmp.h {
            for x in 0..bmp.w {
                assert_eq!(
                    bmp_visual(bmp, x, y),
                    r.get_pixel(x, y).0[0],
                    "visual mismatch at ({x},{y})"
                );
            }
        }
    }

    #[test]
    fn bmp_1bpp_header_stride_palette_and_bit_order() {
        let b = enc(
            "bmp",
            ColorMode::Bw1,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[0],
        );
        let bmp = parse_bmp(&b);
        let r = reference(ColorMode::Bw1, DPI, 0);
        assert_eq!(bmp.bpp, 1);
        assert!(
            bmp.top_down,
            "Brother 1-bpp BMPs are top-down (negative biHeight)"
        );
        assert_eq!(bmp.palette, vec![0, 255], "[black, white]");
        assert_eq!(bmp.clr_used, 2);
        assert_eq!(bmp.stride, (r.width().div_ceil(8)).div_ceil(4) as usize * 4);
        assert_eq!(bmp.size_image as usize, bmp.stride * bmp.h as usize);
        assert_matches_ref(&bmp, &r);
        // MSB-first: first pixel of the first row is bit 7 of byte 0.
        let first_white = r.get_pixel(0, 0).0[0] == 255;
        assert_eq!(bmp.pixels[0] >> 7 == 1, first_white);
        // Far smaller than the 24-bpp file.
        let rgb = enc(
            "bmp",
            ColorMode::Rgb24,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[0],
        );
        assert!(b.len() * 20 < rgb.len(), "{} vs {}", b.len(), rgb.len());
    }

    #[test]
    fn bmp_polarity_flips_palette_not_visual() {
        let a = parse_bmp(&enc(
            "bmp",
            ColorMode::Bw1,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[1],
        ));
        let w = parse_bmp(&enc(
            "bmp",
            ColorMode::Bw1,
            BilevelPolarity::WhiteIsZero,
            DPI,
            &[1],
        ));
        assert_eq!(a.palette, vec![0, 255]);
        assert_eq!(w.palette, vec![255, 0], "[white, black]");
        assert_eq!((a.bpp, w.bpp), (1, 1));
        assert_ne!(a.pixels, w.pixels, "stored bits are inverted");
        let r = reference(ColorMode::Bw1, DPI, 1);
        assert_matches_ref(&a, &r);
        assert_matches_ref(&w, &r);
    }

    #[test]
    fn bmp_8bpp_gray_palette_and_values() {
        let b = enc(
            "dib",
            ColorMode::Gray8,
            BilevelPolarity::WhiteIsZero,
            DPI,
            &[0],
        );
        let bmp = parse_bmp(&b);
        assert_eq!(bmp.bpp, 8);
        assert!(!bmp.top_down, "8-bpp stays bottom-up");
        assert_eq!(bmp.palette.len(), 256);
        assert!(bmp
            .palette
            .iter()
            .enumerate()
            .all(|(i, &v)| v as usize == i));
        assert_eq!(bmp.stride, bmp.w.div_ceil(4) as usize * 4);
        assert_matches_ref(&bmp, &reference(ColorMode::Gray8, DPI, 0));
        let levels: std::collections::HashSet<u8> = bmp.pixels.iter().copied().collect();
        assert!(levels.len() > 2, "{} levels", levels.len());
    }

    #[test]
    fn bmp_rgb24_unchanged() {
        let b = enc(
            "bmp",
            ColorMode::Rgb24,
            BilevelPolarity::WhiteIsZero,
            DPI,
            &[0],
        );
        assert_eq!(u16le(&b, 28), 24);
    }

    struct TiffPage {
        w: u32,
        h: u32,
        bps: u32,
        spp: u32,
        photometric: u32,
        compression: u32,
        rows_per_strip: u32,
        strip_bytes: u32,
        strip: Vec<u8>,
    }

    fn parse_tiff(b: &[u8]) -> Vec<TiffPage> {
        assert_eq!(&b[0..4], &[b'I', b'I', 42, 0]);
        let mut ifd = u32le(b, 4) as usize;
        let mut pages = Vec::new();
        while ifd != 0 {
            let n = u16le(b, ifd) as usize;
            let mut t = std::collections::HashMap::new();
            for i in 0..n {
                let e = ifd + 2 + i * 12;
                t.insert(u16le(b, e), (u16le(b, e + 2), u32le(b, e + 4), e + 8));
            }
            let opt = |tag: u32| {
                t.get(&tag).map(
                    |&(typ, _, at)| {
                        if typ == 3 {
                            u16le(b, at)
                        } else {
                            u32le(b, at)
                        }
                    },
                )
            };
            let val = |tag: u32| opt(tag).unwrap_or_else(|| panic!("missing tag {tag}"));
            let off = val(273) as usize;
            let len = opt(279).unwrap_or((b.len() - off) as u32);
            pages.push(TiffPage {
                w: val(256),
                h: val(257),
                bps: opt(258).unwrap_or(1),
                spp: opt(277).unwrap_or(1),
                photometric: opt(262).unwrap_or(0),
                compression: opt(259).unwrap_or(1),
                rows_per_strip: opt(278).unwrap_or(val(257)),
                strip_bytes: len,
                strip: b[off..off + len as usize].to_vec(),
            });
            ifd = u32le(b, ifd + 2 + n * 12) as usize;
        }
        pages
    }

    fn tiff_visual(p: &TiffPage, x: u32, y: u32) -> u8 {
        let stride = (p.w * p.bps).div_ceil(8) as usize;
        let row = &p.strip[y as usize * stride..];
        if p.bps == 8 {
            return row[x as usize];
        }
        let bit = (row[(x / 8) as usize] >> (7 - x % 8)) & 1;
        // Photometric 1 (MinIsBlack): 1 = white; 0 (MinIsWhite): 1 = black.
        if (bit == 1) == (p.photometric == 1) {
            255
        } else {
            0
        }
    }

    #[test]
    fn tiff_1bit_tags_and_both_polarities_same_visual() {
        let r = reference(ColorMode::Bw1, DPI, 0);
        for (pol, photo) in [
            (BilevelPolarity::BlackIsZero, 1),
            (BilevelPolarity::WhiteIsZero, 0),
        ] {
            let b = enc("tiff-single-uncompressed", ColorMode::Bw1, pol, DPI, &[0]);
            let pages = parse_tiff(&b);
            assert_eq!(pages.len(), 1);
            let p = &pages[0];
            assert_eq!((p.w, p.h), r.dimensions());
            assert_eq!((p.bps, p.spp, p.compression), (1, 1, 1));
            assert_eq!(p.photometric, photo, "{pol:?}");
            assert_eq!(p.rows_per_strip, p.h);
            assert_eq!(p.strip_bytes, r.width().div_ceil(8) * p.h);
            for y in 0..p.h {
                for x in 0..p.w {
                    assert_eq!(
                        tiff_visual(p, x, y),
                        r.get_pixel(x, y).0[0],
                        "{pol:?} ({x},{y})"
                    );
                }
            }
            let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&b)).unwrap();
            assert_eq!(dec.colortype().unwrap(), tiff::ColorType::Gray(1));
            assert_eq!(dec.dimensions().unwrap(), r.dimensions());
        }
    }

    #[test]
    fn tiff_8bit_gray_and_multi_page() {
        let b = enc(
            "tiff-multi-uncompressed",
            ColorMode::Gray8,
            BilevelPolarity::WhiteIsZero,
            DPI,
            &[0, 1],
        );
        let pages = parse_tiff(&b);
        assert_eq!(pages.len(), 2);
        for (i, p) in pages.iter().enumerate() {
            assert_eq!(
                (p.bps, p.spp, p.photometric),
                (8, 1, 1),
                "gray is always MinIsBlack"
            );
            assert_eq!(p.strip_bytes, p.w * p.h);
            let r = reference(ColorMode::Gray8, DPI, i as u32);
            assert_eq!((p.w, p.h), r.dimensions());
            assert_eq!(tiff_visual(p, 5, 7), r.get_pixel(5, 7).0[0]);
        }
        assert_ne!(pages[0].strip, pages[1].strip, "pages stay distinct");
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&b)).unwrap();
        assert_eq!(dec.colortype().unwrap(), tiff::ColorType::Gray(8));
        let rgb = enc(
            "tiff-multi-uncompressed",
            ColorMode::Rgb24,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[0, 1],
        );
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&rgb)).unwrap();
        assert_eq!(dec.colortype().unwrap(), tiff::ColorType::RGB(8));
    }

    #[test]
    fn png_1bit_and_8bit_gray() {
        for (color, depth) in [
            (ColorMode::Bw1, png::BitDepth::One),
            (ColorMode::Gray8, png::BitDepth::Eight),
        ] {
            let b = enc("png", color, BilevelPolarity::WhiteIsZero, DPI, &[0]);
            let dec = png::Decoder::new(std::io::Cursor::new(&b));
            let mut rd = dec.read_info().unwrap();
            let info = rd.info();
            assert_eq!(info.color_type, png::ColorType::Grayscale);
            assert_eq!(info.bit_depth, depth);
            let mut buf = vec![0u8; rd.output_buffer_size().unwrap()];
            let fi = rd.next_frame(&mut buf).unwrap();
            let r = reference(color, DPI, 0);
            let (w, h) = r.dimensions();
            assert_eq!((fi.width, fi.height), (w, h));
            let stride = fi.line_size;
            for y in (0..h).step_by(7) {
                for x in (0..w).step_by(3) {
                    let v = if color == ColorMode::Bw1 {
                        if (buf[y as usize * stride + (x / 8) as usize] >> (7 - x % 8)) & 1 == 1 {
                            255
                        } else {
                            0
                        }
                    } else {
                        buf[y as usize * stride + x as usize]
                    };
                    assert_eq!(v, r.get_pixel(x, y).0[0]);
                }
            }
            let li = image::load_from_memory(&b).unwrap();
            assert_eq!(li.width(), w);
        }
    }

    fn g4_bitmap(tiff_bytes: &[u8]) -> Vec<bool> {
        let p = &parse_tiff(tiff_bytes)[0];
        let mut out = Vec::new();
        fax::decoder::decode_g4(p.strip.iter().copied(), p.w as u16, Some(p.h as u16), |t| {
            out.extend(fax::decoder::pels(t, p.w as u16).map(|c| c == fax::Color::Black));
        })
        .unwrap();
        out
    }

    #[test]
    fn g4_still_decodes_to_the_thresholded_bitmap() {
        let r = reference(ColorMode::Bw1, DPI, 0);
        let want: Vec<bool> = r.pixels().map(|p| p.0[0] < 128).collect();
        let b = enc(
            "tiff-single-g4",
            ColorMode::Bw1,
            BilevelPolarity::WhiteIsZero,
            DPI,
            &[0],
        );
        let p = &parse_tiff(&b)[0];
        assert_eq!((p.compression, p.photometric, p.bps), (4, 0, 1));
        assert_eq!(g4_bitmap(&b), want);
        // Polarity does not touch CCITT output.
        assert_eq!(
            enc(
                "tiff-single-g4",
                ColorMode::Bw1,
                BilevelPolarity::BlackIsZero,
                DPI,
                &[0]
            ),
            b
        );
    }

    #[test]
    fn jfif_bw_modes_component_count_and_bilevel_samples() {
        use crate::effects::JfifBw;
        // gray8 (default): single component; legacy rgb24: Brother-style 3 components.
        // Both carry thresholded ~0/255 samples; polarity does not touch JPEG.
        for (mode, comps) in [(JfifBw::Gray8, 1usize), (JfifBw::Rgb24, 3)] {
            for pol in [BilevelPolarity::BlackIsZero, BilevelPolarity::WhiteIsZero] {
                for fmt in ["jfif", "jpeg", "exif"] {
                    let e = ImageEffects {
                        jfif_bw: mode,
                        ..fx(pol)
                    };
                    let b = encode_pattern_pages(
                        fmt,
                        PatternId::PageId,
                        DPI,
                        &[0],
                        None,
                        Some(&e),
                        ColorMode::Bw1,
                        EncodeOpts::WITH_COM,
                    )
                    .unwrap();
                    assert_eq!(jpeg_sof0_components(&b).len(), comps, "{fmt} {mode:?}");
                    let img = image::load_from_memory(&b).unwrap().to_luma8();
                    let total = img.pixels().count();
                    let extreme = img
                        .pixels()
                        .filter(|p| p.0[0] <= 40 || p.0[0] >= 215)
                        .count();
                    assert!(extreme * 100 >= total * 97, "{extreme}/{total} near 0/255");
                    let dark = img.pixels().filter(|p| p.0[0] < 128).count();
                    assert!(dark > 0 && dark * 2 < total, "mostly light with some dark");
                }
            }
        }
        // Default effects => gray8.
        let b = enc(
            "jfif",
            ColorMode::Bw1,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[0],
        );
        assert_eq!(jpeg_sof0_components(&b).len(), 1);
        assert_eq!(
            image::load_from_memory(&b).unwrap().color(),
            image::ColorType::L8
        );
    }

    #[test]
    fn bytes_per_line_fx_gray_modes() {
        use crate::effects::JfifBw as B;
        use crate::effects::JfifGray as J;
        for (mode, want) in [
            (J::Rgb24, 4944),
            (J::Gray8, 1648),
            (J::Gray8RgbStride, 4944),
            (J::Gray8Tiff, 4944),
        ] {
            assert_eq!(
                bytes_per_line_fx(
                    "jfif",
                    1648,
                    ColorMode::Gray8,
                    false,
                    mode,
                    crate::effects::JfifBw::default()
                ),
                want,
                "{mode:?}"
            );
        }
        // BW1 JFIF follows jfif_bw: gray8 -> width, rgb24 -> width x 3; 1-bit formats unchanged.
        for (bw, want) in [
            (crate::effects::JfifBw::Gray8, 1648),
            (crate::effects::JfifBw::Rgb24, 4944),
        ] {
            for f in ["jfif", "jpeg", "exif"] {
                assert_eq!(
                    bytes_per_line_fx(f, 1648, ColorMode::Bw1, false, J::Gray8, bw),
                    want,
                    "{f} {bw:?}"
                );
            }
            assert_eq!(
                bytes_per_line_fx(
                    "tiff-single-uncompressed",
                    1648,
                    ColorMode::Bw1,
                    false,
                    J::Gray8,
                    bw
                ),
                206
            );
        }
        // Only Gray8 tickets are affected; RGB JFIF and uncompressed formats are not.
        let g = ColorMode::Gray8;
        assert_eq!(
            bytes_per_line_fx(
                "jfif",
                1648,
                ColorMode::Rgb24,
                false,
                J::Gray8,
                B::default()
            ),
            4944
        );
        assert_eq!(
            bytes_per_line_fx("exif", 1648, g, false, J::Gray8, B::default()),
            1648
        );
        assert_eq!(
            bytes_per_line_fx(
                "tiff-single-uncompressed",
                1648,
                g,
                false,
                J::Gray8Tiff,
                B::default()
            ),
            1648
        );
        assert_eq!(
            bytes_per_line_fx("jfif", 1648, g, true, J::Gray8, B::default()),
            0
        );
    }

    #[test]
    fn jfif_gray_mode_encodes_single_component_jpeg() {
        let r = reference(ColorMode::Gray8, DPI, 0);
        let mut pages = vec![(r.clone(), 0u32, None)];
        let spec = PixelSpec {
            color: ColorMode::Gray8,
            polarity: BilevelPolarity::BlackIsZero,
            jfif_gray: crate::effects::JfifGray::Gray8,
            ..PixelSpec::default()
        };
        for f in ["jfif", "exif"] {
            let b = encode_images(f, &pages, 95, EncodeOpts::WITH_COM, DPI, spec).unwrap();
            assert_eq!(jpeg_sof0_components(&b).len(), 1, "{f}");
            let img = image::load_from_memory(&b).unwrap();
            assert_eq!(img.color(), image::ColorType::L8);
            assert_eq!((img.width(), img.height()), r.dimensions());
            // Samples track the pattern luminance.
            let got = img.to_luma8();
            let n = got.pixels().count() as u64;
            let diff: u64 = got
                .pixels()
                .zip(r.pixels())
                .map(|(a, b)| u64::from(a.0[0].abs_diff(b.0[0])))
                .sum();
            assert!(diff / n < 6, "mean abs diff {}", diff / n);
        }
        // Default spec keeps three components for gray tickets.
        pages.truncate(1);
        let b = encode_images(
            "jfif",
            &pages,
            95,
            EncodeOpts::WITH_COM,
            DPI,
            PixelSpec {
                jfif_gray: crate::effects::JfifGray::Rgb24,
                ..spec
            },
        )
        .unwrap();
        assert_eq!(jpeg_sof0_components(&b).len(), 3);
    }

    #[test]
    fn bytes_per_line_table() {
        use ColorMode::*;
        let t = [
            ("bmp", 1648, Bw1, 208), // ceil(1648/8)=206 -> 208
            ("dib", 1648, Bw1, 208),
            ("bmp", 1648, Gray8, 1648),
            ("bmp", 1648, Rgb24, 4944),
            ("bmp", 850, Rgb24, 2552), // 2550 padded to DWORD
            ("bmp", 850, Gray8, 852),
            ("bmp", 900, Bw1, 116), // 113 padded
            ("tiff-single-uncompressed", 1648, Bw1, 206),
            ("tiff-single-uncompressed", 1648, Gray8, 1648),
            ("tiff-single-uncompressed", 1648, Rgb24, 4944),
            ("tiff-multi-uncompressed", 900, Bw1, 113),
            ("png", 900, Bw1, 113),
            ("jfif", 1648, Bw1, 1648), // jfif_bw default gray8
            ("jfif", 1648, Gray8, 1648),
        ];
        for (f, w, c, want) in t {
            assert_eq!(bytes_per_line(f, w, c, false), want, "{f} {w} {c:?}");
        }
        assert_eq!(bytes_per_line("bmp", 1648, Bw1, true), 208);
        assert_eq!(bytes_per_line("jfif", 1648, Bw1, true), 0);
    }

    #[test]
    fn qr_survives_thresholding_to_1bit_bmp() {
        for dpi in [200u32, 100] {
            let b = enc(
                "bmp",
                ColorMode::Bw1,
                BilevelPolarity::WhiteIsZero,
                dpi,
                &[3],
            );
            let bmp = parse_bmp(&b);
            let img = image::GrayImage::from_fn(bmp.w, bmp.h, |x, y| {
                image::Luma([bmp_visual(&bmp, x, y)])
            });
            let mut prep = rqrr::PreparedImage::prepare(img);
            let grids = prep.detect_grids();
            let decoded = grids.first().and_then(|g| g.decode().ok());
            eprintln!(
                "QR after 1-bit threshold at {dpi} dpi: {}",
                decoded.is_some()
            );
            if dpi == 200 {
                let (_m, content) = decoded.expect("QR must decode at default dpi");
                assert_eq!(parse_qr_payload(&content).unwrap().page, 3);
            }
        }
    }

    #[test]
    fn consecutive_bw_pages_differ() {
        let a = enc(
            "bmp",
            ColorMode::Bw1,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[0],
        );
        let b = enc(
            "bmp",
            ColorMode::Bw1,
            BilevelPolarity::BlackIsZero,
            DPI,
            &[1],
        );
        assert_ne!(a, b);
    }

    #[test]
    fn polarity_parse_and_effects_body() {
        use crate::effects::EffectsBody;
        assert_eq!(
            BilevelPolarity::parse("white_is_zero").unwrap(),
            BilevelPolarity::WhiteIsZero
        );
        assert_eq!(
            BilevelPolarity::parse("MinIsBlack").unwrap(),
            BilevelPolarity::BlackIsZero
        );
        assert!(BilevelPolarity::parse("sideways").is_err());
        let mut e = ImageEffects::default();
        e.apply_body(&EffectsBody {
            bilevel_polarity: Some("white_is_zero".into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(e.bilevel_polarity, BilevelPolarity::WhiteIsZero);
        assert!(e.is_clean(), "polarity is not a pixel defect");
        assert_eq!(e.summary().bilevel_polarity, "white_is_zero");
        e.apply_preset("dirty_adf").unwrap();
        assert_eq!(e.bilevel_polarity, BilevelPolarity::WhiteIsZero);
        e.apply_body(&EffectsBody {
            clear: true,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(e.bilevel_polarity, BilevelPolarity::BlackIsZero);
        assert!(e
            .apply_body(&EffectsBody {
                bilevel_polarity: Some("x".into()),
                ..Default::default()
            })
            .is_err());
    }
}

/// Container-level checks: TIFF TN2 photometric, resolution metadata, PDF/XPS geometry,
/// gray/bilevel stream shapes.
#[cfg(test)]
mod container_tests {
    use super::*;
    use crate::effects::{ColorMode, ImageEffects};
    use std::collections::HashMap;

    fn enc(fmt: &str, color: ColorMode, dpi: u32, sheets: &[u32]) -> Vec<u8> {
        encode_pattern_pages(
            fmt,
            PatternId::PageId,
            dpi,
            sheets,
            None,
            Some(&ImageEffects::default()),
            color,
            EncodeOpts::WITH_COM,
        )
        .unwrap()
    }

    fn reference(color: ColorMode, dpi: u32, sheet: u32) -> RgbImage {
        let (mut img, _) = render_pattern_rgb_ctx(
            PatternId::PageId,
            dpi,
            sheet,
            None,
            EncodeOpts::WITH_COM,
            None,
        )
        .unwrap();
        crate::effects::apply_pipeline(&mut img, &ImageEffects::default(), sheet, color);
        img
    }

    fn u16le(b: &[u8], o: usize) -> u32 {
        u32::from(u16::from_le_bytes([b[o], b[o + 1]]))
    }
    fn u32le(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    /// One IFD: tag -> (type, count, value-field offset).
    type Ifd = HashMap<u32, (u32, u32, usize)>;

    fn ifds(b: &[u8]) -> Vec<Ifd> {
        assert_eq!(&b[0..4], &[b'I', b'I', 42, 0]);
        let mut at = u32le(b, 4) as usize;
        let mut out = Vec::new();
        while at != 0 {
            let n = u16le(b, at) as usize;
            let mut prev = 0;
            let mut m = Ifd::new();
            for i in 0..n {
                let e = at + 2 + i * 12;
                let tag = u16le(b, e);
                assert!(tag > prev, "IFD tags must be sorted ({tag} after {prev})");
                prev = tag;
                m.insert(tag, (u16le(b, e + 2), u32le(b, e + 4), e + 8));
            }
            out.push(m);
            at = u32le(b, at + 2 + n * 12) as usize;
        }
        out
    }

    fn short(b: &[u8], ifd: &Ifd, tag: u32) -> u32 {
        let (typ, _, at) = ifd[&tag];
        assert_eq!(typ, 3, "tag {tag} is a SHORT");
        u16le(b, at)
    }

    fn rational(b: &[u8], ifd: &Ifd, tag: u32) -> (u32, u32) {
        let (typ, _, at) = ifd[&tag];
        assert_eq!(typ, 5, "tag {tag} is a RATIONAL");
        let off = u32le(b, at) as usize;
        (u32le(b, off), u32le(b, off + 4))
    }

    #[test]
    fn a_reencoded_upload_keeps_the_resolution_it_carries_whatever_its_width() {
        // An A4-wide page at 100 dpi and an A5-wide page at 200 dpi: inferring from a letter width gave 97 and 137.
        for (width, dpi) in [(827u32, 100u32), (1165, 200), (850, 100)] {
            let img = RgbImage::from_pixel(width, 40, image::Rgb([200, 200, 200]));
            let jfif = encode_images(
                "jfif",
                &[(img, 0, None)],
                90,
                EncodeOpts::default(),
                dpi,
                PixelSpec::default(),
            )
            .expect("jfif");
            assert_eq!(image_dpi(&jfif), Some(dpi), "{width} px at {dpi} dpi");
            let decoded = image::load_from_memory(&jfif).expect("decode").to_rgb8();
            let tiff = encode_image_at(
                "tiff-single-uncompressed",
                decoded,
                0,
                None,
                image_dpi(&jfif),
            )
            .expect("tiff");
            assert_tiff_dpi(&tiff, dpi, &format!("{width} px"));
        }
        // Without a density the old inference still applies.
        assert_eq!(image_dpi(b"not an image"), None);
    }

    fn assert_tiff_dpi(b: &[u8], dpi: u32, label: &str) {
        let pages = ifds(b);
        assert!(!pages.is_empty());
        for ifd in &pages {
            assert_eq!(short(b, ifd, 296), 2, "{label}: ResolutionUnit inch");
            assert_eq!(rational(b, ifd, 282), (dpi, 1), "{label}: XResolution");
            assert_eq!(rational(b, ifd, 283), (dpi, 1), "{label}: YResolution");
        }
    }

    fn strip(b: &[u8], ifd: &Ifd) -> Vec<u8> {
        let off = u32le(b, ifd[&273].2) as usize;
        let len = u32le(b, ifd[&279].2) as usize;
        b[off..off + len].to_vec()
    }

    #[test]
    fn tn2_color_tiff_is_ycbcr_with_matching_subsampling() {
        for fmt in ["tiff-single-jpeg-tn2", "tiff-multi-jpeg-tn2"] {
            let sheets: &[u32] = if fmt.contains("multi") { &[0, 1] } else { &[0] };
            let b = enc(fmt, ColorMode::Rgb24, 100, sheets);
            let pages = ifds(&b);
            assert_eq!(pages.len(), sheets.len());
            for ifd in &pages {
                assert_eq!(short(&b, ifd, 259), 7, "{fmt}: JPEG");
                assert_eq!(short(&b, ifd, 262), 6, "{fmt}: Photometric YCbCr, not RGB");
                assert_eq!(short(&b, ifd, 277), 3);
                assert_eq!(ifd[&258].1, 3, "BitsPerSample count");
                // YCbCrSubSampling must equal the stream's luma sampling factors.
                let (typ, count, at) = ifd[&530];
                assert_eq!((typ, count), (3, 2), "YCbCrSubSampling SHORT[2]");
                let sub = (u16le(&b, at), u16le(&b, at + 2));
                let jpeg = strip(&b, ifd);
                let comps = jpeg_sof0_components(&jpeg);
                assert_eq!(comps.len(), 3);
                let (_, h, v) = comps[0];
                assert_eq!(sub, (u32::from(h), u32::from(v)), "tag 530 == SOF luma H,V");
                assert_eq!(sub, (2, 2), "4:2:0");
                assert_eq!(short(&b, ifd, 531), 1, "YCbCrPositioning centred");
            }
        }
    }

    #[test]
    fn tn2_color_tiff_decodes_with_correct_colours_independently() {
        // Decode the finished TIFF with the independent `tiff` crate (JPEG via zune) and
        // compare the mean of every channel with the rendered page.
        let b = enc("tiff-single-jpeg-tn2", ColorMode::Rgb24, 100, &[0]);
        let want = reference(ColorMode::Rgb24, 100, 0);
        let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&b)).expect("tiff decode");
        let (w, h) = dec.dimensions().unwrap();
        assert_eq!((w, h), want.dimensions());
        let ct = dec.colortype().unwrap();
        let raw = match dec.read_image().expect("read_image") {
            tiff::decoder::DecodingResult::U8(v) => v,
            _ => panic!("unexpected sample type"),
        };
        assert_eq!(raw.len(), (w * h * 3) as usize, "{ct:?}");
        // The tiff crate honours Photometric=YCbCr and hands back the stream's YCbCr samples
        // (it would refuse or mis-label them had the IFD said RGB); convert with JFIF BT.601.
        assert_eq!(ct, tiff::ColorType::YCbCr(8));
        let ycc = |p: &[u8]| -> [u8; 3] {
            let (y, cb, cr) = (
                f32::from(p[0]),
                f32::from(p[1]) - 128.0,
                f32::from(p[2]) - 128.0,
            );
            let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
            [
                q(y + 1.402 * cr),
                q(y - 0.344_136 * cb - 0.714_136 * cr),
                q(y + 1.772 * cb),
            ]
        };
        let mean = |px: &mut dyn Iterator<Item = [u8; 3]>| {
            let (mut s, mut n) = ([0u64; 3], 0u64);
            for p in px {
                for c in 0..3 {
                    s[c] += u64::from(p[c]);
                }
                n += 1;
            }
            [s[0] / n, s[1] / n, s[2] / n]
        };
        let got = mean(&mut raw.as_chunks::<3>().0.iter().map(|p| ycc(p)));
        let exp = mean(&mut want.pixels().map(|p| p.0));
        for c in 0..3 {
            assert!(
                got[c].abs_diff(exp[c]) <= 3,
                "channel {c}: decoded mean {} vs rendered {} ({ct:?})",
                got[c],
                exp[c]
            );
        }
    }

    #[test]
    fn tn2_gray_and_bw_tiff_are_single_component_black_is_zero() {
        for color in [ColorMode::Gray8, ColorMode::Bw1] {
            let b = enc("tiff-multi-jpeg-tn2", color, 100, &[0, 1]);
            let pages = ifds(&b);
            assert_eq!(pages.len(), 2);
            for ifd in &pages {
                assert_eq!(short(&b, ifd, 262), 1, "{color:?}: BlackIsZero");
                assert_eq!(short(&b, ifd, 277), 1);
                assert_eq!(short(&b, ifd, 258), 8);
                assert!(!ifd.contains_key(&530), "no YCbCr tags for gray");
                assert_eq!(jpeg_sof0_components(&strip(&b, ifd)).len(), 1, "{color:?}");
            }
            let mut dec = tiff::decoder::Decoder::new(std::io::Cursor::new(&b)).unwrap();
            assert_eq!(dec.colortype().unwrap(), tiff::ColorType::Gray(8));
        }
    }

    #[test]
    fn tn2_tiff_bytes_are_deterministic() {
        for color in [ColorMode::Rgb24, ColorMode::Gray8] {
            assert_eq!(
                enc("tiff-multi-jpeg-tn2", color, 100, &[0, 1]),
                enc("tiff-multi-jpeg-tn2", color, 100, &[0, 1])
            );
        }
    }

    #[test]
    fn every_tiff_flavour_carries_the_real_dpi() {
        for dpi in [100u32, 150] {
            for (fmt, color) in [
                ("tiff-single-uncompressed", ColorMode::Rgb24),
                ("tiff-multi-uncompressed", ColorMode::Rgb24),
                ("tiff-single-uncompressed", ColorMode::Gray8),
                ("tiff-multi-uncompressed", ColorMode::Bw1),
                ("tiff-single-g4", ColorMode::Bw1),
                ("tiff-multi-g4", ColorMode::Bw1),
                ("tiff-single-g3mh", ColorMode::Bw1),
                ("tiff-single-jpeg-tn2", ColorMode::Rgb24),
                ("tiff-multi-jpeg-tn2", ColorMode::Gray8),
            ] {
                let sheets: &[u32] = if fmt.contains("multi") { &[0, 1] } else { &[0] };
                let b = enc(fmt, color, dpi, sheets);
                assert_tiff_dpi(&b, dpi, &format!("{fmt} {color:?} @{dpi}"));
            }
        }
    }

    #[test]
    fn jfif_density_png_phys_and_bmp_ppm_carry_the_dpi() {
        // 150 dpi -> 5906 px/m (rounded), 100 -> 3937.
        for (dpi, ppm) in [(100u32, 3937u32), (150, 5906)] {
            for color in [ColorMode::Rgb24, ColorMode::Gray8, ColorMode::Bw1] {
                let j = enc("jfif", color, dpi, &[0]);
                assert_eq!(
                    jpeg_jfif_density(&j),
                    Some((1, dpi as u16, dpi as u16)),
                    "jfif {color:?}"
                );
                let png = enc("png", color, dpi, &[0]);
                let rd = png::Decoder::new(std::io::Cursor::new(&png))
                    .read_info()
                    .unwrap();
                let pd = rd.info().pixel_dims.expect("pHYs");
                assert_eq!(
                    (pd.xppu, pd.yppu, pd.unit),
                    (ppm, ppm, png::Unit::Meter),
                    "png {color:?}"
                );
                let bmp = enc("bmp", color, dpi, &[0]);
                assert_eq!(u32le(&bmp, 38), ppm, "bmp X ppm {color:?}");
                assert_eq!(u32le(&bmp, 42), ppm, "bmp Y ppm {color:?}");
            }
        }
    }

    #[test]
    fn bmp_24bpp_stays_bottom_up_and_pixel_exact() {
        let b = enc("bmp", ColorMode::Rgb24, 100, &[0]);
        assert_eq!(u16le(&b, 28), 24);
        assert!((u32le(&b, 22) as i32) > 0, "24-bpp stays bottom-up");
        let want = reference(ColorMode::Rgb24, 100, 0);
        let got = image::load_from_memory_with_format(&b, image::ImageFormat::Bmp)
            .unwrap()
            .to_rgb8();
        assert_eq!(got, want, "BGR order and row order");
    }

    #[test]
    fn gray_and_bw_jpeg2k_are_single_component() {
        // SIZ marker (FF51): Csiz sits 38 bytes after the marker's first byte.
        let csiz = |b: &[u8]| {
            let at = b.windows(2).position(|w| w == [0xFF, 0x51]).expect("SIZ");
            u16::from_be_bytes([b[at + 38], b[at + 39]])
        };
        assert_eq!(csiz(&enc("jpeg2k", ColorMode::Rgb24, 100, &[0])), 3);
        assert_eq!(csiz(&enc("jpeg2k", ColorMode::Gray8, 100, &[0])), 1);
        assert_eq!(csiz(&enc("jpeg2k", ColorMode::Bw1, 100, &[0])), 1);
    }

    #[test]
    fn jfif_gray_and_bw_defaults_are_one_component() {
        // Gray8 JFIF follows the jfif_gray effect (default gray8 -> 1 component) ...
        assert_eq!(
            jpeg_sof0_components(&enc("jfif", ColorMode::Gray8, 100, &[0])).len(),
            1
        );
        // ... Bw1 JFIF follows jfif_bw (default gray8 -> 1 component; rgb24 is covered in
        // bilevel_tests::jfif_bw_modes_*).
        assert_eq!(
            jpeg_sof0_components(&enc("jfif", ColorMode::Bw1, 100, &[0])).len(),
            1
        );
    }

    #[test]
    fn pdf_mediabox_is_physical_points_and_colorspace_follows_color() {
        // 100 dpi page -> pixels * 72 / 100 points.
        let (pw, ph) = capped_page_pixels(100);
        let want_w = fmt_units(f64::from(pw) * 72.0 / 100.0);
        let want_h = fmt_units(f64::from(ph) * 72.0 / 100.0);
        for (color, cs, other) in [
            (ColorMode::Rgb24, "/ColorSpace /DeviceRGB", "DeviceGray"),
            (ColorMode::Gray8, "/ColorSpace /DeviceGray", "DeviceRGB"),
            (ColorMode::Bw1, "/ColorSpace /DeviceGray", "DeviceRGB"),
        ] {
            let b = enc("pdf-a", color, 100, &[0, 1]);
            let t = String::from_utf8_lossy(&b).into_owned();
            let mb = format!("/MediaBox [0 0 {want_w} {want_h}]");
            assert_eq!(t.matches(&mb).count(), 2, "{color:?}: {mb}");
            assert!(
                t.contains(&format!("{want_w} 0 0 {want_h} 0 0 cm")),
                "{color:?}: image fills the page"
            );
            assert_eq!(t.matches(cs).count(), 2, "{color:?}: {cs}");
            assert!(t.contains(&format!("/Width {pw} /Height {ph}")));
            assert!(!t.contains(other), "{color:?}: no stray {other}");
        }
        // 200 dpi letter is ~612x792 pt, not ~1700x2200.
        let b = enc("pdf", ColorMode::Rgb24, 200, &[0]);
        let t = String::from_utf8_lossy(&b).into_owned();
        let mb = t.split("/MediaBox [0 0 ").nth(1).unwrap();
        let w: f64 = mb.split(' ').next().unwrap().parse().unwrap();
        assert!((590.0..=616.0).contains(&w), "MediaBox width {w}pt");
    }

    #[test]
    fn xps_page_is_in_96ths_of_an_inch_with_matching_brush() {
        use std::io::Read;
        for (dpi, color) in [(100u32, ColorMode::Rgb24), (200, ColorMode::Gray8)] {
            let b = enc("xps", color, dpi, &[0, 1]);
            let mut z = zip::ZipArchive::new(std::io::Cursor::new(b)).unwrap();
            let (pw, ph) = capped_page_pixels(dpi);
            let w = fmt_units(f64::from(pw) * 96.0 / f64::from(dpi));
            let h = fmt_units(f64::from(ph) * 96.0 / f64::from(dpi));
            for page in 1..=2 {
                let mut s = String::new();
                z.by_name(&format!("Documents/1/Pages/{page}.fpage"))
                    .unwrap()
                    .read_to_string(&mut s)
                    .unwrap();
                assert!(s.contains(&format!("Width=\"{w}\" Height=\"{h}\"")), "{s}");
                assert!(s.contains(&format!("M 0,0 L {w},0 {w},{h} 0,{h} Z")), "{s}");
                assert!(s.contains(&format!("Viewbox=\"0,0,{w},{h}\"")), "{s}");
                assert!(s.contains(&format!("Viewport=\"0,0,{w},{h}\"")), "{s}");
                let mut jpg = Vec::new();
                z.by_name(&format!("Documents/1/Resources/{page}.jpg"))
                    .unwrap()
                    .read_to_end(&mut jpg)
                    .unwrap();
                // Natural image size = pixels * 96 / dpi relies on the JPEG's own density.
                assert_eq!(jpeg_jfif_density(&jpg), Some((1, dpi as u16, dpi as u16)));
                assert_eq!(
                    jpeg_sof0_components(&jpg).len(),
                    if color == ColorMode::Rgb24 { 3 } else { 1 }
                );
            }
        }
    }
}
