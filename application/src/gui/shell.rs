use super::{
    game_switcher,
    icons::{self, Icon},
    state::Lang,
    theme,
};
use crate::config::{Appearance, Game, GameNavigation, ToolTab};
use eframe::egui;

pub const TITLEBAR_HEIGHT: f32 = 48.0;
pub const SIDEBAR_MIN_WIDTH: f32 = 128.0;

/// Product name follows the built capabilities, not the platform: a capture
/// build is GGScanner wherever its native backend runs.
pub fn product_name() -> &'static str {
    if cfg!(feature = "capture") {
        "GGScanner"
    } else {
        "GGScannerOCR"
    }
}
pub fn site(game: Game) -> &'static str {
    match game {
        Game::Genshin => "https://ggartifact.com",
        Game::StarRail => "https://hsr.ggartifact.com",
    }
}

pub struct Logos {
    pub app: egui::TextureHandle,
    pub genshin: egui::TextureHandle,
    pub star_rail: egui::TextureHandle,
}
impl Logos {
    pub fn load(ctx: &egui::Context) -> Self {
        fn load(ctx: &egui::Context, name: &str, bytes: &[u8]) -> egui::TextureHandle {
            let rgba = image::load_from_memory(bytes)
                .expect("bundled logo must be valid PNG")
                .into_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            ctx.load_texture(
                name,
                egui::ColorImage::from_rgba_unmultiplied(size, &rgba),
                egui::TextureOptions::LINEAR,
            )
        }
        Self {
            // Generated from the same source as the executable's icon.ico.
            app: load(
                ctx,
                "app-icon",
                include_bytes!("../../../assets/icon_64.png"),
            ),
            genshin: load(
                ctx,
                "genshin-site-logo",
                include_bytes!("../../../assets/genshin-logo.png"),
            ),
            star_rail: load(
                ctx,
                "star-rail-site-logo",
                include_bytes!("../../../assets/star-rail-logo.png"),
            ),
        }
    }
}

pub fn titlebar(
    root_ui: &mut egui::Ui,
    lang: &mut Lang,
    game: &mut Game,
    enabled: bool,
    logos: &Logos,
) {
    let ctx = root_ui.ctx().clone();
    egui::Panel::top("titlebar")
        .exact_size(TITLEBAR_HEIGHT)
        .frame(
            egui::Frame::new()
                .fill(theme::palette(ctx.theme() == egui::Theme::Dark).surface)
                .inner_margin(egui::Margin::ZERO),
        )
        .show(root_ui, |ui| {
            let drag = ui.interact(
                ui.max_rect(),
                ui.id().with("window-drag"),
                egui::Sense::click_and_drag(),
            );
            let maximized = ctx.input(|i| i.viewport().maximized.unwrap_or(false));
            if drag.double_clicked() {
                ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(!maximized));
            } else if drag.drag_started() {
                ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
            }
            let rect = ui.max_rect();
            let controls = egui::Rect::from_min_size(
                egui::pos2(rect.right() - 138.0, rect.top()),
                egui::vec2(138.0, 40.0),
            );
            let brand = egui::Rect::from_min_max(
                egui::pos2(rect.left() + 12.0, rect.top() + 10.0),
                egui::pos2(controls.left() - 8.0, rect.top() + 44.0),
            );
            let mut brand_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(brand)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            brand_ui.horizontal_centered(|ui| {
                ui.add(egui::Image::new((logos.app.id(), egui::vec2(28.0, 28.0))));
                ui.label(egui::RichText::new(product_name()).size(16.0).strong());
                ui.add_space(40.0);
                ui.allocate_ui_with_layout(
                    egui::vec2(248.0, 34.0),
                    egui::Layout::left_to_right(egui::Align::Center),
                    |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        game_switcher::show(ui, *lang, game, enabled, Some(logos));
                    },
                );
                ui.add_space(36.0);
                let label = lang.t("打开 GGArtifact", "Open GGArtifact");
                let url = site(*game);
                let width = ui.fonts_mut(|fonts| {
                    fonts
                        .layout_no_wrap(
                            label.into(),
                            egui::FontId::proportional(14.0),
                            theme::palette(ui.visuals().dark_mode).foreground,
                        )
                        .size()
                        .x
                }) + 48.0;
                if icons::button(
                    ui,
                    label,
                    Icon::External,
                    egui::vec2(width, 34.0),
                    false,
                    false,
                )
                .on_hover_text(url)
                .clicked()
                {
                    ctx.open_url(egui::OpenUrl::new_tab(url));
                }
            });
            let mut controls_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(controls)
                    .layout(egui::Layout::left_to_right(egui::Align::Center)),
            );
            controls_ui.spacing_mut().item_spacing.x = 0.0;
            for (index, label, command) in [
                (
                    0,
                    lang.t("最小化", "Minimize"),
                    egui::ViewportCommand::Minimized(true),
                ),
                (
                    1,
                    lang.t("最大化 / 还原", "Maximize / Restore"),
                    egui::ViewportCommand::Maximized(!maximized),
                ),
                (2, lang.t("关闭", "Close"), egui::ViewportCommand::Close),
            ] {
                if caption_button(&mut controls_ui, label, index, maximized).clicked() {
                    ctx.send_viewport_cmd(command);
                }
            }
        });
}

