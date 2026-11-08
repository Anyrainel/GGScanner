use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use hsr_scanner::{
    data_cache::load_data_cache,
    export_observations,
    manager::{
        apply_manager_request, load_manager_recovery_plan, validate_manager_envelope_reference,
        AppendOnlyJsonJournalStore, HsrControllerLease, ManagerInstructionsEnvelope,
        ManagerJournalStore, ManagerPlan,
    },
    reference::ReferenceCache,
    scanner::{HsrScanner, ScanConfig, ScanResult, ScanTargets},
    HsrError, Language,
};

use crate::config::StarRailSettings;

use super::{
    star_rail_state::ManagerPreview,
    state::{LogSource, TaskKind, TaskStatus, UiError, UiText},
    worker::{self, TaskHandle},
};

fn manager_scan_observer(
    progress: Arc<Mutex<super::task_progress::TaskProgress>>,
) -> hsr_scanner::scan_progress::ScanObserver {
    Box::new(move |category, event| {
        if category != hsr_scanner::scan_progress::ScanCategory::Gear {
            return;
        }
        if let hsr_scanner::scan_progress::ScanEvent::Progress {
            recognized, total, ..
        } = event
        {
            if let Some(step) = progress
                .lock()
                .unwrap()
                .steps
                .iter_mut()
                .find(|s| s.key == "gear")
            {
                step.completed = recognized;
                step.total = total;
                step.state = super::task_progress::StepState::Running;
            }
        }
    })
}

fn hsr_language() -> Language {
    if yas::lang::is_en() {
        Language::En
    } else {
        Language::ZhCn
    }
}

fn user_aborted(cancel: &yas::cancel::CancelToken) -> bool {
    cancel.reason() == Some(yas::cancel::StopReason::UserAbort)
}

fn stopped(task: TaskKind) -> UiText {
    match task {
        TaskKind::Scanner => UiText::new("扫描已安全停止。", "The scan stopped safely."),
        TaskKind::Manager => UiText::new(
            "管理器预览已安全停止；未执行任何变更。",
            "The manager preview stopped safely; no changes were made.",
        ),
        TaskKind::Capture => UiText::new("抓包已安全停止。", "Capture stopped safely."),
    }
}

pub(super) fn hsr_ui_error(hint: UiText, error: HsrError) -> UiError {
    let language = hsr_language();
    let actionable = error.hint();
    UiError::from_message(
        UiText::new(
            actionable.select(hsr_scanner::localization::Language::ZhCn),
            actionable.select(hsr_scanner::localization::Language::En),
        ),
        format!(
            "{}\n{}",
            hint.text(match language {
                hsr_scanner::localization::Language::ZhCn => super::state::Lang::Zh,
                hsr_scanner::localization::Language::En => super::state::Lang::En,
            }),
            error.localized_message(language)
        ),
    )
}

fn ensure_ocr_runtime_with_status(status: Option<&Arc<Mutex<TaskStatus>>>) -> Result<(), UiError> {
    #[cfg(target_os = "windows")]
    {
        genshin_scanner::cli::check_vcpp_runtime().map_err(|error| {
            UiError::from_anyhow(
                UiText::new(
                    "星穹铁道扫描器无法启动，因为所需的 Windows 运行组件不可用。请按完整错误中的说明修复后重试。",
                    "The Star Rail scanner cannot start because a required Windows runtime component is unavailable. Follow the full error details, then retry.",
                ),
                &error,
            )
        })?;
    }
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        if !genshin_scanner::cli::check_onnxruntime() {
            if let Some(status) = status {
                *status.lock().unwrap() =
                    TaskStatus::Running(UiText::new("正在下载 OCR 引擎", "Downloading OCR engine"));
            }
            genshin_scanner::cli::download_onnxruntime().map_err(|error| {
                UiError::from_anyhow(
                    UiText::new(
                        "星穹铁道扫描器无法下载 OCR 引擎。请检查网络连接或安全软件设置，然后重试。",
                        "The Star Rail scanner could not download its OCR engine. Check the network connection or security software, then retry.",
                    ),
                    &error,
                )
            })?;
        }
    }
    Ok(())
}

