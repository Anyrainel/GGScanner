use std::collections::HashMap;
use std::sync::Arc;

use crossbeam_channel;

use yas::{log_debug, log_info, log_warn};

use super::ui_actions::{d_action, d_cell};
use crate::scanner::artifact::GoodArtifactScanner;
use crate::scanner::common::annotator;
use crate::scanner::common::artifact_panel::{
    ArtifactCell, ArtifactPanelOptions, ArtifactSelection,
};
use crate::scanner::common::backpack_scanner::{
    self, BackpackScanConfig, BackpackScanner, GridEvent, PanelWaitMode, ScanAction,
    ScanTermination,
};
use crate::scanner::common::capture_frame::CaptureFrame;
use crate::scanner::common::constants::*;
use crate::scanner::common::coord_scaler::CoordScaler;
use crate::scanner::common::debug_dump::DumpCtx;
use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::grid_icon_detector::{GridIconResult, GridMode};
use crate::scanner::common::grid_voter::{
    GridAnnotation, GridVoteSchedule, PagedGridVoter, ReadyItem,
};
use crate::scanner::common::mappings::MappingManager;
use crate::scanner::common::models::GoodArtifact;
use crate::scanner::common::ocr_pool::{OcrPool, SharedOcrPools};
use crate::scanner::common::pixel_utils;

use super::matching;
use super::models::*;
use super::orchestrator::LockTarget;
use super::ui_actions;

/// Single-pass artifact backpack scan with per-page lock toggling.
///
/// Accepts `LockTarget` slices from the orchestrator. Each target specifies
/// an artifact to match and the desired lock state. Rarity early-stop:
/// when a scanned artifact has rarity < 4, the current page is finished
/// but no further items are dispatched.
///
/// 单次遍历圣遗物背包，每页扫描后直接切换锁定。
/// 当检测到稀有度 < 4 时，完成当前页后停止扫描。
pub struct LockManager {
    mappings: Arc<MappingManager>,
    pools: Arc<SharedOcrPools>,
}

