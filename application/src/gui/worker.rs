use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use std::thread::{self, JoinHandle};

use super::log_bridge;
use super::state::{
    AppState, LogSource, NativeException, NativeMemoryOperation, TaskKind, TaskStatus, UiError,
    UiText,
};

// ── Windows SEH guard ────────────────────────────────────────────
//
// `catch_unwind` only catches Rust panics. On Windows, selected native
// crashes (such as access violations) raise SEH exceptions that bypass it.
//
// We install a last-chance unhandled-exception filter (process-wide, installed
// once) that recognizes only threads explicitly enrolled in the active task.
// This deliberately runs only after frame-based handlers have declined the
// exception, so we do not consume a first-chance exception that a native
// dependency intended to handle. If an enrolled task thread faults:
//   1. Copy the native diagnostic into atomics without allocating or locking.
//   2. End only that worker thread with `ExitThread(1)` — the GUI keeps
//      running long enough to display/copy the error and require a restart.
// The GUI thread then turns those atomics into a localized `UiError`.

pub(super) struct NativeCrashState {
    occurred: AtomicBool,
    code: AtomicU32,
    fault_address: AtomicUsize,
    has_memory_details: AtomicBool,
    memory_operation: AtomicUsize,
    attempted_address: AtomicUsize,
}

/// The GUI permits only one game-facing task at a time. This pointer is read
/// only during normal worker enrollment; the exception filter itself uses the
/// lock-free per-thread registry below, so unrelated threads are never guessed
/// to belong to the active scanner/capture task.
#[cfg(target_os = "windows")]
static ACTIVE_NATIVE_CRASH: AtomicPtr<NativeCrashState> = AtomicPtr::new(std::ptr::null_mut());

impl NativeCrashState {
    pub(super) fn new() -> Self {
        Self {
            occurred: AtomicBool::new(false),
            code: AtomicU32::new(0),
            fault_address: AtomicUsize::new(0),
            has_memory_details: AtomicBool::new(false),
            memory_operation: AtomicUsize::new(0),
            attempted_address: AtomicUsize::new(0),
        }
    }

    fn record(&self, code: u32, fault_address: usize, memory_details: Option<(usize, usize)>) {
        self.code.store(code, Ordering::Relaxed);
        self.fault_address.store(fault_address, Ordering::Relaxed);
        if let Some((operation, address)) = memory_details {
            self.memory_operation.store(operation, Ordering::Relaxed);
            self.attempted_address.store(address, Ordering::Relaxed);
            self.has_memory_details.store(true, Ordering::Relaxed);
        } else {
            self.has_memory_details.store(false, Ordering::Relaxed);
        }
        // Publish last so an Acquire load observes every diagnostic field.
        self.occurred.store(true, Ordering::Release);
    }

    fn to_exception(&self, task: TaskKind, phase: Option<UiText>) -> NativeException {
        let has_memory_details = self.has_memory_details.load(Ordering::Relaxed);
        NativeException::new(
            task,
            phase,
            self.code.load(Ordering::Relaxed),
            self.fault_address.load(Ordering::Relaxed),
            has_memory_details.then(|| {
                NativeMemoryOperation::from_windows_value(
                    self.memory_operation.load(Ordering::Relaxed),
                )
            }),
            has_memory_details.then(|| self.attempted_address.load(Ordering::Relaxed)),
        )
    }

    pub(super) fn has_occurred(&self) -> bool {
        self.occurred.load(Ordering::Acquire)
    }

    pub(super) fn claim_exception(
        &self,
        task: TaskKind,
        phase: Option<UiText>,
    ) -> Option<NativeException> {
        self.occurred
            .compare_exchange(true, false, Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| self.to_exception(task, phase))
    }
}

