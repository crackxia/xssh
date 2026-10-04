import { env } from "cloudflare:workers";
import { INSTALL_PS1, latestJson, llmsFull, llmsTxt } from "./agent";
import { langPath, layout, ORIGIN } from "./html";
import type { Lang } from "./i18n";
import { STRINGS } from "./i18n";
import { docs, GUIDE_MD } from "./pages/docs";
import { download, notFound } from "./pages/download";
import { home } from "./pages/home";
import { counts, countDownload, latest, manifest, r2Key } from "./releases";

const SECURITY: Record<string, string> = {
	"x-content-type-options": "nosniff",
	"referrer-policy": "strict-origin-when-cross-origin",
	"content-security-policy":
		"default-src 'self'; script-src 'self' https://static.cloudflareinsights.com; connect-src 'self' https://cloudflareinsights.com; img-src 'self' data:; style-src 'self' 'unsafe-inline'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
};

const PAGES = new Set(["/", "/docs", "/download"]);
const LANG_COOKIE = (l: Lang) => `lang=${l}; Path=/; Max-Age=31536000; SameSite=Lax; Secure`;

/** Every page in both languages, cross-linked with hreflang. */
function sitemap(): string {
	const urls = [...PAGES]
		.map((p) => {
			const en = `${ORIGIN}${langPath(p, "en")}`;
			const zh = `${ORIGIN}${langPath(p, "zh")}`;
			const alt = `<xhtml:link rel="alternate" hreflang="en" href="${en}"/><xhtml:link rel="alternate" hreflang="zh-CN" href="${zh}"/><xhtml:link rel="alternate" hreflang="x-default" href="${en}"/>`;
			return `<url><loc>${en}</loc>${alt}</url><url><loc>${zh}</loc>${alt}</url>`;
		})
		.join("");
	return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9" xmlns:xhtml="http://www.w3.org/1999/xhtml">${urls}</urlset>\n`;
}

/** The site language the browser ranks highest (q-weighted), or null when it lists neither. */
function preferredLang(header: string | null): Lang | null {
	if (!header) return null;
	const ranked = header
		.split(",")
		.map((part, i) => {
			const [tag, ...params] = part.trim().toLowerCase().split(";");
			const q = params.map((p) => p.trim()).find((p) => p.startsWith("q="));
			return { tag: tag.trim(), q: q ? Number(q.slice(2)) || 0 : 1, i };
		})
		.filter((t) => t.q > 0)
		.sort((a, b) => b.q - a.q || a.i - b.i);
	for (const { tag } of ranked) {
		if (tag === "zh" || tag.startsWith("zh-")) return "zh";
		if (tag === "en" || tag.startsWith("en-")) return "en";
	}
	return null;
}

/** The language this visitor last viewed; absent on a first visit. */
function savedLang(req: Request): Lang | null {
	const m = req.headers.get("cookie")?.match(/(?:^|;\s*)lang=(en|zh)(?:;|$)/);
	return m ? (m[1] as Lang) : null;
}

function html(body: string, status = 200): Response {
	return new Response(body, {
		status,
		headers: { "content-type": "text/html; charset=utf-8", "cache-control": "public, max-age=300", ...SECURITY },
	});
}

function text(body: string, type: string): Response {
	return new Response(body, {
		headers: { "content-type": `${type}; charset=utf-8`, "cache-control": "public, max-age=300", "access-control-allow-origin": "*" },
	});
}

async function serveDownload(req: Request, ctx: ExecutionContext, version: string, target: string): Promise<Response> {
	const m = await manifest();
	if (version === "latest") {
		const r = latest(m);
		return Response.redirect(new URL(`/dl/${r.version}/${target}`, req.url).toString(), 302);
	}
	const rel = m.releases.find((r) => r.version === version);
	const file = rel?.files.find((f) => f.target === target);
	if (!rel || !file) return new Response("not found\n", { status: 404 });
	const obj = await env.RELEASES.get(r2Key(rel.version, file.name), { range: req.headers, onlyIf: req.headers });
	if (!obj) return new Response("release file missing\n", { status: 404 });
	const headers = new Headers();
	obj.writeHttpMetadata(headers);
	headers.set("etag", obj.httpEtag);
	headers.set("content-type", "application/zip");
	headers.set("content-disposition", `attachment; filename="${file.name}"`);
	headers.set("cache-control", "public, max-age=31536000, immutable");
	headers.set("accept-ranges", "bytes");
	if (!("body" in obj)) return new Response(null, { status: 304, headers });
	const range = obj.range as { offset?: number; length?: number } | undefined;
	const partial = req.headers.has("range") && range;
	// Count whole downloads and the first chunk of ranged ones, not every resumed chunk.
	if (req.method === "GET" && (!partial || !range.offset)) {
		ctx.waitUntil(countDownload(rel.version, file.target).catch(() => {}));
	}
	if (partial) {
		const offset = range.offset ?? 0;
		const length = range.length ?? obj.size - offset;
		headers.set("content-range", `bytes ${offset}-${offset + length - 1}/${obj.size}`);
		headers.set("content-length", String(length));
		return new Response(obj.body, { status: 206, headers });
	}
	headers.set("content-length", String(obj.size));
	return new Response(obj.body, { headers });
}

