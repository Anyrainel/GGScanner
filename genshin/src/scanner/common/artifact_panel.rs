//! Controller-owned artifact selection and capture. OCR only receives accepted
//! immutable frames; retries never run on an OCR thread.
use std::time::{Duration, Instant};

use anyhow::Result;

use super::capture_frame::CaptureFrame;
use super::constants::*;
use super::coord_scaler::CoordScaler;
use super::game_controller::{panel_frames_similar, GenshinGameController};
use super::grid_icon_detector::artifact_cell_center;

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
    /// A minimum hold after the first stable candidate, not an unchecked sleep
    /// followed by another capture. Changes during this hold restart it.
    pub extra_delay_ms: u64,
}

pub enum ArtifactSelection {
    Ready(CaptureFrame),
    Empty,
    Unconfirmed,
    Cancelled,
}

#[derive(Clone)]
struct PanelSignature {
    regions: Vec<Vec<u8>>,
    readable: bool,
}

impl PanelSignature {
    fn similar(&self, other: &Self) -> bool {
        self.regions.len() == other.regions.len()
            && self.regions.iter().zip(&other.regions).all(|(a, b)| {
                a.len() == b.len()
                    // Small tiles keep a changed digit from disappearing into
                    // the average of a large, mostly unchanged panel.
                    && a.chunks(192)
                        .zip(b.chunks(192))
                        .all(|(a, b)| panel_frames_similar(a, b))
            })
    }
}

struct PanelSample {
    frame: CaptureFrame,
    signature: PanelSignature,
    selected: bool,
    card_absent: bool,
}

#[derive(Clone)]
struct ConfirmedPanel {
    cell: ArtifactCell,
    signature: PanelSignature,
}

#[derive(Default)]
pub struct ArtifactPanelWaiter {
    last: Option<ConfirmedPanel>,
    references: Vec<ConfirmedPanel>,
}

/// Test seam for the actual navigation algorithm, including its clock. The
/// capture APIs currently expose no presentation timestamps, so production
/// spaces observations monotonically instead of claiming fresh game frames.
trait PanelIo {
    fn now(&self) -> Duration;
    fn sleep(&mut self, duration: Duration);
    fn cancelled(&mut self) -> bool;
    fn click(&mut self, cell: ArtifactCell);
    fn capture(&mut self, cell: ArtifactCell) -> Result<PanelSample>;
}

enum PanelWait {
    Ready(PanelSample),
    Unchanged(PanelSample),
    Unstable,
    Cancelled,
}

