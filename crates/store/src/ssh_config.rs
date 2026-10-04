//! OpenSSH client configuration (`~/.ssh/config`), resolved the way `ssh` does it:
//! first value wins, `Host` / `Match` blocks with wildcard and negated patterns, `Include`,
//! multi-value keys (IdentityFile, CertificateFile, UserKnownHostsFile) and `%` tokens.
//!
//! Used by `xssh host import` and for ad-hoc hosts: `xssh exec user@10.0.0.5 -- ...` or a
//! host name defined in ~/.ssh/config works without saving it first.

use crate::hosts::{Host, validate_alias};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Keys whose every occurrence counts (OpenSSH appends instead of keeping the first).
const MULTI: &[&str] = &[
    "identityfile",
    "certificatefile",
    "localforward",
    "remoteforward",
    "dynamicforward",
    "sendenv",
];

#[derive(Debug, Clone)]
enum Line {
    Host(Vec<String>),
    Match(Vec<String>),
    Kv(String, Vec<String>),
}

/// Settings that apply to one host, keys lower-cased.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    values: HashMap<String, Vec<String>>,
    /// Match criteria this resolver cannot evaluate (`exec`, `localnetwork`...); treated as false.
    pub unsupported: Vec<String>,
}

impl Resolved {
    /// First value of a single-valued key (all arguments joined by a space).
    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).and_then(|v| v.first()).map(String::as_str)
    }
    /// Every value of a multi-valued key, in file order.
    pub fn all(&self, key: &str) -> &[String] {
        self.values.get(key).map(Vec::as_slice).unwrap_or(&[])
    }
    fn set(&mut self, key: &str, args: &[String]) {
        if args.is_empty() {
            return;
        }
        if MULTI.contains(&key) {
            self.values.entry(key.to_string()).or_default().push(args.join(" "));
        } else if key == "userknownhostsfile" || key == "globalknownhostsfile" {
            self.values.entry(key.to_string()).or_insert_with(|| args.to_vec());
        } else {
            self.values.entry(key.to_string()).or_insert_with(|| vec![args.join(" ")]);
        }
    }
}

/// `XSSH_SSH_CONFIG` (tests, or a non-default file), else `~/.ssh/config`.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("XSSH_SSH_CONFIG") {
        return Some(PathBuf::from(p));
    }
    std::env::home_dir().map(|h| h.join(".ssh").join("config"))
}

fn ssh_dir() -> PathBuf {
    std::env::home_dir().map(|h| h.join(".ssh")).unwrap_or_default()
}

pub fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "root".into())
}

/// Split a config line into words: whitespace separated, `"..."` groups, an optional `=`
/// between the keyword and its arguments.
fn words(line: &str) -> Vec<String> {
    let mut out = vec![];
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for (i, c) in line.chars().enumerate() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            c if !quoted && (c.is_whitespace() || (c == '=' && out.is_empty() && i > 0)) => {
                if any || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => cur.push(c),
        }
    }
    if any || !cur.is_empty() {
        out.push(cur);
    }
    // `Key = value` leaves a lone "=" word.
    out.retain(|w| w != "=");
    out
}

fn parse_lines(content: &str, base: &Path, depth: usize, out: &mut Vec<Line>) {
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut w = words(line);
        if w.is_empty() {
            continue;
        }
        let key = w.remove(0).to_ascii_lowercase();
        match key.as_str() {
            "host" => out.push(Line::Host(w)),
            "match" => out.push(Line::Match(w)),
            "include" if depth < 16 => {
                for pat in w {
                    for f in include_files(&pat, base) {
                        if let Ok(c) = std::fs::read_to_string(&f) {
                            parse_lines(&c, base, depth + 1, out);
                        }
                    }
                }
            }
            _ => out.push(Line::Kv(key, w)),
        }
    }
}

