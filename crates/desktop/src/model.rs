//! Daemon state shared by the pages, refreshed by one polling task.

use serde_json::Value;
use xssh_core::Error;
use xssh_core::api::{ForwardInfo, SessionInfo};

#[derive(Clone, Debug, PartialEq)]
pub enum DaemonStatus {
    /// Not polled yet.
    Unknown,
    Running,
    Stopped,
    /// Running but unusable (e.g. a different protocol version).
    Error(String),
}

pub struct DaemonModel {
    pub status: DaemonStatus,
    pub pid: Option<u64>,
    pub uptime_secs: u64,
    pub version: String,
    pub home: String,
    pub secret_backend: String,
    /// (alias, SSH connections to it)
    pub connections: Vec<(String, u64)>,
    pub sessions: Vec<SessionInfo>,
    pub forwards: Vec<ForwardInfo>,
    /// A start/stop/restart in progress.
    pub busy: bool,
}

impl DaemonModel {
    pub fn new() -> Self {
        DaemonModel {
            status: DaemonStatus::Unknown,
            pid: None,
            uptime_secs: 0,
            version: String::new(),
            home: String::new(),
            secret_backend: String::new(),
            connections: vec![],
            sessions: vec![],
            forwards: vec![],
            busy: false,
        }
    }

    pub fn running(&self) -> bool {
        self.status == DaemonStatus::Running
    }

    /// Apply a `DaemonStatus` response.
    pub fn apply(&mut self, r: Result<Value, Error>) {
        match r {
            Ok(v) => {
                self.status = DaemonStatus::Running;
                self.pid = v["pid"].as_u64();
                self.uptime_secs = v["uptime_secs"].as_u64().unwrap_or(0);
                self.version = v["version"].as_str().unwrap_or_default().to_string();
                self.home = v["home"].as_str().unwrap_or_default().to_string();
                self.secret_backend = v["secret_backend"].as_str().unwrap_or_default().to_string();
                self.connections = v["connections"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .map(|c| {
                                (
                                    c["alias"].as_str().unwrap_or_default().to_string(),
                                    c["conns"].as_u64().unwrap_or(1),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.sessions = serde_json::from_value(v["sessions"].clone()).unwrap_or_default();
                self.forwards = serde_json::from_value(v["forwards"].clone()).unwrap_or_default();
            }
            Err(e) => {
                self.status = if crate::backend::is_not_running(&e) {
                    DaemonStatus::Stopped
                } else {
                    DaemonStatus::Error(match &e.hint {
                        Some(h) => format!("{}（{h}）", e.message),
                        None => e.message.clone(),
                    })
                };
                self.pid = None;
                self.connections.clear();
                self.sessions.clear();
                self.forwards.clear();
            }
        }
    }
}
