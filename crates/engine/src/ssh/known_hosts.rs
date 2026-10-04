//! Host key verification against xssh's own known_hosts (writable, checked first), extra files
//! named per host (UserKnownHostsFile) and ~/.ssh/known_hosts (read-only).
//!
//! OpenSSH semantics: hashed names (`|1|salt|hash`), `[host]:port`, wildcard and negated
//! patterns, `@revoked` keys are rejected, `@cert-authority` lines are skipped (xssh does not
//! negotiate host certificates). A key only counts as changed when a key of the *same type* is
//! recorded and differs; a new key type is unknown (trusted on first use under tofu). A line that
//! cannot be parsed is skipped on its own.

use base64::Engine as _;
use hmac::{KeyInit, Mac};
use russh::keys::{Algorithm, HashAlg, PublicKey};
use std::path::{Path, PathBuf};
use xssh_core::error::{Error, Result};
use xssh_core::paths::Paths;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Known,
    Unknown,
    Changed { file: PathBuf, line: usize },
    Revoked { file: PathBuf, line: usize },
}

pub struct KnownHosts {
    own: PathBuf,
    /// Read-only files, in lookup order after `own`.
    others: Vec<PathBuf>,
}

pub fn fingerprint(key: &PublicKey) -> String {
    key.fingerprint(HashAlg::Sha256).to_string()
}

/// The name recorded for a host: `host`, or `[host]:port` for a non-standard port.
pub fn host_pattern(host: &str, port: u16) -> String {
    if port == 22 { host.to_string() } else { format!("[{host}]:{port}") }
}

/// Key type used to decide "same type, different key" (all RSA signature variants are one type).
fn family(a: &Algorithm) -> String {
    match a {
        Algorithm::Rsa { .. } => "ssh-rsa".into(),
        a => a.as_str().to_string(),
    }
}

#[derive(Debug)]
struct Entry {
    line: usize,
    marker: Option<String>,
    key: PublicKey,
}

fn hashed_match(field: &str, name: &str) -> bool {
    let Some(rest) = field.strip_prefix("|1|") else { return false };
    let Some((salt, hash)) = rest.split_once('|') else { return false };
    let b64 = base64::engine::general_purpose::STANDARD;
    let (Ok(salt), Ok(hash)) = (b64.decode(salt), b64.decode(hash)) else {
        return false;
    };
    let Ok(mut mac) = hmac::Hmac::<sha1::Sha1>::new_from_slice(&salt) else {
        return false;
    };
    mac.update(name.as_bytes());
    mac.verify_slice(&hash).is_ok()
}

