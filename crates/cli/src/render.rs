//! Compact, token-efficient text rendering of results (the default output).
//! `--json` prints the structured result instead.

use std::fmt::Write;
use xssh_engine::exec::ExecResult;
use xssh_engine::files::{EditResult, Entry, FileContent, WriteResult};
use xssh_engine::forward::ForwardInfo;
use xssh_engine::jobs::{JobLogs, JobStatus};
use xssh_engine::probe::perf::PerfResult;
use xssh_engine::probe::{Finding, Severity, Status};
use xssh_engine::session::prompt::PromptKind;
use xssh_engine::session::{Observation, ScreenSnapshot, SessionInfo, SessionState};
use xssh_engine::transfer::CopyResult;

pub fn human_bytes(b: u64) -> String {
    match b {
        0..1024 => format!("{b} B"),
        1024..1_048_576 => format!("{:.1} KB", b as f64 / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b as f64 / 1_048_576.0),
        _ => format!("{:.2} GB", b as f64 / 1_073_741_824.0),
    }
}

pub fn human_secs(s: u64) -> String {
    let (d, h, m) = (s / 86400, (s % 86400) / 3600, (s % 3600) / 60);
    if d > 0 {
        format!("{d}d{h}h")
    } else if h > 0 {
        format!("{h}h{m}m")
    } else if m > 0 {
        format!("{m}m{}s", s % 60)
    } else {
        format!("{s}s")
    }
}

