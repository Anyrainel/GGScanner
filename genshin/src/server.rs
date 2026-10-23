//! HTTP server for the artifact manager with origin-based CORS security.
//!
//! Two-thread architecture:
//! - HTTP thread: handles all HTTP I/O (spawned)
//! - Execution thread: owns game controller, processes jobs (original thread)
//!
//! Communication: mpsc channel for job submission, Arc<Mutex<JobState>> for status.
//!
//! Security: Origin header checked against allowlist. Origins whose host
//! contains "ggartifact" and localhost origins are permitted. Requests with
//! disallowed origins are rejected with 403. Non-browser clients (no Origin
//! header) are allowed — CORS is a browser-enforced mechanism.
//!
//! 异步 HTTP 服务器。双线程架构：HTTP 线程处理请求，执行线程控制游戏。
//! 安全：通过 Origin 头限制仅允许主机名包含 ggartifact 的来源和 localhost 来源。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use anyhow::{anyhow, Result};
use tiny_http::{Header, Method, Response, Server};
use yas::{log_error, log_info, log_warn};

use crate::cli::{GoodUserConfig, ScanCoreConfig};
use crate::manager::models::*;
use crate::manager::orchestrator::ArtifactManager;
use crate::manager::orchestrator::ProgressFn;
use crate::scanner::common::game_controller::GenshinGameController;
use crate::scanner::common::models::{GoodArtifact, GoodCharacter, GoodExport, GoodWeapon};
use crate::scanner::common::scan_runner::{
    run_scan_phases, ScanFailurePolicy, ScanPhaseResult as PhaseResult, ScanRunOptions,
    ScanRunResult as ScanResult,
};

// ================================================================
// File logging: saves request bodies as JSON for replay/debugging
// ================================================================

/// Format a timestamp string from SystemTime (local time approximation via UNIX epoch offset).
fn timestamp_string() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let millis = dur.subsec_millis();
    let h = (secs / 3600) % 24;
    let m = (secs / 60) % 60;
    let s = secs % 60;
    format!("{:02}-{:02}-{:02}_{:03}", h, m, s, millis)
}

/// Save a request body as a timestamped JSON file in the log/ directory.
fn save_request(endpoint: &str, body: &str) {
    if let Err(error) = yas::utils::request_log::save_request_log(
        std::path::Path::new("log"),
        endpoint,
        body.as_bytes(),
    ) {
        log_error_with_diagnostic("无法保存请求日志。", "Could not save request log.", error);
    }
}

/// Save a job's produced data as one replayable GOOD file.
fn save_job_good_export(
    job_id: &str,
    kind: &str,
    characters: Option<Vec<GoodCharacter>>,
    weapons: Option<Vec<GoodWeapon>>,
    artifacts: Option<Vec<GoodArtifact>>,
    achievements: Option<Vec<u32>>,
) {
    let log_dir = std::path::PathBuf::from("log").join("job_data");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        let zh_hint = format!(
            "任务 {} 的数据无法保存，因为无法创建数据日志文件夹 {}。",
            job_id,
            log_dir.display()
        );
        let en_hint = format!(
            "Data for job {} could not be saved because its log directory could not be created: {}.",
            job_id,
            log_dir.display()
        );
        log_error_with_diagnostic(&zh_hint, &en_hint, e);
        return;
    }

    let export = GoodExport::new(characters, weapons, artifacts).with_achievements(achievements);
    let json = match serde_json::to_string_pretty(&export) {
        Ok(json) => json,
        Err(e) => {
            let zh_hint = format!("任务 {} 的数据无法转换为 GOOD JSON，因此未能保存。", job_id);
            let en_hint = format!(
                "Data for job {} could not be converted to GOOD JSON, so it was not saved.",
                job_id
            );
            log_error_with_diagnostic(&zh_hint, &en_hint, e);
            return;
        },
    };

    let filename = format!("{}_{}_{}.json", timestamp_string(), kind, job_id);
    let path = log_dir.join(&filename);
    if let Err(e) = std::fs::write(&path, json) {
        let zh_hint = format!("任务 {} 的数据文件无法保存到 {}。", job_id, path.display());
        let en_hint = format!(
            "The data file for job {} could not be saved to {}.",
            job_id,
            path.display()
        );
        log_error_with_diagnostic(&zh_hint, &en_hint, e);
    } else {
        log_info!(
            "[job {}] 任务数据已保存: {}",
            "[job {}] Job data saved: {}",
            job_id,
            path.display()
        );
    }
}

/// Job types that can be submitted to the execution thread.
enum JobRequest {
    Manage(LockManageRequest),
    Equip(EquipRequest),
    Scan(ScanRequest),
}

/// Abstraction over game interaction for testability.
pub trait ManageExecutor {
    /// Recheck task data even when the game/OCR executor is reused by the server.
    fn prepare_task(&mut self) -> Result<()> {
        Ok(())
    }