fn glob(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.to_lowercase().chars().collect(), s.to_lowercase().chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Does the host field of a known_hosts line name `name` (already in `[host]:port` form)?
fn hosts_match(field: &str, name: &str) -> bool {
    if field.starts_with("|1|") {
        return hashed_match(field, name);
    }
    let mut hit = false;
    for p in field.split(',') {
        if let Some(neg) = p.strip_prefix('!') {
            if glob(neg, name) {
                return false;
            }
        } else if glob(p, name) {
            hit = true;
        }
    }
    hit
}

/// Entries for `name` in known_hosts text; unparseable lines are skipped.
fn entries(content: &str, name: &str) -> Vec<Entry> {
    let mut out = vec![];
    for (i, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let mut first = parts.next().unwrap_or("");
        let marker = if first.starts_with('@') {
            let m = first.to_string();
            first = parts.next().unwrap_or("");
            Some(m)
        } else {
            None
        };
        if !hosts_match(first, name) {
            continue;
        }
        let (Some(alg), Some(b64)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Ok(key) = PublicKey::from_openssh(&format!("{alg} {b64}")) {
            out.push(Entry { line: i + 1, marker, key });
        }
    }
    out
}

/// Verdict for one file, None when it holds nothing for this key type.
fn check_file(file: &Path, name: &str, key: &PublicKey) -> Option<Verdict> {
    let content = std::fs::read_to_string(file).ok()?;
    let es = entries(&content, name);
    let fam = family(&key.algorithm());
    let mut changed = None;
    for e in es.iter().filter(|e| e.marker.is_none()) {
        if e.key.key_data() == key.key_data() {
            return Some(Verdict::Known);
        }
        if changed.is_none() && family(&e.key.algorithm()) == fam {
            changed = Some(e.line);
        }
    }
    changed.map(|line| Verdict::Changed {
        file: file.to_path_buf(),
        line,
    })
}

fn revoked(file: &Path, name: &str, key: &PublicKey) -> Option<usize> {
    let content = std::fs::read_to_string(file).ok()?;
    entries(&content, name)
        .into_iter()
        .find(|e| e.marker.as_deref() == Some("@revoked") && e.key.key_data() == key.key_data())
        .map(|e| e.line)
}

impl KnownHosts {
    /// `extra`: per-host files (UserKnownHostsFile), consulted after xssh's own file and before
    /// ~/.ssh/known_hosts.
    pub fn new(paths: &Paths, use_system: bool) -> Self {
        Self::with_files(paths.known_hosts(), &[], use_system)
    }

    pub fn with_files(own: PathBuf, extra: &[String], use_system: bool) -> Self {
        let mut others: Vec<PathBuf> = extra.iter().map(PathBuf::from).collect();
        if use_system && let Some(h) = std::env::home_dir() {
            let p = h.join(".ssh").join("known_hosts");
            if !others.contains(&p) {
                others.push(p);
            }
        }
        KnownHosts { own, others }
    }

    fn files(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.own).chain(self.others.iter())
    }

    /// `name` is the host (or HostKeyAlias); xssh's own file wins, so `host trust` overrides a
    /// stale entry in ~/.ssh/known_hosts.
    pub fn verify(&self, name: &str, port: u16, key: &PublicKey) -> Verdict {
        let pat = host_pattern(name, port);
        for f in self.files() {
            if let Some(line) = revoked(f, &pat, key) {
                return Verdict::Revoked { file: f.clone(), line };
            }
        }
        for f in self.files() {
            if let Some(v) = check_file(f, &pat, key) {
                return v;
            }
        }
        Verdict::Unknown
    }

    /// Key types recorded for the host, most trusted file first: offered first during key
    /// exchange so a server with several host keys presents the one already known.
    pub fn recorded_algorithms(&self, name: &str, port: u16) -> Vec<Algorithm> {
        let pat = host_pattern(name, port);
        let mut out: Vec<Algorithm> = vec![];
        for f in self.files() {
            let Ok(content) = std::fs::read_to_string(f) else { continue };
            for e in entries(&content, &pat).into_iter().filter(|e| e.marker.is_none()) {
                let a = e.key.algorithm();
                if !out.iter().any(|o| family(o) == family(&a)) {
                    out.push(a);
                }
            }
        }
        out
    }

    pub fn learn(&self, name: &str, port: u16, key: &PublicKey) -> Result<()> {
        use std::io::Write;
        if let Some(dir) = self.own.parent() {
            std::fs::create_dir_all(dir).map_err(|e| Error::io(format!("create {}: {e}", dir.display())))?;
        }
        let line = format!(
            "{} {}\n",
            host_pattern(name, port),
            key.to_openssh().map_err(|e| Error::internal(format!("encode host key: {e}")))?
        );
        let mut needs_nl = false;
        if let Ok(s) = std::fs::read_to_string(&self.own) {
            needs_nl = !s.is_empty() && !s.ends_with('\n');
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.own)
            .map_err(|e| Error::io(format!("write {}: {e}", self.own.display())))?;
        if needs_nl {
            f.write_all(b"\n")?;
        }
        f.write_all(line.as_bytes())
            .map_err(|e| Error::io(format!("write {}: {e}", self.own.display())))?;
        drop(f);
        xssh_core::paths::restrict_file(&self.own);
        Ok(())
    }

    /// Remove every recorded key for the host from xssh's own known_hosts (plain or hashed
    /// names; lines listing several hosts lose only this one).
    pub fn forget(&self, name: &str, port: u16) -> Result<usize> {
        if !self.own.exists() {
            return Ok(0);
        }
        let pat = host_pattern(name, port);
        let content = std::fs::read_to_string(&self.own)?;
        let mut removed = 0;
        let mut out = String::new();
        for l in content.lines() {
            let mut parts = l.split_whitespace();
            let first = parts.next().unwrap_or("");
            if first.starts_with('@') || first.is_empty() || l.trim_start().starts_with('#') {
                out.push_str(l);
                out.push('\n');
                continue;
            }
            if first.starts_with("|1|") {
                if hashed_match(first, &pat) {
                    removed += 1;
                } else {
                    out.push_str(l);
                    out.push('\n');
                }
                continue;
            }
            let names: Vec<&str> = first.split(',').collect();
            let kept: Vec<&str> = names.iter().copied().filter(|n| !n.eq_ignore_ascii_case(&pat)).collect();
            if kept.len() == names.len() {
                out.push_str(l);
                out.push('\n');
            } else {
                removed += 1;
                if !kept.is_empty() {
                    out.push_str(&l.replacen(first, &kept.join(","), 1));
                    out.push('\n');
                }
            }
        }
        std::fs::write(&self.own, out)?;
        Ok(removed)
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn make(alg: Algorithm) -> String {
        let k = russh::keys::PrivateKey::random(&mut rand::rng(), alg).unwrap();
        k.public_key().to_openssh().unwrap()
    }

    fn key(s: &str) -> PublicKey {
        PublicKey::from_openssh(s).unwrap()
    }

    fn keys() -> (String, String, String) {
        let ec = Algorithm::Ecdsa {
            curve: russh::keys::EcdsaCurve::NistP256,
        };
        (make(Algorithm::Ed25519), make(Algorithm::Ed25519), make(ec))
    }

    fn with(content: &str) -> (KnownHosts, PathBuf) {
        let d = std::env::temp_dir().join(format!("xssh-kh-{}-{}", std::process::id(), rand_suffix()));
        std::fs::create_dir_all(&d).unwrap();
        let sys = d.join("system");
        std::fs::write(&sys, content).unwrap();
        (
            KnownHosts {
                own: d.join("own"),
                others: vec![sys],
            },
            d,
        )
    }

    fn rand_suffix() -> u64 {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        N.fetch_add(1, Ordering::Relaxed)
    }

    #[test]
    fn same_type_changed_other_type_unknown() {
        let (ED, ED2, ECDSA) = keys();
        let (kh, d) = with(&format!("web,10.0.0.1 {ECDSA}\n[web]:2222 {ED2}\ngarbage line here\n"));
        // Only an ecdsa key is recorded: an ed25519 key is new, not "changed".
        assert_eq!(kh.verify("web", 22, &key(&ED)), Verdict::Unknown);
        assert_eq!(kh.verify("web", 22, &key(&ECDSA)), Verdict::Known);
        assert!(matches!(kh.verify("web", 2222, &key(&ED)), Verdict::Changed { line: 2, .. }));
        assert_eq!(kh.recorded_algorithms("web", 22).len(), 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn own_file_overrides_system_and_forget() {
        let (ED, ED2, _) = keys();
        let (kh, d) = with(&format!("web {ED2}\n"));
        assert!(matches!(kh.verify("web", 22, &key(&ED)), Verdict::Changed { .. }));
        kh.learn("web", 22, &key(&ED)).unwrap();
        assert_eq!(kh.verify("web", 22, &key(&ED)), Verdict::Known);
        std::fs::write(&kh.own, format!("web,other {ED}\n")).unwrap();
        assert_eq!(kh.forget("web", 22).unwrap(), 1);
        assert_eq!(std::fs::read_to_string(&kh.own).unwrap().trim(), format!("other {ED}"));
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn hashed_patterns_and_revoked() {
        let (ED, ED2, ECDSA) = keys();
        // `ssh-keygen -H` style entry for "web": salt and HMAC-SHA1 computed here.
        let b64 = base64::engine::general_purpose::STANDARD;
        let salt = [7u8; 20];
        let mut mac = hmac::Hmac::<sha1::Sha1>::new_from_slice(&salt).unwrap();
        mac.update(b"web");
        let hashed = format!("|1|{}|{}", b64.encode(salt), b64.encode(mac.finalize().into_bytes()));
        let (kh, d) = with(&format!(
            "{hashed} {ED}\n*.corp,!gw.corp {ECDSA}\n@revoked * {ED2}\n@cert-authority *.x {ED}\n"
        ));
        assert_eq!(kh.verify("web", 22, &key(&ED)), Verdict::Known);
        assert_eq!(kh.verify("a.corp", 22, &key(&ECDSA)), Verdict::Known);
        assert_eq!(kh.verify("gw.corp", 22, &key(&ECDSA)), Verdict::Unknown);
        assert!(matches!(kh.verify("anything", 22, &key(&ED2)), Verdict::Revoked { line: 3, .. }));
        // @cert-authority lines are not plain keys for matching hosts.
        assert_eq!(kh.verify("h.x", 22, &key(&ED)), Verdict::Unknown);
        let _ = std::fs::remove_dir_all(d);
    }
}
