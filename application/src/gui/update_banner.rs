use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use eframe::egui;

use super::state::{Lang, UiError, UiText, UpdateState};
use super::widgets;

/// Show the update notification banner when an update is available.
///
/// Call this from the `eframe::App::ui` method, before the central panel.
/// `update_state` is the shared state that tracks the update lifecycle.
pub fn show(
    root_ui: &mut egui::Ui,
    l: Lang,
    update_state: &Arc<Mutex<UpdateState>>,
    game_busy: bool,
) {
    let ctx = root_ui.ctx().clone();
    let state_snapshot = update_state.lock().unwrap().clone();

    let show = !matches!(state_snapshot, UpdateState::None | UpdateState::Checking);
    if !show {
        return;
    }

    egui::Panel::top("update_banner").show(root_ui, |ui| match state_snapshot {
        UpdateState::Available {
            ref latest_version,
            ref download_url,
            ref exe_path,
        } => {
            let current = genshin_scanner::updater::current_version_display();
            let message = egui::RichText::new(l.t(
                &format!("发现新版本: {} → {}", current, latest_version),
                &format!("Update available: {} → {}", current, latest_version),
            ))
            .strong()
            .family(egui::FontFamily::Name("bold".into()))
            .color(egui::Color32::from_rgb(255, 200, 50));
            let download = l.t("下载更新", "Download Update");
            let skip = l.t("跳过", "Skip");
            let text_width = |text: egui::WidgetText, style| {
                text.into_galley(ui, Some(egui::TextWrapMode::Extend), f32::INFINITY, style)
                    .size()
                    .x
            };
            let row_width = text_width(message.clone().into(), egui::TextStyle::Body)
                + text_width(download.into(), egui::TextStyle::Button)
                + text_width(skip.into(), egui::TextStyle::Button)
                + 4.0 * ui.spacing().button_padding.x
                + 2.0 * ui.spacing().item_spacing.x;
            let leading_space = ((ui.available_width() - row_width) / 2.0).max(0.0);
            ui.horizontal(|ui| {
                ui.add_space(leading_space);
                ui.label(message);
                if ui.add_enabled(!game_busy, egui::Button::new(download))
                    .on_disabled_hover_text(l.t("请先停止当前任务。", "Stop the current task first."))
                    .clicked() {
                    let arc = update_state.clone();
                    let url = download_url.clone();
                    let exe_path = exe_path.clone();
                    let lang = l;
                    *update_state.lock().unwrap() = UpdateState::Downloading;
                    let worker_state = arc.clone();
                    let worker_ctx = ctx.clone();
                    let spawn_result = std::thread::Builder::new()
                        .name("update-download".to_owned())
                        .spawn(move || {
                        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            genshin_scanner::updater::download_and_replace(&exe_path, &url)
                        }));
                        match result {
                            Ok(Ok(())) => {
                                *worker_state.lock().unwrap() = UpdateState::ShowingDialog;
                                show_restart_dialog(exe_path, worker_state, lang);
                            },
                            Ok(Err(error)) => {
                                let failure = UiError::from_anyhow(
                                    UiText::new(
                                        "更新无法下载或安装。可前往 GitHub 发布页手动下载，或检查网络连接、磁盘空间和安全软件设置后重试。",
                                        "The update could not be downloaded or installed. Download it manually from the GitHub release page, or check the network connection, disk space, and security software before retrying.",
                                    ),
                                    &error,
                                );
                                failure.log_diagnostics();
                                *worker_state.lock().unwrap() = UpdateState::Failed(failure);
                            },
                            Err(panic_info) => {
                                let failure = UiError::from_panic(
                                    UiText::new(
                                        "更新任务因意外的内部错误而停止。请复制完整错误并报告此问题。",
                                        "The update task stopped because of an unexpected internal error. Copy the full error and report this problem.",
                                    ),
                                    panic_info.as_ref(),
                                );
                                failure.log_diagnostics();
                                *worker_state.lock().unwrap() = UpdateState::Failed(failure);
                            },
                        }
                        worker_ctx.request_repaint();
                    });
                    if let Err(error) = spawn_result {
                        let failure = UiError::from_error(
                            UiText::new(
                                "更新后台任务无法启动。请检查系统资源，然后重试。",
                                "The update background task could not start. Check available system resources, then retry.",
                            ),
                            error,
                        );
                        failure.log_diagnostics();
                        *arc.lock().unwrap() = UpdateState::Failed(failure);
                    }
                }
                if ui.button(skip).clicked() {
                    *update_state.lock().unwrap() = UpdateState::None;
                }
            });
        },
        UpdateState::Downloading => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(l.t("正在下载更新...", "Downloading update..."));
            });
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        },
        UpdateState::ShowingDialog | UpdateState::RestartRequested(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(
                    egui::RichText::new(l.t("更新已就绪...", "Update ready..."))
                        .color(egui::Color32::from_rgb(100, 255, 100)),
                );
            });
        },
        UpdateState::Ready => {
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(l.t(
                        "更新已就绪，请重启程序。",
                        "Update ready. Please restart the application.",
                    ))
                    .color(egui::Color32::from_rgb(100, 255, 100)),
                );
            });
        },
        UpdateState::Failed(ref error) => {
            ui.horizontal(|ui| {
                ui.hyperlink_to(
                    l.t("前往 GitHub 手动下载", "Download manually on GitHub"),
                    genshin_scanner::updater::RELEASES_URL,
                );
                if ui.button(l.t("关闭", "Dismiss")).clicked() {
                    *update_state.lock().unwrap() = UpdateState::None;
                }
            });
            widgets::error_card(ui, l, error);
        },
        _ => {},
    });
}

