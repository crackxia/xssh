//! The daemon protocol: request/response envelopes and every type that crosses the IPC
//! boundary between front ends (CLI, desktop app) and the engine.

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;

/// Bump when requests or the result shapes front ends rely on change incompatibly.
pub const PROTOCOL: u32 = 1;

static BUILD_ID: OnceLock<&'static str> = OnceLock::new();

/// Set once at startup by the binary that embeds the engine (the CLI, which also runs the
/// daemon), so a rebuilt CLI detects and restarts a daemon started by an older build.
pub fn set_build_id(id: &'static str) {
    let _ = BUILD_ID.set(id);
}

pub fn build_id() -> &'static str {
    BUILD_ID.get().copied().unwrap_or("0")
}

pub fn version_tag() -> String {
    format!("{}+{}/{}", env!("CARGO_PKG_VERSION"), build_id(), PROTOCOL)
}

/// True when both version tags speak the same protocol.
pub fn same_protocol(a: &str, b: &str) -> bool {
    a.rsplit_once('/').map(|x| x.1) == b.rsplit_once('/').map(|x| x.1)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Envelope {
    pub token: String,
    pub version: String,
    pub client_pid: u32,
    #[serde(default)]
    pub client_cwd: Option<String>,
    /// A monitoring front end (desktop app): accepted by any daemon with the same protocol
    /// (never forces a restart that would drop sessions), and its polling does not keep an
    /// otherwise idle daemon alive.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub observer: bool,
    pub request: Request,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Err {
        error: Error,
    },
    Ok {
        result: serde_json::Value,
        /// Set when a daemon of another build (same protocol) served the request because it was
        /// busy with sessions/forwards; the client tells the user once.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        daemon: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Shutdown,
    DaemonStatus,

    HostTest {
        host: String,
    },
    /// Without `fingerprint`: show the presented key. With it: record that key if the server
    /// still presents exactly it.
    HostTrust {
        host: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fingerprint: Option<String>,
    },
    HostDisconnect {
        host: String,
    },
    KeyDeploy {
        host: String,
        key: String,
    },

    Exec(ExecParams),

    SessionOpen(OpenParams),
    SessionRun {
        id: String,
        command: String,
        timeout_ms: u64,
        /// Who is asking (per-agent read position); default: the "default" reader.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    SessionSend {
        id: String,
        #[serde(default)]
        text: Option<String>,
        #[serde(default)]
        keys: Option<String>,
        #[serde(default)]
        enter: bool,
        wait_ms: u64,
        timeout_ms: u64,
        /// Only send when the current prompt kind matches (shell|password|confirm|input|repl|pager|any|screen).
        #[serde(default)]
        if_prompt: Option<String>,
        /// Send `text` as a (bracketed) paste.
        #[serde(default)]
        paste: bool,
        #[serde(default)]
        wait: WaitSpec,
        /// Who is asking (per-agent read position); default: the "default" reader.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    SessionRead {
        id: String,
        wait_ms: u64,
        timeout_ms: u64,
        #[serde(default)]
        wait: WaitSpec,
        /// Who is asking (per-agent read position); default: the "default" reader.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    SessionExpect {
        id: String,
        pattern: String,
        timeout_ms: u64,
        /// Who is asking (per-agent read position); default: the "default" reader.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<String>,
    },
    SessionScreen {
        id: String,
    },
    /// The screen without counting as activity (monitoring: keeps idle timers and agents' view intact).
    SessionPeek {
        id: String,
    },
    SessionResize {
        id: String,
        cols: u16,
        rows: u16,
    },
    SessionClose {
        id: String,
    },
    SessionList,

    JobStart(JobStartParams),
    JobStatus {
        id: String,
    },
    JobLogs {
        id: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        tail: Option<usize>,
        max_bytes: usize,
    },
    JobWait {
        id: String,
        timeout_ms: u64,
        tail: usize,
    },
    JobKill {
        id: String,
        signal: String,
    },
    JobRm {
        id: String,
    },
    JobList {
        #[serde(default)]
        host: Option<String>,
        /// Include finished jobs older than a week.
        #[serde(default)]
        all: bool,
    },
    /// Delete finished jobs (remote files and records) older than `older_than_secs`.
    JobPrune {
        #[serde(default)]
        host: Option<String>,
        #[serde(default)]
        older_than_secs: u64,
    },

    FileRead {
        host: String,
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
        #[serde(default)]
        sudo: bool,
        #[serde(default)]
        encoding: Option<String>,
    },
    FileWrite {
        host: String,
        path: String,
        content_b64: String,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        backup: bool,
        #[serde(default)]
        mkdirs: bool,
        #[serde(default)]
        sudo: bool,
        /// Only write if the current file has this sha256 (from `file read`).
        #[serde(default)]
        expect_sha256: Option<String>,
        /// Replace `{secret:NAME}` (and `{password}`, `{sudo_password}`) in the content.
        #[serde(default)]
        secrets: bool,
    },
    FileEdit {
        host: String,
        path: String,
        old: String,
        new: String,
        #[serde(default)]
        replace_all: bool,
        #[serde(default)]
        backup: bool,
        #[serde(default)]
        sudo: bool,
        /// Replace `{secret:NAME}` (and `{password}`, `{sudo_password}`) in old/new; the
        /// returned snippet shows `***` instead of their values.
        #[serde(default)]
        secrets: bool,
    },
    FileLs {
        host: String,
        path: String,
    },
    FileStat {
        host: String,
        path: String,
    },
    Copy {
        srcs: Vec<Endpoint>,
        dst: Endpoint,
        #[serde(default)]
        sudo: bool,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        opts: CopyOptions,
    },

    Status {
        host: String,
        #[serde(default)]
        only: Vec<String>,
        top: usize,
        /// Run the probes as root (ports' processes, docker, kernel log).
        #[serde(default)]
        sudo: bool,
    },
    Perf {
        host: String,
        duration_ms: u64,
        interval_ms: u64,
        series: bool,
    },
    Diag {
        host: String,
        #[serde(default)]
        sudo: bool,
    },
    Logs {
        host: String,
        #[serde(default)]
        unit: Option<String>,
        #[serde(default)]
        file: Option<String>,
        #[serde(default)]
        since: Option<String>,
        #[serde(default)]
        grep: Option<String>,
        #[serde(default)]
        priority: Option<String>,
        tail: usize,
        #[serde(default)]
        sudo: bool,
    },

    ForwardAdd {
        host: String,
        #[serde(default)]
        local: Option<String>,
        #[serde(default)]
        remote: Option<String>,
        /// `[bind:]port`: SOCKS5 proxy on this machine, connections go out from HOST.
        #[serde(default)]
        dynamic: Option<String>,
    },
    ForwardList,
    ForwardStop {
        id: String,
    },
}

impl Request {
    /// Requests that only make sense inside a long-lived daemon.
    pub fn needs_daemon(&self) -> bool {
        matches!(
            self,
            Request::SessionOpen(_)
                | Request::SessionRun { .. }
                | Request::SessionSend { .. }
                | Request::SessionRead { .. }
                | Request::SessionExpect { .. }
                | Request::SessionScreen { .. }
                | Request::SessionPeek { .. }
                | Request::SessionResize { .. }
                | Request::SessionClose { .. }
                | Request::SessionList
                | Request::ForwardAdd { .. }
                | Request::ForwardList
                | Request::ForwardStop { .. }
        )
    }

    pub fn name(&self) -> &'static str {
        match self {
            Request::Ping => "ping",
            Request::Shutdown => "shutdown",
            Request::DaemonStatus => "daemon_status",
            Request::HostTest { .. } => "host_test",
            Request::HostTrust { .. } => "host_trust",
            Request::HostDisconnect { .. } => "host_disconnect",
            Request::KeyDeploy { .. } => "key_deploy",
            Request::Exec(_) => "exec",
            Request::SessionOpen(_) => "session_open",
            Request::SessionRun { .. } => "session_run",
            Request::SessionSend { .. } => "session_send",
            Request::SessionRead { .. } => "session_read",
            Request::SessionExpect { .. } => "session_expect",
            Request::SessionScreen { .. } => "session_screen",
            Request::SessionPeek { .. } => "session_peek",
            Request::SessionResize { .. } => "session_resize",
            Request::SessionClose { .. } => "session_close",
            Request::SessionList => "session_list",
            Request::JobStart(_) => "job_start",
            Request::JobStatus { .. } => "job_status",
            Request::JobLogs { .. } => "job_logs",
            Request::JobWait { .. } => "job_wait",
            Request::JobKill { .. } => "job_kill",
            Request::JobRm { .. } => "job_rm",
            Request::JobList { .. } => "job_list",
            Request::JobPrune { .. } => "job_prune",
            Request::FileRead { .. } => "file_read",
            Request::FileWrite { .. } => "file_write",
            Request::FileEdit { .. } => "file_edit",
            Request::FileLs { .. } => "file_ls",
            Request::FileStat { .. } => "file_stat",
            Request::Copy { .. } => "cp",
            Request::Status { .. } => "status",
            Request::Perf { .. } => "perf",
            Request::Diag { .. } => "diag",
            Request::Logs { .. } => "logs",
            Request::ForwardAdd { .. } => "forward_add",
            Request::ForwardList => "forward_list",
            Request::ForwardStop { .. } => "forward_stop",
        }
    }
}

// ---- exec / jobs ----

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecParams {
    pub host: String,
    pub command: String,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub sudo: bool,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// (VAR, secret name) pairs: the secret is delivered via stdin, never on the command line.
    #[serde(default)]
    pub env_secrets: Vec<(String, String)>,
    /// Base64 data fed to the command's stdin.
    #[serde(default)]
    pub stdin_b64: Option<String>,
    #[serde(default)]
    pub max_output: Option<usize>,
    #[serde(default)]
    pub pty: bool,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub parallel: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JobStartParams {
    pub host: String,
    pub command: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub sudo: bool,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// (VAR, SECRET_NAME): stored secrets exported to the job without appearing in its command.
    #[serde(default)]
    pub env_secrets: Vec<(String, String)>,
}

// ---- sessions ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// Output is still arriving.
    Running,
    /// Output stopped and the current line looks like a prompt.
    WaitingInput,
    /// Output stopped but the current line is not a recognizable prompt
    /// (a silent long-running command, or an unusual prompt).
    Quiet,
    /// The remote shell exited / channel closed.
    Exited,
    /// The connection was lost. A persistent session (`--persist`) keeps running in tmux on the
    /// host and is reattached by the next call; any other session is gone.
    Disconnected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptKind {
    /// Password / passphrase prompt.
    Password,
    /// Yes/no style confirmation.
    Confirm,
    /// Shell prompt: the previous command finished.
    Shell,
    /// Interpreter / database REPL prompt.
    Repl,
    /// Pager (less/more) waiting for a key.
    Pager,
    /// Generic question ending in ':' or '?'.
    Input,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponderRule {
    /// Regex matched against the current (last) line of terminal output.
    pub pattern: String,
    /// Text to send. May contain `{password}`, `{sudo_password}` or `{secret:NAME}`.
    pub reply: String,
    /// Append Enter after the reply.
    #[serde(default = "default_true")]
    pub enter: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OpenParams {
    pub host: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cols: Option<u16>,
    #[serde(default)]
    pub rows: Option<u16>,
    /// Extra auto-responder rules.
    #[serde(default)]
    pub responders: Vec<ResponderRule>,
    #[serde(default)]
    pub encoding: Option<String>,
    #[serde(default)]
    pub term: Option<String>,
    /// Disable automatic sudo password filling.
    #[serde(default)]
    pub no_autofill: bool,
    /// Regexes that mark a TUI as busy while visible on screen (e.g. "esc to interrupt").
    #[serde(default)]
    pub busy_patterns: Vec<String>,
    /// Run the shell inside tmux on the host: it survives disconnects and daemon restarts.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persist: bool,
    /// Who opens it (recorded as the session's owner).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
}

/// What to wait for after input (or on its own with `session wait`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WaitSpec {
    /// Return as soon as this regex matches the screen (or new output).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Return once this regex no longer matches the screen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gone: Option<String>,
    /// Return once the screen has not changed (ignoring spinners and counters) for this long
    /// and no busy indicator is visible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_ms: Option<u64>,
    /// `session wait` without conditions: wait for the running command to finish (its end
    /// marker, or the shell prompt after a command typed with `send`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub run_end: bool,
}

impl WaitSpec {
    pub fn is_screen_wait(&self) -> bool {
        self.until.is_some() || self.gone.is_some() || self.stable_ms.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScreenSnapshot {
    pub session: String,
    pub rows: u16,
    pub cols: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub alt_screen: bool,
    pub state: SessionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptKind>,
    pub screen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub host: String,
    pub created_at: String,
    pub idle_secs: u64,
    pub state: SessionState,
    pub cols: u16,
    pub rows: u16,
    pub transcript: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_run: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub last: String,
    /// Last command started with `session run` (e.g. the program now running).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub cmd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<PromptKind>,
    /// The agent that opened it (`--as` / XSSH_AGENT).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub owner: String,
    /// Backed by tmux on the host (`--persist`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub persistent: bool,
}

impl SessionInfo {
    /// The handle agents use: the name if set, else the id.
    pub fn key(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.id)
    }
}

// ---- files ----

/// A local path (host = None, absolute) or a path on a saved host.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Endpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub path: String,
}

impl Endpoint {
    pub fn show(&self) -> String {
        match &self.host {
            Some(h) => format!("{h}:{}", self.path),
            None => self.path.clone(),
        }
    }
}

/// `xssh cp` behaviour beyond sources and destination.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CopyOptions {
    /// Compare file contents (sha256) instead of size + mtime to find unchanged files.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub checksum: bool,
    /// Glob patterns (`*`, `?`, `**`) of paths to leave out, matched against the path inside a
    /// copied directory and against each name.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
    /// Delete destination files that are not in the source (directory copies only).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete: bool,
    /// Report what would be copied / deleted without changing anything.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Copy symlinks as symlinks instead of following them.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub links: bool,
    /// Re-read every written file on the destination and compare its sha256.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify: bool,
    /// Copy every file even when the destination looks identical.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub all: bool,
}

// ---- port forwards ----

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Local,
    Remote,
    /// SOCKS5 proxy (`ssh -D`).
    Dynamic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub kind: Kind,
    pub bind_addr: String,
    pub bind_port: u16,
    pub target_host: String,
    pub target_port: u16,
}

impl Spec {
    /// `[bind_addr:]port:host:hostport`, or `[bind_addr:]port` for `Kind::Dynamic`.
    pub fn parse(kind: Kind, s: &str) -> Result<Spec> {
        let parts = split_spec(s);
        if kind == Kind::Dynamic {
            let (bind_addr, port) = match parts.as_slice() {
                [p] => ("127.0.0.1".to_string(), p.as_str()),
                [b, p] => (b.clone(), p.as_str()),
                _ => return Err(Error::usage(format!("bad dynamic forward spec '{s}' (expected [bind:]port)"))),
            };
            let bind_port: u16 = port.parse().map_err(|_| Error::usage(format!("bad port in '{s}'")))?;
            return Ok(Spec {
                kind,
                bind_addr,
                bind_port,
                target_host: String::new(),
                target_port: 0,
            });
        }
        let (bind_addr, bind_port, th, tp) = match parts.as_slice() {
            [p, h, hp] => ("127.0.0.1".to_string(), p.as_str(), h.clone(), hp.as_str()),
            [b, p, h, hp] => (b.clone(), p.as_str(), h.clone(), hp.as_str()),
            _ => return Err(Error::usage(format!("bad forward spec '{s}' (expected [bind:]port:host:hostport)"))),
        };
        let bind_port: u16 = bind_port.parse().map_err(|_| Error::usage(format!("bad port in '{s}'")))?;
        let target_port: u16 = tp.parse().map_err(|_| Error::usage(format!("bad port in '{s}'")))?;
        Ok(Spec {
            kind,
            bind_addr,
            bind_port,
            target_host: th,
            target_port,
        })
    }
}

/// Split on ':' but keep bracketed IPv6 addresses together.
fn split_spec(s: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            _ => {}
        }
        if c != '[' && c != ']' {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForwardInfo {
    pub id: String,
    pub host: String,
    pub spec: Spec,
    pub created_at: String,
    pub connections: u64,
    pub alive: bool,
    pub description: String,
    /// Connections that could not be forwarded (channel refused, target unreachable...).
    #[serde(default)]
    pub failures: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Times a remote (-R) forward was re-established after its connection dropped.
    #[serde(default)]
    pub reconnects: u64,
}