fn scanner_config(
    settings: &StarRailSettings,
    targets: ScanTargets,
) -> Result<ScanConfig, UiError> {
    let trailblazer = settings.trailblazer();
    if targets.characters && trailblazer.is_none() {
        return Err(UiError::from_message(
            UiText::new(
                "扫描角色前，请填写开拓者的游戏内昵称并选择性别。",
                "Before scanning Characters, enter the Trailblazer's in-game nickname and choose a gender.",
            ),
            "starRail.trailblazerName or starRail.trailblazerGender is empty",
        ));
    }

    Ok(ScanConfig {
        targets,
        // Persisted settings always load Auto; explicit diagnostic CLI overrides
        // are transient and share the same scanner implementation.
        capture_method: settings.capture_method.to_yas(),
        hdr_mode: settings.hdr_mode,
        timings: settings.timings.clone(),
        max_light_cones: settings.max_light_cones,
        max_gear: settings.max_gear,
        max_characters: settings.max_characters,
        trailblazer,
        dump_images: settings.dump_images,
        save_on_cancel: true,
        stop_on_failure: settings.stop_on_failure,
        ..ScanConfig::default()
    })
}

fn ensure_output_dir(settings: &StarRailSettings) -> Result<PathBuf, UiError> {
    let output_dir = settings.export_directory();
    fs::create_dir_all(&output_dir).map_err(|error| {
        UiError::from_error(
            UiText::new(
                "无法创建星穹铁道导出文件夹。请检查路径和文件权限。",
                "The Star Rail export folder could not be created. Check the path and file permissions.",
            ),
            error,
        )
    })?;
    Ok(output_dir)
}

/// All inventory flows share the achievement-free hosted reference.
pub fn load_references() -> Result<ReferenceCache, HsrError> {
    let references = load_data_cache()?;
    references.validate_live_complete_profile()?;
    Ok(references)
}

/// Finalize a real scan through the same policy for GUI, HTTP and CLI callers.
pub fn finish_scan_export(
    settings: &StarRailSettings,
    result: &ScanResult,
    references: &ReferenceCache,
    cancelled: bool,
    status: Option<&Arc<Mutex<TaskStatus>>>,
) -> Result<TaskStatus, UiError> {
    let stopped = || {
        TaskStatus::Stopped(UiText::new(
            "扫描已停止 · 未导出",
            "Scan stopped · no export",
        ))
    };
    let export = export_observations(
        &result.observations,
        references,
        &result.export_details,
        None,
    )
    .map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "扫描结果无法转换为 HSR-Scanner v4 JSON。",
                "The scan result could not be converted into HSR-Scanner v4 JSON.",
            ),
            error,
        )
    })?;
    let counts = v4_inventory_counts(&export);
    if counts == (0, 0, 0, 0) {
        if cancelled {
            return Ok(stopped());
        }
        return Err(UiError::from_message(
            UiText::new("未识别出所选数据，未导出。请检查游戏画面和日志后重试。", "No selected data was recognized. Nothing was exported. Check the game screen and logs, then retry."),
            "scan yielded no validated inventory entries; older exports retained",
        ));
    }
    if let Some(status) = status {
        *status.lock().unwrap() = TaskStatus::Running(UiText::new("正在导出数据", "Saving export"));
    }
    let output_dir = ensure_output_dir(settings)?;
    let partial = cancelled
        || (settings.scan_characters
            && result.coverage.characters != hsr_scanner::model::CoverageLevel::Complete)
        || (settings.scan_light_cones
            && result.coverage.light_cones != hsr_scanner::model::CoverageLevel::Complete)
        || (settings.scan_relics_and_ornaments
            && result.coverage.relics != hsr_scanner::model::CoverageLevel::Complete);
    let path = super::star_rail_exports::write_scan_file(&output_dir, &export, settings.scan_only_keep_latest_export, partial).map_err(|error| {
        UiError::from_error(
            UiText::new(
                "扫描导出未完成，请检查输出位置或旧文件的权限。新文件若已保存，其路径在完整错误中。",
                "Scan export did not finish. Check the output location or old file permissions. If the new file was saved, its path is in the full error.",
            ),
            error,
        )
    })?;
    Ok(TaskStatus::Exported {
        message: v4_export_message(counts, &path),
        path: path.display().to_string(),
        partial,
    })
}