    fn execute(
        &mut self,
        request: LockManageRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> (ManageResult, Option<Vec<GoodArtifact>>);

    fn execute_equip(
        &mut self,
        request: EquipRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> ManageResult;

    fn execute_scan(
        &mut self,
        request: &ScanRequest,
        progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> anyhow::Result<ScanResult>;

    fn execute_scan_observed(
        &mut self,
        request: &ScanRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
        _phase_fn: Option<&crate::scanner::common::progress::ScanPhaseFn<'_>>,
    ) -> anyhow::Result<ScanResult> {
        self.execute_scan(request, progress_fn, cancel_token)
    }
}

/// Real executor: wraps a game controller and artifact manager.
pub struct GameExecutor {
    pub ctrl: GenshinGameController,
    pub manager: ArtifactManager,
    pub user_config: GoodUserConfig,
    pub scan_defaults: ScanCoreConfig,
}

impl ManageExecutor for GameExecutor {
    fn prepare_task(&mut self) -> Result<()> {
        let mappings = crate::scanner::common::mappings::MappingManager::new(
            &self.user_config.to_overrides(),
        )?;
        self.manager.replace_mappings(Arc::new(mappings));
        Ok(())
    }

    fn execute(
        &mut self,
        request: LockManageRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> (ManageResult, Option<Vec<GoodArtifact>>) {
        self.manager
            .execute(&mut self.ctrl, request, progress_fn, cancel_token)
    }

    fn execute_equip(
        &mut self,
        request: EquipRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> ManageResult {
        self.manager
            .execute_equip(&mut self.ctrl, request, progress_fn, cancel_token)
    }

    fn execute_scan(
        &mut self,
        request: &ScanRequest,
        progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
    ) -> anyhow::Result<ScanResult> {
        self.execute_scan_observed(request, progress_fn, cancel_token, None)
    }

    fn execute_scan_observed(
        &mut self,
        request: &ScanRequest,
        progress_fn: Option<&ProgressFn<'_>>,
        cancel_token: yas::cancel::CancelToken,
        phase_fn: Option<&crate::scanner::common::progress::ScanPhaseFn<'_>>,
    ) -> anyhow::Result<ScanResult> {
        let mut config = self.scan_defaults.clone();
        config.scan_characters = request.characters;
        config.scan_weapons = request.weapons;
        config.scan_artifacts = request.artifacts;
        config.scan_achievements = request.achievements;
        if let Some(limit) = request.artifact_limit {
            config.artifact_max_count = limit;
        }
        if request.artifact_mode == ArtifactScanMode::Recent {
            config.artifact_min_rarity = 5;
            config.artifact_keep_five_star_filter = true;
        }

        run_scan_phases(
            &mut self.ctrl,
            self.manager.mappings().clone(),
            self.manager.pools().clone(),
            &self.user_config,
            &config,
            progress_fn,
            None,
            phase_fn,
            cancel_token,
            ScanRunOptions {
                save_on_cancel: false,
                accept_cancelled_success: false,
                failure_policy: ScanFailurePolicy::ContinueOnError,
            },
        )
    }
}

/// Maximum request body size (5 MB).
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;

/// Generic scan data cache: stores the latest results for a given data type
/// along with the jobId that produced them.
///
/// `incomplete_job_id` records the most recent job that attempted to populate
/// this cache but failed to complete (aborted by user, errored, or stopped
/// before reaching this category). Queries matching that id return 503 so the
/// client can distinguish "scan was aborted" from "no such job".
struct ScanDataCache<T> {
    job_id: Option<String>,
    data: Option<Vec<T>>,
    incomplete_job_id: Option<String>,
}

impl<T> ScanDataCache<T> {
    fn empty() -> Self {
        Self {
            job_id: None,
            data: None,
            incomplete_job_id: None,
        }
    }

    fn set(&mut self, job_id: String, data: Vec<T>) {
        self.job_id = Some(job_id);
        self.data = Some(data);
        self.incomplete_job_id = None;
    }

    fn mark_incomplete(&mut self, job_id: String) {
        self.incomplete_job_id = Some(job_id);
    }

    fn invalidate(&mut self) {
        self.data = None;
        self.job_id = None;
        self.incomplete_job_id = None;
    }
}

#[derive(Clone, Copy)]
enum ScanCategory {
    Characters,
    Weapons,
    Artifacts,
    Achievements,
}

impl ScanCategory {
    fn id(self) -> &'static str {
        match self {
            Self::Characters => "characters",
            Self::Weapons => "weapons",
            Self::Artifacts => "artifacts",
            Self::Achievements => "achievements",
        }
    }

    fn failed_hints(self) -> (&'static str, &'static str) {
        match self {
            Self::Characters => (
                "角色扫描遇到错误，因此未能完成。下方包含可复制的完整错误。",
                "The character scan encountered an error and could not finish. The complete copyable error is included below.",
            ),
            Self::Weapons => (
                "武器扫描遇到错误，因此未能完成。下方包含可复制的完整错误。",
                "The weapon scan encountered an error and could not finish. The complete copyable error is included below.",
            ),
            Self::Artifacts => (
                "圣遗物扫描遇到错误，因此未能完成。下方包含可复制的完整错误。",
                "The artifact scan encountered an error and could not finish. The complete copyable error is included below.",
            ),
            Self::Achievements => (
                "成就扫描遇到错误，因此未能完成。下方包含可复制的完整错误。",
                "The achievement scan encountered an error and could not finish. The complete copyable error is included below.",
            ),
        }
    }

    fn stopped_hints(self) -> (&'static str, &'static str) {
        match self {
            Self::Characters => (
                "角色扫描在完成前被停止，因此没有发布不完整的数据。",
                "The character scan was stopped before it finished, so incomplete data was not published.",
            ),
            Self::Weapons => (
                "武器扫描在完成前被停止，因此没有发布不完整的数据。",
                "The weapon scan was stopped before it finished, so incomplete data was not published.",
            ),
            Self::Artifacts => (
                "圣遗物扫描在完成前被停止，因此没有发布不完整的数据。",
                "The artifact scan was stopped before it finished, so incomplete data was not published.",
            ),
            Self::Achievements => (
                "成就扫描在完成前被停止，因此没有发布不完整的数据。",
                "The achievement scan was stopped before it finished, so incomplete data was not published.",
            ),
        }
    }
}

/// Finalize one scan category without duplicating cache, count, and message
/// semantics across characters, weapons, and artifacts.
fn finalize_scan_phase<T: Clone>(
    phase: PhaseResult<T>,
    category: ScanCategory,
    cache: &Arc<Mutex<ScanDataCache<T>>>,
    job_id: &str,
    dump_job_data: bool,
    results: &mut Vec<InstructionResult>,
    phases_complete: &mut usize,
    phases_incomplete: &mut usize,
) -> Option<Vec<T>> {
    match phase {
        PhaseResult::Complete(data) => {
            let dump_data = dump_job_data.then(|| data.clone());
            cache.lock().unwrap().set(job_id.to_string(), data);
            *phases_complete += 1;
            results.push(InstructionResult::outcome(
                category.id(),
                InstructionStatus::Success,
            ));
            dump_data
        },
        PhaseResult::Failed(source) => {
            cache.lock().unwrap().mark_incomplete(job_id.to_string());
            *phases_incomplete += 1;
            let (hint_zh, hint_en) = category.failed_hints();
            results.push(InstructionResult::failure(
                category.id(),
                InstructionStatus::UiError,
                hint_zh,
                hint_en,
                Some(&source),
            ));
            None
        },
        PhaseResult::Incomplete => {
            cache.lock().unwrap().mark_incomplete(job_id.to_string());
            *phases_incomplete += 1;
            let (hint_zh, hint_en) = category.stopped_hints();
            results.push(InstructionResult::failure(
                category.id(),
                InstructionStatus::Aborted,
                hint_zh,
                hint_en,
                None,
            ));
            None
        },
        PhaseResult::NotAttempted => None,
    }
}

/// Allowed production/staging origin host fragment.
const ALLOWED_GGARTIFACT_HOST_FRAGMENT: &str = "ggartifact";

/// Check if an origin is allowed.
///
/// Allows:
/// - `http(s)://*ggartifact*` hosts (production/staging)
/// - `http(s)://localhost[:port]` (development)
/// - loopback IP origins such as `http(s)://127.0.0.1[:port]`
///   and `http(s)://[::1][:port]` (development)
fn is_origin_allowed(origin: &str) -> bool {
    let origin = origin.trim_end_matches('/');
    let Some((_scheme, host)) = parse_http_origin(origin) else {
        return false;
    };

    is_ggartifact_host(host) || is_loopback_host(host)
}

fn parse_http_origin(origin: &str) -> Option<(&str, &str)> {
    let Some((scheme, authority)) = origin.split_once("://") else {
        return None;
    };
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if authority.is_empty()
        || authority.contains('/')
        || authority.contains('?')
        || authority.contains('#')
    {
        return None;
    }

    let host = parse_origin_host(authority)?;
    Some((scheme, host))
}

fn is_ggartifact_host(host: &str) -> bool {
    host.to_ascii_lowercase()
        .contains(ALLOWED_GGARTIFACT_HOST_FRAGMENT)
}

fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

fn parse_origin_host(authority: &str) -> Option<&str> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, suffix) = rest.split_once(']')?;
        if host.is_empty() || !is_valid_port_suffix(suffix) {
            return None;
        }
        return Some(host);
    }

    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if host.is_empty() || host.contains(':') {
        return None;
    }
    if let Some(port) = port {
        if port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
    }
    Some(host)
}

fn is_valid_port_suffix(suffix: &str) -> bool {
    if suffix.is_empty() {
        return true;
    }
    let Some(port) = suffix.strip_prefix(':') else {
        return false;
    };
    !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
}

/// Extract the Origin header from a request.
fn get_origin(request: &tiny_http::Request) -> Option<String> {
    for header in request.headers() {
        if header
            .field
            .as_str()
            .as_str()
            .eq_ignore_ascii_case("origin")
        {
            return Some(header.value.as_str().to_string());
        }
    }
    None
}

/// Check if the game window is currently alive.
///
/// Called from the HTTP thread — does not need the game controller.
/// On Windows uses Win32 EnumWindows; on Linux enumerates X11 windows
/// (works for X11 sessions and XWayland games alike).
///
/// 检查游戏窗口是否存在。从 HTTP 线程调用。
#[cfg(target_os = "windows")]
fn is_game_window_alive() -> bool {
    let window_names = ["\u{539F}\u{795E}", "Genshin Impact"]; // 原神
    let handles = yas::utils::iterate_window();
    for hwnd in &handles {
        if let Some(title) = yas::utils::get_window_title(*hwnd) {
            let trimmed = title.trim();
            if window_names.iter().any(|n| trimmed == *n) {
                return true;
            }
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn is_game_window_alive() -> bool {
    let window_names = ["\u{539F}\u{795E}", "Genshin Impact"]; // 原神
    match yas::utils::enumerate_windows() {
        Ok(windows) => windows
            .iter()
            .any(|w| window_names.iter().any(|n| w.title == *n)),
        Err(_) => false,
    }
}

#[cfg(not(any(target_os = "windows", target_os = "linux")))]
fn is_game_window_alive() -> bool {
    true
}

/// CORS headers for an allowed origin.
fn cors_headers(origin: &str) -> Vec<Header> {
    vec![
        Header::from_bytes("Access-Control-Allow-Origin", origin).unwrap(),
        Header::from_bytes("Access-Control-Allow-Methods", "GET, POST, OPTIONS").unwrap(),
        Header::from_bytes("Access-Control-Allow-Headers", "Content-Type").unwrap(),
        Header::from_bytes("Access-Control-Allow-Private-Network", "true").unwrap(),
        Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap(),
    ]
}

/// Send a JSON response with optional CORS headers.
///
/// `origin`: the validated origin to echo back, or None for non-browser clients.
fn respond_json(request: tiny_http::Request, status: u16, json: &str, origin: Option<&str>) {
    let mut resp = Response::from_string(json).with_status_code(status);
    if let Some(o) = origin {
        for header in cors_headers(o) {
            resp.add_header(header);
        }
    } else {
        resp.add_header(
            Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap(),
        );
    }
    if let Err(e) = request.respond(resp) {
        log_error!("响应失败: {}", "Response failed: {}", e);
    }
}

/// Select one already-authored message using the configured application language.
///
/// This intentionally does not use the legacy `"Chinese / English"` parser: an
/// inner OS/library diagnostic may itself contain `" / "` and must remain exact.
fn configured_text<'a>(zh: &'a str, en: &'a str) -> &'a str {
    if yas::lang::is_en() {
        en
    } else {
        zh
    }
}

/// Serialize the stable HTTP error schema without interpolating into JSON text.
fn error_json(message: &str) -> String {
    serde_json::json!({ "error": message }).to_string()
}

fn respond_error_message(
    request: tiny_http::Request,
    status: u16,
    message: &str,
    origin: Option<&str>,
) {
    let json = error_json(message);
    respond_json(request, status, &json, origin);
}

/// Send a localized error response with the stable `{ "error": string }` schema.
fn respond_error(
    request: tiny_http::Request,
    status: u16,
    zh_hint: &str,
    en_hint: &str,
    origin: Option<&str>,
) {
    respond_error_message(request, status, configured_text(zh_hint, en_hint), origin);
}

fn error_message_with_diagnostic(
    zh_hint: &str,
    en_hint: &str,
    diagnostic: impl std::fmt::Display,
) -> String {
    let details_label = configured_text("完整错误详情", "Full error details");
    format!(
        "{}\n\n{}:\n{:#}",
        configured_text(zh_hint, en_hint),
        details_label,
        diagnostic
    )
}

fn log_error_with_diagnostic(zh_hint: &str, en_hint: &str, diagnostic: impl std::fmt::Display) {
    let message = error_message_with_diagnostic(zh_hint, en_hint, diagnostic);
    log::error!(target: concat!(module_path!(), "::localized"), "{}", message);
}

/// Send a localized readable hint followed by an untouched inner diagnostic.
fn respond_error_with_diagnostic(
    request: tiny_http::Request,
    status: u16,
    zh_hint: &str,
    en_hint: &str,
    diagnostic: impl std::fmt::Display,
    origin: Option<&str>,
) {
    let message = error_message_with_diagnostic(zh_hint, en_hint, diagnostic);
    respond_error_message(request, status, &message, origin);
}

/// Serialization failures are internal server errors at every endpoint. Keep
/// the status in one helper so a successful-data route can never accidentally
/// return an error body with HTTP 200.
fn respond_serialization_error(
    request: tiny_http::Request,
    zh_hint: &str,
    en_hint: &str,
    diagnostic: impl std::fmt::Display,
    origin: Option<&str>,
) {
    respond_error_with_diagnostic(request, 500, zh_hint, en_hint, diagnostic, origin);
}

fn contextualize_server_bind_error(
    port: u16,
    source: Box<dyn std::error::Error + Send + Sync + 'static>,
) -> anyhow::Error {
    let diagnostic = source.to_string();
    let address_in_use = source
        .downcast_ref::<std::io::Error>()
        .is_some_and(|error| error.kind() == std::io::ErrorKind::AddrInUse)
        || diagnostic.contains("Address already in use")
        || diagnostic.contains("address is already in use")
        || diagnostic.contains("AddrInUse")
        || diagnostic.contains("10048");
    let hint = if address_in_use {
        if yas::lang::is_en() {
            format!("Port {port} is already in use. Choose a different port.")
        } else {
            format!("端口 {port} 已被占用。请选择其他端口。")
        }
    } else if yas::lang::is_en() {
        format!("The HTTP server could not start on port {port}.")
    } else {
        format!("HTTP 服务器无法在端口 {port} 上启动。")
    };

    anyhow::Error::from_boxed(source).context(hint)
}

/// Run the artifact manager HTTP server with async job execution.
///
/// This blocks the current thread (which becomes the execution thread).
/// A separate HTTP thread is spawned to handle requests.
///
/// 运行异步圣遗物管理 HTTP 服务器。
/// 当前线程成为执行线程，另起 HTTP 线程处理请求。
pub fn run_server<F>(
    port: u16,
    init_executor: F,
    enabled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    dump_job_data: bool,
    status_fn: Option<Arc<dyn Fn(&str) + Send + Sync>>,
) -> Result<()>
where
    F: FnMut() -> anyhow::Result<Box<dyn ManageExecutor>>,
{
    run_server_observed(
        port,
        init_executor,
        enabled,
        shutdown,
        dump_job_data,
        status_fn,
        None,
    )
}

pub fn run_server_observed<F>(
    port: u16,
    init_executor: F,
    enabled: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    dump_job_data: bool,
    status_fn: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    native_job: Option<Arc<Mutex<JobState>>>,
) -> Result<()>
where
    F: FnMut() -> anyhow::Result<Box<dyn ManageExecutor>>,
{
    let addr = format!("127.0.0.1:{}", port);
    let server = Server::http(&addr).map_err(|e| contextualize_server_bind_error(port, e))?;
    let server = Arc::new(server);
    if let Some(f) = &status_fn {
        f(&format!(
            "等待网页请求 · 端口 {port} / Waiting for a web request · port {port}"
        ));
    }

    log_info!(
        "HTTP服务器已启动：http://{}",
        "HTTP server running at http://{}",
        addr
    );

    // Shared state for async job tracking
    let job_state = native_job.unwrap_or_else(|| Arc::new(Mutex::new(JobState::idle())));
    *job_state.lock().unwrap() = JobState::idle();

    // Per-type data caches (populated by scan/manage jobs).
    let character_cache: Arc<Mutex<ScanDataCache<GoodCharacter>>> =
        Arc::new(Mutex::new(ScanDataCache::empty()));
    let weapon_cache: Arc<Mutex<ScanDataCache<GoodWeapon>>> =
        Arc::new(Mutex::new(ScanDataCache::empty()));
    let artifact_cache: Arc<Mutex<ScanDataCache<GoodArtifact>>> =
        Arc::new(Mutex::new(ScanDataCache::empty()));
    let achievement_cache: Arc<Mutex<ScanDataCache<u32>>> =
        Arc::new(Mutex::new(ScanDataCache::empty()));

    // Channel for submitting jobs from HTTP thread to execution thread
    let (job_tx, job_rx) = mpsc::channel::<(String, JobRequest)>();

    // Clone shared refs for the HTTP thread
    let http_state = job_state.clone();
    let http_enabled = enabled.clone();
    let http_character_cache = character_cache.clone();
    let http_weapon_cache = weapon_cache.clone();
    let http_artifact_cache = artifact_cache.clone();
    let http_achievement_cache = achievement_cache.clone();

    // Clone job_tx for the HTTP thread before moving the original
    let http_job_tx = job_tx.clone();

    // Spawn shutdown watcher: polls the flag and calls server.unblock()
    let shutdown_server = server.clone();
    let shutdown_flag = shutdown.clone();
    let shutdown_watcher = std::thread::spawn(move || {
        while !shutdown_flag.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
        log_info!(
            "收到关闭信号，停止HTTP服务器",
            "Shutdown signal received, stopping HTTP server"
        );
        shutdown_server.unblock();
        // Drop the original sender so job_rx.recv() unblocks once the HTTP thread also exits
        drop(job_tx);
    });

    // Spawn HTTP handler thread
    let http_server = server.clone();
    let http_thread = std::thread::spawn(move || {
        for request in http_server.incoming_requests() {
            let method = request.method().clone();
            let url = request.url().to_string();

            // --- Origin validation ---
            // Browser requests carry Origin; non-browser clients (curl) don't.
            // If Origin is present but not in the allowlist, reject with 403.
            // If absent, allow (CORS is a browser-enforced mechanism).
            let origin = get_origin(&request);
            let cors_origin: Option<String> = match &origin {
                Some(o) if is_origin_allowed(o) => Some(o.trim_end_matches('/').to_string()),
                Some(o) => {
                    log_warn!("拒绝非法来源: {}", "Rejected disallowed origin: {}", o);
                    respond_error(
                        request,
                        403,
                        "不允许该请求来源。",
                        "The request origin is not allowed.",
                        None,
                    );
                    continue;
                },
                None => None,
            };
            let cors_ref = cors_origin.as_deref();

            // CORS preflight (always respond for allowed origins)
            if method == Method::Options {
                let mut resp = Response::empty(204);
                if let Some(o) = cors_ref {
                    for header in cors_headers(o) {
                        resp.add_header(header);
                    }
                }
                if let Err(e) = request.respond(resp) {
                    log_warn!(
                        "CORS preflight 响应失败: {}",
                        "CORS preflight response failed: {}",
                        e
                    );
                }
                continue;
            }

            match (method, url.as_str()) {
                (Method::Post, "/manage") => {
                    handle_manage(
                        request,
                        &http_enabled,
                        &http_state,
                        &http_job_tx,
                        cors_ref,
                        dump_job_data,
                    );
                },

                (Method::Post, "/equip") => {
                    handle_equip(
                        request,
                        &http_enabled,
                        &http_state,
                        &http_job_tx,
                        cors_ref,
                        dump_job_data,
                    );
                },

                (Method::Post, "/scan") => {
                    handle_scan(
                        request,
                        &http_enabled,
                        &http_state,
                        &http_job_tx,
                        cors_ref,
                        dump_job_data,
                    );
                },

                // Lightweight poll — no result payload.
                // Returns state + jobId + progress (running) or summary (completed).
                (Method::Get, "/status") => {
                    let state = http_state.lock().unwrap();
                    let json = state.status_json();
                    drop(state);
                    respond_json(request, 200, &json, cors_ref);
                },

                // Full result — requires jobId query param, idempotent.
                (Method::Get, url) if url.starts_with("/result") => {
                    // Parse jobId from query string: /result?jobId=xxx
                    let query_job_id = url
                        .split('?')
                        .nth(1)
                        .and_then(|qs| qs.split('&').find(|p| p.starts_with("jobId=")))
                        .map(|p| &p[6..]);

                    match query_job_id {
                        None | Some("") => {
                            respond_error(
                                request,
                                400,
                                "缺少必需的查询参数 jobId。",
                                "Missing required query parameter: jobId.",
                                cors_ref,
                            );
                        },
                        Some(requested_id) => {
                            let state = http_state.lock().unwrap();
                            match state.state {
                                JobPhase::Completed => {
                                    let actual_id = state.job_id.as_deref().unwrap_or("");
                                    if actual_id != requested_id {
                                        drop(state);
                                        respond_error(
                                            request,
                                            404,
                                            "找不到该任务。",
                                            "Job not found.",
                                            cors_ref,
                                        );
                                    } else if let Some(ref result) = state.result {
                                        match serde_json::to_string(result) {
                                            Ok(json) => {
                                                drop(state);
                                                respond_json(request, 200, &json, cors_ref);
                                            },
                                            Err(e) => {
                                                drop(state);
                                                respond_serialization_error(
                                                    request,
                                                    "无法序列化任务结果。",
                                                    "The job result could not be serialized.",
                                                    e,
                                                    cors_ref,
                                                );
                                            },
                                        }
                                    } else {
                                        drop(state);
                                        respond_error(
                                            request,
                                            500,
                                            "任务已完成，但结果数据缺失。",
                                            "The job completed, but its result data is missing.",
                                            cors_ref,
                                        );
                                    }
                                },
                                JobPhase::Running => {
                                    let actual_id = state.job_id.as_deref().unwrap_or("");
                                    if actual_id != requested_id {
                                        drop(state);
                                        respond_error(
                                            request,
                                            404,
                                            "找不到该任务。",
                                            "Job not found.",
                                            cors_ref,
                                        );
                                    } else {
                                        drop(state);
                                        respond_error(
                                            request,
                                            409,
                                            "任务仍在运行。请继续轮询 GET /status。",
                                            "The job is still running. Continue polling GET /status.",
                                            cors_ref,
                                        );
                                    }
                                },
                                JobPhase::Idle => {
                                    drop(state);
                                    respond_error(
                                        request,
                                        404,
                                        "找不到该任务。",
                                        "Job not found.",
                                        cors_ref,
                                    );
                                },
                            }
                        },
                    }
                },

                // Health check — includes game window liveness.
                (Method::Get, "/health") => {
                    let is_enabled = http_enabled.load(Ordering::Relaxed);
                    let state = http_state.lock().unwrap();
                    let is_busy = state.state == JobPhase::Running;
                    drop(state);
                    let game_alive = is_game_window_alive();
                    let json = format!(
                        r#"{{"status":"ok","enabled":{},"busy":{},"gameAlive":{}}}"#,
                        is_enabled, is_busy, game_alive
                    );
                    respond_json(request, 200, &json, cors_ref);
                },

                // GET /characters?jobId=xxx
                (Method::Get, url) if url.starts_with("/characters") => {
                    serve_cache(request, url, &http_character_cache, "characters", cors_ref);
                },

                // GET /weapons?jobId=xxx
                (Method::Get, url) if url.starts_with("/weapons") => {
                    serve_cache(request, url, &http_weapon_cache, "weapons", cors_ref);
                },

                // GET /artifacts?jobId=xxx (jobId optional for backwards compat)
                (Method::Get, url) if url.starts_with("/artifacts") => {
                    serve_artifact_cache(request, url, &http_artifact_cache, cors_ref);
                },

                (Method::Get, url) if url.starts_with("/achievements") => {
                    serve_cache(
                        request,
                        url,
                        &http_achievement_cache,
                        "achievements",
                        cors_ref,
                    );
                },

                _ => {
                    respond_error(
                        request,
                        404,
                        "找不到请求的端点。",
                        "The requested endpoint was not found.",
                        cors_ref,
                    );
                },
            }
        }
    });

    // Block on channel — zero CPU when idle, wakes instantly on job arrival.
    // This thread owns ctrl (which is !Send) so it must be the original thread.
    // Game controller + manager are created lazily on first job to avoid
    // focusing the game window at server startup.
    let mut executor: Option<Box<dyn ManageExecutor>> = None;
    let mut init_executor = init_executor;

    while let Ok((job_id, request)) = job_rx.recv() {
        if shutdown.load(Ordering::Relaxed) {
            log_info!(
                "[job {}] 服务器关闭中，跳过",
                "[job {}] Server shutting down, skipping job",
                job_id
            );
            break;
        }
        log_info!(
            "[job {}] 收到任务，1秒后开始执行",
            "[job {}] Job received, starting in 1 second",
            job_id
        );

        if let Some(ref f) = status_fn {
            f("正在初始化... / Initializing...");
        }

        // 1-second delay: let the client see the "running" state update
        // before the game window is focused and takes over the screen.
        yas::utils::sleep(1000);

        // Lazy init: create executor if we don't have one yet. On failure we
        // do NOT poison the server — the next job gets a fresh attempt, since
        // init_executor is FnMut and the user may have just needed to open
        // the game window.
        let initialization = (|| -> Result<()> {
            if executor.is_none() {
                executor = Some(init_executor()?);
            }
            executor.as_mut().unwrap().prepare_task()
        })();
        match initialization {
            Ok(()) => {},
            Err(e) => {
                log_error!(
                    "[job {}] 游戏初始化失败: {}",
                    "[job {}] Game init failed: {}",
                    job_id,
                    yas::lang::localize(&format!("{:#}", e))
                );
                let mut state = job_state.lock().unwrap();
                let total_count = match &request {
                    JobRequest::Manage(r) => r.lock.len() + r.unlock.len(),
                    JobRequest::Equip(r) => r.equip.len(),
                    JobRequest::Scan(r) => {
                        r.characters as usize
                            + r.weapons as usize
                            + r.artifacts as usize
                            + r.achievements as usize
                    },
                };
                let err_results: Vec<_> = (0..total_count)
                        .map(|idx| {
                            InstructionResult::failure(
                                format!("item_{}", idx),
                                InstructionStatus::UiError,
                                "无法准备游戏或基础数据，因此此操作没有执行。请检查游戏和网络后重试。",
                                "The game or base data could not be prepared, so this operation was not performed. Check the game and network, then retry.",
                                Some(&e),
                            )
                        })
                        .collect();
                let summary = crate::manager::models::ManageSummary::from_results(&err_results);
                let result = crate::manager::models::ManageResult {
                    results: err_results,
                    summary,
                };
                *state = state.finished(job_id.clone(), result);
                if let Some(ref f) = status_fn {
                    f(&format!(
                        "服务器运行中，端口 {} / Server running on port {}",
                        port, port
                    ));
                }
                continue;
            },
        }
        let exec = executor.as_mut().unwrap();

        // Immediately invalidate cached data before execution starts.
        // Lock/unlock/equip changes modify in-game state; clients must not read stale data.
        {
            let invalidate_now = match &request {
                JobRequest::Manage(r) => !r.lock.is_empty() || !r.unlock.is_empty(),
                JobRequest::Equip(_) => true,
                JobRequest::Scan(_) => false, // scan is read-only
            };
            if invalidate_now {
                let mut cache = artifact_cache.lock().unwrap();
                if cache.data.is_some() {
                    cache.invalidate();
                }
            }
        }

        // Linear progress_fn for manage/equip: writes into JobState.progress.
        let linear_state = job_state.clone();
        let status_fn_linear = status_fn.clone();
        let linear_progress_fn =
            move |completed: usize, total: usize, current_id: &str, phase: &str| {
                if let Ok(mut state) = linear_state.lock() {
                    state.progress = Some(JobProgress {
                        completed,
                        total,
                        current_id: current_id.to_string(),
                        phase: phase.to_string(),
                    });
                }
                if let Some(ref f) = status_fn_linear {
                    let parts: Vec<&str> = phase.split(" / ").collect();
                    let (zh, en) = if parts.len() == 2 {
                        (parts[0], parts[1])
                    } else {
                        (phase, phase)
                    };
                    let msg_zh = format!("{}: {}/{} (鼠标右键终止)", zh, completed, total);
                    let msg_en = format!("{}: {}/{} (Right-click to abort)", en, completed, total);
                    f(&format!("{} / {}", msg_zh, msg_en));
                }
            };

        // Scan progress_fn: `phase` is the category key ("characters" /
        // "weapons" / "artifacts"). Updates the per-category slot in
        // JobState.scan_progress. Transitions phase state to Running on the
        // first tick; Complete/Aborted are set when execute_scan returns.
        let scan_state = job_state.clone();
        let status_fn_scan = status_fn.clone();
        let scan_progress_fn = move |completed: usize, total: usize, _id: &str, phase: &str| {
            if let Ok(mut state) = scan_state.lock() {
                if let Some(ref mut sp) = state.scan_progress {
                    let slot = match phase {
                        "characters" => sp.characters.as_mut(),
                        "weapons" => sp.weapons.as_mut(),
                        "artifacts" => sp.artifacts.as_mut(),
                        "achievements" => sp.achievements.as_mut(),
                        _ => None,
                    };
                    if let Some(pp) = slot {
                        pp.completed = completed;
                        pp.total = total;
                        pp.state = PhaseState::Running;
                    }
                }
            }
            if let Some(ref f) = status_fn_scan {
                let (zh, en) = match phase {
                    "characters" => ("扫描角色", "Scanning characters"),
                    "weapons" => ("扫描武器", "Scanning weapons"),
                    "artifacts" => ("扫描圣遗物", "Scanning artifacts"),
                    "achievements" => ("扫描成就", "Scanning achievements"),
                    _ => (phase, phase),
                };
                let msg_zh = if total > 0 {
                    format!("{}: {}/{} (鼠标右键终止)", zh, completed, total)
                } else {
                    format!("{}: {} (鼠标右键终止)", zh, completed)
                };
                let msg_en = if total > 0 {
                    format!("{}: {}/{} (Right-click to abort)", en, completed, total)
                } else {
                    format!("{}: {} (Right-click to abort)", en, completed)
                };
                f(&format!("{} / {}", msg_zh, msg_en));
            }
        };

        let phases_state = job_state.clone();
        let scan_phase_fn = move |key: &str, event| {
            use crate::scanner::common::progress::ScanPhaseEvent;
            if let Ok(mut state) = phases_state.lock() {
                if let Some(scan) = &mut state.scan_progress {
                    let slot = match key {
                        "characters" => &mut scan.characters,
                        "weapons" => &mut scan.weapons,
                        "artifacts" => &mut scan.artifacts,
                        "achievements" => &mut scan.achievements,
                        _ => return,
                    };
                    if let Some(phase) = slot {
                        phase.state = match event {
                            ScanPhaseEvent::Started => PhaseState::Running,
                            ScanPhaseEvent::Complete(count) => {
                                phase.completed = count;
                                PhaseState::Complete
                            },
                            ScanPhaseEvent::Interrupted => PhaseState::Aborted,
                        };
                    }
                }
            }
        };
        let cancel_token = yas::cancel::CancelToken::new();

        // Dispatch: manage/equip use ManageResult; scan builds its own ManageResult summary.
        enum JobOutcome {
            ManageEquip {
                result: ManageResult,
                artifact_snapshot: Option<Vec<GoodArtifact>>,
                invalidates_cache: bool,
            },
            Scan(anyhow::Result<ScanResult>),
        }

        let outcome = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || match request {
                JobRequest::Manage(manage_req) => {
                    let has_lock = !manage_req.lock.is_empty() || !manage_req.unlock.is_empty();
                    let (result, snapshot) =
                        exec.execute(manage_req, Some(&linear_progress_fn), cancel_token);
                    JobOutcome::ManageEquip {
                        result,
                        artifact_snapshot: snapshot,
                        invalidates_cache: has_lock,
                    }
                },
                JobRequest::Equip(equip_req) => {
                    let result =
                        exec.execute_equip(equip_req, Some(&linear_progress_fn), cancel_token);
                    JobOutcome::ManageEquip {
                        result,
                        artifact_snapshot: None,
                        invalidates_cache: true,
                    }
                },
                JobRequest::Scan(scan_req) => JobOutcome::Scan(exec.execute_scan_observed(
                    &scan_req,
                    Some(&scan_progress_fn),
                    cancel_token,
                    Some(&scan_phase_fn),
                )),
            },
        )) {
            Ok(r) => r,
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                log_error!(
                    "[job {}] 执行时发生panic: {}",
                    "[job {}] Panic during execution: {}",
                    job_id,
                    msg
                );
                let source = anyhow!(msg);
                let results = vec![InstructionResult::failure(
                        "job",
                        InstructionStatus::UiError,
                        "任务因扫描器内部错误而停止。下方包含可复制的完整错误。",
                        "The job stopped because of an internal scanner error. The complete copyable error is included below.",
                        Some(&source),
                    )];
                let summary = ManageSummary::from_results(&results);
                let result = ManageResult { results, summary };
                let mut state = job_state.lock().unwrap();
                *state = state.finished(job_id.clone(), result);
                continue;
            },
        };

        match outcome {
            JobOutcome::ManageEquip {
                result,
                artifact_snapshot,
                invalidates_cache,
            } => {
                for instruction in &result.results {
                    if let Some(message) = &instruction.message {
                        yas::log_warn!(
                            "管理请求 {} / {}：{}",
                            "Manager request {} / {}: {}",
                            job_id,
                            instruction.id,
                            yas::lang::localize(message)
                        );
                    }
                }
                // Update artifact cache based on scan completeness
                match artifact_snapshot {
                    Some(snapshot) => {
                        let count = snapshot.len();
                        if dump_job_data {
                            save_job_good_export(
                                &job_id,
                                "manage",
                                None,
                                None,
                                Some(snapshot.clone()),
                                None,
                            );
                        }
                        artifact_cache.lock().unwrap().set(job_id.clone(), snapshot);
                        log_info!(
                            "[job {}] 圣遗物快照已更新（{} 个）",
                            "[job {}] Artifact snapshot updated ({} items)",
                            job_id,
                            count
                        );
                    },
                    None => {
                        if invalidates_cache {
                            let mut cache = artifact_cache.lock().unwrap();
                            if cache.data.is_some() {
                                cache.invalidate();
                                log_info!(
                                    "[job {}] 游戏内状态已变更，快照已失效",
                                    "[job {}] In-game state changed, artifact snapshot invalidated",
                                    job_id
                                );
                            }
                        }
                    },
                }
                let mut state = job_state.lock().unwrap();
                *state = state.finished(job_id.clone(), result);
            },
            JobOutcome::Scan(scan_result) => {
                match scan_result {
                    Ok(sr) => {
                        // Per-phase: Complete populates the cache; Failed/Incomplete
                        // mark the cache as incomplete-for-this-jobId (so data queries
                        // return 503); NotAttempted leaves the cache untouched.
                        let mut results = Vec::new();
                        let mut phases_complete = 0usize;
                        let mut phases_incomplete = 0usize;
                        let dump_characters = finalize_scan_phase(
                            sr.characters,
                            ScanCategory::Characters,
                            &character_cache,
                            &job_id,
                            dump_job_data,
                            &mut results,
                            &mut phases_complete,
                            &mut phases_incomplete,
                        );
                        let dump_weapons = finalize_scan_phase(
                            sr.weapons,
                            ScanCategory::Weapons,
                            &weapon_cache,
                            &job_id,
                            dump_job_data,
                            &mut results,
                            &mut phases_complete,
                            &mut phases_incomplete,
                        );
                        let dump_artifacts = finalize_scan_phase(
                            sr.artifacts,
                            ScanCategory::Artifacts,
                            &artifact_cache,
                            &job_id,
                            dump_job_data,
                            &mut results,
                            &mut phases_complete,
                            &mut phases_incomplete,
                        );
                        let dump_achievements = finalize_scan_phase(
                            sr.achievements,
                            ScanCategory::Achievements,
                            &achievement_cache,
                            &job_id,
                            dump_job_data,
                            &mut results,
                            &mut phases_complete,
                            &mut phases_incomplete,
                        );

                        if dump_job_data
                            && (dump_characters.is_some()
                                || dump_weapons.is_some()
                                || dump_artifacts.is_some()
                                || dump_achievements.is_some())
                        {
                            save_job_good_export(
                                &job_id,
                                "scan",
                                dump_characters,
                                dump_weapons,
                                dump_artifacts,
                                dump_achievements,
                            );
                        }

                        log_info!(
                            "[job {}] 扫描结束（{} 完成, {} 中断）",
                            "[job {}] Scan finished ({} complete, {} aborted)",
                            job_id,
                            phases_complete,
                            phases_incomplete
                        );
                        let summary = ManageSummary::from_results(&results);
                        let result = ManageResult { results, summary };
                        let mut state = job_state.lock().unwrap();
                        *state = state.finished(job_id.clone(), result);
                    },
                    Err(e) => {
                        log_error!(
                            "[job {}] 扫描失败: {}",
                            "[job {}] Scan failed: {}",
                            job_id,
                            yas::lang::localize(&format!("{:#}", e))
                        );
                        let results = vec![InstructionResult::failure(
                            "scan",
                            InstructionStatus::UiError,
                            "扫描任务遇到错误，因此无法继续。下方包含可复制的完整错误。",
                            "The scan job encountered an error and could not continue. The complete copyable error is included below.",
                            Some(&e),
                        )];
                        let summary = ManageSummary::from_results(&results);
                        let result = ManageResult { results, summary };
                        let mut state = job_state.lock().unwrap();
                        *state = state.finished(job_id.clone(), result);
                    },
                }
            },
        }

        log_info!("[job {}] 执行完成", "[job {}] Execution completed", job_id);
        if let Some(ref f) = status_fn {
            f(&format!(
                "服务器运行中，端口 {} / Server running on port {}",
                port, port
            ));
        }
    }

    // Channel disconnected — wait for internal threads to fully stop before
    // returning. Without this, detached threads may still be tearing down
    // when the process exits, causing heap corruption in test suites.
    let _ = shutdown_watcher.join();
    let _ = http_thread.join();
    Ok(())
}

/// Validate a single artifact entry. Returns `Some(message)` on failure.
fn validate_artifact(artifact: &crate::scanner::common::models::GoodArtifact) -> Option<String> {
    if artifact.set_key.trim().is_empty() {
        return Some(configured_text("setKey 不能为空", "setKey must not be empty").to_string());
    }
    if artifact.slot_key.trim().is_empty() {
        return Some(configured_text("slotKey 不能为空", "slotKey must not be empty").to_string());
    }
    if artifact.main_stat_key.trim().is_empty() {
        return Some(
            configured_text("mainStatKey 不能为空", "mainStatKey must not be empty").to_string(),
        );
    }
    if artifact.rarity < 4 || artifact.rarity > 5 {
        return Some(if yas::lang::is_en() {
            format!("invalid rarity: {} (must be 4-5)", artifact.rarity)
        } else {
            format!("无效稀有度: {}（必须为 4-5）", artifact.rarity)
        });
    }
    if artifact.level < 0 || artifact.level > 20 {
        return Some(if yas::lang::is_en() {
            format!("invalid level: {} (must be 0-20)", artifact.level)
        } else {
            format!("无效等级: {}（必须为 0-20）", artifact.level)
        });
    }
    None
}

/// Parse jobId from a URL query string like "/path?jobId=xxx".
fn parse_job_id(url: &str) -> Option<&str> {
    url.split('?')
        .nth(1)
        .and_then(|qs| qs.split('&').find(|p| p.starts_with("jobId=")))
        .map(|p| &p[6..])
        .filter(|s| !s.is_empty())
}

fn cache_data_names(label: &str) -> (&str, &str) {
    match label {
        "characters" => ("角色", "character"),
        "weapons" => ("武器", "weapon"),
        "artifacts" => ("圣遗物", "artifact"),
        "achievements" => ("成就", "achievement"),
        _ => (label, label),
    }
}

/// Serve a typed data cache endpoint (GET /characters, /weapons, /artifacts, /achievements).
/// Requires `?jobId=xxx` query parameter.
///
/// 200: cached data for matching jobId.
/// 503: the requested jobId attempted to populate this cache but didn't finish.
/// 404: unknown jobId (never seen, or overwritten by a later scan).
/// 400: jobId query parameter missing.
fn serve_cache<T: serde::Serialize>(
    request: tiny_http::Request,
    url: &str,
    cache: &Arc<Mutex<ScanDataCache<T>>>,
    label: &str,
    cors_origin: Option<&str>,
) {
    let query_job_id = parse_job_id(url);
    match query_job_id {
        None => {
            respond_error(
                request,
                400,
                "缺少必需的查询参数 jobId。",
                "Missing required query parameter: jobId.",
                cors_origin,
            );
        },
        Some(requested_id) => {
            let c = cache.lock().unwrap();
            if let (Some(cached_id), Some(data)) = (&c.job_id, &c.data) {
                if cached_id == requested_id {
                    match serde_json::to_string(data) {
                        Ok(json) => {
                            drop(c);
                            respond_json(request, 200, &json, cors_origin);
                        },
                        Err(e) => {
                            let (zh_name, en_name) = cache_data_names(label);
                            let zh_hint = format!("无法序列化{}数据。", zh_name);
                            let en_hint = format!("The {} data could not be serialized.", en_name);
                            drop(c);
                            respond_serialization_error(
                                request,
                                &zh_hint,
                                &en_hint,
                                e,
                                cors_origin,
                            );
                        },
                    }
                    return;
                }
            }
            if c.incomplete_job_id.as_deref() == Some(requested_id) {
                let (zh_name, en_name) = cache_data_names(label);
                let message = if yas::lang::is_en() {
                    format!("The {} scan did not complete for this jobId.", en_name)
                } else {
                    format!("{}扫描未完成，因此此 jobId 没有可用数据。", zh_name)
                };
                drop(c);
                respond_error_message(request, 503, &message, cors_origin);
                return;
            }
            let (zh_name, en_name) = cache_data_names(label);
            let message = if yas::lang::is_en() {
                format!("No {} data is available for this jobId.", en_name)
            } else {
                format!("此 jobId 没有可用的{}数据。", zh_name)
            };
            drop(c);
            respond_error_message(request, 404, &message, cors_origin);
        },
    }
}

/// Serve the artifact cache with optional jobId (backwards compatible).
/// If jobId is provided, it must match. If omitted, returns the latest data.
///
/// 200: cached data matching jobId (or latest, if jobId omitted).
/// 503: the requested jobId attempted to populate the artifact cache but didn't finish.
/// 404: no cached data, or jobId specified but not recognized.
fn serve_artifact_cache(
    request: tiny_http::Request,
    url: &str,
    cache: &Arc<Mutex<ScanDataCache<GoodArtifact>>>,
    cors_origin: Option<&str>,
) {
    let query_job_id = parse_job_id(url);
    let c = cache.lock().unwrap();
    if let (Some(cached_id), Some(data)) = (&c.job_id, &c.data) {
        // If jobId provided, it must match; otherwise serve the latest.
        if query_job_id.map_or(true, |q| q == cached_id) {
            match serde_json::to_string(data) {
                Ok(json) => {
                    drop(c);
                    respond_json(request, 200, &json, cors_origin);
                },
                Err(e) => {
                    drop(c);
                    respond_serialization_error(
                        request,
                        "无法序列化圣遗物数据。",
                        "The artifact data could not be serialized.",
                        e,
                        cors_origin,
                    );
                },
            }
            return;
        }
    }
    if let Some(requested_id) = query_job_id {
        if c.incomplete_job_id.as_deref() == Some(requested_id) {
            drop(c);
            respond_error(
                request,
                503,
                "圣遗物扫描未完成，因此此 jobId 没有可用数据。",
                "The artifact scan did not complete for this jobId.",
                cors_origin,
            );
            return;
        }
    }
    drop(c);
    respond_error(
        request,
        404,
        "没有可用的圣遗物数据。",
        "No artifact data is available.",
        cors_origin,
    );
}

/// Handle POST /manage: validate origin, check busy, enforce size limit, submit job.
fn handle_manage(
    mut request: tiny_http::Request,
    enabled: &AtomicBool,
    state: &Arc<Mutex<JobState>>,
    job_tx: &mpsc::Sender<(String, JobRequest)>,
    cors_origin: Option<&str>,
    dump_job_data: bool,
) {
    // Check if manager is enabled
    if !enabled.load(Ordering::Relaxed) {
        log_warn!(
            "管理器已暂停，拒绝请求",
            "Manager paused, rejecting request"
        );
        respond_error(
            request,
            503,
            "管理器已暂停。请在界面中启用后再发送请求。",
            "The manager is paused. Enable it in the GUI before sending requests.",
            cors_origin,
        );
        return;
    }

    // Check if already busy
    {
        let s = state.lock().unwrap();
        if s.state == JobPhase::Running {
            respond_error(
                request,
                409,
                "另一个任务正在运行。请轮询 GET /status 查看进度。",
                "Another job is already running. Poll GET /status for progress.",
                cors_origin,
            );
            return;
        }
    }

    // Enforce body size limit (Content-Length header)
    if let Some(len) = request.body_length() {
        if len > MAX_BODY_SIZE {
            let message = if yas::lang::is_en() {
                format!(
                    "Request body too large: {} bytes (max {}).",
                    len, MAX_BODY_SIZE
                )
            } else {
                format!("请求体过大（{} 字节，上限 {} 字节）。", len, MAX_BODY_SIZE)
            };
            respond_error_message(request, 413, &message, cors_origin);
            return;
        }
    }

    // Read body
    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        respond_error_with_diagnostic(
            request,
            400,
            "无法读取请求体。",
            "The request body could not be read.",
            e,
            cors_origin,
        );
        return;
    }

