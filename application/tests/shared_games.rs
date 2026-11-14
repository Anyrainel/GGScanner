use std::{
    fs,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use good_tools_app::{
    config::{
        ApplicationConfigStore, Game, GameNavigation, ToolTab, APPLICATION_CONFIG_FILE_REL,
        APPLICATION_CONFIG_SCHEMA_VERSION,
    },
    gui::{game_switcher, state::Lang},
};
use hsr_scanner::manager::{
    build_manager_plan, JournalEntry, JournalStatus, ManagedGearObservation, ManagedState,
    ManagerInstructionsEnvelope, ManagerJournal, ManagerJournalStore, ManagerPlan,
    MANAGER_JOURNAL_SCHEMA, MANAGER_JOURNAL_SCHEMA_VERSION,
};
const MANAGER_INSTRUCTIONS: &str =
    include_str!("../../experimental/hsr/tests/fixtures/manager_instructions_v1.json");

#[test]
fn debugging_options_default_off_and_saved_choices_survive_both_config_loaders() {
    let root = temp_root("debug-policy");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("good_config.json");
    for continue_on_failure in [true, false] {
        let fixture = serde_json::json!({"continue_on_failure":continue_on_failure, "filter_involved_sets":false, "update_inventory":true, "ocr_pool_v4_override":3, "traveler_name":"Player", "verbose":true});
        fs::write(&path, fixture.to_string()).unwrap();
        let config =
            genshin_scanner::cli::parse_user_config(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(config.stop_on_failure, !continue_on_failure);
        assert!(!config.filter_involved_sets);
        assert!(config.verbose);
        assert_eq!(config.traveler_name, "Player");
        genshin_scanner::cli::save_user_config_to_path(&config, &path).unwrap();
        assert_eq!(
            genshin_scanner::cli::parse_user_config(&fs::read_to_string(&path).unwrap())
                .unwrap()
                .stop_on_failure,
            config.stop_on_failure
        );
        let saved = serde_json::to_string(&config).unwrap();
        for retired in [
            "continue_on_failure",
            "update_inventory",
            "ocr_pool_v4_override",
            "ocr_pool_v5_override",
        ] {
            assert!(!saved.contains(retired));
        }
        assert_eq!(
            genshin_scanner::cli::parse_user_config(&saved)
                .unwrap()
                .stop_on_failure,
            config.stop_on_failure
        );
    }
    let defaults = genshin_scanner::cli::parse_user_config("{}").unwrap();
    assert!(!defaults.stop_on_failure);
    assert!(defaults.filter_involved_sets);
    assert!(defaults.capture_only_keep_latest_export);
    let future = r#"{"schema_version":999,"traveler_name":"future"}"#;
    fs::write(&path, future).unwrap();
    assert!(genshin_scanner::cli::parse_user_config(future).is_err());
    assert!(genshin_scanner::cli::save_user_config_to_path(&defaults, &path).is_err());
    assert_eq!(fs::read_to_string(&path).unwrap(), future);
    let hsr_path = root.join("ui_config.json");
    fs::write(&hsr_path, r#"{"schemaVersion":3,"starRail":{"scanCharacters":false,"captureOnlyKeepLatestExport":false}}"#).unwrap();
    let mut store = ApplicationConfigStore::load(&hsr_path).unwrap();
    assert!(!store.config.star_rail.stop_on_failure);
    assert!(!store.config.star_rail.verbose);
    assert!(!store.config.star_rail.dump_job_data);
    assert!(!store.config.star_rail.capture_only_keep_latest_export);
    store.config.star_rail.stop_on_failure = true;
    store.config.star_rail.verbose = true;
    store.config.star_rail.dump_job_data = true;
    store.persist_now().unwrap();
    let loaded = ApplicationConfigStore::load(&hsr_path).unwrap();
    assert_eq!(loaded.config, store.config);
    remove_test_tree(&root);
}

#[derive(Default)]
struct MemoryJournalStore {
    journal: Option<ManagerJournal>,
}

impl ManagerJournalStore for MemoryJournalStore {
    fn load(&mut self) -> Result<Option<ManagerJournal>, String> {
        Ok(self.journal.clone())
    }

    fn save(&mut self, journal: &ManagerJournal) -> Result<(), String> {
        self.journal = Some(journal.clone());
        Ok(())
    }

    fn holds_exclusive_apply_lease(&self) -> bool {
        false
    }
}

fn manager_envelope() -> ManagerInstructionsEnvelope {
    ManagerInstructionsEnvelope::parse_json(MANAGER_INSTRUCTIONS)
        .expect("manager instruction fixture should be valid")
}

fn manager_plan(envelope: &ManagerInstructionsEnvelope) -> ManagerPlan {
    build_manager_plan(
        envelope,
        &[ManagedGearObservation {
            matcher: envelope.instructions[0].matcher.clone(),
            state: ManagedState {
                lock: Some(false),
                discard: Some(false),
            },
            equipped: Some(false),
        }],
    )
    .expect("matching observation should produce a valid manager plan")
}

fn manager_journal(plan: &ManagerPlan, status: JournalStatus) -> ManagerJournal {
    let (toggle_attempts, outcome_code) = match status {
        JournalStatus::Pending => (0, None),
        JournalStatus::MutationStarted => (1, None),
        JournalStatus::Verified => (1, Some("postverified".to_owned())),
        JournalStatus::NeedsReview => (1, Some("interrupted_mutation_ambiguous".to_owned())),
    };
    ManagerJournal {
        schema: MANAGER_JOURNAL_SCHEMA.to_owned(),
        schema_version: MANAGER_JOURNAL_SCHEMA_VERSION,
        request_id: plan.request_id.clone(),
        idempotency_key: plan.idempotency_key.clone(),
        plan_digest: plan.digest.clone(),
        plan: plan.clone(),
        entries: vec![JournalEntry {
            instruction_id: plan.entries[0].instruction_id.clone(),
            change: plan.entries[0].changes[0].clone(),
            status,
            toggle_attempts,
            outcome_code,
        }],
    }
}

fn temp_root(test_name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock must be after the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "goodscanner-{test_name}-{}-{nonce}",
        std::process::id()
    ))
}

fn remove_test_tree(root: &PathBuf) {
    if let Err(error) = fs::remove_dir_all(root) {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::NotFound,
            "could not remove test directory {}: {error}",
            root.display()
        );
    }
}

