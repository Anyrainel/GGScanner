use eframe::egui::{self, Color32, RichText, Stroke};

use super::state::{Lang, TaskStatus};

pub const BACKGROUND: Color32 = Color32::from_rgb(19, 23, 28);
pub const SURFACE: Color32 = Color32::from_rgb(28, 33, 40);
pub const BORDER: Color32 = Color32::from_rgb(51, 61, 73);
pub const ACCENT: Color32 = Color32::from_rgb(124, 220, 192);
pub const SUCCESS: Color32 = Color32::from_rgb(158, 204, 155);
pub const CONTROL_BORDER: Color32 = Color32::from_rgb(103, 119, 137);
pub const HOVER: Color32 = Color32::from_rgb(42, 51, 61);
pub const PRESSED: Color32 = Color32::from_rgb(52, 65, 78);
pub const MUTED: Color32 = Color32::from_rgb(165, 177, 192);
pub const SELECTED: Color32 = Color32::from_rgb(35, 55, 51);
pub const ERROR: Color32 = Color32::from_rgb(244, 132, 137);
pub const WARNING: Color32 = Color32::from_rgb(238, 190, 110);

#[derive(Clone, Copy)]
pub struct Palette {
    pub background: Color32,
    pub surface: Color32,
    pub border: Color32,
    pub accent: Color32,
    pub success: Color32,
    pub control_border: Color32,
    pub hover: Color32,
    pub pressed: Color32,
    pub muted: Color32,
    pub selected: Color32,
    pub error: Color32,
    pub warning: Color32,
    pub foreground: Color32,
    pub input: Color32,
    pub primary_text: Color32,
    pub primary_hover: Color32,
    pub primary_pressed: Color32,
}

pub fn palette(dark: bool) -> Palette {
    if dark {
        Palette {
            background: BACKGROUND,
            surface: SURFACE,
            border: BORDER,
            accent: ACCENT,
            success: SUCCESS,
            control_border: CONTROL_BORDER,
            hover: HOVER,
            pressed: PRESSED,
            muted: MUTED,
            selected: SELECTED,
            error: ERROR,
            warning: WARNING,
            foreground: Color32::from_rgb(237, 242, 247),
            input: Color32::from_rgb(17, 23, 30),
            primary_text: Color32::from_rgb(16, 41, 31),
            primary_hover: Color32::from_rgb(155, 234, 211),
            primary_pressed: Color32::from_rgb(108, 203, 177),
        }
    } else {
        Palette {
            background: Color32::from_rgb(243, 246, 249),
            surface: Color32::WHITE,
            border: Color32::from_rgb(210, 219, 226),
            accent: Color32::from_rgb(18, 112, 89),
            success: Color32::from_rgb(47, 117, 57),
            control_border: Color32::from_rgb(137, 154, 169),
            hover: Color32::from_rgb(230, 238, 243),
            pressed: Color32::from_rgb(216, 227, 235),
            muted: Color32::from_rgb(88, 107, 123),
            selected: Color32::from_rgb(219, 239, 230),
            error: Color32::from_rgb(180, 44, 53),
            warning: Color32::from_rgb(143, 90, 7),
            foreground: Color32::from_rgb(30, 43, 55),
            input: Color32::from_rgb(248, 250, 252),
            primary_text: Color32::WHITE,
            primary_hover: Color32::from_rgb(22, 130, 103),
            primary_pressed: Color32::from_rgb(13, 91, 72),
        }
    }
}

pub fn preference(appearance: crate::config::Appearance) -> egui::ThemePreference {
    match appearance {
        crate::config::Appearance::System => egui::ThemePreference::System,
        crate::config::Appearance::Light => egui::ThemePreference::Light,
        crate::config::Appearance::Dark => egui::ThemePreference::Dark,
    }
}

pub fn setup(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    super::icons::add_to_fonts(&mut fonts);
    ctx.set_fonts(fonts);
    install(ctx, crate::config::Appearance::Dark);
}

