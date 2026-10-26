/// Grid-based icon detection for artifact/weapon lock, astral mark, and elixir status.
///
/// Instead of waiting for the detail panel animation to settle (100ms+ per item),
/// this module reads icon status directly from the inventory grid in full screenshots.
///
/// Algorithm:
/// 1. Capture a full game screenshot
/// 2. Calibrate grid offset via lightness-based template matching (SAT + edge detectors)
/// 3. For each cell, compute precise icon slot positions using calibrated offset
/// 4. Classify each slot by mean color of a small crop area
///
/// Grid calibration uses the known grid structure (8 cols × 5 rows, fixed spacing)
/// and correlates a template of column gaps (-1, dark) and row-boundary edges
/// (+1 white band → -1 dark gap) against the image lightness via a summed area table.
/// This works for both artifact and weapon tabs without requiring locked items.
///
/// **Artifact layout**: slot 1 = lock, slot 2 = astral (if locked), next = elixir.
/// If lock is absent, astral shifts up; if both absent, elixir is in slot 1.
///
/// **Weapon layout**: slot 1 = refinement badge, slot 2 = lock. No astral/elixir.
///
/// Detection is performed 3 times per page at evenly spaced moments, with majority
/// vote for denoising (the currently-selected item has an animated border that can
/// slightly shift its icon position).
use image::RgbImage;
use yas::log_debug;

use super::coord_scaler::CoordScaler;

// ================================================================
// Grid layout constants (at 1920×1080 base resolution)
// ================================================================

/// Grid cell center origin (first cell). Derived from:
/// GX_LOCK=131.15, GY_LOCK=193.4 at 1080p (half of 4K: 262.3, 386.8)
/// LOCK_DX=-48.65, LOCK_DY=-59.25 at 1080p (half of 4K: -97.3, -118.5)
/// Cell center = lock_pos - lock_offset
const GRID_CX: f64 = 179.8; // 131.15 - (-48.65)
const GRID_CY: f64 = 252.65; // 193.4 - (-59.25)

/// Cell spacing (1080p base)
const GRID_OX: f64 = 146.4; // 292.8 / 2
const GRID_OY: f64 = 175.2; // 350.4 / 2

/// Lock icon offset from cell center (1080p base)
const LOCK_DX: f64 = -48.65; // -97.3 / 2
const LOCK_DY: f64 = -59.25; // -118.5 / 2

/// Vertical spacing between icon slots (1080p base)
const SLOT_SPACING: f64 = 22.65; // 45.3 / 2

/// Half-size of the crop area for mean-color classification (1080p base pixels)
const CROP_HALF: f64 = 4.0; // ~8×8 area at 1080p (scales to ~16×16 at 4K)

/// Grid dimensions
const COLS: usize = 8;
const ROWS: usize = 5;
pub const ITEMS_PER_PAGE: usize = 40;

// ================================================================
// Grid calibration constants (at 1920×1080 base resolution)
// ================================================================

/// Card dimensions (1080p base)
const CARD_W: f64 = 123.5; // 247 / 2
const CARD_H: f64 = 153.0; // 306 / 2

/// Visual card centers, which differ from the deliberately inset legacy click
/// spacing. Use these for selection-rim/occupancy evidence, not click geometry.
pub fn artifact_cell_center(row: usize, col: usize) -> (f64, f64) {
    (
        GRID_CX + col as f64 * GRID_OX,
        GRID_CY + row as f64 * GRID_OY,
    )
}

/// Width of the bright band above each row gap used for Y edge detection.
/// Narrow bands produce sharper Y peaks. 10px at 1080p = 20px at 4K.
const EDGE_W: f64 = 10.0;

/// Weapon grid is ~57px higher at 1080p (114px at 4K) than artifact grid.
const WEAPON_CY_OFFSET: f64 = -57.0;

/// Search radius for grid calibration (1080p base pixels).
/// The actual search is ±SEARCH_R scaled to the current resolution.
const SEARCH_R: f64 = 30.0;