fn wait_for_panel(
    io: &mut impl PanelIo,
    cell: ArtifactCell,
    baseline: &PanelSignature,
    allow_unchanged: bool,
    already_selected: bool,
    expected: Option<&PanelSignature>,
    options: ArtifactPanelOptions,
) -> Result<PanelWait> {
    let start = io.now();
    // A large configured initial wait must not consume the observation budget.
    let initial = Duration::from_millis(options.initial_wait_ms);
    let budget = Duration::from_millis(options.timeout_ms)
        .max(initial + FRAME_INTERVAL * 2 + Duration::from_millis(options.extra_delay_ms));
    let deadline = start + budget;
    let hold = FRAME_INTERVAL.max(Duration::from_millis(options.extra_delay_ms));
    let mut next_sample = start + initial;
    let mut candidate: Option<(PanelSample, Duration)> = None;
    let mut captures = 0;
    let mut first_change = None;
    let mut first_selection = None;
    loop {
        if io.cancelled() {
            return Ok(PanelWait::Cancelled);
        }
        let now = io.now();
        if now > deadline {
            break;
        }
        if next_sample > now {
            io.sleep((next_sample - now).min(deadline - now));
        }
        if io.cancelled() {
            return Ok(PanelWait::Cancelled);
        }
        let sample_started = io.now();
        let sample = io.capture(cell)?;
        let observed = io.now();
        captures += 1;
        // Include capture time in the cadence; do not add another 17ms when
        // capture itself already took that long.
        next_sample = sample_started + FRAME_INTERVAL;
        let changed = !sample.signature.similar(baseline);
        let stable_since = candidate
            .as_ref()
            .filter(|(previous, _)| sample.signature.similar(&previous.signature))
            .map(|(_, since)| *since)
            .unwrap_or(observed);
        let usable = sample.signature.readable && !sample.card_absent;
        let selected = sample.selected || already_selected;
        if changed {
            first_change.get_or_insert((observed - start).as_millis());
        }
        if selected {
            first_selection.get_or_insert((observed - start).as_millis());
        }
        if observed <= deadline
            && usable
            && selected
            && expected.is_none_or(|expected| expected.similar(&sample.signature))
            && (changed || allow_unchanged)
            && observed.saturating_sub(stable_since) >= hold
        {
            yas::log_debug!(
                "[artifact] 详情就绪: {}ms，采样{}次，首次内容变化={:?}ms，首次确认选中={:?}ms",
                "[artifact] Detail ready: {}ms, {} samples, first content change={:?}ms, first selection confirmation={:?}ms",
                (observed - start).as_millis(), captures, first_change, first_selection
            );
            return Ok(PanelWait::Ready(sample));
        }
        // Blank/partial frames cannot accumulate stable time.
        candidate = sample.signature.readable.then_some((sample, stable_since));
        if observed >= deadline {
            break;
        }
    }
    Ok(match candidate {
        Some((sample, since))
            if io.now().saturating_sub(since) >= hold && sample.signature.similar(baseline) =>
        {
            PanelWait::Unchanged(sample)
        },
        _ => PanelWait::Unstable,
    })
}

impl ArtifactPanelWaiter {
    pub fn invalidate(&mut self) {
        *self = Self::default();
    }

    fn accept(&mut self, cell: ArtifactCell, sample: PanelSample) -> ArtifactSelection {
        let confirmed = ConfirmedPanel {
            cell,
            signature: sample.signature,
        };
        self.references.retain(|reference| reference.cell != cell);
        // Keep page-local identities as well as recovery references. A manager
        // revisit must match the detail actually scanned at this position;
        // merely observing some changed, stable panel is insufficient to toggle.
        self.references.push(confirmed.clone());
        if self.references.len() > GRID_ROWS * GRID_COLS {
            self.references.remove(0);
        }
        self.last = Some(confirmed);
        ArtifactSelection::Ready(sample.frame)
    }

