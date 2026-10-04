//! One-shot command execution over an SSH exec channel.
//!
//! sshd runs the command string with the user's login shell (`$SHELL -c`, not a login shell).
//! xssh sends a bootstrap that parses in every common shell (sh, bash, zsh, fish, csh, tcsh):
//! it starts `sh`, records the shell pid for timeouts, and runs the command with `$SHELL -c`
//! when that is a POSIX-family shell (sh, bash, zsh, ksh, dash, ash...), else with bash (or sh).
//! So commands always get sh/bash syntax, also on hosts whose login shell is fish or csh.

use crate::ssh::Conn;
use russh::ChannelMsg;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use xssh_core::error::{Error, Result};
use xssh_core::text::{self, Truncated, shq, shq_path};

/// Raw result of running a command.
#[derive(Debug, Default)]
pub struct RawOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: Option<u32>,
    pub signal: Option<String>,
    pub timed_out: bool,
    /// On timeout: whether the remote process group is verified gone.
    pub killed: bool,
    pub duration: Duration,
}

/// First stdout line written by the bootstrap, carrying the pid of the shell that runs the
/// command. sshd makes that shell a process-group (and session) leader, so the pid covers
/// everything the command started.
const PID_TAG: &[u8] = b"\x1eXSSH_PID=";

/// Hard cap on bytes kept in memory per stream.
const MEM_CAP: usize = 64 * 1024 * 1024;

/// Shells whose `-c` accepts sh syntax; others (fish, csh, tcsh, nu, xonsh...) run commands with
/// bash, else sh.
const POSIX_SHELLS: &str = "sh|bash|zsh|dash|ksh|ksh93|mksh|oksh|loksh|pdksh|ash|yash|posh";

/// Characters that some login shells would interpret even inside single quotes (fish: `\` `'`,
/// csh: `!` and newlines), mapped to control bytes that `tr` restores.
const ENCODE: [(char, char); 4] = [('\'', '\u{1}'), ('\n', '\u{2}'), ('!', '\u{3}'), ('\\', '\u{4}')];

/// A command line every login shell parses the same way, which runs `script` in `sh`.
pub fn wrap(script: &str) -> String {
    if script.contains(['\u{1}', '\u{2}', '\u{3}', '\u{4}']) {
        // Control bytes in the command itself: send it plainly (POSIX login shells only).
        return format!("exec sh -c {} xssh", shq(script));
    }
    let enc: String = script
        .chars()
        .map(|c| ENCODE.iter().find(|(from, _)| *from == c).map(|(_, to)| *to).unwrap_or(c))
        .collect();
    format!("exec sh -c 'eval \"$(printf %s \"$1\" | tr \"\\001\\002\\003\\004\" \"\\047\\012\\041\\134\")\"' xssh '{enc}'")
}

/// The sh script that runs `command`: pid line, shell selection, then `command` in that shell.
fn bootstrap(command: &str, pty: bool) -> String {
    let mut s = String::new();
    if pty {
        // Secrets and stdin written to a pty would be echoed back.
        s.push_str("stty -echo 2>/dev/null; ");
    }
    s.push_str("printf '\\036XSSH_PID=%s\\n' \"$$\"; ");
    s.push_str(&format!(
        "S=${{SHELL:-/bin/sh}}; case \"${{S##*/}}\" in {POSIX_SHELLS}) ;; *) S=$(command -v bash 2>/dev/null || echo /bin/sh) ;; esac; \
         [ -x \"$S\" ] || S=/bin/sh; XSSH_SH=$S; export XSSH_SH; exec \"$S\" -c {}",
        shq(command)
    ));
    s
}

/// Run `command` on `conn`, feeding `stdin` and then EOF.
pub async fn run_raw(conn: &Conn, command: &str, stdin: &[u8], pty: bool, timeout: Duration) -> Result<RawOutput> {
    run_raw_opts(conn, command, stdin, pty, timeout, None).await
}

fn lost(e: impl std::fmt::Display) -> Error {
    Error::remote(format!(
        "connection lost after the command was sent ({e}); it may have run, fully or partly"
    ))
    .hint("check its effects before running it again (xssh does not retry commands that were already sent)")
}

