use image::RgbImage;
use yas::log_debug;

use super::capture_frame::CaptureFrame;
use super::coord_scaler::CoordScaler;

/// Read RGB values at a base-coord position. Returns [0,0,0] if out of bounds.
fn read_pixel_rgb(image: &RgbImage, scaler: &CoordScaler, bx: f64, by: f64) -> [u8; 3] {
    let x = scaler.x(bx) as u32;
    let y = scaler.y(by) as u32;
    if x < image.width() && y < image.height() {
        let p = image.get_pixel(x, y);
        [p[0], p[1], p[2]]
    } else {
        [0, 0, 0]
    }
}

/// Check if a pixel at the given position (in base 1920x1080 coords) is "star yellow".
/// Star yellow = R > 150, G > 100, B < 100
///
/// Port of `isStarYellow()` from GOODScanner/lib/ocr_utils.js
pub fn is_star_yellow(image: &RgbImage, scaler: &CoordScaler, base_x: f64, base_y: f64) -> bool {
    super::annotator::record_region(
        "star_yellow",
        image,
        (base_x - 10.0, base_y - 10.0, 21.0, 21.0),
        scaler,
    );
    let x = scaler.x(base_x) as u32;
    let y = scaler.y(base_y) as u32;

    if x >= image.width() || y >= image.height() {
        return false;
    }

    let pixel = image.get_pixel(x, y);
    let r = pixel[0];
    let g = pixel[1];
    let b = pixel[2];
    r > 150 && g > 100 && b < 100
}

// ================================================================
// Icon animation early-detection thresholds.
//
// Lock/astral icons fade in/out over ~100ms. The button background
// transitions between brightness ~86 (dark = icon present) and ~239
// (light = icon absent). We use two thresholds to detect state early
// during the animation, with a wide margin for PC speed variation:
//
//   brightness < ICON_BRIGHT_PRESENT  → icon IS present   (fast path)
//   brightness > ICON_BRIGHT_ABSENT   → icon NOT present  (fast path)
//   in between                        → ambiguous, need full delay
//
// At ~50ms into the animation, brightness is at its first step:
//   ~115 (disappearing) or ~217 (appearing), both safely outside [130,210].
// ================================================================

/// Brightness at or below which the icon is definitively present (locked / astral marked).
/// True dark value is 86, true bright value is 238 (gap=152).
/// Set at 1/5 of the range so mid-animation frames trigger a retry capture.
pub const ICON_BRIGHT_PRESENT: u32 = 116;
/// Brightness at or above which the icon is definitively absent (unlocked / no astral).
/// Set at 4/5 of the range for the same reason.
pub const ICON_BRIGHT_ABSENT: u32 = 208;

