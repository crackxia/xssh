//! Parser for the macOS / FreeBSD status script output.

use super::*;
use std::collections::HashMap;

pub fn parse(s: &HashMap<String, String>, kind: &str) -> Status {
    let get = |k: &str| s.get(k).map(String::as_str).unwrap_or("");
    let u: Vec<&str> = get("uname").split_whitespace().collect();
    let boot = get("boottime")
        .split("sec =")
        .nth(1)
        .and_then(|r| r.split(',').next())
        .and_then(|v| v.trim().parse::<u64>().ok());
    let now = num::<u64>(get("now").trim());
    let name = if kind == "darwin" {
        let pn = get("osrel")
            .lines()
            .find_map(|l| l.strip_prefix("ProductName:"))
            .map(str::trim)
            .unwrap_or("macOS")
            .to_string();
        let pv = get("osrel")
            .lines()
            .find_map(|l| l.strip_prefix("ProductVersion:"))
            .map(str::trim)
            .unwrap_or("")
            .to_string();
        Some(format!("{pn} {pv}").trim().to_string())
    } else {
        get("osrel").lines().next().map(|v| format!("FreeBSD {}", v.trim()))
    };
    let mut st = Status {
        os: OsInfo {
            kind: kind.to_string(),
            name,
            hostname: u.get(1).unwrap_or(&"").to_string(),
            kernel: u.get(2).unwrap_or(&"").to_string(),
            arch: u.get(3).unwrap_or(&"").to_string(),
            uptime_secs: match (boot, now) {
                (Some(b), Some(n)) if n > b => n - b,
                _ => 0,
            },
        },
        ..Default::default()
    };
    let cores = num::<u32>(get("nproc").lines().next().unwrap_or("")).unwrap_or(1);
    // vm.loadavg: "{ 1.23 1.10 1.00 }"
    let l: Vec<f64> = get("loadavg").split_whitespace().filter_map(|x| x.parse().ok()).collect();
    if l.len() >= 3 {
        st.load = Some([l[0], l[1], l[2]]);
    }
    st.cpu = if kind == "darwin" {
        mac_cpu(get("cputop"), cores)
    } else {
        bsd_cpu(get("cp1"), get("cp2"), cores)
    };
    let mem_lines: Vec<u64> = get("mem").lines().filter_map(|l| l.trim().parse().ok()).collect();
    let (page, physmem) = (mem_lines.first().copied().unwrap_or(4096), mem_lines.get(1).copied().unwrap_or(0));
    st.mem = if kind == "darwin" {
        mac_mem(get("vmstat"), get("swap"), physmem)
    } else {
        bsd_mem(get("bsdmem"), get("swapinfo"), page, physmem)
    };
    st.disks = parse_df(get("df"), None);
    st.net = netstat_ib(get("net1"), get("net2"), 1.0);
    st.top_cpu = parse_ps(get("pscpu"));
    st.top_mem = parse_ps(get("psmem"));
    st.proc_states = parse_proc_states(get("procs"));
    st.ports = if kind == "darwin" {
        lsof_ports(get("lsof"))
    } else {
        sockstat_ports(get("sockstat"))
    };
    st.containers = parse_docker(get("docker"));
    st
}

/// "CPU usage: 5.26% user, 10.52% sys, 84.21% idle"
fn mac_cpu(line: &str, cores: u32) -> Option<Cpu> {
    let grab = |label: &str| -> Option<f64> {
        line.split(',')
            .find(|p| p.contains(label))
            .and_then(|p| p.split_whitespace().find(|t| t.ends_with('%')))
            .and_then(num::<f64>)
    };
    let user = grab("user")?;
    let sys = grab("sys").unwrap_or(0.0);
    let idle = grab("idle").unwrap_or(100.0 - user - sys);
    Some(Cpu {
        cores,
        usage_pct: round1(100.0 - idle),
        user_pct: user,
        system_pct: sys,
        iowait_pct: None,
        steal_pct: None,
        idle_pct: idle,
    })
}

/// kern.cp_time: "user nice sys intr idle"
fn bsd_cpu(a: &str, b: &str, cores: u32) -> Option<Cpu> {
    let f = |s: &str| -> Vec<u64> { s.split_whitespace().filter_map(|x| x.parse().ok()).collect() };
    let (x, y) = (f(a), f(b));
    if x.len() < 5 || y.len() < 5 {
        return None;
    }
    let d: Vec<f64> = (0..5).map(|i| y[i].saturating_sub(x[i]) as f64).collect();
    let total: f64 = d.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let p = |v: f64| round1(v * 100.0 / total);
    Some(Cpu {
        cores,
        usage_pct: p(total - d[4]),
        user_pct: p(d[0] + d[1]),
        system_pct: p(d[2] + d[3]),
        iowait_pct: None,
        steal_pct: None,
        idle_pct: p(d[4]),
    })
}