#[test]
fn switcher_gives_both_games_equal_resting_width() {
    for width in [600.0, 920.0] {
        eframe::egui::__run_test_ui(|ui| {
            ui.set_width(width);
            let available = ui.available_width();
            let gap = ui.spacing().item_spacing.x;
            let expected = (available - gap) / 2.0;
            let mut active_game = Game::Genshin;

            let layout = game_switcher::show(ui, Lang::En, &mut active_game, true, None);

            assert_eq!(active_game, Game::Genshin);
            assert!((layout.genshin_width - layout.star_rail_width).abs() <= 0.5);
            assert!((layout.genshin_width - expected).abs() <= 0.5);
            assert!((layout.star_rail_width - expected).abs() <= 0.5);
        });
    }
}

#[test]
fn default_star_rail_settings_use_automatic_hosted_reference_without_a_folder() {
    let root = temp_root("embedded-reference-default");
    let (_store, warning) = ApplicationConfigStore::for_executable_dir(&root);
    assert!(warning.is_none());
    assert!(
        !root.join("data").join("hsr_reference").exists(),
        "startup must not manufacture or require an external reference folder"
    );

    assert_eq!(
        hsr_scanner::data_cache::DATA_CACHE_URL,
        "https://hsr.ggartifact.com/good/hsr_scanner_data.json"
    );
    assert_eq!(hsr_scanner::data_cache::DATA_CACHE_DIRECTORY, "data/hsr");

    remove_test_tree(&root);
}

