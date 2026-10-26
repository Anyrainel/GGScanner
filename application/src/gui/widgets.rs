//! Shared UI widgets used by both scanner and manager tabs.

use eframe::egui;

use super::state::{AppState, Lang, UiError};

/// Native egui frames provide the same spacing and hierarchy for every form.
pub fn section(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    form(ui, |ui| group(ui, title, body));
}

pub fn group(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.label(section_title(ui, title));
    body(ui);
}

fn section_title(ui: &egui::Ui, title: &str) -> egui::RichText {
    let family = egui::FontFamily::Name("bold".into());
    let text = egui::RichText::new(title).size(13.0).strong();
    if ui.fonts(|fonts| fonts.families().contains(&family)) {
        text.family(family)
    } else {
        text
    }
}

pub fn fold(ui: &mut egui::Ui, title: &str, body: impl FnOnce(&mut egui::Ui)) {
    form(ui, |ui| {
        ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
        ui.spacing_mut().item_spacing.y = 4.0;
        ui.spacing_mut().interact_size.y = 26.0;
        egui::CollapsingHeader::new(section_title(ui, title)).show_unindented(ui, |ui| {
            ui.add_space(5.0);
            body(ui);
        });
    });
}

pub fn form(ui: &mut egui::Ui, body: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    egui::Frame::new()
        .fill(super::theme::palette(ui.visuals().dark_mode).surface)
        .stroke(egui::Stroke::new(
            1.0,
            super::theme::palette(ui.visuals().dark_mode).border,
        ))
        .corner_radius(8.0)
        .inner_margin(egui::vec2(12.0, 10.0))
        .show(ui, |ui| {
            ui.set_width((width - 26.0).max(1.0));
            ui.style_mut().override_font_id = Some(egui::FontId::proportional(13.0));
            ui.spacing_mut().item_spacing = egui::vec2(7.0, 4.0);
            ui.spacing_mut().button_padding = egui::vec2(8.0, 4.0);
            ui.spacing_mut().interact_size.y = 26.0;
            body(ui);
        });
}

/// Two compact columns, with each category and its cap kept on the same row.
pub fn scan_limits(ui: &mut egui::Ui, lang: Lang, values: &mut [(&str, &mut usize)]) {
    ui.horizontal(|ui| {
        ui.strong(lang.t("扫描上限", "Scan limits"));
        hint(ui, lang.t("0 = 全部", "0 = all"));
    });
    let gap = 12.0;
    let width = (ui.available_width() - gap) / 2.0;
    for pair in values.chunks_mut(2) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (label, value) in pair {
                ui.allocate_ui_with_layout(
                    egui::vec2(width, 28.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        ui.add_sized(
                            [width - 54.0, 26.0],
                            egui::Label::new(*label).truncate().halign(egui::Align::Min),
                        )
                        .on_hover_text(*label);
                        ui.add_sized(
                            [50.0, 26.0],
                            egui::DragValue::new(*value).range(0..=10_000).speed(0.0),
                        );
                    },
                );
            }
        });
    }
}

/// Labels and controls share a fixed-height row and a stable label column.
pub fn field_row(ui: &mut egui::Ui, label: &str, control: impl FnOnce(&mut egui::Ui)) {
    let width = ui.available_width();
    let label_width = (width * 0.35).clamp(65.0, 112.0);
    let (row, _) = ui.allocate_exact_size(egui::vec2(width, 28.0), egui::Sense::hover());
    let label_rect =
        egui::Rect::from_min_max(row.min, egui::pos2(row.left() + label_width, row.bottom()));
    let control_rect =
        egui::Rect::from_min_max(egui::pos2(label_rect.right() + 8.0, row.top()), row.max);
    let mut label_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(label_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    label_ui
        .add(egui::Label::new(label).truncate())
        .on_hover_text(label);
    let mut control_ui = ui.new_child(
        egui::UiBuilder::new()
            .max_rect(control_rect)
            .layout(egui::Layout::left_to_right(egui::Align::Center)),
    );
    control(&mut control_ui);
}

pub fn hint(ui: &mut egui::Ui, text: &str) {
    ui.add(
        egui::Label::new(
            egui::RichText::new(text)
                .size(12.0)
                .color(super::theme::palette(ui.visuals().dark_mode).muted),
        )
        .wrap(),
    );
}

/// Only task details scroll. Keep export choices and Start/Stop reachable even
/// at the minimum window height.
pub fn status_content(
    ui: &mut egui::Ui,
    id: &str,
    options: usize,
    body: impl FnOnce(&mut egui::Ui),
) {
    let footer_height =
        60.0 + options as f32 * (ui.spacing().interact_size.y + ui.spacing().item_spacing.y);
    let height = (ui.available_height() - footer_height).max(24.0);
    let width = (ui.available_width() - ui.spacing().scroll.allocated_width()).max(1.0);
    egui::ScrollArea::vertical()
        .id_salt(id)
        .max_height(height)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            ui.set_width(width);
            body(ui);
        });
}