/// Maximum plausible positive artifact Y drift after inventory page scrolling.
///
/// The row-edge scorer can lock onto the next lower horizontal edge, which is
/// almost exactly one icon-slot spacing below the real card origin. Offsets
/// above this threshold are that false solution in current artifact captures.
const MAX_POSITIVE_ARTIFACT_GRID_OFF_Y: f64 = 8.0;

// ================================================================
// Color classification thresholds
// ================================================================

/// Lock icon: pink/red. Threshold: mean R > 180 AND mean (R-G) > 50
const LOCK_R_MIN: f64 = 180.0;
const LOCK_RG_DIFF_MIN: f64 = 50.0;

/// Astral mark: golden/yellow. Threshold: mean (G-B) > 100
const ASTRAL_GB_DIFF_MIN: f64 = 100.0;

/// Elixir mark: purple/blue. Threshold: mean (B-G) > 20 AND mean B > 180
const ELIXIR_BG_DIFF_MIN: f64 = 20.0;
const ELIXIR_B_MIN: f64 = 180.0;

/// Which inventory tab we're scanning — determines icon slot layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GridMode {
    /// Artifacts: slot 1 = lock, slot 2 = astral, slot 3 = elixir
    Artifact,
    /// Weapons: slot 1 = refinement badge (ignored), slot 2 = lock. No astral/elixir.
    Weapon,
}

/// Per-item detection result from a single pass.
#[derive(Debug, Clone, Copy, Default)]
pub struct GridIconResult {
    pub lock: bool,
    pub astral: bool,
    pub elixir: bool,
}

/// Accumulated detection results for a full page with majority voting.
pub struct GridPageDetection {
    /// Results per item index (absolute), accumulated across passes.
    /// Each entry holds vote counts: (lock_yes, lock_no, astral_yes, astral_no, elixir_yes, elixir_no)
    votes: Vec<(u32, u32, u32, u32, u32, u32)>,
    /// The first absolute item index on this page.
    page_start: usize,
    /// Number of items on this page.
    page_items: usize,
    /// Physical screen row containing the first logical item on this page.
    screen_start_row: usize,
    /// Icon slot layout mode.
    mode: GridMode,
    /// Cached grid calibration offset (base resolution).
    /// Computed on the first detection pass and reused for subsequent passes,
    /// since the grid position doesn't change between captures of the same page.
    cached_offset: Option<(f64, f64)>,
    /// Cell geometry from the first detection pass — same coordinates the real
    /// detection logic used, so annotations can never diverge.
    cached_cells: Vec<GridCellAnnotation>,
}

impl GridPageDetection {
    /// Create a new detection accumulator for a page.
    pub fn new(page_start: usize, page_items: usize) -> Self {
        Self::with_mode(page_start, page_items, GridMode::Artifact)
    }

    /// Create a new detection accumulator for a page with explicit mode.
    pub fn with_mode(page_start: usize, page_items: usize, mode: GridMode) -> Self {
        Self::with_layout(page_start, page_items, 0, mode)
    }

    /// Create a detection accumulator for logical items that begin below the
    /// top screen row, as happens when the final scroll reveals a short tail.
    pub fn with_layout(
        page_start: usize,
        page_items: usize,
        screen_start_row: usize,
        mode: GridMode,
    ) -> Self {
        Self {
            votes: vec![(0, 0, 0, 0, 0, 0); page_items],
            page_start,
            page_items,
            screen_start_row,
            mode,
            cached_offset: None,
            cached_cells: Vec::new(),
        }
    }

