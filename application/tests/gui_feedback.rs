mod common;
use common::HeadlessContext;
use eframe::egui::{self, epaint::Shape, Color32};
use good_tools_app::gui::{
    log_panel, scanner_tab, star_rail_scanner_tab,
    star_rail_state::StarRailState,
    state::{AppState, Lang, LogEntry, LogSource, LogStore, TaskStatus, UiError, UiText},
    theme,
};
use std::sync::Arc;

fn text_shapes(shape: &Shape, out: &mut Vec<(String, f32, Color32)>) {
    match shape {
        Shape::Text(text) => {
            for section in &text.galley.job.sections {
                out.push((
                    text.galley.job.text[section.byte_range.start.0..section.byte_range.end.0]
                        .into(),
                    text.pos.y,
                    section.format.color,
                ));
            }
        },
        Shape::Vec(shapes) => shapes.iter().for_each(|shape| text_shapes(shape, out)),
        _ => {},
    }
}

fn render(mut draw: impl FnMut(&mut egui::Ui)) -> Vec<(String, f32, Color32)> {
    let ctx = egui::Context::default();
    theme::setup(&ctx);
    let mut texts = Vec::new();
    for _ in 0..2 {
        let frame = ctx.run_headless(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(370.0, 700.0),
                )),
                ..Default::default()
            },
            |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| draw(ui));
            },
        );
        texts.clear();
        for clipped in &frame.shapes {
            text_shapes(&clipped.shape, &mut texts);
        }
    }
    texts
}

#[test]
fn failure_is_prominent_before_retry_without_expanding_technical_clutter() {
    let status = TaskStatus::Failed(UiError::from_message(
        UiText::new("输出目录无法写入。", "The output folder is not writable."),
        "RAW_DIAGNOSTIC_MARKER: os error 5",
    ));
    let texts = render(|ui| {
        theme::task_status(ui, Lang::En, Some(&status), "Ready");
        theme::primary_action(ui, true, "Retry");
    });
    let hint = texts
        .iter()
        .find(|(text, _, _)| text == "The output folder is not writable.")
        .unwrap();
    let retry = texts.iter().find(|(text, _, _)| text == "Retry").unwrap();
    assert!(hint.1 < retry.1);
    assert_eq!(hint.2, theme::ERROR);
    assert!(texts.iter().any(|(text, _, _)| text == "Copy full error"));
    assert!(!texts
        .iter()
        .any(|(text, _, _)| text.contains("RAW_DIAGNOSTIC_MARKER")));
}

#[test]
fn export_folder_action_is_visible_only_after_a_successful_write() {
    let path = "C:/exports/scan.json";
    for partial in [false, true] {
        let status = TaskStatus::Exported {
            message: UiText::new("完成", "Done"),
            path: path.into(),
            partial,
        };
        let texts = render(|ui| {
            theme::task_status(ui, Lang::En, Some(&status), "Ready");
            good_tools_app::gui::widgets::export_action(ui, Lang::En, Some(&status));
        });
        let title = if partial {
            "Partial results saved"
        } else {
            "Export saved"
        };
        let title = texts.iter().find(|(text, _, _)| text == title).unwrap();
        assert_eq!(
            title.2,
            if partial {
                theme::WARNING
            } else {
                theme::SUCCESS
            }
        );
        assert!(texts.iter().any(|(text, _, _)| text == "scan.json"));
        assert!(!texts.iter().any(|(text, _, _)| text == path));
        assert!(texts.iter().any(|(text, _, _)| text == "Open folder"));
    }
    let status = TaskStatus::Running(UiText::new("正在导出", "Saving export"));
    let texts = render(|ui| theme::task_status(ui, Lang::En, Some(&status), "Ready"));
    assert!(!texts
        .iter()
        .any(|(text, _, _)| text == "Open folder" || text == "Export saved"));
}

#[test]
fn missing_scan_targets_explain_why_start_is_disabled() {
    let mut state = AppState::new();
    state.lang = Lang::En;
    state.scan_characters = false;
    state.scan_weapons = false;
    state.scan_artifacts = false;
    state.scan_achievements = false;
    let mut handle = None;
    let texts = render(|ui| scanner_tab::show_status(ui, &mut state, &mut handle, false, false));
    assert!(texts
        .iter()
        .any(|(text, _, _)| text == "Select scan targets"));
    assert!(!texts.iter().any(|(text, _, _)| text == "Ready to scan"));
}