#[test]
fn star_rail_capture_defaults_match_genshin_sdr_at_the_config_boundary() {
    let expected = yas::capture::CaptureMethod::for_hdr_mode(false);
    assert_eq!(expected, yas::capture::CaptureMethod::BitBlt);
    assert_eq!(
        hsr_scanner::scanner::ScanConfig::default().effective_capture_method(),
        expected
    );
    assert_eq!(
        good_tools_app::config::StarRailSettings::default()
            .capture_method
            .to_yas(),
        None
    );

    let root = temp_root("shared-capture-default");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    // Persisted capture choices are retired; HDR now determines GUI capture.
    fs::write(&path, r#"{"schemaVersion":2,"starRail":{}}"#).unwrap();
    let store = ApplicationConfigStore::load(&path).unwrap();
    assert_eq!(store.config.star_rail.capture_method.to_yas(), None);

    for saved in ["wgc", "bitBlt"] {
        fs::write(
            &path,
            format!(r#"{{"schemaVersion":2,"starRail":{{"captureMethod":"{saved}"}}}}"#),
        )
        .unwrap();
        let mut store = ApplicationConfigStore::load(&path).unwrap();
        assert_eq!(store.config.star_rail.capture_method.to_yas(), None);
        store.persist_now().unwrap();
        assert!(!fs::read_to_string(&path).unwrap().contains("captureMethod"));
        let reloaded = ApplicationConfigStore::load(&path).unwrap();
        assert_eq!(reloaded.config.star_rail.capture_method.to_yas(), None);
    }
    remove_test_tree(&root);
}

#[test]
fn capture_settings_migrate_and_hdr_round_trips_through_public_config_load() {
    use good_tools_app::config::{StarRailCaptureMethod, StarRailSettings};
    use hsr_scanner::scanner::ScanConfig;
    use yas::capture::CaptureMethod;

    let root = temp_root("capture-hdr-migration");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for version in [1, 2, 3, 4] {
        for method in [None, Some("bitBlt"), Some("wgc"), Some("printWindow")] {
            let mut saved = serde_json::json!({
                "schemaVersion": version,
                "navigation": {"activeGame":"starRail", "starRailTab":"manager", "genshinTab":"scanner"},
                "starRail": {
                    "outputDir":"D:\\exports", "scanCharacters":false,
                    "maxCharacters":5, "maxGear":12, "dumpImages":true,
                    "timings":{"panelSwitchMs":1100, "characterPageMs":900},
                    "captureIncludeRelics":false, "managerJournalPath":"D:\\manager.jsonl"
                }
            });
            if let Some(method) = method {
                saved["starRail"]["captureMethod"] = method.into();
            }
            fs::write(&path, serde_json::to_string(&saved).unwrap()).unwrap();
            let mut store = ApplicationConfigStore::load(&path).unwrap();
            assert_eq!(
                store.config.schema_version,
                APPLICATION_CONFIG_SCHEMA_VERSION
            );
            assert!(!store.config.star_rail.hdr_mode);
            assert_eq!(store.config.star_rail.capture_method.to_yas(), None);
            assert_eq!(store.config.navigation.active_game, Game::StarRail);
            assert_eq!(store.config.navigation.selected_tab, ToolTab::Manager);
            assert_eq!(store.config.star_rail.output_dir, r"D:\exports");
            assert!(!store.config.star_rail.scan_characters);
            assert_eq!(store.config.star_rail.max_characters, 5);
            assert_eq!(store.config.star_rail.max_gear, 12);
            assert!(store.config.star_rail.dump_images);
            assert!(!store.config.star_rail.capture_include_relics);
            assert_eq!(store.config.star_rail.timings.panel_switch_ms, 1100);
            assert_eq!(store.config.star_rail.timings.character_page_ms, 900);
            assert_eq!(
                store.config.star_rail.manager_journal_path,
                r"D:\manager.jsonl"
            );
            store.persist_now().unwrap();
            assert_eq!(
                ApplicationConfigStore::load(&path).unwrap().config,
                store.config
            );

            // Restore Automatic and enable HDR as a player would in the GUI.
            store.config.star_rail.set_hdr_mode(true);
            store.persist_now().unwrap();
            let restored = ApplicationConfigStore::load(&path).unwrap();
            let settings = restored.config.star_rail;
            assert!(settings.hdr_mode);
            let scan = ScanConfig {
                capture_method: settings.capture_method.to_yas(),
                hdr_mode: settings.hdr_mode,
                ..Default::default()
            };
            assert_eq!(scan.effective_capture_method(), CaptureMethod::Wgc);
        }
    }
    assert_eq!(
        StarRailSettings::default().capture_method,
        StarRailCaptureMethod::Auto
    );
    remove_test_tree(&root);
}

#[test]
fn retired_capture_override_uses_saved_hdr_mode_after_migration() {
    use hsr_scanner::scanner::ScanConfig;
    use yas::capture::CaptureMethod;

    let root = temp_root("retired-capture-override");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for version in [1, 2, 3, 4] {
        for hdr_mode in [false, true] {
            for method in ["auto", "bitBlt", "wgc", "printWindow"] {
                // Include timings to cover the existing migration's early return.
                let saved = serde_json::json!({
                    "schemaVersion":version,
                    "starRail":{"captureMethod":method, "hdrMode":hdr_mode,
                        "timings":{"panelSwitchMs":650}, "outputDir":"D:\\exports",
                        "scanCharacters":false, "dumpImages":true}
                });
                fs::write(&path, serde_json::to_string(&saved).unwrap()).unwrap();
                let mut store = ApplicationConfigStore::load(&path).unwrap();
                let settings = &store.config.star_rail;
                assert_eq!(settings.capture_method.to_yas(), None);
                assert_eq!(settings.hdr_mode, hdr_mode);
                assert_eq!(settings.timings.panel_switch_ms, 650);
                assert_eq!(settings.output_dir, r"D:\exports");
                assert!(!settings.scan_characters);
                assert!(settings.dump_images);
                let scan = ScanConfig {
                    capture_method: settings.capture_method.to_yas(),
                    hdr_mode: settings.hdr_mode,
                    ..Default::default()
                };
                assert_eq!(
                    scan.effective_capture_method(),
                    CaptureMethod::for_hdr_mode(hdr_mode)
                );
                store.persist_now().unwrap();
                assert!(!fs::read_to_string(&path).unwrap().contains("captureMethod"));
                assert_eq!(
                    ApplicationConfigStore::load(&path).unwrap().config,
                    store.config
                );
            }
        }
    }
    remove_test_tree(&root);
}

#[test]
fn obsolete_custom_reference_path_is_ignored_when_loading_old_settings() {
    // Version 1 serialized an optional developer referenceBundle path. Its
    // removal discards only that setting; player scan preferences survive.
    let root = temp_root("v1-reference-removal");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    fs::write(&path,
        r#"{"schemaVersion":1,"starRail":{"referenceBundle":"C:\\missing-data","scanCharacters":false,"outputDir":"D:\\exports"}}"#
    ).unwrap();
    let store = ApplicationConfigStore::load(&path).unwrap();
    assert_eq!(
        store.config.schema_version,
        APPLICATION_CONFIG_SCHEMA_VERSION
    );
    let settings = &store.config.star_rail;
    assert!(!settings.scan_characters);
    assert!(settings.capture_include_characters);
    assert!(settings.capture_include_light_cones);
    assert!(settings.capture_include_relics);
    assert!(settings.capture_include_achievements);
    assert_eq!(settings.output_dir, r"D:\exports");
    assert!(!serde_json::to_string(&settings)
        .unwrap()
        .contains("referenceBundle"));
    remove_test_tree(&root);
}

#[test]
fn obsolete_archive_import_path_is_removed_without_changing_saved_preferences() {
    let root = temp_root("archive-import-removal");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for version in [1, 2] {
        for import_path in [None, Some(""), Some(r"D:\archives\star-rail.json")] {
            // Pre-removal persisted config, including scan, capture, manager,
            // output and per-game navigation choices that must all survive.
            let mut saved = serde_json::json!({
                "schemaVersion": version,
                "navigation": {
                    "activeGame": "starRail",
                    "genshinTab": "manager",
                    "starRailTab": "scanner"
                },
                "starRail": {
                    "outputDir": r"D:\exports",
                    "scanCharacters": false,
                    "scanLightCones": false,
                    "scanRelicsAndOrnaments": true,
                    "expectedCharacters": 42,
                    "maxCharacters": 85,
                    "maxInventoryItems": 789,
                    "captureMethod": "printWindow",
                    "navigationDelayMs": 600,
                    "panelTimeoutMs": 2500,
                    "nextCharacterKey": "q",
                    "trailblazerName": "Trailblazer",
                    "trailblazerGender": "Stelle",
                    "dumpImages": true,
                    "managerInstructionsPath": r"D:\manager\instructions.json",
                    "managerJournalPath": r"D:\manager\journal.jsonl",
                    "captureIncludeAchievements": false,
                    "captureIncludeCharacters": false,
                    "captureIncludeLightCones": true,
                    "captureIncludeRelics": false,
                    "captureDumpPackets": true,
                    "captureOnlyKeepLatestExport": true
                }
            });
            if let Some(import_path) = import_path {
                saved["starRail"]["offlineImportPath"] = import_path.into();
            }
            fs::write(&path, serde_json::to_string_pretty(&saved).unwrap()).unwrap();

            let mut store = ApplicationConfigStore::load(&path).unwrap();
            assert_eq!(
                store.config.schema_version,
                APPLICATION_CONFIG_SCHEMA_VERSION
            );
            store.persist_now().unwrap();

            saved["schemaVersion"] = APPLICATION_CONFIG_SCHEMA_VERSION.into();
            saved["starRail"]
                .as_object_mut()
                .unwrap()
                .remove("offlineImportPath");
            // Preserve non-scanner preferences while migrating delays/caps.
            let persisted: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
            for key in [
                "expectedCharacters",
                "maxInventoryItems",
                "navigationDelayMs",
                "panelTimeoutMs",
                "nextCharacterKey",
            ] {
                saved["starRail"].as_object_mut().unwrap().remove(key);
            }
            saved["starRail"]["timings"] = serde_json::json!({
                "menuOpenMs": 2100, "menuCloseMs": 1600, "inputSettleMs": 180,
                "inventoryTabMs": 2100, "panelSwitchMs": 1100, "tracesOpenMs": 2600,
                "characterPageMs": 700, "characterSwitchMs": 200, "captureIntervalMs": 80, "keySettleMs": 18,
                "pollIntervalMs": 20, "selectionSettleMs": 280, "panelTimeoutMs": 2500,
                "menuPollIntervalMs": 300, "statusToggleMs": 600
            });
            saved["starRail"]["maxLightCones"] = 0.into();
            saved["starRail"]["maxGear"] = 0.into();
            saved["starRail"]["hdrMode"] = false.into();
            saved["starRail"]["managerPort"] = 8765.into();
            saved["starRail"]["scanOnlyKeepLatestExport"] = true.into();
            saved["starRail"]["verbose"] = false.into();
            saved["starRail"]["stopOnFailure"] = false.into();
            saved["starRail"]["dumpJobData"] = false.into();
            saved["starRail"]["continuousCapture"] = false.into();
            saved["appearance"] = "system".into();
            saved["navigation"] = serde_json::json!({
                "activeGame": "starRail", "selectedTab": "scanner"
            });
            saved["starRail"]
                .as_object_mut()
                .unwrap()
                .remove("captureMethod");
            assert_eq!(persisted, saved);
            let reloaded = ApplicationConfigStore::load(&path).unwrap();
            assert_eq!(reloaded.config, store.config);
        }
    }
    remove_test_tree(&root);
}

#[test]
fn navigation_shares_the_selected_tab_between_games() {
    let mut navigation = GameNavigation::default();
    assert_eq!(navigation.active_game, Game::Genshin);
    assert_eq!(navigation.active_tab(), ToolTab::default_for_build());

    navigation.select_tab(ToolTab::Manager);
    navigation.active_game = Game::StarRail;
    assert_eq!(navigation.active_tab(), ToolTab::Manager);
    navigation.select_tab(ToolTab::Credits);

    navigation.active_game = Game::Genshin;
    assert_eq!(navigation.active_tab(), ToolTab::Credits);
    navigation.active_game = Game::StarRail;
    assert_eq!(navigation.active_tab(), ToolTab::Credits);
}

#[test]
fn legacy_scan_guards_migrate_to_automatic_limits_and_total_timing_settings() {
    let root = temp_root("hsr-scan-guard-migration");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for version in [1, 2, 3] {
        for cap in [None, Some(200), Some(17), Some(0)] {
            let mut old = serde_json::json!({
                "schemaVersion": version,
                "navigation": { "activeGame": "starRail", "genshinTab": "manager", "starRailTab": "scanner" },
                "starRail": {
                    "navigationDelayMs": 600, "panelTimeoutMs": 2500,
                    "maxInventoryItems": 10, "expectedCharacters": 42,
                    "nextCharacterKey": "q", "dumpImages": true,
                    "outputDir": "D:/exports", "scanCharacters": false
                }
            });
            if let Some(cap) = cap {
                old["starRail"]["maxCharacters"] = cap.into();
            }
            fs::write(&path, serde_json::to_string(&old).unwrap()).unwrap();
            let mut store = ApplicationConfigStore::load(&path).unwrap();
            let settings = &store.config.star_rail;
            assert_eq!(
                settings.max_characters,
                cap.filter(|cap| *cap != 200).unwrap_or(0)
            );
            assert_eq!((settings.max_light_cones, settings.max_gear), (0, 0));
            assert_eq!(settings.timings.menu_open_ms, 2100);
            assert_eq!(settings.timings.inventory_tab_ms, 2100);
            assert_eq!(settings.timings.panel_switch_ms, 1100);
            assert_eq!(settings.timings.menu_close_ms, 1600);
            assert_eq!(settings.timings.traces_open_ms, 2600);
            assert_eq!(settings.timings.panel_timeout_ms, 2500);
            assert_eq!(settings.timings.status_toggle_ms, 600);
            assert_eq!(settings.output_dir, "D:/exports");
            assert!(settings.dump_images);
            assert!(!settings.scan_characters);
            assert_eq!(store.config.navigation.active_game, Game::StarRail);
            store.persist_now().unwrap();
            let persisted = fs::read_to_string(&path).unwrap();
            for key in [
                "navigationDelayMs",
                "maxInventoryItems",
                "expectedCharacters",
                "nextCharacterKey",
            ] {
                assert!(!persisted.contains(key));
            }
            assert_eq!(
                ApplicationConfigStore::load(&path).unwrap().config,
                store.config
            );
        }
    }
    // A new explicit cap of 200 is a sample preference, not the old default.
    let mut store = ApplicationConfigStore::load(&path).unwrap();
    store.config.star_rail.max_characters = 200;
    store.config.star_rail.max_light_cones = 5;
    store.config.star_rail.max_gear = 9;
    store.config.star_rail.timings.character_page_ms = 1234;
    store.persist_now().unwrap();
    assert_eq!(
        ApplicationConfigStore::load(&path).unwrap().config,
        store.config
    );
    remove_test_tree(&root);
}

#[test]
fn unavailable_capture_tab_routes_to_scanner_without_erasing_saved_tab() {
    let mut navigation = GameNavigation {
        active_game: Game::Genshin,
        selected_tab: ToolTab::Capture,
    };

    navigation.active_game = Game::StarRail;
    if cfg!(feature = "capture") {
        assert_eq!(navigation.active_tab(), ToolTab::Capture);
    } else {
        assert_eq!(navigation.active_tab(), ToolTab::Scanner);
    }
    assert_eq!(
        navigation.selected_tab,
        ToolTab::Capture,
        "runtime fallback must not erase GOODCapture's saved tab"
    );
}

#[test]
fn star_rail_settings_and_shared_navigation_round_trip() {
    let root = temp_root("shared-config-round-trip");
    let app_config_path = root.join(APPLICATION_CONFIG_FILE_REL);
    let genshin_config_path = root.join("data").join("good_config.json");
    fs::create_dir_all(
        genshin_config_path
            .parent()
            .expect("Genshin config path must have a parent"),
    )
    .expect("test config directory should be creatable");
    let genshin_sentinel = "{\"existingGenshinConfig\":true}\n";
    fs::write(&genshin_config_path, genshin_sentinel)
        .expect("Genshin sentinel config should be writable");

    let mut store =
        ApplicationConfigStore::load(&app_config_path).expect("default app config should load");
    store.config.navigation.active_game = Game::StarRail;
    store.config.navigation.selected_tab = ToolTab::Capture;
    store.config.star_rail.output_dir = "D:\\exports\\star-rail".to_owned();
    store.config.star_rail.scan_characters = false;
    store.config.star_rail.max_gear = 789;
    store
        .persist_now()
        .expect("separate application config should save");

    assert_eq!(
        fs::read_to_string(&genshin_config_path).expect("Genshin sentinel should remain readable"),
        genshin_sentinel,
        "saving Star Rail settings must not rewrite GoodUserConfig"
    );

    let reloaded =
        ApplicationConfigStore::load(&app_config_path).expect("saved app config should reload");
    assert_eq!(reloaded.config.navigation.active_game, Game::StarRail);
    assert_eq!(reloaded.config.navigation.selected_tab, ToolTab::Capture);
    assert_eq!(
        reloaded.config.star_rail.output_dir,
        "D:\\exports\\star-rail"
    );
    assert!(!reloaded.config.star_rail.scan_characters);
    assert_eq!(reloaded.config.star_rail.max_gear, 789);

    remove_test_tree(&root);
}

#[test]
fn manager_port_migrates_from_v3_without_overwriting_a_saved_choice() {
    let root = temp_root("manager-port-migration");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for port in [None, Some(8766)] {
        let mut value = serde_json::json!({"schemaVersion":3,"starRail":{"hdrMode":true,"trailblazerName":"Player"}});
        if let Some(port) = port {
            value["starRail"]["managerPort"] = port.into();
        }
        fs::write(&path, value.to_string()).unwrap();
        let mut store = ApplicationConfigStore::load(&path).unwrap();
        assert_eq!(
            store.config.schema_version,
            APPLICATION_CONFIG_SCHEMA_VERSION
        );
        assert_eq!(store.config.star_rail.manager_port, port.unwrap_or(8765));
        assert!(store.config.star_rail.hdr_mode);
        assert_eq!(store.config.star_rail.trailblazer_name, "Player");
        store.persist_now().unwrap();
        assert_eq!(
            ApplicationConfigStore::load(&path).unwrap().config,
            store.config
        );
    }
    remove_test_tree(&root);
}

#[test]
fn scan_export_preferences_load_old_configs_and_preserve_explicit_choices() {
    let root = temp_root("scan-export-preferences");
    fs::create_dir_all(&root).unwrap();
    let path = root.join("config.json");
    for version in [1, 2, 3, 4] {
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion":version,
                "starRail":{"scanCharacters":false,"outputDir":"D:\\exports","managerPort":8766}
            }))
            .unwrap(),
        )
        .unwrap();
        let mut store = ApplicationConfigStore::load(&path).unwrap();
        assert!(store.config.star_rail.scan_only_keep_latest_export);
        assert!(!store.config.star_rail.scan_characters);
        assert_eq!(store.config.star_rail.manager_port, 8766);
        assert_eq!(store.config.star_rail.output_dir, "D:\\exports");
        store.config.star_rail.scan_only_keep_latest_export = false;
        store.persist_now().unwrap();
        assert_eq!(
            ApplicationConfigStore::load(&path).unwrap().config,
            store.config
        );
    }
    remove_test_tree(&root);
}

