import { env } from "cloudflare:workers";
import type { Lang, Strings } from "./i18n";
import { STRINGS } from "./i18n";

/** Bump with any change to /assets/site.css or site.js (they are cached for a long time). */
export const ASSET_V = "16";

export const ORIGIN = "https://xssh.io";

export function esc(s: string): string {
	return s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]!);
}

/** Path of the same page in the other language. */
export function langPath(path: string, lang: Lang): string {
	const base = path.replace(/^\/zh(?=\/|$)/, "") || "/";
	return lang === "zh" ? (base === "/" ? "/zh/" : `/zh${base}`) : base;
}

const svg = (d: string) =>
	`<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;

export const icons = {
	copy: svg(`<rect x="9" y="9" width="12" height="12" rx="3"/><path d="M5 15a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2"/>`),
	check: svg(`<path d="M5 12.5l4.5 4.5L19 7.5"/>`),
	down: svg(`<path d="M12 4v11M7 10l5 5 5-5M5 20h14"/>`),
	arrow: svg(`<path d="M5 12h14M13 6l6 6-6 6"/>`),
	github: `<svg viewBox="0 0 24 24" width="16" height="16" fill="currentColor" aria-hidden="true"><path d="M12 2a10 10 0 0 0-3.16 19.49c.5.09.68-.22.68-.48v-1.7c-2.78.6-3.37-1.34-3.37-1.34-.45-1.16-1.11-1.47-1.11-1.47-.91-.62.07-.6.07-.6 1 .07 1.53 1.03 1.53 1.03.89 1.53 2.34 1.09 2.91.83.09-.65.35-1.09.63-1.34-2.22-.25-4.56-1.11-4.56-4.94 0-1.09.39-1.98 1.03-2.68-.1-.25-.45-1.27.1-2.65 0 0 .84-.27 2.75 1.02a9.6 9.6 0 0 1 5 0c1.91-1.29 2.75-1.02 2.75-1.02.55 1.38.2 2.4.1 2.65.64.7 1.03 1.59 1.03 2.68 0 3.84-2.34 4.68-4.57 4.93.36.31.68.92.68 1.85v2.75c0 .27.18.58.69.48A10 10 0 0 0 12 2z"/></svg>`,
};

/** Escapes text, then turns `code` spans into <code>. */
export function inline(s: string): string {
	return esc(s).replace(/`([^`]+)`/g, "<code>$1</code>");
}

/** A copy button; the label swaps to `done` with a check for a moment after copying. */
export function copyBtn(text: string, label: string, done: string, cls = ""): string {
	return `<button class="copy ${cls}" type="button" data-copy="${esc(text)}" data-done="${esc(done)}"><span class="i">${icons.copy}</span><span class="t">${esc(label)}</span></button>`;
}

/** One line of code with a copy button. */
export function codeLine(text: string, t: Strings, prompt = true): string {
	return `<div class="codeline"><code>${prompt ? '<span class="ps">&gt;</span> ' : ""}${esc(text)}</code>${copyBtn(text, t.copy, t.copied, "ghost")}</div>`;
}

interface Page {
	lang: Lang;
	path: string;
	title?: string;
	description?: string;
	current?: "docs" | "download";
	filed: string;
	version: string;
	body: string;
}

export function layout(p: Page): string {
	const t: Strings = STRINGS[p.lang];
	const title = p.title ? `${p.title} · xssh` : t.title;
	const desc = p.description ?? t.description;
	const en = langPath(p.path, "en");
	const zh = langPath(p.path, "zh");
	const pre = p.lang === "zh" ? "/zh" : "";
	const home = p.lang === "zh" ? "/zh/" : "/";
	const cur = (k: string) => (p.current === k ? ' aria-current="page"' : "");
	const other = p.lang === "zh" ? en : zh;
	const otherLang = p.lang === "zh" ? "en" : "zh-CN";
	return `<!doctype html>
<html lang="${t.htmlLang}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>${esc(title)}</title>
<meta name="description" content="${esc(desc)}">
<link rel="canonical" href="${ORIGIN}${p.lang === "zh" ? zh : en}">
<link rel="alternate" hreflang="en" href="${ORIGIN}${en}">
<link rel="alternate" hreflang="zh-CN" href="${ORIGIN}${zh}">
<link rel="alternate" hreflang="x-default" href="${ORIGIN}${en}">
<link rel="alternate" type="text/plain" title="llms.txt" href="/llms.txt">
<link rel="icon" href="/favicon.ico" sizes="any">
<link rel="icon" href="/icon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/apple-touch-icon.png">
<meta name="theme-color" content="#f8f8f8">
<meta name="color-scheme" content="light">
<meta property="og:type" content="website">
<meta property="og:title" content="${esc(title)}">
<meta property="og:description" content="${esc(desc)}">
<meta property="og:url" content="${ORIGIN}${p.lang === "zh" ? zh : en}">
<meta property="og:site_name" content="xssh">
<meta property="og:locale" content="${p.lang === "zh" ? "zh_CN" : "en_US"}">
<meta property="og:image" content="${ORIGIN}/og-${p.lang}.png">
<meta property="og:image:width" content="1200">
<meta property="og:image:height" content="630">
<meta property="og:image:alt" content="${esc(t.h1)}">
<meta name="twitter:card" content="summary_large_image">
<link rel="stylesheet" href="/assets/site.css?v=${ASSET_V}">
<script src="/assets/site.js?v=${ASSET_V}" defer></script>
${env.ANALYTICS_TOKEN ? `<script defer src="https://static.cloudflareinsights.com/beacon.min.js" data-cf-beacon='{"token":"${esc(env.ANALYTICS_TOKEN)}"}'></script>` : ""}
</head>
<body>
<a class="skip" href="#main">${esc(t.skip)}</a>
<p class="sr" aria-live="polite" data-live data-msg="${esc(t.copiedSr)}"></p>
<header class="top">
  <div class="wrap top-in">
    <a class="brand" href="${home}" aria-label="xssh home"><img src="/icon.svg" alt="" width="22" height="22"><span>xssh</span></a>
    <a class="tag" href="${pre}/download">v${esc(p.version)}</a>
    <nav class="nav" aria-label="Main">
      <a href="${pre}/docs"${cur("docs")}>${esc(t.nav.docs)}</a>
      <a href="${pre}/download"${cur("download")}>${esc(t.nav.download)}</a>
      <a href="https://github.com/crackxia/xssh" aria-label="GitHub">${icons.github}<span class="gh">${esc(t.nav.github)}</span></a>
      <a class="lang" href="${other}" hreflang="${otherLang}" lang="${otherLang}">${esc(t.nav.other)}</a>
    </nav>
  </div>
</header>
<main id="main">
${p.body}
</main>
<footer class="foot">
  <div class="wrap foot-in">
    <a class="brand small" href="${home}"><img src="/icon.svg" alt="" width="18" height="18"><span>xssh</span></a>
    <p>${esc(t.footer)}</p>
    <nav aria-label="Footer">
      <a href="${pre}/docs">${esc(t.footLinks.docs)}</a>
      <a href="${pre}/download">${esc(t.footLinks.download)}</a>
      <a href="https://github.com/crackxia/xssh">${esc(t.footLinks.github)}</a>
      <a href="/llms.txt">${esc(t.footLinks.llms)}</a>
    </nav>
  </div>
</footer>
</body>
</html>`;
}
