//! Append-only audit log (`audit.jsonl`) of every remote action.

use serde::{Deserialize, Serialize};
use std::io::Write;
use xssh_core::paths::Paths;

const MAX_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuditRecord {
    pub ts: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_cwd: Option<String>,
}

impl AuditRecord {
    pub fn new(action: &str) -> Self {
        AuditRecord {
            ts: now_rfc3339(),
            action: action.to_string(),
            ok: true,
            ..Default::default()
        }
    }
}

pub fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false)
}

pub fn append(paths: &Paths, rec: &AuditRecord) {
    let file = paths.audit_file();
    if let Ok(meta) = std::fs::metadata(&file)
        && meta.len() > MAX_BYTES
    {
        let _ = std::fs::rename(&file, paths.home.join("audit.1.jsonl"));
    }
    if let Ok(line) = serde_json::to_string(rec)
        && let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&file)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// Last `n` records, optionally filtered by host.
pub fn tail(paths: &Paths, n: usize, host: Option<&str>) -> Vec<AuditRecord> {
    let mut out = vec![];
    for f in [paths.home.join("audit.1.jsonl"), paths.audit_file()] {
        if let Ok(s) = std::fs::read_to_string(&f) {
            for line in s.lines() {
                if let Ok(r) = serde_json::from_str::<AuditRecord>(line)
                    && host.is_none_or(|h| r.host.as_deref() == Some(h))
                {
                    out.push(r);
                }
            }
        }
    }
    let skip = out.len().saturating_sub(n);
    out.split_off(skip)
}