fn mac_mem(vmstat: &str, swap: &str, physmem: u64) -> Option<Mem> {
    let page = vmstat
        .lines()
        .next()
        .and_then(|l| l.split("page size of").nth(1))
        .and_then(|r| r.split_whitespace().next())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(4096);
    let mut m: HashMap<String, u64> = HashMap::new();
    for l in vmstat.lines().skip(1) {
        if let Some((k, v)) = l.split_once(':')
            && let Some(n) = num::<u64>(v)
        {
            m.insert(k.trim().to_string(), n);
        }
    }
    if physmem == 0 {
        return None;
    }
    let g = |k: &str| m.get(k).copied().unwrap_or(0);
    let avail = (g("Pages free") + g("Pages inactive") + g("Pages speculative") + g("Pages purgeable")) * page;
    let avail = avail.min(physmem);
    // vm.swapusage: "total = 2048.00M  used = 1024.00M  free = 1024.00M  (encrypted)"
    let sw = |label: &str| -> u64 {
        swap.split(label)
            .nth(1)
            .and_then(|r| r.split_whitespace().nth(1))
            .map(|v| {
                let (n, unit) = v.split_at(v.len().saturating_sub(1));
                let n: f64 = n.parse().unwrap_or(0.0);
                match unit {
                    "G" => (n * 1024.0) as u64,
                    "K" => (n / 1024.0) as u64,
                    _ => n as u64,
                }
            })
            .unwrap_or(0)
    };
    let total_mb = physmem / 1048576;
    let avail_mb = avail / 1048576;
    Some(Mem {
        total_mb,
        used_mb: total_mb.saturating_sub(avail_mb),
        available_mb: avail_mb,
        used_pct: round1(total_mb.saturating_sub(avail_mb) as f64 * 100.0 / total_mb.max(1) as f64),
        swap_total_mb: sw("total"),
        swap_used_mb: sw("used"),
    })
}

fn bsd_mem(counts: &str, swapinfo: &str, page: u64, physmem: u64) -> Option<Mem> {
    if physmem == 0 {
        return None;
    }
    let c: Vec<u64> = counts.lines().filter_map(|l| l.trim().parse().ok()).collect();
    let avail = c.iter().sum::<u64>() * page;
    let total_mb = physmem / 1048576;
    let avail_mb = (avail / 1048576).min(total_mb);
    // swapinfo -k last line: "Total 2097152 0 2097152 0%" or a single device line
    let s: Vec<&str> = swapinfo.split_whitespace().collect();
    let (st, su) = if s.len() >= 3 {
        (num::<u64>(s[1]).unwrap_or(0) / 1024, num::<u64>(s[2]).unwrap_or(0) / 1024)
    } else {
        (0, 0)
    };
    Some(Mem {
        total_mb,
        used_mb: total_mb - avail_mb,
        available_mb: avail_mb,
        used_pct: round1((total_mb - avail_mb) as f64 * 100.0 / total_mb.max(1) as f64),
        swap_total_mb: st,
        swap_used_mb: su,
    })
}

/// `netstat -ibn`: use `<Link#N>` rows; locate Ibytes/Obytes counting from the right
/// because the Address column may be empty.
pub(crate) fn netstat_ib(a: &str, b: &str, dt: f64) -> Vec<Net> {
    let parse = |s: &str| -> HashMap<String, (u64, u64)> {
        let mut lines = s.lines();
        let Some(header) = lines.next() else { return HashMap::new() };
        let h: Vec<&str> = header.split_whitespace().collect();
        let (Some(ib), Some(ob)) = (h.iter().position(|x| *x == "Ibytes"), h.iter().position(|x| *x == "Obytes")) else {
            return HashMap::new();
        };
        let (ibr, obr) = (h.len() - 1 - ib, h.len() - 1 - ob);
        lines
            .filter(|l| l.contains("<Link#"))
            .filter_map(|l| {
                let c: Vec<&str> = l.split_whitespace().collect();
                if c.len() <= ibr.max(obr) {
                    return None;
                }
                let rx = c[c.len() - 1 - ibr].parse().ok()?;
                let tx = c[c.len() - 1 - obr].parse().ok()?;
                Some((c[0].trim_end_matches('*').to_string(), (rx, tx)))
            })
            .collect()
    };
    let (x, y) = (parse(a), parse(b));
    let mut out: Vec<Net> = y
        .iter()
        .filter(|(n, _)| !n.starts_with("lo"))
        .map(|(n, (rx2, tx2))| {
            let (rx1, tx1) = x.get(n).copied().unwrap_or((*rx2, *tx2));
            Net {
                iface: n.clone(),
                rx_kbs: round1(rx2.saturating_sub(rx1) as f64 / 1024.0 / dt),
                tx_kbs: round1(tx2.saturating_sub(tx1) as f64 / 1024.0 / dt),
                rx_total_mb: round1(*rx2 as f64 / 1048576.0),
                tx_total_mb: round1(*tx2 as f64 / 1048576.0),
            }
        })
        .filter(|n| n.rx_total_mb > 0.0 || n.tx_total_mb > 0.0)
        .collect();
    super::linux::trim_ifaces(&mut out);
    out
}