#[cfg(target_os = "windows")]
mod seh_guard {
    use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicUsize, Ordering};
    use std::sync::Once;

    use super::{NativeCrashState, ACTIVE_NATIVE_CRASH};

    struct EnrolledThread {
        thread_id: AtomicU32,
        state: AtomicPtr<NativeCrashState>,
        next: AtomicPtr<EnrolledThread>,
    }

    impl EnrolledThread {
        fn new(thread_id: u32) -> Self {
            Self {
                thread_id: AtomicU32::new(thread_id),
                state: AtomicPtr::new(std::ptr::null_mut()),
                next: AtomicPtr::new(std::ptr::null_mut()),
            }
        }
    }

    /// Lock-free registry read by the exception filter. Registry nodes are
    /// allocated only during ordinary thread enrollment and retained for the
    /// process lifetime, so the filter never allocates, locks, initializes Rust
    /// TLS, or silently loses coverage on machines with many worker threads.
    static ENROLLED_THREADS: AtomicPtr<EnrolledThread> = AtomicPtr::new(std::ptr::null_mut());

    // ── one-time global handler installation ───────────────────────
    static INSTALLED: Once = Once::new();
    static PREVIOUS_FILTER: AtomicUsize = AtomicUsize::new(0);

    pub fn install_global_handler() {
        INSTALLED.call_once(|| unsafe {
            yas::native_crash::install_thread_hooks(enter_active_context, restore_context);
            let previous =
                windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(
                    unhandled_exception_filter,
                ));
            PREVIOUS_FILTER.store(
                previous.map_or(0, |filter| filter as usize),
                Ordering::Release,
            );
        });
    }

    fn find_thread_slot(thread_id: u32) -> Option<&'static EnrolledThread> {
        let mut current = ENROLLED_THREADS.load(Ordering::Acquire);
        while !current.is_null() {
            let slot = unsafe { &*current };
            if slot.thread_id.load(Ordering::Acquire) == thread_id {
                return Some(slot);
            }
            current = slot.next.load(Ordering::Acquire);
        }
        None
    }

    fn current_thread_slot() -> &'static EnrolledThread {
        let thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        if let Some(slot) = find_thread_slot(thread_id) {
            return slot;
        }

        // Reuse a slot from a thread whose outermost task context ended.
        let mut current = ENROLLED_THREADS.load(Ordering::Acquire);
        while !current.is_null() {
            let slot = unsafe { &*current };
            if slot
                .thread_id
                .compare_exchange(0, thread_id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return slot;
            }
            current = slot.next.load(Ordering::Acquire);
        }

        // No reusable slot exists. Publish one new permanent registry node.
        // This runs during normal enrollment, never in the exception filter.
        let new_slot = Box::into_raw(Box::new(EnrolledThread::new(thread_id)));
        loop {
            let head = ENROLLED_THREADS.load(Ordering::Acquire);
            unsafe { (*new_slot).next.store(head, Ordering::Relaxed) };
            if ENROLLED_THREADS
                .compare_exchange(head, new_slot, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return unsafe { &*new_slot };
            }
        }
    }

    fn enter_active_context() -> usize {
        let slot = current_thread_slot();
        slot.state.swap(
            ACTIVE_NATIVE_CRASH.load(Ordering::Acquire),
            Ordering::AcqRel,
        ) as usize
    }

    fn restore_context(previous: usize) {
        let thread_id = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() };
        let Some(slot) = find_thread_slot(thread_id) else {
            return;
        };
        slot.state.store(previous as *mut _, Ordering::Release);
        if previous == 0 {
            let _ =
                slot.thread_id
                    .compare_exchange(thread_id, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }

    pub fn clear_state(state: &NativeCrashState) {
        let state_ptr = std::ptr::from_ref(state).cast_mut();
        let mut current = ENROLLED_THREADS.load(Ordering::Acquire);
        while !current.is_null() {
            let slot = unsafe { &*current };
            if slot
                .state
                .compare_exchange(
                    state_ptr,
                    std::ptr::null_mut(),
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                slot.thread_id.store(0, Ordering::Release);
            }
            current = slot.next.load(Ordering::Acquire);
        }
    }

    unsafe fn delegate_to_previous_filter(
        info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    ) -> i32 {
        const EXCEPTION_CONTINUE_SEARCH: i32 = 0;
        let previous = PREVIOUS_FILTER.load(Ordering::Acquire);
        if previous == 0 {
            return EXCEPTION_CONTINUE_SEARCH;
        }
        let filter: unsafe extern "system" fn(
            *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
        ) -> i32 = std::mem::transmute(previous);
        filter(info)
    }

    /// Native exceptions for which terminating only the faulting background thread is a
    /// defensible last resort. Stack overflow and heap corruption are excluded:
    /// continuing the process after either one cannot be considered safe.
    fn is_worker_recoverable(code: i32) -> bool {
        const ACCESS_VIOLATION: i32 = 0xC0000005_u32 as i32;
        const INT_DIVIDE_BY_ZERO: i32 = 0xC0000094_u32 as i32;
        const ILLEGAL_INSTRUCTION: i32 = 0xC000001D_u32 as i32;
        const PRIVILEGED_INSTRUCTION: i32 = 0xC0000096_u32 as i32;

        matches!(
            code,
            ACCESS_VIOLATION | INT_DIVIDE_BY_ZERO | ILLEGAL_INSTRUCTION | PRIVILEGED_INSTRUCTION
        )
    }

    /// Last-chance unhandled-exception callback. Windows calls this only after
    /// normal frame-based exception handling has failed.
    ///
    /// # Safety
    /// Called by the OS exception dispatcher. We only read the supplied
    /// exception record, access process-wide atomics, and terminate the
    /// current background thread.
    unsafe extern "system" fn unhandled_exception_filter(
        info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS,
    ) -> i32 {
        if info.is_null() {
            return delegate_to_previous_filter(info);
        }

        let record = (*info).ExceptionRecord;
        if record.is_null() {
            return delegate_to_previous_filter(info);
        }
        let code = (*record).ExceptionCode;

        if !is_worker_recoverable(code) {
            return delegate_to_previous_filter(info);
        }

        let code_u32 = code as u32;
        let fault_address = (*record).ExceptionAddress as usize;
        let parameter_count = (*record).NumberParameters as usize;
        let memory_details = (code_u32 == 0xC0000005 && parameter_count >= 2).then(|| {
            (
                (*record).ExceptionInformation[0],
                (*record).ExceptionInformation[1],
            )
        });

        let thread_id = windows_sys::Win32::System::Threading::GetCurrentThreadId();
        let state = find_thread_slot(thread_id)
            .map(|slot| slot.state.load(Ordering::Acquire))
            .unwrap_or(std::ptr::null_mut());
        if state.is_null() {
            return delegate_to_previous_filter(info);
        }
        (*state).record(code_u32, fault_address, memory_details);

        // This callback runs on the faulting worker thread. A direct call uses
        // the normal ABI and never attempts to resume at the invalid address.
        windows_sys::Win32::System::Threading::ExitThread(1)
    }
}