fn v4_inventory_counts(export: &serde_json::Value) -> (usize, usize, usize, usize) {
    let characters = export["characters"].as_array().map(Vec::len).unwrap_or(0);
    let light_cones = export["light_cones"].as_array().map(Vec::len).unwrap_or(0);
    let relics = export["relics"].as_array();
    let planar = relics
        .map(|items| {
            items
                .iter()
                .filter(|item| matches!(item["slot"].as_str(), Some("Planar Sphere" | "Link Rope")))
                .count()
        })
        .unwrap_or(0);
    let cavern = relics.map(Vec::len).unwrap_or(0).saturating_sub(planar);
    (characters, light_cones, cavern, planar)
}

fn v4_export_message(counts: (usize, usize, usize, usize), path: &Path) -> UiText {
    UiText::new(
        format!(
            "已导出：{} 个角色、{} 个光锥、{} 件隧洞遗器、{} 件位面饰品。输出：{}",
            counts.0, counts.1, counts.2, counts.3, path.display()
        ),
        format!(
            "Exported {} Characters, {} Light Cones, {} Cavern Relics, and {} Planar Ornaments. Output: {}",
            counts.0, counts.1, counts.2, counts.3, path.display()
        ),
    )
}

/// Preserve a fully reconciled journal under a unique completed name so the
/// configured base path can safely serve the next distinct instruction set.
/// Incomplete and needs-review journals must never call this function.
pub fn archive_completed_manager_journal(path: &Path) -> Result<Option<PathBuf>, UiError> {
    if !path.exists() {
        return Ok(None);
    }
    let timestamp = genshin_scanner::cli::chrono_timestamp();
    for sequence in 0..1_000_u16 {
        let suffix = if sequence == 0 {
            format!(".completed-{timestamp}")
        } else {
            format!(".completed-{timestamp}-{sequence}")
        };
        let mut archive_name = path.as_os_str().to_os_string();
        archive_name.push(suffix);
        let archive_path = PathBuf::from(archive_name);
        if archive_path.exists() {
            continue;
        }
        match fs::rename(path, &archive_path) {
            Ok(()) => return Ok(Some(archive_path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(UiError::from_error(
                    UiText::new(
                        "所有变更均已验证，但恢复日志无法安全归档。在导入其他指令前，请先选择新的恢复日志路径。",
                        "All changes were verified, but the recovery journal could not be archived safely. Choose a new recovery-journal path before loading different instructions.",
                    ),
                    error,
                ));
            },
        }
    }
    Err(UiError::from_message(
        UiText::new(
            "所有变更均已验证，但找不到可用的恢复日志归档名。请选择新的恢复日志路径。",
            "All changes were verified, but no available recovery-journal archive name was found. Choose a new recovery-journal path.",
        ),
        format!("completed journal archive namespace exhausted; path={}", path.display()),
    ))
}

pub fn spawn_scan(
    settings: &StarRailSettings,
    status: Arc<Mutex<TaskStatus>>,
    progress: Arc<Mutex<super::task_progress::TaskProgress>>,
) -> TaskHandle {
    let settings = settings.clone();
    let status_for_scan = status.clone();
    worker::spawn_cancellable_task(
        TaskKind::Scanner,
        LogSource::Scanner,
        status,
        UiText::new("正在初始化星穹铁道扫描器", "Initializing Star Rail scanner"),
        UiText::new(
            "正在安全停止星穹铁道扫描",
            "Stopping the Star Rail scan safely",
        ),
        move |cancel| run_scan_task(&settings, cancel, status_for_scan, progress),
    )
}

fn run_scan_task(
    settings: &StarRailSettings,
    cancel: yas::cancel::CancelToken,
    status: Arc<Mutex<TaskStatus>>,
    progress: Arc<Mutex<super::task_progress::TaskProgress>>,
) -> Result<TaskStatus, UiError> {
    use super::task_progress::{Step, TaskProgress};
    let mut steps = Vec::new();
    for (enabled, key, zh, en) in [
        (settings.scan_characters, "characters", "角色", "Characters"),
        (
            settings.scan_light_cones,
            "light_cones",
            "光锥",
            "Light Cones",
        ),
        (
            settings.scan_relics_and_ornaments,
            "gear",
            "遗器与位面饰品",
            "Relics & Ornaments",
        ),
    ] {
        if enabled {
            steps.push(Step::new(key, zh, en));
        }
    }
    *progress.lock().unwrap() = TaskProgress { steps };
    let status_for_progress = status.clone();
    let status_for_phases = status;
    let track = progress.clone();
    let cancel_for_observer = cancel.clone();
    let observer = Box::new(move |category, event| {
        use super::task_progress::StepState;
        use hsr_scanner::scan_progress::{ScanCategory, ScanEvent};
        let key = match category {
            ScanCategory::Characters => "characters",
            ScanCategory::LightCones => "light_cones",
            ScanCategory::Gear => "gear",
        };
        if let Some(step) = track
            .lock()
            .unwrap()
            .steps
            .iter_mut()
            .find(|s| s.key == key)
        {
            match event {
                ScanEvent::Started => {
                    step.state = StepState::Running;
                    if !cancel_for_observer.is_cancelled() {
                        *status_for_progress.lock().unwrap() = TaskStatus::Running(UiText::new(
                            format!("正在扫描{}", step.zh),
                            format!("Scanning {}", step.en),
                        ));
                    }
                },
                ScanEvent::Progress {
                    recognized, total, ..
                } => {
                    step.completed = recognized;
                    step.total = total;
                },
                ScanEvent::Finished {
                    recognized,
                    complete,
                } => {
                    step.completed = recognized;
                    step.state = if complete {
                        StepState::Complete
                    } else {
                        StepState::Interrupted
                    };
                },
            }
        }
    });
    let result = run_scan_observed(
        &settings,
        cancel,
        None,
        Some(observer),
        Some(&status_for_phases),
    );
    progress.lock().unwrap().interrupt_unfinished();
    result
}

/// The GUI and shipped command line use the same runtime, lease, scan, and export path.
pub(crate) fn run_scan(
    settings: &StarRailSettings,
    cancel: yas::cancel::CancelToken,
    sample_limit: Option<usize>,
) -> Result<UiText, UiError> {
    run_scan_observed(settings, cancel, sample_limit, None, None).map(|status| match status {
        TaskStatus::Exported { message, .. } | TaskStatus::Stopped(message) => message,
        _ => unreachable!("scan returns an export or a stopped outcome"),
    })
}

fn run_scan_observed(
    settings: &StarRailSettings,
    cancel: yas::cancel::CancelToken,
    sample_limit: Option<usize>,
    observer: Option<hsr_scanner::scan_progress::ScanObserver>,
    status: Option<&Arc<Mutex<TaskStatus>>>,
) -> Result<TaskStatus, UiError> {
    if user_aborted(&cancel) {
        return Ok(TaskStatus::Stopped(UiText::new(
            "扫描已停止 · 未导出",
            "Scan stopped · no export",
        )));
    }
    let targets = ScanTargets {
        characters: settings.scan_characters,
        light_cones: settings.scan_light_cones,
        gear: settings.scan_relics_and_ornaments,
    };
    let mut config = scanner_config(settings, targets)?;
    config.scan_item_limit = sample_limit;
    ensure_output_dir(settings)?;
    if user_aborted(&cancel) {
        return Ok(TaskStatus::Stopped(UiText::new(
            "扫描已停止 · 未导出",
            "Scan stopped · no export",
        )));
    }
    let phase = |zh, en| {
        if !user_aborted(&cancel) {
            if let Some(status) = status {
                *status.lock().unwrap() = TaskStatus::Running(UiText::new(zh, en));
            }
        }
    };
    phase("正在准备 OCR 引擎", "Preparing OCR engine");
    ensure_ocr_runtime_with_status(status)?;
    phase("正在加载游戏数据", "Loading game data");
    let references = load_references().map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "无法加载星穹铁道游戏数据。请重新下载最新版本的程序后重试。",
                "Star Rail game data could not be loaded. Download the latest app build and retry.",
            ),
            error,
        )
    })?;
    if user_aborted(&cancel) {
        return Ok(TaskStatus::Stopped(UiText::new(
            "扫描已停止 · 未导出",
            "Scan stopped · no export",
        )));
    }
    let _controller_lease = HsrControllerLease::try_acquire().map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "另一个星穹铁道操作正在使用游戏控制器。请等待完成后重试。",
                "Another Star Rail operation is using the game controller. Wait for it to finish, then retry.",
            ),
            error,
        )
    })?;
    phase("正在连接游戏", "Connecting to the game");
    let result = match HsrScanner::live_with_cancel(references.clone(), config, cancel.clone())
        .map(|scanner| match observer {
            Some(observer) => scanner.with_observer(observer),
            None => scanner,
        })
        .and_then(HsrScanner::scan)
    {
        Ok(result) => result,
        Err(error)
            if user_aborted(&cancel)
                && matches!(error.code(), "HSR-SCAN-CANCELLED" | "HSR-DEVICE-CANCELLED") =>
        {
            return Ok(TaskStatus::Stopped(UiText::new(
                "扫描已停止 · 未导出",
                "Scan stopped · no export",
            )))
        },
        Err(error) => {
            return Err(hsr_ui_error(
                UiText::new(
                    "星穹铁道扫描未能完成。",
                    "The Star Rail scan could not finish.",
                ),
                error,
            ));
        },
    };
    if cancel.is_cancelled() && !user_aborted(&cancel) {
        return Ok(TaskStatus::Stopped(UiText::new(
            "扫描已停止 · 未导出",
            "Scan stopped · no export",
        )));
    }
    finish_scan_export(
        settings,
        &result,
        &references,
        user_aborted(&cancel),
        status,
    )
}