    fn select(
        &mut self,
        io: &mut impl PanelIo,
        cell: ArtifactCell,
        options: ArtifactPanelOptions,
    ) -> Result<ArtifactSelection> {
        if io.cancelled() {
            return Ok(ArtifactSelection::Cancelled);
        }
        let baseline = match &self.last {
            Some(last) => last.signature.clone(),
            None => io.capture(cell)?.signature,
        };
        let already_selected = self.last.as_ref().is_some_and(|last| last.cell == cell);
        let expected = self
            .references
            .iter()
            .find(|r| r.cell == cell)
            .map(|r| r.signature.clone());
        let mut unchanged = None;
        // No recovery clicks until a complete normal wait has expired.
        for attempt in 0..2 {
            if io.cancelled() {
                return Ok(ArtifactSelection::Cancelled);
            }
            io.click(cell);
            match wait_for_panel(
                io,
                cell,
                &baseline,
                already_selected,
                already_selected,
                expected.as_ref(),
                options,
            )? {
                PanelWait::Ready(sample) => return Ok(self.accept(cell, sample)),
                PanelWait::Cancelled => return Ok(ArtifactSelection::Cancelled),
                PanelWait::Unchanged(sample) => {
                    // A preselected first item has no earlier reference. Only
                    // accept it after the full timeout and a visible selection.
                    if self.references.is_empty() && sample.selected && !sample.card_absent {
                        return Ok(self.accept(cell, sample));
                    }
                    unchanged = Some(sample);
                },
                PanelWait::Unstable => unchanged = None,
            }
            yas::log_debug!(
                "[artifact] 详情等待超时 (行={}, 列={}, 尝试={})",
                "[artifact] Detail wait timed out (row={}, col={}, attempt={})",
                cell.row,
                cell.col,
                attempt + 1
            );
        }

        let reference = self
            .references
            .iter()
            .find(|r| r.cell != cell && !r.signature.similar(&baseline))
            .or_else(|| self.references.iter().find(|r| r.cell != cell))
            .cloned();
        let Some(reference) = reference else {
            self.last = None;
            return Ok(ArtifactSelection::Unconfirmed);
        };
        if io.cancelled() {
            return Ok(ArtifactSelection::Cancelled);
        }
        io.click(reference.cell);
        // It is safe to require only stability for a *known* reference, but
        // still require a selected border and its recorded content below.
        let anchor = match wait_for_panel(
            io,
            reference.cell,
            &baseline,
            true,
            false,
            Some(&reference.signature),
            options,
        )? {
            PanelWait::Ready(sample)
                if sample.selected && sample.signature.similar(&reference.signature) =>
            {
                sample
            },
            PanelWait::Cancelled => return Ok(ArtifactSelection::Cancelled),
            _ => {
                self.last = None;
                return Ok(ArtifactSelection::Unconfirmed);
            },
        };
        if io.cancelled() {
            return Ok(ArtifactSelection::Cancelled);
        }
        io.click(cell);
        let result = match wait_for_panel(
            io,
            cell,
            &anchor.signature,
            false,
            false,
            expected.as_ref(),
            options,
        )? {
            PanelWait::Ready(sample) => self.accept(cell, sample),
            PanelWait::Unchanged(sample)
                if sample.selected
                    && !sample.card_absent
                    && expected
                        .as_ref()
                        .is_none_or(|e| e.similar(&sample.signature)) =>
            {
                // Exact duplicate: the target selection is visible, and the
                // full timeout passed after returning from a confirmed anchor.
                self.accept(cell, sample)
            },
            PanelWait::Unchanged(sample)
                if sample.card_absent
                    && !sample.selected
                    && unchanged.as_ref().is_some_and(|s| s.card_absent) =>
            {
                self.last = Some(reference);
                ArtifactSelection::Empty
            },
            PanelWait::Cancelled => ArtifactSelection::Cancelled,
            _ => {
                self.last = None;
                ArtifactSelection::Unconfirmed
            },
        };
        Ok(result)
    }
}

fn region_samples(
    frame: &CaptureFrame,
    scaler: &CoordScaler,
    rect: (f64, f64, f64, f64),
) -> Vec<u8> {
    let (x, y, w, h) = rect;
    let mut samples = Vec::new();
    for by in (0..h as usize).step_by(2) {
        for bx in (0..w as usize).step_by(2) {
            samples.extend(frame.pixel(scaler, x + bx as f64, y + by as f64));
        }
    }
    samples
}

fn has_text(samples: &[u8]) -> bool {
    samples
        .chunks_exact(3)
        .zip(samples.chunks_exact(3).skip(1))
        .filter(|(a, b)| a.iter().zip(*b).any(|(a, b)| a.abs_diff(*b) > 35))
        .take(6)
        .count()
        >= 6
}

