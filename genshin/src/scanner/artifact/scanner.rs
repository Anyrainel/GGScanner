use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{bail, Result};
use image::RgbImage;
use regex::Regex;
use yas::{log_debug, log_error, log_info, log_warn};

use yas::ocr::ImageToText;

use super::GoodArtifactScannerConfig;
use crate::scanner::common::annotator;
use crate::scanner::common::backpack_scanner::{
    self as backpack_scanner, BackpackScanConfig, BackpackScanner, GridEvent, PanelWaitMode,
    ScanAction,
};
use crate::scanner::common::capture_frame::CaptureFrame;
use crate::scanner::common::constants::*;
use crate::scanner::common::coord_scaler::CoordScaler;
use crate::scanner::common::equip_parser;
use crate::scanner::common::fuzzy_match::fuzzy_match_map;
use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::grid_icon_detector::{GridIconResult, GridMode};
use crate::scanner::common::grid_voter::{GridVoteSchedule, PagedGridVoter, ReadyItem};
use crate::scanner::common::mappings::MappingManager;
use crate::scanner::common::models::{DebugOcrField, DebugScanResult, GoodArtifact, GoodSubStat};
use crate::scanner::common::ocr_factory::{self, same_model};
use crate::scanner::common::ocr_pool::SharedOcrPools;
use crate::scanner::common::pixel_utils;
use crate::scanner::common::roll_solver::{self, OcrCandidate, SolverInput};
use crate::scanner::common::scan_worker::{self, WorkItem};
use crate::scanner::common::stat_parser;

lazy_static::lazy_static! {
    /// Regex for parsing level text like "+20", "+ 12", "0"
    static ref LEVEL_REGEX: Regex = Regex::new(r"\+?\s*(\d+)").unwrap();
}

/// Crop fraction for number-only OCR retry: percent stats need more left trimming
/// to skip the stat name, flat stats need less.
fn crop_frac_for_stat(is_percent: bool) -> f64 {
    if is_percent {
        0.40
    } else {
        0.25
    }
}

/// Count Chinese characters (CJK Unified Ideographs) in a string.
fn cn_char_count(s: &str) -> usize {
    s.chars()
        .filter(|&c| c >= '\u{4E00}' && c <= '\u{9FFF}')
        .count()
}

/// Crop a sub-region from a captured frame using window base coordinates.
fn crop_region(
    frame: &CaptureFrame,
    rect: (f64, f64, f64, f64),
    y_shift: f64,
    scaler: &CoordScaler,
) -> Option<RgbImage> {
    frame.crop(rect, y_shift, scaler)
}

