//! Request dispatcher shared by the daemon and the in-process (`--no-daemon`) mode.

use crate::exec::{self, ExecParams, ExecResult};
use crate::files::{self, SudoCtx};
use crate::forward::{ForwardManager, Kind, Spec};
use crate::jobs;
use crate::probe;
use crate::session::{self, DEFAULT_READER, OpenCtx, Session, SessionManager, SessionSecrets};
use crate::ssh::pool::Pool;
use crate::ssh::{Conn, Ctx, HostKeyEvent};
use crate::transfer;
use base64::Engine as _;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use xssh_core::api::Request;
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::text::{self, shq, shq_path};
use xssh_store::audit::{self, AuditRecord};
use xssh_store::hosts::{Facts, Host, HostStore};
use xssh_store::jobs as job_store;
use xssh_store::secrets;
use zeroize::Zeroizing;

pub struct Engine {
    pub ctx: Arc<Ctx>,
    pub pool: Arc<Pool>,
    pub sessions: SessionManager,
    pub forwards: ForwardManager,
    pub started: Instant,
    pub shutdown: tokio::sync::Notify,
    pub is_daemon: bool,
    /// Serializes reattaching persistent sessions (one control client per session).
    reattach_lock: tokio::sync::Mutex<()>,
}

#[derive(Debug, Clone, Default)]
pub struct ClientInfo {
    pub pid: Option<u32>,
    pub cwd: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostTestResult {
    pub host: String,
    pub ok: bool,
    pub auth: String,
    pub connect_ms: u64,
    pub facts: Facts,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner: Option<String>,
}

fn to_value<T: Serialize>(v: T) -> Result<Value> {
    Ok(serde_json::to_value(v)?)
}

fn ms(v: u64) -> Duration {
    Duration::from_millis(v)
}

impl Engine {
    pub fn new(ctx: Ctx, is_daemon: bool) -> Arc<Engine> {
        let ctx = Arc::new(ctx);
        Arc::new(Engine {
            pool: Arc::new(Pool::new(ctx.clone())),
            ctx,
            sessions: SessionManager::default(),
            forwards: ForwardManager::default(),
            started: Instant::now(),
            shutdown: tokio::sync::Notify::new(),
            is_daemon,
            reattach_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn hosts(&self) -> HostStore {
        HostStore::new(&self.ctx.paths)
    }

    /// Sudo password for a host: dedicated sudo secret, else the login password.
    fn sudo_password(&self, alias: &str) -> Result<Option<Zeroizing<String>>> {
        if let Some(p) = self.ctx.secrets.get(&secrets::host_sudo(alias))? {
            return Ok(Some(p));
        }
        self.ctx.secrets.get(&secrets::host_password(alias))
    }

    fn user_secret(&self, name: &str) -> Result<Zeroizing<String>> {
        self.ctx.secrets.get(&secrets::user_secret(name))?.ok_or_else(|| {
            Error::not_found(format!("secret '{name}' is not stored")).hint(format!(
                "ask the user to run `xssh secret set {name}` (the value is never shown to you)"
            ))
        })
    }

    pub async fn handle(&self, req: Request, client: ClientInfo) -> Result<Value> {
        let start = Instant::now();
        let mut rec = AuditRecord::new(req.name());
        rec.client_pid = client.pid;
        rec.client_cwd = client.cwd.clone();
        audit_fill(&mut rec, &req);
        // Session input: withheld at password prompts, pastes summarized, secrets masked.
        match &req {
            Request::SessionSend {
                id,
                text,
                keys,
                enter,
                paste,
                ..
            } => {
                if let Ok(s) = self.sessions.get(id) {
                    rec.command = Some(clip(&s.input_summary(text.as_deref(), *paste, keys.as_deref(), *enter)));
                }
            }
            Request::SessionRun { id, .. } => {
                if let (Ok(s), Some(c)) = (self.sessions.get(id), rec.command.as_ref()) {
                    rec.command = Some(s.redact(c));
                }
            }
            _ => {}
        }
        let quiet = matches!(
            req,
            Request::Ping
                | Request::DaemonStatus
                | Request::SessionList
                | Request::ForwardList
                | Request::SessionScreen { .. }
                | Request::SessionPeek { .. }
        );
        let res = self.dispatch(req).await;
        if !quiet {
            rec.duration_ms = Some(start.elapsed().as_millis() as u64);
            match &res {
                Ok(v) => {
                    if let Some(c) = v.get("exit_code").and_then(Value::as_i64) {
                        rec.exit_code = Some(c);
                    } else if let Some(arr) = v.as_array()
                        && arr.len() == 1
                    {
                        rec.exit_code = arr[0].get("exit_code").and_then(Value::as_i64);
                    }
                }
                Err(e) => {
                    rec.ok = false;
                    rec.error = Some(format!("{:?}: {}", e.code, e.message));
                }
            }
            audit::append(&self.ctx.paths, &rec);
        }
        res
    }

    async fn dispatch(&self, req: Request) -> Result<Value> {
        let cfg = &self.ctx.config;
        match req {
            Request::Ping => Ok(
                json!({"pong": true, "version": xssh_core::api::version_tag(), "pid": std::process::id(),
                                       "sessions": self.sessions.count_open(), "forwards": self.forwards.count()}),
            ),
            Request::Shutdown => {
                // Persistent sessions only detach: their shells keep running in tmux.
                for s in self.sessions.all() {
                    s.detach_or_close().await;
                }
                self.shutdown.notify_waiters();
                Ok(json!({"stopping": true}))
            }
            Request::DaemonStatus => Ok(json!({
                "version": xssh_core::api::version_tag(),
                "pid": std::process::id(),
                "uptime_secs": self.started.elapsed().as_secs(),
                "home": self.ctx.paths.home.display().to_string(),
                "secret_backend": format!("{:?}", self.ctx.secrets.backend()).to_lowercase(),
                "connections": self.pool.list().await,
                "sessions": self.sessions.list(),
                "forwards": self.forwards.list(),
            })),
            Request::HostTest { host } => to_value(self.host_test(&host).await?),
            Request::HostTrust { host, fingerprint } => self.host_trust(&host, fingerprint.as_deref()).await,
            Request::HostDisconnect { host } => {
                self.pool.drop_conn(&host).await;
                Ok(json!({"host": host, "disconnected": true}))
            }
            Request::KeyDeploy { host, key } => self.key_deploy(&host, &key).await,

            Request::Exec(p) => self.exec(p).await,

            Request::SessionOpen(p) => {
                // Before opening anything remote: a taken name must not leave a shell behind.
                self.sessions.check_name(p.name.as_deref())?;
                if let Some(n) = &p.name
                    && session::tmux::find(&self.ctx.paths, n).is_some()
                {
                    return Err(Error::new(
                        ErrorCode::AlreadyExists,
                        format!("a persistent session named '{n}' exists (detached)"),
                    )
                    .hint(format!(
                        "any `xssh session` command on '{n}' reattaches it; `xssh session close {n}` ends it"
                    )));
                }
                let host = self.hosts().get(&p.host)?;
                let conn = self.pool.get(&host.alias).await?;
                let secrets = self.session_secrets(&host, &p)?;
                let mut p = p;
                if p.encoding.is_none() {
                    p.encoding = host.encoding.clone();
                }
                if p.busy_patterns.is_empty() {
                    p.busy_patterns = cfg.busy_patterns.clone();
                }
                let reader = p.agent.clone().unwrap_or_else(|| DEFAULT_READER.into());
                let octx = OpenCtx {
                    paths: &self.ctx.paths,
                    max_output: cfg.max_output_bytes,
                    default_size: (cfg.session_cols, cfg.session_rows),
                    owner: &reader,
                };
                let s = Session::open(conn, &p, secrets, &octx).await?;
                if let Err(e) = self.sessions.insert(s.clone()) {
                    s.close().await;
                    return Err(e);
                }
                if let Some(rec) = s.persist_rec() {
                    session::tmux::upsert(&self.ctx.paths, rec)?;
                }
                // A non-UTF-8 login locale (e.g. POSIX in containers) makes ls/readline escape
                // non-ASCII text; switch the shell to an installed UTF-8 locale.
                let locale = host.facts.as_ref().and_then(|f| f.locale_fix()).filter(|_| host.encoding.is_none());
                if let Some(loc) = locale {
                    s.inject(&format!(" export LANG={loc} LC_ALL={loc}\n")).await?;
                }
                let mut obs = s.read(&reader, ms(700), ms(10_000), true, &Default::default()).await?;
                if let Some(loc) = locale {
                    obs.events.push(format!("login locale is not UTF-8; session switched to {loc}"));
                }
                // The login banner/MOTD is noise for an agent; the prompt line stays in the footer.
                // Shell startup errors (a broken ~/.bashrc) are kept: they affect every command.
                static STARTUP_ERR: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
                    regex::Regex::new(r"(?i)^-?\w*sh: |no such file or directory|command not found|permission denied|\berror\b").unwrap()
                });
                let text = std::mem::take(&mut obs.output);
                let lines: Vec<&str> = text
                    .lines()
                    .filter(|l| !l.trim().is_empty() && !l.contains(session::marker::HOOK_FN))
                    .collect();
                let errors: Vec<&str> = lines.iter().copied().filter(|l| STARTUP_ERR.is_match(l)).collect();
                let banner = lines.len() - errors.len();
                obs.output = errors.iter().map(|l| format!("{l}\n")).collect();
                if !errors.is_empty() {
                    obs.events
                        .push("the login shell printed the errors above while starting (e.g. ~/.bashrc / ~/.profile)".into());
                }
                if banner > 1 {
                    obs.events
                        .push(format!("login banner hidden ({banner} lines; `session log` shows it)"));
                }
                let mut v = to_value(&obs)?;
                v["session"] = json!(s.key());
                v["id"] = json!(s.id);
                if let Some(n) = &s.name {
                    v["name"] = json!(n);
                }
                if s.persist_rec().is_some() {
                    v["persistent"] = json!(true);
                }
                Ok(v)
            }
            Request::SessionRun {
                id,
                command,
                timeout_ms,
                agent,
            } => {
                let s = self.session(&id).await?;
                to_value(s.run(reader(&agent), &command, ms(timeout_ms)).await?)
            }
            Request::SessionSend {
                id,
                text,
                keys,
                enter,
                wait_ms,
                timeout_ms,
                if_prompt,
                paste,
                wait,
                agent,
            } => {
                let s = self.session(&id).await?;
                let last = s.input_summary(text.as_deref(), paste, keys.as_deref(), enter);
                s.set_last(&last);
                s.note_input(&last);
                // Text, keys and Enter go out as separate writes (see `Session::send`).
                let mut chunks: Vec<Vec<u8>> = vec![];
                if let Some(t) = text {
                    chunks.push(if paste { s.paste_bytes(&t) } else { s.encode_text(&t) });
                }
                if let Some(k) = keys {
                    chunks.extend(session::keys::encode_each(&k, s.app_cursor())?);
                }
                if enter {
                    chunks.push(b"\r".to_vec());
                }
                if chunks.iter().all(Vec::is_empty) {
                    return Err(Error::usage("nothing to send: give TEXT, --keys or --enter"));
                }
                to_value(
                    s.send(reader(&agent), &chunks, ms(wait_ms), ms(timeout_ms), if_prompt.as_deref(), &wait)
                        .await?,
                )
            }
            Request::SessionRead {
                id,
                wait_ms,
                timeout_ms,
                wait,
                agent,
            } => {
                let s = self.session(&id).await?;
                to_value(s.read(reader(&agent), ms(wait_ms), ms(timeout_ms), true, &wait).await?)
            }
            Request::SessionExpect {
                id,
                pattern,
                timeout_ms,
                agent,
            } => {
                let s = self.session(&id).await?;
                let (obs, groups) = s.expect(reader(&agent), &pattern, ms(timeout_ms)).await?;
                let mut v = to_value(&obs)?;
                v["matched"] = json!(groups.is_some());
                if let Some(g) = groups {
                    v["groups"] = json!(g);
                }
                Ok(v)
            }
            Request::SessionScreen { id } => to_value(self.session(&id).await?.screen()),
            Request::SessionPeek { id } => to_value(self.sessions.get(&id)?.snapshot()),
            Request::SessionResize { id, cols, rows } => {
                let s = self.session(&id).await?;
                s.resize(cols, rows).await?;
                to_value(s.screen())
            }
            Request::SessionClose { id } => {
                let s = match self.sessions.get(&id) {
                    Ok(s) => s,
                    Err(e) => {
                        // A detached persistent session: end its tmux session without attaching.
                        let Some(rec) = session::tmux::find(&self.ctx.paths, &id) else {
                            return Err(e);
                        };
                        let conn = self.pool.get(&rec.host).await?;
                        session::kill_persistent(&conn, &rec, &self.ctx.paths).await?;
                        return Ok(json!({"session": rec.id, "closed": true, "transcript": rec.transcript}));
                    }
                };
                if s.is_disconnected()
                    && let Some(rec) = s.persist_rec()
                {
                    let conn = self.pool.get(&rec.host).await?;
                    session::kill_persistent(&conn, rec, &self.ctx.paths).await?;
                }
                s.close().await;
                self.sessions.remove(&s.id);
                Ok(json!({"session": s.id, "closed": true, "transcript": s.transcript_path}))
            }
            Request::SessionList => {
                let mut v = self.sessions.list();
                // Persistent sessions from before a daemon restart: listed as detached.
                for r in session::tmux::load(&self.ctx.paths) {
                    if !v.iter().any(|i| i.id == r.id) {
                        v.push(r.detached_info());
                    }
                }
                to_value(v)
            }

            Request::JobStart(p) => {
                let host = self.hosts().get(&p.host)?;
                let conn = self.pool.get(&host.alias).await?;
                let (sudo, pw) = self.sudo_for(&host, p.sudo)?;
                let mut values = vec![];
                for (_, name) in &p.env_secrets {
                    values.push(self.user_secret(name)?.to_string());
                }
                let p = jobs::JobStartParams { sudo, ..p };
                let st = jobs::start(&conn, &self.ctx.paths, &p, &values, pw.as_ref().map(|x| x.as_str())).await;
                job_store::gc(&self.ctx.paths, cfg.job_retention_days);
                to_value(st?)
            }
            Request::JobStatus { id } => {
                let (mut rec, conn) = self.job(&id).await?;
                let st = jobs::status(&conn, &rec, 20).await?;
                jobs::remember(&self.ctx.paths, &mut rec, &st);
                to_value(st)
            }
            Request::JobLogs {
                id,
                offset,
                tail,
                max_bytes,
            } => {
                let (rec, conn) = self.job(&id).await?;
                let enc = self.hosts().get(&rec.host).ok().and_then(|h| h.encoding);
                to_value(jobs::logs(&conn, &rec, offset, tail, max_bytes, enc.as_deref()).await?)
            }
            Request::JobWait { id, timeout_ms, tail } => {
                let (mut rec, conn) = self.job(&id).await?;
                let st = jobs::wait(&conn, &rec, ms(timeout_ms), tail).await?;
                jobs::remember(&self.ctx.paths, &mut rec, &st);
                to_value(st)
            }
            Request::JobKill { id, signal } => {
                let (mut rec, conn) = self.job(&id).await?;
                let pw = if rec.sudo { self.sudo_password(&rec.host)? } else { None };
                let st = jobs::kill(&conn, &rec, &signal, pw.as_ref().map(|x| x.as_str())).await?;
                jobs::remember(&self.ctx.paths, &mut rec, &st);
                to_value(st)
            }
            Request::JobRm { id } => {
                let (rec, conn) = self.job(&id).await?;
                let pw = if rec.sudo { self.sudo_password(&rec.host)? } else { None };
                let killed = jobs::remove(&conn, &self.ctx.paths, &rec, pw.as_ref().map(|x| x.as_str())).await?;
                Ok(json!({"id": id, "removed": true, "killed": killed}))
            }
            Request::JobList { host, all } => {
                job_store::gc(&self.ctx.paths, cfg.job_retention_days);
                // Finished jobs older than a week are hidden unless --all.
                let mut recs: Vec<job_store::JobRecord> = job_store::list(&self.ctx.paths)?
                    .into_iter()
                    .filter(|r| host.as_ref().is_none_or(|h| &r.host == h))
                    .filter(|r| all || !r.finished() || r.age_secs() < 7 * 86400)
                    .collect();
                let fresh = self.refresh_jobs(&recs).await;
                for r in recs.iter_mut() {
                    if let Some(Ok(st)) = fresh.get(&r.id) {
                        jobs::remember(&self.ctx.paths, r, st);
                    }
                }
                let rows: Vec<Value> = recs
                    .iter()
                    .map(|rec| {
                        let err = match fresh.get(&rec.id) {
                            Some(Err(e)) => Some(e.clone()),
                            _ => None,
                        };
                        json!({
                            "id": rec.id,
                            "host": rec.host,
                            "name": rec.name,
                            "command": rec.command,
                            "started_at": rec.started_at,
                            "state": if err.is_some() { "unknown".to_string() } else { rec.last_state.clone().unwrap_or_else(|| "unknown".into()) },
                            "exit_code": rec.exit_code,
                            "error": err,
                        })
                    })
                    .collect();
                Ok(Value::Array(rows))
            }
            Request::JobPrune { host, older_than_secs } => {
                let recs: Vec<job_store::JobRecord> = job_store::list(&self.ctx.paths)?
                    .into_iter()
                    .filter(|r| host.as_ref().is_none_or(|h| &r.host == h))
                    .filter(|r| r.age_secs() >= older_than_secs)
                    .collect();
                let fresh = self.refresh_jobs(&recs).await;
                let mut removed = vec![];
                let mut kept_running = 0usize;
                for mut r in recs {
                    match fresh.get(&r.id) {
                        Some(Ok(st)) => jobs::remember(&self.ctx.paths, &mut r, st),
                        Some(Err(_)) => continue,
                        None => {}
                    }
                    if !r.finished() {
                        kept_running += 1;
                        continue;
                    }
                    if let Ok(conn) = self.pool.get(&r.host).await {
                        jobs::remove_files(&conn, &r).await;
                    }
                    job_store::remove(&self.ctx.paths, &r.id)?;
                    removed.push(r.id);
                }
                Ok(json!({"removed": removed, "running_kept": kept_running}))
            }

            Request::FileRead {
                host,
                path,
                offset,
                limit,
                sudo,
                encoding,
            } => {
                let (h, conn) = self.host_conn(&host).await?;
                let (sudo, pw) = self.sudo_for(&h, sudo)?;
                let sc = SudoCtx {
                    enabled: sudo,
                    password: pw.as_ref().map(|x| x.as_str()),
                };
                let enc = encoding.or(h.encoding.clone());
                to_value(files::read(&conn, &path, offset, limit, enc.as_deref(), &sc).await?)
            }
            Request::FileWrite {
                host,
                path,
                content_b64,
                mode,
                backup,
                mkdirs,
                sudo,
                expect_sha256,
                secrets,
            } => {
                let mut data = base64::engine::general_purpose::STANDARD
                    .decode(content_b64)
                    .map_err(|e| Error::usage(format!("bad base64 content: {e}")))?;
                if secrets {
                    let text = String::from_utf8(data).map_err(|_| Error::usage("--secrets needs UTF-8 content"))?;
                    data = self.expand_placeholders(&host, &text, &mut vec![])?.as_bytes().to_vec();
                }
                let (h, conn) = self.host_conn(&host).await?;
                let (sudo, pw) = self.sudo_for(&h, sudo)?;
                let sc = SudoCtx {
                    enabled: sudo,
                    password: pw.as_ref().map(|x| x.as_str()),
                };
                let opts = files::WriteOpts {
                    mode,
                    backup,
                    mkdirs,
                    expect_sha256,
                    backup_keep: cfg.backup_keep,
                    backup_days: cfg.backup_retention_days,
                };
                to_value(files::write(&conn, &path, &data, &opts, &sc).await?)
            }
            Request::FileEdit {
                host,
                path,
                old,
                new,
                replace_all,
                backup,
                sudo,
                secrets,
            } => {
                let mut redact = vec![];
                let (old, new) = if secrets {
                    (
                        self.expand_placeholders(&host, &old, &mut redact)?.to_string(),
                        self.expand_placeholders(&host, &new, &mut redact)?.to_string(),
                    )
                } else {
                    (old, new)
                };
                let (h, conn) = self.host_conn(&host).await?;
                let (sudo, pw) = self.sudo_for(&h, sudo)?;
                let sc = SudoCtx {
                    enabled: sudo,
                    password: pw.as_ref().map(|x| x.as_str()),
                };
                let opts = files::WriteOpts {
                    backup,
                    backup_keep: cfg.backup_keep,
                    backup_days: cfg.backup_retention_days,
                    ..Default::default()
                };
                let hide = |mut s: String| {
                    for v in redact.iter().filter(|v| !v.is_empty()) {
                        s = s.replace(v.as_str(), "***");
                    }
                    s
                };
                let mut r = files::edit(&conn, &path, &old, &new, replace_all, &opts, h.encoding.as_deref(), &sc)
                    .await
                    .map_err(|e| Error {
                        message: hide(e.message),
                        hint: e.hint.map(hide),
                        ..e
                    })?;
                r.snippet = hide(r.snippet);
                to_value(r)
            }
            Request::FileLs { host, path } => {
                let (_, conn) = self.host_conn(&host).await?;
                to_value(files::ls(&conn, &path).await?)
            }
            Request::FileStat { host, path } => {
                let (_, conn) = self.host_conn(&host).await?;
                to_value(files::stat(&conn, &path).await?)
            }
            Request::Copy {
                srcs,
                dst,
                sudo,
                mode,
                opts,
            } => {
                let mode = match mode {
                    Some(m) => Some(
                        u32::from_str_radix(&m, 8)
                            .ok()
                            .filter(|x| *x <= 0o7777)
                            .ok_or_else(|| Error::usage(format!("invalid mode '{m}' (octal like 644)")))?,
                    ),
                    None => None,
                };
                let mut hosts = std::collections::HashMap::new();
                for e in srcs.iter().chain(std::iter::once(&dst)) {
                    if let Some(a) = &e.host
                        && !hosts.contains_key(a)
                    {
                        let (h, conn) = self.host_conn(a).await.map_err(|e| tag_host(e, a))?;
                        let (sudo, password) = self.sudo_for(&h, sudo)?;
                        hosts.insert(a.clone(), transfer::HostIo { conn, sudo, password });
                    }
                }
                to_value(transfer::copy(&hosts, &srcs, &dst, mode, &opts).await?)
            }

            Request::Status { host, only, top, sudo } => self.status(&host, &only, top, sudo).await,
            Request::Perf {
                host,
                duration_ms,
                interval_ms,
                series,
            } => {
                let (h, conn) = self.host_conn(&host).await?;
                to_value(probe::perf::perf(&conn, &h.alias, ms(duration_ms), ms(interval_ms), series).await?)
            }
            Request::Diag { host, sudo } => self.diag(&host, sudo).await,
            Request::Logs {
                host,
                unit,
                file,
                since,
                grep,
                priority,
                tail,
                sudo,
            } => {
                let (h, conn) = self.host_conn(&host).await?;
                let os = probe::detect_os(&conn).await?;
                if file.is_some() && (since.is_some() || priority.is_some()) {
                    return Err(
                        Error::usage("--since/--priority filter the system journal; they do not apply to --file")
                            .hint("with --file use --grep and --tail (or `xssh exec HOST -- awk ...` on the timestamps)"),
                    );
                }
                // Without journald the fallback file has no priorities and is not time-filtered.
                let ignored = (file.is_none() && !matches!(os.as_str(), "linux" | "darwin"))
                    .then(|| match (&since, &priority) {
                        (None, None) => None,
                        _ => Some(format!(
                            "--since/--priority are not supported on {os} (/var/log/messages is read as is)"
                        )),
                    })
                    .flatten();
                // A grep over the whole journal is slow: default to the last 24h and say so.
                let default_since = (grep.is_some() && since.is_none() && file.is_none()).then_some("24h");
                let since = since.as_deref().or(default_since);
                let cmd = logs_command(
                    &os,
                    unit.as_deref(),
                    file.as_deref(),
                    since,
                    grep.as_deref(),
                    priority.as_deref(),
                    tail,
                )?;
                let params = ExecParams {
                    host: h.alias.clone(),
                    command: cmd,
                    sudo,
                    timeout_ms: Some(60_000),
                    ..Default::default()
                };
                let mut r = self.exec_one(&h, &params, b"", &[], cfg.max_output_bytes, ms(60_000)).await?;
                if let Some(i) = ignored {
                    r.hint = Some(i);
                } else if r.hint.is_none() {
                    let lines = r.stdout.text.lines().count();
                    r.hint = if lines == 0 {
                        Some(match (&unit, &file) {
                            (Some(u), _) => format!(
                                "no entries{} for unit '{u}'; check the name with `xssh exec {host} -- systemctl list-units --all '*{u}*'`",
                                since.map(|s| format!(" in the last {s}")).unwrap_or_default()
                            ),
                            _ => format!(
                                "no matching lines{}",
                                since.map(|s| format!(" in the last {s}")).unwrap_or_default()
                            ),
                        })
                    } else {
                        default_since
                            .map(|s| format!("{lines} lines from the last {s} (default window with --grep; widen with --since 7d)"))
                    };
                }
                to_value(r)
            }

            Request::ForwardAdd {
                host,
                local,
                remote,
                dynamic,
            } => {
                if !self.is_daemon {
                    return Err(Error::usage("port forwarding requires the daemon (drop --no-daemon)"));
                }
                self.hosts().get(&host)?;
                let spec = match (local, remote, dynamic) {
                    (Some(l), None, None) => Spec::parse(Kind::Local, &l)?,
                    (None, Some(r), None) => Spec::parse(Kind::Remote, &r)?,
                    (None, None, Some(d)) => Spec::parse(Kind::Dynamic, &d)?,
                    _ => return Err(Error::usage("give exactly one of -L, -R or -D")),
                };
                to_value(self.forwards.add(self.pool.clone(), &host, spec).await?)
            }
            Request::ForwardList => to_value(self.forwards.list()),
            Request::ForwardStop { id } => to_value(self.forwards.stop(&id).await?),
        }
    }

    async fn host_conn(&self, alias: &str) -> Result<(Host, Arc<Conn>)> {
        let h = self.hosts().get(alias)?;
        let c = self.pool.get(alias).await?;
        Ok((h, c))
    }

    fn sudo_for(&self, h: &Host, sudo: bool) -> Result<(bool, Option<Zeroizing<String>>)> {
        if !sudo || h.user == "root" {
            return Ok((false, None));
        }
        Ok((true, self.sudo_password(&h.alias)?))
    }

    async fn job(&self, id: &str) -> Result<(job_store::JobRecord, Arc<Conn>)> {
        let rec = job_store::load(&self.ctx.paths, id)?;
        let conn = self.pool.get(&rec.host).await?;
        Ok((rec, conn))
    }

    /// Current status of the unfinished jobs among `recs`: one exec per host, hosts in
    /// parallel, 20s per host (an unreachable host marks its jobs with the error).
    async fn refresh_jobs(&self, recs: &[job_store::JobRecord]) -> std::collections::HashMap<String, Result<jobs::JobStatus>> {
        let mut by_host: std::collections::HashMap<&str, Vec<&job_store::JobRecord>> = std::collections::HashMap::new();
        for r in recs.iter().filter(|r| !r.finished()) {
            by_host.entry(r.host.as_str()).or_default().push(r);
        }
        let results = futures::future::join_all(by_host.into_iter().map(|(host, list)| async move {
            let r = tokio::time::timeout(Duration::from_secs(20), async {
                let conn = self.pool.get(host).await?;
                jobs::status_many(&conn, &list).await
            })
            .await
            .unwrap_or_else(|_| Err(Error::timeout(format!("{host} did not answer within 20s"))));
            (list, r)
        }))
        .await;
        let mut out = std::collections::HashMap::new();
        for (list, r) in results {
            match r {
                Ok(mut map) => {
                    for rec in list {
                        if let Some(st) = map.remove(&rec.id) {
                            out.insert(rec.id.clone(), Ok(st));
                        }
                    }
                }
                Err(e) => {
                    for rec in list {
                        out.insert(rec.id.clone(), Err(e.clone()));
                    }
                }
            }
        }
        out
    }

    /// A session by id or name. A persistent session whose connection dropped, or one left
    /// from before a daemon restart, is reattached first.
    async fn session(&self, key: &str) -> Result<Arc<Session>> {
        let needs = |s: &Arc<Session>| s.is_disconnected() && s.persist_rec().is_some();
        match self.sessions.get(key) {
            Ok(s) if !needs(&s) => return Ok(s),
            Ok(_) => {}
            Err(e) if session::tmux::find(&self.ctx.paths, key).is_none() => return Err(e),
            Err(_) => {}
        }
        let _g = self.reattach_lock.lock().await;
        // Another caller may have reattached it meanwhile.
        let rec = match self.sessions.get(key) {
            Ok(s) if !needs(&s) => return Ok(s),
            Ok(s) => s.persist_rec().cloned().expect("persistent"),
            Err(e) => session::tmux::find(&self.ctx.paths, key).ok_or(e)?,
        };
        let host = self.hosts().get(&rec.host)?;
        let conn = self.pool.get(&host.alias).await?;
        let p = session::OpenParams {
            host: rec.host.clone(),
            no_autofill: rec.no_autofill,
            ..Default::default()
        };
        let secrets = self.session_secrets(&host, &p)?;
        let cfg = &self.ctx.config;
        let octx = OpenCtx {
            paths: &self.ctx.paths,
            max_output: cfg.max_output_bytes,
            default_size: (cfg.session_cols, cfg.session_rows),
            owner: &rec.owner,
        };
        let s = Session::reattach(conn, &rec, secrets, &octx, cfg.busy_patterns.clone()).await?;
        self.sessions.replace(s.clone());
        Ok(s)
    }

    fn session_secrets(&self, host: &Host, p: &session::OpenParams) -> Result<SessionSecrets> {
        let sudo = if p.no_autofill || host.user == "root" {
            None
        } else {
            self.sudo_password(&host.alias)?
        };
        let mut redact: Vec<String> = vec![];
        if let Some(s) = &sudo {
            redact.push(s.to_string());
        }
        if let Some(pw) = self.ctx.secrets.get(&secrets::host_password(&host.alias))? {
            redact.push(pw.to_string());
        }
        let mut responders = vec![];
        for rule in self.ctx.config.responders.iter().chain(p.responders.iter()) {
            let reply = self.expand_placeholders(&host.alias, &rule.reply, &mut redact)?;
            responders.push((rule.clone(), reply));
        }
        Ok(SessionSecrets {
            sudo_password: sudo,
            responders,
            redact,
        })
    }

    /// Expand `{password}`, `{sudo_password}` and `{secret:NAME}` in a responder reply.
    fn expand_placeholders(&self, alias: &str, reply: &str, redact: &mut Vec<String>) -> Result<Zeroizing<String>> {
        let mut out = Zeroizing::new(String::new());
        let mut rest = reply;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let Some(j) = rest[i..].find('}') else {
                out.push_str(&rest[i..]);
                rest = "";
                break;
            };
            let key = &rest[i + 1..i + j];
            let val = match key {
                "password" => self.ctx.secrets.get(&secrets::host_password(alias))?.ok_or_else(|| {
                    Error::secret(format!("no password stored for '{alias}'"))
                        .hint(format!("ask the user to run `xssh host set-password {alias}`"))
                })?,
                "sudo_password" => self.sudo_password(alias)?.ok_or_else(|| {
                    Error::secret(format!("no sudo password stored for '{alias}'"))
                        .hint(format!("ask the user to run `xssh host set-password {alias} --sudo`"))
                })?,
                k if k.starts_with("secret:") => self.user_secret(&k[7..])?,
                _ => {
                    out.push_str(&rest[i..i + j + 1]);
                    rest = &rest[i + j + 1..];
                    continue;
                }
            };
            redact.push(val.to_string());
            out.push_str(&val);
            rest = &rest[i + j + 1..];
        }
        out.push_str(rest);
        Ok(out)
    }