    // Log request body to file
    if dump_job_data {
        save_request("manage", &body);
    }

    // Enforce size limit for chunked transfers (no Content-Length)
    if body.len() > MAX_BODY_SIZE {
        let message = if yas::lang::is_en() {
            format!(
                "Request body too large: {} bytes (max {}).",
                body.len(),
                MAX_BODY_SIZE
            )
        } else {
            format!(
                "请求体过大（{} 字节，上限 {} 字节）。",
                body.len(),
                MAX_BODY_SIZE
            )
        };
        respond_error_message(request, 413, &message, cors_origin);
        return;
    }

    // Parse JSON
    let manage_request: LockManageRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            respond_error_with_diagnostic(
                request,
                400,
                "请求体不是有效的 JSON。",
                "The request body is not valid JSON.",
                e,
                cors_origin,
            );
            return;
        },
    };

    if manage_request.lock.is_empty() && manage_request.unlock.is_empty() {
        respond_error(
            request,
            400,
            "lock 和 unlock 列表均为空。",
            "Both the lock and unlock lists are empty.",
            cors_origin,
        );
        return;
    }

    // Validate ALL entries upfront — reject the whole request on any invalid entry.
    for (list_name, artifacts) in [
        ("lock", &manage_request.lock),
        ("unlock", &manage_request.unlock),
    ] {
        for (idx, artifact) in artifacts.iter().enumerate() {
            if let Some(err) = validate_artifact(artifact) {
                let message = if yas::lang::is_en() {
                    format!("{}[{}] is invalid: {}", list_name, idx, err)
                } else {
                    format!("{}[{}] 无效: {}", list_name, idx, err)
                };
                respond_error_message(request, 400, &message, cors_origin);
                return;
            }
        }
    }

    let total = manage_request.lock.len() + manage_request.unlock.len();
    let job_id = uuid::Uuid::new_v4().to_string();

    log_info!(
        "[job {}] 收到 {} 条管理请求（lock: {}, unlock: {}）",
        "[job {}] Received {} manage items (lock: {}, unlock: {})",
        job_id,
        total,
        manage_request.lock.len(),
        manage_request.unlock.len()
    );

    // Set state to Running
    {
        let mut s = state.lock().unwrap();
        *s = JobState::running(job_id.clone(), total);
        s.ui.kind = Some(crate::manager::models::JobKind::Manage {
            lock: manage_request.lock.len(),
            unlock: manage_request.unlock.len(),
        });
    }

    // Send to execution thread
    if let Err(e) = job_tx.send((job_id.clone(), JobRequest::Manage(manage_request))) {
        let mut s = state.lock().unwrap();
        *s = JobState::idle();
        respond_error_with_diagnostic(
            request,
            500,
            "无法提交任务，因为执行线程不可用。",
            "The job could not be submitted because the execution thread is unavailable.",
            e,
            cors_origin,
        );
        return;
    }

    // Return 202 Accepted immediately
    let json = format!(r#"{{"jobId":"{}","total":{}}}"#, job_id, total);
    respond_json(request, 202, &json, cors_origin);
}

