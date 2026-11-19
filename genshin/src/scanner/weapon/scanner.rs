use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{bail, Result};
use image::{GenericImageView, RgbImage};
use regex::Regex;
use yas::{log_debug, log_error, log_info, log_warn};

use yas::ocr::ImageToText;

use super::GoodWeaponScannerConfig;
use crate::scanner::common::backpack_scanner::{
    BackpackScanConfig, BackpackScanner, GridEvent, PanelWaitMode, ScanAction,
};
use crate::scanner::common::constants::*;
use crate::scanner::common::coord_scaler::CoordScaler;
use crate::scanner::common::equip_parser;
use crate::scanner::common::fuzzy_match::{fuzzy_match_map, fuzzy_match_map_pair};
use crate::scanner::common::grid_icon_detector::{GridIconResult, GridMode};
use crate::scanner::common::grid_voter::{PagedGridVoter, ReadyItem};

lazy_static::lazy_static! {
    static ref SLASH_RE: Regex = Regex::new(r"(\d+)\s*/\s*(\d+)").unwrap();
    static ref LV_RE: Regex = Regex::new(r"(?i)[Ll][Vv]\.?\s*(\d+)").unwrap();
    static ref REFINE_CN_RE: Regex = Regex::new(r"\u{7CBE}\u{70BC}\s*(\d)").unwrap();
    static ref REFINE_R_RE: Regex = Regex::new(r"(?i)[Rr](\d)").unwrap();
    static ref BARE_DIGIT_RE: Regex = Regex::new(r"(\d)").unwrap();
}
use crate::scanner::common::annotator;
use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::mappings::MappingManager;
use crate::scanner::common::models::{DebugOcrField, DebugScanResult, GoodWeapon};
use crate::scanner::common::ocr_factory::same_model;
use crate::scanner::common::ocr_pool::SharedOcrPools;
use crate::scanner::common::pixel_utils;
use crate::scanner::common::scan_worker::{self, WorkItem};
use crate::scanner::common::stat_parser::level_to_ascension;

/// OCR regions for weapon card (at 1920x1080 base).
///
/// Computed from card proportions (card origin 1307,119, size 494x841).
/// The proportional approach adapts better to the actual card layout than
/// the hardcoded constants from the JS port.
pub(crate) struct WeaponOcrRegions {
    name: (f64, f64, f64, f64),
    level: (f64, f64, f64, f64),
    refinement: (f64, f64, f64, f64),
    equip: (f64, f64, f64, f64),
}

impl WeaponOcrRegions {
    pub(crate) fn new() -> Self {
        let card_x: f64 = 1307.0;
        let card_y: f64 = 119.0;
        let card_w: f64 = 494.0;
        let card_h: f64 = 841.0;

        Self {
            name: (1337.0, 121.0, 434.0, 55.0),
            level: (
                card_x + (card_w * 0.060).round(),
                card_y + (card_h * 0.367).round(),
                (card_w * 0.272).round(),
                (card_h * 0.035).round(),
            ),
            refinement: (
                card_x + (card_w * 0.058).round(),
                card_y + (card_h * 0.417).round(),
                (card_w * 0.30).round(),
                (card_h * 0.038).round(),
            ),
            // Equip text "CharName已装备" at bottom of card area.
            // Narrowed to skip avatar icon on left and excess space on right.
            equip: (1386.0, 905.0, 315.0, 50.0),
        }
    }
}

/// Weapon scanner ported from GOODScanner/lib/weapon_scanner.js.
///
/// Scans weapons from the backpack grid, detecting name/level/refinement/equip
/// via OCR on captured game images. Stops when low-tier weapons are reached.
///
/// The scanner holds only business logic (OCR model, mappings, config).
/// The game controller is passed to `scan()` to avoid borrow checker conflicts
/// with `BackpackScanner`.
pub struct GoodWeaponScanner {
    config: GoodWeaponScannerConfig,
    mappings: Arc<MappingManager>,
    ocr_regions: WeaponOcrRegions,
}