    async fn exec(&self, p: ExecParams) -> Result<Value> {
        let cfg = &self.ctx.config;
        let hosts = self.hosts().select(&p.host)?;
        let max = p.max_output.unwrap_or(cfg.max_output_bytes);
        let timeout = ms(p.timeout_ms.unwrap_or(cfg.exec_timeout_secs * 1000));
        let stdin = match &p.stdin_b64 {
            Some(b) => base64::engine::general_purpose::STANDARD
                .decode(b)
                .map_err(|e| Error::usage(format!("bad stdin: {e}")))?,
            None => vec![],
        };
        let mut env_secret_values = vec![];
        for (_, name) in &p.env_secrets {
            env_secret_values.push(self.user_secret(name)?.to_string());
        }
        let single = hosts.len() == 1;
        let par = p.parallel.unwrap_or(8).max(1);
        let results: Vec<(usize, String, Result<ExecResult>)> = futures::stream::iter(hosts.into_iter().enumerate())
            .map(|(i, h)| {
                let (p, stdin, env) = (&p, &stdin, &env_secret_values);
                async move { (i, h.alias.clone(), self.exec_one(&h, p, stdin, env, max, timeout).await) }
            })
            .buffer_unordered(par)
            .collect()
            .await;
        let mut results = results;
        results.sort_by_key(|(i, _, _)| *i);
        if single {
            let (_, _, r) = results.pop().unwrap();
            return to_value(vec![r?]);
        }
        let out: Vec<ExecResult> = results
            .into_iter()
            .map(|(_, alias, r)| match r {
                Ok(r) => r,
                Err(e) => ExecResult {
                    host: alias,
                    error: Some(e),
                    ..Default::default()
                },
            })
            .collect();
        to_value(out)
    }

