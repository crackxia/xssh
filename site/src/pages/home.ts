import { AGENT_MESSAGE, AGENTS, INSTALL_PS, type Lang, STRINGS } from "../i18n";
import { codeLine, copyBtn, esc, icons } from "../html";
import { latest, type Manifest, mb } from "../releases";

/** Terminal lines: "$ cmd" is input, "[xssh …]" a status line (drawn as the app's state tag), anything else output. */
function lines(ls: string[]): string {
	return ls
		.map((l) => {
			if (l.startsWith("$ ")) return `<span class="in"><span class="ps">$</span> ${esc(l.slice(2))}</span>`;
			const st = l.match(/^\[xssh \S+ (\S+)/);
			if (st) return `<span class="st st-${st[1]}">${esc(l)}</span>`;
			return `<span class="out">${esc(l)}</span>`;
		})
		.join("\n");
}

/** A terminal panel: a framed grey well with a caption strip. */
function term(title: string, ls: string[]): string {
	return `<figure class="term"><figcaption>${esc(title)}</figcaption><pre>${lines(ls)}</pre></figure>`;
}

const HERO = [
	"$ xssh session run w -- 'sudo apt-get upgrade'",
	"Do you want to continue? [Y/n]",
	"[xssh w waiting prompt=confirm]",
	"$ xssh session send w y --enter",
	"Setting up nginx (1.24.0-2ubuntu7.5) …",
	"[xssh w done exit=0]",
	"$ xssh session run w -- pwd",
	"/srv/app",
	"[xssh w done exit=0]",
];

/** The app's state tag class for each status-line state. */
const STATE_TAG: Record<string, string> = { done: "ok", waiting: "wait", running: "run", quiet: "grey", tui: "run", disconnected: "bad" };

// Lucide outlines, as the desktop app draws its navigation.
const ico = (d: string, size = 14) =>
	`<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;
const NAV: [string, string, string?][] = [
	["主机", `<circle cx="12" cy="12" r="10"/><path d="M12 2a14.5 14.5 0 0 0 0 20 14.5 14.5 0 0 0 0-20M2 12h20"/>`, "hosts"],
	["会话", `<path d="m7 11 2-2-2-2M11 13h4"/><rect width="18" height="18" x="3" y="3" rx="2"/>`, "sessions"],
	["端口转发", `<rect x="16" y="16" width="6" height="6" rx="1"/><rect x="2" y="16" width="6" height="6" rx="1"/><rect x="9" y="2" width="6" height="6" rx="1"/><path d="M5 16v-3a1 1 0 0 1 1-1h12a1 1 0 0 1 1 1v3M12 12V8"/>`],
	["后台任务", `<rect width="16" height="16" x="4" y="4" rx="2"/><rect width="6" height="6" x="9" y="9" rx="1"/><path d="M15 2v2M15 20v2M2 15h2M2 9h2M20 15h2M20 9h2M9 2v2M9 20v2"/>`],
	["审计日志", `<path d="M15 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V7Z"/><path d="M14 2v4a2 2 0 0 0 2 2h4M16 13H8M16 17H8M10 9H8"/>`, "audit"],
	["守护进程", `<circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.65 1.65 0 0 0 .33 1.82l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.65 1.65 0 0 0-1.82-.33 1.65 1.65 0 0 0-1 1.51V21a2 2 0 0 1-4 0v-.09A1.65 1.65 0 0 0 9 19.4a1.65 1.65 0 0 0-1.82.33l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.65 1.65 0 0 0 4.6 15a1.65 1.65 0 0 0-1.51-1H3a2 2 0 0 1 0-4h.09A1.65 1.65 0 0 0 4.6 9a1.65 1.65 0 0 0-.33-1.82l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.65 1.65 0 0 0 9 4.6a1.65 1.65 0 0 0 1-1.51V3a2 2 0 0 1 4 0v.09a1.65 1.65 0 0 0 1 1.51 1.65 1.65 0 0 0 1.82-.33l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.65 1.65 0 0 0 19.4 9a1.65 1.65 0 0 0 1.51 1H21a2 2 0 0 1 0 4h-.09a1.65 1.65 0 0 0-1.51 1z"/>`],
	["集成", `<path d="M12 8V4H8"/><rect width="16" height="12" x="4" y="8" rx="2"/><path d="M2 14h2M20 14h2M15 13v2M9 13v2"/>`],
];

