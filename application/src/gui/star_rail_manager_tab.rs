use super::{
    star_rail_state::StarRailState,
    star_rail_worker,
    state::{Lang, TaskStatus, UiError, UiText},
    widgets, worker,
};
use crate::config::StarRailSettings;
use eframe::egui;

pub fn show_status(
    ui: &mut egui::Ui,
    lang: Lang,
    settings: &mut StarRailSettings,
    state: &mut StarRailState,
    game_busy: bool,
    restart_required: bool,
) {
    if restart_required {
        super::theme::restart_required(
            ui,
            lang,
            state
                .manager_handle
                .as_ref()
                .and_then(worker::TaskHandle::native_failure)
                .as_ref(),
        );
        return;
    }
    let running = state.manager_running();
    let status = worker::try_task_status(&state.manager_status);
    widgets::status_content(ui, "hsr-manager-status", 0, |ui| {
        super::theme::task_status(
            ui,
            lang,
            status.as_ref(),
            lang.t("服务器未启动", "Server is off"),
        );
        let job = state.manager_job.lock().unwrap().clone();
        if job.kind.is_some() {
            widgets::hint(
                ui,
                &format!(
                    "{} · {}",
                    match job.kind {
                        Some("manage") => lang.t("管理请求", "Manage request"),
                        Some("scan") => lang.t("扫描请求", "Scan request"),
                        _ => lang.t("请求数量", "Requested"),
                    },
                    job.count
                ),
            );
            if let Ok(progress) = state.manager_progress.try_lock() {
                super::task_progress::show(ui, lang, &progress);
            }
        }
        if running && job.phase == "idle" {
            widgets::hint(
                ui,
                lang.t("在 GGArtifact 中选择操作", "Choose an action in GGArtifact"),
            );
        }
    });
    ui.add_space(12.0);
    widgets::server_port(ui, lang, &mut settings.manager_port, !running && !game_busy);
    #[cfg(all(feature = "capture", target_os = "windows"))]
    ui.add_enabled_ui(!running && !game_busy, |ui| {
        ui.checkbox(&mut settings.continuous_capture, lang.t("持续抓包同步", "Continuous capture sync"))
            .on_hover_text(lang.t("启动服务器后持续接收游戏数据，每十分钟保存有变化的最新导出。请在登录游戏前启动。", "Receive game data while the server runs and save changed exports every ten minutes. Start before logging in."));
    });
    if running {
        let stopping = state
            .manager_handle
            .as_ref()
            .is_some_and(worker::TaskHandle::is_stopping);
        if super::theme::primary_action(
            ui,
            !stopping,
            if stopping {
                lang.t("正在停止", "Stopping")
            } else {
                lang.t("停止服务器", "Stop server")
            },
        )
        .clicked()
        {
            if let Some(handle) = &state.manager_handle {
                handle.stop();
            }
        }
    } else if super::theme::primary_action(ui, !game_busy, lang.t("启动服务器", "Start server"))
        .clicked()
    {
        if let Err(error) = super::privilege::ensure_admin_for_action() {
            let failure = UiError::from_anyhow(
                UiText::new("请以管理员身份启动程序", "Restart the app as administrator"),
                &error,
            );
            failure.log_diagnostics();
            *state.manager_status.lock().unwrap() = TaskStatus::Failed(failure);
        } else {
            state.manager_handle = Some(star_rail_worker::spawn_manager_server(
                settings,
                state.manager_status.clone(),
                state.manager_progress.clone(),
                state.manager_job.clone(),
            ));
        }
    }
}

pub fn show_settings(
    ui: &mut egui::Ui,
    lang: Lang,
    settings: &mut StarRailSettings,
    _state: &mut StarRailState,
    is_running: bool,
) {
    ui.add_enabled_ui(!is_running, |ui| {
        widgets::form(ui, |ui| {
            super::star_rail_scanner_tab::trailblazer_row(ui, lang, settings, false);
            ui.separator();
            if widgets::hdr_checkbox(ui, lang, &mut settings.hdr_mode) {
                settings.set_hdr_mode(settings.hdr_mode);
            }
        });
        super::star_rail_scanner_tab::timing_settings(ui, lang, settings, false);
        widgets::fold(ui, lang.t("高级选项", "Advanced"), |ui| {
            widgets::manager_debug_options(
                ui,
                lang,
                &mut settings.verbose,
                &mut settings.dump_images,
                &mut settings.dump_job_data,
            );
        });
    });
}