    async fn exec_one(
        &self,
        h: &Host,
        p: &ExecParams,
        stdin: &[u8],
        env_secret_values: &[String],
        max: usize,
        timeout: Duration,
    ) -> Result<ExecResult> {
        let (sudo, pw) = self.sudo_for(h, p.sudo)?;
        let prepared = exec::prepare(
            &p.command,
            p.cwd.as_deref(),
            &p.env,
            &p.env_secrets,
            sudo.then_some(pw.as_ref().map(|x| x.as_str())),
            stdin,
            env_secret_values,
        );
        let sudo_arg = sudo.then_some(pw.as_ref().map(|x| x.as_str()));
        let run = |conn: Arc<Conn>| {
            let prepared = &prepared;
            async move { exec::run_raw_opts(&conn, &prepared.command, &prepared.stdin, p.pty, timeout, sudo_arg).await }
        };
        let mut conn = self.pool.get(&h.alias).await.map_err(|e| tag_host(e, &h.alias))?;
        let raw = match run(conn.clone()).await {
            // Connect errors happen before the command was sent (a stale pooled connection):
            // reconnect once. Later failures are never retried (the command may have run).
            Err(e) if e.code == ErrorCode::Connect => {
                self.pool.drop_conn(&h.alias).await;
                conn = self.pool.get(&h.alias).await.map_err(|e| tag_host(e, &h.alias))?;
                run(conn.clone()).await.map_err(|e| tag_host(e, &h.alias))?
            }
            r => r.map_err(|e| tag_host(e, &h.alias))?,
        };
        let enc = p.encoding.clone().or(h.encoding.clone());
        // The login password can show up in output too (e.g. a script echoing its input).
        let mut redact = prepared.redact.clone();
        if !h.ad_hoc
            && let Some(lp) = self.ctx.secrets.get(&secrets::host_password(&h.alias))?
        {
            redact.push(lp.to_string());
        }
        let mut r = exec::finish(&h.alias, raw, &redact, enc.as_deref(), max, &self.ctx.paths, timeout);
        r.host_key = conn.take_hostkey_note();
        Ok(r)
    }

