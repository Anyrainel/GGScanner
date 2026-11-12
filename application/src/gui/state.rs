use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use anyhow::Context;
use genshin_scanner::cli::{GoodUserConfig, OcrEngineArgs, ScanCoreConfig};

thread_local! {
    /// Panic-hook details captured on the panicking thread. A surrounding
    /// `catch_unwind` can consume this after the hook runs, preserving the
    /// source location in the error card instead of only the panic payload.
    static LAST_PANIC_DETAILS: std::cell::RefCell<Option<String>> = const {
        std::cell::RefCell::new(None)
    };
}

/// Persist a complete error report next to the application. Callers can add
/// the returned path to their UI, or show this function's full source chain if
/// even the fallback report cannot be written.
pub fn persist_error_report(
    file_name: &str,
    full_error: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let base_dir = match std::env::current_exe() {
        Ok(path) => match path.parent() {
            Some(parent) => parent.to_path_buf(),
            None => std::env::current_dir()
                .context("the application path has no parent and the current directory is unavailable")?,
        },
        Err(executable_error) => std::env::current_dir().with_context(|| {
            format!(
                "neither the application directory nor current directory could be located; application-path error: {executable_error}"
            )
        })?,
    };
    let log_dir = base_dir.join("log");
    std::fs::create_dir_all(&log_dir).with_context(|| {
        format!(
            "error-report directory could not be created: {}",
            log_dir.display()
        )
    })?;
    let path = log_dir.join(file_name);
    std::fs::write(&path, full_error)
        .with_context(|| format!("error report could not be written: {}", path.display()))?;
    Ok(path)
}

/// Record and return the complete diagnostic available to the panic hook.
pub fn record_panic_details(info: &std::panic::PanicHookInfo<'_>) -> String {
    let payload = if let Some(message) = info.payload().downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = info.payload().downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown non-string payload".to_string()
    };
    let location = info
        .location()
        .map(|location| {
            format!(
                " at {}:{}:{}",
                location.file(),
                location.line(),
                location.column()
            )
        })
        .unwrap_or_default();
    let details = format!("panic: {}{}", payload, location);
    LAST_PANIC_DETAILS.with(|slot| *slot.borrow_mut() = Some(details.clone()));
    details
}

/// UI language.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    pub fn from_config_str(s: &str) -> Self {
        if s == "en" {
            Lang::En
        } else {
            Lang::Zh
        }
    }

    pub fn to_str(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }

    /// Pick the right string based on current language.
    pub fn t<'a>(self, zh: &'a str, en: &'a str) -> &'a str {
        match self {
            Lang::Zh => zh,
            Lang::En => en,
        }
    }
}

/// Bilingual text intended for visible GUI state.
#[derive(Clone, Debug, PartialEq)]
pub struct UiText {
    zh: String,
    en: String,
}

impl UiText {
    pub fn new(zh: impl Into<String>, en: impl Into<String>) -> Self {
        Self {
            zh: zh.into(),
            en: en.into(),
        }
    }

    pub fn from_bilingual(msg: impl AsRef<str>) -> Self {
        let msg = msg.as_ref();
        if let Some(idx) = msg.find(" / ") {
            Self::new(&msg[..idx], &msg[idx + 3..])
        } else {
            Self::new(msg, msg)
        }
    }

    pub fn text(&self, lang: Lang) -> &str {
        match lang {
            Lang::Zh => &self.zh,
            Lang::En => &self.en,
        }
    }
}

/// Background operation that failed. Kept small and copyable so native crash
/// handlers can record it without allocating.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskKind {
    Scanner,
    Manager,
    Capture,
}

impl TaskKind {
    pub fn text(self, lang: Lang) -> &'static str {
        match (self, lang) {
            (Self::Scanner, Lang::Zh) => "扫描器",
            (Self::Scanner, Lang::En) => "Scanner",
            (Self::Manager, Lang::Zh) => "管理器",
            (Self::Manager, Lang::En) => "Manager",
            (Self::Capture, Lang::Zh) => "抓包器",
            (Self::Capture, Lang::En) => "Capture",
        }
    }
}