/// Returns (stdout text, stderr text, exit code).
pub fn exec(results: &[ExecResult]) -> (String, String, i32) {
    if results.len() == 1 {
        let r = &results[0];
        let mut err = r.stderr.text.clone();
        let mut notes = vec![];
        if let Some(e) = &r.error {
            notes.push(format!("error: {}", e.message));
        }
        if let Some(k) = &r.host_key {
            notes.push(k.clone());
        }
        let code = exit_of(r);
        if code != 0 || r.timed_out {
            notes.push(format!(
                "exit={}{} time={:.1}s",
                r.exit_code.map(|c| c.to_string()).unwrap_or("none".into()),
                r.signal.as_ref().map(|s| format!(" signal={s}")).unwrap_or_default(),
                r.duration_ms as f64 / 1000.0
            ));
        }
        if r.stdout.truncated {
            notes.push(format!(
                "stdout truncated ({} bytes total){}",
                r.stdout.total_bytes,
                r.stdout
                    .full_output_path
                    .as_ref()
                    .map(|p| format!(", full: {p}"))
                    .unwrap_or_default()
            ));
        }
        if r.stderr.truncated {
            notes.push(format!(
                "stderr truncated ({} bytes total){}",
                r.stderr.total_bytes,
                r.stderr
                    .full_output_path
                    .as_ref()
                    .map(|p| format!(", full: {p}"))
                    .unwrap_or_default()
            ));
        }
        if let Some(h) = &r.hint {
            notes.push(format!("hint: {h}"));
        }
        if !notes.is_empty() {
            if !err.is_empty() && !err.ends_with('\n') {
                err.push('\n');
            }
            for n in notes {
                let _ = writeln!(err, "[xssh] {n}");
            }
        }
        return (r.stdout.text.clone(), err, code);
    }
    let mut out = String::new();
    // All failing hosts agree on the code: return it; otherwise 1.
    let codes: Vec<i32> = results.iter().map(exit_of).filter(|c| *c != 0).collect();
    let worst = match codes.first() {
        None => 0,
        Some(c) if codes.iter().all(|x| x == c) => *c,
        Some(_) => 1,
    };
    for r in results {
        if let Some(k) = &r.host_key {
            let _ = writeln!(out, "[xssh] {k}");
        }
        match &r.error {
            Some(e) => {
                let _ = writeln!(out, "=== {} (error) ===\n{}", r.host, e.message);
                if let Some(h) = &e.hint {
                    let _ = writeln!(out, "hint: {h}");
                }
            }
            None => {
                let _ = writeln!(
                    out,
                    "=== {} (exit {}, {:.1}s) ===",
                    r.host,
                    r.exit_code.map(|c| c.to_string()).unwrap_or("none".into()),
                    r.duration_ms as f64 / 1000.0
                );
                out.push_str(&r.stdout.text);
                if !r.stdout.text.is_empty() && !r.stdout.text.ends_with('\n') {
                    out.push('\n');
                }
                if !r.stderr.text.is_empty() {
                    let _ = writeln!(out, "--- stderr ---");
                    out.push_str(&r.stderr.text);
                    if !r.stderr.text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                if let Some(h) = &r.hint {
                    let _ = writeln!(out, "hint: {h}");
                }
            }
        }
    }
    (out, String::new(), worst)
}

fn exit_of(r: &ExecResult) -> i32 {
    if r.error.is_some() {
        return r.error.as_ref().map(|e| e.code.exit_code()).unwrap_or(1);
    }
    if r.timed_out {
        return 124;
    }
    match (r.exit_code, &r.signal) {
        (Some(c), _) => c as i32,
        (None, Some(s)) => 128 + signal_number(s),
        (None, None) => 255,
    }
}

/// POSIX signal numbers for the exit code (128 + n), as shells report killed commands.
fn signal_number(name: &str) -> i32 {
    match name.trim_start_matches("SIG") {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "ABRT" => 6,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        _ => 9,
    }
}

/// Session result: output (or the screen for full-screen programs), then one footer line
/// `[xssh <id> <state> ...]` where state is done|running|waiting|quiet|tui|exited.
pub fn observation(o: &Observation) -> String {
    use xssh_engine::session::SessionState as S;
    let mut s = String::new();
    if o.alt_screen {
        // Lines that scrolled off the top since the last call, then the live screen.
        if !o.output.trim().is_empty() {
            let _ = writeln!(s, "{}", o.output.trim_end());
        }
        match &o.screen {
            Some(sc) => {
                let _ = writeln!(s, "--- screen ---\n{}\n--------------", sc.trim_end());
            }
            None if o.screen_unchanged => s.push_str("--- screen unchanged ---\n"),
            None => {}
        }
    } else {
        s.push_str(&o.output);
        if !o.output.is_empty() && !o.output.ends_with('\n') {
            s.push('\n');
        }
    }
    let state = if o.state == S::Exited {
        "exited"
    } else if o.state == S::Disconnected {
        "disconnected"
    } else if o.completed {
        "done"
    } else if o.alt_screen {
        "tui"
    } else {
        match o.state {
            S::Running => "running",
            S::WaitingInput => "waiting",
            _ => "quiet",
        }
    };
    let mut footer = format!("[xssh {} {state}", o.session);
    if o.busy {
        footer.push_str(" busy");
    }
    if o.timed_out {
        footer.push_str(" timeout");
    }
    if let Some(c) = o.exit_code {
        let _ = write!(footer, " exit={c}");
    }
    // The prompt kind matters only when input is expected (a shell prompt after `done` is implied).
    if let Some(p) = o
        .prompt
        .filter(|_| o.state == S::WaitingInput && !o.alt_screen && !(o.completed && state == "done"))
    {
        let _ = write!(footer, " prompt={}", enum_name(&p));
    }
    footer.push(']');
    let _ = writeln!(s, "{footer}");
    let shell_prompt = o.prompt == Some(xssh_engine::session::prompt::PromptKind::Shell);
    let shown = o.output.trim_end().ends_with(o.current_line.as_str());
    if matches!(o.state, S::WaitingInput | S::Quiet) && !o.current_line.is_empty() && !o.alt_screen && !shell_prompt && !shown {
        let _ = writeln!(s, "[line] {}", o.current_line);
    }
    for e in &o.events {
        let _ = writeln!(s, "[event] {e}");
    }
    if let Some(p) = &o.truncated_output_path {
        let _ = writeln!(s, "[truncated; full output: {p}]");
    }
    if let Some(h) = &o.hint {
        let _ = writeln!(s, "[hint] {h}");
    }
    s
}

fn enum_name<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

pub fn screen(s: &ScreenSnapshot) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "[screen {}x{} cursor={},{}{}]",
        s.cols,
        s.rows,
        s.cursor_row,
        s.cursor_col,
        if s.alt_screen { " tui" } else { "" }
    );
    out.push_str(&s.screen);
    out
}

pub fn sessions(v: &[SessionInfo]) -> String {
    if v.is_empty() {
        return "no open sessions\n".into();
    }
    let mut s = String::new();
    for i in v {
        let _ = writeln!(
            s,
            "{}{}  host={}{}{}  state={}  idle={}{}{}{}",
            i.id,
            i.name.as_ref().map(|n| format!(" name={n}")).unwrap_or_default(),
            i.host,
            if i.owner.is_empty() {
                String::new()
            } else {
                format!("  owner={}", i.owner)
            },
            if i.persistent { "  persistent" } else { "" },
            // At the shell prompt nothing needs an answer: "idle". A detached persistent session
            // is reattached by the next call.
            match (&i.state, i.prompt) {
                (SessionState::WaitingInput, Some(PromptKind::Shell)) => "idle".to_string(),
                (SessionState::WaitingInput, Some(k)) => format!("waiting prompt={}", enum_name(&k)),
                (SessionState::Disconnected, _) if i.persistent => "detached".to_string(),
                (st, _) => enum_name(st),
            },
            human_secs(i.idle_secs),
            i.pending_run.as_ref().map(|_| "  run=unfinished").unwrap_or(""),
            if i.cmd.is_empty() {
                String::new()
            } else {
                format!("  cmd: {}", i.cmd)
            },
            if i.last.is_empty() || i.last == i.cmd {
                String::new()
            } else {
                format!("  last: {}", i.last)
            }
        );
    }
    s
}

