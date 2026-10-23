use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
};

use eframe::egui;
use hsr_scanner::{
    packet_capture::{CaptureTargets, HsrCaptureCommand, HsrCaptureMonitor, HsrCaptureState},
    reference::ReferenceCache,
    HsrError, LocalizedText,
};

use crate::config::StarRailSettings;

use super::{
    star_rail_worker,
    state::{Lang, TaskKind, UiError, UiText},
    widgets, worker,
};

pub struct StarRailCaptureHandle {
    thread: std::thread::JoinHandle<()>,
    command_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<HsrCaptureCommand>>>,
    startup_gate: Arc<CaptureStartupGate>,
    native_crash: Arc<worker::NativeCrashState>,
    native_failure: Mutex<Option<UiError>>,
}

impl StarRailCaptureHandle {
    fn stop(&self) {
        self.startup_gate
            .request_stop(|| match self.command_tx.lock() {
                Ok(mut sender) => {
                    if let Some(sender) = sender.take() {
                        let _ = sender.send(HsrCaptureCommand::StopCapture);
                    }
                },
                Err(poisoned) => {
                    self.command_tx.clear_poison();
                    if let Some(sender) = poisoned.into_inner().take() {
                        let _ = sender.send(HsrCaptureCommand::StopCapture);
                    }
                },
            });
    }

    fn surface_native_failure(&self, phase: Option<UiText>) {
        if !self.native_crash.has_occurred() {
            return;
        }
        let Some(exception) = self.native_crash.claim_exception(TaskKind::Capture, phase) else {
            return;
        };
        worker::deactivate_native_crash(&self.native_crash);
        self.stop();
        let error = UiError::native_exception(exception);
        match self.native_failure.lock() {
            Ok(mut slot) => *slot = Some(error.clone()),
            Err(poisoned) => {
                self.native_failure.clear_poison();
                *poisoned.into_inner() = Some(error.clone());
            },
        }
        error.log_diagnostics();
    }

    fn is_finished(&self) -> bool {
        self.surface_native_failure(None);
        self.native_failure().is_some() || self.thread.is_finished()
    }

    fn native_failure(&self) -> Option<UiError> {
        match self.native_failure.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => {
                self.native_failure.clear_poison();
                poisoned.into_inner().clone()
            },
        }
    }
}

/// Serializes the user's Stop request with the worker's first Start command.
/// If Stop wins, the native packet-capture boundary is never opened. If Start
/// wins, Stop is necessarily queued after it and the monitor cancels normally.
#[derive(Default)]
struct CaptureStartupGate {
    serial: Mutex<()>,
    stopped: AtomicBool,
}

impl CaptureStartupGate {
    fn request_stop(&self, stop: impl FnOnce()) {
        let _guard = lock_shared(&self.serial);
        self.stopped.store(true, Ordering::SeqCst);
        stop();
    }

    fn start_if_allowed(&self, start: impl FnOnce() -> bool) -> bool {
        let _guard = lock_shared(&self.serial);
        !self.stopped.load(Ordering::SeqCst) && start()
    }
}

#[cfg(feature = "test-as-invoker")]
#[doc(hidden)]
pub fn stop_before_worker_start_suppresses_start_for_test() -> bool {
    let startup_gate = CaptureStartupGate::default();
    let start_attempted = AtomicBool::new(false);
    startup_gate.request_stop(|| {});
    let start_sent = startup_gate.start_if_allowed(|| {
        start_attempted.store(true, Ordering::SeqCst);
        true
    });
    !start_sent && !start_attempted.load(Ordering::SeqCst)
}

struct PendingExport {
    receiver: mpsc::Receiver<Result<super::star_rail_exports::CaptureFiles, UiError>>,
    thread: std::thread::JoinHandle<()>,
    result: Option<Result<super::star_rail_exports::CaptureFiles, UiError>>,
}

#[derive(Clone, Debug, PartialEq)]
enum CapturePhase {
    Idle,
    Initializing,
    Waiting,
    Stopping,
    Stopped,
    Exporting,
    Done { summary: UiText, path: String },
    Failed(UiError),
}

pub struct StarRailCaptureState {
    handle: Option<StarRailCaptureHandle>,
    shared: Arc<Mutex<HsrCaptureState>>,
    references: Arc<Mutex<Option<ReferenceCache>>>,
    pending_export: Option<PendingExport>,
    phase: CapturePhase,
    output_dir: String,
    only_keep_latest_export: bool,
}