#[cfg(target_os = "windows")]
pub(super) fn activate_native_crash(state: &Arc<NativeCrashState>) -> bool {
    ACTIVE_NATIVE_CRASH
        .compare_exchange(
            std::ptr::null_mut(),
            Arc::as_ptr(state).cast_mut(),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_ok()
}

#[cfg(not(target_os = "windows"))]
pub(super) fn activate_native_crash(_state: &Arc<NativeCrashState>) -> bool {
    true
}

#[cfg(target_os = "windows")]
pub(super) fn deactivate_native_crash(state: &Arc<NativeCrashState>) {
    let _ = ACTIVE_NATIVE_CRASH.compare_exchange(
        Arc::as_ptr(state).cast_mut(),
        std::ptr::null_mut(),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
    seh_guard::clear_state(state);
}

#[cfg(not(target_os = "windows"))]
pub(super) fn deactivate_native_crash(_state: &Arc<NativeCrashState>) {}

/// Install the process-wide SEH handler. Only worker threads that register
/// through `spawn_with_safety_net` are intercepted; other threads continue
/// through Windows' normal exception handling.
#[cfg(target_os = "windows")]
pub fn install_seh_handler() {
    seh_guard::install_global_handler();
}

/// Run the CLI scanner on an enrolled worker so native access violations use
/// the same searchable diagnostics as the GUI instead of bypassing Rust's
/// panic/anyhow handling.
pub fn run_cli_with_safety_net<T>(
    failure_hint: UiText,
    f: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, UiError>
where
    T: Send + 'static,
{
    #[cfg(target_os = "windows")]
    seh_guard::install_global_handler();

    let native_crash = Arc::new(NativeCrashState::new());
    let thread_crash = native_crash.clone();
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
    let handle = thread::Builder::new()
        .name("scanner-cli".to_string())
        .spawn(move || {
            let active = activate_native_crash(&thread_crash);
            if !active {
                let _ = result_tx.send(Err(UiError::from_message(
                    UiText::new(
                        "扫描器无法启动，因为另一个任务仍在关闭。请稍候重试。",
                        "The scanner could not start because another task is still shutting down. Retry shortly.",
                    ),
                    "native crash boundary is still owned by another task",
                )));
                return;
            }
            let native_context = yas::native_crash::inherit_current_task();
            let result = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(UiError::from_anyhow(failure_hint.clone(), &error)),
                Err(panic_info) => Err(UiError::from_panic(failure_hint, panic_info.as_ref())),
            };
            let _ = result_tx.send(result);
            drop(native_context);
            deactivate_native_crash(&thread_crash);
        })
        .map_err(|error| {
            UiError::from_error(
                UiText::new(
                    "扫描器后台任务无法启动。请检查系统资源，然后重试。",
                    "The scanner background task could not start. Check available system resources, then retry.",
                ),
                error,
            )
        })?;

    let mut pending_result = None;
    loop {
        if let Some(exception) = native_crash.claim_exception(TaskKind::Scanner, None) {
            deactivate_native_crash(&native_crash);
            return Err(UiError::native_exception(exception));
        }
        if pending_result.is_none() {
            match result_rx.try_recv() {
                Ok(result) => pending_result = Some(result),
                Err(std::sync::mpsc::TryRecvError::Disconnected) if handle.is_finished() => {
                    return Err(UiError::from_message(
                        UiText::new(
                            "扫描器后台任务意外停止，且没有返回结果。请复制完整错误并报告问题。",
                            "The scanner background task stopped unexpectedly without returning a result. Copy the full error and report the problem.",
                        ),
                        "scanner CLI worker exited without reporting a result",
                    ));
                },
                Err(_) => {},
            }
        }
        if handle.is_finished() {
            if let Some(exception) = native_crash.claim_exception(TaskKind::Scanner, None) {
                deactivate_native_crash(&native_crash);
                return Err(UiError::native_exception(exception));
            }
            if let Some(result) = pending_result {
                return result;
            }
            if matches!(
                result_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Disconnected)
            ) {
                return Err(UiError::from_message(
                    UiText::new(
                        "扫描器后台任务意外停止，且没有返回结果。请复制完整错误并报告问题。",
                        "The scanner background task stopped unexpectedly without returning a result. Copy the full error and report the problem.",
                    ),
                    "scanner CLI worker exited without reporting a result",
                ));
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Run a closure on a background thread with comprehensive error handling:
/// - Catches Rust panics via `catch_unwind`
/// - On Windows, activates a last-chance unhandled-exception boundary for
///   selected native crashes (access violations, etc.) in task-related
///   background/native threads.
///
/// The closure should set `status` to Completed/Failed on success/error.
/// This wrapper only intervenes for unexpected crashes.
fn spawn_with_safety_net(
    task: TaskKind,
    log_source: LogSource,
    status: Arc<Mutex<TaskStatus>>,
    native_crash: Arc<NativeCrashState>,
    f: impl FnOnce(Arc<Mutex<TaskStatus>>) + Send + 'static,
) -> Result<JoinHandle<()>, UiError> {
    // Ensure the global SEH handler is registered (idempotent).
    #[cfg(target_os = "windows")]
    seh_guard::install_global_handler();

    let thread_name = match task {
        TaskKind::Scanner => "scanner-worker",
        TaskKind::Manager => "manager-worker",
        TaskKind::Capture => "capture-worker",
    };
    thread::Builder::new()
        .name(thread_name.to_owned())
        .spawn(move || {
        log_bridge::set_thread_log_source(log_source);

        // Activation happens only after thread creation succeeds, so the
        // process-wide enrollment source can never point to a dropped Arc.
        let native_guard_active = activate_native_crash(&native_crash);
        if !native_guard_active {
            set_task_failure(
                &status,
                UiError::from_message(
                    UiText::new(
                        "当前任务无法启动，因为另一个游戏数据任务仍在关闭。请稍候重试；若一直如此，请重启程序。",
                        "This task could not start because another game-data task is still shutting down. Retry shortly; if it persists, restart the application.",
                    ),
                    "native crash boundary is still owned by another task",
                ),
            );
            return;
        }
        let _native_thread_context =
            native_guard_active.then(yas::native_crash::inherit_current_task);

        let status_for_crash = status.clone();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            f(status);
        }));

        if let Err(panic_info) = result {
            let hint = match task {
                TaskKind::Scanner => UiText::new(
                    "扫描器因意外的内部错误而停止。请复制完整错误以搜索或寻求帮助。",
                    "The scanner stopped because of an unexpected internal error. Copy the full error to search or ask for help.",
                ),
                TaskKind::Manager => UiText::new(
                    "管理器因意外的内部错误而停止。请复制完整错误以搜索或寻求帮助。",
                    "The manager stopped because of an unexpected internal error. Copy the full error to search or ask for help.",
                ),
                TaskKind::Capture => UiText::new(
                    "抓包器因意外的内部错误而停止。请复制完整错误以搜索或寻求帮助。",
                    "Capture stopped because of an unexpected internal error. Copy the full error to search or ask for help.",
                ),
            };
            let error = UiError::from_panic(hint, panic_info.as_ref());
            error.log_diagnostics();
            let mut guard = lock_status_recover(&status_for_crash);
            // Only overwrite if still Running — don't clobber a proper Failed/Completed
            if matches!(*guard, TaskStatus::Running(_)) {
                *guard = TaskStatus::Failed(error);
            }
        }

        drop(_native_thread_context);
        if native_guard_active {
            deactivate_native_crash(&native_crash);
        }
        })
        .map_err(|error| {
            let hint = match task {
                TaskKind::Scanner => UiText::new(
                    "扫描器后台任务无法启动。请检查系统资源，然后重试。",
                    "The scanner background task could not start. Check available system resources, then retry.",
                ),
                TaskKind::Manager => UiText::new(
                    "管理器后台任务无法启动。请检查系统资源，然后重试。",
                    "The manager background task could not start. Check available system resources, then retry.",
                ),
                TaskKind::Capture => UiText::new(
                    "抓包器后台任务无法启动。请检查系统资源，然后重试。",
                    "The capture background task could not start. Check available system resources, then retry.",
                ),
            };
            UiError::from_error(hint, error)
        })
}

/// Handle to a running background task.
pub struct TaskHandle {
    _handle: Option<JoinHandle<()>>,
    task: TaskKind,
    native_crash: Arc<NativeCrashState>,
    /// Materialized on the GUI thread from the lock-free native crash record.
    /// The worker never touches this mutex, so an `ExitThread` cannot leave it
    /// permanently locked and freeze error rendering.
    native_failure: Mutex<Option<UiError>>,
    /// Optional shutdown flag — set to true to request graceful stop.
    shutdown: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Optional cancel token for scan operations.
    cancel_token: Option<yas::cancel::CancelToken>,
    /// Shared status — used to surface immediate "Stopping..." feedback
    /// when `stop()` is called, without waiting for the worker thread to
    /// notice the cancel token.
    status: Arc<Mutex<TaskStatus>>,
    /// Message shown immediately on stop, in the active UI language.
    stopping_msg: UiText,
}

impl TaskHandle {
    pub fn is_finished(&self) -> bool {
        self.surface_native_failure();
        lock_recover(&self.native_failure).is_some()
            || self._handle.as_ref().is_none_or(JoinHandle::is_finished)
    }

    /// The native failure, if this worker was terminated by the Windows crash
    /// boundary. Its presence means the process may hold leaked native state;
    /// callers must require a full application restart instead of retrying.
    pub fn native_failure(&self) -> Option<UiError> {
        self.surface_native_failure();
        lock_recover(&self.native_failure).clone()
    }

    pub fn requires_restart(&self) -> bool {
        self.native_failure().is_some()
    }

    fn surface_native_failure(&self) {
        if !self.native_crash.has_occurred() {
            return;
        }

        // ExitThread skips Rust drops. If the exception happened while this
        // status was locked, never wait for that lock: the independent
        // `native_failure` slot remains safe and keeps the GUI responsive.
        let phase = match self.status.try_lock() {
            Ok(status) => match &*status {
                TaskStatus::Running(phase) => Some(phase.clone()),
                _ => None,
            },
            Err(TryLockError::Poisoned(error)) => {
                self.status.clear_poison();
                match &*error.into_inner() {
                    TaskStatus::Running(phase) => Some(phase.clone()),
                    _ => None,
                }
            },
            Err(TryLockError::WouldBlock) => None,
        };
        let Some(exception) = self.native_crash.claim_exception(self.task, phase) else {
            return;
        };
        let error = UiError::native_exception(exception);
        deactivate_native_crash(&self.native_crash);
        // A manager native crash bypasses Rust cleanup via ExitThread. Signal
        // its watcher so the HTTP listener and helper threads are still told
        // to shut down before the user retries.
        if let Some(shutdown) = &self.shutdown {
            shutdown.store(true, Ordering::Relaxed);
        }
        if let Some(cancel_token) = &self.cancel_token {
            cancel_token.cancel(yas::cancel::StopReason::Error);
        }
        *lock_recover(&self.native_failure) = Some(error.clone());

        error.log_diagnostics();

        // Keep the ordinary status in sync when possible, but never block on
        // it after an abnormal thread exit.
        match self.status.try_lock() {
            Ok(mut status) => *status = TaskStatus::Failed(error),
            Err(TryLockError::Poisoned(poisoned)) => {
                self.status.clear_poison();
                *poisoned.into_inner() = TaskStatus::Failed(error);
            },
            Err(TryLockError::WouldBlock) => {},
        }
    }

    /// Whether the underlying cancel token has been tripped (by `stop()`
    /// or by RMB). Lets the UI distinguish "running" from "stopping".
    pub fn is_stopping(&self) -> bool {
        self.cancel_token.as_ref().is_some_and(|t| t.is_cancelled())
            || self
                .shutdown
                .as_ref()
                .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Signal the task to shut down gracefully.
    ///
    /// Sets the cancel token (the same one RMB uses) and immediately
    /// overwrites the status with a "stopping" message so the user gets
    /// feedback before the worker thread notices.
    pub fn stop(&self) {
        if let Some(ref flag) = self.shutdown {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        if let Some(ref token) = self.cancel_token {
            token.cancel(yas::cancel::StopReason::UserAbort);
        }
        // A native exception can terminate the worker while it owns this
        // mutex. Stopping must remain non-blocking so the GUI can still show
        // and copy the independently recorded native failure.
        match self.status.try_lock() {
            Ok(mut status) => {
                if matches!(*status, TaskStatus::Running(_)) {
                    *status = TaskStatus::Running(self.stopping_msg.clone());
                }
            },
            Err(TryLockError::Poisoned(poisoned)) => {
                self.status.clear_poison();
                let mut status = poisoned.into_inner();
                if matches!(*status, TaskStatus::Running(_)) {
                    *status = TaskStatus::Running(self.stopping_msg.clone());
                }
            },
            Err(TryLockError::WouldBlock) => {},
        }
    }
}

/// Store and log an ordinary task failure without flattening its inner cause.
pub(super) fn set_task_failure(status: &Arc<Mutex<TaskStatus>>, error: UiError) {
    error.log_diagnostics();
    *lock_status_recover(status) = TaskStatus::Failed(error);
}

/// Shared cancellable worker boundary used by game-specific GUI adapters.
/// The game adapter owns domain setup and result wording; this function owns
/// panic/native-crash containment and the common stop handle.
pub(super) fn spawn_cancellable_task<R: Into<TaskStatus> + Send + 'static>(
    task: TaskKind,
    log_source: LogSource,
    status: Arc<Mutex<TaskStatus>>,
    initial_msg: UiText,
    stopping_msg: UiText,
    run: impl FnOnce(yas::cancel::CancelToken) -> Result<R, UiError> + Send + 'static,
) -> TaskHandle {
    let cancel_token = yas::cancel::CancelToken::new();
    let worker_cancel = cancel_token.clone();
    *lock_status_recover(&status) = TaskStatus::Running(initial_msg);

    let native_crash = Arc::new(NativeCrashState::new());
    let handle = match spawn_with_safety_net(
        task,
        log_source,
        status.clone(),
        native_crash.clone(),
        move |status| match run(worker_cancel.clone()) {
            Ok(message) => {
                let outcome = message.into();
                *lock_status_recover(&status) = match outcome {
                    TaskStatus::Completed(message) if worker_cancel.is_cancelled() => {
                        TaskStatus::Stopped(message)
                    },
                    other => other,
                };
            },
            Err(error) => set_task_failure(&status, error),
        },
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            set_task_failure(&status, error);
            None
        },
    };

    TaskHandle {
        _handle: handle,
        task,
        native_crash,
        native_failure: Mutex::new(None),
        shutdown: None,
        cancel_token: Some(cancel_token),
        status,
        stopping_msg,
    }
}

fn lock_status_recover(status: &Mutex<TaskStatus>) -> std::sync::MutexGuard<'_, TaskStatus> {
    match status.lock() {
        Ok(guard) => guard,
        Err(error) => {
            status.clear_poison();
            error.into_inner()
        },
    }
}

pub(super) fn try_task_status(status: &Mutex<TaskStatus>) -> Option<TaskStatus> {
    match status.try_lock() {
        Ok(status) => Some(status.clone()),
        Err(TryLockError::Poisoned(poisoned)) => {
            status.clear_poison();
            Some(poisoned.into_inner().clone())
        },
        Err(TryLockError::WouldBlock) => None,
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(error) => {
            mutex.clear_poison();
            error.into_inner()
        },
    }
}

/// Spawn a scan operation on a background thread.
pub fn spawn_scan(state: &AppState) -> TaskHandle {
    let status = state.scan_status.clone();
    let progress = state.scan_progress.clone();
    {
        use super::task_progress::{Step, TaskProgress};
        let mut steps = Vec::new();
        for (selected, key, zh, en) in [
            (state.scan_characters, "characters", "角色", "Characters"),
            (state.scan_weapons, "weapons", "武器", "Weapons"),
            (state.scan_artifacts, "artifacts", "圣遗物", "Artifacts"),
            (
                state.scan_achievements,
                "achievements",
                "成就",
                "Achievements",
            ),
        ] {
            if selected {
                steps.push(Step::new(key, zh, en));
            }
        }
        *progress.lock().unwrap() = TaskProgress { steps };
    }
    let user_config = state.user_config.clone();
    let scan_config = state.to_scan_config();
    let token = yas::cancel::CancelToken::new();
    let stop_token = token.clone();
    let cancel_for_result = token.clone();
    *status.lock().unwrap() = TaskStatus::Running(UiText::new("正在初始化...", "Initializing..."));

    // Check ONNX runtime before spawning
    #[cfg(any(target_os = "windows", target_os = "linux"))]
    {
        if !genshin_scanner::cli::check_onnxruntime() {
            *status.lock().unwrap() = TaskStatus::Running(UiText::new(
                "正在下载 ONNX Runtime...",
                "Downloading ONNX Runtime...",
            ));
        }
    }

    let stopping_msg = UiText::new("正在停止扫描...", "Stopping scan...");
    let native_crash = Arc::new(NativeCrashState::new());

    let handle = match spawn_with_safety_net(
        TaskKind::Scanner,
        LogSource::Scanner,
        status.clone(),
        native_crash.clone(),
        move |status| {
            // Check VC++ runtime before loading ONNX
            #[cfg(target_os = "windows")]
            {
                if let Err(e) = genshin_scanner::cli::check_vcpp_runtime() {
                    set_task_failure(
                        &status,
                        UiError::from_anyhow(
                            UiText::new(
                                "扫描器无法启动，因为所需的 Windows 运行组件不可用。请按完整错误中的说明修复后重试。",
                                "The scanner cannot start because a required Windows runtime component is unavailable. Follow the full error details, then retry.",
                            ),
                            &e,
                        ),
                    );
                    return;
                }
            }

            // Ensure ONNX runtime on the worker thread
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                if !genshin_scanner::cli::check_onnxruntime() {
                    if let Err(e) = genshin_scanner::cli::download_onnxruntime() {
                        set_task_failure(
                            &status,
                            UiError::from_anyhow(
                                UiText::new(
                                    "扫描器无法下载 OCR 引擎。请检查网络连接或安全软件设置，然后重试。",
                                    "The scanner could not download its OCR engine. Check the network connection or security software, then retry.",
                                ),
                                &e,
                            ),
                        );
                        return;
                    }
                }
            }

            let status_for_cb = status.clone();
            // Once the cancel token is tripped, don't let deeper phases
            // overwrite the "Stopping..." message with their own progress.
            let cancel_for_cb = cancel_for_result.clone();
            let stopping_msg_cb = UiText::new("正在停止扫描...", "Stopping scan...");
            let status_fn = move |msg: &str| {
                if cancel_for_cb.is_cancelled() {
                    *status_for_cb.lock().unwrap() = TaskStatus::Running(stopping_msg_cb.clone());
                    return;
                }
                let phase = UiText::from_bilingual(msg);
                *status_for_cb.lock().unwrap() = TaskStatus::Running(phase);
            };

            let counts = |completed, total, _id: &str, key: &str| {
                use super::task_progress::StepState;
                if let Some(step) = progress
                    .lock()
                    .unwrap()
                    .steps
                    .iter_mut()
                    .find(|s| s.key == key)
                {
                    step.completed = completed;
                    step.total = if key != "characters" && total > 0 {
                        Some(total)
                    } else {
                        None
                    };
                    step.state = StepState::Running;
                }
            };
            let phases = |key: &str, event| {
                use super::task_progress::StepState;
                use genshin_scanner::scanner::common::progress::ScanPhaseEvent;
                if let Some(step) = progress
                    .lock()
                    .unwrap()
                    .steps
                    .iter_mut()
                    .find(|s| s.key == key)
                {
                    step.state = match event {
                        ScanPhaseEvent::Started => StepState::Running,
                        ScanPhaseEvent::Complete(count) => {
                            step.completed = count;
                            StepState::Complete
                        },
                        ScanPhaseEvent::Interrupted => StepState::Interrupted,
                    };
                }
            };
            let result = genshin_scanner::cli::run_scan_core_observed(
                &user_config,
                &scan_config,
                Some(&status_fn),
                Some(token),
                Some(&counts),
                Some(&phases),
            );
            progress.lock().unwrap().interrupt_unfinished();
            let outcome = scan_result_status(
                result,
                &progress.lock().unwrap(),
                cancel_for_result.is_cancelled(),
            );
            if let TaskStatus::Failed(error) = outcome {
                set_task_failure(&status, error);
            } else {
                *status.lock().unwrap() = outcome;
            }
        },
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            set_task_failure(&status, error);
            None
        },
    };

    TaskHandle {
        _handle: handle,
        task: TaskKind::Scanner,
        native_crash,
        native_failure: Mutex::new(None),
        shutdown: None,
        cancel_token: Some(stop_token),
        status,
        stopping_msg,
    }
}

