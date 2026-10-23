use std::sync::{Arc, Mutex};

use eframe::egui;

use super::state::{self, Lang, TaskKind, UiError, UiText};
use super::widgets;
use super::worker;

use genshin_scanner::capture::monitor::{CaptureCommand, CaptureState};
use genshin_scanner::capture::player_data::CaptureExportSettings;
use genshin_scanner::scanner::common::models::GoodExport;

/// Handle to the capture monitor running on a background tokio runtime.
pub struct CaptureHandle {
    _thread: std::thread::JoinHandle<()>,
    cmd_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<CaptureCommand>>>,
    native_crash: Arc<worker::NativeCrashState>,
    native_failure: Mutex<Option<UiError>>,
}

impl CaptureHandle {
    pub fn send(&self, cmd: CaptureCommand) {
        if let Ok(cmd_tx) = self.cmd_tx.lock() {
            if let Some(cmd_tx) = &*cmd_tx {
                let _ = cmd_tx.send(cmd);
            }
        }
    }

    pub fn close(&self) {
        match self.cmd_tx.lock() {
            Ok(mut cmd_tx) => {
                cmd_tx.take();
            },
            Err(poisoned) => {
                self.cmd_tx.clear_poison();
                poisoned.into_inner().take();
            },
        }
    }

    fn surface_native_failure(&self, phase: Option<UiText>) {
        if !self.native_crash.has_occurred() {
            return;
        }
        let Some(exception) = self.native_crash.claim_exception(TaskKind::Capture, phase) else {
            return;
        };
        worker::deactivate_native_crash(&self.native_crash);
        self.send(CaptureCommand::StopCapture);
        self.close();
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

    pub fn is_finished(&self) -> bool {
        self.surface_native_failure(None);
        self.has_native_failure() || self._thread.is_finished()
    }

    fn has_native_failure(&self) -> bool {
        match self.native_failure.lock() {
            Ok(slot) => slot.is_some(),
            Err(poisoned) => poisoned.into_inner().is_some(),
        }
    }

    fn native_failure(&self, phase: Option<UiText>) -> Option<UiError> {
        self.surface_native_failure(phase);
        match self.native_failure.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

/// Pending export result (polled each frame).
struct PendingExport {
    rx: tokio::sync::oneshot::Receiver<anyhow::Result<GoodExport>>,
}

/// Lifecycle phases for the capture tab.
#[derive(Clone, Debug, PartialEq)]
enum Phase {
    /// Nothing running yet. Show Start button.
    Idle,
    /// Background thread initializing (downloading data cache, loading keys).
    Initializing,
    /// Capture active, waiting for game packets.
    Waiting,
    /// Stop/close requested; keep the handle until its thread really exits.
    Stopping,
    Stopped,
    /// All data received — auto-exporting.
    Exporting,
    /// Done — file written.
    Done {
        summary: UiText,
        path: String,
    },
    /// Something failed.
    Failed(UiError),
}

/// State specific to the capture tab (lives in GuiApp, not AppState).
pub struct CaptureTabState {
    pub handle: Option<CaptureHandle>,
    pub capture_state: Arc<Mutex<CaptureState>>,
    phase: Phase,
    pending_export: Option<PendingExport>,

    // Export settings
    pub include_characters: bool,
    pub include_weapons: bool,
    pub include_artifacts: bool,
    pub include_achievements: bool,
    pub output_dir: String,

    // Advanced
    pub dump_packets: bool,
    pub only_keep_latest_dump: bool,
}

impl CaptureTabState {
    #[cfg(feature = "dev-tools")]
    pub(super) fn preview_feedback(&mut self, scenario: &str) {
        if scenario == "done" {
            let mut shared = self.capture_state.lock().unwrap();
            shared.has_key_exchange = true;
            shared.has_characters = true;
            shared.has_items = true;
            shared.has_achievements = true;
            shared.character_count = 86;
            shared.weapon_count = 247;
            shared.artifact_count = 1342;
            shared.achievement_count = 900;
        }
        self.phase = match scenario {
            "error" => Phase::Failed(super::preview::preview_error()),
            "stopped" => Phase::Stopped,
            "exporting" => Phase::Exporting,
            "login" => Phase::Waiting,
            "done" => Phase::Done {
                summary: UiText::new("抓包已完成", "Capture completed"),
                path: "C:\\Users\\Player\\Exports\\capture.json".into(),
            },
            _ => return,
        };
    }

    pub fn new(output_dir: String) -> Self {
        Self {
            handle: None,
            capture_state: Arc::new(Mutex::new(CaptureState::default())),
            phase: Phase::Idle,
            pending_export: None,
            include_characters: true,
            include_weapons: true,
            include_artifacts: true,
            include_achievements: true,
            output_dir,
            dump_packets: false,
            only_keep_latest_dump: true,
        }
    }

    pub fn from_config(output_dir: String, config: &genshin_scanner::cli::GoodUserConfig) -> Self {
        let mut state = Self::new(output_dir);
        state.include_characters = config.capture_include_characters;
        state.include_weapons = config.capture_include_weapons;
        state.include_artifacts = config.capture_include_artifacts;
        state.include_achievements = config.capture_include_achievements;
        state.dump_packets = config.capture_dump_packets;
        state.only_keep_latest_dump = config.capture_only_keep_latest_export;
        state
    }

    pub fn sync_to_config(&self, config: &mut genshin_scanner::cli::GoodUserConfig) {
        config.capture_include_characters = self.include_characters;
        config.capture_include_weapons = self.include_weapons;
        config.capture_include_artifacts = self.include_artifacts;
        config.capture_include_achievements = self.include_achievements;
        config.capture_dump_packets = self.dump_packets;
        config.capture_only_keep_latest_export = self.only_keep_latest_dump;
    }

    pub fn is_busy(&self) -> bool {
        matches!(
            self.phase,
            Phase::Initializing | Phase::Waiting | Phase::Stopping | Phase::Exporting
        ) || self
            .handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    pub fn tick(&mut self) {
        let previous = self.phase.clone();
        update_phase(self);
        if self.phase != previous {
            if let Phase::Failed(error) = &self.phase {
                error.log_diagnostics();
            }
        }
    }

    pub fn requires_restart(&self) -> bool {
        self.native_failure().is_some()
    }

    fn native_failure(&self) -> Option<UiError> {
        let phase = match &self.phase {
            Phase::Initializing => Some(UiText::new("正在初始化抓包器", "Initializing capture")),
            Phase::Waiting => Some(UiText::new("正在读取游戏数据", "Reading game data")),
            Phase::Stopping => Some(UiText::new("正在停止抓包器", "Stopping capture")),
            Phase::Exporting => Some(UiText::new("正在导出抓包数据", "Exporting captured data")),
            _ => None,
        };
        self.handle
            .as_ref()
            .and_then(|handle| handle.native_failure(phase))
    }

    #[cfg(feature = "test-as-invoker")]
    #[doc(hidden)]
    pub fn inject_export_for_test(&mut self, export: GoodExport) -> std::sync::mpsc::Sender<()> {
        let native_crash = Arc::new(worker::NativeCrashState::new());
        let thread_crash = native_crash.clone();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            assert!(worker::activate_native_crash(&thread_crash));
            let context = yas::native_crash::inherit_current_task();
            // Deliberately deliver data before cleanup to test the UI's exit gate.
            let _ = result_tx.send(Ok(export));
            ready_tx.send(()).unwrap();
            let _ = release_rx.recv();
            drop(context);
            worker::deactivate_native_crash(&thread_crash);
        });
        ready_rx
            .recv()
            .expect("test capture worker must acquire its boundary");
        self.handle = Some(CaptureHandle {
            _thread: thread,
            cmd_tx: Mutex::new(None),
            native_crash,
            native_failure: Mutex::new(None),
        });
        *self.capture_state.lock().unwrap() = CaptureState {
            complete: true,
            has_key_exchange: true,
            ..CaptureState::default()
        };
        self.pending_export = Some(PendingExport { rx: result_rx });
        self.phase = Phase::Initializing;
        release_tx
    }

    #[cfg(feature = "test-as-invoker")]
    #[doc(hidden)]
    pub fn completed_export_path_for_test(&self) -> Option<&str> {
        match &self.phase {
            Phase::Done { path, .. } => Some(path),
            Phase::Failed(error) => panic!("capture export failed: {error:?}"),
            _ => None,
        }
    }
}

#[cfg(feature = "test-as-invoker")]
#[doc(hidden)]
pub fn native_boundary_available_for_test() -> bool {
    let next_task = Arc::new(worker::NativeCrashState::new());
    if !worker::activate_native_crash(&next_task) {
        return false;
    }
    worker::deactivate_native_crash(&next_task);
    true
}

/// Spawn the capture monitor on a background thread with a tokio runtime.
fn spawn_capture(
    capture_state: Arc<Mutex<CaptureState>>,
    cmd_tx_out: &mut Option<tokio::sync::mpsc::UnboundedSender<CaptureCommand>>,
    dump_packets: bool,
    settings: CaptureExportSettings,
    native_crash: Arc<worker::NativeCrashState>,
) -> Result<(std::thread::JoinHandle<()>, PendingExport), UiError> {
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();

    let state = capture_state.clone();

    let thread = std::thread::Builder::new()
        .name("capture-monitor".to_owned())
        .spawn(move || {
        let native_guard_active = worker::activate_native_crash(&native_crash);
        if !native_guard_active {
            if let Ok(mut state) = state.lock() {
                state.error =
                    Some("native crash boundary is still owned by another task".to_string());
            }
            return;
        }
        let native_thread_context =
            native_guard_active.then(yas::native_crash::inherit_current_task);
        let state_for_crash = state.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .on_thread_start(|| {
                    // Tokio owns these threads for this capture runtime. The
                    // registry is cleared when the task ends or crashes.
                    std::mem::forget(yas::native_crash::inherit_current_task());
                })
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    yas::log_error!(
                        "抓包运行环境无法启动。请检查系统资源，然后重试。完整错误详情: {:#}",
                        "The capture runtime could not start. Check available system resources, then retry. Full error details: {:#}",
                        e,
                    );
                    if let Ok(mut s) = state.lock() {
                        s.error = Some(format!("{:#}", e));
                    }
                    return Ok(None);
                },
            };

            rt.block_on(async {
                let monitor = match genshin_scanner::capture::monitor::CaptureMonitor::new(
                    state.clone(),
                    dump_packets,
                ) {
                    Ok(m) => m,
                    Err(e) => {
                        yas::log_error!(
                            "抓包器无法初始化。请检查下方完整错误。完整错误详情: {:#}",
                            "Capture could not initialize. Check the complete error below. Full error details: {:#}",
                            e,
                        );
                        if let Ok(mut s) = state.lock() {
                            s.error = Some(format!("{:#}", e));
                        }
                        return Ok(None);
                    },
                };

                monitor.run(cmd_rx, settings).await
            })
        }));

        if let Err(ref panic_info) = result {
            let failure = UiError::from_panic(
                UiText::new(
                    "抓包任务因意外的内部错误而停止。",
                    "The capture task stopped because of an unexpected internal error.",
                ),
                panic_info.as_ref(),
            );
            let lang = if yas::lang::is_en() {
                Lang::En
            } else {
                Lang::Zh
            };
            failure.log_diagnostics();
            let msg = failure.technical_details(lang);
            if let Ok(mut s) = state_for_crash.lock() {
                s.error = Some(msg);
            }
        }
        drop(native_thread_context);
        if native_guard_active {
            worker::deactivate_native_crash(&native_crash);
        }
        // Publish data only after the runtime and its native boundary are gone.
        // The completed result must never leave another task blocked on capture.
        match result {
            Ok(Ok(Some(export))) => { let _ = result_tx.send(Ok(export)); },
            Ok(Err(error)) => { let _ = result_tx.send(Err(error)); },
            Ok(Ok(None)) | Err(_) => {},
        }
        })
        .map_err(|error| {
            UiError::from_error(
                UiText::new(
                    "抓包器后台任务无法启动。请检查系统资源，然后重试。",
                    "The capture background task could not start. Check available system resources, then retry.",
                ),
                error,
            )
        })?;
    *cmd_tx_out = Some(cmd_tx);
    Ok((thread, PendingExport { rx: result_rx }))
}