/// Files named by an `Include` argument: `~` expanded, relative paths under ~/.ssh, `*`/`?`
/// wildcards in the last component, sorted like glob(3).
fn include_files(pat: &str, base: &Path) -> Vec<PathBuf> {
    let p = PathBuf::from(expand_tilde(pat));
    let p = if p.is_absolute() { p } else { base.join(p) };
    let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        return vec![p];
    }
    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut v: Vec<PathBuf> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| wildcard(&name, &e.file_name().to_string_lossy(), false))
        .map(|e| e.path())
        .filter(|p| p.is_file())
        .collect();
    v.sort();
    v
}

/// OpenSSH `match_pattern`: `*` and `?` wildcards; host names compare case-insensitively.
fn wildcard(pattern: &str, s: &str, fold: bool) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = if fold {
        (pattern.to_lowercase().chars().collect(), s.to_lowercase().chars().collect())
    } else {
        (pattern.chars().collect(), s.chars().collect())
    };
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

/// A pattern list (`a b !c` or `a,b,!c`): some positive pattern matches and no negated one does.
fn pattern_list(patterns: &[String], s: &str) -> bool {
    let mut hit = false;
    for p in patterns.iter().flat_map(|p| p.split(',')).filter(|p| !p.is_empty()) {
        if let Some(neg) = p.strip_prefix('!') {
            if wildcard(neg, s, true) {
                return false;
            }
        } else if wildcard(p, s, true) {
            hit = true;
        }
    }
    hit
}

fn eval_match(criteria: &[String], name: &str, r: &Resolved, user: Option<&str>, unsupported: &mut Vec<String>) -> bool {
    let mut i = 0;
    let mut ok = true;
    while i < criteria.len() {
        let raw = criteria[i].to_ascii_lowercase();
        let (neg, c) = match raw.strip_prefix('!') {
            Some(c) => (true, c.to_string()),
            None => (false, raw.clone()),
        };
        let arg = criteria.get(i + 1).cloned().unwrap_or_default();
        let (res, used) = match c.as_str() {
            "all" => (true, 0),
            // Evaluated in the final (only) pass.
            "final" => (true, 0),
            "canonical" => (false, 0),
            "host" => (pattern_list(&[arg], r.get("hostname").unwrap_or(name)), 1),
            "originalhost" => (pattern_list(&[arg], name), 1),
            "user" => {
                let u = user.or(r.get("user")).map(String::from).unwrap_or_else(local_user);
                (pattern_list(&[arg], &u), 1)
            }
            "localuser" => (pattern_list(&[arg], &local_user()), 1),
            other => {
                unsupported.push(format!("Match {other}"));
                (
                    false,
                    if matches!(other, "exec" | "localnetwork" | "tagged" | "version" | "sessiontype" | "command") {
                        1
                    } else {
                        0
                    },
                )
            }
        };
        ok &= res != neg;
        i += 1 + used;
    }
    ok
}

/// Settings for host `name` from config text (`base`: the directory relative Includes use).
pub fn resolve_text(content: &str, base: &Path, name: &str, user: Option<&str>) -> Resolved {
    let mut lines = vec![];
    parse_lines(content, base, 0, &mut lines);
    let mut r = Resolved::default();
    let mut unsupported = vec![];
    let mut active = true;
    for l in &lines {
        match l {
            Line::Host(p) => active = pattern_list(p, name),
            Line::Match(c) => active = eval_match(c, name, &r, user, &mut unsupported),
            Line::Kv(k, v) if active => r.set(k, v),
            Line::Kv(..) => {}
        }
    }
    unsupported.dedup();
    r.unsupported = unsupported;
    r
}

/// Settings for host `name` from the user's config (empty when there is none).
pub fn resolve(name: &str, user: Option<&str>) -> Resolved {
    let Some(path) = config_path() else { return Resolved::default() };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Resolved::default();
    };
    resolve_text(&content, &ssh_dir(), name, user)
}

