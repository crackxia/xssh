import { codeLine, copyBtn, esc, icons, inline } from "../html";
import { INSTALL_PS, type Lang, STRINGS } from "../i18n";
import type { Manifest } from "../releases";
import { mb } from "../releases";

export function download(lang: Lang, m: Manifest, counts: Map<string, number>): string {
	const s = STRINGS[lang];
	const t = s.download;
	const releases = m.releases
		.map((r) => {
			const files = r.files
				.map((f) => {
					const n = counts.get(`${r.version}/${f.target}`) ?? 0;
					const verify = `(Get-FileHash .\\${f.name} -Algorithm SHA256).Hash -eq '${f.sha256.toUpperCase()}'`;
					return `
<div class="file">
  <div class="file-main">
    <div><code class="fname">${esc(f.name)}</code><p class="dim">${esc(t.size)} ${esc(mb(f.size))} · ${esc(t.count)} ${n.toLocaleString("en-US")}</p></div>
    <a class="btn primary" href="/dl/${esc(r.version)}/${esc(f.target)}">${icons.down}<span>${esc(t.get)}</span></a>
  </div>
  <dl class="facts">
    <div><dt>${esc(t.sha)}</dt><dd class="hash"><code>${esc(f.sha256)}</code>${copyBtn(f.sha256, s.copy, s.copied, "ghost")}</dd></div>
    <div><dt>${esc(t.verify)}</dt><dd>${codeLine(verify, s)}</dd></div>
  </dl>
</div>`;
				})
				.join("");
			return `
<section class="release" aria-labelledby="v-${esc(r.version)}">
  <div class="release-head">
    <h2 id="v-${esc(r.version)}">v${esc(r.version)}</h2>
    ${r.version === m.latest ? `<span class="tag ok">${esc(t.latest)}</span>` : ""}
    <time class="dim" datetime="${esc(r.date)}">${esc(r.date)}</time>
  </div>
  <div class="release-body">
  ${files}
  <h3>${esc(t.notes)}</h3>
  <ul class="notes">${r.notes[lang].map((n) => `<li>${esc(n)}</li>`).join("")}</ul>
  </div>
</section>`;
		})
		.join("");

	return `
<div class="wrap page narrow">
  <header class="page-head"><h1>${esc(t.title)}</h1><p>${inline(t.lede)}</p></header>
  <div class="install">
    <span class="label">${esc(t.install)}</span>
    ${codeLine(INSTALL_PS, s)}
    <p class="dim">${inline(t.agentsNote)}</p>
  </div>
  ${releases}
</div>`;
}

export function notFound(lang: Lang): string {
	const t = STRINGS[lang].notFound;
	return `
<div class="wrap page nf">
  <p class="code404">404</p>
  <h1>${esc(t.title)}</h1>
  <p>${esc(t.text)}</p>
  <a class="btn" href="${lang === "zh" ? "/zh/" : "/"}">${esc(t.back)}</a>
</div>`;
}