    /// Run one detection pass on a full screenshot and accumulate votes.
    ///
    /// `selected_item` is the absolute index of the currently selected item
    /// (which has an animated border — its detection is less reliable, but we
    /// still include it since the crop area is tolerant enough).
    pub fn detect_pass(&mut self, image: &RgbImage, scaler: &CoordScaler, _selected_item: usize) {
        super::annotator::record_region("grid_icons", image, (0.0, 100.0, 1310.0, 980.0), scaler);
        // Calibrate on first pass, reuse for subsequent passes
        let (off_x, off_y) = match self.cached_offset {
            Some(offset) => offset,
            None => {
                let offset = calibrate_grid(image, scaler, self.mode);
                self.cached_offset = Some(offset);
                offset
            },
        };

        let (results, cells) = detect_page_icons(
            image,
            scaler,
            self.page_items,
            self.screen_start_row,
            off_x,
            off_y,
            self.mode,
        );
        // Cache cell geometry from first pass (positions don't change between passes)
        if self.cached_cells.is_empty() {
            self.cached_cells = cells;
        }
        for (i, result) in results.iter().enumerate() {
            let v = &mut self.votes[i];
            if result.lock {
                v.0 += 1;
            } else {
                v.1 += 1;
            }
            if result.astral {
                v.2 += 1;
            } else {
                v.3 += 1;
            }
            if result.elixir {
                v.4 += 1;
            } else {
                v.5 += 1;
            }
        }
    }

    /// Get the majority-vote result for an absolute item index.
    /// Returns `None` if the index is outside this page.
    pub fn get(&self, abs_index: usize) -> Option<GridIconResult> {
        if abs_index < self.page_start || abs_index >= self.page_start + self.page_items {
            return None;
        }
        let i = abs_index - self.page_start;
        let v = &self.votes[i];
        Some(GridIconResult {
            lock: v.0 > v.1,
            astral: v.2 > v.3,
            elixir: v.4 > v.5,
        })
    }

    /// Build annotation data for all cells on this page: bounding boxes + detection results.
    /// Cell geometry comes from the actual detection pass — same coordinates used for real logic.
    /// Returns `None` if no detection pass has run.
    pub fn annotation_snapshot(
        &self,
    ) -> Option<(Vec<GridCellAnnotation>, Vec<(usize, bool, bool)>)> {
        if self.cached_cells.is_empty() {
            return None;
        }
        let detections: Vec<(usize, bool, bool)> = (0..self.page_items)
            .map(|i| {
                let v = &self.votes[i];
                (i, v.0 > v.1, v.2 > v.3) // (index, lock, astral)
            })
            .collect();
        Some((self.cached_cells.clone(), detections))
    }

    /// Number of detection passes accumulated so far.
    pub fn pass_count(&self) -> u32 {
        if self.page_items == 0 {
            return 0;
        }
        let v = &self.votes[0];
        v.0 + v.1 // lock_yes + lock_no = total passes
    }
}

// ================================================================
// Lightness Summed Area Table (SAT) for efficient grid calibration
// ================================================================

/// Summed area table of pixel lightness values for O(1) rectangle queries.
///
/// Lightness L = (max(R,G,B) + min(R,G,B)) / 2, normalized to [0, 1].
/// The SAT enables computing the sum of L within any rectangle in constant time.
struct LightnessSat {
    /// SAT data, dimensions (h+1) × (w+1). Row-major, 0-indexed.
    /// sat[(y+1) * stride + (x+1)] = sum of L for pixels [0..y, 0..x].
    data: Vec<f64>,
    w: usize,
    h: usize,
}

impl LightnessSat {
    /// Build the SAT from an RGB image.
    fn new(image: &RgbImage) -> Self {
        let w = image.width() as usize;
        let h = image.height() as usize;
        let stride = w + 1;
        let mut data = vec![0.0f64; (h + 1) * stride];

        for y in 0..h {
            for x in 0..w {
                let p = image.get_pixel(x as u32, y as u32);
                let r = p[0] as f64 / 255.0;
                let g = p[1] as f64 / 255.0;
                let b = p[2] as f64 / 255.0;
                let l = (r.max(g).max(b) + r.min(g).min(b)) / 2.0;
                data[(y + 1) * stride + (x + 1)] =
                    l + data[y * stride + (x + 1)] + data[(y + 1) * stride + x]
                        - data[y * stride + x];
            }
        }

        Self { data, w, h }
    }

