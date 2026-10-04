//! Parser for the Linux status script output.

use super::*;
use std::collections::HashMap;

pub fn parse(s: &HashMap<String, String>, kind: &str) -> Status {
    let get = |k: &str| s.get(k).map(String::as_str).unwrap_or("");
    let mut st = Status {
        os: os_info(get("uname"), get("osrel"), get("uptime"), kind),
        ..Default::default()
    };
    let cores = num::<u32>(get("nproc").lines().next().unwrap_or("")).unwrap_or(1);
    let dt = match (num::<f64>(get("t1")), num::<f64>(get("t2"))) {
        (Some(a), Some(b)) if b > a => b - a,
        _ => 1.0,
    };
    st.cpu = cpu(get("stat1"), get("stat2"), cores);
    let l: Vec<f64> = get("loadavg").split_whitespace().take(3).filter_map(|x| x.parse().ok()).collect();
    if l.len() == 3 {
        st.load = Some([l[0], l[1], l[2]]);
    }
    st.mem = meminfo(get("meminfo"));
    st.disks = parse_df(get("df"), Some(get("dfi")));
    st.disk_io = diskstats(get("disk1"), get("disk2"), dt);
    st.net = netdev(get("net1"), get("net2"), dt);
    st.top_cpu = parse_ps(get("pscpu"));
    st.top_mem = parse_ps(get("psmem"));
    st.proc_states = parse_proc_states(get("procs"));
    st.ports = ports(get("ports"));
    st.services_checked = get("failed").lines().any(|l| l == "SYSTEMD");
    st.failed_units = get("failed")
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .filter(|u| u.contains('.'))
        .map(String::from)
        .collect();
    st.containers = parse_docker(get("docker"));
    st.pressure = get("psi").lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    st.oom_events = get("oom").lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    st.oom_window = match get("oomsrc").trim() {
        "journal" => Some("24h".into()),
        "dmesg" => Some("boot".into()),
        _ => None,
    };
    st
}

pub(crate) fn os_info(uname: &str, osrel: &str, uptime: &str, kind: &str) -> OsInfo {
    // uname -srmn: "Linux host 5.15.0-1 x86_64"
    let u: Vec<&str> = uname.split_whitespace().collect();
    let pretty = osrel
        .lines()
        .find_map(|l| l.strip_prefix("PRETTY_NAME="))
        .map(|v| v.trim_matches('"').to_string());
    OsInfo {
        kind: kind.to_string(),
        name: pretty,
        hostname: u.get(1).unwrap_or(&"").to_string(),
        kernel: u.get(2).unwrap_or(&"").to_string(),
        arch: u.get(3).unwrap_or(&"").to_string(),
        uptime_secs: uptime.split_whitespace().next().and_then(|x| x.parse::<f64>().ok()).unwrap_or(0.0) as u64,
    }
}

fn cpu_fields(line: &str) -> Vec<u64> {
    line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect()
}

pub(crate) fn cpu(a: &str, b: &str, cores: u32) -> Option<Cpu> {
    let x = cpu_fields(a.lines().next()?);
    let y = cpu_fields(b.lines().next()?);
    if x.len() < 4 || y.len() < 4 {
        return None;
    }
    // user nice system idle iowait irq softirq steal (guest time is included in user)
    let d: Vec<f64> = (0..8.min(x.len()).min(y.len())).map(|i| y[i].saturating_sub(x[i]) as f64).collect();
    let total: f64 = d.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let p = |v: f64| round1(v * 100.0 / total);
    let at = |i: usize| d.get(i).copied().unwrap_or(0.0);
    let idle = at(3) + at(4);
    Some(Cpu {
        cores,
        usage_pct: p(total - idle),
        user_pct: p(at(0) + at(1)),
        system_pct: p(at(2) + at(5) + at(6)),
        iowait_pct: Some(p(at(4))),
        steal_pct: Some(p(at(7))),
        idle_pct: p(at(3)),
    })
}

