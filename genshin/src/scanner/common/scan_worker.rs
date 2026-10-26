use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use yas::{log_debug, log_error};

use super::capture_frame::CaptureFrame;
use super::grid_voter::GridAnnotation;

/// A work item sent from the capture thread to the worker pool.
pub struct WorkItem<M: Send> {
    pub index: usize,
    pub frame: CaptureFrame,
    pub metadata: M,
    /// Grid overlay annotation data for the page (for debug image dumps).
    pub grid_annotation: Option<GridAnnotation>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_ocr_failure_is_focused_after_capture_has_closed() {
        let (release_tx, release_rx) = crossbeam_channel::bounded::<()>(0);
        let (sender, worker) =
            start_worker(true, move |_: WorkItem<()>| -> anyhow::Result<Option<()>> {
                release_rx.recv().unwrap();
                anyhow::bail!("late OCR failure");
            });
        sender
            .send(WorkItem {
                index: 0,
                frame: CaptureFrame::full(image::RgbImage::new(1, 1)),
                metadata: (),
                grid_annotation: None,
            })
            .unwrap();
        drop(sender);
        release_tx.send(()).unwrap();
        let mut focused = None;
        let error = worker.join(|index| focused = Some(index)).unwrap_err();
        assert_eq!(focused, Some(0));
        assert!(error.to_string().contains("late OCR failure"));
    }

    #[test]
    fn individual_ocr_errors_continue_or_fail_the_scan_according_to_policy() {
        for stop in [false, true] {
            let (sender, worker) = start_worker(stop, |item: WorkItem<()>| {
                if item.index == 1 {
                    anyhow::bail!("failed field: main stat / OCR_MARKER");
                }
                Ok(Some(item.index))
            });
            for index in 0..3 {
                sender
                    .send(WorkItem {
                        index,
                        frame: CaptureFrame::full(image::RgbImage::new(1, 1)),
                        metadata: (),
                        grid_annotation: None,
                    })
                    .unwrap();
            }
            drop(sender);
            let mut failed_index = None;
            let result = worker.join(|index| failed_index = Some(index));
            if stop {
                assert!(result.unwrap_err().to_string().contains("OCR_MARKER"));
                assert_eq!(failed_index, Some(1));
            } else {
                assert_eq!(failed_index, None);
                let (items, indices) = result.unwrap();
                assert_eq!(items, [0, 2]);
                assert_eq!(indices, [0, 2]);
            }
        }
    }

    #[test]
    fn continue_policy_does_not_stop_after_a_run_of_ocr_errors() {
        let (sender, worker) = start_worker(false, |item: WorkItem<()>| {
            if item.index < 12 {
                anyhow::bail!("unreadable item");
            }
            Ok(Some(item.index))
        });
        let stop = worker.should_stop.clone();
        for index in 0..14 {
            sender
                .send(WorkItem {
                    index,
                    frame: CaptureFrame::full(image::RgbImage::new(1, 1)),
                    metadata: (),
                    grid_annotation: None,
                })
                .unwrap();
        }
        drop(sender);
        let (items, indices) = worker
            .join(|_| panic!("continue policy focused an error"))
            .unwrap();
        assert!(!stop.load(Ordering::Relaxed));
        assert_eq!(items, [12, 13]);
        assert_eq!(indices, [12, 13]);
    }

    #[test]
    fn skipped_capture_indices_preserve_later_successes_and_ocr_error_policy() {
        let (sender, worker) = start_worker(false, |item: WorkItem<()>| {
            if item.index == 5 {
                anyhow::bail!("unreadable captured artifact");
            }
            Ok(Some(item.index))
        });
        // Missing 0/1 and holes in the middle model skipped readiness failures.
        for index in [2, 5, 8] {
            sender
                .send(WorkItem {
                    index,
                    frame: CaptureFrame::full(image::RgbImage::new(1, 1)),
                    metadata: (),
                    grid_annotation: None,
                })
                .unwrap();
        }
        drop(sender);
        let (items, indices) = worker
            .join(|_| panic!("continue policy relocated the mouse"))
            .unwrap();
        assert_eq!(items, [2, 8]);
        assert_eq!(indices, [2, 8]);
    }

