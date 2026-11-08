#[cfg(all(feature = "capture", target_os = "windows"))]
mod capture_status;
#[cfg(all(feature = "capture", target_os = "windows"))]
pub mod capture_tab;
#[cfg(all(feature = "capture", target_os = "windows"))]
mod continuous_capture;
pub mod credits;
pub mod data_refresh;
mod file_location;
pub mod game_switcher;
pub mod hsr_manager_progress;
pub mod icons;
pub mod layout;
#[cfg(target_os = "linux")]
pub mod linux_theme;
pub mod log_bridge;
pub mod log_panel;
pub mod manager_progress;
pub mod manager_tab;
#[cfg(feature = "dev-tools")]
pub mod preview;
mod privilege;
pub mod restart;
pub mod scanner_tab;
pub mod shell;
#[cfg(all(feature = "capture", target_os = "windows"))]
pub mod star_rail_capture_tab;
pub mod star_rail_exports;
pub mod star_rail_manager_tab;
pub mod star_rail_scanner_tab;
pub mod star_rail_state;
pub mod star_rail_worker;
pub mod state;
pub mod task_progress;
pub mod theme;
pub mod update_banner;
pub mod widgets;
pub mod worker;

use crate::config::{ApplicationConfigStore, Game, ToolTab};
use eframe::egui;
use state::{AppState, Lang, UpdateState};
use worker::TaskHandle;

/// Launch the GUI application.
pub fn run_gui() {
    run_gui_with_agent_mode(consume_agent_mode_marker());
}

/// Launch the GUI with its selected game's loopback manager started immediately.
/// This is reserved for the local, opt-in scanner iteration workflow.
pub fn run_gui_agent_mode() {
    run_gui_with_agent_mode(true);
}

fn run_gui_with_agent_mode(agent_mode: bool) {
    #[cfg(all(feature = "capture", target_os = "windows"))]
    const PRODUCT_NAME: &str = "GGScanner";
    #[cfg(not(all(feature = "capture", target_os = "windows")))]
    const PRODUCT_NAME: &str = "GGScannerOCR";

    if let Err(error) = restart::wait_for_update_parent() {
        show_startup_error(PRODUCT_NAME, Lang::Zh, &state::UiError::from_error(
            state::UiText::new(
                "上一个版本尚未退出，更新无法启动。请关闭旧版本后重新打开程序。",
                "The update could not start while waiting for the previous version. Close the old application and reopen this one.",
            ),
            error,
        ));
        return;
    }

    // Register the process-wide SEH handler early. Worker threads explicitly
    // enroll in it when they start; unregistered threads are left alone.
    #[cfg(target_os = "windows")]
    worker::install_seh_handler();

    let state = AppState::new();
    let (app_config, app_config_warning) = ApplicationConfigStore::for_running_executable();

    // Set global language from config
    yas::lang::set_lang(state.lang.to_str());

    // Init GUI logger (replaces env_logger in GUI mode)
    let logger = log_bridge::GuiLogger::new(
        state.scanner_log_lines.clone(),
        state.manager_log_lines.clone(),
        2000,
    );
    if let Err(error) = logger.init(state.verbose) {
        let failure = state::UiError::from_message(
            state::UiText::new(
                "程序无法启动错误记录功能。请复制完整错误并报告此问题。",
                "The application could not start its error logging. Copy the full error and report this problem.",
            ),
            error.to_string(),
        );
        show_startup_error(PRODUCT_NAME, state.lang, &failure);
        return;
    }

    if let Some(error) = app_config_warning {
        let warning = state::UiError::from_anyhow(
            state::UiText::new(
                "星穹铁道设置无法读取，因此本次启动使用默认值且不会覆盖原文件。请检查完整错误；如需重置，请先手动重命名原文件。",
                "Star Rail settings could not be read, so defaults are used for this session and the original file will not be overwritten. Check the full error; to reset, manually rename the original file first.",
            ),
            &error,
        );
        log::warn!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", warning.hint_text(state.lang));
    }

    // Install a panic hook that writes to the log file (the default hook
    // writes to stderr, which GUI users never see).  This covers panics on
    // ALL threads — worker, update, refresh, and GUI main.
    install_panic_hook();

    // Clean up the previous update only after logging is ready, so even this
    // non-fatal startup failure retains its readable hint and inner error.
    genshin_scanner::updater::cleanup_old_exe();

    // Kick off background update check for the executable that is running.
    #[cfg(all(feature = "capture", target_os = "windows"))]
    const UPDATE_ASSET: &str = genshin_scanner::updater::ASSET_CAPTURE;
    #[cfg(not(all(feature = "capture", target_os = "windows")))]
    const UPDATE_ASSET: &str = genshin_scanner::updater::ASSET_SCANNER;
    update_banner::spawn_check(UPDATE_ASSET, &state.update_state);

    let window_title = genshin_scanner::updater::window_title(PRODUCT_NAME);

    let mut viewport = egui::ViewportBuilder::default()
        .with_title(window_title)
        .with_inner_size([1040.0, 760.0])
        .with_min_inner_size([760.0, 500.0])
        .with_clamp_size_to_monitor_size(true)
        .with_decorations(false);
    match eframe::icon_data::from_png_bytes(include_bytes!("../../../assets/icon_64.png")) {
        Ok(icon) => viewport = viewport.with_icon(std::sync::Arc::new(icon)),
        Err(error) => {
            let failure = state::UiError::from_error(
                state::UiText::new(
                    "窗口图标无法加载，但程序仍可继续使用。",
                    "The window icon could not be loaded, but the application can continue.",
                ),
                error,
            );
            log::error!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", failure.hint_text(state.lang));
        },
    }

    let options = eframe::NativeOptions {
        viewport,
        ..Default::default()
    };

    // Cloned before `state` moves into `GuiApp`: an update-triggered restart is
    // detected after `run_native` returns.
    let update_state = state.update_state.clone();
    if let Err(error) = eframe::run_native(
        PRODUCT_NAME,
        options,
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            Ok(Box::new(GuiApp::new(
                state,
                app_config,
                &cc.egui_ctx,
                agent_mode,
                #[cfg(target_os = "linux")]
                linux_theme::spawn_theme_watcher(cc.egui_ctx.clone()),
            )))
        }),
    ) {
        let lang = if yas::lang::is_en() {
            Lang::En
        } else {
            Lang::Zh
        };
        let failure = state::UiError::from_error(
            state::UiText::new(
                "程序窗口无法启动或意外关闭。请复制下方完整错误以搜索或寻求帮助。",
                "The application window could not start or closed unexpectedly. Copy the full error below to search or ask for help.",
            ),
            error,
        );
        log::error!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", failure.hint_text(lang));
        show_startup_error(PRODUCT_NAME, lang, &failure);
    }
    if matches!(
        *update_state.lock().unwrap(),
        UpdateState::RestartRequested(_)
    ) {
        restart::exit_for_update();
    }
}