// Sample data for the replica (documentation address ranges, invented names).
const HOSTS: [string, string, string, string, string[], string, string][] = [
	["web1", "生产 Web 前端", "deploy@192.0.2.10", "密钥 id_ed25519", ["prod", "web"], "Ubuntu 24.04.1 LTS", "10-04 09:12:03"],
	["db", "PostgreSQL 16 主库", "deploy@192.0.2.20", "密码 · sudo", ["prod", "db"], "Debian GNU/Linux 12", "10-04 09:10:41"],
	["staging", "", "ubuntu@198.51.100.7", "密钥 staging.pem", ["staging"], "Ubuntu 22.04.5 LTS", "10-03 18:22:15"],
	["nas", "备份存储", "admin@203.0.113.5:2222", "agent / 默认密钥", ["经 web1", "backup"], "FreeBSD 14.1-RELEASE", "10-02 21:04:56"],
];
const AUDIT: [string, string, string, string, string | null, string][] = [
	["10-04 09:12:03", "exec", "web1", "sudo apt-get upgrade", null, "41.2 s"],
	["10-04 09:10:41", "cp", "db", "./backup.sql → db:/var/backups/", null, "3.4 s"],
	["10-04 09:09:15", "exec", "db", "[sudo] systemctl restart postgresql", null, "2.1 s"],
	["10-04 09:05:52", "file_edit", "web1", "/srv/app/.env", null, "180 ms"],
	["10-04 09:01:30", "exec", "staging", "npm test", "退出 1", "12.8 s"],
	["10-04 08:58:07", "job_start", "web1", "./deploy.sh --tag v2.3.1", null, "0.9 s"],
	["10-04 08:41:19", "session_run", "web1", "cd /srv/app ↵ git pull --ff-only", null, "1.6 s"],
];
const SCREEN = [
	"deploy@web1:~$ sudo apt-get upgrade",
	"Reading package lists... Done",
	"The following packages will be upgraded:",
	"  nginx nginx-common",
	"Do you want to continue? [Y/n] y",
	"Setting up nginx (1.24.0-2ubuntu7.5) ...",
	"deploy@web1:~$ cd /srv/app",
	"deploy@web1:/srv/app$ ",
];