fn scan_result_status(
    result: anyhow::Result<String>,
    progress: &super::task_progress::TaskProgress,
    cancelled: bool,
) -> TaskStatus {
    match result {
        Ok(path) => TaskStatus::Exported {
            message: UiText::new("扫描已完成", "Scan completed"),
            partial: cancelled
                || std::path::Path::new(&path)
                    .file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("good_export_partial_"))
                || progress.steps.iter().any(|step| {
                    step.state != super::task_progress::StepState::Complete
                        || step.total.is_some_and(|total| step.completed < total)
                }),
            path,
        },
        Err(error) if error.is::<genshin_scanner::scanner::common::progress::ScanCancelled>() => {
            TaskStatus::Stopped(UiText::new(
                "扫描已停止 · 未导出",
                "Scan stopped · no export",
            ))
        },
        Err(error) => TaskStatus::Failed(UiError::from_anyhow(
            UiText::new("扫描未能完成。", "The scan could not finish."),
            &error,
        )),
    }
}

#[cfg(test)]
mod feedback_tests {
    use super::*;
    use crate::gui::state::Lang;
    use crate::gui::task_progress::{Step, StepState, TaskProgress};

    #[test]
    fn cancellation_does_not_hide_an_export_write_failure() {
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "cannot write partial export: access denied",
        ));
        let status = scan_result_status(Err(error), &TaskProgress::default(), true);
        let TaskStatus::Failed(error) = status else {
            panic!("write failure was hidden")
        };
        assert!(error.copy_text(Lang::En).contains("access denied"));
    }

    #[test]
    fn partial_filename_marks_capped_exports_without_a_cancel_signal() {
        assert!(matches!(
            scan_result_status(
                Ok("C:/exports/good_export_partial_2026-10-13_12-00-00.json".into()),
                &TaskProgress::default(),
                false
            ),
            TaskStatus::Exported { partial: true, .. }
        ));
    }

    #[test]
    fn a_clean_stop_and_a_saved_partial_export_have_distinct_outcomes() {
        let stopped = scan_result_status(
            Err(genshin_scanner::scanner::common::progress::ScanCancelled.into()),
            &TaskProgress::default(),
            true,
        );
        assert!(matches!(stopped, TaskStatus::Stopped(_)));
        let path = "C:/exports/partial.json".to_owned();
        let saved = scan_result_status(Ok(path.clone()), &TaskProgress::default(), true);
        assert!(
            matches!(saved, TaskStatus::Exported { path: saved_path, partial: true, .. } if saved_path == path)
        );
    }

    #[test]
    fn incomplete_category_counts_cannot_claim_a_full_export() {
        let mut step = Step::new("weapons", "武器", "Weapons");
        step.state = StepState::Complete;
        step.completed = 9;
        step.total = Some(10);
        let mut progress = TaskProgress { steps: vec![step] };
        assert!(matches!(
            scan_result_status(Ok("export.json".into()), &progress, false),
            TaskStatus::Exported { partial: true, .. }
        ));
        progress.steps[0].completed = 10;
        assert!(matches!(
            scan_result_status(Ok("export.json".into()), &progress, false),
            TaskStatus::Exported { partial: false, .. }
        ));
    }
}