/// Memory operation reported with Windows exception `0xC0000005`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeMemoryOperation {
    Read,
    Write,
    Execute,
    Unknown(usize),
}

impl NativeMemoryOperation {
    pub fn from_windows_value(value: usize) -> Self {
        match value {
            0 => Self::Read,
            1 => Self::Write,
            8 => Self::Execute,
            other => Self::Unknown(other),
        }
    }

    fn text(self, lang: Lang) -> String {
        match (self, lang) {
            (Self::Read, Lang::Zh) => "读取 / read".to_string(),
            (Self::Read, Lang::En) => "read".to_string(),
            (Self::Write, Lang::Zh) => "写入 / write".to_string(),
            (Self::Write, Lang::En) => "write".to_string(),
            (Self::Execute, Lang::Zh) => "执行 / execute".to_string(),
            (Self::Execute, Lang::En) => "execute".to_string(),
            (Self::Unknown(value), Lang::Zh) => format!("未知 ({}) / unknown", value),
            (Self::Unknown(value), Lang::En) => format!("unknown ({})", value),
        }
    }
}

/// Native exception information plus the phase that the GUI moves out of the
/// worker status after observing the crash record.
///
/// Formatting and phase capture are intentionally deferred to the GUI thread.
/// The exception handler only writes preallocated atomic fields while the
/// process may be in a compromised state.
#[derive(Clone, Debug, PartialEq)]
pub struct NativeException {
    pub task: TaskKind,
    pub phase: Option<UiText>,
    pub code: u32,
    pub fault_address: usize,
    pub memory_operation: Option<NativeMemoryOperation>,
    pub attempted_address: Option<usize>,
}

impl NativeException {
    pub fn new(
        task: TaskKind,
        phase: Option<UiText>,
        code: u32,
        fault_address: usize,
        memory_operation: Option<NativeMemoryOperation>,
        attempted_address: Option<usize>,
    ) -> Self {
        Self {
            task,
            phase,
            code,
            fault_address,
            memory_operation,
            attempted_address,
        }
    }

    fn canonical_name(&self) -> &'static str {
        match self.code {
            0xC0000005 => "Access Violation",
            0xC00000FD => "Stack Overflow",
            0xC0000094 => "Integer Divide by Zero",
            0xC000001D => "Illegal Instruction",
            0xC0000096 => "Privileged Instruction",
            0xC0000374 => "Heap Corruption",
            _ => "Unknown Native Exception",
        }
    }

    fn localized_name(&self, lang: Lang) -> &'static str {
        if lang == Lang::En {
            return self.canonical_name();
        }
        match self.code {
            0xC0000005 => "访问违规",
            0xC00000FD => "栈溢出",
            0xC0000094 => "整数除零",
            0xC000001D => "非法指令",
            0xC0000096 => "特权指令",
            0xC0000374 => "堆损坏",
            _ => "未知原生异常",
        }
    }
}

/// A user-facing explanation paired with the original technical error.
///
/// Every visible failure uses this type so the localized hint is never
/// replaced by an opaque library/OS message, while the exact inner error
/// remains available for copying, searching, and support.
#[derive(Clone, Debug, PartialEq)]
pub enum UiError {
    Detailed {
        hint: UiText,
        technical_details: String,
    },
    NativeException(NativeException),
}

impl UiError {
    pub fn from_anyhow(hint: UiText, error: &anyhow::Error) -> Self {
        Self::Detailed {
            hint,
            // Alternate Display preserves anyhow's complete source chain.
            technical_details: format!("{:#}", error),
        }
    }

