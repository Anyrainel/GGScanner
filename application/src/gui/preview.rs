//! Development-only native screenshots using the production UI renderer.
use super::{
    state::{AppState, Lang, TaskStatus, UiText, UpdateState},
    GuiApp,
};
use crate::config::{ApplicationConfigStore, Game, ToolTab};
use eframe::egui;
use std::path::PathBuf;

pub fn run(
    output: PathBuf,
    game: Game,
    tab: ToolTab,
    lang: Lang,
    size: [f32; 2],
    completed: bool,
    click: Option<[f32; 2]>,
    scenario: String,
    hover: Option<[f32; 2]>,
    pressed: bool,
    appearance: String,
) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("GGScanner UI preview")
            .with_inner_size(size)
            .with_decorations(false),
        ..Default::default()
    };
    eframe::run_native(
        "GGScanner UI preview",
        options,
        Box::new(move |cc| {
            super::setup_fonts(&cc.egui_ctx);
            let mut state = AppState::new();
            state.lang = lang;
            state.user_config.traveler_name = "旅行者".into();
            state.names_need_attention = false;
            *state.update_state.lock().unwrap() = UpdateState::None;
            if scenario == "update" {
                *state.update_state.lock().unwrap() = UpdateState::Available {
                    latest_version: "v99.0.0".into(),
                    download_url: "https://example.invalid/preview-only".into(),
                };
            }
            let (mut config, _) = ApplicationConfigStore::for_executable_dir(
                &std::env::temp_dir().join("ggscanner-ui-preview"),
            );
            config.config.navigation.active_game = game;
            config.config.appearance = match appearance.as_str() {
                "light" => crate::config::Appearance::Light,
                "dark" => crate::config::Appearance::Dark,
                "system" => crate::config::Appearance::System,
                _ => panic!("unsupported appearance"),
            };
            config.config.navigation.select_tab(tab);
            let mut app = GuiApp::new(
                state,
                config,
                &cc.egui_ctx,
                false,
                #[cfg(target_os = "linux")]
                None,
            );
            app.save_settings = false;
            app.data_refresh.preview_age(5 * 3600);
            if completed {
                app.app_config.config.star_rail.trailblazer_name =
                    lang.t("开拓者", "Trailblazer").into();
                app.app_config.config.star_rail.trailblazer_gender =
                    Some(hsr_scanner::TrailblazerGender::Stelle);
                for (timestamp, zh, en) in [
                    ("14:32:08", "已找到游戏窗口", "Game window found"),
                    ("14:32:10", "游戏数据已加载", "Game data loaded"),
                    (
                        "14:33:26",
                        "所选类别扫描结束",
                        "Selected categories processed",
                    ),
                ] {
                    app.state.scanner_log_lines.push(super::state::LogEntry {
                        level: log::Level::Info,
                        message: lang.t(zh, en).into(),
                        timestamp: timestamp.into(),
                        source: super::state::LogSource::Scanner,
                    });
                }
                use super::task_progress::{Step, StepState, TaskProgress};
                let categories = match game {
                    Game::Genshin => vec![
                        ("characters", "角色", "Characters", 86),
                        ("weapons", "武器", "Weapons", 247),
                        ("artifacts", "圣遗物", "Artifacts", 1342),
                    ],
                    Game::StarRail => vec![
                        ("characters", "角色", "Characters", 58),
                        ("light_cones", "光锥", "Light Cones", 198),
                        ("gear", "遗器与位面饰品", "Relics & Ornaments", 1126),
                    ],
                };
                let steps = categories
                    .into_iter()
                    .map(|(key, zh, en, count)| {
                        let mut step = Step::new(key, zh, en);
                        step.completed = count;
                        step.state = StepState::Complete;
                        if key != "characters" {
                            step.total = Some(count);
                        }
                        step
                    })
                    .collect();
                let (status, progress) = match game {
                    Game::Genshin => (&app.state.scan_status, &app.state.scan_progress),
                    Game::StarRail => (&app.star_rail.scan_status, &app.star_rail.scan_progress),
                };
                *status.lock().unwrap() = TaskStatus::Exported {
                    message: UiText::new("扫描已完成", "Scan completed"),
                    path: "C:\\Users\\Player\\Exports\\scan_20261008_143326.json".into(),
                    partial: false,
                };
                *progress.lock().unwrap() = TaskProgress { steps };
                if scenario == "partial" {
                    if let Some(step) = progress.lock().unwrap().steps.last_mut() {
                        step.state = StepState::Interrupted;
                        step.total = Some(step.completed + 300);
                    }
                }
            }
            let status = match game {
                Game::Genshin => &app.state.scan_status,
                Game::StarRail => &app.star_rail.scan_status,
            };
            match scenario.as_str() {
                "error" => *status.lock().unwrap() = TaskStatus::Failed(preview_error()),
                "partial" => {
                    *status.lock().unwrap() = TaskStatus::Exported {
                        message: UiText::new("扫描已停止", "Scan stopped"),
                        path: "C:\\Users\\Player\\Exports\\partial_scan.json".into(),
                        partial: true,
                    }
                },
                "stopped" => {
                    *status.lock().unwrap() = TaskStatus::Stopped(UiText::new(
                        "扫描已停止 · 未导出",
                        "Scan stopped · no export",
                    ))
                },
                "exporting" => {
                    *status.lock().unwrap() =
                        TaskStatus::Running(UiText::new("正在导出数据", "Saving export"))
                },
                "ready" | "login" | "update" | "done" => {},
                _ => panic!("unsupported preview scenario"),
            }
            #[cfg(feature = "capture")]
            if tab == ToolTab::Capture
                && game == Game::Genshin
                && !matches!(scenario.as_str(), "ready" | "update")
            {
                app.capture_tab.preview_feedback(&scenario);
            }
            #[cfg(all(feature = "capture", target_os = "windows"))]
            if tab == ToolTab::Capture
                && game == Game::StarRail
                && !matches!(scenario.as_str(), "ready" | "update")
            {
                app.star_rail.capture.preview_feedback(&scenario);
            }
            if scenario == "error" {
                app.state.scanner_log_lines.push(super::state::LogEntry {
                    level: log::Level::Error,
                    message: lang
                        .t(
                            "导出失败：拒绝访问输出目录",
                            "Export failed: access denied to the output folder",
                        )
                        .into(),
                    timestamp: "14:33:27".into(),
                    source: super::state::LogSource::Scanner,
                });
            }
            Ok(Box::new(Preview {
                app,
                output,
                frames: 0,
                click,
                hover,
                pressed,
            }))
        }),
    )
}