/// Additional forging material stop names not in the shared constants
const WEAPON_FORGING_STOP_NAMES: &[&str] = &[
    "\u{7CBE}\u{953B}\u{7528}\u{9B54}\u{77FF}", // 精锻用魔矿
    "\u{7CBE}\u{953B}\u{7528}\u{826F}\u{77FF}", // 精锻用良矿
    "\u{7CBE}\u{953B}\u{7528}\u{6742}\u{77FF}", // 精锻用杂矿
];

/// Result of scanning a single weapon: weapon data, stop signal, or skip
pub(crate) enum WeaponScanResult {
    Weapon(GoodWeapon),
    Stop,
    Skip,
}

impl GoodWeaponScanner {
    pub fn new(config: GoodWeaponScannerConfig, mappings: Arc<MappingManager>) -> Result<Self> {
        Ok(Self {
            config,
            mappings,
            ocr_regions: WeaponOcrRegions::new(),
        })
    }
}

impl GoodWeaponScanner {
    /// OCR a sub-region of a captured game image.
    fn ocr_image_region(
        ocr: &dyn ImageToText<RgbImage>,
        image: &RgbImage,
        rect: (f64, f64, f64, f64),
        scaler: &CoordScaler,
    ) -> Result<String> {
        let (bx, by, bw, bh) = rect;
        let x = scaler.x(bx) as u32;
        let y = scaler.y(by) as u32;
        let w = scaler.x(bw) as u32;
        let h = scaler.y(bh) as u32;

        let x = x.min(image.width().saturating_sub(1));
        let y = y.min(image.height().saturating_sub(1));
        let w = w.min(image.width().saturating_sub(x));
        let h = h.min(image.height().saturating_sub(y));

        if w == 0 || h == 0 {
            return Ok(String::new());
        }

        let sub = image.view(x, y, w, h).to_image();
        let text = ocr.image_to_text(&sub, false)?;
        Ok(text.trim().to_string())
    }

