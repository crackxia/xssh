//! Host inventory (`hosts.toml`). Contains only non-secret metadata; passwords
//! and passphrases live in the secret store.

use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, Instant};
use xssh_core::error::{Error, ErrorCode, Result};
use xssh_core::paths::{Paths, write_private};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Host {
    pub alias: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub user: String,
    /// Path to a private key file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// Name of a key generated/managed by xssh (see `xssh key`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key_name: Option<String>,
    /// Alias of a jump host (ProxyJump).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Remote output encoding (e.g. "gbk"); default utf-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub facts: Option<Facts>,
    /// Command whose stdin/stdout carry the SSH connection instead of TCP (ssh_config
    /// ProxyCommand; `%h`, `%p`, `%r`, `%n`, `%%` are expanded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_command: Option<String>,
    /// More private key files, tried after `key` (extra ssh_config IdentityFile lines).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub identity_files: Vec<String>,
    /// OpenSSH user certificate for the key (CertificateFile). `<key>-cert.pub` next to a key is
    /// used automatically.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub certificate: Option<String>,
    /// With configured keys: `Some(false)` also tries ssh-agent and default keys afterwards
    /// (ssh_config IdentitiesOnly=no); unset or true uses only the configured keys.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identities_only: Option<bool>,
    /// Name used instead of host:port to look up and record the host key (HostKeyAlias).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_alias: Option<String>,
    /// Per-host host key policy, overriding config.toml: "tofu" or "strict".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_key_policy: Option<String>,
    /// Extra known_hosts files consulted read-only (UserKnownHostsFile).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub known_hosts_files: Vec<String>,
    /// Also offer legacy algorithms (sha1 key exchange, CBC ciphers, hmac-sha1, ssh-rsa) for old
    /// devices that support nothing newer.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub legacy_algos: bool,
    /// Prefer zlib compression (ssh_config Compression); unset uses config.toml's `compression`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compression: Option<bool>,
    /// RFC 3339 time after which the host is considered gone (temporary machines); listed as
    /// expired and removed by `host prune`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires: Option<String>,
    /// Not in hosts.toml: resolved on the fly from `user@host[:port]` or ~/.ssh/config.
    #[serde(skip)]
    pub ad_hoc: bool,
}

/// Host key policies accepted in config.toml and per host.
pub const HOST_KEY_POLICIES: &[&str] = &["tofu", "strict"];

pub fn validate_host_key_policy(p: &str) -> Result<()> {
    if HOST_KEY_POLICIES.contains(&p) {
        Ok(())
    } else {
        Err(Error::usage(format!("invalid host_key_policy '{p}': use tofu or strict")))
    }
}

fn default_port() -> u16 {
    22
}

/// Cached facts learned from the host (by `host test` or `status`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Facts {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os_pretty: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<String>,
    /// `host:port` connected to at `last_ok`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_addr: Option<String>,
    /// Character set of the login locale (`locale charmap`), e.g. UTF-8 or ANSI_X3.4-1968.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charmap: Option<String>,
    /// An installed UTF-8 locale (C.UTF-8 / en_US.UTF-8) to switch to when the login locale is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utf8_locale: Option<String>,
}

impl Host {
    /// `host:port` as connected to (the address recorded in `Facts::last_addr`).
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Expiry time, if set and valid.
    pub fn expires_at(&self) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        chrono::DateTime::parse_from_rfc3339(self.expires.as_deref()?).ok()
    }

    pub fn is_expired(&self) -> bool {
        self.expires_at().is_some_and(|t| t <= chrono::Local::now())
    }

    /// Why `host prune` removes this host: expired, or (with `unused`) last connected longer ago.
    /// Hosts never connected are not "unused".
    pub fn prune_reason(&self, unused: Option<Duration>) -> Option<String> {
        if self.is_expired() {
            return Some(format!("expired {}", self.expires.as_deref().unwrap_or_default()));
        }
        let limit = chrono::Duration::from_std(unused?).ok()?;
        let last = self.facts.as_ref()?.last_ok.as_deref()?;
        let t = chrono::DateTime::parse_from_rfc3339(last).ok()?;
        (chrono::Local::now().signed_duration_since(t) > limit).then(|| format!("last ok {last}"))
    }

    /// Same login on the same server: user, address (case-insensitive), port and route.
    pub fn same_target(&self, o: &Host) -> bool {
        self.user == o.user
            && self.host.eq_ignore_ascii_case(&o.host)
            && self.port == o.port
            && self.jump == o.jump
            && self.proxy_command == o.proxy_command
    }
}

