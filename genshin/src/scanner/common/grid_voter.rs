//! Per-page grid-icon voting state machine.
//!
//! Encapsulates the 3-pass voting scheme used by both the artifact/weapon
//! scanners and the lock manager to classify icon status (lock / astral /
//! elixir) across inventory pages.
//!
//! ## Algorithm
//!
//! Three detection passes run at page-relative item indices spread across the
//! items actually present on the page. A full 40-item page keeps the original
//! `0, 13, 26` schedule; shorter pages derive equivalent positions.
//!
//! ### Tie-breaking
//!
//! We want every page to end up with exactly 1 or 3 passes — never 2, which
//! produces an ambiguous majority. Items are handled as follows:
//!
//! - Full pages retain the streaming behavior for compatibility.
//! - Short pages defer all items until their three scheduled passes complete,
//!   so a recent-artifact scan ending mid-page gives every returned item a
//!   majority-voted lock state.
//! - Early-stop callers finish any remaining grid passes before
//!   flushing deferred items.
//!
//! ## Payload
//!
//! The voter is generic over a caller-supplied payload `T` carried alongside
//! each deferred item. Scanners use `T = ()`; the lock manager uses
//! `T = (row, col)` so it can re-click the same grid cell later to toggle
//! the lock.

use std::sync::Arc;

use image::RgbImage;

use super::capture_frame::CaptureFrame;
use super::coord_scaler::CoordScaler;
use super::grid_icon_detector::{
    GridCellAnnotation, GridIconResult, GridMode, GridPageDetection, ITEMS_PER_PAGE,
};

/// Grid annotation snapshot: cell bounding boxes + per-cell (index, lock, astral).
pub type GridAnnotation = Arc<(Vec<GridCellAnnotation>, Vec<(usize, bool, bool)>)>;

/// Grid-vote positions for one visible page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridVoteSchedule {
    pub indices: [usize; 3],
}

impl GridVoteSchedule {
    /// Preserve the historical 40-item schedule: 0, 13, 26.
    /// Short pages use equivalent one-third and two-thirds positions.
    pub fn for_page(page_items: usize) -> Self {
        match page_items {
            0 | 1 => Self { indices: [0, 0, 0] },
            2 => Self { indices: [0, 1, 1] },
            n => Self {
                indices: [0, n / 3, (n * 2) / 3],
            },
        }
    }
}

/// An item that is ready to be emitted by the caller (voting has settled).
pub struct ReadyItem<T> {
    pub idx: usize,
    pub frame: CaptureFrame,
    pub metadata: Option<GridIconResult>,
    /// Grid overlay annotation data for the page (shared across items on the same page).
    pub grid_annotation: Option<GridAnnotation>,
    pub payload: T,
}

/// Per-page state held by [`PagedGridVoter`].
struct PageState<T> {
    page_start: usize,
    page_items: usize,
    screen_start_row: usize,
    detection: GridPageDetection,
    include_grid_overlay: bool,
    schedule: GridVoteSchedule,
    passes_done: u32,
    defer_until_complete: bool,
    last_idx: usize,
    deferred: Vec<(usize, CaptureFrame, T)>,
}

impl<T> PageState<T> {
    fn grid_overlay(&self) -> Option<GridAnnotation> {
        if self.include_grid_overlay {
            self.detection.annotation_snapshot().map(Arc::new)
        } else {
            None
        }
    }
}

/// State machine that ingests items page-by-page and returns them in voting
/// order: either immediately (1 or 3 passes done) or after deferral.
pub struct PagedGridVoter<T> {
    mode: GridMode,
    total: usize,
    state: Option<PageState<T>>,
}

impl<T> PagedGridVoter<T> {
    /// Create a voter for `total` items using the given grid `mode`.
    pub fn new(total: usize, mode: GridMode) -> Self {
        Self {
            mode,
            total,
            state: None,
        }
    }

    /// Clear page state. Call when `scan_grid` emits `PageScrolled` (the
    /// next `record` call will lazily initialize fresh state for the new
    /// page).
    pub fn reset_page(&mut self) {
        self.state = None;
    }

    pub fn needs_additional_passes(&self) -> bool {
        self.state
            .as_ref()
            .is_some_and(|s| s.passes_done < 3 && !s.deferred.is_empty())
    }

    pub fn last_page_idx(&self) -> Option<usize> {
        self.state.as_ref().map(|s| s.last_idx)
    }

