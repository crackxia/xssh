//! Time-series performance sampling (`xssh perf`).

use super::*;
use crate::exec::run_raw;
use crate::files::sh;
use crate::ssh::Conn;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use xssh_core::error::{Error, Result};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Sample {
    pub t: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iowait_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub steal_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mem_used_pct: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swap_used_mb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load1: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_rx_kbs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub net_tx_kbs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_read_kbs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_write_kbs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_util_max_pct: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Stat {
    pub min: f64,
    pub avg: f64,
    pub p95: f64,
    pub max: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerfResult {
    pub host: String,
    pub os: String,
    pub interval_secs: f64,
    pub samples: usize,
    pub summary: BTreeMap<String, Stat>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub series: Vec<Sample>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Samples per run; longer durations sample less often instead of stopping early.
const MAX_SAMPLES: usize = 600;

/// (samples, interval) covering `duration`: the interval grows when it would need more than
/// `MAX_SAMPLES` samples.
fn plan(duration: Duration, interval: Duration) -> (usize, f64) {
    let mut interval_s = interval.as_secs_f64().max(0.5);
    let want = duration.as_secs_f64().max(interval_s);
    if want / interval_s > MAX_SAMPLES as f64 {
        interval_s = (want / MAX_SAMPLES as f64 * 10.0).ceil() / 10.0;
    }
    let n = ((want / interval_s).round() as usize).clamp(1, MAX_SAMPLES);
    (n, interval_s)
}

const LINUX: &str = r#"LC_ALL=C; export LC_ALL
i=0
while [ $i -le __N__ ]; do
  echo @@sample; date +%s.%N; grep '^cpu ' /proc/stat
  grep -E '^(MemTotal|MemAvailable|SwapTotal|SwapFree):' /proc/meminfo
  echo "load $(cut -d' ' -f1 /proc/loadavg)"
  tail -n +3 /proc/net/dev | sed 's/^/net /'
  sed 's/^/disk /' /proc/diskstats 2>/dev/null
  i=$((i+1)); [ $i -le __N__ ] && sleep __I__
done
echo @@end
"#;

const FREEBSD: &str = r#"LC_ALL=C; export LC_ALL
i=0
while [ $i -le __N__ ]; do
  echo @@sample; date +%s; echo "cp $(sysctl -n kern.cp_time)"
  echo "load $(sysctl -n vm.loadavg | awk '{print $2}')"
  echo "mem $(sysctl -n hw.pagesize) $(sysctl -n hw.physmem) $(sysctl -n vm.stats.vm.v_free_count) $(sysctl -n vm.stats.vm.v_inactive_count)"
  i=$((i+1)); [ $i -le __N__ ] && sleep __I__
done
echo @@end
"#;

pub async fn perf(conn: &Conn, host: &str, duration: Duration, interval: Duration, keep_series: bool) -> Result<PerfResult> {
    let os = detect_os(conn).await?;
    let (n, interval_s) = plan(duration, interval);
    let hint = (interval_s > interval.as_secs_f64().max(0.5) + 1e-9)
        .then(|| format!("sampled every {interval_s}s to keep {MAX_SAMPLES} samples over the full duration"));
    let timeout = Duration::from_secs_f64(n as f64 * interval_s + 60.0);
    let (samples, os_kind) = match os.as_str() {
        "linux" => {
            let script = LINUX.replace("__N__", &n.to_string()).replace("__I__", &format!("{interval_s}"));
            let out = run_raw(conn, &sh(&script), b"", false, timeout).await?;
            (linux_samples(&String::from_utf8_lossy(&out.stdout)), "linux")
        }
        "freebsd" | "dragonfly" => {
            let script = FREEBSD
                .replace("__N__", &n.to_string())
                .replace("__I__", &format!("{}", interval_s.ceil() as u64));
            let out = run_raw(conn, &sh(&script), b"", false, timeout).await?;
            (freebsd_samples(&String::from_utf8_lossy(&out.stdout)), "freebsd")
        }
        "darwin" => {
            let cmd = sh(&format!("top -l {} -s {} -n 0 2>/dev/null", n + 1, interval_s.ceil() as u64));
            let out = run_raw(conn, &cmd, b"", false, timeout).await?;
            (mac_samples(&String::from_utf8_lossy(&out.stdout), interval_s.ceil()), "darwin")
        }
        other => {
            return Err(Error::remote(format!("perf is not supported on '{other}' hosts"))
                .hint("use `xssh status` for a snapshot, or `xssh exec HOST -- vmstat 2 10`"));
        }
    };
    if samples.is_empty() {
        return Err(Error::remote("no samples collected"));
    }
    let summary = summarize(&samples);
    let findings = perf_findings(&summary);
    Ok(PerfResult {
        host: host.to_string(),
        os: os_kind.into(),
        interval_secs: interval_s,
        samples: samples.len(),
        summary,
        series: if keep_series { samples } else { vec![] },
        findings,
        hint,
    })
}

/// A named metric and how to read it from one sample.
type Metric = (&'static str, fn(&Sample) -> Option<f64>);
/// One FreeBSD sample: time, per-CPU tick counters, then two optional gauges.
type RawSample = (f64, Vec<u64>, Option<f64>, Option<f64>);

fn summarize(samples: &[Sample]) -> BTreeMap<String, Stat> {
    let mut m = BTreeMap::new();
    let metrics: [Metric; 11] = [
        ("cpu_pct", |s| s.cpu_pct),
        ("iowait_pct", |s| s.iowait_pct),
        ("steal_pct", |s| s.steal_pct),
        ("mem_used_pct", |s| s.mem_used_pct),
        ("swap_used_mb", |s| s.swap_used_mb),
        ("load1", |s| s.load1),
        ("net_rx_kbs", |s| s.net_rx_kbs),
        ("net_tx_kbs", |s| s.net_tx_kbs),
        ("disk_read_kbs", |s| s.disk_read_kbs),
        ("disk_write_kbs", |s| s.disk_write_kbs),
        ("disk_util_max_pct", |s| s.disk_util_max_pct),
    ];
    for (name, f) in metrics {
        let mut v: Vec<f64> = samples.iter().filter_map(f).collect();
        if v.is_empty() {
            continue;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let avg = v.iter().sum::<f64>() / v.len() as f64;
        let idx = ((v.len() as f64 * 0.95).ceil() as usize).clamp(1, v.len()) - 1;
        m.insert(
            name.to_string(),
            Stat {
                min: round1(v[0]),
                avg: round1(avg),
                p95: round1(v[idx]),
                max: round1(v[v.len() - 1]),
            },
        );
    }
    m
}

fn perf_findings(s: &BTreeMap<String, Stat>) -> Vec<Finding> {
    let mut f = vec![];
    let mut add = |sev, msg: String| {
        f.push(Finding {
            severity: sev,
            message: msg,
            suggestion: None,
        })
    };
    if let Some(c) = s.get("cpu_pct") {
        if c.avg >= 85.0 {
            add(Severity::Critical, format!("CPU saturated: avg {:.0}%, p95 {:.0}%", c.avg, c.p95));
        } else if c.p95 >= 90.0 {
            add(Severity::Warning, format!("CPU spikes: p95 {:.0}% (avg {:.0}%)", c.p95, c.avg));
        }
    }
    if let Some(w) = s.get("iowait_pct")
        && w.avg >= 15.0
    {
        add(Severity::Warning, format!("sustained IO wait: avg {:.0}%", w.avg));
    }
    if let Some(st) = s.get("steal_pct")
        && st.avg >= 5.0
    {
        add(
            Severity::Warning,
            format!("CPU steal avg {:.0}% (noisy neighbours / overcommitted host)", st.avg),
        );
    }
    if let Some(m) = s.get("mem_used_pct") {
        if m.max >= 95.0 {
            add(Severity::Critical, format!("memory nearly full (max {:.0}%)", m.max));
        }
        if m.max - m.min >= 10.0 {
            add(
                Severity::Info,
                format!(
                    "memory usage moved {:.0} points during sampling ({:.0}% → {:.0}%)",
                    m.max - m.min,
                    m.min,
                    m.max
                ),
            );
        }
    }
    if let Some(u) = s.get("disk_util_max_pct")
        && u.avg >= 80.0
    {
        add(Severity::Warning, format!("a disk is busy {:.0}% of the time on average", u.avg));
    }
    f.sort_by_key(|x| std::cmp::Reverse(x.severity));
    f
}

fn linux_samples(out: &str) -> Vec<Sample> {
    struct Raw {
        t: f64,
        cpu: Vec<u64>,
        mem: HashMap<String, u64>,
        load: Option<f64>,
        net: (u64, u64),
        disk: HashMap<String, (u64, u64, u64)>,
    }
    let mut raws: Vec<Raw> = vec![];
    for block in out.split("@@sample\n").skip(1) {
        let mut lines = block.lines();
        let t = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0.0);
        let mut r = Raw {
            t,
            cpu: vec![],
            mem: HashMap::new(),
            load: None,
            net: (0, 0),
            disk: HashMap::new(),
        };
        for l in lines {
            if l.starts_with("@@") {
                break;
            }
            if let Some(rest) = l.strip_prefix("cpu ") {
                r.cpu = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            } else if let Some(rest) = l.strip_prefix("load ") {
                r.load = rest.trim().parse().ok();
            } else if let Some(rest) = l.strip_prefix("net ") {
                if let Some((iface, v)) = rest.split_once(':')
                    && iface.trim() != "lo"
                {
                    let c: Vec<u64> = v.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                    if c.len() >= 9 {
                        r.net.0 += c[0];
                        r.net.1 += c[8];
                    }
                }
            } else if let Some(rest) = l.strip_prefix("disk ") {
                let c: Vec<&str> = rest.split_whitespace().collect();
                if c.len() >= 14 {
                    let name = c[2];
                    if !linux::is_pseudo_disk(name) {
                        let n = |i: usize| c[i].parse::<u64>().unwrap_or(0);
                        r.disk.insert(name.to_string(), (n(5), n(9), n(12)));
                    }
                }
            } else if let Some((k, v)) = l.split_once(':')
                && let Some(n) = v.split_whitespace().next().and_then(|x| x.parse().ok())
            {
                r.mem.insert(k.to_string(), n);
            }
        }
        raws.push(r);
    }
    let mut out = vec![];
    for w in raws.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        let dt = (b.t - a.t).max(0.001);
        let mut s = Sample {
            t: round1(b.t - raws[0].t),
            ..Default::default()
        };
        if a.cpu.len() >= 8 && b.cpu.len() >= 8 {
            let d: Vec<f64> = (0..8).map(|i| b.cpu[i].saturating_sub(a.cpu[i]) as f64).collect();
            let tot: f64 = d.iter().sum();
            if tot > 0.0 {
                s.cpu_pct = Some(round1((tot - d[3] - d[4]) * 100.0 / tot));
                s.iowait_pct = Some(round1(d[4] * 100.0 / tot));
                s.steal_pct = Some(round1(d[7] * 100.0 / tot));
            }
        }
        if let (Some(t), Some(av)) = (b.mem.get("MemTotal"), b.mem.get("MemAvailable")) {
            s.mem_used_pct = Some(round1(t.saturating_sub(*av) as f64 * 100.0 / (*t).max(1) as f64));
        }
        if let (Some(t), Some(f)) = (b.mem.get("SwapTotal"), b.mem.get("SwapFree")) {
            s.swap_used_mb = Some(round1(t.saturating_sub(*f) as f64 / 1024.0));
        }
        s.load1 = b.load;
        s.net_rx_kbs = Some(round1(b.net.0.saturating_sub(a.net.0) as f64 / 1024.0 / dt));
        s.net_tx_kbs = Some(round1(b.net.1.saturating_sub(a.net.1) as f64 / 1024.0 / dt));
        let (mut rd, mut wr, mut util) = (0.0, 0.0, 0.0f64);
        for (name, (r2, w2, io2)) in &b.disk {
            if linux::is_partition(name) {
                continue;
            }
            if let Some((r1, w1, io1)) = a.disk.get(name) {
                // LVM/RAID devices repeat their disks' IO: busy-ness yes, totals no.
                if !linux::is_stacked(name) {
                    rd += r2.saturating_sub(*r1) as f64 * 512.0 / 1024.0 / dt;
                    wr += w2.saturating_sub(*w1) as f64 * 512.0 / 1024.0 / dt;
                }
                util = util.max((io2.saturating_sub(*io1) as f64 / (dt * 1000.0) * 100.0).min(100.0));
            }
        }
        s.disk_read_kbs = Some(round1(rd));
        s.disk_write_kbs = Some(round1(wr));
        s.disk_util_max_pct = Some(round1(util));
        out.push(s);
    }
    out
}

fn freebsd_samples(out: &str) -> Vec<Sample> {
    let mut raws: Vec<RawSample> = vec![];
    for block in out.split("@@sample\n").skip(1) {
        let mut lines = block.lines();
        let t = lines.next().and_then(|l| l.trim().parse().ok()).unwrap_or(0.0);
        let (mut cp, mut load, mut mem) = (vec![], None, None);
        for l in lines {
            if let Some(r) = l.strip_prefix("cp ") {
                cp = r.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            } else if let Some(r) = l.strip_prefix("load ") {
                load = r.trim().parse().ok();
            } else if let Some(r) = l.strip_prefix("mem ") {
                let v: Vec<f64> = r.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if v.len() == 4 && v[1] > 0.0 {
                    mem = Some(round1(100.0 - (v[2] + v[3]) * v[0] * 100.0 / v[1]));
                }
            }
        }
        raws.push((t, cp, load, mem));
    }
    raws.windows(2)
        .map(|w| {
            let (a, b) = (&w[0], &w[1]);
            let mut s = Sample {
                t: b.0 - raws[0].0,
                load1: b.2,
                mem_used_pct: b.3,
                ..Default::default()
            };
            if a.1.len() >= 5 && b.1.len() >= 5 {
                let d: Vec<f64> = (0..5).map(|i| b.1[i].saturating_sub(a.1[i]) as f64).collect();
                let tot: f64 = d.iter().sum();
                if tot > 0.0 {
                    s.cpu_pct = Some(round1((tot - d[4]) * 100.0 / tot));
                }
            }
            s
        })
        .collect()
}

/// `top -l N -s I -n 0`: skip the first sample (it is an average since boot).
fn mac_samples(out: &str, interval: f64) -> Vec<Sample> {
    let mut v = vec![];
    let mut cur: Option<Sample> = None;
    let mut idx = 0f64;
    for l in out.lines() {
        if l.starts_with("Processes:") {
            if let Some(s) = cur.take() {
                v.push(s);
            }
            cur = Some(Sample {
                t: idx * interval,
                ..Default::default()
            });
            idx += 1.0;
        } else if let Some(s) = cur.as_mut() {
            if let Some(r) = l.strip_prefix("Load Avg:") {
                s.load1 = r.split(',').next().and_then(|x| x.trim().parse().ok());
            } else if l.starts_with("CPU usage:") {
                let idle = l
                    .split(',')
                    .find(|p| p.contains("idle"))
                    .and_then(|p| p.split_whitespace().find(|t| t.ends_with('%')))
                    .and_then(num::<f64>);
                s.cpu_pct = idle.map(|i| round1(100.0 - i));
            }
        }
    }
    if let Some(s) = cur {
        v.push(s);
    }
    if v.len() > 1 {
        v.remove(0);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linux_series() {
        let out = "@@sample\n100.0\ncpu  100 0 50 800 50 0 0 0 0 0\nMemTotal: 1000 kB\nMemAvailable: 500 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\nload 0.5\nnet   eth0: 1000 0 0 0 0 0 0 0 1000 0 0 0 0 0 0 0\ndisk    8 0 sda 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n@@sample\n101.0\ncpu  200 0 100 1600 100 0 0 0 0 0\nMemTotal: 1000 kB\nMemAvailable: 400 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\nload 0.7\nnet   eth0: 103400 0 0 0 0 0 0 0 1000 0 0 0 0 0 0 0\ndisk    8 0 sda 0 0 2048 0 0 0 0 0 0 1000 0 0 0 0\n@@end\n";
        let s = linux_samples(out);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].cpu_pct, Some(15.0));
        assert_eq!(s[0].mem_used_pct, Some(60.0));
        assert_eq!(s[0].net_rx_kbs, Some(100.0));
        assert_eq!(s[0].disk_read_kbs, Some(1024.0));
        assert_eq!(s[0].disk_util_max_pct, Some(100.0));
        let sum = summarize(&s);
        assert_eq!(sum["cpu_pct"].avg, 15.0);
    }

    #[test]
    fn long_durations_keep_full_coverage() {
        let (n, i) = plan(Duration::from_secs(20), Duration::from_secs(2));
        assert_eq!((n, i), (10, 2.0));
        let (n, i) = plan(Duration::from_secs(3600), Duration::from_secs(1));
        assert_eq!((n, i), (600, 6.0));
        assert!(n as f64 * i >= 3600.0);
    }

    #[test]
    fn stacked_disks_are_not_double_counted() {
        let out = "@@sample\n100.0\ndisk 8 0 sda 0 0 0 0 0 0 0 0 0 0 0 0 0 0\ndisk 253 0 dm-0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\ndisk 252 0 zram0 0 0 0 0 0 0 0 0 0 0 0 0 0 0\n@@sample\n101.0\ndisk 8 0 sda 0 0 2048 0 0 0 0 0 0 0 0 0 0 0\ndisk 253 0 dm-0 0 0 2048 0 0 0 0 0 0 0 0 0 0 0\ndisk 252 0 zram0 0 0 9999 0 0 0 0 0 0 0 0 0 0 0\n@@end\n";
        let s = linux_samples(out);
        assert_eq!(s[0].disk_read_kbs, Some(1024.0));
    }

    #[test]
    fn mac_series() {
        let out = "Processes: 1\nLoad Avg: 1.0, 1.0, 1.0\nCPU usage: 1% user, 1% sys, 98% idle\nProcesses: 2\nLoad Avg: 2.5, 1.0, 1.0\nCPU usage: 30.0% user, 10.0% sys, 60.0% idle\n";
        let s = mac_samples(out, 1.0);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].cpu_pct, Some(40.0));
        assert_eq!(s[0].load1, Some(2.5));
    }
}
