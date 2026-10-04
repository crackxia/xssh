//! The long-lived daemon: owns the connection pool, interactive sessions and
//! port forwards; serves CLI requests over local IPC; exits when idle.

use crate::engine::{ClientInfo, Engine};
use crate::ssh::Ctx;
use interprocess::local_socket::traits::tokio::Listener as _;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use xssh_core::api::{Envelope, Request, Response, same_protocol, version_tag};
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::ipc;
use xssh_core::paths::{Paths, write_private};
use xssh_core::text;

pub fn log(paths: &Paths, msg: &str) {
    let f = paths.daemon_log();
    if std::fs::metadata(&f).map(|m| m.len() > 5 * 1024 * 1024).unwrap_or(false) {
        let _ = std::fs::rename(&f, f.with_extension("log.1"));
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&f) {
        let _ = writeln!(file, "{} [{}] {msg}", xssh_store::audit::now_rfc3339(), std::process::id());
    }
}

pub async fn run(paths: Paths) -> Result<()> {
    let listener = ipc::listen(&paths).await?;
    let token = text::short_id(32);
    write_private(&paths.daemon_token(), token.as_bytes())?;
    std::fs::write(paths.daemon_pid(), std::process::id().to_string())?;
    let ctx = Ctx::new(paths.clone())?;
    let idle_exit = Duration::from_secs(ctx.config.daemon_idle_exit_secs.max(60));
    let session_idle = Duration::from_secs(ctx.config.session_idle_close_secs.max(60));
    let conn_idle = Duration::from_secs(ctx.config.connection_idle_secs.max(30));
    let retention = ctx.config.outputs_retention_hours;
    let engine = Engine::new(ctx, true);
    log(
        &paths,
        &format!("daemon started, version {}, home {}", version_tag(), paths.home.display()),
    );

    let started = Instant::now();
    let last_request = Arc::new(AtomicU64::new(0));
    let in_flight = Arc::new(AtomicU64::new(0));

    // Housekeeping: GC idle connections/sessions, old outputs, idle exit.
    {
        let engine = engine.clone();
        let paths = paths.clone();
        let last_request = last_request.clone();
        let in_flight = in_flight.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            loop {
                tick.tick().await;
                engine.pool.gc(conn_idle).await;
                engine.sessions.gc(session_idle).await;
                text::gc_outputs(&paths, retention);
                let idle_for = started.elapsed().as_secs().saturating_sub(last_request.load(Ordering::Relaxed));
                if engine.sessions.count_open() == 0
                    && engine.forwards.count() == 0
                    && in_flight.load(Ordering::Relaxed) == 0
                    && idle_for > idle_exit.as_secs()
                {
                    log(&paths, &format!("idle for {idle_for}s, exiting"));
                    engine.shutdown.notify_waiters();
                    break;
                }
            }
        });
    }

    loop {
        let shutdown = engine.shutdown.notified();
        tokio::select! {
            _ = shutdown => break,
            conn = listener.accept() => {
                let Ok(stream) = conn else { continue };
                let engine = engine.clone();
                let token = token.clone();
                let paths = paths.clone();
                let last_request = last_request.clone();
                let in_flight = in_flight.clone();
                tokio::spawn(async move {
                    // Readiness probes connect and disconnect without sending anything. A wrong
                    // token is rejected before the rest of a (possibly huge) request is read.
                    let line = match ipc::read_line_checked(&stream, Some(&ipc::token_prefix(&token))).await {
                        Ok(l) if !l.is_empty() => l,
                        Ok(_) => return,
                        Err(e) => {
                            let line = serde_json::to_string(&Response::Err { error: e }).unwrap_or_default();
                            let _ = ipc::write_line(&stream, &line).await;
                            return;
                        }
                    };
                    let env = serde_json::from_str::<Envelope>(&line).map_err(|e| {
                        // A newer client may use a request this daemon (an older, busy build) lacks.
                        let theirs = serde_json::from_str::<serde_json::Value>(&line)
                            .ok()
                            .and_then(|v| v.get("version").and_then(|v| v.as_str()).map(String::from))
                            .filter(|v| *v != version_tag());
                        match theirs {
                            Some(v) => Error::new(
                                ErrorCode::Daemon,
                                format!("the running daemon ({}) is another build than this xssh ({v}) and does not know this request: {e}", version_tag()),
                            )
                            .hint(format!(
                                "`xssh daemon restart` loads this build but closes {} session(s) and {} forward(s) other agents may use",
                                engine.sessions.count_open(),
                                engine.forwards.count()
                            )),
                            None => Error::usage(format!("bad request: {e}")),
                        }
                    });
                    // Observers (the desktop app polling) do not keep an idle daemon alive.
                    let active = !env.as_ref().is_ok_and(|e| e.observer);
                    if active {
                        last_request.store(started.elapsed().as_secs(), Ordering::Relaxed);
                        in_flight.fetch_add(1, Ordering::Relaxed);
                    }
                    let busy = in_flight.load(Ordering::Relaxed) > 1;
                    let resp = match env {
                        Ok(env) => serve_one(&engine, env, &token, busy).await,
                        Err(e) => Err(e),
                    };
                    if active {
                        in_flight.fetch_sub(1, Ordering::Relaxed);
                        last_request.store(started.elapsed().as_secs(), Ordering::Relaxed);
                    }
                    let line = match resp {
                        Ok((v, daemon)) => serde_json::to_string(&Response::Ok { result: v, daemon }),
                        Err(e) => serde_json::to_string(&Response::Err { error: e }),
                    }
                    .unwrap_or_else(|e| format!("{{\"error\":{{\"code\":\"INTERNAL\",\"message\":\"serialize: {e}\"}}}}"));
                    if let Err(e) = ipc::write_line(&stream, &line).await {
                        log(&paths, &format!("write response failed: {e}"));
                    }
                });
            }
        }
    }
    // Give in-flight responses (e.g. the shutdown reply) a moment to flush.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let _ = std::fs::remove_file(paths.daemon_token());
    let _ = std::fs::remove_file(paths.daemon_pid());
    #[cfg(unix)]
    let _ = std::fs::remove_file(paths.socket_path());
    log(&paths, "daemon stopped");
    Ok(())
}

