---
version: 1
slug: "site"
primary_target: "site"
related_targets: []
---

# Surface: xssh.io website

Scope: landing (`/`, `/zh/`, Persuade), docs (`/docs`, Read: renders docs/guide.md), download (`/download`, releases, sha256, counts), agent entry points (`/llms.txt`, `/guide.md`, `/install.ps1`, `/latest.json`). Bilingual: English default, Chinese under `/zh/`; the guide body stays English.

Audience/job: developers deciding what their coding agent may use; the agent itself fetches llms.txt and installs. Primary action: copy a one-line message for the agent; secondary: PowerShell one-liner, zip download.
Proof on hand: real xssh output formats, commands, exit codes, and the desktop manager itself. No testimonials, numbers, logos. Desktop replica uses synthetic hosts (documentation IPs), labelled as sample data.

User decisions (2026-10-04): same design style as the desktop app (xssh-desktop, gpui-kit default light theme); light only; keep all content and sections, add one section showing the desktop app as an HTML replica; terminals and code as light grey wells.

## Direction contract

Replaces v2 (navy icon world). User-pinned world, no roll.

THESIS: The site is the desktop manager's sibling. It speaks the app's own interface language (white ground, hairline frames, near-black primary) instead of the dark terminal-hero developer landing.

OWN-WORLD: white page; #fafafa panels and table heads; #e5e5e5 hairlines; #f5f5f5 wells for terminals and code (Consolas-class mono, 13px); primary button #171717 with white text, 6px radius; outline buttons white with #e5e5e5 border; frames 8px radius; outline status tags (green done, cyan running, amber waiting, red disconnected) exactly as the app draws them; system UI sans (Segoe UI / PingFang / YaHei); title-bar header #f8f8f8 with icon + xssh. No gradients, glows, shadows beyond one soft window shadow on the replica.

STORY: headline + agent message to copy → same job via ssh vs xssh → the desktop manager (replica) → command list → six states → secrets stay with you → install steps → footer.

FIRST VIEWPORT: title-bar header (icon, xssh, version, Docs/Download/GitHub/中文). Left: h1, lede, bordered message box with black Copy, PowerShell line, zip button. Right: a session panel drawn like the app's session detail: name + state tag, meta line, grey well with real xssh lines where status lines render as the app's tags.

FORM: user-pinned (desktop app world), no concept-seed; signature interaction: the replica's sidebar switches between Hosts, Sessions and Audit views.

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