/** xssh-desktop, rebuilt in HTML: the sidebar switches between three of its pages. */
function desktop(note: string): string {
	const nav = NAV.map(([label, path, view]) => {
		const n = view === "sessions" ? `<span class="n">1</span>` : "";
		return view
			? `<button type="button" data-view="${view}"${view === "hosts" ? ' aria-current="true"' : ""}>${ico(path)}<span>${label}</span>${n}</button>`
			: `<span>${ico(path)}<span>${label}</span></span>`;
	}).join("");
	const title = (h: string, p: string, tools = "") =>
		`<div class="app-title"><div><h3>${h}</h3><p>${p}</p></div>${tools ? `<div class="app-tools">${tools}</div>` : ""}</div>`;
	const search = (placeholder: string) => `<span class="app-search">${placeholder}</span>`;
	const refresh = `<span class="app-icon-btn">${ico(`<path d="M3 12a9 9 0 0 1 9-9 9.75 9.75 0 0 1 6.74 2.74L21 8"/><path d="M21 3v5h-5"/><path d="M21 12a9 9 0 0 1-9 9 9.75 9.75 0 0 1-6.74-2.74L3 16"/><path d="M8 16H3v5"/>`)}</span>`;
	const hosts = HOSTS.map(
		([alias, note, addr, auth, tags, os, last]) => `<div class="tbl-row">
  <div class="clip"><strong>${esc(alias)}</strong>${note ? `<span class="sub">${esc(note)}</span>` : ""}</div>
  <div class="clip">${esc(addr)}</div>
  <div class="clip">${esc(auth)}</div>
  <div class="tags">${tags.map((t) => `<span class="tag ${t.startsWith("经") ? "run" : "grey"}">${esc(t)}</span>`).join("")}</div>
  <div class="clip">${esc(os)}<span class="sub">最近连通 ${esc(last)}</span></div>
  <div class="acts"><span>测试</span><span>编辑</span><span>凭据</span><span>删除</span></div>
</div>`,
	).join("");
	const audit = AUDIT.map(
		([ts, act, host, what, bad, took]) => `<div class="tbl-row">
  <div class="clip">${esc(ts)}</div><div class="clip">${esc(act)}</div><div class="clip">${esc(host)}</div>
  <div class="clip mono">${esc(what)}</div>
  <div>${bad ? `<span class="tag wait">${esc(bad)}</span>` : `<span class="dim">成功</span>`}</div>
  <div class="r">${esc(took)}</div>
</div>`,
	).join("");
	const head = (cols: string[]) => `<div class="tbl-head">${cols.map((c) => `<span>${c}</span>`).join("")}</div>`;
	return `<div class="app" role="group" aria-label="xssh desktop">
  <div class="app-bar"><img src="/icon.svg" alt="" width="16" height="16"><span>xssh</span>
    <span class="app-ctl" aria-hidden="true"><span>${ico(`<path d="M5 12h14"/>`, 13)}</span><span>${ico(`<rect x="5" y="5" width="14" height="14" rx="1"/>`, 12)}</span><span>${ico(`<path d="M6 6l12 12M18 6 6 18"/>`, 13)}</span></span>
  </div>
  <div class="app-body">
    <div class="app-side">
      <nav class="app-nav" aria-label="xssh desktop">${nav}</nav>
      <div class="app-status"><div class="k"><span class="d"></span>守护进程</div><div>运行中 · 1 会话 · 0 转发</div></div>
    </div>
    <section class="app-main" data-pane="hosts">
      ${title("主机", "4 台服务器。agent 按别名使用，密码存在系统凭据管理器里，agent 看不到。", `${search("搜索别名、地址、标签或备注")}${refresh}<span class="btn primary">${ico(`<path d="M5 12h14M12 5v14"/>`, 13)}添加主机</span>`)}
      <div class="tbl hosts">${head(["别名 / 备注", "地址", "认证", "跳板 / 标签", "系统", ""])}${hosts}</div>
    </section>
    <section class="app-main" data-pane="sessions" hidden>
      ${title("会话", "1 个交互式会话，由 agent 通过 `xssh session` 打开。这里只读查看。")}
      <div class="sessions">
        <div class="s-list"><div class="s-card"><div class="row">w <span class="tag ok">空闲</span></div><p>web1 · 空闲 12 秒</p><p class="cmd">运行：bash</p><p>最近输入：sudo apt-get upgrade</p></div></div>
        <div class="s-detail">
          <div class="app-title"><div><h3>w <span class="tag ok">空闲</span></h3><p>web1 · 120×32 · 创建于 10-04 09:08:51</p></div></div>
          <pre>${SCREEN.map(esc).join("\n")}</pre>
          <p class="note">只读查看：不会向会话发送任何输入，也不影响 agent 的读取进度。</p>
        </div>
      </div>
    </section>
    <section class="app-main" data-pane="audit" hidden>
      ${title("审计日志", "最近 7 条远程操作（最新在前），来自 audit.jsonl。点击一行查看完整命令。", `${search("筛选主机、动作或命令")}${refresh}`)}
      <div class="tbl audit">${head(["时间", "动作", "主机", "内容", "结果", '<span class="r">耗时</span>'])}${audit}</div>
    </section>
  </div>
</div>
<p class="app-note">${esc(note)}</p>`;
}