impl StarRailCaptureState {
    #[cfg(feature = "dev-tools")]
    pub(super) fn preview_feedback(&mut self, scenario: &str) {
        if scenario == "done" {
            let mut shared = self.shared.lock().unwrap();
            shared.has_key_exchange = true;
            shared.has_characters = true;
            shared.has_light_cones = true;
            shared.has_relics = true;
            shared.has_achievements = true;
            shared.character_count = 58;
            shared.light_cone_count = 198;
            shared.relic_count = 1126;
            shared.achievement_count = 600;
        }
        self.phase = match scenario {
            "error" => CapturePhase::Failed(super::preview::preview_error()),
            "stopped" => CapturePhase::Stopped,
            "exporting" => CapturePhase::Exporting,
            "login" => CapturePhase::Waiting,
            "done" => CapturePhase::Done {
                summary: UiText::new("抓包已完成", "Capture completed"),
                path: "C:\\Users\\Player\\Exports\\capture.json".into(),
            },
            _ => return,
        };
    }

    pub fn new(output_dir: String) -> Self {
        Self {
            handle: None,
            shared: Arc::new(Mutex::new(HsrCaptureState::default())),
            references: Arc::new(Mutex::new(None)),
            pending_export: None,
            phase: CapturePhase::Idle,
            output_dir,
            only_keep_latest_export: false,
        }
    }

    pub fn is_busy(&self) -> bool {
        let phase_busy = matches!(
            self.phase,
            CapturePhase::Initializing
                | CapturePhase::Waiting
                | CapturePhase::Stopping
                | CapturePhase::Exporting
        );
        let monitor_running = self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        let export_running = self
            .pending_export
            .as_ref()
            .is_some_and(|pending| !pending.thread.is_finished());
        phase_busy || monitor_running || export_running
    }

    /// Advance capture, stop, and export transitions independently from the
    /// visible tab. The app calls this every frame so switching to another
    /// Star Rail tab cannot strand a completed monitor or its export.
    pub fn tick(&mut self) {
        let previous = self.phase.clone();
        update_phase(self);
        if self.phase != previous {
            if let CapturePhase::Failed(error) = &self.phase {
                error.log_diagnostics();
            }
        }
    }

    pub fn requires_restart(&self) -> bool {
        self.handle
            .as_ref()
            .and_then(StarRailCaptureHandle::native_failure)
            .is_some()
    }

    fn native_failure(&self) -> Option<UiError> {
        let phase = match self.phase {
            CapturePhase::Initializing => Some(UiText::new(
                "正在初始化星穹铁道抓包",
                "Initializing Star Rail capture",
            )),
            CapturePhase::Waiting => Some(UiText::new(
                "正在等待星穹铁道数据",
                "Waiting for Star Rail data",
            )),
            CapturePhase::Stopping => Some(UiText::new(
                "正在停止星穹铁道抓包",
                "Stopping Star Rail capture",
            )),
            CapturePhase::Exporting => Some(UiText::new(
                "正在导出星穹铁道数据",
                "Exporting Star Rail data",
            )),
            CapturePhase::Idle
            | CapturePhase::Stopped
            | CapturePhase::Done { .. }
            | CapturePhase::Failed(_) => None,
        };
        self.handle.as_ref().and_then(|handle| {
            handle.surface_native_failure(phase);
            handle.native_failure()
        })
    }

    #[cfg(feature = "test-as-invoker")]
    #[doc(hidden)]
    pub fn inject_completed_for_test(
        &mut self,
        references: ReferenceCache,
        completed_ids: Vec<u32>,
        inventory: hsr_scanner::ObservationSnapshot,
    ) {
        let achievement_count = completed_ids.len();
        *lock_shared(&self.shared) = HsrCaptureState {
            capturing: false,
            complete: true,
            has_key_exchange: true,
            achievement_count,
            quests: completed_ids
                .into_iter()
                .map(|id| hsr_scanner::model::ObservedQuest {
                    id,
                    status: 3,
                    progress: 1,
                    counters: vec![],
                })
                .collect(),
            inventory: Some(inventory),
            has_achievements: true,
            ..HsrCaptureState::default()
        };
        *lock_shared(&self.references) = Some(references);
        self.phase = CapturePhase::Initializing;
    }