/// Serve one request. A client of another build is served by this daemon only while it is busy
/// (sessions, forwards or other requests in flight) and speaks the same protocol, so upgrading
/// xssh never silently kills other agents' sessions: an idle daemon answers "version mismatch"
/// and the client restarts it; a busy one of another protocol refuses with a hint.
async fn serve_one(engine: &Arc<Engine>, env: Envelope, token: &str, other_requests: bool) -> Result<(serde_json::Value, Option<String>)> {
    if !ipc::constant_time_eq(env.token.as_bytes(), token.as_bytes()) {
        return Err(Error::new(ErrorCode::Daemon, "invalid daemon token"));
    }
    let ours = version_tag();
    let mut served_other = None;
    if env.version != ours && !matches!(env.request, Request::Shutdown | Request::Ping) {
        let compatible = same_protocol(&env.version, &ours);
        let (sessions, forwards) = (engine.sessions.count_open(), engine.forwards.count());
        let busy = sessions > 0 || forwards > 0 || other_requests;
        if env.observer {
            if !compatible {
                return Err(Error::new(
                    ErrorCode::Daemon,
                    format!("version mismatch: daemon {ours} vs client {}", env.version),
                ));
            }
        } else if !busy {
            return Err(Error::new(
                ErrorCode::Daemon,
                format!("version mismatch: daemon {ours} vs client {}", env.version),
            ));
        } else if !compatible {
            return Err(Error::new(
                ErrorCode::Daemon,
                format!(
                    "the running daemon ({ours}) speaks another protocol than this xssh ({}) and is busy: {sessions} session(s), {forwards} forward(s)",
                    env.version
                ),
            )
            .hint(format!(
                "`xssh daemon restart` closes those {sessions} session(s) and {forwards} forward(s) (other agents may be using them); or keep using the xssh build that started the daemon"
            )));
        } else {
            served_other = Some(ours.clone());
        }
    }
    let client = ClientInfo {
        pid: Some(env.client_pid),
        cwd: env.client_cwd,
    };
    engine.handle(env.request, client).await.map(|v| (v, served_other))
}
