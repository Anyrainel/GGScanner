use anyhow::{anyhow, Result};
use image::RgbImage;
use regex::Regex;
use yas::{log_debug, log_error, log_info, log_warn};

use yas::ocr::ImageToText;
use yas::utils;

use super::annotator;
use super::artifact_panel::{
    ArtifactCell, ArtifactPanelOptions, ArtifactPanelSignature, ArtifactSelection,
};
use super::capture_frame::CaptureFrame;
use super::constants::*;
use super::coord_scaler::CoordScaler;
use super::debug_dump::{next_filter_dump_index, DumpCollector};
use super::game_controller::GenshinGameController;
use super::grid_icon_detector::visible_artifact_extent;
use super::grid_voter::GridVoteSchedule;
use super::pixel_utils;

fn dump_order_by_recent_filter(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    active: bool,
    action: &str,
) {
    let mut collector =
        DumpCollector::new("debug_images", "filter", next_filter_dump_index(), scaler);
    let img_idx = collector.add_frame("order_by_recent", frame);
    collector.record_pixel(
        img_idx,
        "order_by_recent",
        ARTIFACT_FIVE_STAR_FILTER_POS,
        frame.pixel(
            scaler,
            ARTIFACT_FIVE_STAR_FILTER_POS.0,
            ARTIFACT_FIVE_STAR_FILTER_POS.1,
        ),
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

/// Decide how far the inventory advanced using the detail of each artifact,
/// in column order. A grid icon alone cannot distinguish rolls on two pieces.
fn ordered_row_advance<T>(
    old_page: &[Option<T>],
    first_row: &[T],
    requested: usize,
    same: impl Fn(&T, &T) -> bool,
) -> Option<usize> {
    if old_page.len() != GRID_ROWS * GRID_COLS
        || first_row.len() != GRID_COLS
        || first_row.iter().all(|item| same(item, &first_row[0]))
    {
        return None;
    }
    let mut candidates = old_page
        .chunks_exact(GRID_COLS)
        .take(requested + 1)
        .enumerate()
        .filter(|(_, row)| {
            // A single stale frame (or an unread slot) cannot disprove an
            // entire ordered row and turn a short scroll into a full page.
            row.iter()
                .zip(first_row)
                .filter(|(old, new)| old.as_ref().is_some_and(|old| !same(old, new)))
                .take(2)
                .count()
                < 2
        });
    let Some((shift, matching_row)) = candidates.next() else {
        // A full-page turn has no retained rows. A shorter command must have
        // an overlapping row; a mismatch there is inconclusive.
        return (requested == GRID_ROWS).then_some(requested);
    };
    if candidates.next().is_some()
        || !matching_row
            .iter()
            .zip(first_row)
            .all(|(old, new)| old.as_ref().is_some_and(|old| same(old, new)))
    {
        return None;
    }
    Some(shift)
}

// Skipped pages have no per-piece detail. Sample the artwork away from the
// animated card borders, badges and clipped level band at the bottom.
type CardSample = [u8; 7 * 5 * 3];

fn card_row_samples(
    image: &RgbImage,
    scaler: &CoordScaler,
    row: usize,
    y_offset: i32,
) -> [CardSample; GRID_COLS] {
    std::array::from_fn(|col| {
        let (cx, cy) = super::grid_icon_detector::artifact_cell_center(row, col);
        let mut samples = [0; 7 * 5 * 3];
        let mut pixels = samples.chunks_exact_mut(3);
        for dy in (-52..=-24).step_by(7) {
            for dx in (-36..=36).step_by(12) {
                let mut sums = [0u16; 3];
                for sy in [-1, 1] {
                    let y = scaler.y(cy + f64::from(y_offset + dy + sy)) as u32;
                    for sx in [-1, 1] {
                        let x = scaler.x(cx + f64::from(dx + sx)) as u32;
                        for (sum, value) in sums.iter_mut().zip(image.get_pixel(x, y).0) {
                            *sum += u16::from(value);
                        }
                    }
                }
                pixels
                    .next()
                    .unwrap()
                    .copy_from_slice(&sums.map(|s| (s / 4) as u8));
            }
        }
        samples
    })
}

fn card_difference(left: &CardSample, right: &CardSample) -> u32 {
    left.iter()
        .zip(right)
        .map(|(a, b)| u32::from(a.abs_diff(*b)))
        .sum()
}

fn skipped_grid_advance(
    before: &RgbImage,
    after: &RgbImage,
    scaler: &CoordScaler,
    requested: usize,
) -> Option<usize> {
    if before.dimensions() != after.dimensions()
        || after.width() < scaler.x(1270.0) as u32
        || after.height() < scaler.y(985.0) as u32
    {
        return None;
    }
    let old: Vec<_> = (0..GRID_ROWS)
        .map(|r| card_row_samples(before, scaler, r, 0))
        .collect();
    let mut matches = [false; GRID_ROWS];
    // A clamped final scroll can leave the cards between nominal row centers.
    // All retained cells must agree at the same vertical displacement.
    for offset in (-48..=48).step_by(2) {
        let new: Vec<_> = (0..GRID_ROWS)
            .map(|r| card_row_samples(after, scaler, r, offset))
            .collect();
        for shift in 0..=requested.min(GRID_ROWS - 1) {
            if matches[shift] {
                continue;
            }
            let mut varied = false;
            let aligned = old[shift..]
                .iter()
                .zip(&new)
                .enumerate()
                .all(|(r, (left, right))| {
                    let bytes = left[0].len() as u32;
                    let mut sum = 0;
                    for (a, b) in left.iter().zip(right) {
                        let difference = card_difference(a, b);
                        // Hover can lighten one card, but cannot change the rest
                        // of its row. Require every column and a tight row average.
                        if difference > 26 * bytes {
                            return false;
                        }
                        sum += difference;
                        if r + 1 < GRID_ROWS && card_difference(&right[0], b) > 28 * bytes {
                            varied = true;
                        }
                    }
                    sum <= 11 * GRID_COLS as u32 * bytes
                });
            matches[shift] = aligned && varied;
        }
    }
    let mut candidates = matches.iter().enumerate().filter(|(_, matched)| **matched);
    let (shift, _) = candidates.next()?;
    candidates.next().is_none().then_some(shift)
}

// The scrollbar track is anchored to the inventory rather than to the cards.
// Store positions in the same 1080p coordinates at all window resolutions.
const SCROLLBAR_RECT: (f64, f64, f64, f64) = (1278.0, 80.0, 22.0, 950.0);
const SCROLLBAR_TRACK_BOTTOM: f64 = 860.0; // relative to SCROLLBAR_RECT

#[derive(Clone, Copy, Debug)]
struct ScrollThumb {
    top: f64,
    bottom: f64,
}

impl ScrollThumb {
    fn center(self) -> f64 {
        (self.top + self.bottom) / 2.0
    }

    fn at_bottom(self) -> bool {
        // A single remaining row in a 2,700-piece inventory moves the thumb
        // only about two base pixels; do not discard it as endpoint noise.
        (self.bottom - SCROLLBAR_TRACK_BOTTOM).abs() <= 1.0
    }
}

fn scroll_thumb(image: &RgbImage) -> Option<ScrollThumb> {
    if image.width() < 9 || image.height() < 10 {
        return None;
    }
    let scale_x = image.width() as f64 / SCROLLBAR_RECT.2;
    let scale_y = image.height() as f64 / SCROLLBAR_RECT.3;
    let brightness = |x: f64, y| {
        let [r, g, b] = image.get_pixel((x * scale_x) as u32, y).0;
        (u16::from(r) + u16::from(g) + u16::from(b)) / 3
    };
    let mut thumb = None;
    let mut run_start = 0;
    for y in 0..=image.height() {
        let bright = y < image.height() && {
            let center = (brightness(9.0, y) + brightness(11.0, y)) / 2;
            let surround = brightness(3.0, y).max(brightness(18.0, y));
            center > surround + 30
        };
        if !bright {
            if (y - run_start) as f64 >= 6.0 * scale_y {
                // Multiple bright shapes could be an overlay. Do not guess
                // which one is the thumb.
                if thumb.is_some() {
                    return None;
                }
                thumb = Some(ScrollThumb {
                    top: run_start as f64 / scale_y,
                    bottom: y as f64 / scale_y,
                });
            }
            run_start = y + 1;
        }
    }
    thumb
}

/// A short final page can be measured even if many artifacts have identical
/// detail. Learn the distance per row from earlier turns in the same inventory.
fn scroll_advance_from_thumb(
    before: ScrollThumb,
    after: ScrollThumb,
    requested: usize,
    pixels_per_row: Option<f64>,
) -> Option<usize> {
    if ((before.bottom - before.top) - (after.bottom - after.top)).abs() > 3.0 {
        return None;
    }
    let delta = after.center() - before.center();
    if delta.abs() < 1.0 {
        return Some(0);
    }
    if delta < 0.0 {
        return None;
    }
    if let Some(unit) = pixels_per_row {
        let rows = (delta / unit).round() as usize;
        if rows > 0
            && rows <= requested
            && (rows as f64 * unit - delta).abs() <= (unit * 0.35).min(2.0)
        {
            return Some(rows);
        }
        return None;
    }
    // A settled atomic wheel turn normally travels its calibrated distance.
    // The bottom stop can truncate it; there we need an actual measurement.
    (after.bottom < SCROLLBAR_TRACK_BOTTOM - 2.0).then_some(requested)
}

fn resolve_scroll_advance(
    ordered_rows: Option<usize>,
    before: Option<ScrollThumb>,
    after: Option<ScrollThumb>,
    requested: usize,
    pixels_per_row: Option<f64>,
) -> Option<VerifiedScrollOutcome> {
    let thumbs = before.zip(after);
    let measured = thumbs.and_then(|(before, after)| {
        scroll_advance_from_thumb(before, after, requested, pixels_per_row)
    });
    let bottom = after.is_some_and(ScrollThumb::at_bottom);
    let rows = match ordered_rows {
        // Identical ordered rows can recur on a list that actually moved.
        Some(0) if thumbs.is_some_and(|(a, b)| (b.center() - a.center()).abs() >= 1.0) => measured?,
        // At a bottom stop, incorrect detail reads could hide a retained row.
        // A calibrated shorter physical turn keeps that row out of the new indices.
        Some(GRID_ROWS) if bottom => measured.unwrap_or(GRID_ROWS),
        Some(rows) => rows,
        None => measured?,
    };
    if rows == 0 && after.is_some() && !bottom {
        // If the thumb is still in the middle, wheel input had no effect.
        // Reporting a successful end would omit the remaining inventory.
        return None;
    }
    Some(if rows == 0 {
        VerifiedScrollOutcome::EndOfList
    } else if bottom || (rows < requested && ordered_rows == Some(rows)) {
        // A shorter-than-requested ordered shift is the final clamped page,
        // even when the scrollbar itself is not visible.
        VerifiedScrollOutcome::FinalRows(rows)
    } else {
        VerifiedScrollOutcome::AdvancedRows(rows)
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

fn visual_end_is_authoritative(scanned_count: usize, minimum_count: usize) -> bool {
    scanned_count >= minimum_count
}

#[derive(Debug, PartialEq, Eq)]
enum TailAction {
    Emit,
    Hold,
    End,
}

/// Empty cells leave the last real detail on screen. Keep a candidate out
/// of OCR until the next cell confirms it. Visible cards protect real twins.
fn artifact_tail_action(
    position: usize,
    same_as_previous: Option<bool>,
    visible_cards: Option<usize>,
    final_page: bool,
    holding_candidate: bool,
    last_slot: bool,
) -> TailAction {
    let past_cards = visible_cards.is_some_and(|extent| position >= extent);
    let blank_first_cell = position == 0 && visible_cards == Some(0);
    let unchanged = same_as_previous == Some(true);
    if !(blank_first_cell || unchanged && (past_cards || final_page && visible_cards.is_none())) {
        return TailAction::Emit;
    }
    if holding_candidate || last_slot && past_cards {
        TailAction::End
    } else if last_slot {
        TailAction::Emit
    } else {
        TailAction::Hold
    }
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

/// The position of the track in the returned crop is (12, 12).
pub(crate) fn read_recent_sort(ctrl: &GenshinGameController) -> Result<(CaptureFrame, bool)> {
    let (x, y) = ARTIFACT_FIVE_STAR_FILTER_POS;
    let frame = CaptureFrame::from_region(ctrl, (x - 12.0, y - 12.0, 45.0, 25.0))?;
    let active =
        pixel_utils::five_star_filter_track_active(&frame.image, &ctrl.scaler, (12.0, 12.0));
    Ok((frame, active))
}

fn set_recent_sort(
    ctrl: &mut GenshinGameController,
    enabled: bool,
    tab_delay: u64,
    dump_images: bool,
) {
    let (frame, active) = match read_recent_sort(ctrl) {
        Ok(state) => state,
        Err(e) => {
            log_warn!(
                "[backpack] 截图失败，跳过筛选检测: {}",
                "[backpack] capture failed, skipping filter check: {}",
                e
            );
            return;
        },
    };
    if dump_images {
        let action = match (active, enabled) {
            (true, true) => "already_active",
            (false, false) => "already_inactive",
            (true, false) => "dismiss",
            (false, true) => "enable",
        };
        dump_order_by_recent_filter(&frame, &ctrl.scaler, active, action);
    }
    if active != enabled {
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
    set_recent_sort(ctrl, keep_five_star_filter, tab_delay, dump_images);
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
    /// Only valid for an eligible leading page's tail.
    SkipPage,
}

/// Gate the physical tail click as well as its OCR. Once scanning starts,
/// subsequent pages can go directly to their first new cell.
fn probe_leading_page(enabled: &mut bool, probe: impl FnOnce() -> ScanAction) -> ScanAction {
    if !*enabled {
        return ScanAction::Continue;
    }
    let action = probe();
    *enabled = matches!(action, ScanAction::SkipPage);
    action
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
    /// Consecutive tail slots kept the last artifact's detail on screen.
    EmptyCell,
    /// Ordered row overlap or the scrollbar identified the end of the list.
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

pub enum GridEvent<'a> {
    PageTail {
        frame: &'a CaptureFrame,
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
        grid_image: Option<RgbImage>,
    },
    /// Fired after all items on a page have been processed, before scrolling.
    /// Only fires if at least one item was emitted on the page. Useful for
    /// draining per-page OCR results / applying side effects while the page
    /// is still in view.
    PageCompleted {
        page_start_idx: usize,
    },
    /// A page scroll just completed (useful for clearing per-page state).
    PageScrolled,
}

/// Panel wait strategy.
pub enum PanelWaitMode {
    /// Fixed delay then stability check (for weapons — identical items have identical panels).
    FixedDelay { delay_ms: u64 },
    /// One click, followed by bounded polling of the right-hand detail panel.
    Artifact {
        timeout_ms: u64,
        initial_wait_ms: u64,
        stop_on_failure: bool,
    },
}

/// Configuration for backpack grid scanning.
pub struct BackpackScanConfig {
    pub delay_scroll: u64,
    /// How to wait for the detail panel to load after clicking a grid item.
    pub panel_wait: PanelWaitMode,
    /// Minimum stable hold for artifacts; post-wait delay for weapons.
    pub extra_delay: u64,
    /// A manager searching for lower-level targets can skip a leading run of pages.
    pub probe_leading_pages: bool,
    /// Infer the visible inventory's end when its item count is unknown.
    pub detect_empty_cells: bool,
    /// A visual empty/page-end result before this many positions have been
    /// visited is recovery evidence, not a valid end. Unfiltered manager scans
    /// use the observed header count; filtered and ordinary scans use zero.
    pub min_items_before_visual_end: usize,
}

/// Substat region used by the weapon stability check.
const PANEL_POOL_RECT: (f64, f64, f64, f64) = (1330.0, 478.0, 370.0, 187.0);

/// Delay between scroll ticks (milliseconds).
const SCROLL_TICK_DELAY_MS: u32 = 10;

/// Let the game retarget the wheel after the pointer is parked on the grid.
const SCROLL_CURSOR_SETTLE_MS: u32 = 50;

/// Minimum wait after all scroll ticks are sent, for animation to settle.
const MIN_SCROLL_SETTLE_MS: u64 = 200;

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

/// Center of the backpack grid. The wheel hit-tests whatever is under the
/// pointer, so a lock click on the detail panel would otherwise scroll nothing.
fn inventory_scroll_cursor() -> (f64, f64) {
    (
        GRID_FIRST_X + (GRID_COLS / 2) as f64 * GRID_OFFSET_X,
        GRID_FIRST_Y + (GRID_ROWS / 2) as f64 * GRID_OFFSET_Y,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifiedScrollOutcome {
    AdvancedRows(usize),
    /// The thumb has reached its bottom stop. Process these rows once.
    FinalRows(usize),
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
    scrollbar_pixels_per_row: Option<f64>,
}

impl<'a> BackpackScanner<'a> {
    pub fn new(ctrl: &'a mut GenshinGameController) -> Self {
        Self {
            ctrl,
            pages_scrolled: 0,
            scrollbar_pixels_per_row: None,
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
        let header = CaptureFrame::from_region(self.ctrl, ITEM_COUNT_RECT)?;
        let text = ocr_model.image_to_text(&header.image, false)?;
        log_debug!(
            "[backpack] 物品数量OCR原文: '{}'",
            "[backpack] item count OCR raw text: '{}'",
            text.trim()
        );
        let parsed = parse_item_count_text(&text);
        if annotator::is_enabled() {
            let mut collector = DumpCollector::new(
                "debug_images",
                "item_count",
                next_filter_dump_index(),
                &self.ctrl.scaler,
            );
            let img_idx = collector.add_frame("header", &header);
            collector.record_ocr(img_idx, "count", ITEM_COUNT_RECT, text.trim());
            let display = match &parsed {
                Ok((cur, cap)) => format!("{cur}/{cap}"),
                Err(e) => e.to_string(),
            };
            collector.set_final_result("count", &display);
            collector.finalize_success(&display);
        }
        parsed
    }

    /// Scroll down by a given number of rows using calibrated tick counts.
    ///
    /// Uses SCROLL_TICKS_PER_PAGE (49 ticks for 5 rows) as the base ratio.
    /// Applies correction every SCROLL_CORRECTION_INTERVAL pages.
    fn scroll_rows(&mut self, row_count: usize, settle_ms: u64) -> bool {
        if row_count == 0 {
            return true;
        }

        let ticks = scroll_ticks_for_rows(row_count, self.pages_scrolled);
        self.send_scroll_ticks(ticks, settle_ms)
    }

    fn send_scroll_ticks(&mut self, ticks: i32, settle_ms: u64) -> bool {
        // Always park on the grid first. The pointer is often still on the
        // detail panel after a lock click, and the wheel follows it.
        let (x, y) = inventory_scroll_cursor();
        self.ctrl.move_to(x, y);
        utils::sleep(SCROLL_CURSOR_SETTLE_MS);
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

    /// Match the newly visible first row against the previous page in order.
    /// Return its captures for normal scanning when the entire page is new.
    fn scroll_rows_verified(
        &mut self,
        requested_rows: usize,
        settle_ms: u64,
        old_page: &[Option<ArtifactPanelSignature>],
        options: ArtifactPanelOptions,
        skipped_page_grid: Option<&RgbImage>,
    ) -> Result<(VerifiedScrollOutcome, Vec<CaptureFrame>)> {
        anyhow::ensure!(
            (1..=GRID_ROWS).contains(&requested_rows) && old_page.len() == GRID_ROWS * GRID_COLS,
            "无效的已验证滚动行数或页面 / Invalid verified scroll row count or page"
        );
        let has_prior_row = old_page
            .chunks_exact(GRID_COLS)
            .any(|row| row.iter().all(Option::is_some));

        if self.ctrl.check_rmb() {
            return Ok((VerifiedScrollOutcome::Cancelled, vec![]));
        }
        let before = self.read_scroll_thumb();
        if before.is_some_and(ScrollThumb::at_bottom) {
            return Ok((VerifiedScrollOutcome::EndOfList, vec![]));
        }
        let ticks = scroll_ticks_for_rows(requested_rows, self.pages_scrolled);
        if !self.send_scroll_ticks(ticks, settle_ms) {
            return Ok((VerifiedScrollOutcome::Cancelled, vec![]));
        }
        self.ctrl.reset_artifact_panel();
        let mut first_row = Vec::new();
        let mut frames = Vec::new();
        if has_prior_row {
            for col in 0..GRID_COLS {
                match self
                    .ctrl
                    .select_artifact(ArtifactCell { row: 0, col }, options)?
                {
                    ArtifactSelection::Ready(frame) => {
                        first_row.push(
                            self.ctrl
                                .last_artifact_signature()
                                .expect("ready artifact must have a signature")
                                .clone(),
                        );
                        frames.push(frame);
                    },
                    ArtifactSelection::Cancelled => {
                        return Ok((VerifiedScrollOutcome::Cancelled, vec![]));
                    },
                }
            }
        }
        let mut row_advance = ordered_row_advance(
            old_page,
            &first_row,
            requested_rows,
            ArtifactPanelSignature::similar,
        );
        let after = self.read_scroll_thumb();
        let thumbs = before.zip(after);
        if !has_prior_row {
            if let Some(old_grid) = skipped_page_grid {
                match self.ctrl.capture_artifact_grid() {
                    Ok(new_grid) => {
                        row_advance = skipped_grid_advance(
                            old_grid,
                            &new_grid,
                            &self.ctrl.scaler,
                            requested_rows,
                        );
                        // With a known scrollbar scale, a conflicting visual
                        // match can be a coincidentally repeated set of icons.
                        if let (Some(rows), Some((a, b)), Some(unit)) =
                            (row_advance, thumbs, self.scrollbar_pixels_per_row)
                        {
                            let measured =
                                scroll_advance_from_thumb(a, b, requested_rows, Some(unit));
                            if measured.is_some_and(|n| n != rows) {
                                row_advance = None;
                            }
                        }
                    },
                    Err(error) => log_warn!(
                        "[backpack] 翻页后网格不可用，将用滚动条确认: {}",
                        "[backpack] Grid unavailable after scrolling; checking scrollbar: {}",
                        yas::lang::localize(&error.to_string())
                    ),
                }
            }
        }
        let outcome = resolve_scroll_advance(row_advance, before, after, requested_rows, self.scrollbar_pixels_per_row)
            .ok_or_else(|| anyhow!(
                "无法从已扫描圣遗物或滚动条确认翻页距离 / Could not confirm the scroll distance from scanned artifacts or the scrollbar"
            ))?;
        if let VerifiedScrollOutcome::AdvancedRows(rows) | VerifiedScrollOutcome::FinalRows(rows) =
            outcome
        {
            if rows == GRID_ROWS {
                if let Some((before, after)) = thumbs {
                    let unit = (after.center() - before.center()) / rows as f64;
                    if unit >= 1.0 && !after.at_bottom() {
                        self.scrollbar_pixels_per_row = Some(unit);
                    }
                }
            } else {
                frames.clear(); // this first row overlaps already scanned positions
            }
            self.record_completed_scroll(rows);
        } else {
            frames.clear();
        }
        Ok((outcome, frames))
    }

    fn read_scroll_thumb(&self) -> Option<ScrollThumb> {
        let (x, y, w, h) = SCROLLBAR_RECT;
        match self.ctrl.capture_region(x, y, w, h) {
            Ok(image) => {
                annotator::record_input("scrollbar_band", &image, "scrollbar track", false);
                scroll_thumb(&image)
            },
            Err(error) => {
                log_warn!(
                    "[backpack] 无法读取滚动条: {}",
                    "[backpack] Could not read the scrollbar: {}",
                    yas::lang::localize(&error.to_string())
                );
                None
            },
        }
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

    /// Main grid traversal with panel-load detection.
    ///
    /// Deliver each item's captured detail once, in inventory order.
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
        F: FnMut(&mut GenshinGameController, GridEvent<'_>) -> ScanAction,
    {
        self.ctrl.reset_artifact_panel();
        let artifact_options = match config.panel_wait {
            PanelWaitMode::Artifact {
                timeout_ms,
                initial_wait_ms,
                ..
            } => Some(ArtifactPanelOptions {
                timeout_ms,
                initial_wait_ms,
                extra_delay_ms: config.extra_delay,
            }),
            PanelWaitMode::FixedDelay { .. } => None,
        };
        let mut scanned_count = 0;
        let mut missed_count = 0;
        let mut start_row = 0;
        let mut termination = ScanTermination::Exhausted;
        let mut page_signatures = vec![None; GRID_ROWS * GRID_COLS];
        let mut prefetched = Vec::new().into_iter();
        let mut final_page = false;
        let mut probe_leading_pages = config.probe_leading_pages && artifact_options.is_some();

        let skip_rows = start_at.min(total) / (GRID_ROWS * GRID_COLS) * GRID_ROWS;
        if skip_rows > 0 {
            if !self.scroll_rows(skip_rows, config.delay_scroll) {
                return ScanGridOutcome {
                    termination: ScanTermination::Cancelled,
                    scanned_count,
                    missed_count,
                };
            }
            self.record_completed_scroll(skip_rows);
            scanned_count = skip_rows * GRID_COLS;
            utils::sleep(200);
        }

        while scanned_count < total {
            let page_start_idx = scanned_count;
            let page_items = (total - scanned_count).min((GRID_ROWS - start_row) * GRID_COLS);
            let layout = GridPageLayout {
                page_start_idx,
                page_items,
                screen_start_row: start_row,
            };
            let votes = GridVoteSchedule::for_page(page_items);
            let mut page_had_items = false;
            let mut stopped = false;
            let mut visible_cards = None;
            let mut pending_tail = None;
            let mut first_grid = None;
            let mut probed_tail = None;
            let page_action = probe_leading_page(&mut probe_leading_pages, || {
                let options = artifact_options.expect("only artifact management probes page tails");
                let tail_position = start_row * GRID_COLS + page_items - 1;
                let cell = ArtifactCell {
                    row: tail_position / GRID_COLS,
                    col: tail_position % GRID_COLS,
                };
                first_grid = self.ctrl.capture_artifact_grid().ok();
                visible_cards = first_grid
                    .as_ref()
                    .map(|grid| visible_artifact_extent(grid, &self.ctrl.scaler));
                // An empty tail slot leaves the previous selection on screen.
                if visible_cards.is_none_or(|count| tail_position >= count) {
                    return ScanAction::Continue;
                }
                match self.ctrl.select_artifact(cell, options) {
                    Ok(ArtifactSelection::Ready(frame)) => {
                        let action = callback(self.ctrl, GridEvent::PageTail { frame: &frame });
                        if matches!(action, ScanAction::Continue) {
                            probed_tail = Some(frame);
                        }
                        action
                    },
                    Ok(ArtifactSelection::Cancelled) => {
                        termination = ScanTermination::Cancelled;
                        ScanAction::Stop
                    },
                    Err(e) => {
                        log_warn!(
                            "[backpack] 页尾截图失败，改为顺序扫描: {}",
                            "[backpack] Page tail capture failed, starting sequential scan: {}",
                            e
                        );
                        ScanAction::Continue
                    },
                }
            });
            if matches!(page_action, ScanAction::Stop) {
                if termination != ScanTermination::Cancelled {
                    termination = ScanTermination::CallbackStop;
                }
                break;
            }
            let skip = matches!(page_action, ScanAction::SkipPage);
            if skip {
                scanned_count += page_items;
            }

            // Weapons keep their fixed-delay shortcut for identical adjacent grid icons.
            let weapon_samples = if artifact_options.is_none() {
                match self.capture_game_with_retry() {
                    Ok(image) => sample_grid_cells(
                        &image,
                        &self.ctrl.scaler,
                        start_row,
                        page_items.div_ceil(GRID_COLS),
                        total.div_ceil(GRID_COLS),
                        scanned_count / GRID_COLS,
                        (total - 1) % GRID_COLS + 1,
                    ),
                    Err(e) => {
                        log_warn!(
                            "[backpack] 无法获取武器网格，将使用常规等待: {}",
                            "[backpack] Weapon grid unavailable; using normal waits: {}",
                            e
                        );
                        Vec::new()
                    },
                }
            } else {
                Vec::new()
            };

            let items_to_read = if skip { 0 } else { page_items };
            for offset in 0..items_to_read {
                if self.ctrl.check_rmb() {
                    termination = ScanTermination::Cancelled;
                    stopped = true;
                    break;
                }
                let index = page_start_idx + offset;
                let position = start_row * GRID_COLS + offset;
                let cell = ArtifactCell {
                    row: position / GRID_COLS,
                    col: position % GRID_COLS,
                };
                if index < start_at {
                    scanned_count += 1;
                    continue;
                }

                let mut grid_image = None;
                let mut tail_action = TailAction::Emit;
                let capture = if let Some(options) = artifact_options {
                    let cached = prefetched.next().or_else(|| {
                        if offset + 1 == page_items {
                            probed_tail.take()
                        } else {
                            None
                        }
                    });
                    let (selected, from_probe) = match cached {
                        Some(frame) => (Ok(ArtifactSelection::Ready(frame)), true),
                        None => (self.ctrl.select_artifact(cell, options), false),
                    };
                    match selected {
                        Ok(ArtifactSelection::Cancelled) => {
                            termination = ScanTermination::Cancelled;
                            stopped = true;
                            break;
                        },
                        Ok(ArtifactSelection::Ready(frame)) => {
                            if votes.indices.contains(&offset) {
                                match first_grid.take().map_or_else(|| self.ctrl.capture_artifact_grid(), Ok) {
                                    Ok(image) => {
                                        if config.detect_empty_cells {
                                            visible_cards = Some(visible_artifact_extent(&image, &self.ctrl.scaler));
                                        }
                                        grid_image = Some(image);
                                    },
                                    Err(e) => log_warn!("[backpack] 网格图标截图失败，将使用详情: {}",
                                        "[backpack] Grid icon capture failed; using artifact details: {}", e),
                                }
                            }
                            if config.detect_empty_cells {
                                let signature = if from_probe {
                                    ArtifactPanelSignature::from_frame(&frame, &self.ctrl.scaler)
                                } else {
                                    self.ctrl
                                        .last_artifact_signature()
                                        .expect("accepted detail must have a signature")
                                        .clone()
                                };
                                // The page image is only a tie-breaker: an actual card with
                                // identical stats must be emitted. An empty slot keeps the
                                // last real item's detail displayed after its click.
                                if visual_end_is_authoritative(
                                    index,
                                    config.min_items_before_visual_end,
                                ) {
                                    let same = position
                                        .checked_sub(1)
                                        .and_then(|i| page_signatures[i].as_ref())
                                        .map(|previous| signature.similar(previous));
                                    tail_action = artifact_tail_action(
                                        position,
                                        same,
                                        visible_cards,
                                        final_page,
                                        pending_tail.is_some(),
                                        offset + 1 == page_items,
                                    );
                                }
                                page_signatures[position] = Some(signature);
                            }
                            Ok(frame)
                        },
                        Err(e) => Err(e),
                    }
                } else {
                    let PanelWaitMode::FixedDelay { delay_ms } = config.panel_wait else {
                        unreachable!()
                    };
                    self.ctrl.click_at(
                        GRID_FIRST_X + cell.col as f64 * GRID_OFFSET_X,
                        GRID_FIRST_Y + cell.row as f64 * GRID_OFFSET_Y,
                    );
                    let same_icon = offset > 0
                        && offset < weapon_samples.len()
                        && visual_samples_similar(
                            &weapon_samples[offset],
                            &weapon_samples[offset - 1],
                        );
                    if !same_icon {
                        utils::sleep(delay_ms as u32);
                        if let Err(e) = self.ctrl.ensure_panel_stable(PANEL_POOL_RECT, 100) {
                            log_error!(
                                "[backpack] 武器面板稳定检查失败: {}",
                                "[backpack] Weapon panel stability check failed: {}",
                                e
                            );
                            missed_count += 1;
                        }
                    }
                    if config.extra_delay > 0 {
                        utils::sleep(config.extra_delay as u32);
                    }
                    self.ctrl.capture_game().map(CaptureFrame::full)
                };

                if tail_action == TailAction::End {
                    // The first unchanged candidate was held back from both OCR and indexing.
                    termination = ScanTermination::EmptyCell;
                    stopped = true;
                    break;
                }
                if let Some(pending) = pending_tail.take() {
                    scanned_count += 1;
                    page_had_items = true;
                    if matches!(callback(self.ctrl, pending), ScanAction::Stop) {
                        termination = ScanTermination::CallbackStop;
                        stopped = true;
                        break;
                    }
                }
                let frame = match capture {
                    Ok(frame) => frame,
                    Err(e) => {
                        missed_count += 1;
                        log_error!(
                            "[backpack] 物品详情截图失败 (idx={}): {}",
                            "[backpack] Item detail capture failed (idx={}): {}",
                            index,
                            e
                        );
                        if matches!(
                            config.panel_wait,
                            PanelWaitMode::Artifact {
                                stop_on_failure: true,
                                ..
                            }
                        ) {
                            termination = ScanTermination::CaptureFailure;
                            stopped = true;
                            break;
                        }
                        scanned_count += 1;
                        continue;
                    },
                };
                let event = GridEvent::Item {
                    idx: index,
                    row: cell.row,
                    col: cell.col,
                    layout,
                    frame,
                    grid_image,
                };
                if tail_action == TailAction::Hold {
                    pending_tail = Some(event);
                    continue;
                }
                scanned_count += 1;
                page_had_items = true;
                if matches!(callback(self.ctrl, event), ScanAction::Stop) {
                    termination = ScanTermination::CallbackStop;
                    stopped = true;
                    break;
                }
            }

            if page_had_items
                && matches!(
                    callback(self.ctrl, GridEvent::PageCompleted { page_start_idx }),
                    ScanAction::Stop
                )
            {
                if !stopped {
                    termination = ScanTermination::CallbackStop;
                }
                break;
            }
            if stopped || scanned_count >= total {
                break;
            }
            if final_page {
                if visual_end_is_authoritative(scanned_count, config.min_items_before_visual_end) {
                    termination = ScanTermination::UnchangedPage;
                } else {
                    termination = ScanTermination::CaptureFailure;
                    missed_count += 1;
                    log_error!(
                        "[backpack] 最后一页结束时仍未达到已读数量",
                        "[backpack] Final page ended before the observed inventory count"
                    );
                }
                break;
            }

            let requested = (total - scanned_count).div_ceil(GRID_COLS).min(GRID_ROWS);
            let advanced = if let Some(options) =
                artifact_options.filter(|_| config.detect_empty_cells)
            {
                match self.scroll_rows_verified(
                    requested,
                    config.delay_scroll,
                    &page_signatures,
                    options,
                    first_grid.as_ref(),
                ) {
                    Ok((VerifiedScrollOutcome::AdvancedRows(rows), frames)) => {
                        prefetched = frames.into_iter();
                        rows
                    },
                    Ok((VerifiedScrollOutcome::FinalRows(rows), frames)) => {
                        prefetched = frames.into_iter();
                        final_page = true;
                        rows
                    },
                    Ok((VerifiedScrollOutcome::EndOfList, _)) => {
                        if visual_end_is_authoritative(
                            scanned_count,
                            config.min_items_before_visual_end,
                        ) {
                            termination = ScanTermination::UnchangedPage;
                        } else {
                            missed_count += 1;
                            termination = ScanTermination::CaptureFailure;
                            log_error!(
                                "[backpack] 尚未达到已读数量，列表已停止滚动",
                                "[backpack] Reached the end before the observed inventory count"
                            );
                        }
                        break;
                    },
                    Ok((VerifiedScrollOutcome::Cancelled, _)) => {
                        termination = ScanTermination::Cancelled;
                        break;
                    },
                    Err(e) => {
                        log_error!(
                            "[backpack] 无法确认翻页: {}",
                            "[backpack] Could not confirm page turn: {}",
                            yas::lang::localize(&e.to_string())
                        );
                        missed_count += 1;
                        termination = ScanTermination::CaptureFailure;
                        break;
                    },
                }
            } else {
                if !self.scroll_rows(requested, config.delay_scroll) {
                    termination = ScanTermination::Cancelled;
                    break;
                }
                self.record_completed_scroll(requested);
                self.ctrl.reset_artifact_panel();
                requested
            };
            start_row = GRID_ROWS - advanced;
            page_signatures.rotate_left(advanced * GRID_COLS);
            page_signatures[start_row * GRID_COLS..].fill(None);
            if matches!(
                callback(self.ctrl, GridEvent::PageScrolled),
                ScanAction::Stop
            ) {
                termination = ScanTermination::CallbackStop;
                break;
            }
        }
        ScanGridOutcome {
            termination,
            scanned_count,
            missed_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use image::Rgb;

    use super::*;

    #[test]
    fn first_page_needing_ocr_permanently_stops_tail_clicks() {
        let mut enabled = true;
        let mut clicks = Vec::new();
        for page in 0..6 {
            let action = probe_leading_page(&mut enabled, || {
                clicks.push(page);
                if page == 2 {
                    ScanAction::Continue
                } else {
                    ScanAction::SkipPage
                }
            });
            assert_eq!(matches!(action, ScanAction::SkipPage), page < 2);
        }
        assert_eq!(clicks, [0, 1, 2]);
    }

    #[test]
    fn ordered_short_turn_identifies_final_page_without_a_scrollbar() {
        assert_eq!(
            resolve_scroll_advance(Some(3), None, None, 5, None),
            Some(VerifiedScrollOutcome::FinalRows(3))
        );
        assert_eq!(
            resolve_scroll_advance(Some(3), None, None, 3, None),
            Some(VerifiedScrollOutcome::AdvancedRows(3))
        );
    }

    #[test]
    fn equal_artifacts_on_visible_cards_survive_tail_detection() {
        use TailAction::*;
        // Positions 1 and 2 are real twins. Clicks 3 and 4 retain their detail.
        assert_eq!(
            artifact_tail_action(2, Some(true), Some(3), true, false, false),
            Emit
        );
        assert_eq!(
            artifact_tail_action(3, Some(true), Some(3), true, false, false),
            Hold
        );
        assert_eq!(
            artifact_tail_action(4, Some(true), Some(3), true, true, false),
            End
        );
        // New detail at the next position releases the previous candidate.
        assert_eq!(
            artifact_tail_action(4, Some(false), Some(3), true, true, false),
            Emit
        );
        assert_eq!(
            artifact_tail_action(39, Some(true), Some(39), true, false, true),
            End
        );
        assert_eq!(
            artifact_tail_action(39, Some(true), Some(40), true, false, true),
            Emit
        );
    }

    #[test]
    fn empty_first_page_holds_its_first_frame_out_of_ocr() {
        use TailAction::*;
        assert_eq!(
            artifact_tail_action(0, None, Some(0), false, false, false),
            Hold
        );
        assert_eq!(
            artifact_tail_action(1, Some(true), Some(0), false, true, false),
            End
        );
        assert_eq!(
            artifact_tail_action(0, None, Some(1), false, false, false),
            Emit
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
    fn ordered_artifact_details_distinguish_partial_full_and_unchanged_turns() {
        let old: Vec<_> = (0..GRID_ROWS * GRID_COLS).map(Some).collect();
        for rows in 0..GRID_ROWS {
            let new: Vec<_> = (rows * GRID_COLS..(rows + 1) * GRID_COLS).collect();
            assert_eq!(
                ordered_row_advance(&old, &new, GRID_ROWS, usize::eq),
                Some(rows)
            );
        }
        let new: Vec<_> = (100..100 + GRID_COLS).collect();
        assert_eq!(
            ordered_row_advance(&old, &new, GRID_ROWS, usize::eq),
            Some(GRID_ROWS)
        );
        assert_eq!(ordered_row_advance(&old, &new, 3, usize::eq), None);
    }

    #[test]
    fn matching_an_icon_or_one_artifact_does_not_confirm_a_retained_row() {
        let mut old: Vec<_> = (0..GRID_ROWS * GRID_COLS).map(Some).collect();
        let mut new: Vec<_> = (0..GRID_COLS).collect();
        // The first piece agrees but the other seven have different rolls.
        new[1..].fill(91);
        assert_eq!(
            ordered_row_advance(&old, &new, GRID_ROWS, usize::eq),
            Some(GRID_ROWS)
        );
        new = (0..GRID_COLS).collect();
        new[0] = 91;
        assert_eq!(ordered_row_advance(&old, &new, GRID_ROWS, usize::eq), None);
        new[0] = 0;
        // Two truly identical sequences are ambiguous.
        for (cell, &item) in old[GRID_COLS..GRID_COLS * 2].iter_mut().zip(&new) {
            *cell = Some(item);
        }
        assert_eq!(ordered_row_advance(&old, &new, GRID_ROWS, usize::eq), None);
        old[GRID_COLS..GRID_COLS * 2].fill(None);
        assert_eq!(ordered_row_advance(&old, &new, GRID_ROWS, usize::eq), None);
        // An empty click or identical artifacts across a row need independent
        // evidence; this row provides no ordered pattern.
        assert_eq!(
            ordered_row_advance(&old, &[91; GRID_COLS], GRID_ROWS, usize::eq),
            None
        );
    }

    #[test]
    fn skipped_page_visuals_align_a_real_three_row_bottom_clamp() {
        // Only the anonymized artwork sample points from the two settled
        // inventory frames are retained. No details or scrollbar are present.
        let strips = image::load_from_memory(include_bytes!(
            "../../../tests/fixtures/artifact_skipped_grid.png"
        ))
        .unwrap()
        .to_rgb8();
        let before = image::imageops::crop_imm(&strips, 0, 0, 655, 540).to_image();
        let after = image::imageops::crop_imm(&strips, 655, 0, 655, 540).to_image();
        let scaler = CoordScaler::new(960, 540);
        assert_eq!(skipped_grid_advance(&before, &after, &scaler, 5), Some(3));
        assert_eq!(skipped_grid_advance(&before, &before, &scaler, 5), Some(0));
    }

    #[test]
    fn skipped_page_with_repeated_icons_does_not_prove_row_movement() {
        let scaler = CoordScaler::new(960, 540);
        let identical = RgbImage::from_pixel(655, 540, Rgb([150, 121, 70]));
        assert_eq!(
            skipped_grid_advance(&identical, &identical, &scaler, 5),
            None
        );
        // Each row has visibly different icons, but reproduces the same order.
        let repeated = RgbImage::from_fn(655, 540, |x, _| {
            if (x / 73) % 2 == 0 {
                Rgb([30, 80, 170])
            } else {
                Rgb([180, 145, 50])
            }
        });
        assert_eq!(skipped_grid_advance(&repeated, &repeated, &scaler, 5), None);
    }

    #[test]
    fn real_filtered_inventory_scrollbar_records_full_partial_and_stopped_turns() {
        // Anonymized 1080p track crops: first full turn, last partial turn,
        // then the same bottom position after the animation finished.
        let sheet = image::load_from_memory(include_bytes!(
            "../../../tests/fixtures/artifact_scrollbar.png"
        ))
        .unwrap()
        .to_rgb8();
        for scale in [1, 2] {
            let mut thumbs = Vec::new();
            for i in 0..5 {
                let image = image::imageops::crop_imm(&sheet, i * 22, 0, 22, 950).to_image();
                let scaled = image::imageops::resize(
                    &image,
                    22 * scale,
                    950 * scale,
                    image::imageops::FilterType::Nearest,
                );
                thumbs.push(scroll_thumb(&scaled).unwrap());
            }
            let unit = (thumbs[1].center() - thumbs[0].center()) / GRID_ROWS as f64;
            assert_eq!(
                scroll_advance_from_thumb(thumbs[0], thumbs[1], GRID_ROWS, None),
                Some(5)
            );
            assert_eq!(
                scroll_advance_from_thumb(thumbs[2], thumbs[3], GRID_ROWS, None),
                None
            );
            assert_eq!(
                scroll_advance_from_thumb(thumbs[2], thumbs[3], GRID_ROWS, Some(unit)),
                Some(3)
            );
            assert_eq!(
                scroll_advance_from_thumb(thumbs[3], thumbs[4], GRID_ROWS, Some(unit)),
                Some(0)
            );
            assert!(thumbs[3].at_bottom());
            assert_eq!(
                resolve_scroll_advance(None, Some(thumbs[2]), Some(thumbs[3]), 5, Some(unit)),
                Some(VerifiedScrollOutcome::FinalRows(3))
            );
            assert_eq!(
                resolve_scroll_advance(Some(0), Some(thumbs[3]), Some(thumbs[4]), 5, Some(unit)),
                Some(VerifiedScrollOutcome::EndOfList)
            );
            // A unique old first row still matches on a single-page inventory
            // where the UI hides the scrollbar entirely.
            assert_eq!(
                resolve_scroll_advance(Some(0), None, None, 5, None),
                Some(VerifiedScrollOutcome::EndOfList)
            );
            assert_eq!(resolve_scroll_advance(None, None, None, 5, None), None);
            assert_eq!(
                resolve_scroll_advance(Some(0), Some(thumbs[1]), Some(thumbs[1]), 5, Some(unit)),
                None
            );
        }
    }

    #[test]
    fn inventory_scroll_parks_on_the_grid_center() {
        assert_eq!(
            inventory_scroll_cursor(),
            (
                GRID_FIRST_X + (GRID_COLS / 2) as f64 * GRID_OFFSET_X,
                GRID_FIRST_Y + (GRID_ROWS / 2) as f64 * GRID_OFFSET_Y,
            )
        );
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
