/// Elixir (祝圣) Y-offset in pixels (at 1920x1080 base)
pub const ELIXIR_SHIFT: f64 = 40.0;

/// Low-tier weapon names that signal we've reached the end of useful weapons.
/// When these are detected, the weapon scanner stops.
pub const WEAPON_STOP_NAMES: &[&str] = &[
    "\u{5386}\u{7EC3}\u{7684}\u{730E}\u{5F13}", // 历练的猎弓
    "\u{53E3}\u{888B}\u{9B54}\u{5BFC}\u{4E66}", // 口袋魔导书
    "\u{94C1}\u{5C16}\u{67AA}",                 // 铁尖枪
    "\u{4F63}\u{5175}\u{91CD}\u{5251}",         // 佣兵重剑
    "\u{94F6}\u{5251}",                         // 银剑
    "\u{730E}\u{5F13}",                         // 猎弓
    "\u{5B66}\u{5F92}\u{7B14}\u{8BB0}",         // 学徒笔记
    "\u{65B0}\u{624B}\u{957F}\u{67AA}",         // 新手长枪
    "\u{8BAD}\u{7EC3}\u{5927}\u{5251}",         // 训练大剑
    "\u{65E0}\u{950B}\u{5251}",                 // 无锋剑
];

/// Characters that have no constellations or special constellation handling (skip scanning).
/// Traveler has element-specific constellations that require special UI handling.
/// Characters with no constellation scanning.
/// Aloy/Manekin/Manekina have no constellations.
/// Traveler has element-specific constellation sub-tabs that require special UI handling.
pub const NO_CONSTELLATION_CHARACTERS: &[&str] = &["Aloy", "Manekin", "Manekina"];

// ================================================================
// Default delay values (in milliseconds), matching GOODScanner settings.json
// ================================================================

pub const DEFAULT_DELAY_OPEN_SCREEN: u64 = 1500;
pub const DEFAULT_DELAY_CLOSE_SCREEN: u64 = 500;
pub const DEFAULT_DELAY_CHAR_TAB_SWITCH: u64 = 650;
pub const DEFAULT_DELAY_CHAR_NEXT: u64 = 300;
pub const DEFAULT_DELAY_INV_TAB_SWITCH: u64 = 400;
pub const DEFAULT_DELAY_SCROLL: u64 = 100;
pub const DEFAULT_DELAY_GRID_ITEM: u64 = 125;
/// Weapon: fixed delay (ms) before stability check. Weapons use fixed-delay
/// mode because identical weapons have identical panels (fingerprint won't change).
pub const DEFAULT_WEAPON_PANEL_DELAY: u64 = 50;
/// Artifact: initial wait (ms) after click before starting panel detection.
pub const DEFAULT_ARTIFACT_INITIAL_WAIT: u64 = 20;
/// Artifact: normal readiness timeout (ms), before bounded retry/recovery.
pub const DEFAULT_ARTIFACT_PANEL_TIMEOUT: u64 = 200;
/// Artifact: minimum stable-detail hold (ms), revalidated throughout.
pub const DEFAULT_ARTIFACT_EXTRA_DELAY: u64 = 0;

/// Right-side artifact detail panel for partial capture (base 1920×1080).
/// Covers all OCR regions and pixel checks (stars, lock, elixir) on the panel.
pub const ARTIFACT_DETAIL_PANEL_RECT: (f64, f64, f64, f64) = (1310.0, 110.0, 480.0, 860.0);

// ================================================================
// Character scanner coordinates (at 1920x1080 base resolution)
// From GOODScanner/lib/character_scanner.js
// ================================================================

/// Character name + element OCR region
pub const CHAR_NAME_RECT: (f64, f64, f64, f64) = (128.0, 18.0, 330.0, 60.0);
/// Character level OCR region
pub const CHAR_LEVEL_RECT: (f64, f64, f64, f64) = (1443.0, 203.0, 242.0, 42.0);

/// Left-side tab positions (character detail screen)
pub const CHAR_TAB_ATTRIBUTES: (f64, f64) = (220.0, 158.0);
pub const CHAR_TAB_CONSTELLATION: (f64, f64) = (220.0, 368.0);
pub const CHAR_TAB_TALENTS: (f64, f64) = (170.0, 435.0);