    #[test]
    fn stop_policy_reports_an_ocr_failure_after_a_skipped_capture() {
        let (sender, worker) =
            start_worker(true, |_: WorkItem<()>| -> anyhow::Result<Option<()>> {
                anyhow::bail!("failure after missing screenshot");
            });
        sender
            .send(WorkItem {
                index: 5,
                frame: CaptureFrame::full(image::RgbImage::new(1, 1)),
                metadata: (),
                grid_annotation: None,
            })
            .unwrap();
        drop(sender);
        let mut focused = None;
        let error = worker.join(|index| focused = Some(index)).unwrap_err();
        assert_eq!(focused, Some(5));
        assert!(error
            .to_string()
            .contains("failure after missing screenshot"));
    }
}

/// Handle to a running worker. Call `join()` to wait for results.
pub struct WorkerHandle<R> {
    handle: std::thread::JoinHandle<(Vec<R>, Vec<usize>)>,
    /// Set when an OCR error occurs and stop-on-failure is enabled.
    /// The capture thread should check this periodically and stop.
    pub should_stop: Arc<AtomicBool>,
    failure: Arc<Mutex<Option<(usize, String)>>>,
}

impl<R> WorkerHandle<R> {
    /// Wait for the worker to finish and return ordered results plus index map.
    ///
    /// Returns `(items, index_map)` where `index_map[i]` is the original
    /// work item index that produced `items[i]`. This allows correlating
    /// output positions with debug image folder names.
    /// After all OCR completes, let the controller focus the failed item before
    /// returning the error. This also covers errors arriving after capture ends.
    pub fn join(self, focus_failure: impl FnOnce(usize)) -> anyhow::Result<(Vec<R>, Vec<usize>)> {
        let output = self
            .handle
            .join()
            .expect("工作线程崩溃 / Worker thread panicked");
        if let Some((index, error)) = self.failure.lock().unwrap().take() {
            focus_failure(index);
            anyhow::bail!("识别失败时停止 / Stopped on OCR error: {error}");
        }
        Ok(output)
    }

    /// Check if the worker has signaled that scanning should stop.
    pub fn stop_requested(&self) -> bool {
        self.should_stop.load(Ordering::Relaxed)
    }
}

enum WorkerEvent<R> {
    ItemProcessed(usize, anyhow::Result<Option<R>>),
    CaptureFinished { total: usize },
}

/// Announce the handoff from controller-driven capture to background OCR.
///
/// Call this only after the controller has submitted every screenshot for the
/// current phase. `processed` may be non-zero because OCR runs concurrently.
pub fn log_capture_finished(processed: usize, total: usize) {
    if processed < total {
        log_debug!(
            "[OCR] 本轮截图采集已完成，OCR仍在处理中，请等待：{}/{}",
            "[OCR] this screenshot capture phase is complete; OCR is still processing, please wait: {}/{}",
            processed,
            total
        );
    } else {
        log_debug!(
            "[OCR] 本轮截图采集已完成，OCR处理进度：{}/{}",
            "[OCR] this screenshot capture phase is complete; OCR processing progress: {}/{}",
            processed,
            total
        );
    }
}

/// Log OCR progress after [`log_capture_finished`] has announced the handoff.
pub fn log_ocr_progress(processed: usize, total: usize) {
    log_debug!(
        "[OCR] 处理进度：{}/{}",
        "[OCR] processing progress: {}/{}",
        processed,
        total
    );
}

