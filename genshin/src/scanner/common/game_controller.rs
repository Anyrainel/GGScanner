use std::rc::Rc;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use image::RgbImage;
use yas::{log_debug, log_info, log_warn};

use yas::cancel::CancelToken;
use yas::capture::{CaptureMethod, Capturer};
use yas::game_info::GameInfo;
use yas::ocr::ImageToText;
use yas::positioning::Rect;
use yas::system_control::SystemControl;
use yas::utils;

use super::coord_scaler::CoordScaler;

/// Shared game controller for all Genshin scanners.
///
/// Consolidates game_info, scaler, capturer, and system_control into a single
/// struct with high-level methods for navigation, capture, OCR, and
/// YAS-style panel-load detection.
///
/// All coordinate parameters use the 1920x1080 base resolution and are
/// automatically scaled by the internal `CoordScaler`.
pub struct GenshinGameController {
    pub game_info: GameInfo,
    pub scaler: CoordScaler,
    pub capturer: Rc<dyn Capturer<RgbImage>>,
    pub system_control: SystemControl,

    pub(super) artifact_panel_waiter: super::artifact_panel::ArtifactPanelWaiter,

    /// Per-run cancellation token.
    cancel: CancelToken,
}

/// Squared Euclidean color distance between two RGB pixels.
pub fn color_distance(c1: &image::Rgb<u8>, c2: &image::Rgb<u8>) -> usize {
    let r = c1.0[0] as i32 - c2.0[0] as i32;
    let g = c1.0[1] as i32 - c2.0[1] as i32;
    let b = c1.0[2] as i32 - c2.0[2] as i32;
    (r * r + g * g + b * b) as usize
}

/// Per-byte capture noise ignored when comparing panel frames.
const PANEL_FRAME_NOISE_FLOOR: u8 = 6;
/// Mean per-byte excess above the noise floor allowed for "same frame".
const PANEL_FRAME_MAX_MEAN_EXCESS_DIFF: f64 = 0.20;
/// Hard guard against small averages hiding too many changed bytes.
const PANEL_FRAME_MAX_CHANGED_BYTE_RATIO: f64 = 0.01;
/// Byte deltas above this are counted by the ratio guard.
const PANEL_FRAME_CHANGED_BYTE_DIFF: u8 = 16;

pub(crate) fn panel_frames_similar(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    if a.is_empty() {
        return true;
    }

    let mut excess_diff_sum: u64 = 0;
    let mut changed_bytes: usize = 0;
    for (&left, &right) in a.iter().zip(b.iter()) {
        let diff = left.abs_diff(right);
        excess_diff_sum += diff.saturating_sub(PANEL_FRAME_NOISE_FLOOR) as u64;
        if diff > PANEL_FRAME_CHANGED_BYTE_DIFF {
            changed_bytes += 1;
        }
    }

    let len = a.len() as f64;
    let mean_excess_diff = excess_diff_sum as f64 / len;
    let changed_byte_ratio = changed_bytes as f64 / len;
    mean_excess_diff <= PANEL_FRAME_MAX_MEAN_EXCESS_DIFF
        && changed_byte_ratio <= PANEL_FRAME_MAX_CHANGED_BYTE_RATIO
}

impl GenshinGameController {
    pub fn new(game_info: GameInfo, capture_method: CaptureMethod) -> Result<Self> {
        let window_size = game_info.window.to_rect_usize().size();
        let scaler = CoordScaler::new(window_size.width as u32, window_size.height as u32);
        let hdr_mode = super::pixel_profile::is_hdr_mode();
        #[cfg(target_os = "windows")]
        match (capture_method, hdr_mode) {
            (CaptureMethod::BitBlt, false) => log_info!(
                "截图方式: BitBlt（非HDR模式，使用低开销GDI截图）",
                "Capture method: BitBlt (non-HDR mode; using low-overhead GDI capture)"
            ),
            (CaptureMethod::Wgc, true) => log_info!(
                "截图方式: WGC（HDR模式，使用色调映射后的截图）",
                "Capture method: WGC (HDR mode; using tone-mapped capture)"
            ),
            (method, true) => log_info!(
                "截图方式: {:?}（HDR模式，显式使用该截图方式）",
                "Capture method: {:?} (HDR mode; explicitly requested)",
                method
            ),
            (method, false) => log_info!(
                "截图方式: {:?}（非HDR模式，显式使用该截图方式）",
                "Capture method: {:?} (non-HDR mode; explicitly requested)",
                method
            ),
        }

        let capturer = yas::capture::create_capturer(
            game_info.native_window_handle(),
            capture_method,
            hdr_mode,
            super::pixel_profile::hdr_white_point(),
        )?;
        let system_control = yas::system_control::prepared_control()?;
        #[cfg(target_os = "linux")]
        log_info!("截图方式: X11/XWayland", "Capture method: X11/XWayland");
        Ok(Self {
            game_info,
            scaler,
            capturer,
            system_control,
            artifact_panel_waiter: Default::default(),
            cancel: CancelToken::new(),
        })
    }
}