#[test]
fn unsupported_config_is_not_overwritten_by_startup_or_shutdown() {
    let root = temp_root("future-config-preserved");
    let app_config_path = root.join(APPLICATION_CONFIG_FILE_REL);
    fs::create_dir_all(
        app_config_path
            .parent()
            .expect("application config path must have a parent"),
    )
    .expect("test config directory should be creatable");
    let future_config = concat!(
        "{\n",
        "  \"schemaVersion\": 999,\n",
        "  \"navigation\": {\n",
        "    \"activeGame\": \"starRail\",\n",
        "    \"genshinTab\": \"manager\",\n",
        "    \"starRailTab\": \"credits\"\n",
        "  },\n",
        "  \"starRail\": {}\n",
        "}\n",
    );
    fs::write(&app_config_path, future_config).expect("future config fixture should be writable");

    let (mut store, warning) = ApplicationConfigStore::for_executable_dir(&root);
    assert!(warning.is_some(), "future schema must surface a warning");
    assert_eq!(
        store.config.star_rail.output_dir,
        root.display().to_string()
    );
    assert!(!store.save_pending());

    // Ordinary navigation/settings interaction is not permission to destroy
    // a config written by a newer version of the application.
    store.config.navigation.active_game = Game::StarRail;
    store.config.star_rail.max_gear = 456;
    store
        .auto_save_tick()
        .expect("future-schema fallback should remain read-only");
    store
        .persist_now()
        .expect("shutdown after UI edits should leave the future config untouched");
    assert_eq!(
        fs::read_to_string(&app_config_path).expect("future config should remain readable"),
        future_config
    );

    remove_test_tree(&root);
}

