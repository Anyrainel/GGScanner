//! Loopback HTTP transport. The worker owns game execution; this thread only
//! validates and queues requests, so status stays responsive during OCR.
use hsr_scanner::manager::ManagerInstructionsEnvelope;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::Read,
    sync::{mpsc, Arc, Mutex},
    time::Duration,
};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use yas::cancel::CancelToken;

const MAX_BODY: u64 = 16 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScanRequest {
    #[serde(default)]
    pub characters: bool,
    #[serde(default)]
    pub light_cones: bool,
    #[serde(default)]
    pub relics: bool,
}

pub enum Job {
    Manage(ManagerInstructionsEnvelope),
    Scan(ScanRequest),
}

impl Job {
    fn kind(&self) -> &'static str {
        match self {
            Self::Manage(_) => "manage",
            Self::Scan(_) => "scan",
        }
    }
    fn key(&self) -> Option<&str> {
        match self {
            Self::Manage(e) => Some(&e.idempotency_key),
            _ => None,
        }
    }
    fn count(&self) -> usize {
        match self {
            Self::Manage(e) => e.instructions.len(),
            Self::Scan(s) => {
                usize::from(s.characters) + usize::from(s.light_cones) + usize::from(s.relics)
            },
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JobState {
    pub game: &'static str,
    pub job_id: Option<String>,
    pub kind: Option<&'static str>,
    pub phase: &'static str,
    pub count: usize,
    #[serde(skip)]
    key: Option<String>,
    #[serde(skip)]
    result: Option<Value>,
    #[serde(skip)]
    active_cancel: Option<CancelToken>,
}

impl Default for JobState {
    fn default() -> Self {
        Self {
            game: "star-rail",
            job_id: None,
            kind: None,
            phase: "idle",
            count: 0,
            key: None,
            result: None,
            active_cancel: None,
        }
    }
}

pub fn allowed_origin(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return false;
    }
    match url.host_str() {
        Some("hsr.ggartifact.com") => {
            url.scheme() == "https" && url.port_or_known_default() == Some(443)
        },
        Some("localhost" | "127.0.0.1" | "[::1]") => matches!(url.scheme(), "http" | "https"),
        _ => false,
    }
}

fn reply(request: Request, code: u16, body: Value, origin: Option<&str>) {
    let mut response = Response::from_string(body.to_string()).with_status_code(StatusCode(code));
    response.add_header(Header::from_bytes("Content-Type", "application/json").unwrap());
    response.add_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
    if let Some(origin) = origin {
        for (name, value) in [
            ("Access-Control-Allow-Origin", origin),
            ("Vary", "Origin"),
            ("Access-Control-Allow-Methods", "GET, POST, OPTIONS"),
            ("Access-Control-Allow-Headers", "Content-Type"),
            ("Access-Control-Allow-Private-Network", "true"),
        ] {
            response.add_header(Header::from_bytes(name, value).unwrap());
        }
    }
    let _ = request.respond(response);
}

fn handle(
    mut request: Request,
    sender: &mpsc::Sender<Job>,
    shared: &Mutex<JobState>,
    sequence: &mut u64,
    progress: &impl Fn() -> Value,
    cancel: &CancelToken,
    dump_job_data: bool,
    continuous: Option<&genshin_scanner::continuous::ContinuousCapture>,
) {
    let origin = request
        .headers()
        .iter()
        .find(|h| h.field.equiv("Origin"))
        .map(|h| h.value.as_str().to_owned());
    if origin.as_deref().is_some_and(|o| !allowed_origin(o)) {
        reply(request, 403, json!({"error":"origin_rejected"}), None);
        return;
    }
    let origin = origin.as_deref();
    if request.method() == &Method::Options {
        reply(request, 204, Value::Null, origin);
        return;
    }
    let Ok(url) = url::Url::parse(&format!("http://localhost{}", request.url())) else {
        reply(request, 400, json!({"error":"invalid_url"}), origin);
        return;
    };
    let path = url.path();
    if request.method() == &Method::Get {
        let state = shared.lock().unwrap().clone();
        match path {
            "/continuous" => {
                let (status, body) =
                    genshin_scanner::continuous::ContinuousCapture::response(continuous);
                reply(request, status, body, origin);
            },
            "/health" => reply(
                request,
                200,
                json!({"game":"star-rail", "status":"ok", "busy": matches!(state.phase,"pending"|"running"), "enabled":!cancel.is_cancelled()}),
                origin,
            ),
            "/status" => {
                let mut body = serde_json::to_value(state).unwrap();
                body["progress"] = progress();
                reply(request, 200, body, origin);
            },
            "/result" => {
                let job = url
                    .query_pairs()
                    .find(|(k, _)| k == "jobId")
                    .map(|(_, v)| v.into_owned());
                if job.is_none() || job != state.job_id {
                    reply(request, 404, json!({"error":"job_not_found"}), origin);
                } else if let Some(result) = state.result {
                    reply(request, 200, result, origin);
                } else {
                    reply(request, 409, json!({"error":"job_not_finished"}), origin);
                }
            },
            _ => reply(request, 404, json!({"error":"not_found"}), origin),
        }
        return;
    }
    if request.method() == &Method::Post && path == "/cancel" {
        let job_id = url
            .query_pairs()
            .find(|(k, _)| k == "jobId")
            .map(|(_, v)| v.into_owned());
        let state = shared.lock().unwrap();
        if job_id.is_none() || job_id != state.job_id {
            drop(state);
            reply(request, 404, json!({"error":"job_not_found"}), origin);
            return;
        }
        if let Some(token) = &state.active_cancel {
            token.cancel(yas::cancel::StopReason::UserAbort);
        }
        drop(state);
        reply(request, 202, json!({"status":"stopping"}), origin);
        return;
    }
    if request.method() != &Method::Post || !matches!(path, "/manage" | "/scan") {
        reply(request, 404, json!({"error":"not_found"}), origin);
        return;
    }
    let manage = path == "/manage";
    if !request.headers().iter().any(|h| {
        h.field.equiv("Content-Type")
            && h.value
                .as_str()
                .split(';')
                .next()
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("application/json"))
    }) {
        reply(request, 415, json!({"error":"json_required"}), origin);
        return;
    }
    let mut bytes = Vec::new();
    if request
        .as_reader()
        .take(MAX_BODY + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_BODY
    {
        reply(request, 413, json!({"error":"request_too_large"}), origin);
        return;
    }
    if dump_job_data {
        let endpoint = if manage { "hsr_manage" } else { "hsr_scan" };
        if let Err(error) =
            yas::utils::request_log::save_request_log(std::path::Path::new("log"), endpoint, &bytes)
        {
            yas::log_error!(
                "无法保存星铁请求数据：{:#}",
                "Could not save Star Rail request data: {:#}",
                error
            );
        }
    }
    let job = if manage {
        let text = match std::str::from_utf8(&bytes) {
            Ok(text) => text,
            Err(_) => {
                reply(request, 400, json!({"error":"invalid_utf8"}), origin);
                return;
            },
        };
        match ManagerInstructionsEnvelope::parse_json(text) {
            Ok(envelope) => Some(Job::Manage(envelope)),
            Err(error) => {
                let hint = error.hint();
                reply(
                    request,
                    400,
                    json!({"error": {
                        "zh": hint.select(hsr_scanner::localization::Language::ZhCn),
                        "en": hint.select(hsr_scanner::localization::Language::En),
                        "details": error.localized_message(hsr_scanner::localization::Language::En),
                    }}),
                    origin,
                );
                return;
            },
        }
    } else {
        serde_json::from_slice::<ScanRequest>(&bytes)
            .ok()
            .filter(|s| s.characters || s.light_cones || s.relics)
            .map(Job::Scan)
    };
    let Some(job) = job else {
        reply(request, 400, json!({"error":"invalid_request"}), origin);
        return;
    };
    let mut state = shared.lock().unwrap();
    if cancel.is_cancelled() {
        drop(state);
        reply(request, 503, json!({"error":"connection_stopping"}), origin);
        return;
    }
    if state.phase != "failed" && job.key().is_some() && job.key() == state.key.as_deref() {
        let body = json!({"jobId":state.job_id, "game":"star-rail"});
        drop(state);
        reply(request, 202, body, origin);
        return;
    }
    if matches!(state.phase, "pending" | "running") {
        drop(state);
        reply(request, 409, json!({"error":"busy"}), origin);
        return;
    }
    *sequence += 1;
    let job_id = format!(
        "hsr-{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        sequence
    );
    *state = JobState {
        job_id: Some(job_id.clone()),
        kind: Some(job.kind()),
        phase: "pending",
        count: job.count(),
        key: job.key().map(str::to_owned),
        active_cancel: Some(CancelToken::new()),
        ..Default::default()
    };
    if sender.send(job).is_err() {
        state.phase = "failed";
        drop(state);
        reply(request, 503, json!({"error":"connection_stopped"}), origin);
        return;
    }
    drop(state);
    reply(
        request,
        202,
        json!({"jobId":job_id, "game":"star-rail"}),
        origin,
    );
}

/// The bound Server can use port zero in tests. No test-specific runtime path.
pub fn serve(
    server: Server,
    cancel: CancelToken,
    state: Arc<Mutex<JobState>>,
    dump_job_data: bool,
    execute: impl FnMut(Job, CancelToken) -> Result<Value, Value>,
    progress: impl Fn() -> Value + Send + Sync,
) -> anyhow::Result<()> {
    serve_continuous(
        server,
        None,
        cancel,
        state,
        dump_job_data,
        execute,
        progress,
    )
}

pub fn serve_continuous(
    server: Server,
    continuous: Option<genshin_scanner::continuous::ContinuousCapture>,
    cancel: CancelToken,
    state: Arc<Mutex<JobState>>,
    dump_job_data: bool,
    execute: impl FnMut(Job, CancelToken) -> Result<Value, Value>,
    progress: impl Fn() -> Value + Send + Sync,
) -> anyhow::Result<()> {
    let (sender, receiver) = mpsc::channel();
    let done = CancelToken::new();
    struct StopOnDrop(CancelToken);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.cancel(yas::cancel::StopReason::UserAbort);
        }
    }
    std::thread::scope(|scope| {
        let done_http = done.clone();
        let state_http = state.clone();
        let cancel_http = cancel.clone();
        let http = scope.spawn(move || {
            let mut sequence = 0;
            while !done_http.is_cancelled() {
                if cancel_http.is_cancelled() {
                    if let Some(token) = &state_http.lock().unwrap().active_cancel {
                        token.cancel(yas::cancel::StopReason::UserAbort);
                    }
                }
                match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(request)) => handle(
                        request,
                        &sender,
                        &state_http,
                        &mut sequence,
                        &progress,
                        &cancel_http,
                        dump_job_data,
                        continuous.as_ref(),
                    ),
                    Ok(None) => {},
                    Err(error) => return Err(anyhow::anyhow!(error)),
                }
            }
            Ok(())
        });
        let _stop = StopOnDrop(done.clone());
        let mut execute = execute;
        while !cancel.is_cancelled() && !http.is_finished() {
            match receiver.recv_timeout(Duration::from_millis(100)) {
                Ok(job) => {
                    let token = {
                        let mut state = state.lock().unwrap();
                        state.phase = "running";
                        state.active_cancel.as_ref().unwrap().clone()
                    };
                    let outcome = execute(job, token);
                    let mut state = state.lock().unwrap();
                    state.phase = if outcome.is_ok() {
                        "completed"
                    } else {
                        "failed"
                    };
                    let mut result = outcome.unwrap_or_else(|error| json!({"error":error}));
                    result["jobId"] = json!(state.job_id);
                    state.result = Some(result);
                    state.active_cancel = None;
                },
                Err(mpsc::RecvTimeoutError::Timeout) => {},
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        done.cancel(yas::cancel::StopReason::UserAbort);
        http.join()
            .map_err(|_| anyhow::anyhow!("Star Rail HTTP thread panicked"))?
    })
}
