//! `xssh cp`: copy files and directories between this machine and saved hosts, or between two
//! hosts (streamed through this machine, so the hosts need no route or credentials to each
//! other). Directories copy recursively. With `--sudo`, root-only paths are staged through
//! `~/.xssh/tmp` on the host.
//!
//! Beyond scp/rsync defaults, for agents:
//! - every file is written to `.NAME.xssh-part` and renamed over the target (atomic: readers
//!   never see half a file, running binaries can be replaced);
//! - unchanged files (same size and mtime, or same sha256 with `--checksum`) are skipped, and
//!   mtimes are preserved so the next run can tell;
//! - an interrupted large file resumes from its `.xssh-part` (its prefix is verified by sha256);
//! - copies within one host run on the host (no round trip through this machine);
//! - `--exclude`, `--delete`, `--dry-run`, `--links`, `--verify`.

use crate::exec::run_raw;
use crate::files::{self, SH_HASH, SudoCtx, sh};
use crate::ssh::Conn;
use russh_sftp::client::{RawSftpSession, SftpSession};
use russh_sftp::protocol::{FileAttributes, OpenFlags, Packet, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeekExt, AsyncWrite, AsyncWriteExt};
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::text::{short_id, shq, shq_path};
use zeroize::Zeroizing;

pub use xssh_core::api::{CopyOptions, Endpoint};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CopyResult {
    /// Files written.
    pub files: usize,
    pub dirs: usize,
    /// Bytes transferred.
    pub bytes: u64,
    /// Files skipped because the destination already had them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unchanged: usize,
    /// Destination entries removed by `--delete`.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub deleted: usize,
    /// Symlinks created (`--links`).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub links: usize,
    /// Files continued from an interrupted copy.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub resumed: usize,
    /// Where each source ended up.
    pub targets: Vec<String>,
    /// Changed destination paths: `+` new, `~` replaced, `-` deleted (first 50).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changed: Vec<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub changed_more: usize,
    /// Set when exactly one file was copied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// For one copied directory: sha256 of its `sha256sum` listing (see `tree_sha`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tree_sha256: Option<String>,
    /// Entries that are neither files nor directories (sockets, devices, broken links, loops).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub skipped: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped_paths: Vec<String>,
    /// Nothing was changed (`--dry-run`): the counts say what would happen.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Every written file was re-read on the destination and matched (`--verify`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verified: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub duration_ms: u64,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

/// A connected host taking part in a copy.
pub struct HostIo {
    pub conn: Arc<Conn>,
    pub sudo: bool,
    pub password: Option<Zeroizing<String>>,
}

impl HostIo {
    fn sudo_ctx(&self) -> SudoCtx<'_> {
        SudoCtx {
            enabled: self.sudo,
            password: self.password.as_ref().map(|p| p.as_str()),
        }
    }
}

/// Root-side staging copies: large trees take long; the agent's own call timeout applies anyway.
const STAGE_TIMEOUT: Duration = Duration::from_secs(6 * 3600);
/// Files at least this large keep their `.xssh-part` after a failure, so a rerun resumes.
const RESUME_MIN: u64 = 16 * 1024 * 1024;
/// Symlinked directories nested deeper than this are treated as a loop.
const MAX_DEPTH: usize = 256;
/// Entries listed in `changed` / `skipped_paths`.
const LIST_MAX: usize = 50;
const PART: &str = ".xssh-part";
/// Files copied at once: SFTP requests are pipelined per file, so small-file trees gain most.
const PARALLEL_FILES: usize = 8;
const CHUNK: usize = 256 * 1024;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    File,
    Dir,
    Missing,
    Other,
    /// A symlink kept as a link (`--links`).
    Link,
}

#[derive(Clone, Copy, Debug)]
struct Meta {
    kind: Kind,
    mode: Option<u32>,
    size: u64,
    mtime: Option<u32>,
    /// The entry itself is a symlink (its target's metadata is shown unless `kind == Link`).
    link: bool,
}