pub fn show_settings(ui: &mut egui::Ui, l: Lang, tab: &mut CaptureTabState, is_busy: bool) {
    widgets::section(ui, l.t("导出内容", "Export targets"), |ui| {
        ui.add_enabled_ui(!is_busy, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
                ui.checkbox(&mut tab.include_characters, l.t("角色", "Characters"));
                ui.checkbox(&mut tab.include_weapons, l.t("武器", "Weapons"));
                ui.checkbox(&mut tab.include_artifacts, l.t("圣遗物", "Artifacts"));
                ui.checkbox(&mut tab.include_achievements, l.t("成就", "Achievements"));
            });
        });
    });
    widgets::fold(ui, l.t("高级选项", "Advanced"), |ui| {
        ui.add_enabled_ui(!is_busy, |ui| {
            ui.checkbox(
                &mut tab.dump_packets,
                l.t("保存解密数据包", "Save decoded packets"),
            );
        });
    });
}

/// Start a fresh capture directly from either the initial or completed state.
fn start_capture(tab: &mut CaptureTabState) {
    if let Err(e) = super::privilege::ensure_admin_for_action() {
        tab.phase = Phase::Failed(UiError::from_anyhow(
            UiText::new(
                "抓包器需要管理员权限才能读取游戏网络数据。请以管理员身份重新启动程序。",
                "Capture needs administrator access to read game network data. Restart the application as administrator.",
            ),
            &e,
        ));
        if let Phase::Failed(error) = &tab.phase {
            error.log_diagnostics();
        }
        return;
    }
    tab.capture_state = Arc::new(Mutex::new(CaptureState::default()));
    let mut cmd_tx = None;
    let native_crash = Arc::new(worker::NativeCrashState::new());
    let settings = CaptureExportSettings {
        include_characters: tab.include_characters,
        include_weapons: tab.include_weapons,
        include_artifacts: tab.include_artifacts,
        include_achievements: tab.include_achievements,
        ..Default::default()
    };
    match spawn_capture(
        tab.capture_state.clone(),
        &mut cmd_tx,
        tab.dump_packets,
        settings,
        native_crash.clone(),
    ) {
        Ok((thread, pending_export)) => {
            tab.handle = Some(CaptureHandle {
                _thread: thread,
                cmd_tx: Mutex::new(cmd_tx),
                native_crash,
                native_failure: Mutex::new(None),
            });
            tab.pending_export = Some(pending_export);
            tab.phase = Phase::Initializing;
        },
        Err(error) => {
            error.log_diagnostics();
            tab.handle = None;
            tab.pending_export = None;
            tab.phase = Phase::Failed(error);
        },
    }
}