    /// Scan a single weapon from a captured game image.
    ///
    /// Called from the worker thread with a checked-out OCR model.
    pub(crate) fn scan_single_weapon(
        ocr: &dyn ImageToText<RgbImage>,
        equip_fallback_ocr: Option<&dyn ImageToText<RgbImage>>,
        image: &RgbImage,
        scaler: &CoordScaler,
        ocr_regions: &WeaponOcrRegions,
        mappings: &MappingManager,
        config: &GoodWeaponScannerConfig,
        _item_index: usize,
        grid_icons: Option<GridIconResult>,
    ) -> Result<WeaponScanResult> {
        use crate::scanner::common::debug_dump::level_cap_display;

        // OCR weapon name
        let name_text = Self::ocr_image_region(ocr, image, ocr_regions.name, scaler)?;
        let weapon_pair = fuzzy_match_map_pair(&name_text, &mappings.weapon_name_map);
        let weapon_key = weapon_pair.as_ref().map(|(_, k)| k.clone());
        annotator::record_ocr("name", ocr_regions.name, &name_text);
        annotator::set_label_below("name");
        if let Some((entity, key)) = &weapon_pair {
            log_debug!(
                "[weapon] 名称OCR: {:?} -> {} ({})",
                "[weapon] name OCR: {:?} -> {} ({})",
                name_text,
                entity,
                key
            );
        }

        if weapon_key.is_none() {
            // Check if it's a stop-signal weapon/material
            for &stop_name in WEAPON_STOP_NAMES
                .iter()
                .chain(WEAPON_FORGING_STOP_NAMES.iter())
            {
                if name_text.contains(stop_name) {
                    log_info!(
                        "[weapon] 检测到「{}」，停止扫描",
                        "[weapon] detected 「{}」, stopping",
                        stop_name
                    );
                    annotator::finalize_skip(&format!("stop signal: {}", stop_name));
                    return Ok(WeaponScanResult::Stop);
                }
            }

            if pixel_utils::weapon_below_min_rarity(image, scaler, config.min_rarity) {
                annotator::finalize_skip("below min rarity");
                return Ok(WeaponScanResult::Stop);
            }

            if config.continue_on_failure {
                log_warn!(
                    "[weapon] 无法匹配「{}」，跳过",
                    "[weapon] cannot match: 「{}」, skipping",
                    name_text
                );
                annotator::finalize_error(
                    None,
                    &format!("cannot match weapon name: {}", name_text),
                );
                return Ok(WeaponScanResult::Skip);
            }
            annotator::finalize_error(None, &format!("cannot match weapon name: {}", name_text));
            bail!(
                "无法匹配武器 / Cannot match weapon: \u{300C}{}\u{300D}",
                name_text
            );
        }

        let (cn_name_owned, weapon_key) = weapon_pair.unwrap();
        annotator::set_final("name", &weapon_key);
        let cn_name: &str = &cn_name_owned;
        annotator::set_display("name", cn_name);

        // OCR level
        let level_text = Self::ocr_image_region(ocr, image, ocr_regions.level, scaler)?;
        let (level, ascended) = Self::parse_weapon_level(&level_text);
        let level_display = level_cap_display(level, ascended);
        // Show raw OCR only if it doesn't trivially resolve to the same level/cap
        let raw_digits: String = level_text
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '/')
            .collect();
        let display_digits: String = level_display
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '/')
            .collect();
        if raw_digits == display_digits {
            annotator::record_ocr("level", ocr_regions.level, &level_display);
        } else {
            annotator::record_ocr("level", ocr_regions.level, &level_text);
            annotator::set_display("level", &level_display);
        }
        annotator::set_final("level", &level_display);

        // OCR refinement
        let ref_text = Self::ocr_image_region(ocr, image, ocr_regions.refinement, scaler)?;
        let refinement = Self::parse_refinement(&ref_text);
        annotator::record_ocr("refinement", ocr_regions.refinement, &ref_text);
        annotator::set_final("refinement", &format!("R{}", refinement));

        // OCR equip status
        // Try primary engine first, fall back to v5 if no match (v4 dict lacks rare chars like 魈)
        let equip_text = Self::ocr_image_region(ocr, image, ocr_regions.equip, scaler)?;
        let mut location = Self::parse_equip_location(&equip_text, mappings);
        if location.is_empty() && equip_text.trim().len() >= 2 {
            if let Some(fallback) = equip_fallback_ocr.filter(|f| !same_model(ocr, *f)) {
                let equip_text_v5 =
                    Self::ocr_image_region(fallback, image, ocr_regions.equip, scaler)?;
                location = Self::parse_equip_location(&equip_text_v5, mappings);
                if !location.is_empty() {
                    log_debug!(
                        "[weapon] {} 装备: v4「{}」失败, v5「{}」→ {}",
                        "[weapon] {} equip: v4「{}」failed, v5「{}」→ {}",
                        weapon_key,
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
        }
        if !equip_text.is_empty() {
            log_debug!(
                "[weapon] {} 装备OCR: {:?} -> {}",
                "[weapon] {} equip OCR: {:?} -> {}",
                weapon_key,
                equip_text,
                location
            );
        }
        annotator::record_ocr("equip", ocr_regions.equip, &equip_text);
        if location.is_empty() {
            annotator::set_final("equip", "(none)");
        } else {
            annotator::set_final("equip", &location);
            let cn_name = mappings
                .character_name_map
                .iter()
                .find(|(_, v)| v.as_str() == location)
                .map(|(cn, _)| cn.as_str())
                .unwrap_or(&location);
            annotator::set_display("equip", cn_name);
        }

        // Pixel-based detections (annotate internally)
        let rarity = pixel_utils::detect_weapon_rarity(image, scaler);
        // Lock: use grid detection when available, fall back to panel pixel detection
        let lock = if let Some(gi) = grid_icons {
            annotator::add_warning(&format!(
                "lock={} (source: grid)",
                if gi.lock { "locked" } else { "unlocked" }
            ));
            gi.lock
        } else {
            pixel_utils::detect_weapon_lock(image, scaler)
        };
        let ascension = level_to_ascension(level, ascended);

        let weapon = GoodWeapon {
            key: weapon_key,
            level,
            ascension,
            refinement,
            rarity,
            location,
            lock,
        };

        annotator::finalize_success(&serde_json::to_string_pretty(&weapon).unwrap_or_default());

        Ok(WeaponScanResult::Weapon(weapon))
    }

    /// Valid level caps for weapons.
    const VALID_MAX_LEVELS: &'static [i32] = &[20, 40, 50, 60, 70, 80, 90];

    /// Snap a raw max-level value to the nearest valid cap.
    fn snap_max_level(raw: i32) -> i32 {
        Self::VALID_MAX_LEVELS
            .iter()
            .copied()
            .min_by_key(|&v| (v - raw).unsigned_abs())
            .unwrap_or(raw)
    }

    /// Try to split a digit string into (level, max) pair.
    fn try_split_digits(digits: &str) -> Option<(i32, i32)> {
        for i in (1..digits.len()).rev() {
            if let (Ok(lv), Ok(mx)) = (digits[..i].parse::<i32>(), digits[i..].parse::<i32>()) {
                if (1..=90).contains(&lv) && (10..=90).contains(&mx) && mx >= lv {
                    return Some((lv, mx));
                }
            }
        }
        None
    }

    /// Parse weapon level from "XX/YY" or "Lv.X" format.
    /// Returns (level, ascended).
    ///
    /// Uses the same multi-stage parsing as character level:
    /// 1. Try "digits/digits" with slash
    /// 2. Extract all digits, try split
    /// 3. Remove one noise char and retry (OCR drops "/" → concatenated digits)
    /// 4. Partial extract (level only)
    fn parse_weapon_level(text: &str) -> (i32, bool) {
        if text.is_empty() {
            return (1, false);
        }

        // Phase 0: "XX/YY" with explicit slash
        if let Some(caps) = SLASH_RE.captures(text) {
            let level: i32 = caps[1].parse().unwrap_or(1);
            let raw_max: i32 = caps[2].parse().unwrap_or(20);
            let max_level = Self::snap_max_level(raw_max);
            let ascended = level >= 20 && level < max_level;
            return (level, ascended);
        }

        // Extract all digit chars
        let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
            return (1, false);
        }

        // Phase 1: direct split of all digits
        if let Some((lv, mx)) = Self::try_split_digits(&digits) {
            let max_level = Self::snap_max_level(mx);
            let ascended = lv >= 20 && lv < max_level;
            log_debug!(
                "[weapon] 等级OCR直接分割: {:?} -> {}/{}",
                "[weapon] level OCR direct split: {:?} -> {}/{}",
                text,
                lv,
                max_level
            );
            return (lv, ascended);
        }

        // Phase 2: remove one noise char at each position, prefer mid-string
        {
            let mid = digits.len() as f64 / 2.0;
            let mut best: Option<(i32, i32, usize, f64)> = None;
            for remove_idx in 0..digits.len() {
                let reduced: String = digits
                    .char_indices()
                    .filter(|&(i, _)| i != remove_idx)
                    .map(|(_, c)| c)
                    .collect();
                if let Some((lv, mx)) = Self::try_split_digits(&reduced) {
                    let dist = (remove_idx as f64 - mid).abs();
                    if best.is_none() || dist < best.unwrap().3 {
                        best = Some((lv, mx, remove_idx, dist));
                    }
                }
            }
            if let Some((lv, mx, idx, _)) = best {
                let max_level = Self::snap_max_level(mx);
                let ascended = lv >= 20 && lv < max_level;
                log_debug!(
                    "[weapon] 等级OCR去噪: {:?} (移除索引 {}) -> {}/{}",
                    "[weapon] level OCR noise-remove: {:?} (rm idx {}) -> {}/{}",
                    text,
                    idx,
                    lv,
                    max_level
                );
                return (lv, ascended);
            }
        }

        // Phase 3: partial extract — just the level number
        if let Some(caps) = LV_RE.captures(text) {
            let level: i32 = caps[1].parse().unwrap_or(1);
            log_debug!(
                "[weapon] 等级OCR部分匹配(Lv): {:?} -> {}",
                "[weapon] level OCR partial (Lv): {:?} -> {}",
                text,
                level
            );
            return (level, false);
        }

        let level: i32 = digits.parse().unwrap_or(0);
        if (1..=90).contains(&level) {
            log_debug!(
                "[weapon] 等级OCR纯数字: {:?} -> {}",
                "[weapon] level OCR bare digits: {:?} -> {}",
                text,
                level
            );
            return (level, false);
        }

        log_warn!(
            "[weapon] 等级OCR失败: {:?}",
            "[weapon] level OCR failed: {:?}",
            text
        );
        (1, false)
    }

    /// Parse refinement from text.
    /// Tries: "精炼X" → "RX" → bare digit 1-5.
    ///
    /// Port of refinement parsing from weapon_scanner.js
    fn parse_refinement(text: &str) -> i32 {
        if text.is_empty() {
            return 1;
        }

        // Try "精炼X"
        if let Some(caps) = REFINE_CN_RE.captures(text) {
            return caps[1].parse().unwrap_or(1);
        }

        // Try "RX"
        if let Some(caps) = REFINE_R_RE.captures(text) {
            return caps[1].parse().unwrap_or(1);
        }

        // Try bare digit
        if let Some(caps) = BARE_DIGIT_RE.captures(text) {
            let d: i32 = caps[1].parse().unwrap_or(0);
            if (1..=5).contains(&d) {
                return d;
            }
        }

        1
    }

    fn parse_equip_location(text: &str, mappings: &MappingManager) -> String {
        equip_parser::parse_equip_location(text, &mappings.character_name_map)
    }

    /// Scan all weapons from the backpack.
    ///
    /// Uses pipelined architecture: the main thread navigates the grid and
    /// captures screenshots, while a worker pool OCRs them in parallel.
    ///
    /// If `start_at > 0`, skips directly to that item index.
    pub fn scan(
        &self,
        ctrl: &mut GenshinGameController,
        skip_open_backpack: bool,
        start_at: usize,
        pools: &SharedOcrPools,
        progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
    ) -> Result<Vec<GoodWeapon>> {
        log_debug!("[weapon] 开始扫描...", "[weapon] starting scan...");
        let now = SystemTime::now();

        let cancel = ctrl.cancel_token();

        if !skip_open_backpack {
            // Return to main world using BGI-style strategy:
            // press Escape one at a time, verify after each press.
            ctrl.focus_game_window();
            if cancel.check_rmb() {
                anyhow::bail!("cancelled");
            }
            ctrl.return_to_main_ui(8);
            if cancel.check_rmb() {
                anyhow::bail!("cancelled");
            }
        }

        let mut bp = BackpackScanner::new(ctrl);

        if !skip_open_backpack {
            bp.open_backpack(self.config.open_delay);
        }
        bp.select_tab("weapon", self.config.delay_tab);
        if cancel.check_rmb() {
            anyhow::bail!("cancelled");
        }

        // Borrow a model from the v4 pool for reading item count
        let count_ocr_guard = pools.weapon().v4().get();
        let (current_count, _max_capacity) = bp.read_item_count(&count_ocr_guard)?;

        // If count is 0, try reopening backpack
        let total_count = if current_count == 0 {
            log_debug!(
                "[weapon] 数量=0，重新打开背包...",
                "[weapon] count=0, reopening backpack..."
            );
            drop(bp);
            if cancel.check_rmb() {
                anyhow::bail!("cancelled");
            }
            ctrl.return_to_main_ui(4);
            if cancel.check_rmb() {
                anyhow::bail!("cancelled");
            }
            let mut bp2 = BackpackScanner::new(ctrl);
            bp2.open_backpack(self.config.open_delay);
            bp2.select_tab("weapon", self.config.delay_tab);
            let (count, _) = bp2.read_item_count(&count_ocr_guard)?;
            drop(bp2);
            bp = BackpackScanner::new(ctrl);
            count
        } else {
            current_count
        };

        if total_count == 0 {
            log_info!("[weapon] 背包中无武器", "[weapon] no weapons in backpack");
            return Ok(Vec::new());
        }

        let total_count = if self.config.max_count > 0 {
            let capped = (total_count as usize).min(self.config.max_count + start_at) as i32;
            log_debug!(
                "[weapon] 总计: {} (受max_count={}限制，截取为{})",
                "[weapon] total: {} (capped to {} by max_count={})",
                total_count,
                self.config.max_count,
                capped
            );
            capped
        } else {
            log_debug!("[weapon] 总计: {}", "[weapon] total: {}", total_count);
            total_count
        };

        let scaler = bp.scaler().clone();

        // Return count OCR model to pool before scan loop
        drop(count_ocr_guard);

        // Use shared OCR pools (v4 for primary, v5 for equip fallback).
        let ocr_pool = pools.weapon().v4().clone();
        let equip_fallback_pool = pools.weapon().v5().clone();

        // Shared context for worker threads
        let worker_mappings = self.mappings.clone();
        let worker_config = self.config.clone();
        let worker_scaler = scaler.clone();
        let worker_ocr_pool = ocr_pool.clone();
        let worker_equip_pool = equip_fallback_pool.clone();
        let worker_ocr_regions = WeaponOcrRegions::new();
        let worker_cancel = cancel.clone();

        let (item_tx, worker_handle) =
            scan_worker::start_worker::<Option<GridIconResult>, GoodWeapon, _>(
                !self.config.continue_on_failure,
                move |work_item: WorkItem<Option<GridIconResult>>| {
                    // Skip queued work if the run was cancelled mid-scan.
                    if worker_cancel.is_cancelled() {
                        return Ok(None);
                    }

                    annotator::begin_item("weapons", work_item.index, &worker_scaler);
                    annotator::add_frame("panel", &work_item.frame);
                    if let Some(ref ann) = work_item.grid_annotation {
                        annotator::record_grid_overlay(ann.0.clone(), ann.1.clone());
                    }

                    let ocr_guard = worker_ocr_pool.get();
                    let equip_guard = worker_equip_pool.get();

                    match Self::scan_single_weapon(
                        &ocr_guard,
                        Some(&equip_guard as &dyn ImageToText<RgbImage>),
                        &work_item.frame.image,
                        &worker_scaler,
                        &worker_ocr_regions,
                        &worker_mappings,
                        &worker_config,
                        work_item.index,
                        work_item.metadata,
                    ) {
                        Ok(WeaponScanResult::Weapon(weapon)) => {
                            if weapon.rarity >= worker_config.min_rarity {
                                Ok(Some(weapon))
                            } else {
                                annotator::finalize_skip("below min rarity filter");
                                Ok(None)
                            }
                        },
                        Ok(WeaponScanResult::Stop) => {
                            annotator::finalize_skip("stop signal");
                            Ok(None)
                        },
                        Ok(WeaponScanResult::Skip) => Ok(None),
                        Err(e) => {
                            annotator::finalize_error(None, &e.to_string());
                            Err(e)
                        },
                    }
                },
            );

        let scan_config = BackpackScanConfig {
            delay_scroll: self.config.delay_scroll,
            panel_wait: PanelWaitMode::FixedDelay {
                delay_ms: self.config.panel_delay,
            },
            extra_delay: 0,
            probe_leading_pages: false,
            detect_empty_cells: false,
            min_items_before_visual_end: 0,
        };

        let total = total_count as usize;
        // Initial (0, total) tick so the polling client sees the real backpack size.
        if let Some(pf) = progress_fn {
            pf(0, total, "", "");
        }
        let mut voter: PagedGridVoter<()> = PagedGridVoter::new(total, GridMode::Weapon);
        let mut visible_layout = None;

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
                    log_error!("[weapon] 工作通道已关闭", "[weapon] worker channel closed");
                    return Err(());
                }
            }
            Ok(())
        };

        bp.scan_grid(total, &scan_config, start_at, |ctrl, event| match event {
            GridEvent::PageCompleted { .. } => {
                if worker_handle.stop_requested() {
                    ScanAction::Stop
                } else {
                    ScanAction::Continue
                }
            },
            GridEvent::PageTail { .. } => ScanAction::Continue,
            GridEvent::PageScrolled => {
                visible_layout = None;
                voter.reset_page();
                ScanAction::Continue
            },
            GridEvent::Item {
                idx, frame, layout, ..
            } => {
                visible_layout = Some(layout);
                if worker_handle.stop_requested() {
                    return ScanAction::Stop;
                }
                if let Some(pf) = progress_fn {
                    pf(idx + 1, total, "", "");
                }
                if pixel_utils::weapon_below_min_rarity(
                    &frame.image,
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
                    None,
                );
                if emit_ready(ready, &item_tx).is_err() {
                    return ScanAction::Stop;
                }
                ScanAction::Continue
            },
        });

        voter.finish_additional_passes(&scaler, || ctrl.capture_game().ok());
        let leftover = voter.final_flush();
        let _ = emit_ready(leftover, &item_tx);

        drop(item_tx);
        let (weapons, index_map) = worker_handle.join(|index| {
            crate::scanner::common::backpack_scanner::focus_failed_item(
                ctrl,
                visible_layout,
                index + start_at,
                self.config.panel_delay,
            );
        })?;

        // Write index map for debug image correlation (output position → folder name)
        if self.config.dump_images {
            let map_path = crate::scanner::common::dump_paths::category_dir(
                "debug_images",
                "genshin",
                "weapons",
            )
            .join("index_map.json");
            let _ = std::fs::write(
                &map_path,
                serde_json::to_string(&index_map).unwrap_or_default(),
            );
        }

        let elapsed = now.elapsed().unwrap_or_default().as_secs_f64();
        if cancel.is_cancelled() {
            log_info!(
                "[weapon] 已中断，扫描了{}把武器，耗时{:.3}s",
                "[weapon] interrupted, {} weapons scanned in {:.3}s",
                weapons.len(),
                elapsed
            );
        } else {
            log_info!(
                "[weapon] 完成，扫描了{}把武器，耗时{:.3}s",
                "[weapon] complete, {} weapons scanned in {:.3}s",
                weapons.len(),
                elapsed
            );
        }

        Ok(weapons)
    }

    /// Debug scan a single weapon from a captured image.
    ///
    /// Returns detailed per-field OCR results including raw text, parsed values,
    /// and timing information. Used by the re-scan debug mode.
    pub fn debug_scan_single(
        &self,
        ocr: &dyn ImageToText<RgbImage>,
        image: &RgbImage,
        scaler: &CoordScaler,
    ) -> DebugScanResult {
        use std::time::Instant;

        let total_start = Instant::now();
        let mut fields = Vec::new();

        // Name
        let t = Instant::now();
        let name_text =
            Self::ocr_image_region(ocr, image, self.ocr_regions.name, scaler).unwrap_or_default();
        let name_key =
            fuzzy_match_map(&name_text, &self.mappings.weapon_name_map).unwrap_or_default();
        fields.push(DebugOcrField {
            field_name: "name".into(),
            raw_text: name_text,
            parsed_value: name_key.clone(),
            region: self.ocr_regions.name,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Level
        let t = Instant::now();
        let level_text =
            Self::ocr_image_region(ocr, image, self.ocr_regions.level, scaler).unwrap_or_default();
        let (level, ascended) = Self::parse_weapon_level(&level_text);
        fields.push(DebugOcrField {
            field_name: "level".into(),
            raw_text: level_text,
            parsed_value: format!("lv={} ascended={}", level, ascended),
            region: self.ocr_regions.level,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Refinement
        let t = Instant::now();
        let ref_text = Self::ocr_image_region(ocr, image, self.ocr_regions.refinement, scaler)
            .unwrap_or_default();
        let refinement = Self::parse_refinement(&ref_text);
        fields.push(DebugOcrField {
            field_name: "refinement".into(),
            raw_text: ref_text,
            parsed_value: format!("R{}", refinement),
            region: self.ocr_regions.refinement,
            duration_ms: t.elapsed().as_millis() as u64,
        });

        // Equip
        let t = Instant::now();
        let equip_text =
            Self::ocr_image_region(ocr, image, self.ocr_regions.equip, scaler).unwrap_or_default();
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

        // Pixel detections (not OCR but still timed)
        let t = Instant::now();
        let rarity = pixel_utils::detect_weapon_rarity(image, scaler);
        let lock = pixel_utils::detect_weapon_lock(image, scaler);
        let ascension = level_to_ascension(level, ascended);
        fields.push(DebugOcrField {
            field_name: "pixel_detect".into(),
            raw_text: String::new(),
            parsed_value: format!("rarity={} lock={} ascension={}", rarity, lock, ascension),
            region: (0.0, 0.0, 0.0, 0.0),
            duration_ms: t.elapsed().as_millis() as u64,
        });

        let weapon = GoodWeapon {
            key: name_key,
            level,
            ascension,
            refinement,
            rarity,
            location,
            lock,
        };
        let parsed_json = serde_json::to_string_pretty(&weapon).unwrap_or_default();

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

    fn default_config() -> GoodWeaponScannerConfig {
        GoodWeaponScannerConfig {
            verbose: false,
            dump_images: false,
            continue_on_failure: false,
            ..Default::default()
        }
    }

    fn make_weapon_image(rarity: i32, locked: bool) -> RgbImage {
        let mut img = make_1080p_image();
        paint_rarity_stars(&mut img, rarity);
        paint_weapon_lock(&mut img, locked);
        img
    }

    #[test]
    fn test_weapon_happy_path_5star_locked() {
        let image = make_weapon_image(5, true);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["天空之翼", "90/90", "精炼1阶", ""]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        )
        .unwrap();

        match result {
            WeaponScanResult::Weapon(w) => {
                assert_eq!(w.key, "SkywardHarp");
                assert_eq!(w.level, 90);
                assert_eq!(w.refinement, 1);
                assert_eq!(w.rarity, 5);
                assert!(w.lock);
                assert!(w.location.is_empty());
            },
            other => panic!("Expected Weapon, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_weapon_with_equip_location() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["护摩之杖", "90/90", "精炼1阶", "胡桃已装备"]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        )
        .unwrap();

        match result {
            WeaponScanResult::Weapon(w) => {
                assert_eq!(w.key, "StaffOfHoma");
                assert_eq!(w.location, "HuTao");
                assert!(!w.lock);
            },
            other => panic!("Expected Weapon, got {:?}", std::mem::discriminant(&other)),
        }
    }

    #[test]
    fn test_weapon_low_rarity_stops() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 2);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["something"]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        )
        .unwrap();

        assert!(matches!(result, WeaponScanResult::Stop));
    }

    #[test]
    fn test_weapon_unmatched_name_continue_on_failure_skips() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let mut config = default_config();
        config.continue_on_failure = true;

        let ocr = FakeOcr::new(vec!["完全看不懂的名字"]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        )
        .unwrap();

        assert!(matches!(result, WeaponScanResult::Skip));
    }

    #[test]
    fn test_weapon_unmatched_name_errors_without_continue() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["完全看不懂的名字"]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        );

        assert!(result.is_err());
    }

    #[test]
    fn test_weapon_equip_fallback_to_v5() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["风鹰剑", "80/90", "精炼3阶", "口口口已装备"])
            .with_model_id("ppocrv4");
        let fallback = FakeOcr::new(vec!["纳西妲已装备"]).with_model_id("ppocrv5");

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr,
            Some(&fallback),
            &image,
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            WeaponScanResult::Weapon(w) => {
                assert_eq!(w.key, "AquilaFavonia");
                assert_eq!(w.location, "Nahida");
                assert_eq!(w.level, 80);
                assert_eq!(w.refinement, 3);
            },
            other => panic!("Expected Weapon, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(fallback.call_count(), 1);
    }

    #[test]
    fn test_weapon_equip_fallback_skipped_for_same_model() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["风鹰剑", "80/90", "精炼3阶", "口口口已装备"])
            .with_model_id("ppocrv6tiny");
        let fallback = FakeOcr::new(vec![]).with_model_id("ppocrv6tiny");

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr,
            Some(&fallback),
            &image,
            &scaler,
            &regions,
            &mappings,
            &config,
            0,
            None,
        )
        .unwrap();

        match result {
            WeaponScanResult::Weapon(w) => assert!(w.location.is_empty()),
            other => panic!("Expected Weapon, got {:?}", std::mem::discriminant(&other)),
        }
        assert_eq!(ocr.call_count(), 4);
        assert_eq!(fallback.call_count(), 0);
    }

    #[test]
    fn test_weapon_level_ascended() {
        let image = make_weapon_image(5, false);
        let scaler = make_1080p_scaler();
        let regions = WeaponOcrRegions::new();
        let mappings = make_test_mappings();
        let config = default_config();

        let ocr = FakeOcr::new(vec!["天空之翼", "80/90", "精炼1阶", ""]);

        let result = GoodWeaponScanner::scan_single_weapon(
            &ocr, None, &image, &scaler, &regions, &mappings, &config, 0, None,
        )
        .unwrap();

        match result {
            WeaponScanResult::Weapon(w) => {
                assert_eq!(w.level, 80);
                assert_eq!(w.ascension, 6);
            },
            other => panic!("Expected Weapon, got {:?}", std::mem::discriminant(&other)),
        }
    }
}
