//! Click one artifact and poll its right-hand detail panel for OCR.
use std::time::{Duration, Instant};

use anyhow::Result;

use super::capture_frame::CaptureFrame;
use super::constants::{
    ARTIFACT_DETAIL_PANEL_RECT, GRID_FIRST_X, GRID_FIRST_Y, GRID_OFFSET_X, GRID_OFFSET_Y,
};
use super::coord_scaler::CoordScaler;
use super::game_controller::{panel_frames_similar, GenshinGameController};

const FRAME_INTERVAL: Duration = Duration::from_millis(17);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArtifactCell {
    pub row: usize,
    pub col: usize,
}

#[derive(Clone, Copy)]
pub struct ArtifactPanelOptions {
    pub timeout_ms: u64,
    pub initial_wait_ms: u64,
    pub extra_delay_ms: u64,
}

pub enum ArtifactSelection {
    Ready(CaptureFrame),
    Cancelled,
}

/// Sample the text fields at both possible layout offsets, excluding animated icons.
#[derive(Clone, Debug)]
pub(crate) struct ArtifactPanelSignature {
    samples: Vec<u8>,
    readable: bool,
}

// A few contrasting strokes distinguish text from a blank or fading panel.
fn has_text(samples: &[u8]) -> bool {
    samples
        .chunks_exact(3)
        .zip(samples.chunks_exact(3).skip(1))
        .filter(|(a, b)| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 35))
        .take(6)
        .count()
        == 6
}

impl ArtifactPanelSignature {
    pub(crate) fn from_frame(frame: &CaptureFrame, scaler: &CoordScaler) -> Self {
        let mut samples = Vec::with_capacity(150 * 1024);
        let mut readable = true;
        for (x, y, width, height, require_text) in [
            (1327, 120, 430, 40, false),  // name
            (1327, 184, 155, 40, true),   // slot
            (1327, 266, 185, 100, true),  // main stat
            (1330, 420, 110, 83, true),   // level
            (1356, 478, 355, 230, false), // substats
            (1330, 710, 360, 150, false), // set
            (1386, 905, 315, 50, false),  // equipped character
            (1508, 410, 36, 35, false),   // elixir banner
        ] {
            let start = samples.len();
            for by in (y..y + height).step_by(2) {
                for bx in (x..x + width).step_by(2) {
                    samples.extend(frame.pixel(scaler, bx as f64, by as f64));
                }
            }
            if require_text {
                readable &= has_text(&samples[start..]);
            }
        }
        Self { samples, readable }
    }

    pub(crate) fn similar(&self, other: &Self) -> bool {
        self.samples.len() == other.samples.len()
            // Small tiles keep a changed digit from disappearing in a large average.
            && self.samples.chunks(192).zip(other.samples.chunks(192)).all(|(a, b)| panel_frames_similar(a, b))
    }
}

trait PanelIo {
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
    fn cancelled(&self) -> bool;
    fn click(&mut self, cell: ArtifactCell);
    fn capture_detail(&mut self) -> Result<(CaptureFrame, ArtifactPanelSignature)>;
}

#[derive(Default)]
pub struct ArtifactPanelWaiter {
    last: Option<(ArtifactCell, ArtifactPanelSignature)>,
}