export function home(lang: Lang, m: Manifest): string {
	const t = STRINGS[lang];
	const rel = latest(m);
	const file = rel.files[0];
	const pre = lang === "zh" ? "/zh" : "";
	const msg = AGENT_MESSAGE[lang];

	const commands = t.commands
		.map(([c, d]) => `<div class="tbl-row cmd"><dt><code><span class="x">xssh</span>${esc(c.replace(/^xssh/, ""))}</code></dt><dd>${esc(d)}</dd></div>`)
		.join("");

	const states = t.states
		.map(
			([name, line, mean]) =>
				`<li class="tbl-row state"><span><span class="tag ${STATE_TAG[name]}">${esc(name)}</span></span><code>${esc(line)}</code><span class="mean">${esc(mean)}</span></li>`,
		)
		.join("");

	const steps = t.steps
		.map(([h, p], i) => {
			const cmd = i === 0 ? codeLine(msg, t, false) : codeLine(t.stepCmds[i], t);
			// Step 2 is an alternative to step 1, not a next step: it is marked "or", and the steps that follow count on.
			const num = i === 1 ? `<span class="num or" aria-hidden="true">${esc(t.or)}</span>` : `<span class="num" aria-hidden="true">${i === 0 ? 1 : i}</span>`;
			return `<li class="step">${num}<div><h3>${esc(h)}</h3><p>${esc(p)}</p>${cmd}</div></li>`;
		})
		.join("");

	return `
<section class="hero wrap" aria-labelledby="h1">
  <div class="hero-copy">
    <h1 id="h1">${esc(t.h1)}</h1>
    <p class="lede">${esc(t.lede)}</p>
    <div class="ask">
      <span class="label" id="ask-label">${esc(t.messageLabel)}</span>
      <div class="ask-box" role="group" aria-labelledby="ask-label">
        <code>${esc(msg)}</code>
        ${copyBtn(msg, t.copy, t.copied, "primary")}
      </div>
    </div>
    <div class="alts">
      <div>
        <span class="label">${esc(t.orPowershell)}</span>
        ${codeLine(INSTALL_PS, t)}
      </div>
      <div>
        <span class="label">${esc(t.orDownload)}</span>
        <div class="dl-row">
          <a class="btn" href="/dl/${esc(rel.version)}/${esc(file.target)}">${icons.down}<span>${esc(file.name)}</span><span class="dim">${esc(mb(file.size))}</span></a>
          <a class="more" href="${pre}/download">${esc(t.allReleases)}${icons.arrow}</a>
        </div>
      </div>
    </div>
    <p class="meta">${esc(t.meta)}</p>
  </div>
  <figure class="session">
    <div class="session-head">
      <div><div class="who"><strong>w</strong><span class="tag ok">done</span></div><p>session w · web1</p></div>
    </div>
    <pre>${lines(HERO)}</pre>
    <figcaption class="session-foot">${esc(t.heroNote)}</figcaption>
  </figure>
</section>

<section class="band" aria-labelledby="h-compare">
  <div class="wrap">
    <div class="head"><h2 id="h-compare">${esc(t.compareH)}</h2><p>${esc(t.compareP)}</p></div>
    <div class="compare">
      <div class="case bad">
        ${term("ssh", ["$ ssh deploy@web1 'sudo apt-get upgrade'", "[sudo] password for deploy:", "… 120 s, no output …"])}
        <p>${esc(t.sshVerdict)}</p>
      </div>
      <div class="case good">
        ${term("xssh", ["$ xssh session run w -- 'sudo apt-get upgrade'", "Do you want to continue? [Y/n]", "[xssh w waiting prompt=confirm]", "$ xssh session send w y --enter", "[xssh w done exit=0]"])}
        <p>${esc(t.xsshVerdict)}</p>
      </div>
    </div>
  </div>
</section>

<section class="wrap block" aria-labelledby="h-desktop">
  <div class="head"><h2 id="h-desktop">${esc(t.desktopH)}</h2><p>${esc(t.desktopP)}</p></div>
  ${desktop(t.desktopNote)}
</section>

<section class="wrap block" aria-labelledby="h-commands">
  <div class="head"><h2 id="h-commands">${esc(t.commandsH)}</h2><p>${esc(t.commandsP)}</p></div>
  <dl class="tbl cmds"><div class="tbl-head"><span>${esc(t.cols.command)}</span><span>${esc(t.cols.does)}</span></div>${commands}</dl>
</section>

<section class="wrap block" aria-labelledby="h-states">
  <div class="head"><h2 id="h-states">${esc(t.statesH)}</h2><p>${esc(t.statesP)}</p><p class="fmt"><code>[xssh NAME STATE exit=CODE prompt=KIND]</code></p></div>
  <ul class="tbl states"><li class="tbl-head" aria-hidden="true"><span>${esc(t.cols.state)}</span><span>${esc(t.cols.line)}</span><span>${esc(t.cols.meaning)}</span></li>${states}</ul>
</section>

<section class="wrap block keys" aria-labelledby="h-keys">
  <div>
    <div class="head"><h2 id="h-keys">${esc(t.keysH)}</h2><p>${esc(t.keysP)}</p></div>
    <ul class="checks">${t.keysList.map((k) => `<li>${icons.check}<span>${esc(k)}</span></li>`).join("")}</ul>
  </div>
  ${term("xssh", [
		"$ xssh file edit db /srv/app/.env --old 'DB_PASS=old' --new 'DB_PASS={secret:dbpw}' --secrets",
		"edited /srv/app/.env (1 replacement)",
		"12  DB_PASS=***",
		"$ xssh host set-password db",
		`→ ${t.keysAside}`,
	])}
</section>

<section class="wrap block" aria-labelledby="h-install">
  <div class="head"><h2 id="h-install">${esc(t.installH)}</h2><p>${esc(t.installP)}</p></div>
  <ul class="steps">${steps}</ul>
  <p class="agents"><span>${esc(t.agentsLabel)}</span> ${AGENTS.map(esc).join(" · ")}</p>
</section>`;
}
