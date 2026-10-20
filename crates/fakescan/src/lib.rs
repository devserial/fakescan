//! fakescan library: profiles, control-plane state, WSD Scan Service.

pub mod ctl;
pub mod daemon;
pub mod diag_fid;
pub mod diag_v2;
pub mod diag_v3;
pub mod discovery;
pub mod effects;
pub mod image_charts;
pub mod image_gen;
pub mod image_iso;
pub mod lab;
pub mod manager;
pub mod pipe;
pub mod profile;
pub mod registry;
pub mod scenarios;
pub mod server;
pub mod state;
pub mod tickets;
pub mod timeutil;
#[cfg(all(windows, feature = "tray"))]
pub mod tray;
pub mod tray_model;
pub mod ui;
pub mod wsd;

pub use lab::{is_fake_uuid, FAKE_UUID_PREFIX};