pub fn job(j: &JobStatus, with_command: bool) -> String {
    let mut s = String::new();
    if with_command {
        let _ = writeln!(s, "command: {}", j.command);
    }
    if let Some(t) = &j.tail
        && !t.trim().is_empty()
    {
        let _ = writeln!(s, "--- log tail ---\n{}", t.trim_end());
    }
    let _ = writeln!(
        s,
        "[xssh job={} host={} state={}{} next_offset={}]",
        j.id,
        j.host,
        enum_name(&j.state),
        j.exit_code.map(|c| format!(" exit={c}")).unwrap_or_default(),
        j.log_bytes
    );
    if let Some(h) = &j.hint {
        let _ = writeln!(s, "[hint] {h}");
    }
    s
}

pub fn job_logs(l: &JobLogs) -> String {
    let mut s = l.output.clone();
    if !s.is_empty() && !s.ends_with('\n') {
        s.push('\n');
    }
    let _ = writeln!(
        s,
        "[xssh job={} state={}{} next_offset={} log_bytes={}{}]",
        l.id,
        serde_json::to_value(l.state)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_default(),
        l.exit_code.map(|c| format!(" exit={c}")).unwrap_or_default(),
        l.next_offset,
        l.log_bytes,
        if l.more { " more" } else { "" }
    );
    s
}

pub fn file_content(f: &FileContent) -> String {
    let mut s = f.content.clone();
    // Only what the numbered lines cannot show: line endings, missing final newline, encoding.
    let mut notes = vec![];
    if let Some(e) = &f.eol {
        notes.push(format!("eol={e}"));
    }
    if f.no_final_newline {
        notes.push("no final newline".to_string());
    }
    if let Some(e) = &f.encoding {
        notes.push(format!("encoding={e}"));
    }
    if !notes.is_empty() {
        let _ = writeln!(s, "[xssh {}]", notes.join(" "));
    }
    if let Some(h) = &f.hint {
        let _ = writeln!(s, "[hint] {h}");
    }
    s
}

pub fn write_result(w: &WriteResult) -> String {
    format!(
        "wrote {} bytes to {} (sha256 {}){}\n",
        w.bytes,
        w.path,
        &w.sha256[..12],
        w.backup.as_ref().map(|b| format!(", backup: {b}")).unwrap_or_default()
    )
}

pub fn edit_result(e: &EditResult) -> String {
    format!(
        "edited {} ({} replacement{}){}{}\n{}\n",
        e.path,
        e.replacements,
        if e.replacements == 1 { "" } else { "s" },
        e.backup.as_ref().map(|b| format!(", backup: {b}")).unwrap_or_default(),
        e.note.as_ref().map(|n| format!("; {n}")).unwrap_or_default(),
        e.snippet
    ) + &e.eol.as_ref().map(|x| format!("[xssh eol={x}]\n")).unwrap_or_default()
}

pub fn entries(v: &[Entry]) -> String {
    let mut s = String::new();
    for e in v {
        let _ = writeln!(
            s,
            "{:<7} {:>5} {:>10} {:<19} {}{}",
            e.kind,
            e.mode,
            e.size,
            e.mtime.clone().unwrap_or_default(),
            e.name,
            if e.kind == "dir" { "/" } else { "" }
        );
    }
    s
}