    /// Without `confirm`: report the key the server presents and what is recorded (nothing is
    /// changed). With `confirm` (the fingerprint the user verified): if the server still presents
    /// exactly that key, replace xssh's record for the host with it (xssh's known_hosts wins over
    /// ~/.ssh/known_hosts, so this also resolves a stale entry there), under any policy.
    async fn host_trust(&self, alias: &str, confirm: Option<&str>) -> Result<Value> {
        let h = self.hosts().get(alias)?;
        let jump = self.pool.jump_for(&h).await?;
        let key = crate::ssh::probe_host_key(&self.ctx, &h, jump).await?;
        let fp = crate::ssh::known_hosts::fingerprint(&key);
        let kh = crate::ssh::known_hosts_for(&self.ctx, &h);
        let name = crate::ssh::key_name(&h);
        let status = match kh.verify(name, h.port, &key) {
            crate::ssh::known_hosts::Verdict::Known => "known".to_string(),
            crate::ssh::known_hosts::Verdict::Unknown => "unknown".to_string(),
            crate::ssh::known_hosts::Verdict::Changed { file, line } => {
                format!("changed (recorded key differs: {}:{line})", file.display())
            }
            crate::ssh::known_hosts::Verdict::Revoked { file, line } => format!("revoked ({}:{line})", file.display()),
        };
        let algorithm = key.algorithm().to_string();
        let Some(confirm) = confirm else {
            return Ok(json!({"host": alias, "presented_fingerprint": fp, "algorithm": algorithm, "status": status, "trusted": false}));
        };
        if confirm.trim() != fp {
            return Err(Error::new(
                ErrorCode::HostKeyMismatch,
                format!("{alias} now presents {fp}, not the confirmed {}", confirm.trim()),
            )
            .hint("nothing was changed; verify the fingerprint again"));
        }
        if status.starts_with("revoked") {
            return Err(Error::new(
                ErrorCode::HostKeyMismatch,
                format!("{alias}: key {fp} is revoked ({status})"),
            ));
        }
        let removed = kh.forget(name, h.port)?;
        kh.learn(name, h.port, &key)?;
        self.pool.drop_conn(alias).await;
        Ok(
            json!({"host": alias, "presented_fingerprint": fp, "algorithm": algorithm, "status": status,
                  "trusted": true, "removed_old_entries": removed, "trusted_fingerprint": fp}),
        )
    }