/// Get the average brightness of a pixel at base 1920x1080 coordinates.
/// Returns 0 if out of bounds.
pub fn get_pixel_brightness(
    image: &RgbImage,
    scaler: &CoordScaler,
    base_x: f64,
    base_y: f64,
) -> u32 {
    super::annotator::record_region(
        "pixel_brightness",
        image,
        (base_x - 10.0, base_y - 10.0, 21.0, 21.0),
        scaler,
    );
    let x = scaler.x(base_x) as u32;
    let y = scaler.y(base_y) as u32;
    if x >= image.width() || y >= image.height() {
        return 0;
    }
    let pixel = image.get_pixel(x, y);
    (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3
}

/// Check if any icon detection pixel is in the ambiguous brightness zone
/// (mid-animation). Used for early-capture retry: if true, the caller
/// should wait longer and re-capture.
///
/// Checks artifact lock and astral positions (without elixir y_shift,
/// which is unknown at early-capture time; elixir artifacts will simply
/// fall through to the full delay, which is correct).
///
/// Also checks for impossible semantic state: astral present but lock absent.
/// All astraled artifacts are locked in-game, so this means the lock icon
/// animation hasn't settled yet.
pub fn is_artifact_icon_ambiguous(image: &RgbImage, scaler: &CoordScaler) -> bool {
    use super::constants::{ARTIFACT_ASTRAL_POS1, ARTIFACT_LOCK_POS1};
    let lock_b = get_pixel_brightness(image, scaler, ARTIFACT_LOCK_POS1.0, ARTIFACT_LOCK_POS1.1);
    if is_icon_brightness_ambiguous(lock_b) {
        return true;
    }
    let astral_b = get_pixel_brightness(
        image,
        scaler,
        ARTIFACT_ASTRAL_POS1.0,
        ARTIFACT_ASTRAL_POS1.1,
    );
    if is_icon_brightness_ambiguous(astral_b) {
        return true;
    }
    // Impossible state: astral present (dark) but lock not present.
    // All astraled artifacts must be locked — retry to let lock icon settle.
    let astral_present = astral_b <= ICON_BRIGHT_PRESENT;
    let lock_present = lock_b <= ICON_BRIGHT_PRESENT;
    if astral_present && !lock_present {
        return true;
    }
    false
}

/// Check whether the artifact lock button is still transitioning between its
/// locked and unlocked states. Both calibrated pixels must agree on a stable
/// side of the brightness gap before a manager action treats the panel state
/// as authoritative.
pub fn is_artifact_lock_ambiguous(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    y_shift: f64,
) -> bool {
    let [b1, b2] = artifact_lock_brightness(frame, scaler, y_shift);
    let p1 = b1 < 128;
    let p2 = b2 < 128;

    is_icon_brightness_ambiguous(b1) || is_icon_brightness_ambiguous(b2) || p1 != p2
}

fn is_icon_brightness_ambiguous(brightness: u32) -> bool {
    brightness >= ICON_BRIGHT_PRESENT && brightness <= ICON_BRIGHT_ABSENT
}

/// Check if the weapon lock icon pixel is in the ambiguous brightness zone.
pub fn is_weapon_icon_ambiguous(image: &RgbImage, scaler: &CoordScaler) -> bool {
    use super::constants::WEAPON_LOCK_POS1;
    let b = get_pixel_brightness(image, scaler, WEAPON_LOCK_POS1.0, WEAPON_LOCK_POS1.1);
    b >= ICON_BRIGHT_PRESENT && b <= ICON_BRIGHT_ABSENT
}

/// Check if a pixel at the given position is dark (brightness < 128).
///
/// Port of `isPixelDark()` from GOODScanner/lib/ocr_utils.js
pub fn is_pixel_dark(image: &RgbImage, scaler: &CoordScaler, base_x: f64, base_y: f64) -> bool {
    super::annotator::record_region(
        "pixel_dark",
        image,
        (base_x - 10.0, base_y - 10.0, 21.0, 21.0),
        scaler,
    );
    let x = scaler.x(base_x) as u32;
    let y = scaler.y(base_y) as u32;

    if x >= image.width() || y >= image.height() {
        return false;
    }

    let pixel = image.get_pixel(x, y);
    let brightness = (pixel[0] as u32 + pixel[1] as u32 + pixel[2] as u32) / 3;
    brightness < 128
}

/// Dual-pixel dark icon verification.
/// Checks two pixels and returns true if the first pixel is dark.
/// Logs a warning if the two pixels disagree.
///
/// Port of `detectDarkIcon()` from GOODScanner/lib/ocr_utils.js
pub fn detect_dark_icon(
    image: &RgbImage,
    scaler: &CoordScaler,
    x1: f64,
    y1: f64,
    x2: f64,
    y2: f64,
    label: &str,
) -> bool {
    let d1 = is_pixel_dark(image, scaler, x1, y1);
    let d2 = is_pixel_dark(image, scaler, x2, y2);
    if d1 != d2 {
        log_debug!(
            "[{}] 检测不一致: ({},{})={} ({},{})={}",
            "[{}] detection inconsistent: ({},{})={} ({},{})={}",
            label,
            x1,
            y1,
            d1,
            x2,
            y2,
            d2
        );
    }
    d1
}

/// Check if the "5-star sort by acquired time" filter is active.
/// When active, the pixel at ARTIFACT_FIVE_STAR_FILTER_POS is the light track;
/// when inactive it is the dark track.
/// HDR WGC dumps make the active checkmark pixel much darker than SDR:
/// RGB(117,129,145), brightness 130 on the sampled 4K captures. Use a
/// profile-specific threshold so that detector has margin in both modes.
pub fn is_five_star_filter_active(image: &RgbImage, scaler: &CoordScaler) -> bool {
    use super::constants::ARTIFACT_FIVE_STAR_FILTER_POS;
    five_star_filter_track_active(image, scaler, ARTIFACT_FIVE_STAR_FILTER_POS)
}

/// The track position is relative to the supplied screenshot.
pub(crate) fn five_star_filter_track_active(
    image: &RgbImage,
    scaler: &CoordScaler,
    track: (f64, f64),
) -> bool {
    let threshold = if super::pixel_profile::is_hdr_mode() {
        120
    } else {
        128
    };
    let brightness = get_pixel_brightness(image, scaler, track.0, track.1);
    let active = brightness >= threshold;
    if active {
        log_debug!(
            "[backpack] 五星圣遗物按获得时间排序已开启 (brightness={}, threshold={})",
            "[backpack] Five-star artifacts are sorted by acquisition time (brightness={}, threshold={})",
            brightness,
            threshold
        );
    }
    active
}

/// Detect weapon rarity from star pixels.
///
/// Scans the star row at y=STAR_Y. Returns 5, 4, 3, or 2.
///
/// Port from GOODScanner/lib/weapon_scanner.js rarity detection
pub fn detect_weapon_rarity(image: &RgbImage, scaler: &CoordScaler) -> i32 {
    use super::constants::STAR_Y;

    // Scan horizontal band to find rightmost star pixel
    let y_offsets: [f64; 3] = [-2.0, 0.0, 2.0];
    let mut rightmost_star_x: f64 = 0.0;
    let mut star_pixel_count = 0;

    for &dy in &y_offsets {
        let by = STAR_Y + dy;
        for bx_int in (1300..=1500).step_by(2) {
            let bx = bx_int as f64;
            let x = scaler.x(bx) as u32;
            let y = scaler.y(by) as u32;
            if x < image.width() && y < image.height() {
                let px = image.get_pixel(x, y);
                if px[0] > 150 && px[1] > 100 && px[2] < 100 {
                    star_pixel_count += 1;
                    if bx > rightmost_star_x {
                        rightmost_star_x = bx;
                    }
                }
            }
        }
    }

    let rarity = if rightmost_star_x > 1470.0 {
        5
    } else if rightmost_star_x > 1430.0 {
        4
    } else if rightmost_star_x > 1400.0 {
        3
    } else if star_pixel_count > 0 {
        2
    } else {
        // Fallback to original single-pixel checks
        if is_star_yellow(image, scaler, 1485.0, STAR_Y) {
            5
        } else if is_star_yellow(image, scaler, 1450.0, STAR_Y) {
            4
        } else if is_star_yellow(image, scaler, 1416.0, STAR_Y) {
            3
        } else {
            2
        }
    };

    let star_pos = match rarity {
        5 => (1485.0, STAR_Y),
        4 => (1450.0, STAR_Y),
        3 => (1416.0, STAR_Y),
        _ => (1380.0, STAR_Y),
    };
    let rgb = read_pixel_rgb(image, scaler, star_pos.0, star_pos.1);
    super::annotator::record_pixel("rarity", star_pos, rgb, &format!("{}*", rarity));

    rarity
}

/// Detect artifact rarity only when at least one star pixel is present.
///
/// This is used for scan-boundary decisions where a blank or unsettled frame
/// must remain unknown instead of being treated as a three-star artifact.
pub fn detect_artifact_rarity_evidence(frame: &CaptureFrame, scaler: &CoordScaler) -> Option<i32> {
    use super::constants::STAR_Y;

    super::annotator::record_region(
        "rarity_stars",
        &frame.image,
        (
            1340.0 - frame.origin.0,
            STAR_Y - 2.0 - frame.origin.1,
            161.0,
            5.0,
        ),
        scaler,
    );
    let y_offsets: [f64; 3] = [-2.0, 0.0, 2.0];
    let mut rightmost_star_x: f64 = 0.0;
    let mut star_pixel_count = 0;

    for &dy in &y_offsets {
        let by = STAR_Y + dy;
        for bx_int in (1340..=1500).step_by(2) {
            let bx = bx_int as f64;
            let rgb = frame.pixel(scaler, bx, by);
            if rgb[0] > 150 && rgb[1] > 100 && rgb[2] < 100 {
                star_pixel_count += 1;
                if bx > rightmost_star_x {
                    rightmost_star_x = bx;
                }
            }
        }
    }

    if rightmost_star_x > 1470.0 {
        Some(5)
    } else if rightmost_star_x > 1430.0 {
        Some(4)
    } else if rightmost_star_x > 1400.0 && star_pixel_count >= 3 {
        Some(3)
    } else if is_star_yellow_frame(frame, scaler, 1485.0, STAR_Y) {
        Some(5)
    } else if is_star_yellow_frame(frame, scaler, 1450.0, STAR_Y) {
        Some(4)
    } else {
        None
    }
}

/// Detect artifact rarity from star pixels (window coords via [`CaptureFrame`]).
pub fn detect_artifact_rarity(frame: &CaptureFrame, scaler: &CoordScaler) -> i32 {
    use super::constants::STAR_Y;

    // Preserve the scanner's historical fallback for normal identification;
    // boundary-sensitive callers use detect_artifact_rarity_evidence instead.
    let rarity = detect_artifact_rarity_evidence(frame, scaler).unwrap_or(3);

    let star_pos = match rarity {
        5 => (1485.0, STAR_Y),
        4 => (1450.0, STAR_Y),
        _ => (1416.0, STAR_Y),
    };
    let rgb = frame.pixel(scaler, star_pos.0, star_pos.1);
    super::annotator::record_pixel("rarity", star_pos, rgb, &format!("{}*", rarity));

    rarity
}

fn is_star_yellow_frame(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    base_x: f64,
    base_y: f64,
) -> bool {
    let rgb = frame.pixel(scaler, base_x, base_y);
    rgb[0] > 150 && rgb[1] > 100 && rgb[2] < 100
}

/// Check if an artifact's detected rarity is below the minimum threshold.
pub fn artifact_below_min_rarity(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    min_rarity: i32,
) -> bool {
    let rarity = detect_artifact_rarity(frame, scaler);
    if rarity < min_rarity {
        log_debug!(
            "[rarity] {}星 < 最低{}星，应停止",
            "[rarity] {}* < min {}*, should stop",
            rarity,
            min_rarity
        );
        true
    } else {
        false
    }
}

/// Legacy full-window rarity detection (tests and callers with raw images).
pub fn detect_artifact_rarity_image(image: &RgbImage, scaler: &CoordScaler) -> i32 {
    detect_artifact_rarity(&CaptureFrame::full(image.clone()), scaler)
}

/// Check if an artifact's detected rarity is below the minimum threshold.
pub fn artifact_below_min_rarity_image(
    image: &RgbImage,
    scaler: &CoordScaler,
    min_rarity: i32,
) -> bool {
    artifact_below_min_rarity(&CaptureFrame::full(image.clone()), scaler, min_rarity)
}

/// Check if a weapon's detected rarity is below the minimum threshold.
pub fn weapon_below_min_rarity(image: &RgbImage, scaler: &CoordScaler, min_rarity: i32) -> bool {
    let rarity = detect_weapon_rarity(image, scaler);
    if rarity < min_rarity {
        log_debug!(
            "[rarity] {}星 < 最低{}星，应停止",
            "[rarity] {}* < min {}*, should stop",
            rarity,
            min_rarity
        );
        true
    } else {
        false
    }
}

/// Detect if a substat line region appears dimmed (inactive/unactivated).
///
/// Active substats have bright white text (brightness > 200).
/// Inactive substats have dimmed grey text (brightness ~120-160).
/// We count the fraction of pixels above a "bright text" threshold.
/// Active lines have many bright text pixels; inactive lines have fewer.
///
/// Only samples the right 2/3 of the region to avoid the stat icon.
pub fn is_substat_dimmed(
    image: &RgbImage,
    scaler: &CoordScaler,
    rect: (f64, f64, f64, f64),
    y_shift: f64,
) -> bool {
    super::annotator::record_region(
        "substat_dimmed",
        image,
        (rect.0, rect.1 + y_shift, rect.2, rect.3),
        scaler,
    );
    let (bx, by, bw, bh) = rect;
    let x = scaler.x(bx) as u32;
    let y = scaler.y(by + y_shift) as u32;
    let w = scaler.x(bw) as u32;
    let h = scaler.y(bh) as u32;

    let x = x.min(image.width().saturating_sub(1));
    let y = y.min(image.height().saturating_sub(1));
    let w = w.min(image.width().saturating_sub(x));
    let h = h.min(image.height().saturating_sub(y));

    if w == 0 || h == 0 {
        return false;
    }

    // Skip left 1/3 (icon area), sample right 2/3
    let start_x = w / 3;
    let mut bright_count: u32 = 0;
    let mut mid_count: u32 = 0;
    let mut total_count: u32 = 0;

    for py in (0..h).step_by(2) {
        for px in (start_x..w).step_by(2) {
            let p = image.get_pixel(x + px, y + py);
            let brightness = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
            total_count += 1;
            if brightness > 200 {
                bright_count += 1;
            } else if brightness > 130 {
                mid_count += 1;
            }
        }
    }

    if total_count == 0 {
        return false;
    }

    let bright_pct = bright_count * 100 / total_count;
    let mid_pct = mid_count * 100 / total_count;
    // Active substats: bright ~85-95%, mid ~2-5%
    // Inactive substats: bright ~72-80%, mid ~19-23%
    // Threshold: mid > 15% indicates dimmed/inactive text.
    // Combined with bright < 82% to avoid false positives on active lines
    // that happen to have slightly more mid-range pixels.
    mid_pct > 20 && bright_pct < 78
}

/// Detect weapon lock state via dual-pixel verification.
///
/// Port of `detectWeaponLock()` from GOODScanner/lib/ocr_utils.js
pub fn detect_weapon_lock(image: &RgbImage, scaler: &CoordScaler) -> bool {
    use super::constants::{WEAPON_LOCK_POS1, WEAPON_LOCK_POS2};
    let locked = detect_dark_icon(
        image,
        scaler,
        WEAPON_LOCK_POS1.0,
        WEAPON_LOCK_POS1.1,
        WEAPON_LOCK_POS2.0,
        WEAPON_LOCK_POS2.1,
        "\u{6B66}\u{5668}\u{9501}\u{5B9A}", // 武器锁定
    );
    let pos = (WEAPON_LOCK_POS1.0, WEAPON_LOCK_POS1.1);
    let rgb = read_pixel_rgb(image, scaler, pos.0, pos.1);
    super::annotator::record_pixel("lock", pos, rgb, if locked { "locked" } else { "unlocked" });
    locked
}

/// Detect artifact lock state via dual-pixel verification.
/// Supports y_shift for elixir-crafted artifacts.
///
/// Port of `detectArtifactLock()` from GOODScanner/lib/ocr_utils.js
pub fn detect_artifact_lock(frame: &CaptureFrame, scaler: &CoordScaler, y_shift: f64) -> bool {
    use super::constants::{ARTIFACT_LOCK_POS1, ARTIFACT_LOCK_POS2};
    let (pos, rgb, locked) = read_artifact_icon(
        frame,
        scaler,
        [ARTIFACT_LOCK_POS1, ARTIFACT_LOCK_POS2],
        y_shift,
    );
    super::annotator::record_pixel("lock", pos, rgb, if locked { "locked" } else { "unlocked" });
    locked
}

fn brightness([r, g, b]: [u8; 3]) -> u32 {
    (u32::from(r) + u32::from(g) + u32::from(b)) / 3
}

fn read_artifact_icon(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    positions: [(f64, f64); 2],
    y_shift: f64,
) -> ((f64, f64), [u8; 3], bool) {
    let [first, second] = positions.map(|(x, y)| frame.pixel(scaler, x, y + y_shift));
    let present = brightness(first) < 128;
    if present != (brightness(second) < 128) {
        log_debug!(
            "圣遗物图标像素不一致 ({:?}): {:?} / {:?}",
            "Artifact icon pixels disagree ({:?}): {:?} / {:?}",
            positions[0],
            first,
            second
        );
    }
    ((positions[0].0, positions[0].1 + y_shift), first, present)
}

fn artifact_lock_brightness(frame: &CaptureFrame, scaler: &CoordScaler, y_shift: f64) -> [u32; 2] {
    use super::constants::{ARTIFACT_LOCK_POS1, ARTIFACT_LOCK_POS2};
    [ARTIFACT_LOCK_POS1, ARTIFACT_LOCK_POS2]
        .map(|(x, y)| brightness(frame.pixel(scaler, x, y + y_shift)))
}

/// Verify that an artifact lock toggle succeeded, using asymmetric brightness
/// thresholds that are permissive to mid-animation frames.
///
/// The lock icon has two end-states: dark (locked, brightness ~86) and bright
/// (unlocked, brightness ~238). During the toggle animation the pixel passes
/// through mid-brightness. A single threshold (<128) works for unlock→lock
/// because the pixel darkens fast, but fails for lock→unlock because the
/// brightening animation may not have crossed 128 by verify time.
///
/// Logic:
/// - `desired_lock = true`  (just locked): accept if brightness is below
///   `ICON_BRIGHT_ABSENT` (208). Any sign the pixel is darkening below the
///   definitively-bright level means the lock animation started.
/// - `desired_lock = false` (just unlocked): accept if brightness is above
///   `ICON_BRIGHT_PRESENT` (116). Any sign the pixel is brightening above
///   the definitively-dark level means the unlock animation started.
///
/// Checks both POS1 and POS2; either passing counts as verified.
pub fn verify_artifact_lock_toggled(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    y_shift: f64,
    desired_lock: bool,
) -> bool {
    let [b1, b2] = artifact_lock_brightness(frame, scaler, y_shift);
    let ok = if desired_lock {
        // Expect darkening: below "definitely bright" threshold
        b1 < ICON_BRIGHT_ABSENT || b2 < ICON_BRIGHT_ABSENT
    } else {
        // Expect brightening: above "definitely dark" threshold
        b1 > ICON_BRIGHT_PRESENT || b2 > ICON_BRIGHT_PRESENT
    };
    log_debug!(
        "[verify_lock] desired={} b1={} b2={} ok={}",
        "[verify_lock] desired={} b1={} b2={} ok={}",
        desired_lock,
        b1,
        b2,
        ok
    );
    ok
}

/// Detect artifact astral mark via dual-pixel verification.
/// Supports y_shift for elixir-crafted artifacts.
///
/// Port of `detectArtifactAstralMark()` from GOODScanner/lib/ocr_utils.js
pub fn detect_artifact_astral_mark(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    y_shift: f64,
) -> bool {
    use super::constants::{ARTIFACT_ASTRAL_POS1, ARTIFACT_ASTRAL_POS2};
    let (pos, rgb, marked) = read_artifact_icon(
        frame,
        scaler,
        [ARTIFACT_ASTRAL_POS1, ARTIFACT_ASTRAL_POS2],
        y_shift,
    );
    super::annotator::record_pixel(
        "astral",
        pos,
        rgb,
        if marked { "marked" } else { "unmarked" },
    );
    marked
}

/// Sample average brightness in a ring around a constellation icon position.
///
/// `c_index` is 0-based: 0=C1, 1=C2, ..., 5=C6.
/// Samples pixels between r_inner and r_outer from the icon center.
/// The ring avoids the icon center (where active art and locked lock icon have
/// similar brightness) and captures the glow vs dark-circle region.
///
/// Returns the average (R+G+B)/3 brightness of sampled pixels.
fn sample_constellation_brightness(image: &RgbImage, scaler: &CoordScaler, c_index: usize) -> f64 {
    use super::constants::{
        CONSTELLATION_NODES, CONSTELLATION_RING_INNER, CONSTELLATION_RING_OUTER,
    };

    let (cx, cy) = CONSTELLATION_NODES[c_index];
    let r_inner = CONSTELLATION_RING_INNER;
    let r_outer = CONSTELLATION_RING_OUTER;
    let r_inner_sq = (r_inner as f64) * (r_inner as f64);
    let r_outer_sq = (r_outer as f64) * (r_outer as f64);

    let mut sum = 0.0_f64;
    let mut count = 0u32;

    // Sample every other pixel (step=2) for speed
    let mut bx = cx as i32 - r_outer;
    let bx_end = cx as i32 + r_outer;
    while bx <= bx_end {
        let mut dy = -r_outer;
        while dy <= r_outer {
            let dx = bx as f64 - cx;
            let dist_sq = dx * dx + (dy as f64) * (dy as f64);
            if dist_sq >= r_inner_sq && dist_sq <= r_outer_sq {
                let px = scaler.x(bx as f64) as u32;
                let py = scaler.y(cy + dy as f64) as u32;
                if px < image.width() && py < image.height() {
                    let pixel = image.get_pixel(px, py);
                    sum += (pixel[0] as f64 + pixel[1] as f64 + pixel[2] as f64) / 3.0;
                    count += 1;
                }
            }
            dy += 2;
        }
        bx += 2;
    }

    if count > 0 {
        sum / count as f64
    } else {
        0.0
    }
}

/// Per-node constellation detection result (for annotation).
pub struct ConstellationNodeInfo {
    /// Icon center position in base 1920×1080 coords.
    pub pos: (f64, f64),
    /// Measured brightness (ring average).
    pub brightness: f64,
    /// Per-position detection threshold.
    pub threshold: f64,
    /// Whether this constellation is activated (brightness >= threshold).
    pub activated: bool,
}

/// Full constellation detection result.
pub struct ConstellationResult {
    /// Final constellation level (0–6).
    pub level: i32,
    /// Whether the detection pattern is monotonic (A*L* — no gaps).
    pub monotonic: bool,
    /// Per-node detection info for all 6 constellation nodes.
    pub nodes: [ConstellationNodeInfo; 6],
}

struct ConstellationSamples {
    brightnesses: [f64; 6],
    active: [bool; 6],
}

fn sample_all_constellation_nodes(image: &RgbImage, scaler: &CoordScaler) -> ConstellationSamples {
    use super::constants::CONSTELLATION_THRESHOLDS;

    let brightnesses = std::array::from_fn(|ci| sample_constellation_brightness(image, scaler, ci));
    let active = std::array::from_fn(|ci| brightnesses[ci] >= CONSTELLATION_THRESHOLDS[ci]);
    ConstellationSamples {
        brightnesses,
        active,
    }
}

/// Phase 1: constellation = length of the leading ON prefix.
/// `monotonic` is false when any later node is ON (a gap).
fn constellation_level_from_prefix(active: [bool; 6]) -> (i32, bool) {
    let mut constellation = 0;
    for (ci, on) in active.iter().enumerate() {
        if *on {
            constellation = ci as i32 + 1;
        } else {
            break;
        }
    }
    let monotonic = active
        .iter()
        .enumerate()
        .all(|(ci, on)| *on == (ci < constellation as usize));
    (constellation, monotonic)
}

/// Phase 2 click tree: C2 → C1 / C6 → C3 → C4 → C5.
/// Unsampled nodes must stay false (the live click path only probes the
/// branch it takes).
pub(crate) fn constellation_level_from_tree(active: [bool; 6]) -> i32 {
    if !active[1] {
        if active[0] {
            1
        } else {
            0
        }
    } else if active[5] {
        6
    } else if !active[2] {
        2
    } else if !active[3] {
        3
    } else if active[4] {
        5
    } else {
        4
    }
}

fn constellation_result_from_samples(
    samples: ConstellationSamples,
    level: i32,
    monotonic: bool,
) -> ConstellationResult {
    use super::constants::CONSTELLATION_NODES;
    use super::constants::CONSTELLATION_THRESHOLDS;

    if !monotonic {
        let det_str: String = samples
            .active
            .iter()
            .map(|&a| if a { 'A' } else { 'L' })
            .collect();
        log_debug!(
            "[constellation-pixel] 非单调: [{}] br=[{:.0},{:.0},{:.0},{:.0},{:.0},{:.0}] → C{}",
            "[constellation-pixel] NON-MONOTONIC: [{}] br=[{:.0},{:.0},{:.0},{:.0},{:.0},{:.0}] → C{}",
            det_str,
            samples.brightnesses[0],
            samples.brightnesses[1],
            samples.brightnesses[2],
            samples.brightnesses[3],
            samples.brightnesses[4],
            samples.brightnesses[5],
            level
        );
    }

    let nodes = std::array::from_fn(|ci| ConstellationNodeInfo {
        pos: CONSTELLATION_NODES[ci],
        brightness: samples.brightnesses[ci],
        threshold: CONSTELLATION_THRESHOLDS[ci],
        activated: samples.active[ci],
    });

    let result = ConstellationResult {
        level,
        monotonic,
        nodes,
    };
    super::annotator::record_constellation(&result);
    result
}

/// Phase 1 constellation detection: sample all 6 rings from one capture.
///
/// Level is the leading ON prefix. A later ON node is non-monotonic and
/// should be resolved by the phase-2 click tree.
pub fn detect_constellation_pixel(image: &RgbImage, scaler: &CoordScaler) -> ConstellationResult {
    let samples = sample_all_constellation_nodes(image, scaler);
    let (level, monotonic) = constellation_level_from_prefix(samples.active);
    constellation_result_from_samples(samples, level, monotonic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::common::test_utils::*;

    #[test]
    fn test_artifact_rarity_5_star() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        let scaler = make_1080p_scaler();
        assert_eq!(
            detect_artifact_rarity(&CaptureFrame::full(image), &scaler),
            5
        );
    }

    #[test]
    fn test_artifact_rarity_4_star() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 4);
        let scaler = make_1080p_scaler();
        assert_eq!(
            detect_artifact_rarity(&CaptureFrame::full(image), &scaler),
            4
        );
    }

    #[test]
    fn test_artifact_rarity_3_star() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 3);
        let scaler = make_1080p_scaler();
        assert_eq!(
            detect_artifact_rarity(&CaptureFrame::full(image), &scaler),
            3
        );
    }

    #[test]
    fn test_artifact_rarity_blank_image() {
        let image = make_1080p_image();
        let scaler = make_1080p_scaler();
        assert_eq!(
            detect_artifact_rarity_evidence(&CaptureFrame::full(image.clone()), &scaler),
            None
        );
        // Blank image has no star pixels; fallback single-pixel checks also fail → returns 3
        assert_eq!(
            detect_artifact_rarity(&CaptureFrame::full(image), &scaler),
            3
        );
    }

    #[test]
    fn test_artifact_rarity_partial_star_row_is_not_boundary_evidence() {
        let mut image = make_1080p_image();
        let yellow = image::Rgb([200, 180, 50]);
        for y in 370..=374 {
            for x in (1350..=1398).step_by(2) {
                image.put_pixel(x, y, yellow);
            }
        }
        let scaler = make_1080p_scaler();
        assert_eq!(
            detect_artifact_rarity_evidence(&CaptureFrame::full(image), &scaler),
            None
        );
    }

    #[test]
    fn test_weapon_rarity_5_star() {
        let mut image = make_1080p_image();
        paint_rarity_stars(&mut image, 5);
        let scaler = make_1080p_scaler();
        assert_eq!(detect_weapon_rarity(&image, &scaler), 5);
    }

    #[test]
    fn test_artifact_lock_detected() {
        let mut image = make_1080p_image();
        paint_artifact_lock(&mut image, true, 0.0);
        let scaler = make_1080p_scaler();
        assert!(detect_artifact_lock(
            &CaptureFrame::full(image),
            &scaler,
            0.0
        ));
    }

    #[test]
    fn test_artifact_unlock_detected() {
        let mut image = make_1080p_image();
        paint_artifact_lock(&mut image, false, 0.0);
        let scaler = make_1080p_scaler();
        assert!(!detect_artifact_lock(
            &CaptureFrame::full(image),
            &scaler,
            0.0
        ));
    }

    #[test]
    fn test_artifact_lock_with_elixir_shift() {
        let mut image = make_1080p_image();
        // Paint unlocked at base position so black pixels don't read as "dark"
        paint_artifact_lock(&mut image, false, 0.0);
        // Paint locked at shifted position
        paint_artifact_lock(&mut image, true, 40.0);
        let scaler = make_1080p_scaler();
        let frame = CaptureFrame::full(image);
        assert!(!detect_artifact_lock(&frame, &scaler, 0.0));
        assert!(detect_artifact_lock(&frame, &scaler, 40.0));
    }

    #[test]
    fn test_artifact_astral_mark_detected() {
        let mut image = make_1080p_image();
        paint_artifact_astral(&mut image, true, 0.0);
        let scaler = make_1080p_scaler();
        assert!(detect_artifact_astral_mark(
            &CaptureFrame::full(image),
            &scaler,
            0.0
        ));
    }

    #[test]
    fn test_artifact_astral_mark_absent() {
        let mut image = make_1080p_image();
        paint_artifact_astral(&mut image, false, 0.0);
        let scaler = make_1080p_scaler();
        assert!(!detect_artifact_astral_mark(
            &CaptureFrame::full(image),
            &scaler,
            0.0
        ));
    }

    #[test]
    fn test_weapon_lock_detected() {
        let mut image = make_1080p_image();
        paint_weapon_lock(&mut image, true);
        let scaler = make_1080p_scaler();
        assert!(detect_weapon_lock(&image, &scaler));
    }

    #[test]
    fn test_weapon_unlock_detected() {
        let mut image = make_1080p_image();
        paint_weapon_lock(&mut image, false);
        let scaler = make_1080p_scaler();
        assert!(!detect_weapon_lock(&image, &scaler));
    }

    #[test]
    fn test_icon_ambiguous_mid_animation() {
        let mut image = make_1080p_image();
        let scaler = make_1080p_scaler();
        let mid: [u8; 3] = [150, 150, 150];
        set_pixel(&mut image, 1683, 428, mid);
        assert!(is_artifact_icon_ambiguous(&image, &scaler));
    }

    #[test]
    fn test_icon_not_ambiguous_when_clearly_locked() {
        let mut image = make_1080p_image();
        let scaler = make_1080p_scaler();
        let dark: [u8; 3] = [60, 60, 60];
        set_pixel(&mut image, 1683, 428, dark);
        set_pixel(&mut image, 1768, 428, dark);
        assert!(!is_artifact_icon_ambiguous(&image, &scaler));
    }

    #[test]
    fn test_artifact_lock_ambiguity_respects_elixir_shift() {
        let mut image = make_1080p_image();
        let scaler = make_1080p_scaler();
        paint_artifact_lock(&mut image, false, 0.0);
        paint_artifact_lock(&mut image, true, 40.0);

        let mut frame = CaptureFrame::full(image);
        assert!(!is_artifact_lock_ambiguous(&frame, &scaler, 40.0));

        let mid: [u8; 3] = [150, 150, 150];
        set_pixel(&mut frame.image, 1683, 468, mid);
        set_pixel(&mut frame.image, 1768, 468, mid);
        assert!(is_artifact_lock_ambiguous(&frame, &scaler, 40.0));
    }

    #[test]
    fn cropped_artifact_details_read_and_verify_lock_at_the_same_game_pixels() {
        use super::super::constants::ARTIFACT_DETAIL_PANEL_RECT;
        let scaler = make_1080p_scaler();
        let mut image = make_1080p_image();
        paint_artifact_lock(&mut image, false, 0.0);
        paint_artifact_astral(&mut image, false, 0.0);
        paint_artifact_lock(&mut image, true, 40.0);
        paint_artifact_astral(&mut image, true, 40.0);
        let full = CaptureFrame::full(image);
        let rect = ARTIFACT_DETAIL_PANEL_RECT;
        let detail = CaptureFrame {
            image: full.crop(rect, 0.0, &scaler).unwrap(),
            origin: (rect.0, rect.1),
        };
        for frame in [&full, &detail] {
            for (shift, expected) in [(0.0, false), (40.0, true)] {
                assert_eq!(detect_artifact_lock(frame, &scaler, shift), expected);
                assert_eq!(detect_artifact_astral_mark(frame, &scaler, shift), expected);
                assert!(!is_artifact_lock_ambiguous(frame, &scaler, shift));
                assert!(verify_artifact_lock_toggled(
                    frame, &scaler, shift, expected
                ));
                assert!(!verify_artifact_lock_toggled(
                    frame, &scaler, shift, !expected
                ));
            }
        }
    }

    #[test]
    fn five_star_filter_distinguishes_track_from_off_knob() {
        let mut image = make_1080p_image();
        let scaler = make_1080p_scaler();
        // OFF (1080p SDR capture): white knob on the left, dark track at the probe.
        set_pixel(&mut image, 1228, 135, [236, 229, 216]);
        set_pixel(&mut image, 1236, 135, [53, 61, 79]);
        assert!(!is_five_star_filter_active(&image, &scaler));

        // ON: beige track across the probe, knob moved right.
        set_pixel(&mut image, 1228, 135, [212, 189, 142]);
        set_pixel(&mut image, 1236, 135, [222, 197, 146]);
        assert!(is_five_star_filter_active(&image, &scaler));
    }

    #[test]
    fn constellation_phase1_prefix_flags_gaps() {
        fn on(n: usize) -> [bool; 6] {
            std::array::from_fn(|i| i < n)
        }
        for n in 0..=6 {
            assert_eq!(constellation_level_from_prefix(on(n)), (n as i32, true));
        }
        // C1 on, C2 off, C3 on — phase 1 keeps the prefix and flags the gap.
        let gapped = [true, false, true, false, false, false];
        assert_eq!(constellation_level_from_prefix(gapped), (1, false));
    }

    #[test]
    fn constellation_phase2_tree_follows_c2_c1_c6_c3_c4_c5() {
        fn on(n: usize) -> [bool; 6] {
            std::array::from_fn(|i| i < n)
        }
        assert_eq!(constellation_level_from_tree(on(0)), 0);
        assert_eq!(constellation_level_from_tree(on(1)), 1);
        assert_eq!(constellation_level_from_tree(on(2)), 2);
        assert_eq!(constellation_level_from_tree(on(3)), 3);
        assert_eq!(constellation_level_from_tree(on(4)), 4);
        assert_eq!(constellation_level_from_tree(on(5)), 5);
        assert_eq!(constellation_level_from_tree(on(6)), 6);
        // C2 and C6 on with a dark C1: phase 1 would be C0+gap; the tree yields C6.
        assert_eq!(
            constellation_level_from_tree([false, true, false, false, false, true]),
            6
        );
    }
}
