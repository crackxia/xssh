export type Lang = "en" | "zh";

export const AGENT_MESSAGE: Record<Lang, string> = {
	en: "Install xssh: read https://xssh.io/llms.txt and follow it.",
	zh: "安装 xssh：阅读 https://xssh.io/llms.txt 并照做。",
};

export const INSTALL_PS = "irm https://xssh.io/install.ps1 | iex";

export const AGENTS = [
	"Claude Code", "Codex", "Gemini CLI", "GitHub Copilot", "Cursor", "OpenCode", "Windsurf",
	"Amp", "Qwen Code", "Kiro", "Trae", "Roo Code", "~/.agents (Cline…)",
];

const en = {
	htmlLang: "en",
	title: "xssh: an SSH client built for AI agents",
	description:
		"SSH for AI agents: persistent shells, prompt handling, long jobs, safe edits. Secrets stay with you. One Rust binary, MIT.",
	skip: "Skip to content",
	nav: { docs: "Docs", download: "Download", github: "GitHub", other: "中文" },
	h1: "An SSH client built for AI agents",
	lede: "ssh hangs on prompts, forgets every cd, and needs your passwords. xssh keeps shells alive across tool calls, reports state in one line, and keeps secrets from the agent.",
	messageLabel: "Tell your agent",
	copy: "Copy",
	copied: "Copied",
	copiedSr: "Copied to clipboard",
	orPowershell: "Or PowerShell",
	orDownload: "Or download",
	allReleases: "All releases",
	meta: "Windows x64 · Remote: Linux, macOS, FreeBSD · MIT",
	heroNote: "Output as the agent sees it.",

	compareH: "No more hanging on prompts",
	compareP: "Same job, two tools.",
	sshVerdict: "Times out at the password prompt.",
	xsshVerdict: "The prompt is reported and answered. The shell keeps its state.",

	desktopH: "A desktop manager for the human",
	desktopP: "Hosts, sessions, forwards, jobs, audit log and daemon in one window. Watching never disturbs the agent.",
	desktopNote: "Interface preview with sample data. The app's interface is in Chinese.",

	cols: { command: "Command", does: "What it does", state: "State", line: "Status line", meaning: "Meaning" },

	commandsH: "One command, one exact result",
	commandsP: "Terse, ANSI-free output with exit codes and next steps.",
	commands: [
		["xssh host list", "Saved servers: notes, tags, OS, last success."],
		["xssh exec H -- cmd", "One host, many, or @tag in parallel. Real exit codes."],
		["xssh session run N -- cmd", "A shell that keeps cwd, env and programs across calls."],
		["xssh session send N …", "Answer prompts. Drive REPLs, vim, top."],
		["xssh job start H -- cmd", "Outlives timeouts and disconnects."],
		["xssh file edit H PATH", "Exact, atomic replace with backup."],
		["xssh cp SRC… DST", "rsync-like. Skips unchanged files, resumes large ones."],
		["xssh forward add H -L …", "-L, -R, SOCKS5, held by the daemon."],
		["xssh status | diag | logs H", "Health, diagnosis with the next step, logs."],
		["xssh host trust H", "Host key changed? A human confirms."],
	] as [string, string][],

	statesH: "Six states. No guessing.",
	statesP: "Every reply ends with:",
	states: [
		["done", "[xssh w done exit=0]", "Finished. Real exit code."],
		["waiting", "[xssh w waiting prompt=confirm]", "Prompt open. Answer with session send."],
		["running", "[xssh w running]", "Still printing. Read or wait."],
		["quiet", "[xssh w quiet]", "Running, silent."],
		["tui", "[xssh w tui]", "Full-screen app. The reply is the screen."],
		["disconnected", "[xssh w disconnected]", "Dropped (exit 69). Persistent sessions reattach."],
	] as [string, string, string][],

	keysH: "Secrets stay with you",
	keysP: "Agents get capabilities, not credentials.",
	keysList: [
		"Passwords live in the OS keyring.",
		"You type them in a terminal or dialog, never in chat.",
		"sudo is filled from the keyring.",
		"{secret:NAME} injects secrets; output shows ***.",
		"Changed host keys are never bypassed.",
	],
	keysAside: "typed by you in a dialog",

	installH: "Install",
	installP: "Windows x64. Remote: any POSIX shell.",
	steps: [
		["Tell your agent", "It installs xssh and its skill."],
		["Or do it yourself", "One line, or unzip anywhere."],
		["Add passwords", "You type each once. The agent uses the alias."],
	] as [string, string][],
	stepCmds: ["", INSTALL_PS, "xssh host add web1 --host 10.0.0.5 --user deploy --ask-password"],
	agentsLabel: "Skill for",
	or: "or",

	footer: "MIT licensed.",
	footLinks: { docs: "Docs", download: "Download", github: "GitHub", llms: "llms.txt" },

	docs: {
		title: "xssh guide",
		lede: "What your agent reads. Same as `xssh guide`.",
		toc: "On this page",
		raw: "Raw markdown",
		langNote: "",
	},
	download: {
		title: "Download",
		lede: "Unzip anywhere. Data stays in `data\\`.",
		latest: "Latest",
		size: "Size",
		sha: "SHA-256",
		count: "Downloads",
		get: "Download",
		notes: "Changes",
		verify: "Verify",
		install: "Install",
		agentsNote: "Agents: `xssh.io/llms.txt`, `xssh.io/latest.json`.",
	},
	notFound: { title: "Not found", text: "Nothing here.", back: "Home" },
};

