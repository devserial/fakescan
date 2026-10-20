use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub label: String,
    pub connection: String,
    pub optical: Option<Optical>,
    pub feeder: FeederCaps,
    pub dpi: Vec<u32>,
    pub formats: Vec<String>,
    pub page_sizes: Vec<String>,
    pub sources: Vec<String>,
    /// WSD ColorProcessing tokens this profile accepts (`RGB24`, `Grayscale8`,
    /// `BlackAndWhite1`). Empty / omitted → any token soft-maps via
    /// [`crate::effects::ColorMode::parse`] (no allow-list).
    #[serde(default)]
    pub colors: Vec<String>,
    /// When true, ValidateScanTicket / CreateScanJob refuse dpi, format, or
    /// color values that are not in this profile's lists (default true).
    #[serde(default = "default_refuse_unknown")]
    pub refuse_unknown: bool,
    /// When true (default), JFIF / JPEG embeds inject `fakescan-page:` COM
    /// markers. Set false for WIA experiments that suspect COM breaks accept.
    /// Overridable at runtime via `POST /v1/jpeg-com` or `FAKESCAN_JPEG_COM`.
    #[serde(default = "default_jpeg_com")]
    pub jpeg_com: bool,
    /// When true, ImageInformation `BytesPerLine` is 0 for compressed formats
    /// (MS remarks). Default false keeps Brother/wsdscdrv stride (JFIF =
    /// width×3).
    #[serde(default)]
    pub bytes_per_line_ms: bool,
}

fn default_refuse_unknown() -> bool {
    true
}

fn default_jpeg_com() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Optical {
    pub x_dpi: u32,
    pub y_dpi: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeederCaps {
    pub sticky_ready: bool,
    pub pages_min: u32,
    pub pages_max: u32,
    /// Hardware ADF duplex (`ADFSupportsDuplex` + `ADFBack`; `InputSource` ADFDuplex).
    /// Distinct from manual duplex (`POST /v1/pages` side/order + pdfcraft `--reverse`).
    #[serde(default)]
    pub duplex: bool,
    /// Sheets the tray physically holds. Absent = no tray limit beyond `pages_max`.
    /// Loading more via `POST /v1/pages` is refused (see `PagesError`).
    #[serde(default)]
    pub capacity: Option<u32>,
    /// Sheets loaded into the tray when the device's world is created and on every reset or
    /// profile reload (clamped to `capacity` / `pages_max`; a loaded tray also makes the
    /// feeder ready). Default 0 = start empty. Runtime state, not a published capability: it
    /// is in neither registry fingerprint. A device or the daemon can override it
    /// (`--initial-pages`, `DeviceConfig::initial_pages`).
    #[serde(default)]
    pub initial_pages: u32,
    /// Sheet sizes the ADF accepts (`letter`, `a4`, `legal`). Absent/empty = unrestricted
    /// (today's behavior: ADF max 8500x14000, any InputMediaSize).
    #[serde(default)]
    pub media_sizes: Option<Vec<String>>,
}

/// Sheet sizes a feeder profile may list in `feeder.media_sizes`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaSize {
    Letter,
    A4,
    Legal,
}

impl MediaSize {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "letter" => Some(Self::Letter),
            "a4" => Some(Self::A4),
            "legal" => Some(Self::Legal),
            _ => None,
        }
    }

    /// (width, height) in 1/1000 inch (WSD InputMediaSize units). Letter height matches
    /// [`crate::image_gen::LETTER_H_THOU`] (live Brother 11700).
    pub fn dims_thou(self) -> (u32, u32) {
        match self {
            Self::Letter => (
                crate::image_gen::LETTER_W_THOU,
                crate::image_gen::LETTER_H_THOU,
            ),
            Self::A4 => (8267, 11692),
            Self::Legal => (
                crate::image_gen::LETTER_W_THOU,
                crate::image_gen::ADF_MAX_H_THOU,
            ),
        }
    }

    /// True when a ticket InputMediaSize is this size (width +-100, height +-800).
    pub fn matches(self, w: u32, h: u32) -> bool {
        let (sw, sh) = self.dims_thou();
        w.abs_diff(sw) <= 100 && h.abs_diff(sh) <= 800
    }
}

impl FeederCaps {
    /// Parsed `media_sizes`; `None` = unrestricted.
    pub fn media_size_list(&self) -> Option<Vec<MediaSize>> {
        let list = self.media_sizes.as_ref()?;
        if list.is_empty() {
            return None;
        }
        Some(list.iter().filter_map(|s| MediaSize::parse(s)).collect())
    }

    /// True when `w`x`h` (1/1000 inch) is acceptable to the ADF.
    pub fn accepts_media(&self, w: u32, h: u32) -> bool {
        match self.media_size_list() {
            None => true,
            Some(list) => list.iter().any(|m| m.matches(w, h)),
        }
    }