/// lsof -nP -iTCP -sTCP:LISTEN: "nginx 123 user 6u IPv4 0x.. 0t0 TCP *:80 (LISTEN)"
fn lsof_ports(s: &str) -> Vec<Port> {
    let mut out: Vec<Port> = vec![];
    for l in s.lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        let Some(i) = c.iter().position(|x| *x == "TCP") else { continue };
        let Some(addr) = c.get(i + 1) else { continue };
        let Some((a, p)) = addr.rsplit_once(':') else { continue };
        let Ok(port) = p.parse() else { continue };
        let a = a.trim_start_matches('[').trim_end_matches(']').to_string();
        if !out.iter().any(|x| x.port == port && x.addr == a) {
            out.push(Port {
                addr: a,
                port,
                process: c.first().map(|x| x.to_string()),
            });
        }
    }
    out.sort_by_key(|a| a.port);
    out
}

/// sockstat -46 -l -P tcp: "USER COMMAND PID FD PROTO LOCAL FOREIGN"
fn sockstat_ports(s: &str) -> Vec<Port> {
    let mut out: Vec<Port> = vec![];
    for l in s.lines() {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() < 6 {
            continue;
        }
        let Some((a, p)) = c[5].rsplit_once(':') else { continue };
        let Ok(port) = p.parse() else { continue };
        if !out.iter().any(|x| x.port == port && x.addr == a) {
            out.push(Port {
                addr: a.to_string(),
                port,
                process: Some(c[1].to_string()),
            });
        }
    }
    out.sort_by_key(|a| a.port);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_macos_sample() {
        let raw = "@@uname\nDarwin mbp 23.4.0 arm64\n@@osrel\nProductName:\t\tmacOS\nProductVersion:\t\t14.4\n@@boottime\n{ sec = 1700000000, usec = 0 } Tue Nov 14\n@@now\n1700003600\n@@nproc\n8\n@@loadavg\n{ 2.10 1.90 1.70 }\n@@net1\nName  Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll\nlo0   16384 <Link#1>                         100     0      5000      100     0       5000     0\nen0   1500  <Link#6>    aa:bb:cc:dd:ee:ff   1000     0   1048576      500     0     524288     0\n@@cputop\nCPU usage: 12.5% user, 7.5% sys, 80.0% idle\n@@net2\nName  Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll\nlo0   16384 <Link#1>                         100     0      5000      100     0       5000     0\nen0   1500  <Link#6>    aa:bb:cc:dd:ee:ff   1100     0   1150976      600     0     575488     0\n@@mem\n16384\n17179869184\n@@vmstat\nMach Virtual Memory Statistics: (page size of 16384 bytes)\nPages free:                               65536.\nPages active:                            300000.\nPages inactive:                          131072.\nPages speculative:                        0.\n@@swap\ntotal = 2048.00M  used = 512.00M  free = 1536.00M  (encrypted)\n@@df\nFilesystem 1024-blocks Used Available Capacity Mounted on\n/dev/disk3s1s1 971350180 10000000 500000000 2% /\n@@pscpu\n  PID USER %CPU %MEM RSS ELAPSED STAT COMM\n  321 me 30.0 1.0 102400 01:02:03 S /Applications/Foo.app/Contents/MacOS/Foo\n@@psmem\n  PID USER %CPU %MEM RSS ELAPSED STAT COMM\n@@procs\n 300 S\n@@lsof\nnginx 123 me 6u IPv4 0x1 0t0 TCP *:8080 (LISTEN)\n@@docker\n@@end\n";
        let st = parse(&sections(raw), "darwin");
        assert_eq!(st.os.name.as_deref(), Some("macOS 14.4"));
        assert_eq!(st.os.uptime_secs, 3600);
        assert_eq!(st.load, Some([2.1, 1.9, 1.7]));
        let cpu = st.cpu.unwrap();
        assert_eq!(cpu.usage_pct, 20.0);
        let mem = st.mem.unwrap();
        assert_eq!(mem.total_mb, 16384);
        assert_eq!(mem.available_mb, 3072);
        assert_eq!(mem.swap_used_mb, 512);
        assert_eq!(st.net.len(), 1);
        assert_eq!(st.net[0].rx_kbs, 100.0);
        assert_eq!(st.ports[0].port, 8080);
        assert_eq!(st.top_cpu[0].elapsed_secs, 3723);
    }

    #[test]
    fn parses_freebsd_bits() {
        let c = bsd_cpu("100 0 50 10 840", "200 0 100 20 1680", 2).unwrap();
        assert_eq!(c.usage_pct, 16.0);
        let m = bsd_mem("100000\n50000\n0\n", "Total 2097152 1048576 1048576 50%", 4096, 4294967296).unwrap();
        assert_eq!(m.total_mb, 4096);
        assert_eq!(m.available_mb, 585);
        assert_eq!(m.swap_used_mb, 1024);
        let p = sockstat_ports("root sshd 700 3 tcp4 *:22 *:*\nwww nginx 800 6 tcp4 127.0.0.1:8080 *:*\n");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].port, 22);
    }
}
