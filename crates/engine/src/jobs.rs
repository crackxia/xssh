//! Remote background jobs: detached in their own process group (setsid, or perl's setpgrp
//! where setsid is missing, e.g. macOS), output logged to a file on the remote host, so they
//! survive disconnects and daemon restarts and are not bound by the agent's tool-call timeout.
//!
//! Safety: the job's pid is recorded together with the process start time, and a pid whose
//! start time differs (reused after a reboot or an external kill) is never reported as running
//! and never signalled. Environment values (`--env`, `--env-secret`) reach the job through a
//! 0600 file that the job deletes on start, never through the command line.

use crate::exec::{self, run_raw};
use crate::files::sh;
use crate::ssh::Conn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::paths::Paths;
use xssh_core::text::{self, short_id, shq, shq_path};
use xssh_store::jobs::{self, JobRecord};

const T: Duration = Duration::from_secs(30);

pub use xssh_core::api::JobStartParams;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Exited,
    /// Stopped by `xssh job kill`.
    Killed,
    /// Neither running nor an exit code recorded (host rebooted, killed externally...).
    Lost,
}

impl JobState {
    pub fn as_str(self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Exited => "exited",
            JobState::Killed => "killed",
            JobState::Lost => "lost",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub id: String,
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub command: String,
    pub state: JobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub started_at: String,
    pub log_bytes: u64,
    pub log_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Last lines of the log (for status/wait).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobLogs {
    pub id: String,
    pub state: JobState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    pub output: String,
    /// Byte offset to pass as `--offset` to continue reading (always at a character boundary).
    pub next_offset: u64,
    pub log_bytes: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub more: bool,
}

/// `st PID` prints the process start time (empty when it cannot be determined), `alive PID`
/// tells whether the pid exists (also for other users' processes).
const SH_PROC: &str = "st() { if [ -r /proc/$1/stat ]; then sed 's/^.*) //' /proc/$1/stat | cut -d' ' -f20; \
     else ps -o lstart= -p \"$1\" 2>/dev/null | tr -s ' '; fi; }; \
     alive() { [ -d /proc/$1 ] || ps -p \"$1\" >/dev/null 2>&1; }; ";

/// Remote job files older than this many days are deleted when a new job starts on the host.
const REMOTE_RETENTION_DAYS: u64 = 30;

fn files_of(rec: &JobRecord, ext: &str) -> String {
    format!("{}/{}.{ext}", rec.remote_dir, rec.id)
}

/// `export K='v'` lines for the job's environment file.
fn env_file(env: &[(String, String)]) -> String {
    env.iter().map(|(k, v)| format!("export {k}={}\n", shq(v))).collect()
}

pub async fn start(conn: &Conn, paths: &Paths, p: &JobStartParams, secret_values: &[String], sudo_pw: Option<&str>) -> Result<JobStatus> {
    for (k, _) in p.env.iter().chain(&p.env_secrets) {
        if k.is_empty() || !k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(Error::usage(format!("invalid variable name '{k}'")));
        }
    }
    // The job directory is private (0700): logs may contain secrets. Old finished jobs' files
    // are pruned here, so ~/.xssh/jobs does not grow without bound.
    let prep = format!(
        "umask 077; d=\"$HOME/.xssh/jobs\"; mkdir -p \"$d\" && chmod 700 \"$HOME/.xssh\" \"$d\" && cd \"$d\" || exit 1; \
         find . -type f \\( -name 'j*.exit' -o -name 'j*.killed' \\) -mtime +{REMOTE_RETENTION_DAYS} 2>/dev/null | \
         while IFS= read -r f; do b=${{f%.*}}; rm -f \"$b.log\" \"$b.exit\" \"$b.pid\" \"$b.killed\" \"$b.env\"; done; pwd"
    );
    let dir = exec::run_text(conn, &sh(&prep), T).await?;
    let dir = dir.trim().lines().last().unwrap_or("").to_string();
    if dir.is_empty() {
        return Err(Error::remote("could not create ~/.xssh/jobs on the remote host"));
    }
    let id = format!("j{}", short_id(6));
    let mut rec = JobRecord {
        id: id.clone(),
        host: p.host.clone(),
        command: p.command.clone(),
        name: p.name.clone(),
        cwd: p.cwd.clone(),
        started_at: xssh_store::audit::now_rfc3339(),
        remote_dir: dir.clone(),
        pid: None,
        pid_start: None,
        sudo: p.sudo,
        last_state: Some("running".into()),
        exit_code: None,
    };
    let mut env: Vec<(String, String)> = p.env.clone();
    for ((var, _), v) in p.env_secrets.iter().zip(secret_values) {
        env.push((var.clone(), v.clone()));
    }
    let envf = files_of(&rec, "env");
    let launcher = launcher(&rec, &p.command, p.cwd.as_deref(), (!env.is_empty()).then_some(envf.as_str()));
    let stdin = if env.is_empty() { Vec::new() } else { env_file(&env).into_bytes() };
    let prepared = exec::prepare(&sh(&launcher), None, &[], &[], p.sudo.then_some(sudo_pw), &stdin, &[]);
    let mut redact = prepared.redact.clone();
    redact.extend(secret_values.iter().cloned());
    let out = run_raw(conn, &prepared.command, &prepared.stdin, false, T).await?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| stdout.lines().find_map(|l| l.strip_prefix(k)).map(|v| v.trim().to_string());
    let pid = field("PID=").and_then(|v| v.parse::<u32>().ok());
    if out.exit_code != Some(0) || pid.is_none() {
        let err = text::redact(&String::from_utf8_lossy(&out.stderr), &redact);
        return Err(Error::remote(format!("failed to start job: {}", err.trim())));
    }
    rec.pid = pid;
    rec.pid_start = field("ST=").filter(|s| !s.is_empty());
    jobs::save(paths, &rec)?;
    // Give very short commands a moment so the first status is meaningful.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let mut st = status(conn, &rec, 20).await?;
    remember(paths, &mut rec, &st);
    if st.state == JobState::Running {
        st.hint = Some(format!(
            "`xssh job logs {id} --offset {}` reads new output; `xssh job wait {id} --timeout 90s` blocks (exit 124 = still running)",
            st.log_bytes
        ));
    }
    Ok(st)
}

/// The launcher script; with `envf` it first stores the environment file read from stdin.
fn launcher(rec: &JobRecord, command: &str, cwd: Option<&str>, envf: Option<&str>) -> String {
    let cd = match cwd {
        Some(c) => format!("cd {} || exit 1; ", shq_path(c)),
        None => String::new(),
    };
    let pidf = files_of(rec, "pid");
    let log = rec.log_path();
    // The wrapper records its own pid ($$): in its own process group it is the group leader, so
    // `kill -SIG -PID` reaches the whole job (setsid may fork, so $! is not reliable). It loads
    // and deletes the environment file, runs the command in the login shell and records the
    // exit code atomically.
    let wrapper = "echo $$ > \"$3\"; if [ -f \"$4\" ]; then . \"$4\"; rm -f \"$4\"; fi; \
                   \"${SHELL:-/bin/sh}\" -c \"$1\"; echo $? > \"$2.tmp\" && mv \"$2.tmp\" \"$2\"";
    let args = format!(
        "xssh-job {cmd} {exit} {pid} {env}",
        cmd = shq(command),
        exit = shq(&rec.exit_path()),
        pid = shq(&pidf),
        env = shq(envf.unwrap_or("/nonexistent")),
    );
    let mut s = String::new();
    if let Some(f) = envf {
        // The env file (0600) arrives on stdin; it is removed by the job as soon as it starts.
        s.push_str(&format!("(umask 077; cat > {}) || exit 1; ", shq(f)));
    }
    s.push_str(&format!(
        "{cd}{SH_PROC}rm -f {pid}; : > {log} && chmod 644 {log} || exit 1; W={w}; \
         if command -v setsid >/dev/null 2>&1; then nohup setsid sh -c \"$W\" {args} >> {log} 2>&1 < /dev/null & \
         elif command -v perl >/dev/null 2>&1; then nohup perl -e 'setpgrp(0,0); exec @ARGV' sh -c \"$W\" {args} >> {log} 2>&1 < /dev/null & \
         else nohup sh -c \"$W\" {args} >> {log} 2>&1 < /dev/null & fi; \
         i=0; while [ ! -s {pid} ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i+1)); done; P=$(cat {pid} 2>/dev/null); \
         echo \"PID=$P\"; echo \"ST=$(st \"$P\")\"",
        pid = shq(&pidf),
        log = shq(&log),
        w = shq(wrapper),
    ));
    s
}

/// Shell lines printing one job's raw state, used for single and batched status queries.
fn status_lines(rec: &JobRecord, tail_lines: usize) -> String {
    let expect = rec.pid_start.as_deref().map(shq).unwrap_or_else(|| "''".into());
    format!(
        "E={exit}; L={log}; P=$(cat {pid} 2>/dev/null); \
         if [ -f \"$E\" ]; then echo \"EXIT:$(cat \"$E\")\"; fi; \
         if [ -f {killed} ]; then echo KILLED; fi; \
         if [ -n \"$P\" ] && alive \"$P\"; then X={expect}; if [ -z \"$X\" ] || [ \"$(st \"$P\")\" = \"$X\" ]; then echo RUNNING; else echo REUSED; fi; fi; \
         echo \"SIZE:$(wc -c < \"$L\" 2>/dev/null | tr -d ' ')\"; echo '--TAIL--'; {tail}",
        exit = shq(&rec.exit_path()),
        log = shq(&rec.log_path()),
        pid = shq(&files_of(rec, "pid")),
        killed = shq(&files_of(rec, "killed")),
        tail = if tail_lines > 0 {
            format!("tail -n {tail_lines} \"$L\" 2>/dev/null")
        } else {
            ":".to_string()
        },
    )
}

fn parse_status(rec: &JobRecord, out: &str, tail_lines: usize) -> JobStatus {
    let (head, tail) = out.split_once("--TAIL--\n").unwrap_or((out, ""));
    let mut exit_code = None;
    let (mut running, mut killed, mut reused) = (false, false, false);
    let mut size = 0u64;
    for l in head.lines() {
        if let Some(v) = l.strip_prefix("EXIT:") {
            exit_code = v.trim().parse().ok();
        } else if l == "RUNNING" {
            running = true;
        } else if l == "REUSED" {
            reused = true;
        } else if l == "KILLED" {
            killed = true;
        } else if let Some(v) = l.strip_prefix("SIZE:") {
            size = v.trim().parse().unwrap_or(0);
        }
    }
    let state = if exit_code.is_some() {
        JobState::Exited
    } else if running {
        JobState::Running
    } else if killed {
        JobState::Killed
    } else {
        JobState::Lost
    };
    JobStatus {
        id: rec.id.clone(),
        host: rec.host.clone(),
        name: rec.name.clone(),
        command: rec.command.clone(),
        state,
        exit_code,
        started_at: rec.started_at.clone(),
        log_bytes: size,
        log_path: rec.log_path(),
        pid: rec.pid,
        tail: (tail_lines > 0).then(|| text::clean_plain(tail)),
        hint: (reused && state == JobState::Lost).then(|| {
            "the job's pid now belongs to another process (the host rebooted or the job was killed externally); the job is gone".to_string()
        }),
    }
}

pub async fn status(conn: &Conn, rec: &JobRecord, tail_lines: usize) -> Result<JobStatus> {
    let cmd = format!("{SH_PROC}{}", status_lines(rec, tail_lines));
    let out = exec::run_raw(conn, &sh(&cmd), b"", false, T).await?;
    if out.timed_out {
        return Err(Error::timeout("job status query timed out"));
    }
    let text = text::decode(&out.stdout, None);
    Ok(parse_status(rec, &text, tail_lines))
}

/// Status of several jobs on one host in a single exec.
pub async fn status_many(conn: &Conn, recs: &[&JobRecord]) -> Result<HashMap<String, JobStatus>> {
    if recs.is_empty() {
        return Ok(HashMap::new());
    }
    let mut cmd = String::from(SH_PROC);
    for r in recs {
        cmd.push_str(&format!("echo '@@JOB {}'; {}; ", r.id, status_lines(r, 0)));
    }
    let out = exec::run_raw(conn, &sh(&cmd), b"", false, T).await?;
    if out.timed_out {
        return Err(Error::timeout("job status query timed out"));
    }
    if out.exit_code != Some(0) {
        return Err(Error::remote(format!(
            "job status query failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut map = HashMap::new();
    for block in text.split("@@JOB ").skip(1) {
        let (id, body) = block.split_once('\n').unwrap_or((block, ""));
        if let Some(r) = recs.iter().find(|r| r.id == id.trim()) {
            map.insert(r.id.clone(), parse_status(r, body, 0));
        }
    }
    Ok(map)
}

/// Store the last seen state, so finished jobs are not queried again by `job list`.
pub fn remember(paths: &Paths, rec: &mut JobRecord, st: &JobStatus) {
    let state = Some(st.state.as_str().to_string());
    if rec.last_state != state || rec.exit_code != st.exit_code {
        rec.last_state = state;
        rec.exit_code = st.exit_code;
        let _ = jobs::save(paths, rec);
    }
}

/// Length of the longest prefix of `b` that does not end inside a UTF-8 sequence, and the
/// number of leading continuation bytes (an offset that points inside a character).
fn utf8_bounds(b: &[u8]) -> (usize, usize) {
    let front = b.iter().take(3).take_while(|x| (**x & 0xC0) == 0x80).count();
    let mut end = b.len();
    // Look back at most 3 bytes for the start of the last sequence.
    for back in 1..=3.min(b.len().saturating_sub(front)) {
        let c = b[b.len() - back];
        if c & 0xC0 == 0x80 {
            continue;
        }
        let need = if c >= 0xF0 {
            4
        } else if c >= 0xE0 {
            3
        } else if c >= 0xC0 {
            2
        } else {
            1
        };
        if need > back {
            end = b.len() - back;
        }
        break;
    }
    (end.max(front), front)
}

pub async fn logs(
    conn: &Conn,
    rec: &JobRecord,
    offset: Option<u64>,
    tail: Option<usize>,
    max: usize,
    encoding: Option<&str>,
) -> Result<JobLogs> {
    let st = status(conn, rec, 0).await?;
    let log = shq(&rec.log_path());
    let offset = offset.map(|o| o.min(st.log_bytes));
    let (cmd, start) = match (tail, offset) {
        (Some(n), _) => (format!("tail -n {n} {log}"), None),
        (None, Some(off)) => (format!("tail -c +{} {log} | head -c {max}", off + 1), Some(off)),
        (None, None) => (format!("head -c {max} {log}"), Some(0)),
    };
    let out = exec::run_raw(conn, &sh(&cmd), b"", false, T).await?;
    let mut bytes: &[u8] = &out.stdout;
    let next = match start {
        Some(s) => {
            // Never cut a character: skip a partial one at the front, stop before a partial one
            // at the end (it is returned by the next call).
            let (end, front) = if encoding.is_some_and(|e| !e.eq_ignore_ascii_case("utf-8")) {
                (bytes.len(), 0)
            } else {
                utf8_bounds(bytes)
            };
            bytes = &bytes[front..end];
            s + end as u64
        }
        None => st.log_bytes,
    };
    let output = text::clean_plain(&text::decode(bytes, encoding));
    Ok(JobLogs {
        id: rec.id.clone(),
        state: st.state,
        exit_code: st.exit_code,
        output,
        next_offset: next,
        log_bytes: st.log_bytes,
        more: next < st.log_bytes,
    })
}

pub async fn wait(conn: &Conn, rec: &JobRecord, timeout: Duration, tail_lines: usize) -> Result<JobStatus> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut delay = Duration::from_millis(500);
    loop {
        let st = status(conn, rec, 0).await?;
        if st.state != JobState::Running || tokio::time::Instant::now() >= deadline {
            let mut st = status(conn, rec, tail_lines).await?;
            if st.state == JobState::Running {
                st.hint = Some(format!(
                    "still running after {}s (exit 124). Wait again, or `xssh job logs {} --offset {}` for new output",
                    timeout.as_secs(),
                    rec.id,
                    st.log_bytes
                ));
            }
            return Ok(st);
        }
        let left = deadline - tokio::time::Instant::now();
        tokio::time::sleep(delay.min(left)).await;
        delay = (delay * 2).min(Duration::from_secs(3));
    }
}

fn kill_script(rec: &JobRecord, pid: u32, sig: &str) -> String {
    let expect = rec.pid_start.as_deref().map(shq).unwrap_or_else(|| "''".into());
    // Re-check the start time right before signalling: a reused pid is never signalled.
    // The group kill reaches everything the job started; without a group (no setsid/perl) the
    // process tree is walked with pgrep.
    format!(
        "{SH_PROC}P={pid}; X={expect}; if ! alive $P || {{ [ -n \"$X\" ] && [ \"$(st $P)\" != \"$X\" ]; }}; then echo GONE; exit 0; fi; \
         echo {sig} > {killed}; \
         kt() {{ for c in $(pgrep -P \"$1\" 2>/dev/null); do kt \"$c\"; done; kill -{sig} \"$1\" 2>/dev/null; }}; \
         kill -{sig} -$P 2>/dev/null || kt $P",
        killed = shq(&files_of(rec, "killed"))
    )
}

pub async fn kill(conn: &Conn, rec: &JobRecord, signal: &str, sudo_pw: Option<&str>) -> Result<JobStatus> {
    let sig = signal.trim_start_matches("SIG").to_ascii_uppercase();
    if sig.is_empty() || !sig.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(Error::usage(format!("bad signal '{signal}'")));
    }
    let Some(pid) = rec.pid else {
        return Err(Error::new(ErrorCode::NotFound, "job has no recorded pid"));
    };
    // Never signal a finished job: its pid may have been reused by another process.
    let before = status(conn, rec, 0).await?;
    if before.state != JobState::Running {
        let mut st = status(conn, rec, 10).await?;
        st.hint = Some("job is not running; nothing was signalled".into());
        return Ok(st);
    }
    let prepared = exec::prepare(
        &sh(&kill_script(rec, pid, &sig)),
        None,
        &[],
        &[],
        rec.sudo.then_some(sudo_pw),
        b"",
        &[],
    );
    let out = run_raw(conn, &prepared.command, &prepared.stdin, false, T).await?;
    if out.exit_code != Some(0) {
        let err = text::redact(&String::from_utf8_lossy(&out.stderr), &prepared.redact);
        return Err(Error::remote(format!("kill failed: {}", err.trim())));
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let mut st = status(conn, rec, 10).await?;
    if String::from_utf8_lossy(&out.stdout).contains("GONE") {
        st.hint = Some("the job had already ended; nothing was signalled".into());
    }
    Ok(st)
}

/// Delete a job's files and record; a running job is killed first. Returns whether it was killed.
pub async fn remove(conn: &Conn, paths: &Paths, rec: &JobRecord, sudo_pw: Option<&str>) -> Result<bool> {
    let mut killed = false;
    if status(conn, rec, 0).await?.state == JobState::Running {
        kill(conn, rec, "TERM", sudo_pw).await?;
        if status(conn, rec, 0).await?.state == JobState::Running {
            kill(conn, rec, "KILL", sudo_pw).await?;
        }
        killed = true;
    }
    remove_files(conn, rec).await;
    jobs::remove(paths, &rec.id)?;
    Ok(killed)
}

/// Delete a finished job's remote files (best effort).
pub async fn remove_files(conn: &Conn, rec: &JobRecord) {
    let files: Vec<String> = ["log", "exit", "pid", "killed", "env"]
        .iter()
        .map(|x| shq(&files_of(rec, x)))
        .collect();
    // Files of sudo jobs may be root-owned; removing them from the user's own dir still works.
    let _ = run_raw(conn, &sh(&format!("rm -f {}", files.join(" "))), b"", false, T).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> JobRecord {
        JobRecord {
            id: "jabc".into(),
            host: "h".into(),
            command: "make".into(),
            name: None,
            cwd: None,
            started_at: "2026-01-01T00:00:00Z".into(),
            remote_dir: "/home/u/.xssh/jobs".into(),
            pid: Some(42),
            pid_start: Some("123456".into()),
            sudo: false,
            last_state: None,
            exit_code: None,
        }
    }

    #[test]
    fn utf8_boundaries() {
        let s = "a中b".as_bytes(); // 61 e4 b8 ad 62
        assert_eq!(utf8_bounds(s), (5, 0));
        assert_eq!(utf8_bounds(&s[..3]), (1, 0), "cut inside 中");
        assert_eq!(utf8_bounds(&s[2..]), (3, 2), "starts inside 中");
        assert_eq!(utf8_bounds(b""), (0, 0));
        assert_eq!(utf8_bounds("é".as_bytes()), (2, 0));
    }

    #[test]
    fn status_parsing() {
        let r = rec();
        let st = parse_status(&r, "RUNNING\nSIZE:10\n--TAIL--\nhello\n", 5);
        assert_eq!((st.state, st.log_bytes), (JobState::Running, 10));
        assert_eq!(st.tail.as_deref(), Some("hello\n"));
        let st = parse_status(&r, "REUSED\nSIZE:3\n--TAIL--\n", 0);
        assert_eq!(st.state, JobState::Lost);
        assert!(st.hint.unwrap().contains("another process"));
        let st = parse_status(&r, "EXIT:2\nSIZE:0\n--TAIL--\n", 0);
        assert_eq!((st.state, st.exit_code), (JobState::Exited, Some(2)));
    }

    #[test]
    fn scripts_keep_secrets_off_the_command_line() {
        let r = rec();
        let s = launcher(&r, "echo $TOKEN", Some("/srv/app"), Some("/home/u/.xssh/jobs/jabc.env"));
        assert!(!s.contains("TOKEN="));
        assert!(s.starts_with("(umask 077; cat > /home/u/.xssh/jobs/jabc.env)"));
        assert!(s.contains("cd /srv/app || exit 1;"));
        assert!(s.contains("setpgrp"));
        let env = env_file(&[("TOKEN".into(), "s3 cr'et".into())]);
        assert_eq!(env, "export TOKEN='s3 cr'\\''et'\n");
        let k = kill_script(&r, 42, "TERM");
        assert!(k.contains("X=123456"));
        assert!(k.contains("kill -TERM -$P"));
        assert!(status_lines(&r, 0).contains("X=123456"));
    }
}