    #[cfg(feature = "test-as-invoker")]
    #[doc(hidden)]
    pub fn completed_export_path_for_test(&self) -> Option<&str> {
        match &self.phase {
            CapturePhase::Done { path, .. } => Some(path.as_str()),
            CapturePhase::Failed(error) => panic!("capture export failed: {error:?}"),
            _ => None,
        }
    }
}

pub fn show_settings(
    ui: &mut egui::Ui,
    lang: Lang,
    settings: &mut StarRailSettings,
    state: &mut StarRailCaptureState,
    is_busy: bool,
) {
    let game_busy = false;
    let _ = is_busy;

    widgets::section(ui, lang.t("导出内容", "Export targets"), |ui| {
        ui.add_enabled_ui(!state.is_busy() && !game_busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
                ui.checkbox(
                    &mut settings.capture_include_characters,
                    lang.t("角色", "Characters"),
                );
                ui.checkbox(
                    &mut settings.capture_include_light_cones,
                    lang.t("光锥", "Light Cones"),
                );
                ui.checkbox(
                    &mut settings.capture_include_relics,
                    lang.t("遗器与饰品", "Relics"),
                );
                ui.checkbox(
                    &mut settings.capture_include_achievements,
                    lang.t("成就", "Achievements"),
                );
            });
        });
    });
    widgets::fold(ui, lang.t("高级选项", "Advanced"), |ui| {
        ui.add_enabled_ui(!state.is_busy() && !game_busy, |ui| {
            ui.checkbox(
                &mut settings.capture_dump_packets,
                lang.t("保存解密数据包", "Save decoded packets"),
            );
        });
    });
}