/// A duration such as `30d` or `4h`.
pub fn parse_age(s: &str) -> Result<Duration> {
    humantime::parse_duration(s.trim()).map_err(|_| Error::usage(format!("invalid duration '{s}': use e.g. 4h, 7d, 30d")))
}

/// Parse an expiry: a duration from now (`4h`, `7d`), a date (`2026-10-10`, 00:00 local time) or
/// an RFC 3339 time. Returns RFC 3339.
pub fn parse_expires(s: &str) -> Result<String> {
    use chrono::{Local, NaiveDate, TimeZone};
    let s = s.trim();
    let bad = || {
        Error::usage(format!(
            "invalid --expires '{s}': use a duration (4h, 7d), a date (2026-10-10) or an RFC 3339 time"
        ))
    };
    if let Ok(t) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let t = Local
            .from_local_datetime(&d.and_hms_opt(0, 0, 0).ok_or_else(bad)?)
            .earliest()
            .ok_or_else(bad)?;
        return Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false));
    }
    let d = humantime::parse_duration(s).map_err(|_| bad())?;
    let t = Local::now() + chrono::Duration::from_std(d).map_err(|_| bad())?;
    Ok(t.to_rfc3339_opts(chrono::SecondsFormat::Secs, false))
}

impl Facts {
    /// A UTF-8 locale to use in interactive sessions when the login locale is not UTF-8.
    pub fn locale_fix(&self) -> Option<&str> {
        let cm = self.charmap.as_deref()?.to_ascii_lowercase().replace('-', "");
        (cm != "utf8").then_some(self.utf8_locale.as_deref()?)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct HostsFile {
    #[serde(default)]
    hosts: Vec<Host>,
}

pub struct HostStore {
    path: std::path::PathBuf,
    lock: std::path::PathBuf,
}

impl HostStore {
    pub fn new(paths: &Paths) -> Self {
        HostStore {
            path: paths.hosts_file(),
            lock: paths.home.join("hosts.lock"),
        }
    }

    pub fn list(&self) -> Result<Vec<Host>> {
        Ok(read_file(&self.path)?.hosts)
    }

    /// A saved host, else an ad-hoc one (`user@host[:port]`, an IP address, or a host named in
    /// ~/.ssh/config).
    pub fn get(&self, alias: &str) -> Result<Host> {
        if let Some(h) = self.list()?.into_iter().find(|h| h.alias == alias) {
            return Ok(h);
        }
        crate::ssh_config::ad_hoc(alias).ok_or_else(|| {
            Error::not_found(format!("unknown host alias '{alias}'")).hint(
                "run `xssh host list` to see saved hosts; `xssh host add` saves one; `user@host[:port]` or a ~/.ssh/config host name also works without saving (key/agent auth only)",
            )
        })
    }

    /// Only hosts saved in hosts.toml.
    pub fn get_saved(&self, alias: &str) -> Result<Host> {
        self.list()?.into_iter().find(|h| h.alias == alias).ok_or_else(|| {
            Error::not_found(format!("unknown host alias '{alias}'"))
                .hint("run `xssh host list` to see configured hosts, or `xssh host add` to add one")
        })
    }

    pub fn exists(&self, alias: &str) -> Result<bool> {
        Ok(self.list()?.iter().any(|h| h.alias == alias))
    }

    /// Read-modify-write under a cross-process lock.
    pub fn update<T>(&self, f: impl FnOnce(&mut Vec<Host>) -> Result<T>) -> Result<T> {
        let _guard = FileLock::acquire(&self.lock)?;
        let mut file = read_file(&self.path)?;
        let out = f(&mut file.hosts)?;
        file.hosts.sort_by(|a, b| a.alias.cmp(&b.alias));
        let s = toml::to_string_pretty(&file)?;
        write_private(&self.path, s.as_bytes())?;
        Ok(out)
    }

    pub fn add(&self, host: Host) -> Result<()> {
        validate_alias(&host.alias)?;
        self.update(|hosts| {
            if hosts.iter().any(|h| h.alias == host.alias) {
                return Err(
                    Error::new(ErrorCode::AlreadyExists, format!("host '{}' already exists", host.alias))
                        .hint("use `xssh host edit` to modify it or `xssh host rm` first"),
                );
            }
            hosts.push(host);
            Ok(())
        })
    }

    /// Remove a host; refused while another host uses it as its jump host.
    pub fn remove(&self, alias: &str) -> Result<()> {
        self.update(|hosts| {
            let used_by: Vec<&str> = hosts
                .iter()
                .filter(|h| h.jump.as_deref().is_some_and(|j| jump_chain(j).any(|x| x == alias)))
                .map(|h| h.alias.as_str())
                .collect();
            if !used_by.is_empty() {
                return Err(Error::usage(format!("'{alias}' is the jump host of: {}", used_by.join(", "))));
            }
            let before = hosts.len();
            hosts.retain(|h| h.alias != alias);
            if hosts.len() == before {
                return Err(Error::not_found(format!("unknown host alias '{alias}'")));
            }
            Ok(())
        })
    }

    pub fn set_facts(&self, alias: &str, facts: Facts) -> Result<()> {
        self.update(|hosts| {
            if let Some(h) = hosts.iter_mut().find(|h| h.alias == alias) {
                h.facts = Some(facts);
            }
            Ok(())
        })
    }

    /// Record a successful connection (`last_ok`, `last_addr`). Skips the write when the same
    /// address was already recorded within the last hour.
    pub fn record_ok(&self, alias: &str, addr: &str) -> Result<()> {
        let fresh = |f: &Facts| {
            f.last_addr.as_deref() == Some(addr)
                && f.last_ok
                    .as_deref()
                    .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
                    .is_some_and(|t| chrono::Local::now().signed_duration_since(t) < chrono::Duration::hours(1))
        };
        let saved = self.list()?.into_iter().find(|h| h.alias == alias);
        if saved.as_ref().is_none_or(|h| h.facts.as_ref().is_some_and(fresh)) {
            return Ok(());
        }
        self.update(|hosts| {
            if let Some(h) = hosts.iter_mut().find(|h| h.alias == alias) {
                let f = h.facts.get_or_insert_with(Facts::default);
                f.last_ok = Some(crate::audit::now_rfc3339());
                f.last_addr = Some(addr.to_string());
            }
            Ok(())
        })
    }

    /// Resolve a host selector: `web1`, `web1,db1`, `@tag`, `@all`.
    pub fn select(&self, selector: &str) -> Result<Vec<Host>> {
        let all = self.list()?;
        let mut out: Vec<Host> = vec![];
        for part in selector.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            if let Some(tag) = part.strip_prefix('@') {
                let matched: Vec<&Host> = all.iter().filter(|h| tag == "all" || h.tags.iter().any(|t| t == tag)).collect();
                if matched.is_empty() {
                    return Err(Error::not_found(format!("no hosts with tag '{tag}'")));
                }
                for h in matched {
                    if !out.iter().any(|o| o.alias == h.alias) {
                        out.push(h.clone());
                    }
                }
            } else {
                let h = match all.iter().find(|h| h.alias == part).cloned() {
                    Some(h) => h,
                    None => crate::ssh_config::ad_hoc(part).ok_or_else(|| {
                        Error::not_found(format!("unknown host alias '{part}'"))
                            .hint("run `xssh host list` to see saved hosts; `user@host[:port]` or a ~/.ssh/config host name also works")
                    })?,
                };
                if !out.iter().any(|o| o.alias == h.alias) {
                    out.push(h);
                }
            }
        }
        if out.is_empty() {
            return Err(Error::usage("empty host selector"));
        }
        Ok(out)
    }
}

/// Hops of a jump specification, first hop first: `a` or the ProxyJump chain `a,b`.
pub fn jump_chain(jump: &str) -> impl Iterator<Item = &str> {
    jump.split(',').map(str::trim).filter(|s| !s.is_empty())
}

/// Whether following jump hosts from `start` returns to a host already on the path. Returns the
/// cycle, e.g. `["a", "b", "a"]`.
pub fn find_jump_cycle(start: &str, lookup: impl Fn(&str) -> Option<String>) -> Option<Vec<String>> {
    fn walk(alias: &str, lookup: &dyn Fn(&str) -> Option<String>, path: &mut Vec<String>) -> Option<Vec<String>> {
        if path.iter().any(|p| p == alias) {
            let mut c = path.clone();
            c.push(alias.to_string());
            return Some(c);
        }
        if path.len() > 16 {
            return None;
        }
        path.push(alias.to_string());
        if let Some(j) = lookup(alias) {
            for hop in jump_chain(&j) {
                if let Some(c) = walk(hop, lookup, path) {
                    return Some(c);
                }
            }
        }
        path.pop();
        None
    }
    walk(start, &lookup, &mut vec![])
}

pub fn validate_alias(alias: &str) -> Result<()> {
    let ok = !alias.is_empty()
        && alias.len() <= 64
        && alias.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        && !alias.starts_with('-');
    if ok {
        Ok(())
    } else {
        Err(Error::usage(format!(
            "invalid alias '{alias}': use letters, digits, '-', '_', '.' (max 64 chars)"
        )))
    }
}

fn read_file(path: &Path) -> Result<HostsFile> {
    if !path.exists() {
        return Ok(HostsFile::default());
    }
    let s = std::fs::read_to_string(path)?;
    toml::from_str(&s).map_err(|e| Error::io(format!("{}: {e}", path.display())))
}

/// Minimal cross-process lock based on exclusive file creation.
pub struct FileLock {
    path: std::path::PathBuf,
}

impl FileLock {
    pub fn acquire(path: &Path) -> Result<Self> {
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
                Ok(_) => return Ok(FileLock { path: path.to_path_buf() }),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    // Break stale locks (older than 10s).
                    if let Ok(meta) = std::fs::metadata(path)
                        && let Ok(modified) = meta.modified()
                        && modified.elapsed().unwrap_or_default() > Duration::from_secs(10)
                    {
                        let _ = std::fs::remove_file(path);
                        continue;
                    }
                    if start.elapsed() > Duration::from_secs(5) {
                        return Err(Error::new(ErrorCode::Busy, format!("lock busy: {}", path.display())));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => return Err(Error::io(format!("lock {}: {e}", path.display()))),
            }
        }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(alias: &str) -> Host {
        Host {
            alias: alias.into(),
            host: "10.0.0.1".into(),
            port: 22,
            user: "root".into(),
            ..Default::default()
        }
    }