pub fn install(ctx: &egui::Context, appearance: crate::config::Appearance) {
    for mode in [egui::Theme::Light, egui::Theme::Dark] {
        ctx.set_style_of(
            mode,
            styled((*ctx.style_of(mode)).clone(), mode == egui::Theme::Dark),
        );
    }
    ctx.set_theme(preference(appearance));
}

fn styled(mut style: egui::Style, dark: bool) -> egui::Style {
    let p = palette(dark);
    // Native widgets and custom surfaces share the selected palette.
    style.visuals = if dark {
        egui::Visuals::dark()
    } else {
        egui::Visuals::light()
    };
    style.visuals.panel_fill = p.background;
    style.visuals.window_fill = p.surface;
    style.visuals.faint_bg_color = p.surface;
    style.visuals.extreme_bg_color = p.input;
    style.visuals.override_text_color = Some(p.foreground);
    style.visuals.selection.bg_fill = p.selected;
    style.visuals.selection.stroke = Stroke::new(1.0, p.accent);
    style.visuals.hyperlink_color = p.accent;
    style.visuals.interact_cursor = Some(egui::CursorIcon::PointingHand);
    style.visuals.error_fg_color = p.error;
    style.visuals.warn_fg_color = p.warning;
    // Panes have independent clipping. Keep every interaction state within the
    // widget's allocated bounds instead of egui's default 1px hover/press growth.
    for widget in [
        &mut style.visuals.widgets.noninteractive,
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.hovered,
        &mut style.visuals.widgets.active,
        &mut style.visuals.widgets.open,
    ] {
        widget.expansion = 0.0;
    }
    for widget in [
        &mut style.visuals.widgets.inactive,
        &mut style.visuals.widgets.noninteractive,
    ] {
        widget.bg_fill = p.surface;
        widget.weak_bg_fill = p.surface;
        widget.bg_stroke = Stroke::new(1.0, p.control_border);
        widget.corner_radius = egui::CornerRadius::same(4);
    }
    style.visuals.widgets.hovered.bg_fill = p.hover;
    style.visuals.widgets.hovered.weak_bg_fill = p.hover;
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, p.accent);
    style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(4);
    style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(4);
    style.spacing.item_spacing = egui::vec2(8.0, 5.0);
    style.spacing.button_padding = egui::vec2(9.0, 4.0);
    style.spacing.interact_size.y = 26.0;
    style.spacing.scroll.bar_width = 6.0;
    style.spacing.scroll.floating = false;
    style
        .text_styles
        .insert(egui::TextStyle::Body, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Button, egui::FontId::proportional(14.0));
    style
        .text_styles
        .insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
    style
        .text_styles
        .insert(egui::TextStyle::Heading, egui::FontId::proportional(23.0));
    style
}

pub fn primary_action(ui: &mut egui::Ui, enabled: bool, text: &str) -> egui::Response {
    ui.scope(|ui| {
        ui.visuals_mut().widgets.inactive.weak_bg_fill = if enabled {
            palette(ui.visuals().dark_mode).accent
        } else {
            palette(ui.visuals().dark_mode).selected
        };
        ui.visuals_mut().widgets.noninteractive.weak_bg_fill =
            palette(ui.visuals().dark_mode).selected;
        ui.visuals_mut().widgets.hovered.weak_bg_fill =
            palette(ui.visuals().dark_mode).primary_hover;
        ui.visuals_mut().widgets.active.weak_bg_fill =
            palette(ui.visuals().dark_mode).primary_pressed;
        ui.add_enabled_ui(enabled, |ui| {
            ui.add_sized(
                [ui.available_width(), 36.0],
                egui::Button::new(RichText::new(text).color(if enabled {
                    palette(ui.visuals().dark_mode).primary_text
                } else {
                    palette(ui.visuals().dark_mode).muted
                }))
                .stroke(Stroke::NONE)
                .corner_radius(5.0),
            )
        })
        .inner
    })
    .inner
}