/// Spawn the HTTP server on a background thread.
pub fn spawn_server(state: &AppState) -> TaskHandle {
    let status = state.server_status.clone();
    let job = state.server_job.clone();
    *job.lock().unwrap() = genshin_scanner::manager::models::JobState::idle();
    let user_config = state.user_config.clone();
    let port = state.server_port;
    #[cfg(all(feature = "capture", target_os = "windows"))]
    let continuous_dir = std::path::PathBuf::from(&state.output_dir);
    let enabled = state.server_enabled.clone();
    // GUI management always filters target sets. The legacy CLI preference
    // remains available to command-line callers, but is not a GUI setting.
    let filter_involved_sets = true;
    // Management changes flags only; inventory refresh is a separate scan.
    let stop_on_all_matched = true;
    let dump_images = state.dump_images;
    let dump_job_data = state.dump_job_data;
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    let msg = UiText::new(
        format!("正在启动服务器 · 端口 {}", port),
        format!("Starting server · port {}", port),
    );
    *status.lock().unwrap() = TaskStatus::Running(msg);

    let native_crash = Arc::new(NativeCrashState::new());
    let handle = match spawn_with_safety_net(
        TaskKind::Manager,
        LogSource::Manager,
        status.clone(),
        native_crash.clone(),
        move |status| {
            // Check VC++ runtime before loading ONNX
            #[cfg(target_os = "windows")]
            {
                if let Err(e) = genshin_scanner::cli::check_vcpp_runtime() {
                    set_task_failure(
                        &status,
                        UiError::from_anyhow(
                            UiText::new(
                                "管理器无法启动，因为所需的 Windows 运行组件不可用。请按完整错误中的说明修复后重试。",
                                "The manager cannot start because a required Windows runtime component is unavailable. Follow the full error details, then retry.",
                            ),
                            &e,
                        ),
                    );
                    return;
                }
            }

            // Ensure ONNX runtime
            #[cfg(any(target_os = "windows", target_os = "linux"))]
            {
                if !genshin_scanner::cli::check_onnxruntime() {
                    if let Err(e) = genshin_scanner::cli::download_onnxruntime() {
                        set_task_failure(
                            &status,
                            UiError::from_anyhow(
                                UiText::new(
                                    "管理器无法下载 OCR 引擎。请检查网络连接或安全软件设置，然后重试。",
                                    "The manager could not download its OCR engine. Check the network connection or security software, then retry.",
                                ),
                                &e,
                            ),
                        );
                        return;
                    }
                }
            }

            let status_clone = status.clone();
            let shutdown_for_status = shutdown_clone.clone();
            let status_fn = Arc::new(move |msg: &str| {
                *status_clone.lock().unwrap() = TaskStatus::Running(
                    if shutdown_for_status.load(std::sync::atomic::Ordering::Relaxed) {
                        UiText::new("正在停止服务器", "Stopping server")
                    } else {
                        UiText::from_bilingual(msg)
                    },
                );
            });

            #[cfg(all(feature = "capture", target_os = "windows"))]
            let continuous_session = user_config
                .continuous_capture
                .then(|| super::continuous_capture::Session::genshin(continuous_dir));
            #[cfg(all(feature = "capture", target_os = "windows"))]
            let continuous = continuous_session
                .as_ref()
                .map(|session| session.latest.clone());
            #[cfg(not(all(feature = "capture", target_os = "windows")))]
            let continuous = None;
            match genshin_scanner::cli::run_server_core_observed_continuous(
                &user_config,
                port,
                &genshin_scanner::cli::OcrEngineArgs::default(),
                enabled,
                shutdown_clone,
                stop_on_all_matched,
                filter_involved_sets,
                dump_images,
                dump_job_data,
                Some(status_fn),
                Some(job),
                continuous,
            ) {
                Ok(()) => {
                    *status.lock().unwrap() =
                        TaskStatus::Completed(UiText::new("服务器已停止", "Server stopped"));
                },
                Err(e) => {
                    set_task_failure(
                        &status,
                        UiError::from_anyhow(
                            UiText::new(
                                "管理器服务器已停止。下方完整错误包含底层原因，可复制后搜索或寻求帮助。",
                                "The manager server stopped. The full error below contains the underlying cause and can be copied for searching or support.",
                            ),
                            &e,
                        ),
                    );
                },
            }
        },
    ) {
        Ok(handle) => Some(handle),
        Err(error) => {
            set_task_failure(&status, error);
            None
        },
    };

    let stopping_msg = UiText::new("正在停止服务器...", "Stopping server...");
    TaskHandle {
        _handle: handle,
        task: TaskKind::Manager,
        native_crash,
        native_failure: Mutex::new(None),
        shutdown: Some(shutdown),
        cancel_token: None,
        status,
        stopping_msg,
    }
}