fn build_manager_preview(
    plan: ManagerPlan,
    settings_identity: String,
    recovered: bool,
) -> Result<ManagerPreview, UiError> {
    let exact_json = serde_json::to_string_pretty(&plan).map_err(|error| {
        UiError::from_error(
            UiText::new(
                "无法显示精确预览；不会执行任何游戏操作。",
                "The exact preview could not be displayed; no game action was performed.",
            ),
            error,
        )
    })?;
    Ok(ManagerPreview {
        plan,
        exact_json,
        recovered,
        settings_identity,
    })
}

/// Load and render the journal's exact original plan without touching the
/// game. Keeping this adapter pure makes restart recovery testable at the UI
/// boundary before a controller or OCR device is created.
pub fn load_recovery_preview<S: ManagerJournalStore>(
    envelope: &ManagerInstructionsEnvelope,
    journal: &mut S,
    settings_identity: String,
) -> Result<Option<ManagerPreview>, UiError> {
    load_manager_recovery_plan(envelope, journal)
        .map_err(|error| {
            hsr_ui_error(
                UiText::new(
                    "恢复日志无法安全载入原始预览；不会执行任何游戏操作。",
                    "The recovery journal could not safely load its original preview; no game action was performed.",
                ),
                error,
            )
        })?
        .map(|plan| build_manager_preview(plan, settings_identity, true))
        .transpose()
}

