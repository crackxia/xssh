//! Remote file operations tailored for agents.
//!
//! `read`/`write`/`edit` go through exec (POSIX `sh` scripts) so they work with `sudo` and
//! without an SFTP subsystem; `ls`/`stat`/`upload`/`download` use SFTP.
//!
//! - `read` slices on the remote side (awk), so a line range of a multi-GB log costs only the
//!   bytes shown; it reports size, total lines, line endings, encoding and the file's sha256.
//! - `write` replaces the file atomically (temp file + rename) and can refuse to overwrite a file
//!   that changed since it was read (`expect_sha256`), which `edit` always does.

use crate::exec::{self, run_raw};
use crate::ssh::Conn;
use russh_sftp::client::SftpSession;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::text::{self, short_id, shq, shq_path};

const T: Duration = Duration::from_secs(120);
/// Whole-file operations (`edit`, `read_bytes`) fetch the entire file.
const MAX_WHOLE: usize = 32 * 1024 * 1024;
/// Largest `file write` payload (it travels through the daemon in one message).
pub const MAX_WRITE: usize = 64 * 1024 * 1024;
/// Bytes of one `read` slice.
const MAX_SLICE: usize = 8 * 1024 * 1024;
/// Characters of one line shown by `read`; the remote side cuts at 4x this.
const MAX_LINE: usize = 2000;
/// Files up to this size get a sha256 in `read` results (hashing is remote and cheap).
const HASH_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContent {
    pub path: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    /// Size of the whole file.
    pub bytes: u64,
    /// `cat -n` style numbered lines.
    pub content: String,
    /// sha256 of the whole file: pass it to `file write --expect-sha256` to write only if the
    /// file has not changed since.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// `crlf` or `mixed` when the shown lines end in CRLF (the CR is not shown); absent for LF.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eol: Option<String>,
    /// The file does not end with a newline.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub no_final_newline: bool,
    /// Encoding the content was decoded from when it is not UTF-8 (e.g. `gbk`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WriteResult {
    pub path: String,
    pub bytes: usize,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditResult {
    pub path: String,
    pub replacements: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<String>,
    /// Numbered lines around the first change, after editing.
    pub snippet: String,
    /// sha256 of the file after the edit (for a later `--expect-sha256`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sha256: String,
    /// How the match was made when it needed adapting (e.g. line endings converted to CRLF).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// `crlf` or `mixed` when the edited file does not use plain LF line endings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eol: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub size: u64,
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

pub struct SudoCtx<'a> {
    pub enabled: bool,
    pub password: Option<&'a str>,
}

/// Options of `write` beyond the content.
#[derive(Debug, Clone, Default)]
pub struct WriteOpts {
    pub mode: Option<String>,
    pub backup: bool,
    pub mkdirs: bool,
    /// Refuse to write when the current file's sha256 differs (or it does not exist).
    pub expect_sha256: Option<String>,
    /// Backups kept per file (0 = unlimited).
    pub backup_keep: usize,
    /// Backups older than this many days are deleted (0 = never).
    pub backup_days: u64,
}

/// Run a POSIX sh script whatever the login shell is (fish and csh cannot parse sh syntax).
pub(crate) fn sh(script: &str) -> String {
    format!("sh -c {}", shq(script))
}

/// Defines `hs` printing the sha256 of stdin and `h FILE` for a file (empty output when no tool
/// is available).
pub(crate) const SH_HASH: &str = "if command -v sha256sum >/dev/null 2>&1; then hs() { sha256sum | cut -d' ' -f1; }; \
     elif command -v shasum >/dev/null 2>&1; then hs() { shasum -a 256 | cut -d' ' -f1; }; \
     elif command -v sha256 >/dev/null 2>&1; then hs() { sha256 -q; }; \
     elif command -v openssl >/dev/null 2>&1; then hs() { openssl dgst -sha256 | sed 's/.*= *//'; }; \
     else hs() { cat >/dev/null; }; fi; h() { hs < \"$1\"; }; ";

pub(crate) async fn run_maybe_sudo(conn: &Conn, cmd: &str, stdin: &[u8], sudo: &SudoCtx<'_>) -> Result<exec::RawOutput> {
    run_maybe_sudo_t(conn, cmd, stdin, sudo, T).await
}

/// Run a sh script (as root with `sudo`); a timeout is an error, not an exit code.
pub(crate) async fn run_maybe_sudo_t(
    conn: &Conn,
    cmd: &str,
    stdin: &[u8],
    sudo: &SudoCtx<'_>,
    timeout: Duration,
) -> Result<exec::RawOutput> {
    let prepared = exec::prepare(&sh(cmd), None, &[], &[], sudo.enabled.then_some(sudo.password), stdin, &[]);
    let mut out = run_raw(conn, &prepared.command, &prepared.stdin, false, timeout).await?;
    if !prepared.redact.is_empty() {
        let e = text::redact(&String::from_utf8_lossy(&out.stderr), &prepared.redact);
        out.stderr = e.into_bytes();
    }
    if out.timed_out {
        return Err(Error::timeout(format!(
            "remote file operation timed out after {}s",
            timeout.as_secs()
        )));
    }
    Ok(out)
}

fn remote_err(what: &str, path: &str, out: &exec::RawOutput, sudo: bool) -> Error {
    let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
    let lower = msg.to_ascii_lowercase();
    let mut e = Error::remote(format!("{what} {path}: {msg}"));
    if lower.contains("no such file") {
        e.code = ErrorCode::NotFound;
    } else if lower.contains("permission denied") && !sudo {
        e = e.hint("retry with --sudo");
    } else if lower.contains("password is required") || lower.contains("authentication failed") {
        e = e.hint("store the sudo password with `xssh host set-password <host> --sudo`");
    }
    e
}

/// Checks shared by read scripts: exists, not a directory, readable.
fn check_readable(p: &str) -> String {
    format!(
        "p={p}; if [ ! -e \"$p\" ]; then cat -- \"$p\" >/dev/null; exit 1; fi; \
         if [ -d \"$p\" ]; then echo 'is a directory' >&2; exit 1; fi; \
         [ -r \"$p\" ] || {{ cat -- \"$p\" >/dev/null; exit 1; }}; "
    )
}

/// The whole file (up to `MAX_WHOLE`).
pub async fn read_bytes(conn: &Conn, path: &str, sudo: &SudoCtx<'_>) -> Result<Vec<u8>> {
    let cmd = format!(
        "{}s=$(wc -c < \"$p\" | tr -d ' '); if [ \"$s\" -gt {MAX_WHOLE} ]; then echo \"XSSH_TOO_BIG $s\" >&2; exit 4; fi; cat -- \"$p\"",
        check_readable(&shq_path(path))
    );
    let out = run_maybe_sudo(conn, &cmd, b"", sudo).await?;
    if out.exit_code == Some(4) {
        return Err(Error::usage(format!("{path} is larger than {} MB", MAX_WHOLE / 1024 / 1024))
            .hint("read it in slices with `xssh file read --offset/--limit`, edit it with `xssh exec` (sed) or copy it with `xssh cp`"));
    }
    if out.exit_code != Some(0) {
        return Err(remote_err("read", path, &out, sudo.enabled));
    }
    Ok(out.stdout)
}

/// Metadata line printed by the read script before the slice.
#[derive(Debug, Default, PartialEq)]
struct ReadMeta {
    size: u64,
    newlines: usize,
    final_newline: bool,
    binary: bool,
    sha256: Option<String>,
}

fn parse_meta(line: &str) -> Option<ReadMeta> {
    let rest = line.strip_prefix("@XSSH ")?;
    let mut m = ReadMeta::default();
    for kv in rest.split_whitespace() {
        let (k, v) = kv.split_once('=')?;
        match k {
            "size" => m.size = v.parse().ok()?,
            "nl" => m.newlines = v.parse().ok()?,
            "last" => m.final_newline = v == "1",
            "bin" => m.binary = v == "1",
            "sha" => m.sha256 = (v.len() == 64).then(|| v.to_string()),
            _ => {}
        }
    }
    Some(m)
}

pub async fn read(
    conn: &Conn,
    path: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    encoding: Option<&str>,
    sudo: &SudoCtx<'_>,
) -> Result<FileContent> {
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(2000).max(1);
    let end = start.saturating_add(limit - 1);
    // Everything is computed remotely: only the requested lines cross the network. Lines longer
    // than 4*MAX_LINE bytes are cut there, with their full length appended after \037.
    let cmd = format!(
        "{check}{SH_HASH}s=$(wc -c < \"$p\" | tr -d ' '); n=$(wc -l < \"$p\" | tr -d ' '); \
         last=1; if [ \"$s\" -gt 0 ] && [ \"$(tail -c 1 \"$p\" | wc -l | tr -d ' ')\" = 0 ]; then last=0; fi; \
         a=$(head -c 8192 \"$p\" | wc -c | tr -d ' '); b=$(head -c 8192 \"$p\" | tr -d '\\000' | wc -c | tr -d ' '); \
         bin=0; [ \"$a\" != \"$b\" ] && bin=1; sha=; [ \"$s\" -le {HASH_LIMIT} ] && sha=$(h \"$p\"); \
         echo \"@XSSH size=$s nl=$n last=$last bin=$bin sha=${{sha:--}}\"; [ $bin = 1 ] && exit 0; \
         LC_ALL=C awk -v s={start} -v e={end} -v m={cut} 'NR>=s {{ if (length($0) > m) printf \"%s\\037%d\\n\", substr($0, 1, m), length($0); else print }} NR>=e {{ exit }}' \"$p\" | head -c {MAX_SLICE}",
        check = check_readable(&shq_path(path)),
        cut = MAX_LINE * 4,
    );
    let out = run_maybe_sudo(conn, &cmd, b"", sudo).await?;
    if out.exit_code != Some(0) {
        return Err(remote_err("read", path, &out, sudo.enabled));
    }
    let nl = out.stdout.iter().position(|b| *b == b'\n').unwrap_or(out.stdout.len());
    let meta = parse_meta(&String::from_utf8_lossy(&out.stdout[..nl]))
        .ok_or_else(|| Error::remote(format!("read {path}: unexpected output from the remote read script")))?;
    if meta.binary {
        return Err(Error::usage(format!("{path} looks like a binary file ({} bytes)", meta.size))
            .hint("copy it with `xssh cp HOST:PATH ./local` and inspect it locally"));
    }
    let slice = out.stdout.get(nl + 1..).unwrap_or_default();
    let slice_cut = slice.len() >= MAX_SLICE;
    let (decoded, detected) = text::decode_detect(slice, encoding);
    let total = meta.newlines + usize::from(meta.size > 0 && !meta.final_newline);
    let (content, shown, eol) = render_slice(&decoded, start, slice_cut);
    let end_line = if shown == 0 { start.saturating_sub(1) } else { start + shown - 1 };
    let hint = if start > total.max(1) {
        Some(format!("--offset {start} is past the end: the file has {total} lines"))
    } else if end_line < total {
        Some(format!(
            "showing lines {start}-{end_line} of {total}; continue with --offset {}",
            end_line + 1
        ))
    } else {
        None
    };
    let hint = match detected {
        Some(e) => Some(format!(
            "file is not UTF-8; decoded as {e} (`xssh host edit HOST --encoding {e}` makes this permanent){}",
            hint.map(|h| format!("; {h}")).unwrap_or_default()
        )),
        None => hint,
    };
    Ok(FileContent {
        path: path.to_string(),
        total_lines: total,
        start_line: start,
        end_line,
        bytes: meta.size,
        content,
        sha256: meta.sha256,
        eol,
        no_final_newline: meta.size > 0 && !meta.final_newline,
        encoding: detected.map(String::from).or_else(|| {
            encoding
                .filter(|e| !e.eq_ignore_ascii_case("utf-8") && !e.eq_ignore_ascii_case("utf8"))
                .map(String::from)
        }),
        hint,
    })
}

/// Number the lines of a slice starting at `start`. Returns (content, lines shown, eol kind).
fn render_slice(decoded: &str, start: usize, slice_cut: bool) -> (String, usize, Option<String>) {
    let mut lines: Vec<&str> = decoded.split('\n').collect();
    // `split` yields an empty tail after the final newline; a cut slice ends mid-line: drop it.
    if lines.last().is_some_and(|l| l.is_empty()) || (slice_cut && lines.len() > 1) {
        lines.pop();
    }
    let (mut crlf, mut lf) = (0usize, 0usize);
    let mut content = String::new();
    for (i, l) in lines.iter().enumerate() {
        let (body, full_len) = match l.rsplit_once('\u{1f}') {
            Some((b, n)) if n.chars().all(|c| c.is_ascii_digit()) && !n.is_empty() => (b, n.parse::<usize>().ok()),
            _ => (*l, None),
        };
        let body = match body.strip_suffix('\r') {
            Some(b) if full_len.is_none() => {
                crlf += 1;
                b
            }
            _ => {
                lf += 1;
                body
            }
        };
        // A lone CR would make the line render as if overwritten: show it explicitly.
        let body = if body.contains('\r') {
            body.replace('\r', "\\r")
        } else {
            body.to_string()
        };
        let len = full_len.unwrap_or(body.len());
        let body = if body.len() > MAX_LINE || full_len.is_some() {
            let mut cut = MAX_LINE.min(body.len());
            while !body.is_char_boundary(cut) {
                cut -= 1;
            }
            format!("{}… [line truncated, {len} bytes]", &body[..cut])
        } else {
            body
        };
        content.push_str(&format!("{:>6}\t{}\n", start + i, body));
    }
    let eol = match (crlf, lf) {
        (0, _) => None,
        (_, 0) => Some("crlf".to_string()),
        _ => Some("mixed".to_string()),
    };
    (content, lines.len(), eol)
}

/// Atomically replace `path` with `data`, preserving mode/owner of an existing file.
pub async fn write(conn: &Conn, path: &str, data: &[u8], opts: &WriteOpts, sudo: &SudoCtx<'_>) -> Result<WriteResult> {
    if let Some(m) = &opts.mode
        && (!m.chars().all(|c| c.is_ascii_digit() && c < '8') || m.is_empty() || m.len() > 4)
    {
        return Err(Error::usage(format!("invalid mode '{m}' (octal like 644)")));
    }
    if data.len() > MAX_WRITE {
        return Err(Error::usage(format!(
            "content is {} MB; `file write` takes up to {} MB",
            data.len() / 1024 / 1024,
            MAX_WRITE / 1024 / 1024
        ))
        .hint("copy large files with `xssh cp LOCAL HOST:PATH` (atomic, resumable)"));
    }
    if let Some(s) = &opts.expect_sha256
        && (s.len() != 64 || !s.chars().all(|c| c.is_ascii_hexdigit()))
    {
        return Err(Error::usage(format!("--expect-sha256 '{s}' is not a sha256 hex digest")));
    }
    let script = write_script(path, opts, &short_id(6));
    // ~1 MB/s worst case on a slow link, on top of the base timeout.
    let timeout = T + Duration::from_secs(data.len() as u64 / (1024 * 1024));
    let out = run_maybe_sudo_t(conn, &script, data, sudo, timeout).await?;
    if out.exit_code == Some(3) {
        let err = String::from_utf8_lossy(&out.stderr);
        let now = err
            .lines()
            .find_map(|l| l.strip_prefix("XSSH_CHANGED "))
            .unwrap_or("?")
            .trim()
            .to_string();
        let what = if now == "missing" {
            "no longer exists".to_string()
        } else {
            format!("changed since it was read (sha256 now {now})")
        };
        return Err(Error::new(ErrorCode::Busy, format!("{path} {what}; nothing was written"))
            .hint("read the file again, re-apply your change to the current content, then write"));
    }
    if out.exit_code == Some(4) && String::from_utf8_lossy(&out.stderr).contains("XSSH_NODIR") {
        return Err(Error::new(
            ErrorCode::NotFound,
            format!("{path}: parent directory does not exist; nothing was written"),
        )
        .hint("add --mkdirs to create it"));
    }
    if out.exit_code != Some(0) {
        return Err(remote_err("write", path, &out, sudo.enabled));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let field = |k: &str| stdout.lines().find_map(|l| l.strip_prefix(k)).map(str::to_string);
    let target = field("TARGET=").unwrap_or_else(|| path.to_string());
    Ok(WriteResult {
        path: target,
        bytes: data.len(),
        sha256: hex::encode(Sha256::digest(data)),
        backup: field("BACKUP="),
    })
}

fn write_script(path: &str, opts: &WriteOpts, id: &str) -> String {
    // Writes go to the symlink target (replacing the link would break e.g. sites-enabled/*).
    // Backups live in ~/.xssh/backups (0700), never next to the file where `conf.d/*` globs
    // would load them.
    let mut s = format!("umask 022; t={}; ", shq_path(path));
    s.push_str(
        "if [ -L \"$t\" ]; then t=$(readlink -f -- \"$t\" 2>/dev/null || realpath -- \"$t\" 2>/dev/null || printf '%s' \"$t\"); fi; ",
    );
    s.push_str("if [ -d \"$t\" ]; then echo \"$t: is a directory\" >&2; exit 1; fi; ");
    if opts.mkdirs {
        s.push_str("mkdir -p \"$(dirname -- \"$t\")\" || exit 1; ");
    } else {
        s.push_str("if [ ! -d \"$(dirname -- \"$t\")\" ]; then echo 'XSSH_NODIR' >&2; exit 4; fi; ");
    }
    if let Some(want) = &opts.expect_sha256 {
        s.push_str(SH_HASH);
        s.push_str(&format!(
            "if [ ! -e \"$t\" ]; then echo 'XSSH_CHANGED missing' >&2; exit 3; fi; \
             cur=$(h \"$t\"); if [ -n \"$cur\" ] && [ \"$cur\" != {want} ]; then echo \"XSSH_CHANGED $cur\" >&2; exit 3; fi; ",
            want = want.to_ascii_lowercase()
        ));
    }
    s.push_str(&format!("tmp=\"$t.xssh-tmp-{id}\"; trap 'rm -f \"$tmp\"' EXIT HUP INT TERM; "));
    s.push_str("if [ -e \"$t\" ]; then ");
    if opts.backup {
        s.push_str(&format!(
            "bd=\"$HOME/.xssh/backups\"; (umask 077; mkdir -p \"$bd\") && chmod 700 \"$HOME/.xssh\" \"$bd\" 2>/dev/null; \
             n=$(printf '%s' \"$t\" | tr / %); b=\"$bd/$n.$(date +%Y%m%d-%H%M%S).{id}\"; \
             cp -p \"$t\" \"$b\" || exit 1; chmod go-rwx \"$b\" 2>/dev/null; echo \"BACKUP=$b\"; "
        ));
        if opts.backup_keep > 0 {
            s.push_str(&format!(
                "ls -1t \"$bd/$n\".* 2>/dev/null | tail -n +{} | while IFS= read -r f; do rm -f -- \"$f\"; done; ",
                opts.backup_keep + 1
            ));
        }
        if opts.backup_days > 0 {
            s.push_str(&format!(
                "find \"$bd\" -type f -mtime +{} -exec rm -f {{}} + 2>/dev/null; ",
                opts.backup_days
            ));
        }
    }
    s.push_str("cp -p \"$t\" \"$tmp\" || exit 1; fi; cat > \"$tmp\" || exit 1; ");
    if let Some(m) = &opts.mode {
        s.push_str(&format!("chmod {m} \"$tmp\" || exit 1; "));
    }
    s.push_str("mv -f \"$tmp\" \"$t\" || exit 1; echo \"TARGET=$t\"");
    s
}

/// Line-ending variants of the `--old`/`--new` pair to try, exact first.
fn eol_variants(old: &str, new: &str) -> Vec<(String, String, Option<&'static str>)> {
    let lf = |s: &str| s.replace("\r\n", "\n");
    let crlf = |s: &str| lf(s).replace('\n', "\r\n");
    let mut v: Vec<(String, String, Option<&'static str>)> = vec![(old.to_string(), new.to_string(), None)];
    for (o, n, note) in [
        (crlf(old), crlf(new), "matched with CRLF line endings (the file uses CRLF)"),
        (lf(old), lf(new), "matched with LF line endings (your text had CRLF)"),
    ] {
        if !v.iter().any(|x| x.0 == o) {
            v.push((o, n, Some(note)));
        }
    }
    v
}

#[allow(clippy::too_many_arguments)]
pub async fn edit(
    conn: &Conn,
    path: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    opts: &WriteOpts,
    encoding: Option<&str>,
    sudo: &SudoCtx<'_>,
) -> Result<EditResult> {
    if old.is_empty() {
        return Err(Error::usage("--old must not be empty"));
    }
    if old == new {
        return Err(Error::usage("--old and --new are identical"));
    }
    let bytes = read_bytes(conn, path, sudo).await?;
    let original_sha = hex::encode(Sha256::digest(&bytes));
    // Non-UTF-8 files are edited in their own encoding (the host's, or detected GBK).
    let enc = match encoding.and_then(|l| encoding_rs::Encoding::for_label(l.as_bytes())) {
        Some(e) if e != encoding_rs::UTF_8 => Some(e),
        _ => match std::str::from_utf8(&bytes) {
            Ok(_) => None,
            Err(_) => match text::decode_detect(&bytes, None) {
                (_, Some(label)) => encoding_rs::Encoding::for_label(label.as_bytes()),
                _ => {
                    return Err(Error::usage(format!("{path} is neither UTF-8 nor GBK text"))
                        .hint("pass --encoding on the host (`xssh host edit HOST --encoding LABEL`) or replace it with `file write`"));
                }
            },
        },
    };
    let content = match enc {
        Some(e) => e.decode_without_bom_handling(&bytes).0.into_owned(),
        None => String::from_utf8(bytes).map_err(|_| Error::internal("utf-8 check"))?,
    };
    let Some((old_m, new_m, note)) = eol_variants(old, new).into_iter().find(|(o, _, _)| content.contains(o.as_str())) else {
        let crlf = content.matches("\r\n").count();
        let lf = content.matches('\n').count() - crlf;
        let eol = if crlf > 0 {
            format!(" (the file has {crlf} CRLF and {lf} LF line endings; both forms were tried)")
        } else {
            String::new()
        };
        return Err(Error::not_found(format!("--old text not found in {path}{eol}"))
            .hint("the match must be exact (including whitespace/indentation); view the file with `xssh file read`"));
    };
    let count = content.matches(&old_m).count();
    if count > 1 && !replace_all {
        return Err(Error::usage(format!("--old text occurs {count} times in {path}"))
            .hint("add surrounding lines to make it unique, or pass --replace-all"));
    }
    let first = content.find(&old_m).unwrap_or(0);
    let updated = if replace_all {
        content.replace(&old_m, &new_m)
    } else {
        content.replacen(&old_m, &new_m, 1)
    };
    let out_bytes = match enc {
        Some(e) => text::encode_for(e, &updated),
        None => updated.clone().into_bytes(),
    };
    let wopts = WriteOpts {
        expect_sha256: Some(original_sha),
        mode: None,
        mkdirs: false,
        ..opts.clone()
    };
    let w = write(conn, path, &out_bytes, &wopts, sudo).await?;
    let line_no = content[..first].matches('\n').count() + 1;
    let new_lines = new_m.matches('\n').count() + 1;
    let lines: Vec<&str> = updated.lines().collect();
    let from = line_no.saturating_sub(3).max(1);
    let to = (line_no + new_lines + 2).min(lines.len());
    let snippet = (from..=to)
        .filter_map(|i| lines.get(i - 1).map(|l| format!("{i:>6}\t{}", l.trim_end_matches('\r'))))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(EditResult {
        path: w.path.clone(),
        replacements: if replace_all { count } else { 1 },
        backup: w.backup,
        snippet,
        sha256: w.sha256,
        note: note.map(String::from),
        eol: match (updated.matches("\r\n").count(), updated.matches('\n').count()) {
            (0, _) => None,
            (c, n) if c == n => Some("crlf".to_string()),
            _ => Some("mixed".to_string()),
        },
    })
}

// ---------------- SFTP based ----------------

/// An SFTP session with deeper request pipelining than the library default (64 reads/writes
/// in flight, up to 255 KiB each, bounded by the server's `limits@openssh.com`).
pub async fn sftp(conn: &Conn) -> Result<SftpSession> {
    let ch = conn.open_session().await?;
    ch.request_subsystem(true, "sftp").await?;
    let cfg = russh_sftp::client::Config {
        max_concurrent_reads: 64,
        max_concurrent_writes: 64,
        max_write_packet_len: 255 * 1024,
        ..Default::default()
    };
    SftpSession::new_with_config(ch.into_stream(), cfg).await.map_err(|e| {
        Error::remote(format!("SFTP unavailable: {e}"))
            .hint("the server may have the sftp subsystem disabled; use `file read/write` (exec based) instead")
    })
}

/// SFTP paths are relative to the login directory; `~/x` becomes `x`.
pub(crate) fn sftp_path(p: &str) -> String {
    if p == "~" {
        ".".into()
    } else if let Some(rest) = p.strip_prefix("~/") {
        rest.to_string()
    } else {
        p.to_string()
    }
}

fn mode_string(perm: Option<u32>) -> String {
    match perm {
        Some(p) => format!("{:o}", p & 0o7777),
        None => "?".into(),
    }
}

pub(crate) fn kind_of(perm: Option<u32>) -> &'static str {
    match perm.map(|p| p & 0o170000) {
        Some(0o040000) => "dir",
        Some(0o100000) => "file",
        Some(0o120000) => "symlink",
        Some(0o020000) | Some(0o060000) => "device",
        Some(0o010000) => "fifo",
        Some(0o140000) => "socket",
        _ => "other",
    }
}

fn fmt_mtime(t: Option<u32>) -> Option<String> {
    t.and_then(|s| chrono::DateTime::from_timestamp(s as i64, 0))
        .map(|d| d.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M:%S").to_string())
}

pub async fn ls(conn: &Conn, path: &str) -> Result<Vec<Entry>> {
    let s = sftp(conn).await?;
    let p = sftp_path(path);
    let mut out = vec![];
    for e in s.read_dir(p.clone()).await.map_err(|e| Error::remote(format!("ls {path}: {e}")))? {
        let m = e.metadata();
        out.push(Entry {
            name: e.file_name(),
            kind: kind_of(m.permissions).into(),
            size: m.size.unwrap_or(0),
            mode: mode_string(m.permissions),
            mtime: fmt_mtime(m.mtime),
            owner: m.user.clone().or(m.uid.map(|u| u.to_string())),
        });
    }
    out.sort_by_key(|a| (a.kind != "dir", a.name.clone()));
    let _ = s.close().await;
    Ok(out)
}

pub async fn stat(conn: &Conn, path: &str) -> Result<Entry> {
    let s = sftp(conn).await?;
    let m = s
        .symlink_metadata(sftp_path(path))
        .await
        .map_err(|e| Error::not_found(format!("stat {path}: {e}")))?;
    let _ = s.close().await;
    Ok(Entry {
        name: path.to_string(),
        kind: kind_of(m.permissions).into(),
        size: m.size.unwrap_or(0),
        mode: mode_string(m.permissions),
        mtime: fmt_mtime(m.mtime),
        owner: m.user.clone().or(m.uid.map(|u| u.to_string())),
    })
}

pub(crate) fn join_remote(dir: &str, name: &str) -> String {
    if dir.is_empty() || dir == "." {
        name.to_string()
    } else {
        format!("{}/{}", dir.trim_end_matches('/'), name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_line() {
        let m = parse_meta("@XSSH size=12 nl=2 last=0 bin=0 sha=-").unwrap();
        assert_eq!(
            m,
            ReadMeta {
                size: 12,
                newlines: 2,
                final_newline: false,
                binary: false,
                sha256: None
            }
        );
        let sha = "a".repeat(64);
        assert_eq!(
            parse_meta(&format!("@XSSH size=1 nl=1 last=1 bin=0 sha={sha}")).unwrap().sha256,
            Some(sha)
        );
        assert!(parse_meta("junk").is_none());
    }

    #[test]
    fn slice_rendering() {
        let (c, n, eol) = render_slice("a\r\nb\r\n", 5, false);
        assert_eq!(c, "     5\ta\n     6\tb\n");
        assert_eq!((n, eol.as_deref()), (2, Some("crlf")));
        let (_, _, eol) = render_slice("a\r\nb\n", 1, false);
        assert_eq!(eol.as_deref(), Some("mixed"));
        let (c, n, eol) = render_slice("x\u{1f}9000\nlast", 1, false);
        assert_eq!(n, 2);
        assert!(c.contains("x… [line truncated, 9000 bytes]"), "{c}");
        assert!(eol.is_none());
        // A slice cut by the byte cap drops its partial last line.
        let (_, n, _) = render_slice("a\nb\npart", 1, true);
        assert_eq!(n, 2);
        let (c, _, _) = render_slice("a\rb\n", 1, false);
        assert_eq!(c, "     1\ta\\rb\n");
    }

    #[test]
    fn edit_line_ending_variants() {
        let v = eol_variants("a\nb", "c\nd");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0, "a\r\nb");
        assert_eq!(v[1].1, "c\r\nd");
        let v = eol_variants("a\r\nb", "c");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].0, "a\nb");
        assert_eq!(eol_variants("one line", "x").len(), 1);
    }

    #[test]
    fn write_script_parts() {
        let o = WriteOpts {
            backup: true,
            backup_keep: 5,
            backup_days: 30,
            expect_sha256: Some("AB".repeat(32)),
            mode: Some("600".into()),
            ..Default::default()
        };
        let s = write_script("/etc/x y.conf", &o, "abc123");
        assert!(s.contains("t='/etc/x y.conf'"));
        assert!(s.contains("XSSH_CHANGED"));
        assert!(s.contains(&"ab".repeat(32)), "expected digest is lowercased");
        assert!(
            s.contains("$(date +%Y%m%d-%H%M%S).abc123"),
            "backup names are unique within a second"
        );
        assert!(s.contains("tail -n +6"));
        assert!(s.contains("-mtime +30"));
        assert!(s.contains("chmod 600 \"$tmp\" || exit 1"));
        assert!(s.contains("trap 'rm -f \"$tmp\"'"));
        let plain = write_script("~/a", &WriteOpts::default(), "id");
        assert!(plain.starts_with("umask 022; t=\"$HOME\"/a;"));
        assert!(!plain.contains("BACKUP") && !plain.contains("XSSH_CHANGED"));
    }
}