pub(crate) fn meminfo(s: &str) -> Option<Mem> {
    let mut m: HashMap<&str, u64> = HashMap::new();
    for l in s.lines() {
        let mut it = l.split_whitespace();
        if let (Some(k), Some(v)) = (it.next(), it.next())
            && let Ok(v) = v.parse()
        {
            m.insert(k.trim_end_matches(':'), v);
        }
    }
    let total = *m.get("MemTotal")?;
    let avail = m
        .get("MemAvailable")
        .copied()
        .unwrap_or_else(|| m.get("MemFree").unwrap_or(&0) + m.get("Buffers").unwrap_or(&0) + m.get("Cached").unwrap_or(&0));
    let swap_total = m.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = m.get("SwapFree").copied().unwrap_or(0);
    Some(Mem {
        total_mb: total / 1024,
        used_mb: total.saturating_sub(avail) / 1024,
        available_mb: avail / 1024,
        used_pct: round1(total.saturating_sub(avail) as f64 * 100.0 / total.max(1) as f64),
        swap_total_mb: swap_total / 1024,
        swap_used_mb: swap_total.saturating_sub(swap_free) / 1024,
    })
}

/// Not a disk: loop devices, RAM disks (zram too), optical and floppy drives.
pub(crate) fn is_pseudo_disk(name: &str) -> bool {
    ["loop", "ram", "zram", "sr", "fd"].iter().any(|p| name.starts_with(p))
}

/// Built on top of other disks (device mapper/LVM/LUKS, md RAID, drbd, bcache): its IO is
/// already counted on the disks below, so totals must skip it.
pub(crate) fn is_stacked(name: &str) -> bool {
    ["dm-", "md", "drbd", "bcache"].iter().any(|p| name.starts_with(p))
}

pub(crate) fn is_partition(name: &str) -> bool {
    let re_like = |prefixes: &[&str]| {
        prefixes.iter().any(|p| {
            name.strip_prefix(p).is_some_and(|rest| {
                let letters = rest.trim_end_matches(|c: char| c.is_ascii_digit());
                !letters.is_empty() && letters.len() < rest.len() && letters.chars().all(|c| c.is_ascii_lowercase())
            })
        })
    };
    re_like(&["sd", "vd", "xvd", "hd"])
        || ((name.starts_with("nvme") || name.starts_with("mmcblk"))
            && name.contains('p')
            && name.rsplit('p').next().is_some_and(|x| x.chars().all(|c| c.is_ascii_digit())))
}