/// Keep text, selection and caret centered when a form gives the input extra height.
pub fn singleline_input(text: &mut dyn egui::TextBuffer) -> egui::TextEdit<'_> {
    egui::TextEdit::singleline(text).vertical_align(egui::Align::Center)
}

/// Both games expose the same export choices with identical copy and order.
pub fn scan_export_options(ui: &mut egui::Ui, lang: Lang, only_latest: &mut bool) {
    ui.checkbox(
        only_latest,
        lang.t("仅保留最新导出", "Keep latest export only"),
    );
}

pub fn partial_export_hint(
    lang: Lang,
    progress: &super::task_progress::TaskProgress,
    limits: &[(&str, usize)],
) -> &'static str {
    let limit_reached = limits.iter().any(|(key, limit)| {
        *limit > 0
            && progress
                .steps
                .iter()
                .any(|step| step.key == *key && step.completed >= *limit)
    });
    if limit_reached {
        lang.t("已达到扫描上限。", "Scan limit reached.")
    } else if progress
        .steps
        .iter()
        .any(|step| step.state == super::task_progress::StepState::Interrupted)
    {
        lang.t("部分扫描未完成。", "Some categories did not finish.")
    } else {
        lang.t("扫描未完成。", "Scan incomplete.")
    }
}

pub fn scan_debug_options(
    ui: &mut egui::Ui,
    lang: Lang,
    verbose: &mut bool,
    stop_on_failure: &mut bool,
    dump_images: &mut bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(verbose, lang.t("开发者调试日志", "Developer debug logs"))
            .on_hover_text(lang.t(
                "仅供开发调试使用。普通错误排查所需的信息会默认记录。",
                "For developer debugging only. Information needed to diagnose ordinary failures is logged by default.",
            ));
        ui.checkbox(
        stop_on_failure,
        lang.t("识别失败时停止", "Stop on OCR errors"),
    )
    .on_hover_text(lang.t(
        "调试时在失败条目处停止。关闭时跳过失败条目，日志会保留失败原因。",
        "Stop at a failed entry for debugging. When off, skip failed entries and log their causes.",
    ));
        ui.checkbox(dump_images, lang.t("保存 OCR 截图", "Save OCR screenshots"));
    });
}

pub fn manager_debug_options(
    ui: &mut egui::Ui,
    lang: Lang,
    verbose: &mut bool,
    dump_images: &mut bool,
    dump_job_data: &mut bool,
) {
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(verbose, lang.t("开发者调试日志", "Developer debug logs"))
            .on_hover_text(lang.t(
                "仅供开发调试使用。普通错误排查所需的信息会默认记录。",
                "For developer debugging only. Information needed to diagnose ordinary failures is logged by default.",
            ));
        ui.checkbox(dump_images, lang.t("保存 OCR 截图", "Save OCR screenshots"));
        ui.checkbox(dump_job_data, lang.t("保存请求数据", "Save request data"));
    });
}

/// Show a concise failure reason with an action to copy the full diagnostic.
pub fn error_card(ui: &mut egui::Ui, l: Lang, error: &UiError) {
    let hint = error.hint_text(l);
    let copy_text = error.copy_text(l);

    ui.scope(|ui| {
        ui.set_width(ui.available_width());
        ui.colored_label(
            ui.visuals().error_fg_color,
            egui::RichText::new(hint).strong(),
        );
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if ui
                .add(
                    egui::Button::new(l.t("复制完整错误", "Copy full error"))
                        .min_size(egui::vec2(0.0, 26.0)),
                )
                .clicked()
            {
                ui.ctx().copy_text(copy_text.clone());
            }
        });
    });
}

pub fn export_file(ui: &mut egui::Ui, l: Lang, path: &str) {
    let error_id = ui.id().with(("reveal-export-error", path));
    if super::theme::secondary_action(ui, l.t("打开文件夹", "Open folder")).clicked() {
        match super::file_location::reveal_file(path) {
            Ok(()) => ui.ctx().data_mut(|data| data.remove::<UiError>(error_id)),
            Err(error) => {
                let error = UiError::from_anyhow(
                    super::state::UiText::new(
                        "无法打开导出文件所在文件夹。请确认文件仍然存在，然后重试。",
                        "Could not open the export folder. Check that the file still exists and try again.",
                    ),
                    &error,
                );
                log::error!(
                    "Could not reveal export file: {}",
                    error.technical_details(l)
                );
                ui.ctx().data_mut(|data| data.insert_temp(error_id, error));
            },
        }
    }
    let error = ui.ctx().data(|data| data.get_temp::<UiError>(error_id));
    if let Some(error) = error {
        error_card(ui, l, &error);
    }
}