/// Start a parallel scan worker.
///
/// Items are received via the returned sender, dispatched to rayon for
/// parallel processing, and results collected in index order.
///
/// The `process_fn` receives a `WorkItem` and returns:
/// - `Ok(Some(result))` — include in output
/// - `Ok(None)` — skip this item (e.g., non-artifact)
/// - `Err(e)` — log error, skip item
///
/// OCR progress callbacks drive the GUI while scanning. Optional debug logs
/// retain timing details for developer investigations without duplicating the
/// ordinary progress display.
pub fn start_worker<M, R, F>(
    stop_on_failure: bool,
    process_fn: F,
) -> (crossbeam_channel::Sender<WorkItem<M>>, WorkerHandle<R>)
where
    M: Send + 'static,
    R: Send + 'static,
    F: Fn(WorkItem<M>) -> anyhow::Result<Option<R>> + Send + Sync + 'static,
{
    // Bounded channel prevents memory blowup if OCR falls behind capture.
    // Buffer of 16 items ≈ 16 × ~1MB = ~16MB max in-flight images.
    let (item_tx, item_rx) = crossbeam_channel::bounded::<WorkItem<M>>(16);
    let should_stop = Arc::new(AtomicBool::new(false));
    let should_stop_clone = should_stop.clone();
    let failure = Arc::new(Mutex::new(None));
    let worker_failure = failure.clone();

    let handle = std::thread::spawn(move || {
        let _native_crash_context = yas::native_crash::inherit_current_task();
        let process_fn = Arc::new(process_fn);
        let dispatch_stop = should_stop_clone.clone();

        // Result channel: rayon tasks send completed items here. The dispatch
        // thread sends CaptureFinished only after the screenshot sender closes,
        // so no OCR progress is logged while the controller is still active.
        let (result_tx, result_rx) = crossbeam_channel::unbounded::<WorkerEvent<R>>();

        // Dispatch: receive items and spawn rayon tasks
        let dispatch_result_tx = result_tx.clone();
        let dispatch_handle = std::thread::spawn(move || {
            let _native_crash_context = yas::native_crash::inherit_current_task();
            let mut total = 0;
            for item in item_rx {
                total += 1;
                let process_fn = process_fn.clone();
                let tx = dispatch_result_tx.clone();
                let index = item.index;
                let dispatch_stop = dispatch_stop.clone();
                rayon::spawn(move || {
                    let _native_crash_context = yas::native_crash::inherit_current_task();
                    let result = if stop_on_failure && dispatch_stop.load(Ordering::Relaxed) {
                        Ok(None)
                    } else {
                        let result = process_fn(item);
                        if stop_on_failure && result.is_err() {
                            dispatch_stop.store(true, Ordering::Relaxed);
                        }
                        result
                    };
                    let _ = tx.send(WorkerEvent::ItemProcessed(index, result));
                });
            }
            let _ = dispatch_result_tx.send(WorkerEvent::CaptureFinished { total });
            // Drop our sender so result_rx eventually closes
            drop(dispatch_result_tx);
        });
        // Drop the original sender clone
        drop(result_tx);

        // Collection: reorder results via BTreeMap
        let mut results_map: BTreeMap<usize, anyhow::Result<Option<R>>> = BTreeMap::new();
        let mut next_index: usize = 0;
        let mut output: Vec<R> = Vec::new();
        let mut index_map: Vec<usize> = Vec::new();
        let mut processed: usize = 0;
        let mut capture_total: Option<usize> = None;
        let mut collect = |current_index, result: anyhow::Result<Option<R>>| match result {
            Ok(Some(item)) => {
                output.push(item);
                index_map.push(current_index);
            },
            Ok(None) => {},
            Err(e) => {
                log_error!(
                    "[worker] 第{}项错误: {:#}",
                    "[worker] item {} error: {:#}",
                    current_index,
                    e
                );
                if stop_on_failure {
                    worker_failure.lock().unwrap().get_or_insert_with(|| {
                        (current_index, format!("item {current_index}: {e:#}"))
                    });
                    should_stop_clone.store(true, Ordering::Relaxed);
                }
            },
        };

        for event in result_rx {
            match event {
                WorkerEvent::ItemProcessed(index, result) => {
                    processed += 1;
                    results_map.insert(index, result);

                    if let Some(total) = capture_total {
                        log_ocr_progress(processed, total);
                    }

                    // Drain consecutive ready results
                    while let Some(result) = results_map.remove(&next_index) {
                        let current_index = next_index;
                        next_index += 1;

                        collect(current_index, result);
                    }
                },
                WorkerEvent::CaptureFinished { total } => {
                    capture_total = Some(total);
                    log_capture_finished(processed, total);
                },
            }
        }

        // Some grid positions never produce work (unconfirmed panel/capture).
        // At EOF there can be legitimate holes, so drain all remaining keys in
        // order rather than waiting forever for a missing screenshot index.
        for (index, result) in results_map {
            collect(index, result);
        }

        let _ = dispatch_handle.join();

        (output, index_map)
    });

    (
        item_tx,
        WorkerHandle {
            handle,
            should_stop,
            failure,
        },
    )
}
