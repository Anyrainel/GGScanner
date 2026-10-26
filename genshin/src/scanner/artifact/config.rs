use crate::scanner::common::ocr_pool::{DEFAULT_ARTIFACT_LEVEL_OCR, DEFAULT_ARTIFACT_OCR};

/// Artifact scanner configuration.
///
/// All fields are set by the orchestrator (`cli.rs`) from global CLI flags
/// and `good_config.json`. This struct has no clap derives.
#[derive(Clone, Debug)]
pub struct GoodArtifactScannerConfig {
    pub min_rarity: i32,
    pub verbose: bool,
    /// OCR backend for artifact level
    pub ocr_backend: String,
    /// OCR backend for all other fields: name, main stat, set, equip, substats
    pub substat_ocr_backend: String,
    pub delay_scroll: u64,
    pub delay_tab: u64,
    pub open_delay: u64,
    pub continue_on_failure: bool,
    pub log_progress: bool,
    pub dump_images: bool,
    pub max_count: usize,
    /// Initial wait (ms) after click before starting panel detection.
    pub initial_wait: u64,
    /// Timeout (ms) for fingerprint-based panel load detection.
    pub panel_timeout: u64,
    /// Minimum stable-detail hold (ms); changes restart the hold.
    pub extra_delay: u64,
    /// If true, keep the 5-star sort filter enabled (for scanning recent artifacts).
    /// If false, dismiss the 5-star sort filter to scan all artifacts.
    pub keep_five_star_filter: bool,
}

impl Default for GoodArtifactScannerConfig {
    fn default() -> Self {
        Self {
            min_rarity: 4,
            verbose: false,
            ocr_backend: DEFAULT_ARTIFACT_LEVEL_OCR.to_string(),
            substat_ocr_backend: DEFAULT_ARTIFACT_OCR.to_string(),
            delay_scroll: 200,
            delay_tab: 400,
            open_delay: 1200,
            continue_on_failure: false,
            log_progress: false,
            dump_images: false,
            max_count: 0,
            initial_wait: 20,
            panel_timeout: 200,
            extra_delay: 0,
            keep_five_star_filter: false,
        }
    }
}