/// A matched artifact whose authoritative panel lock state still needs to be
/// confirmed before deciding whether to toggle or report `AlreadyCorrect`.
struct PageLockAction {
    /// Absolute scanned artifact index.
    scanned_idx: usize,
    result_id: String,
    /// Row within the current visible page (0-based).
    row: usize,
    /// Column (0-based).
    col: usize,
    /// Desired lock state.
    desired_lock: bool,
    /// Lock state reported by the fast grid detector.
    grid_lock: bool,
    /// Y-shift for elixir artifacts.
    y_shift: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfirmedLockDecision {
    AlreadyCorrect,
    Toggle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetFilterPlan {
    /// Every requested set was selected; the filtered grid is safe to use.
    UseFiltered,
    /// No filter is active, so scan the complete inventory.
    ScanAll,
    /// A subset was selected. Clear it before scanning to avoid omitting the
    /// targets whose sets were not found in the filter panel.
    ClearAndScanAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct InventoryScanBounds {
    observed_count: usize,
    scan_limit: usize,
    count_confirmed: bool,
    capacity_confirmed: bool,
}

/// Traversal budget, not a claimed game capacity. The scanner normally stops
/// at independently confirmed empty/page-end evidence long before this limit.
/// Keeping a generous floor prevents a clipped denominator OCR (for example,
/// 210 instead of 2100) from truncating a real inventory.
const MANAGER_SCAN_BUDGET: usize = backpack_scanner::FILTERED_BACKPACK_SCAN_BUDGET;

fn inventory_scan_bounds(readings: &[(i32, i32)]) -> InventoryScanBounds {
    let observed_count = readings
        .iter()
        .map(|(count, _)| (*count).max(0) as usize)
        .max()
        .unwrap_or(0);
    let valid_reading =
        |(count, capacity): &(i32, i32)| *count >= 0 && *capacity > 0 && *count <= *capacity;
    let max_capacity = readings
        .iter()
        .filter(|reading| valid_reading(reading))
        .map(|(_, capacity)| *capacity as usize)
        .max();
    let scan_limit = if observed_count == 0 {
        0
    } else {
        // Count/capacity OCR only provides a lower-bound clue. A fixed budget
        // avoids both clipped early bounds and corrupt unbounded traversals.
        MANAGER_SCAN_BUDGET
    };
    let capacity_consistent = readings.len() >= 2
        && max_capacity.is_some()
        && readings
            .iter()
            .all(|reading| valid_reading(reading) && Some(reading.1 as usize) == max_capacity);
    let count_confirmed = readings.len() >= 2
        && capacity_consistent
        && readings
            .iter()
            .all(|(count, _)| (*count).max(0) as usize == observed_count);
    let capacity_confirmed = observed_count > 0 && capacity_consistent;

    InventoryScanBounds {
        observed_count,
        scan_limit,
        count_confirmed,
        capacity_confirmed,
    }
}

fn set_filter_plan(requested_count: usize, selected_count: usize) -> SetFilterPlan {
    if requested_count > 0 && selected_count == requested_count {
        SetFilterPlan::UseFiltered
    } else if selected_count == 0 {
        SetFilterPlan::ScanAll
    } else {
        SetFilterPlan::ClearAndScanAll
    }
}

fn should_stop_at_rarity_boundary(below_min_rarity: bool, level: i32) -> bool {
    // scan_level_only uses -1 as its OCR-failure sentinel. Only a confirmed
    // level 0 item proves that the sorted >=4-star section has ended.
    below_min_rarity && level == 0
}

fn repeated_rarity_below_min(first: Option<i32>, second: Option<i32>, min_rarity: i32) -> bool {
    first.is_some_and(|rarity| rarity < min_rarity)
        && second.is_some_and(|rarity| rarity < min_rarity)
}

fn visual_end_floor(observed_count: usize, filter_applied: bool) -> usize {
    // After a set filter is applied, the header continues to show the
    // unfiltered inventory count. It is not a lower bound for the visible
    // list; verified empty cells or a repeatedly immovable page are its end.
    if filter_applied {
        0
    } else {
        observed_count
    }
}

fn scan_proves_absence(
    termination: ScanTermination,
    rarity_stopped: bool,
    missed_count: usize,
    skipped_count: usize,
    identity_failure_count: usize,
    scanned_count: usize,
    observed_count: usize,
    filtered_or_uncertain: bool,
) -> bool {
    let reached_observed_count = scanned_count >= observed_count;
    missed_count == 0
        && skipped_count == 0
        && identity_failure_count == 0
        && !filtered_or_uncertain
        && (rarity_stopped || (reached_observed_count && termination == ScanTermination::EmptyCell))
}

fn decide_confirmed_lock_action(
    grid_lock: bool,
    panel_lock: bool,
    desired_lock: bool,
) -> (ConfirmedLockDecision, bool) {
    let decision = if panel_lock == desired_lock {
        ConfirmedLockDecision::AlreadyCorrect
    } else {
        ConfirmedLockDecision::Toggle
    };
    (decision, grid_lock != panel_lock)
}

fn update_scanned_lock_state(
    scanned_artifacts: &mut [(usize, GoodArtifact)],
    scanned_idx: usize,
    lock: bool,
) {
    if let Some((_, artifact)) = scanned_artifacts
        .iter_mut()
        .find(|(idx, _)| *idx == scanned_idx)
    {
        artifact.lock = lock;
        if !lock {
            artifact.astral_mark = false;
        }
    }
}

type OcrResult = (usize, usize, usize, Option<GoodArtifact>);

fn spawn_identify_artifact_task(
    idx: usize,
    row: usize,
    col: usize,
    frame: CaptureFrame,
    grid_icons: Option<GridIconResult>,
    grid_annotation: Option<GridAnnotation>,
    tx: crossbeam_channel::Sender<OcrResult>,
    ocr_pool: Arc<OcrPool>,
    substat_pool: Arc<OcrPool>,
    scaler: Arc<CoordScaler>,
    mappings: Arc<MappingManager>,
) {
    rayon::spawn(move || {
        let _native_crash_context = yas::native_crash::inherit_current_task();
        annotator::begin_item("manager_artifacts", idx, &scaler);
        annotator::add_frame("panel", &frame);
        if let Some(ref ann) = grid_annotation {
            annotator::record_grid_overlay(ann.0.clone(), ann.1.clone());
        }

        let ocr = ocr_pool.get();
        let sub_ocr = substat_pool.get();
        let artifact = match GoodArtifactScanner::identify_artifact_at(
            &ocr as &dyn yas::ocr::ImageToText<image::RgbImage>,
            &sub_ocr as &dyn yas::ocr::ImageToText<image::RgbImage>,
            &frame,
            &scaler,
            &mappings,
            idx,
            grid_icons,
        ) {
            Ok(a) => a,
            Err(e) => {
                log_warn!(
                    "[lock_manager] OCR失败 #{}: {}",
                    "[lock_manager] OCR failed #{}: {}",
                    idx,
                    e
                );
                annotator::finalize_error(None, &e.to_string());
                None
            },
        };
        let _ = tx.send((idx, row, col, artifact));
    });
}

impl LockManager {
    pub fn new(mappings: Arc<MappingManager>, pools: Arc<SharedOcrPools>) -> Self {
        Self { mappings, pools }
    }

    /// Execute lock change targets by scanning the artifact backpack.
    ///
    /// Returns:
    /// - Results for processed targets
    /// - Scanned artifacts as (index, artifact)
    /// - Map from target vec index -> scanned artifact index (for snapshot building)
    /// - Whether the scan completed fully (all items visited, no interruption)
    ///
    /// 执行锁定变更目标。返回：
    /// - 已处理目标的结果
    /// - 扫描到的圣遗物列表 (index, artifact)
    /// - 目标向量索引→圣遗物索引的映射
    /// - 扫描是否完整
    pub fn execute(
        &self,
        ctrl: &mut GenshinGameController,
        targets: &[LockTarget],
        capture_delay: u64,
        delay_scroll: u64,
        panel_timeout: u64,
        initial_wait: u64,
        stop_on_all_matched: bool,
        filter_involved_sets: bool,
        max_target_level: i32,
        dump_images: bool,
        progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
    ) -> (
        Vec<InstructionResult>,
        Vec<(usize, GoodArtifact)>,
        HashMap<usize, usize>,
        bool,
        usize,
    ) {
        let mut results: HashMap<String, InstructionResult> = HashMap::new();
        let mut scanned_artifacts: Vec<(usize, GoodArtifact)> = Vec::new();
        let mut ocr_failures: usize = 0;

        let make_error_results = |targets: &[LockTarget],
                                  status: InstructionStatus,
                                  hint_zh: &str,
                                  hint_en: &str,
                                  source: Option<&anyhow::Error>|
         -> Vec<InstructionResult> {
            targets
                .iter()
                .map(|t| {
                    InstructionResult::failure(
                        t.result_id.clone(),
                        status.clone(),
                        hint_zh,
                        hint_en,
                        source,
                    )
                })
                .collect()
        };

        if targets.is_empty() {
            return (Vec::new(), scanned_artifacts, HashMap::new(), false, 0);
        }

        // Use shared OCR pools (v5 for level, v4 for everything else).
        let ocr_pool = self.pools.artifact().v5().clone();
        let substat_pool = self.pools.artifact().v4().clone();
        // Borrow a model from the v5 pool for reading item count
        let count_ocr_guard = ocr_pool.get();

        // Track which targets have been matched (target vec index -> scanned artifact index)
        let mut matched: HashMap<usize, usize> = HashMap::new();

        // --- Open backpack to artifact tab (same as artifact scanner) ---
        let first_count_reading = match backpack_scanner::open_backpack_to_tab(
            ctrl,
            "artifact",
            1200,
            400,
            &count_ocr_guard,
            false,
            dump_images,
        ) {
            Ok(reading) => reading,
            Err(e) => {
                log_warn!(
                    "无法读取圣遗物数量: {}",
                    "Cannot read artifact count: {}",
                    e
                );
                return (
                    make_error_results(
                        targets,
                        InstructionStatus::OcrError,
                        "无法读取背包中的圣遗物数量，因此没有执行锁定变更。请打开圣遗物背包后重试。",
                        "The artifact count could not be read, so no lock changes were made. Open the artifact inventory and retry.",
                        Some(&e),
                    ),
                    scanned_artifacts,
                    HashMap::new(),
                    false,
                    0,
                );
            },
        };

        // A filter left active by an earlier equip/manage workflow must not
        // hide artifacts from an ordinary full scan. The involved-set path
        // clears the selector inside apply_backpack_multi_set_filter instead.
        if !filter_involved_sets {
            log_debug!(
                "[lock_manager] 扫描完整背包前清除已有套装筛选",
                "[lock_manager] Clearing any existing set filter before the full inventory scan"
            );
            let filter_ocr_guard = substat_pool.get();
            if let Err(e) = ui_actions::clear_backpack_set_filter(
                ctrl,
                &self.mappings,
                &filter_ocr_guard as &dyn yas::ocr::ImageToText<image::RgbImage>,
            ) {
                log_warn!(
                    "[lock_manager] 无法确认已有套装筛选已清除: {}",
                    "[lock_manager] Could not verify that the existing set filter was cleared: {}",
                    e
                );
                return (
                    make_error_results(
                        targets,
                        InstructionStatus::UiError,
                        "无法确认套装筛选已清除，因此没有开始锁定扫描。请保持圣遗物背包打开后重试。",
                        "The set filter could not be verified as cleared, so the lock scan was not started. Keep the artifact inventory open and retry.",
                        Some(&e),
                    ),
                    scanned_artifacts,
                    HashMap::new(),
                    false,
                    0,
                );
            }
        }

        // One clipped or transient OCR read must not become the traversal
        // boundary. Re-read the header after filter normalization, then use a
        // bounded internal traversal budget; empty-cell/page-end evidence
        // determines the actual end in normal inventories.
        yas::utils::sleep(100);
        let mut count_readings = vec![first_count_reading];
        {
            let bp = BackpackScanner::new(ctrl);
            match bp.read_item_count(&count_ocr_guard) {
                Ok(reading) => count_readings.push(reading),
                Err(e) => log_warn!(
                    "[lock_manager] 第二次圣遗物数量读取失败，将使用保守扫描上限: {}",
                    "[lock_manager] The second artifact-count read failed; using a conservative scan limit: {}",
                    e
                ),
            }
        }
        let scan_bounds = inventory_scan_bounds(&count_readings);
        let total = scan_bounds.scan_limit;
        let progress_total = scan_bounds.observed_count.max(1);
        log_debug!(
            "[lock_manager] 圣遗物数量读数={:?}, 当前数量={}, 扫描上限={}, 容量已确认={}",
            "[lock_manager] Artifact count readings={:?}, observed count={}, scan limit={}, capacity confirmed={}",
            count_readings,
            scan_bounds.observed_count,
            total,
            scan_bounds.capacity_confirmed
        );

        let mut filter_applied = false;
        let mut filter_state_uncertain = false;
        if scan_bounds.observed_count > 0 && filter_involved_sets {
            let mut involved_sets: Vec<&str> = Vec::new();
            for target in targets {
                let set_key = target.artifact.set_key.as_str();
                if !involved_sets.contains(&set_key) {
                    involved_sets.push(set_key);
                }
            }

            if !involved_sets.is_empty() {
                let filter_ocr_guard = substat_pool.get();
                let requested_count = involved_sets.len();
                match ui_actions::apply_backpack_multi_set_filter(
                    ctrl,
                    &involved_sets,
                    &self.mappings,
                    &filter_ocr_guard as &dyn yas::ocr::ImageToText<image::RgbImage>,
                    dump_images,
                ) {
                    Ok(selected_count) => match set_filter_plan(requested_count, selected_count) {
                        SetFilterPlan::UseFiltered => {
                            filter_applied = true;
                            // 筛选后游戏UI右上角仍显示总容量而非筛选后数量，
                            // 无法通过 read_item_count 获取准确的筛选后数量。
                            // 保留筛选前的 total（总容量），依赖 detect_empty_cells
                            // 机制在扫描遇到空格子时自动停止。
                            log_info!(
                                "[lock_manager] 已筛选全部{}个相关套装（筛选后UI显示总容量，将依赖空格子检测停止扫描，total={}）",
                                "[lock_manager] Filtered all {} involved sets (UI shows total capacity after filter; will rely on empty-cell detection, total={})",
                                selected_count,
                                total
                            );
                        },
                        SetFilterPlan::ScanAll => {
                            filter_state_uncertain = true;
                            log_warn!(
                                "[lock_manager] 未能应用相关套装筛选，将继续扫描完整圣遗物列表",
                                "[lock_manager] Could not apply involved set filter, scanning full artifact list"
                            );
                        },
                        SetFilterPlan::ClearAndScanAll => {
                            filter_state_uncertain = true;
                            log_warn!(
                                "[lock_manager] 仅筛选到{}/{}个相关套装；为避免遗漏目标，将清除部分筛选并扫描完整圣遗物列表",
                                "[lock_manager] Only {}/{} involved sets were filtered; clearing the partial filter and scanning the full artifact list to avoid omitted targets",
                                selected_count,
                                requested_count
                            );
                            if let Err(e) = ui_actions::clear_backpack_set_filter(
                                ctrl,
                                &self.mappings,
                                &filter_ocr_guard as &dyn yas::ocr::ImageToText<image::RgbImage>,
                            ) {
                                log_warn!(
                                    "[lock_manager] 无法确认部分套装筛选已清除: {}",
                                    "[lock_manager] Could not verify that the partial set filter was cleared: {}",
                                    e
                                );
                                return (
                                    make_error_results(
                                        targets,
                                        InstructionStatus::UiError,
                                        "只应用了部分套装筛选，且无法确认这些筛选已清除，因此没有开始锁定扫描。请重试。",
                                        "Only part of the set filter was applied, and it could not be verified as cleared, so the lock scan was not started. Retry the operation.",
                                        Some(&e),
                                    ),
                                    scanned_artifacts,
                                    HashMap::new(),
                                    false,
                                    0,
                                );
                            }
                        },
                    },
                    Err(e) => {
                        filter_state_uncertain = true;
                        log_warn!(
                            "[lock_manager] 套装筛选失败，将继续扫描完整圣遗物列表: {}",
                            "[lock_manager] Set filter failed, scanning full artifact list: {}",
                            e
                        );
                    },
                }
            }
        }

        if scan_bounds.observed_count == 0 {
            log_info!(
                "[lock_manager] 背包中没有圣遗物，无法执行锁定操作",
                "[lock_manager] No artifacts in backpack, cannot perform lock operations"
            );
            let (status, hint_zh, hint_en) = if scan_bounds.count_confirmed {
                (
                    InstructionStatus::NotFound,
                    "背包中没有可匹配的圣遗物，因此无法执行锁定变更。",
                    "There are no matching artifacts in the inventory, so the lock change could not be made.",
                )
            } else {
                (
                    InstructionStatus::OcrError,
                    "无法确认背包中的圣遗物数量，因此没有执行锁定变更。请保持圣遗物背包打开后重试。",
                    "The artifact count could not be confirmed, so no lock changes were made. Keep the artifact inventory open and retry.",
                )
            };
            return (
                make_error_results(targets, status, hint_zh, hint_en, None),
                scanned_artifacts,
                HashMap::new(),
                true, // empty backpack is a "complete" scan
                0,
            );
        }

        // Return count OCR model to pool before scan loop
        drop(count_ocr_guard);

        let scaler = ctrl.scaler.clone();
        let scaler_arc = Arc::new(scaler.clone());

        // Pre-focus the first grid cell (unchanged behavior from previous impl).
        ctrl.click_at(GRID_FIRST_X, GRID_FIRST_Y);
        yas::utils::sleep(d_action() * 3 / 8);

        // --- Per-page callback state ---
        // A fresh OCR result channel is created before each page; PageCompleted
        // drains it and re-creates one for the next page.
        let (init_tx, init_rx) = crossbeam_channel::unbounded::<OcrResult>();
        let mut result_tx: crossbeam_channel::Sender<OcrResult> = init_tx;
        let mut result_rx: crossbeam_channel::Receiver<OcrResult> = init_rx;
        let mut dispatched: usize = 0;

        // Per-page 3-pass voter (payload carries the (row, col) needed to
        // re-click the grid cell for lock toggling).
        let mut voter: PagedGridVoter<(usize, usize)> =
            PagedGridVoter::new(total, GridMode::Artifact);

        // Scan-wide flags.
        let mut rarity_stopped = false;
        let mut stop_requested = false;
        let mut lock_action_counter: usize = 0;
        // Once the last-cell probe shows a level within range (or OCR succeeds
        // with level <= max_target_level), we stop probing and scan every item.
        // OCR failure (-1) does NOT count as "in range" — keep probing.
        let mut level_in_range = false;

        // Build a scan_grid config that enables the per-page level probe
        // when a max target level has been computed (fast mode).
        let scan_config = BackpackScanConfig {
            delay_scroll,
            panel_wait: PanelWaitMode::Artifact {
                timeout_ms: panel_timeout,
                initial_wait_ms: initial_wait,
                stop_on_failure: false,
            },
            extra_delay: capture_delay,
            detail_panel_rect: None,
            grid_vote_schedule: GridVoteSchedule::for_page,
            probe_last_cell_per_page: max_target_level >= 0,
            // The OCR count is only a hint; traverse up to the confirmed
            // inventory capacity and use visual occupancy/page-end evidence to
            // find the real boundary. This also handles filtered inventories,
            // whose header keeps showing the unfiltered count.
            detect_grid_duplicates: false,
            detect_empty_cells: true,
            min_items_before_visual_end: visual_end_floor(
                scan_bounds.observed_count,
                filter_applied,
            ),
        };

        // Clones for closure capture.
        let ocr_pool_cb = ocr_pool.clone();
        let substat_pool_cb = substat_pool.clone();
        let mappings_cb = self.mappings.clone();
        let scaler_cb = scaler.clone();

        // Report the observed inventory count to clients; the much larger
        // traversal budget is an internal safeguard against clipped OCR.
        if let Some(pf) = progress_fn {
            pf(0, progress_total, "", "锁定变更 / Lock changes");
        }

        let mut bp = BackpackScanner::new(ctrl);
        let scan_outcome = bp.scan_grid(total, &scan_config, 0, |ctrl_cb, event| {
            match event {
                // ---------------- Page probe: level-based skip ----------------
                GridEvent::PageStarted { page_start_idx, last_cell_image } => {
                    if max_target_level < 0 || level_in_range {
                        return ScanAction::Continue;
                    }
                    let ocr_guard = ocr_pool_cb.get();
                    let probe_frame = CaptureFrame::full(last_cell_image);
                    let level = GoodArtifactScanner::scan_level_only(
                        &ocr_guard as &dyn yas::ocr::ImageToText<image::RgbImage>,
                        &probe_frame,
                        &scaler_cb,
                    );
                    if level > max_target_level {
                        log_debug!(
                            "[lock_manager] 页面跳过：末尾等级={} > 最高目标等级={} (page_start={})",
                            "[lock_manager] Page skip: last level={} > max target={} (page_start={})",
                            level, max_target_level, page_start_idx
                        );
                        ScanAction::SkipPage
                    } else if level >= 0 {
                        // Level is in range — stop probing on future pages.
                        level_in_range = true;
                        log_debug!(
                            "[lock_manager] 末尾等级={} 在范围内，后续页面不再探测 (page_start={})",
                            "[lock_manager] Last level={} in range, disabling probe for future pages (page_start={})",
                            level, page_start_idx
                        );
                        ScanAction::Continue
                    } else {
                        // OCR failed (-1) — scan this page but keep probing.
                        log_debug!(
                            "[lock_manager] 末尾等级OCR失败，扫描此页但继续探测 (page_start={})",
                            "[lock_manager] Last level OCR failed, scanning page but keeping probe (page_start={})",
                            page_start_idx
                        );
                        ScanAction::Continue
                    }
                }

                // ---------------- Per-item voting + OCR dispatch ----------------
                GridEvent::Item {
                    idx,
                    row,
                    col,
                    layout,
                    frame,
                } => {
                    // Tick progress per item as we walk through the backpack.
                    // Reports (idx+1, backpack_total) so clients see a real
                    // moving number rather than 0/N until the very end.
                    if let Some(pf) = progress_fn {
                        pf(
                            (idx + 1).min(progress_total),
                            progress_total,
                            "",
                            "锁定变更 / Lock changes",
                        );
                    }

                    // Dispatch helper: spawns a rayon OCR task for one ready item.
                    let dispatch = |ready: Vec<ReadyItem<(usize, usize)>>,
                                    dispatched: &mut usize| {
                        for item in ready {
                            let d_idx = item.idx;
                            let (d_row, d_col) = item.payload;
                            let d_frame = item.frame;
                            let gi: Option<GridIconResult> = item.metadata;
                            let ann = item.grid_annotation;
                            let tx = result_tx.clone();
                            let pool = ocr_pool_cb.clone();
                            let sub_pool = substat_pool_cb.clone();
                            let sc = scaler_arc.clone();
                            let mp = mappings_cb.clone();
                            spawn_identify_artifact_task(
                                d_idx, d_row, d_col, d_frame, gi, ann, tx, pool, sub_pool, sc, mp,
                            );
                            *dispatched += 1;
                        }
                    };

                    // Rarity early-stop: low-rarity lv0 artifact → stop after
                    // current page finishes (PageCompleted will drain and
                    // process toggles for whatever was dispatched so far).
                    let first_rarity = pixel_utils::detect_artifact_rarity_evidence(
                        &frame,
                        &scaler_cb,
                    );
                    let mut settled_frame = None;
                    let below_min_rarity = if first_rarity.is_some_and(|rarity| rarity < 4) {
                        // The star row is outside the fast panel fingerprint.
                        // Confirm the same low-rarity geometry in a later full
                        // capture so a partially rendered 4/5-star row cannot
                        // become an authoritative inventory boundary.
                        yas::utils::sleep(100);
                        match ctrl_cb.capture_game() {
                            Ok(image) => {
                                let second_frame = CaptureFrame::full(image);
                                let second_rarity = pixel_utils::detect_artifact_rarity_evidence(
                                    &second_frame,
                                    &scaler_cb,
                                );
                                let confirmed =
                                    repeated_rarity_below_min(first_rarity, second_rarity, 4);
                                settled_frame = Some(second_frame);
                                confirmed
                            },
                            Err(e) => {
                                log_warn!(
                                    "[lock_manager] 无法复核低稀有度外观，将继续扫描: {}",
                                    "[lock_manager] Could not confirm the low-rarity appearance; continuing the scan: {}",
                                    e
                                );
                                false
                            },
                        }
                    } else {
                        false
                    };
                    let evidence_frame = settled_frame.as_ref().unwrap_or(&frame);
                    let boundary_level = if below_min_rarity {
                        let guard = ocr_pool_cb.get();
                        GoodArtifactScanner::scan_level_only(&guard, evidence_frame, &scaler_cb)
                    } else {
                        -1
                    };
                    if should_stop_at_rarity_boundary(below_min_rarity, boundary_level) {
                        log_debug!(
                            "[lock_manager] 检测到低稀有度lv0圣遗物，当前页后停止",
                            "[lock_manager] Low rarity lv0 artifact detected, stopping after current page"
                        );
                        rarity_stopped = true;
                        stop_requested = true;
                        voter.finish_additional_passes(&scaler_cb, || ctrl_cb.capture_game().ok());
                        let ready = voter.early_stop_flush();
                        dispatch(ready, &mut dispatched);
                        return ScanAction::Stop;
                    }
                    if below_min_rarity && boundary_level < 0 {
                        log_warn!(
                            "[lock_manager] 检测到低稀有度外观，但等级OCR失败；无法确认边界，将继续扫描",
                            "[lock_manager] The item looked low-rarity but level OCR failed; the inventory boundary is unconfirmed, continuing the scan"
                        );
                    }

                    let frame = settled_frame.unwrap_or(frame);
                    let ready = voter.record_with_layout(
                        idx,
                        frame,
                        (row, col),
                        &scaler_cb,
                        layout.page_start_idx,
                        layout.page_items,
                        layout.screen_start_row,
                    );
                    dispatch(ready, &mut dispatched);

                    ScanAction::Continue
                }

                // ---------------- Drain + match + toggle locks ----------------
                GridEvent::PageCompleted { .. } => {
                    // Finish short-page voting while the current page is still
                    // visible; final_flush only drains settled metadata.
                    voter.finish_additional_passes(&scaler_cb, || ctrl_cb.capture_game().ok());
                    let leftover = voter.final_flush();
                    for item in leftover {
                        let d_idx = item.idx;
                        let (d_row, d_col) = item.payload;
                        let d_frame = item.frame;
                        let gi: Option<GridIconResult> = item.metadata;
                        let ann = item.grid_annotation;
                        let tx = result_tx.clone();
                        let pool = ocr_pool_cb.clone();
                        let sub_pool = substat_pool_cb.clone();
                        let sc = scaler_arc.clone();
                        let mp = mappings_cb.clone();
                        spawn_identify_artifact_task(
                            d_idx, d_row, d_col, d_frame, gi, ann, tx, pool, sub_pool, sc, mp,
                        );
                        dispatched += 1;
                    }

                    // Drain OCR results for this page.
                    let (fresh_tx, fresh_rx) = crossbeam_channel::unbounded::<OcrResult>();
                    let old_tx = std::mem::replace(&mut result_tx, fresh_tx);
                    drop(old_tx);
                    let old_rx = std::mem::replace(&mut result_rx, fresh_rx);
                    let mut page_results: Vec<OcrResult> = Vec::with_capacity(dispatched);
                    for r in old_rx {
                        page_results.push(r);
                    }
                    page_results.sort_by_key(|(idx, _, _, _)| *idx);
                    let missing_worker_results = dispatched.saturating_sub(page_results.len());
                    if missing_worker_results > 0 {
                        ocr_failures += missing_worker_results;
                        log_warn!(
                            "[lock_manager] {}个已派发的OCR任务未返回结果；本次扫描不能证明目标不存在",
                            "[lock_manager] {} dispatched OCR tasks returned no result; this scan cannot prove target absence",
                            missing_worker_results
                        );
                    }
                    dispatched = 0;

                    // Match against unmatched targets. Grid lock state is only
                    // the fast scan observation; every matched artifact is
                    // re-opened below so the settled panel decides whether the
                    // manager toggles or reports AlreadyCorrect.
                    let mut page_actions: Vec<PageLockAction> = Vec::new();
                    for (idx, row, col, artifact_opt) in &page_results {
                        if let Some(ref artifact) = artifact_opt {
                            scanned_artifacts.push((*idx, artifact.clone()));

                            let unmatched: Vec<(usize, &GoodArtifact)> = targets.iter()
                                .enumerate()
                                .filter(|(i, _)| !matched.contains_key(i))
                                .map(|(i, t)| (i, &t.artifact))
                                .collect();

                            if let Some((target_idx, _score)) =
                                matching::find_best_match(artifact, &unmatched)
                            {
                                matched.insert(target_idx, *idx);
                                let target = &targets[target_idx];
                                let y_shift = if artifact.elixir_crafted { 40.0 } else { 0.0 };
                                page_actions.push(PageLockAction {
                                    scanned_idx: *idx,
                                    result_id: target.result_id.clone(),
                                    row: *row,
                                    col: *col,
                                    desired_lock: target.desired_lock,
                                    grid_lock: artifact.lock,
                                    y_shift,
                                });
                            }
                        } else {
                            ocr_failures += 1;
                        }
                    }

                    // Confirm every matched target from its settled detail
                    // panel, then toggle only when that authoritative state
                    // differs from the requested state.
                    for action in &page_actions {
                        if ctrl_cb.check_rmb() {
                            results.insert(
                                action.result_id.clone(),
                                InstructionResult::failure(
                                    action.result_id.clone(),
                                    InstructionStatus::Aborted,
                                    "用户已停止此锁定操作。",
                                    "This lock operation was stopped by the user.",
                                    None,
                                ),
                            );
                            continue;
                        }

                        let action_idx = lock_action_counter;
                        lock_action_counter += 1;
                        let mut panel_image = match ctrl_cb.select_artifact(
                            ArtifactCell { row: action.row, col: action.col },
                            ArtifactPanelOptions {
                                timeout_ms: panel_timeout,
                                initial_wait_ms: initial_wait,
                                // Lock icons animate separately from text.
                                extra_delay_ms: capture_delay.max(d_cell() as u64),
                            },
                            true,
                        ) {
                            Ok(ArtifactSelection::Ready(frame)) => frame.image,
                            Ok(ArtifactSelection::Cancelled) => {
                                results.insert(
                                    action.result_id.clone(),
                                    InstructionResult::failure(
                                        action.result_id.clone(), InstructionStatus::Aborted,
                                        "用户已停止此锁定操作。",
                                        "This lock operation was stopped by the user.", None,
                                    ),
                                );
                                continue;
                            },
                            other => {
                                let error = other.err();
                                log_warn!("[lock_manager] 目标详情未确认，跳过锁定操作", "[lock_manager] Target detail unconfirmed; skipping lock operation");
                                results.insert(
                                    action.result_id.clone(),
                                    InstructionResult::failure(
                                        action.result_id.clone(),
                                        InstructionStatus::UiError,
                                        "无法确认目标圣遗物的详情已加载，因此没有更改锁定状态。",
                                        "The target artifact's detail panel could not be confirmed ready, so its lock state was not changed.",
                                        error.as_ref(),
                                    ),
                                );
                                continue;
                            }
                        };
                        if pixel_utils::is_artifact_lock_ambiguous(
                            &panel_image,
                            &scaler_cb,
                            action.y_shift,
                        ) {
                            yas::utils::sleep(d_cell());
                            panel_image = match ctrl_cb.capture_game() {
                                Ok(img) => img,
                                Err(e) => {
                                    log_warn!("[lock_manager] 面板确认重试截图失败: {}", "[lock_manager] Panel confirmation retry capture failed: {}", e);
                                    results.insert(
                                        action.result_id.clone(),
                                        InstructionResult::failure(
                                            action.result_id.clone(),
                                            InstructionStatus::UiError,
                                            "重试后仍无法读取目标圣遗物的详情画面，因此没有更改锁定状态。",
                                            "The target artifact's detail panel still could not be captured after retrying, so its lock state was not changed.",
                                            Some(&e),
                                        ),
                                    );
                                    continue;
                                }
                            };
                            if pixel_utils::is_artifact_lock_ambiguous(
                                &panel_image,
                                &scaler_cb,
                                action.y_shift,
                            ) {
                                log_warn!(
                                    "[lock_manager] 面板锁定状态仍不明确 ({},{}), 跳过操作",
                                    "[lock_manager] Panel lock state remains ambiguous ({},{}); skipping action",
                                    action.row,
                                    action.col
                                );
                                results.insert(
                                    action.result_id.clone(),
                                    InstructionResult::failure(
                                        action.result_id.clone(),
                                        InstructionStatus::UiError,
                                        "无法可靠判断目标圣遗物当前是否已锁定，因此为避免误操作已跳过。",
                                        "The target artifact's current lock state could not be determined reliably, so it was skipped to avoid a wrong change.",
                                        None,
                                    ),
                                );
                                continue;
                            }
                        }
                        let panel_lock = pixel_utils::detect_artifact_lock(
                            &panel_image,
                            &scaler_cb,
                            action.y_shift,
                        );
                        let (decision, grid_disagrees) = decide_confirmed_lock_action(
                            action.grid_lock,
                            panel_lock,
                            action.desired_lock,
                        );
                        update_scanned_lock_state(
                            &mut scanned_artifacts,
                            action.scanned_idx,
                            panel_lock,
                        );

                        if grid_disagrees {
                            log_warn!(
                                "[lock_manager] 网格与面板锁定状态不一致 ({},{}): 网格={} 面板={}，采用面板状态",
                                "[lock_manager] Grid/panel lock disagreement ({},{}): grid={} panel={}; using panel state",
                                action.row,
                                action.col,
                                action.grid_lock,
                                panel_lock
                            );
                        }
                        if dump_images {
                            let ctx = DumpCtx::new(
                                "debug_images", "manager_lock_confirm", action_idx, "",
                            );
                            ctx.dump_full(&panel_image);
                            ctx.dump_pixel(
                                "lock_px",
                                &panel_image,
                                (ARTIFACT_LOCK_POS1.0, ARTIFACT_LOCK_POS1.1 + action.y_shift),
                                5,
                                &scaler_cb,
                            );
                        }
                        if decision == ConfirmedLockDecision::AlreadyCorrect {
                            results.insert(
                                action.result_id.clone(),
                                InstructionResult::outcome(
                                    action.result_id.clone(),
                                    InstructionStatus::AlreadyCorrect,
                                ),
                            );
                            continue;
                        }

                        if let Err(e) = ui_actions::click_lock_button(ctrl_cb, action.y_shift) {
                            log_warn!("[lock_manager] 锁定切换失败: {}", "[lock_manager] Lock toggle failed: {}", e);
                            results.insert(
                                action.result_id.clone(),
                                InstructionResult::failure(
                                    action.result_id.clone(),
                                    InstructionStatus::UiError,
                                    "无法点击游戏中的锁定按钮，因此锁定状态没有改变。",
                                    "The in-game lock button could not be clicked, so the lock state was not changed.",
                                    Some(&e),
                                ),
                            );
                            continue;
                        }

                        yas::utils::sleep(d_cell() * 2);
                        let image = match ctrl_cb.capture_game() {
                            Ok(img) => img,
                            Err(e) => {
                                log_warn!("[lock_manager] 截图失败: {}", "[lock_manager] Capture failed: {}", e);
                                results.insert(
                                    action.result_id.clone(),
                                    InstructionResult::failure(
                                        action.result_id.clone(),
                                        InstructionStatus::UiError,
                                        "点击锁定按钮后无法读取游戏画面，因此无法确认更改是否成功。",
                                        "The game screen could not be captured after clicking the lock button, so the change could not be confirmed.",
                                        Some(&e),
                                    ),
                                );
                                continue;
                            }
                        };
                        let verified = pixel_utils::verify_artifact_lock_toggled(
                            &image, &scaler_cb, action.y_shift, action.desired_lock,
                        );
                        let new_lock = if verified {
                            action.desired_lock
                        } else {
                            pixel_utils::detect_artifact_lock(&image, &scaler_cb, action.y_shift)
                        };
                        update_scanned_lock_state(
                            &mut scanned_artifacts,
                            action.scanned_idx,
                            new_lock,
                        );

                        if dump_images {
                            let ctx = DumpCtx::new(
                                "debug_images", "manager_lock_verify", action_idx, "",
                            );
                            ctx.dump_full(&image);
                            ctx.dump_pixel("lock_px", &image, ARTIFACT_LOCK_POS1, 5, &scaler_cb);
                        }
                        if new_lock == action.desired_lock {
                            results.insert(
                                action.result_id.clone(),
                                InstructionResult::outcome(
                                    action.result_id.clone(),
                                    InstructionStatus::Success,
                                ),
                            );
                        } else {
                            log_warn!(
                                "[lock_manager] 锁定验证失败 ({},{}): 期望={} 实际={}",
                                "[lock_manager] Lock verify failed ({},{}): expected={} actual={}",
                                action.row, action.col, action.desired_lock, new_lock
                            );
                            results.insert(
                                action.result_id.clone(),
                                InstructionResult::failure(
                                    action.result_id.clone(),
                                    InstructionStatus::UiError,
                                    format!(
                                        "游戏中的锁定状态没有变为请求的值（期望={}，实际={}）。",
                                        action.desired_lock, new_lock
                                    ),
                                    format!(
                                        "The in-game lock state did not change to the requested value (expected={}, actual={}).",
                                        action.desired_lock, new_lock
                                    ),
                                    None,
                                ),
                            );
                        }
                    }

                    // Early stop if all targets matched (fast mode only).
                    if stop_on_all_matched && matched.len() == targets.len() {
                        log_info!("[lock_manager] 所有目标已匹配，提前停止", "[lock_manager] All targets matched, stopping early");
                        stop_requested = true;
                    }

                    if stop_requested {
                        ScanAction::Stop
                    } else {
                        ScanAction::Continue
                    }
                }

                // ---------------- Reset voter state for next page ----------------
                GridEvent::PageScrolled => {
                    voter.reset_page();
                    ScanAction::Continue
                }
            }
        });
        drop(bp);

        if dump_images {
            annotator::flush();
        }

        let solver_failures = scanned_artifacts
            .iter()
            .filter(|(_, artifact)| artifact.total_rolls.is_none())
            .count();
        let identity_failures = ocr_failures + solver_failures;

        log_debug!(
            "[lock_manager] 扫描结束: {:?}, 已遍历位置={}, 未确认位置={}, 快速跳过位置={}, 识别失败={}",
            "[lock_manager] Scan ended: {:?}, traversed positions={}, unconfirmed positions={}, shortcut-skipped positions={}, identity failures={}",
            scan_outcome.termination,
            scan_outcome.scanned_count,
            scan_outcome.missed_count,
            scan_outcome.skipped_count,
            identity_failures
        );

        // Compute scan completeness from traversal, rather than the last OCR
        // success. Rarity early-stop counts as complete because a confirmed
        // low-rarity level-0 item is the logical end of the managed >=4★
        // section. Fast mode remains partial because it may skip pages.
        let absence_confirmed = scan_proves_absence(
            scan_outcome.termination,
            rarity_stopped,
            scan_outcome.missed_count,
            scan_outcome.skipped_count,
            identity_failures,
            scan_outcome.scanned_count,
            scan_bounds.observed_count,
            filter_applied || filter_state_uncertain,
        );
        let scan_complete = !stop_on_all_matched && absence_confirmed && !ctrl.is_cancelled();

        // Mark unmatched targets. Heuristic filtered endings and callback
        // interruptions cannot prove absence, so they must never be reported
        // as NotFound.
        let was_cancelled = ctrl.is_cancelled();
        for target in targets {
            if !results.contains_key(&target.result_id) {
                results.insert(
                    target.result_id.clone(),
                    if was_cancelled || scan_outcome.termination == ScanTermination::Cancelled {
                        InstructionResult::failure(
                            target.result_id.clone(),
                            InstructionStatus::Aborted,
                            "用户已停止此锁定操作。",
                            "This lock operation was stopped by the user.",
                            None,
                        )
                    } else if absence_confirmed {
                        InstructionResult::failure(
                            target.result_id.clone(),
                            InstructionStatus::NotFound,
                            "未能在背包中找到匹配的圣遗物。请确认背包内容和目标数据仍然一致。",
                            "A matching artifact was not found in the inventory. Check that the inventory and target data are still in sync.",
                            None,
                        )
                    } else {
                        InstructionResult::failure(
                            target.result_id.clone(),
                            InstructionStatus::Skipped,
                            "扫描在确认所有相关背包位置前结束，因此没有把此圣遗物标记为不存在。请保持圣遗物背包打开后重试；若已启用套装筛选，请关闭后再试。",
                            "The scan ended before every relevant inventory position was confirmed, so this artifact was not marked missing. Keep the artifact inventory open and retry; if set filtering is enabled, turn it off for the retry.",
                            None,
                        )
                    },
                );
            }
        }

        let ordered_results: Vec<InstructionResult> = targets
            .iter()
            .filter_map(|t| results.remove(&t.result_id))
            .collect();

        (
            ordered_results,
            scanned_artifacts,
            matched,
            scan_complete,
            ocr_failures,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_lock_overrides_false_unlocked_grid_before_unlock() {
        let (decision, grid_disagrees) = decide_confirmed_lock_action(false, true, false);

        assert_eq!(decision, ConfirmedLockDecision::Toggle);
        assert!(grid_disagrees);
    }

    #[test]
    fn panel_lock_overrides_false_locked_grid_before_lock() {
        let (decision, grid_disagrees) = decide_confirmed_lock_action(true, false, true);

        assert_eq!(decision, ConfirmedLockDecision::Toggle);
        assert!(grid_disagrees);
    }

    #[test]
    fn panel_state_controls_already_correct_even_when_grid_disagrees() {
        let (decision, grid_disagrees) = decide_confirmed_lock_action(true, false, false);

        assert_eq!(decision, ConfirmedLockDecision::AlreadyCorrect);
        assert!(grid_disagrees);
    }

    #[test]
    fn set_filter_must_be_all_or_nothing() {
        assert_eq!(set_filter_plan(3, 3), SetFilterPlan::UseFiltered);
        assert_eq!(set_filter_plan(3, 2), SetFilterPlan::ClearAndScanAll);
        assert_eq!(set_filter_plan(3, 0), SetFilterPlan::ScanAll);
        assert_eq!(set_filter_plan(0, 0), SetFilterPlan::ScanAll);
    }

    #[test]
    fn scan_bound_uses_capacity_instead_of_a_possibly_clipped_count() {
        let bounds = inventory_scan_bounds(&[(200, 2100), (200, 2100)]);
        assert_eq!(bounds.observed_count, 200);
        assert_eq!(bounds.scan_limit, MANAGER_SCAN_BUDGET);
        assert!(bounds.count_confirmed);
        assert!(bounds.capacity_confirmed);

        let mismatched = inventory_scan_bounds(&[(1200, 2100), (200, 2100)]);
        assert_eq!(mismatched.scan_limit, MANAGER_SCAN_BUDGET);
        assert!(!mismatched.count_confirmed);
        assert!(mismatched.capacity_confirmed);

        let clipped_capacity = inventory_scan_bounds(&[(120, 210), (120, 210)]);
        assert_eq!(clipped_capacity.scan_limit, MANAGER_SCAN_BUDGET);

        let oversized = inventory_scan_bounds(&[(99_999, 99_999), (99_999, 99_999)]);
        assert_eq!(oversized.scan_limit, MANAGER_SCAN_BUDGET);

        let unreadable = inventory_scan_bounds(&[(0, 0), (0, 0)]);
        assert_eq!(unreadable.scan_limit, 0);
        assert!(!unreadable.count_confirmed);
        assert!(!unreadable.capacity_confirmed);

        let confirmed_empty = inventory_scan_bounds(&[(0, 2100), (0, 2100)]);
        assert_eq!(confirmed_empty.scan_limit, 0);
        assert!(confirmed_empty.count_confirmed);
        assert!(!confirmed_empty.capacity_confirmed);
    }

    #[test]
    fn filtered_header_count_is_not_a_visual_end_floor() {
        assert_eq!(visual_end_floor(2497, true), 0);
        assert_eq!(visual_end_floor(2497, false), 2497);
    }

    #[test]
    fn rarity_boundary_requires_a_confirmed_level_zero() {
        assert!(should_stop_at_rarity_boundary(true, 0));
        assert!(!should_stop_at_rarity_boundary(true, -1));
        assert!(!should_stop_at_rarity_boundary(true, 1));
        assert!(!should_stop_at_rarity_boundary(false, 0));

        assert!(repeated_rarity_below_min(Some(3), Some(3), 4));
        assert!(!repeated_rarity_below_min(Some(3), Some(4), 4));
        assert!(!repeated_rarity_below_min(Some(3), None, 4));
    }

    #[test]
    fn only_complete_and_reliable_scan_evidence_proves_target_absence() {
        // Reaching an OCR-derived traversal limit alone is never authoritative.
        assert!(!scan_proves_absence(
            ScanTermination::Exhausted,
            false,
            0,
            0,
            0,
            10_000,
            200,
            false,
        ));
        assert!(scan_proves_absence(
            ScanTermination::CallbackStop,
            true,
            0,
            0,
            0,
            0,
            200,
            false,
        ));
        assert!(scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            0,
            0,
            0,
            200,
            200,
            false,
        ));
        // Even after a focused retry, an immovable page is not independent
        // proof that unresolved targets do not exist.
        assert!(!scan_proves_absence(
            ScanTermination::UnchangedPage,
            false,
            0,
            0,
            0,
            200,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::UnchangedPage,
            false,
            0,
            0,
            0,
            200,
            200,
            true,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            0,
            0,
            0,
            199,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::UnchangedPage,
            false,
            0,
            0,
            0,
            199,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            1,
            0,
            0,
            200,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            0,
            1,
            0,
            200,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            0,
            0,
            1,
            200,
            200,
            false,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::EmptyCell,
            false,
            0,
            0,
            0,
            200,
            200,
            true,
        ));
        assert!(!scan_proves_absence(
            ScanTermination::CaptureFailure,
            false,
            1,
            0,
            0,
            200,
            200,
            false,
        ));
    }
}