#[test]
fn inventory_only_scans_require_names_for_equipped_owners() {
    let mut genshin = AppState::new();
    genshin.scan_characters = false;
    genshin.scan_artifacts = false;
    genshin.scan_weapons = true;
    genshin.user_config.traveler_name.clear();
    assert!(genshin.missing_required_character_names());
    genshin.user_config.traveler_name = "Player".into();
    assert!(!genshin.missing_required_character_names());
    let mut hsr = good_tools_app::config::StarRailSettings::default();
    hsr.scan_characters = false;
    hsr.scan_relics_and_ornaments = false;
    hsr.scan_light_cones = true;
    hsr.trailblazer_name.clear();
    assert!(hsr.missing_trailblazer());
}

#[test]
fn genshin_name_settings_remain_visible_and_preserved_without_character_scanning() {
    let mut state = AppState::new();
    state.lang = Lang::En;
    state.user_config.traveler_name = "Player".into();
    state.user_config.wanderer_name = "Wanderer nickname".into();
    state.user_config.manekin_name = "Manekin nickname".into();
    state.user_config.manekina_name = "Manekina nickname".into();
    for selected in [true, false] {
        state.scan_characters = selected;
        let before = (
            state.user_config.traveler_name.clone(),
            state.user_config.wanderer_name.clone(),
            state.user_config.manekin_name.clone(),
            state.user_config.manekina_name.clone(),
        );
        let texts = render(|ui| {
            ui.ctx().memory_mut(|m| m.set_everything_is_visible(true));
            scanner_tab::show_settings(ui, &mut state, false);
        });
        for label in [
            "In-game nickname*",
            "Other renamed characters",
            "Wanderer",
            "Manekin",
            "Manekina",
        ] {
            assert!(
                texts.iter().any(|(text, _, _)| text == label),
                "missing {label}"
            );
        }
        assert_eq!(
            (
                state.user_config.traveler_name.clone(),
                state.user_config.wanderer_name.clone(),
                state.user_config.manekin_name.clone(),
                state.user_config.manekina_name.clone()
            ),
            before
        );
    }
}

#[test]
fn exported_scans_place_folder_before_scan_again_in_both_games() {
    for hsr in [false, true] {
        let status = TaskStatus::Exported {
            message: UiText::new("完成", "Done"),
            path: "C:/exports/scan.json".into(),
            partial: false,
        };
        let mut genshin = AppState::new();
        genshin.lang = Lang::En;
        genshin.user_config.traveler_name = "Player".into();
        *genshin.scan_status.lock().unwrap() = status.clone();
        let mut settings = good_tools_app::config::StarRailSettings::default();
        settings.scan_characters = false;
        let mut state = StarRailState::new(String::new());
        *state.scan_status.lock().unwrap() = status;
        let mut handle = None;
        let texts = render(|ui| {
            if hsr {
                star_rail_scanner_tab::show_status(
                    ui,
                    Lang::En,
                    &mut settings,
                    &mut state,
                    false,
                    false,
                );
            } else {
                scanner_tab::show_status(ui, &mut genshin, &mut handle, false, false);
            }
        });
        let y = |caption: &str| texts.iter().find(|(text, _, _)| text == caption).unwrap().1;
        assert!(y("scan.json") < y("Open folder"));
        assert!(y("Open folder") < y("Scan again"));
        assert!(!texts.iter().any(|(text, _, _)| text == "Start scan"));
    }
}

#[test]
fn export_result_shows_only_filename_for_windows_and_slash_paths() {
    for path in [
        "C:\\Users\\Player\\Exports\\scan.json",
        "D:/exports/scan.json",
    ] {
        let texts = render(|ui| good_tools_app::gui::widgets::export_location(ui, path));
        assert!(texts.iter().any(|(text, _, _)| text == "scan.json"));
        assert!(!texts.iter().any(|(text, _, _)| text == path));
    }
}