/// Handle POST /equip: validate, parse EquipRequest, submit job.
fn handle_equip(
    mut request: tiny_http::Request,
    enabled: &AtomicBool,
    state: &Arc<Mutex<JobState>>,
    job_tx: &mpsc::Sender<(String, JobRequest)>,
    cors_origin: Option<&str>,
    dump_job_data: bool,
) {
    if !enabled.load(Ordering::Relaxed) {
        log_warn!(
            "管理器已暂停，拒绝请求",
            "Manager paused, rejecting request"
        );
        respond_error(
            request,
            503,
            "管理器已暂停。请在界面中启用后再发送请求。",
            "The manager is paused. Enable it in the GUI before sending requests.",
            cors_origin,
        );
        return;
    }

    {
        let s = state.lock().unwrap();
        if s.state == JobPhase::Running {
            respond_error(
                request,
                409,
                "另一个任务正在运行。请轮询 GET /status 查看进度。",
                "Another job is already running. Poll GET /status for progress.",
                cors_origin,
            );
            return;
        }
    }

    if let Some(len) = request.body_length() {
        if len > MAX_BODY_SIZE {
            let message = if yas::lang::is_en() {
                format!(
                    "Request body too large: {} bytes (max {}).",
                    len, MAX_BODY_SIZE
                )
            } else {
                format!("请求体过大（{} 字节，上限 {} 字节）。", len, MAX_BODY_SIZE)
            };
            respond_error_message(request, 413, &message, cors_origin);
            return;
        }
    }

    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        respond_error_with_diagnostic(
            request,
            400,
            "无法读取请求体。",
            "The request body could not be read.",
            e,
            cors_origin,
        );
        return;
    }

    // Log request body to file
    if dump_job_data {
        save_request("equip", &body);
    }

    if body.len() > MAX_BODY_SIZE {
        let message = if yas::lang::is_en() {
            format!(
                "Request body too large: {} bytes (max {}).",
                body.len(),
                MAX_BODY_SIZE
            )
        } else {
            format!(
                "请求体过大（{} 字节，上限 {} 字节）。",
                body.len(),
                MAX_BODY_SIZE
            )
        };
        respond_error_message(request, 413, &message, cors_origin);
        return;
    }

    let equip_request: EquipRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            respond_error_with_diagnostic(
                request,
                400,
                "请求体不是有效的 JSON。",
                "The request body is not valid JSON.",
                e,
                cors_origin,
            );
            return;
        },
    };

    if equip_request.equip.is_empty() {
        respond_error(
            request,
            400,
            "equip 列表为空。",
            "The equip list is empty.",
            cors_origin,
        );
        return;
    }

    // Validate all artifact entries
    for (idx, instr) in equip_request.equip.iter().enumerate() {
        if let Some(err) = validate_artifact(&instr.artifact) {
            let message = if yas::lang::is_en() {
                format!("equip[{}] is invalid: {}", idx, err)
            } else {
                format!("equip[{}] 无效: {}", idx, err)
            };
            respond_error_message(request, 400, &message, cors_origin);
            return;
        }
    }

    let total = equip_request.equip.len();
    let job_id = uuid::Uuid::new_v4().to_string();

    log_info!(
        "[job {}] 收到 {} 条装备请求",
        "[job {}] Received {} equip instructions",
        job_id,
        total
    );

    {
        let mut s = state.lock().unwrap();
        *s = JobState::running(job_id.clone(), total);
        s.ui.kind = Some(crate::manager::models::JobKind::Equip);
    }

    if let Err(e) = job_tx.send((job_id.clone(), JobRequest::Equip(equip_request))) {
        let mut s = state.lock().unwrap();
        *s = JobState::idle();
        respond_error_with_diagnostic(
            request,
            500,
            "无法提交任务，因为执行线程不可用。",
            "The job could not be submitted because the execution thread is unavailable.",
            e,
            cors_origin,
        );
        return;
    }

    let json = format!(r#"{{"jobId":"{}","total":{}}}"#, job_id, total);
    respond_json(request, 202, &json, cors_origin);
}

