//! Everything the desktop app does outside the UI: daemon requests over local IPC, daemon
//! lifecycle through the `xssh` CLI, and direct access to the local store (hosts, secrets,
//! audit log, job records).
//!
//! The app is an *observer* of the daemon: it never starts one implicitly while polling, never
//! forces a version restart (that would drop agents' sessions), and its polling does not keep an
//! idle daemon alive. Closing the app leaves the daemon untouched.

use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;
use xssh_core::api::Request;
use xssh_core::ipc::{self, CallError};
use xssh_core::paths::Paths;
use xssh_core::{Error, ErrorCode, Result};
use xssh_store::audit::{self, AuditRecord};
use xssh_store::hosts::{Host, HostStore};
use xssh_store::jobs::{self, JobRecord};
use xssh_store::secrets::{self, SecretStore};

/// IPC runs on tokio; GPUI tasks await the join handles.
static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime")
});

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SecretKind {
    Password,
    Sudo,
    Passphrase,
}

impl SecretKind {
    pub fn label(self) -> &'static str {
        match self {
            SecretKind::Password => "登录密码",
            SecretKind::Sudo => "sudo 密码",
            SecretKind::Passphrase => "私钥口令",
        }
    }

    fn name(self, alias: &str) -> String {
        match self {
            SecretKind::Password => secrets::host_password(alias),
            SecretKind::Sudo => secrets::host_sudo(alias),
            SecretKind::Passphrase => secrets::host_passphrase(alias),
        }
    }
}

pub struct Backend {
    pub paths: Paths,
}

impl Backend {
    /// `--home DIR` or `XSSH_HOME`, else `data` next to this executable (same rules as the CLI,
    /// so the two share one home when installed side by side).
    pub fn open() -> Result<Arc<Self>> {
        let mut args = std::env::args().skip(1);
        let mut home: Option<PathBuf> = std::env::var_os("XSSH_HOME").map(PathBuf::from);
        while let Some(a) = args.next() {
            if a == "--home" {
                home = args.next().map(PathBuf::from);
            } else if let Some(v) = a.strip_prefix("--home=") {
                home = Some(PathBuf::from(v));
            }
        }
        Ok(Arc::new(Backend {
            paths: Paths::resolve(home.as_deref())?,
        }))
    }

    /// A request to a running daemon; never starts one.
    pub fn observe(&self, req: Request) -> impl Future<Output = Result<Value>> + use<> {
        let paths = self.paths.clone();
        run(async move { observe(&paths, &req).await })
    }

    /// A user action: starts the daemon first when it is not running.
    pub fn act(&self, req: Request) -> impl Future<Output = Result<Value>> + use<> {
        let paths = self.paths.clone();
        run(async move {
            match observe(&paths, &req).await {
                Err(e) if is_not_running(&e) => {
                    start_daemon(&paths).await?;
                    observe(&paths, &req).await
                }
                r => r,
            }
        })
    }

    pub fn start_daemon(&self) -> impl Future<Output = Result<()>> + use<> {
        let paths = self.paths.clone();
        run(async move { start_daemon(&paths).await })
    }

    /// Stop the daemon: closes every session and forward it holds.
    pub fn stop_daemon(&self) -> impl Future<Output = Result<()>> + use<> {
        let paths = self.paths.clone();
        run(async move { stop_daemon(&paths).await })
    }

    pub fn restart_daemon(&self) -> impl Future<Output = Result<()>> + use<> {
        let paths = self.paths.clone();
        run(async move {
            stop_daemon(&paths).await?;
            start_daemon(&paths).await
        })
    }

    pub fn daemon_log(&self, lines: usize) -> String {
        let s = std::fs::read_to_string(self.paths.daemon_log()).unwrap_or_default();
        let all: Vec<&str> = s.lines().collect();
        all[all.len().saturating_sub(lines)..].join("\n")
    }

    // ---- local store ----

    pub fn hosts(&self) -> Result<Vec<Host>> {
        HostStore::new(&self.paths).list()
    }

