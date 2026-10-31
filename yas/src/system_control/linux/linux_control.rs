//! Input injection for Linux with two backends:
//!
//! - **XTEST** (X11 sessions): fake input through the X server. Zero external
//!   dependencies, exact absolute positioning.
//! - **uinput** (Wayland sessions, X11 fallback): compositors such as KWin
//!   ignore XTEST fake input, so the scanner creates its own virtual pointer
//!   and touchscreen directly on `/dev/uinput` (see `uinput_dev.rs`) — no
//!   uinput daemon required.
//!
//! The backend is picked on first use: an X11 session prefers XTEST (with a
//! live effectiveness check, exact absolute positioning), then falls back to
//! uinput; a Wayland session goes straight to uinput and only tries XTEST if
//! `/dev/uinput` is inaccessible. The public method surface mirrors
//! `WindowsSystemControl`; `enigo::Key` stays the key type so callers remain
//! platform-independent.
//!
//! Key resolution: the XTEST path always asks the X server's own keymap
//! (`keysym_to_keycode`), so it follows the session layout. The uinput path
//! prefers the same keymap via XWayland (X keycodes are evdev keycodes + 8)
//! and falls back to a US-QWERTY table when X is unreachable.

use enigo::Key;
use evdev::KeyCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use x11rb::protocol::xproto::{
    BUTTON_PRESS_EVENT, BUTTON_RELEASE_EVENT, KEY_PRESS_EVENT, KEY_RELEASE_EVENT,
    MOTION_NOTIFY_EVENT,
};
use xkeysym::{key as xk, Keysym};

use crate::system_control::linux::uinput_dev::{qwerty_evdev_code, UinputDevices};
use crate::utils;

// X buttons: 1=left, 3=right; wheel is 4 (up) / 5 (down).
const BUTTON_LEFT: u8 = 1;
const BUTTON_WHEEL_UP: u8 = 4;
const BUTTON_WHEEL_DOWN: u8 = 5;

enum InputBackend {
    XTest,
    Uinput(UinputDevices),
}

impl InputBackend {
    /// Pick a working backend: on X11 prefer XTEST (exact absolute motion),
    /// then our own uinput devices; on Wayland go straight to uinput (XTEST
    /// is typically swallowed) and only try XTEST if `/dev/uinput` is
    /// inaccessible.
    fn select() -> anyhow::Result<Self> {
        let wayland_session = std::env::var("WAYLAND_DISPLAY")
            .map(|v| !v.is_empty())
            .unwrap_or(false);

        if !wayland_session && utils::xtest_available() && utils::xtest_effective() {
            log_info!(
                "输入后端: XTEST（X11 会话，绝对定位精确）",
                "input backend: XTEST (X11 session; exact absolute positioning)"
            );
            return Ok(Self::XTest);
        }

        match UinputDevices::create() {
            Ok(devices) => {
                // Let the compositor discover the new devices before navigation.
                utils::sleep(250);
                log_info!(
                    "输入后端: uinput（直连 /dev/uinput{}）",
                    "input backend: uinput (direct /dev/uinput{})",
                    if wayland_session {
                        "; Wayland"
                    } else {
                        "; XTEST ineffective"
                    }
                );
                return Ok(Self::Uinput(devices));
            },
            Err(uinput_err) => {
                if utils::xtest_available() && utils::xtest_effective() {
                    log_info!(
                        "输入后端: XTEST（uinput 不可用，已回退）",
                        "input backend: XTEST (uinput unavailable, falling back)"
                    );
                    return Ok(Self::XTest);
                }
                return Err(anyhow::anyhow!(
                    "Input unavailable: cannot create /dev/uinput devices ({uinput_err}) and XTEST injection is ineffective. Configure a udev rule granting your user access to /dev/uinput, reload it, and log in again, or use an X11 session. See docs/LINUX.md."
                ));
            },
        }
    }
}