/// Names written literally (no wildcard, no negation) in `Host` lines, in file order.
pub fn concrete_names(content: &str, base: &Path) -> Vec<String> {
    let mut lines = vec![];
    parse_lines(content, base, 0, &mut lines);
    let mut out: Vec<String> = vec![];
    for l in lines {
        if let Line::Host(p) = l {
            for n in p.iter().flat_map(|p| p.split(',')) {
                if !n.contains(['*', '?', '!']) && !n.is_empty() && !out.iter().any(|o| o.eq_ignore_ascii_case(n)) {
                    out.push(n.to_string());
                }
            }
        }
    }
    out
}

/// Expand `%` tokens: %h host name, %p port, %r remote user, %n original name, %u local user,
/// %d home directory, %% a percent sign.
pub fn expand_tokens(s: &str, host: &str, port: u16, user: &str, original: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('h') => out.push_str(host),
            Some('p') => out.push_str(&port.to_string()),
            Some('r') => out.push_str(user),
            Some('n') => out.push_str(original),
            Some('u') => out.push_str(&local_user()),
            Some('d') => out.push_str(&std::env::home_dir().map(|h| h.display().to_string()).unwrap_or_default()),
            Some('%') => out.push('%'),
            Some(o) => {
                out.push('%');
                out.push(o);
            }
            None => out.push('%'),
        }
    }
    out
}

fn expand_tilde(p: &str) -> String {
    if let Some(rest) = p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\"))
        && let Some(h) = std::env::home_dir()
    {
        return h.join(rest).display().to_string();
    }
    p.to_string()
}

const LEGACY_NAMES: &[&str] = &["-sha1", "cbc", "hmac-sha1", "ssh-rsa", "ssh-dss", "group1"];

/// Build a host from resolved settings. `user`/`port` (from `user@host:port`) take precedence
/// like on the ssh command line.
pub fn to_host(alias: &str, r: &Resolved, user: Option<&str>, port: Option<u16>, default_user: &str) -> Host {
    let hostname = r
        .get("hostname")
        .map(|h| expand_tokens(h, alias, 22, "", alias))
        .unwrap_or_else(|| alias.to_string());
    let port = port.or_else(|| r.get("port").and_then(|p| p.parse().ok())).unwrap_or(22);
    let user = user
        .map(String::from)
        .or_else(|| r.get("user").map(String::from))
        .unwrap_or_else(|| default_user.to_string());
    let file = |f: &str| expand_tilde(&expand_tokens(f, &hostname, port, &user, alias));
    let mut keys: Vec<String> = r
        .all("identityfile")
        .iter()
        .map(|f| file(f))
        .filter(|f| Path::new(f).is_file())
        .collect();
    keys.dedup();
    let key = (!keys.is_empty()).then(|| keys.remove(0));
    let certificate = r.all("certificatefile").iter().map(|f| file(f)).find(|f| Path::new(f).is_file());
    let identities_only = match r.get("identitiesonly").map(str::to_ascii_lowercase).as_deref() {
        Some("yes") => Some(true),
        _ if key.is_some() => Some(false),
        _ => None,
    };
    let none = |v: Option<&str>| v.filter(|v| !v.eq_ignore_ascii_case("none")).map(String::from);
    let jump = none(r.get("proxyjump")).map(|j| jump_hops(&j).into_iter().collect::<Vec<_>>().join(","));
    let host_key_policy = match r.get("stricthostkeychecking").map(str::to_ascii_lowercase).as_deref() {
        Some("yes" | "ask") => Some("strict".to_string()),
        Some("no" | "off" | "accept-new") => Some("tofu".to_string()),
        _ => None,
    };
    let known_hosts_files = r
        .all("userknownhostsfile")
        .iter()
        .map(|f| file(f))
        .filter(|f| f != "/dev/null" && !f.eq_ignore_ascii_case("none"))
        .filter(|f| std::env::home_dir().is_none_or(|h| Path::new(f) != h.join(".ssh").join("known_hosts")))
        .collect();
    let legacy_algos = [
        "kexalgorithms",
        "ciphers",
        "macs",
        "hostkeyalgorithms",
        "pubkeyacceptedalgorithms",
        "pubkeyacceptedkeytypes",
    ]
    .iter()
    .filter_map(|k| r.get(k))
    .any(|v| {
        v.split([',', '+', '^', ' '])
            .any(|a| LEGACY_NAMES.iter().any(|l| a.ends_with(l) || a.contains(l)))
    });
    Host {
        alias: alias.to_string(),
        host: hostname,
        port,
        user,
        key,
        identity_files: keys,
        certificate,
        identities_only,
        jump,
        proxy_command: none(r.get("proxycommand")),
        host_key_alias: r.get("hostkeyalias").map(String::from),
        host_key_policy,
        known_hosts_files,
        legacy_algos,
        compression: r.get("compression").map(|v| v.eq_ignore_ascii_case("yes")),
        ..Default::default()
    }
}