fn consume_agent_mode_marker() -> bool {
    let marker = genshin_scanner::cli::exe_dir()
        .join("data")
        .join("ggscanner-agent-mode.once");
    let Ok(value) = std::fs::read_to_string(&marker) else {
        return false;
    };
    if value.trim() != "agent-mode-v1" || std::fs::remove_file(marker).is_err() {
        return false;
    }
    true
}

fn show_startup_error(product_name: &str, lang: Lang, failure: &state::UiError) {
    let full_error = failure.copy_text(lang);
    let mut description = full_error.clone();
    match state::persist_error_report("startup_error.txt", &full_error) {
        Ok(path) => {
            description.push_str("\n\n");
            description.push_str(lang.t(
                "完整错误也已保存到以下文件，可打开后复制：\n",
                "The full error was also saved here so it can be opened and copied:\n",
            ));
            description.push_str(&path.display().to_string());
        },
        Err(error) => {
            description.push_str("\n\n");
            description.push_str(lang.t(
                "程序还无法保存错误报告。请拍摄此窗口，或复制其中可选择的文字。完整错误详情：\n",
                "The application also could not save the error report. Take a screenshot of this window, or copy any selectable text. Full error details:\n",
            ));
            description.push_str(&format!("{error:#}"));
        },
    }

    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(format!("{} — {}", product_name, lang.t("错误", "Error")))
        .set_description(description)
        .set_buttons(rfd::MessageButtons::Ok)
        .show();
}

/// Replace the default panic hook so panics on ANY thread are written
/// to the `log::error!` logger (visible in the GUI log panel + log file).
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let technical_details = state::record_panic_details(info);
        let failure = state::UiError::from_message(
            state::UiText::new(
                "程序中的任务遇到意外内部错误，当前操作可能已停止。请复制完整错误以搜索或寻求帮助。",
                "An application task encountered an unexpected internal error and may have stopped. Copy the full error to search or ask for help.",
            ),
            technical_details,
        );
        let lang = if yas::lang::is_en() {
            Lang::En
        } else {
            Lang::Zh
        };
        log::error!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", failure.hint_text(lang));
    }));
}