/// Resolve a key to `(keysym, evdev code)`; `ch` carries the Layout character
/// for layout-aware resolution. Named keys map to fixed evdev codes.
fn key_to_codes(key: &Key) -> Result<Option<(Keysym, Option<KeyCode>, Option<char>)>, String> {
    // (keysym, fixed evdev code for named keys, layout char)
    Ok(Some(match key {
        Key::Layout(c) => {
            // xkb-compatible keysym derivation; chars without a keysym
            // (and no IME) cannot be typed.
            match Keysym::from_char(*c) {
                keysym if keysym.raw() != xk::VoidSymbol => (keysym, None, Some(*c)),
                _ => return Err(format!("cannot type character {c:?} (no keysym)")),
            }
        },
        Key::Return => (Keysym::Return, Some(KeyCode::KEY_ENTER), None),
        Key::Escape => (Keysym::Escape, Some(KeyCode::KEY_ESC), None),
        Key::Tab => (Keysym::Tab, Some(KeyCode::KEY_TAB), None),
        Key::Space => (Keysym::space, Some(KeyCode::KEY_SPACE), None),
        Key::Backspace => (Keysym::BackSpace, Some(KeyCode::KEY_BACKSPACE), None),
        Key::Delete => (Keysym::Delete, Some(KeyCode::KEY_DELETE), None),
        Key::Home => (Keysym::Home, Some(KeyCode::KEY_HOME), None),
        Key::End => (Keysym::End, Some(KeyCode::KEY_END), None),
        Key::PageUp => (Keysym::Page_Up, Some(KeyCode::KEY_PAGEUP), None),
        Key::PageDown => (Keysym::Page_Down, Some(KeyCode::KEY_PAGEDOWN), None),
        Key::LeftArrow => (Keysym::Left, Some(KeyCode::KEY_LEFT), None),
        Key::UpArrow => (Keysym::Up, Some(KeyCode::KEY_UP), None),
        Key::RightArrow => (Keysym::Right, Some(KeyCode::KEY_RIGHT), None),
        Key::DownArrow => (Keysym::Down, Some(KeyCode::KEY_DOWN), None),
        Key::Shift => (Keysym::Shift_L, Some(KeyCode::KEY_LEFTSHIFT), None),
        Key::Control => (Keysym::Control_L, Some(KeyCode::KEY_LEFTCTRL), None),
        Key::Alt => (Keysym::Alt_L, Some(KeyCode::KEY_LEFTALT), None),
        Key::Option => (Keysym::Alt_L, Some(KeyCode::KEY_LEFTALT), None),
        Key::Meta => (Keysym::Meta_L, Some(KeyCode::KEY_LEFTMETA), None),
        Key::CapsLock => (Keysym::Caps_Lock, Some(KeyCode::KEY_CAPSLOCK), None),
        Key::F1 => (Keysym::F1, Some(KeyCode::KEY_F1), None),
        Key::F2 => (Keysym::F2, Some(KeyCode::KEY_F2), None),
        Key::F3 => (Keysym::F3, Some(KeyCode::KEY_F3), None),
        Key::F4 => (Keysym::F4, Some(KeyCode::KEY_F4), None),
        Key::F5 => (Keysym::F5, Some(KeyCode::KEY_F5), None),
        Key::F6 => (Keysym::F6, Some(KeyCode::KEY_F6), None),
        Key::F7 => (Keysym::F7, Some(KeyCode::KEY_F7), None),
        Key::F8 => (Keysym::F8, Some(KeyCode::KEY_F8), None),
        Key::F9 => (Keysym::F9, Some(KeyCode::KEY_F9), None),
        Key::F10 => (Keysym::F10, Some(KeyCode::KEY_F10), None),
        Key::F11 => (Keysym::F11, Some(KeyCode::KEY_F11), None),
        Key::F12 => (Keysym::F12, Some(KeyCode::KEY_F12), None),
        _ => return Err(format!("unsupported key {key:?} on Linux")),
    }))
}

// ---------------------------------------------------------------------------
// Absolute pointer positioning (uinput)
// ---------------------------------------------------------------------------