/// Like `run_raw`; `sudo` (Some for sudo commands, with the password if one is needed) lets a
/// timeout kill root-owned processes.
pub async fn run_raw_opts(
    conn: &Conn,
    command: &str,
    stdin: &[u8],
    pty: bool,
    timeout: Duration,
    sudo: Option<Option<&str>>,
) -> Result<RawOutput> {
    let start = Instant::now();
    // Failures up to and including sending the exec request mean the command did not start:
    // they keep the Connect code, so callers may retry on a fresh connection.
    let ch = conn.open_session().await?;
    if pty {
        ch.request_pty(false, "xterm", 200, 50, 0, 0, &[])
            .await
            .map_err(|e| Error::connect(format!("request pty: {e}")))?;
    }
    ch.exec(true, wrap(&bootstrap(command, pty)).into_bytes())
        .await
        .map_err(|e| Error::connect(format!("send command: {e}")))?;

    let (mut rd, wr) = ch.split();
    let writer = async {
        if !stdin.is_empty() {
            wr.data_bytes(stdin.to_vec()).await?;
        }
        wr.eof().await
    };
    tokio::pin!(writer);
    let mut writing = true;
    let mut out = RawOutput::default();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let msg = tokio::select! {
            m = rd.wait() => m,
            r = &mut writer, if writing => {
                writing = false;
                // The command may exit without reading all of its stdin; that is not an error.
                let _ = r;
                continue;
            }
            _ = tokio::time::sleep_until(deadline) => {
                out.timed_out = true;
                let _ = wr.close().await;
                break;
            }
        };
        match msg {
            Some(ChannelMsg::Data { data }) => {
                if out.stdout.len() < MEM_CAP {
                    out.stdout.extend_from_slice(&data);
                }
            }
            Some(ChannelMsg::ExtendedData { data, .. }) => {
                if out.stderr.len() < MEM_CAP {
                    out.stderr.extend_from_slice(&data);
                }
            }
            Some(ChannelMsg::ExitStatus { exit_status }) => out.exit_code = Some(exit_status),
            Some(ChannelMsg::ExitSignal { signal_name, .. }) => out.signal = Some(format!("{signal_name:?}")),
            Some(ChannelMsg::Failure) => {
                return Err(Error::remote("server refused to execute the command"));
            }
            Some(_) => {}
            None => break,
        }
    }
    if !out.timed_out && out.exit_code.is_none() && out.signal.is_none() && conn.is_closed() {
        return Err(lost("the SSH connection closed"));
    }
    let pid = take_pid(&mut out.stdout);
    if out.timed_out {
        out.killed = match pid {
            Some(pid) => kill_group(conn, pid, sudo).await,
            None => false,
        };
    }
    out.duration = start.elapsed();
    Ok(out)
}

/// Terminate a process group and verify it is gone (true) — as root when the command ran with
/// sudo, since a user cannot signal root-owned processes.
async fn kill_group(conn: &Conn, pid: u32, sudo: Option<Option<&str>>) -> bool {
    let script = format!(
        "p={pid}; alive() {{ (ps -A -o pgid= 2>/dev/null || ps -o pgid= 2>/dev/null) | awk -v p=\"$p\" '$1==p {{f=1}} END {{exit !f}}'; }}; \
         kill -TERM -$p 2>/dev/null || kill -TERM $p 2>/dev/null; \
         for i in 1 2 3 4; do alive || exit 0; sleep 0.5 2>/dev/null || sleep 1; done; \
         kill -KILL -$p 2>/dev/null || kill -KILL $p 2>/dev/null; sleep 0.5 2>/dev/null || sleep 1; \
         alive && exit 1; exit 0"
    );
    let p = prepare(&script, None, &[], &[], sudo, b"", &[]);
    let run = async {
        let ch = conn.open_session().await.ok()?;
        ch.exec(true, wrap(&bootstrap(&p.command, false)).into_bytes()).await.ok()?;
        let (mut rd, wr) = ch.split();
        if !p.stdin.is_empty() {
            wr.data_bytes(p.stdin.clone()).await.ok()?;
        }
        wr.eof().await.ok()?;
        let mut code = None;
        while let Some(m) = rd.wait().await {
            if let ChannelMsg::ExitStatus { exit_status } = m {
                code = Some(exit_status);
            }
        }
        code
    };
    matches!(tokio::time::timeout(Duration::from_secs(15), run).await, Ok(Some(0)))
}