pub(crate) fn diskstats(a: &str, b: &str, dt: f64) -> Vec<DiskIo> {
    let parse = |s: &str| -> HashMap<String, Vec<u64>> {
        s.lines()
            .filter_map(|l| {
                let c: Vec<&str> = l.split_whitespace().collect();
                if c.len() < 14 {
                    return None;
                }
                Some((c[2].to_string(), c[3..14].iter().filter_map(|x| x.parse().ok()).collect()))
            })
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    let mut out = vec![];
    for (name, v2) in &y {
        if is_pseudo_disk(name) || is_partition(name) {
            continue;
        }
        let Some(v1) = x.get(name) else { continue };
        if v1.len() < 10 || v2.len() < 10 {
            continue;
        }
        let d = |i: usize| v2[i].saturating_sub(v1[i]) as f64;
        // fields: reads, rmerged, sectors_read, ms_read, writes, wmerged, sectors_written, ms_write, in_flight, ms_io
        let read_kbs = d(2) * 512.0 / 1024.0 / dt;
        let write_kbs = d(6) * 512.0 / 1024.0 / dt;
        let util = (d(9) / (dt * 1000.0) * 100.0).min(100.0);
        out.push(DiskIo {
            device: name.clone(),
            read_kbs: round1(read_kbs),
            write_kbs: round1(write_kbs),
            util_pct: round1(util),
        });
    }
    out.sort_by(|a, b| {
        b.util_pct
            .partial_cmp(&a.util_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.device.cmp(&b.device))
    });
    out
}

pub(crate) fn netdev(a: &str, b: &str, dt: f64) -> Vec<Net> {
    let parse = |s: &str| -> HashMap<String, (u64, u64)> {
        s.lines()
            .filter_map(|l| {
                let (iface, rest) = l.split_once(':')?;
                let c: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
                if c.len() < 9 {
                    return None;
                }
                Some((iface.trim().to_string(), (c[0], c[8])))
            })
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    let mut out: Vec<Net> = y
        .iter()
        .filter(|(n, _)| n.as_str() != "lo")
        .filter_map(|(n, (rx2, tx2))| {
            let (rx1, tx1) = x.get(n)?;
            Some(Net {
                iface: n.clone(),
                rx_kbs: round1(rx2.saturating_sub(*rx1) as f64 / 1024.0 / dt),
                tx_kbs: round1(tx2.saturating_sub(*tx1) as f64 / 1024.0 / dt),
                rx_total_mb: round1(*rx2 as f64 / 1048576.0),
                tx_total_mb: round1(*tx2 as f64 / 1048576.0),
            })
        })
        .filter(|n| n.rx_total_mb > 0.0 || n.tx_total_mb > 0.0)
        .collect();
    trim_ifaces(&mut out);
    out
}

/// Drop idle virtual interfaces (containers/bridges) and keep the busiest few.
pub(crate) fn trim_ifaces(v: &mut Vec<Net>) {
    const VIRTUAL: [&str; 10] = ["veth", "br-", "docker", "virbr", "cali", "flannel", "cni", "lxc", "vnet", "tap"];
    v.retain(|n| !VIRTUAL.iter().any(|p| n.iface.starts_with(p)) || n.rx_kbs + n.tx_kbs >= 10.0);
    v.sort_by(|a, b| {
        (b.rx_kbs + b.tx_kbs)
            .partial_cmp(&(a.rx_kbs + a.tx_kbs))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.iface.cmp(&b.iface))
    });
    v.truncate(8);
}

pub(crate) fn ports(s: &str) -> Vec<Port> {
    let mut out: Vec<Port> = vec![];
    for l in s.lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        // ss -Htlnp: State Recv-Q Send-Q Local Peer [users:(("name",pid=1,fd=3))]
        // netstat -tlnp: Proto Recv-Q Send-Q Local Foreign State PID/Program
        let (local, process) = if c.first() == Some(&"LISTEN") && c.len() >= 5 {
            let p = c.get(5).and_then(|u| u.split('"').nth(1)).map(String::from);
            (c[3], p)
        } else if c.first().is_some_and(|p| p.starts_with("tcp")) && c.len() >= 6 {
            let p = c.get(6).and_then(|x| x.split('/').nth(1)).map(String::from);
            (c[3], p)
        } else {
            continue;
        };
        let Some((addr, port)) = local.rsplit_once(':') else { continue };
        let Ok(port) = port.parse::<u16>() else { continue };
        // "[fe80::1]%eth0" / "127.0.0.53%lo": drop the zone and brackets.
        let addr = addr.split('%').next().unwrap_or(addr);
        let addr = addr.trim_start_matches('[').trim_end_matches(']').to_string();
        if out.iter().any(|p| p.port == port && p.addr == addr) {
            continue;
        }
        out.push(Port { addr, port, process });
    }
    out.sort_by(|a, b| a.port.cmp(&b.port).then(a.addr.cmp(&b.addr)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_linux_sample() {
        let raw = "@@uname\nLinux web1 6.8.0-45-generic x86_64\n@@osrel\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\nID=ubuntu\n@@uptime\n12345.67 23456.00\n@@nproc\n4\n@@loadavg\n0.50 0.40 0.30 1/234 5678\n@@t1\n1000.0\n@@stat1\ncpu  100 0 50 800 50 0 0 0 0 0\n@@net1\n  eth0: 1000 10 0 0 0 0 0 0 2000 20 0 0 0 0 0 0\n    lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n@@disk1\n   8       0 sda 100 0 2000 50 200 0 4000 100 0 100 150 0 0 0 0\n   8       1 sda1 100 0 2000 50 200 0 4000 100 0 100 150 0 0 0 0\n@@t2\n1001.0\n@@stat2\ncpu  200 0 100 1600 100 0 0 0 0 0\n@@net2\n  eth0: 103400 110 0 0 0 0 0 0 53200 70 0 0 0 0 0 0\n    lo: 500 5 0 0 0 0 0 0 500 5 0 0 0 0 0 0\n@@disk2\n   8       0 sda 200 0 4048 60 300 0 6048 110 0 600 160 0 0 0 0\n@@meminfo\nMemTotal:        8000000 kB\nMemFree:          500000 kB\nMemAvailable:    2000000 kB\nSwapTotal:       1000000 kB\nSwapFree:         900000 kB\n@@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 10485760 5242880 5242880 50% /\n@@dfi\nFilesystem Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 655360 65536 589824 10% /\n@@pscpu\n    PID USER     %CPU %MEM   RSS ELAPSED STAT COMMAND\n   1234 www-data 55.0  2.0 163840 3600 S nginx\n@@psmem\n    PID USER     %CPU %MEM   RSS ELAPSED STAT COMMAND\n   2345 mysql 1.0 30.0 2457600 86400 Sl mysqld\n@@procs\n    120 S\n      2 R\n      1 Z\n@@ports\nLISTEN 0      511          0.0.0.0:80        0.0.0.0:*    users:((\"nginx\",pid=1234,fd=6))\nLISTEN 0      4096       127.0.0.1:3306      0.0.0.0:*    users:((\"mysqld\",pid=2345,fd=20))\nLISTEN 0      511             [::]:80           [::]:*    users:((\"nginx\",pid=1234,fd=7))\n@@failed\nfoo.service loaded failed failed Foo daemon\n@@docker\nweb|Up 2 hours|nginx:latest\n@@psi\nmemory some avg10=25.00 avg60=1.00 avg300=0.50 total=1234\n@@oom\n@@end\n";
        let secs = sections(raw);
        let st = parse(&secs, "linux");
        assert_eq!(st.os.hostname, "web1");
        assert_eq!(st.os.name.as_deref(), Some("Ubuntu 24.04 LTS"));
        let cpu = st.cpu.clone().unwrap();
        assert_eq!(cpu.cores, 4);
        assert_eq!(cpu.usage_pct, 15.0);
        assert_eq!(cpu.iowait_pct, Some(5.0));
        let mem = st.mem.clone().unwrap();
        assert_eq!(mem.total_mb, 7812);
        assert_eq!(mem.available_mb, 1953);
        assert_eq!(st.disk_io.len(), 1);
        assert_eq!(st.disk_io[0].device, "sda");
        assert_eq!(st.disk_io[0].util_pct, 50.0);
        assert_eq!(st.net.len(), 1);
        assert_eq!(st.net[0].rx_kbs, 100.0);
        assert_eq!(st.ports.len(), 3);
        assert_eq!(st.ports[0].port, 80);
        assert_eq!(st.ports[0].process.as_deref(), Some("nginx"));
        assert_eq!(st.failed_units, vec!["foo.service"]);
        assert_eq!(st.containers[0].name, "web");
        assert_eq!(st.top_cpu[0].command, "nginx");
        assert_eq!(st.top_mem[0].rss_mb, 2400.0);
        assert_eq!(st.proc_states.get("Z"), Some(&1));
        let f = findings(&Status { host: "x".into(), ..st });
        assert!(f.iter().any(|f| f.message.contains("failed systemd")));
        assert!(f.iter().any(|f| f.message.contains("memory pressure")));
    }

    #[test]
    fn partitions() {
        assert!(is_partition("sda1"));
        assert!(is_partition("nvme0n1p2"));
        assert!(!is_partition("sda"));
        assert!(!is_partition("nvme0n1"));
        assert!(!is_partition("dm-0"));
    }
}