/// Prepare the local executable and check for updates in the background.
pub fn spawn_check(asset_name: &'static str, update_state: &Arc<Mutex<UpdateState>>) {
    // Self-update is Windows-only (release assets are .exe); skip silently
    // instead of nagging with a failure banner.
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (asset_name, update_state);
        return;
    }
    #[cfg(target_os = "windows")]
    {
        let state = update_state.clone();
        let spawn_failure_state = state.clone();
        let spawn_result = std::thread::Builder::new()
        .name("update-check".to_owned())
        .spawn(move || {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            use genshin_scanner::updater::{check_for_update, prepare_local_executable};

            let exe_path = prepare_local_executable(asset_name)?;
            check_for_update(asset_name).map(|status| (exe_path, status))
        }));
        match result {
            Ok(Ok((exe_path, genshin_scanner::updater::UpdateStatus::UpdateAvailable {
                latest_version,
                download_url,
                ..
            }))) => {
                *state.lock().unwrap() = UpdateState::Available {
                    latest_version,
                    download_url,
                    exe_path,
                };
            },
            Ok(Ok((_, genshin_scanner::updater::UpdateStatus::ManualDownload { latest_version }))) => {
                let failure = UiError::from_message(
                    UiText::new(
                        format!("发现新版本 {latest_version}，但未找到此版本的程序文件。请前往 GitHub 发布页手动下载。当前版本仍可继续使用。"),
                        format!("Release {latest_version} is available, but its executable for this edition was not found. Download manually from the GitHub release page. You can keep using the current version."),
                    ),
                    format!("No executable for {asset_name} in release {latest_version}"),
                );
                failure.log_diagnostics();
                *state.lock().unwrap() = UpdateState::Failed(failure);
            },
            Ok(Ok(_)) => {
                *state.lock().unwrap() = UpdateState::None;
            },
            Ok(Err(error)) => {
                let failure = UiError::from_anyhow(
                    UiText::new(
                        "无法检查更新。当前版本仍可继续使用；请检查网络连接，重启程序以重试，或关闭此提示。",
                        "Updates could not be checked. You can keep using this version; check the network connection, restart the application to retry, or dismiss this message.",
                    ),
                    &error,
                );
                failure.log_diagnostics();
                *state.lock().unwrap() = UpdateState::Failed(failure);
            },
            Err(panic_info) => {
                let failure = UiError::from_panic(
                    UiText::new(
                        "检查更新的后台任务因意外的内部错误而停止。当前版本仍可继续使用。",
                        "The background update check stopped because of an unexpected internal error. You can keep using the current version.",
                    ),
                    panic_info.as_ref(),
                );
                failure.log_diagnostics();
                *state.lock().unwrap() = UpdateState::Failed(failure);
            },
        }
    });
        if let Err(error) = spawn_result {
            *spawn_failure_state.lock().unwrap() = UpdateState::Failed(UiError::from_error(
            UiText::new(
                "检查更新的后台任务无法启动。当前版本仍可继续使用；请检查系统资源后重启程序。",
                "The background update check could not start. You can keep using the current version; check system resources, then restart the application.",
            ),
            error,
        ));
        }
    } // cfg(windows)
}

/// Show a native OS dialog asking the user to restart now or later.
fn show_restart_dialog(exe_path: PathBuf, update_state: Arc<Mutex<UpdateState>>, lang: Lang) {
    let (title, description) = match lang {
        Lang::Zh => ("更新完成", "更新已下载完成。是否立即重启？"),
        Lang::En => (
            "Update Complete",
            "The update has been downloaded. Restart now?",
        ),
    };

    let result = rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Info)
        .set_title(title)
        .set_description(description)
        .set_buttons(rfd::MessageButtons::YesNo)
        .show();

    match result {
        rfd::MessageDialogResult::Yes => {
            yas::log_info!("用户选择立即重启", "User chose to restart now");
            *update_state.lock().unwrap() = UpdateState::RestartRequested(exe_path);
        },
        _ => {
            yas::log_info!("用户选择稍后重启", "User chose to restart later");
            *update_state.lock().unwrap() = UpdateState::Ready;
        },
    }
}