/// Constellation node click positions for the phase-2 tree
/// (x=1695, y = 270 + index*113). Used only when phase-1 pixels are invalid.
pub const CHAR_CONSTELLATION_X: f64 = 1695.0;
pub const CHAR_CONSTELLATION_Y_BASE: f64 = 270.0;
pub const CHAR_CONSTELLATION_Y_STEP: f64 = 113.0;

/// Constellation activate status OCR region ("已激活") on a node detail popup.
pub const CHAR_CONSTELLATION_ACTIVATE_RECT: (f64, f64, f64, f64) = (218.0, 1002.0, 82.0, 31.0);

// Constellation icon pixel detection constants.
// Icons are arranged on an S-curve, each with a unique (cx, cy) position.
// We sample a ring (not full circle) to avoid the icon center where both
// active (ability art) and locked (lock icon) have similar brightness.
// The ring captures the glow (active) vs dark circle edge (locked).
//
// Phase 1 samples all 6 rings from one capture (prefix + gap check).
// Phase 2 clicks along C2 → C1 / C6 → C3 → C4 → C5, never C1..C6 in order.
//
// Accuracy: 100% on 109 test characters (min gap=+55.4, d'=7.14).
/// Per-node icon centers on the S-curve (base 1920x1080 coords)
pub const CONSTELLATION_NODES: [(f64, f64); 6] = [
    (1524.0, 277.0), // C1
    (1578.0, 387.0), // C2
    (1614.0, 500.0), // C3
    (1614.0, 611.0), // C4
    (1578.0, 722.0), // C5
    (1507.0, 832.0), // C6
];
/// Ring inner radius (skip icon center)
pub const CONSTELLATION_RING_INNER: i32 = 30;
/// Ring outer radius (capture glow region)
pub const CONSTELLATION_RING_OUTER: i32 = 41;
/// Per-position brightness thresholds (midpoint of min_active and max_locked)
pub const CONSTELLATION_THRESHOLDS: [f64; 6] = [
    // Computed from 109 test characters with ring(30,41) sampling
    // C1: gap=+77.5  C2: gap=+74.5  C3: gap=+71.0
    // C4: gap=+69.4  C5: gap=+64.2  C6: gap=+55.4
    85.9, 86.6, 88.8, 93.7, 98.3, 107.3,
];

/// Talent overview OCR regions (level display on right side of talent list)
/// Width: 90px to accommodate 2-digit levels (Lv.13) at 1080p.
pub const CHAR_TALENT_OVERVIEW_AUTO: (f64, f64, f64, f64) = (1620.0, 167.0, 90.0, 30.0);
pub const CHAR_TALENT_OVERVIEW_SKILL: (f64, f64, f64, f64) = (1620.0, 258.0, 90.0, 30.0);
pub const CHAR_TALENT_OVERVIEW_BURST: (f64, f64, f64, f64) = (1620.0, 349.0, 90.0, 30.0);
/// Special burst position for Ayaka/Mona (4-slot talent layout)
pub const CHAR_TALENT_OVERVIEW_BURST_SPECIAL: (f64, f64, f64, f64) = (1620.0, 440.0, 90.0, 30.0);

/// Talent detail click positions (x=1695, y = 165 + index*90)
pub const CHAR_TALENT_CLICK_X: f64 = 1695.0;
pub const CHAR_TALENT_FIRST_Y: f64 = 165.0;
pub const CHAR_TALENT_OFFSET_Y: f64 = 90.0;

/// Talent level OCR region (in detail view)
pub const CHAR_TALENT_LEVEL_RECT: (f64, f64, f64, f64) = (255.0, 168.0, 68.0, 26.0);

/// Next character button position
pub const CHAR_NEXT_POS: (f64, f64) = (1850.0, 530.0);
/// Previous character button, mirrored across the character screen.
pub const CHAR_PREV_POS: (f64, f64) = (70.0, 530.0);

// ================================================================
// Weapon scanner coordinates (at 1920x1080 base resolution)
// ================================================================

/// Weapon card region base
pub const WEAPON_CARD_X: f64 = 1307.0;
pub const WEAPON_CARD_Y: f64 = 119.0;

/// Weapon OCR regions (relative offsets from card base are baked in)
pub const WEAPON_NAME_RECT: (f64, f64, f64, f64) = (1305.0, 119.0, 496.0, 59.0);
pub const WEAPON_LEVEL_RECT: (f64, f64, f64, f64) = (1370.0, 389.0, 131.0, 30.0);
pub const WEAPON_REFINEMENT_RECT: (f64, f64, f64, f64) = (1368.0, 439.0, 124.0, 32.0);
pub const WEAPON_EQUIP_RECT: (f64, f64, f64, f64) = (1417.0, 999.0, 419.0, 50.0);

