//! Host status, performance sampling and diagnosis.
//!
//! A POSIX sh script is run in a single exec; it prints `@@section` markers
//! followed by raw command output, which is parsed here into structured data.

pub mod bsd;
pub mod linux;
pub mod perf;

use crate::exec::{self, run_raw};
use crate::files::{SudoCtx, sh};
use crate::ssh::Conn;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
use xssh_core::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OsInfo {
    /// linux | darwin | freebsd | ...
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub kernel: String,
    pub arch: String,
    pub hostname: String,
    pub uptime_secs: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Cpu {
    pub cores: u32,
    pub usage_pct: f64,
    pub user_pct: f64,
    pub system_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iowait_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steal_pct: Option<f64>,
    pub idle_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Mem {
    pub total_mb: u64,
    pub used_mb: u64,
    pub available_mb: u64,
    pub used_pct: f64,
    pub swap_total_mb: u64,
    pub swap_used_mb: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Disk {
    pub mount: String,
    pub fs: String,
    pub size_gb: f64,
    pub used_gb: f64,
    pub avail_gb: f64,
    pub used_pct: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inodes_used_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiskIo {
    pub device: String,
    pub read_kbs: f64,
    pub write_kbs: f64,
    pub util_pct: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Net {
    pub iface: String,
    pub rx_kbs: f64,
    pub tx_kbs: f64,
    pub rx_total_mb: f64,
    pub tx_total_mb: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Proc {
    pub pid: u32,
    pub user: String,
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub rss_mb: f64,
    pub elapsed_secs: u64,
    pub stat: String,
    pub command: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Port {
    pub addr: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Container {
    pub name: String,
    pub status: String,
    pub image: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Status {
    pub host: String,
    pub os: OsInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu: Option<Cpu>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<[f64; 3]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem: Option<Mem>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disks: Vec<Disk>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disk_io: Vec<DiskIo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub net: Vec<Net>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_cpu: Vec<Proc>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub top_mem: Vec<Proc>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub proc_states: HashMap<String, u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<Port>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failed_units: Vec<String>,
    /// systemd was queried (so an empty `failed_units` means none failed).
    #[serde(default)]
    pub services_checked: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub containers: Vec<Container>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pressure: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub oom_events: Vec<String>,
    /// Time span `oom_events` covers: `24h` (journal) or `boot` (dmesg).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oom_window: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// What this snapshot could not see and how to get it (e.g. run with --sudo).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub collected_at: String,
    pub duration_ms: u64,
}

/// Split script output into `@@name` sections.
pub fn sections(out: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut cur: Option<String> = None;
    let mut buf = String::new();
    for line in out.lines() {
        if let Some(name) = line.strip_prefix("@@") {
            if let Some(c) = cur.take() {
                map.insert(c, std::mem::take(&mut buf));
            }
            cur = Some(name.trim().to_string());
        } else if cur.is_some() {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if let Some(c) = cur {
        map.insert(c, buf);
    }
    map
}

pub(crate) fn script(src: &str, top: usize) -> String {
    src.replace('\r', "").replace("__TOP1__", &(top + 1).to_string())
}

/// Detect the remote OS family (`uname -s`), lowercased.
pub async fn detect_os(conn: &Conn) -> Result<String> {
    let out = run_raw(conn, "uname -s", b"", false, Duration::from_secs(20)).await?;
    let s = String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err(Error::remote(
            "cannot determine remote OS (uname failed); only Unix-like hosts are supported",
        ));
    }
    Ok(s)
}

/// Run a probe script (POSIX sh, as root with `sudo`), keeping partial output on timeout.
pub(crate) async fn run_script(conn: &Conn, script: &str, sudo: Option<&SudoCtx<'_>>, timeout: Duration) -> Result<exec::RawOutput> {
    let prepared = exec::prepare(
        &sh(script),
        None,
        &[],
        &[],
        sudo.filter(|s| s.enabled).map(|s| s.password),
        b"",
        &[],
    );
    let mut out = run_raw(conn, &prepared.command, &prepared.stdin, false, timeout).await?;
    if !prepared.redact.is_empty() {
        out.stderr = xssh_core::text::redact(&String::from_utf8_lossy(&out.stderr), &prepared.redact).into_bytes();
    }
    Ok(out)
}

pub async fn status(conn: &Conn, host: &str, top: usize, sudo: Option<&SudoCtx<'_>>) -> Result<Status> {
    let start = std::time::Instant::now();
    let os = detect_os(conn).await?;
    type Parser = fn(&HashMap<String, String>, &str) -> Status;
    let (src, parse): (&str, Parser) = match os.as_str() {
        "linux" => (include_str!("linux.sh"), linux::parse),
        "darwin" | "freebsd" | "openbsd" | "netbsd" | "dragonfly" => (include_str!("bsd.sh"), bsd::parse),
        other => {
            return Err(Error::remote(format!("status is not supported on '{other}' hosts")).hint("use `xssh exec` with native tools"));
        }
    };
    let out = run_script(conn, &script(src, top), sudo, Duration::from_secs(60)).await?;
    let text = String::from_utf8_lossy(&out.stdout);
    let secs = sections(&text);
    if out.exit_code == Some(126) && sudo.is_some_and(|s| s.enabled) && !secs.contains_key("uname") {
        return Err(
            Error::auth(format!("status --sudo: {}", String::from_utf8_lossy(&out.stderr).trim()))
                .hint("store the sudo password with `xssh host set-password <host> --sudo`"),
        );
    }
    // Every probe runs under `timeout`; a whole-script timeout still returns what was collected.
    let partial = !secs.contains_key("end");
    if partial && !secs.contains_key("uname") {
        return Err(Error::remote(format!(
            "status script did not complete: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let mut st = parse(&secs, &os);
    if partial {
        let last = text
            .lines()
            .filter_map(|l| l.strip_prefix("@@"))
            .next_back()
            .unwrap_or("?")
            .to_string();
        st.notes.push(format!(
            "the status script stopped during '{last}' (timed out); later sections are missing"
        ));
    }
    let user = secs.get("user").map(|u| u.trim().to_string()).unwrap_or_default();
    if !user.is_empty() && user != "root" && os == "linux" {
        if !st.ports.is_empty() && st.ports.iter().all(|p| p.process.is_none()) {
            st.notes.push("ports show no owning process without root: add --sudo".into());
        }
        if st.oom_window.is_none() {
            st.notes.push("kernel log not readable, OOM kills unknown: add --sudo".into());
        }
    }
    st.host = host.to_string();
    st.top_cpu.truncate(top);
    st.top_mem.truncate(top);
    st.findings = findings(&st);
    st.collected_at = xssh_store::audit::now_rfc3339();
    st.duration_ms = start.elapsed().as_millis() as u64;
    Ok(st)
}

/// Keep only the requested sections (comma separated).
pub fn filter(mut st: Status, only: &[String]) -> Status {
    if only.is_empty() {
        return st;
    }
    let has = |k: &str| only.iter().any(|o| o == k);
    if !has("cpu") {
        st.cpu = None;
        st.load = None;
    }
    if !has("mem") {
        st.mem = None;
    }
    if !has("disk") {
        st.disks.clear();
        st.disk_io.clear();
    }
    if !has("net") {
        st.net.clear();
    }
    if !has("proc") {
        st.top_cpu.clear();
        st.top_mem.clear();
        st.proc_states.clear();
    }
    if !has("ports") {
        st.ports.clear();
    }
    if !has("services") {
        st.failed_units.clear();
        st.services_checked = false;
    }
    if !has("docker") {
        st.containers.clear();
    }
    // Only report findings about the sections that were asked for.
    st.findings = findings(&st);
    st
}

pub fn findings(st: &Status) -> Vec<Finding> {
    let mut f = vec![];
    let mut push = |sev, msg: String, sug: Option<&str>| {
        f.push(Finding {
            severity: sev,
            message: msg,
            suggestion: sug.map(String::from),
        })
    };
    for d in &st.disks {
        if d.used_pct >= 95.0 {
            push(
                Severity::Critical,
                format!("filesystem {} is {:.0}% full ({:.1} GB free)", d.mount, d.used_pct, d.avail_gb),
                Some("find large dirs: du -xh <mount> --max-depth=2 2>/dev/null | sort -h | tail -20"),
            );
        } else if d.used_pct >= 85.0 {
            push(
                Severity::Warning,
                format!("filesystem {} is {:.0}% full", d.mount, d.used_pct),
                Some("check growth: du -xh <mount> --max-depth=2 | sort -h | tail"),
            );
        }
        if let Some(i) = d.inodes_used_pct
            && i >= 90.0
        {
            push(
                Severity::Warning,
                format!("filesystem {} uses {:.0}% of inodes", d.mount, i),
                Some("look for dirs with many small files: find <mount> -xdev -type d -size +1M"),
            );
        }
    }
    if let Some(m) = &st.mem {
        let avail_pct = if m.total_mb > 0 {
            m.available_mb as f64 * 100.0 / m.total_mb as f64
        } else {
            100.0
        };
        if avail_pct < 5.0 {
            push(
                Severity::Critical,
                format!("memory nearly exhausted: {} MB available of {} MB", m.available_mb, m.total_mb),
                Some("see top_mem processes; check for leaks or OOM kills"),
            );
        } else if avail_pct < 10.0 {
            push(Severity::Warning, format!("low available memory: {:.1}%", avail_pct), None);
        }
        if m.swap_total_mb > 0 && m.swap_used_mb * 2 > m.swap_total_mb {
            push(
                Severity::Warning,
                format!("swap usage high: {} of {} MB", m.swap_used_mb, m.swap_total_mb),
                Some("vmstat 1 5 (watch si/so for active swapping)"),
            );
        }
    }
    if let (Some(load), Some(cpu)) = (&st.load, &st.cpu) {
        let cores = cpu.cores.max(1) as f64;
        if load[0] / cores > 2.0 {
            push(
                Severity::Critical,
                format!(
                    "load average {:.2} is {:.1}x the core count ({})",
                    load[0],
                    load[0] / cores,
                    cpu.cores
                ),
                Some("ps -eo pid,stat,pcpu,comm --sort=-pcpu | head (D state = blocked on IO)"),
            );
        } else if load[0] / cores > 1.0 {
            push(
                Severity::Warning,
                format!("load average {:.2} exceeds core count ({})", load[0], cpu.cores),
                None,
            );
        }
    }
    if let Some(cpu) = &st.cpu {
        if cpu.usage_pct >= 90.0 {
            push(
                Severity::Warning,
                format!("CPU busy: {:.0}% used", cpu.usage_pct),
                Some("see top_cpu processes"),
            );
        }
        if cpu.iowait_pct.unwrap_or(0.0) >= 20.0 {
            push(
                Severity::Warning,
                format!("high IO wait: {:.0}%", cpu.iowait_pct.unwrap_or(0.0)),
                Some("iostat -x 1 3 / check disk_io util"),
            );
        }
        if cpu.steal_pct.unwrap_or(0.0) >= 10.0 {
            push(
                Severity::Warning,
                format!("CPU steal {:.0}%: the hypervisor is overcommitted", cpu.steal_pct.unwrap_or(0.0)),
                None,
            );
        }
    }
    for d in &st.disk_io {
        if d.util_pct >= 90.0 {
            push(
                Severity::Warning,
                format!("disk {} is {:.0}% busy", d.device, d.util_pct),
                Some("iotop -o / pidstat -d 1 to find the IO-heavy process"),
            );
        }
    }
    if let Some(z) = st.proc_states.get("Z")
        && *z >= 10
    {
        push(
            Severity::Warning,
            format!("{z} zombie processes"),
            Some("ps -eo pid,ppid,stat,comm | awk '$3 ~ /Z/' (fix/restart the parent)"),
        );
    }
    if let Some(d) = st.proc_states.get("D")
        && *d >= 5
    {
        push(
            Severity::Warning,
            format!("{d} processes in uninterruptible IO wait (D state)"),
            None,
        );
    }
    if !st.failed_units.is_empty() {
        push(
            Severity::Warning,
            format!("{} failed systemd unit(s): {}", st.failed_units.len(), st.failed_units.join(", ")),
            Some("systemctl status <unit>; journalctl -u <unit> -n 50"),
        );
    }
    let unhealthy: Vec<&str> = st
        .containers
        .iter()
        .filter(|c| c.status.contains("unhealthy"))
        .map(|c| c.name.as_str())
        .collect();
    if !unhealthy.is_empty() {
        push(
            Severity::Warning,
            format!("unhealthy container(s): {}", unhealthy.join(", ")),
            Some("docker inspect --format '{{json .State.Health}}' <name>; docker logs --tail 50 <name>"),
        );
    }
    let restarting: Vec<&str> = st
        .containers
        .iter()
        .filter(|c| c.status.starts_with("Restarting"))
        .map(|c| c.name.as_str())
        .collect();
    if !restarting.is_empty() {
        push(
            Severity::Warning,
            format!("container(s) in a restart loop: {}", restarting.join(", ")),
            Some("docker logs --tail 50 <name>"),
        );
    }
    if !st.oom_events.is_empty() {
        let (when, how) = match st.oom_window.as_deref() {
            Some("boot") => ("since boot", "dmesg -T | grep -i -E 'oom|killed process'"),
            _ => ("in the last 24h", "journalctl -k --since=-24h | grep -i oom"),
        };
        push(
            Severity::Critical,
            format!("OOM killer fired {when} ({} event(s))", st.oom_events.len()),
            Some(how),
        );
    }
    for p in &st.pressure {
        // "memory some avg10=12.34 avg60=... "
        if let Some(v) = p
            .split_whitespace()
            .find_map(|t| t.strip_prefix("avg10="))
            .and_then(|v| v.parse::<f64>().ok())
            && v >= 20.0
        {
            let res = p.split_whitespace().next().unwrap_or("?");
            push(Severity::Warning, format!("{res} pressure (PSI some avg10) is {v:.1}%"), None);
        }
    }
    if st.os.uptime_secs > 0 && st.os.uptime_secs < 600 {
        push(
            Severity::Info,
            format!("host rebooted {} minutes ago", st.os.uptime_secs / 60),
            Some("last -x | head; journalctl -b -1 -n 50 (previous boot)"),
        );
    }
    f.sort_by_key(|x| std::cmp::Reverse(x.severity));
    f
}

// ---------- shared parsing helpers ----------

pub(crate) fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    s.trim().trim_end_matches('%').trim_end_matches('.').parse().ok()
}

pub(crate) fn parse_df(s: &str, inodes: Option<&str>) -> Vec<Disk> {
    let mut ipct: HashMap<String, f64> = HashMap::new();
    if let Some(i) = inodes {
        for l in i.lines().skip(1) {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() >= 6
                && let Some(p) = num::<f64>(c[4])
            {
                ipct.insert(c[5..].join(" "), p);
            }
        }
    }
    let mut out = vec![];
    for l in s.lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() < 6 || c[0] == "Filesystem" {
            continue;
        }
        let (Some(size), Some(used), Some(avail)) = (num::<f64>(c[1]), num::<f64>(c[2]), num::<f64>(c[3])) else {
            continue;
        };
        if size <= 0.0 {
            continue;
        }
        let mount = c[5..].join(" ");
        let gb = |k: f64| (k / 1024.0 / 1024.0 * 10.0).round() / 10.0;
        let pct = num::<f64>(c[4]).unwrap_or_else(|| if used + avail > 0.0 { used * 100.0 / (used + avail) } else { 0.0 });
        out.push(Disk {
            inodes_used_pct: ipct.get(&mount).copied(),
            mount,
            fs: c[0].to_string(),
            size_gb: gb(size),
            used_gb: gb(used),
            avail_gb: gb(avail),
            used_pct: pct,
        });
    }
    out
}

/// Parse `ps -o pid,user,pcpu,pmem,rss,etime(s),stat,comm` output.
pub(crate) fn parse_ps(s: &str) -> Vec<Proc> {
    let mut out = vec![];
    for l in s.lines().skip(1) {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() < 8 {
            continue;
        }
        let Some(pid) = num::<u32>(c[0]) else { continue };
        out.push(Proc {
            pid,
            user: c[1].to_string(),
            cpu_pct: num(c[2]).unwrap_or(0.0),
            mem_pct: num(c[3]).unwrap_or(0.0),
            rss_mb: (num::<f64>(c[4]).unwrap_or(0.0) / 1024.0 * 10.0).round() / 10.0,
            elapsed_secs: parse_etime(c[5]),
            stat: c[6].to_string(),
            command: c[7..].join(" "),
        });
    }
    out
}

/// `etimes` (seconds) or `etime` ([[dd-]hh:]mm:ss).
pub(crate) fn parse_etime(s: &str) -> u64 {
    if let Ok(n) = s.parse::<u64>() {
        return n;
    }
    let (days, rest) = match s.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().unwrap_or(0), r),
        None => (0, s),
    };
    let parts: Vec<u64> = rest.split(':').filter_map(|p| p.parse().ok()).collect();
    let hms = match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        [s] => *s,
        _ => 0,
    };
    days * 86400 + hms
}

pub(crate) fn parse_proc_states(s: &str) -> HashMap<String, u64> {
    let mut m = HashMap::new();
    for l in s.lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() == 2
            && let Some(n) = num::<u64>(c[0])
        {
            m.insert(c[1].to_string(), n);
        }
    }
    m
}

pub(crate) fn parse_docker(s: &str) -> Vec<Container> {
    s.lines()
        .filter_map(|l| {
            let mut p = l.splitn(3, '|');
            Some(Container {
                name: p.next()?.to_string(),
                status: p.next()?.to_string(),
                image: p.next()?.to_string(),
            })
        })
        .collect()
}

pub(crate) fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_split() {
        let s = sections("junk\n@@a\n1\n2\n@@b\n@@c\nx\n@@end\n");
        assert_eq!(s["a"], "1\n2\n");
        assert_eq!(s["b"], "");
        assert_eq!(s["c"], "x\n");
        assert!(s.contains_key("end"));
    }

    #[test]
    fn etime() {
        assert_eq!(parse_etime("05:03"), 303);
        assert_eq!(parse_etime("1-02:00:00"), 93600);
        assert_eq!(parse_etime("123"), 123);
    }

    #[test]
    fn df() {
        let d = parse_df(
            "Filesystem     1024-blocks      Used Available Capacity Mounted on\n/dev/sda1 100000000 96000000 4000000 96% /\n",
            Some("Filesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 6000000 5700000 300000 95% /\n"),
        );
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].used_pct, 96.0);
        assert_eq!(d[0].inodes_used_pct, Some(95.0));
        let st = Status {
            disks: d,
            ..Default::default()
        };
        let f = findings(&st);
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f.len(), 2);
    }
}