    /// Sum of lightness values in the rectangle [x1, x2) × [y1, y2).
    /// Coordinates are clamped to image bounds.
    #[inline]
    fn rect_sum(&self, x1: f64, y1: f64, x2: f64, y2: f64) -> f64 {
        let x1 = (x1 as i32).clamp(0, self.w as i32) as usize;
        let y1 = (y1 as i32).clamp(0, self.h as i32) as usize;
        let x2 = (x2 as i32).clamp(0, self.w as i32) as usize;
        let y2 = (y2 as i32).clamp(0, self.h as i32) as usize;
        if x1 >= x2 || y1 >= y2 {
            return 0.0;
        }
        let s = self.w + 1;
        self.data[y2 * s + x2] - self.data[y1 * s + x2] - self.data[y2 * s + x1]
            + self.data[y1 * s + x1]
    }
}

// ================================================================
// Grid calibration via lightness template matching
// ================================================================

/// Calibrate grid Y offset using lightness-based template matching.
///
/// The grid X position is anchored by the UI and never drifts — only vertical
/// scroll displaces cards — so `off_x` is always 0; only `off_y` is searched.
/// An earlier version tried to also search X by summing lightness in candidate
/// gap rectangles, but because card interiors contain dark content (text, icons,
/// borders) the score latched onto intra-card stripes ~22px off the true gap,
/// causing 1500+ false unlocks across the inventory.
///
/// The Y score is: +1 at bright card-bottom level band, −1 at dark row gap,
/// integrated over all 8 columns × 4 row boundaries.
///
/// Returns `(0.0, off_y)` in 1080p base coordinates. For artifacts: off_y ≈ 0
/// at top of inventory; scroll causes ±12 px gy variation. For weapons: the
/// expected center is shifted by `WEAPON_CY_OFFSET`.
pub fn calibrate_grid(image: &RgbImage, scaler: &CoordScaler, mode: GridMode) -> (f64, f64) {
    super::annotator::record_region(
        "grid_calibration",
        image,
        (0.0, 100.0, 1310.0, 980.0),
        scaler,
    );
    let sat = LightnessSat::new(image);

    // Scale all constants to actual resolution
    let grid_cx = scaler.scale_x(GRID_CX);
    let grid_cy = scaler.scale_y(GRID_CY);
    let grid_ox = scaler.scale_x(GRID_OX);
    let grid_oy = scaler.scale_y(GRID_OY);
    let card_w = scaler.scale_x(CARD_W);
    let card_h = scaler.scale_y(CARD_H);
    let edge_w = scaler.scale_y(EDGE_W);
    let search_r = scaler.factor_y().max(1.0) as i32 * SEARCH_R as i32;

    let gap_h = grid_oy - card_h;

    // Expected grid center Y (mode-dependent); X is fixed.
    let exp_cy = match mode {
        GridMode::Artifact => grid_cy,
        GridMode::Weapon => grid_cy + scaler.scale_y(WEAPON_CY_OFFSET),
    };

    // Find best gy using row edge score.
    let mut best_y_score = f64::NEG_INFINITY;
    let mut best_gy = exp_cy;

    for dy in -search_r..=search_r {
        let gy = exp_cy + dy as f64;
        let mut score = 0.0;

        // Row boundary edges: 4 boundaries × 8 columns
        // +1 at white band (card bottom), -1 at dark row gap
        for row in 0..(ROWS - 1) {
            let y_bot = gy + row as f64 * grid_oy + card_h / 2.0;
            for col in 0..COLS {
                let cx = grid_cx + col as f64 * grid_ox;
                let xl = cx - card_w / 2.0;
                let xr = cx + card_w / 2.0;

                // +1: bright band at card bottom (white level band area)
                score += sat.rect_sum(xl, y_bot - edge_w, xr, y_bot);
                // -1: dark row gap
                score -= sat.rect_sum(xl, y_bot, xr, y_bot + gap_h);
            }
        }

        if score > best_y_score {
            best_y_score = score;
            best_gy = gy;
        }
    }

    let raw_off_y = (best_gy - exp_cy) / scaler.factor_y();
    let off_y = match mode {
        GridMode::Artifact => normalize_artifact_grid_y_offset(raw_off_y),
        GridMode::Weapon => raw_off_y,
    };
    if (off_y - raw_off_y).abs() > f64::EPSILON {
        log_debug!(
            "[grid-cal] Y校准命中下方边缘，修正 {:+.1} -> {:+.1}",
            "[grid-cal] Y calibration hit lower edge, corrected {:+.1} -> {:+.1}",
            raw_off_y,
            off_y
        );
    }

    (0.0, off_y)
}