/// Star rarity pixel Y position and X thresholds
pub const STAR_Y: f64 = 372.0;
pub const STAR_5_X: f64 = 1485.0;
pub const STAR_4_X: f64 = 1450.0;
pub const STAR_3_X: f64 = 1416.0;

/// Weapon lock detection pixels
pub const WEAPON_LOCK_POS1: (f64, f64) = (1768.0, 428.0);
pub const WEAPON_LOCK_POS2: (f64, f64) = (1740.0, 429.0);

/// Backpack item count OCR region
pub const ITEM_COUNT_RECT: (f64, f64, f64, f64) = (1545.0, 30.0, 263.0, 38.0);

/// Backpack tab positions
pub const TAB_WEAPON: (f64, f64) = (585.0, 50.0);
pub const TAB_ARTIFACT: (f64, f64) = (675.0, 50.0);

/// "5-star sort by acquired time" filter toggle, sampled on the track just
/// left of the ON-state knob. OFF: dark track (~64) with the white knob on the
/// left, so x≤1230 hits that knob and reads bright in both states. ON: beige
/// track (~185). Also the click target for toggling.
pub const ARTIFACT_FIVE_STAR_FILTER_POS: (f64, f64) = (1236.0, 135.0);

// ================================================================
// Artifact scanner coordinates (at 1920x1080 base resolution)
// ================================================================

pub const ARTIFACT_PART_RECT: (f64, f64, f64, f64) = (1348.0, 190.0, 200.0, 35.0);
pub const ARTIFACT_MAIN_STAT_RECT: (f64, f64, f64, f64) = (1348.0, 283.0, 226.0, 35.0);
pub const ARTIFACT_ELIXIR_RECT: (f64, f64, f64, f64) = (1360.0, 410.0, 140.0, 26.0);
pub const ARTIFACT_LEVEL_RECT: (f64, f64, f64, f64) = (1360.0, 455.0, 65.0, 33.0);
pub const ARTIFACT_SUBSTATS_RECT: (f64, f64, f64, f64) = (1353.0, 475.0, 247.0, 150.0);
/// Base Y for set name; adjusted by -(4 - num_substats) * 40
pub const ARTIFACT_SET_NAME_BASE_Y: f64 = 630.0;
pub const ARTIFACT_SET_NAME_RECT_BASE: (f64, f64, f64, f64) = (1330.0, 630.0, 200.0, 30.0);
pub const ARTIFACT_EQUIP_RECT: (f64, f64, f64, f64) = (1357.0, 999.0, 419.0, 50.0);

/// Artifact lock detection pixels (with y_shift support)
pub const ARTIFACT_LOCK_POS1: (f64, f64) = (1683.0, 428.0);
pub const ARTIFACT_LOCK_POS2: (f64, f64) = (1708.0, 428.0);

/// Artifact astral mark detection pixels (with y_shift support)
pub const ARTIFACT_ASTRAL_POS1: (f64, f64) = (1768.0, 428.0);
pub const ARTIFACT_ASTRAL_POS2: (f64, f64) = (1740.0, 429.0);

// ================================================================
// Backpack grid layout (at 1920x1080 base resolution)
// ================================================================

pub const GRID_COLS: usize = 8;
pub const GRID_ROWS: usize = 5;
pub const GRID_FIRST_X: f64 = 180.0;
pub const GRID_FIRST_Y: f64 = 253.0;
pub const GRID_OFFSET_X: f64 = 145.0;
pub const GRID_OFFSET_Y: f64 = 166.0;

/// Scroll ticks per grid page
pub const SCROLL_TICKS_PER_PAGE: i32 = 49;
/// Correction: scroll back 1 tick every N pages
pub const SCROLL_CORRECTION_INTERVAL: i32 = 3;

/// Characters with special talent layout (4 talents instead of 3)
pub const SPECIAL_BURST_CHARACTERS: &[&str] = &["KamisatoAyaka", "Mona"];

/// Tartaglia's auto talent has an innate +1 bonus that must be subtracted
pub const TARTAGLIA_KEY: &str = "Tartaglia";