#[test]
fn fixing_required_input_clears_the_old_blocking_status() {
    let mut state = AppState::new();
    state.lang = Lang::En;
    state.user_config.traveler_name = "Traveler".into();
    *state.scan_status.lock().unwrap() = TaskStatus::AwaitingInput(UiText::new(
        "请填写游戏内昵称",
        "Enter your in-game nickname",
    ));
    let mut handle = None;
    let texts = render(|ui| scanner_tab::show_status(ui, &mut state, &mut handle, false, false));
    assert!(texts.iter().any(|(text, _, _)| text == "Ready to scan"));
    assert!(!texts
        .iter()
        .any(|(text, _, _)| text == "Enter your in-game nickname"));
}

#[test]
fn cache_age_uses_compact_bilingual_units_without_wrapping_future_times() {
    use good_tools_app::gui::data_refresh::age_text;
    let now = 1_000_000;
    assert_eq!(age_text(Lang::Zh, Some(now - 5 * 3600), now), "5小时前");
    assert_eq!(age_text(Lang::En, Some(now - 5 * 3600), now), "5h ago");
    assert_eq!(age_text(Lang::Zh, Some(now - 3 * 86400), now), "3天前");
    assert_eq!(age_text(Lang::En, Some(now - 3 * 86400), now), "3d ago");
    assert_eq!(age_text(Lang::En, Some(now - 59), now), "Just now");
    assert_eq!(age_text(Lang::En, Some(now - 60), now), "1m ago");
    assert_eq!(age_text(Lang::En, Some(now + 60), now), "");
    assert_eq!(age_text(Lang::En, None, now), "");
}

#[test]
fn required_star_rail_inputs_replace_ready_until_they_are_supplied() {
    let mut settings = good_tools_app::config::StarRailSettings::default();
    let mut state = StarRailState::new(String::new());
    settings.scan_characters = true;
    let texts = render(|ui| {
        star_rail_scanner_tab::show_status(ui, Lang::En, &mut settings, &mut state, false, false)
    });
    assert!(!texts.iter().any(|(text, _, _)| text == "Ready to scan"));
    settings.trailblazer_name = "Trailblazer".into();
    settings.trailblazer_gender = Some(hsr_scanner::TrailblazerGender::Stelle);
    let texts = render(|ui| {
        star_rail_scanner_tab::show_status(ui, Lang::En, &mut settings, &mut state, false, false)
    });
    assert!(texts.iter().any(|(text, _, _)| text == "Ready to scan"));
}

#[test]
fn star_rail_exports_need_no_folder_selection_and_preserve_saved_destinations() {
    let mut settings = good_tools_app::config::StarRailSettings::default();
    assert_eq!(settings.export_directory(), genshin_scanner::cli::exe_dir());
    settings.output_dir = "D:/custom-exports".into();
    assert_eq!(
        settings.export_directory(),
        std::path::PathBuf::from("D:/custom-exports")
    );
    let texts =
        render(|ui| star_rail_scanner_tab::show_settings(ui, Lang::En, &mut settings, false));
    assert!(!texts.iter().any(|(text, _, _)| text == "Output folder"));
}

#[cfg(feature = "capture")]
#[test]
fn capture_forms_have_no_manual_or_folder_picker_and_start_with_actionable_guidance() {
    use good_tools_app::gui::capture_tab::{self, CaptureTabState};

    let mut genshin = CaptureTabState::new(String::new());
    let settings_text = render(|ui| capture_tab::show_settings(ui, Lang::En, &mut genshin, false));
    assert!(!settings_text
        .iter()
        .any(|(text, _, _)| text == "How to use" || text == "Output folder"));
    let status_text =
        render(|ui| capture_tab::show_status(ui, Lang::En, &mut genshin, false, false));
    assert!(status_text
        .iter()
        .any(|(text, _, _)| text == "Close the game first"));
    assert!(!status_text
        .iter()
        .any(|(text, _, _)| text.contains("Import the file")));
    assert!(!status_text.iter().any(|(text, _, _)| text == "Characters"
        || text == "Login handshake"
        || text == "Task status"));
}