// Cancellation methods.
impl GenshinGameController {
    /// Set the cancellation token for this run.
    pub fn set_cancel_token(&mut self, token: CancelToken) {
        self.cancel = token;
    }

    /// Get a clone of the current cancellation token.
    pub fn cancel_token(&self) -> CancelToken {
        self.cancel.clone()
    }

    /// Check if the current run has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// Check RMB and cancel if pressed. Returns true if cancelled.
    pub fn check_rmb(&self) -> bool {
        #[cfg(target_os = "linux")]
        if !utils::is_window_handle_valid(self.game_info.window_id) {
            self.cancel.cancel(yas::cancel::StopReason::GameLost);
        }
        self.cancel.check_rmb()
    }
}

// Focus methods.
impl GenshinGameController {
    /// Apply a freshly queried client rect to the controller state (shared by
    /// the per-platform refresh helpers).
    fn apply_window_rect(&mut self, result: anyhow::Result<yas::positioning::Rect<i32>>) {
        match result {
            Ok(rect) if rect.width > 0 && rect.height > 0 => {
                if rect != self.game_info.window {
                    log_debug!(
                        "[controller] 更新游戏窗口区域: {:?} -> {:?}",
                        "[controller] refreshed game window rect: {:?} -> {:?}",
                        self.game_info.window,
                        rect
                    );
                    self.game_info.window = rect;
                    self.scaler = CoordScaler::new(rect.width as u32, rect.height as u32);
                }
            },
            Ok(rect) => {
                log_warn!(
                    "[controller] 游戏窗口区域无效: {:?}",
                    "[controller] game window rect is invalid: {:?}",
                    rect
                );
            },
            Err(e) => {
                log_warn!(
                    "[controller] 无法刷新游戏窗口区域: {}",
                    "[controller] cannot refresh game window rect: {}",
                    e
                );
            },
        }
    }

    fn focus_window(&mut self, window: yas::window::NativeWindow, settle_ms: u32) -> bool {
        use yas::window::FocusRequest;
        if !window.is_valid() {
            return false;
        }
        match window.request_focus(false) {
            Ok(request) => {
                self.apply_window_rect(window.client_rect());
                if request == FocusRequest::Denied {
                    yas::log_error!(
                        "游戏窗口已恢复但未获得前台焦点；请手动点击游戏窗口后重试",
                        "Game window was restored but did not become foreground; click the game window and retry"
                    );
                    return false;
                }
                if settle_ms > 0 {
                    utils::sleep(settle_ms);
                }
                request == FocusRequest::Granted || window.is_foreground()
            },
            Err(e) => {
                log_warn!(
                    "[controller] 切换游戏窗口焦点失败: {}",
                    "[controller] failed to focus game window: {}",
                    e
                );
                false
            },
        }
    }

    /// Focus the selected native game window.
    /// Ensures subsequent keyboard events go to Genshin, not the terminal.
    ///
    /// If the window isn't found, logs an error and returns WITHOUT moving
    /// the mouse. Clicking/typing into whatever window is currently focused
    /// was previously masquerading as "navigation" and caused random input
    /// to unrelated apps.
    pub fn focus_game_window(&mut self) {
        self.focus_game_window_settle(500);
    }

    /// Same as [`Self::focus_game_window`] without the 500ms settle. Wheel
    /// bursts must re-focus after the scanner console steals foreground, but
    /// that sleep also drops Genshin's scrollbar hover.
    pub fn focus_game_window_quick(&mut self) {
        self.focus_game_window_settle(0);
    }