    async fn host_test(&self, alias: &str) -> Result<HostTestResult> {
        self.pool.drop_conn(alias).await;
        let conn = self.pool.get(alias).await?;
        let out = exec::run_text(
            &conn,
            "uname -s; uname -r; uname -m; hostname; (grep '^PRETTY_NAME=' /etc/os-release 2>/dev/null | cut -d= -f2- | tr -d '\"') || (sw_vers -productName 2>/dev/null); \
             echo \"CHARMAP=$(locale charmap 2>/dev/null)\"; echo \"UTF8LOC=$(locale -a 2>/dev/null | grep -i -m1 -E '^(C|en_US)\\.utf-?8$')\"",
            Duration::from_secs(20),
        )
        .await
        .unwrap_or_default();
        let field = |k: &str| {
            out.lines()
                .find_map(|l| l.strip_prefix(k))
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(String::from)
        };
        let l: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with("CHARMAP=") && !l.starts_with("UTF8LOC="))
            .collect();
        let facts = Facts {
            os: l.first().map(|s| s.to_ascii_lowercase()),
            kernel: l.get(1).map(|s| s.to_string()),
            arch: l.get(2).map(|s| s.to_string()),
            hostname: l.get(3).map(|s| s.to_string()),
            os_pretty: l.get(4).map(|s| s.to_string()),
            last_ok: Some(audit::now_rfc3339()),
            last_addr: self.hosts().get(alias).ok().map(|h| h.addr()),
            charmap: field("CHARMAP="),
            utf8_locale: field("UTF8LOC="),
        };
        let _ = self.hosts().set_facts(alias, facts.clone());
        // host test reconnects, so the key learned on this connection is always reported here.
        let host_key = match &conn.hostkey_event {
            Some(HostKeyEvent::Learned { fingerprint, saved }) => Some(format!(
                "new host key trusted on first use: {fingerprint}{}",
                if *saved { "" } else { " (NOT saved: known_hosts is not writable)" }
            )),
            _ => None,
        };
        let _ = conn.take_hostkey_note();
        Ok(HostTestResult {
            host: alias.to_string(),
            ok: true,
            auth: conn.auth_method.clone(),
            connect_ms: conn.connect_ms,
            facts,
            host_key,
            banner: conn.banner.clone(),
        })
    }