struct GuiApp {
    save_settings: bool,
    splits: layout::Splits,
    logos: shell::Logos,
    data_refresh: data_refresh::DataRefresh,
    restart_started: bool,
    state: AppState,
    app_config: ApplicationConfigStore,
    star_rail: star_rail_state::StarRailState,
    scan_handle: Option<TaskHandle>,
    server_handle: Option<TaskHandle>,
    /// Desktop theme updates (Linux: winit cannot follow the system theme).
    #[cfg(target_os = "linux")]
    theme_rx: Option<std::sync::mpsc::Receiver<Option<egui::Theme>>>,
    /// Latest theme reported by the desktop (linux watcher thread).
    #[cfg(target_os = "linux")]
    desktop_theme: Option<egui::Theme>,
    #[cfg(all(feature = "capture", target_os = "windows"))]
    capture_tab: capture_tab::CaptureTabState,
}

impl GuiApp {
    fn new(
        state: AppState,
        app_config: ApplicationConfigStore,
        ctx: &egui::Context,
        agent_mode: bool,
        #[cfg(target_os = "linux")] theme_rx: Option<
            std::sync::mpsc::Receiver<Option<egui::Theme>>,
        >,
    ) -> Self {
        theme::install(ctx, app_config.config.appearance);
        #[cfg(all(feature = "capture", target_os = "windows"))]
        let capture_tab_state =
            capture_tab::CaptureTabState::from_config(state.output_dir.clone(), &state.user_config);
        let star_rail =
            star_rail_state::StarRailState::new(app_config.config.star_rail.output_dir.clone());
        let mut app = Self {
            save_settings: true,
            splits: layout::Splits::default(),
            logos: shell::Logos::load(ctx),
            data_refresh: data_refresh::DataRefresh::default(),
            restart_started: false,
            state,
            app_config,
            star_rail,
            scan_handle: None,
            server_handle: None,
            #[cfg(target_os = "linux")]
            theme_rx,
            #[cfg(target_os = "linux")]
            desktop_theme: None,
            #[cfg(all(feature = "capture", target_os = "windows"))]
            capture_tab: capture_tab_state,
        };
        if agent_mode {
            app.start_agent_manager();
        }
        app
    }

    fn start_agent_manager(&mut self) {
        self.app_config
            .config
            .navigation
            .select_tab(ToolTab::Manager);
        if let Err(error) = self.app_config.persist_now() {
            log::warn!(
                target: yas::lang::LOCALIZED_LOG_TARGET,
                "{}",
                error
            );
        }

        if let Err(error) = privilege::ensure_admin_for_action() {
            let failure = state::UiError::from_anyhow(
                state::UiText::new(
                    "Agent 模式需要管理员权限；请通过 GGScanner Agent 启动任务重新打开程序。",
                    "Agent mode requires administrator privileges; restart through the GGScanner Agent task.",
                ),
                &error,
            );
            match self.app_config.config.navigation.active_game {
                Game::Genshin => {
                    *self.state.server_status.lock().unwrap() = state::TaskStatus::Failed(failure);
                },
                Game::StarRail => {
                    *self.star_rail.manager_status.lock().unwrap() =
                        state::TaskStatus::Failed(failure);
                },
            }
            return;
        }

        match self.app_config.config.navigation.active_game {
            Game::Genshin => {
                self.state
                    .server_enabled
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.server_handle = Some(worker::spawn_server(&self.state));
            },
            Game::StarRail => {
                let settings = &self.app_config.config.star_rail;
                self.star_rail.manager_handle = Some(star_rail_worker::spawn_manager_server(
                    settings,
                    self.star_rail.manager_status.clone(),
                    self.star_rail.manager_progress.clone(),
                    self.star_rail.manager_job.clone(),
                ));
            },
        }
    }
}