/// Keep the saved location selectable in the result, and its action in the footer.
pub fn export_location(ui: &mut egui::Ui, path: &str) {
    let filename = path
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path);
    ui.add(
        egui::Label::new(
            egui::RichText::new(filename)
                .size(12.0)
                .color(super::theme::palette(ui.visuals().dark_mode).muted),
        )
        .truncate(),
    )
    .on_hover_text(path);
}

pub fn export_action(ui: &mut egui::Ui, lang: Lang, status: Option<&super::state::TaskStatus>) {
    if let Some(super::state::TaskStatus::Exported { path, .. }) = status {
        export_file(ui, lang, path);
        ui.add_space(4.0);
    }
}

/// Numeric input for u64 values (clamped to 5000).
pub fn num_input_u64(ui: &mut egui::Ui, value: &mut u64, width: f32) {
    ui.add_sized(
        [width, 24.0],
        egui::DragValue::new(value).range(0..=5000).speed(1.0),
    );
}

/// A labeled group of delay fields with aligned value controls.
/// Each field is `(label, value, default, tooltip)`. Values below their default get a
/// warning asterisk, and a footnote is shown underneath when any field is below default.
pub fn delay_group(
    ui: &mut egui::Ui,
    id: &str,
    category: &str,
    l: Lang,
    fields: &mut [(&str, &mut u64, u64, &str)],
) {
    ui.add_space(6.0);
    ui.label(section_title(ui, &format!("{category} (ms)")));
    let any_below = fields
        .iter()
        .any(|(_, value, default, _)| **value < *default);
    let warn_color = super::theme::palette(ui.visuals().dark_mode).warning;
    let gap = 16.0;
    let columns = 2;
    let width = (ui.available_width() - gap * (columns - 1) as f32) / columns as f32;
    ui.push_id(id, |ui| {
        for row in fields.chunks_mut(columns) {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for (label, value, default, tooltip) in row {
                    let below = **value < *default;
                    let label_text = if below {
                        format!("{}*", label)
                    } else {
                        label.to_string()
                    };
                    let rich = if below {
                        egui::RichText::new(&label_text).color(warn_color)
                    } else {
                        egui::RichText::new(&label_text)
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(width, 26.0),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    num_input_u64(ui, value, 60.0);
                                    ui.with_layout(
                                        egui::Layout::left_to_right(egui::Align::Center),
                                        |ui| {
                                            ui.spacing_mut().item_spacing.x = 3.0;
                                            // Reserve the help icon even when a long label truncates.
                                            let label_width =
                                                (ui.available_width() - 19.0).max(0.0);
                                            ui.add_sized(
                                                egui::vec2(label_width, 20.0),
                                                egui::Label::new(rich)
                                                    .halign(egui::Align::Min)
                                                    .truncate(),
                                            )
                                            .on_hover_cursor(egui::CursorIcon::Help)
                                            .on_hover_text(*tooltip);
                                            let (rect, response) = ui.allocate_exact_size(
                                                egui::Vec2::splat(16.0),
                                                egui::Sense::hover(),
                                            );
                                            super::icons::paint(
                                                ui,
                                                rect,
                                                super::icons::Icon::Help,
                                                super::theme::palette(ui.visuals().dark_mode).muted,
                                            );
                                            response
                                                .on_hover_cursor(egui::CursorIcon::Help)
                                                .on_hover_text(*tooltip);
                                        },
                                    );
                                },
                            );
                        },
                    );
                }
            });
        }
    });
    if any_below {
        ui.colored_label(
            warn_color,
            l.t(
                "* 低于默认值，可能导致扫描不稳定",
                "* Below default — may cause unreliable scans",
            ),
        );
    }
}

pub fn server_port(ui: &mut egui::Ui, lang: Lang, port: &mut u16, enabled: bool) {
    ui.add_enabled_ui(enabled, |ui| {
        field_row(
            ui,
            lang.t("本地服务器端口", "Local server port"),
            |ui| {
                ui.add(egui::DragValue::new(port).range(1024..=65535).speed(0.0));
            },
        );
    });
    ui.add_space(4.0);
}