pub fn show_status(
    ui: &mut egui::Ui,
    lang: Lang,
    settings: &mut StarRailSettings,
    state: &mut StarRailCaptureState,
    game_busy: bool,
    restart_required: bool,
) {
    if restart_required {
        super::theme::restart_required(
            ui,
            lang,
            state
                .handle
                .as_ref()
                .and_then(StarRailCaptureHandle::native_failure)
                .as_ref(),
        );
        return;
    }
    let shared = lock_shared(&state.shared).clone();
    use super::capture_status::{self, Stage};
    let stage = match &state.phase {
        CapturePhase::Stopped => Stage::Stopped,
        CapturePhase::Idle => Stage::Ready,
        CapturePhase::Initializing => Stage::Starting,
        CapturePhase::Waiting => capture_status::waiting(
            shared.has_key_exchange,
            (settings.capture_include_characters && !shared.has_characters)
                || (settings.capture_include_light_cones && !shared.has_light_cones)
                || (settings.capture_include_relics && !shared.has_relics),
            settings.capture_include_achievements && !shared.has_achievements,
        ),
        CapturePhase::Stopping => Stage::Stopping,
        CapturePhase::Exporting => Stage::Exporting,
        CapturePhase::Done { .. } => Stage::Done,
        CapturePhase::Failed(_) => Stage::Failed,
    };
    let text = if stage == Stage::Achievements {
        lang.t("正在接收成就", "Receiving achievements")
    } else {
        stage.headline(lang)
    };
    let feedback = match &state.phase {
        CapturePhase::Idle if !capture_targets(settings).any() => {
            Some(super::state::TaskStatus::AwaitingInput(UiText::new(
                "请选择导出内容",
                "Select export targets",
            )))
        },
        CapturePhase::Failed(error) => Some(super::state::TaskStatus::Failed(error.clone())),
        CapturePhase::Done { summary, path } => Some(super::state::TaskStatus::Exported {
            message: summary.clone(),
            path: path.clone(),
            partial: false,
        }),
        CapturePhase::Stopped => Some(super::state::TaskStatus::Stopped(UiText::new(text, text))),
        _ => None,
    };
    widgets::status_content(ui, "hsr-capture-status", 1, |ui| {
        super::theme::task_status(ui, lang, feedback.as_ref(), text);
        if stage != Stage::Ready && stage != Stage::Done && capture_targets(settings).any() {
            capture_status::show_connection(ui, lang, stage, shared.has_key_exchange);
            capture_status::show_counters(
                ui,
                lang,
                shared.elapsed_secs,
                shared.packet_count,
                shared.command_count,
            );
        }
        for (selected, key, zh, en, complete, count) in [
            (
                settings.capture_include_characters,
                "characters",
                "角色",
                "Characters",
                shared.has_characters,
                shared.character_count,
            ),
            (
                settings.capture_include_light_cones,
                "light_cones",
                "光锥",
                "Light Cones",
                shared.has_light_cones,
                shared.light_cone_count,
            ),
            (
                settings.capture_include_relics,
                "gear",
                "遗器与位面饰品",
                "Relics & Ornaments",
                shared.has_relics,
                shared.relic_count,
            ),
            (
                settings.capture_include_achievements,
                "achievements",
                "成就",
                "Achievements",
                shared.has_achievements,
                shared.achievement_count,
            ),
        ] {
            if selected && stage != Stage::Ready {
                let mut step = super::task_progress::Step::new(key, zh, en);
                step.completed = count;
                if complete {
                    step.state = super::task_progress::StepState::Complete;
                } else if matches!(state.phase, CapturePhase::Failed(_) | CapturePhase::Stopped) {
                    step.state = super::task_progress::StepState::Interrupted;
                } else if matches!(state.phase, CapturePhase::Waiting) && shared.has_key_exchange {
                    step.state = super::task_progress::StepState::Running;
                }
                super::task_progress::row(ui, lang, &step);
            }
        }
    });
    ui.add_space(12.0);
    ui.add_enabled_ui(!state.is_busy() && !game_busy, |ui| {
        ui.checkbox(
            &mut settings.capture_only_keep_latest_export,
            lang.t("仅保留最新导出", "Keep latest export only"),
        );
    });
    ui.add_space(4.0);
    widgets::export_action(ui, lang, feedback.as_ref());
    match &state.phase {
        CapturePhase::Initializing | CapturePhase::Waiting => {
            if super::theme::primary_action(ui, true, lang.t("停止抓包", "Stop capture")).clicked()
            {
                if let Some(h) = &state.handle {
                    h.stop();
                }
                state.phase = CapturePhase::Stopping;
            }
        },
        CapturePhase::Stopping | CapturePhase::Exporting => {
            super::theme::primary_action(ui, false, lang.t("正在处理", "Processing"));
        },
        CapturePhase::Failed(_) => {
            if super::theme::primary_action(
                ui,
                !game_busy && !state.is_busy() && capture_targets(settings).any(),
                lang.t("重试", "Retry"),
            )
            .clicked()
            {
                start_capture(settings, state);
            }
        },
        CapturePhase::Idle | CapturePhase::Stopped | CapturePhase::Done { .. } => {
            if super::theme::primary_action(
                ui,
                !game_busy && !state.is_busy() && capture_targets(settings).any(),
                if matches!(state.phase, CapturePhase::Done { .. }) {
                    lang.t("再次抓包", "Capture again")
                } else {
                    lang.t("开始抓包", "Start capture")
                },
            )
            .clicked()
            {
                start_capture(settings, state);
            }
        },
    }
}

fn capture_targets(settings: &StarRailSettings) -> CaptureTargets {
    CaptureTargets {
        characters: settings.capture_include_characters,
        light_cones: settings.capture_include_light_cones,
        relics: settings.capture_include_relics,
        achievements: settings.capture_include_achievements,
    }
}

fn start_capture(settings: &StarRailSettings, state: &mut StarRailCaptureState) {
    // Freeze export destination at Start. Shared settings are disabled while
    // any Star Rail task runs, but this snapshot also protects future callers.
    state.output_dir = settings.export_directory().display().to_string();
    state.only_keep_latest_export = settings.capture_only_keep_latest_export;
    *lock_shared(&state.shared) = HsrCaptureState::default();
    *lock_shared(&state.references) = None;
    state.pending_export = None;
    state.phase = CapturePhase::Initializing;
    let native_crash = Arc::new(worker::NativeCrashState::new());
    let startup_gate = Arc::new(CaptureStartupGate::default());
    match spawn_capture_monitor(
        capture_targets(settings),
        settings.capture_dump_packets,
        state.shared.clone(),
        state.references.clone(),
        startup_gate.clone(),
        native_crash.clone(),
    ) {
        Ok((thread, command_tx)) => {
            state.handle = Some(StarRailCaptureHandle {
                thread,
                command_tx: Mutex::new(Some(command_tx)),
                startup_gate,
                native_crash,
                native_failure: Mutex::new(None),
            });
        },
        Err(error) => {
            error.log_diagnostics();
            state.phase = CapturePhase::Failed(error);
        },
    }
}