#[test]
fn manager_adapter_surfaces_original_interrupted_plan_as_recovery_preview() {
    let envelope = manager_envelope();
    let plan = manager_plan(&envelope);
    let mut store = MemoryJournalStore {
        journal: Some(manager_journal(&plan, JournalStatus::MutationStarted)),
    };

    let preview = good_tools_app::gui::star_rail_worker::load_recovery_preview(
        &envelope,
        &mut store,
        "settings-v1".to_owned(),
    )
    .expect("valid interrupted journal should load")
    .expect("interrupted journal should produce a recovery preview");

    assert!(preview.recovered);
    assert_eq!(preview.plan, plan);
    assert_eq!(preview.settings_identity, "settings-v1");
    assert!(preview.exact_json.contains(&preview.plan.digest));
}

#[test]
fn completed_journal_is_archived_before_a_distinct_plan_uses_the_base_path() {
    let root = temp_root("completed-manager-journal");
    fs::create_dir_all(&root).expect("manager journal directory should be creatable");
    let journal_path = root.join("hsr_manager_journal.jsonl");
    let first_envelope = manager_envelope();
    let first_plan = manager_plan(&first_envelope);
    let pending_journal = manager_journal(&first_plan, JournalStatus::Pending);
    let started_journal = manager_journal(&first_plan, JournalStatus::MutationStarted);
    let first_journal = manager_journal(&first_plan, JournalStatus::Verified);
    fs::write(
        &journal_path,
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&pending_journal)
                .expect("pending manager journal should serialize"),
            serde_json::to_string(&started_journal)
                .expect("started manager journal should serialize"),
            serde_json::to_string(&first_journal)
                .expect("verified manager journal should serialize")
        ),
    )
    .expect("verified manager journal should be writable");

    let mut first_store = hsr_scanner::manager::AppendOnlyJsonJournalStore::new(&journal_path);
    let recovered = good_tools_app::gui::star_rail_worker::load_recovery_preview(
        &first_envelope,
        &mut first_store,
        "settings-v1".to_owned(),
    )
    .expect("verified journal should still be safely recoverable")
    .expect("verified journal should retain its original plan");
    assert!(recovered.recovered);

    let archived =
        good_tools_app::gui::star_rail_worker::archive_completed_manager_journal(&journal_path)
            .expect("completed journal should archive")
            .expect("completed journal file should exist");
    assert!(!journal_path.exists());
    assert!(archived.exists());

    let mut second_envelope = first_envelope;
    second_envelope.request_id = "second-request".to_owned();
    second_envelope.instructions[0].id = "hsr-manager-0002-lock".to_owned();
    second_envelope.idempotency_key = second_envelope
        .expected_idempotency_key()
        .expect("second instruction set should have a deterministic key");
    let mut second_store = hsr_scanner::manager::AppendOnlyJsonJournalStore::new(&journal_path);
    let second_preview = good_tools_app::gui::star_rail_worker::load_recovery_preview(
        &second_envelope,
        &mut second_store,
        "settings-v2".to_owned(),
    )
    .expect("a distinct plan should not be blocked by the archived journal");
    assert!(second_preview.is_none());

    remove_test_tree(&root);
}