/// Residual error (px) accepted before another positioning attempt is made.
const POINTER_TOLERANCE_PX: i32 = 2;
/// Residual error (px) above which the one-shot self-check warns instead of
/// logging at debug level.
const POINTER_WARN_PX: i32 = 8;
/// Pause between the corner slam and the target delta.
///
/// These must land in different compositor frames: pending relative motion is
/// coalesced into one event, so a delta sent back-to-back with the slam is
/// swallowed and the pointer stays in the corner. This was the whole reason the
/// scanner clicked into empty space on Wayland.
const POINTER_SLAM_SETTLE_MS: u32 = 40;
/// Settle time after an injected move before reading the position back.
const POINTER_SETTLE_MS: u32 = 12;
/// Polling step while waiting for the readback to catch up with injected motion.
const POINTER_READBACK_POLL_MS: u32 = 8;
/// Longest wait for the X11 pointer readback to follow one injected move.
///
/// The X server reports the pointer position a frame or two behind the
/// injected report, and a window that grabs the pointer can hold it longer. A
/// single fixed delay therefore either samples mid-flight or mistakes the lag
/// for a frozen readback — the latter used to abort the move, and with it the
/// whole job.
const POINTER_READBACK_TIMEOUT_MS: u32 = 80;
/// Attempts allowed for one move. The first one can be clamped at the screen
/// edge while the speed multiplier is still unknown, so a couple of refinements
/// are expected.
const POINTER_MAX_ATTEMPTS: u32 = 4;
/// Smallest requested axis delta that is still a usable sample for measuring
/// the compositor's pointer-speed multiplier.
const POINTER_MIN_SAMPLE_PX: i32 = 24;
/// Smallest requested delta whose absence of motion proves a stale readback.
const POINTER_MIN_STALE_DELTA_PX: i32 = 4;
/// Accepted range for a measured multiplier; anything else means the readback
/// is stale rather than that the session moves the pointer 100x.
const POINTER_SCALE_MIN: f32 = 0.2;
const POINTER_SCALE_MAX: f32 = 8.0;
/// Pointer-speed multiplier assumed before a live readback has measured it.
const POINTER_SCALE_FALLBACK: f32 = 1.0;

