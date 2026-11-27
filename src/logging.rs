//! Bounded local operational logs. Events contain metadata and upstream errors,
//! never prompt bodies, authorization headers or stored credentials.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::json;

const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Clone, Default)]
pub struct Logger {
    path: Option<PathBuf>,
    lock: Arc<Mutex<()>>,
}

impl Logger {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self {
            path,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn event(&self, kind: &str, fields: serde_json::Value) {
        let Some(path) = &self.path else { return };
        let line = json!({
            "ts": chrono::Utc::now().to_rfc3339(),
            "kind": kind,
            "detail": fields,
        })
        .to_string();
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let rotate = std::fs::metadata(path)
            .map(|m| m.len() >= MAX_LOG_BYTES)
            .unwrap_or(false);
        if rotate {
            let old = path.with_extension("log.1");
            let _ = std::fs::rename(path, old);
        }
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let _ = writeln!(file, "{line}");
        }
    }
}