    async fn key_deploy(&self, alias: &str, key: &str) -> Result<Value> {
        let info = crate::ssh::keys::info(&self.ctx.paths, key)?;
        if self.hosts().get(alias)?.ad_hoc {
            return Err(
                Error::usage(format!("'{alias}' is not a saved host")).hint("save it with `xssh host add` first, then deploy the key")
            );
        }
        let (h, conn) = self.host_conn(alias).await?;
        let pk = shq(&info.public_key);
        let script = format!(
            "umask 077; mkdir -p ~/.ssh && touch ~/.ssh/authorized_keys && chmod 700 ~/.ssh && chmod 600 ~/.ssh/authorized_keys && \
             (grep -qxF {pk} ~/.ssh/authorized_keys || echo {pk} >> ~/.ssh/authorized_keys)"
        );
        exec::run_text(&conn, &script, Duration::from_secs(30)).await?;
        // Verify the key works on its own before switching the host to it.
        let test_host = Host {
            key_name: Some(key.to_string()),
            key: None,
            ..h.clone()
        };
        let jump = self.pool.jump_for(&h).await?;
        let c = crate::ssh::connect(&self.ctx, &test_host, jump).await?;
        if c.auth_method != "publickey" {
            return Err(Error::auth(format!(
                "key '{key}' was installed but the server did not accept it (auth used: {})",
                c.auth_method
            )));
        }
        self.hosts().update(|hosts| {
            if let Some(x) = hosts.iter_mut().find(|x| x.alias == alias) {
                x.key_name = Some(key.to_string());
                x.key = None;
            }
            Ok(())
        })?;
        self.pool.drop_conn(alias).await;
        Ok(json!({"host": alias, "key": key, "fingerprint": info.fingerprint, "verified": true,
                  "hint": format!("host now uses key '{key}'. The stored password is still used for sudo; only remove it (`xssh host set-password {alias} --clear`) if sudo needs none or a separate one is stored with --sudo")}))
    }