pub fn copy(r: &CopyResult) -> String {
    let verb = if r.dry_run { "would copy" } else { "copied" };
    let mut s = format!("{verb} {} file(s), {}", r.files, human_bytes(r.bytes));
    if r.dirs > 0 {
        let _ = write!(s, ", {} new dir(s)", r.dirs);
    }
    if r.unchanged > 0 {
        let _ = write!(s, ", {} unchanged", r.unchanged);
    }
    if r.deleted > 0 {
        let _ = write!(s, ", {} {}", r.deleted, if r.dry_run { "to delete" } else { "deleted" });
    }
    if r.links > 0 {
        let _ = write!(s, ", {} symlink(s)", r.links);
    }
    if r.resumed > 0 {
        let _ = write!(s, ", {} resumed", r.resumed);
    }
    let _ = write!(s, ", {:.1}s", r.duration_ms as f64 / 1000.0);
    if let Some(h) = &r.sha256 {
        let _ = write!(s, ", sha256 {h}");
    }
    if let Some(h) = &r.tree_sha256 {
        let _ = write!(s, ", tree sha256 {h}");
    }
    if r.verified {
        s.push_str(", verified");
    }
    if r.skipped > 0 {
        let _ = write!(s, ", skipped {}", r.skipped);
    }
    s.push('\n');
    for t in &r.targets {
        let _ = writeln!(s, "-> {t}");
    }
    // Single-file copies already say everything (their sha256); list changes for trees.
    if r.sha256.is_none() || r.deleted > 0 || r.dry_run {
        for c in &r.changed {
            let _ = writeln!(s, "{c}");
        }
        if r.changed_more > 0 {
            let _ = writeln!(s, "... {} more", r.changed_more);
        }
    }
    for p in &r.skipped_paths {
        let _ = writeln!(s, "[skipped] {p}");
    }
    for w in &r.warnings {
        let _ = writeln!(s, "[warning] {w}");
    }
    s
}

pub fn forwards(v: &[ForwardInfo]) -> String {
    if v.is_empty() {
        return "no active forwards\n".into();
    }
    v.iter()
        .map(|f| {
            format!(
                "{}  {}  connections={}{}{}{}{}\n",
                f.id,
                f.description,
                f.connections,
                if f.failures > 0 {
                    format!(" failures={}", f.failures)
                } else {
                    String::new()
                },
                if f.reconnects > 0 {
                    format!(" reconnects={}", f.reconnects)
                } else {
                    String::new()
                },
                if f.alive { "" } else { "  [down]" },
                f.last_error.as_ref().map(|e| format!("  last_error: {e}")).unwrap_or_default()
            )
        })
        .collect()
}

pub fn findings(f: &[Finding]) -> String {
    let mut s = String::new();
    for x in f {
        let sev = match x.severity {
            Severity::Critical => "CRITICAL",
            Severity::Warning => "WARNING",
            Severity::Info => "info",
        };
        let _ = writeln!(s, "  [{sev}] {}", x.message);
        if let Some(sug) = &x.suggestion {
            let _ = writeln!(s, "      try: {sug}");
        }
    }
    s
}

