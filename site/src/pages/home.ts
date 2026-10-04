import { AGENT_MESSAGE, AGENTS, INSTALL_PS, type Lang, STRINGS } from "../i18n";
import { codeLine, copyBtn, esc, icons } from "../html";
import { latest, type Manifest, mb } from "../releases";

/** A terminal panel. Lines: "$ cmd" is input, "[xssh …]" a status line, anything else output. */
function term(title: string, lines: string[], cls = ""): string {
	const body = lines
		.map((l) => {
			if (l.startsWith("$ ")) return `<span class="in"><span class="ps">$</span> ${esc(l.slice(2))}</span>`;
			const st = l.match(/^\[xssh \S+ (\S+)/);
			if (st) return `<span class="st st-${st[1]}">${esc(l)}</span>`;
			return `<span class="out">${esc(l)}</span>`;
		})
		.join("\n");
	return `<figure class="term ${cls}"><figcaption>${esc(title)}</figcaption><pre>${body}</pre></figure>`;
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

export function home(lang: Lang, m: Manifest): string {
	const t = STRINGS[lang];
	const rel = latest(m);
	const file = rel.files[0];
	const pre = lang === "zh" ? "/zh" : "";
	const msg = AGENT_MESSAGE[lang];

	const commands = t.commands
		.map(([c, d]) => `<div class="cmd"><dt><code><span class="x">xssh</span>${esc(c.replace(/^xssh/, ""))}</code></dt><dd>${esc(d)}</dd></div>`)
		.join("");

	const states = t.states
		.map(
			([name, line, mean]) =>
				`<li class="state"><span class="name"><span class="dot s-${name}" aria-hidden="true"></span>${esc(name)}</span><code class="st st-${name}">${esc(line)}</code><span class="mean">${esc(mean)}</span></li>`,
		)
		.join("");

	const steps = t.steps
		.map(([h, p], i) => {
			const cmd = i === 0 ? codeLine(msg, t, false) : codeLine(t.stepCmds[i], t);
			return `<li class="step"><span class="num" aria-hidden="true">${i + 1}</span><div><h3>${esc(h)}</h3><p>${esc(p)}</p>${cmd}</div></li>`;
		})
		.join("");

	return `
<section class="hero wrap" aria-labelledby="h1">
  <div class="hero-copy">
    <h1 id="h1">${esc(t.h1)}<span class="cursor" aria-hidden="true"></span></h1>
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
  <div class="hero-art">${term("session w @ web1", HERO)}</div>
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

<section class="wrap block" aria-labelledby="h-commands">
  <div class="head"><h2 id="h-commands">${esc(t.commandsH)}</h2><p>${esc(t.commandsP)}</p></div>
  <dl class="cmds">${commands}</dl>
</section>

<section class="wrap block" aria-labelledby="h-states">
  <div class="head"><h2 id="h-states">${esc(t.statesH)}</h2><p>${esc(t.statesP)}</p><p class="fmt"><code>[xssh NAME STATE exit=CODE prompt=KIND]</code></p></div>
  <ul class="states">${states}</ul>
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
  <ol class="steps">${steps}</ol>
  <p class="agents"><span>${esc(t.agentsLabel)}</span> ${AGENTS.map(esc).join(" · ")}</p>
</section>`;
}