pub(super) fn preview_error() -> super::state::UiError {
    super::state::UiError::from_message(
        UiText::new(
            "导出文件无法写入。请检查输出目录和文件权限。",
            "Cannot save the export. Check the output folder and file permissions.",
        ),
        "write C:\\Users\\Player\\Exports\\scan.json\nCaused by: Access is denied. (os error 5)",
    )
}

struct Preview {
    app: GuiApp,
    output: PathBuf,
    frames: usize,
    click: Option<[f32; 2]>,
    hover: Option<[f32; 2]>,
    pressed: bool,
}
impl eframe::App for Preview {
    fn raw_input_hook(&mut self, _ctx: &egui::Context, input: &mut egui::RawInput) {
        if let Some(point) = self.hover {
            input.events.push(egui::Event::PointerMoved(point.into()));
            if self.pressed && self.frames == 1 {
                input.events.push(egui::Event::PointerButton {
                    pos: point.into(),
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::NONE,
                });
            }
        }
        if let Some(point) = self.click.filter(|_| self.frames == 1 || self.frames == 2) {
            input.events.push(egui::Event::PointerMoved(point.into()));
            input.events.push(egui::Event::PointerButton {
                pos: point.into(),
                button: egui::PointerButton::Primary,
                pressed: self.frames == 1,
                modifiers: egui::Modifiers::NONE,
            });
        }
    }
    fn logic(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        self.app.logic(ctx, frame);
    }
    fn ui(&mut self, root_ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = root_ui.ctx().clone();
        self.app.ui(root_ui, frame);
        self.frames += 1;
        let screenshot = ctx.input(|i| {
            i.events.iter().find_map(|event| match event {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(screenshot) = screenshot {
            let pixels: Vec<u8> = screenshot
                .pixels
                .iter()
                .flat_map(|p| p.to_array())
                .collect();
            image::save_buffer(
                &self.output,
                &pixels,
                screenshot.width() as u32,
                screenshot.height() as u32,
                image::ColorType::Rgba8,
            )
            .expect("save native UI screenshot");
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        } else if self.frames == if self.click.is_some() { 30 } else { 5 } {
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(Default::default()));
        }
        ctx.request_repaint();
    }
}
