use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const DEBUG_MARKER: &str = "debug.enabled";
const DEBUG_DIRECTORY: &str = "debug-logs";
const DEBUG_SCHEMA: &str = "needle.worker-debug/1";
const MAX_EVENT_BYTES: usize = 256 * 1024;
const MAX_LOG_FILES: usize = 20;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DebugLoggingStatus {
    pub enabled: bool,
    pub directory: PathBuf,
    pub latest: Option<PathBuf>,
}

pub fn enable_debug_logging(data_directory: &Path) -> Result<DebugLoggingStatus, String> {
    fs::create_dir_all(data_directory).map_err(|error| error.to_string())?;
    fs::create_dir_all(debug_directory(data_directory)).map_err(|error| error.to_string())?;
    fs::write(
        data_directory.join(DEBUG_MARKER),
        b"Needle worker debug logging enabled. Logs may contain local repository evidence.\n",
    )
    .map_err(|error| error.to_string())?;
    debug_logging_status(data_directory)
}

pub fn disable_debug_logging(data_directory: &Path) -> Result<DebugLoggingStatus, String> {
    let marker = data_directory.join(DEBUG_MARKER);
    if marker.is_file() {
        fs::remove_file(marker).map_err(|error| error.to_string())?;
    }
    debug_logging_status(data_directory)
}

pub fn debug_logging_status(data_directory: &Path) -> Result<DebugLoggingStatus, String> {
    let directory = debug_directory(data_directory);
    Ok(DebugLoggingStatus {
        enabled: data_directory.join(DEBUG_MARKER).is_file(),
        latest: latest_debug_log_in(&directory)?,
        directory,
    })
}

fn debug_directory(data_directory: &Path) -> PathBuf {
    data_directory.join(DEBUG_DIRECTORY)
}

fn latest_debug_log_in(directory: &Path) -> Result<Option<PathBuf>, String> {
    if !directory.is_dir() {
        return Ok(None);
    }
    let mut entries = fs::read_dir(directory)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                .then(|| entry.metadata().ok().map(|metadata| (metadata.modified().ok(), path)))
                .flatten()
        })
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    Ok(entries.pop().map(|(_, path)| path))
}

pub(crate) struct WorkerDebugLog {
    path: Option<PathBuf>,
}

impl WorkerDebugLog {
    pub(crate) fn start(data_directory: &Path, details: Value) -> Self {
        if !data_directory.join(DEBUG_MARKER).is_file() {
            return Self { path: None };
        }
        let directory = debug_directory(data_directory);
        if fs::create_dir_all(&directory).is_err() {
            return Self { path: None };
        }
        prune_debug_logs(&directory);
        let now = now_unix_ms();
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
        let path = directory.join(format!("worker-{now}-{}-{nonce}.jsonl", std::process::id()));
        let log = Self { path: Some(path) };
        log.event("start", details);
        log
    }

    pub(crate) fn event(&self, event: &str, details: Value) {
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let mut record = json!({
            "schema": DEBUG_SCHEMA,
            "created_unix_ms": now_unix_ms(),
            "event": event,
            "details": details,
        });
        let mut encoded = match serde_json::to_vec(&record) {
            Ok(encoded) => encoded,
            Err(_) => return,
        };
        if encoded.len() > MAX_EVENT_BYTES {
            record["details"] = json!({
                "omitted": true,
                "reason": "debug event exceeded the 256 KiB bound",
                "original_bytes": encoded.len(),
            });
            encoded = match serde_json::to_vec(&record) {
                Ok(encoded) => encoded,
                Err(_) => return,
            };
        }
        encoded.push(b'\n');
        if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
            let _ = file.write_all(&encoded);
        }
    }

    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }
}

fn prune_debug_logs(directory: &Path) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let path = entry.path();
            (path.extension().and_then(|value| value.to_str()) == Some("jsonl"))
                .then(|| entry.metadata().ok().map(|metadata| (metadata.modified().ok(), path)))
                .flatten()
        })
        .collect::<Vec<_>>();
    logs.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));
    let remove_count = logs.len().saturating_sub(MAX_LOG_FILES.saturating_sub(1));
    for (_, path) in logs.into_iter().take(remove_count) {
        let _ = fs::remove_file(path);
    }
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_directory(name: &str) -> PathBuf {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        std::env::temp_dir().join(format!("needle-debug-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn debug_logging_is_explicit_and_latest_is_discoverable() {
        let root = temporary_directory("lifecycle");
        assert!(!debug_logging_status(&root).unwrap().enabled);

        let enabled = enable_debug_logging(&root).unwrap();
        assert!(enabled.enabled);
        let log = WorkerDebugLog::start(&root, json!({"route": "trace.state-flow"}));
        log.event("response", json!({"artifacts": []}));
        let path = log.path().unwrap().to_path_buf();
        assert_eq!(debug_logging_status(&root).unwrap().latest, Some(path.clone()));
        let contents = fs::read_to_string(path).unwrap();
        assert!(contents.contains("needle.worker-debug/1"));
        assert!(contents.contains("trace.state-flow"));

        assert!(!disable_debug_logging(&root).unwrap().enabled);
        let _ = fs::remove_dir_all(root);
    }
}