    pub fn from_error(hint: UiText, error: impl std::error::Error + 'static) -> Self {
        let mut technical_details = error.to_string();
        let mut source = error.source();
        while let Some(cause) = source {
            technical_details.push_str("\nCaused by: ");
            technical_details.push_str(&cause.to_string());
            source = cause.source();
        }
        Self::Detailed {
            hint,
            technical_details,
        }
    }

    pub fn from_message(hint: UiText, technical_details: impl Into<String>) -> Self {
        Self::Detailed {
            hint,
            technical_details: technical_details.into(),
        }
    }

    pub fn from_panic(hint: UiText, panic_info: &(dyn std::any::Any + Send)) -> Self {
        let technical_details = LAST_PANIC_DETAILS
            .with(|slot| slot.borrow_mut().take())
            .unwrap_or_else(|| {
                if let Some(message) = panic_info.downcast_ref::<&str>() {
                    format!("panic: {}", message)
                } else if let Some(message) = panic_info.downcast_ref::<String>() {
                    format!("panic: {}", message)
                } else {
                    "panic: unknown non-string payload".to_string()
                }
            });
        Self::from_message(hint, technical_details)
    }

    pub fn native_exception(exception: NativeException) -> Self {
        Self::NativeException(exception)
    }

    pub fn is_native_exception(&self) -> bool {
        matches!(self, Self::NativeException(_))
    }

    pub fn hint_text(&self, lang: Lang) -> String {
        match self {
            Self::Detailed { hint, .. } => hint.text(lang).to_string(),
            Self::NativeException(exception) => match (exception.task, lang) {
                (TaskKind::Scanner, Lang::Zh) =>
                    "扫描器已停止，因为底层 Windows 组件发生崩溃。为避免损坏后续操作，请重启本程序；完整错误可用于搜索或寻求帮助。".to_string(),
                (TaskKind::Scanner, Lang::En) =>
                    "The scanner stopped because a low-level Windows component crashed. Restart this application before doing anything else; the full error can be searched or shared for help.".to_string(),
                (TaskKind::Manager, Lang::Zh) =>
                    "管理器已停止，因为底层 Windows 组件发生崩溃。为避免损坏后续操作，请重启本程序；完整错误可用于搜索或寻求帮助。".to_string(),
                (TaskKind::Manager, Lang::En) =>
                    "The manager stopped because a low-level Windows component crashed. Restart this application before doing anything else; the full error can be searched or shared for help.".to_string(),
                (TaskKind::Capture, Lang::Zh) =>
                    "抓包器已停止，因为底层 Windows 组件发生崩溃。为避免损坏后续操作，请重启本程序；完整错误可用于搜索或寻求帮助。".to_string(),
                (TaskKind::Capture, Lang::En) =>
                    "Capture stopped because a low-level Windows component crashed. Restart this application before doing anything else; the full error can be searched or shared for help.".to_string(),
            },
        }
    }

    pub fn technical_details(&self, lang: Lang) -> String {
        match self {
            Self::Detailed {
                technical_details, ..
            } => technical_details.clone(),
            Self::NativeException(exception) => {
                let mut lines = vec![
                    match lang {
                        Lang::Zh => format!("任务: {}", exception.task.text(lang)),
                        Lang::En => format!("Task: {}", exception.task.text(lang)),
                    },
                    match lang {
                        Lang::Zh => format!(
                            "Windows 异常: 0x{:08X} ({} / {})",
                            exception.code,
                            exception.canonical_name(),
                            exception.localized_name(lang)
                        ),
                        Lang::En => format!(
                            "Windows exception: 0x{:08X} ({})",
                            exception.code,
                            exception.canonical_name()
                        ),
                    },
                    match lang {
                        Lang::Zh => format!("故障指令地址: 0x{:X}", exception.fault_address),
                        Lang::En => format!(
                            "Faulting instruction address: 0x{:X}",
                            exception.fault_address
                        ),
                    },
                ];

                if let Some(phase) = &exception.phase {
                    lines.insert(
                        1,
                        match lang {
                            Lang::Zh => format!("发生时的步骤: {}", phase.text(lang)),
                            Lang::En => format!("Step when it happened: {}", phase.text(lang)),
                        },
                    );
                }
                if let Some(operation) = exception.memory_operation {
                    lines.push(match lang {
                        Lang::Zh => format!("内存操作: {}", operation.text(lang)),
                        Lang::En => format!("Memory operation: {}", operation.text(lang)),
                    });
                }
                if let Some(address) = exception.attempted_address {
                    lines.push(match lang {
                        Lang::Zh => format!("尝试访问的地址: 0x{:X}", address),
                        Lang::En => format!("Attempted memory address: 0x{:X}", address),
                    });
                }
                if exception.code == 0xC0000005 {
                    lines.push(match lang {
                        Lang::Zh => "说明: 底层组件尝试访问无效内存。这不是文件权限或管理员权限错误。".to_string(),
                        Lang::En => "Meaning: A low-level component attempted to access invalid memory. This is not a file-permission or administrator-access error.".to_string(),
                    });
                }
                lines.join("\n")
            },
        }
    }