/// ProxyJump hops without `ssh://` prefixes.
fn jump_hops(j: &str) -> Vec<String> {
    j.split(',')
        .map(|h| h.trim().trim_start_matches("ssh://").trim_end_matches('/').to_string())
        .filter(|h| !h.is_empty())
        .collect()
}

/// `[user@]host[:port]`, `ssh://[user@]host[:port]`, `[user@][v6addr]:port`.
pub fn split_target(spec: &str) -> (Option<String>, String, Option<u16>) {
    let s = spec.trim().trim_start_matches("ssh://").trim_end_matches('/');
    let (user, rest) = match s.rsplit_once('@') {
        Some((u, r)) if !u.is_empty() => (Some(u.to_string()), r),
        _ => (None, s),
    };
    if let Some(r) = rest.strip_prefix('[')
        && let Some((h, tail)) = r.split_once(']')
    {
        let port = tail.strip_prefix(':').and_then(|p| p.parse().ok());
        return (user, h.to_string(), port);
    }
    // A single colon separates the port; more than one is a bare IPv6 address.
    match rest.split_once(':') {
        Some((h, p)) if !p.contains(':') => match p.parse() {
            Ok(port) => (user, h.to_string(), Some(port)),
            Err(_) => (user, rest.to_string(), None),
        },
        _ => (user, rest.to_string(), None),
    }
}

/// An unsaved host from `user@host[:port]`, an IP address, or a name that a `Host` line of
/// ~/.ssh/config spells out literally. Plain unknown names return None, so a mistyped alias is
/// an error instead of a DNS lookup.
pub fn ad_hoc(spec: &str) -> Option<Host> {
    let path = config_path();
    let content = path.as_ref().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
    ad_hoc_in(spec, &content, &ssh_dir())
}

pub fn ad_hoc_in(spec: &str, content: &str, base: &Path) -> Option<Host> {
    let (user, host, port) = split_target(spec);
    if host.is_empty() || host.contains(char::is_whitespace) || host.starts_with('-') {
        return None;
    }
    let named = concrete_names(content, base).iter().any(|n| n.eq_ignore_ascii_case(&host));
    let is_ip = host.parse::<std::net::IpAddr>().is_ok();
    if !(named || is_ip || user.is_some() || port.is_some()) {
        return None;
    }
    let r = resolve_text(content, base, &host, user.as_deref());
    let mut h = to_host(&host, &r, user.as_deref(), port, &local_user());
    h.alias = spec.to_string();
    h.ad_hoc = true;
    Some(h)
}

/// Hosts for `xssh host import`: every name written literally in a `Host` line, resolved with
/// the whole file (so `Host *` and later blocks apply the OpenSSH way).
pub fn parse(content: &str, default_user: &str) -> Vec<Host> {
    parse_in(content, &ssh_dir(), default_user)
}