#[cfg(all(feature = "capture", target_os = "windows"))]
#[test]
fn star_rail_capture_forms_start_with_actionable_guidance() {
    use good_tools_app::gui::star_rail_capture_tab::{self, StarRailCaptureState};

    let mut settings = good_tools_app::config::StarRailSettings::default();
    let mut hsr = StarRailCaptureState::new(String::new());
    let settings_text = render(|ui| {
        star_rail_capture_tab::show_settings(ui, Lang::En, &mut settings, &mut hsr, false)
    });
    assert!(!settings_text
        .iter()
        .any(|(text, _, _)| text == "How to use" || text == "Output folder"));
    let status_text = render(|ui| {
        star_rail_capture_tab::show_status(ui, Lang::En, &mut settings, &mut hsr, false, false)
    });
    assert!(status_text
        .iter()
        .any(|(text, _, _)| text == "Close the game first"));
}

#[test]
fn sidebar_offers_capture_only_where_the_game_has_a_backend() {
    use good_tools_app::config::{Appearance, Game, GameNavigation, ToolTab};
    use good_tools_app::gui::shell;

    fn capture_visible(game: Game) -> bool {
        let mut navigation = GameNavigation {
            active_game: game,
            selected_tab: ToolTab::Scanner,
        };
        let mut appearance = Appearance::System;
        let mut lang = Lang::En;
        let texts = render(|ui| shell::sidebar(ui, &mut lang, &mut navigation, &mut appearance));
        texts.iter().any(|(text, _, _)| text == "Capture")
    }

    assert_eq!(
        capture_visible(Game::Genshin),
        cfg!(all(
            feature = "capture",
            any(target_os = "windows", target_os = "linux")
        )),
        "Genshin capture exists on Windows and Linux builds that compiled a backend"
    );
    assert_eq!(
        capture_visible(Game::StarRail),
        cfg!(all(feature = "capture", target_os = "windows")),
        "Star Rail capture keeps its Windows-only backend"
    );
}

#[test]
fn compact_logs_preserve_warning_and_error_emphasis() {
    let store = Arc::new(LogStore::new(100));
    for (level, message) in [
        (log::Level::Warn, "WARN_MARKER"),
        (log::Level::Error, "ERROR_MARKER"),
    ] {
        store.push(LogEntry {
            level,
            message: message.into(),
            timestamp: "12:00".into(),
            source: LogSource::Scanner,
        });
    }
    let texts = render(|ui| log_panel::show_with(ui, Lang::En, &store));
    assert!(texts
        .iter()
        .any(|(text, _, color)| text.contains("WARN_MARKER") && *color == theme::WARNING));
    assert!(texts
        .iter()
        .any(|(text, _, color)| text.contains("ERROR_MARKER") && *color == theme::ERROR));
}

#[cfg(feature = "capture")]
#[test]
fn failed_capture_write_displays_the_cause_and_never_shows_an_export_path() {
    use good_tools_app::gui::capture_tab::{self, CaptureTabState};
    let root = std::env::temp_dir().join(format!(
        "ggscanner-feedback-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&root).unwrap();
    // Missing folders are created automatically. A file occupying the output
    // directory reliably exercises a real write failure on every platform.
    let blocked = root.join("blocked-output");
    std::fs::write(&blocked, "not a directory").unwrap();
    let mut capture = CaptureTabState::new(blocked.display().to_string());
    capture.only_keep_latest_dump = false;
    let export = genshin_scanner::scanner::common::models::GoodExport::new(
        Some(vec![]),
        Some(vec![]),
        Some(vec![]),
    );
    capture.inject_export_for_test(export).send(()).unwrap();
    for _ in 0..200 {
        capture.tick();
        if !capture.is_busy() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    assert!(!capture.is_busy());
    let texts = render(|ui| capture_tab::show_status(ui, Lang::En, &mut capture, false, false));
    assert!(texts
        .iter()
        .any(|(text, _, _)| text.contains("export could not be saved")));
    assert!(texts.iter().any(|(text, _, _)| text == "Retry"));
    assert!(!texts
        .iter()
        .any(|(text, _, _)| text == "Open folder" || text == "Export saved"));
    std::fs::remove_file(blocked).unwrap();
    std::fs::remove_dir(root).unwrap();
}
