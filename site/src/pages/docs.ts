import { Marked } from "marked";
import guide from "../../../docs/guide.md?raw";
import { esc, inline } from "../html";
import { type Lang, STRINGS } from "../i18n";

export const GUIDE_MD: string = guide;

function slug(s: string): string {
	return s
		.toLowerCase()
		.replace(/<[^>]+>/g, "")
		.replace(/[^a-z0-9]+/g, "-")
		.replace(/^-|-$/g, "");
}

/** The guide rendered once per isolate: sections (h2) get ids for the field index. */
let rendered: { html: string; toc: { id: string; text: string }[] } | undefined;

function render() {
	if (rendered) return rendered;
	const toc: { id: string; text: string }[] = [];
	const marked = new Marked({
		gfm: true,
		renderer: {
			// Short spans never wrap (a line break inside `--` reads as two flags); long ones may.
			codespan({ text }) {
				return `<code${text.length <= 24 ? ' class="nb"' : ""}>${esc(text)}</code>`;
			},
			heading({ tokens, depth }) {
				const text = this.parser.parseInline(tokens);
				if (depth === 2) {
					const id = slug(text);
					toc.push({ id, text });
					return `<h2 id="${id}">${text}</h2>\n`;
				}
				// The page header carries the title; the guide's own h1 is dropped.
				if (depth === 1) return "";
				return `<h${depth}>${text}</h${depth}>\n`;
			},
		},
	});
	rendered = { html: marked.parse(GUIDE_MD, { async: false }) as string, toc };
	return rendered;
}

export function docs(lang: Lang): string {
	const t = STRINGS[lang].docs;
	const { html, toc } = render();
	return `
<div class="wrap page">
<header class="page-head"><h1>${esc(t.title)}</h1><p>${inline(t.lede)}</p>${t.langNote ? `<p class="dim">${esc(t.langNote)}</p>` : ""}</header>
<div class="docs">
  <aside class="toc">
    <nav aria-label="${esc(t.toc)}">
      <span class="label">${esc(t.toc)}</span>
      <ol>${toc.map((h) => `<li><a href="#${h.id}">${h.text}</a></li>`).join("")}</ol>
      <a class="more" href="/guide.md">${esc(t.raw)}</a>
    </nav>
  </aside>
  <article class="prose" lang="en">${html}</article>
</div>
</div>`;
}