impl Meta {
    fn missing() -> Meta {
        Meta {
            kind: Kind::Missing,
            mode: None,
            size: 0,
            mtime: None,
            link: false,
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The mtime to give a copied file. Mtimes only travel in whole seconds, so a source modified in
/// the last couple of seconds could change again within that same second and then look unchanged
/// (same size, same second) to the next run. Like git's "racily clean" entries, such a copy gets
/// a mtime one second earlier: the next run copies it once more, after which it is stable.
fn racy_safe_mtime(mtime: u32, now: u64) -> u32 {
    if u64::from(mtime) + 2 >= now {
        mtime.saturating_sub(1)
    } else {
        mtime
    }
}

fn meta_of(a: &FileAttributes) -> Meta {
    let kind = match files::kind_of(a.permissions) {
        "dir" => Kind::Dir,
        "file" => Kind::File,
        "symlink" => Kind::Link,
        _ => Kind::Other,
    };
    Meta {
        kind,
        mode: a.permissions.map(|x| x & 0o7777),
        size: a.size.unwrap_or(0),
        mtime: a.mtime,
        link: kind == Kind::Link,
    }
}

fn local_meta(m: &std::fs::Metadata) -> Meta {
    let kind = if m.is_dir() {
        Kind::Dir
    } else if m.is_file() {
        Kind::File
    } else if m.file_type().is_symlink() {
        Kind::Link
    } else {
        Kind::Other
    };
    Meta {
        kind,
        mode: local_mode(m),
        size: m.len(),
        mtime: m
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as u32),
        link: kind == Kind::Link,
    }
}

/// One host's SFTP session, plus a raw session for `posix-rename@openssh.com` (atomic replace).
pub(crate) struct Remote {
    alias: String,
    sftp: SftpSession,
    raw: Option<RawSftpSession>,
    conn: Arc<Conn>,
}

impl Remote {
    async fn open(alias: &str, conn: Arc<Conn>) -> Result<Remote> {
        let sftp = files::sftp(&conn).await?;
        // A second channel for extended requests; optional (a server may cap channels).
        let raw = async {
            let ch = conn.open_session().await.ok()?;
            ch.request_subsystem(true, "sftp").await.ok()?;
            let raw = RawSftpSession::new(ch.into_stream());
            let v = raw.init().await.ok()?;
            v.extensions.contains_key("posix-rename@openssh.com").then_some(raw)
        }
        .await;
        Ok(Remote {
            alias: alias.to_string(),
            sftp,
            raw,
            conn,
        })
    }

    async fn close(&self) {
        let _ = self.sftp.close().await;
    }
}

#[derive(Clone, Copy)]
enum Fs<'a> {
    Local,
    Remote(&'a Remote),
}

fn ssh_string(s: &str) -> Vec<u8> {
    let mut v = (s.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(s.as_bytes());
    v
}

impl<'a> Fs<'a> {
    fn show(&self, p: &str) -> String {
        match self {
            Fs::Local => p.to_string(),
            Fs::Remote(r) => format!("{}:{p}", r.alias),
        }
    }

    fn same_host(&self, other: &Fs) -> bool {
        matches!((self, other), (Fs::Remote(a), Fs::Remote(b)) if a.alias == b.alias)
    }

    fn denied(&self, what: &str, p: &str, e: impl std::fmt::Display) -> Error {
        let mut msg = e.to_string();
        // SFTP status errors render as "<status>: <message>", usually the same text twice.
        if let Some((a, b)) = msg.split_once(": ")
            && a.eq_ignore_ascii_case(b)
        {
            msg = a.to_string();
        }
        let mut err = Error::remote(format!("{what} {}: {msg}", self.show(p)));
        let lower = msg.to_ascii_lowercase();
        if lower.contains("no such file") || lower.contains("not found") {
            err.code = ErrorCode::NotFound;
        } else if lower.contains("permission denied") && matches!(self, Fs::Remote(..)) {
            err = err.hint("retry with --sudo");
        }
        if matches!(self, Fs::Local) {
            err.code = if err.code == ErrorCode::NotFound {
                ErrorCode::NotFound
            } else {
                ErrorCode::Io
            };
        }
        err
    }

    /// Metadata following symlinks (a missing path is `Kind::Missing`, not an error).
    async fn stat(&self, p: &str) -> Result<Meta> {
        match self {
            Fs::Local => match tokio::fs::metadata(p).await {
                Ok(m) => Ok(local_meta(&m)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Meta::missing()),
                Err(e) => Err(self.denied("stat", p, e)),
            },
            Fs::Remote(r) => match r.sftp.metadata(p.to_string()).await {
                Ok(m) => Ok(meta_of(&m)),
                Err(e) if e.to_string().to_ascii_lowercase().contains("no such file") => Ok(Meta::missing()),
                Err(e) => Err(self.denied("stat", p, e)),
            },
        }
    }

    /// Metadata of the entry itself (a symlink is `Kind::Link`).
    async fn lstat(&self, p: &str) -> Result<Meta> {
        match self {
            Fs::Local => match tokio::fs::symlink_metadata(p).await {
                Ok(m) => Ok(local_meta(&m)),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Meta::missing()),
                Err(e) => Err(self.denied("stat", p, e)),
            },
            Fs::Remote(r) => match r.sftp.symlink_metadata(p.to_string()).await {
                Ok(m) => Ok(meta_of(&m)),
                Err(e) if e.to_string().to_ascii_lowercase().contains("no such file") => Ok(Meta::missing()),
                Err(e) => Err(self.denied("stat", p, e)),
            },
        }
    }

    /// Entries of a directory. With `follow`, symlinks show their target's metadata (`link` set;
    /// a broken link is `Missing`); without, they are `Kind::Link`.
    async fn list(&self, p: &str, follow: bool) -> Result<Vec<(String, Meta)>> {
        let mut v = vec![];
        match self {
            Fs::Local => {
                let mut rd = tokio::fs::read_dir(p).await.map_err(|e| self.denied("list", p, e))?;
                while let Some(e) = rd.next_entry().await? {
                    let name = e.file_name().to_string_lossy().to_string();
                    let full = self.join(p, &name);
                    let mut m = self.lstat(&full).await?;
                    if m.kind == Kind::Link && follow {
                        m = Meta {
                            link: true,
                            ..self.stat(&full).await?
                        };
                    }
                    v.push((name, m));
                }
            }
            Fs::Remote(r) => {
                for e in r.sftp.read_dir(p.to_string()).await.map_err(|e| self.denied("list", p, e))? {
                    let name = e.file_name();
                    if name == "." || name == ".." {
                        continue;
                    }
                    let mut m = meta_of(&e.metadata());
                    if m.kind == Kind::Link && follow {
                        m = Meta {
                            link: true,
                            ..self.stat(&self.join(p, &name)).await?
                        };
                    }
                    v.push((name, m));
                }
            }
        }
        v.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(v)
    }

    async fn canon(&self, p: &str) -> String {
        match self {
            Fs::Local => std::fs::canonicalize(p)
                .map(|x| x.display().to_string())
                .unwrap_or_else(|_| p.to_string()),
            Fs::Remote(r) => r.sftp.canonicalize(p.to_string()).await.unwrap_or_else(|_| p.to_string()),
        }
    }

    /// Real path of `p`, also when it does not exist yet (its parent's real path + name).
    async fn canon_new(&self, p: &str) -> String {
        if self.lstat(p).await.is_ok_and(|m| m.kind != Kind::Missing) {
            return self.canon(p).await;
        }
        let parent = self.parent(p);
        let base = if parent.is_empty() { ".".to_string() } else { parent };
        self.join(&self.canon(&base).await, &base_name(p))
    }

    /// Create one directory (its parent exists); an existing directory is fine.
    async fn mkdir(&self, p: &str) -> Result<()> {
        match self {
            Fs::Local => tokio::fs::create_dir_all(p).await.map_err(|e| self.denied("mkdir", p, e)),
            Fs::Remote(r) => {
                if let Err(e) = r.sftp.create_dir(p.to_string()).await {
                    match self.stat(p).await?.kind {
                        Kind::Dir => {}
                        Kind::Missing => return Err(self.denied("mkdir", p, e)),
                        _ => return Err(Error::usage(format!("{} exists and is not a directory", self.show(p)))),
                    }
                }
                Ok(())
            }
        }
    }

    async fn mkdir_p(&self, p: &str) -> Result<()> {
        match self {
            Fs::Local => tokio::fs::create_dir_all(p).await.map_err(|e| self.denied("mkdir", p, e)),
            Fs::Remote(r) => {
                let mut cur = if p.starts_with('/') { "/".to_string() } else { String::new() };
                for part in p.split('/').filter(|x| !x.is_empty() && *x != ".") {
                    cur = if cur.is_empty() || cur == "/" {
                        format!("{cur}{part}")
                    } else {
                        format!("{cur}/{part}")
                    };
                    match self.stat(&cur).await?.kind {
                        Kind::Dir => {}
                        Kind::Missing => r.sftp.create_dir(cur.clone()).await.map_err(|e| self.denied("mkdir", &cur, e))?,
                        _ => return Err(Error::usage(format!("{} exists and is not a directory", self.show(&cur)))),
                    }
                }
                Ok(())
            }
        }
    }

    async fn reader(&self, p: &str, offset: u64) -> Result<Box<dyn AsyncRead + Unpin + Send + 'a>> {
        Ok(match self {
            Fs::Local => {
                let mut f = tokio::fs::File::open(p).await.map_err(|e| self.denied("open", p, e))?;
                if offset > 0 {
                    f.seek(std::io::SeekFrom::Start(offset))
                        .await
                        .map_err(|e| self.denied("seek", p, e))?;
                }
                Box::new(f)
            }
            Fs::Remote(r) => {
                let mut f = r.sftp.open(p.to_string()).await.map_err(|e| self.denied("open", p, e))?;
                if offset > 0 {
                    f.seek(std::io::SeekFrom::Start(offset))
                        .await
                        .map_err(|e| self.denied("seek", p, e))?;
                }
                Box::new(f)
            }
        })
    }

    /// Open `p` for writing: from the start (created with `create_mode`, truncated), or
    /// appending at `offset` to an existing partial file.
    async fn writer(&self, p: &str, create_mode: Option<u32>, offset: u64) -> Result<Box<dyn AsyncWrite + Unpin + Send + 'a>> {
        Ok(match self {
            Fs::Local => {
                let f = if offset > 0 {
                    let mut f = tokio::fs::OpenOptions::new()
                        .write(true)
                        .open(p)
                        .await
                        .map_err(|e| self.denied("open", p, e))?;
                    f.seek(std::io::SeekFrom::Start(offset))
                        .await
                        .map_err(|e| self.denied("seek", p, e))?;
                    f
                } else {
                    tokio::fs::File::create(p).await.map_err(|e| self.denied("create", p, e))?
                };
                if offset == 0
                    && let Some(m) = create_mode
                {
                    self.chmod(p, m).await?;
                }
                Box::new(f)
            }
            Fs::Remote(r) => {
                if offset > 0 {
                    let mut f = r
                        .sftp
                        .open_with_flags(p.to_string(), OpenFlags::WRITE)
                        .await
                        .map_err(|e| self.denied("open", p, e))?;
                    f.seek(std::io::SeekFrom::Start(offset))
                        .await
                        .map_err(|e| self.denied("seek", p, e))?;
                    Box::new(f)
                } else {
                    let attrs = FileAttributes {
                        permissions: create_mode,
                        ..FileAttributes::empty()
                    };
                    Box::new(
                        r.sftp
                            .open_with_flags_and_attributes(
                                p.to_string(),
                                OpenFlags::CREATE | OpenFlags::TRUNCATE | OpenFlags::WRITE,
                                attrs,
                            )
                            .await
                            .map_err(|e| self.denied("create", p, e))?,
                    )
                }
            }
        })
    }