    /// Run one additional grid detection pass while the page is still visible.
    pub fn run_additional_pass(
        &mut self,
        tiebreak_image: &RgbImage,
        trigger_idx: usize,
        scaler: &CoordScaler,
    ) {
        if let Some(state) = self.state.as_mut() {
            if state.passes_done < 3 {
                state
                    .detection
                    .detect_pass(tiebreak_image, scaler, trigger_idx);
                state.passes_done += 1;
            }
        }
    }

    /// Finish passes that traversal could not reach while the current page is
    /// still visible.
    pub fn finish_additional_passes<F>(&mut self, scaler: &CoordScaler, mut capture: F)
    where
        F: FnMut() -> Option<RgbImage>,
    {
        while self.needs_additional_passes() {
            let idx = self.last_page_idx().unwrap_or(0);
            let Some(image) = capture() else { break };
            self.run_additional_pass(&image, idx, scaler);
        }
    }

    /// Record an item captured on the current page.
    /// Returns any items that are now ready for emission. The returned items may
    /// include the one just recorded and/or previously-deferred items that
    /// became ready on this call.
    pub fn record(
        &mut self,
        idx: usize,
        frame: CaptureFrame,
        payload: T,
        scaler: &CoordScaler,
    ) -> Vec<ReadyItem<T>> {
        let page_start = (idx / ITEMS_PER_PAGE) * ITEMS_PER_PAGE;
        let page_items = (self.total - page_start).min(ITEMS_PER_PAGE);
        self.record_with_layout(idx, frame, payload, scaler, page_start, page_items, 0, None)
    }

    /// Record an item using traversal-provided page geometry.
    ///
    /// `screen_start_row` is non-zero when a clamped final scroll reveals only
    /// the bottom rows of the grid. Keeping this mapping authoritative prevents
    /// lock/astral votes from being read from overlapping rows above the tail.
    pub fn record_with_layout(
        &mut self,
        idx: usize,
        frame: CaptureFrame,
        payload: T,
        scaler: &CoordScaler,
        page_start: usize,
        page_items: usize,
        screen_start_row: usize,
        grid_image: Option<&RgbImage>,
    ) -> Vec<ReadyItem<T>> {
        debug_assert!(idx >= page_start && idx < page_start + page_items);
        debug_assert!(screen_start_row < 5);
        let page_rel = idx - page_start;

        // Lazily initialize per-page state.
        if self.state.is_none() {
            self.state = Some(PageState {
                page_start,
                page_items,
                screen_start_row,
                detection: GridPageDetection::with_layout(
                    page_start,
                    page_items,
                    screen_start_row,
                    self.mode,
                ),
                include_grid_overlay: frame.is_full_window(),
                schedule: GridVoteSchedule::for_page(page_items),
                passes_done: 0,
                defer_until_complete: page_items < ITEMS_PER_PAGE,
                last_idx: idx,
                deferred: Vec::new(),
            });
        }
        let state = self.state.as_mut().unwrap();
        debug_assert_eq!(state.page_start, page_start);
        debug_assert_eq!(state.page_items, page_items);
        debug_assert_eq!(state.screen_start_row, screen_start_row);
        state.last_idx = idx;

        let mut ready: Vec<ReadyItem<T>> = Vec::new();

        // Run detection pass at scheduled page-relative indices.
        if state.passes_done < 3 && state.schedule.indices.contains(&page_rel) {
            // Weapon frames include the grid. Artifact details have a nonzero
            // origin and use a separate grid crop only at vote positions.
            if let Some(image) =
                grid_image.or_else(|| frame.is_full_window().then_some(&frame.image))
            {
                state.detection.detect_pass(image, scaler, idx);
                state.passes_done += 1;
            }
        }

        let voting_complete = state.passes_done >= 3;
        let should_defer = if state.defer_until_complete {
            !voting_complete
        } else {
            state.passes_done == 2 && page_rel >= state.schedule.indices[1]
        };

        if should_defer {
            state.deferred.push((idx, frame, payload));
        } else {
            if voting_complete && !state.deferred.is_empty() {
                let ann = state.grid_overlay();
                for (d_idx, d_frame, d_payload) in state.deferred.drain(..) {
                    let gi = state.detection.get(d_idx);
                    ready.push(ReadyItem {
                        idx: d_idx,
                        frame: d_frame,
                        metadata: gi,
                        grid_annotation: ann.clone(),
                        payload: d_payload,
                    });
                }
            }
            let gi = state.detection.get(idx);
            let ann = state.grid_overlay();
            ready.push(ReadyItem {
                idx,
                frame,
                metadata: gi,
                grid_annotation: ann,
                payload,
            });
        }

        ready
    }