export type Strings = typeof en;

const zh: Strings = {
	htmlLang: "zh-CN",
	title: "xssh：专为 AI agent 打造的 SSH 客户端",
	description: "AI agent 专用 SSH 客户端：持久 shell、提示处理、长任务、安全编辑，密码留在你手里。单一 Rust 程序，MIT。",
	skip: "跳到正文",
	nav: { docs: "文档", download: "下载", github: "GitHub", other: "English" },
	h1: "专为 AI agent 打造的 SSH 客户端",
	lede: "ssh 会卡在提示上、忘掉 cd、要你的密码。xssh 跨工具调用保持 shell，一行报告状态，密码不给 agent。",
	messageLabel: "发给你的 agent",
	copy: "复制",
	copied: "已复制",
	copiedSr: "已复制到剪贴板",
	orPowershell: "或 PowerShell",
	orDownload: "或下载",
	allReleases: "全部版本",
	meta: "Windows x64 · 远端：Linux、macOS、FreeBSD · MIT",
	heroNote: "agent 看到的输出就是这样。",

	compareH: "不再卡在提示上",
	compareP: "同一件事，两个工具。",
	sshVerdict: "卡在密码提示，超时。",
	xsshVerdict: "提示被报告、被回答。shell 状态保留。",

	desktopH: "给人用的桌面管理器",
	desktopP: "主机、会话、端口转发、后台任务、审计日志和守护进程，一个窗口管理。查看会话不会打扰 agent。",
	desktopNote: "界面示意，数据为示例。",

	cols: { command: "命令", does: "作用", state: "状态", line: "状态行", meaning: "含义" },

	commandsH: "一条命令，一个精确结果",
	commandsP: "输出简洁、无颜色码，带退出码和下一步。",
	commands: [
		["xssh host list", "已存服务器：备注、标签、系统、上次成功。"],
		["xssh exec H -- cmd", "单台、多台或 @tag 并行。真实退出码。"],
		["xssh session run N -- cmd", "跨调用保留 cwd、环境变量和程序的 shell。"],
		["xssh session send N …", "回答提示，驱动 REPL、vim、top。"],
		["xssh job start H -- cmd", "不怕超时和断线。"],
		["xssh file edit H PATH", "精确原子替换，自动备份。"],
		["xssh cp SRC… DST", "rsync 式：跳过未变文件，大文件续传。"],
		["xssh forward add H -L …", "-L、-R、SOCKS5，由守护进程托管。"],
		["xssh status | diag | logs H", "健康、诊断与下一步、日志。"],
		["xssh host trust H", "主机密钥变了？由人确认。"],
	],

	statesH: "六种状态，不靠猜",
	statesP: "每次回复以此结尾：",
	states: [
		["done", "[xssh w done exit=0]", "已结束，退出码真实。"],
		["waiting", "[xssh w waiting prompt=confirm]", "有提示，用 session send 回答。"],
		["running", "[xssh w running]", "仍在输出，再读或等待。"],
		["quiet", "[xssh w quiet]", "在运行，无输出。"],
		["tui", "[xssh w tui]", "全屏程序，回复即屏幕。"],
		["disconnected", "[xssh w disconnected]", "断开（exit 69），持久会话自动重连。"],
	],

	keysH: "密码留在你手里",
	keysP: "agent 拿到能力，不拿凭据。",
	keysList: [
		"密码存于系统钥匙串。",
		"由你在终端或对话框输入，不进对话。",
		"sudo 自动从钥匙串填写。",
		"{secret:NAME} 注入秘密，输出显示 ***。",
		"主机密钥变化绝不绕过。",
	],
	keysAside: "由你在对话框输入",

	installH: "安装",
	installP: "Windows x64。远端：任意 POSIX shell。",
	steps: [
		["告诉 agent", "它自己装好 xssh 和 skill。"],
		["或自己装", "一行命令，或解压到任意位置。"],
		["录入密码", "你输入一次，agent 只用别名。"],
	],
	stepCmds: ["", INSTALL_PS, "xssh host add web1 --host 10.0.0.5 --user deploy --ask-password"],
	agentsLabel: "skill 支持",
	or: "或",

	footer: "MIT 开源。",
	footLinks: { docs: "文档", download: "下载", github: "GitHub", llms: "llms.txt" },

	docs: {
		title: "xssh 手册",
		lede: "agent 读的手册，即 `xssh guide`。",
		toc: "本页目录",
		raw: "原始 markdown",
		langNote: "正文为英文，与命令输出一致。",
	},
	download: {
		title: "下载",
		lede: "解压即用，数据存于 `data\\`。",
		latest: "最新",
		size: "大小",
		sha: "SHA-256",
		count: "下载次数",
		get: "下载",
		notes: "更新内容",
		verify: "校验",
		install: "安装",
		agentsNote: "Agent：`xssh.io/llms.txt`、`xssh.io/latest.json`。",
	},
	notFound: { title: "页面不存在", text: "这里没有内容。", back: "回首页" },
};

export const STRINGS: Record<Lang, Strings> = { en, zh };