    /// Log the diagnostic details behind the concise UI hint. The hint is
    /// already visible in the status card, so logs should add the cause users
    /// need when they share the log window for support.
    pub fn log_diagnostics(&self) {
        let lang = if yas::lang::is_en() {
            Lang::En
        } else {
            Lang::Zh
        };
        let details = match self {
            Self::Detailed {
                technical_details, ..
            } => yas::lang::localize(technical_details),
            Self::NativeException(_) => self.technical_details(lang),
        };
        yas::log_error!(
            "任务失败。诊断详情：{}",
            "Task failed. Diagnostic details: {}",
            details
        );
    }

    pub fn copy_text(&self, lang: Lang) -> String {
        format!(
            "{}\n\n{}\n{}",
            self.hint_text(lang),
            lang.t("完整错误详情:", "Full error details:"),
            self.technical_details(lang)
        )
    }
}

impl std::fmt::Display for UiText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(if yas::lang::is_en() {
            &self.en
        } else {
            &self.zh
        })
    }
}

/// Status of a background operation.
#[derive(Clone, Debug, PartialEq)]
pub enum TaskStatus {
    Idle,
    Running(UiText),
    Completed(UiText),
    Stopped(UiText),
    AwaitingInput(UiText),
    Exported {
        message: UiText,
        path: String,
        partial: bool,
    },
    Failed(UiError),
}

impl From<UiText> for TaskStatus {
    fn from(message: UiText) -> Self {
        Self::Completed(message)
    }
}

/// Which tab a log entry belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    Scanner,
    Manager,
}

/// A single log entry displayed in the log panel.
#[derive(Clone, Debug)]
pub struct LogEntry {
    pub level: log::Level,
    pub message: String,
    pub timestamp: String,
    pub source: LogSource,
}

/// Lock-free producer queue plus a GUI-owned retained snapshot. Background
/// workers never hold the snapshot mutex, so a native thread exit cannot
/// strand the window while preserving every pending diagnostic.
pub struct LogStore {
    pending: crossbeam_queue::SegQueue<LogEntry>,
    lines: Mutex<Vec<LogEntry>>,
    max_lines: usize,
}

impl LogStore {
    pub fn new(max_lines: usize) -> Self {
        Self {
            pending: crossbeam_queue::SegQueue::new(),
            lines: Mutex::new(Vec::with_capacity(max_lines.min(1000))),
            max_lines,
        }
    }

    pub fn push(&self, entry: LogEntry) {
        self.pending.push(entry);
    }

    pub fn snapshot(&self) -> Vec<LogEntry> {
        let mut lines = match self.lines.lock() {
            Ok(lines) => lines,
            Err(poisoned) => {
                self.lines.clear_poison();
                poisoned.into_inner()
            },
        };
        while let Some(entry) = self.pending.pop() {
            lines.push(entry);
        }
        if lines.len() > self.max_lines {
            let excess = lines.len() - self.max_lines;
            lines.drain(0..excess);
        }
        lines.clone()
    }