fn observe(frame: CaptureFrame, scaler: &CoordScaler, cell: ArtifactCell) -> PanelSample {
    // Exclude illustration, cursor, animated lock/star icons and selection glow.
    // Both normal and +40px layouts are covered by the lower text regions.
    let regions: Vec<_> = [
        (1327.0, 120.0, 430.0, 40.0),
        (1327.0, 184.0, 155.0, 40.0),
        (1327.0, 266.0, 185.0, 100.0),
        (1330.0, 420.0, 110.0, 83.0),
        (1356.0, 478.0, 355.0, 230.0),
        (1330.0, 710.0, 360.0, 150.0),
        (1386.0, 905.0, 315.0, 50.0),
        // Elixir banner determines whether all lower OCR crops shift by 40px.
        (1508.0, 410.0, 36.0, 35.0),
    ]
    .into_iter()
    .map(|rect| region_samples(&frame, scaler, rect))
    .collect();
    let readable = has_text(&regions[1]) && has_text(&regions[2]) && has_text(&regions[3]);
    let (x, y) = artifact_cell_center(cell.row, cell.col);
    // Look for the neutral bright selection rim on multiple sides, outside the
    // illustration and level band. Sample a band to tolerate grid Y drift.
    let mut selected_sides = 0;
    for side in [-1.0, 1.0] {
        let mut hits = 0;
        for dy in (-50..50).step_by(4) {
            let bright = (59..69).any(|dx| {
                let rgb = frame.pixel(scaler, x + side * dx as f64, y + dy as f64);
                let lo = *rgb.iter().min().unwrap();
                let hi = *rgb.iter().max().unwrap();
                lo > 180 && hi - lo < 55
            });
            hits += usize::from(bright);
        }
        selected_sides += usize::from(hits >= 12);
    }
    let interior = region_samples(&frame, scaler, (x - 40.0, y - 40.0, 80.0, 80.0));
    let range = interior
        .chunks_exact(3)
        .map(|rgb| rgb.iter().map(|v| *v as u16).sum::<u16>() / 3)
        .fold((255, 0), |(lo, hi), value| (lo.min(value), hi.max(value)));
    PanelSample {
        frame,
        signature: PanelSignature { regions, readable },
        selected: selected_sides == 2,
        // Uniform dark inventory background is independent evidence. Unknown
        // artwork is never treated as empty merely because text stayed still.
        card_absent: range.1 < 140 && range.1 - range.0 < 18,
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
    fn cancelled(&mut self) -> bool {
        self.ctrl.check_rmb()
    }
    fn click(&mut self, cell: ArtifactCell) {
        self.ctrl.click_at(
            GRID_FIRST_X + cell.col as f64 * GRID_OFFSET_X,
            GRID_FIRST_Y + cell.row as f64 * GRID_OFFSET_Y,
        );
    }
    fn capture(&mut self, cell: ArtifactCell) -> Result<PanelSample> {
        let frame = CaptureFrame::full(self.ctrl.capture_game()?);
        Ok(observe(frame, &self.ctrl.scaler, cell))
    }
}

impl GenshinGameController {
    pub fn select_artifact(
        &mut self,
        cell: ArtifactCell,
        options: ArtifactPanelOptions,
        full_capture: bool,
    ) -> Result<ArtifactSelection> {
        let mut waiter = std::mem::take(&mut self.artifact_panel_waiter);
        let started = Instant::now();
        let result = waiter.select(
            &mut ControllerIo {
                ctrl: self,
                start: started,
            },
            cell,
            options,
        );
        if result.is_err() || matches!(&result, Ok(ArtifactSelection::Cancelled)) {
            waiter.invalidate();
        }
        self.artifact_panel_waiter = waiter;
        yas::log_debug!(
            "[artifact] 选择并确认详情耗时 {}ms (行={}, 列={})",
            "[artifact] Selection and detail confirmation took {}ms (row={}, col={})",
            started.elapsed().as_millis(),
            cell.row,
            cell.col
        );
        // Cropping is from the accepted frame, never a second screenshot.
        Ok(match result? {
            ArtifactSelection::Ready(frame) if !full_capture => {
                let image = frame
                    .crop(ARTIFACT_DETAIL_PANEL_RECT, 0.0, &self.scaler)
                    .expect("artifact detail capture rectangle must fit the full frame");
                ArtifactSelection::Ready(CaptureFrame {
                    image,
                    origin: (ARTIFACT_DETAIL_PANEL_RECT.0, ARTIFACT_DETAIL_PANEL_RECT.1),
                })
            },
            other => other,
        })
    }

    pub fn reset_artifact_panel(&mut self) {
        self.artifact_panel_waiter.invalidate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgb, RgbImage};
    use std::collections::VecDeque;

    const FIRST: ArtifactCell = ArtifactCell { row: 0, col: 0 };
    const TARGET: ArtifactCell = ArtifactCell { row: 0, col: 1 };
    const REFERENCE: ArtifactCell = ArtifactCell { row: 0, col: 2 };

    fn options() -> ArtifactPanelOptions {
        ArtifactPanelOptions {
            timeout_ms: 200,
            initial_wait_ms: 20,
            extra_delay_ms: 0,
        }
    }

    fn signature(value: u8) -> PanelSignature {
        PanelSignature {
            regions: vec![vec![value; 384]; 7],
            readable: true,
        }
    }

    #[derive(Clone)]
    struct Step {
        at: u64,
        signature: PanelSignature,
        selected: bool,
        absent: bool,
    }

    fn step(at: u64, value: u8, selected: bool) -> Step {
        Step {
            at,
            signature: signature(value),
            selected,
            absent: false,
        }
    }

    struct ScriptIo {
        time: Duration,
        plans: VecDeque<(ArtifactCell, Vec<Step>)>,
        active: Vec<Step>,
        clicked_at: Duration,
        clicks: Vec<(ArtifactCell, Duration)>,
        captures: u8,
        capture_ms: u64,
        cancel_at: Option<Duration>,
        fail_capture: bool,
    }

    impl ScriptIo {
        fn new(plans: Vec<(ArtifactCell, Vec<Step>)>) -> Self {
            Self {
                time: Duration::ZERO,
                plans: plans.into(),
                active: vec![step(0, 40, false)],
                clicked_at: Duration::ZERO,
                clicks: vec![],
                captures: 0,
                capture_ms: 0,
                cancel_at: None,
                fail_capture: false,
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
        fn cancelled(&mut self) -> bool {
            self.cancel_at.is_some_and(|at| self.time >= at)
        }
        fn click(&mut self, cell: ArtifactCell) {
            let (expected, plan) = self.plans.pop_front().expect("unexpected recovery click");
            assert_eq!(cell, expected);
            self.active = plan;
            self.clicked_at = self.time;
            self.clicks.push((cell, self.time));
        }
        fn capture(&mut self, _: ArtifactCell) -> Result<PanelSample> {
            anyhow::ensure!(!self.fail_capture, "capture unavailable");
            self.time += Duration::from_millis(self.capture_ms);
            self.captures += 1;
            let elapsed = (self.time - self.clicked_at).as_millis() as u64;
            let state = self.active.iter().rev().find(|s| s.at <= elapsed).unwrap();
            Ok(PanelSample {
                frame: CaptureFrame::full(RgbImage::from_pixel(1, 1, Rgb([self.captures, 0, 0]))),
                signature: state.signature.clone(),
                selected: state.selected,
                card_absent: state.absent,
            })
        }
    }

    fn seeded() -> ArtifactPanelWaiter {
        let first = ConfirmedPanel {
            cell: FIRST,
            signature: signature(40),
        };
        ArtifactPanelWaiter {
            last: Some(first.clone()),
            references: vec![first],
        }
    }

    #[test]
    fn one_or_two_frame_input_latency_never_triggers_recovery() {
        for update_at in [17, 34, 110, 170] {
            let mut io = ScriptIo::new(vec![(
                TARGET,
                vec![step(0, 40, false), step(update_at, 90, true)],
            )]);
            let result = seeded().select(&mut io, TARGET, options()).unwrap();
            let ArtifactSelection::Ready(frame) = result else {
                panic!("new detail was not accepted")
            };
            assert_eq!(io.clicks.len(), 1);
            assert!(io.time < Duration::from_millis(200));
            assert!(io.time >= Duration::from_millis(update_at + 17));
            // OCR receives exactly the accepted screenshot, with no recapture.
            assert_eq!(frame.image.get_pixel(0, 0)[0], io.captures);
        }
    }

    #[test]
    fn selection_border_moving_before_detail_does_not_accept_old_text() {
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 40, true), step(80, 90, true)])]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert!(io.time >= Duration::from_millis(97));
        assert_eq!(io.clicks.len(), 1);
    }

    #[test]
    fn all_regions_must_settle_and_blank_frames_reset_stability() {
        let mut partial = step(17, 90, true);
        partial.signature.regions[4].fill(40);
        let mut blank = step(34, 0, true);
        blank.signature.readable = false;
        let mut io = ScriptIo::new(vec![(
            TARGET,
            vec![step(0, 40, false), partial, blank, step(68, 90, true)],
        )]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert!(io.time >= Duration::from_millis(85));
        assert_eq!(io.clicks.len(), 1);
    }

    #[test]
    fn changed_text_without_target_selection_is_not_dispatched() {
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 90, false)]),
            (TARGET, vec![step(0, 90, false)]),
            (FIRST, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 90, false)]),
        ]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Unconfirmed
        ));
        assert!(io.clicks[1].1 >= Duration::from_millis(200));
    }

    #[test]
    fn dropped_click_is_retried_only_after_full_timeout() {
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 40, false)]),
            (TARGET, vec![step(0, 40, false), step(34, 90, true)]),
        ]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert_eq!(io.clicks.len(), 2);
        assert_eq!(io.clicks[1].1, Duration::from_millis(200));
    }

    #[test]
    fn identical_details_use_a_different_reference_after_timeout() {
        let mut waiter = seeded();
        waiter.references.push(ConfirmedPanel {
            cell: REFERENCE,
            signature: signature(100),
        });
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 40, true)]),
            (REFERENCE, vec![step(0, 100, true)]),
            (TARGET, vec![step(0, 40, true)]),
        ]);
        assert!(matches!(
            waiter.select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert_eq!(io.clicks.len(), 4);
        assert_eq!(io.clicks[2].1, Duration::from_millis(400));
        assert_eq!(waiter.last.unwrap().cell, TARGET);
    }

    #[test]
    fn identical_reference_also_requires_full_target_timeout() {
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 40, true)]),
            (FIRST, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 40, true)]),
        ]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert!(io.time >= Duration::from_millis(600));
    }

    #[test]
    fn empty_requires_background_evidence_and_a_confirmed_reference_roundtrip() {
        let mut empty = step(0, 40, false);
        empty.absent = true;
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![empty.clone()]),
            (TARGET, vec![empty.clone()]),
            (FIRST, vec![step(0, 40, true)]),
            (TARGET, vec![empty]),
        ]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Empty
        ));
        assert_eq!(io.clicks.len(), 4);
    }

    #[test]
    fn manager_revisit_requires_the_recorded_target_detail() {
        let mut waiter = seeded();
        waiter.references.push(ConfirmedPanel {
            cell: TARGET,
            signature: signature(90),
        });
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 70, true), step(85, 90, true)])]);
        assert!(matches!(
            waiter.select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert!(io.time >= Duration::from_millis(102));
        assert_eq!(io.clicks.len(), 1);
    }

    #[test]
    fn extra_delay_is_revalidated_and_capture_time_counts_toward_cadence() {
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 90, true), step(70, 100, true)])]);
        io.capture_ms = 20;
        let config = ArtifactPanelOptions {
            extra_delay_ms: 80,
            ..options()
        };
        assert!(matches!(
            seeded().select(&mut io, TARGET, config).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert!(io.time >= Duration::from_millis(150));
        assert!(io.time <= Duration::from_millis(180));
    }

    #[test]
    fn cancellation_never_runs_recovery_and_capture_errors_are_propagated() {
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 40, false)])]);
        io.cancel_at = Some(Duration::from_millis(50));
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Cancelled
        ));
        assert_eq!(io.clicks.len(), 1);
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 90, true)])]);
        io.fail_capture = true;
        assert!(seeded().select(&mut io, TARGET, options()).is_err());
        assert_eq!(io.clicks.len(), 1);
    }

    #[test]
    fn preselected_first_item_waits_full_timeout_and_page_reset_discards_identities() {
        let mut waiter = ArtifactPanelWaiter::default();
        let mut io = ScriptIo::new(vec![(TARGET, vec![step(0, 40, true)])]);
        assert!(matches!(
            waiter.select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert_eq!(io.time, Duration::from_millis(200));
        waiter.invalidate();
        assert!(waiter.last.is_none() && waiter.references.is_empty());
    }

    #[test]
    fn tile_comparison_detects_small_digit_changes_and_tolerates_capture_noise() {
        let baseline = signature(80);
        let mut noise = baseline.clone();
        for region in &mut noise.regions {
            region.iter_mut().for_each(|v| *v += 5);
        }
        assert!(baseline.similar(&noise));
        let mut digit = baseline.clone();
        digit.regions[4][150] = 255;
        assert!(!baseline.similar(&digit));
    }

    #[test]
    fn real_panel_fixture_has_readable_text_and_only_the_target_rim_is_selected() {
        // Sanitized spatial crops from a real inventory capture: unrelated UI
        // and the inventory count are removed. Test actual pixel extraction,
        // not only the scripted state-machine observations.
        let image =
            image::load_from_memory(include_bytes!("../../../tests/fixtures/artifact_panel.png"))
                .unwrap()
                .to_rgb8();
        for (width, height) in [(960, 540), (1920, 1080)] {
            let scaled = image::imageops::resize(
                &image,
                width,
                height,
                image::imageops::FilterType::Nearest,
            );
            let scaler = CoordScaler::new(width, height);
            let selected = observe(CaptureFrame::full(scaled.clone()), &scaler, FIRST);
            assert!(
                selected.signature.readable,
                "unreadable at {width}x{height}"
            );
            assert!(
                selected.selected,
                "missing selection rim at {width}x{height}"
            );
            assert!(!selected.card_absent);
            let neighbor = observe(CaptureFrame::full(scaled), &scaler, TARGET);
            assert!(!neighbor.selected, "unselected card mistaken for selected");
            assert!(!neighbor.card_absent);
        }
    }

    #[test]
    fn wrong_detail_after_an_earlier_failure_cannot_bypass_manager_identity_check() {
        let mut waiter = seeded();
        waiter.last = None;
        waiter.references.push(ConfirmedPanel {
            cell: TARGET,
            signature: signature(90),
        });
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 40, true)]),
            (FIRST, vec![step(0, 40, true)]),
            (TARGET, vec![step(0, 40, true)]),
        ]);
        assert!(matches!(
            waiter.select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Unconfirmed
        ));
    }

    #[test]
    fn recovery_waits_for_reference_selection_even_when_its_text_is_already_visible() {
        let mut io = ScriptIo::new(vec![
            (TARGET, vec![step(0, 40, false)]),
            (TARGET, vec![step(0, 40, false)]),
            (FIRST, vec![step(0, 40, false), step(85, 40, true)]),
            (TARGET, vec![step(0, 90, true)]),
        ]);
        assert!(matches!(
            seeded().select(&mut io, TARGET, options()).unwrap(),
            ArtifactSelection::Ready(_)
        ));
        assert_eq!(io.clicks.len(), 4);
        assert!(io.clicks[3].1 >= Duration::from_millis(485));
    }
}