fn start_button(ui: &mut egui::Ui, l: Lang, tab: &mut CaptureTabState, game_busy: bool) {
    if super::theme::primary_action(
        ui,
        !game_busy
            && !tab.is_busy()
            && (tab.include_characters
                || tab.include_weapons
                || tab.include_artifacts
                || tab.include_achievements),
        if matches!(tab.phase, Phase::Failed(_)) {
            l.t("重试", "Retry")
        } else if matches!(tab.phase, Phase::Done { .. }) {
            l.t("再次抓包", "Capture again")
        } else {
            l.t("开始抓包", "Start capture")
        },
    )
    .clicked()
    {
        start_capture(tab);
    }
}

pub fn show_status(
    ui: &mut egui::Ui,
    l: Lang,
    tab: &mut CaptureTabState,
    game_busy: bool,
    restart_required: bool,
) {
    if restart_required {
        super::theme::restart_required(ui, l, tab.native_failure().as_ref());
        return;
    }
    let shared = tab
        .capture_state
        .try_lock()
        .map(|s| s.clone())
        .unwrap_or_default();
    use super::capture_status::{self, Stage};
    let stage = match &tab.phase {
        Phase::Stopped => Stage::Stopped,
        Phase::Idle => Stage::Ready,
        Phase::Initializing => Stage::Starting,
        Phase::Waiting => capture_status::waiting(
            shared.has_key_exchange,
            (tab.include_characters && !shared.has_characters)
                || (tab.include_weapons && !shared.has_items && shared.weapon_count == 0)
                || (tab.include_artifacts && !shared.has_items && shared.artifact_count == 0),
            tab.include_achievements && !shared.has_achievements,
        ),
        Phase::Stopping => Stage::Stopping,
        Phase::Exporting => Stage::Exporting,
        Phase::Done { .. } => Stage::Done,
        Phase::Failed(_) => Stage::Failed,
    };
    let text = stage.headline(l);
    let feedback = match &tab.phase {
        Phase::Idle
            if !tab.include_characters
                && !tab.include_weapons
                && !tab.include_artifacts
                && !tab.include_achievements =>
        {
            Some(state::TaskStatus::AwaitingInput(UiText::new(
                "请选择导出内容",
                "Select export targets",
            )))
        },
        Phase::Failed(error) => Some(super::state::TaskStatus::Failed(error.clone())),
        Phase::Done { summary, path } => Some(super::state::TaskStatus::Exported {
            message: summary.clone(),
            path: path.clone(),
            partial: false,
        }),
        Phase::Stopped => Some(super::state::TaskStatus::Stopped(UiText::new(text, text))),
        _ => None,
    };
    widgets::status_content(ui, "genshin-capture-status", 1, |ui| {
        super::theme::task_status(ui, l, feedback.as_ref(), text);
        if stage != Stage::Ready
            && stage != Stage::Done
            && (tab.include_characters
                || tab.include_weapons
                || tab.include_artifacts
                || tab.include_achievements)
        {
            capture_status::show_connection(ui, l, stage, shared.has_key_exchange);
        }
        if stage != Stage::Ready {
            let cs = &shared;
            for (selected, key, zh, en, complete, count) in [
                (
                    tab.include_characters,
                    "characters",
                    "角色",
                    "Characters",
                    cs.has_characters,
                    cs.character_count,
                ),
                (
                    tab.include_weapons,
                    "weapons",
                    "武器",
                    "Weapons",
                    cs.has_items || cs.weapon_count > 0,
                    cs.weapon_count,
                ),
                (
                    tab.include_artifacts,
                    "artifacts",
                    "圣遗物",
                    "Artifacts",
                    cs.has_items || cs.artifact_count > 0,
                    cs.artifact_count,
                ),
                (
                    tab.include_achievements,
                    "achievements",
                    "成就",
                    "Achievements",
                    cs.has_achievements,
                    cs.achievement_count,
                ),
            ] {
                if selected {
                    let mut step = super::task_progress::Step::new(key, zh, en);
                    step.completed = count;
                    if complete {
                        step.state = super::task_progress::StepState::Complete;
                    } else if matches!(tab.phase, Phase::Failed(_) | Phase::Stopped) {
                        step.state = super::task_progress::StepState::Interrupted;
                    } else if matches!(tab.phase, Phase::Waiting) && shared.has_key_exchange {
                        step.state = super::task_progress::StepState::Running;
                    }
                    super::task_progress::row(ui, l, &step);
                }
            }
        }
    });
    ui.add_space(12.0);
    if matches!(
        tab.phase,
        Phase::Waiting | Phase::Stopped | Phase::Failed(_)
    ) {
        capture_status::show_counters(
            ui,
            l,
            shared.elapsed_secs,
            shared.packet_count,
            shared.command_count,
        );
    }
    ui.add_enabled_ui(!tab.is_busy() && !game_busy, |ui| {
        ui.checkbox(
            &mut tab.only_keep_latest_dump,
            l.t("仅保留最新导出", "Keep latest export only"),
        );
    });
    ui.add_space(4.0);
    widgets::export_action(ui, l, feedback.as_ref());
    match &tab.phase {
        Phase::Initializing | Phase::Waiting => {
            if super::theme::primary_action(ui, true, l.t("停止抓包", "Stop capture")).clicked()
            {
                if let Some(h) = &tab.handle {
                    h.send(CaptureCommand::StopCapture);
                    h.close();
                }
                tab.phase = Phase::Stopping;
            }
        },
        Phase::Stopping | Phase::Exporting => {
            super::theme::primary_action(ui, false, l.t("正在处理", "Processing"));
        },
        Phase::Failed(_) => {
            start_button(ui, l, tab, game_busy);
        },
        Phase::Done { .. } | Phase::Idle | Phase::Stopped => start_button(ui, l, tab, game_busy),
    }
}