impl ArtifactPanelWaiter {
    fn select(
        &mut self,
        io: &mut impl PanelIo,
        cell: ArtifactCell,
        options: ArtifactPanelOptions,
    ) -> Result<ArtifactSelection> {
        // An interrupted operation leaves no cached baseline for the next click.
        let cached = self.last.take();
        if io.cancelled() {
            return Ok(ArtifactSelection::Cancelled);
        }
        let baseline = match cached {
            // The caller invalidates the cache after page navigation. Refresh
            // the pixels (including the lock icon) on a same-cell revisit.
            Some((previous_cell, _)) if previous_cell == cell => {
                let (frame, signature) = io.capture_detail()?;
                if io.cancelled() {
                    return Ok(ArtifactSelection::Cancelled);
                }
                self.last = Some((cell, signature));
                return Ok(ArtifactSelection::Ready(frame));
            },
            Some((_, signature)) => signature,
            None => io.capture_detail()?.1,
        };
        if io.cancelled() {
            return Ok(ArtifactSelection::Cancelled);
        }
        io.click(cell);
        let start = io.now();
        let timeout = Duration::from_millis(options.timeout_ms);
        let deadline = start + timeout;
        let hold = FRAME_INTERVAL.max(Duration::from_millis(options.extra_delay_ms));
        let mut next_capture = start + Duration::from_millis(options.initial_wait_ms).min(timeout);
        let mut previous: Option<(ArtifactPanelSignature, Duration)> = None;
        loop {
            let now = io.now();
            if next_capture > now {
                io.sleep(next_capture - now);
            }
            if io.cancelled() {
                return Ok(ArtifactSelection::Cancelled);
            }
            let capture_started = io.now();
            let (frame, signature) = io.capture_detail()?;
            if io.cancelled() {
                return Ok(ArtifactSelection::Cancelled);
            }
            let observed = io.now();
            let stable_since = previous
                .as_ref()
                .filter(|(previous, _)| previous.readable && previous.similar(&signature))
                .map(|(_, since)| *since)
                .unwrap_or(observed);
            let settled = observed <= deadline
                && signature.readable
                && observed - stable_since >= hold
                && !baseline.similar(&signature);
            // A slow read can return after T with pixels sampled before T.
            // For the timeout fallback, start the final capture at or after T.
            if settled || capture_started >= deadline {
                yas::log_debug!(
                    "[artifact] 详情采样 {}ms (行={}, 列={}, 变化已稳定={})",
                    "[artifact] Detail sampling took {}ms (row={}, col={}, changed and stable={})",
                    (observed - start).as_millis(),
                    cell.row,
                    cell.col,
                    settled
                );
                self.last = Some((cell, signature));
                return Ok(ArtifactSelection::Ready(frame));
            }
            previous = Some((signature, stable_since));
            next_capture = (capture_started + FRAME_INTERVAL).min(deadline);
        }
    }
}

struct ControllerIo<'a> {
    ctrl: &'a mut GenshinGameController,
    start: Instant,
}

impl PanelIo for ControllerIo<'_> {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
    fn cancelled(&self) -> bool {
        self.ctrl.check_rmb()
    }
    fn click(&mut self, cell: ArtifactCell) {
        self.ctrl.click_at(
            GRID_FIRST_X + cell.col as f64 * GRID_OFFSET_X,
            GRID_FIRST_Y + cell.row as f64 * GRID_OFFSET_Y,
        );
    }
    fn capture_detail(&mut self) -> Result<(CaptureFrame, ArtifactPanelSignature)> {
        let frame = CaptureFrame::from_region(self.ctrl, ARTIFACT_DETAIL_PANEL_RECT)?;
        let signature = ArtifactPanelSignature::from_frame(&frame, &self.ctrl.scaler);
        Ok((frame, signature))
    }
}

impl GenshinGameController {
    pub fn select_artifact(
        &mut self,
        cell: ArtifactCell,
        options: ArtifactPanelOptions,
    ) -> Result<ArtifactSelection> {
        let mut waiter = std::mem::take(&mut self.artifact_panel_waiter);
        let result = waiter.select(
            &mut ControllerIo {
                ctrl: self,
                start: Instant::now(),
            },
            cell,
            options,
        );
        self.artifact_panel_waiter = waiter;
        result
    }

    pub fn reset_artifact_panel(&mut self) {
        self.artifact_panel_waiter = ArtifactPanelWaiter::default();
    }

    pub(crate) fn last_artifact_signature(&self) -> Option<&ArtifactPanelSignature> {
        self.artifact_panel_waiter
            .last
            .as_ref()
            .map(|(_, signature)| signature)
    }

    pub(crate) fn last_artifact_cell(&self) -> Option<ArtifactCell> {
        self.artifact_panel_waiter
            .last
            .as_ref()
            .map(|(cell, _)| *cell)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};

    const CELL: ArtifactCell = ArtifactCell { row: 3, col: 4 };
    const OPTIONS: ArtifactPanelOptions = ArtifactPanelOptions {
        timeout_ms: 100,
        initial_wait_ms: 20,
        extra_delay_ms: 0,
    };

    fn signature(value: u8) -> ArtifactPanelSignature {
        ArtifactPanelSignature {
            samples: vec![value; 384],
            readable: value != 0,
        }
    }

    // Each scheduled value describes the detail visible after the click.
    struct ScriptIo {
        time: Duration,
        clicked_at: Option<Duration>,
        steps: Vec<(u64, u8)>,
        capture_ms: u64,
        captures: usize,
        cancel_at: Option<Duration>,
        fail_at: Option<usize>,
    }

    impl ScriptIo {
        fn new(steps: Vec<(u64, u8)>) -> Self {
            Self {
                time: Duration::ZERO,
                clicked_at: None,
                steps,
                capture_ms: 0,
                captures: 0,
                cancel_at: None,
                fail_at: None,
            }
        }