    pub fn clear(&self) {
        while self.pending.pop().is_some() {}
        match self.lines.lock() {
            Ok(mut lines) => lines.clear(),
            Err(poisoned) => {
                self.lines.clear_poison();
                poisoned.into_inner().clear();
            },
        }
    }
}

/// State of the auto-update check.
#[derive(Clone, Debug)]
pub enum UpdateState {
    /// Background check in progress.
    Checking,
    /// A newer version is available.
    Available {
        latest_version: String,
        download_url: String,
    },
    /// Download is in progress.
    Downloading,
    /// Update downloaded and applied — showing restart dialog.
    ShowingDialog,
    /// User accepted the restart; the GUI performs the shutdown and handoff.
    RestartRequested(std::path::PathBuf),
    /// Update downloaded, user chose to restart later.
    Ready,
    /// Already on the latest version (or dev build).
    None,
    /// Check or download failed (non-fatal).
    Failed(UiError),
}

/// State of a one-shot data refresh operation.
pub enum RefreshState {
    Idle,
    Running(std::thread::JoinHandle<Result<(), UiError>>),
    Ok,
    Failed(UiError),
}

impl RefreshState {
    /// Poll the background thread; transition Running → Ok/Failed when done.
    pub fn poll(&mut self) {
        let finished = matches!(self, RefreshState::Running(h) if h.is_finished());
        if finished {
            let old = std::mem::replace(self, RefreshState::Idle);
            if let RefreshState::Running(h) = old {
                match h.join() {
                    Ok(Ok(())) => *self = RefreshState::Ok,
                    Ok(Err(msg)) => {
                        msg.log_diagnostics();
                        *self = RefreshState::Failed(msg);
                    },
                    Err(panic_info) => {
                        let error = UiError::from_panic(
                            UiText::new(
                                "无法刷新游戏数据，因为后台任务意外停止。",
                                "Game data could not be refreshed because the background task stopped unexpectedly.",
                            ),
                            panic_info.as_ref(),
                        );
                        error.log_diagnostics();
                        *self = RefreshState::Failed(error)
                    },
                }
            }
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, RefreshState::Running(_))
    }
}

/// Debounced user-config persistence shared by GGScannerOCR and GGScanner.
pub fn save_config_debounced(
    user_config: &GoodUserConfig,
    config_snapshot: &mut String,
    config_dirty_since: &mut Option<Instant>,
) {
    let current = serde_json::to_string(user_config).unwrap_or_default();
    if current != *config_snapshot {
        // Config changed — start/reset the debounce timer
        *config_dirty_since = Some(Instant::now());
        *config_snapshot = current;
    }
    if let Some(since) = *config_dirty_since {
        if since.elapsed() >= std::time::Duration::from_millis(300) {
            write_config_snapshot(user_config, config_snapshot, config_dirty_since);
        }
    }
}

/// Flush a pending debounced save, if any.
pub fn flush_pending_config_save(
    user_config: &GoodUserConfig,
    config_snapshot: &mut String,
    config_dirty_since: &mut Option<Instant>,
) {
    if config_dirty_since.is_some() {
        write_config_snapshot(user_config, config_snapshot, config_dirty_since);
    }
}

/// Sync capture-tab fields and write config immediately (before exit).
pub fn persist_user_config_now(
    user_config: &GoodUserConfig,
    config_snapshot: &mut String,
    config_dirty_since: &mut Option<Instant>,
) {
    write_config_snapshot(user_config, config_snapshot, config_dirty_since);
}

fn write_config_snapshot(
    user_config: &GoodUserConfig,
    config_snapshot: &mut String,
    config_dirty_since: &mut Option<Instant>,
) {
    if let Err(e) = genshin_scanner::cli::save_config(user_config) {
        yas::log_warn!(
            "设置无法保存；本次更改可能在重启后丢失。完整错误详情: {:#}",
            "Settings could not be saved; these changes may be lost after restart. Full error details: {:#}",
            e
        );
    }
    *config_snapshot = serde_json::to_string(user_config).unwrap_or_default();
    *config_dirty_since = None;
}