/// Remove the pid line from the start of stdout and return the pid (a pty adds `\r`).
fn take_pid(stdout: &mut Vec<u8>) -> Option<u32> {
    if !stdout.starts_with(PID_TAG) {
        return None;
    }
    let end = stdout.iter().position(|&b| b == b'\n')?;
    let pid = std::str::from_utf8(&stdout[PID_TAG.len()..end]).ok()?.trim().parse().ok();
    stdout.drain(..=end);
    pid
}

/// Convenience: run and return decoded stdout, failing on non-zero exit.
pub async fn run_text(conn: &Conn, command: &str, timeout: Duration) -> Result<String> {
    let o = run_raw(conn, command, b"", false, timeout).await?;
    if o.timed_out {
        return Err(Error::timeout(format!("remote command timed out after {}s", timeout.as_secs())));
    }
    if o.exit_code != Some(0) {
        let err = String::from_utf8_lossy(&o.stderr);
        return Err(Error::remote(format!(
            "remote command failed (exit {:?}): {}",
            o.exit_code,
            err.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

pub use xssh_core::api::ExecParams;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecResult {
    pub host: String,
    pub exit_code: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    pub stdout: Truncated,
    pub stderr: Truncated,
    pub duration_ms: u64,
    #[serde(default)]
    pub timed_out: bool,
    /// After a timeout: the remote process group was verified gone.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub killed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// A host key learned on first contact during this call (one line).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key: Option<String>,
}

/// A command plus the stdin preamble that delivers secrets to it.
pub struct Prepared {
    pub command: String,
    pub stdin: Vec<u8>,
    /// Secret values to redact from any output.
    pub redact: Vec<String>,
}

/// The shell that runs commands (chosen by the bootstrap; see the module docs).
const RUN_SHELL: &str = "\"${XSSH_SH:-${SHELL:-/bin/sh}}\"";

/// Build the remote command line for exec-like operations (an sh script; `run_raw` adds the
/// bootstrap).
///
/// Secrets (sudo password, env secrets) are written as the first lines of stdin and read by the
/// wrapper script, so they never appear in the remote process list, the audit log or the agent's
/// view. The user's command always runs in the same shell (see the module docs), with or
/// without sudo/secrets, so shell syntax behaves the same in every mode.
pub fn prepare(
    command: &str,
    cwd: Option<&str>,
    env: &[(String, String)],
    env_secrets: &[(String, String)],
    sudo: Option<Option<&str>>,
    user_stdin: &[u8],
    secret_values: &[String],
) -> Prepared {
    let mut stdin = Vec::new();
    let mut redact: Vec<String> = vec![];

    let mut inner = String::new();
    if let Some(dir) = cwd {
        // `|| exit`: never run the command somewhere else when the directory is missing.
        inner.push_str(&format!("cd {} || exit 1; ", shq_path(dir)));
    }
    for (k, v) in env {
        inner.push_str(&format!("export {k}={}; ", shq(v)));
    }
    inner.push_str(command);

    // Reads secret env vars from stdin (POSIX sh), then execs the shell with the command.
    let mut runner = String::new();
    for (i, (var, _)) in env_secrets.iter().enumerate() {
        runner.push_str(&format!(
            "IFS= read -r __xssh_s{i}; export {var}=\"$__xssh_s{i}\"; unset __xssh_s{i}; "
        ));
    }
    runner.push_str("exec \"$0\" -c \"$1\"");

    let script = match sudo {
        None if env_secrets.is_empty() => inner,
        None => format!("sh -c {} {RUN_SHELL} {}", shq(&runner), shq(&inner)),
        Some(pw) => {
            // `sudo -v` validates (reading the password from our pipe, not from the
            // command's stdin), then `sudo -n` runs the command using the cached credential.
            let mut s = String::new();
            if let Some(pw) = pw {
                s.push_str("IFS= read -r __xssh_pw; ");
                s.push_str(SUDO_VALIDATE);
                stdin.extend_from_slice(pw.as_bytes());
                stdin.push(b'\n');
                redact.push(pw.to_string());
            }
            // The sudo'd sh reads the secret env vars itself (sudo resets the environment).
            // No `exec` before sudo: without a tty sudo caches credentials per parent pid, so both
            // sudo invocations must be children of the same shell.
            s.push_str(&format!("sudo -n -- sh -c {} {RUN_SHELL} {}", shq(&runner), shq(&inner)));
            format!("sh -c {}", shq(&s))
        }
    };
    for v in secret_values.iter().take(env_secrets.len()) {
        stdin.extend_from_slice(v.as_bytes());
        stdin.push(b'\n');
        redact.push(v.clone());
    }
    stdin.extend_from_slice(user_stdin);
    Prepared {
        command: script,
        stdin,
        redact,
    }
}

/// Validate the sudo password; on failure say why (exit 126). sudo must be a direct child of
/// this shell (its credential cache is per parent pid without a tty), so its stderr goes to a
/// file rather than through `$(...)`.
const SUDO_VALIDATE: &str = "__xssh_f=$(umask 077; mktemp 2>/dev/null) || __xssh_f=/tmp/.xssh-sudo.$$; \
printf '%s\\n' \"$__xssh_pw\" | sudo -S -p '' -v >/dev/null 2>\"$__xssh_f\"; __xssh_rc=$?; unset __xssh_pw; \
__xssh_e=$(cat \"$__xssh_f\" 2>/dev/null); rm -f \"$__xssh_f\"; \
if [ $__xssh_rc -ne 0 ]; then \
case \"$__xssh_e\" in \
*'not in the sudoers'*|*'is not allowed'*|*'may not run sudo'*|*'afraid I can'*|*'not permitted'*) __xssh_m=\"user $(id -un 2>/dev/null) may not use sudo on this host\" ;; \
*'terminal is required'*|*requiretty*|*'no tty'*) __xssh_m='sudo requires a terminal here (Defaults requiretty): use exec --pty or a session' ;; \
*'incorrect password'*|*'try again'*|*'Authentication failed'*|*'authentication failure'*|*'no password was provided'*) __xssh_m='wrong sudo password' ;; \
*'not found'*) __xssh_m='sudo is not installed' ;; \
*) __xssh_m='sudo -v failed' ;; \
esac; \
printf 'xssh: sudo: %s [%s]\\n' \"$__xssh_m\" \"$(printf '%s' \"$__xssh_e\" | tr '\\n' ' ')\" >&2; exit 126; fi; ";

/// Post-process raw output into what the agent sees.
pub fn finish(
    host: &str,
    raw: RawOutput,
    redact: &[String],
    encoding: Option<&str>,
    max: usize,
    paths: &xssh_core::paths::Paths,
    timeout: Duration,
) -> ExecResult {
    let (stdout, det_out) = text::decode_detect(&raw.stdout, encoding);
    let (stderr, det_err) = text::decode_detect(&raw.stderr, encoding);
    let stdout = text::redact(&text::clean_plain(&stdout), redact);
    let stderr = text::redact(&text::clean_plain(&stderr), redact);
    let mut hint = det_out
        .or(det_err)
        .map(|e| format!("output was not UTF-8 and was decoded as {e}; `xssh host edit {host} --encoding {e}` makes this explicit"));
    if raw.timed_out {
        let fate = if raw.killed {
            "the remote process group was killed (verified)"
        } else {
            "killing the remote processes could not be verified, they may still run (check with `pgrep -af ...`)"
        };
        hint = Some(format!(
            "timed out after {}s; {fate}. For long work use `xssh job start {host} -- <cmd>` then `xssh job wait`",
            timeout.as_secs()
        ));
    } else if raw.exit_code == Some(126) && stderr.contains("xssh: sudo: wrong sudo password") {
        hint = Some(format!(
            "ask the user to store the right sudo password: `xssh host set-password {host} --sudo` (the login password is used when none is stored)"
        ));
    } else if hint.is_none() && (stderr.contains("sudo: a password is required") || stderr.contains("sudo: a terminal is required")) {
        hint = Some(format!(
            "sudo needs a password: ask the user and store it with `xssh host set-password {host} --sudo` (or the login password is used by default)"
        ));
    }
    ExecResult {
        host: host.to_string(),
        exit_code: raw.exit_code.map(|c| c as i64),
        signal: raw.signal,
        stdout: text::truncate(stdout, max, Some(paths), &format!("{host}-stdout")),
        stderr: text::truncate(stderr, max, Some(paths), &format!("{host}-stderr")),
        duration_ms: raw.duration.as_millis() as u64,
        timed_out: raw.timed_out,
        killed: raw.killed,
        error: None,
        hint,
        host_key: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_plain() {
        let p = prepare("ls -la", Some("/tmp x"), &[("A".into(), "1 2".into())], &[], None, b"", &[]);
        assert_eq!(p.command, "cd '/tmp x' || exit 1; export A='1 2'; ls -la");
        assert!(p.stdin.is_empty());
    }

    #[test]
    fn prepare_sudo_keeps_password_off_command_line() {
        let p = prepare("id", None, &[], &[], Some(Some("s3cr3t")), b"data", &[]);
        assert!(!p.command.contains("s3cr3t"));
        assert!(p.command.starts_with("sh -c '"));
        assert!(p.command.contains("sudo -S -p '\\'''\\'' -v"));
        assert!(p.command.contains("sudo -n -- sh -c"));
        assert!(p.command.ends_with(" id'"));
        assert_eq!(p.stdin, b"s3cr3t\ndata");
        assert_eq!(p.redact, vec!["s3cr3t".to_string()]);
    }

    #[test]
    fn prepare_env_secret() {
        let p = prepare(
            "echo $TOKEN",
            None,
            &[],
            &[("TOKEN".into(), "tok".into())],
            None,
            b"",
            &["abc123".into()],
        );
        assert!(!p.command.contains("abc123"));
        assert!(p.command.starts_with("sh -c 'IFS= read -r __xssh_s0; export TOKEN=\"$__xssh_s0\""));
        assert_eq!(p.stdin, b"abc123\n");
    }

    #[test]
    fn wrap_has_no_characters_other_shells_interpret() {
        let w = wrap("echo 'a' \"b\" \\n !x\nls");
        // After the fixed prefix, the payload is one single-quoted word without ' \ ! or newline.
        let payload = w.rsplit_once(" xssh '").unwrap().1;
        let payload = payload.strip_suffix('\'').unwrap();
        assert!(!payload.contains(['\'', '\\', '!', '\n']));
        assert!(!w.contains('\n'));
        assert!(!w.contains('!'));
        // The fixed part has no `\\` or `\'` pairs (fish would collapse them inside '...').
        assert!(!w.contains("\\\\") && !w.contains("\\'"));
    }

    #[test]
    fn pid_line_with_pty_carriage_return() {
        let mut v = b"\x1eXSSH_PID=4242\r\nhello\r\n".to_vec();
        assert_eq!(take_pid(&mut v), Some(4242));
        assert_eq!(v, b"hello\r\n");
    }

    /// Runs the bootstrap through local shells when they exist (sh always on Unix CI; fish/csh/zsh
    /// when installed), checking that quoting survives.
    #[cfg(unix)]
    #[test]
    fn wrap_round_trips_through_local_shells() {
        let script = "printf '%s|' \"it's\" 'a\\b' '!x' \"$((1+2))\"; printf '\\n'";
        for sh in ["sh", "bash", "zsh", "fish", "csh", "tcsh"] {
            let Ok(out) = std::process::Command::new(sh).arg("-c").arg(wrap(script)).output() else {
                continue;
            };
            assert_eq!(String::from_utf8_lossy(&out.stdout), "it's|a\\b|!x|3|\n", "shell {sh}");
        }
    }

    #[test]
    fn error_codes_distinguish_before_and_after_send() {
        assert_eq!(lost("x").code, xssh_core::error::ErrorCode::Remote);
    }
}