fn spawn_capture_monitor(
    targets: CaptureTargets,
    dump_packets: bool,
    shared: Arc<Mutex<HsrCaptureState>>,
    references_out: Arc<Mutex<Option<ReferenceCache>>>,
    startup_gate: Arc<CaptureStartupGate>,
    native_crash: Arc<worker::NativeCrashState>,
) -> Result<
    (
        std::thread::JoinHandle<()>,
        tokio::sync::mpsc::UnboundedSender<HsrCaptureCommand>,
    ),
    UiError,
> {
    let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();
    let ui_command_tx = command_tx.clone();
    let shared_for_thread = shared.clone();
    let thread_crash = native_crash.clone();
    let thread = std::thread::Builder::new()
        .name("star-rail-capture".to_owned())
        .spawn(move || {
            let native_guard_active = worker::activate_native_crash(&thread_crash);
            if !native_guard_active {
                lock_shared(&shared_for_thread).error = Some(HsrError::new(
                    "HSR-ACHIEVEMENT-CAPTURE-BUSY",
                    LocalizedText::new(
                        "另一个游戏数据任务仍在关闭。请稍候重试。",
                        "Another game-data task is still shutting down. Retry shortly.",
                    ),
                    "native crash boundary is still owned by another task",
                ));
                return;
            }
            let native_context =
                native_guard_active.then(yas::native_crash::inherit_current_task);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let references = match star_rail_worker::load_references() {
                    Ok(references) => references,
                    Err(error) => {
                        lock_shared(&shared_for_thread).error = Some(error);
                        return;
                    },
                };
                let monitor = match HsrCaptureMonitor::new(
                    shared_for_thread.clone(),
                    references.clone(),
                    targets,
                ) {
                    Ok(monitor) => monitor,
                    Err(error) => {
                        lock_shared(&shared_for_thread).error = Some(error);
                        return;
                    },
                };
                let monitor = if dump_packets {
                    monitor.with_packet_dump(genshin_scanner::cli::exe_dir().join("debug_capture").join("hsr"))
                } else { monitor };
                *lock_shared(&references_out) = Some(references);

                let runtime = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        lock_shared(&shared_for_thread).error = Some(HsrError::new(
                            "HSR-ACHIEVEMENT-CAPTURE-RUNTIME",
                            LocalizedText::new(
                                "抓包运行环境无法启动。请检查系统资源后重试。",
                                "The capture runtime could not start. Check system resources, then retry.",
                            ),
                            format!("tokio runtime construction failed; cause={error}"),
                        ));
                        return;
                    },
                };
                let start_sent = startup_gate.start_if_allowed(|| {
                    command_tx
                        .send(HsrCaptureCommand::StartCapture)
                        .is_ok()
                });
                drop(command_tx);
                if !start_sent {
                    return;
                }
                runtime.block_on(monitor.run(command_rx));
            }));
            if let Err(panic_info) = result {
                let details = if let Some(message) = panic_info.downcast_ref::<&str>() {
                    (*message).to_owned()
                } else if let Some(message) = panic_info.downcast_ref::<String>() {
                    message.clone()
                } else {
                    "unknown non-string panic payload".to_owned()
                };
                lock_shared(&shared_for_thread).error = Some(HsrError::new(
                    "HSR-ACHIEVEMENT-CAPTURE-PANIC",
                    LocalizedText::new(
                        "抓包任务意外停止。请重试，并在问题再次出现时复制完整错误。",
                        "The capture task stopped unexpectedly. Retry, and copy the full error if it happens again.",
                    ),
                    format!("capture monitor panicked; cause={details}"),
                ));
            }
            drop(native_context);
            if native_guard_active {
                worker::deactivate_native_crash(&thread_crash);
            }
        })
        .map_err(|error| {
            UiError::from_error(
                UiText::new(
                    "抓包后台任务无法启动。请检查系统资源后重试。",
                    "The capture background task could not start. Check system resources, then retry.",
                ),
                error,
            )
        })?;
    Ok((thread, ui_command_tx))
}