        fn elapsed_after_click(&self) -> u64 {
            (self.time - self.clicked_at.unwrap()).as_millis() as u64
        }

        fn select(&mut self, options: ArtifactPanelOptions) -> CaptureFrame {
            match ArtifactPanelWaiter::default()
                .select(self, CELL, options)
                .unwrap()
            {
                ArtifactSelection::Ready(frame) => frame,
                ArtifactSelection::Cancelled => panic!("unexpected cancellation"),
            }
        }
    }

    impl PanelIo for ScriptIo {
        fn now(&self) -> Duration {
            self.time
        }

        fn sleep(&mut self, duration: Duration) {
            self.time += duration;
        }

        fn cancelled(&self) -> bool {
            self.cancel_at.is_some_and(|at| self.time >= at)
        }

        fn click(&mut self, cell: ArtifactCell) {
            assert_eq!(cell, CELL);
            assert!(self.clicked_at.replace(self.time).is_none(), "extra click");
        }

        fn capture_detail(&mut self) -> Result<(CaptureFrame, ArtifactPanelSignature)> {
            self.captures += 1;
            anyhow::ensure!(self.fail_at != Some(self.captures), "capture failed");
            let value = self
                .clicked_at
                .and_then(|_| {
                    self.steps
                        .iter()
                        .rev()
                        .find(|(at, _)| *at <= self.elapsed_after_click())
                })
                .map_or(40, |(_, value)| *value);
            self.time += Duration::from_millis(self.capture_ms);
            Ok((
                CaptureFrame {
                    image: RgbImage::from_pixel(1, 1, Rgb([value, self.captures as u8, 0])),
                    origin: (ARTIFACT_DETAIL_PANEL_RECT.0, ARTIFACT_DETAIL_PANEL_RECT.1),
                },
                signature(value),
            ))
        }
    }

    #[test]
    fn accepts_the_final_changed_stable_crop_after_one_click() {
        let mut io = ScriptIo::new(vec![(34, 70), (50, 110)]);
        let frame = io.select(OPTIONS);
        assert_eq!(io.elapsed_after_click(), 71);
        assert_eq!(frame.image.get_pixel(0, 0).0, [110, io.captures as u8, 0]);
        assert!(!frame.is_full_window());
    }

    #[test]
    fn stable_4k_detail_with_16ms_region_captures_finishes_in_53ms() {
        let mut io = ScriptIo::new(vec![(0, 90)]);
        io.capture_ms = 16;
        io.select(OPTIONS);
        assert_eq!(io.elapsed_after_click(), 53);
        assert_eq!(io.captures, 3); // pre-click baseline and two matching changed details
    }

    #[test]
    fn identical_details_wait_until_timeout_without_reselecting() {
        let mut io = ScriptIo::new(vec![(0, 40)]);
        let frame = io.select(OPTIONS);
        assert_eq!(io.elapsed_after_click(), OPTIONS.timeout_ms);
        assert_eq!(frame.image.get_pixel(0, 0).0, [40, io.captures as u8, 0]);
    }