/// Pick the best OCR candidate from a set (used when solver fails).
/// Same-key candidates: prefer decimal value, then larger value.
/// Different-key candidates: prefer first (primary engine).
fn pick_best_candidate(candidates: &[OcrCandidate]) -> Option<&OcrCandidate> {
    if candidates.len() <= 1 {
        return candidates.first();
    }
    if candidates.iter().all(|c| c.key == candidates[0].key) {
        // All same key — prefer candidate with decimal, then larger value
        candidates.iter().max_by(|a, b| {
            let a_dec = a.value.fract().abs() > 0.001;
            let b_dec = b.value.fract().abs() > 0.001;
            a_dec.cmp(&b_dec).then(
                a.value
                    .partial_cmp(&b.value)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        })
    } else {
        // Different keys — prefer first (primary engine)
        Some(&candidates[0])
    }
}

/// Computed OCR regions for artifact card (at 1920x1080 base).
///
/// Coordinates derived from the old window_info JSON at 2560x1440, scaled by 0.75.
pub struct ArtifactOcrRegions {
    part_name: (f64, f64, f64, f64),
    main_stat: (f64, f64, f64, f64),
    level: (f64, f64, f64, f64),
    /// Per-line substat regions: (x, y, w, h) for each of the 4 possible substats.
    /// Heights increase for lower lines to match the game's actual layout.
    substat_lines: [(f64, f64, f64, f64); 4],
    set_name_x: f64,
    set_name_w: f64,
    set_name_base_y: f64,
    set_name_h: f64,
    equip: (f64, f64, f64, f64),
}

impl ArtifactOcrRegions {
    pub fn new() -> Self {
        let card_x: f64 = 1307.0;
        let card_y: f64 = 119.0;
        let card_w: f64 = 494.0;
        let card_h: f64 = 841.0;

        // Substat regions (width calibrated at 255px — wider causes OCR failures)
        // Sub3 (4th line) is wider (355px) to capture "(待激活)" text on unactivated substats
        let sub_x = 1356.0;
        let sub_w = 255.0;
        let sub3_w = 355.0;

        Self {
            part_name: (1327.0, 184.0, 155.0, 40.0),
            main_stat: (1327.0, 266.0, 155.0, 35.0),
            level: (
                card_x + (card_w * 0.0506).round(),
                card_y + (card_h * 0.3634).round(),
                (card_w * 0.1417).round(),
                (card_h * 0.0416).round(),
            ),
            substat_lines: [
                (sub_x, 478.0, sub_w, 35.0),
                (sub_x, 513.0, sub_w, 37.0),
                (sub_x, 550.0, sub_w, 39.0),
                (sub_x, 589.0, sub3_w, 39.0),
            ],
            set_name_x: 1330.0,
            set_name_w: 280.0,
            set_name_base_y: 625.0,
            set_name_h: 45.0,
            // Equip text "CharName已装备" — narrowed to skip avatar icon on left.
            equip: (1386.0, 905.0, 315.0, 50.0),
        }
    }
}

/// Result of scanning a single artifact
pub enum ArtifactScanResult {
    Artifact(GoodArtifact),
    Stop,
    Skip,
}

/// Artifact scanner ported from GOODScanner/lib/artifact_scanner.js.
///
/// Features elixir detection with Y-shift, astral marks, unactivated substats,
/// row-level deduplication, and post-processing filters.
///
/// Uses pipelined architecture: the main thread captures screenshots while
/// a worker pool OCRs them in parallel using `OcrPool` + `scan_worker`.
pub struct GoodArtifactScanner {
    config: GoodArtifactScannerConfig,
    mappings: Arc<MappingManager>,
    ocr_regions: ArtifactOcrRegions,
}

impl GoodArtifactScanner {
    pub fn new(config: GoodArtifactScannerConfig, mappings: Arc<MappingManager>) -> Result<Self> {
        Ok(Self {
            config,
            mappings,
            ocr_regions: ArtifactOcrRegions::new(),
        })
    }
}

impl GoodArtifactScanner {
    /// OCR a sub-region of a captured game image.
    fn ocr_image_region(
        ocr: &dyn ImageToText<RgbImage>,
        frame: &CaptureFrame,
        rect: (f64, f64, f64, f64),
        scaler: &CoordScaler,
    ) -> Result<String> {
        let sub = match crop_region(frame, rect, 0.0, scaler) {
            Some(img) => img,
            None => return Ok(String::new()),
        };
        let text = ocr.image_to_text(&sub, false)?;
        Ok(text.trim().to_string())
    }

    /// OCR a sub-region after converting to high-contrast grayscale.
    /// Tries grayscale then green-channel extraction for colored text (green set names).
    fn ocr_image_region_grayscale(
        ocr: &dyn ImageToText<RgbImage>,
        frame: &CaptureFrame,
        rect: (f64, f64, f64, f64),
        scaler: &CoordScaler,
        mappings: &MappingManager,
    ) -> Result<String> {
        let sub = match crop_region(frame, rect, 0.0, scaler) {
            Some(img) => img,
            None => return Ok(String::new()),
        };

        // Convert to grayscale
        let gray_img = RgbImage::from_fn(sub.width(), sub.height(), |px, py| {
            let p = sub.get_pixel(px, py);
            let g = (0.299 * p[0] as f64 + 0.587 * p[1] as f64 + 0.114 * p[2] as f64) as u8;
            image::Rgb([g, g, g])
        });

        let text_gray = ocr.image_to_text(&gray_img, false)?.trim().to_string();
        if Self::find_set_key_in_text(&text_gray, mappings).is_some() {
            return Ok(text_gray);
        }

        // Green-channel extraction: the set name text is green (high G, low R/B).
        // Extract green saturation: G - max(R, B). Text pixels will have high values.
        // Then invert to get dark text on white background.
        let green_extracted = RgbImage::from_fn(sub.width(), sub.height(), |px, py| {
            let p = sub.get_pixel(px, py);
            let r = p[0] as i32;
            let g = p[1] as i32;
            let b = p[2] as i32;
            let green_excess = (g - r.max(b)).max(0);
            let v = (255 - (green_excess * 4).min(255)) as u8;
            image::Rgb([v, v, v])
        });
        let text_green = ocr
            .image_to_text(&green_extracted, false)?
            .trim()
            .to_string();
        if Self::find_set_key_in_text(&text_green, mappings).is_some() {
            return Ok(text_green);
        }

        // Return whichever has more Chinese characters
        Ok([text_gray, text_green]
            .into_iter()
            .max_by_key(|s| cn_char_count(s))
            .unwrap_or_default())
    }

    /// OCR a sub-region with Y-offset and left-side icon masking.
    /// Replaces the leftmost ~18 pixels of the cropped image with the
    /// average background color to remove stat icons that confuse OCR.
    fn ocr_image_region_shifted_masked(
        ocr: &dyn ImageToText<RgbImage>,
        frame: &CaptureFrame,
        rect: (f64, f64, f64, f64),
        y_shift: f64,
        scaler: &CoordScaler,
    ) -> Result<String> {
        let mut sub = match crop_region(frame, rect, y_shift, scaler) {
            Some(img) => img,
            None => return Ok(String::new()),
        };

        let w = sub.width();
        let h = sub.height();

        // Mask the first ~18 pixels (stat icon area) with background color.
        // Sample background color from the right side of the image.
        let mask_width = 18u32.min(w);
        let sample_x = (w * 3 / 4).min(w.saturating_sub(1));
        let bg_color = {
            let mut r_sum = 0u32;
            let mut g_sum = 0u32;
            let mut b_sum = 0u32;
            let mut count = 0u32;
            for sy in [0, h / 2, h.saturating_sub(1)] {
                let p = sub.get_pixel(sample_x, sy);
                r_sum += p[0] as u32;
                g_sum += p[1] as u32;
                b_sum += p[2] as u32;
                count += 1;
            }
            image::Rgb([
                (r_sum / count) as u8,
                (g_sum / count) as u8,
                (b_sum / count) as u8,
            ])
        };

        for px in 0..mask_width {
            for py in 0..h {
                sub.put_pixel(px, py, bg_color);
            }
        }

        let text = ocr.image_to_text(&sub, false)?;
        Ok(text.trim().to_string())
    }

    /// OCR the number portion of a substat line with configurable crop.
    /// `left_frac` is how much of the left side to skip (0.0-1.0).
    /// Upscales 2x before OCR for better digit recognition on small text.
    fn ocr_substat_number_crop(
        ocr: &dyn ImageToText<RgbImage>,
        frame: &CaptureFrame,
        rect: (f64, f64, f64, f64),
        y_shift: f64,
        scaler: &CoordScaler,
        left_frac: f64,
    ) -> Result<String> {
        let (bx, by, bw, bh) = rect;
        let num_rect = (bx + bw * left_frac, by, bw * (1.0 - left_frac), bh);
        let sub = match crop_region(frame, num_rect, y_shift, scaler) {
            Some(img) => img,
            None => return Ok(String::new()),
        };
        let scaled = image::imageops::resize(
            &sub,
            sub.width() * 2,
            sub.height() * 2,
            image::imageops::FilterType::Lanczos3,
        );
        let text = ocr.image_to_text(&scaled, false)?;
        Ok(text.trim().to_string())
    }

    /// OCR a sub-region with Y-offset for elixir artifacts.
    fn ocr_image_region_shifted(
        ocr: &dyn ImageToText<RgbImage>,
        frame: &CaptureFrame,
        rect: (f64, f64, f64, f64),
        y_shift: f64,
        scaler: &CoordScaler,
    ) -> Result<String> {
        let (x, y, w, h) = rect;
        Self::ocr_image_region(ocr, frame, (x, y + y_shift, w, h), scaler)
    }

    /// Find artifact set key in OCR text (with multi-line fallback).
    ///
    /// Port of `findSetKeyInText()` from artifact_scanner.js
    fn find_set_key_in_text(text: &str, mappings: &MappingManager) -> Option<String> {
        if text.is_empty() {
            return None;
        }

        // Strip trailing punctuation that the OCR picks up from the set description
        // (e.g., "风起之日：" → "风起之日")
        let cleaned = text
            .trim()
            .trim_end_matches('：')
            .trim_end_matches(':')
            .trim_end_matches('；')
            .trim_end_matches(';')
            .trim();

        // Try cleaned text first
        if let Some(key) = fuzzy_match_map(cleaned, &mappings.artifact_set_map) {
            return Some(key);
        }

        // Try full text (in case cleaning removed something needed)
        if cleaned != text.trim() {
            if let Some(key) = fuzzy_match_map(text.trim(), &mappings.artifact_set_map) {
                return Some(key);
            }
        }

        // Try each line (for multi-line OCR results)
        for line in text.split('\n') {
            let line = line
                .trim()
                .trim_end_matches('：')
                .trim_end_matches(':')
                .trim();
            if line.len() < 2 {
                continue;
            }
            if let Some(key) = fuzzy_match_map(line, &mappings.artifact_set_map) {
                return Some(key);
            }
        }

        log_debug!(
            "[find_set_key] 未匹配 text={:?}",
            "[find_set_key] NO MATCH for text={:?}",
            text
        );
        None
    }

    /// Detect elixir crafted status via multi-pixel color check.
    /// Elixir artifacts have a purple banner with color ~(220, 192, 255).
    /// Normal artifacts have beige background ~(236, 229, 216) at that position.
    /// Checks 3 positions in the solid right-side region of the banner to avoid
    /// false positives from decorative text patterns or transient overlays.
    fn detect_elixir_crafted(frame: &CaptureFrame, scaler: &CoordScaler) -> bool {
        annotator::record_region(
            "elixir_banner",
            &frame.image,
            (1500.0 - frame.origin.0, 413.0 - frame.origin.1, 41.0, 21.0),
            scaler,
        );
        let positions: [(f64, f64); 3] = [(1510.0, 423.0), (1520.0, 423.0), (1530.0, 423.0)];
        let mut purple_count = 0;
        for &(bx, by) in &positions {
            let rgb = frame.pixel(scaler, bx, by);
            let is_purple = (rgb[2] as u16) > 230 && (rgb[2] as u16) > (rgb[1] as u16) + 40;
            if is_purple {
                purple_count += 1;
            }
        }
        let crafted = purple_count >= 2;
        let elixir_pos = (1520.0, 423.0);
        let rgb = frame.pixel(scaler, elixir_pos.0, elixir_pos.1);
        annotator::record_pixel_colored(
            "elixir",
            elixir_pos,
            rgb,
            if crafted { "crafted" } else { "not crafted" },
            [200, 160, 255],
        ); // light purple
        crafted
    }

    fn parse_equip_location(text: &str, mappings: &MappingManager) -> String {
        equip_parser::parse_equip_location(text, &mappings.character_name_map)
    }

    /// OCR one substat line and return candidates.
    ///
    /// Uses only the substat engine (ppocrv4) for substats. Evaluation on 9236
    /// substat lines showed ppocrv4 at 98.55% accuracy vs ppocrv5 at 55.36%,
    /// with zero cases where v5 was correct and v4 wasn't. Using v5 for substats
    /// only adds noise that can cause the solver to pick wrong values.
    ///
    /// Tries direct OCR first, then icon-masked fallback.
    /// Returns (candidates, stop_marker_hit, raw_text) where stop_marker_hit is true
    /// if "2件套" was detected. raw_text contains the best OCR text for diagnostic
    /// logging and rescue attempts.
    fn ocr_substat_line_candidates(
        _ocr: &dyn ImageToText<RgbImage>,
        substat_ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        sub_rect: (f64, f64, f64, f64),
        y_shift: f64,
        scaler: &CoordScaler,
    ) -> (Vec<OcrCandidate>, bool, [String; 2]) {
        let mut candidates = Vec::new();

        // Best OCR text from substat engine (direct + masked fallback)
        let text1 = {
            let text =
                Self::ocr_image_region_shifted(substat_ocr, image, sub_rect, y_shift, scaler)
                    .unwrap_or_default();
            if stat_parser::parse_stat_from_text(&text).is_some() {
                text
            } else {
                let masked = Self::ocr_image_region_shifted_masked(
                    substat_ocr,
                    image,
                    sub_rect,
                    y_shift,
                    scaler,
                )
                .unwrap_or_default();
                if stat_parser::parse_stat_from_text(&masked).is_some() {
                    masked
                } else if cn_char_count(&masked) > cn_char_count(&text) {
                    masked
                } else {
                    text
                }
            }
        };
        // text2 slot kept empty — main engine not used for substats
        let text2 = String::new();

        // Check for stop marker
        let stop = text1.contains("2\u{4EF6}\u{5957}");
        if stop || text1.trim().len() < 2 {
            return (candidates, stop, [text1, text2]);
        }

        // Parse and collect candidate
        if let Some(p1) = stat_parser::parse_stat_from_text(text1.trim()) {
            candidates.push(OcrCandidate {
                key: p1.key,
                value: p1.value,
                inactive: p1.inactive,
            });
        }

        (candidates, false, [text1, text2])
    }

    /// Scan a single artifact from a captured game image.
    ///
    /// This is called from the worker thread with a checked-out OCR model.
    /// `ocr` = level engine (v5, good at reading "+20" style text)
    /// `substat_ocr` = general engine (v4, used for everything else: name, main stat, set, equip, substats)
    pub fn scan_single_artifact(
        ocr: &dyn ImageToText<RgbImage>,
        substat_ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        scaler: &CoordScaler,
        ocr_regions: &ArtifactOcrRegions,
        mappings: &MappingManager,
        config: &GoodArtifactScannerConfig,
        item_index: usize,
        grid_icons: Option<GridIconResult>,
    ) -> Result<ArtifactScanResult> {
        // 0. Detect rarity — stop below min_rarity only if level is 0.
        let rarity = pixel_utils::detect_artifact_rarity(image, scaler);

        if rarity < config.min_rarity {
            // Quick level OCR to check if this is lv0 (no elixir shift — rough check is fine)
            let level_text =
                Self::ocr_image_region_shifted(ocr, image, ocr_regions.level, 0.0, scaler)
                    .unwrap_or_default();
            let quick_level = LEVEL_REGEX
                .captures(&level_text)
                .and_then(|c| c[1].parse::<i32>().ok())
                .unwrap_or(0);
            if quick_level == 0 {
                log_debug!(
                    "[artifact] idx={} {}星 lv0 < 最低{}星，停止",
                    "[artifact] idx={} {}* lv0 < min {}*, stopping",
                    item_index,
                    rarity,
                    config.min_rarity
                );
                annotator::finalize_skip(&format!(
                    "{}* lv0 below min {}*",
                    rarity, config.min_rarity
                ));
                return Ok(ArtifactScanResult::Stop);
            }
            log_debug!(
                "[artifact] idx={} {}星 lv{} < 最低{}星，跳过（非lv0）",
                "[artifact] idx={} {}* lv{} < min {}*, skipping (not lv0)",
                item_index,
                rarity,
                quick_level,
                config.min_rarity
            );
            annotator::finalize_skip(&format!(
                "{}* lv{} below min {}*",
                rarity, quick_level, config.min_rarity
            ));
            return Ok(ArtifactScanResult::Skip);
        }

        // 1. Part name → slot key
        let part_text = Self::ocr_image_region(substat_ocr, image, ocr_regions.part_name, scaler)?;
        annotator::record_ocr("slot", ocr_regions.part_name, &part_text);
        let slot_key = stat_parser::match_slot_key(&part_text);

        let slot_key = match slot_key {
            Some(k) => {
                annotator::set_final("slot", k);
                k.to_string()
            },
            None => {
                if rarity == 4 {
                    log_debug!("[artifact] idx={} 4星无法识别部位（可能是圣遗物经验素材），跳过", "[artifact] idx={} 4* unrecognizable slot (possibly artifact EXP material), skipping", item_index);
                    annotator::finalize_skip("4* unrecognizable slot");
                    return Ok(ArtifactScanResult::Skip);
                }
                if config.continue_on_failure {
                    log_warn!(
                        "[artifact] 无法识别部位: 「{}」，跳过",
                        "[artifact] cannot identify slot: 「{}」, skipping",
                        part_text
                    );
                    annotator::finalize_error(
                        None,
                        &format!("cannot identify slot: {}", part_text),
                    );
                    return Ok(ArtifactScanResult::Skip);
                }
                annotator::finalize_error(None, &format!("cannot identify slot: {}", part_text));
                bail!(
                    "无法识别圣遗物部位 / Cannot identify artifact slot: \u{300C}{}\u{300D}",
                    part_text
                );
            },
        };

        // 2. Main stat
        let main_stat_text =
            Self::ocr_image_region(substat_ocr, image, ocr_regions.main_stat, scaler)?;
        annotator::record_ocr("main_stat", ocr_regions.main_stat, &main_stat_text);
        let main_stat_key = if slot_key == "flower" {
            Some("hp".to_string())
        } else if slot_key == "plume" {
            Some("atk".to_string())
        } else {
            stat_parser::parse_stat_from_text(&main_stat_text)
                .map(|s| stat_parser::main_stat_key_fixup(&s.key))
        };

        let main_stat_key = match main_stat_key {
            Some(k) => {
                annotator::set_final("main_stat", &k);
                k
            },
            None => {
                if config.continue_on_failure {
                    log_warn!(
                        "[artifact] 无法识别主词条: 「{}」，跳过",
                        "[artifact] cannot identify main stat: 「{}」, skipping",
                        main_stat_text
                    );
                    annotator::finalize_error(
                        None,
                        &format!("cannot identify main stat: {}", main_stat_text),
                    );
                    return Ok(ArtifactScanResult::Skip);
                }
                annotator::finalize_error(
                    None,
                    &format!("cannot identify main stat: {}", main_stat_text),
                );
                bail!(
                    "无法识别主词条 / Cannot identify main stat: \u{300C}{}\u{300D}",
                    main_stat_text
                );
            },
        };

        // 3. Detect elixir crafted — panel pixel detection only (grid detection is unreliable)
        let elixir_crafted = Self::detect_elixir_crafted(image, scaler);
        let y_shift = if elixir_crafted { ELIXIR_SHIFT } else { 0.0 };

        // 4. Level — dual-engine OCR, collect both for solver
        let parse_level = |text: &str| -> i32 {
            LEVEL_REGEX
                .captures(text)
                .and_then(|c| c[1].parse::<i32>().ok())
                .filter(|&v| v <= 20)
                .unwrap_or(-1)
        };
        let level_text1 =
            Self::ocr_image_region_shifted(ocr, image, ocr_regions.level, y_shift, scaler)
                .unwrap_or_default();
        let lv1 = parse_level(&level_text1);

        let level_text2 = if same_model(ocr, substat_ocr) {
            level_text1.clone()
        } else {
            Self::ocr_image_region_shifted(substat_ocr, image, ocr_regions.level, y_shift, scaler)
                .unwrap_or_default()
        };
        let lv2 = parse_level(&level_text2);

        let level = if lv1 >= 0 && lv2 >= 0 {
            lv1.max(lv2)
        } else if lv1 >= 0 {
            lv1
        } else if lv2 >= 0 {
            lv2
        } else {
            0
        };
        if lv1 != lv2 {
            log_debug!(
                "[artifact] idx={} 等级双引擎OCR: 引擎1=「{}」→{} 引擎2=「{}」→{} → {}",
                "[artifact] idx={} level dual-OCR: engine1=「{}」→{} engine2=「{}」→{} → {}",
                item_index,
                level_text1.trim(),
                lv1,
                level_text2.trim(),
                lv2,
                level
            );
        }
        {
            let level_rect = (
                ocr_regions.level.0,
                ocr_regions.level.1 + y_shift,
                ocr_regions.level.2,
                ocr_regions.level.3,
            );
            // Show single value if both engines agree, otherwise "v1 | v2 -> final"
            let raw_display = if lv1 == lv2 {
                format!("+{}", level)
            } else {
                format!("+{} | +{}", lv1, lv2)
            };
            annotator::record_ocr("level", level_rect, &raw_display);
            annotator::set_final("level", &format!("+{}", level));
            if lv1 != lv2 {
                annotator::set_display("level", &format!("+{}", level));
            }
        }

        // 5. Lock and astral mark
        let (mut lock, astral_mark) = if let Some(ref gi) = grid_icons {
            // Grid-based detection (majority vote across multiple passes)
            annotator::add_warning(&format!(
                "lock={} astral={} (source: grid)",
                if gi.lock { "locked" } else { "unlocked" },
                if gi.astral { "marked" } else { "unmarked" }
            ));
            (gi.lock, gi.astral)
        } else {
            // Legacy: panel pixel-based detection (requires animation delay)
            // detect_artifact_lock/astral_mark annotate internally
            (
                pixel_utils::detect_artifact_lock(&image.image, scaler, y_shift),
                pixel_utils::detect_artifact_astral_mark(&image.image, scaler, y_shift),
            )
        };
        // All astraled artifacts are locked in-game. If we still see
        // astral=true + lock=false, force lock=true.
        if astral_mark && !lock {
            log_debug!(
                "[artifact] idx={} 星标=true 但锁定=false — 强制锁定=true（游戏规则）",
                "[artifact] idx={} astral=true but lock=false — forcing lock=true (game invariant)",
                item_index
            );
            lock = true;
        }

        // 6. Substats — dual-engine OCR, collect candidates, solve with roll validator.
        //
        // Phase 1: OCR both engines on each line, collect candidates for the solver.
        // Phase 2: Run roll solver to validate/select the correct combination.
        // Phase 3: If solver fails, retry with progressively cropped substat widths.
        // Phase 4: If still unsolved, fall back to heuristic merge.
        let mut solver_candidates: Vec<Vec<OcrCandidate>> = Vec::new();
        let mut level_candidates: Vec<i32> = Vec::new();
        if lv1 >= 0 {
            level_candidates.push(lv1);
        }
        if lv2 >= 0 && lv2 != lv1 {
            level_candidates.push(lv2);
        }
        if level_candidates.is_empty() {
            level_candidates.push(0);
        }

        // Maximum possible substat lines for this rarity and level.
        // 5-star: always 4 (init 3+1 unactivated or init 4).
        // 4-star lv0: max 3 (init 2 or 3, no 4th line possible).
        // 4-star lv4+: max 4 (init 2→gained 3rd at lv4, or init 3→gained 4th).
        let max_init = if rarity == 5 { 4 } else { 3 };
        let max_scan_lines = ((max_init + level / 4) as usize).min(4);

        // Track which physical line index each solver_candidates entry came from,
        // so we can map solver outputs back to the correct OCR region for dump.
        let mut candidate_line_indices: Vec<usize> = Vec::new();
        // Substat-engine text per physical line from phase 1, so later passes
        // don't re-run the same model on the same crop.
        let mut phase1_texts: [Option<String>; 4] = Default::default();

        // Phase 1: OCR at original width
        for i in 0..max_scan_lines {
            let sub_rect = ocr_regions.substat_lines[i];
            let (cands, stop, raw_texts) = Self::ocr_substat_line_candidates(
                ocr,
                substat_ocr,
                image,
                sub_rect,
                y_shift,
                scaler,
            );
            phase1_texts[i] = Some(raw_texts[0].clone());

            // Record raw OCR text for this line immediately
            {
                let shifted_rect = (sub_rect.0, sub_rect.1 + y_shift, sub_rect.2, sub_rect.3);
                let raw = if raw_texts[0].trim().is_empty() {
                    &raw_texts[1]
                } else {
                    &raw_texts[0]
                };
                annotator::record_ocr(&format!("sub[{}]", i), shifted_rect, raw.trim());
            }

            if stop {
                break;
            }

            // If this line's OCR text matches a set name, we've run past the
            // last real substat — the set name has moved up into this slot.
            // This happens on 4-star lv0 artifacts that have only 2 initial
            // substats (max_scan_lines assumes 3). Stop here; set detection
            // downstream will pick up the set name from its computed Y.
            if cands.is_empty()
                && (Self::find_set_key_in_text(raw_texts[0].trim(), mappings).is_some()
                    || Self::find_set_key_in_text(raw_texts[1].trim(), mappings).is_some())
            {
                log_debug!(
                    "[artifact] idx={} sub[{}] 识别为套装名行，停止扫描副词条",
                    "[artifact] idx={} sub[{}] detected as set name row, stopping substat scan",
                    item_index,
                    i
                );
                break;
            }

            let (sub_x, sub_y, sub_w, sub_h) = sub_rect;
            let mut did_extend = false;

            // Rescue: when full-text parsing failed but OCR produced text,
            // try extracting the stat key from the raw text and get the value
            // from a number-only crop.
            if cands.is_empty()
                && !did_extend
                && (raw_texts[0].trim().len() >= 2 || raw_texts[1].trim().len() >= 2)
            {
                // Try to identify the stat key from either engine's raw text
                let key_info = stat_parser::try_extract_stat_key(raw_texts[0].trim())
                    .or_else(|| stat_parser::try_extract_stat_key(raw_texts[1].trim()));

                if let Some((key, has_pct, is_inactive)) = key_info {
                    let crop_frac = crop_frac_for_stat(has_pct);
                    // Try number crop with substat engine (ppocrv4 only)
                    let mut rescue_val = None;
                    if let Ok(num_text) = Self::ocr_substat_number_crop(
                        substat_ocr,
                        image,
                        (sub_x, sub_y, sub_w, sub_h),
                        y_shift,
                        scaler,
                        crop_frac,
                    ) {
                        if let Some(v) = stat_parser::extract_number(num_text.trim()) {
                            if v > 0.5 {
                                rescue_val = Some(v);
                            }
                        }
                    }
                    if let Some(val) = rescue_val {
                        log_debug!("[artifact] idx={} sub[{}] 抢救成功: key={} val={} 原始OCR 「{}」/「{}」",
                            "[artifact] idx={} sub[{}] RESCUE: key={} val={} from raw OCR 「{}」/「{}」",
                            item_index, i, key, val, raw_texts[0].trim(), raw_texts[1].trim());
                        solver_candidates.push(vec![OcrCandidate {
                            key,
                            value: val,
                            inactive: is_inactive,
                        }]);
                        did_extend = true;
                    } else {
                        log_debug!("[artifact] idx={} sub[{}] 抢救: 找到key={} 但无有效数值，原始「{}」/「{}」",
                            "[artifact] idx={} sub[{}] rescue: found key={} but no valid number from raw 「{}」/「{}」",
                            item_index, i, key, raw_texts[0].trim(), raw_texts[1].trim());
                    }
                } else {
                    log_debug!("[artifact] idx={} sub[{}] 无候选且未找到key，原始OCR「{}」/「{}」",
                        "[artifact] idx={} sub[{}] no candidates, no key found in raw OCR 「{}」/「{}」",
                        item_index, i, raw_texts[0].trim(), raw_texts[1].trim());
                }
            }

            // If we didn't extend above (or no number retry), push original candidates
            if !did_extend {
                if cands.is_empty() && i == max_scan_lines - 1 {
                    // Last expected line produced nothing — log for diagnostics
                    log_debug!(
                        "[artifact] idx={} sub[{}] 空（{}星 lv{}），OCR「{}」",
                        "[artifact] idx={} sub[{}] empty ({}* lv{}), OCR 「{}」",
                        item_index,
                        i,
                        rarity,
                        level,
                        raw_texts[0].trim()
                    );
                }
                solver_candidates.push(cands);
            }

            // Track physical line index for this candidate entry
            candidate_line_indices.push(i);
        }

        // Phase 1c: Substat count guard.
        //
        // Minimum required substat lines by rarity and level:
        //   5-star: always 4 (init 3 or 4, but 3+1 unactivated = 4 lines)
        //   4-star: lv0 → 2, lv4 → 3, lv8+ → 4
        //     (init can be 2 or 3; each level-up at lv4/lv8 adds a line if below 4)
        //
        // If we got fewer non-empty lines than the minimum, retry failed lines
        // with the fallback engine (v5) and slightly shifted crops.
        let min_required = if rarity == 5 {
            4
        } else {
            // 4-star: min(2 + level/4, 4)
            (2 + level / 4).min(4) as usize
        };
        let non_empty_count = solver_candidates.iter().filter(|c| !c.is_empty()).count();
        if non_empty_count < min_required {
            log_info!("[artifact] idx={} {}星 lv{} 仅有{}条副词条（期望≥{}），使用备选引擎重试",
                "[artifact] idx={} {}* lv{} has only {} substat lines (expected ≥{}), retrying with fallback",
                item_index, rarity, level, non_empty_count, min_required);

            // Ensure we have exactly 4 slots (pad if the loop broke early on stop marker)
            while solver_candidates.len() < 4 {
                solver_candidates.push(Vec::new());
            }

            // Retry with: V5 at original rect, then shifted V4, then shifted V5.
            // All crops are from the in-memory image — no re-capture needed.
            // Y-offsets: ±2px. Width offsets: ±10px (right edge only, X is fixed).
            let shifts: &[(f64, f64)] = &[
                // (dy, dw) — v5 at original rect first
                (0.0, 0.0),
                // shifted v4, then shifted v5
                (-2.0, 0.0),
                (-2.0, 10.0),
                (-2.0, -10.0),
                (2.0, 0.0),
                (2.0, 10.0),
                (2.0, -10.0),
                (0.0, 10.0),
                (0.0, -10.0),
            ];

            let engines_share_model = same_model(ocr, substat_ocr);
            for i in 0..min_required {
                if !solver_candidates[i].is_empty() {
                    continue; // Already have candidates for this line
                }
                let (sub_x, sub_y, sub_w, sub_h) = ocr_regions.substat_lines[i];
                let mut found = false;

                for (step, &(dy, dw)) in shifts.iter().enumerate() {
                    let rect = (sub_x, sub_y + dy, sub_w + dw, sub_h);

                    // Step 0: v5 at original rect
                    // Other steps: v4 shifted first, then v5 shifted
                    let engines: &[&dyn ImageToText<RgbImage>] = match (step, engines_share_model) {
                        (0, true) if phase1_texts[i].is_some() => &[],
                        (0, _) => &[ocr],
                        (_, true) => &[substat_ocr],
                        (_, false) => &[substat_ocr, ocr],
                    };

                    for &engine in engines {
                        let text =
                            Self::ocr_image_region_shifted(engine, image, rect, y_shift, scaler)
                                .unwrap_or_default();
                        if text.trim().len() < 2 || text.contains("2\u{4EF6}\u{5957}") {
                            continue;
                        }
                        if let Some(p) = stat_parser::parse_stat_from_text(text.trim()) {
                            let already = solver_candidates[i]
                                .iter()
                                .any(|c| c.key == p.key && (c.value - p.value).abs() < 0.01);
                            if !already {
                                let eng_name = if std::ptr::eq(engine, ocr) {
                                    "v5"
                                } else {
                                    "v4"
                                };
                                log_info!("[artifact] idx={} sub[{}] 恢复成功 via {} (dy={}, dw={}): {}={:.1}",
                                    "[artifact] idx={} sub[{}] RECOVERED via {} (dy={}, dw={}): {}={:.1}",
                                    item_index, i, eng_name, dy, dw, p.key, p.value);
                                solver_candidates[i].push(OcrCandidate {
                                    key: p.key,
                                    value: p.value,
                                    inactive: p.inactive,
                                });
                                found = true;
                                break;
                            }
                        }
                    }
                    if found {
                        break;
                    }
                }

                if !found {
                    // Log what the substat engine saw on this line
                    let fallback_text = match &phase1_texts[i] {
                        Some(text) => text.clone(),
                        None => Self::ocr_image_region_shifted(
                            substat_ocr,
                            image,
                            ocr_regions.substat_lines[i],
                            y_shift,
                            scaler,
                        )
                        .unwrap_or_default(),
                    };
                    log_warn!("[artifact] idx={} sub[{}] 备选重试后仍为空（{}星 lv{}），OCR「{}」",
                        "[artifact] idx={} sub[{}] STILL EMPTY after fallback ({}* lv{}), OCR 「{}」",
                        item_index, i, rarity, level, fallback_text.trim());
                }
            }
        }

        // Filter out empty candidate lines (OCR failures) — the solver doesn't
        // care which physical line a substat came from, only the candidate sets.
        let non_empty_candidates: Vec<Vec<OcrCandidate>> = solver_candidates
            .iter()
            .filter(|c| !c.is_empty())
            .cloned()
            .collect();

        // Phase 2: Try the roll solver
        let solver_input = SolverInput {
            rarity,
            level_candidates: level_candidates.clone(),
            substat_candidates: non_empty_candidates.clone(),
        };
        let mut solved = roll_solver::solve(&solver_input);

        // Phase 3: If solver failed, retry with progressively cropped widths.
        // Remove 10px from right side of each substat region per attempt.
        if solved.is_none() {
            for crop_attempt in 1..=2 {
                let crop_px = crop_attempt as f64 * 10.0;
                let mut retry_candidates = solver_candidates.clone();

                for i in 0..retry_candidates.len().min(4) {
                    let (sub_x, sub_y, sub_w, sub_h) = ocr_regions.substat_lines[i];
                    let cropped_rect = (sub_x, sub_y, sub_w - crop_px, sub_h);
                    let (new_cands, _, _) = Self::ocr_substat_line_candidates(
                        ocr,
                        substat_ocr,
                        image,
                        cropped_rect,
                        y_shift,
                        scaler,
                    );
                    // Add new candidates to existing ones (deduplicated)
                    for nc in new_cands {
                        let exists = retry_candidates[i]
                            .iter()
                            .any(|c| c.key == nc.key && (c.value - nc.value).abs() < 0.01);
                        if !exists {
                            retry_candidates[i].push(nc);
                        }
                    }
                }

                let retry_non_empty: Vec<Vec<OcrCandidate>> = retry_candidates
                    .iter()
                    .filter(|c| !c.is_empty())
                    .cloned()
                    .collect();
                let retry_input = SolverInput {
                    rarity,
                    level_candidates: level_candidates.clone(),
                    substat_candidates: retry_non_empty,
                };
                solved = roll_solver::solve(&retry_input);
                if solved.is_some() {
                    log_debug!(
                        "[artifact] idx={} 求解器在裁剪尝试{}成功（-{}px）",
                        "[artifact] idx={} solver succeeded on crop attempt {} (-{}px)",
                        item_index,
                        crop_attempt,
                        crop_px
                    );
                    break;
                }
            }
        }

        // Build substats from solver result or fall back to heuristic merge
        let (substats, unactivated_substats, total_rolls) = if let Some(ref result) = solved {
            let mut subs = Vec::new();
            let mut unact = Vec::new();
            for s in &result.substats {
                let sub = GoodSubStat {
                    key: s.key.clone(),
                    value: s.value,
                    initial_value: s.initial_value,
                    rolls: vec![],
                };
                if s.inactive {
                    unact.push(sub);
                } else {
                    subs.push(sub);
                }
            }
            {
                let roll_str: Vec<String> = result
                    .substats
                    .iter()
                    .map(|s| {
                        format!(
                            "{}={} ({}r{})",
                            s.key,
                            s.value,
                            s.roll_count,
                            if s.inactive { " inactive" } else { "" }
                        )
                    })
                    .collect();
                log_debug!(
                    "[artifact] idx={} 求解器: total_rolls={} init={} [{}]",
                    "[artifact] idx={} solver: total_rolls={} init={} [{}]",
                    item_index,
                    result.total_rolls,
                    result.initial_substat_count,
                    roll_str.join(", ")
                );
            }
            (subs, unact, Some(result.total_rolls))
        } else {
            // Phase 4: Solver failed — could not find a valid roll assignment
            // for all candidate lines. Use heuristic (best candidate per line)
            // but ALWAYS warn the user with detailed context.
            let mut line_details: Vec<String> = Vec::new();
            for (i, cands) in non_empty_candidates.iter().enumerate() {
                let best = pick_best_candidate(cands);
                let detail = match best {
                    Some(c) => format!(
                        "  sub[{}]: {}={}{}",
                        i,
                        c.key,
                        c.value,
                        if c.inactive { " (inactive)" } else { "" }
                    ),
                    None => format!("  sub[{}]: (no candidates)", i),
                };
                line_details.push(detail);
            }
            log_warn!(
                "[artifact] 求解失败 {}星 lv{} {}·{} (锁定={}, 星标={}, 祝圣之霜={})\n\
                   检测到{}条副词条但无法找到有效roll分配:\n{}\n\
                   使用启发式回退——副词条数值可能不准确。",
                "[artifact] SOLVER FAILED on {}* lv{} {}·{} (lock={}, astral={}, elixir={})\n\
                   Detected {} substat lines but cannot find valid roll assignment:\n{}\n\
                   Using heuristic fallback — substat values may be inaccurate.",
                rarity,
                level,
                slot_key,
                main_stat_key,
                lock,
                astral_mark,
                elixir_crafted,
                non_empty_candidates.len(),
                line_details.join("\n")
            );

            let mut subs = Vec::new();
            let mut unact = Vec::new();
            for cands in &non_empty_candidates {
                if let Some(best) = pick_best_candidate(cands) {
                    let sub = GoodSubStat {
                        key: best.key.clone(),
                        value: best.value,
                        initial_value: None,
                        rolls: vec![],
                    };
                    if best.inactive {
                        unact.push(sub);
                    } else {
                        subs.push(sub);
                    }
                }
            }
            (subs, unact, None)
        };

        // Note: unactivated substats are now handled by the solver pipeline.
        // The OCR candidates include inactive=true when "(待激活)" is detected,
        // and the solver propagates this flag through to SolvedSubstat.inactive.

        // Update substat dump entries with solver results.
        // The non_empty_candidates (input to solver) map 1:1 to solver output substats.
        // We use candidate_line_indices to find which physical line each corresponds to,
        // then update the `sub[line]` entry that was recorded during the OCR loop.
        {
            // Build mapping: non-empty candidate index → physical line index
            let non_empty_line_indices: Vec<usize> = candidate_line_indices
                .iter()
                .enumerate()
                .filter(|(ci, _)| !solver_candidates[*ci].is_empty())
                .map(|(_, &line)| line)
                .collect();

            // Update final results for each solved substat
            let all_subs: Vec<(&GoodSubStat, bool)> = substats
                .iter()
                .map(|s| (s, false))
                .chain(unactivated_substats.iter().map(|s| (s, true)))
                .collect();
            for (si, (sub, inactive)) in all_subs.iter().enumerate() {
                let line_idx = non_empty_line_indices.get(si).copied().unwrap_or(si);
                let field = format!("sub[{}]", line_idx);
                let info = if *inactive {
                    format!("{}={} (inactive)", sub.key, sub.value)
                } else if let Some(iv) = sub.initial_value {
                    format!("{}={} (init={})", sub.key, sub.value, iv)
                } else {
                    format!("{}={}", sub.key, sub.value)
                };
                annotator::set_final(&field, &info);
            }
            if let Some(tr) = total_rolls {
                annotator::add_warning(&format!("solver: total_rolls={}", tr));
            } else {
                annotator::add_warning("solver FAILED — heuristic fallback used");
            }
        }

        // 6. Set name — the set name label always appears on the row immediately
        //    after the last substat. With N substats, the set name is at row N+1
        //    (using substat line Y positions). The set name starts slightly left
        //    of substats (set_name_x vs sub_x).
        //
        //    Strategy:
        //    a) Primary: read set name at substat_lines[N] Y (row right after last stat)
        //    b) Cross-validate: if row N+1 has a set name but row N had no valid stat,
        //       it means OCR missed a stat; if row N also has no stat AND no set name,
        //       try reading row N as set name (artifact has fewer stats than expected)
        //    c) Fallback: try the legacy Y positions (set_name_base_y - offset)
        let stat_count = (substats.len() + unactivated_substats.len()).clamp(1, 4);
        if stat_count < 4 && rarity == 5 {
            log_debug!(
                "[artifact] idx={} 5星仅识别到{}条副词条",
                "[artifact] idx={} 5* only identified {} substats",
                item_index,
                stat_count
            );
        }

        let mut set_key: Option<String> = None;
        let mut set_name_text = String::new();
        let mut tried_y = 0.0;

        // Helper closure: OCR a set name region and try to match
        let try_set_ocr = |set_rect: (f64, f64, f64, f64)| -> Result<(Option<String>, String)> {
            let text_rgb = Self::ocr_image_region(substat_ocr, image, set_rect, scaler)?;
            if let Some(key) = Self::find_set_key_in_text(&text_rgb, mappings) {
                return Ok((Some(key), text_rgb));
            }
            let text_gray =
                Self::ocr_image_region_grayscale(substat_ocr, image, set_rect, scaler, mappings)?;
            if let Some(key) = Self::find_set_key_in_text(&text_gray, mappings) {
                return Ok((Some(key), text_gray));
            }
            let text = if cn_char_count(&text_rgb) >= cn_char_count(&text_gray) {
                text_rgb
            } else {
                text_gray
            };
            Ok((None, text))
        };

        // (a) Primary: try the row right after the last recognized stat.
        //     stat_count substats → set name at substat_lines[stat_count] Y
        //     (for stat_count=4, use the legacy set_name_base_y which is below line 3)
        let primary_y = if stat_count < 4 && (stat_count as usize) < ocr_regions.substat_lines.len()
        {
            ocr_regions.substat_lines[stat_count as usize].1 + y_shift
        } else {
            ocr_regions.set_name_base_y + y_shift
        };
        let primary_rect = (
            ocr_regions.set_name_x,
            primary_y,
            ocr_regions.set_name_w,
            ocr_regions.set_name_h,
        );
        let (primary_key, primary_text) = try_set_ocr(primary_rect)?;
        log_debug!(
            "[artifact] idx={} 套装探测: stat_count={} set_y={:.0} text=「{}」",
            "[artifact] idx={} set probe: stat_count={} set_y={:.0} text=「{}」",
            item_index,
            stat_count,
            primary_y,
            primary_text
        );
        if let Some(key) = primary_key {
            set_key = Some(key);
            set_name_text = primary_text.clone();
            tried_y = primary_y;
        }

        // (b) If primary failed, try all substat line Y positions as set name rows.
        //     The set name could be at any of these positions if the stat count is wrong.
        //     Also try the legacy positions for robustness.
        if set_key.is_none() {
            set_name_text = primary_text;
            tried_y = primary_y;

            // Try each substat line Y as a set name position (using set_name_x for wider crop)
            let mut candidates: Vec<f64> = Vec::new();
            for line in &ocr_regions.substat_lines {
                candidates.push(line.1 + y_shift);
            }
            // Also add the legacy base position
            candidates.push(ocr_regions.set_name_base_y + y_shift);
            // Add legacy offset positions
            for missing in 1..=3 {
                candidates.push(ocr_regions.set_name_base_y + y_shift - (missing as f64 * 40.0));
            }
            // Deduplicate
            candidates.sort_by(|a, b| b.partial_cmp(a).unwrap());
            candidates.dedup_by(|a, b| (*a - *b).abs() < 3.0);

            for &set_y in &candidates {
                // Skip the primary position we already tried
                if (set_y - primary_y).abs() < 3.0 {
                    continue;
                }
                let set_rect = (
                    ocr_regions.set_name_x,
                    set_y,
                    ocr_regions.set_name_w,
                    ocr_regions.set_name_h,
                );
                let (maybe_key, text) = try_set_ocr(set_rect)?;
                log_debug!(
                    "[artifact] idx={} 套装探测: 备选 set_y={:.0} text=「{}」",
                    "[artifact] idx={} set probe: fallback set_y={:.0} text=「{}」",
                    item_index,
                    set_y,
                    text
                );
                if let Some(key) = maybe_key {
                    set_key = Some(key);
                    set_name_text = text;
                    tried_y = set_y;
                    break;
                }
                if set_name_text.is_empty() {
                    set_name_text = text;
                    tried_y = set_y;
                }
            }
        }

        // Record set name OCR in dump
        {
            let set_rect = (
                ocr_regions.set_name_x,
                tried_y,
                ocr_regions.set_name_w,
                ocr_regions.set_name_h,
            );
            annotator::record_ocr("set_name", set_rect, &set_name_text);
        }

        let set_key = match set_key {
            Some(k) => {
                annotator::set_final("set_name", &k);
                // Reverse-lookup the matched Chinese name for display
                let cn_name = mappings
                    .artifact_set_map
                    .iter()
                    .find(|(_, v)| v.as_str() == k)
                    .map(|(cn, _)| cn.as_str())
                    .unwrap_or(&k);
                annotator::set_display("set_name", cn_name);
                k
            },
            None => {
                let stat_keys: Vec<String> = substats
                    .iter()
                    .map(|s| s.key.clone())
                    .chain(
                        unactivated_substats
                            .iter()
                            .map(|s| format!("{}(inactive)", s.key)),
                    )
                    .collect();
                log_warn!(
                    "[artifact] 无法识别套装: setY={} stats=[{}] text=「{}」",
                    "[artifact] cannot identify set: setY={} stats=[{}] text=「{}」",
                    tried_y,
                    stat_keys.join(", "),
                    set_name_text
                );
                if config.continue_on_failure {
                    annotator::finalize_error(
                        None,
                        &format!("cannot identify set: {}", set_name_text),
                    );
                    return Ok(ArtifactScanResult::Skip);
                }
                annotator::finalize_error(None, &format!("cannot identify set: {}", set_name_text));
                bail!(
                    "无法识别圣遗物套装 / Cannot identify artifact set (substats={}): \u{300C}{}\u{300D}",
                    stat_count,
                    set_name_text
                );
            },
        };

        // Reject lower-rarity versions of higher-rarity sets, and all 3-star
        // artifacts globally. In Genshin, 5-star sets have 4-star variants and
        // 4-star sets have 3-star variants — the mappings file stores each
        // set's canonical (max) rarity.
        if rarity == 3 {
            log_debug!(
                "[artifact] idx={} 忽略3星圣遗物",
                "[artifact] idx={} ignoring 3* artifact",
                item_index
            );
            annotator::finalize_skip("3* artifact");
            return Ok(ArtifactScanResult::Skip);
        }
        if let Some(&set_max_rarity) = mappings.artifact_set_max_rarity.get(&set_key) {
            if rarity < set_max_rarity {
                log_debug!(
                    "[artifact] idx={} 忽略{}星 {} 变体（套装最高{}星）",
                    "[artifact] idx={} ignoring {}* {} variant (set max {}*)",
                    item_index,
                    rarity,
                    set_key,
                    set_max_rarity
                );
                annotator::finalize_skip(&format!(
                    "{}* variant of {}* set",
                    rarity, set_max_rarity
                ));
                return Ok(ArtifactScanResult::Skip);
            }
        }

        // 8. Equipped character
        // Try v4 first, fall back to v5 if no match (v4 dict lacks some rare chars like 魈/Xiao)
        let equip_text = Self::ocr_image_region(substat_ocr, image, ocr_regions.equip, scaler)?;
        let mut location = Self::parse_equip_location(&equip_text, mappings);
        if location.is_empty() && equip_text.trim().len() >= 2 && !same_model(ocr, substat_ocr) {
            let equip_text_v5 = Self::ocr_image_region(ocr, image, ocr_regions.equip, scaler)?;
            location = Self::parse_equip_location(&equip_text_v5, mappings);
            if !location.is_empty() {
                log_debug!(
                    "[artifact] idx={} 装备: v4「{}」失败, v5「{}」→ {}",
                    "[artifact] idx={} equip: v4「{}」failed, v5「{}」→ {}",
                    item_index,
                    equip_text.trim(),
                    equip_text_v5.trim(),
                    location
                );
                annotator::add_warning(&format!(
                    "equip: v4「{}」failed, v5「{}」matched",
                    equip_text.trim(),
                    equip_text_v5.trim()
                ));
            }
        }
        annotator::record_ocr("equip", ocr_regions.equip, &equip_text);
        if location.is_empty() {
            annotator::set_final("equip", "(none)");
        } else {
            annotator::set_final("equip", &location);
            // Reverse-lookup the Chinese character name for display
            let cn_name = mappings
                .character_name_map
                .iter()
                .find(|(_, v)| v.as_str() == location)
                .map(|(cn, _)| cn.as_str())
                .unwrap_or(&location);
            annotator::set_display("equip", cn_name);
        }

        let artifact = GoodArtifact {
            set_key,
            slot_key,
            level,
            rarity,
            main_stat_key,
            substats,
            location,
            lock,
            astral_mark,
            elixir_crafted,
            unactivated_substats,
            total_rolls,
        };

        {
            let json = serde_json::to_string_pretty(&artifact).unwrap_or_default();
            annotator::finalize_success(&json);
        }

        Ok(ArtifactScanResult::Artifact(artifact))
    }

    /// Identify a single artifact from a captured game screenshot (synchronous).
    ///
    /// This is a public wrapper around the internal `scan_single_artifact` for use
    /// by the artifact manager module. It uses permissive settings (continue on
    /// failure, no debug dumps) and returns `Ok(None)` for low-rarity or
    /// unrecognizable artifacts instead of stopping/erroring.
    ///
    /// Quick level-only OCR for page-skip optimization.
    /// Returns the artifact level (0-20), or -1 if OCR fails.
    ///
    /// 快速等级OCR，用于页面跳过优化。
    pub fn scan_level_only(
        ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        scaler: &CoordScaler,
    ) -> i32 {
        let regions = ArtifactOcrRegions::new();
        let y_shift = if Self::detect_elixir_crafted(image, scaler) {
            ELIXIR_SHIFT
        } else {
            0.0
        };
        let text = Self::ocr_image_region_shifted(ocr, image, regions.level, y_shift, scaler)
            .unwrap_or_default();
        LEVEL_REGEX
            .captures(&text)
            .and_then(|c| c[1].parse::<i32>().ok())
            .filter(|&v| v <= 20)
            .unwrap_or(-1)
    }

    /// 从游戏截图中识别单个圣遗物（同步调用）。
    /// 供圣遗物管理模块使用。
    ///
    /// `grid_icons`: pre-computed lock/astral from grid-based detection.
    /// Elixir is always detected via panel pixels. When `None`, all fields use panel pixel detection.
    pub fn identify_artifact(
        ocr: &dyn ImageToText<RgbImage>,
        substat_ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        scaler: &CoordScaler,
        mappings: &MappingManager,
        grid_icons: Option<GridIconResult>,
    ) -> Result<Option<GoodArtifact>> {
        Self::identify_artifact_at(ocr, substat_ocr, image, scaler, mappings, 0, grid_icons)
    }

    /// Identify a single artifact and preserve the caller's scan index for
    /// logs and debug annotations.
    pub fn identify_artifact_at(
        ocr: &dyn ImageToText<RgbImage>,
        substat_ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        scaler: &CoordScaler,
        mappings: &MappingManager,
        item_index: usize,
        grid_icons: Option<GridIconResult>,
    ) -> Result<Option<GoodArtifact>> {
        let regions = ArtifactOcrRegions::new();
        let config = GoodArtifactScannerConfig {
            continue_on_failure: true,
            dump_images: false,
            verbose: false,
            min_rarity: 1, // don't stop on low rarity, just return None
            ..Default::default()
        };

        match Self::scan_single_artifact(
            ocr,
            substat_ocr,
            image,
            scaler,
            &regions,
            mappings,
            &config,
            item_index,
            grid_icons,
        ) {
            Ok(ArtifactScanResult::Artifact(a)) => Ok(Some(a)),
            Ok(ArtifactScanResult::Stop) | Ok(ArtifactScanResult::Skip) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Public wrapper for `find_set_key_in_text` — used by the artifact manager
    /// to match set names OCR'd from the selection view detail panel.
    pub fn find_set_key_in_text_pub(text: &str, mappings: &MappingManager) -> Option<String> {
        Self::find_set_key_in_text(text, mappings)
    }

    /// Scan all artifacts from the backpack.
    ///
    /// Uses pipelined architecture: the main thread navigates the grid and
    /// captures screenshots, while a worker pool OCRs them in parallel.
    /// Results are collected in order via `scan_worker`.
    ///
    /// If `start_at > 0`, skips directly to that item index.
    pub fn scan(
        &self,
        ctrl: &mut GenshinGameController,
        skip_open_backpack: bool,
        start_at: usize,
        pools: &SharedOcrPools,
        progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
    ) -> Result<Vec<GoodArtifact>> {
        log_debug!("[artifact] 开始扫描...", "[artifact] starting scan...");
        let now = SystemTime::now();

        // Grab a clone of the cancel token up-front, before any mutable
        // borrows of `ctrl` start (the BackpackScanner below holds one for
        // the duration of the scan).
        let worker_cancel = ctrl.cancel_token();

        // Borrow a model from the v5 pool for reading item count
        let count_ocr_guard = pools.artifact().v5().get();

        let total_count = if !skip_open_backpack {
            // Use shared opening sequence (focus → main UI → open → tab → count with retry)
            let (count, _) = backpack_scanner::open_backpack_to_tab(
                ctrl,
                "artifact",
                self.config.open_delay,
                self.config.delay_tab,
                &count_ocr_guard,
                self.config.keep_five_star_filter,
                self.config.dump_images,
            )?;
            count
        } else {
            // Already in backpack — select the artifact tab and read count.
            // Retry once on a zero or unreadable header; skip-open used to
            // fail the whole scan on a single over-cap or OCR glitch.
            match backpack_scanner::select_tab_and_read_count(
                ctrl,
                "artifact",
                self.config.delay_tab,
                &count_ocr_guard,
                self.config.keep_five_star_filter,
                self.config.dump_images,
            ) {
                Ok((count, _)) if count > 0 => count,
                first => {
                    match &first {
                        Ok(_) => log_info!(
                            "[artifact] 数量=0，重新选择标签后重试...",
                            "[artifact] count=0, reselecting tab before retrying..."
                        ),
                        Err(e) => log_warn!(
                            "[artifact] 首次物品数量读取失败，重新选择标签后重试: {}",
                            "[artifact] first item-count read failed; reselecting tab before retrying: {}",
                            e
                        ),
                    }
                    backpack_scanner::select_tab_and_read_count(
                        ctrl,
                        "artifact",
                        self.config.delay_tab,
                        &count_ocr_guard,
                        self.config.keep_five_star_filter,
                        self.config.dump_images,
                    )?
                    .0
                },
            }
        };

        // Return count OCR model to pool before scan loop
        drop(count_ocr_guard);

        let mut bp = BackpackScanner::new(ctrl);

        if total_count == 0 && !self.config.keep_five_star_filter {
            log_info!(
                "[artifact] 背包中没有圣遗物",
                "[artifact] no artifacts in backpack"
            );
            return Ok(Vec::new());
        }

        // The header's count describes the unfiltered inventory. When the
        // recent-five-star filter is retained, use it only as a diagnostic;
        // traverse a bounded budget and stop on visually confirmed empty cells.
        let filtered_scan = self.config.keep_five_star_filter;
        let scan_budget = if filtered_scan {
            backpack_scanner::FILTERED_BACKPACK_SCAN_BUDGET
        } else {
            total_count.max(0) as usize
        };
        let total_count = if self.config.max_count > 0 {
            let capped = scan_budget.min(self.config.max_count.saturating_add(start_at)) as i32;
            log_debug!(
                "[artifact] 扫描上限: {}（限制为{}，max_count={}）",
                "[artifact] scan limit: {} (capped to {} by max_count={})",
                total_count,
                capped,
                self.config.max_count
            );
            capped
        } else if filtered_scan {
            log_debug!(
                "[artifact] 近期五星筛选已保留；标题数量仅作参考，使用有界视觉扫描上限 {}",
                "[artifact] Recent-five-star filter retained; header count is informational, using bounded visual scan limit {}",
                scan_budget
            );
            scan_budget as i32
        } else {
            log_debug!("[artifact] 总计: {}", "[artifact] total: {}", total_count);
            total_count
        };

        // Clone scaler so callback doesn't conflict with BackpackScanner's borrow
        let scaler = bp.scaler().clone();

        // Use shared OCR pools (v5 for level, v4 for everything else).
        let ocr_pool = pools.artifact().v5().clone();
        let substat_ocr_pool = pools.artifact().v4().clone();
        log_debug!(
            "[artifact] 使用共享OCR池: v5(等级)={}, v4(通用)={}",
            "[artifact] using shared OCR pools: v5(level)={}, v4(general)={}",
            pools.config().v5_count,
            pools.config().v4_count
        );

        // Shared context for worker threads
        let worker_mappings = self.mappings.clone();
        let worker_config = self.config.clone();
        let worker_scaler = scaler.clone();
        let worker_ocr_pool = ocr_pool.clone();
        let worker_substat_ocr_pool = substat_ocr_pool.clone();
        let worker_ocr_regions = ArtifactOcrRegions::new();

        // Start the parallel worker.
        // Metadata carries grid-based icon detection results (lock/astral/elixir).
        let (item_tx, worker_handle) =
            scan_worker::start_worker::<Option<GridIconResult>, GoodArtifact, _>(
                !self.config.continue_on_failure,
                move |work_item: WorkItem<Option<GridIconResult>>| {
                    // Skip queued work if the run was cancelled mid-scan.
                    if worker_cancel.is_cancelled() {
                        return Ok(None);
                    }
                    // Quick rarity check — stop below min_rarity.
                    if pixel_utils::artifact_below_min_rarity(
                        &work_item.frame,
                        &worker_scaler,
                        worker_config.min_rarity,
                    ) {
                        return Ok(None);
                    }

                    // Begin annotation for this item (no-op when disabled)
                    annotator::begin_item("artifacts", work_item.index, &worker_scaler);
                    annotator::add_frame("panel", &work_item.frame);
                    if let Some(ref ann) = work_item.grid_annotation {
                        annotator::record_grid_overlay(ann.0.clone(), ann.1.clone());
                    }

                    // Checkout OCR models from pools (blocks until available)
                    let ocr_guard = worker_ocr_pool.get();
                    let substat_ocr_guard = worker_substat_ocr_pool.get();

                    match Self::scan_single_artifact(
                        &ocr_guard,
                        &substat_ocr_guard,
                        &work_item.frame,
                        &worker_scaler,
                        &worker_ocr_regions,
                        &worker_mappings,
                        &worker_config,
                        work_item.index,
                        work_item.metadata,
                    ) {
                        Ok(ArtifactScanResult::Artifact(artifact)) => {
                            if artifact.rarity >= worker_config.min_rarity {
                                Ok(Some(artifact))
                            } else {
                                Ok(None)
                            }
                        },
                        Ok(ArtifactScanResult::Stop) => Ok(None),
                        Ok(ArtifactScanResult::Skip) => Ok(None),
                        Err(e) => {
                            annotator::finalize_error(None, &e.to_string());
                            Err(e)
                        },
                    }
                },
            );

        // Main thread: navigate grid and send captured images to worker
        let scan_config = BackpackScanConfig {
            delay_scroll: self.config.delay_scroll,
            panel_wait: PanelWaitMode::Artifact {
                timeout_ms: self.config.panel_timeout,
                initial_wait_ms: self.config.initial_wait,
                stop_on_failure: !self.config.continue_on_failure,
            },
            extra_delay: self.config.extra_delay,
            detail_panel_rect: Some(ARTIFACT_DETAIL_PANEL_RECT),
            grid_vote_schedule: GridVoteSchedule::for_page,
            probe_last_cell_per_page: false,
            detect_grid_duplicates: false,
            detect_empty_cells: filtered_scan,
            min_items_before_visual_end: 0,
        };

        // Per-page 3-pass voting state (shared across artifact / weapon / manager).
        let total = total_count as usize;
        // A filtered scan's traversal budget is not an item-count estimate.
        let progress_total = if filtered_scan && self.config.max_count == 0 {
            0
        } else {
            total
        };
        if let Some(pf) = progress_fn {
            pf(0, progress_total, "", "");
        }
        let mut voter: PagedGridVoter<()> = PagedGridVoter::new(total, GridMode::Artifact);
        let mut visible_layout = None;

        // Helper to emit a batch of ready items to the worker channel.
        // Returns Err(()) on channel close (caller should Stop).
        let emit_ready = |ready: Vec<ReadyItem<()>>,
                          item_tx: &crossbeam_channel::Sender<WorkItem<Option<GridIconResult>>>|
         -> Result<(), ()> {
            for item in ready {
                let worker_idx = item.idx - start_at;
                if item_tx
                    .send(WorkItem {
                        index: worker_idx,
                        frame: item.frame,
                        metadata: item.metadata,
                        grid_annotation: item.grid_annotation,
                    })
                    .is_err()
                {
                    log_error!(
                        "[artifact] 工作通道已关闭",
                        "[artifact] worker channel closed"
                    );
                    return Err(());
                }
            }
            Ok(())
        };

        let scan_outcome = bp.scan_grid(total, &scan_config, start_at, |ctrl, event| {
            match event {
                GridEvent::PageStarted { .. } => ScanAction::Continue,
                GridEvent::PageCompleted { .. } => {
                    if worker_handle.stop_requested() {
                        ScanAction::Stop
                    } else {
                        ScanAction::Continue
                    }
                },
                GridEvent::PageScrolled => {
                    visible_layout = None;
                    voter.reset_page();
                    ScanAction::Continue
                },
                GridEvent::Item {
                    idx, frame, layout, ..
                } => {
                    visible_layout = Some(layout);
                    // Check if worker has signaled stop (e.g., too many errors)
                    if worker_handle.stop_requested() {
                        return ScanAction::Stop;
                    }

                    if let Some(pf) = progress_fn {
                        pf(idx + 1, progress_total, "", "");
                    }

                    // Quick rarity check on main thread to stop early.
                    if pixel_utils::artifact_below_min_rarity(
                        &frame,
                        &scaler,
                        self.config.min_rarity,
                    ) {
                        voter.finish_additional_passes(&scaler, || ctrl.capture_game().ok());
                        let ready = voter.early_stop_flush();
                        let _ = emit_ready(ready, &item_tx);
                        return ScanAction::Stop;
                    }

                    let ready = voter.record_with_layout(
                        idx,
                        frame,
                        (),
                        &scaler,
                        layout.page_start_idx,
                        layout.page_items,
                        layout.screen_start_row,
                    );
                    if emit_ready(ready, &item_tx).is_err() {
                        return ScanAction::Stop;
                    }

                    ScanAction::Continue
                },
            }
        });

        voter.finish_additional_passes(&scaler, || ctrl.capture_game().ok());
        let leftover = voter.final_flush();
        let _ = emit_ready(leftover, &item_tx);

        // Drop sender to signal worker that no more items are coming
        drop(item_tx);

        // Wait for all OCR work to complete and collect results
        let (artifacts, index_map) = worker_handle.join(|index| {
            crate::scanner::common::backpack_scanner::focus_failed_item(
                ctrl,
                visible_layout,
                index + start_at,
                self.config.panel_timeout + self.config.extra_delay,
            );
        })?;

        if !self.config.continue_on_failure
            && scan_outcome.termination == backpack_scanner::ScanTermination::CaptureFailure
        {
            anyhow::bail!(
                "无法确认圣遗物详情，已停止扫描 / Artifact detail could not be confirmed; scan stopped (index={})",
                scan_outcome.scanned_count
            );
        }

        // Write index map for debug image correlation (output position → folder name)
        if self.config.dump_images {
            let map_path = crate::scanner::common::dump_paths::category_dir(
                "debug_images",
                "genshin",
                "artifacts",
            )
            .join("index_map.json");
            let _ = std::fs::write(
                &map_path,
                serde_json::to_string(&index_map).unwrap_or_default(),
            );
        }

        // Note: previously filtered unleveled 4-star artifacts from 5-star-capable sets,
        // but this removed valid data (e.g., AubadeOfMorningstarAndMoon, ADayCarvedFromRisingWinds).
        // All scanned artifacts are now kept regardless.

        let elapsed = now.elapsed().unwrap_or_default().as_secs_f64();
        if ctrl.cancel_token().is_cancelled() {
            log_info!(
                "[artifact] 已中断，扫描了{}个圣遗物（≥{}星），耗时{:.3}s",
                "[artifact] interrupted, {} artifacts scanned (>={}*) in {:.3}s",
                artifacts.len(),
                self.config.min_rarity,
                elapsed
            );
        } else {
            log_info!(
                "[artifact] 完成，扫描了{}个圣遗物（≥{}星），耗时{:.3}s",
                "[artifact] complete, {} artifacts scanned (>={}*) in {:.3}s",
                artifacts.len(),
                self.config.min_rarity,
                elapsed
            );
        }

        Ok(artifacts)
    }

    /// Debug scan a single artifact from a captured image.
    ///
    /// Returns detailed per-field OCR results including raw text, parsed values,
    /// and timing information. Used by the re-scan debug mode.
    pub fn debug_scan_single(
        &self,
        ocr: &dyn ImageToText<RgbImage>,
        image: &CaptureFrame,
        scaler: &CoordScaler,
    ) -> DebugScanResult {
        // Create substat OCR if different from main backend
        let substat_ocr_model: Option<Box<dyn ImageToText<RgbImage> + Send>> =
            if self.config.substat_ocr_backend != self.config.ocr_backend {
                ocr_factory::create_ocr_model(&self.config.substat_ocr_backend).ok()
            } else {
                None
            };
        let substat_ocr: &dyn ImageToText<RgbImage> = match substat_ocr_model {
            Some(ref m) => m.as_ref(),
            None => ocr,
        };
        use std::time::Instant;

        let total_start = Instant::now();
        let mut fields = Vec::new();

        // Rarity (pixel)
        let t = Instant::now();
        let rarity = pixel_utils::detect_artifact_rarity(image, scaler);
        fields.push(DebugOcrField {
            field_name: "rarity".into(),
            raw_text: String::new(),
            parsed_value: format!("{}*", rarity),
            region: (0.0, 0.0, 0.0, 0.0),
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Part name → slot key (use substat_ocr = v4 for text fields)
        let t = Instant::now();
        let part_text =
            Self::ocr_image_region(substat_ocr, image, self.ocr_regions.part_name, scaler)
                .unwrap_or_default();
        let slot_key = stat_parser::match_slot_key(&part_text)
            .map(|s| s.to_string())
            .unwrap_or_default();
        fields.push(DebugOcrField {
            field_name: "slot".into(),
            raw_text: part_text,
            parsed_value: slot_key.clone(),
            region: self.ocr_regions.part_name,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Main stat (use substat_ocr = v4 for text fields)
        let t = Instant::now();
        let main_stat_text =
            Self::ocr_image_region(substat_ocr, image, self.ocr_regions.main_stat, scaler)
                .unwrap_or_default();
        let main_stat_key = if slot_key == "flower" {
            "hp".to_string()
        } else if slot_key == "plume" {
            "atk".to_string()
        } else {
            stat_parser::parse_stat_from_text(&main_stat_text)
                .map(|s| stat_parser::main_stat_key_fixup(&s.key))
                .unwrap_or_default()
        };
        fields.push(DebugOcrField {
            field_name: "mainStat".into(),
            raw_text: main_stat_text,
            parsed_value: main_stat_key.clone(),
            region: self.ocr_regions.main_stat,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Elixir detection
        let t = Instant::now();
        let elixir_crafted = Self::detect_elixir_crafted(image, scaler);
        let y_shift = if elixir_crafted { ELIXIR_SHIFT } else { 0.0 };
        fields.push(DebugOcrField {
            field_name: "elixir".into(),
            raw_text: String::new(),
            parsed_value: format!("{}", elixir_crafted),
            region: (1520.0, 423.0, 1.0, 1.0), // pixel check, not a region
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Level
        let t = Instant::now();
        let level_text =
            Self::ocr_image_region_shifted(ocr, image, self.ocr_regions.level, y_shift, scaler)
                .unwrap_or_default();
        let level = {
            LEVEL_REGEX
                .captures(&level_text)
                .and_then(|c| c[1].parse::<i32>().ok())
                .unwrap_or(0)
        };
        fields.push(DebugOcrField {
            field_name: "level".into(),
            raw_text: level_text,
            parsed_value: format!("+{}", level),
            region: self.ocr_regions.level,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Substats — read each line individually (using substat OCR backend)
        let t = Instant::now();
        let mut substats: Vec<GoodSubStat> = Vec::new();
        let mut unactivated_substats: Vec<GoodSubStat> = Vec::new();
        let mut subs_raw_lines = Vec::new();
        for i in 0..4 {
            let (sub_x, sub_y, sub_w, sub_h) = self.ocr_regions.substat_lines[i];
            let line_text = Self::ocr_image_region_shifted(
                substat_ocr,
                image,
                (sub_x, sub_y, sub_w, sub_h),
                y_shift,
                scaler,
            )
            .unwrap_or_default();
            let line = line_text.trim().to_string();
            if line.len() < 2 {
                subs_raw_lines.push(line);
                continue;
            }
            if line.contains("2\u{4EF6}\u{5957}") {
                break;
            }
            if let Some(parsed) = stat_parser::parse_stat_from_text(&line) {
                let sub = GoodSubStat {
                    key: parsed.key,
                    value: parsed.value,
                    initial_value: None,
                    rolls: vec![],
                };
                if parsed.inactive {
                    unactivated_substats.push(sub);
                } else {
                    substats.push(sub);
                }
            }
            subs_raw_lines.push(line);
        }
        let subs_summary: Vec<String> = substats
            .iter()
            .map(|s| format!("{}={}", s.key, s.value))
            .chain(
                unactivated_substats
                    .iter()
                    .map(|s| format!("{}={}(inactive)", s.key, s.value)),
            )
            .collect();
        fields.push(DebugOcrField {
            field_name: "substats".into(),
            raw_text: subs_raw_lines.join(" | "),
            parsed_value: subs_summary.join(", "),
            region: self.ocr_regions.substat_lines[0],
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Set name
        let t = Instant::now();
        let stat_count = (substats.len() + unactivated_substats.len()).clamp(1, 4);
        let missing_stats = 4 - stat_count as i32;
        let set_y = self.ocr_regions.set_name_base_y + y_shift - (missing_stats as f64 * 40.0);
        let set_rect = (
            self.ocr_regions.set_name_x,
            set_y,
            self.ocr_regions.set_name_w,
            self.ocr_regions.set_name_h,
        );
        let set_name_text = {
            let rgb =
                Self::ocr_image_region(substat_ocr, image, set_rect, scaler).unwrap_or_default();
            if Self::find_set_key_in_text(&rgb, &self.mappings).is_some() {
                rgb
            } else {
                let gray = Self::ocr_image_region_grayscale(
                    substat_ocr,
                    image,
                    set_rect,
                    scaler,
                    &self.mappings,
                )
                .unwrap_or_default();
                if Self::find_set_key_in_text(&gray, &self.mappings).is_some() {
                    gray
                } else {
                    rgb
                }
            }
        };
        let set_key =
            Self::find_set_key_in_text(&set_name_text, &self.mappings).unwrap_or_default();
        fields.push(DebugOcrField {
            field_name: "setName".into(),
            raw_text: set_name_text,
            parsed_value: set_key.clone(),
            region: set_rect,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Equip (use substat_ocr = v4 for text fields)
        let t = Instant::now();
        let equip_text = Self::ocr_image_region(substat_ocr, image, self.ocr_regions.equip, scaler)
            .unwrap_or_default();
        let location = Self::parse_equip_location(&equip_text, &self.mappings);
        fields.push(DebugOcrField {
            field_name: "equip".into(),
            raw_text: equip_text,
            parsed_value: if location.is_empty() {
                "(none)".into()
            } else {
                location.clone()
            },
            region: self.ocr_regions.equip,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Lock + astral mark (pixel)
        let t = Instant::now();
        let lock = pixel_utils::detect_artifact_lock(&image.image, scaler, y_shift);
        let astral_mark = pixel_utils::detect_artifact_astral_mark(&image.image, scaler, y_shift);
        fields.push(DebugOcrField {
            field_name: "pixel_detect".into(),
            raw_text: String::new(),
            parsed_value: format!("lock={} astral={}", lock, astral_mark),
            region: (0.0, 0.0, 0.0, 0.0),
            duration_ms: t.elapsed().as_millis() as u64,
        });

        let artifact = GoodArtifact {
            set_key,
            slot_key,
            level,
            rarity,
            main_stat_key,
            substats,
            location,
            lock,
            astral_mark,
            elixir_crafted,
            unactivated_substats,
            total_rolls: None,
        };
        let parsed_json = serde_json::to_string_pretty(&artifact).unwrap_or_default();

        DebugScanResult {
            fields,
            total_duration_ms: total_start.elapsed().as_millis() as u64,
            parsed_json,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::common::test_utils::*;

    fn default_config() -> GoodArtifactScannerConfig {
        GoodArtifactScannerConfig {
            verbose: false,
            dump_images: false,
            continue_on_failure: false,
            ..Default::default()
        }
    }

    #[test]
    fn test_artifact_low_rarity_stops() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 3);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        // Quick level OCR returns "+0" → lv0, so rarity < min_rarity triggers Stop
        let level_ocr = FakeOcr::new(vec!["+0"]);
        let general_ocr = FakeOcr::new(vec![]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        assert!(matches!(result, ArtifactScanResult::Stop));
        assert_eq!(level_ocr.call_count(), 1); // quick level check
        assert_eq!(general_ocr.call_count(), 0);
    }

    #[test]
    fn test_artifact_unrecognizable_slot_4star_skips() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 4);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let level_ocr = FakeOcr::new(vec![]);
        let general_ocr = FakeOcr::new(vec!["乱码无法识别"]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        assert!(matches!(result, ArtifactScanResult::Skip));
    }

    #[test]
    fn test_artifact_unrecognizable_slot_5star_errors() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let level_ocr = FakeOcr::new(vec![]);
        let general_ocr = FakeOcr::new(vec!["乱码无法识别"]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_artifact_unrecognizable_slot_5star_skips_with_continue() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        let level_ocr = FakeOcr::new(vec![]);
        let general_ocr = FakeOcr::new(vec!["乱码无法识别"]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        assert!(matches!(result, ArtifactScanResult::Skip));
    }

    #[test]
    fn test_artifact_flower_happy_path() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        paint_artifact_lock(&mut image, true, 0.0);
        paint_artifact_astral(&mut image, false, 0.0);
        paint_elixir_banner(&mut image, false);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        // level_ocr (v5): 1 call for level
        let level_ocr = FakeOcr::new(vec!["+20"]);

        // general_ocr (v4) calls:
        // 1. part name
        // 2. main stat
        // 3. level v4
        // 4-7. substats (1 call each if text parses cleanly)
        // 8. set name RGB
        // 9. equip
        let general_ocr = FakeOcr::new(vec![
            "生之花",            // 1. part name
            "生命值",            // 2. main stat
            "+20",               // 3. level v4
            "暴击率+10.5%",      // 4. sub0 direct (parses → no masked call)
            "暴击伤害+21.0%",    // 5. sub1 direct
            "攻击力+9.3%",       // 6. sub2 direct
            "元素充能效率+6.5%", // 7. sub3 direct
            "角斗士的终幕礼",    // 8. set name RGB (matches → no grayscale call)
            "",                  // 9. equip (empty)
        ]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            ArtifactScanResult::Artifact(a) => {
                assert_eq!(a.slot_key, "flower");
                assert_eq!(a.main_stat_key, "hp");
                assert_eq!(a.level, 20);
                assert_eq!(a.rarity, 5);
                assert!(a.lock);
                assert!(!a.astral_mark);
                assert!(!a.elixir_crafted);
                assert_eq!(a.set_key, "GladiatorsFinale");
                assert!(a.location.is_empty());
                assert_eq!(a.substats.len(), 4);
                assert_eq!(a.substats[0].key, "critRate_");
                assert!((a.substats[0].value - 10.5).abs() < 0.1);
                assert!(a.total_rolls.is_some());
            },
            other => panic!(
                "Expected Artifact, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_artifact_elixir_crafted_detected() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        paint_elixir_banner(&mut image, true);
        paint_artifact_lock(&mut image, false, 40.0);
        paint_artifact_astral(&mut image, false, 40.0);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        let level_ocr = FakeOcr::new(vec!["+20"]);
        let general_ocr = FakeOcr::new(vec![
            "生之花",
            "生命值",
            "+20",
            "暴击率+10.5%",
            "暴击伤害+21.0%",
            "攻击力+9.3%",
            "元素充能效率+6.5%",
            "角斗士的终幕礼",
            "",
        ]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            ArtifactScanResult::Artifact(a) => {
                assert!(a.elixir_crafted);
                assert!(!a.lock);
            },
            other => panic!(
                "Expected Artifact, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_artifact_astral_forces_lock_true() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        paint_elixir_banner(&mut image, false);
        paint_artifact_lock(&mut image, false, 0.0);
        paint_artifact_astral(&mut image, true, 0.0);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        let level_ocr = FakeOcr::new(vec!["+20"]);
        let general_ocr = FakeOcr::new(vec![
            "生之花",
            "生命值",
            "+20",
            "暴击率+10.5%",
            "暴击伤害+21.0%",
            "攻击力+9.3%",
            "元素充能效率+6.5%",
            "角斗士的终幕礼",
            "",
        ]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            ArtifactScanResult::Artifact(a) => {
                assert!(a.astral_mark);
                assert!(a.lock, "Lock should be forced true when astral is present");
            },
            other => panic!(
                "Expected Artifact, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_artifact_equipped_to_character() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        paint_artifact_lock(&mut image, false, 0.0);
        paint_artifact_astral(&mut image, false, 0.0);
        paint_elixir_banner(&mut image, false);
        let scaler = make_1080p_scaler();
        let regions = ArtifactOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        let level_ocr = FakeOcr::new(vec!["+20"]);
        let general_ocr = FakeOcr::new(vec![
            "空之杯",
            "岩元素伤害加成",
            "+20",
            "暴击率+10.5%",
            "暴击伤害+21.0%",
            "攻击力+9.3%",
            "元素充能效率+6.5%",
            "角斗士的终幕礼",
            "芙宁娜已装备",
        ]);

        let result = GoodArtifactScanner::scan_single_artifact(
            &level_ocr,
            &general_ocr,
            &CaptureFrame::full(image.clone()),
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            ArtifactScanResult::Artifact(a) => {
                assert_eq!(a.slot_key, "goblet");
                assert_eq!(a.location, "Furina");
            },
            other => panic!(
                "Expected Artifact, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    /// Scan a 5-star flower whose 4th substat line only parses on a shifted crop.
    fn scan_flower_with_shifted_substat(
        level_ocr: &FakeOcr,
        general_ocr: &FakeOcr,
    ) -> GoodArtifact {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        paint_artifact_lock(&mut image, true, 0.0);
        paint_artifact_astral(&mut image, false, 0.0);
        paint_elixir_banner(&mut image, false);
        let mut config = default_config();
        config.continue_on_failure = true;

        let result = GoodArtifactScanner::scan_single_artifact(
            level_ocr,
            general_ocr,
            &CaptureFrame::full(image),
            &make_1080p_scaler(),
            &ArtifactOcrRegions::new(),
            &make_test_mappings(),
            &config,
            0,
            None,
        )
        .unwrap();
        match result {
            ArtifactScanResult::Artifact(a) => a,
            other => panic!(
                "Expected Artifact, got {:?}",
                std::mem::discriminant(&other)
            ),
        }
    }

    #[test]
    fn test_artifact_same_model_skips_duplicate_crops_only() {
        let level_ocr = FakeOcr::new(vec!["+20"]).with_model_id("ppocrv6tiny");
        let general_ocr = FakeOcr::new(vec![
            "生之花",
            "生命值",
            "暴击率+10.5%",
            "暴击伤害+21.0%",
            "攻击力+9.3%",
            "乱码无法识别",      // sub3 direct
            "乱码无法识别",      // sub3 icon-masked
            "元素充能效率+6.5%", // sub3 shifted crop (dy=-2)
            "角斗士的终幕礼",
            "口口口已装备", // equip: unmatched, no same-model retry
        ])
        .with_model_id("ppocrv6tiny");

        let a = scan_flower_with_shifted_substat(&level_ocr, &general_ocr);

        assert_eq!(a.level, 20);
        assert_eq!(a.substats.len(), 4);
        assert!(a.location.is_empty());
        assert_eq!(level_ocr.call_count(), 1);
        assert_eq!(general_ocr.call_count(), 10);
    }

    #[test]
    fn test_artifact_different_models_keep_second_engine_reads() {
        let level_ocr = FakeOcr::new(vec![
            "+20",
            "",             // sub3 original rect retry
            "纳西妲已装备", // equip retry
        ])
        .with_model_id("ppocrv5");
        let general_ocr = FakeOcr::new(vec![
            "生之花",
            "生命值",
            "+20", // level cross-read
            "暴击率+10.5%",
            "暴击伤害+21.0%",
            "攻击力+9.3%",
            "乱码无法识别",
            "乱码无法识别",
            "元素充能效率+6.5%",
            "角斗士的终幕礼",
            "口口口已装备",
        ])
        .with_model_id("ppocrv4");

        let a = scan_flower_with_shifted_substat(&level_ocr, &general_ocr);

        assert_eq!(a.level, 20);
        assert_eq!(a.substats.len(), 4);
        assert_eq!(a.location, "Nahida");
        assert_eq!(level_ocr.call_count(), 3);
        assert_eq!(general_ocr.call_count(), 11);
    }
}
