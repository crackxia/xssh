//! CLI-side request routing: talk to the daemon (auto-starting it), or run
//! in-process with `--no-daemon`.

use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::OnceCell;
use xssh_core::api::Request;
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::ipc::{self, CallError};
use xssh_core::paths::Paths;
use xssh_engine::engine::{ClientInfo, Engine};
use xssh_engine::ssh::Ctx;

pub struct Client {
    pub paths: Paths,
    pub no_daemon: bool,
    local: OnceCell<Arc<Engine>>,
}

impl Client {
    pub fn new(paths: Paths, no_daemon: bool) -> Self {
        Client {
            paths,
            no_daemon,
            local: OnceCell::new(),
        }
    }

    pub async fn call(&self, req: Request) -> Result<Value> {
        if self.no_daemon {
            if req.needs_daemon() {
                return Err(Error::usage(format!(
                    "'{}' needs the daemon (sessions/forwards live there); drop --no-daemon",
                    req.name()
                )));
            }
            let engine = self
                .local
                .get_or_try_init(|| async { Ok::<_, Error>(Engine::new(Ctx::new(self.paths.clone())?, false)) })
                .await?;
            let client = ClientInfo {
                pid: Some(std::process::id()),
                cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
            };
            return engine.handle(req, client).await;
        }
        let mut restarted = false;
        let mut token_retries = 0;
        loop {
            match ipc::request(&self.paths, &req).await {
                Ok(v) => return Ok(v),
                // A freshly started daemon may not have written its token yet.
                Err(CallError::Failed(e)) if e.code == ErrorCode::Daemon && e.message == "invalid daemon token" && token_retries < 20 => {
                    token_retries += 1;
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Err(CallError::Failed(e)) => return Err(e),
                Err(CallError::NotRunning) => {
                    if matches!(req, Request::Shutdown) {
                        return Ok(serde_json::json!({"stopping": false, "running": false}));
                    }
                    self.start_daemon().await?;
                }
                Err(CallError::VersionMismatch) => {
                    if restarted {
                        return Err(Error::new(ErrorCode::Daemon, "daemon version mismatch persists after restart"));
                    }
                    // Only replace an idle daemon: sessions and forwards belong to other agents too.
                    if let Ok(st) = ipc::observe(&self.paths, &Request::DaemonStatus).await {
                        let n = |k: &str| st.get(k).and_then(Value::as_array).map_or(0, Vec::len);
                        let (sessions, forwards) = (n("sessions"), n("forwards"));
                        if sessions + forwards > 0 {
                            let ver = st.get("version").and_then(Value::as_str).unwrap_or("?");
                            return Err(Error::new(
                                ErrorCode::Daemon,
                                format!(
                                    "the running daemon is another xssh build ({ver}) and has {sessions} session(s), {forwards} forward(s) open"
                                ),
                            )
                            .hint(
                                "`xssh daemon restart` switches to this build but closes them (other agents may be using them); or use the xssh build that started it"
                            ));
                        }
                    }
                    restarted = true;
                    let _ = ipc::request(&self.paths, &Request::Shutdown).await;
                    ipc::wait_stopped(&self.paths, Duration::from_secs(5)).await;
                    self.start_daemon().await?;
                }
            }
        }
    }

    /// Call only if a daemon is already running (never spawns one).
    pub async fn call_if_running(&self, req: Request) -> Option<Result<Value>> {
        if self.no_daemon {
            return None;
        }
        match ipc::request(&self.paths, &req).await {
            Ok(v) => Some(Ok(v)),
            Err(CallError::NotRunning) => None,
            Err(CallError::VersionMismatch) => None,
            Err(CallError::Failed(e)) => Some(Err(e)),
        }
    }

    pub async fn start_daemon(&self) -> Result<()> {
        ipc::spawn_daemon(&self.paths)?;
        if !ipc::wait_ready(&self.paths, Duration::from_secs(10)).await {
            return Err(Error::new(ErrorCode::Daemon, "daemon did not start within 10s").hint(format!(
                "see {} or run `xssh daemon run` in the foreground to debug",
                self.paths.daemon_log().display()
            )));
        }
        Ok(())
    }
}