    #[test]
    fn same_cell_revisit_refreshes_the_crop_without_another_click() {
        let mut io = ScriptIo::new(vec![(0, 90), (50, 150)]);
        let mut waiter = ArtifactPanelWaiter::default();
        assert!(matches!(
            waiter.select(&mut io, CELL, OPTIONS).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        let before = io.captures;
        io.time += Duration::from_millis(100);
        let ArtifactSelection::Ready(frame) = waiter.select(&mut io, CELL, OPTIONS).unwrap() else {
            panic!()
        };
        assert_eq!(io.captures, before + 1);
        assert_eq!(frame.image.get_pixel(0, 0)[0], 150);
        assert!(waiter.last.unwrap().1.similar(&signature(150)));
    }

    #[test]
    fn timeout_returns_the_last_frame_even_if_it_just_changed() {
        let mut io = ScriptIo::new(vec![(100, 120)]);
        let frame = io.select(OPTIONS);
        assert_eq!(io.elapsed_after_click(), 100);
        assert_eq!(frame.image.get_pixel(0, 0).0, [120, io.captures as u8, 0]);
    }

    #[test]
    fn capture_crossing_the_deadline_cannot_supply_older_pixels_as_the_final_frame() {
        let mut io = ScriptIo::new(vec![(100, 120)]);
        io.capture_ms = 43;
        let frame = io.select(OPTIONS);
        // The 63ms capture still saw the old item when it returned at 106ms.
        assert_eq!(io.elapsed_after_click(), 149);
        assert_eq!(frame.image.get_pixel(0, 0).0, [120, io.captures as u8, 0]);
    }

    #[test]
    fn stable_blank_is_only_accepted_at_the_deadline() {
        for (steps, value, accepted_at) in
            [(vec![(0, 0), (70, 95)], 95, 88), (vec![(0, 0)], 0, 100)]
        {
            let mut io = ScriptIo::new(steps);
            let frame = io.select(OPTIONS);
            assert_eq!(io.elapsed_after_click(), accepted_at);
            assert_eq!(frame.image.get_pixel(0, 0)[0], value);
        }
    }

    #[test]
    fn hold_cannot_extend_a_revisit_past_its_timeout() {
        let mut io = ScriptIo::new(vec![(0, 90)]);
        let options = ArtifactPanelOptions {
            extra_delay_ms: 150,
            ..OPTIONS
        };
        let frame = io.select(options);
        assert_eq!(io.elapsed_after_click(), options.timeout_ms);
        assert_eq!(frame.image.get_pixel(0, 0)[0], 90);
    }

    #[test]
    fn slow_capture_and_large_initial_wait_do_not_add_another_capture_after_timeout() {
        let mut io = ScriptIo::new(vec![(0, 95)]);
        io.capture_ms = 66;
        let frame = io.select(ArtifactPanelOptions {
            timeout_ms: 10,
            ..OPTIONS
        });
        assert_eq!(io.elapsed_after_click(), 76);
        assert_eq!(io.captures, 2); // one pre-click baseline, one post-click frame
        assert_eq!(frame.image.get_pixel(0, 0)[0], 95);
    }

    #[test]
    fn cancellation_during_capture_and_capture_errors_discard_the_cached_baseline() {
        for after_click in [false, true] {
            let mut io = ScriptIo::new(vec![(0, 90)]);
            io.capture_ms = 15;
            io.cancel_at = Some(Duration::from_millis(if after_click { 30 } else { 0 }));
            let mut waiter = ArtifactPanelWaiter {
                last: Some((ArtifactCell { row: 0, col: 0 }, signature(40))),
            };
            assert!(matches!(
                waiter.select(&mut io, CELL, OPTIONS).unwrap(),
                ArtifactSelection::Cancelled
            ));
            assert_eq!(io.clicked_at.is_some(), after_click);
            assert!(waiter.last.is_none());
        }
        let mut io = ScriptIo::new(vec![(0, 90)]);
        io.fail_at = Some(2);
        let mut waiter = ArtifactPanelWaiter::default();
        assert!(waiter.select(&mut io, CELL, OPTIONS).is_err());
        assert!(waiter.last.is_none());
    }

    #[test]
    fn detail_fingerprint_matches_a_crop_and_detects_stat_changes() {
        let image =
            image::load_from_memory(include_bytes!("../../../tests/fixtures/artifact_panel.png"))
                .unwrap()
                .to_rgb8();
        let full = CaptureFrame::full(image);
        let scaler = CoordScaler::new(full.image.width(), full.image.height());
        let rect = ARTIFACT_DETAIL_PANEL_RECT;
        let mut detail = CaptureFrame {
            image: full.crop(rect, 0.0, &scaler).unwrap(),
            origin: (rect.0, rect.1),
        };
        let reference = ArtifactPanelSignature::from_frame(&full, &scaler);
        assert!(reference.readable);
        assert!(reference.similar(&ArtifactPanelSignature::from_frame(&detail, &scaler)));
        let x = scaler.x(1380.0) - scaler.x(rect.0);
        let y = scaler.y(492.0) - scaler.y(rect.1);
        let pixel = detail.image.get_pixel_mut(x as u32, y as u32);
        for channel in &mut pixel.0 {
            *channel = if *channel > 127 { 0 } else { 255 };
        }
        assert!(!reference.similar(&ArtifactPanelSignature::from_frame(&detail, &scaler)));

        // Hiding only the slot text is enough to suppress early acceptance
        // while the panel is still drawing, even when other fields are visible.
        for by in 184..224 {
            for bx in 1327..1482 {
                let x = scaler.x(bx as f64) - scaler.x(rect.0);
                let y = scaler.y(by as f64) - scaler.y(rect.1);
                detail.image.put_pixel(x as u32, y as u32, Rgb([30; 3]));
            }
        }
        assert!(!ArtifactPanelSignature::from_frame(&detail, &scaler).readable);
    }
}