/// Handle POST /scan: validate, parse ScanRequest, submit job.
fn handle_scan(
    mut request: tiny_http::Request,
    enabled: &AtomicBool,
    state: &Arc<Mutex<JobState>>,
    job_tx: &mpsc::Sender<(String, JobRequest)>,
    cors_origin: Option<&str>,
    dump_job_data: bool,
) {
    if !enabled.load(Ordering::Relaxed) {
        log_warn!(
            "管理器已暂停，拒绝请求",
            "Manager paused, rejecting request"
        );
        respond_error(
            request,
            503,
            "管理器已暂停。请在界面中启用后再发送请求。",
            "The manager is paused. Enable it in the GUI before sending requests.",
            cors_origin,
        );
        return;
    }

    {
        let s = state.lock().unwrap();
        if s.state == JobPhase::Running {
            respond_error(
                request,
                409,
                "另一个任务正在运行。请轮询 GET /status 查看进度。",
                "Another job is already running. Poll GET /status for progress.",
                cors_origin,
            );
            return;
        }
    }

    let mut body = String::new();
    if let Err(e) = request.as_reader().read_to_string(&mut body) {
        respond_error_with_diagnostic(
            request,
            400,
            "无法读取请求体。",
            "The request body could not be read.",
            e,
            cors_origin,
        );
        return;
    }

    if dump_job_data {
        save_request("scan", &body);
    }

    let scan_request: ScanRequest = match serde_json::from_str(&body) {
        Ok(r) => r,
        Err(e) => {
            respond_error_with_diagnostic(
                request,
                400,
                "请求体不是有效的 JSON。",
                "The request body is not valid JSON.",
                e,
                cors_origin,
            );
            return;
        },
    };

    if !scan_request.characters
        && !scan_request.weapons
        && !scan_request.artifacts
        && !scan_request.achievements
    {
        respond_error(
            request,
            400,
            "至少需要启用一个扫描目标。",
            "At least one scan target must be true.",
            cors_origin,
        );
        return;
    }

    if scan_request.artifact_mode == ArtifactScanMode::Recent && !scan_request.artifacts {
        respond_error(
            request,
            400,
            "artifactMode=recent 需要 artifacts=true。",
            "artifactMode=recent requires artifacts=true.",
            cors_origin,
        );
        return;
    }

    if scan_request.artifact_mode == ArtifactScanMode::Recent
        && scan_request.artifact_limit.is_none()
    {
        respond_error(
            request,
            400,
            "最近圣遗物扫描需要 artifactLimit。",
            "Recent artifact scans require artifactLimit.",
            cors_origin,
        );
        return;
    }

    if let Some(limit) = scan_request.artifact_limit {
        if limit == 0 {
            respond_error(
                request,
                400,
                "artifactLimit 必须大于 0。",
                "artifactLimit must be greater than 0.",
                cors_origin,
            );
            return;
        }

        if limit > 1000 {
            respond_error(
                request,
                400,
                "artifactLimit 不能超过 1000。",
                "artifactLimit cannot exceed 1000.",
                cors_origin,
            );
            return;
        }
    }

    let scan_chars = scan_request.characters;
    let scan_wpns = scan_request.weapons;
    let scan_arts = scan_request.artifacts;
    let scan_achs = scan_request.achievements;
    let artifact_mode = scan_request.artifact_mode;
    let artifact_limit = scan_request.artifact_limit;
    let job_id = uuid::Uuid::new_v4().to_string();

    log_info!(
        "[job {}] 收到扫描请求（角色: {}, 武器: {}, 圣遗物: {}, 成就: {}, 圣遗物模式: {:?}, 限制: {:?}）",
        "[job {}] Received scan request (characters: {}, weapons: {}, artifacts: {}, achievements: {}, artifact_mode: {:?}, artifact_limit: {:?})",
        job_id,
        scan_chars,
        scan_wpns,
        scan_arts,
        scan_achs,
        artifact_mode,
        artifact_limit
    );

    {
        let mut s = state.lock().unwrap();
        *s = JobState::running_scan(job_id.clone(), scan_chars, scan_wpns, scan_arts, scan_achs);
    }

    if let Err(e) = job_tx.send((job_id.clone(), JobRequest::Scan(scan_request))) {
        let mut s = state.lock().unwrap();
        *s = JobState::idle();
        respond_error_with_diagnostic(
            request,
            500,
            "无法提交任务，因为执行线程不可用。",
            "The job could not be submitted because the execution thread is unavailable.",
            e,
            cors_origin,
        );
        return;
    }

    let artifact_mode_json = match artifact_mode {
        ArtifactScanMode::All => "all",
        ArtifactScanMode::Recent => "recent",
    };
    let limit_json = artifact_limit
        .map(|limit| limit.to_string())
        .unwrap_or_else(|| "null".to_string());
    let json = format!(
        r#"{{"jobId":"{}","targets":{{"characters":{},"weapons":{},"artifacts":{},"achievements":{}}},"artifactMode":"{}","artifactLimit":{}}}"#,
        job_id, scan_chars, scan_wpns, scan_arts, scan_achs, artifact_mode_json, limit_json
    );
    respond_json(request, 202, &json, cors_origin);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scanner::common::models::{
        GoodArtifact, GoodCharacter, GoodSubStat, GoodTalent, GoodWeapon,
    };
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    // Serialize all server tests to prevent concurrent tiny_http teardown,
    // which causes STATUS_HEAP_CORRUPTION on Windows.
    static SERVER_LOCK: Mutex<()> = Mutex::new(());

    struct TestLanguageGuard {
        previous: &'static str,
    }

    impl TestLanguageGuard {
        fn set(lang: &str) -> Self {
            let previous = yas::lang::get_lang();
            yas::lang::set_lang(lang);
            Self { previous }
        }
    }

    impl Drop for TestLanguageGuard {
        fn drop(&mut self) {
            yas::lang::set_lang(self.previous);
        }
    }

    fn assert_error_response(resp: reqwest::blocking::Response, expected_status: u16) -> String {
        assert_eq!(resp.status().as_u16(), expected_status);
        let body: serde_json::Value = resp.json().expect("error response must be valid JSON");
        let object = body
            .as_object()
            .expect("error response must be a JSON object");
        assert_eq!(object.len(), 1, "error response schema must remain stable");
        object
            .get("error")
            .and_then(serde_json::Value::as_str)
            .expect("error response must contain one string error field")
            .to_string()
    }

    #[test]
    fn test_origin_allowlist_accepts_loopback_hosts() {
        assert!(is_origin_allowed("https://ggartifact.com"));
        assert!(is_origin_allowed("http://ggartifact.com"));
        assert!(is_origin_allowed(
            "https://ggartifact.vanyrainel.workers.dev"
        ));
        assert!(is_origin_allowed("https://preview-ggartifact.pages.dev"));
        assert!(is_origin_allowed("https://GGARTIFACT.example.dev"));
        assert!(is_origin_allowed("http://localhost:3000"));
        assert!(is_origin_allowed("https://LOCALHOST:3000"));
        assert!(is_origin_allowed("http://127.0.0.1:5173"));
        assert!(is_origin_allowed("https://127.12.34.56:5173"));
        assert!(is_origin_allowed("http://[::1]:5173"));

        assert!(!is_origin_allowed("https://evil.com"));
        assert!(!is_origin_allowed("https://evil.com/ggartifact"));
        assert!(!is_origin_allowed("https://evil.com?site=ggartifact"));
        assert!(!is_origin_allowed("http://127.0.0.1.evil.com:5173"));
        assert!(!is_origin_allowed("http://192.168.0.1:5173"));
        assert!(!is_origin_allowed("http://[::2]:5173"));
        assert!(!is_origin_allowed("ftp://ggartifact.com"));
        assert!(!is_origin_allowed("ftp://127.0.0.1"));
    }

    #[test]
    fn test_error_json_localizes_hint_and_preserves_exact_diagnostic() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("zh");
        let diagnostic = "send failed / device \"adapter\"\nsecond line";

        let zh_message = error_message_with_diagnostic(
            "无法提交任务。",
            "The job could not be submitted.",
            diagnostic,
        );
        let zh_json = error_json(&zh_message);
        let zh_body: serde_json::Value = serde_json::from_str(&zh_json).unwrap();
        assert_eq!(
            zh_body["error"],
            format!("无法提交任务。\n\n完整错误详情:\n{}", diagnostic)
        );
        assert!(zh_json.contains("\\\"adapter\\\""));
        assert!(zh_json.contains("\\nsecond line"));

        yas::lang::set_lang("en");
        let en_message = error_message_with_diagnostic(
            "无法提交任务。",
            "The job could not be submitted.",
            diagnostic,
        );
        let en_json = error_json(&en_message);
        let en_body: serde_json::Value = serde_json::from_str(&en_json).unwrap();
        assert_eq!(
            en_body["error"],
            format!(
                "The job could not be submitted.\n\nFull error details:\n{}",
                diagnostic
            )
        );
    }

    #[test]
    fn test_server_bind_error_keeps_hint_and_original_io_diagnostic() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");
        let source: Box<dyn std::error::Error + Send + Sync> = Box::new(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "bind marker / STATUS_MARKER",
        ));

        let error = contextualize_server_bind_error(19123, source);
        assert_eq!(
            format!("{error:#}"),
            "Port 19123 is already in use. Choose a different port.: bind marker / STATUS_MARKER"
        );
    }

    #[test]
    fn test_serialization_error_response_uses_http_500() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");
        let port = next_port();
        let server = Server::http(format!("127.0.0.1:{port}"))
            .expect("serialization response test server should bind");
        let response_thread = std::thread::spawn(move || {
            let request = server
                .recv()
                .expect("serialization response test should receive one request");
            respond_serialization_error(
                request,
                "无法序列化测试数据。",
                "The test data could not be serialized.",
                "serializer marker / STATUS_MARKER",
                None,
            );
        });

        let response = reqwest::blocking::get(format!("http://127.0.0.1:{port}/data")).unwrap();
        let error = assert_error_response(response, 500);
        assert_eq!(
            error,
            "The test data could not be serialized.\n\nFull error details:\nserializer marker / STATUS_MARKER"
        );
        response_thread.join().unwrap();
    }

    #[test]
    fn test_job_panic_surfaces_localized_hint_and_exact_payload() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");
        // An empty fake response queue deliberately panics with this stable
        // payload when the accepted manage job begins execution.
        let panic_payload = "FakeExecutor: no more responses queued";
        let (port, shutdown, handle) = start_test_server(VecDeque::new(), 0);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{port}");

        let response = client
            .post(format!("{base}/manage"))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["panic-target"]))
            .send()
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
        let job_id = response.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let result: serde_json::Value = client
            .get(format!("{base}/result?jobId={job_id}"))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(result["summary"]["errors"], 1);
        assert_eq!(result["results"][0]["id"], "job");
        assert_eq!(result["results"][0]["status"], "ui_error");
        assert_eq!(
            result["results"][0]["message"],
            format!(
                "The job stopped because of an internal scanner error. The complete copyable error is included below.\n\nFull error details:\n{panic_payload}"
            )
        );

        stop_server(&shutdown, handle);
    }

    struct FakeExecutor {
        responses: Arc<Mutex<VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)>>>,
        scan_responses: Arc<Mutex<VecDeque<anyhow::Result<ScanResult>>>>,
        delay_ms: u64,
    }

    static DATA_PREPARATIONS: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);

    impl ManageExecutor for FakeExecutor {
        fn prepare_task(&mut self) -> Result<()> {
            DATA_PREPARATIONS.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn execute(
            &mut self,
            _request: LockManageRequest,
            progress_fn: Option<&ProgressFn<'_>>,
            _cancel_token: yas::cancel::CancelToken,
        ) -> (ManageResult, Option<Vec<GoodArtifact>>) {
            let (result, snapshot) = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeExecutor: no more responses queued");

            // Report per-item progress spread across delay_ms so polling clients
            // can observe intermediate values. Mirrors real orchestrator behaviour
            // of reporting `completed` ticks from 0 to N.
            let total = result.results.len();
            if let Some(pf) = progress_fn {
                pf(0, total, "", "锁定变更 / Lock changes");
            }
            let per_item_delay = if total > 0 {
                self.delay_ms / total as u64
            } else {
                self.delay_ms
            };
            for (idx, r) in result.results.iter().enumerate() {
                if per_item_delay > 0 {
                    std::thread::sleep(Duration::from_millis(per_item_delay));
                }
                if let Some(pf) = progress_fn {
                    pf(idx + 1, total, &r.id, "锁定变更 / Lock changes");
                }
            }
            (result, snapshot)
        }

        fn execute_equip(
            &mut self,
            _request: EquipRequest,
            _progress_fn: Option<&ProgressFn<'_>>,
            _cancel_token: yas::cancel::CancelToken,
        ) -> ManageResult {
            let results = Vec::new();
            let summary = ManageSummary::from_results(&results);
            ManageResult { results, summary }
        }

        fn execute_scan(
            &mut self,
            _request: &ScanRequest,
            progress_fn: Option<&crate::scanner::common::progress::ProgressFn<'_>>,
            _cancel_token: yas::cancel::CancelToken,
        ) -> anyhow::Result<ScanResult> {
            let outcome = self
                .scan_responses
                .lock()
                .unwrap()
                .pop_front()
                .expect("FakeExecutor: no more scan responses queued");

            // Emit per-category per-item ticks spread across delay_ms. Each
            // requested phase gets a fake "total = 10" and ticks through 10 items
            // so polling clients can observe intermediate (completed, total)
            // values inside each category bar.
            let phases: Vec<(&'static str, bool)> = match &outcome {
                Ok(sr) => vec![
                    (
                        "characters",
                        !matches!(sr.characters, PhaseResult::NotAttempted),
                    ),
                    ("weapons", !matches!(sr.weapons, PhaseResult::NotAttempted)),
                    (
                        "artifacts",
                        !matches!(sr.artifacts, PhaseResult::NotAttempted),
                    ),
                    (
                        "achievements",
                        !matches!(sr.achievements, PhaseResult::NotAttempted),
                    ),
                ],
                Err(_) => vec![],
            };
            let active_phases: Vec<&'static str> = phases
                .iter()
                .filter_map(|(k, active)| if *active { Some(*k) } else { None })
                .collect();
            let fake_total: usize = 10;
            let total_ticks = active_phases.len() * fake_total;
            let per_tick_delay = if total_ticks > 0 {
                self.delay_ms / total_ticks as u64
            } else {
                0
            };
            for phase_key in &active_phases {
                if let Some(pf) = progress_fn {
                    pf(0, fake_total, "", phase_key);
                }
                for i in 0..fake_total {
                    if per_tick_delay > 0 {
                        std::thread::sleep(Duration::from_millis(per_tick_delay));
                    }
                    if let Some(pf) = progress_fn {
                        pf(i + 1, fake_total, "", phase_key);
                    }
                }
            }
            outcome
        }
    }

    fn make_result(statuses: &[(&str, InstructionStatus)]) -> ManageResult {
        let results: Vec<InstructionResult> = statuses
            .iter()
            .map(|(id, status)| InstructionResult::outcome(*id, status.clone()))
            .collect();
        let summary = ManageSummary::from_results(&results);
        ManageResult { results, summary }
    }

    fn make_artifact(set: &str, slot: &str, level: i32, locked: bool) -> GoodArtifact {
        GoodArtifact {
            set_key: set.to_string(),
            slot_key: slot.to_string(),
            rarity: 5,
            level,
            main_stat_key: "hp".to_string(),
            substats: vec![GoodSubStat {
                key: "critRate_".to_string(),
                value: 3.9,
                initial_value: None,
                rolls: vec![],
            }],
            location: String::new(),
            lock: locked,
            astral_mark: false,
            elixir_crafted: false,
            unactivated_substats: Vec::new(),
            total_rolls: None,
        }
    }

    fn make_manage_body(ids: &[&str]) -> String {
        let artifacts: Vec<String> = ids
            .iter()
            .map(|_id| {
                r#"{"setKey":"GladiatorsFinale","slotKey":"flower","rarity":5,"level":20,"mainStatKey":"hp","substats":[],"location":"","lock":false,"astralMark":false,"elixirCrafted":false,"unactivatedSubstats":[]}"#.to_string()
            })
            .collect();
        format!(r#"{{"lock":[{}]}}"#, artifacts.join(","))
    }

    static NEXT_PORT: AtomicU16 = AtomicU16::new(19100);
    fn next_port() -> u16 {
        NEXT_PORT.fetch_add(1, Ordering::SeqCst)
    }

    fn start_test_server(
        responses: VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)>,
        delay_ms: u64,
    ) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        start_test_server_full(
            responses,
            VecDeque::new(),
            delay_ms,
            Arc::new(AtomicBool::new(true)),
        )
    }

    fn start_test_server_with_enabled(
        responses: VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)>,
        delay_ms: u64,
        enabled: Arc<AtomicBool>,
    ) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        start_test_server_full(responses, VecDeque::new(), delay_ms, enabled)
    }

    fn start_test_server_with_scans(
        responses: VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)>,
        scan_responses: VecDeque<anyhow::Result<ScanResult>>,
        delay_ms: u64,
    ) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        start_test_server_full(
            responses,
            scan_responses,
            delay_ms,
            Arc::new(AtomicBool::new(true)),
        )
    }

    fn start_test_server_full(
        responses: VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)>,
        scan_responses: VecDeque<anyhow::Result<ScanResult>>,
        delay_ms: u64,
        enabled: Arc<AtomicBool>,
    ) -> (u16, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
        let port = next_port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let responses = Arc::new(Mutex::new(responses));
        let responses_clone = responses.clone();
        let scan_responses = Arc::new(Mutex::new(scan_responses));
        let scan_responses_clone = scan_responses.clone();

        let handle = std::thread::spawn(move || {
            let init = move || -> anyhow::Result<Box<dyn ManageExecutor>> {
                Ok(Box::new(FakeExecutor {
                    responses: responses_clone.clone(),
                    scan_responses: scan_responses_clone.clone(),
                    delay_ms,
                }))
            };
            let _ = run_server(port, init, enabled, shutdown_clone, false, None);
        });

        let client = reqwest::blocking::Client::new();
        let url = format!("http://127.0.0.1:{}/health", port);
        for _ in 0..50 {
            if client.get(&url).send().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        (port, shutdown, handle)
    }

    fn stop_server(shutdown: &AtomicBool, handle: std::thread::JoinHandle<()>) {
        shutdown.store(true, Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(300));
        let _ = handle.join();
    }

    /// Poll /status until `state == "completed"` or timeout.
    fn poll_until_completed(port: u16) {
        let client = reqwest::blocking::Client::new();
        let url = format!("http://127.0.0.1:{}/status", port);
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(100));
            let resp = client.get(&url).send().unwrap();
            let body: serde_json::Value = resp.json().unwrap();
            if body["state"] == "completed" {
                return;
            }
        }
        panic!("Job did not complete within timeout");
    }

    #[test]
    fn test_http_error_language_status_and_schema_contract() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");

        let mut responses = VecDeque::new();
        responses.push_back((make_result(&[("busy", InstructionStatus::Success)]), None));
        let enabled = Arc::new(AtomicBool::new(true));
        let (port, shutdown, handle) =
            start_test_server_with_enabled(responses, 1_000, enabled.clone());
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        let error =
            assert_error_response(client.get(format!("{}/result", base)).send().unwrap(), 400);
        assert_eq!(error, "Missing required query parameter: jobId.");

        let error = assert_error_response(
            client
                .get(format!("{}/not-an-endpoint", base))
                .send()
                .unwrap(),
            404,
        );
        assert_eq!(error, "The requested endpoint was not found.");

        let error = assert_error_response(
            client
                .get(format!("{}/health", base))
                .header("Origin", "https://evil.example")
                .send()
                .unwrap(),
            403,
        );
        assert_eq!(error, "The request origin is not allowed.");

        yas::lang::set_lang("zh");
        let error = assert_error_response(
            client
                .post(format!("{}/scan", base))
                .header("Content-Type", "application/json")
                .body("{\n  \"characters\": true,\n  \"broken\": \"unterminated\n}")
                .send()
                .unwrap(),
            400,
        );
        assert!(error.starts_with("请求体不是有效的 JSON。\n\n完整错误详情:\n"));
        assert!(error.contains("line"));

        let error = assert_error_response(
            client
                .post(format!("{}/manage", base))
                .header("Content-Type", "application/json")
                .body(vec![b' '; MAX_BODY_SIZE + 1])
                .send()
                .unwrap(),
            413,
        );
        assert!(error.starts_with("请求体过大"));

        yas::lang::set_lang("en");
        let accepted = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["busy"]))
            .send()
            .unwrap();
        assert_eq!(accepted.status().as_u16(), 202);

        let error = assert_error_response(
            client
                .post(format!("{}/scan", base))
                .header("Content-Type", "application/json")
                .body(r#"{"characters":true}"#)
                .send()
                .unwrap(),
            409,
        );
        assert_eq!(
            error,
            "Another job is already running. Poll GET /status for progress."
        );

        enabled.store(false, Ordering::Relaxed);
        let error = assert_error_response(
            client
                .post(format!("{}/manage", base))
                .header("Content-Type", "application/json")
                .body(r#"{"lock":[],"unlock":[]}"#)
                .send()
                .unwrap(),
            503,
        );
        assert_eq!(
            error,
            "The manager is paused. Enable it in the GUI before sending requests."
        );

        stop_server(&shutdown, handle);
    }

    // -----------------------------------------------------------------------
    // Tests: consolidated from 13 → 5 to minimize server instances.
    // All tests acquire SERVER_LOCK to run sequentially.
    // -----------------------------------------------------------------------

    /// Read-only endpoints + basic submit/lifecycle + artifacts + sequential jobs.
    /// Consolidates: test_readonly_endpoints, test_manage_accepts_valid_request,
    /// test_full_lifecycle_submit_poll_result, test_artifacts_returns_200_after_complete_scan,
    /// test_artifacts_stays_404_after_no_snapshot_job, test_sequential_jobs_reset_state.
    #[test]
    fn test_standard_flow() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut responses = VecDeque::new();
        // Job 1: single item, no snapshot (tests accept + artifacts 404)
        responses.push_back((make_result(&[("a", InstructionStatus::Success)]), None));
        // Job 2: 3 items, no snapshot (tests full lifecycle)
        responses.push_back((
            make_result(&[
                ("i1", InstructionStatus::Success),
                ("i2", InstructionStatus::NotFound),
                ("i3", InstructionStatus::AlreadyCorrect),
            ]),
            None,
        ));
        // Job 3: with snapshot (tests artifacts 200)
        let artifacts = vec![
            make_artifact("GladiatorsFinale", "flower", 20, true),
            make_artifact("WanderersTroupe", "plume", 16, false),
        ];
        responses.push_back((
            make_result(&[("art1", InstructionStatus::Success)]),
            Some(artifacts),
        ));
        // Jobs 4-5: sequential jobs (tests state reset)
        responses.push_back((make_result(&[("j1", InstructionStatus::Success)]), None));
        responses.push_back((make_result(&[("j2", InstructionStatus::NotFound)]), None));

        let (port, shutdown, handle) = start_test_server(responses, 0);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        // === Read-only checks (no jobs submitted yet) ===

        // health returns ok when idle
        let resp = client.get(format!("{}/health", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["status"], "ok");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["busy"], false);

        // CORS: allowed origins
        let resp = client
            .get(format!("{}/health", base))
            .header("Origin", "https://ggartifact.com")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let acao = resp
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(acao, "https://ggartifact.com");

        let resp = client
            .get(format!("{}/health", base))
            .header("Origin", "https://ggartifact.vanyrainel.workers.dev")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let acao = resp
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(acao, "https://ggartifact.vanyrainel.workers.dev");

        let resp = client
            .get(format!("{}/health", base))
            .header("Origin", "http://localhost:3000")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        let resp = client
            .get(format!("{}/health", base))
            .header("Origin", "http://127.0.0.1:5173")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        let resp = client.get(format!("{}/health", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // CORS: disallowed origin returns 403
        let resp = client
            .get(format!("{}/health", base))
            .header("Origin", "https://evil.com")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 403);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(
            body["error"],
            configured_text("不允许该请求来源。", "The request origin is not allowed.")
        );

        // CORS: preflight OPTIONS
        let resp = client
            .request(reqwest::Method::OPTIONS, format!("{}/manage", base))
            .header("Origin", "https://ggartifact.com")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 204);
        let acao = resp
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(acao, "https://ggartifact.com");

        // manage: empty instructions returns 400
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(r#"{"lock":[],"unlock":[]}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // manage: bad JSON returns 400
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let body = resp.text().unwrap();
        assert!(body.contains("JSON"));

        // status: idle before any job
        let resp = client.get(format!("{}/status", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["state"], "idle");

        // result: 400 without jobId
        let resp = client.get(format!("{}/result", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // result: 404 for unknown jobId
        let resp = client
            .get(format!("{}/result?jobId=nonexistent", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // unknown route returns 404
        let resp = client.get(format!("{}/nonexistent", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // artifacts: 404 before any scan (no jobId required)
        let resp = client.get(format!("{}/artifacts", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // artifacts: 404 with unknown jobId
        let resp = client
            .get(format!("{}/artifacts?jobId=nonexistent", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Job 1: basic accept + artifacts stays 404 ===

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        assert!(body["jobId"].is_string());
        let job1_early_id = body["jobId"].as_str().unwrap().to_string();
        assert_eq!(body["total"], 1);

        poll_until_completed(port);

        // No snapshot → artifacts 404 for this jobId
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job1_early_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Job 2: full lifecycle (submit/poll/result) ===

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["i1", "i2", "i3"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let submit_body: serde_json::Value = resp.json().unwrap();
        let job_id = submit_body["jobId"].as_str().unwrap().to_string();

        poll_until_completed(port);

        // Check status summary
        let resp = client.get(format!("{}/status", base)).send().unwrap();
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["state"], "completed");
        assert_eq!(body["summary"]["total"], 3);
        assert_eq!(body["summary"]["success"], 1);
        assert_eq!(body["summary"]["not_found"], 1);
        assert_eq!(body["summary"]["already_correct"], 1);

        // Get full result (with jobId)
        let resp = client
            .get(format!("{}/result?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"][0]["id"], "i1");
        assert_eq!(body["results"][0]["status"], "success");
        assert_eq!(body["results"][1]["id"], "i2");
        assert_eq!(body["results"][1]["status"], "not_found");
        assert_eq!(body["results"][2]["id"], "i3");
        assert_eq!(body["results"][2]["status"], "already_correct");

        // Result is idempotent
        let resp = client
            .get(format!("{}/result?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // === Job 3: artifacts snapshot ===

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["art1"]))
            .send()
            .unwrap();
        let job3_id = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job3_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert!(body.is_array());
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(body[0]["setKey"], "GladiatorsFinale");
        assert_eq!(body[1]["setKey"], "WanderersTroupe");

        // /artifacts without jobId returns latest (backwards compat)
        let resp = client.get(format!("{}/artifacts", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        // === Jobs 4-5: sequential jobs reset state ===

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["j1"]))
            .send()
            .unwrap();
        let job1_id = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/result?jobId={}", base, job1_id))
            .send()
            .unwrap();
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"][0]["id"], "j1");
        assert_eq!(body["results"][0]["status"], "success");

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["j2"]))
            .send()
            .unwrap();
        let job2_id = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/result?jobId={}", base, job2_id))
            .send()
            .unwrap();
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"][0]["id"], "j2");
        assert_eq!(body["results"][0]["status"], "not_found");

        // Job 1's result is gone — replaced by job 2
        let resp = client
            .get(format!("{}/result?jobId={}", base, job1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        stop_server(&shutdown, handle);
    }

    /// Artifact cache invalidation across multiple job patterns.
    /// Consolidates: test_artifacts_returns_503_after_aborted_scan_invalidates_cache,
    /// test_artifacts_invalidated_when_lock_job_returns_no_snapshot,
    /// test_artifacts_cleared_when_update_inventory_off_after_on.
    #[test]
    fn test_artifact_cache_invalidation() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut responses = VecDeque::new();
        // Pair 1: populate → aborted invalidates → 503
        responses.push_back((
            make_result(&[("a", InstructionStatus::Success)]),
            Some(vec![make_artifact("GladiatorsFinale", "flower", 20, true)]),
        ));
        responses.push_back((make_result(&[("b", InstructionStatus::Aborted)]), None));
        // Pair 2: populate → success no snapshot (stop_on_all_matched) → 503
        responses.push_back((
            make_result(&[("c", InstructionStatus::Success)]),
            Some(vec![make_artifact("GladiatorsFinale", "flower", 20, true)]),
        ));
        responses.push_back((make_result(&[("d", InstructionStatus::Success)]), None));
        // Pair 3: populate with 2 items → success no snapshot (update_inv off) → not 200
        responses.push_back((
            make_result(&[("e", InstructionStatus::Success)]),
            Some(vec![
                make_artifact("GladiatorsFinale", "flower", 20, true),
                make_artifact("WanderersTroupe", "plume", 16, false),
            ]),
        ));
        responses.push_back((make_result(&[("f", InstructionStatus::Success)]), None));

        let (port, shutdown, handle) = start_test_server(responses, 0);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        // === Pair 1: aborted scan invalidates cache ===
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a"]))
            .send()
            .unwrap();
        let job_a = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_a))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["b"]))
            .send()
            .unwrap();
        poll_until_completed(port);

        // Cache invalidated — old jobId no longer works
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_a))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        // Also 404 without jobId (no data at all after invalidation)
        let resp = client.get(format!("{}/artifacts", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Pair 2: lock job with no snapshot invalidates cache ===
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["c"]))
            .send()
            .unwrap();
        let job_c = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_c))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["d"]))
            .send()
            .unwrap();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_c))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Pair 3: update_inventory off after on ===
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["e"]))
            .send()
            .unwrap();
        let job_e = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_e))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);

        client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["f"]))
            .send()
            .unwrap();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_e))
            .send()
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            200,
            "/artifacts must not serve stale data after a scan with update_inventory OFF"
        );

        stop_server(&shutdown, handle);
    }

    /// Manager disabled returns 503.
    #[test]
    fn test_manage_disabled_returns_503() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let responses = VecDeque::new();
        let enabled = Arc::new(AtomicBool::new(false));
        let (port, shutdown, handle) = start_test_server_with_enabled(responses, 0, enabled);
        let client = reqwest::blocking::Client::new();

        let resp = client
            .post(format!("http://127.0.0.1:{}/manage", port))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503);

        stop_server(&shutdown, handle);
    }

    /// Busy-state behavior + mid-execution cache invalidation.
    /// Consolidates: test_busy_state_behavior, test_artifacts_cleared_immediately_when_job_starts.
    #[test]
    fn test_busy_and_delayed_jobs() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut responses = VecDeque::new();
        // Job 1: busy-state test (3s delay is enough — we check at 500ms)
        responses.push_back((make_result(&[("a", InstructionStatus::Success)]), None));
        // Job 2: populate snapshot for cache-clear test
        responses.push_back((
            make_result(&[("c", InstructionStatus::Success)]),
            Some(vec![make_artifact("GladiatorsFinale", "flower", 20, true)]),
        ));
        // Job 3: slow job, check cache cleared mid-execution
        responses.push_back((make_result(&[("d", InstructionStatus::Success)]), None));

        let (port, shutdown, handle) = start_test_server(responses, 3000);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        // === Busy-state checks ===

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let job_id = body["jobId"].as_str().unwrap().to_string();

        // Wait for job to start processing (past the 1s pre-delay)
        std::thread::sleep(Duration::from_millis(1500));

        // 409 when busy: second job rejected
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["b"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 409);

        // health shows busy during job
        let resp = client.get(format!("{}/health", base)).send().unwrap();
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["busy"], true);

        // result returns 409 when still running
        let resp = client
            .get(format!("{}/result?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 409);

        poll_until_completed(port);

        // === Cache cleared mid-execution ===

        // Populate cache
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["c"]))
            .send()
            .unwrap();
        let job_c = resp.json::<serde_json::Value>().unwrap()["jobId"]
            .as_str()
            .unwrap()
            .to_string();
        poll_until_completed(port);

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_c))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // Submit slow job and check cache while running
        client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["d"]))
            .send()
            .unwrap();

        // Wait past 1s pre-delay for execution to start
        std::thread::sleep(Duration::from_millis(1500));

        // Cache must already be invalidated mid-execution
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_c))
            .send()
            .unwrap();
        assert_ne!(
            resp.status().as_u16(),
            200,
            "/artifacts must be cleared as soon as a lock job starts, not after it finishes"
        );

        poll_until_completed(port);
        stop_server(&shutdown, handle);
    }

    #[test]
    fn reused_executor_checks_data_before_every_job() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let initial = DATA_PREPARATIONS.load(Ordering::SeqCst);
        let responses = VecDeque::from([
            (make_result(&[("a", InstructionStatus::Success)]), None),
            (make_result(&[("b", InstructionStatus::Success)]), None),
        ]);
        let (port, shutdown, handle) = start_test_server(responses, 0);
        let client = reqwest::blocking::Client::new();
        for (index, id) in ["a", "b"].iter().enumerate() {
            let response = client
                .post(format!("http://127.0.0.1:{port}/manage"))
                .header("Content-Type", "application/json")
                .body(make_manage_body(&[id]))
                .send()
                .unwrap();
            assert_eq!(response.status().as_u16(), 202);
            poll_until_completed(port);
            assert_eq!(
                DATA_PREPARATIONS.load(Ordering::SeqCst),
                initial + index + 1
            );
        }
        stop_server(&shutdown, handle);
    }

    /// Game init failure produces ui_error results for all items.
    #[test]
    fn test_game_init_failure_produces_ui_error_results() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");

        let port = next_port();
        let enabled = Arc::new(AtomicBool::new(true));
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();

        let handle = std::thread::spawn(move || {
            let init = move || -> anyhow::Result<Box<dyn ManageExecutor>> {
                Err(anyhow::anyhow!("Game window not found")
                    .context("initializing scanner controller"))
            };
            let _ = run_server(port, init, enabled, shutdown_clone, false, None);
        });

        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);
        for _ in 0..50 {
            if client.get(format!("{}/health", base)).send().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        // Submit job
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["x", "y"]))
            .send()
            .unwrap();
        let submit_body: serde_json::Value = resp.json().unwrap();
        let job_id = submit_body["jobId"].as_str().unwrap().to_string();
        poll_until_completed(port);

        // Check result
        let resp = client
            .get(format!("{}/result?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["status"], "ui_error");
        assert_eq!(results[1]["status"], "ui_error");
        for result in results {
            let message = result["message"].as_str().unwrap();
            assert!(message.starts_with(
                "The game or base data could not be prepared, so this operation was not performed."
            ));
            assert!(message.contains("\n\nFull error details:\n"));
            assert!(message.contains("initializing scanner controller: Game window not found"));
        }

        stop_server(&shutdown, handle);
    }

    fn make_character(key: &str, level: i32) -> GoodCharacter {
        GoodCharacter {
            key: key.to_string(),
            level,
            constellation: 0,
            ascension: 6,
            talent: GoodTalent {
                auto: 1,
                skill: 1,
                burst: 1,
            },
            element: None,
        }
    }

    fn make_weapon(key: &str, level: i32) -> GoodWeapon {
        GoodWeapon {
            key: key.to_string(),
            level,
            ascension: 6,
            refinement: 1,
            rarity: 5,
            location: String::new(),
            lock: false,
        }
    }

    /// Scan API: full E2E flow — submit, poll, fetch results from each data endpoint.
    /// Also tests: validation (empty targets), jobId mismatch, scan after manage updates artifact cache.
    #[test]
    fn test_scan_api_flow() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _lang = TestLanguageGuard::set("en");

        let manage_responses = VecDeque::new();
        let mut scan_responses: VecDeque<anyhow::Result<ScanResult>> = VecDeque::new();

        // Scan 1: all three targets complete
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::Complete(vec![
                make_character("Furina", 90),
                make_character("RaidenShogun", 80),
            ]),
            weapons: PhaseResult::Complete(vec![make_weapon("SkywardHarp", 90)]),
            artifacts: PhaseResult::Complete(vec![make_artifact(
                "GladiatorsFinale",
                "flower",
                20,
                true,
            )]),
            achievements: PhaseResult::NotAttempted,
        }));

        // Scan 2: characters only
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::Complete(vec![make_character("Nahida", 90)]),
            weapons: PhaseResult::NotAttempted,
            artifacts: PhaseResult::NotAttempted,
            achievements: PhaseResult::NotAttempted,
        }));

        // Scan 3: scan error
        scan_responses.push_back(Err(anyhow::anyhow!(
            "Game window not found / WINDOW_MARKER"
        )
        .context("initializing scan runtime")));

        // Scan 4: characters complete, weapons fail, artifacts are stopped.
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::Complete(vec![make_character("Furina", 90)]),
            weapons: PhaseResult::Failed(
                anyhow::anyhow!("inner scan marker / STATUS_MARKER")
                    .context("weapon scan phase failed"),
            ),
            artifacts: PhaseResult::Incomplete,
            achievements: PhaseResult::NotAttempted,
        }));

        // Scan 5: achievements only
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::NotAttempted,
            weapons: PhaseResult::NotAttempted,
            artifacts: PhaseResult::NotAttempted,
            achievements: PhaseResult::Complete(vec![81006, 84519]),
        }));

        let (port, shutdown, handle) =
            start_test_server_with_scans(manage_responses, scan_responses, 0);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        // === Validation: empty targets returns 400 ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":false,"weapons":false,"artifacts":false}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // all-false via defaults (empty object)
        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // recent artifact mode must target artifacts and must provide a limit
        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true,"artifactMode":"recent","artifactLimit":25}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"artifacts":true,"artifactMode":"recent"}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"artifacts":true,"artifactMode":"recent","artifactLimit":0}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // bad JSON
        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body("not json")
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // === Data endpoints: 400 without jobId ===

        let resp = client.get(format!("{}/characters", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let resp = client.get(format!("{}/weapons", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 400);
        let resp = client.get(format!("{}/achievements", base)).send().unwrap();
        assert_eq!(resp.status().as_u16(), 400);

        // === Scan 1: all targets ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true,"weapons":true,"artifacts":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let scan1_id = body["jobId"].as_str().unwrap().to_string();
        assert_eq!(body["targets"]["characters"], true);
        assert_eq!(body["targets"]["weapons"], true);
        assert_eq!(body["targets"]["artifacts"], true);
        assert_eq!(body["targets"]["achievements"], false);
        assert_eq!(body["artifactMode"], "all");
        assert!(body["artifactLimit"].is_null());

        poll_until_completed(port);

        // /status shows completed with 3 phases
        let resp = client.get(format!("{}/status", base)).send().unwrap();
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["state"], "completed");
        assert_eq!(body["summary"]["total"], 3);
        assert_eq!(body["summary"]["success"], 3);

        // /result returns per-phase results
        let resp = client
            .get(format!("{}/result?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        let results = body["results"].as_array().unwrap();
        assert_eq!(results.len(), 3);
        assert_eq!(results[0]["id"], "characters");
        assert_eq!(results[1]["id"], "weapons");
        assert_eq!(results[2]["id"], "artifacts");

        // /characters returns character data
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(body[0]["key"], "Furina");
        assert_eq!(body[1]["key"], "RaidenShogun");

        // /weapons returns weapon data
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["key"], "SkywardHarp");

        // /artifacts returns artifact data
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["setKey"], "GladiatorsFinale");

        // wrong jobId → 404
        let resp = client
            .get(format!("{}/characters?jobId=wrong", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let resp = client
            .get(format!("{}/weapons?jobId=wrong", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let resp = client
            .get(format!("{}/artifacts?jobId=wrong", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let resp = client
            .get(format!("{}/achievements?jobId=wrong", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Scan 2: characters only — weapons/artifacts keep scan1 data ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let scan2_id = body["jobId"].as_str().unwrap().to_string();
        assert_eq!(body["targets"]["characters"], true);
        assert_eq!(body["targets"]["weapons"], false);
        assert_eq!(body["targets"]["artifacts"], false);

        poll_until_completed(port);

        // /result shows 1 phase
        let resp = client
            .get(format!("{}/result?jobId={}", base, scan2_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["id"], "characters");

        // /characters with scan2 jobId returns new data
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan2_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["key"], "Nahida");

        // /characters with scan1 jobId → 404 (overwritten)
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // /weapons still has scan1 data (scan2 didn't scan weapons)
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body[0]["key"], "SkywardHarp");

        // /artifacts still has scan1 data
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // === Scan 3: error — caches not updated ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true,"weapons":true,"artifacts":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let scan3_id = body["jobId"].as_str().unwrap().to_string();

        poll_until_completed(port);

        // /result shows error
        let resp = client
            .get(format!("{}/result?jobId={}", base, scan3_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["summary"]["errors"], 1);
        assert_eq!(body["results"][0]["id"], "scan");
        assert_eq!(body["results"][0]["status"], "ui_error");
        let message = body["results"][0]["message"].as_str().unwrap();
        assert!(message.starts_with("The scan job encountered an error"));
        assert!(message.contains(
            "Full error details:\ninitializing scan runtime: Game window not found / WINDOW_MARKER"
        ));

        // Previous scan data still intact (error didn't wipe caches)
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan2_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // === Scan 4: characters complete, weapon failed, artifacts stopped ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true,"weapons":true,"artifacts":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let scan4_id = body["jobId"].as_str().unwrap().to_string();

        poll_until_completed(port);

        // /result: one success, one technical error, and one clean stop.
        let resp = client
            .get(format!("{}/result?jobId={}", base, scan4_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["summary"]["success"], 1);
        assert_eq!(body["summary"]["errors"], 1);
        assert_eq!(body["summary"]["aborted"], 1);
        let results = body["results"].as_array().unwrap();
        assert!(results[0].get("message").is_none());
        let weapon_result = results
            .iter()
            .find(|result| result["id"] == "weapons")
            .unwrap();
        assert_eq!(weapon_result["status"], "ui_error");
        let weapon_message = weapon_result["message"].as_str().unwrap();
        assert!(weapon_message.starts_with("The weapon scan encountered an error"));
        assert!(weapon_message.contains(
            "Full error details:\nweapon scan phase failed: inner scan marker / STATUS_MARKER"
        ));
        let artifact_result = results
            .iter()
            .find(|result| result["id"] == "artifacts")
            .unwrap();
        assert_eq!(artifact_result["status"], "aborted");
        let artifact_message = artifact_result["message"].as_str().unwrap();
        assert_eq!(
            artifact_message,
            "The artifact scan was stopped before it finished, so incomplete data was not published."
        );
        assert!(!artifact_message.contains("Full error details:"));

        // Completed phase: /characters returns 200 for scan4
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan4_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body[0]["key"], "Furina");

        // Aborted phases: /weapons and /artifacts return 503 for scan4
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan4_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503);
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, scan4_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 503);

        // Old jobIds are no longer the cached id for characters (overwritten by scan4);
        // nothing was written for weapons/artifacts in scan4, so scan1 data is still served.
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan2_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        // Unknown jobId still returns 404 (not 503).
        let resp = client
            .get(format!("{}/weapons?jobId=nonexistent", base))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 404);

        // === Scan 5: achievements only — other caches stay as they were ===

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"achievements":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let scan5_id = body["jobId"].as_str().unwrap().to_string();
        assert_eq!(body["targets"]["achievements"], true);
        assert_eq!(body["targets"]["characters"], false);
        assert_eq!(body["targets"]["weapons"], false);
        assert_eq!(body["targets"]["artifacts"], false);

        poll_until_completed(port);

        let resp = client
            .get(format!("{}/result?jobId={}", base, scan5_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["id"], "achievements");
        assert_eq!(body["results"][0]["status"], "success");

        let resp = client
            .get(format!("{}/achievements?jobId={}", base, scan5_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(
            body.as_array().unwrap(),
            &vec![serde_json::json!(81006), serde_json::json!(84519),]
        );

        // Earlier character/weapon/artifact caches are left alone.
        let resp = client
            .get(format!("{}/characters?jobId={}", base, scan4_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let resp = client
            .get(format!("{}/weapons?jobId={}", base, scan1_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);

        stop_server(&shutdown, handle);
    }

    #[test]
    fn test_scan_recent_artifact_options() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let manage_responses = VecDeque::new();
        let mut scan_responses: VecDeque<anyhow::Result<ScanResult>> = VecDeque::new();
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::NotAttempted,
            weapons: PhaseResult::NotAttempted,
            artifacts: PhaseResult::Complete(vec![make_artifact(
                "GladiatorsFinale",
                "flower",
                20,
                true,
            )]),
            achievements: PhaseResult::NotAttempted,
        }));

        let (port, shutdown, handle) =
            start_test_server_with_scans(manage_responses, scan_responses, 0);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"artifacts":true,"artifactMode":"recent","artifactLimit":25}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        let body: serde_json::Value = resp.json().unwrap();
        let job_id = body["jobId"].as_str().unwrap().to_string();
        assert_eq!(body["targets"]["artifacts"], true);
        assert_eq!(body["artifactMode"], "recent");
        assert_eq!(body["artifactLimit"], 25);

        poll_until_completed(port);

        let resp = client
            .get(format!("{}/result?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body["results"].as_array().unwrap().len(), 1);
        assert_eq!(body["results"][0]["id"], "artifacts");
        assert_eq!(body["results"][0]["status"], "success");

        let resp = client
            .get(format!("{}/artifacts?jobId={}", base, job_id))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        let body: serde_json::Value = resp.json().unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);

        stop_server(&shutdown, handle);
    }

    /// While a manage job is running, GET /status must expose intermediate
    /// `completed` values (not just 0 and N). Guards the full plumbing:
    /// `LockManager::execute` → progress_fn → JobState.progress →
    /// status_json → client response.
    #[test]
    fn test_manage_progress_visible_mid_run() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut responses = VecDeque::new();
        responses.push_back((
            make_result(&[
                ("a", InstructionStatus::Success),
                ("b", InstructionStatus::Success),
                ("c", InstructionStatus::Success),
                ("d", InstructionStatus::Success),
                ("e", InstructionStatus::Success),
            ]),
            None,
        ));

        // 1500ms total → 300ms between ticks. Gives the client plenty of time
        // to observe intermediate values with a tight poll loop.
        let (port, shutdown, handle) = start_test_server(responses, 1500);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a", "b", "c", "d", "e"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);

        // Poll for up to ~4s (1s pre-delay + 1.5s execution + slack).
        let mut observed: Vec<(u64, u64)> = Vec::new();
        let mut total_observed: Option<u64> = None;
        for _ in 0..80 {
            std::thread::sleep(Duration::from_millis(50));
            let resp = client.get(format!("{}/status", base)).send().unwrap();
            let body: serde_json::Value = resp.json().unwrap();
            if body["state"] == "running" {
                if let (Some(c), Some(t)) = (
                    body["progress"]["completed"].as_u64(),
                    body["progress"]["total"].as_u64(),
                ) {
                    if observed.last() != Some(&(c, t)) {
                        observed.push((c, t));
                    }
                    total_observed = Some(t);
                }
            } else if body["state"] == "completed" {
                break;
            }
        }

        poll_until_completed(port);

        // Every running snapshot must have reported total=5.
        assert_eq!(
            total_observed,
            Some(5),
            "total field not reported through /status; observed: {:?}",
            observed
        );

        // Must observe at least one intermediate tick (completed > 0 && < total).
        // If we only see [0] and [5] the client has no per-item feedback.
        let has_intermediate = observed.iter().any(|&(c, t)| c > 0 && c < t);
        assert!(has_intermediate,
            "expected /status to expose intermediate completed values (per-item progress); observed: {:?}",
            observed);

        // Completed must monotonically increase.
        for w in observed.windows(2) {
            assert!(
                w[1].0 >= w[0].0,
                "completed regressed: {:?} -> {:?}",
                w[0],
                w[1]
            );
        }

        stop_server(&shutdown, handle);
    }

    /// Scan's per-category progress must flow through /status.scanProgress.
    /// Each of the 3 categories has its own (completed, total, state) and all
    /// three move independently — this is what lets the client draw 3 bars.
    #[test]
    fn test_scan_progress_visible_mid_run() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let mut scan_responses: VecDeque<anyhow::Result<ScanResult>> = VecDeque::new();
        scan_responses.push_back(Ok(ScanResult {
            characters: PhaseResult::Complete(vec![make_character("Furina", 90)]),
            weapons: PhaseResult::Complete(vec![make_weapon("SkywardHarp", 90)]),
            artifacts: PhaseResult::Complete(vec![make_artifact(
                "GladiatorsFinale",
                "flower",
                20,
                true,
            )]),
            achievements: PhaseResult::NotAttempted,
        }));

        // 1500ms across 30 ticks (3 phases × 10 items each) → 50ms per tick.
        let (port, shutdown, handle) =
            start_test_server_with_scans(VecDeque::new(), scan_responses, 1500);
        let client = reqwest::blocking::Client::new();
        let base = format!("http://127.0.0.1:{}", port);

        let resp = client
            .post(format!("{}/scan", base))
            .header("Content-Type", "application/json")
            .body(r#"{"characters":true,"weapons":true,"artifacts":true}"#)
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);

        #[derive(Clone, Debug)]
        struct CatObs {
            completed: u64,
            total: u64,
            state: String,
        }
        let mut chars_obs: Vec<CatObs> = Vec::new();
        let mut weapons_obs: Vec<CatObs> = Vec::new();
        let mut artifacts_obs: Vec<CatObs> = Vec::new();

        let record = |obs: &mut Vec<CatObs>, node: &serde_json::Value| {
            if let (Some(c), Some(t), Some(s)) = (
                node["completed"].as_u64(),
                node["total"].as_u64(),
                node["state"].as_str(),
            ) {
                let entry = CatObs {
                    completed: c,
                    total: t,
                    state: s.to_string(),
                };
                if obs.last().map(|p| {
                    (p.completed, p.total, p.state.as_str())
                        == (entry.completed, entry.total, entry.state.as_str())
                }) != Some(true)
                {
                    obs.push(entry);
                }
            }
        };

        for _ in 0..120 {
            std::thread::sleep(Duration::from_millis(30));
            let resp = client.get(format!("{}/status", base)).send().unwrap();
            let body: serde_json::Value = resp.json().unwrap();
            if body["state"] == "running" {
                let sp = &body["scanProgress"];
                // progress.* should NOT be populated for scan jobs.
                assert!(
                    body["progress"].is_null(),
                    "scan should use scanProgress, not the linear progress field; body={}",
                    body
                );
                if sp.is_object() {
                    record(&mut chars_obs, &sp["characters"]);
                    record(&mut weapons_obs, &sp["weapons"]);
                    record(&mut artifacts_obs, &sp["artifacts"]);
                }
            } else if body["state"] == "completed" {
                break;
            }
        }

        poll_until_completed(port);

        // All three categories must have been observed.
        assert!(!chars_obs.is_empty(), "no characters progress observed");
        assert!(!weapons_obs.is_empty(), "no weapons progress observed");
        assert!(!artifacts_obs.is_empty(), "no artifacts progress observed");

        // Each category must report intermediate completed values (not just 0 and total).
        let has_mid = |obs: &[CatObs]| obs.iter().any(|o| o.completed > 0 && o.completed < o.total);
        assert!(
            has_mid(&chars_obs),
            "expected intermediate characters progress; observed: {:?}",
            chars_obs
        );
        assert!(
            has_mid(&weapons_obs),
            "expected intermediate weapons progress; observed: {:?}",
            weapons_obs
        );
        assert!(
            has_mid(&artifacts_obs),
            "expected intermediate artifacts progress; observed: {:?}",
            artifacts_obs
        );

        // Each category transitioned from pending → running (FakeExecutor emits
        // running ticks; pending is only visible if we poll before that category
        // starts, which isn't guaranteed at these timing but the terminal state
        // we care about is Running).
        assert!(chars_obs.iter().any(|o| o.state == "running"));
        assert!(weapons_obs.iter().any(|o| o.state == "running"));
        assert!(artifacts_obs.iter().any(|o| o.state == "running"));

        // Completed must monotonically increase within each category.
        for obs in [&chars_obs, &weapons_obs, &artifacts_obs] {
            for w in obs.windows(2) {
                assert!(
                    w[1].completed >= w[0].completed,
                    "completed regressed: {:?}",
                    obs
                );
            }
        }

        stop_server(&shutdown, handle);
    }

    /// Init failure must not poison the server: the second job gets a fresh
    /// attempt. This guards against `init_executor.take()` semantics where a
    /// single failure would leave `executor = None` forever.
    #[test]
    fn test_init_failure_does_not_poison_server() {
        let _guard = SERVER_LOCK.lock().unwrap_or_else(|e| e.into_inner());

        // Custom init closure that fails the first two attempts, then succeeds.
        let responses: VecDeque<(ManageResult, Option<Vec<GoodArtifact>>)> = {
            let mut q = VecDeque::new();
            q.push_back((make_result(&[("a", InstructionStatus::Success)]), None));
            q
        };
        let responses = Arc::new(Mutex::new(responses));
        let scan_responses: Arc<Mutex<VecDeque<anyhow::Result<ScanResult>>>> =
            Arc::new(Mutex::new(VecDeque::new()));

        let port = next_port();
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        let attempt_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let attempt_count_inner = attempt_count.clone();
        let responses_inner = responses.clone();
        let scan_responses_inner = scan_responses.clone();

        let handle = std::thread::spawn(move || {
            let init = move || -> anyhow::Result<Box<dyn ManageExecutor>> {
                let n = attempt_count_inner.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(anyhow::anyhow!("simulated init failure #{}", n))
                } else {
                    Ok(Box::new(FakeExecutor {
                        responses: responses_inner.clone(),
                        scan_responses: scan_responses_inner.clone(),
                        delay_ms: 0,
                    }))
                }
            };
            let _ = run_server(
                port,
                init,
                Arc::new(AtomicBool::new(true)),
                shutdown_clone,
                false,
                None,
            );
        });

        // Wait for server to come up.
        let client = reqwest::blocking::Client::new();
        let health_url = format!("http://127.0.0.1:{}/health", port);
        for _ in 0..50 {
            if client.get(&health_url).send().is_ok() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let base = format!("http://127.0.0.1:{}", port);

        // Job 1: init fails. Summary shows 1 error, server stays usable.
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["x"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        poll_until_completed(port);
        let body: serde_json::Value = client
            .get(format!("{}/status", base))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["summary"]["errors"], 1);

        // Job 2: init fails again — same 1 error summary, not a panic.
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["y"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        poll_until_completed(port);
        let body: serde_json::Value = client
            .get(format!("{}/status", base))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["summary"]["errors"], 1);

        // Job 3: init succeeds — job runs and reports Success.
        let resp = client
            .post(format!("{}/manage", base))
            .header("Content-Type", "application/json")
            .body(make_manage_body(&["a"]))
            .send()
            .unwrap();
        assert_eq!(resp.status().as_u16(), 202);
        poll_until_completed(port);
        let body: serde_json::Value = client
            .get(format!("{}/status", base))
            .send()
            .unwrap()
            .json()
            .unwrap();
        assert_eq!(body["summary"]["success"], 1);

        // Init was attempted 3 times (two failures + one success).
        assert_eq!(attempt_count.load(Ordering::SeqCst), 3);

        stop_server(&shutdown, handle);
    }
}