    fn validate(&self) -> Result<()> {
        if let Some(cap) = self.capacity {
            if cap == 0 {
                bail!("feeder.capacity must be > 0 (omit it for no tray limit)");
            }
            if self.pages_max > cap {
                bail!(
                    "feeder.pages_max ({}) exceeds feeder.capacity ({cap})",
                    self.pages_max
                );
            }
        }
        if let Some(list) = &self.media_sizes {
            for s in list {
                if MediaSize::parse(s).is_none() {
                    bail!("feeder.media_sizes: unknown size {s:?} (allowed: letter, a4, legal)");
                }
            }
        }
        Ok(())
    }
}

/// A profile id is one file name under the profiles directory: `[A-Za-z0-9_-]`, at most 64
/// characters. Slashes, `..` and other path syntax are rejected so `POST /v1/profile` cannot
/// read outside that directory.
pub fn valid_profile_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

impl Profile {
    pub fn load(dir: &Path, id: &str) -> Result<Self> {
        if !valid_profile_id(id) {
            bail!("invalid profile id {id:?} (use [A-Za-z0-9_-], one file name)");
        }
        let path = dir.join(format!("{id}.json"));
        // `valid_profile_id` already forbids separators. This catches a symlink planted in the
        // profiles directory that points outside it.
        if path.is_file() {
            if let (Ok(root), Ok(canon)) = (dir.canonicalize(), path.canonicalize()) {
                if !canon.starts_with(&root) {
                    bail!(
                        "profile {id:?} resolves outside the profiles directory {}",
                        dir.display()
                    );
                }
            }
        }
        if !path.is_file() {
            bail!("profile not found: {}", path.display());
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let profile: Profile =
            serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
        if profile.id != id {
            bail!(
                "profile id mismatch: file {} declares id {:?}",
                path.display(),
                profile.id
            );
        }
        profile
            .feeder
            .validate()
            .with_context(|| format!("profile {}", path.display()))?;
        Ok(profile)
    }

    pub fn list(dir: &Path) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        if !dir.is_dir() {
            return Ok(ids);
        }
        for entry in fs::read_dir(dir).with_context(|| format!("list {}", dir.display()))? {
            let entry = entry?;
            let path: PathBuf = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                ids.push(stem.to_string());
            }
        }
        ids.sort();
        Ok(ids)
    }
}

#[cfg(test)]
mod tray_tests {
    use super::*;

    fn dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
    }

    #[test]
    fn shipped_tray_profiles_load() {
        for (id, cap, n_sizes, duplex) in [
            ("feeder-tray5", 5, 1, false),
            ("feeder-tray50", 50, 3, false),
            ("feeder-duplex30", 30, 3, true),
            ("feeder-a4only", 20, 1, false),
        ] {
            let p = Profile::load(&dir(), id).expect(id);
            assert_eq!(p.feeder.capacity, Some(cap), "{id}");
            assert_eq!(p.feeder.media_size_list().unwrap().len(), n_sizes, "{id}");
            assert_eq!(p.feeder.duplex, duplex, "{id}");
            assert_eq!(p.connection, "wsd");
            assert!(p.formats.iter().any(|f| f == "jfif"));
        }
    }

    #[test]
    fn brother_wsd_has_no_tray_limit() {
        let p = Profile::load(&dir(), "brother-wsd").unwrap();
        assert_eq!(p.feeder.capacity, None);
        assert!(p.feeder.media_size_list().is_none());
        assert!(!p.feeder.duplex);
    }

    #[test]
    fn invalid_feeder_blocks_are_rejected() {
        let tmp = std::env::temp_dir().join(format!("fakescan-prof-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let base = std::fs::read_to_string(dir().join("brother-wsd.json")).unwrap();
        for (name, feeder, needle) in [
            (
                "bad-cap",
                r#"{"sticky_ready":false,"pages_min":0,"pages_max":30,"capacity":10}"#,
                "exceeds",
            ),
            (
                "bad-size",
                r#"{"sticky_ready":false,"pages_min":0,"pages_max":3,"media_sizes":["a3"]}"#,
                "unknown size",
            ),
        ] {
            let mut v: serde_json::Value = serde_json::from_str(&base).unwrap();
            v["id"] = name.into();
            v["feeder"] = serde_json::from_str(feeder).unwrap();
            std::fs::write(tmp.join(format!("{name}.json")), v.to_string()).unwrap();
            let err = format!("{:#}", Profile::load(&tmp, name).unwrap_err());
            assert!(err.contains(needle), "{name}: {err}");
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn profile_id_cannot_escape_the_directory() {
        let dir = dir();
        for id in [
            "../brother-wsd",
            "..",
            "brother-wsd/../../etc/passwd",
            "a/b",
            r"a\b",
            "",
        ] {
            let err = Profile::load(&dir, id).unwrap_err().to_string();
            assert!(
                err.contains("invalid profile id"),
                "{id:?} should be rejected before any read: {err}"
            );
        }
        assert!(valid_profile_id("brother-wsd"));
        assert!(valid_profile_id("feeder-a4only"));
    }
}