fn normalize_artifact_grid_y_offset(raw_off_y: f64) -> f64 {
    if raw_off_y > MAX_POSITIVE_ARTIFACT_GRID_OFF_Y {
        raw_off_y - SLOT_SPACING
    } else {
        raw_off_y
    }
}

// ================================================================
// Icon detection
// ================================================================

/// Detect icons for all items on a page from a single full screenshot.
///
/// Uses a pre-computed grid offset to position icon slot sampling areas.
/// Returns detection results AND the cell geometry that was actually used,
/// so annotations can never diverge from the real detection coordinates.
fn detect_page_icons(
    image: &RgbImage,
    scaler: &CoordScaler,
    page_items: usize,
    screen_start_row: usize,
    off_x: f64,
    off_y: f64,
    mode: GridMode,
) -> (Vec<GridIconResult>, Vec<GridCellAnnotation>) {
    let cy_base = match mode {
        GridMode::Weapon => GRID_CY + WEAPON_CY_OFFSET,
        GridMode::Artifact => GRID_CY,
    };

    let mut results = Vec::with_capacity(page_items);
    let mut cells = Vec::with_capacity(page_items);

    for i in 0..page_items {
        let (row, col) = page_cell_position(i, screen_start_row);

        // Cell center (1080p base coords) with calibration offset
        let cx = GRID_CX + col as f64 * GRID_OX + off_x;
        let cy = cy_base + row as f64 * GRID_OY + off_y;

        // Icon slot 1 position (= cell center + lock offset)
        let slot_x = cx + LOCK_DX;
        let slot1_y = cy + LOCK_DY;
        let slot2_y = slot1_y + SLOT_SPACING;

        let (lock_pos, astral_pos) = match mode {
            GridMode::Artifact => ((slot_x, slot1_y), (slot_x, slot2_y)),
            GridMode::Weapon => ((slot_x, slot2_y), (slot_x, slot2_y)),
        };

        cells.push(GridCellAnnotation {
            rect: (cx - CARD_W / 2.0, cy - CARD_H / 2.0, CARD_W, CARD_H),
            lock_pos,
            astral_pos,
        });

        match mode {
            GridMode::Artifact => {
                // Artifact: slot 1 = lock, slot 2 = astral (if locked), slot 3 = elixir
                let slot1_color = sample_mean_color(image, scaler, slot_x, slot1_y);
                let has_lock = is_lock_color(&slot1_color);

                let mut has_astral = false;
                let mut has_elixir = false;

                let slot2_color = sample_mean_color(image, scaler, slot_x, slot2_y);

                if has_lock && is_astral_color(&slot2_color) {
                    has_astral = true;
                } else if is_elixir_color(&slot2_color) {
                    has_elixir = true;
                }

                if has_lock && has_astral && !has_elixir {
                    let slot3_y = slot1_y + 2.0 * SLOT_SPACING;
                    let slot3_color = sample_mean_color(image, scaler, slot_x, slot3_y);
                    if is_elixir_color(&slot3_color) {
                        has_elixir = true;
                    }
                }

                if has_astral && !has_lock {
                    log_debug!(
                        "[grid-icon] idx={} 星标但无锁，强制锁定",
                        "[grid-icon] idx={} astral but no lock, forcing lock",
                        i
                    );
                }

                results.push(GridIconResult {
                    lock: has_lock,
                    astral: has_astral,
                    elixir: has_elixir,
                });
            },
            GridMode::Weapon => {
                // Weapon: slot 1 = refinement badge (ignored), slot 2 = lock. No astral/elixir.
                let slot2_color = sample_mean_color(image, scaler, slot_x, slot2_y);
                let has_lock = is_lock_color(&slot2_color);

                results.push(GridIconResult {
                    lock: has_lock,
                    astral: false,
                    elixir: false,
                });
            },
        }
    }

    (results, cells)
}

fn page_cell_position(page_relative_index: usize, screen_start_row: usize) -> (usize, usize) {
    (
        screen_start_row + page_relative_index / COLS,
        page_relative_index % COLS,
    )
}