/// Shared state between GUI thread and background workers.
pub struct AppState {
    // --- Language ---
    pub lang: Lang,

    // --- Auto-update ---
    pub update_state: Arc<Mutex<UpdateState>>,

    // --- Scanner tab config ---
    pub user_config: GoodUserConfig,
    pub scan_characters: bool,
    pub scan_weapons: bool,
    pub scan_artifacts: bool,
    pub scan_achievements: bool,
    pub verbose: bool,
    pub stop_on_failure: bool,
    pub dump_images: bool,
    pub hdr_mode: bool,
    pub dump_job_data: bool,
    // Retained for old saved preferences and previews; scans always save on stop.
    pub only_keep_latest_export: bool,
    pub output_dir: String,
    pub char_max_count: usize,
    pub weapon_max_count: usize,
    pub artifact_max_count: usize,
    pub achievement_max_count: usize,

    /// Set to true when Start Scan is pressed but a required character name is empty.
    /// Forces the Character Names section open with a warning.
    pub names_need_attention: bool,

    /// Snapshot of user_config for change detection (debounced auto-save).
    pub config_snapshot: String,
    /// When Some, a config change was detected and save is pending after 300ms.
    pub config_dirty_since: Option<Instant>,

    // --- Scanner task ---
    pub scan_status: Arc<Mutex<TaskStatus>>,
    pub scan_progress: Arc<Mutex<super::task_progress::TaskProgress>>,