impl eframe::App for GuiApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        #[cfg(target_os = "linux")]
        {
            if let Some(rx) = &self.theme_rx {
                while let Ok(theme) = rx.try_recv() {
                    self.desktop_theme = theme;
                }
            }
            ctx.set_theme(linux_theme::effective_preference(
                self.app_config.config.appearance,
                self.desktop_theme,
            ));
        }
        #[cfg(not(target_os = "linux"))]
        ctx.set_theme(theme::preference(self.app_config.config.appearance));
        if self.restart_started {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // Debounced auto-save: check if config changed and save after 300ms
        log_bridge::set_game_verbose(self.state.verbose, self.app_config.config.star_rail.verbose);
        if self.save_settings {
            #[cfg(all(feature = "capture", target_os = "windows"))]
            self.capture_tab.sync_to_config(&mut self.state.user_config);
            self.state.auto_save_tick();
            if let Err(error) = self.app_config.auto_save_tick() {
                let failure = state::UiError::from_anyhow(
                state::UiText::new(
                    "星穹铁道设置无法保存；本次更改可能在重启后丢失。",
                    "Star Rail settings could not be saved; these changes may be lost after restart.",
                ),
                &error,
            );
                log::warn!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", failure.hint_text(self.state.lang));
            }
        }
        #[cfg(all(feature = "capture", target_os = "windows"))]
        self.star_rail.capture.tick();
        #[cfg(all(feature = "capture", target_os = "windows"))]
        self.capture_tab.tick();

        self.data_refresh.poll();
        // Logic also runs while the viewport is hidden; keep capture/export
        // polling and debounced settings saves moving without painting a frame.
        #[cfg(all(feature = "capture", target_os = "windows"))]
        let capture_busy = self.capture_tab.is_busy() || self.star_rail.capture.is_busy();
        #[cfg(not(all(feature = "capture", target_os = "windows")))]
        let capture_busy = false;
        if capture_busy
            || self.data_refresh.is_pending()
            || self.state.config_dirty_since.is_some()
            || self.app_config.save_pending()
        {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }

    fn ui(&mut self, root_ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();

        let is_scan_running = self
            .scan_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        let is_server_running = self
            .server_handle
            .as_ref()
            .is_some_and(|handle| !handle.is_finished());
        #[cfg(all(feature = "capture", target_os = "windows"))]
        let is_capture_busy = self.capture_tab.is_busy();
        #[cfg(not(all(feature = "capture", target_os = "windows")))]
        let is_capture_busy = false;
        let genshin_busy = is_scan_running || is_server_running || is_capture_busy;
        let star_rail_scan_running = self.star_rail.scan_running();
        let star_rail_manager_running = self.star_rail.manager_running();
        #[cfg(all(feature = "capture", target_os = "windows"))]
        let star_rail_capture_busy = self.star_rail.capture.is_busy();
        #[cfg(not(all(feature = "capture", target_os = "windows")))]
        let star_rail_capture_busy = false;
        let star_rail_busy =
            star_rail_scan_running || star_rail_manager_running || star_rail_capture_busy;
        let data_refreshing = self.data_refresh.is_running();
        let game_task_busy = genshin_busy || star_rail_busy || data_refreshing;

        shell::window_resize(&ctx);
        shell::titlebar(
            root_ui,
            &mut self.state.lang,
            &mut self.app_config.config.navigation.active_game,
            !game_task_busy,
            &self.logos,
        );
        self.state.user_config.lang = self.state.lang.to_str().to_owned();
        yas::lang::set_lang(self.state.lang.to_str());

        // Update banner (between tabs and content)
        update_banner::show(
            root_ui,
            self.state.lang,
            &self.state.update_state,
            game_task_busy,
        );
        let update_in_progress = matches!(
            *self.state.update_state.lock().unwrap(),
            UpdateState::Downloading
                | UpdateState::ShowingDialog
                | UpdateState::RestartRequested(_)
        );
        let restart_path = match self.state.update_state.lock().unwrap().clone() {
            UpdateState::RestartRequested(path) => Some(path),
            _ => None,
        };
        if let Some(path) = restart_path {
            if !game_task_busy {
                match restart::launch_updated_app(&path) {
                    Ok(_) => {
                        self.restart_started = true;
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                        return;
                    },
                    Err(error) => {
                        *self.state.update_state.lock().unwrap() = UpdateState::Failed(
                            state::UiError::from_error(
                                state::UiText::new(
                                    "更新已安装，但新版本无法自动启动。请手动重新打开程序。",
                                    "The update was installed, but the new version could not start automatically. Reopen the application manually.",
                                ),
                                error,
                            ),
                        );
                    },
                }
            }
        }

        // A native worker crash exits the thread without running Rust/native
        // cleanup. Block every game-facing task until the whole application
        // is restarted; retrying in the same process is not safe.
        let restart_required = self
            .scan_handle
            .as_ref()
            .is_some_and(TaskHandle::requires_restart)
            || self
                .server_handle
                .as_ref()
                .is_some_and(TaskHandle::requires_restart);
        #[cfg(all(feature = "capture", target_os = "windows"))]
        let restart_required = restart_required || self.capture_tab.requires_restart();
        let restart_required = restart_required || self.star_rail.requires_restart();

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(theme::palette(ctx.theme() == egui::Theme::Dark).background),
            )
            .show(root_ui, |ui| {
                let rect = ui.available_rect_before_wrap();
                let [sidebar, right] = layout::split(
                    ui,
                    rect,
                    "sidebar-split",
                    &mut self.splits.sidebar,
                    true,
                    shell::SIDEBAR_MIN_WIDTH,
                );
                layout::content_region(
                    ui,
                    sidebar.shrink(layout::PANE_GAP - 4.0),
                    "sidebar",
                    |ui| {
                        shell::sidebar(
                            ui,
                            &mut self.state.lang,
                            &mut self.app_config.config.navigation,
                            &mut self.app_config.config.appearance,
                        )
                    },
                );
                let l = self.state.lang;
                let active_game = self.app_config.config.navigation.active_game;
                let active_tab = self.app_config.config.navigation.active_tab();
                let [workspace, logs] = layout::split(
                    ui,
                    right,
                    "log-split",
                    &mut self.splits.workspace,
                    false,
                    150.0,
                );
                let log_buf = match active_tab {
                    ToolTab::Manager => &self.state.manager_log_lines,
                    _ => &self.state.scanner_log_lines,
                };
                layout::region(ui, logs.shrink(layout::PANE_GAP - 2.0), "logs", |ui| {
                    log_panel::show_with(ui, l, log_buf)
                });
                layout::region(ui, layout::workspace_bounds(workspace), "workspace", |ui| {
                    self.data_refresh.heading(
                        ui,
                        l,
                        active_game,
                        active_tab,
                        !game_task_busy && !update_in_progress && !restart_required,
                        active_game == Game::StarRail
                            && active_tab == ToolTab::Capture
                            && self
                                .app_config
                                .config
                                .star_rail
                                .capture_include_achievements,
                    );
                    let data_refreshing = self.data_refresh.is_running();
                    let game_task_busy = genshin_busy || star_rail_busy || data_refreshing;
                    ui.add_space(10.0);
                    if active_tab == ToolTab::Credits {
                        credits::show(
                            ui,
                            l,
                            if cfg!(all(feature = "capture", target_os = "windows")) {
                                credits::CreditSet::Full
                            } else {
                                credits::CreditSet::Scanner
                            },
                        );
                        return;
                    }
                    layout::workspace(ui, &mut self.splits.settings, |ui, pane| {
                        let owns_running_task = match (active_game, active_tab) {
                            (Game::Genshin, ToolTab::Scanner) => is_scan_running,
                            (Game::Genshin, ToolTab::Manager) => is_server_running,
                            (Game::Genshin, ToolTab::Capture) => is_capture_busy,
                            (Game::StarRail, ToolTab::Scanner) => star_rail_scan_running,
                            (Game::StarRail, ToolTab::Manager) => star_rail_manager_running,
                            (Game::StarRail, ToolTab::Capture) => star_rail_capture_busy,
                            _ => false,
                        };
                        if matches!(pane, layout::Pane::Status)
                            && !restart_required
                            && !owns_running_task
                            && (game_task_busy || update_in_progress)
                        {
                            let running_tab = if is_server_running || star_rail_manager_running {
                                ToolTab::Manager
                            } else if is_scan_running || star_rail_scan_running {
                                ToolTab::Scanner
                            } else {
                                ToolTab::Capture
                            };
                            let reason = if data_refreshing {
                                l.t("正在更新游戏数据", "Updating game data")
                            } else if update_in_progress {
                                l.t("正在更新程序", "Updating the app")
                            } else {
                                match running_tab {
                                    ToolTab::Manager => l.t("管理器正在运行", "Manager is running"),
                                    ToolTab::Scanner => l.t("扫描正在进行", "Scan is running"),
                                    _ => l.t("抓包正在进行", "Capture is running"),
                                }
                            };
                            theme::blocked_status(ui, l, reason);
                            if !update_in_progress
                                && !data_refreshing
                                && ui.button(l.t("查看任务", "View task")).clicked()
                            {
                                self.app_config.config.navigation.select_tab(running_tab);
                            }
                            return;
                        }
                        match (active_game, active_tab, pane) {
                            (Game::Genshin, ToolTab::Scanner, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(is_server_running
                                            || is_capture_busy
                                            || star_rail_busy
                                            || update_in_progress)
                                        && !is_scan_running,
                                    |ui| {
                                        scanner_tab::show_settings(
                                            ui,
                                            &mut self.state,
                                            is_scan_running,
                                        )
                                    },
                                )
                                .inner
                            },
                            (Game::Genshin, ToolTab::Scanner, layout::Pane::Status) => {
                                scanner_tab::show_status(
                                    ui,
                                    &mut self.state,
                                    &mut self.scan_handle,
                                    is_server_running
                                        || is_capture_busy
                                        || star_rail_busy
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            (Game::Genshin, ToolTab::Manager, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(is_scan_running
                                            || is_capture_busy
                                            || star_rail_busy
                                            || update_in_progress)
                                        && !is_server_running,
                                    |ui| {
                                        manager_tab::show_settings(
                                            ui,
                                            &mut self.state,
                                            is_server_running,
                                        )
                                    },
                                )
                                .inner
                            },
                            (Game::Genshin, ToolTab::Manager, layout::Pane::Status) => {
                                manager_tab::show_status(
                                    ui,
                                    &mut self.state,
                                    &mut self.server_handle,
                                    is_scan_running
                                        || is_capture_busy
                                        || star_rail_busy
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            (Game::StarRail, ToolTab::Scanner, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(genshin_busy
                                            || star_rail_manager_running
                                            || star_rail_capture_busy
                                            || update_in_progress)
                                        && !star_rail_scan_running,
                                    |ui| {
                                        star_rail_scanner_tab::show_settings(
                                            ui,
                                            l,
                                            &mut self.app_config.config.star_rail,
                                            star_rail_scan_running,
                                        )
                                    },
                                )
                                .inner
                            },
                            (Game::StarRail, ToolTab::Scanner, layout::Pane::Status) => {
                                star_rail_scanner_tab::show_status(
                                    ui,
                                    l,
                                    &mut self.app_config.config.star_rail,
                                    &mut self.star_rail,
                                    genshin_busy
                                        || star_rail_manager_running
                                        || star_rail_capture_busy
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            (Game::StarRail, ToolTab::Manager, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(genshin_busy
                                            || star_rail_scan_running
                                            || star_rail_capture_busy
                                            || update_in_progress)
                                        && !star_rail_manager_running,
                                    |ui| {
                                        star_rail_manager_tab::show_settings(
                                            ui,
                                            l,
                                            &mut self.app_config.config.star_rail,
                                            &mut self.star_rail,
                                            star_rail_manager_running,
                                        )
                                    },
                                )
                                .inner
                            },
                            (Game::StarRail, ToolTab::Manager, layout::Pane::Status) => {
                                star_rail_manager_tab::show_status(
                                    ui,
                                    l,
                                    &mut self.app_config.config.star_rail,
                                    &mut self.star_rail,
                                    genshin_busy
                                        || star_rail_scan_running
                                        || star_rail_capture_busy
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            #[cfg(all(feature = "capture", target_os = "windows"))]
                            (Game::Genshin, ToolTab::Capture, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(is_scan_running
                                            || is_server_running
                                            || star_rail_busy
                                            || update_in_progress)
                                        && !is_capture_busy,
                                    |ui| {
                                        capture_tab::show_settings(
                                            ui,
                                            l,
                                            &mut self.capture_tab,
                                            is_capture_busy,
                                        )
                                    },
                                )
                                .inner
                            },
                            #[cfg(all(feature = "capture", target_os = "windows"))]
                            (Game::Genshin, ToolTab::Capture, layout::Pane::Status) => {
                                capture_tab::show_status(
                                    ui,
                                    l,
                                    &mut self.capture_tab,
                                    is_scan_running
                                        || is_server_running
                                        || star_rail_busy
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            #[cfg(all(feature = "capture", target_os = "windows"))]
                            (Game::StarRail, ToolTab::Capture, layout::Pane::Settings) => {
                                ui.add_enabled_ui(
                                    !restart_required
                                        && !(genshin_busy
                                            || star_rail_scan_running
                                            || star_rail_manager_running
                                            || update_in_progress)
                                        && !star_rail_capture_busy,
                                    |ui| {
                                        star_rail_capture_tab::show_settings(
                                            ui,
                                            l,
                                            &mut self.app_config.config.star_rail,
                                            &mut self.star_rail.capture,
                                            star_rail_capture_busy,
                                        )
                                    },
                                )
                                .inner
                            },
                            #[cfg(all(feature = "capture", target_os = "windows"))]
                            (Game::StarRail, ToolTab::Capture, layout::Pane::Status) => {
                                star_rail_capture_tab::show_status(
                                    ui,
                                    l,
                                    &mut self.app_config.config.star_rail,
                                    &mut self.star_rail.capture,
                                    genshin_busy
                                        || star_rail_scan_running
                                        || star_rail_manager_running
                                        || update_in_progress,
                                    restart_required,
                                )
                            },
                            _ => unreachable!("navigation is normalized before rendering"),
                        }
                    });
                });
            });

        // Request repaint while tasks or update check are in progress
        let update_busy = matches!(
            *self.state.update_state.lock().unwrap(),
            UpdateState::Checking
                | UpdateState::Downloading
                | UpdateState::ShowingDialog
                | UpdateState::RestartRequested(_),
        );
        let config_save_pending =
            self.state.config_dirty_since.is_some() || self.app_config.save_pending();
        let any_running = is_scan_running
            || is_server_running
            || is_capture_busy
            || star_rail_busy
            || self.data_refresh.is_pending()
            || update_busy
            || config_save_pending;
        if any_running {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        } else {
            ctx.request_repaint_after(std::time::Duration::from_secs(60));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if !self.save_settings {
            return;
        }
        #[cfg(all(feature = "capture", target_os = "windows"))]
        self.capture_tab.sync_to_config(&mut self.state.user_config);
        self.state.persist_config_now();
        if let Err(error) = self.app_config.persist_now() {
            let failure = state::UiError::from_anyhow(
                state::UiText::new(
                    "星穹铁道设置无法保存；请检查完整错误。",
                    "Star Rail settings could not be saved. Check the full error.",
                ),
                &error,
            );
            log::error!(target: yas::lang::LOCALIZED_LOG_TARGET, "{}", failure.hint_text(self.state.lang));
        }
    }
}

/// Load system CJK font for Chinese text rendering.
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    #[cfg(target_os = "linux")]
    {
        if let Some(font) = yas::utils::find_cjk_font() {
            if let Ok(font_data) = std::fs::read(&font.path) {
                let mut data = egui::FontData::from_owned(font_data);
                // .ttc collections: use the exact face fontdb resolved.
                data.index = font.index;
                fonts
                    .font_data
                    .insert("system_cjk".to_owned(), std::sync::Arc::new(data));
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .push("system_cjk".to_owned());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("system_cjk".to_owned());
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // Try to load Microsoft YaHei from Windows system fonts
        let cjk_font_paths = [
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
        ];

        for path in &cjk_font_paths {
            if let Ok(font_data) = std::fs::read(path) {
                fonts.font_data.insert(
                    "system_cjk".to_owned(),
                    std::sync::Arc::new(egui::FontData::from_owned(font_data)),
                );
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Proportional)
                    .unwrap()
                    .push("system_cjk".to_owned());
                fonts
                    .families
                    .get_mut(&egui::FontFamily::Monospace)
                    .unwrap()
                    .push("system_cjk".to_owned());
                break;
            }
        }
    }
    let mut bold_fonts = Vec::new();
    for (name, path) in [
        ("system_bold", "C:\\Windows\\Fonts\\segoeuib.ttf"),
        ("system_cjk_bold", "C:\\Windows\\Fonts\\msyhbd.ttc"),
    ] {
        if let Ok(data) = std::fs::read(path) {
            fonts.font_data.insert(
                name.to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(data)),
            );
            bold_fonts.push(name.to_owned());
        }
    }
    bold_fonts.extend(
        fonts.families[&egui::FontFamily::Proportional]
            .iter()
            .cloned(),
    );
    fonts
        .families
        .insert(egui::FontFamily::Name("bold".into()), bold_fonts);
    icons::add_to_fonts(&mut fonts);
    ctx.set_fonts(fonts);
}