/// Drive phase transitions based on shared capture state.
fn update_phase(tab: &mut CaptureTabState) {
    if let Some(error) = tab.native_failure() {
        tab.phase = Phase::Failed(error);
        return;
    }

    let worker_finished = tab.handle.as_ref().is_none_or(CaptureHandle::is_finished);
    if worker_finished && matches!(tab.phase, Phase::Done { .. } | Phase::Failed(_)) {
        tab.handle = None;
    }

    // Initialization failures have no export result. Preserve their actual cause.
    if matches!(
        tab.phase,
        Phase::Initializing | Phase::Waiting | Phase::Exporting | Phase::Stopping
    ) {
        let error = tab
            .capture_state
            .try_lock()
            .ok()
            .and_then(|s| s.error.clone());
        if let Some(error) = error {
            if let Some(handle) = &tab.handle {
                handle.close();
            }
            tab.pending_export = None;
            let hint = tab
                .capture_state
                .lock()
                .map(|state| {
                    if state.elapsed_secs >= 300 {
                        let (zh, en) = state.timeout_hint();
                        UiText::new(zh, en)
                    } else {
                        UiText::new(
                            "抓包器在启动或读取游戏数据时停止。",
                            "Capture stopped while starting or reading game data.",
                        )
                    }
                })
                .unwrap_or_else(|_| UiText::new("抓包失败", "Capture failed"));
            tab.phase = Phase::Failed(UiError::from_message(hint, error));
            return;
        }
    }

    if tab.phase == Phase::Stopping {
        if worker_finished {
            tab.handle = None;
            tab.pending_export = None;
            tab.phase = Phase::Stopped;
        }
        return;
    }

    // Do not expose completion or enable another game task until the capture
    // thread has exited, including runtime cleanup and native boundary release.
    if let Some(pending) = tab.pending_export.as_mut().filter(|_| worker_finished) {
        let export_result = pending.rx.try_recv();
        if !matches!(
            export_result,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ) {
            tab.handle = None;
        }
        match export_result {
            Ok(Ok(export)) => {
                let timestamp = genshin_scanner::cli::chrono_timestamp();
                match serde_json::to_string_pretty(&export) {
                    Ok(json) => match genshin_scanner::fs_utils::write_capture_export(
                        std::path::Path::new(&tab.output_dir),
                        &json,
                        &timestamp,
                        tab.only_keep_latest_dump,
                    ) {
                        Ok(path) => {
                            let cc = export.characters.as_ref().map_or(0, |v| v.len());
                            let wc = export.weapons.as_ref().map_or(0, |v| v.len());
                            let ac = export.artifacts.as_ref().map_or(0, |v| v.len());
                            let hc = export.achievements.as_ref().map_or(0, |v| v.len());
                            // Captured inventory includes gear excluded by export
                            // filters. Once saved, progress must describe this file.
                            {
                                let mut shared =
                                    tab.capture_state.lock().expect("capture state poisoned");
                                shared.character_count = cc;
                                shared.weapon_count = wc;
                                shared.artifact_count = ac;
                                shared.achievement_count = hc;
                            }
                            let summary = UiText::new(
                                format!(
                                    "已导出: {} 角色, {} 武器, {} 圣遗物, {} 成就",
                                    cc, wc, ac, hc
                                ),
                                format!(
                                    "Exported: {} characters, {} weapons, {} artifacts, {} achievements",
                                    cc, wc, ac, hc
                                ),
                            );
                            yas::log_info!("{} → {}", "{} → {}", summary, path.display());
                            yas::log_info!(
                                "导出范围：武器 3–5★，圣遗物 4–5★（包含五星套装的四星圣遗物）；可在网站导入时进一步筛选。",
                                "Export scope: 3–5★ weapons and 4–5★ artifacts, including 4★ pieces from 5★ sets. Further filtering is available when importing on the website."
                            );
                            tab.phase = Phase::Done {
                                summary,
                                path: path.display().to_string(),
                            };
                        },
                        Err(e) => {
                            tab.phase = Phase::Failed(UiError::from_anyhow(
                                UiText::new(
                                    "导出文件无法保存或旧文件无法清理。请检查输出目录、磁盘空间和文件权限；详情中会标明是否已保存新文件。",
                                    "The export could not be saved or older files could not be removed. Check the folder, disk space and permissions; details indicate whether the new file was saved.",
                                ),
                                &e,
                            ));
                        },
                    },
                    Err(e) => {
                        tab.phase = Phase::Failed(UiError::from_error(
                            UiText::new(
                                "抓取的数据无法转换为导出文件。请复制完整错误并报告此问题。",
                                "The captured data could not be converted into an export file. Copy the full error and report this problem.",
                            ),
                            e,
                        ));
                    },
                }
                tab.pending_export = None;
                return;
            },
            Ok(Err(e)) => {
                tab.phase = Phase::Failed(UiError::from_anyhow(
                    UiText::new(
                        "抓包数据导出未能完成。下方完整错误包含底层原因。",
                        "The captured data could not be exported. The full error below contains the underlying cause.",
                    ),
                    &e,
                ));
                tab.pending_export = None;
                return;
            },
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                return; // still waiting
            },
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                tab.phase = Phase::Failed(UiError::from_message(
                    UiText::new(
                        "导出任务意外停止，未能返回结果。请重试；若再次发生，请复制完整错误并报告问题。",
                        "The export task stopped unexpectedly without returning a result. Retry; if it happens again, copy the full error and report the problem.",
                    ),
                    "tokio oneshot export result channel closed before sending a result",
                ));
                tab.pending_export = None;
                return;
            },
        }
    }

    // Transition: Initializing → Waiting (when capture starts)
    if tab.phase == Phase::Initializing && tab.capture_state.try_lock().is_ok_and(|s| s.capturing) {
        tab.phase = Phase::Waiting;
    }

    // The worker exports and stops on its own, even when this tab is inactive.
    if matches!(tab.phase, Phase::Initializing | Phase::Waiting)
        && tab.capture_state.try_lock().is_ok_and(|s| s.complete)
    {
        tab.phase = Phase::Exporting;
    }
}
#[cfg(test)]
mod feedback_tests {
    use super::*;
    #[test]
    fn next_action_changes_only_after_relevant_capture_data_arrives() {
        use super::super::capture_status::feedback_tests::texts;
        let mut tab = CaptureTabState::new(String::new());
        tab.include_achievements = true;
        tab.phase = Phase::Waiting;
        let login = texts(|ui| show_status(ui, Lang::En, &mut tab, false, false));
        assert!(login.iter().any(|s| s == "Launch the game and log in"));
        assert!(!login.iter().any(|s| s.contains("Open Achievements")));
        tab.capture_state.lock().unwrap().has_key_exchange = true;
        let handshake = texts(|ui| show_status(ui, Lang::En, &mut tab, false, false));
        assert!(handshake.iter().any(|s| s == "Receiving game data"));
        assert!(!tab.capture_state.lock().unwrap().has_characters);
        {
            let mut shared = tab.capture_state.lock().unwrap();
            shared.has_characters = true;
        }
        let inventory = texts(|ui| show_status(ui, Lang::En, &mut tab, false, false));
        assert!(inventory.iter().any(|s| s == "Receiving game data"));
        assert!(!inventory.iter().any(|s| s.contains("Open Achievements")));
        tab.capture_state.lock().unwrap().has_items = true;
        let achievements = texts(|ui| show_status(ui, Lang::En, &mut tab, false, false));
        assert!(achievements
            .iter()
            .any(|s| s == "Open Achievements in the game"));
        assert!(!achievements.iter().any(|s| s.contains("Import the file")));
        tab.phase = Phase::Done {
            summary: UiText::new("已完成", "Completed"),
            path: "capture.json".into(),
        };
        let done = texts(|ui| show_status(ui, Lang::En, &mut tab, false, false));
        assert!(done.iter().any(|s| s == "Export saved"));
        assert!(!done.iter().any(|s| s.contains("Open Achievements")));
    }
    #[test]
    fn stop_remains_visible_after_the_worker_exits() {
        let mut tab = CaptureTabState::new(String::new());
        tab.phase = Phase::Stopping;
        tab.tick();
        assert_eq!(tab.phase, Phase::Stopped);
        tab.tick();
        assert_eq!(tab.phase, Phase::Stopped);
        assert!(!tab.is_busy());
    }
    #[test]
    fn capture_failure_cannot_become_export_success() {
        let mut tab = CaptureTabState::new(String::new());
        tab.phase = Phase::Stopping;
        tab.capture_state.lock().unwrap().error = Some("CAPTURE_ERROR_MARKER".into());
        tab.tick();
        let Phase::Failed(error) = &tab.phase else {
            panic!("capture failure was hidden")
        };
        assert!(error.copy_text(Lang::En).contains("CAPTURE_ERROR_MARKER"));
    }
}