    // --- Manager tab config ---
    pub server_port: u16,
    /// Controls whether POST /manage requests are executed or rejected (503).
    /// Shared with the server thread via Arc.
    pub server_enabled: Arc<AtomicBool>,
    pub server_status: Arc<Mutex<TaskStatus>>,
    pub server_job: Arc<Mutex<genshin_scanner::manager::models::JobState>>,
    #[cfg(all(feature = "capture", target_os = "windows"))]
    pub server_capture_state: Arc<Mutex<genshin_scanner::capture::monitor::CaptureState>>,
    // --- Per-tab log buffers ---
    pub scanner_log_lines: Arc<LogStore>,
    pub manager_log_lines: Arc<LogStore>,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        let user_config = genshin_scanner::cli::load_config_or_default();
        let lang = Lang::from_config_str(&user_config.lang);
        let config_snapshot = serde_json::to_string(&user_config).unwrap_or_default();
        Self {
            lang,
            scan_characters: user_config.scan_characters,
            scan_weapons: user_config.scan_weapons,
            scan_artifacts: user_config.scan_artifacts,
            scan_achievements: user_config.scan_achievements,
            verbose: user_config.verbose,
            stop_on_failure: user_config.stop_on_failure,
            dump_images: user_config.dump_images,
            hdr_mode: user_config.hdr_mode,
            dump_job_data: user_config.dump_job_data,
            only_keep_latest_export: user_config.only_keep_latest_export,
            char_max_count: user_config.char_max_count,
            weapon_max_count: user_config.weapon_max_count,
            artifact_max_count: user_config.artifact_max_count,
            achievement_max_count: user_config.achievement_max_count,
            server_port: user_config.server_port,
            user_config,
            update_state: Arc::new(Mutex::new(UpdateState::Checking)),
            output_dir: genshin_scanner::cli::exe_dir().display().to_string(),
            names_need_attention: false,
            config_snapshot,
            config_dirty_since: None,
            scan_status: Arc::new(Mutex::new(TaskStatus::Idle)),
            scan_progress: Arc::new(Mutex::new(super::task_progress::TaskProgress::default())),
            server_enabled: Arc::new(AtomicBool::new(true)),
            server_status: Arc::new(Mutex::new(TaskStatus::Idle)),
            server_job: Arc::new(Mutex::new(
                genshin_scanner::manager::models::JobState::idle(),
            )),
            #[cfg(all(feature = "capture", target_os = "windows"))]
            server_capture_state: Arc::new(Mutex::new(Default::default())),
            scanner_log_lines: Arc::new(LogStore::new(2000)),
            manager_log_lines: Arc::new(LogStore::new(2000)),
        }
    }

    /// Shorthand for language selection.
    pub fn t<'a>(&self, zh: &'a str, en: &'a str) -> &'a str {
        self.lang.t(zh, en)
    }

    /// Character-name fields that must be filled before starting a scan.
    pub fn missing_required_character_names(&self) -> bool {
        self.needs_character_names() && self.user_config.traveler_name.trim().is_empty()
    }

    /// Inventory OCR also resolves equipped owners through renamed characters.
    pub fn needs_character_names(&self) -> bool {
        self.scan_characters || self.scan_weapons || self.scan_artifacts
    }

    /// Sync GUI fields back into user_config so they get serialized on save.
    fn sync_to_config(&mut self) {
        self.user_config.lang = self.lang.to_str().to_string();
        self.user_config.scan_characters = self.scan_characters;
        self.user_config.scan_weapons = self.scan_weapons;
        self.user_config.scan_artifacts = self.scan_artifacts;
        self.user_config.scan_achievements = self.scan_achievements;
        self.user_config.verbose = self.verbose;
        self.user_config.stop_on_failure = self.stop_on_failure;
        self.user_config.dump_images = self.dump_images;
        self.user_config.hdr_mode = self.hdr_mode;
        self.user_config.hdr_white_point = yas::capture::constants::DEFAULT_HDR_WHITE_POINT;
        self.user_config.dump_job_data = self.dump_job_data;
        self.user_config.only_keep_latest_export = self.only_keep_latest_export;
        self.user_config.char_max_count = self.char_max_count;
        self.user_config.weapon_max_count = self.weapon_max_count;
        self.user_config.artifact_max_count = self.artifact_max_count;
        self.user_config.achievement_max_count = self.achievement_max_count;
        self.user_config.server_port = self.server_port;
    }

    /// Check if user_config changed, and if so, schedule a debounced save.
    /// Call this once per frame from the main update loop.
    pub fn auto_save_tick(&mut self) {
        self.sync_to_config();
        save_config_debounced(
            &self.user_config,
            &mut self.config_snapshot,
            &mut self.config_dirty_since,
        );
    }

    /// Sync all GUI fields and write config immediately (before scan/server start or exit).
    pub fn persist_config_now(&mut self) {
        self.sync_to_config();
        write_config_snapshot(
            &self.user_config,
            &mut self.config_snapshot,
            &mut self.config_dirty_since,
        );
    }

    /// Build a ScanCoreConfig from current UI state.
    pub fn to_scan_config(&self) -> ScanCoreConfig {
        ScanCoreConfig {
            scan_characters: self.scan_characters,
            scan_weapons: self.scan_weapons,
            scan_artifacts: self.scan_artifacts,
            scan_achievements: self.scan_achievements,
            weapon_min_rarity: 3,
            artifact_min_rarity: 4,
            verbose: self.verbose,
            stop_on_failure: self.stop_on_failure,
            log_progress: false,
            dump_images: self.dump_images,
            hdr_mode: self.hdr_mode,
            hdr_white_point: yas::capture::constants::DEFAULT_HDR_WHITE_POINT,
            output_dir: self.output_dir.clone(),
            ocr: OcrEngineArgs::default(),
            char_max_count: self.char_max_count,
            weapon_max_count: self.weapon_max_count,
            artifact_max_count: self.artifact_max_count,
            achievement_max_count: self.achievement_max_count,
            artifact_keep_five_star_filter: false,
            debug_ach_scroll: false,
        }
    }
}