    #[test]
    fn expiry_and_prune() {
        assert!(parse_expires("7d").is_ok());
        assert!(parse_expires("2026-10-10").unwrap().starts_with("2026-10-10T00:00:00"));
        assert!(parse_expires("2026-10-10T08:00:00+08:00").is_ok());
        assert!(parse_expires("soon").is_err());
        let mut h = host("a");
        assert!(!h.is_expired() && h.prune_reason(None).is_none());
        h.expires = Some("2000-01-01T00:00:00+00:00".into());
        assert!(h.is_expired() && h.prune_reason(None).unwrap().starts_with("expired"));
        let mut u = host("b");
        assert!(
            u.prune_reason(Some(parse_age("30d").unwrap())).is_none(),
            "never connected is not unused"
        );
        u.facts = Some(Facts {
            last_ok: Some("2000-01-01T00:00:00+00:00".into()),
            ..Default::default()
        });
        assert!(u.prune_reason(None).is_none());
        assert!(u.prune_reason(Some(parse_age("30d").unwrap())).is_some());
    }

    #[test]
    fn same_target_ignores_alias_and_case() {
        let a = host("a");
        let mut b = host("b");
        b.host = "10.0.0.1".to_uppercase();
        assert!(a.same_target(&b));
        b.port = 2222;
        assert!(!a.same_target(&b));
    }
}