    async fn chmod(&self, p: &str, mode: u32) -> Result<()> {
        match self {
            Fs::Local => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    tokio::fs::set_permissions(p, std::fs::Permissions::from_mode(mode))
                        .await
                        .map_err(|e| self.denied("chmod", p, e))?;
                }
                let _ = (p, mode);
                Ok(())
            }
            Fs::Remote(r) => {
                let attrs = FileAttributes {
                    permissions: Some(mode),
                    ..FileAttributes::empty()
                };
                r.sftp
                    .set_metadata(p.to_string(), attrs)
                    .await
                    .map_err(|e| self.denied("chmod", p, e))
            }
        }
    }

    /// Set atime and mtime (best effort: some filesystems refuse).
    async fn set_mtime(&self, p: &str, mtime: u32) -> bool {
        match self {
            Fs::Local => {
                let p = p.to_string();
                tokio::task::spawn_blocking(move || {
                    let t = std::time::UNIX_EPOCH + Duration::from_secs(mtime as u64);
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&p)
                        .and_then(|f| f.set_times(std::fs::FileTimes::new().set_modified(t).set_accessed(t)))
                        .is_ok()
                })
                .await
                .unwrap_or(false)
            }
            Fs::Remote(r) => {
                let attrs = FileAttributes {
                    atime: Some(mtime),
                    mtime: Some(mtime),
                    ..FileAttributes::empty()
                };
                r.sftp.set_metadata(p.to_string(), attrs).await.is_ok()
            }
        }
    }

    /// Rename `from` over `to`, replacing it atomically where the server allows.
    async fn rename_over(&self, from: &str, to: &str) -> Result<()> {
        match self {
            Fs::Local => tokio::fs::rename(from, to).await.map_err(|e| self.denied("rename", to, e)),
            Fs::Remote(r) => {
                if let Some(raw) = &r.raw {
                    let mut data = ssh_string(from);
                    data.extend(ssh_string(to));
                    match raw.extended("posix-rename@openssh.com", data).await {
                        Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => return Ok(()),
                        Ok(Packet::Status(s)) => return Err(self.denied("rename", to, s.error_message)),
                        _ => {}
                    }
                }
                if r.sftp.rename(from.to_string(), to.to_string()).await.is_ok() {
                    return Ok(());
                }
                // Plain SFTP rename refuses to replace: an atomic `mv` on the host does.
                let out = run_raw(
                    &r.conn,
                    &sh(&format!("mv -f -- {} {}", shq(from), shq(to))),
                    b"",
                    false,
                    Duration::from_secs(60),
                )
                .await?;
                if out.exit_code == Some(0) {
                    Ok(())
                } else {
                    Err(self.denied("rename", to, String::from_utf8_lossy(&out.stderr).trim()))
                }
            }
        }
    }

    async fn remove_file(&self, p: &str) {
        match self {
            Fs::Local => {
                let _ = tokio::fs::remove_file(p).await;
            }
            Fs::Remote(r) => {
                let _ = r.sftp.remove_file(p.to_string()).await;
            }
        }
    }

    async fn read_link(&self, p: &str) -> Result<String> {
        match self {
            Fs::Local => tokio::fs::read_link(p)
                .await
                .map(|t| t.display().to_string())
                .map_err(|e| self.denied("readlink", p, e)),
            Fs::Remote(r) => r.sftp.read_link(p.to_string()).await.map_err(|e| self.denied("readlink", p, e)),
        }
    }

    /// Create symlink `p` pointing at `target` (replacing an existing file or link).
    async fn symlink(&self, p: &str, target: &str) -> Result<()> {
        match self {
            Fs::Local => {
                let _ = tokio::fs::remove_file(p).await;
                #[cfg(unix)]
                {
                    tokio::fs::symlink(target, p).await.map_err(|e| self.denied("symlink", p, e))
                }
                #[cfg(not(unix))]
                {
                    let _ = target;
                    Err(Error::io(format!("cannot create symlink {p} on Windows")))
                }
            }
            Fs::Remote(r) => {
                // OpenSSH's sftp-server takes SSH_FXP_SYMLINK arguments in reverse order, so use
                // `ln -s` on the host, which has no ambiguity.
                let out = run_raw(
                    &r.conn,
                    &sh(&format!("ln -sfn -- {} {}", shq(target), shq(p))),
                    b"",
                    false,
                    Duration::from_secs(60),
                )
                .await?;
                if out.exit_code == Some(0) {
                    Ok(())
                } else {
                    Err(self.denied("symlink", p, String::from_utf8_lossy(&out.stderr).trim()))
                }
            }
        }
    }

    /// sha256 of the first `n` bytes (for resuming a partial file).
    async fn prefix_sha(&self, p: &str, n: u64) -> Result<String> {
        match self {
            Fs::Local => {
                let mut f = tokio::fs::File::open(p).await.map_err(|e| self.denied("open", p, e))?.take(n);
                let mut h = Sha256::new();
                let mut buf = vec![0u8; CHUNK];
                loop {
                    let k = f.read(&mut buf).await.map_err(|e| self.denied("read", p, e))?;
                    if k == 0 {
                        break;
                    }
                    h.update(&buf[..k]);
                }
                Ok(hex::encode(h.finalize()))
            }
            Fs::Remote(r) => {
                let script = format!("{SH_HASH}head -c {n} -- {} | hs", shq(p));
                let out = run_raw(&r.conn, &sh(&script), b"", false, Duration::from_secs(600)).await?;
                Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
            }
        }
    }

    /// sha256 of several files (missing or unreadable ones are left out).
    async fn hash_many(&self, paths: &[String]) -> Result<HashMap<String, String>> {
        let mut out = HashMap::new();
        if paths.is_empty() {
            return Ok(out);
        }
        match self {
            Fs::Local => {
                for p in paths {
                    if let Ok(h) = self.prefix_sha(p, u64::MAX).await {
                        out.insert(p.clone(), h);
                    }
                }
            }
            Fs::Remote(r) => {
                let list: String = paths.iter().filter(|p| !p.contains('\n')).map(|p| format!("{p}\n")).collect();
                let script = format!("{SH_HASH}while IFS= read -r f; do [ -f \"$f\" ] && printf '%s %s\\n' \"$(h \"$f\")\" \"$f\"; done");
                let res = run_raw(&r.conn, &sh(&script), list.as_bytes(), false, Duration::from_secs(3600)).await?;
                for l in String::from_utf8_lossy(&res.stdout).lines() {
                    if let Some((h, p)) = l.split_once(' ')
                        && h.len() == 64
                    {
                        out.insert(p.to_string(), h.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    fn join(&self, dir: &str, name: &str) -> String {
        match self {
            Fs::Local => Path::new(dir).join(name).display().to_string(),
            Fs::Remote(..) => files::join_remote(dir, name),
        }
    }

    fn parent(&self, p: &str) -> String {
        match self {
            Fs::Local => Path::new(p).parent().map(|x| x.display().to_string()).unwrap_or_default(),
            Fs::Remote(..) => remote_parent(p),
        }
    }

    /// Last path component; resolves `.`/`~`/`/` style names through the server.
    async fn name(&self, p: &str) -> String {
        let b = base_name(p);
        if !b.is_empty() && b != "." && b != ".." {
            return b;
        }
        let real = match self {
            Fs::Local => std::fs::canonicalize(p).map(|x| x.display().to_string()).unwrap_or_default(),
            Fs::Remote(r) => r.sftp.canonicalize(p.to_string()).await.unwrap_or_default(),
        };
        Some(base_name(&real)).filter(|x| !x.is_empty()).unwrap_or_else(|| "root".into())
    }

    /// Where a file is written before it replaces `dst`: `.NAME.xssh-part` next to it.
    fn part_of(&self, dst: &str) -> String {
        let name = base_name(dst);
        let dir = self.parent(dst);
        let part = format!(".{name}{PART}");
        if dir.is_empty() { part } else { self.join(&dir, &part) }
    }
}

fn base_name(x: &str) -> String {
    x.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or("").to_string()
}

/// `DIR/.` (cp idiom): copy the directory's contents rather than the directory itself.
fn is_contents(p: &str) -> bool {
    p == "." || p.ends_with("/.") || p.ends_with("\\.")
}

fn remote_parent(p: &str) -> String {
    let t = p.trim_end_matches('/');
    match t.rfind('/') {
        Some(0) => "/".into(),
        Some(i) => t[..i].to_string(),
        None => ".".into(),
    }
}

#[cfg(unix)]
fn local_mode(m: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(m.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn local_mode(_: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Glob match: `*` and `?` stay within one path segment, `**` spans segments.
pub(crate) fn glob_match(pat: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => {
                let rest = if p.get(2) == Some(&b'/') { &p[3..] } else { &p[2..] };
                (0..=t.len()).any(|i| go(rest, &t[i..]))
            }
            Some(b'*') => {
                let rest = &p[1..];
                for i in 0..=t.len() {
                    if go(rest, &t[i..]) {
                        return true;
                    }
                    if i < t.len() && t[i] == b'/' {
                        break;
                    }
                }
                false
            }
            Some(b'?') => !t.is_empty() && t[0] != b'/' && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(pat.as_bytes(), text.as_bytes())
}

/// Whether `rel` (path inside a copied directory, `/`-separated) is excluded. Patterns with a
/// `/` match the whole relative path, others match any single name; a trailing `/` matches
/// directories only.
pub(crate) fn excluded(patterns: &[String], rel: &str, is_dir: bool) -> bool {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    patterns.iter().any(|p| {
        let (p, dir_only) = match p.strip_suffix('/') {
            Some(x) => (x, true),
            None => (p.as_str(), false),
        };
        if dir_only && !is_dir {
            return false;
        }
        match p.strip_prefix('/') {
            Some(anchored) => glob_match(anchored, rel),
            None if p.contains('/') => glob_match(p, rel) || glob_match(&format!("**/{p}"), rel),
            None => glob_match(p, name),
        }
    })
}

/// Remote paths: `~/x` → `x` (SFTP paths are relative to the login directory).
fn norm(e: &Endpoint) -> String {
    match &e.host {
        Some(_) if e.path.is_empty() => ".".into(),
        Some(_) => files::sftp_path(&e.path),
        None => e.path.clone(),
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Change {
    New,
    Replace,
    Unchanged,
    /// Same size, contents to be compared (`--checksum`).
    Compare,
}

/// A file to copy; `rel` is its path inside the copied directory (for the tree checksum).
struct FileJob<'a> {
    sfs: Fs<'a>,
    src: String,
    dst: String,
    meta: Meta,
    dst_meta: Meta,
    change: Change,
    rel: String,
}

struct LinkJob {
    dst: String,
    target: String,
    existing: bool,
}

struct DirJob {
    dst: String,
    mode: Option<u32>,
    mtime: Option<u32>,
    existed: bool,
}

#[derive(Default)]
struct Plan<'a> {
    dirs: Vec<DirJob>,
    files: Vec<FileJob<'a>>,
    links: Vec<LinkJob>,
    delete: Vec<(String, bool)>,
    skipped: Vec<String>,
}

/// Copy-wide settings for planning.
struct Ctx<'o> {
    opts: &'o CopyOptions,
    /// Destination writes are staged (sudo): the destination listing is meaningless.
    staged_dst: bool,
}

/// Collect directories (parents first), files and links under `src`.
#[allow(clippy::too_many_arguments)]
fn plan_tree<'a, 'p>(
    cx: &'p Ctx<'p>,
    sfs: Fs<'a>,
    src: String,
    dfs: Fs<'a>,
    dst: String,
    rel: String,
    meta: Meta,
    dst_meta: Meta,
    plan: &'p mut Plan<'a>,
    chain: Vec<String>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send + 'p>>
where
    'a: 'p,
{
    Box::pin(async move {
        match meta.kind {
            Kind::File => {
                let change = if dst_meta.kind == Kind::Missing || cx.staged_dst {
                    Change::New
                } else if dst_meta.kind != Kind::File || cx.opts.all || dst_meta.size != meta.size {
                    Change::Replace
                } else if cx.opts.checksum {
                    Change::Compare
                } else if meta.mtime.is_some() && meta.mtime == dst_meta.mtime {
                    Change::Unchanged
                } else {
                    Change::Replace
                };
                plan.files.push(FileJob {
                    sfs,
                    src,
                    dst,
                    meta,
                    dst_meta,
                    change,
                    rel,
                });
            }
            Kind::Link => {
                let target = sfs.read_link(&src).await?;
                plan.links.push(LinkJob {
                    dst,
                    target,
                    existing: dst_meta.kind != Kind::Missing,
                });
            }
            Kind::Dir => {
                // Symlinked directories can loop: compare real paths along the current chain.
                let real = if meta.link || chain.is_empty() {
                    sfs.canon(&src).await
                } else {
                    sfs.join(chain.last().map(String::as_str).unwrap_or(""), &base_name(&src))
                };
                if chain.contains(&real) || chain.len() > MAX_DEPTH {
                    plan.skipped.push(format!("{} (symlink loop)", sfs.show(&src)));
                    return Ok(());
                }
                let mut chain = chain;
                chain.push(real);
                let existed = dst_meta.kind == Kind::Dir;
                plan.dirs.push(DirJob {
                    dst: dst.clone(),
                    mode: meta.mode,
                    mtime: meta.mtime,
                    existed,
                });
                // One listing of the destination directory gives every child's metadata.
                let theirs: HashMap<String, Meta> = if existed && !cx.staged_dst {
                    dfs.list(&dst, false).await?.into_iter().collect()
                } else {
                    HashMap::new()
                };
                let mine = sfs.list(&src, !cx.opts.links).await?;
                let names: BTreeSet<&str> = mine.iter().map(|(n, _)| n.as_str()).collect();
                if cx.opts.delete && existed {
                    for (name, m) in &theirs {
                        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                        if !names.contains(name.as_str()) && !name.ends_with(PART) && !excluded(&cx.opts.exclude, &r, m.kind == Kind::Dir) {
                            plan.delete.push((dfs.join(&dst, name), m.kind == Kind::Dir));
                        }
                    }
                }
                for (name, m) in mine {
                    let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
                    if excluded(&cx.opts.exclude, &r, m.kind == Kind::Dir) {
                        continue;
                    }
                    let child_src = sfs.join(&src, &name);
                    let child_dst = dfs.join(&dst, &name);
                    match m.kind {
                        Kind::Missing | Kind::Other => {
                            let why = if m.kind == Kind::Missing {
                                "broken symlink"
                            } else {
                                "special file"
                            };
                            plan.skipped.push(format!("{} ({why})", sfs.show(&child_src)));
                            continue;
                        }
                        _ => {}
                    }
                    let mut dm = theirs.get(&name).copied().unwrap_or_else(Meta::missing);
                    // A symlink at the destination is written through (like cp), unless links
                    // are copied as links.
                    if dm.kind == Kind::Link && !cx.opts.links && m.kind == Kind::File {
                        let target = dfs.canon(&child_dst).await;
                        dm = dfs.stat(&target).await?;
                        plan_tree(cx, sfs, child_src, dfs, target, r, m, dm, plan, chain.clone()).await?;
                        continue;
                    }
                    plan_tree(cx, sfs, child_src, dfs, child_dst, r, m, dm, plan, chain.clone()).await?;
                }
            }
            Kind::Missing | Kind::Other => plan.skipped.push(sfs.show(&src)),
        }
        Ok(())
    })
}

struct FileDone {
    bytes: u64,
    /// sha256 of the whole source when it was streamed from the start.
    sha: Option<String>,
    resumed: bool,
    warning: Option<String>,
}

/// Mode of the written file: `--mode`, else the replaced file's, else the source's; `#!` files
/// from sources without modes (Windows) become 755.
fn target_mode(mode: Option<u32>, job: &FileJob, shebang: bool) -> Option<u32> {
    mode.or((job.dst_meta.kind == Kind::File).then_some(job.dst_meta.mode).flatten())
        .or(job.meta.mode)
        .or((shebang && matches!(job.sfs, Fs::Local)).then_some(0o755))
        .map(|m| m & 0o7777)
}

async fn copy_file(job: &FileJob<'_>, dfs: Fs<'_>, mode: Option<u32>) -> Result<FileDone> {
    let (sfs, src, dst) = (job.sfs, job.src.as_str(), job.dst.as_str());
    let part = dfs.part_of(dst);
    // Resume a large interrupted copy when the partial file is a verified prefix.
    let mut offset = 0u64;
    if job.meta.size >= RESUME_MIN {
        let pm = dfs.lstat(&part).await?;
        if pm.kind == Kind::File && pm.size > 0 && pm.size < job.meta.size {
            let (a, b) = tokio::join!(sfs.prefix_sha(src, pm.size), dfs.prefix_sha(&part, pm.size));
            if let (Ok(a), Ok(b)) = (a, b)
                && !a.is_empty()
                && a == b
            {
                offset = pm.size;
            }
        }
    }
    let mut r = sfs.reader(src, offset).await?;
    let mut hasher = Sha256::new();
    let mut first = vec![0u8; CHUNK];
    // Read the first chunk before creating the target: a `#!` line decides its mode.
    let n = r.read(&mut first).await.map_err(|e| sfs.denied("read", src, e))?;
    first.truncate(n);
    let shebang = offset == 0 && first.starts_with(b"#!");
    let line = first.split(|b| *b == b'\n').next().unwrap_or_default();
    let warning = (shebang && line.ends_with(b"\r") && matches!(dfs, Fs::Remote(..))).then(|| {
        format!(
            "{} has CRLF line endings: its #! line fails on Unix (\"required file not found\"); convert it to LF and copy again",
            sfs.show(src)
        )
    });
    let create_mode = target_mode(mode, job, shebang);
    let result = async {
        let mut w = dfs.writer(&part, create_mode, offset).await?;
        // Reading and writing overlap: the reader runs ahead by up to 8 chunks.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
        let read_side = async {
            let mut total = 0u64;
            let mut chunk = first;
            loop {
                if chunk.is_empty() {
                    break;
                }
                total += chunk.len() as u64;
                if offset == 0 {
                    hasher.update(&chunk);
                }
                if tx.send(chunk).await.is_err() {
                    break;
                }
                let mut buf = vec![0u8; CHUNK];
                let k = r.read(&mut buf).await.map_err(|e| sfs.denied("read", src, e))?;
                buf.truncate(k);
                chunk = buf;
            }
            drop(tx);
            Ok::<u64, Error>(total)
        };
        let write_side = async {
            while let Some(b) = rx.recv().await {
                w.write_all(&b).await.map_err(|e| dfs.denied("write", dst, e))?;
            }
            w.flush().await.map_err(|e| dfs.denied("write", dst, e))?;
            w.shutdown().await.map_err(|e| dfs.denied("close", dst, e))?;
            Ok::<(), Error>(())
        };
        let (total, written) = tokio::join!(read_side, write_side);
        written?;
        let total = total?;
        drop(w);
        if let Some(m) = mode {
            dfs.chmod(&part, m & 0o7777).await?;
        }
        if let Some(t) = job.meta.mtime {
            dfs.set_mtime(&part, racy_safe_mtime(t, now_secs())).await;
        }
        dfs.rename_over(&part, dst).await?;
        Ok::<u64, Error>(total)
    }
    .await;
    match result {
        Ok(total) => Ok(FileDone {
            bytes: total,
            sha: (offset == 0).then(|| hex::encode(hasher.finalize())),
            resumed: offset > 0,
            warning,
        }),
        Err(e) => {
            // Keep a large partial file for the next run; small ones are just removed.
            if job.meta.size < RESUME_MIN {
                dfs.remove_file(&part).await;
            }
            Err(e)
        }
    }
}

/// Copy files within one host on the host itself: `cp` into a part file, then `mv` over the
/// target (keeping a replaced file's mode). Returns dst -> sha256 of the copied files.
async fn copy_same_host(host: &Remote, jobs: &[&FileJob<'_>], mode: Option<u32>, total_bytes: u64) -> Result<HashMap<String, String>> {
    let fs = Fs::Remote(host);
    let mut input = String::new();
    for j in jobs {
        if j.src.contains('\n') || j.dst.contains('\n') {
            return Err(Error::usage(format!(
                "{}: file names with newlines are not supported",
                fs.show(&j.src)
            )));
        }
        input.push_str(&format!("{}\n{}\n{}\n", j.src, j.dst, fs.part_of(&j.dst)));
    }
    let chmod = mode
        .map(|m| format!("chmod {m:o} \"$t\" || {{ echo \"ERR $d\"; rm -f \"$t\"; continue; }}; "))
        .unwrap_or_default();
    let script = format!(
        "{SH_HASH}while IFS= read -r s && IFS= read -r d && IFS= read -r t; do \
           if [ -f \"$d\" ]; then cp -p -- \"$d\" \"$t\" && cat -- \"$s\" > \"$t\" && touch -r \"$s\" \"$t\"; else cp -p -- \"$s\" \"$t\"; fi \
             || {{ echo \"ERR $d\"; rm -f \"$t\"; continue; }}; {chmod}\
           mv -f -- \"$t\" \"$d\" && printf 'OK %s %s\\n' \"$(h \"$d\")\" \"$d\" || echo \"ERR $d\"; \
         done"
    );
    let timeout = Duration::from_secs(600 + total_bytes / (20 * 1024 * 1024));
    let out = run_raw(&host.conn, &sh(&script), input.as_bytes(), false, timeout).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut done = HashMap::new();
    for l in text.lines() {
        if let Some(rest) = l.strip_prefix("OK ")
            && let Some((h, d)) = rest.split_once(' ')
        {
            done.insert(d.to_string(), h.to_string());
        } else if let Some(d) = l.strip_prefix("ERR ") {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(Error::remote(format!("copy to {} failed: {}", fs.show(d), err.trim())));
        }
    }
    if out.timed_out || done.len() < jobs.len() {
        return Err(Error::remote(format!(
            "copy on {} did not finish ({} of {} files): {}",
            host.alias,
            done.len(),
            jobs.len(),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(done)
}

/// sha256 over `sha256sum`-style lines ("<sha>  ./<path>\n") sorted by path, i.e. what
/// `cd DIR && find . -type f | LC_ALL=C sort | xargs -d '\n' sha256sum | sha256sum` prints.
fn tree_sha(files: &mut [(String, String)]) -> String {
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut h = Sha256::new();
    for (rel, sha) in files.iter() {
        h.update(format!("{sha}  ./{rel}\n").as_bytes());
    }
    hex::encode(h.finalize())
}

/// `~/.xssh/tmp/cp-<id>` on a host (absolute, 0700), created once per copy. Stale stages of
/// interrupted copies (over a day old) are removed first.
async fn stage_dir(host: &Remote, stages: &mut Vec<(String, String)>) -> Result<String> {
    if let Some((_, d)) = stages.iter().find(|(x, _)| *x == host.alias) {
        return Ok(d.clone());
    }
    let script = format!(
        "umask 077; b=\"$HOME/.xssh/tmp\"; mkdir -p \"$b\" && chmod 700 \"$HOME/.xssh\" \"$b\" || exit 1; \
         find \"$b\" -maxdepth 1 -name 'cp-*' -mmin +1440 -exec rm -rf {{}} + 2>/dev/null; \
         d=\"$b/cp-{}\"; mkdir -m 700 \"$d\" && echo \"$d\"",
        short_id(8)
    );
    let out = run_raw(&host.conn, &sh(&script), b"", false, Duration::from_secs(60)).await?;
    let dir = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if out.exit_code != Some(0) || dir.is_empty() {
        return Err(Error::remote(format!(
            "create a private staging directory on {}: {}",
            host.alias,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    stages.push((host.alias.clone(), dir.clone()));
    Ok(dir)
}

/// Run a script as root on a host; returns stdout.
async fn sudo_sh(io: &HostIo, script: &str, stdin: &[u8], what: &str, timeout: Duration) -> Result<String> {
    let out = files::run_maybe_sudo_t(&io.conn, script, stdin, &io.sudo_ctx(), timeout).await?;
    if out.exit_code != Some(0) {
        let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let mut e = Error::remote(format!("{what}: {msg}"));
        if msg.to_ascii_lowercase().contains("no such file") {
            e.code = ErrorCode::NotFound;
        } else if msg.contains("password is required") || msg.contains("incorrect password") || msg.contains("authentication failed") {
            e = e.hint("store the sudo password with `xssh host set-password <host> --sudo`");
        }
        return Err(e);
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Install staged files as root: each record is `F|L|D`, stage path, final path. Identical
/// files are left alone, replaced files keep owner and mode, all go through a part file + mv.
fn install_script(mode: Option<u32>) -> String {
    let chmod = mode.map(|m| format!("chmod {m:o} \"$t\"; ")).unwrap_or_default();
    let chmod_same = mode.map(|m| format!("chmod {m:o} \"$d\"; ")).unwrap_or_default();
    format!(
        "while IFS= read -r k && IFS= read -r s && IFS= read -r d; do case $k in \
           D) mkdir -p -- \"$d\" || echo \"ERR $d\" ;; \
           L) mkdir -p -- \"$(dirname -- \"$d\")\" && rm -f -- \"$d\" && cp -P -- \"$s\" \"$d\" && echo \"NEW $d\" || echo \"ERR $d\" ;; \
           F) mkdir -p -- \"$(dirname -- \"$d\")\" || {{ echo \"ERR $d\"; continue; }}; \
              if [ -f \"$d\" ] && cmp -s -- \"$s\" \"$d\"; then {chmod_same}echo \"SAME $d\"; continue; fi; \
              t=\"$d{PART}\"; \
              if [ -f \"$d\" ]; then cp -p -- \"$d\" \"$t\" && cat -- \"$s\" > \"$t\"; else cp -- \"$s\" \"$t\"; fi \
                || {{ echo \"ERR $d\"; rm -f -- \"$t\"; continue; }}; \
              touch -r \"$s\" \"$t\" 2>/dev/null; {chmod}mv -f -- \"$t\" \"$d\" && echo \"NEW $d\" || echo \"ERR $d\" ;; \
         esac; done"
    )
}

pub async fn copy(
    hosts: &HashMap<String, HostIo>,
    srcs: &[Endpoint],
    dst: &Endpoint,
    mode: Option<u32>,
    opts: &CopyOptions,
) -> Result<CopyResult> {
    let start = std::time::Instant::now();
    if srcs.is_empty() {
        return Err(Error::usage("no source given"));
    }
    if dst.host.is_none() && srcs.iter().all(|s| s.host.is_none()) {
        return Err(Error::usage("both sides are local").hint("one side must be HOST:PATH (saved hosts: `xssh host list`)"));
    }
    let sudo_dst_host = dst.host.as_deref().filter(|h| hosts[*h].sudo);
    if opts.delete && sudo_dst_host.is_some() {
        return Err(Error::usage("--delete cannot be combined with --sudo on the destination")
            .hint("delete extra files with `xssh exec HOST --sudo -- rm ...` after checking them"));
    }
    let mut remotes: HashMap<&str, Remote> = HashMap::new();
    for (alias, io) in hosts {
        remotes.insert(alias.as_str(), Remote::open(alias, io.conn.clone()).await?);
    }
    let fs_of = |e: &Endpoint| -> Fs<'_> {
        match &e.host {
            Some(h) => Fs::Remote(&remotes[h.as_str()]),
            None => Fs::Local,
        }
    };

    // Paths handed to root shells: `~`/relative mean the login user's home, not root's.
    let mut homes: HashMap<&str, String> = HashMap::new();
    for (h, io) in hosts {
        if io.sudo {
            let home = remotes[h.as_str()]
                .sftp
                .canonicalize(".".to_string())
                .await
                .map_err(|e| Error::remote(format!("resolve home dir on {h}: {e}")))?;
            homes.insert(h.as_str(), home);
        }
    }
    let root_path = |e: &Endpoint| -> String {
        let home = e.host.as_deref().and_then(|h| homes.get(h)).map(String::as_str).unwrap_or("~");
        let p = e.path.as_str();
        if p.starts_with('/') {
            p.to_string()
        } else if p == "~" || p.is_empty() || p == "." {
            home.to_string()
        } else {
            files::join_remote(home, p.strip_prefix("~/").unwrap_or(p))
        }
    };

    // Stage root-only sources / destinations on hosts that use sudo.
    let mut stages: Vec<(String, String)> = vec![];
    let res = async {
        // (fs, path to read, name at target, copy the directory's contents only)
        let mut plan_srcs: Vec<(Fs, String, String, bool)> = vec![];
        for (i, e) in srcs.iter().enumerate() {
            let fs = fs_of(e);
            let path = norm(e);
            let contents = is_contents(&e.path);
            let name = fs.name(&path).await;
            let path = match e.host.as_deref() {
                Some(h) if hosts[h].sudo => {
                    let r = &remotes[h];
                    let stage = stage_dir(r, &mut stages).await?;
                    let dir = format!("{stage}/s{i}");
                    fs.mkdir_p(&dir).await?;
                    let staged = format!("{dir}/{name}");
                    let m = r
                        .sftp
                        .metadata(dir.clone())
                        .await
                        .map_err(|e| Error::remote(format!("stat {dir}: {e}")))?;
                    let deref = if opts.links { "-RPp" } else { "-RHp" };
                    sudo_sh(
                        &hosts[h],
                        &format!(
                            "cp {deref} -- {} {} && chown -R {}:{} {}",
                            shq_path(&root_path(e)),
                            shq_path(&staged),
                            m.uid.unwrap_or(0),
                            m.gid.unwrap_or(0),
                            shq_path(&staged)
                        ),
                        b"",
                        &format!("read {}", e.show()),
                        STAGE_TIMEOUT,
                    )
                    .await?;
                    staged
                }
                _ => path,
            };
            plan_srcs.push((fs, path, name, contents));
        }

        let dfs = fs_of(dst);
        let dpath = norm(dst);
        let trailing = dst.path.ends_with('/') || dst.path.ends_with('\\');
        let droot = root_path(dst);
        let plain = dfs.stat(&dpath).await;
        let dkind = match sudo_dst_host {
            Some(h) if plain.is_err() => {
                let out = sudo_sh(
                    &hosts[h],
                    &format!(
                        "if [ -d {p} ]; then echo D; elif [ -e {p} ]; then echo F; else echo M; fi",
                        p = shq_path(&droot)
                    ),
                    b"",
                    &format!("stat {}", dst.show()),
                    Duration::from_secs(60),
                )
                .await?;
                match out.trim() {
                    "D" => Kind::Dir,
                    "F" => Kind::File,
                    _ => Kind::Missing,
                }
            }
            _ => plain?.kind,
        };
        let into_dir = dkind == Kind::Dir || trailing || plan_srcs.len() > 1;
        if into_dir && !matches!(dkind, Kind::Dir | Kind::Missing) {
            return Err(Error::usage(format!("{} is not a directory", dst.show())));
        }
        // With sudo the tree is first written to a stage dir, then installed as root.
        let (wpath, final_name) = match sudo_dst_host {
            Some(h) => {
                let stage = format!("{}/d", stage_dir(&remotes[h], &mut stages).await?);
                if into_dir {
                    (stage, None)
                } else {
                    let n = dfs.name(&dpath).await;
                    (stage.clone(), Some(n))
                }
            }
            None => (dpath.clone(), None),
        };
        if !opts.dry_run || sudo_dst_host.is_some() {
            if into_dir || sudo_dst_host.is_some() {
                dfs.mkdir_p(&wpath).await?;
            } else {
                let parent = dfs.parent(&wpath);
                if !parent.is_empty() {
                    dfs.mkdir_p(&parent).await?;
                }
            }
        }

        let cx = Ctx {
            opts,
            staged_dst: sudo_dst_host.is_some(),
        };
        let mut targets = vec![];
        let mut plan = Plan::default();
        let mut dir_sources = 0;
        for (sfs, spath, name, contents) in plan_srcs.iter() {
            let smeta = sfs.stat(spath).await?;
            if smeta.kind == Kind::Missing {
                return Err(Error::not_found(format!("{}: no such file or directory", sfs.show(spath))));
            }
            let dshown = if sudo_dst_host.is_some() { &droot } else { &dpath };
            let (target, shown) = if into_dir && *contents && smeta.kind == Kind::Dir {
                (wpath.clone(), dshown.clone())
            } else if into_dir {
                (dfs.join(&wpath, name), dfs.join(dshown, name))
            } else if let Some(n) = &final_name {
                (dfs.join(&wpath, n), droot.clone())
            } else {
                (wpath.clone(), dpath.clone())
            };
            if !into_dir && smeta.kind == Kind::Dir && dkind == Kind::File {
                return Err(Error::usage(format!("cannot copy a directory onto file {}", dst.show())));
            }
            if sfs.same_host(&dfs) {
                let (a, b) = (sfs.canon(spath).await, dfs.canon_new(&target).await);
                if b == a || b.starts_with(&format!("{}/", a.trim_end_matches('/'))) {
                    return Err(Error::usage(format!("cannot copy {} into itself", sfs.show(spath))));
                }
            }
            let tmeta = if sudo_dst_host.is_some() {
                Meta::missing()
            } else {
                dfs.lstat(&target).await?
            };
            let tmeta = if tmeta.kind == Kind::Link && smeta.kind == Kind::File && !opts.links {
                dfs.stat(&target).await?
            } else {
                tmeta
            };
            plan_tree(
                &cx,
                *sfs,
                spath.clone(),
                dfs,
                target.clone(),
                String::new(),
                smeta,
                tmeta,
                &mut plan,
                vec![],
            )
            .await?;
            if smeta.kind == Kind::Dir {
                dir_sources += 1;
            }
            let shown = match (&dfs, sudo_dst_host) {
                (Fs::Remote(r), None) => r.sftp.canonicalize(shown.clone()).await.unwrap_or(shown),
                _ => shown,
            };
            targets.push(dfs.show(&shown));
        }

        // --checksum: same-size files are compared by content.
        let mut src_hash: HashMap<String, String> = HashMap::new();
        let compare: Vec<usize> = (0..plan.files.len()).filter(|i| plan.files[*i].change == Change::Compare).collect();
        if !compare.is_empty() {
            let dh = dfs
                .hash_many(&compare.iter().map(|i| plan.files[*i].dst.clone()).collect::<Vec<_>>())
                .await?;
            let shash = hash_by_fs(&plan.files, &compare).await?;
            for i in compare {
                let f = &mut plan.files[i];
                let s = shash.get(&(fs_key(&f.sfs), f.src.clone()));
                f.change = if s.is_some() && s == dh.get(&f.dst) {
                    Change::Unchanged
                } else {
                    Change::Replace
                };
                if let Some(s) = s {
                    src_hash.insert(f.dst.clone(), s.clone());
                }
            }
        }

        let mut r = CopyResult {
            targets,
            dry_run: opts.dry_run,
            ..Default::default()
        };
        let mut changed: Vec<String> = vec![];
        r.skipped = plan.skipped.len();
        r.skipped_paths = plan.skipped.iter().take(LIST_MAX).cloned().collect();
        r.dirs = plan.dirs.iter().filter(|d| !d.existed).count();
        for (p, _) in &plan.delete {
            changed.push(format!("- {}", dfs.show(p)));
        }
        r.deleted = plan.delete.len();
        let to_copy: Vec<usize> = (0..plan.files.len())
            .filter(|i| plan.files[*i].change != Change::Unchanged)
            .collect();
        r.unchanged = plan.files.len() - to_copy.len();
        if opts.dry_run {
            for i in &to_copy {
                let f = &plan.files[*i];
                changed.push(format!("{} {}", if f.change == Change::New { "+" } else { "~" }, dfs.show(&f.dst)));
                r.files += 1;
                r.bytes += f.meta.size;
            }
            r.links = plan.links.len();
            finish_lists(&mut r, changed);
            return Ok(r);
        }

        // Deletions first (like rsync), then directories, files, links; directory modes and
        // mtimes last so a read-only directory does not block its own contents.
        if !plan.delete.is_empty() {
            delete_paths(dfs, &plan.delete).await?;
        }
        for d in &plan.dirs {
            dfs.mkdir(&d.dst).await?;
        }
        let mut hashes: HashMap<String, String> = HashMap::new();
        // Same-host copies run on the host in one batch.
        let (streamed, on_host): (Vec<usize>, Vec<usize>) = to_copy.iter().partition(|i| !plan.files[**i].sfs.same_host(&dfs));
        if !on_host.is_empty()
            && let Fs::Remote(host) = dfs
        {
            let jobs: Vec<&FileJob> = on_host.iter().map(|i| &plan.files[*i]).collect();
            let total: u64 = jobs.iter().map(|j| j.meta.size).sum();
            let done = copy_same_host(host, &jobs, mode, total).await?;
            for j in jobs {
                r.files += 1;
                r.bytes += j.meta.size;
                changed.push(format!("{} {}", if j.change == Change::New { "+" } else { "~" }, dfs.show(&j.dst)));
                if let Some(h) = done.get(&j.dst) {
                    hashes.insert(j.dst.clone(), h.clone());
                }
            }
        }
        {
            use futures::StreamExt;
            type Job<'f> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<FileDone>> + Send + 'f>>;
            let jobs: Vec<Job<'_>> = streamed
                .iter()
                .map(|i| Box::pin(copy_file(&plan.files[*i], dfs, mode)) as Job<'_>)
                .collect();
            let results: Vec<Result<FileDone>> = futures::stream::iter(jobs).buffered(PARALLEL_FILES).collect().await;
            let mut first_err = None;
            for (i, res) in streamed.iter().zip(results) {
                let f = &plan.files[*i];
                match res {
                    Ok(d) => {
                        r.files += 1;
                        r.bytes += d.bytes;
                        r.resumed += usize::from(d.resumed);
                        r.warnings.extend(d.warning);
                        if let Some(h) = d.sha {
                            hashes.insert(f.dst.clone(), h);
                        }
                        changed.push(format!("{} {}", if f.change == Change::New { "+" } else { "~" }, dfs.show(&f.dst)));
                    }
                    Err(e) => {
                        first_err.get_or_insert(e);
                    }
                }
            }
            if let Some(mut e) = first_err {
                if r.files > 0 {
                    e.message = format!(
                        "{} ({} other file(s) were copied; rerun to finish: unchanged files are skipped)",
                        e.message, r.files
                    );
                }
                return Err(e);
            }
        }
        for l in &plan.links {
            dfs.symlink(&l.dst, &l.target).await?;
            r.links += 1;
            changed.push(format!(
                "{} {} -> {}",
                if l.existing { "~" } else { "+" },
                dfs.show(&l.dst),
                l.target
            ));
        }
        // `--mode` also applies to files that were already up to date.
        if let Some(m) = mode {
            for f in plan
                .files
                .iter()
                .filter(|f| f.change == Change::Unchanged && f.dst_meta.mode != Some(m))
            {
                dfs.chmod(&f.dst, m).await?;
            }
        }
        for d in plan.dirs.iter().rev() {
            if !d.existed
                && let Some(m) = mode.is_none().then_some(d.mode).flatten()
            {
                let _ = dfs.chmod(&d.dst, m).await;
            }
            if let Some(t) = d.mtime {
                dfs.set_mtime(&d.dst, t).await;
            }
        }

        // Hashes the result needs that streaming did not provide (resumed, unchanged files).
        let single_file = plan.files.len() == 1 && dir_sources == 0;
        let tree = dir_sources == 1 && plan_srcs.len() == 1;
        if tree || single_file || opts.verify {
            let missing: Vec<usize> = (0..plan.files.len())
                .filter(|i| !hashes.contains_key(&plan.files[*i].dst) && !src_hash.contains_key(&plan.files[*i].dst))
                .collect();
            if !missing.is_empty() {
                let got = hash_by_fs(&plan.files, &missing).await?;
                for i in missing {
                    let f = &plan.files[i];
                    if let Some(h) = got.get(&(fs_key(&f.sfs), f.src.clone())) {
                        src_hash.insert(f.dst.clone(), h.clone());
                    }
                }
            }
        }
        let hash_of = |dst: &str| hashes.get(dst).or_else(|| src_hash.get(dst)).cloned();
        if opts.verify && !plan.files.is_empty() && sudo_dst_host.is_none() {
            let dsts: Vec<String> = plan.files.iter().map(|f| f.dst.clone()).collect();
            let got = dfs.hash_many(&dsts).await?;
            let bad: Vec<String> = plan
                .files
                .iter()
                .filter(|f| hash_of(&f.dst).is_none() || got.get(&f.dst) != hash_of(&f.dst).as_ref())
                .map(|f| dfs.show(&f.dst))
                .collect();
            if !bad.is_empty() {
                return Err(Error::remote(format!(
                    "verification failed for {} file(s): {}",
                    bad.len(),
                    bad.iter().take(10).cloned().collect::<Vec<_>>().join(", ")
                ))
                .hint("copy again with -c (compares content, not size+mtime); if it persists, check the destination disk (df -h) and filesystem"));
            }
            r.verified = true;
        }
        if single_file {
            r.sha256 = hash_of(&plan.files[0].dst);
        }
        if tree {
            let mut all: Vec<(String, String)> = vec![];
            let complete = plan.files.iter().all(|f| match hash_of(&f.dst) {
                Some(h) => {
                    all.push((f.rel.clone(), h));
                    true
                }
                None => false,
            });
            if complete {
                r.tree_sha256 = Some(tree_sha(&mut all));
            }
        }

        if let Some(h) = sudo_dst_host {
            let mut input = String::new();
            let stage_to_final = |p: &str| -> String {
                let rest = p.strip_prefix(&wpath).unwrap_or(p).trim_start_matches('/');
                match &final_name {
                    Some(_) => droot.clone(),
                    None if rest.is_empty() => droot.clone(),
                    None => files::join_remote(&droot, rest),
                }
            };
            for d in &plan.dirs {
                input.push_str(&format!("D\n{}\n{}\n", d.dst, stage_to_final(&d.dst)));
            }
            for f in &plan.files {
                input.push_str(&format!("F\n{}\n{}\n", f.dst, stage_to_final(&f.dst)));
            }
            for l in &plan.links {
                input.push_str(&format!("L\n{}\n{}\n", l.dst, stage_to_final(&l.dst)));
            }
            let out = sudo_sh(
                &hosts[h],
                &install_script(mode),
                input.as_bytes(),
                &format!("write {}", dst.show()),
                STAGE_TIMEOUT,
            )
            .await?;
            // Report what actually changed on the destination.
            changed.clear();
            let (mut same, mut errs) = (0usize, vec![]);
            for l in out.lines() {
                if let Some(p) = l.strip_prefix("NEW ") {
                    changed.push(format!("+ {}", dfs.show(p)));
                } else if l.starts_with("SAME ") {
                    same += 1;
                } else if let Some(p) = l.strip_prefix("ERR ") {
                    errs.push(p.to_string());
                }
            }
            if !errs.is_empty() {
                return Err(Error::remote(format!("installing as root failed for: {}", errs.join(", "))));
            }
            r.unchanged += same;
            r.files = r.files.saturating_sub(same);
        }
        finish_lists(&mut r, changed);
        Ok(r)
    }
    .await;

    // Staged copies may include root-only files: remove them now, also after a failure.
    for (h, dir) in &stages {
        let io = &hosts[h];
        let rm = format!("rm -rf -- {}", shq(dir));
        let ok = run_raw(&io.conn, &sh(&rm), b"", false, Duration::from_secs(600))
            .await
            .is_ok_and(|o| o.exit_code == Some(0));
        if !ok && io.sudo {
            let _ = files::run_maybe_sudo_t(&io.conn, &rm, b"", &io.sudo_ctx(), Duration::from_secs(600)).await;
        }
    }
    for r in remotes.values() {
        r.close().await;
    }
    let mut r = res?;
    r.duration_ms = start.elapsed().as_millis() as u64;
    Ok(r)
}

fn finish_lists(r: &mut CopyResult, changed: Vec<String>) {
    r.changed_more = changed.len().saturating_sub(LIST_MAX);
    r.changed = changed.into_iter().take(LIST_MAX).collect();
}

/// Key identifying a file system in hash maps.
fn fs_key(f: &Fs) -> String {
    match f {
        Fs::Local => String::new(),
        Fs::Remote(r) => r.alias.clone(),
    }
}

/// Source hashes of the given jobs, batched per file system.
async fn hash_by_fs(files: &[FileJob<'_>], idx: &[usize]) -> Result<HashMap<(String, String), String>> {
    let mut groups: HashMap<String, (Fs, Vec<String>)> = HashMap::new();
    for i in idx {
        let f = &files[*i];
        groups.entry(fs_key(&f.sfs)).or_insert((f.sfs, vec![])).1.push(f.src.clone());
    }
    let mut out = HashMap::new();
    for (k, (fs, paths)) in groups {
        for (p, h) in fs.hash_many(&paths).await? {
            out.insert((k.clone(), p), h);
        }
    }
    Ok(out)
}

async fn delete_paths(dfs: Fs<'_>, list: &[(String, bool)]) -> Result<()> {
    match dfs {
        Fs::Local => {
            for (p, dir) in list {
                let r = if *dir {
                    tokio::fs::remove_dir_all(p).await
                } else {
                    tokio::fs::remove_file(p).await
                };
                r.map_err(|e| dfs.denied("delete", p, e))?;
            }
            Ok(())
        }
        Fs::Remote(r) => {
            let args: Vec<String> = list.iter().map(|(p, _)| shq(p)).collect();
            let out = run_raw(
                &r.conn,
                &sh(&format!("rm -rf -- {}", args.join(" "))),
                b"",
                false,
                Duration::from_secs(600),
            )
            .await?;
            if out.exit_code != Some(0) {
                return Err(Error::remote(format!(
                    "delete on {}: {}",
                    r.alias,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn racy_mtime_is_set_one_second_early() {
        // Modified long ago: kept as is, so the next run sees it unchanged.
        assert_eq!(racy_safe_mtime(1_000, 5_000), 1_000);
        // Modified in the last two seconds (or "in the future" on a skewed clock): it may change
        // again within the same second, so the copy must not look up to date.
        assert_eq!(racy_safe_mtime(4_999, 5_000), 4_998);
        assert_eq!(racy_safe_mtime(4_998, 5_000), 4_997);
        assert_eq!(racy_safe_mtime(5_003, 5_000), 5_002);
    }

    #[test]
    fn tree_checksum_matches_sha256sum_listing() {
        let mut f = vec![("b/c".to_string(), "22".to_string()), ("a".to_string(), "11".to_string())];
        let want = hex::encode(Sha256::digest(b"11  ./a\n22  ./b/c\n"));
        assert_eq!(tree_sha(&mut f), want);
    }

    #[test]
    fn contents_suffix() {
        assert!(is_contents("site/."));
        assert!(is_contents("C:\\x\\site\\."));
        assert!(!is_contents("site/"));
        assert!(!is_contents("site/.env"));
    }

    #[test]
    fn remote_parent_and_join() {
        assert_eq!(remote_parent("/etc/nginx/x.conf"), "/etc/nginx");
        assert_eq!(remote_parent("/etc/"), "/");
        assert_eq!(remote_parent("x.conf"), ".");
        assert_eq!(files::join_remote(".", "a"), "a");
        assert_eq!(files::join_remote("/tmp/", "a"), "/tmp/a");
    }

    #[test]
    fn remote_paths_normalize() {
        let e = |p: &str| Endpoint {
            host: Some("h".into()),
            path: p.into(),
        };
        assert_eq!(norm(&e("")), ".");
        assert_eq!(norm(&e("~")), ".");
        assert_eq!(norm(&e("~/x/y")), "x/y");
        assert_eq!(norm(&e("/etc/x")), "/etc/x");
    }

    #[test]
    fn globs() {
        assert!(glob_match("*.log", "a.log"));
        assert!(!glob_match("*.log", "d/a.log"));
        assert!(glob_match("**/*.log", "d/e/a.log"));
        assert!(glob_match("**/*.log", "a.log"));
        assert!(glob_match("node_modules", "node_modules"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
        let ex = vec![
            "*.pyc".to_string(),
            "build/".to_string(),
            "/dist".to_string(),
            "docs/*.md".to_string(),
        ];
        assert!(excluded(&ex, "x/y/z.pyc", false));
        assert!(excluded(&ex, "src/build", true));
        assert!(!excluded(&ex, "src/build", false), "trailing / matches directories only");
        assert!(excluded(&ex, "dist", true));
        assert!(!excluded(&ex, "src/dist", true), "leading / anchors at the copied directory");
        assert!(excluded(&ex, "docs/a.md", false));
        assert!(excluded(&ex, "sub/docs/a.md", false));
        assert!(!excluded(&ex, "docs/x/a.md", false));
    }

    #[test]
    fn part_names() {
        assert!(
            Fs::Local
                .part_of(if cfg!(windows) { "C:\\d\\a.txt" } else { "/d/a.txt" })
                .ends_with(".a.txt.xssh-part")
        );
        assert_eq!(remote_parent("a.txt"), ".");
    }

    #[test]
    fn install_script_shape() {
        let s = install_script(Some(0o640));
        assert!(s.contains("cmp -s"));
        assert!(s.contains("chmod 640 \"$t\""));
        assert!(s.contains("mv -f -- \"$t\" \"$d\""));
        assert!(s.contains("touch -r"));
    }

    #[test]
    fn modes() {
        let mk = |kind, mode| Meta {
            kind,
            mode,
            size: 1,
            mtime: None,
            link: false,
        };
        let job = FileJob {
            sfs: Fs::Local,
            src: "a".into(),
            dst: "b".into(),
            meta: mk(Kind::File, None),
            dst_meta: mk(Kind::File, Some(0o600)),
            change: Change::Replace,
            rel: String::new(),
        };
        assert_eq!(target_mode(None, &job, true), Some(0o600), "a replaced file keeps its mode");
        assert_eq!(target_mode(Some(0o644), &job, true), Some(0o644));
        let new = FileJob {
            dst_meta: Meta::missing(),
            ..job
        };
        assert_eq!(target_mode(None, &new, true), Some(0o755), "#! from Windows becomes executable");
        assert_eq!(target_mode(None, &new, false), None);
    }
}
