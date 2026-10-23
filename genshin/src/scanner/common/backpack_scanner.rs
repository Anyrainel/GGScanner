use anyhow::{anyhow, Result};
use image::RgbImage;
use regex::Regex;
use yas::{log_debug, log_error, log_info, log_warn};

use yas::ocr::ImageToText;
use yas::utils;

use super::annotator;
use super::capture_frame::CaptureFrame;
use super::constants::*;
use super::coord_scaler::CoordScaler;
use super::debug_dump::{next_filter_dump_index, DumpCollector};
use super::game_controller::{panel_frames_similar, GenshinGameController};
#[cfg(test)]
use super::grid_icon_detector::ITEMS_PER_PAGE;
use super::grid_voter::GridVoteSchedule;
use super::pixel_utils;

fn pixel_rgb(image: &RgbImage, scaler: &CoordScaler, pos: (f64, f64)) -> [u8; 3] {
    let x = scaler.x(pos.0) as u32;
    let y = scaler.y(pos.1) as u32;
    if x < image.width() && y < image.height() {
        let p = image.get_pixel(x, y);
        [p[0], p[1], p[2]]
    } else {
        [0, 0, 0]
    }
}

fn dump_order_by_recent_filter(image: &RgbImage, scaler: &CoordScaler, active: bool, action: &str) {
    let mut collector =
        DumpCollector::new("debug_images", "filter", next_filter_dump_index(), scaler);
    let img_idx = collector.add_image("order_by_recent", image);
    collector.record_pixel(
        img_idx,
        "order_by_recent",
        ARTIFACT_FIVE_STAR_FILTER_POS,
        pixel_rgb(image, scaler, ARTIFACT_FIVE_STAR_FILTER_POS),
        if active { "active" } else { "inactive" },
    );
    collector.add_warning(&format!("action: {}", action));
    let result = serde_json::json!({
        "kind": "order_by_recent_filter",
        "active": active,
        "action": action,
    })
    .to_string();
    collector.finalize_success(&result);
}

fn cell_sample_bounds(
    image: &RgbImage,
    scaler: &CoordScaler,
    row: usize,
    col: usize,
) -> (u32, u32, u32, u32) {
    cell_sample_bounds_with_y_offset(image, scaler, row, col, 0.0)
}

fn cell_sample_bounds_with_y_offset(
    image: &RgbImage,
    scaler: &CoordScaler,
    row: usize,
    col: usize,
    y_offset: f64,
) -> (u32, u32, u32, u32) {
    let cx = GRID_FIRST_X + col as f64 * GRID_OFFSET_X;
    let cy = GRID_FIRST_Y + row as f64 * GRID_OFFSET_Y + y_offset;
    // Exclude outer 3px on each edge to avoid selection highlight bleed.
    let bx = cx - GRID_OFFSET_X * 0.5 + 3.0;
    let by = cy - GRID_OFFSET_Y * 0.5 + 3.0;
    let bw = GRID_OFFSET_X - 6.0;
    let bh = GRID_OFFSET_Y - 6.0;

    let x0 = scaler.x(bx) as u32;
    let y0 = scaler.y(by) as u32;
    let x1 = (scaler.x(bx + bw) as u32).min(image.width());
    let y1 = (scaler.y(by + bh) as u32).min(image.height());

    (x0, y0, x1, y1)
}

fn visit_cell_samples(
    image: &RgbImage,
    scaler: &CoordScaler,
    row: usize,
    col: usize,
    mut visit: impl FnMut(u8),
) {
    let (x0, y0, x1, y1) = cell_sample_bounds(image, scaler, row, col);

    let mut y = y0;
    while y < y1 {
        let mut x = x0;
        while x < x1 {
            for channel in image.get_pixel(x, y).0 {
                visit(channel);
            }
            x += 4;
        }
        y += 4;
    }
}

/// Sample a grid cell's interior, excluding the selection-highlight border.
fn cell_samples(image: &RgbImage, scaler: &CoordScaler, row: usize, col: usize) -> Vec<u8> {
    let mut samples = Vec::new();
    visit_cell_samples(image, scaler, row, col, |channel| samples.push(channel));
    samples
}

/// Sample every visible grid position for page-change detection.
///
/// Empty positions are intentionally included: an unchanged final page has the
/// same occupied and empty layout, while a real scroll changes many positional
/// samples. Comparing the complete grid avoids the old two-cell false positives
/// on inventories containing repeated icons.
#[cfg(test)]
fn visible_grid_samples(image: &RgbImage, scaler: &CoordScaler) -> Vec<u8> {
    visible_grid_cell_samples(image, scaler)
        .into_iter()
        .flatten()
        .collect()
}

fn visible_grid_cell_samples(image: &RgbImage, scaler: &CoordScaler) -> Vec<Vec<u8>> {
    annotator::record_region(
        "visible_grid_samples",
        image,
        (0.0, 100.0, 1310.0, 980.0),
        scaler,
    );
    let mut samples = Vec::with_capacity(GRID_ROWS * GRID_COLS);
    for row in 0..GRID_ROWS {
        for col in 0..GRID_COLS {
            samples.push(cell_samples(image, scaler, row, col));
        }
    }
    samples
}

/// Sample the vertical gutter that contains the backpack scrollbar.
///
/// Genshin's inventory layout shifts the detail panel slightly between UI
/// scales, so this deliberately uses a wide band instead of a single pixel.
/// The selected detail card remains unchanged while the grid scrolls; the
/// moving scrollbar thumb therefore provides independent movement evidence
/// even when two filtered pages contain the same artifact icons.
fn scrollbar_band_samples(image: &RgbImage, scaler: &CoordScaler) -> Vec<Vec<u8>> {
    // Local 1920x1080 and 3840x2160 captures both place the track at base
    // x≈1288. This narrow gutter excludes the rightmost grid cell (ending near
    // x=1265) and the detail panel (starting near x=1310). Cover the complete
    // vertical travel: the thumb can be near y=1000 late in a large inventory.
    const BAND_RECT: (f64, f64, f64, f64) = (1278.0, 80.0, 22.0, 950.0);
    annotator::record_region("scrollbar_band", image, BAND_RECT, scaler);
    let (x, y, width, height) = BAND_RECT;
    let x0 = scaler.x(x).max(0) as u32;
    let y0 = scaler.y(y).max(0) as u32;
    let x1 = (scaler.x(x + width).max(0) as u32).min(image.width());
    let y1 = (scaler.y(y + height).max(0) as u32).min(image.height());
    let mut columns = Vec::new();
    let mut sample_x = x0;
    while sample_x < x1 {
        let mut column = Vec::new();
        let mut sample_y = y0;
        while sample_y < y1 {
            column.extend_from_slice(&image.get_pixel(sample_x, sample_y).0);
            sample_y += 4;
        }
        columns.push(column);
        sample_x += 4;
    }
    columns
}

#[derive(Debug, Clone)]
struct ScrollVisualState {
    grid: Vec<u8>,
    grid_cells: Vec<Vec<u8>>,
    scrollbar_band: Vec<Vec<u8>>,
}

fn scroll_visual_state(image: &RgbImage, scaler: &CoordScaler) -> ScrollVisualState {
    let grid_cells = visible_grid_cell_samples(image, scaler);
    ScrollVisualState {
        grid: grid_cells.iter().flatten().copied().collect(),
        grid_cells,
        scrollbar_band: scrollbar_band_samples(image, scaler),
    }
}

fn nested_samples_compatible(first: &[Vec<u8>], second: &[Vec<u8>]) -> bool {
    first.len() == second.len()
        && !first.is_empty()
        && first
            .iter()
            .zip(second.iter())
            .all(|(left, right)| left.len() == right.len() && !left.is_empty())
}

fn scroll_state_shapes_compatible(first: &ScrollVisualState, second: &ScrollVisualState) -> bool {
    first.grid.len() == second.grid.len()
        && !first.grid.is_empty()
        && nested_samples_compatible(&first.grid_cells, &second.grid_cells)
        && first.grid_cells.len() == GRID_ROWS * GRID_COLS
        && nested_samples_compatible(&first.scrollbar_band, &second.scrollbar_band)
}

fn scrollbar_band_moved(first: &[Vec<u8>], second: &[Vec<u8>]) -> Option<bool> {
    if first.len() != second.len() || first.is_empty() {
        return None;
    }

    // A scrollbar thumb is narrow but changes a substantial vertical slice.
    // Requiring two adjacent sampled columns rejects isolated capture noise
    // without diluting the thumb movement across the much wider search band.
    const MIN_THUMB_COLUMNS: usize = 2;
    const MAX_THUMB_COLUMNS: usize = 8;
    let mut consecutive_changed_columns = 0;
    let mut changed_runs = Vec::new();
    for (left, right) in first.iter().zip(second.iter()) {
        if left.len() != right.len() || left.is_empty() {
            return None;
        }
        if visual_samples_similar(left, right) {
            if consecutive_changed_columns > 0 {
                changed_runs.push(consecutive_changed_columns);
            }
            consecutive_changed_columns = 0;
        } else {
            consecutive_changed_columns += 1;
        }
    }
    if consecutive_changed_columns > 0 {
        changed_runs.push(consecutive_changed_columns);
    }

    Some(
        changed_runs
            .into_iter()
            .any(|width| (MIN_THUMB_COLUMNS..=MAX_THUMB_COLUMNS).contains(&width)),
    )
}