#[cfg(all(feature = "capture", target_os = "windows"))]
#[test]
fn stop_before_worker_start_never_opens_capture_boundary() {
    assert!(
        good_tools_app::gui::star_rail_capture_tab::stop_before_worker_start_suppresses_start_for_test(),
        "a Stop request during reference loading must suppress the worker's later Start command"
    );
}

#[cfg(all(feature = "capture", target_os = "windows"))]
#[test]
fn genshin_capture_completion_releases_native_boundary_without_recapture_or_rendering() {
    use good_tools_app::gui::capture_tab::{native_boundary_available_for_test, CaptureTabState};

    let root = temp_root("genshin-capture-handoff");
    fs::create_dir_all(&root).unwrap();
    let mut capture = CaptureTabState::new(root.display().to_string());
    let mut export = genshin_scanner::scanner::common::models::GoodExport::new(
        Some(vec![]),
        Some(vec![]),
        Some(vec![]),
    );
    export.achievements = Some(vec![80001]);
    let release = capture.inject_export_for_test(export);

    // A result alone is insufficient: the old task must release its native
    // boundary before the scanner/manager can be enabled or Done is shown.
    capture.tick();
    assert!(capture.is_busy());
    assert!(capture.completed_export_path_for_test().is_none());
    assert!(!native_boundary_available_for_test());
    release.send(()).unwrap();

    // No Capture tab rendering or Recapture click throughout this handoff.
    let path = (0..200)
        .find_map(|_| {
            capture.tick();
            let path = capture.completed_export_path_for_test().map(PathBuf::from);
            if path.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            path
        })
        .expect("capture must export and finish without a user action");
    assert!(!capture.is_busy());
    assert!(capture.handle.is_none());
    assert!(
        native_boundary_available_for_test(),
        "next game task must acquire the boundary"
    );
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
    assert_eq!(saved["format"], "GOOD");
    assert_eq!(saved["version"], 3);
    assert_eq!(saved["achievements"], serde_json::json!([80001]));
    remove_test_tree(&root);
}