    /// Add (`original` = None) or replace a host. Renaming moves its stored secrets.
    pub fn save_host(&self, original: Option<&str>, host: Host) -> Result<()> {
        xssh_store::hosts::validate_alias(&host.alias)?;
        let store = HostStore::new(&self.paths);
        match original {
            None => store.add(host.clone())?,
            Some(orig) => {
                store.update(|hosts| {
                    if orig != host.alias && hosts.iter().any(|h| h.alias == host.alias) {
                        return Err(Error::new(ErrorCode::AlreadyExists, format!("主机别名 '{}' 已存在", host.alias)));
                    }
                    let h = hosts
                        .iter_mut()
                        .find(|h| h.alias == orig)
                        .ok_or_else(|| Error::not_found(format!("主机 '{orig}' 不存在")))?;
                    // The form edits only these fields; keep the rest (facts, proxy command,
                    // extra keys, policies, expiry...).
                    *h = Host {
                        alias: host.alias.clone(),
                        host: host.host.clone(),
                        port: host.port,
                        user: host.user.clone(),
                        key: host.key.clone(),
                        key_name: host.key_name.clone(),
                        jump: host.jump.clone(),
                        tags: host.tags.clone(),
                        note: host.note.clone(),
                        encoding: host.encoding.clone(),
                        ..h.clone()
                    };
                    if orig != host.alias {
                        for other in hosts.iter_mut() {
                            if other.jump.as_deref() == Some(orig) {
                                other.jump = Some(host.alias.clone());
                            }
                        }
                    }
                    Ok(())
                })?;
                if orig != host.alias {
                    let store = SecretStore::open(&self.paths)?;
                    for kind in [SecretKind::Password, SecretKind::Sudo, SecretKind::Passphrase] {
                        if let Some(v) = store.get(&kind.name(orig))? {
                            store.set(&kind.name(&host.alias), &v)?;
                        }
                    }
                    store.forget_host(orig);
                }
                self.disconnect(orig);
            }
        }
        Ok(())
    }

    pub fn remove_host(&self, alias: &str) -> Result<()> {
        HostStore::new(&self.paths).remove(alias)?;
        SecretStore::open(&self.paths)?.forget_host(alias);
        self.disconnect(alias);
        Ok(())
    }

    /// Which secrets are stored per host (from the secret index; never values).
    pub fn stored_secrets(&self, aliases: &[String]) -> HashMap<String, Vec<SecretKind>> {
        let names = SecretStore::open(&self.paths).and_then(|s| s.names()).unwrap_or_default();
        aliases
            .iter()
            .map(|a| {
                let kinds = [SecretKind::Password, SecretKind::Sudo, SecretKind::Passphrase]
                    .into_iter()
                    .filter(|k| names.contains(&k.name(a)))
                    .collect();
                (a.clone(), kinds)
            })
            .collect()
    }

    /// Store (Some) or delete (None) a host secret in the OS keyring / encrypted file.
    pub fn set_secret(&self, alias: &str, kind: SecretKind, value: Option<&str>) -> Result<()> {
        let store = SecretStore::open(&self.paths)?;
        match value {
            Some(v) => store.set(&kind.name(alias), v)?,
            None => {
                store.delete(&kind.name(alias))?;
            }
        }
        self.disconnect(alias);
        Ok(())
    }

    /// Drop the daemon's pooled connection so the next use picks up changed settings.
    fn disconnect(&self, alias: &str) {
        let paths = self.paths.clone();
        let host = alias.to_string();
        RT.spawn(async move {
            let _ = ipc::observe(&paths, &Request::HostDisconnect { host }).await;
        });
    }

    pub fn audit(&self, n: usize) -> Vec<AuditRecord> {
        let mut v = audit::tail(&self.paths, n, None);
        v.reverse();
        v
    }

    pub fn job_records(&self) -> Vec<JobRecord> {
        let mut v = jobs::list(&self.paths).unwrap_or_default();
        v.reverse();
        v
    }