fn update_phase(state: &mut StarRailCaptureState) {
    if let Some(error) = state.native_failure() {
        state.phase = CapturePhase::Failed(error);
        return;
    }

    if state.phase == CapturePhase::Stopping {
        if let Some(error) = shared_snapshot(&state.shared).error {
            state.phase = CapturePhase::Failed(star_rail_worker::hsr_ui_error(
                UiText::new(
                    "星穹铁道抓包未能完成。",
                    "Star Rail capture could not finish.",
                ),
                error,
            ));
            return;
        }
        if state
            .handle
            .as_ref()
            .is_none_or(StarRailCaptureHandle::is_finished)
        {
            state.handle = None;
            state.phase = CapturePhase::Stopped;
        }
        return;
    }

    if state.phase == CapturePhase::Exporting {
        if let Some(pending) = &mut state.pending_export {
            if pending.result.is_none() {
                match pending.receiver.try_recv() {
                    Ok(result) => pending.result = Some(result),
                    Err(mpsc::TryRecvError::Disconnected) if pending.thread.is_finished() => {
                        pending.result = Some(Err(UiError::from_message(
                            UiText::new(
                                "数据导出任务意外停止。请复制完整错误并报告问题。",
                                "The data export task stopped unexpectedly. Copy the full error and report the problem.",
                            ),
                            "data export worker disconnected without returning a result",
                        )));
                    },
                    Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => {},
                }
            }
        }

        let export_finished = state
            .pending_export
            .as_ref()
            .is_some_and(|pending| pending.result.is_some() && pending.thread.is_finished());
        if export_finished {
            let pending = state
                .pending_export
                .take()
                .expect("finished export must still be retained");
            let result = pending
                .result
                .expect("finished retained export must have a result");
            let _ = pending.thread.join();
            match result {
                Ok(files) => {
                    let summary = UiText::new("已导出所选数据。", "Selected data exported.");
                    state.phase = CapturePhase::Done {
                        summary,
                        path: files
                            .paths
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join("\n→ "),
                    };
                },
                Err(error) => state.phase = CapturePhase::Failed(error),
            }
        }
        return;
    }

    // Completion remains in shared state until the next explicit Start. Only
    // an active capture may consume it, otherwise every frame re-exports it.
    if !matches!(
        state.phase,
        CapturePhase::Initializing | CapturePhase::Waiting
    ) {
        return;
    }

    let shared = shared_snapshot(&state.shared);
    if let Some(error) = shared.error {
        state.phase = CapturePhase::Failed(star_rail_worker::hsr_ui_error(
            UiText::new(
                "星穹铁道抓包未能完成。",
                "Star Rail capture could not finish.",
            ),
            error,
        ));
        if let Some(handle) = &state.handle {
            handle.stop();
        }
        return;
    }
    if shared.complete {
        if let Some(handle) = &state.handle {
            handle.stop();
        }
        let references = lock_shared(&state.references).clone();
        let Some(references) = references else {
            state.phase = CapturePhase::Failed(UiError::from_message(
                UiText::new(
                    "游戏数据已读取，但参考数据状态丢失，无法安全导出。请重新抓包。",
                    "Game data was read, but reference state was lost and cannot be exported safely. Capture again.",
                ),
                "complete achievement state has no retained ReferenceCache",
            ));
            return;
        };
        match spawn_export(
            shared,
            references,
            PathBuf::from(state.output_dir.trim()),
            state.only_keep_latest_export,
        ) {
            Ok(pending) => {
                state.pending_export = Some(pending);
                state.phase = CapturePhase::Exporting;
            },
            Err(error) => state.phase = CapturePhase::Failed(error),
        }
    } else if shared.capturing {
        state.phase = CapturePhase::Waiting;
    }
}