fn manager_scan_config(settings: &StarRailSettings) -> Result<ScanConfig, UiError> {
    let mut config = scanner_config(
        settings,
        ScanTargets {
            characters: false,
            light_cones: false,
            gear: true,
        },
    )?;
    // Sample caps belong to export scans. Manager matching needs the complete
    // inventory and must not inherit a user's diagnostic sample preference.
    config.max_gear = 0;
    config.save_on_cancel = false;
    // TODO(hsr-manager-set-filter): narrow traversal to requested Relic sets
    // once game-side set filtering is implemented and coverage can be proven
    // within that filter. Until then matching requires a complete inventory.
    config.stop_on_failure = true;
    Ok(config)
}

pub fn spawn_manager_server(
    settings: &StarRailSettings,
    status: Arc<Mutex<TaskStatus>>,
    progress: Arc<Mutex<super::task_progress::TaskProgress>>,
    job: Arc<Mutex<crate::hsr_server::JobState>>,
) -> TaskHandle {
    let settings = settings.clone();
    let status_worker = status.clone();
    let progress_http = progress.clone();
    let status_http = status.clone();
    *job.lock().unwrap() = Default::default();
    progress.lock().unwrap().steps.clear();
    worker::spawn_cancellable_task(
        TaskKind::Manager,
        LogSource::Manager,
        status,
        UiText::new("正在启动服务器", "Starting server"),
        UiText::new("正在停止服务器", "Stopping server"),
        move |cancel| {
            let server =
                tiny_http::Server::http(("127.0.0.1", settings.manager_port)).map_err(|error| {
                    UiError::from_message(
                        UiText::new(
                            "无法启动服务器。端口可能已被占用，请更换端口。",
                            "Server could not start. The port may be in use; choose another port.",
                        ),
                        error.to_string(),
                    )
                })?;
            *status_worker.lock().unwrap() =
                TaskStatus::Running(UiText::new("等待网站请求", "Waiting for a website request"));
            #[cfg(all(feature = "capture", target_os = "windows"))]
            let continuous_session = settings.continuous_capture.then(|| {
                super::continuous_capture::Session::star_rail(if settings.output_dir.is_empty() {
                    genshin_scanner::cli::exe_dir()
                } else {
                    PathBuf::from(&settings.output_dir)
                })
            });
            #[cfg(all(feature = "capture", target_os = "windows"))]
            let continuous = continuous_session
                .as_ref()
                .map(|session| session.latest.clone());
            #[cfg(not(all(feature = "capture", target_os = "windows")))]
            let continuous = None;
            crate::hsr_server::serve_continuous(server, continuous, cancel, job, settings.dump_job_data,
                move |request, job_cancel| {
                    progress.lock().unwrap().steps.clear();
                    *status_worker.lock().unwrap() = TaskStatus::Running(UiText::new("正在启动任务", "Starting task"));
                    let result = match request {
                        crate::hsr_server::Job::Manage(envelope) => run_manager_request(
                            &settings, &envelope, job_cancel.clone(), status_worker.clone(), progress.clone()),
                        crate::hsr_server::Job::Scan(targets) => {
                            let mut scan_settings = settings.clone();
                            scan_settings.scan_characters = targets.characters;
                            scan_settings.scan_light_cones = targets.light_cones;
                            scan_settings.scan_relics_and_ornaments = targets.relics;
                            run_scan_task(&scan_settings, job_cancel.clone(), status_worker.clone(), progress.clone())
                                .and_then(|outcome| match outcome {
                                    TaskStatus::Exported { path, partial, message } => {
                                        let data = fs::read_to_string(&path).map_err(|error| UiError::from_error(
                                            UiText::new("无法读取扫描结果", "Could not read scan results"), error))?;
                                        let export: serde_json::Value = serde_json::from_str(&data).map_err(|error| UiError::from_error(
                                            UiText::new("无法读取扫描结果", "Could not read scan results"), error))?;
                                        *status_worker.lock().unwrap() = TaskStatus::Exported { message, path, partial };
                                        Ok(serde_json::json!({"kind":"scan", "export":export, "partial":partial}))
                                    },
                                    TaskStatus::Stopped(message) => {
                                        *status_worker.lock().unwrap() = TaskStatus::Stopped(message.clone());
                                        Err(UiError::from_message(message, "scan cancelled"))
                                    },
                                    _ => unreachable!("scan produces export or stopped"),
                                })
                        },
                    };
                    progress.lock().unwrap().interrupt_unfinished();
                    result.map_err(|error| {
                        let body = serde_json::json!({
                            "zh":error.hint_text(super::state::Lang::Zh),
                            "en":error.hint_text(super::state::Lang::En),
                            "details":error.copy_text(super::state::Lang::En),
                        });
                        let mut status = status_worker.lock().unwrap();
                        if !matches!(*status, TaskStatus::Stopped(_)) { *status = TaskStatus::Failed(error); }
                        body
                    })
                },
                move || {
                    let track = progress_http.lock().unwrap();
                    let status = status_http.lock().unwrap();
                    let message = match &*status {
                        TaskStatus::Running(text) | TaskStatus::Completed(text) | TaskStatus::Stopped(text) =>
                            serde_json::json!({"zh":text.text(super::state::Lang::Zh), "en":text.text(super::state::Lang::En)}),
                        TaskStatus::Failed(error) => serde_json::json!({"zh":error.hint_text(super::state::Lang::Zh), "en":error.hint_text(super::state::Lang::En)}),
                        _ => serde_json::Value::Null,
                    };
                    serde_json::json!({"message":message,"steps":track.steps.iter().map(|step|
                        serde_json::json!({"key":step.key,"zh":step.zh,"en":step.en,"completed":step.completed,"total":step.total,
                            "showCount":step.show_count, "state":match step.state {
                                super::task_progress::StepState::Pending => "pending",
                                super::task_progress::StepState::Running => "running",
                                super::task_progress::StepState::Complete => "complete",
                                super::task_progress::StepState::Interrupted => "interrupted",
                            }})).collect::<Vec<_>>()})
                },
            ).map_err(|error| UiError::from_anyhow(UiText::new("服务器已停止", "Server stopped"), &error))?;
            Ok(stopped(TaskKind::Manager))
        },
    )
}