/// Whether absolute touchscreen clicks are enabled.
///
/// On by default; `GOODSCANNER_TOUCH_CLICK=0` falls back to moving the
/// relative pointer and clicking, should a compositor/game mishandle
/// synthesized touch.
fn touch_clicks_enabled() -> bool {
    std::env::var("GOODSCANNER_TOUCH_CLICK")
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Whether this process already reported its absolute-positioning self-check.
static POINTER_SELF_CHECK_REPORTED: AtomicBool = AtomicBool::new(false);
/// Whether this process already warned that the pointer readback is frozen.
static POINTER_FROZEN_REPORTED: AtomicBool = AtomicBool::new(false);

/// Positioning state of the uinput backend.
#[derive(Default)]
struct PointerState {
    /// Compositor pointer-speed multiplier learned from a live move.
    ///
    /// Relative pointer motion is multiplied by the session's pointer-speed
    /// setting (a "flat" profile applies a constant factor, 2.5x on the KDE
    /// session this was calibrated against), so deltas are divided by it.
    scale: Option<f32>,
    /// Last *verified* pointer position. `None` means the origin is unknown and
    /// the next move has to slam the pointer into the corner first.
    known: Option<(i32, i32)>,
}

/// Move the pointer to `(x, y)` in root coordinates.
///
/// The virtual *pointer* is built from `REL_X`/`REL_Y` — not because uinput
/// cannot do absolute motion (the touch device in `super::uinput_dev` is built
/// from `ABS_X`/`ABS_Y`), but because hover and the wheel need real pointer
/// semantics, and an absolute pointer's axis values are mapped onto the screen
/// by the compositor's device-to-output assignment plus output scale: a mapping
/// the client cannot set and that fails silently when wrong. So absolute
/// positioning here needs a known origin: the pointer is slammed into the
/// top-left corner once and every later move is a relative hop from the last
/// verified position. Three environment facts shape this:
///
/// 1. The slam must be separated from the following delta by a frame, otherwise
///    the compositor coalesces them and the delta is lost (pointer stays at 0,0).
/// 2. Relative motion is multiplied by the session's pointer-speed factor, so
///    the delta has to be divided by the learned factor.
/// 3. Slamming on *every* move would be slow and would jerk the pointer to the
///    corner and back in the middle of a scroll burst, where the game needs a
///    stable hover over the grid. So the slam is only used to (re)establish the
///    origin; everything else is a small relative hop, verified each time.
///
/// The X11 pointer position is the source of truth whenever it follows injected
/// motion. A readback that does not move for a non-trivial delta is stale (the
/// game can hold the pointer); the origin is then dropped so the next move
/// re-slams, and the situation is reported once.
///
/// `strict` decides what an unverified landing means. A move a click depends on
/// fails, so a click never lands somewhere else silently; a hover only gives up
/// quietly, because the click that follows goes through the absolute
/// touchscreen when one exists, and a few pixels of hover error never change
/// which widget receives the wheel or the click.
fn move_pointer_absolute(
    devices: &mut UinputDevices,
    x: i32,
    y: i32,
    state: &mut PointerState,
    strict: bool,
) -> anyhow::Result<()> {
    let mut last_pos: Option<(i32, i32)> = None;
    let mut stale_readback = false;
    let mut attempts_used = 0u32;

    for _ in 0..POINTER_MAX_ATTEMPTS {
        attempts_used += 1;
        let factor = state
            .scale
            .unwrap_or(POINTER_SCALE_FALLBACK)
            .clamp(POINTER_SCALE_MIN, POINTER_SCALE_MAX);
        let requested_x = ((x - state.known.unwrap_or((0, 0)).0) as f32 / factor).round() as i32;
        let requested_y = ((y - state.known.unwrap_or((0, 0)).1) as f32 / factor).round() as i32;
        // Distance still to cover, measured before the speed factor is applied:
        // a large residual that produced no motion is a stale readback, however
        // small the divided delta turned out to be.
        let residual_x = (x - state.known.unwrap_or((0, 0)).0).abs();
        let residual_y = (y - state.known.unwrap_or((0, 0)).1).abs();
        let origin = match state.known {
            // Relative hop from the position verified by the previous move: no
            // slam, so the game keeps its hover and the move costs one report.
            Some(known) => {
                if requested_x != 0 || requested_y != 0 {
                    devices.pointer_move_rel(requested_x, requested_y)?;
                }
                known
            },
            // Unknown origin: slam to the corner, then apply the target.
            None => {
                devices.pointer_slam()?;
                utils::sleep(POINTER_SLAM_SETTLE_MS);
                devices.pointer_move_rel(requested_x, requested_y)?;
                (0, 0)
            },
        };
        utils::sleep(POINTER_SETTLE_MS);

        let Some(pos) = read_pointer_settled(origin) else {
            break;
        };
        last_pos = Some(pos);

        if (pos.0 - x).abs() <= POINTER_TOLERANCE_PX && (pos.1 - y).abs() <= POINTER_TOLERANCE_PX {
            state.known = Some(pos);
            break;
        }
        if pos == origin
            && (residual_x >= POINTER_MIN_STALE_DELTA_PX
                || residual_y >= POINTER_MIN_STALE_DELTA_PX)
        {
            // A non-trivial delta produced no reported motion: the readback is
            // stale, so stop trusting it and re-slam on the next move.
            state.known = None;
            stale_readback = true;
            break;
        }
        // The readback is live: re-anchor on it (this absorbs a user moving the
        // physical mouse mid-scan) and refine the speed multiplier.
        if let Some(measured) =
            measure_pointer_scale(requested_x, requested_y, pos.0 - origin.0, pos.1 - origin.1)
        {
            state.scale = Some(measured);
        }
        state.known = Some(pos);
    }

    report_pointer_self_check(x, y, last_pos, state.scale, attempts_used);
    if stale_readback {
        report_frozen_readback();
    }
    pointer_move_outcome(x, y, last_pos, strict)
}

/// Decide whether a move may proceed after its attempts are spent.
///
/// Anything inside [`POINTER_WARN_PX`] is a usable landing for both a hover and
/// a click: the exact-positioning tolerance only decides when another attempt is
/// worth it, and [`report_pointer_self_check`] has already reported the error.
/// A hard failure here used to abort whole jobs over a handful of pixels, which
/// is why it is limited to genuinely unplaceable moves.
fn pointer_move_outcome(
    x: i32,
    y: i32,
    last_pos: Option<(i32, i32)>,
    strict: bool,
) -> anyhow::Result<()> {
    match last_pos.map(|p| (p.0 - x).abs().max((p.1 - y).abs())) {
        Some(off) if off <= POINTER_WARN_PX => Ok(()),
        // A hover never needs to be exact: the click that follows goes through
        // the absolute touchscreen, and a few pixels never change which widget
        // receives the wheel.
        _ if !strict => Ok(()),
        None => anyhow::bail!(
            "uinput pointer positioning could not be verified: the X11 pointer position is unreadable, so a click at ({x}, {y}) cannot be placed \
             / 无法校验 uinput 指针定位：读不到 X11 指针位置，点击 ({x}, {y}) 无法定位"
        ),
        Some(off) => anyhow::bail!(
            "uinput pointer positioning could not be verified; requested ({x}, {y}), observed {last_pos:?} ({off}px off) \
             / 无法校验 uinput 指针定位：目标 ({x}, {y})，实际 {last_pos:?}，偏差 {off}px"
        ),
    }
}

/// Read the pointer position once the readback has caught up with the motion.
///
/// The X server can report the pointer position a frame or two behind the
/// injected report, and a window that grabs the pointer can hold it longer, so a
/// single fixed delay either samples mid-flight or mistakes the lag for a frozen
/// readback. Poll until two consecutive samples agree and the position has left
/// `origin`, or until the timeout expires; the caller then decides whether a
/// motionless position means "stale readback".
fn read_pointer_settled(origin: (i32, i32)) -> Option<(i32, i32)> {
    let deadline = Instant::now() + Duration::from_millis(u64::from(POINTER_READBACK_TIMEOUT_MS));
    let mut previous = read_pointer()?;
    loop {
        utils::sleep(POINTER_READBACK_POLL_MS);
        let current = read_pointer()?;
        if current == previous && current != origin {
            return Some(current);
        }
        if Instant::now() >= deadline {
            return Some(current);
        }
        previous = current;
    }
}

/// Read the pointer position, or `None` when X11 is unavailable.
fn read_pointer() -> Option<(i32, i32)> {
    utils::query_pointer().ok().map(|p| (p.root_x, p.root_y))
}

/// Derive the compositor's pointer-speed multiplier from one measured move.
///
/// `origin` was the pointer position the move started from, so a live readback
/// reports `origin + delta * factor`.
fn measure_pointer_scale(dx: i32, dy: i32, observed_x: i32, observed_y: i32) -> Option<f32> {
    let (delta, observed) = if dx.abs() >= dy.abs() {
        (dx, observed_x)
    } else {
        (dy, observed_y)
    };
    if delta.abs() < POINTER_MIN_SAMPLE_PX || delta.signum() != observed.signum() {
        return None;
    }
    let measured = observed as f32 / delta as f32;
    (POINTER_SCALE_MIN..=POINTER_SCALE_MAX)
        .contains(&measured)
        .then_some(measured)
}

/// Report once that the X11 pointer does not follow injected motion.
///
/// A frozen readback means the pointer is probably not over the game window (or
/// the game holds the pointer), so clicks go somewhere else. Absolute
/// positioning cannot be verified in that state.
fn report_frozen_readback() {
    if POINTER_FROZEN_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    log_warn!(
        "[input] 无法回读鼠标位置：注入的移动没有反映到 X11 指针上。\
         若游戏窗口未置顶，点击会落到其它窗口；请把游戏窗口切到前台后重试。",
        "[input] The pointer position cannot be read back: injected motion never reached the X11 pointer. \
         If the game window is not in front, clicks land in another window; bring it to the foreground and retry.",
    );
}

/// Report the first absolute-positioning self-check of this process.
fn report_pointer_self_check(
    x: i32,
    y: i32,
    pos: Option<(i32, i32)>,
    scale: Option<f32>,
    attempts_used: u32,
) {
    if POINTER_SELF_CHECK_REPORTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let scale_text = match scale {
        Some(value) => format!("{value:.2}"),
        None => "?".to_string(),
    };
    match pos {
        Some(pos) if (pos.0 - x).abs().max((pos.1 - y).abs()) <= POINTER_WARN_PX => {
            log_debug!(
                "[input] uinput 定位自检：请求 ({}, {})，实际 ({}, {})，尝试 {} 次，指针速度倍率 {}",
                "[input] uinput positioning self-check: asked for ({}, {}), landed at ({}, {}), {} attempt(s), pointer-speed factor {}",
                x,
                y,
                pos.0,
                pos.1,
                attempts_used,
                scale_text
            );
        },
        Some(pos) => {
            log_warn!(
                "[input] uinput 定位自检未通过：请求 ({}, {})，实际 ({}, {})，偏差 {}px（指针速度倍率 {}）。\
                 系统设置 → 鼠标 里把指针速度调回中间值会让定位更准。",
                "[input] uinput positioning self-check failed: asked for ({}, {}), landed at ({}, {}), {}px off (pointer-speed factor {}). \
                 Moving the pointer-speed slider back to its centre in System Settings -> Mouse makes this exact.",
                x,
                y,
                pos.0,
                pos.1,
                (pos.0 - x).abs().max((pos.1 - y).abs()),
                scale_text
            );
        },
        None => {
            log_warn!(
                "[input] uinput 定位自检：无法读取鼠标位置，请求 ({}, {}) 已按指针速度倍率 {} 补偿。\
                 若点击落到错误位置，请把系统设置 → 鼠标 的指针速度调回中间值。",
                "[input] uinput positioning self-check: the pointer position is unreadable; ({}, {}) was injected with pointer-speed factor {}. \
                 If clicks land in the wrong place, move the pointer-speed slider back to its centre in System Settings -> Mouse.",
                x,
                y,
                scale_text
            );
        },
    }
}

pub struct LinuxControl {
    /// Lazily selected on first injected event; errors are surfaced there
    /// (the constructor mirrors the infallible Windows API).
    backend: Option<InputBackend>,
    backend_error: Option<String>,
    /// Absolute-positioning state (see [`move_pointer_absolute`]).
    pointer: PointerState,
}

impl LinuxControl {
    pub fn new() -> LinuxControl {
        LinuxControl {
            backend: None,
            backend_error: None,
            pointer: PointerState::default(),
        }
    }

    fn with_backend<T>(
        &mut self,
        f: impl FnOnce(&mut InputBackend) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if self.backend.is_none() {
            match InputBackend::select() {
                Ok(b) => self.backend = Some(b),
                Err(e) => {
                    if self.backend_error.is_none() {
                        self.backend_error = Some(format!("{e:#}"));
                    }
                    return Err(anyhow::Error::msg(
                        self.backend_error.clone().expect("just set"),
                    ));
                },
            }
        }
        f(self.backend.as_mut().expect("backend was just initialized"))
    }

    /// Validate input access before scan navigation can start.
    pub fn prepare(&mut self) -> anyhow::Result<()> {
        self.with_backend(|_| Ok(()))
    }

    fn mouse_button(&mut self, pressed: bool) -> anyhow::Result<()> {
        self.with_backend(|backend| match backend {
            InputBackend::XTest => utils::fake_input(
                if pressed {
                    BUTTON_PRESS_EVENT
                } else {
                    BUTTON_RELEASE_EVENT
                },
                BUTTON_LEFT,
                0,
                0,
            ),
            InputBackend::Uinput(devices) => devices.pointer_button(pressed),
        })
    }

    pub fn mouse_drag(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) -> anyhow::Result<()> {
        crate::system_control::gesture::drag(
            self,
            crate::system_control::gesture::DragTiming {
                steps: 12,
                move_settle_ms: 0,
                press_settle_ms: 0,
            },
            x0,
            y0,
            x1,
            y1,
        )
    }

    /// Move the pointer without clicking.
    ///
    /// Hover only: a landing that could not be verified to the exact tolerance is
    /// accepted (the self-check still reports it), because callers use this to
    /// put the pointer over a widget before a click that goes through the
    /// absolute touchscreen, and to aim the wheel. Failing here would abort a
    /// whole scan over a few pixels of hover error.
    pub fn mouse_move_to(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        self.move_pointer_to(x, y, false)
    }

    /// Place the pointer at `(x, y)`, optionally requiring a verified landing.
    fn move_pointer_to(&mut self, x: i32, y: i32, strict: bool) -> anyhow::Result<()> {
        // The positioning state is moved out so the backend borrow of `self`
        // does not overlap the field borrow.
        let mut pointer = std::mem::take(&mut self.pointer);
        let result = self.with_backend(|backend| match backend {
            InputBackend::XTest => utils::fake_input(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16),
            InputBackend::Uinput(devices) => {
                move_pointer_absolute(devices, x, y, &mut pointer, strict)
            },
        });
        self.pointer = pointer;
        result
    }

    pub fn mouse_click(&mut self) -> anyhow::Result<()> {
        self.with_backend(|backend| match backend {
            InputBackend::XTest => {
                // Hold briefly, mirroring WindowsSystemControl, so the game
                // registers the press.
                utils::fake_input(BUTTON_PRESS_EVENT, BUTTON_LEFT, 0, 0)?;
                utils::sleep(20);
                utils::fake_input(BUTTON_RELEASE_EVENT, BUTTON_LEFT, 0, 0)
            },
            InputBackend::Uinput(devices) => devices.pointer_click(),
        })
    }

    /// Scroll the wheel by `amount` detents; **positive scrolls down** (towards
    /// the end of a list).
    ///
    /// That is the convention callers get on the other platforms: enigo's
    /// Windows `mouse_scroll_y` negates the value before sending
    /// `MOUSEEVENTF_WHEEL` (`length * -120`, enigo 0.1.3), and macOS negates it
    /// explicitly. Getting this backwards is silent and nasty — every page turn
    /// scrolls the inventory *up* instead of down, so a scan past the first page
    /// re-reads the same page forever. REL_WHEEL's native sign is the opposite
    /// (positive = wheel up), hence the negations here.
    pub fn mouse_scroll(&mut self, amount: i32, _try_find: bool) -> anyhow::Result<()> {
        self.with_backend(|backend| match backend {
            InputBackend::XTest => {
                // X11 buttons: 4 = wheel up, 5 = wheel down.
                let button = if amount >= 0 {
                    BUTTON_WHEEL_DOWN
                } else {
                    BUTTON_WHEEL_UP
                };
                for _ in 0..amount.abs() {
                    utils::fake_input(BUTTON_PRESS_EVENT, button, 0, 0)?;
                    utils::fake_input(BUTTON_RELEASE_EVENT, button, 0, 0)?;
                }
                anyhow::Ok(())
            },
            InputBackend::Uinput(devices) => devices.wheel(-amount),
        })
    }

    pub fn mouse_scroll_wheel_delta(&mut self, delta: i32) -> anyhow::Result<()> {
        let notches = if delta.unsigned_abs() >= 120 {
            delta / 120
        } else if delta == 0 {
            0
        } else {
            delta.signum()
        };
        if notches != 0 {
            // Raw wheel delta follows WM_MOUSEWHEEL: positive means up.
            self.mouse_scroll(-notches, false)?;
        }
        anyhow::Ok(())
    }

    /// Click at absolute screen coordinates.
    ///
    /// Wayland/uinput: through the absolute touchscreen when it exists — the
    /// tap lands at exact coordinates with no acceleration and no display-scale
    /// conversion, so no pointer positioning is needed (and the relative
    /// pointer stays put, keeping wheel hovers stable). Otherwise the pointer
    /// is moved (slam/hop/verify) and the click follows.
    pub fn mouse_click_at(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        let mut pointer = std::mem::take(&mut self.pointer);
        let result = self.with_backend(|backend| match backend {
            InputBackend::XTest => {
                utils::fake_input(MOTION_NOTIFY_EVENT, 0, x as i16, y as i16)?;
                utils::sleep(10);
                utils::fake_input(BUTTON_PRESS_EVENT, BUTTON_LEFT, 0, 0)?;
                utils::sleep(20);
                utils::fake_input(BUTTON_RELEASE_EVENT, BUTTON_LEFT, 0, 0)
            },
            InputBackend::Uinput(devices) => {
                if devices.has_touch() && touch_clicks_enabled() {
                    let (screen_w, screen_h) = utils::screen_size().unwrap_or((1920, 1080));
                    devices.touch_tap(x, y, screen_w, screen_h)?;
                    // Let the game process the tap before the caller moves on.
                    utils::sleep(30);
                    Ok(())
                } else {
                    // The click lands wherever the pointer is, so an unverified
                    // position must fail instead of clicking elsewhere.
                    move_pointer_absolute(devices, x, y, &mut pointer, true)?;
                    utils::sleep(10);
                    devices.pointer_click()
                }
            },
        });
        self.pointer = pointer;
        result
    }

    pub fn key_press(&mut self, key: Key) -> anyhow::Result<()> {
        let Some((keysym, named_evdev, layout_char)) =
            key_to_codes(&key).map_err(anyhow::Error::msg)?
        else {
            return anyhow::Ok(());
        };

        self.with_backend(|backend| match backend {
            InputBackend::XTest => {
                let (keycode, needs_shift) = utils::keysym_to_keycode(keysym.raw())?;
                let shift = if needs_shift {
                    utils::keysym_to_keycode(xk::Shift_L)
                        .ok()
                        .map(|(code, _)| code)
                } else {
                    None
                };

                if let Some(shift_code) = shift {
                    utils::fake_input(KEY_PRESS_EVENT, shift_code, 0, 0)?;
                }
                utils::fake_input(KEY_PRESS_EVENT, keycode, 0, 0)?;
                utils::sleep(10);
                utils::fake_input(KEY_RELEASE_EVENT, keycode, 0, 0)?;
                if let Some(shift_code) = shift {
                    utils::fake_input(KEY_RELEASE_EVENT, shift_code, 0, 0)?;
                }
                anyhow::Ok(())
            },
            InputBackend::Uinput(devices) => {
                let resolved: Option<(u16, bool)> = match named_evdev {
                    Some(code) => Some((code.0, false)),
                    None => utils::keysym_to_keycode(keysym.raw())
                        .ok()
                        .filter(|(k, _)| *k >= 8)
                        .map(|(k, shift)| (u16::from(k) - 8, shift))
                        .or_else(|| {
                            layout_char.and_then(|ch| {
                                qwerty_evdev_code(ch.to_ascii_lowercase())
                                    .map(|kc| (kc.code(), ch.is_ascii_uppercase()))
                            })
                        }),
                };
                let Some((code, shift)) = resolved else {
                    return Err(anyhow::anyhow!("keysym {keysym:?} has no evdev keycode"));
                };
                if shift {
                    devices.key(KeyCode::KEY_LEFTSHIFT.0, true)?;
                }
                let result = (|| {
                    devices.key(code, true)?;
                    utils::sleep(10);
                    devices.key(code, false)
                })();
                let release = if shift {
                    devices.key(KeyCode::KEY_LEFTSHIFT.0, false)
                } else {
                    Ok(())
                };
                result.and(release)?;
                anyhow::Ok(())
            },
        })
    }
}

impl crate::system_control::gesture::DragDriver for LinuxControl {
    fn move_to(&mut self, x: i32, y: i32) -> anyhow::Result<()> {
        // A drag presses at the pointer position, so this is a click position in
        // all but name: an unverified landing must fail.
        self.move_pointer_to(x, y, true)
    }
    fn left_button(&mut self, pressed: bool) -> anyhow::Result<()> {
        self.mouse_button(pressed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hover that lands a few pixels off is a success, not a job-killing
    /// failure: this is the exact case reported from a live Wayland session
    /// (asked for (300, 500), landed at (300, 507) and aborted the whole manage
    /// job).
    #[test]
    fn small_residual_never_fails_a_move() {
        assert!(pointer_move_outcome(300, 500, Some((300, 507)), false).is_ok());
        assert!(pointer_move_outcome(300, 500, Some((300, 507)), true).is_ok());
        assert!(pointer_move_outcome(300, 500, Some((300, 500)), true).is_ok());
        // The documented self-check window itself is still accepted.
        assert!(pointer_move_outcome(300, 500, Some((308, 500)), true).is_ok());
    }

    /// A hover degrades gracefully when the readback is useless; a click must
    /// fail instead of clicking somewhere else.
    #[test]
    fn unverified_landing_only_fails_click_positions() {
        assert!(pointer_move_outcome(300, 500, None, false).is_ok());
        assert!(pointer_move_outcome(300, 500, None, true).is_err());
        assert!(pointer_move_outcome(300, 500, Some((380, 500)), false).is_ok());
        assert!(pointer_move_outcome(300, 500, Some((380, 500)), true).is_err());
    }

    /// The speed multiplier is only sampled from moves large enough to average
    /// out compositor rounding.
    #[test]
    fn pointer_scale_needs_a_large_same_direction_sample() {
        assert_eq!(measure_pointer_scale(300, 500, 375, 625), Some(1.25));
        assert_eq!(measure_pointer_scale(500, 300, 625, 375), Some(1.25));
        // Too small to sample.
        assert_eq!(measure_pointer_scale(10, 10, 12, 12), None);
        // Motion in the opposite direction means the readback is not ours.
        assert_eq!(measure_pointer_scale(300, 500, -375, -625), None);
    }
}