    async fn status(&self, selector: &str, only: &[String], top: usize, sudo: bool) -> Result<Value> {
        let hosts = self.hosts().select(selector)?;
        let single = hosts.len() == 1;
        let results: Vec<(usize, Result<probe::Status>)> = futures::stream::iter(hosts.into_iter().enumerate())
            .map(|(i, h)| async move {
                let r = async {
                    let conn = self.pool.get(&h.alias).await?;
                    let (sudo, pw) = self.sudo_for(&h, sudo)?;
                    let sc = SudoCtx {
                        enabled: sudo,
                        password: pw.as_ref().map(|x| x.as_str()),
                    };
                    probe::status(&conn, &h.alias, top, Some(&sc)).await
                }
                .await;
                (i, r)
            })
            .buffer_unordered(8)
            .collect()
            .await;
        let mut results = results;
        results.sort_by_key(|(i, _)| *i);
        if single {
            let st = results.pop().unwrap().1?;
            return to_value(probe::filter(st, only));
        }
        let arr: Vec<Value> = results
            .into_iter()
            .map(|(_, r)| match r {
                Ok(st) => serde_json::to_value(probe::filter(st, only)).unwrap_or(Value::Null),
                Err(e) => json!({"error": e}),
            })
            .collect();
        Ok(Value::Array(arr))
    }