fn run_manager_request(
    settings: &StarRailSettings,
    envelope: &ManagerInstructionsEnvelope,
    cancel: yas::cancel::CancelToken,
    status: Arc<Mutex<TaskStatus>>,
    progress: Arc<Mutex<super::task_progress::TaskProgress>>,
) -> Result<serde_json::Value, UiError> {
    *status.lock().unwrap() = TaskStatus::Running(UiText::new(
        format!("管理请求 · {} 项操作", envelope.instructions.len()),
        format!(
            "Manage request · {} operations",
            envelope.instructions.len()
        ),
    ));
    let references = load_references().map_err(|error| {
        hsr_ui_error(
            UiText::new("无法加载游戏数据", "Could not load game data"),
            error,
        )
    })?;
    validate_manager_envelope_reference(envelope, &references).map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "管理请求与游戏数据不一致",
                "Request does not match game data",
            ),
            error,
        )
    })?;
    ensure_ocr_runtime_with_status(Some(&status))?;
    if user_aborted(&cancel) {
        return Err(UiError::from_message(
            UiText::new("操作已停止", "Operation stopped"),
            "cancelled before game control",
        ));
    }
    let controller_lease = HsrControllerLease::try_acquire().map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "游戏正在被其他任务使用",
                "Another task is controlling the game",
            ),
            error,
        )
    })?;
    let journal_path = if settings.manager_journal_path.trim().is_empty() {
        settings.export_directory().join("star_rail_manager.jsonl")
    } else {
        PathBuf::from(settings.manager_journal_path.trim())
    };
    let mut journal = AppendOnlyJsonJournalStore::new(&journal_path)
        .try_acquire_apply_lease_with_controller(controller_lease)
        .map_err(|error| {
            hsr_ui_error(
                UiText::new("无法打开恢复记录", "Could not open recovery journal"),
                error,
            )
        })?;
    let recovered = load_manager_recovery_plan(envelope, &mut journal)
        .map_err(|error| {
            hsr_ui_error(
                UiText::new("无法恢复原任务", "Could not recover the original task"),
                error,
            )
        })?
        .is_some();
    let mut scanner =
        HsrScanner::live_with_cancel(references, manager_scan_config(settings)?, cancel.clone())
            .map_err(|error| {
                hsr_ui_error(
                    UiText::new("无法连接游戏", "Could not connect to the game"),
                    error,
                )
            })?
            .with_observer(manager_scan_observer(progress.clone()));
    let mut inventory_step = if recovered {
        super::task_progress::Step::new("gear", "恢复中断任务", "Recover interrupted task")
    } else {
        super::task_progress::Step::new("gear", "重扫遗器库存", "Rescan Relic inventory")
    };
    inventory_step.show_count = !recovered;
    progress.lock().unwrap().steps = vec![inventory_step];
    *status.lock().unwrap() = TaskStatus::Running(if recovered {
        UiText::new("正在恢复中断任务", "Recovering interrupted task")
    } else {
        UiText::new("正在重扫遗器库存", "Rescanning Relic inventory")
    });
    let mut journal = super::hsr_manager_progress::TrackedJournal {
        inner: journal,
        progress: progress.clone(),
        status: status.clone(),
        cancel,
    };
    let (plan, report) = apply_manager_request(
        envelope,
        &mut scanner,
        &mut journal,
        |scanner| scanner.scan_manager_inventory(),
        |plan| {
            let mut track = progress.lock().unwrap();
            if let Some(step) = track.steps.iter_mut().find(|s| s.key == "gear") {
                step.state = super::task_progress::StepState::Complete;
            }
            super::hsr_manager_progress::plan_steps(&mut track, plan);
            Ok(())
        },
    )
    .map_err(|error| {
        hsr_ui_error(
            UiText::new(
                "操作未完成，恢复记录已保留",
                "Operation did not finish; recovery journal retained",
            ),
            error,
        )
    })?;
    let entries = journal
        .load()
        .map_err(|error| {
            UiError::from_message(
                UiText::new("无法读取操作结果", "Could not read operation results"),
                error,
            )
        })?
        .map(|saved| saved.entries)
        .unwrap_or_default();
    let blocked = plan
        .entries
        .iter()
        .filter(|entry| entry.changes.is_empty())
        .count();
    let message = UiText::new(
        format!(
            "已验证 {} 项 · 跳过 {} 项 · 待复核 {} 项",
            report.verified_actions, blocked, report.needs_review_actions
        ),
        format!(
            "{} verified · {} skipped · {} need review",
            report.verified_actions, blocked, report.needs_review_actions
        ),
    );
    if report.needs_review_actions > 0 {
        *status.lock().unwrap() = TaskStatus::Failed(UiError::from_message(
            UiText::new(
                "部分操作需要人工复核，请保留恢复记录",
                "Some operations need review; keep the recovery journal",
            ),
            message.text(super::state::Lang::En),
        ));
    } else {
        // Release the locked journal before archiving (Windows cannot rename an open locked file).
        drop(journal);
        archive_completed_manager_journal(&journal_path)?;
        *status.lock().unwrap() = TaskStatus::Completed(message);
    }
    Ok(serde_json::json!({
        "kind":"manage", "verified":report.verified_actions, "needsReview":report.needs_review_actions,
        "total":report.total_actions, "skipped":blocked, "entries":entries,
        "instructions":plan.entries,
    }))
}
#[cfg(test)]
mod feedback_tests {
    use super::*;
    #[test]
    fn manager_scan_cannot_inherit_partial_export_or_sample_preferences() {
        let settings = StarRailSettings {
            max_gear: 12,
            ..Default::default()
        };
        let config = manager_scan_config(&settings).unwrap();
        assert!(config.stop_on_failure);
        assert!(!config.save_on_cancel);
        assert_eq!(config.max_gear, 0);
        assert!(!config.targets.characters);
        assert!(!config.targets.light_cones);
    }
    #[test]
    fn actionable_backend_hint_is_primary_and_context_is_retained() {
        let error = HsrError::new(
            "HSR-TEST",
            hsr_scanner::localization::LocalizedText::new(
                "请切回游戏。",
                "Switch back to the game.",
            ),
            "FOCUS_LOST_MARKER",
        );
        let failure = hsr_ui_error(UiText::new("扫描未完成", "Scan did not finish"), error);
        assert_eq!(
            failure.hint_text(super::super::state::Lang::En),
            "Switch back to the game."
        );
        assert_eq!(
            failure.hint_text(super::super::state::Lang::Zh),
            "请切回游戏。"
        );
        assert!(failure
            .copy_text(super::super::state::Lang::En)
            .contains("FOCUS_LOST_MARKER"));
    }
}