    fn focus_game_window_settle(&mut self, settle_ms: u32) {
        let selected = self.game_info.native_window();
        if self.focus_window(selected, settle_ms) {
            return;
        }
        let window_names = ["原神", "Genshin Impact"];
        if let Ok(windows) = yas::window::enumerate() {
            for (window, title) in windows {
                if window == selected || !window_names.contains(&title.trim()) {
                    continue;
                }
                if self.focus_window(window, settle_ms) {
                    self.game_info.set_native_window(window);
                    // X11 capture is bound to the selected client. Rebind it
                    // if the game recreated its window, rather than capturing a stale id.
                    #[cfg(target_os = "linux")]
                    match yas::capture::create_capturer(
                        window.0,
                        CaptureMethod::BitBlt,
                        false,
                        super::pixel_profile::hdr_white_point(),
                    ) {
                        Ok(capturer) => self.capturer = capturer,
                        Err(_) => self.cancel.cancel(yas::cancel::StopReason::GameLost),
                    }
                    return;
                }
            }
        }
        #[cfg(target_os = "linux")]
        self.cancel.cancel(yas::cancel::StopReason::GameLost);
        yas::log_error!(
            "游戏窗口不可用或无法获得焦点，请点击游戏窗口后重试。",
            "Game window unavailable or focus denied; click the game window and retry."
        );
    }
}

// Return to main UI — adapted from BetterGenshinImpact's ReturnMainUiTask.
// Press Escape one at a time, verify after each press, loop up to 8 times.
impl GenshinGameController {
    /// Check if the game appears to be in the main world (HUD visible, no menu open).
    ///
    /// Detects the Paimon icon button in the top-left corner. In the main world,
    /// this area contains the bright Paimon icon. In any menu, it's covered by
    /// the menu's dark background or header.
    ///
    /// Uses pixel brightness sampling — not as robust as template matching but
    /// sufficient for the return-to-main-UI loop.
    pub fn is_likely_main_world(&self) -> bool {
        let image = match self.capture_game() {
            Ok(img) => img,
            Err(_) => return false,
        };

        super::annotator::record_region(
            "main_world",
            &image,
            (30.0, 20.0, 60.0, 70.0),
            &self.scaler,
        );
        // The Paimon icon at 1920x1080 is a bright white/cream circular button
        // centered around (58, 50) with radius ~25px.
        // Sample several points across the icon face area.
        let check_points: &[(f64, f64)] = &[
            (62.0, 51.0), // Center of icon face
            (53.0, 47.0), // Inner-left
            (49.0, 35.0), // Upper portion
            (55.0, 70.0), // Lower portion
            (67.0, 77.0), // Lower-right
        ];

        let mut bright_count = 0;
        for &(bx, by) in check_points {
            let x = self.scaler.x(bx) as u32;
            let y = self.scaler.y(by) as u32;
            if x < image.width() && y < image.height() {
                let p = image.get_pixel(x, y);
                let brightness = (p[0] as u32 + p[1] as u32 + p[2] as u32) / 3;
                if brightness > 160 {
                    bright_count += 1;
                }
            }
        }

        bright_count >= 3
    }

    /// Return to the main world UI by pressing Escape one at a time and verifying.
    ///
    /// Adapted from BetterGenshinImpact's ReturnMainUiTask strategy:
    /// 1. Check if already in main UI — if so, return immediately.
    /// 2. Loop up to `max_attempts` times: press Escape, wait, check.
    /// 3. Final fallback: press Enter (dismiss dialogs) then Escape.
    ///
    /// Returns true if main UI was detected, false if still uncertain.
    pub fn return_to_main_ui(&mut self, max_attempts: u32) -> bool {
        if self.is_likely_main_world() {
            log_debug!(
                "[return_to_main_ui] 已在主界面",
                "[return_to_main_ui] already in main world"
            );
            return true;
        }

        for i in 0..max_attempts {
            if self.check_rmb() {
                return false;
            }
            self.key_press(enigo::Key::Escape);
            utils::sleep(900);

            if self.is_likely_main_world() {
                log_debug!(
                    "[return_to_main_ui] 按{}次Escape后到达主界面",
                    "[return_to_main_ui] reached main world after {} Escape(s)",
                    i + 1
                );
                return true;
            }
        }

        if self.check_rmb() {
            return false;
        }

        // Fallback: Enter (dismiss any stuck dialog) + Escape
        log_debug!(
            "[return_to_main_ui] 回退策略: Enter + Escape",
            "[return_to_main_ui] fallback: Enter + Escape"
        );
        self.key_press(enigo::Key::Return);
        utils::sleep(500);
        self.key_press(enigo::Key::Escape);
        utils::sleep(900);

        let result = self.is_likely_main_world();
        if result {
            log_debug!(
                "[return_to_main_ui] 回退后到达主界面",
                "[return_to_main_ui] reached main world after fallback"
            );
        } else {
            log_warn!(
                "[return_to_main_ui] 尝试{}次+回退后可能仍未在主界面",
                "[return_to_main_ui] may not be in main world after {} attempts + fallback",
                max_attempts
            );
        }
        result
    }
}