pub fn parse_in(content: &str, base: &Path, default_user: &str) -> Vec<Host> {
    concrete_names(content, base)
        .into_iter()
        .filter(|n| validate_alias(n).is_ok())
        .map(|n| {
            let r = resolve_text(content, base, &n, None);
            let mut h = to_host(&n, &r, None, None, default_user);
            h.tags = vec!["imported".into()];
            h
        })
        .collect()
}

/// `hosts` as an OpenSSH config (`xssh host export`). `managed_key` maps an xssh key name to its
/// file; `password` says whether a host's login password is stored (it is never exported).
pub fn export(hosts: &[Host], managed_key: impl Fn(&str) -> PathBuf, password: impl Fn(&str) -> bool) -> String {
    fn q(v: &str) -> String {
        if v.contains([' ', '\t', '#']) {
            format!("\"{v}\"")
        } else {
            v.to_string()
        }
    }
    let mut out = String::from("# Generated by `xssh host export`.\n");
    for h in hosts {
        out.push('\n');
        if let Some(n) = h.note.as_deref().filter(|n| !n.trim().is_empty()) {
            out.push_str(&format!("# {}\n", n.replace('\n', " ")));
        }
        if password(&h.alias) {
            out.push_str("# login password is stored in xssh only (not exported)\n");
        }
        out.push_str(&format!("Host {}\n  HostName {}\n  User {}\n", h.alias, h.host, h.user));
        if h.port != 22 {
            out.push_str(&format!("  Port {}\n", h.port));
        }
        let mut keys: Vec<String> = h.key.iter().cloned().collect();
        if let Some(k) = &h.key_name {
            out.push_str(&format!("# key '{k}' is encrypted with a passphrase kept in xssh's secret store\n"));
            keys.push(managed_key(k).display().to_string());
        }
        keys.extend(h.identity_files.iter().cloned());
        for k in &keys {
            out.push_str(&format!("  IdentityFile {}\n", q(k)));
        }
        if !keys.is_empty() && h.identities_only != Some(false) {
            out.push_str("  IdentitiesOnly yes\n");
        }
        if let Some(c) = &h.certificate {
            out.push_str(&format!("  CertificateFile {}\n", q(c)));
        }
        if let Some(j) = &h.jump {
            out.push_str(&format!("  ProxyJump {j}\n"));
        }
        if let Some(pc) = &h.proxy_command {
            out.push_str(&format!("  ProxyCommand {pc}\n"));
        }
        if let Some(a) = &h.host_key_alias {
            out.push_str(&format!("  HostKeyAlias {a}\n"));
        }
        match h.host_key_policy.as_deref() {
            Some("strict") => out.push_str("  StrictHostKeyChecking yes\n"),
            Some("tofu") => out.push_str("  StrictHostKeyChecking accept-new\n"),
            _ => {}
        }
        if !h.known_hosts_files.is_empty() {
            let files: Vec<String> = h.known_hosts_files.iter().map(|f| q(f)).collect();
            out.push_str(&format!("  UserKnownHostsFile ~/.ssh/known_hosts {}\n", files.join(" ")));
        }
        if let Some(c) = h.compression {
            out.push_str(&format!("  Compression {}\n", if c { "yes" } else { "no" }));
        }
        if h.legacy_algos {
            out.push_str(
                "  KexAlgorithms +diffie-hellman-group14-sha1,diffie-hellman-group1-sha1\n  Ciphers +aes128-cbc,aes256-cbc,3des-cbc\n  MACs +hmac-sha1\n  HostKeyAlgorithms +ssh-rsa\n  PubkeyAcceptedAlgorithms +ssh-rsa\n",
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_round_trips() {
        let d = tmp("export");
        let key = d.join("id key");
        std::fs::write(&key, "k").unwrap();
        let mk = d.join("managed");
        std::fs::write(&mk, "k").unwrap();
        let hosts = vec![
            Host {
                alias: "a".into(),
                host: "10.0.0.1".into(),
                port: 2222,
                user: "root".into(),
                key: Some(key.display().to_string()),
                jump: Some("b,u@j:22".into()),
                host_key_policy: Some("strict".into()),
                legacy_algos: true,
                compression: Some(true),
                note: Some("web\nfront".into()),
                ..Default::default()
            },
            Host {
                alias: "b".into(),
                host: "b.example".into(),
                port: 22,
                user: "me".into(),
                key_name: Some("managed".into()),
                ..Default::default()
            },
        ];
        let text = export(&hosts, |_| mk.clone(), |a| a == "b");
        assert!(text.contains("# web front\n"));
        assert!(text.contains("# login password is stored in xssh only"));
        let back = parse_in(&text, &d, "nobody");
        let a = back.iter().find(|h| h.alias == "a").unwrap();
        assert_eq!((a.host.as_str(), a.port, a.user.as_str()), ("10.0.0.1", 2222, "root"));
        assert_eq!(a.key.as_deref(), Some(key.display().to_string().as_str()));
        assert_eq!(a.identities_only, Some(true));
        assert_eq!(a.jump.as_deref(), Some("b,u@j:22"));
        assert_eq!(a.host_key_policy.as_deref(), Some("strict"));
        assert!(a.legacy_algos);
        assert_eq!(a.compression, Some(true));
        let b = back.iter().find(|h| h.alias == "b").unwrap();
        assert_eq!(b.key.as_deref(), Some(mk.display().to_string().as_str()));
        assert_eq!(b.port, 22);
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("xssh-sshcfg-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn first_match_wins_and_defaults() {
        let base = tmp("a");
        let key = base.join("id_web");
        std::fs::write(&key, "x").unwrap();
        let cfg = format!(
            r#"
Host web1 web2
    HostName 10.0.0.1
    IdentityFile {}

Host db
    HostName db.internal
    User postgres
    Port 22
    ProxyJump web1

Host *
    User admin
    Port 2222

Host *.corp
    User x
"#,
            key.display()
        );
        let hosts = parse_in(&cfg, &base, "me");
        assert_eq!(hosts.len(), 3);
        let web2 = hosts.iter().find(|h| h.alias == "web2").unwrap();
        assert_eq!(web2.host, "10.0.0.1");
        assert_eq!(web2.user, "admin");
        assert_eq!(web2.port, 2222);
        assert_eq!(web2.key.as_deref(), Some(key.display().to_string().as_str()));
        assert_eq!(web2.identities_only, Some(false));
        let db = hosts.iter().find(|h| h.alias == "db").unwrap();
        assert_eq!(db.user, "postgres");
        // `Port 22` in the db block comes first and wins over `Host *`.
        assert_eq!(db.port, 22);
        assert_eq!(db.jump.as_deref(), Some("web1"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn host_star_first_wins_over_later_blocks() {
        // OpenSSH: the first obtained value is used, so `Host *` at the top beats later blocks.
        let r = resolve_text("Host *\n User a\nHost web\n User b\n", Path::new("."), "web", None);
        assert_eq!(r.get("user"), Some("a"));
    }

    #[test]
    fn negation_wildcards_and_match() {
        let cfg = "Host *.prod !bastion.prod\n  User deploy\nMatch host 10.1.*\n  Port 2200\nHost app1\n  HostName 10.1.2.3\n";
        let r = resolve_text(cfg, Path::new("."), "web.prod", None);
        assert_eq!(r.get("user"), Some("deploy"));
        let r = resolve_text(cfg, Path::new("."), "bastion.prod", None);
        assert_eq!(r.get("user"), None);
        // `Match host` compares the HostName known so far; app1's HostName comes after the Match.
        let r = resolve_text(
            "Host app1\n HostName 10.1.2.3\nMatch host 10.1.*\n Port 2200\n",
            Path::new("."),
            "app1",
            None,
        );
        assert_eq!(r.get("port"), Some("2200"));
        let r = resolve_text("Match exec \"true\"\n Port 1\n", Path::new("."), "x", None);
        assert_eq!(r.get("port"), None);
        assert_eq!(r.unsupported, vec!["Match exec".to_string()]);
    }

    #[test]
    fn include_and_multi_values() {
        let base = tmp("b");
        std::fs::create_dir_all(base.join("conf.d")).unwrap();
        std::fs::write(
            base.join("conf.d").join("10-app.conf"),
            "Host app\n HostName app.example\n IdentityFile ~/k1\n IdentityFile ~/k2\n",
        )
        .unwrap();
        let cfg = "Include conf.d/*.conf\nHost app\n User ops\n ProxyCommand ssh -W %h:%p gw\n";
        let r = resolve_text(cfg, &base, "app", None);
        assert_eq!(r.get("hostname"), Some("app.example"));
        assert_eq!(r.all("identityfile").len(), 2);
        let h = to_host("app", &r, None, None, "me");
        assert_eq!(h.user, "ops");
        assert_eq!(h.proxy_command.as_deref(), Some("ssh -W %h:%p gw"));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn jumps_policies_and_legacy() {
        let cfg = "Host old\n HostName 10.9.9.9\n ProxyJump ssh://ops@gw:2222,bastion\n StrictHostKeyChecking yes\n KexAlgorithms +diffie-hellman-group1-sha1\n HostKeyAlias oldbox\n";
        let r = resolve_text(cfg, Path::new("."), "old", None);
        let h = to_host("old", &r, None, None, "me");
        assert_eq!(h.jump.as_deref(), Some("ops@gw:2222,bastion"));
        assert_eq!(h.host_key_policy.as_deref(), Some("strict"));
        assert!(h.legacy_algos);
        assert_eq!(h.host_key_alias.as_deref(), Some("oldbox"));
    }

    #[test]
    fn targets_and_ad_hoc() {
        assert_eq!(
            split_target("bob@10.0.0.5:2222"),
            (Some("bob".into()), "10.0.0.5".into(), Some(2222))
        );
        assert_eq!(split_target("ssh://bob@[::1]:22"), (Some("bob".into()), "::1".into(), Some(22)));
        assert_eq!(split_target("fe80::1"), (None, "fe80::1".into(), None));
        assert_eq!(split_target("web"), (None, "web".into(), None));
        let cfg = "Host gw\n HostName 1.2.3.4\n User ops\n";
        let h = ad_hoc_in("gw", cfg, Path::new(".")).unwrap();
        assert!(h.ad_hoc);
        assert_eq!((h.host.as_str(), h.user.as_str(), h.port), ("1.2.3.4", "ops", 22));
        let h = ad_hoc_in("root@gw:2200", cfg, Path::new(".")).unwrap();
        assert_eq!(
            (h.host.as_str(), h.user.as_str(), h.port, h.alias.as_str()),
            ("1.2.3.4", "root", 2200, "root@gw:2200")
        );
        assert!(ad_hoc_in("10.0.0.7", "", Path::new(".")).is_some());
        // A plain unknown name is a typo, not a DNS lookup.
        assert!(ad_hoc_in("web1x", cfg, Path::new(".")).is_none());
    }

    #[test]
    fn wildcard_matching() {
        assert!(wildcard("*.corp", "a.corp", true));
        assert!(wildcard("web?", "WEB1", true));
        assert!(!wildcard("web?", "web12", true));
        assert!(wildcard("*", "", true));
        assert!(pattern_list(&["a,b".into()], "b"));
        assert!(!pattern_list(&["*".into(), "!b".into()], "b"));
        assert_eq!(words(r#"ProxyCommand="ssh -W %h:%p gw""#), vec!["ProxyCommand", "ssh -W %h:%p gw"]);
        assert_eq!(words("Port=22"), vec!["Port", "22"]);
        assert_eq!(expand_tokens("%r@%h:%p %%", "h", 22, "u", "n"), "u@h:22 %");
    }
}
