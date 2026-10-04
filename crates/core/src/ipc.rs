//! Local IPC between CLI and daemon: one newline-delimited JSON request and one
//! response per connection, over a Windows named pipe or a Unix domain socket
//! inside the (owner-only) xssh home. A random token written to an owner-only
//! file authenticates clients.
//!
//! Windows: the pipe only admits the current user (and SYSTEM), and a client checks that the
//! pipe it reached was created by the current user before it sends the token, so another user
//! who creates the pipe name first cannot collect it.

use crate::api::{Envelope, Request, Response, version_tag};
use crate::error::{Error, ErrorCode, Result};
use crate::paths::Paths;
use interprocess::local_socket::tokio::{Listener, Stream, prelude::*};
use interprocess::local_socket::{GenericFilePath, GenericNamespaced, ListenerOptions, Name};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const MAX_MSG: usize = 256 * 1024 * 1024;

fn name(paths: &Paths) -> Result<Name<'static>> {
    if cfg!(windows) {
        paths
            .socket_name()
            .to_ns_name::<GenericNamespaced>()
            .map(|n| n.into_owned())
            .map_err(|e| Error::internal(format!("pipe name: {e}")))
    } else {
        paths
            .socket_path()
            .to_fs_name::<GenericFilePath>()
            .map(|n| n.into_owned())
            .map_err(|e| Error::internal(format!("socket name: {e}")))
    }
}

pub async fn connect(paths: &Paths) -> std::io::Result<Stream> {
    let n = name(paths).map_err(|e| std::io::Error::other(e.message))?;
    let s = Stream::connect(n).await?;
    #[cfg(windows)]
    verify_server(&s)?;
    Ok(s)
}

/// Refuse a pipe created by another user (PermissionDenied).
#[cfg(windows)]
fn verify_server(s: &Stream) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let Stream::NamedPipe(p) = s;
    let h = p.inner().as_raw_handle();
    // SAFETY: the handle belongs to `s`, which outlives this call.
    let (ok, owner) = unsafe { crate::winsec::owned_by_us(h)? };
    if !ok {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("the xssh daemon pipe was created by another account ({owner}); refusing to talk to it"),
        ));
    }
    Ok(())
}

pub async fn listen(paths: &Paths) -> Result<Listener> {
    if connect(paths).await.is_ok() {
        return Err(Error::new(ErrorCode::Daemon, "a daemon is already running for this home"));
    }
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(paths.socket_path());
    }
    let n = name(paths)?;
    let opts = ListenerOptions::new().name(n);
    #[cfg(windows)]
    let opts = {
        use interprocess::os::windows::local_socket::ListenerOptionsExt;
        use interprocess::os::windows::security_descriptor::SecurityDescriptor;
        let sddl = crate::winsec::pipe_sddl().map_err(|e| Error::new(ErrorCode::Daemon, format!("pipe security: {e}")))?;
        let sddl = widestring::U16CString::from_str(&sddl).map_err(|e| Error::internal(format!("pipe security: {e}")))?;
        let sd = SecurityDescriptor::deserialize(&sddl).map_err(|e| Error::new(ErrorCode::Daemon, format!("pipe security: {e}")))?;
        opts.security_descriptor(sd)
    };
    let l = opts
        .create_tokio()
        .map_err(|e| Error::new(ErrorCode::Daemon, format!("cannot listen: {e}")))?;
    #[cfg(unix)]
    crate::paths::restrict_file(&paths.socket_path());
    Ok(l)
}

pub async fn read_line(stream: &Stream) -> Result<String> {
    read_line_checked(stream, None).await
}

/// Read one request line; with `prefix`, fail as soon as the line does not start with it
/// (the daemon checks the token, the first field of every envelope, before reading on).
pub async fn read_line_checked(stream: &Stream, prefix: Option<&[u8]>) -> Result<String> {
    let mut reader = BufReader::new(stream);
    let mut buf = Vec::new();
    let mut checked = prefix.is_none();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            break;
        }
        let (take, done) = match chunk.iter().position(|b| *b == b'\n') {
            Some(i) => (i, true),
            None => (chunk.len(), false),
        };
        buf.extend_from_slice(&chunk[..take]);
        reader.consume(if done { take + 1 } else { take });
        if !checked && let Some(p) = prefix {
            let n = buf.len().min(p.len());
            if !constant_time_eq(&buf[..n], &p[..n]) {
                return Err(Error::new(ErrorCode::Daemon, "invalid daemon token"));
            }
            checked = buf.len() >= p.len();
        }
        if done {
            break;
        }
        if buf.len() > MAX_MSG {
            return Err(Error::internal("IPC message too large"));
        }
    }
    if !checked && !buf.is_empty() {
        return Err(Error::new(ErrorCode::Daemon, "invalid daemon token"));
    }
    String::from_utf8(buf).map_err(|_| Error::internal("IPC message is not UTF-8"))
}