    async fn diag(&self, alias: &str, sudo: bool) -> Result<Value> {
        let (h, conn) = self.host_conn(alias).await?;
        let (sudo, pw) = self.sudo_for(&h, sudo)?;
        let sc = SudoCtx {
            enabled: sudo,
            password: pw.as_ref().map(|x| x.as_str()),
        };
        let (st, perf, errors) = tokio::join!(
            probe::status(&conn, &h.alias, 5, Some(&sc)),
            probe::perf::perf(&conn, &h.alias, Duration::from_secs(5), Duration::from_secs(1), false),
            probe::run_script(
                &conn,
                "T=; timeout 5 true 2>/dev/null && T='timeout 20'; \
                 { J=$($T journalctl -p err --since=-1h --no-pager -q -o short-iso -n 20 2>/dev/null); \
                 if [ -n \"$J\" ]; then printf '%s\\n' \"$J\"; else $T dmesg 2>/dev/null | tail -n 20; fi; } | cut -c1-300",
                Some(&sc),
                Duration::from_secs(30)
            )
        );
        let st = st?;
        let mut findings = st.findings.clone();
        let mut perf_summary = Value::Null;
        if let Ok(p) = perf {
            for f in p.findings {
                if !findings.iter().any(|x| x.message == f.message) {
                    findings.push(f);
                }
            }
            perf_summary = serde_json::to_value(&p.summary)?;
        }
        findings.sort_by_key(|f| std::cmp::Reverse(f.severity));
        let recent_errors: Vec<String> = errors
            .map(|o| dedupe_log_lines(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        let healthy = !findings.iter().any(|f| f.severity >= probe::Severity::Warning);
        Ok(json!({
            "host": h.alias,
            "healthy": healthy,
            "findings": findings,
            "recent_errors": recent_errors,
            "perf_5s": perf_summary,
            "status": {
                "os": st.os, "cpu": st.cpu, "load": st.load, "mem": st.mem, "disks": st.disks,
                "top_cpu": st.top_cpu, "top_mem": st.top_mem, "failed_units": st.failed_units,
            },
        }))
    }
}

fn tag_host(mut e: Error, alias: &str) -> Error {
    if !e.message.contains(alias) {
        e.message = format!("[{alias}] {}", e.message);
    }
    e
}

/// The reader (agent) a session request speaks for.
fn reader(agent: &Option<String>) -> &str {
    agent.as_deref().filter(|a| !a.is_empty()).unwrap_or(DEFAULT_READER)
}

/// Audit records keep at most 500 bytes of a command.
fn clip(s: &str) -> String {
    if s.len() > 500 {
        format!("{}…", &s[..s.floor_char_boundary(500)])
    } else {
        s.to_string()
    }
}

fn audit_fill(rec: &mut AuditRecord, req: &Request) {
    match req {
        Request::Exec(p) => {
            rec.host = Some(p.host.clone());
            rec.command = Some(clip(&format!("{}{}", if p.sudo { "[sudo] " } else { "" }, p.command)));
        }
        Request::SessionOpen(p) => rec.host = Some(p.host.clone()),
        Request::SessionRun { id, command, .. } => {
            rec.target = Some(id.clone());
            rec.command = Some(clip(command));
        }
        Request::SessionSend {
            id,
            text,
            keys,
            enter,
            paste,
            ..
        } => {
            rec.target = Some(id.clone());
            let mut c = match text {
                Some(t) if *paste => format!("[paste: {} chars]", t.chars().count()),
                Some(t) => clip(t),
                None => String::new(),
            };
            if let Some(k) = keys {
                c.push_str(&format!(" [keys:{k}]"));
            }
            if *enter {
                c.push_str(" [enter]");
            }
            rec.command = Some(c);
        }
        Request::JobStart(p) => {
            rec.host = Some(p.host.clone());
            rec.command = Some(clip(&p.command));
        }
        Request::FileWrite { host, path, .. } | Request::FileEdit { host, path, .. } | Request::FileRead { host, path, .. } => {
            rec.host = Some(host.clone());
            rec.target = Some(path.clone());
        }
        Request::Copy { srcs, dst, .. } => {
            rec.host = srcs.iter().chain(std::iter::once(dst)).find_map(|e| e.host.clone());
            let from: Vec<String> = srcs.iter().map(|e| e.show()).collect();
            rec.target = Some(format!("{} -> {}", from.join(" "), dst.show()));
        }
        Request::HostTest { host }
        | Request::HostTrust { host, .. }
        | Request::Status { host, .. }
        | Request::Perf { host, .. }
        | Request::Diag { host, .. }
        | Request::Logs { host, .. }
        | Request::FileLs { host, .. }
        | Request::FileStat { host, .. }
        | Request::KeyDeploy { host, .. }
        | Request::ForwardAdd { host, .. } => rec.host = Some(host.clone()),
        Request::JobStatus { id }
        | Request::JobWait { id, .. }
        | Request::JobLogs { id, .. }
        | Request::JobKill { id, .. }
        | Request::JobRm { id } => rec.target = Some(id.clone()),
        Request::JobPrune { host, .. } => rec.host = host.clone(),
        _ => {}
    }
}

/// Collapse repeated journal lines (`TS HOST proc[pid]: msg`): lines that differ only in
/// timestamp, pid or numbers are shown once with a count and the last time.
fn dedupe_log_lines(text: &str) -> Vec<String> {
    let mut groups: Vec<(String, String, usize, String)> = vec![]; // (key, first line, count, last ts)
    for l in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let mut parts = l.splitn(3, ' ');
        let (ts, rest) = match (parts.next(), parts.next(), parts.next()) {
            (Some(ts), Some(_host), Some(rest)) => (ts, rest),
            _ => ("", l),
        };
        let key: String = rest.chars().map(|c| if c.is_ascii_digit() { '0' } else { c }).collect();
        match groups.iter_mut().find(|g| g.0 == key) {
            Some(g) => {
                g.2 += 1;
                g.3 = ts.to_string();
            }
            None => groups.push((key, l.to_string(), 1, ts.to_string())),
        }
    }
    groups
        .into_iter()
        .map(|(_, first, n, last)| if n > 1 { format!("{first}  [×{n}, last {last}]") } else { first })
        .collect()
}

/// Build a log query command for the remote OS.
pub fn logs_command(
    os: &str,
    unit: Option<&str>,
    file: Option<&str>,
    since: Option<&str>,
    grep: Option<&str>,
    priority: Option<&str>,
    tail: usize,
) -> Result<String> {
    let secs = match since {
        Some(s) => Some(text::parse_duration(s).map_err(Error::usage)?.as_secs().max(1)),
        None => None,
    };
    if let Some(p) = priority
        && !["emerg", "alert", "crit", "err", "warning", "notice", "info", "debug"].contains(&p)
    {
        return Err(Error::usage(format!(
            "invalid --priority '{p}' (emerg|alert|crit|err|warning|notice|info|debug)"
        )));
    }
    // Case-insensitive: agents search for "error" and expect to see "ERROR" too.
    let filter = |base: String| match grep {
        Some(g) => format!("{base} | grep -Ei -- {} | tail -n {tail}", shq(g)),
        None => format!("{base} | tail -n {tail}"),
    };
    if let Some(f) = file {
        return Ok(match grep {
            Some(g) => format!("grep -Ei -- {} {} | tail -n {tail}", shq(g), shq_path(f)),
            None => format!("tail -n {tail} {}", shq_path(f)),
        });
    }
    Ok(match os {
        "linux" => {
            let mut j = String::from("journalctl --no-pager -q -o short-iso");
            if let Some(u) = unit {
                j.push_str(&format!(" -u {}", shq(u)));
            }
            if let Some(s) = secs {
                j.push_str(&format!(" --since=-{s}s"));
            }
            if let Some(p) = priority {
                j.push_str(&format!(" -p {p}"));
            }
            if grep.is_none() {
                format!("{j} -n {tail} 2>&1")
            } else {
                filter(format!("{j} 2>&1"))
            }
        }
        "darwin" => {
            let mut l = format!("log show --style compact --last {}s", secs.unwrap_or(600));
            let mut pred = vec![];
            if let Some(u) = unit {
                pred.push(format!("process == \"{u}\""));
            }
            if matches!(priority, Some("emerg" | "alert" | "crit" | "err")) {
                pred.push("messageType == error OR messageType == fault".into());
            }
            if !pred.is_empty() {
                l.push_str(&format!(" --predicate {}", shq(&pred.join(" AND "))));
            }
            filter(l)
        }
        _ => {
            if unit.is_some() {
                return Err(Error::usage("--unit is only supported on Linux (journald) and macOS").hint("use --file /var/log/<name>"));
            }
            filter("cat /var/log/messages".into())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dedupes_journal_lines() {
        let t = concat!(
            "2026-09-28T10:00:01+08:00 h sudo[101]: auth failure uid=5\n",
            "2026-09-28T10:00:09+08:00 h sudo[202]: auth failure uid=5\n",
            "2026-09-28T10:01:00+08:00 h kernel: oops"
        );
        let d = dedupe_log_lines(t);
        assert_eq!(d.len(), 2);
        assert!(d[0].ends_with("[×2, last 2026-09-28T10:00:09+08:00]"), "{d:?}");
    }

    #[test]
    fn log_commands() {
        assert_eq!(
            logs_command("linux", Some("nginx"), None, Some("10m"), None, Some("err"), 100).unwrap(),
            "journalctl --no-pager -q -o short-iso -u nginx --since=-600s -p err -n 100 2>&1"
        );
        assert_eq!(
            logs_command("linux", None, Some("/var/log/x.log"), None, Some("err|warn"), None, 50).unwrap(),
            "grep -Ei -- 'err|warn' /var/log/x.log | tail -n 50"
        );
        assert!(logs_command("freebsd", Some("x"), None, None, None, None, 10).is_err());
    }
}