#[cfg(all(feature = "capture", target_os = "windows"))]
#[test]
fn completed_capture_advances_and_exports_while_its_tab_is_inactive() {
    let root = temp_root("inactive-capture-completion");
    let output_dir = root.join("exports");
    let references = hsr_scanner::load_embedded_gilore_reference().unwrap();
    let achievement_id = references.achievement_ids().next().unwrap();

    let mut capture = good_tools_app::gui::star_rail_capture_tab::StarRailCaptureState::new(
        output_dir.display().to_string(),
    );
    let mut inventory: hsr_scanner::ObservationSnapshot = serde_json::from_str(include_str!(
        "../../experimental/hsr/tests/fixtures/observations.json"
    ))
    .unwrap();
    inventory.evidence.kind = hsr_scanner::EvidenceKind::PacketCapture;
    for gear in &mut inventory.gear {
        gear.main_stat_value = references
            .relic_main_stat_value_for_piece(
                references.gear(gear.piece_id).unwrap(),
                &gear.main_stat_key,
                gear.level,
            )
            .unwrap();
    }
    capture.inject_completed_for_test(references, vec![achievement_id], inventory);

    // Deliberately never render the Capture tab. GuiApp's unconditional tick
    // must still advance completion, stop cleanup, and export delivery.
    let exported_path = (0..200).find_map(|_| {
        capture.tick();
        let path = capture.completed_export_path_for_test().map(PathBuf::from);
        if path.is_none() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        path
    });
    let exported_path = exported_path.expect("off-screen capture should finish its export");
    let saved_bytes = fs::read(&exported_path).unwrap();
    let saved_modified = fs::metadata(&exported_path).unwrap().modified().unwrap();
    for _ in 0..200 {
        capture.tick();
        assert_eq!(
            capture.completed_export_path_for_test().map(PathBuf::from),
            Some(exported_path.clone()),
            "completed capture must remain done until an explicit new Start"
        );
        assert!(
            !capture.is_busy(),
            "completed capture must not start another export"
        );
    }
    assert_eq!(fs::read(&exported_path).unwrap(), saved_bytes);
    assert_eq!(
        fs::metadata(&exported_path).unwrap().modified().unwrap(),
        saved_modified
    );
    let common: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(&exported_path).expect("Fribbels export should be readable"),
    )
    .expect("Fribbels export should be valid JSON");
    assert_eq!(common["source"], "HSR-Scanner");
    assert_eq!(common["version"], 4);
    assert_eq!(common["generator"]["name"], "GOODScanner");
    assert_eq!(common["metadata"]["uid"], 600_000_001);
    assert!(!common["characters"].as_array().unwrap().is_empty());
    assert!(!common["light_cones"].as_array().unwrap().is_empty());
    assert!(!common["relics"].as_array().unwrap().is_empty());

    assert!(exported_path
        .file_name()
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("star_rail_capture_"));
    assert_eq!(
        common["quests"]["records"],
        serde_json::json!([{ "id": achievement_id, "status": 3, "progress": 1, "counters": [] }])
    );
    assert!(common.get("achievements").is_none());
    let extras: Vec<_> = fs::read_dir(&output_dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != exported_path.file_name().unwrap().to_str().unwrap())
        .collect();
    assert!(
        extras.is_empty(),
        "capture must write a single JSON file, found {extras:?}"
    );
    if let Ok(path) = std::env::var("HSR_INTEROP_FIXTURE") {
        fs::write(path, serde_json::to_vec_pretty(&common).unwrap()).unwrap();
    }
    remove_test_tree(&root);
}

#[test]
fn capture_selection_defaults_and_saved_choices_are_independent_of_ocr() {
    // Older schema-2 settings always captured inventory and had only the achievement toggle.
    let mut settings: good_tools_app::config::StarRailSettings =
        serde_json::from_str(r#"{"captureIncludeAchievements":false,"scanCharacters":false}"#)
            .unwrap();
    assert!(
        settings.capture_include_characters
            && settings.capture_include_light_cones
            && settings.capture_include_relics
    );
    assert!(!settings.capture_include_achievements);
    assert!(!settings.capture_dump_packets && settings.capture_only_keep_latest_export);
    settings.capture_dump_packets = true;
    settings.capture_only_keep_latest_export = true;
    settings.capture_include_characters = false;
    settings.capture_include_relics = false;
    let restored: good_tools_app::config::StarRailSettings =
        serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
    assert!(
        !restored.capture_include_characters
            && restored.capture_include_light_cones
            && !restored.capture_include_relics
            && !restored.capture_include_achievements
    );
    assert!(restored.scan_light_cones && restored.scan_relics_and_ornaments);
    assert!(restored.capture_dump_packets && restored.capture_only_keep_latest_export);
}
