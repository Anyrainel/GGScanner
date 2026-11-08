//! Latest packet-capture snapshot, independent of OCR job caches.
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
pub struct ContinuousCapture(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    session_id: String,
    latest: Option<Value>,
    revision: u64,
    updated_at: u64,
    error: Option<String>,
    saved: Option<Value>,
}

impl Default for ContinuousCapture {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(State {
            session_id: uuid::Uuid::new_v4().to_string(),
            ..State::default()
        })))
    }
}

impl ContinuousCapture {
    pub fn publish(&self, data: Value) {
        let mut state = self.0.lock().unwrap();
        if state.latest.as_ref() != Some(&data) {
            state.revision += 1;
            state.updated_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            state.latest = Some(data);
        }
    }

    /// Start a new login generation even if clients miss the no-data interval.
    /// The server stays running, but no previous account data or error survives.
    pub fn reset(&self) {
        let mut state = self.0.lock().unwrap();
        state.session_id = uuid::Uuid::new_v4().to_string();
        state.latest = None;
        state.revision = 0;
        state.updated_at = 0;
        state.error = None;
    }

    /// Withhold an incomplete snapshot without creating another login generation.
    pub fn clear(&self) {
        self.0.lock().unwrap().latest = None;
    }

    pub fn fail(&self, error: String) {
        self.0.lock().unwrap().error = Some(error);
    }

    /// Stable machine-readable errors; snapshots are never restored from disk.
    pub fn response(capture: Option<&Self>) -> (u16, Value) {
        let Some(capture) = capture else {
            return (409, json!({"error":"continuous_capture_disabled"}));
        };
        let state = capture.0.lock().unwrap();
        if let Some(error) = &state.error {
            return (
                503,
                json!({"error":"continuous_capture_failed", "details":error}),
            );
        }
        match &state.latest {
            Some(data) => (
                200,
                json!({"sessionId":state.session_id, "revision":state.revision, "updatedAt":state.updated_at, "data":data}),
            ),
            None => (503, json!({"error":"continuous_capture_no_data"})),
        }
    }

    /// Replace the single latest file only when its content changes. Advance
    /// saved state after success so a failed write is retried at the next tick.
    pub fn save(&self, path: &Path) -> anyhow::Result<bool> {
        let data = {
            let state = self.0.lock().unwrap();
            match &state.latest {
                Some(data) if state.saved.as_ref() != Some(data) => data.clone(),
                _ => return Ok(false),
            }
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let bytes = serde_json::to_vec_pretty(&data)?;
        // Compare against a previous server session as well.
        if std::fs::read(path).ok().as_deref() != Some(bytes.as_slice()) {
            let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
            let result = (|| -> std::io::Result<()> {
                std::fs::write(&temporary, bytes)?;
                std::fs::rename(&temporary, path)
            })();
            if result.is_err() {
                let _ = std::fs::remove_file(&temporary);
            }
            result?;
        }
        self.0.lock().unwrap().saved = Some(data);
        Ok(true)
    }
}

/// Periodic export policy shared by both games.
pub struct ExportTimer(Instant);
impl Default for ExportTimer {
    fn default() -> Self {
        Self(Instant::now())
    }
}
impl ExportTimer {
    pub fn tick(&mut self, capture: &ContinuousCapture, path: &Path) {
        if self.0.elapsed() < Duration::from_secs(600) {
            return;
        }
        self.0 = Instant::now();
        if let Err(error) = capture.save(path) {
            yas::log_warn!(
                "无法保存持续抓包导出：{}",
                "Could not save continuous capture export: {}",
                error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distinct_states_and_revisions() {
        assert_eq!(
            ContinuousCapture::response(None),
            (409, json!({"error":"continuous_capture_disabled"}))
        );
        let capture = ContinuousCapture::default();
        assert_eq!(
            ContinuousCapture::response(Some(&capture)),
            (503, json!({"error":"continuous_capture_no_data"}))
        );
        capture.publish(json!({"artifacts":[]}));
        let first = ContinuousCapture::response(Some(&capture));
        assert_eq!(first.0, 200);
        assert_eq!(first.1["revision"], 1);
        capture.publish(json!({"artifacts":[]}));
        assert_eq!(ContinuousCapture::response(Some(&capture)), first);
        capture.publish(json!({"artifacts":[{"level":4}]}));
        assert_eq!(ContinuousCapture::response(Some(&capture)).1["revision"], 2);
        capture.clear();
        assert_eq!(
            ContinuousCapture::response(Some(&capture)).1["error"],
            "continuous_capture_no_data"
        );
        capture.fail("listener closed".into());
        assert_eq!(
            ContinuousCapture::response(Some(&capture)).1["error"],
            "continuous_capture_failed"
        );
    }

    #[test]
    fn account_reset_is_visible_when_clients_miss_the_empty_interval() {
        let capture = ContinuousCapture::default();
        let first_account = json!({"metadata":{"uid":"800000001"}, "artifacts":[{"level":4}]});
        let second_account = json!({"metadata":{"uid":"800000002"}, "artifacts":[{"level":20}]});
        capture.publish(first_account.clone());
        let first = ContinuousCapture::response(Some(&capture)).1;
        capture.reset();
        capture.publish(second_account.clone());
        let second = ContinuousCapture::response(Some(&capture)).1;
        assert_ne!(first["sessionId"], second["sessionId"]);
        assert_eq!(second["revision"], 1);
        assert_eq!(second["data"], second_account);

        // Reconnecting to the same account also starts a fresh generation.
        capture.reset();
        capture.publish(second_account.clone());
        let reconnected = ContinuousCapture::response(Some(&capture)).1;
        assert_ne!(second["sessionId"], reconnected["sessionId"]);
        assert_eq!(reconnected["data"], second_account);

        capture.fail("old session failed".into());
        capture.reset();
        assert_eq!(
            ContinuousCapture::response(Some(&capture)),
            (503, json!({"error":"continuous_capture_no_data"}))
        );
        capture.publish(first_account.clone());
        assert_eq!(ContinuousCapture::response(Some(&capture)).1["data"], first_account);
    }

    #[test]
    fn periodic_export_replaces_only_changed_data_and_retries_failures() {
        let root = std::env::temp_dir().join(format!("continuous-{}", uuid::Uuid::new_v4()));
        let path = root.join("latest.json");
        let capture = ContinuousCapture::default();
        assert!(!capture.save(&path).unwrap());
        capture.publish(json!({"weapons":[]}));
        let mut timer = ExportTimer::default();
        timer.tick(&capture, &path);
        assert!(!path.exists());
        timer.0 -= Duration::from_secs(600);
        timer.tick(&capture, &path);
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!capture.save(&path).unwrap());
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        capture.publish(json!({"weapons":[{"level":80}]}));
        assert!(capture.save(&root.join("latest.json/invalid")).is_err());
        assert!(capture.save(&path).unwrap());
        assert_eq!(
            serde_json::from_slice::<Value>(&std::fs::read(&path).unwrap()).unwrap(),
            json!({"weapons":[{"level":80}]})
        );
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        // A fresh server session doesn't rewrite identical data from a previous run.
        let fresh = ContinuousCapture::default();
        fresh.publish(json!({"weapons":[{"level":80}]}));
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        fresh.save(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