fn spawn_export(
    captured: HsrCaptureState,
    references: ReferenceCache,
    output_dir: PathBuf,
    only_latest: bool,
) -> Result<PendingExport, UiError> {
    std::fs::create_dir_all(&output_dir).map_err(|error| {
        UiError::from_error(
            UiText::new(
                "无法创建星穹铁道导出文件夹。请检查路径和文件权限。",
                "The Star Rail export folder could not be created. Check the path and file permissions.",
            ),
            error,
        )
    })?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread = std::thread::Builder::new()
        .name("star-rail-export".to_owned())
        .spawn(move || {
            let result = (|| {
                let export = captured.build_scanner_export(&references)
                    .map_err(|error| star_rail_worker::hsr_ui_error(UiText::new("无法生成星穹铁道导出文件。", "The Star Rail export could not be built."), error))?;
                super::star_rail_exports::write_capture_files(&output_dir, &export, only_latest)
                    .map_err(|e| star_rail_worker::hsr_ui_error(UiText::new(
                        "导出文件保存或旧文件清理失败。请查看完整错误中的文件路径。",
                        "Saving exports or removing older files failed. See the file paths in the full error."), e))
            })();
            let _ = sender.send(result);
        })
        .map_err(|error| {
            UiError::from_error(
                UiText::new(
                    "数据导出后台任务无法启动。请检查系统资源后重试。",
                    "The export background task could not start. Check system resources, then retry.",
                ),
                error,
            )
        })?;
    Ok(PendingExport {
        receiver,
        thread,
        result: None,
    })
}

fn shared_snapshot<T: Clone>(shared: &Mutex<T>) -> T {
    match shared.lock() {
        Ok(value) => value.clone(),
        Err(poisoned) => {
            shared.clear_poison();
            poisoned.into_inner().clone()
        },
    }
}

fn lock_shared<T>(shared: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match shared.lock() {
        Ok(value) => value,
        Err(poisoned) => {
            shared.clear_poison();
            poisoned.into_inner()
        },
    }
}
#[cfg(test)]
mod feedback_tests {
    use super::*;
    #[test]
    fn next_action_changes_only_after_relevant_capture_data_arrives() {
        use super::super::capture_status::feedback_tests::texts;
        let mut settings = StarRailSettings::default();
        let mut state = StarRailCaptureState::new(String::new());
        state.phase = CapturePhase::Waiting;
        let login = texts(|ui| show_status(ui, Lang::En, &mut settings, &mut state, false, false));
        assert!(login.iter().any(|s| s == "Launch the game and log in"));
        lock_shared(&state.shared).command_count = 1;
        let dispatch =
            texts(|ui| show_status(ui, Lang::En, &mut settings, &mut state, false, false));
        assert!(dispatch.iter().any(|s| s == "Launch the game and log in"));
        lock_shared(&state.shared).has_key_exchange = true;
        let inventory =
            texts(|ui| show_status(ui, Lang::En, &mut settings, &mut state, false, false));
        assert!(inventory.iter().any(|s| s == "Receiving game data"));
        assert!(!inventory.iter().any(|s| s.contains("Open Achievements")));
        {
            let mut shared = lock_shared(&state.shared);
            shared.has_characters = true;
            shared.has_light_cones = true;
            shared.has_relics = true;
        }
        let achievements =
            texts(|ui| show_status(ui, Lang::En, &mut settings, &mut state, false, false));
        assert!(achievements.iter().any(|s| s == "Receiving achievements"));
        assert!(!achievements.iter().any(|s| s.contains("Import the file")));
        state.phase = CapturePhase::Done {
            summary: UiText::new("已完成", "Completed"),
            path: "capture.json".into(),
        };
        let done = texts(|ui| show_status(ui, Lang::En, &mut settings, &mut state, false, false));
        assert!(done.iter().any(|s| s == "Export saved"));
        assert!(!done.iter().any(|s| s.contains("Open Achievements")));
    }
    #[test]
    fn stop_cannot_hide_a_reported_capture_failure() {
        let mut state = StarRailCaptureState::new(String::new());
        state.phase = CapturePhase::Stopping;
        lock_shared(&state.shared).error = Some(HsrError::new(
            "HSR-TEST",
            LocalizedText::new("抓包失败", "Capture failed"),
            "CAPTURE_ERROR_MARKER",
        ));
        state.tick();
        let CapturePhase::Failed(error) = &state.phase else {
            panic!("capture failure was hidden")
        };
        assert!(error.copy_text(Lang::En).contains("CAPTURE_ERROR_MARKER"));
    }
    #[test]
    fn stop_remains_visible_after_the_worker_exits() {
        let mut state = StarRailCaptureState::new(String::new());
        state.phase = CapturePhase::Stopping;
        state.tick();
        assert!(matches!(state.phase, CapturePhase::Stopped));
        state.tick();
        assert!(matches!(state.phase, CapturePhase::Stopped));
        assert!(!state.is_busy());
    }
}
