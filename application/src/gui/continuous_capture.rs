//! Server-owned packet listener. Drop closes the command channel and joins
//! the monitor, including native pktmon cleanup, before the manager exits.
use genshin_scanner::continuous::{ContinuousCapture, ExportTimer};
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::JoinHandle,
};

pub struct Session {
    pub latest: ContinuousCapture,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    fn spawn(
        run: impl FnOnce(ContinuousCapture, Arc<AtomicBool>) -> anyhow::Result<()> + Send + 'static,
    ) -> Self {
        let latest = ContinuousCapture::default();
        let stop = Arc::new(AtomicBool::new(false));
        let store = latest.clone();
        let stopped = stop.clone();
        let failure = latest.clone();
        let thread = std::thread::Builder::new()
            .name("continuous-capture".into())
            .spawn(move || {
                let result =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(store, stopped)));
                let error = match result {
                    Ok(Ok(())) => None,
                    Ok(Err(error)) => Some(format!("{error:#}")),
                    Err(_) => Some("continuous capture worker panicked".into()),
                };
                if let Some(error) = error {
                    yas::log_error!(
                        "持续抓包失败：{}",
                        "Continuous capture failed: {}",
                        yas::lang::localize(&error)
                    );
                    failure.fail(error);
                }
            });
        let thread = match thread {
            Ok(thread) => Some(thread),
            Err(error) => {
                latest.fail(error.to_string());
                None
            },
        };
        Self {
            latest,
            stop,
            thread,
        }
    }

    pub fn genshin(dir: PathBuf) -> Self {
        Self::spawn(move |latest, stop| {
            let runtime = tokio::runtime::Runtime::new()?;
            let state = Arc::new(Mutex::new(Default::default()));
            let monitor = genshin_scanner::capture::monitor::CaptureMonitor::new(state, false)?;
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            runtime.block_on(drive(
                monitor.run_continuous(receiver, latest.clone()),
                sender,
                stop,
                latest,
                dir.join("genshin_export_latest.json"),
            ))?;
            Ok(())
        })
    }

    pub fn star_rail(dir: PathBuf) -> Self {
        Self::spawn(move |latest, stop| {
            use hsr_scanner::packet_capture::{
                CaptureTargets, HsrCaptureCommand, HsrCaptureMonitor, HsrCaptureState,
            };
            let runtime = tokio::runtime::Runtime::new()?;
            let state = Arc::new(Mutex::new(HsrCaptureState::default()));
            let references = super::star_rail_worker::load_references()?;
            let export_references = references.clone();
            let observer = latest.clone();
            let observed_generation = std::cell::Cell::new(0);
            let monitor = HsrCaptureMonitor::new(
                state.clone(),
                references,
                CaptureTargets {
                    characters: true,
                    light_cones: true,
                    relics: true,
                    achievements: false,
                },
            )?
            .with_continuous(move |state| {
                if observed_generation.replace(state.session_generation) != state.session_generation {
                    observer.reset();
                }
                if let Some(error) = &state.error {
                    observer.fail(error.to_string());
                } else if state.complete {
                    match state.build_scanner_export(&export_references) {
                        Ok(data) => observer.publish(data),
                        Err(error) => observer.fail(error.to_string()),
                    }
                } else {
                    observer.clear();
                }
            });
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            sender.send(HsrCaptureCommand::StartCapture)?;
            runtime.block_on(drive(
                async move {
                    monitor.run(receiver).await;
                    match state.lock().unwrap().error.as_ref() {
                        Some(error) => Err(anyhow::anyhow!(error.to_string())),
                        None => Ok(()),
                    }
                },
                sender,
                stop,
                latest,
                dir.join("star_rail_export_latest.json"),
            ))?;
            Ok(())
        })
    }
}

async fn drive<T, R>(
    future: impl std::future::Future<Output = anyhow::Result<R>>,
    sender: tokio::sync::mpsc::UnboundedSender<T>,
    stop: Arc<AtomicBool>,
    latest: ContinuousCapture,
    path: PathBuf,
) -> anyhow::Result<R> {
    tokio::pin!(future);
    let mut timer = tokio::time::interval(std::time::Duration::from_millis(200));
    let mut export = ExportTimer::default();
    loop {
        tokio::select! {
            result = &mut future => return result,
            _ = timer.tick() => {
                if stop.load(Ordering::Relaxed) { break; }
                export.tick(&latest, &path);
            }
        }
    }
    drop(sender);
    future.await
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