// Navigation methods — all coordinates at 1920x1080 base, scaled by CoordScaler.
impl GenshinGameController {
    /// Click at a position specified in base 1920x1080 coordinates.
    ///
    /// Delegates to the platform backend: on Linux/Wayland the click may go
    /// through the absolute touchscreen device (exact coordinates, no pointer
    /// positioning), everything else moves the pointer and clicks.
    pub fn click_at(&mut self, base_x: f64, base_y: f64) {
        let x = self.game_info.window.left as f64 + self.scaler.scale_x(base_x);
        let y = self.game_info.window.top as f64 + self.scaler.scale_y(base_y);
        self.system_control
            .mouse_click_at(x as i32, y as i32)
            .unwrap();
    }

    /// Move mouse to a position specified in base 1920x1080 coordinates.
    pub fn move_to(&mut self, base_x: f64, base_y: f64) {
        let x = self.game_info.window.left as f64 + self.scaler.scale_x(base_x);
        let y = self.game_info.window.top as f64 + self.scaler.scale_y(base_y);
        self.system_control
            .mouse_move_to(x as i32, y as i32)
            .unwrap();
    }

    /// Press a keyboard key.
    pub fn key_press(&mut self, key: enigo::Key) {
        self.system_control.key_press(key).unwrap();
    }

    /// Scroll the mouse wheel by `amount` detents (enigo multiplies by 120).
    pub fn mouse_scroll(&mut self, amount: i32) {
        self.system_control.mouse_scroll(amount, false).unwrap();
    }

    /// Scroll by a raw wheel delta. 120 is one detent.
    pub fn mouse_scroll_wheel_delta(&mut self, delta: i32) {
        #[cfg(target_os = "windows")]
        self.post_wheel_to_game(delta);
        self.system_control.mouse_scroll_wheel_delta(delta).unwrap();
    }

    /// Genshin's UI often ignores injected `SendInput` wheel. `WM_MOUSEWHEEL`
    /// posted to the game hwnd still uses the cursor position in `lParam`.
    #[cfg(target_os = "windows")]
    fn post_wheel_to_game(&self, delta: i32) {
        use windows_sys::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            GetCursorPos, PostMessageW, WM_MOUSEWHEEL,
        };

        let hwnd = self.game_info.hwnd as HWND;
        if hwnd.is_null() {
            return;
        }
        let mut pt = POINT { x: 0, y: 0 };
        unsafe {
            if GetCursorPos(&mut pt) == 0 {
                return;
            }
            let wparam = ((delta as i16 as u16 as u32) << 16) as WPARAM;
            let lparam = ((pt.y as u16 as u32) << 16 | (pt.x as u16 as u32)) as LPARAM;
            let _ = PostMessageW(hwnd, WM_MOUSEWHEEL, wparam, lparam);
        }
    }
}

// Capture and OCR methods.
impl GenshinGameController {
    /// Capture the full game window.
    pub fn capture_game(&self) -> Result<RgbImage> {
        self.capturer.capture_rect(self.game_info.window)
    }

    /// Capture a sub-region of the game window.
    /// Coordinates are in base 1920x1080 and will be scaled.
    pub fn capture_region(
        &self,
        base_x: f64,
        base_y: f64,
        base_w: f64,
        base_h: f64,
    ) -> Result<RgbImage> {
        let rect = Rect {
            left: self.scaler.scale_x(base_x) as i32,
            top: self.scaler.scale_y(base_y) as i32,
            width: self.scaler.scale_x(base_w) as i32,
            height: self.scaler.scale_y(base_h) as i32,
        };
        self.capturer
            .capture_relative_to(rect, self.game_info.window.origin())
    }

    /// OCR a region and return trimmed text.
    /// Coordinates are in base 1920x1080 and will be scaled.
    pub fn ocr_region(
        &self,
        ocr_model: &dyn ImageToText<RgbImage>,
        rect: (f64, f64, f64, f64),
    ) -> Result<String> {
        let (x, y, w, h) = rect;
        let im = self.capture_region(x, y, w, h)?;
        let text = ocr_model.image_to_text(&im, false)?;
        Ok(text.trim().to_string())
    }

    /// OCR a region with Y-offset support (for elixir artifacts, etc).
    pub fn ocr_region_shifted(
        &self,
        ocr_model: &dyn ImageToText<RgbImage>,
        rect: (f64, f64, f64, f64),
        y_shift: f64,
    ) -> Result<String> {
        let (x, y, w, h) = rect;
        self.ocr_region(ocr_model, (x, y + y_shift, w, h))
    }
}