    /// Tie-break with `trigger_image` and flush all deferred items. Use
    /// when an early-stop condition is detected on an item that the caller
    /// will NOT emit itself (e.g. a rarity cutoff trigger).
    ///
    /// Call [`Self::finish_additional_passes`] first when traversal stopped
    /// before all scheduled grid passes were reached.
    pub fn early_stop_flush(&mut self) -> Vec<ReadyItem<T>> {
        let state = match self.state.as_mut() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let ann = state.grid_overlay();
        let mut ready = Vec::with_capacity(state.deferred.len());
        for (d_idx, d_frame, d_payload) in state.deferred.drain(..) {
            let gi = state.detection.get(d_idx);
            ready.push(ReadyItem {
                idx: d_idx,
                frame: d_frame,
                metadata: gi,
                grid_annotation: ann.clone(),
                payload: d_payload,
            });
        }
        ready
    }

    /// Final flush at end-of-scan after callers have finished any additional
    /// grid passes.
    pub fn final_flush(&mut self) -> Vec<ReadyItem<T>> {
        let state = match self.state.as_mut() {
            Some(s) => s,
            None => return Vec::new(),
        };
        let ann = state.grid_overlay();
        let mut ready = Vec::with_capacity(state.deferred.len());
        for (d_idx, d_frame, d_payload) in state.deferred.drain(..) {
            let gi = state.detection.get(d_idx);
            ready.push(ReadyItem {
                idx: d_idx,
                frame: d_frame,
                metadata: gi,
                grid_annotation: ann.clone(),
                payload: d_payload,
            });
        }
        ready
    }
}

#[cfg(test)]
mod tests {
    use image::RgbImage;

    use super::{GridMode, GridVoteSchedule, PagedGridVoter};
    use crate::scanner::common::capture_frame::CaptureFrame;
    use crate::scanner::common::coord_scaler::CoordScaler;

    #[test]
    fn preserves_full_page_vote_positions() {
        assert_eq!(GridVoteSchedule::for_page(40).indices, [0, 13, 26]);
    }

    #[test]
    fn spreads_votes_across_short_pages() {
        assert_eq!(GridVoteSchedule::for_page(25).indices, [0, 8, 16]);
        assert_eq!(GridVoteSchedule::for_page(10).indices, [0, 3, 6]);
        assert_eq!(GridVoteSchedule::for_page(3).indices, [0, 1, 2]);
    }

    #[test]
    fn handles_pages_with_fewer_than_three_items() {
        assert_eq!(GridVoteSchedule::for_page(2).indices, [0, 1, 1]);
        assert_eq!(GridVoteSchedule::for_page(1).indices, [0, 0, 0]);
        assert_eq!(GridVoteSchedule::for_page(0).indices, [0, 0, 0]);
    }

    #[test]
    fn cropped_detail_needs_separate_grid_evidence_and_has_no_grid_overlay() {
        let scaler = CoordScaler::new(1920, 1080);
        let detail = || CaptureFrame {
            image: RgbImage::new(20, 20),
            origin: (1310.0, 110.0),
        };
        let mut voter = PagedGridVoter::new(40, GridMode::Artifact);
        let first = voter.record_with_layout(0, detail(), (), &scaler, 0, 40, 0, None);
        assert!(first[0].metadata.is_none());
        let grid = RgbImage::new(1310, 1080);
        let next = voter.record_with_layout(13, detail(), (), &scaler, 0, 40, 0, Some(&grid));
        assert!(next[0].metadata.is_some());
        assert!(next[0].grid_annotation.is_none());
    }

    #[test]
    fn authoritative_short_tail_layout_drives_voting() {
        let scaler = CoordScaler::new(1920, 1080);
        let frame = || CaptureFrame::full(RgbImage::new(1920, 1080));
        let mut voter = PagedGridVoter::new(10_000, GridMode::Artifact);

        assert!(voter
            .record_with_layout(40, frame(), (), &scaler, 40, 16, 3, None)
            .is_empty());
        {
            let state = voter.state.as_ref().unwrap();
            assert_eq!(state.page_start, 40);
            assert_eq!(state.page_items, 16);
            assert_eq!(state.screen_start_row, 3);
            assert_eq!(state.schedule.indices, [0, 5, 10]);
            assert_eq!(state.detection.pass_count(), 1);
        }

        assert!(voter
            .record_with_layout(45, frame(), (), &scaler, 40, 16, 3, None)
            .is_empty());
        let ready = voter.record_with_layout(50, frame(), (), &scaler, 40, 16, 3, None);
        assert_eq!(
            ready.iter().map(|item| item.idx).collect::<Vec<_>>(),
            [40, 45, 50]
        );
        assert_eq!(voter.state.as_ref().unwrap().detection.pass_count(), 3);
    }
}