pub fn secondary_action(ui: &mut egui::Ui, text: &str) -> egui::Response {
    ui.add_sized([ui.available_width(), 30.0], egui::Button::new(text))
}

/// A quiet native button with a visible hover fill, border, and brighter text.
pub fn text_button(
    ui: &mut egui::Ui,
    text: &str,
    size: egui::Vec2,
    selected: bool,
) -> egui::Response {
    let p = palette(ui.visuals().dark_mode);
    ui.scope(|ui| {
        let visuals = ui.visuals_mut();
        visuals.override_text_color = None;
        visuals.widgets.inactive.weak_bg_fill = if selected {
            p.selected
        } else {
            Color32::TRANSPARENT
        };
        visuals.widgets.inactive.bg_stroke = Stroke::NONE;
        visuals.widgets.inactive.fg_stroke.color = if selected { p.accent } else { p.muted };
        visuals.widgets.hovered.bg_stroke = Stroke::NONE;
        visuals.widgets.active.bg_stroke = Stroke::NONE;
        visuals.widgets.active.weak_bg_fill = p.pressed;
        visuals.widgets.hovered.fg_stroke.color = p.accent;
        visuals.widgets.active.fg_stroke.color = p.accent;
        ui.add_sized(size, egui::Button::new(text))
    })
    .inner
}

pub fn task_status(ui: &mut egui::Ui, lang: Lang, status: Option<&TaskStatus>, idle: &str) {
    match status {
        Some(TaskStatus::Running(message)) => {
            ui.add(egui::Label::new(RichText::new(message.text(lang)).size(16.0).strong()).wrap())
                .on_hover_text(message.text(lang));
        },
        Some(TaskStatus::Completed(message))
        | Some(TaskStatus::Stopped(message))
        | Some(TaskStatus::AwaitingInput(message)) => {
            let color = if matches!(
                status,
                Some(TaskStatus::Stopped(_) | TaskStatus::AwaitingInput(_))
            ) {
                ui.visuals().warn_fg_color
            } else {
                palette(ui.visuals().dark_mode).success
            };
            ui.add(
                egui::Label::new(
                    RichText::new(message.text(lang))
                        .size(16.0)
                        .strong()
                        .color(color),
                )
                .wrap(),
            );
        },
        Some(TaskStatus::Exported { path, partial, .. }) => {
            let title = if *partial {
                lang.t("部分结果已导出", "Partial results saved")
            } else {
                lang.t("数据已导出", "Export saved")
            };
            ui.add(
                egui::Label::new(RichText::new(title).size(16.0).strong().color(if *partial {
                    ui.visuals().warn_fg_color
                } else {
                    palette(ui.visuals().dark_mode).success
                }))
                .wrap(),
            );
            super::widgets::export_location(ui, path);
        },
        Some(TaskStatus::Failed(error)) => {
            super::widgets::error_card(ui, lang, error);
        },
        _ => {
            ui.add(egui::Label::new(RichText::new(idle).size(16.0).strong()).wrap())
                .on_hover_text(idle);
        },
    }
    ui.add_space(6.0);
}

pub fn blocked_status(ui: &mut egui::Ui, lang: Lang, reason: &str) {
    task_status(
        ui,
        lang,
        Some(&TaskStatus::AwaitingInput(super::state::UiText::new(
            reason, reason,
        ))),
        "",
    );
}

pub fn restart_required(ui: &mut egui::Ui, lang: Lang, error: Option<&super::state::UiError>) {
    super::widgets::status_content(ui, "restart-status", 0, |ui| {
        if let Some(error) = error {
            super::widgets::error_card(ui, lang, error);
        } else {
            ui.colored_label(
                ui.visuals().error_fg_color,
                lang.t(
                    "任务发生内部错误，请重启程序。",
                    "An internal task error requires restarting the app.",
                ),
            );
        }
    });
    primary_action(ui, false, lang.t("需重启程序", "Restart required"));
}