fn scroll_states_similar(first: &ScrollVisualState, second: &ScrollVisualState) -> Option<bool> {
    if !scroll_state_shapes_compatible(first, second) {
        return None;
    }
    Some(
        visual_samples_similar(&first.grid, &second.grid)
            && !scrollbar_band_moved(&first.scrollbar_band, &second.scrollbar_band)?,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrollAdvance {
    NoMovement,
    AdvancedRows(usize),
}

fn classify_scroll_advance(
    first: &ScrollVisualState,
    second: &ScrollVisualState,
    requested_rows: usize,
) -> Option<ScrollAdvance> {
    if !scroll_state_shapes_compatible(first, second)
        || requested_rows == 0
        || requested_rows > GRID_ROWS
    {
        return None;
    }

    let grid_moved = !visual_samples_similar(&first.grid, &second.grid);
    let scrollbar_moved = scrollbar_band_moved(&first.scrollbar_band, &second.scrollbar_band)?;
    // Scrolling distance belongs to the calibrated control path, not to image
    // recognition. Detection answers only whether the page moved. This avoids
    // mistaking coincidentally aligned or pixel-identical artifact rows for a
    // short turn and then skipping part of the next page.
    Some(if grid_moved || scrollbar_moved {
        ScrollAdvance::AdvancedRows(requested_rows)
    } else {
        ScrollAdvance::NoMovement
    })
}

fn visual_samples_similar(first: &[u8], second: &[u8]) -> bool {
    const NOISE_FLOOR: u8 = 6;
    const CHANGED_BYTE_DIFF: u8 = 16;
    const MAX_MEAN_EXCESS_DIFF: f64 = 0.5;
    const MAX_CHANGED_BYTE_RATIO: f64 = 0.02;

    if first.len() != second.len() || first.is_empty() {
        return false;
    }

    let mut excess_diff_sum: u64 = 0;
    let mut changed_bytes: usize = 0;
    for (&left, &right) in first.iter().zip(second.iter()) {
        let diff = left.abs_diff(right);
        excess_diff_sum += diff.saturating_sub(NOISE_FLOOR) as u64;
        if diff > CHANGED_BYTE_DIFF {
            changed_bytes += 1;
        }
    }

    let len = first.len() as f64;
    excess_diff_sum as f64 / len <= MAX_MEAN_EXCESS_DIFF
        && changed_bytes as f64 / len <= MAX_CHANGED_BYTE_RATIO
}

fn remember_occupied_samples(known: &mut Vec<Vec<u8>>, candidate: &[u8]) {
    const MAX_KNOWN_OCCUPIED_CELLS: usize = 16;

    if known
        .iter()
        .any(|samples| visual_samples_similar(samples, candidate))
    {
        return;
    }
    if known.len() == MAX_KNOWN_OCCUPIED_CELLS {
        known.remove(0);
    }
    known.push(candidate.to_vec());
}

fn seeded_first_cell_is_preselected(
    page_start_idx: usize,
    page_item_idx: usize,
    probe_changed_selection: bool,
) -> bool {
    page_start_idx == 0 && page_item_idx == 0 && !probe_changed_selection
}

fn visual_end_is_authoritative(scanned_count: usize, minimum_count: usize) -> bool {
    scanned_count >= minimum_count
}

fn parse_item_count_text(text: &str) -> Result<(i32, i32)> {
    let re = Regex::new(r"(\d+)\s*/\s*(\d+)")?;
    let caps = re.captures(text).ok_or_else(|| {
        anyhow!(
            "无法识别背包物品数量 '{}' / Could not parse backpack item count '{}'",
            text.trim(),
            text.trim()
        )
    })?;
    let current: i32 = caps[1].parse().map_err(|e| {
        anyhow!(
            "背包当前数量无效 '{}' / Invalid current backpack count '{}': {}",
            &caps[1],
            &caps[1],
            e
        )
    })?;
    // The game draws "current/capacity", but capacity is not a scan input.
    // Inventories can temporarily exceed the cap; only `current` is used.
    let capacity: i32 = caps[2].parse().unwrap_or(0);
    Ok((current, capacity))
}

/// Sample every expected grid cell for the current page.
fn sample_grid_cells(
    image: &RgbImage,
    scaler: &CoordScaler,
    start_row: usize,
    visible_rows: usize,
    total_row: usize,
    scanned_row: usize,
    last_row_col: usize,
) -> Vec<Vec<u8>> {
    annotator::record_region(
        "grid_cell_samples",
        image,
        (0.0, 100.0, 1310.0, 980.0),
        scaler,
    );
    let mut samples = Vec::new();
    for r in start_row..start_row + visible_rows {
        let cum_row = scanned_row + (r - start_row);
        let cols = if cum_row == total_row - 1 {
            last_row_col
        } else {
            GRID_COLS
        };
        for c in 0..cols {
            samples.push(cell_samples(image, scaler, r, c));
        }
    }
    samples
}

/// If the "5-star sort by acquired time" filter is active on the artifact tab,
/// click it to dismiss so that all rarities are visible.
///
/// Must be called after the artifact tab is selected.
pub fn dismiss_five_star_filter(
    ctrl: &mut GenshinGameController,
    tab_delay: u64,
    dump_images: bool,
) {
    let image = match ctrl.capture_game() {
        Ok(img) => img,
        Err(e) => {
            log_warn!(
                "[backpack] 截图失败，跳过筛选检测: {}",
                "[backpack] capture failed, skipping filter check: {}",
                e
            );
            return;
        },
    };
    let active = pixel_utils::is_five_star_filter_active(&image, &ctrl.scaler);
    if dump_images {
        dump_order_by_recent_filter(
            &image,
            &ctrl.scaler,
            active,
            if active {
                "dismiss"
            } else {
                "already_inactive"
            },
        );
    }
    if active {
        ctrl.click_at(
            ARTIFACT_FIVE_STAR_FILTER_POS.0,
            ARTIFACT_FIVE_STAR_FILTER_POS.1,
        );
        utils::sleep(tab_delay as u32);
    }
}

/// Ensure the "5-star sort by acquired time" filter is active on the artifact tab.
/// If it's not active, click it to enable so that only recent 5-star artifacts are visible.
///
/// Must be called after the artifact tab is selected.
pub fn ensure_five_star_filter_active(
    ctrl: &mut GenshinGameController,
    tab_delay: u64,
    dump_images: bool,
) {
    let image = match ctrl.capture_game() {
        Ok(img) => img,
        Err(e) => {
            log_warn!(
                "[backpack] 截图失败，跳过筛选检测: {}",
                "[backpack] capture failed, skipping filter check: {}",
                e
            );
            return;
        },
    };
    let active = pixel_utils::is_five_star_filter_active(&image, &ctrl.scaler);
    if dump_images {
        dump_order_by_recent_filter(
            &image,
            &ctrl.scaler,
            active,
            if active { "already_active" } else { "enable" },
        );
    }
    if !active {
        log_debug!(
            "[backpack] 五星排序筛选未开启，将点击开启",
            "[backpack] 5-star sort filter not active, will click to enable"
        );
        ctrl.click_at(
            ARTIFACT_FIVE_STAR_FILTER_POS.0,
            ARTIFACT_FIVE_STAR_FILTER_POS.1,
        );
        utils::sleep(tab_delay as u32);
    }
}

/// Open the backpack to a specific tab with the same proven sequence as the
/// artifact/weapon scanners.
///
/// 1. Focus game window
/// 2. Return to main world (Escape × 8)
/// 3. Press B to open backpack
/// 4. Click the requested tab
/// 5. Read item count; if 0, retry from step 2
///
/// Returns `(current_count, max_capacity)` on success.
///
/// This is a free function (not a method) so callers can use `ctrl` freely
/// after it returns without keeping a `BackpackScanner` borrow alive.
pub fn open_backpack_to_tab(
    ctrl: &mut GenshinGameController,
    tab: &str,
    open_delay: u64,
    tab_delay: u64,
    count_ocr: &dyn ImageToText<RgbImage>,
    keep_five_star_filter: bool,
    dump_images: bool,
) -> Result<(i32, i32)> {
    ctrl.focus_game_window();
    if ctrl.check_rmb() {
        anyhow::bail!("cancelled");
    }
    ctrl.return_to_main_ui(8);
    if ctrl.check_rmb() {
        anyhow::bail!("cancelled");
    }

    {
        let mut bp = BackpackScanner::new(ctrl);
        bp.open_backpack(open_delay);
    }

    let first_reading = select_tab_and_read_count(
        ctrl,
        tab,
        tab_delay,
        count_ocr,
        keep_five_star_filter,
        dump_images,
    );

    match first_reading {
        Ok((count, max)) if count > 0 => return Ok((count, max)),
        Ok(_) => log_info!(
            "[backpack] 标签'{}'数量=0，重新打开背包...",
            "[backpack] count=0 on tab '{}', reopening backpack...",
            tab
        ),
        Err(ref e) => log_warn!(
            "[backpack] 首次物品数量读取失败，重新打开背包后重试: {}",
            "[backpack] first item-count read failed; reopening the backpack before retrying: {}",
            yas::lang::localize(&e.to_string())
        ),
    }

    if ctrl.check_rmb() {
        anyhow::bail!("cancelled");
    }
    ctrl.return_to_main_ui(4);
    if ctrl.check_rmb() {
        anyhow::bail!("cancelled");
    }
    {
        let mut bp = BackpackScanner::new(ctrl);
        bp.open_backpack(open_delay);
    }
    select_tab_and_read_count(
        ctrl,
        tab,
        tab_delay,
        count_ocr,
        keep_five_star_filter,
        dump_images,
    )
}

fn apply_artifact_star_filter(
    ctrl: &mut GenshinGameController,
    tab: &str,
    tab_delay: u64,
    keep_five_star_filter: bool,
    dump_images: bool,
) {
    if tab != "artifact" {
        return;
    }
    if keep_five_star_filter {
        ensure_five_star_filter_active(ctrl, tab_delay, dump_images);
    } else {
        dismiss_five_star_filter(ctrl, tab_delay, dump_images);
    }
}

/// Select an inventory tab, apply the artifact 5-star filter if needed, then
/// OCR the header count. Shared by the full open path and the skip-open path.
pub fn select_tab_and_read_count(
    ctrl: &mut GenshinGameController,
    tab: &str,
    tab_delay: u64,
    count_ocr: &dyn ImageToText<RgbImage>,
    keep_five_star_filter: bool,
    dump_images: bool,
) -> Result<(i32, i32)> {
    {
        let mut bp = BackpackScanner::new(ctrl);
        bp.select_tab(tab, tab_delay);
    }
    apply_artifact_star_filter(ctrl, tab, tab_delay, keep_five_star_filter, dump_images);
    if ctrl.check_rmb() {
        anyhow::bail!("cancelled");
    }
    let bp = BackpackScanner::new(ctrl);
    bp.read_item_count(count_ocr)
}

/// What the scan callback should do after processing an event.
pub enum ScanAction {
    /// Continue scanning.
    Continue,
    /// Stop scanning immediately.
    Stop,
    /// Skip the rest of the current page. Only meaningful as a response to
    /// `PageStarted` — ignored (treated as `Continue`) if returned from other
    /// events.
    SkipPage,
}

/// Why grid traversal ended.
///
/// Callers must not treat heuristic endings as proof that every requested item
/// was absent. In particular, filtered scans use an unfiltered item count, so
/// `EmptyCell` and `UnchangedPage` are expected ways to find the filtered end,
/// but they are not authoritative enough to label unresolved work `NotFound`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanTermination {
    /// Every item implied by the supplied count was visited or skipped.
    Exhausted,
    /// The callback intentionally requested an early stop.
    CallbackStop,
    /// User cancellation interrupted traversal.
    Cancelled,
    /// The detail panel did not change after clicking a presumed grid item.
    EmptyCell,
    /// Re-focused, repeated scrolling left both the complete visible grid and
    /// scrollbar gutter unchanged. This is an end-of-list hint, not proof of
    /// absence for unresolved work.
    UnchangedPage,
    /// Required capture evidence was unavailable before traversal completed.
    CaptureFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanGridOutcome {
    pub termination: ScanTermination,
    pub scanned_count: usize,
    /// Grid positions that could not be fully confirmed.
    pub missed_count: usize,
    /// Grid positions intentionally skipped by the callback's page shortcut.
    pub skipped_count: usize,
}

/// Events delivered to the scan callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridPageLayout {
    pub page_start_idx: usize,
    pub page_items: usize,
    pub screen_start_row: usize,
}

impl GridPageLayout {
    fn cell_position(&self, index: usize) -> Option<(usize, usize)> {
        let offset = index.checked_sub(self.page_start_idx)?;
        if offset >= self.page_items {
            return None;
        }
        Some((
            self.screen_start_row + offset / GRID_COLS,
            offset % GRID_COLS,
        ))
    }
}

/// Only reselect a failed item if its original grid page is still visible.
pub fn focus_failed_item(
    ctrl: &mut GenshinGameController,
    layout: Option<GridPageLayout>,
    index: usize,
    delay_ms: u64,
) {
    if ctrl.check_rmb() {
        return;
    }
    if let Some((row, col)) = layout.and_then(|page| page.cell_position(index)) {
        ctrl.click_at(
            GRID_FIRST_X + col as f64 * GRID_OFFSET_X,
            GRID_FIRST_Y + row as f64 * GRID_OFFSET_Y,
        );
        utils::sleep(delay_ms as u32);
        log_info!(
            "[OCR] 已选中识别失败的物品（索引{}），请截图反馈",
            "[OCR] Selected the failed item (index {}); please attach a screenshot to your report",
            index
        );
    } else {
        log_warn!(
            "[OCR] 识别失败的物品（索引{}）已不在当前页，无法回选",
            "[OCR] Failed item (index {}) is no longer on the current page; cannot reselect it",
            index
        );
    }
}

pub enum GridEvent {
    /// Fired at the start of each page if `probe_last_cell_per_page` is set.
    /// `scan_grid` has already clicked the bottom-right visible cell, waited
    /// for the panel, and captured the image. The callback may return
    /// `SkipPage` to skip the entire page without OCRing individual items.
    PageStarted {
        /// Cumulative item index at the top-left of the page.
        page_start_idx: usize,
        /// The captured image of the bottom-right visible cell.
        last_cell_image: RgbImage,
    },
    /// An item was clicked and captured. `row` and `col` are screen-relative
    /// grid positions (for re-clicking the same cell later).
    Item {
        idx: usize,
        row: usize,
        col: usize,
        /// Authoritative logical-to-screen mapping for this visible page.
        layout: GridPageLayout,
        frame: CaptureFrame,
    },
    /// Fired after all items on a page have been processed, before scrolling.
    /// Only fires if at least one item was emitted on the page. Useful for
    /// draining per-page OCR results / applying side effects while the page
    /// is still in view.
    PageCompleted { page_start_idx: usize },
    /// A page scroll just completed (useful for clearing per-page state).
    PageScrolled,
}

/// Panel wait strategy.
pub enum PanelWaitMode {
    /// Fixed delay then stability check (for weapons — identical items have identical panels).
    FixedDelay { delay_ms: u64 },
    /// Fingerprint-based detection: wait until panel content differs from previous AND is stable.
    Fingerprint {
        timeout_ms: u64,
        initial_wait_ms: u64,
    },
}

/// Configuration for backpack grid scanning.
pub struct BackpackScanConfig {
    pub delay_scroll: u64,
    /// How to wait for the detail panel to load after clicking a grid item.
    pub panel_wait: PanelWaitMode,
    /// Extra delay (ms) after panel is ready, before capture.
    pub extra_delay: u64,
    /// When set, most grid items capture only this rect. Items at the
    /// page-relative indices produced by `grid_vote_schedule` still capture
    /// the full window for grid icon voting.
    pub detail_panel_rect: Option<(f64, f64, f64, f64)>,
    /// Produces the full-window vote positions for the current page size.
    pub grid_vote_schedule: fn(usize) -> GridVoteSchedule,
    /// If true, `scan_grid` clicks the bottom-right visible cell at the start
    /// of each page, captures its image, and emits `GridEvent::PageStarted`
    /// so the caller can decide whether to skip the page.
    pub probe_last_cell_per_page: bool,
    /// Enable duplicate grid cell detection. When true, at the start of each
    /// page the grid cells are sampled. If a cell matches the previous
    /// cell, the panel wait is skipped entirely (for FixedDelay mode) or uses
    /// a shorter timeout (for Fingerprint mode).
    pub detect_grid_duplicates: bool,
    /// When true, detect empty grid cells by checking if the detail panel
    /// failed to change after clicking (wait_until_panel_loaded returns
    /// false). On empty cell: skip the probe (don't SkipPage) and stop
    /// item scanning. Also enables stuck-page detection after scroll.
    /// Used by LockManager when filter_involved_sets is active, because
    /// the game's item count display shows total capacity (not filtered
    /// count) after filtering, making `total` unreliable.
    pub detect_empty_cells: bool,
    /// A visual empty/page-end result before this many positions have been
    /// visited is recovery evidence, not a valid end. Unfiltered manager scans
    /// use the observed header count; filtered and ordinary scans use zero.
    pub min_items_before_visual_end: usize,
}

/// Panel pool rect — substats + set name region whose pixel sum changes
/// when a different item is selected. Covers both artifact and weapon panels.
/// Panel fingerprint region — substats text area.
/// Top at y=478 avoids the lock icon area (y=428) whose fade-in animation
/// would prevent fingerprint stabilization.
const PANEL_POOL_RECT: (f64, f64, f64, f64) = (1330.0, 478.0, 370.0, 187.0);
/// Wider, settled detail-panel region used only to confirm whether a timed-out
/// cell is occupied. It includes the item name, main stat, substats, set, and
/// state icons, making two different occupied cards much less likely to look
/// identical than the fast substat-only pool.
const OCCUPANCY_PANEL_RECT: (f64, f64, f64, f64) = (1310.0, 110.0, 480.0, 860.0);

// NOTE: The full right-panel detail area (covering all OCR + pixel-check
// regions for both artifacts and weapons, with 10px margin) is approximately:
//   (1310, 110, 480, 860)  — right edge at 1790, bottom at 970.
// Artifact scans use this as a partial capture; grid voting still needs full
// window at the schedule positions for the current page size.

/// Fast timeout for duplicate items in Fingerprint mode (e.g., identical weapons).
const PANEL_LOAD_FAST_TIMEOUT_MS: u64 = 100;

fn capture_item_frame(
    ctrl: &GenshinGameController,
    config: &BackpackScanConfig,
    page_rel: usize,
    page_items: usize,
) -> Result<CaptureFrame> {
    let need_full = match config.detail_panel_rect {
        None => true,
        Some(_) => (config.grid_vote_schedule)(page_items)
            .indices
            .contains(&page_rel),
    };
    if need_full {
        Ok(CaptureFrame::full(ctrl.capture_game()?))
    } else {
        CaptureFrame::from_region(ctrl, config.detail_panel_rect.unwrap())
    }
}

/// Delay between scroll ticks (milliseconds).
const SCROLL_TICK_DELAY_MS: u32 = 10;

/// Minimum wait after all scroll ticks are sent, for animation to settle.
const MIN_SCROLL_SETTLE_MS: u64 = 200;
/// Additional stable-frame interval used before accepting scroll evidence.
const SCROLL_STABLE_INTERVAL_MS: u32 = 80;
/// A first unchanged page turn is never terminal; reposition the pointer and
/// send the same atomic wheel input once more before deciding the list ended.
const SCROLL_RETRY_COUNT: usize = 1;

/// Upper bound used when the backpack header count cannot describe the visible
/// list (for example, after a set or recent-five-star filter is applied).
pub const FILTERED_BACKPACK_SCAN_BUDGET: usize = 10_000;

fn scroll_ticks_for_rows(row_count: usize, completed_pages: u32) -> i32 {
    let ticks_per_row = SCROLL_TICKS_PER_PAGE as f64 / GRID_ROWS as f64;
    let mut ticks = (ticks_per_row * row_count as f64).round() as i32;
    if row_count == GRID_ROWS
        && SCROLL_CORRECTION_INTERVAL > 0
        && (completed_pages + 1) % SCROLL_CORRECTION_INTERVAL as u32 == 0
    {
        ticks -= 1;
    }
    ticks.max(1)
}

fn completed_page_turn_increment(row_count: usize) -> u32 {
    u32::from(row_count == GRID_ROWS)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifiedScrollOutcome {
    AdvancedRows(usize),
    EndOfList,
    Cancelled,
}

/// Reusable backpack grid scanner.
///
/// Uses pre-calibrated scroll constants (SCROLL_TICKS_PER_PAGE) for reliable
/// page-level scrolling. Each page scroll sends exactly SCROLL_TICKS_PER_PAGE
/// ticks, with a correction tick subtracted every SCROLL_CORRECTION_INTERVAL
/// pages to prevent drift.
pub struct BackpackScanner<'a> {
    ctrl: &'a mut GenshinGameController,
    /// Number of pages scrolled so far (for correction tracking).
    pages_scrolled: u32,
}

/// Occupancy evidence must not navigate away from the item being captured.
/// Known occupied icons also cover distinct artifacts with identical panels.
fn matches_occupied_cell(candidate_samples: Option<&[u8]>, occupied_samples: &[Vec<u8>]) -> bool {
    candidate_samples.is_some_and(|candidate| {
        occupied_samples
            .iter()
            .any(|occupied| visual_samples_similar(candidate, occupied))
    })
}

fn timed_out_cell_occupied(
    candidate_samples: Option<&[u8]>,
    occupied_samples: &[Vec<u8>],
    previous_panel: Option<&[u8]>,
    current_panel: &[u8],
) -> Option<bool> {
    if matches_occupied_cell(candidate_samples, occupied_samples) {
        return Some(true);
    }
    previous_panel
        .filter(|previous| !previous.is_empty() && previous.len() == current_panel.len())
        .map(|previous| !panel_frames_similar(previous, current_panel))
}

impl<'a> BackpackScanner<'a> {
    pub fn new(ctrl: &'a mut GenshinGameController) -> Self {
        Self {
            ctrl,
            pages_scrolled: 0,
        }
    }

    /// Access the controller's scaler (useful for cloning before scan_grid).
    pub fn scaler(&self) -> &super::coord_scaler::CoordScaler {
        &self.ctrl.scaler
    }

    /// Open the backpack by pressing B.
    /// Assumes the game is on the main overworld UI.
    pub fn open_backpack(&mut self, delay: u64) {
        self.ctrl.key_press(enigo::Key::Layout('b'));
        utils::sleep(delay as u32);
    }

    /// Select a backpack tab by clicking its position.
    pub fn select_tab(&mut self, tab: &str, delay: u64) {
        let (bx, by) = match tab {
            "weapon" => TAB_WEAPON,
            "artifact" => TAB_ARTIFACT,
            _ => {
                log_error!("[backpack] 未知标签: {}", "[backpack] unknown tab: {}", tab);
                return;
            },
        };
        self.ctrl.click_at(bx, by);
        utils::sleep(delay as u32);
    }

    /// Read the item count from the backpack header ("X/Y" format).
    pub fn read_item_count(&self, ocr_model: &dyn ImageToText<RgbImage>) -> Result<(i32, i32)> {
        let text = self.ctrl.ocr_region(ocr_model, ITEM_COUNT_RECT)?;
        log_debug!(
            "[backpack] 物品数量OCR原文: '{}'",
            "[backpack] item count OCR raw text: '{}'",
            text.trim()
        );
        let parsed = parse_item_count_text(&text);
        if annotator::is_enabled() {
            if let Ok(full) = self.ctrl.capture_game() {
                let mut collector = DumpCollector::new(
                    "debug_images",
                    "item_count",
                    next_filter_dump_index(),
                    &self.ctrl.scaler,
                );
                let img_idx = collector.add_image("header", &full);
                collector.record_ocr(img_idx, "count", ITEM_COUNT_RECT, text.trim());
                let display = match &parsed {
                    Ok((cur, cap)) => format!("{cur}/{cap}"),
                    Err(e) => e.to_string(),
                };
                collector.set_final_result("count", &display);
                collector.finalize_success(&display);
            }
        }
        parsed
    }

    /// Scroll down by a given number of rows using calibrated tick counts.
    ///
    /// Uses SCROLL_TICKS_PER_PAGE (49 ticks for 5 rows) as the base ratio.
    /// Applies correction every SCROLL_CORRECTION_INTERVAL pages.
    fn prepare_scroll_target(&mut self) {
        let center_x = GRID_FIRST_X + 3.0 * GRID_OFFSET_X;
        let center_y = GRID_FIRST_Y + 2.0 * GRID_OFFSET_Y;
        // Preserve the original calibrated control path: hover the grid, but
        // do not click or split the wheel input into separately verified parts.
        self.ctrl.move_to(center_x, center_y);
        utils::sleep(30);
    }

    fn scroll_rows(&mut self, row_count: usize, settle_ms: u64) -> bool {
        if row_count == 0 {
            return true;
        }

        self.prepare_scroll_target();
        let ticks = scroll_ticks_for_rows(row_count, self.pages_scrolled);
        self.send_scroll_ticks(ticks, settle_ms)
    }

    fn send_scroll_ticks(&mut self, ticks: i32, settle_ms: u64) -> bool {
        // Send scroll ticks with small delays to avoid overwhelming the game.
        for i in 0..ticks {
            if self.ctrl.check_rmb() {
                return false;
            }
            self.ctrl.mouse_scroll(1);
            // Small delay between ticks
            if (i + 1) % 5 == 0 {
                utils::sleep(SCROLL_TICK_DELAY_MS);
            }
        }

        // Honor the configured scroll delay, while retaining the historical
        // 200 ms minimum that was calibrated for the inventory animation.
        utils::sleep(settle_ms.max(MIN_SCROLL_SETTLE_MS).min(u32::MAX as u64) as u32);
        true
    }

    fn record_completed_scroll(&mut self, row_count: usize) {
        self.pages_scrolled = self
            .pages_scrolled
            .saturating_add(completed_page_turn_increment(row_count));
    }

    /// Send the original calibrated page turn as one atomic wheel sequence,
    /// then verify only whether movement occurred after the animation settles.
    /// The calibrated input remains authoritative for distance.
    fn scroll_rows_verified(
        &mut self,
        requested_rows: usize,
        settle_ms: u64,
    ) -> Result<VerifiedScrollOutcome> {
        if requested_rows == 0 || requested_rows > GRID_ROWS {
            return Err(anyhow!(
                "无效的已验证滚动行数: {} / Invalid verified scroll row count: {}",
                requested_rows,
                requested_rows
            ));
        }

        for attempt in 0..=SCROLL_RETRY_COUNT {
            self.prepare_scroll_target();
            let before = self.capture_stable_scroll_state()?;
            let ticks = scroll_ticks_for_rows(requested_rows, self.pages_scrolled);
            if !self.send_scroll_ticks(ticks, settle_ms) {
                return Ok(VerifiedScrollOutcome::Cancelled);
            }
            let after = self.capture_stable_scroll_state()?;
            match classify_scroll_advance(&before, &after, requested_rows) {
                Some(ScrollAdvance::AdvancedRows(advanced_rows)) => {
                    self.record_completed_scroll(advanced_rows);
                    return Ok(VerifiedScrollOutcome::AdvancedRows(advanced_rows));
                },
                Some(ScrollAdvance::NoMovement) if attempt < SCROLL_RETRY_COUNT => {
                    log_warn!(
                        "[backpack] 整页滚动未移动；重新定位鼠标后重试",
                        "[backpack] The atomic page turn did not move; repositioning the pointer and retrying"
                    );
                },
                Some(ScrollAdvance::NoMovement) => {
                    return Ok(VerifiedScrollOutcome::EndOfList);
                },
                None => {
                    return Err(anyhow!(
                        "翻页前后的截图尺寸不一致 / Pre- and post-scroll captures had incompatible dimensions"
                    ));
                },
            }
        }

        unreachable!("verified scroll retry loop always returns")
    }

    /// Re-check a cell whose fast substat panel did not change.
    ///
    /// A visually matching occupied card is independent evidence that the candidate
    /// is occupied. Otherwise compare the settled wide panel with the screenshot
    /// saved before this click. Never select an earlier item while capture is
    /// continuing: that changes the panel underneath the OCR pipeline.
    fn confirm_timed_out_cell_occupied(
        &mut self,
        candidate_samples: Option<&[u8]>,
        occupied_samples: &[Vec<u8>],
        previous_panel: Option<&[u8]>,
        timeout_ms: u64,
    ) -> Result<Option<bool>> {
        if matches_occupied_cell(candidate_samples, occupied_samples) {
            return Ok(Some(true));
        }

        let occupancy_result = (|| -> Result<_> {
            self.ctrl
                .ensure_panel_stable(OCCUPANCY_PANEL_RECT, timeout_ms.max(100))?;
            let (x, y, w, h) = OCCUPANCY_PANEL_RECT;
            let current = self.ctrl.capture_region(x, y, w, h)?;
            Ok(timed_out_cell_occupied(
                candidate_samples,
                occupied_samples,
                previous_panel,
                current.as_raw(),
            ))
        })();
        // The controller has one fingerprint buffer; restore its normal rect
        // even when the wide-panel capture fails.
        let baseline_result = self
            .ctrl
            .ensure_panel_stable(PANEL_POOL_RECT, timeout_ms.max(100));
        let occupied = occupancy_result?;
        baseline_result?;
        Ok(occupied)
    }

    fn capture_game_with_retry(&self) -> Result<RgbImage> {
        match self.ctrl.capture_game() {
            Ok(image) => Ok(image),
            Err(first_error) => {
                log_warn!(
                    "[backpack] 网格截图失败，将重试: {}",
                    "[backpack] grid capture failed, retrying: {}",
                    first_error
                );
                utils::sleep(50);
                self.ctrl.capture_game()
            },
        }
    }

    fn capture_scroll_state_with_retry(&self) -> Result<ScrollVisualState> {
        let image = self.capture_game_with_retry()?;
        Ok(scroll_visual_state(&image, &self.ctrl.scaler))
    }

    fn capture_stable_scroll_state(&self) -> Result<ScrollVisualState> {
        let mut previous = self.capture_scroll_state_with_retry()?;
        for _ in 0..3 {
            utils::sleep(SCROLL_STABLE_INTERVAL_MS);
            let current = self.capture_scroll_state_with_retry()?;
            match scroll_states_similar(&previous, &current) {
                Some(true) => return Ok(current),
                Some(false) => previous = current,
                None => {
                    return Err(anyhow!(
                        "翻页确认截图尺寸不一致 / Scroll-confirmation captures had incompatible dimensions"
                    ));
                },
            }
        }
        Err(anyhow!(
            "翻页后的背包画面持续变化，无法确认滚动位置 / Backpack view kept changing after the page turn; scroll position could not be confirmed"
        ))
    }

    /// Main grid traversal with panel-load detection.
    ///
    /// For each item: clicks the grid position, waits for panel to load
    /// (pixel pool detection), captures the game screen, and delivers a
    /// `GridEvent::Item` to the callback.
    ///
    /// After each page scroll, delivers `GridEvent::PageScrolled`.
    ///
    /// The callback returns `ScanAction::Continue` or `ScanAction::Stop`.
    pub fn scan_grid<F>(
        &mut self,
        total: usize,
        config: &BackpackScanConfig,
        start_at: usize,
        mut callback: F,
    ) -> ScanGridOutcome
    where
        F: FnMut(&mut GenshinGameController, GridEvent) -> ScanAction,
    {
        let total_row = (total + GRID_COLS - 1) / GRID_COLS;
        let last_row_col = if total % GRID_COLS == 0 {
            GRID_COLS
        } else {
            total % GRID_COLS
        };

        log_debug!(
            "[backpack] 总计={}个物品，{}行，最后一行有{}个",
            "[backpack] total={} items, {} rows, last row has {} items",
            total,
            total_row,
            last_row_col
        );

        // Click the first grid position to ensure focus
        self.ctrl.click_at(GRID_FIRST_X, GRID_FIRST_Y);

        // When detecting empty cells, seed a fresh baseline from the first
        // item's stable panel. Never reuse a snapshot from an earlier scan or
        // a different rectangle.
        if config.detect_empty_cells {
            let (settle_ms, timeout_ms) = match &config.panel_wait {
                PanelWaitMode::FixedDelay { delay_ms } => (*delay_ms, 100),
                PanelWaitMode::Fingerprint {
                    timeout_ms,
                    initial_wait_ms,
                } => (*initial_wait_ms, *timeout_ms),
            };
            if settle_ms > 0 {
                utils::sleep(settle_ms as u32);
            }
            if let Err(first_error) = self
                .ctrl
                .ensure_panel_stable(PANEL_POOL_RECT, timeout_ms.max(100))
            {
                log_warn!(
                    "[backpack] 初始面板基线截图失败，将重试: {}",
                    "[backpack] initial panel-baseline capture failed, retrying: {}",
                    first_error
                );
                utils::sleep(50);
                if let Err(e) = self
                    .ctrl
                    .ensure_panel_stable(PANEL_POOL_RECT, timeout_ms.max(100))
                {
                    log_error!(
                        "[backpack] 无法建立初始面板基线: {}",
                        "[backpack] could not establish the initial panel baseline: {}",
                        e
                    );
                    return ScanGridOutcome {
                        termination: ScanTermination::CaptureFailure,
                        scanned_count: 0,
                        missed_count: 1,
                        skipped_count: 0,
                    };
                }
            }
        }

        let row = GRID_ROWS.min(total_row);
        let mut scanned_row: usize = 0;
        let mut scanned_count: usize = 0;
        let mut start_row: usize = 0;
        let mut termination = ScanTermination::Exhausted;
        let mut missed_count: usize = 0;
        let mut skipped_count: usize = 0;
        let mut occupied_cell_samples: Vec<Vec<u8>> = Vec::new();
        // Per-page grid cell samples for duplicate and occupancy detection.
        // Index within this vec = position on visible page.
        let mut page_cell_samples: Vec<Vec<u8>> = Vec::new();

        // Skip pages by scrolling
        if start_at > 0 {
            let skip_rows = start_at / GRID_COLS;
            let full_pages = skip_rows / GRID_ROWS;
            if full_pages > 0 {
                log_debug!(
                    "[backpack] 跳转到第{}个物品(跳过{}行)",
                    "[backpack] jumping to item {} ({} rows to skip)",
                    start_at,
                    skip_rows
                );
                let rows_to_scroll = full_pages * GRID_ROWS;
                if !self.scroll_rows(rows_to_scroll, config.delay_scroll) {
                    return ScanGridOutcome {
                        termination: ScanTermination::Cancelled,
                        scanned_count,
                        missed_count,
                        skipped_count,
                    };
                }
                self.record_completed_scroll(rows_to_scroll);
                scanned_row = rows_to_scroll;
                scanned_count = rows_to_scroll * GRID_COLS;
                utils::sleep(200);
            }
        }

        'outer: while scanned_count < total {
            let page_start_idx = scanned_count;
            let mut page_had_items = false;
            let mut skip_page = false;
            let mut confirmed_probe_cell: Option<(usize, usize)> = None;
            let mut probe_changed_selection = false;

            // --- Optional page probe: click bottom-right new cell, capture, ask callback. ---
            if config.probe_last_cell_per_page {
                // Screen row of the bottom-most new row is always `row - 1`
                // (new rows occupy [start_row..row); bottom is row-1).
                let probe_screen_row = row - 1;
                let cum_row_at_probe = scanned_row + (probe_screen_row - start_row);
                let probe_col = if cum_row_at_probe == total_row - 1 {
                    last_row_col - 1
                } else {
                    GRID_COLS - 1
                };
                let x = GRID_FIRST_X + probe_col as f64 * GRID_OFFSET_X;
                let y = GRID_FIRST_Y + probe_screen_row as f64 * GRID_OFFSET_Y;
                self.ctrl.click_at(x, y);
                let mut probe_panel_loaded = true;
                let mut probe_wait_failed = false;
                match &config.panel_wait {
                    PanelWaitMode::FixedDelay { delay_ms } => {
                        utils::sleep(*delay_ms as u32);
                        if let Err(e) = self.ctrl.ensure_panel_stable(PANEL_POOL_RECT, 100) {
                            log_error!(
                                "[backpack] 探测面板稳定检查失败: {}",
                                "[backpack] probe panel stability check failed: {}",
                                e
                            );
                            probe_wait_failed = true;
                        }
                    },
                    PanelWaitMode::Fingerprint {
                        timeout_ms,
                        initial_wait_ms,
                    } => {
                        match self.ctrl.wait_until_panel_loaded(
                            PANEL_POOL_RECT,
                            *timeout_ms,
                            *initial_wait_ms,
                        ) {
                            Ok(loaded) => probe_panel_loaded = loaded,
                            Err(e) => {
                                log_error!(
                                    "[backpack] 探测面板加载检查失败: {}",
                                    "[backpack] probe panel load check failed: {}",
                                    e
                                );
                                probe_wait_failed = true;
                            },
                        }
                    },
                }
                // Empty cell detection: if the panel didn't change after
                // clicking, the probed cell is likely empty. Skip the probe
                // and scan the page normally.
                if probe_wait_failed {
                    log_warn!(
                        "[backpack] 无法确认页末探测位置，将禁用本页快速跳过并逐项扫描",
                        "[backpack] Could not confirm the page probe; disabling the page-skip shortcut and scanning each item"
                    );
                } else if config.detect_empty_cells && !probe_panel_loaded {
                    log_debug!(
                        "[backpack] 探测点击后面板未变化，可能为空格子，正常扫描此页 (page_start={})",
                        "[backpack] Panel unchanged after probe click, likely empty cell, scanning page normally (page_start={})",
                        page_start_idx
                    );
                } else {
                    probe_changed_selection = true;
                    confirmed_probe_cell = Some((probe_screen_row, probe_col));
                    if config.extra_delay > 0 {
                        utils::sleep(config.extra_delay as u32);
                    }
                    match self.ctrl.capture_game() {
                        Ok(image) => {
                            let action = callback(
                                &mut *self.ctrl,
                                GridEvent::PageStarted {
                                    page_start_idx,
                                    last_cell_image: image,
                                },
                            );
                            match action {
                                ScanAction::Continue => {},
                                ScanAction::SkipPage => skip_page = true,
                                ScanAction::Stop => {
                                    termination = ScanTermination::CallbackStop;
                                    break 'outer;
                                },
                            }
                        },
                        Err(e) => {
                            log_error!(
                                "[backpack] 探测截图失败: {}",
                                "[backpack] probe capture failed: {}",
                                e
                            );
                        },
                    }
                } // else: probe_panel_loaded or !detect_empty_cells
            }

            if skip_page {
                // Advance counters past the skipped page without emitting items.
                let before_skip = scanned_count;
                let new_rows = row - start_row;
                let mut rows_added = 0usize;
                while rows_added < new_rows {
                    let cum_row = scanned_row + rows_added;
                    let row_item_count = if cum_row == total_row - 1 {
                        last_row_col
                    } else {
                        GRID_COLS
                    };
                    scanned_count += row_item_count;
                    rows_added += 1;
                    if scanned_count >= total {
                        break;
                    }
                }
                scanned_row += rows_added;
                skipped_count += scanned_count - before_skip;
            } else {
                let visible_page_capacity = (row - start_row) * GRID_COLS;
                let page_items = (total - page_start_idx).min(visible_page_capacity);
                // Duplicate and occupancy detection: sample grid cells at page start.
                page_cell_samples.clear();
                if config.detect_grid_duplicates {
                    match self.capture_game_with_retry() {
                        Ok(grid_img) => {
                            let visible_rows = row - start_row;
                            page_cell_samples = sample_grid_cells(
                                &grid_img,
                                &self.ctrl.scaler,
                                start_row,
                                visible_rows,
                                total_row,
                                scanned_row,
                                last_row_col,
                            );
                        },
                        Err(e) if config.detect_empty_cells => {
                            log_error!(
                                "[backpack] 无法获取本页网格占用证据: {}",
                                "[backpack] Could not capture occupancy evidence for this page: {}",
                                e
                            );
                            missed_count += 1;
                            termination = ScanTermination::CaptureFailure;
                            break 'outer;
                        },
                        Err(e) => {
                            log_warn!(
                                "[backpack] 无法获取本页重复项证据，将使用常规面板等待: {}",
                                "[backpack] Could not capture duplicate-cell evidence; using normal panel waits: {}",
                                e
                            );
                        },
                    }
                }

                let mut stopped = false;
                let mut page_item_idx: usize = 0; // position within this page
                if let Some((probe_row, probe_col)) = confirmed_probe_cell {
                    let probe_page_idx = (probe_row - start_row) * GRID_COLS + probe_col;
                    if let Some(samples) = page_cell_samples.get(probe_page_idx) {
                        remember_occupied_samples(&mut occupied_cell_samples, samples);
                    }
                }
                // Reset after every page/probe/lock callback, which may change
                // selection. Later baselines come from the actual item captures.
                let mut previous_panel = if config.detect_empty_cells {
                    let (x, y, w, h) = OCCUPANCY_PANEL_RECT;
                    match self.ctrl.capture_region(x, y, w, h) {
                        Ok(panel) => Some(panel.into_raw()),
                        Err(e) => {
                            log_warn!(
                                "[backpack] 无法保存面板参照；不会将未变化的面板视为空格子: {}",
                                "[backpack] Could not save the panel baseline; an unchanged panel will not prove an empty cell: {}",
                                e
                            );
                            None
                        },
                    }
                } else {
                    None
                };
                'page: for cur_row in start_row..row {
                    let row_item_count = if scanned_row == total_row - 1 {
                        last_row_col
                    } else {
                        GRID_COLS
                    };

                    for col in 0..row_item_count {
                        if self.ctrl.check_rmb() {
                            termination = ScanTermination::Cancelled;
                            stopped = true;
                            break 'page;
                        }
                        if scanned_count >= total {
                            stopped = true;
                            break 'page;
                        }

                        // Skip items before start_at
                        if scanned_count < start_at {
                            scanned_count += 1;
                            page_item_idx += 1;
                            continue;
                        }

                        // Click the grid item
                        let x = GRID_FIRST_X + col as f64 * GRID_OFFSET_X;
                        let y = GRID_FIRST_Y + cur_row as f64 * GRID_OFFSET_Y;
                        self.ctrl.click_at(x, y);

                        // Is this cell a known duplicate of the previous?
                        let is_duplicate = config.detect_grid_duplicates
                            && page_item_idx > 0
                            && page_item_idx < page_cell_samples.len()
                            && visual_samples_similar(
                                &page_cell_samples[page_item_idx],
                                &page_cell_samples[page_item_idx - 1],
                            );
                        let candidate_samples = page_cell_samples.get(page_item_idx);

                        // Wait for panel to load based on configured mode
                        let mut panel_loaded = true;
                        let mut panel_wait_failed = false;
                        match &config.panel_wait {
                            PanelWaitMode::FixedDelay { delay_ms } => {
                                if !is_duplicate {
                                    utils::sleep(*delay_ms as u32);
                                    if let Err(e) =
                                        self.ctrl.ensure_panel_stable(PANEL_POOL_RECT, 100)
                                    {
                                        log_error!(
                                            "[backpack] 面板稳定检查失败: {}",
                                            "[backpack] panel stability check failed: {}",
                                            e
                                        );
                                        panel_wait_failed = true;
                                    }
                                }
                            },
                            PanelWaitMode::Fingerprint {
                                timeout_ms,
                                initial_wait_ms,
                            } => {
                                let timeout = if is_duplicate && !config.detect_empty_cells {
                                    PANEL_LOAD_FAST_TIMEOUT_MS
                                } else {
                                    *timeout_ms
                                };
                                match self.ctrl.wait_until_panel_loaded(
                                    PANEL_POOL_RECT,
                                    timeout,
                                    *initial_wait_ms,
                                ) {
                                    Ok(loaded) => panel_loaded = loaded,
                                    Err(e) => {
                                        log_error!(
                                            "[backpack] 面板加载检查失败: {}",
                                            "[backpack] panel load check failed: {}",
                                            e
                                        );
                                        panel_wait_failed = true;
                                    },
                                }
                            },
                        }

                        if panel_wait_failed {
                            missed_count += 1;
                            if config.detect_empty_cells {
                                termination = ScanTermination::CaptureFailure;
                                stopped = true;
                                break 'page;
                            }
                            // Non-filtered artifact/weapon scans historically
                            // proceeded to the item capture after a transient
                            // wait error. Preserve that behavior: the capture
                            // itself remains the authoritative fallible step.
                        }

                        // An unchanged fast panel is only a suspicion. Re-check
                        // it against known occupied icons and the saved wider
                        // detail panel before concluding the candidate is empty.
                        //
                        // Exception: the first cell was selected to seed the
                        // initial panel baseline. Until a successful page probe
                        // changes selection, re-clicking that cell cannot change
                        // the panel and must not be mistaken for an empty slot.
                        let preselected_first_cell = seeded_first_cell_is_preselected(
                            page_start_idx,
                            page_item_idx,
                            probe_changed_selection,
                        );
                        if config.detect_empty_cells
                            && !is_duplicate
                            && !panel_loaded
                            && !preselected_first_cell
                        {
                            let timeout_ms = match &config.panel_wait {
                                PanelWaitMode::Fingerprint { timeout_ms, .. } => *timeout_ms,
                                PanelWaitMode::FixedDelay { delay_ms } => (*delay_ms).max(100),
                            };
                            match self.confirm_timed_out_cell_occupied(
                                candidate_samples.map(Vec::as_slice),
                                &occupied_cell_samples,
                                previous_panel.as_deref(),
                                timeout_ms,
                            ) {
                                Ok(Some(true)) => {
                                    log_debug!(
                                        "[backpack] 快速面板未变化，但占用确认成功，继续扫描 (idx={})",
                                        "[backpack] Fast panel was unchanged, but occupancy was confirmed; continuing (idx={})",
                                        scanned_count
                                    );
                                },
                                Ok(Some(false)) => {
                                    if !visual_end_is_authoritative(
                                        scanned_count,
                                        config.min_items_before_visual_end,
                                    ) {
                                        log_warn!(
                                            "[backpack] 在已读取数量之前出现疑似空格子；跳过该位置并继续扫描 (idx={}, 最低数量={})",
                                            "[backpack] A suspected empty cell appeared before the observed count; skipping that position and continuing (idx={}, minimum={})",
                                            scanned_count,
                                            config.min_items_before_visual_end
                                        );
                                        missed_count += 1;
                                        scanned_count += 1;
                                        page_item_idx += 1;
                                        continue;
                                    }
                                    log_debug!(
                                        "[backpack] 多重确认后检测到空格子，停止扫描 (idx={})",
                                        "[backpack] Empty cell confirmed after independent checks, stopping (idx={})",
                                        scanned_count
                                    );
                                    termination = ScanTermination::EmptyCell;
                                    stopped = true;
                                    break 'page;
                                },
                                Ok(None) => {
                                    log_warn!(
                                        "[backpack] 快速面板未变化且缺少可用的占用参照；跳过此位置但继续扫描 (idx={})",
                                        "[backpack] The fast panel was unchanged and no usable occupancy baseline is available; skipping this position but continuing (idx={})",
                                        scanned_count
                                    );
                                    missed_count += 1;
                                    scanned_count += 1;
                                    page_item_idx += 1;
                                    continue;
                                },
                                Err(e) => {
                                    log_error!(
                                        "[backpack] 空格子确认截图失败: {}",
                                        "[backpack] empty-cell confirmation capture failed: {}",
                                        e
                                    );
                                    missed_count += 1;
                                    termination = ScanTermination::CaptureFailure;
                                    stopped = true;
                                    break 'page;
                                },
                            }
                        }

                        // Extra delay after panel ready
                        if config.extra_delay > 0 {
                            utils::sleep(config.extra_delay as u32);
                        }

                        let frame = match capture_item_frame(
                            self.ctrl,
                            config,
                            page_item_idx,
                            page_items,
                        ) {
                            Ok(f) => f,
                            Err(e) => {
                                log_error!(
                                    "[backpack] 截图失败: {}",
                                    "[backpack] capture failed: {}",
                                    e
                                );
                                // Selection may have changed without a usable
                                // capture; don't compare the next item with a
                                // stale baseline from two positions ago.
                                previous_panel = None;
                                missed_count += 1;
                                scanned_count += 1;
                                page_item_idx += 1;
                                continue;
                            },
                        };

                        if config.detect_empty_cells {
                            previous_panel = frame
                                .crop(OCCUPANCY_PANEL_RECT, 0.0, &self.ctrl.scaler)
                                .map(RgbImage::into_raw);
                            if let Some(samples) = candidate_samples {
                                remember_occupied_samples(&mut occupied_cell_samples, samples);
                            }
                        }

                        page_had_items = true;
                        let action = callback(
                            &mut *self.ctrl,
                            GridEvent::Item {
                                idx: scanned_count,
                                row: cur_row,
                                col,
                                layout: GridPageLayout {
                                    page_start_idx,
                                    page_items,
                                    screen_start_row: start_row,
                                },
                                frame,
                            },
                        );
                        match action {
                            ScanAction::Continue => {},
                            ScanAction::Stop => {
                                scanned_count += 1;
                                termination = ScanTermination::CallbackStop;
                                stopped = true;
                                break 'page;
                            },
                            ScanAction::SkipPage => {
                                // SkipPage is only valid from PageStarted; ignored here.
                            },
                        }

                        scanned_count += 1;
                        page_item_idx += 1;
                    }

                    scanned_row += 1;
                }

                // Emit PageCompleted (if any items were processed) before scrolling
                // or exiting. Gives the caller a chance to drain per-page work
                // while the page is still in view.
                if page_had_items {
                    let action =
                        callback(&mut *self.ctrl, GridEvent::PageCompleted { page_start_idx });
                    if matches!(action, ScanAction::Stop) {
                        if !stopped {
                            termination = ScanTermination::CallbackStop;
                        }
                        break 'outer;
                    }
                }

                if stopped {
                    break 'outer;
                }
            }

            // Calculate how many rows remain and scroll
            let remain = total - scanned_count;
            if remain == 0 {
                termination = ScanTermination::Exhausted;
                break;
            }
            let remain_row = (remain + GRID_COLS - 1) / GRID_COLS;
            let scroll_row = remain_row.min(GRID_ROWS);
            let mut page_advanced = false;

            if config.detect_empty_cells {
                match self.scroll_rows_verified(scroll_row, config.delay_scroll) {
                    Ok(VerifiedScrollOutcome::AdvancedRows(advanced_rows)) => {
                        // Image recognition confirms movement only. The restored
                        // atomic control path owns the calibrated row distance,
                        // even when adjacent filtered pages look identical.
                        start_row = GRID_ROWS - advanced_rows;
                        page_advanced = true;
                    },
                    Ok(VerifiedScrollOutcome::EndOfList) => {},
                    Ok(VerifiedScrollOutcome::Cancelled) => {
                        termination = ScanTermination::Cancelled;
                        break 'outer;
                    },
                    Err(e) => {
                        log_error!(
                            "[backpack] 无法确认完整翻页距离: {}",
                            "[backpack] Could not confirm the requested scroll distance: {}",
                            yas::lang::localize(&e.to_string())
                        );
                        missed_count += 1;
                        termination = ScanTermination::CaptureFailure;
                        break 'outer;
                    },
                }
            } else {
                start_row = GRID_ROWS - scroll_row;
                if !self.scroll_rows(scroll_row, config.delay_scroll) {
                    termination = ScanTermination::Cancelled;
                    break 'outer;
                }
                self.record_completed_scroll(scroll_row);
                page_advanced = true;
            }

            if !page_advanced {
                if visual_end_is_authoritative(scanned_count, config.min_items_before_visual_end) {
                    log_info!(
                        "[backpack] 重新定位鼠标并重试后网格与滚动条仍未变化，已到达可见列表末尾",
                        "[backpack] Grid and scrollbar remained unchanged after a pointer-position retry; reached the end of the visible list"
                    );
                    termination = ScanTermination::UnchangedPage;
                } else {
                    log_error!(
                        "[backpack] 在已读取数量之前，重新定位鼠标并重试后仍无法翻页；停止本次扫描以避免遗漏",
                        "[backpack] The page still did not advance after a pointer-position retry before the observed count; stopping this scan to avoid omissions"
                    );
                    missed_count += 1;
                    termination = ScanTermination::CaptureFailure;
                }
                break 'outer;
            }

            // Reset fingerprint after scroll — new page means panel content changed.
            // Skip reset when detecting empty cells: keeping the previous panel
            // snapshot as baseline allows detecting empty cells on the new page.
            if !config.detect_empty_cells {
                self.ctrl.reset_panel_fingerprint();
            }

            let action = callback(&mut *self.ctrl, GridEvent::PageScrolled);
            if matches!(action, ScanAction::Stop) {
                termination = ScanTermination::CallbackStop;
                break 'outer;
            }
        }

        ScanGridOutcome {
            termination,
            scanned_count,
            missed_count,
            skipped_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use image::Rgb;

    use super::*;

    #[test]
    fn occupancy_uses_saved_wide_panel_without_reselecting_earlier_items() {
        let previous = vec![40; 3_000];
        let mut different_item = previous.clone();
        // Name/main-stat changes can lie outside the fast substat fingerprint.
        different_item[..300].fill(180);
        assert_eq!(
            timed_out_cell_occupied(None, &[], Some(&previous), &different_item),
            Some(true)
        );
        assert_eq!(
            timed_out_cell_occupied(None, &[], Some(&previous), &previous),
            Some(false)
        );
        assert_eq!(timed_out_cell_occupied(None, &[], None, &previous), None);
        assert_eq!(
            timed_out_cell_occupied(None, &[], Some(&previous[..30]), &previous),
            None
        );
    }

    #[test]
    fn identical_panels_remain_occupied_when_the_grid_icon_is_known() {
        let panel = vec![40; 3_000];
        let icon = vec![100; 300];
        assert_eq!(
            timed_out_cell_occupied(Some(&icon), &[icon.clone()], Some(&panel), &panel),
            Some(true)
        );
    }

    #[test]
    fn failed_item_position_respects_partial_tail_pages_and_page_boundaries() {
        let page = GridPageLayout {
            page_start_idx: 56,
            page_items: 11,
            screen_start_row: 3,
        };
        assert_eq!(page.cell_position(55), None);
        assert_eq!(page.cell_position(56), Some((3, 0)));
        assert_eq!(page.cell_position(64), Some((4, 0)));
        assert_eq!(page.cell_position(66), Some((4, 2)));
        assert_eq!(page.cell_position(67), None);
    }

    fn fill_grid_cell(
        image: &mut RgbImage,
        scaler: &CoordScaler,
        row: usize,
        col: usize,
        color: Rgb<u8>,
    ) {
        let (x0, y0, x1, y1) = cell_sample_bounds(image, scaler, row, col);
        for y in y0..y1 {
            for x in x0..x1 {
                image.put_pixel(x, y, color);
            }
        }
    }

    fn paint_distinct_grid(
        image: &mut RgbImage,
        scaler: &CoordScaler,
        mut cell_id: impl FnMut(usize, usize) -> u8,
    ) {
        for row in 0..GRID_ROWS {
            for col in 0..GRID_COLS {
                let id = cell_id(row, col);
                fill_grid_cell(
                    image,
                    scaler,
                    row,
                    col,
                    Rgb([id, id.wrapping_mul(3), id.wrapping_mul(7)]),
                );
            }
        }
    }

    #[test]
    fn cell_sample_comparison_tolerates_noise_but_detects_layout_changes() {
        let scaler = CoordScaler::new(1920, 1080);
        let mut first = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
        let mut second = first.clone();
        let noisy = RgbImage::from_pixel(1920, 1080, Rgb([106, 106, 106]));
        let (x0, y0, x1, y1) = cell_sample_bounds(&first, &scaler, 0, 0);
        let midpoint = x0 + (x1 - x0) / 2;
        for y in y0..y1 {
            for x in x0..x1 {
                if x < midpoint {
                    first.put_pixel(x, y, Rgb([180, 20, 20]));
                    second.put_pixel(x, y, Rgb([20, 20, 180]));
                } else {
                    first.put_pixel(x, y, Rgb([20, 20, 180]));
                    second.put_pixel(x, y, Rgb([180, 20, 20]));
                }
            }
        }

        assert!(visual_samples_similar(
            &cell_samples(
                &RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100])),
                &scaler,
                0,
                0,
            ),
            &cell_samples(&noisy, &scaler, 0, 0)
        ));
        assert!(!visual_samples_similar(
            &cell_samples(&first, &scaler, 0, 0),
            &cell_samples(&second, &scaler, 0, 0)
        ));
    }

    #[test]
    fn grid_sample_comparison_tolerates_noise_but_detects_layout_changes() {
        let scaler = CoordScaler::new(1920, 1080);
        let first = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
        let noisy = RgbImage::from_pixel(1920, 1080, Rgb([106, 106, 106]));

        let first_samples = visible_grid_samples(&first, &scaler);
        assert!(visual_samples_similar(
            &first_samples,
            &visible_grid_samples(&first.clone(), &scaler)
        ));
        assert!(visual_samples_similar(
            &first_samples,
            &visible_grid_samples(&noisy, &scaler)
        ));

        // Swap two full cell interiors. Aggregate color is unchanged, but the
        // page layout is materially different.
        let mut layout_a = first.clone();
        let mut layout_b = first;
        let (x0, y0, x1, y1) = cell_sample_bounds(&layout_a, &scaler, 0, 0);
        for y in y0..y1 {
            for x in x0..x1 {
                layout_a.put_pixel(x, y, Rgb([180, 20, 20]));
                layout_b.put_pixel(x, y, Rgb([20, 20, 180]));
            }
        }
        let (x0, y0, x1, y1) = cell_sample_bounds(&layout_a, &scaler, 0, 1);
        for y in y0..y1 {
            for x in x0..x1 {
                layout_a.put_pixel(x, y, Rgb([20, 20, 180]));
                layout_b.put_pixel(x, y, Rgb([180, 20, 20]));
            }
        }
        assert!(!visual_samples_similar(
            &visible_grid_samples(&layout_a, &scaler),
            &visible_grid_samples(&layout_b, &scaler)
        ));
    }

    #[test]
    fn small_scrollbar_movements_distinguish_repeated_pages_at_common_scales() {
        for (width, height) in [(1920, 1080), (3840, 2160)] {
            let scaler = CoordScaler::new(width, height);
            let thumb_frame = |thumb_y: f64| {
                let mut frame = RgbImage::from_pixel(width, height, Rgb([100, 100, 100]));
                let x0 = scaler.x(1284.0).max(0) as u32;
                let x1 = scaler.x(1296.0).max(0) as u32;
                let y0 = scaler.y(thumb_y).max(0) as u32;
                let y1 = scaler.y(thumb_y + 36.0).max(0) as u32;
                for y in y0..y1 {
                    for x in x0..x1 {
                        frame.put_pixel(x, y, Rgb([220, 220, 220]));
                    }
                }
                frame
            };

            for base_y in [100.0, 500.0, 970.0] {
                for shift in [4.0, 8.0, 16.0] {
                    let first = scroll_visual_state(&thumb_frame(base_y), &scaler);
                    let moved = scroll_visual_state(&thumb_frame(base_y + shift), &scaler);
                    assert!(visual_samples_similar(&first.grid, &moved.grid));
                    assert_eq!(
                        scrollbar_band_moved(&first.scrollbar_band, &moved.scrollbar_band),
                        Some(true),
                        "{width}x{height}, y={base_y}, shift={shift}"
                    );
                    assert_eq!(
                        classify_scroll_advance(&first, &moved, GRID_ROWS),
                        Some(ScrollAdvance::AdvancedRows(GRID_ROWS))
                    );
                    assert_eq!(
                        classify_scroll_advance(&first, &moved, 1),
                        Some(ScrollAdvance::AdvancedRows(1))
                    );
                    assert_eq!(scroll_states_similar(&first, &moved), Some(false));
                }
            }

            let unchanged = scroll_visual_state(&thumb_frame(100.0), &scaler);
            assert_eq!(
                classify_scroll_advance(&unchanged, &unchanged, GRID_ROWS),
                Some(ScrollAdvance::NoMovement)
            );
        }
    }

    #[test]
    fn broad_detail_panel_change_is_not_a_scrollbar_thumb() {
        let scaler = CoordScaler::new(1920, 1080);
        let first = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
        let mut panel_changed = first.clone();
        for y in 100..180 {
            for x in 1340..1420 {
                panel_changed.put_pixel(x, y, Rgb([220, 220, 220]));
            }
        }

        let first_state = scroll_visual_state(&first, &scaler);
        let changed_state = scroll_visual_state(&panel_changed, &scaler);
        assert_eq!(
            scrollbar_band_moved(&first_state.scrollbar_band, &changed_state.scrollbar_band),
            Some(false)
        );
    }

    #[test]
    fn grid_movement_still_advances_when_scrollbar_pixels_repeat() {
        let scaler = CoordScaler::new(1920, 1080);
        let mut first = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
        let mut grid_moved = first.clone();
        paint_distinct_grid(&mut first, &scaler, |row, col| {
            (row * GRID_COLS + col) as u8
        });
        paint_distinct_grid(&mut grid_moved, &scaler, |row, col| {
            100 + (row * GRID_COLS + col) as u8
        });

        let first_state = scroll_visual_state(&first, &scaler);
        let moved_state = scroll_visual_state(&grid_moved, &scaler);
        assert_eq!(
            scrollbar_band_moved(&first_state.scrollbar_band, &moved_state.scrollbar_band),
            Some(false)
        );
        assert_eq!(
            classify_scroll_advance(&first_state, &moved_state, GRID_ROWS),
            Some(ScrollAdvance::AdvancedRows(GRID_ROWS))
        );
    }

    #[test]
    fn atomic_page_turn_uses_requested_distance_when_rows_happen_to_overlap() {
        let scaler = CoordScaler::new(1920, 1080);
        let mut first = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
        paint_distinct_grid(&mut first, &scaler, |row, col| {
            20 + (row * GRID_COLS + col) as u8
        });

        for shift in 1..GRID_ROWS {
            let mut partial_shift = RgbImage::from_pixel(1920, 1080, Rgb([100, 100, 100]));
            paint_distinct_grid(&mut partial_shift, &scaler, |row, col| {
                if row + shift < GRID_ROWS {
                    20 + ((row + shift) * GRID_COLS + col) as u8
                } else {
                    180 + ((row + shift - GRID_ROWS) * GRID_COLS + col) as u8
                }
            });

            assert_eq!(
                classify_scroll_advance(
                    &scroll_visual_state(&first, &scaler),
                    &scroll_visual_state(&partial_shift, &scaler),
                    GRID_ROWS,
                ),
                Some(ScrollAdvance::AdvancedRows(GRID_ROWS)),
                "shift={shift}"
            );

            if shift == 1 {
                assert_eq!(
                    classify_scroll_advance(
                        &scroll_visual_state(&first, &scaler),
                        &scroll_visual_state(&partial_shift, &scaler),
                        1,
                    ),
                    Some(ScrollAdvance::AdvancedRows(1))
                );
            }
        }
    }

    #[test]
    fn atomic_page_turn_preserves_original_tick_calibration() {
        assert_eq!(scroll_ticks_for_rows(GRID_ROWS, 0), SCROLL_TICKS_PER_PAGE);
        assert_eq!(
            scroll_ticks_for_rows(GRID_ROWS, SCROLL_CORRECTION_INTERVAL as u32 - 1),
            SCROLL_TICKS_PER_PAGE - 1
        );
        assert_eq!(scroll_ticks_for_rows(1, 0), 10);
        assert_eq!(completed_page_turn_increment(GRID_ROWS), 1);
        assert_eq!(completed_page_turn_increment(GRID_ROWS * 2), 0);
    }

    #[test]
    fn incompatible_scroll_samples_are_uncertain() {
        let state = ScrollVisualState {
            grid: vec![1, 2, 3],
            grid_cells: vec![vec![1, 2, 3]; GRID_ROWS * GRID_COLS],
            scrollbar_band: vec![vec![1, 2, 3], vec![4, 5, 6]],
        };
        let mismatched_grid = ScrollVisualState {
            grid: vec![1, 2],
            grid_cells: state.grid_cells.clone(),
            scrollbar_band: state.scrollbar_band.clone(),
        };
        let mismatched_band = ScrollVisualState {
            grid: vec![9, 8, 7],
            grid_cells: state.grid_cells.clone(),
            scrollbar_band: vec![vec![1, 2, 3]],
        };
        let empty_band_column = ScrollVisualState {
            grid: state.grid.clone(),
            grid_cells: state.grid_cells.clone(),
            scrollbar_band: vec![vec![], vec![4, 5, 6]],
        };
        let mismatched_band_column = ScrollVisualState {
            grid: state.grid.clone(),
            grid_cells: state.grid_cells.clone(),
            scrollbar_band: vec![vec![1, 2], vec![4, 5, 6]],
        };

        assert_eq!(
            classify_scroll_advance(&state, &mismatched_grid, GRID_ROWS),
            None
        );
        assert_eq!(scroll_states_similar(&state, &mismatched_grid), None);
        assert_eq!(
            classify_scroll_advance(&state, &mismatched_band, GRID_ROWS),
            None
        );
        assert_eq!(scroll_states_similar(&state, &mismatched_band), None);
        assert_eq!(
            classify_scroll_advance(&state, &empty_band_column, GRID_ROWS),
            None
        );
        assert_eq!(scroll_states_similar(&state, &empty_band_column), None);
        assert_eq!(
            classify_scroll_advance(&state, &mismatched_band_column, GRID_ROWS),
            None
        );
        assert_eq!(scroll_states_similar(&state, &mismatched_band_column), None);
    }

    #[test]
    fn seeded_first_cell_remains_preselected_without_a_successful_probe() {
        assert!(seeded_first_cell_is_preselected(0, 0, false));
        assert!(!seeded_first_cell_is_preselected(0, 0, true));
        assert!(!seeded_first_cell_is_preselected(0, 1, false));
        assert!(!seeded_first_cell_is_preselected(ITEMS_PER_PAGE, 0, false));
    }

    #[test]
    fn item_count_parser_uses_current_and_ignores_capacity() {
        assert_eq!(parse_item_count_text("120 / 2100").unwrap(), (120, 2100));
        assert_eq!(parse_item_count_text("0/2100").unwrap(), (0, 2100));
        assert_eq!(parse_item_count_text("0/0").unwrap(), (0, 0));
        assert_eq!(parse_item_count_text("2101/2100").unwrap(), (2101, 2100));
        assert_eq!(
            parse_item_count_text("圣遗物 2720/2700").unwrap(),
            (2720, 2700)
        );
        assert!(parse_item_count_text("not a count").is_err());
    }

    #[test]
    fn visual_end_before_observed_count_requires_recovery() {
        assert!(!visual_end_is_authoritative(199, 200));
        assert!(visual_end_is_authoritative(200, 200));
        assert!(visual_end_is_authoritative(0, 0));
    }
}