/// Mean color (R, G, B) of a small crop area around a base-resolution point.
fn sample_mean_color(
    image: &RgbImage,
    scaler: &CoordScaler,
    base_x: f64,
    base_y: f64,
) -> (f64, f64, f64) {
    let cx = scaler.scale_x(base_x);
    let cy = scaler.scale_y(base_y);
    let half_w = scaler.scale_x(CROP_HALF);
    let half_h = scaler.scale_y(CROP_HALF);

    let img_w = image.width();
    let img_h = image.height();

    let x1 = ((cx - half_w) as u32).max(0).min(img_w.saturating_sub(1));
    let y1 = ((cy - half_h) as u32).max(0).min(img_h.saturating_sub(1));
    let x2 = ((cx + half_w) as u32).min(img_w);
    let y2 = ((cy + half_h) as u32).min(img_h);

    let mut sum_r: f64 = 0.0;
    let mut sum_g: f64 = 0.0;
    let mut sum_b: f64 = 0.0;
    let mut count: u32 = 0;

    for py in y1..y2 {
        for px in x1..x2 {
            let pixel = image.get_pixel(px, py);
            sum_r += pixel[0] as f64;
            sum_g += pixel[1] as f64;
            sum_b += pixel[2] as f64;
            count += 1;
        }
    }

    if count == 0 {
        return (0.0, 0.0, 0.0);
    }

    (
        sum_r / count as f64,
        sum_g / count as f64,
        sum_b / count as f64,
    )
}

/// Per-cell annotation data for grid overlay drawing.
#[derive(Clone)]
pub struct GridCellAnnotation {
    /// Cell bounding box in base 1920×1080 coords (x, y, w, h).
    pub rect: (f64, f64, f64, f64),
    /// Lock icon position (base coords). Always computed; use with GridIconResult.lock.
    pub lock_pos: (f64, f64),
    /// Astral icon position (base coords, artifacts only).
    pub astral_pos: (f64, f64),
}

fn is_lock_color(color: &(f64, f64, f64)) -> bool {
    let (r, g, _b) = *color;
    r > LOCK_R_MIN && (r - g) > LOCK_RG_DIFF_MIN
}

fn is_astral_color(color: &(f64, f64, f64)) -> bool {
    let (_r, g, b) = *color;
    (g - b) > ASTRAL_GB_DIFF_MIN
}

fn is_elixir_color(color: &(f64, f64, f64)) -> bool {
    let (_r, g, b) = *color;
    (b - g) > ELIXIR_BG_DIFF_MIN && b > ELIXIR_B_MIN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_y_normalization_corrects_lower_edge_alias() {
        // These are the repeated bad page offsets observed in captured 4K
        // artifact grids. They are one icon-slot spacing below the true grid.
        assert!((normalize_artifact_grid_y_offset(24.5) - 1.85).abs() < 1e-9);
        assert!((normalize_artifact_grid_y_offset(18.5) - -4.15).abs() < 1e-9);
        assert!((normalize_artifact_grid_y_offset(12.5) - -10.15).abs() < 1e-9);
    }

    #[test]
    fn grid_y_normalization_preserves_plausible_offsets() {
        assert_eq!(normalize_artifact_grid_y_offset(-10.5), -10.5);
        assert_eq!(normalize_artifact_grid_y_offset(-4.5), -4.5);
        assert_eq!(normalize_artifact_grid_y_offset(1.5), 1.5);
        assert_eq!(
            normalize_artifact_grid_y_offset(MAX_POSITIVE_ARTIFACT_GRID_OFF_Y),
            MAX_POSITIVE_ARTIFACT_GRID_OFF_Y
        );
    }

    #[test]
    fn short_tail_indices_start_on_the_authoritative_screen_row() {
        assert_eq!(page_cell_position(0, 3), (3, 0));
        assert_eq!(page_cell_position(7, 3), (3, 7));
        assert_eq!(page_cell_position(8, 3), (4, 0));
        assert_eq!(page_cell_position(15, 3), (4, 7));
    }
}
