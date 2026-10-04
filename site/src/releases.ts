import { env } from "cloudflare:workers";

export interface ReleaseFile {
	/** Platform id used in download URLs, e.g. windows-x86_64. */
	target: string;
	name: string;
	size: number;
	sha256: string;
}

export interface Release {
	version: string;
	date: string;
	notes: { en: string[]; zh: string[] };
	files: ReleaseFile[];
}

export interface Manifest {
	latest: string;
	releases: Release[];
}

/** Used when KV has no manifest yet (first deploy); `scripts/release.mjs` writes the real one. */
const FALLBACK: Manifest = {
	latest: "0.2.1",
	releases: [
		{
			version: "0.2.1",
			date: "2026-10-04",
			notes: {
				en: [
					"Skill: the full xssh path is written only when xssh is not already on PATH, saving agent context.",
					"Desktop Integrations: adding xssh to or removing it from PATH marks installed skills for update.",
				],
				zh: [
					"Skill：仅在 xssh 未加入 PATH 时写入完整路径，节省 agent 上下文。",
					"桌面端集成页：加入 / 移出 PATH 后，已安装的 skill 显示为需更新。",
				],
			},
			files: [
				{
					target: "windows-x86_64",
					name: "xssh-0.2.1-windows-x86_64.zip",
					size: 14751669,
					sha256: "eb8b2d26e568726c4f162f7fa2de97856d5a6554e69688a7f4d2c204ae001ac3",
				},
			],
		},
		{
			version: "0.2.0",
			date: "2026-10-04",
			notes: {
				en: [
					"First public release.",
					"Hosts: --expires, prune, export to ssh_config, duplicate logins flagged.",
					"Connect errors show last success and address; local proxy/TUN detected.",
					"Dead pooled connections replaced; daemon stop/restart refuse while others' sessions are open.",
					"No terminal: password and host key prompts use a desktop dialog.",
					"file write/edit --secrets, job start -q, xssh path.",
				],
				zh: [
					"首个公开版本。",
					"主机：--expires、prune、导出 ssh_config，标出重复登录。",
					"连接失败显示上次成功时间与地址；识别本地代理/TUN。",
					"失效连接自动替换；他人会话未结束时拒绝 daemon stop/restart。",
					"无终端时，密码与主机密钥确认改用桌面对话框。",
					"file write/edit --secrets、job start -q、xssh path。",
				],
			},
			files: [
				{
					target: "windows-x86_64",
					name: "xssh-0.2.0-windows-x86_64.zip",
					size: 14755342,
					sha256: "9b3dffc1a98789aab2381cfea51cb5da88748f82ec241fc643c8e6f66d56da45",
				},
			],
		},
	],
};

export async function manifest(): Promise<Manifest> {
	try {
		const m = await env.MANIFEST.get<Manifest>("manifest", { type: "json", cacheTtl: 60 });
		if (m?.releases?.length) return m;
	} catch {
		// KV unavailable: serve the bundled manifest.
	}
	return FALLBACK;
}

export function latest(m: Manifest): Release {
	return m.releases.find((r) => r.version === m.latest) ?? m.releases[0];
}

export function r2Key(version: string, name: string): string {
	return `releases/${version}/${name}`;
}

export function mb(bytes: number): string {
	return `${(bytes / 1048576).toFixed(1)} MB`;
}

/** Download counts per version+target from D1 (empty map when D1 is unavailable). */
export async function counts(): Promise<Map<string, number>> {
	const out = new Map<string, number>();
	try {
		const { results } = await env.DB.prepare(
			"SELECT version, target, SUM(count) AS n FROM downloads GROUP BY version, target",
		).all<{ version: string; target: string; n: number }>();
		for (const r of results) out.set(`${r.version}/${r.target}`, r.n);
	} catch {
		// No table yet or D1 unavailable.
	}
	return out;
}

export async function countDownload(version: string, target: string): Promise<void> {
	const day = new Date().toISOString().slice(0, 10);
	await env.DB.prepare(
		"INSERT INTO downloads (version, target, day, count) VALUES (?1, ?2, ?3, 1) " +
			"ON CONFLICT (version, target, day) DO UPDATE SET count = count + 1",
	)
		.bind(version, target, day)
		.run();
}
