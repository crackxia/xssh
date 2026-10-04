//! Global settings (`config.toml`). Every field has a sensible default so the
//! file is optional.

use serde::{Deserialize, Serialize};
use xssh_core::error::Result;
use xssh_core::paths::Paths;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Max bytes of stdout/stderr returned inline; the rest is saved to a file.
    pub max_output_bytes: usize,
    /// Default timeout for `exec`, seconds.
    pub exec_timeout_secs: u64,
    /// TCP connect + handshake timeout, seconds.
    pub connect_timeout_secs: u64,
    /// SSH keepalive interval, seconds (0 disables).
    pub keepalive_secs: u64,
    /// Daemon exits after this many idle seconds with no sessions/forwards.
    pub daemon_idle_exit_secs: u64,
    /// Interactive sessions idle longer than this are closed.
    pub session_idle_close_secs: u64,
    /// Idle pooled connections are dropped after this many seconds.
    pub connection_idle_secs: u64,
    pub session_cols: u16,
    pub session_rows: u16,
    /// "tofu" (trust on first use, fail on change) or "strict" (fail on unknown).
    pub host_key_policy: String,
    /// Also consult ~/.ssh/known_hosts when verifying host keys.
    pub use_system_known_hosts: bool,
    /// Try ssh-agent / Pageant when no explicit key is configured.
    pub use_agent: bool,
    /// Try ~/.ssh/id_* default keys when no explicit key is configured.
    pub use_default_keys: bool,
    /// Prefer zlib compression on SSH connections (helps slow links; costs CPU). Hosts can
    /// override it (`host edit --compression yes|no`).
    pub compression: bool,
    /// Extra auto-responder rules applied to every interactive session.
    pub responders: Vec<ResponderRule>,
    /// Delete saved full outputs older than this many hours.
    pub outputs_retention_hours: u64,
    /// Regexes that mark a full-screen program as still working while visible on screen;
    /// screen waits do not count it as settled until they disappear.
    pub busy_patterns: Vec<String>,
    /// `file write/edit` backups kept per file in the host's ~/.xssh/backups (0 = all).
    pub backup_keep: usize,
    /// Backups older than this many days are deleted on the next write (0 = never).
    pub backup_retention_days: u64,
    /// Local records of finished jobs older than this many days are forgotten (0 = never).
    pub job_retention_days: u64,
}

pub use xssh_core::api::ResponderRule;

impl Default for Config {
    fn default() -> Self {
        Config {
            // ~4k tokens; longer output keeps head + tail inline and saves the full text to a file.
            max_output_bytes: 16_000,
            // Below the default 120s tool timeout of common agent shells.
            exec_timeout_secs: 100,
            connect_timeout_secs: 15,
            keepalive_secs: 30,
            daemon_idle_exit_secs: 3600,
            session_idle_close_secs: 3600,
            connection_idle_secs: 900,
            session_cols: 200,
            session_rows: 50,
            host_key_policy: "tofu".into(),
            use_system_known_hosts: true,
            use_agent: true,
            use_default_keys: true,
            compression: false,
            responders: vec![],
            outputs_retention_hours: 48,
            busy_patterns: vec![
                // Claude Code, Codex and similar agent CLIs while generating.
                r"(?i)\besc to (interrupt|cancel)\b".into(),
                r"(?i)\bctrl\+c to (interrupt|cancel|stop)\b".into(),
            ],
            backup_keep: 20,
            backup_retention_days: 30,
            job_retention_days: 30,
        }
    }
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let f = paths.config_file();
        if !f.exists() {
            return Ok(Config::default());
        }
        let s = std::fs::read_to_string(&f)?;
        let c: Config = toml::from_str(&s)?;
        // A typo must not silently fall back to trust-on-first-use.
        crate::hosts::validate_host_key_policy(&c.host_key_policy).map_err(|e| {
            let mut e = e;
            e.message = format!("{}: {}", f.display(), e.message);
            e
        })?;
        Ok(c)
    }
}