/// The bytes every envelope for `token` starts with (serde writes `token` first).
pub fn token_prefix(token: &str) -> Vec<u8> {
    format!("{{\"token\":\"{token}\"").into_bytes()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub async fn write_line(mut stream: &Stream, s: &str) -> Result<()> {
    stream.write_all(s.as_bytes()).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;
    Ok(())
}

pub fn read_token(paths: &Paths) -> Option<String> {
    std::fs::read_to_string(paths.daemon_token()).ok().map(|s| s.trim().to_string())
}

/// Send one request to a running daemon.
pub async fn request(paths: &Paths, req: &Request) -> std::result::Result<serde_json::Value, CallError> {
    send(paths, req, false).await
}

/// Like `request`, as a monitoring front end (see `Envelope::observer`).
pub async fn observe(paths: &Paths, req: &Request) -> std::result::Result<serde_json::Value, CallError> {
    send(paths, req, true).await
}

async fn send(paths: &Paths, req: &Request, observer: bool) -> std::result::Result<serde_json::Value, CallError> {
    let stream = connect(paths).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            CallError::Failed(
                Error::new(ErrorCode::Daemon, e.to_string())
                    .hint("another account holds xssh's pipe name; stop that process, or use a separate home (--home / XSSH_HOME)"),
            )
        } else {
            CallError::NotRunning
        }
    })?;
    let env = Envelope {
        token: read_token(paths).unwrap_or_default(),
        version: version_tag(),
        client_pid: std::process::id(),
        client_cwd: std::env::current_dir().ok().map(|p| p.display().to_string()),
        observer,
        request: req.clone(),
    };
    let line = serde_json::to_string(&env).map_err(|e| CallError::Failed(e.into()))?;
    write_line(&stream, &line).await.map_err(CallError::Failed)?;
    let resp = read_line(&stream).await.map_err(CallError::Failed)?;
    if resp.is_empty() {
        return Err(CallError::Failed(Error::new(
            ErrorCode::Daemon,
            "daemon closed the connection without replying",
        )));
    }
    match serde_json::from_str::<Response>(&resp) {
        Ok(Response::Ok { result, daemon }) => {
            if let Some(d) = daemon
                && !observer
            {
                warn_old_daemon(paths, &d);
            }
            Ok(result)
        }
        Ok(Response::Err { error }) => {
            if error.code == ErrorCode::Daemon && error.message.starts_with("version mismatch") {
                Err(CallError::VersionMismatch)
            } else {
                Err(CallError::Failed(error))
            }
        }
        Err(e) => Err(CallError::Failed(Error::internal(format!("bad daemon response: {e}")))),
    }
}

/// One stderr line per (daemon build, client build) pair: the request was served by a running
/// daemon of another build that was kept because it has sessions/forwards.
fn warn_old_daemon(paths: &Paths, daemon: &str) {
    use sha2::{Digest, Sha256};
    let tag = hex::encode(&Sha256::digest(format!("{daemon}|{}", version_tag()).as_bytes())[..6]);
    let marker = paths.run_dir().join(format!("warned-{tag}"));
    if std::fs::OpenOptions::new().write(true).create_new(true).open(&marker).is_ok() {
        eprintln!(
            "[xssh] served by a running daemon of another build ({daemon}; this is {}), kept because it has open sessions/forwards; changes in this build apply after `xssh daemon restart` (closes them)",
            version_tag()
        );
    }
}

pub enum CallError {
    NotRunning,
    VersionMismatch,
    Failed(Error),
}

/// Start a detached daemon process for this home. Only the `xssh` CLI calls this: the daemon
/// runs from (a copy of) the current executable. Other front ends run `xssh daemon start`.
pub fn spawn_daemon(paths: &Paths) -> Result<()> {
    let exe = daemon_exe(paths)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--home")
        .arg(&paths.home)
        .args(["daemon", "run"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::{CREATE_BREAKAWAY_FROM_JOB, CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
        disable_std_handle_inheritance();
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_BREAKAWAY_FROM_JOB);
        if cmd.spawn().is_ok() {
            return Ok(());
        }
        // The job object may forbid breakaway; retry without it.
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
        cmd.spawn()
            .map_err(|e| Error::new(ErrorCode::Daemon, format!("cannot start daemon: {e}")))?;
        Ok(())
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        cmd.spawn()
            .map_err(|e| Error::new(ErrorCode::Daemon, format!("cannot start daemon: {e}")))?;
        Ok(())
    }
}

/// On Windows a running executable cannot be replaced, so the daemon runs from
/// a per-build copy inside the home directory; the installed binary stays
/// upgradable while a daemon is running.
#[cfg(windows)]
fn daemon_exe(paths: &Paths) -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let name = format!("xssh-daemon-{}.exe", crate::api::build_id());
    let target = paths.run_dir().join(&name);
    if !target.exists() {
        let tmp = paths.run_dir().join(format!("{name}.{}.tmp", std::process::id()));
        std::fs::copy(&exe, &tmp).map_err(|e| Error::io(format!("copy daemon executable: {e}")))?;
        if std::fs::rename(&tmp, &target).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
    // Remove copies from older builds (fails harmlessly while one is running).
    if let Ok(rd) = std::fs::read_dir(paths.run_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("xssh-daemon-") && n != name {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
    Ok(target)
}

#[cfg(not(windows))]
fn daemon_exe(_paths: &Paths) -> Result<std::path::PathBuf> {
    Ok(std::env::current_exe()?)
}

/// Make our own stdio handles non-inheritable so the detached daemon does not
/// keep the caller's pipes open (which would make e.g. an agent's shell tool
/// wait for the daemon to exit).
#[cfg(windows)]
fn disable_std_handle_inheritance() {
    use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
    for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: GetStdHandle/SetHandleInformation only read/modify handle flags.
        unsafe {
            let handle = GetStdHandle(h);
            if !handle.is_null() {
                SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
}

/// Wait until the daemon accepts connections.
pub async fn wait_ready(paths: &Paths, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if connect(paths).await.is_ok() && read_token(paths).is_some() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

/// Wait until the daemon stopped accepting connections.
pub async fn wait_stopped(paths: &Paths, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if connect(paths).await.is_err() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}