export default {
	async fetch(req, _env, ctx) {
		const url = new URL(req.url);
		if (url.hostname === "www.xssh.io") {
			url.hostname = "xssh.io";
			return Response.redirect(url.toString(), 301);
		}
		if (req.method !== "GET" && req.method !== "HEAD") return new Response("method not allowed\n", { status: 405 });

		let path = url.pathname;
		if (path === "/zh") return Response.redirect(new URL("/zh/", url).toString(), 301);
		if (path.length > 1 && path.endsWith("/") && path !== "/zh/") {
			return Response.redirect(new URL(path.slice(0, -1) + url.search, url).toString(), 301);
		}
		const lang: Lang = path === "/zh/" || path.startsWith("/zh/") ? "zh" : "en";
		const page = lang === "zh" ? path.replace(/^\/zh/, "") || "/" : path;

		switch (path) {
			case "/llms.txt":
				return text(llmsTxt(await manifest()), "text/plain");
			case "/llms-full.txt":
				return text(llmsFull(await manifest()), "text/plain");
			case "/guide.md":
				return text(GUIDE_MD, "text/markdown");
			case "/install.ps1":
				return text(INSTALL_PS1, "text/plain");
			case "/robots.txt":
				return text(`User-agent: *\nAllow: /\nSitemap: ${ORIGIN}/sitemap.xml\n`, "text/plain");
			case "/sitemap.xml":
				return new Response(sitemap(), { headers: { "content-type": "application/xml; charset=utf-8", "cache-control": "public, max-age=3600" } });
			case "/latest.json":
				return new Response(JSON.stringify(latestJson(await manifest()), null, 2), {
					headers: { "content-type": "application/json", "cache-control": "public, max-age=60", "access-control-allow-origin": "*" },
				});
		}
		const dl = path.match(/^\/dl\/([\w.-]+)\/([\w-]+)$/);
		if (dl) return serveDownload(req, ctx, dl[1], dl[2]);

		// Language: a /zh/ URL is an explicit choice and is always served. An unprefixed (English) URL
		// entered from outside the site (typed, bookmark, external link) redirects to /zh/ when the
		// remembered language, or on a first visit the browser's, is Chinese. Every page served records
		// its language in the cookie, so the language switch works without JS.
		let setLang: Lang | null = null;
		if (PAGES.has(page)) {
			const saved = savedLang(req);
			const site = req.headers.get("sec-fetch-site");
			const internal = site ? site === "same-origin" : (req.headers.get("referer") ?? "").startsWith(`${url.origin}/`);
			if (lang === "en" && !internal && (saved ?? preferredLang(req.headers.get("accept-language"))) === "zh") {
				return new Response(null, {
					status: 302,
					headers: {
						location: langPath(path, "zh") + url.search,
						"set-cookie": LANG_COOKIE("zh"),
						"cache-control": "private, no-store",
						vary: "accept-language, cookie",
					},
				});
			}
			if (saved !== lang && (saved || lang === "zh" || internal)) setLang = lang;
		}
		const res = await render(page, lang, path);
		if (PAGES.has(page)) {
			// Entry may redirect by cookie/browser language, so the browser revalidates instead of reusing.
			res.headers.set("cache-control", "private, no-cache");
			res.headers.set("vary", "accept-language, cookie");
		}
		if (setLang) res.headers.append("set-cookie", LANG_COOKIE(setLang));
		return res;
	},
} satisfies ExportedHandler;

async function render(page: string, lang: Lang, path: string): Promise<Response> {
	const m = await manifest();
	const rel = latest(m);
	const base = { lang, path, filed: rel.date, version: rel.version };
	switch (page) {
		case "/":
			return html(layout({ ...base, body: home(lang, m) }));
		case "/docs":
			return html(layout({ ...base, current: "docs", title: STRINGS[lang].docs.title, body: docs(lang) }));
		case "/download":
			return html(layout({ ...base, current: "download", title: STRINGS[lang].download.title, body: download(lang, m, await counts()) }));
	}
	return html(layout({ ...base, title: STRINGS[lang].notFound.title, body: notFound(lang) }), 404);
}