/// Standard window controls use the same bundled icon font as the navigation.
fn caption_button(ui: &mut egui::Ui, label: &str, index: u8, maximized: bool) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(egui::vec2(46.0, 40.0), egui::Sense::click());
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, label));
    let hover = response.hovered() || response.is_pointer_button_down_on();
    if hover {
        ui.painter().rect_filled(
            rect,
            0.0,
            if index == 2 {
                egui::Color32::from_rgb(196, 43, 28)
            } else {
                theme::palette(ui.visuals().dark_mode).hover
            },
        );
    }
    let stroke = egui::Stroke::new(
        1.0,
        if hover && index == 2 {
            egui::Color32::WHITE
        } else if hover {
            theme::palette(ui.visuals().dark_mode).foreground
        } else {
            theme::palette(ui.visuals().dark_mode).muted
        },
    );
    let icon = match index {
        0 => Icon::Minimize,
        1 if maximized => Icon::Restore,
        1 => Icon::Maximize,
        _ => Icon::Close,
    };
    icons::paint(
        ui,
        egui::Rect::from_center_size(rect.center(), egui::vec2(16.0, 16.0)),
        icon,
        stroke.color,
    );
    response.on_hover_text(label)
}

pub fn sidebar(
    ui: &mut egui::Ui,
    lang: &mut Lang,
    navigation: &mut GameNavigation,
    appearance: &mut Appearance,
) {
    let rect = ui.available_rect_before_wrap();
    let footer = egui::Rect::from_min_max(
        egui::pos2(rect.left(), (rect.bottom() - 124.0).max(rect.top())),
        rect.max,
    );
    let tabs = egui::Rect::from_min_max(rect.min, egui::pos2(rect.right(), footer.top() - 12.0));
    super::layout::region(ui, tabs, "tool-tabs", |ui| {
        ui.spacing_mut().scroll = egui::style::ScrollStyle::floating();
        let width = tabs.width() - 4.0;
        egui::ScrollArea::vertical()
            .id_salt("navigation-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Frame::new().inner_margin(2.0).show(ui, |ui| {
                    let width = (width - 4.0).max(1.0);
                    ui.set_width(width);
                    ui.visuals_mut().selection.bg_fill =
                        theme::palette(ui.visuals().dark_mode).selected;
                    ui.visuals_mut().selection.stroke = egui::Stroke::NONE;
                    ui.visuals_mut().widgets.hovered.bg_stroke =
                        egui::Stroke::new(1.0, theme::palette(ui.visuals().dark_mode).accent);
                    ui.visuals_mut().widgets.active.bg_stroke =
                        egui::Stroke::new(1.0, theme::palette(ui.visuals().dark_mode).accent);
                    let mut tab = navigation.active_tab();
                    let original = tab;
                    for (value, zh, en, icon) in [
                        (ToolTab::Capture, "抓包器", "Capture", Icon::Capture),
                        (ToolTab::Scanner, "扫描器", "Scanner", Icon::Scan),
                        (ToolTab::Manager, "管理器", "Manager", Icon::Manager),
                    ] {
                        // Capture is offered only where this build and the
                        // active game have a native backend (Star Rail capture
                        // is Windows-only).
                        if value.available_for(navigation.active_game) != value {
                            continue;
                        }
                        let selected = tab == value;
                        if icons::button(
                            ui,
                            lang.t(zh, en),
                            icon,
                            egui::vec2(width, 42.0),
                            selected,
                            false,
                        )
                        .clicked()
                        {
                            tab = value;
                        }
                    }
                    if tab != original {
                        navigation.select_tab(tab);
                    }
                });
            });
    });
    super::layout::region(ui, footer, "sidebar-footer", |ui| {
        egui::Frame::new().inner_margin(2.0).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 4.0;
            if icons::button(
                ui,
                lang.t("English", "中文"),
                Icon::Language,
                egui::vec2(ui.available_width(), 32.0),
                false,
                false,
            )
            .on_hover_text(lang.t("Switch to English", "切换到中文"))
            .clicked()
            {
                *lang = match lang {
                    Lang::Zh => Lang::En,
                    Lang::En => Lang::Zh,
                };
            }
            let dark = ui.visuals().dark_mode;
            let (icon, label, hint) = if dark {
                (
                    Icon::Sun,
                    lang.t("浅色", "Light"),
                    lang.t("切换为浅色模式", "Switch to light theme"),
                )
            } else {
                (
                    Icon::Moon,
                    lang.t("深色", "Dark"),
                    lang.t("切换为深色模式", "Switch to dark theme"),
                )
            };
            if icons::button(
                ui,
                label,
                icon,
                egui::vec2(ui.available_width(), 32.0),
                false,
                false,
            )
            .on_hover_text(hint)
            .clicked()
            {
                *appearance = if dark {
                    Appearance::Light
                } else {
                    Appearance::Dark
                };
                ui.ctx().set_theme(theme::preference(*appearance));
            }
            if icons::button(
                ui,
                lang.t("关于", "About"),
                Icon::About,
                egui::vec2(ui.available_width(), 32.0),
                navigation.active_tab() == ToolTab::Credits,
                false,
            )
            .clicked()
            {
                navigation.select_tab(ToolTab::Credits);
            }
        });
    });
}