pub fn status(st: &Status) -> String {
    let mut s = String::new();
    let o = &st.os;
    let _ = writeln!(
        s,
        "{}  {} ({} {} {})  up {}",
        st.host,
        o.name.clone().unwrap_or_else(|| o.kind.clone()),
        o.kind,
        o.kernel,
        o.arch,
        human_secs(o.uptime_secs)
    );
    if let Some(c) = &st.cpu {
        let mut extra = String::new();
        if let Some(w) = c.iowait_pct {
            let _ = write!(extra, " iowait {w}");
        }
        if let Some(w) = c.steal_pct {
            let _ = write!(extra, " steal {w}");
        }
        let _ = write!(
            s,
            "cpu   {:.1}% (user {} sys {}{extra}) cores={}",
            c.usage_pct, c.user_pct, c.system_pct, c.cores
        );
        if let Some(l) = st.load {
            let _ = write!(s, "  load {:.2} {:.2} {:.2}", l[0], l[1], l[2]);
        }
        s.push('\n');
    }
    if let Some(m) = &st.mem {
        let _ = writeln!(
            s,
            "mem   {}/{} MB used ({:.1}%), available {} MB, swap {}/{} MB",
            m.used_mb, m.total_mb, m.used_pct, m.available_mb, m.swap_used_mb, m.swap_total_mb
        );
    }
    for d in &st.disks {
        let _ = writeln!(
            s,
            "disk  {:<20} {:>5.1}% of {:.1}G ({:.1}G free){}",
            d.mount,
            d.used_pct,
            d.size_gb,
            d.avail_gb,
            d.inodes_used_pct.map(|i| format!(" inodes {i:.0}%")).unwrap_or_default()
        );
    }
    for d in st
        .disk_io
        .iter()
        .filter(|d| d.util_pct > 0.0 || d.read_kbs > 0.0 || d.write_kbs > 0.0)
        .take(5)
    {
        let _ = writeln!(
            s,
            "io    {:<10} read {:.0} KB/s  write {:.0} KB/s  util {:.0}%",
            d.device, d.read_kbs, d.write_kbs, d.util_pct
        );
    }
    for n in &st.net {
        let _ = writeln!(
            s,
            "net   {:<10} rx {:.1} KB/s  tx {:.1} KB/s  (total rx {:.0} MB tx {:.0} MB)",
            n.iface, n.rx_kbs, n.tx_kbs, n.rx_total_mb, n.tx_total_mb
        );
    }
    if !st.proc_states.is_empty() {
        let total: u64 = st.proc_states.values().sum();
        let _ = writeln!(
            s,
            "procs {total} total, {} running, {} blocked(D), {} zombie",
            st.proc_states.get("R").unwrap_or(&0),
            st.proc_states.get("D").unwrap_or(&0),
            st.proc_states.get("Z").unwrap_or(&0)
        );
    }
    let procs = |label: &str, v: &[xssh_engine::probe::Proc], s: &mut String| {
        if v.is_empty() {
            return;
        }
        let _ = writeln!(s, "{label}");
        for p in v {
            let _ = writeln!(
                s,
                "  {:>7} {:<10} cpu {:>5.1}%  mem {:>5.1}% ({:.0} MB)  {}",
                p.pid, p.user, p.cpu_pct, p.mem_pct, p.rss_mb, p.command
            );
        }
    };
    procs("top cpu:", &st.top_cpu, &mut s);
    procs("top mem:", &st.top_mem, &mut s);
    if !st.ports.is_empty() {
        // One entry per port; mark ports bound only to loopback.
        let mut seen: Vec<(u16, String, bool)> = vec![];
        for p in &st.ports {
            let local = p.addr.starts_with("127.") || p.addr == "::1";
            match seen.iter_mut().find(|x| x.0 == p.port) {
                Some(x) => x.2 &= local,
                None => seen.push((p.port, p.process.clone().unwrap_or_default(), local)),
            }
        }
        let ports: Vec<String> = seen
            .iter()
            .map(|(port, proc_, local)| {
                format!(
                    "{port}{}{}",
                    if proc_.is_empty() { String::new() } else { format!("({proc_})") },
                    if *local { "[lo]" } else { "" }
                )
            })
            .collect();
        let _ = writeln!(s, "listen {}", ports.join(" "));
    }
    if !st.failed_units.is_empty() {
        let _ = writeln!(s, "failed units: {}", st.failed_units.join(", "));
    } else if st.services_checked {
        // Explicit, so "none failed" is not confused with "not collected".
        s.push_str(
            "failed units: none
",
        );
    }
    if !st.containers.is_empty() {
        let bad: Vec<&xssh_engine::probe::Container> = st
            .containers
            .iter()
            .filter(|c| !c.status.starts_with("Up") || c.status.contains("unhealthy"))
            .collect();
        let _ = writeln!(s, "docker {} containers ({} not healthy/up)", st.containers.len(), bad.len());
        for c in bad.iter().take(10) {
            let _ = writeln!(s, "  {} [{}] {}", c.name, c.status, c.image);
        }
        if st.containers.len() <= 8 {
            for c in st.containers.iter().filter(|c| !bad.iter().any(|b| b.name == c.name)) {
                let _ = writeln!(s, "  {} [{}] {}", c.name, c.status, c.image);
            }
        }
    }
    if !st.oom_events.is_empty() {
        let window = if st.oom_window.as_deref() == Some("boot") {
            "since boot"
        } else {
            "last 24h"
        };
        let _ = writeln!(s, "oom events ({window}):");
        for e in &st.oom_events {
            let _ = writeln!(s, "  {e}");
        }
    }
    if !st.findings.is_empty() {
        let _ = writeln!(s, "findings:");
        s.push_str(&findings(&st.findings));
    }
    for n in &st.notes {
        let _ = writeln!(s, "[note] {n}");
    }
    s
}

pub fn perf(p: &PerfResult) -> String {
    let mut s = format!("{}  {} samples every {}s ({})\n", p.host, p.samples, p.interval_secs, p.os);
    let _ = writeln!(s, "{:<18} {:>9} {:>9} {:>9} {:>9}", "metric", "min", "avg", "p95", "max");
    for (k, v) in &p.summary {
        let _ = writeln!(s, "{:<18} {:>9} {:>9} {:>9} {:>9}", k, v.min, v.avg, v.p95, v.max);
    }
    if !p.findings.is_empty() {
        let _ = writeln!(s, "findings:");
        s.push_str(&findings(&p.findings));
    }
    if let Some(h) = &p.hint {
        let _ = writeln!(s, "[hint] {h}");
    }
    s
}
