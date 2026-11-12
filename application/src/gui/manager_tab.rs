use std::sync::atomic::Ordering;

use eframe::egui;

use super::state::{AppState, TaskStatus, UiError, UiText};
use super::widgets;
use super::worker::{self, TaskHandle};

pub fn show_settings(ui: &mut egui::Ui, state: &mut AppState, is_server_running: bool) {
    let l = state.lang;

    widgets::form(ui, |ui| {
        ui.add_enabled_ui(!is_server_running, |ui| {
            widgets::character_names_section(ui, state, true, true);
            ui.separator();
            widgets::hdr_checkbox(ui, l, &mut state.hdr_mode);
        });
    });

    // === Timing Delays ===
    //
    // Scan API runs the same character/weapon/artifact scanners as the
    // scanner tab, so all their delays apply when the server executes a
    // scan job. Layout: character + inventory + manager in one row.
    widgets::fold(ui, l.t("延迟设置", "Timing"), |ui| {
        ui.add_enabled_ui(!is_server_running, |ui| {
                        let defaults = genshin_scanner::cli::GoodUserConfig::default();
                        {
                            widgets::delay_group(ui, "char_delays", l.t("角色", "Character"), l, &mut [
                                (l.t("打开界面", "Open screen"), &mut state.user_config.char_open_delay, defaults.char_open_delay,
                                    l.t("打开角色界面后等待完全加载的时间", "Wait time for character screen to fully load after opening")),
                                (l.t("关闭界面", "Close screen"), &mut state.user_config.char_close_delay, defaults.char_close_delay,
                                    l.t("关闭角色界面后等待返回主界面的时间", "Wait time after closing character screen to return to main view")),
                                (l.t("面板切换", "Panel switch"), &mut state.user_config.char_tab_delay, defaults.char_tab_delay,
                                    l.t("切换角色详情标签页（天赋/命座等）后的等待", "Wait after switching character detail tabs (talents/constellations etc.)")),
                                (l.t("切换角色", "Next character"), &mut state.user_config.char_next_delay, defaults.char_next_delay,
                                    l.t("切换到下一个角色后等待面板更新的时间", "Wait after switching to next character for panel to update")),
                            ]);
                            widgets::inventory_delays(ui, state, l);
                            widgets::delay_group(ui, "mgr_delays", l.t("管理器", "Manager"), l, &mut [
                                (l.t("画面切换", "Screen transition"), &mut state.user_config.mgr_transition_delay, defaults.mgr_transition_delay,
                                    l.t("打开/关闭角色面板等大画面切换后的等待", "Wait after major screen transitions like opening/closing character panel")),
                                (l.t("操作按钮", "Action button"), &mut state.user_config.mgr_action_delay, defaults.mgr_action_delay,
                                    l.t("点击锁定/装备等操作按钮后的等待", "Wait after clicking action buttons like lock/equip")),
                                (l.t("格子点击", "Grid cell click"), &mut state.user_config.mgr_cell_delay, defaults.mgr_cell_delay,
                                    l.t("锁定切换后点击下一个格子前的等待", "Wait before clicking the next grid cell after a lock toggle")),
                                (l.t("滚动等待", "Scroll settle"), &mut state.user_config.mgr_scroll_delay, defaults.mgr_scroll_delay,
                                    l.t("翻页后等待物品列表稳定的时间", "Wait after scrolling for item list to stabilize")),
                            ]);
                        }
                    });
    });

    // === Advanced Options (shared with scanner tab) ===
    widgets::fold(ui, l.t("高级选项", "Advanced"), |ui| {
        ui.add_enabled_ui(!is_server_running, |ui| {
            widgets::manager_debug_options(
                ui,
                l,
                &mut state.verbose,
                &mut state.dump_images,
                &mut state.dump_job_data,
            );
        });
    });
}

/// Top action bar with port, start/stop button, and status.
pub fn show_status(
    ui: &mut egui::Ui,
    state: &mut AppState,
    handle: &mut Option<TaskHandle>,
    game_busy: bool,
    restart: bool,
) {
    let l = state.lang;
    if restart {
        super::theme::restart_required(
            ui,
            l,
            handle
                .as_ref()
                .and_then(TaskHandle::native_failure)
                .as_ref(),
        );
        return;
    }
    let running = handle.as_ref().is_some_and(|h| !h.is_finished());
    let status = worker::try_task_status(&state.server_status);
    widgets::status_content(ui, "genshin-manager-status", 0, |ui| {
        if handle.as_ref().is_some_and(TaskHandle::is_stopping) {
            super::theme::task_status(
                ui,
                l,
                status.as_ref(),
                l.t("正在停止服务器", "Stopping server"),
            );
        } else {
            super::manager_progress::show(ui, l, &state.server_job, status.as_ref());
        }
    });
    ui.add_space(12.0);
    widgets::server_port(ui, l, &mut state.server_port, !running && !game_busy);
    #[cfg(all(feature = "capture", target_os = "windows"))]
    ui.add_enabled_ui(!running && !game_busy, |ui| {
        ui.checkbox(&mut state.user_config.continuous_capture, l.t("持续同步（抓包）", "Continuous Sync (Capture)"))
            .on_hover_text(l.t("启动服务器后持续接收游戏数据，每十分钟保存有变化的最新导出。请在登录游戏前启动。", "Receive game data while the server runs and save changed exports every ten minutes. Start before logging in."));
    });
    if running {
        let stopping = handle.as_ref().is_some_and(TaskHandle::is_stopping);
        if super::theme::primary_action(
            ui,
            !stopping,
            if stopping {
                l.t("正在停止", "Stopping")
            } else {
                l.t("停止服务器", "Stop server")
            },
        )
        .clicked()
        {
            if let Some(h) = handle {
                h.stop();
            }
        }
    } else if super::theme::primary_action(ui, !game_busy, l.t("启动服务器", "Start server"))
        .clicked()
    {
        if let Err(e) = super::privilege::ensure_admin_for_action() {
            let error = UiError::from_anyhow(
                UiText::new("请以管理员身份启动程序", "Restart the app as administrator"),
                &e,
            );
            error.log_diagnostics();
            *state.server_status.lock().unwrap() = TaskStatus::Failed(error);
        } else {
            state.server_enabled.store(true, Ordering::Relaxed);
            state.persist_config_now();
            *handle = Some(worker::spawn_server(state));
        }
    }
}