pub fn window_resize(ctx: &egui::Context) {
    if ctx.input(|i| i.viewport().maximized.unwrap_or(false)) {
        return;
    }
    let rect = ctx.content_rect();
    let Some(pointer) = ctx.input(|i| i.pointer.hover_pos()) else {
        return;
    };
    let left = pointer.x < rect.left() + 5.0;
    let right = pointer.x > rect.right() - 5.0;
    let top = pointer.y < rect.top() + 5.0;
    let bottom = pointer.y > rect.bottom() - 5.0;
    let direction = match (left, right, top, bottom) {
        (true, _, true, _) => egui::ResizeDirection::NorthWest,
        (true, _, _, true) => egui::ResizeDirection::SouthWest,
        (_, true, true, _) => egui::ResizeDirection::NorthEast,
        (_, true, _, true) => egui::ResizeDirection::SouthEast,
        (true, _, _, _) => egui::ResizeDirection::West,
        (_, true, _, _) => egui::ResizeDirection::East,
        (_, _, true, _) => egui::ResizeDirection::North,
        (_, _, _, true) => egui::ResizeDirection::South,
        _ => return,
    };
    ctx.set_cursor_icon(match direction {
        egui::ResizeDirection::North | egui::ResizeDirection::South => {
            egui::CursorIcon::ResizeVertical
        },
        egui::ResizeDirection::East | egui::ResizeDirection::West => {
            egui::CursorIcon::ResizeHorizontal
        },
        egui::ResizeDirection::NorthWest | egui::ResizeDirection::SouthEast => {
            egui::CursorIcon::ResizeNwSe
        },
        _ => egui::CursorIcon::ResizeNeSw,
    });
    if ctx.input(|i| i.pointer.primary_pressed()) {
        ctx.send_viewport_cmd(egui::ViewportCommand::BeginResize(direction));
    }
}