/// Character names section — shared between scanner and manager tabs.
/// Shows the 4 renameable character name fields with required-field validation.
pub fn character_names_section(
    ui: &mut egui::Ui,
    state: &mut AppState,
    enabled: bool,
    required: bool,
) {
    let l = state.lang;

    ui.add_enabled_ui(enabled, |ui| {
        field_row(
            ui,
            if required {
                l.t("游戏内昵称*", "In-game nickname*")
            } else {
                l.t("游戏内昵称", "In-game nickname")
            },
            |ui| {
                if required && state.user_config.traveler_name.trim().is_empty() {
                    ui.visuals_mut().widgets.inactive.bg_stroke.color =
                        super::theme::palette(ui.visuals().dark_mode).warning;
                }
                ui.add(
                    singleline_input(&mut state.user_config.traveler_name)
                        .desired_width(ui.available_width())
                        .min_size(egui::vec2(0.0, 26.0)),
                );
            },
        );
        egui::CollapsingHeader::new(l.t("其他可改名角色", "Other renamed characters"))
            .show_unindented(ui, |ui| {
                for (label, name) in [
                    (
                        l.t("流浪者", "Wanderer"),
                        &mut state.user_config.wanderer_name,
                    ),
                    (
                        l.t("奇偶·男性", "Manekin"),
                        &mut state.user_config.manekin_name,
                    ),
                    (
                        l.t("奇偶·女性", "Manekina"),
                        &mut state.user_config.manekina_name,
                    ),
                ] {
                    field_row(ui, label, |ui| {
                        ui.add(
                            singleline_input(name)
                                .desired_width(ui.available_width())
                                .min_size(egui::vec2(0.0, 26.0)),
                        );
                    });
                }
            });

        if state.names_need_attention && !state.missing_required_character_names() {
            state.names_need_attention = false;
        }
    });
}

/// Inventory delay fields — shared between scanner and manager tabs.
/// Renders as a delay_group with the 7 inventory timing fields.
pub fn inventory_delays(ui: &mut egui::Ui, state: &mut AppState, l: Lang) {
    let defaults = genshin_scanner::cli::GoodUserConfig::default();
    delay_group(ui, "inv_delays", l.t("背包", "Inventory"), l, &mut [
        (l.t("打开背包", "Open backpack"), &mut state.user_config.inv_open_delay, defaults.inv_open_delay,
            l.t("按下快捷键后等待背包界面完全加载的时间", "Wait time after pressing hotkey for backpack to fully load")),
        (l.t("标签切换", "Tab switch"), &mut state.user_config.inv_tab_delay, defaults.inv_tab_delay,
            l.t("切换武器/圣遗物标签后等待内容加载的时间", "Wait time after switching weapon/artifact tab for content to load")),
        (l.t("翻页等待", "Page scroll"), &mut state.user_config.inv_scroll_delay, defaults.inv_scroll_delay,
            l.t("翻页后等待物品列表稳定的时间", "Wait time after scrolling for item list to stabilize")),
        (l.t("武器面板延迟", "Weapon panel delay"), &mut state.user_config.weapon_panel_delay, defaults.weapon_panel_delay,
            l.t("点击武器后固定等待时间，然后检查面板稳定", "Fixed wait after clicking a weapon, then verify panel is stable")),
        (l.t("圣遗物初始等待", "Artifact initial wait"), &mut state.user_config.artifact_initial_wait, defaults.artifact_initial_wait,
            l.t("点击圣遗物后、开始检测面板变化前的最小等待", "Minimum wait after clicking an artifact before starting panel change detection")),
        (l.t("圣遗物面板超时", "Artifact panel timeout"), &mut state.user_config.artifact_panel_timeout, defaults.artifact_panel_timeout,
            l.t("等待详情更新并稳定的时间上限；超时后重试确认", "Wait limit for updated, stable details; retry confirmation on timeout")),
        (l.t("圣遗物额外延迟", "Artifact extra delay"), &mut state.user_config.artifact_extra_delay, defaults.artifact_extra_delay,
            l.t("延长详情保持稳定的确认时间；等待期间仍会检查画面（通常为0）", "Extend the stable-detail confirmation time while continuing to check the image (usually 0)")),
    ]);
}

/// Keep the Windows control unchanged; Linux capture currently requires SDR.
pub fn hdr_checkbox(ui: &mut egui::Ui, lang: Lang, hdr: &mut bool) -> bool {
    #[cfg(not(target_os = "linux"))]
    {
        ui.checkbox(hdr, lang.t("游戏使用 HDR", "Game uses HDR"))
            .changed()
    }
    #[cfg(target_os = "linux")]
    {
        let enabled = *hdr; // allow clearing a setting imported from Windows
        ui.add_enabled(
            enabled,
            egui::Checkbox::new(
                hdr,
                lang.t(
                    "游戏使用 HDR（Linux 暂不支持，请关闭）",
                    "Game uses HDR (unsupported on Linux; turn off)",
                ),
            ),
        )
        .changed()
    }
}