// Screenshot save helpers.
impl GenshinGameController {
    /// Save the full game window as a PNG file.
    pub fn save_screenshot(&self, path: &str) -> Result<()> {
        let im = self.capture_game()?;
        im.save(path)
            .map_err(|e| anyhow!("截图保存失败 / Failed to save screenshot: {}", e))?;
        log_debug!(
            "[screenshot] 已保存完整截图: {}",
            "[screenshot] saved full: {}",
            path
        );
        Ok(())
    }

    /// Save a sub-region of the game window as a PNG file.
    /// Coordinates are in base 1920x1080 and will be scaled.
    pub fn save_region_screenshot(
        &self,
        path: &str,
        base_x: f64,
        base_y: f64,
        base_w: f64,
        base_h: f64,
    ) -> Result<()> {
        let im = self.capture_region(base_x, base_y, base_w, base_h)?;
        im.save(path)
            .map_err(|e| anyhow!("截图保存失败 / Failed to save screenshot: {}", e))?;
        log_debug!(
            "[screenshot] 已保存区域截图({},{},{},{}) -> {}",
            "[screenshot] saved region ({},{},{},{}) -> {}",
            base_x,
            base_y,
            base_w,
            base_h,
            path
        );
        Ok(())
    }
}

// Panel-load detection.
impl GenshinGameController {
    /// Ensure the panel region is stable (two captures one frame apart match).
    /// Used after a fixed delay — doesn't require content to differ from previous.
    pub fn ensure_panel_stable(
        &mut self,
        pool_rect: (f64, f64, f64, f64),
        timeout_ms: u64,
    ) -> Result<()> {
        let now = SystemTime::now();
        let (px, py, pw, ph) = pool_rect;
        let rect = Rect {
            left: self.scaler.scale_x(px) as i32,
            top: self.scaler.scale_y(py) as i32,
            width: self.scaler.scale_x(pw) as i32,
            height: self.scaler.scale_y(ph) as i32,
        };

        let mut last_capture: Vec<u8> = Vec::new();
        let mut capture_count: u32 = 0;

        while now.elapsed().unwrap().as_millis() < timeout_ms as u128 {
            let cap_start = SystemTime::now();
            let im = self
                .capturer
                .capture_relative_to(rect, self.game_info.window.origin())?;
            capture_count += 1;
            super::annotator::record_input("panel_stability", &im, "panel comparison input", false);
            let raw = im.into_raw();

            if panel_frames_similar(&raw, &last_capture) && capture_count > 1 {
                let wait_ms = now.elapsed().unwrap().as_millis();
                log_debug!(
                    "[controller] 面板稳定(等待{}ms, 截图{}次)",
                    "[controller] panel stable (wait {}ms, {} caps)",
                    wait_ms,
                    capture_count
                );
                return Ok(());
            }
            last_capture = raw;
            let cap_ms = cap_start.elapsed().unwrap().as_millis() as u32;
            if cap_ms < 18 {
                utils::sleep(18 - cap_ms);
            }
        }

        log_debug!(
            "[controller] 面板稳定超时({}ms, 截图{}次)",
            "[controller] panel stable timed out ({}ms, {} caps)",
            timeout_ms,
            capture_count
        );
        Ok(())
    }

    /// Capture the color of a single pixel at base 1920x1080 coordinates.
    /// Used for scroll flag detection.
    pub fn get_flag_color(&self, flag_x: f64, flag_y: f64) -> Result<image::Rgb<u8>> {
        let pos = yas::positioning::Pos {
            x: self.game_info.window.left + self.scaler.scale_x(flag_x) as i32,
            y: self.game_info.window.top + self.scaler.scale_y(flag_y) as i32,
        };
        let result = self.capturer.capture_color(pos);
        if super::annotator::is_enabled() {
            if let Ok(color) = &result {
                super::annotator::record_input(
                    "scroll_flag",
                    &RgbImage::from_pixel(1, 1, *color),
                    &format!("({flag_x}, {flag_y}): {color:?}"),
                    false,
                );
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_frames_similar_accepts_small_capture_noise() {
        let first = vec![100u8; 3_000];
        let mut second = first.clone();
        for value in second.iter_mut().take(200) {
            *value += PANEL_FRAME_NOISE_FLOOR;
        }

        assert!(panel_frames_similar(&first, &second));
    }

    #[test]
    fn panel_frames_similar_rejects_real_panel_text_changes() {
        let first = vec![100u8; 3_000];
        let mut second = first.clone();
        for value in second.iter_mut().take(40) {
            *value = 180;
        }

        assert!(!panel_frames_similar(&first, &second));
    }

    #[test]
    fn panel_frames_similar_rejects_dimension_changes() {
        assert!(!panel_frames_similar(&[1, 2, 3], &[1, 2]));
    }
}