    /// The newest session transcripts (file stem, path), newest first.
    pub fn closed_transcripts(&self, n: usize) -> Vec<(String, PathBuf)> {
        let mut v: Vec<(std::time::SystemTime, String, PathBuf)> = std::fs::read_dir(self.paths.sessions_dir())
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().strip_suffix(".log")?.to_string();
                let t = e.metadata().and_then(|m| m.modified()).ok()?;
                Some((t, name, e.path()))
            })
            .collect();
        v.sort_by_key(|e| std::cmp::Reverse(e.0));
        v.into_iter().take(n).map(|(_, n, p)| (n, p)).collect()
    }

    pub fn transcript(&self, path: &str, lines: usize) -> Vec<String> {
        let s = std::fs::read_to_string(path).unwrap_or_default();
        let mut all = xssh_core::transcript::render(&s);
        all.split_off(all.len().saturating_sub(lines))
    }
}

/// Run `f` on the tokio runtime; the returned future can be awaited from GPUI tasks.
fn run<T: Send + 'static>(f: impl Future<Output = Result<T>> + Send + 'static) -> impl Future<Output = Result<T>> {
    let h = RT.spawn(f);
    async move { h.await.map_err(|e| Error::internal(format!("task: {e}")))? }
}

pub fn is_not_running(e: &Error) -> bool {
    e.code == ErrorCode::Daemon && e.message == NOT_RUNNING
}

const NOT_RUNNING: &str = "守护进程未运行";

async fn observe(paths: &Paths, req: &Request) -> Result<Value> {
    let mut retries = 0;
    loop {
        match ipc::observe(paths, req).await {
            Ok(v) => return Ok(v),
            // A freshly started daemon may not have written its token yet.
            Err(CallError::Failed(e)) if e.message == "invalid daemon token" && retries < 20 => {
                retries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(CallError::Failed(e)) => return Err(e),
            Err(CallError::NotRunning) => return Err(Error::new(ErrorCode::Daemon, NOT_RUNNING)),
            Err(CallError::VersionMismatch) => {
                return Err(
                    Error::new(ErrorCode::Daemon, "守护进程的协议版本与本程序不同").hint("重启守护进程（会关闭其中的会话和端口转发）")
                );
            }
        }
    }
}

/// The CLI next to this executable (an install is one folder; dev builds share a target dir).
pub fn cli_sibling() -> Option<PathBuf> {
    let exe = xssh_core::paths::current_exe().ok()?;
    Some(exe.parent()?.join(format!("xssh{}", std::env::consts::EXE_SUFFIX))).filter(|p| p.exists())
}

/// The CLI to run: the one beside this app, else `xssh` on PATH.
fn cli_exe() -> PathBuf {
    cli_sibling().unwrap_or_else(|| PathBuf::from(format!("xssh{}", std::env::consts::EXE_SUFFIX)))
}

/// The CLI owns daemon startup (detached process, per-build executable copy, version tag), so the
/// app delegates to `xssh daemon start` instead of spawning the daemon itself.
async fn start_daemon(paths: &Paths) -> Result<()> {
    let mut cmd = tokio::process::Command::new(cli_exe());
    cmd.arg("--home").arg(&paths.home).args(["daemon", "start"]);
    cmd.stdin(std::process::Stdio::null());
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    let out = cmd
        .output()
        .await
        .map_err(|e| Error::io(format!("无法运行 {}: {e}", cli_exe().display())).hint("把 xssh 可执行文件放在本程序旁边或 PATH 中"))?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(Error::new(ErrorCode::Daemon, format!("启动守护进程失败: {msg}")));
    }
    Ok(())
}

async fn stop_daemon(paths: &Paths) -> Result<()> {
    match ipc::observe(paths, &Request::Shutdown).await {
        Ok(_) => {
            ipc::wait_stopped(paths, Duration::from_secs(5)).await;
            Ok(())
        }
        Err(CallError::NotRunning) => Ok(()),
        Err(CallError::Failed(e)) => Err(e),
        Err(CallError::VersionMismatch) => Err(Error::new(ErrorCode::Daemon, "守护进程拒绝了停止请求")),
    }
}
