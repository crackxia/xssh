---
version: 2
slug: "site"
primary_target: "site"
related_targets: []
---

# Surface: xssh.io website

Scope: landing (`/`, `/zh/`, Persuade), docs (`/docs`, Read: renders docs/guide.md), download (`/download`, releases, sha256, counts), agent entry points (`/llms.txt`, `/guide.md`, `/install.ps1`, `/latest.json`). Bilingual: English default, Chinese under `/zh/`; the guide body stays English.

Audience/job: developers deciding what their coding agent may use; the agent itself fetches llms.txt and installs. Primary action: copy a one-line message for the agent; secondary: PowerShell one-liner, zip download.
Proof on hand: real xssh output formats, commands, exit codes. No testimonials, numbers, logos.

## Direction contract

Replaces v1 ("The Telegram", rejected by the user as too idiosyncratic). User brief: simple, clean, derived from the app icon.

THESIS: The icon is the system. Deep navy tile, two weaving strokes (cyan→blue, lavender→indigo), one lit teal cursor. The site is quiet navy space where only commands and state lines are lit.

OWN-WORLD: flat navy ground #090E22 (the tile); text cool white; secondary text tinted from the navy (not grey). Accents taken only from the icon: cyan #67E8F9 (links), lavender #C4B5FD (prompts/waiting), teal #2DD4BF (the cursor: primary action, done state, the one blinking element). Corners follow the tile's soft squircle radius. Type: Schibsted Grotesk (UI/display), Commit Mono (code only). No glows, gradients or background blobs. No illustrations beyond the icon itself.

STORY: headline + agent message to copy → same job via ssh vs xssh (real status lines) → command list → six states → secrets stay with you → install steps → footer.

FIRST VIEWPORT: header (icon, xssh, Docs/Download/GitHub/中文). Left: headline ending in a teal block cursor, lede, copy box with the agent message + teal Copy button, then PowerShell one-liner and zip link. Right: a terminal panel with a real xssh session.

SIGNATURE: the teal cursor from the icon: blinks once after the headline (honours reduced motion), reused as the copy confirmation.

FINISH: one batched desktop+mobile inspection, one fix batch, DESIGN.md recorded.
