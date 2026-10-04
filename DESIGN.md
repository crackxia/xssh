---
name: xssh.io
description: The website of xssh, drawn in the desktop manager's own interface language.
colors:
  page: "#ffffff"
  panel: "#fafafa"
  title-bar: "#f8f8f8"
  well: "#f5f5f5"
  select: "#e5e5e5"
  hairline: "#e5e5e5"
  hairline-strong: "#d4d4d4"
  ink: "#0a0a0a"
  ink-secondary: "#404040"
  output-grey: "#525252"
  ink-muted: "#737373"
  primary: "#171717"
  primary-hover: "#262626"
  on-primary: "#fafafa"
  done-line: "#22c55e"
  done-text: "#15803d"
  done-icon: "#16a34a"
  running-line: "#06b6d4"
  running-text: "#0e7490"
  waiting-line: "#eab308"
  waiting-text: "#a16207"
  disconnected-line: "#ef4444"
  disconnected-text: "#b91c1c"
typography:
  display:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "44px"
    fontWeight: 600
    lineHeight: 1.15
    letterSpacing: "-0.02em"
  headline:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "28px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "-0.01em"
  title:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "16px"
    fontWeight: 600
    lineHeight: 1.6
  body:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "15px"
    fontWeight: 400
    lineHeight: 1.6
  lede:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "17px"
    fontWeight: 400
    lineHeight: 1.65
  label:
    fontFamily: "system-ui, Segoe UI, -apple-system, PingFang SC, Microsoft YaHei, Noto Sans SC, sans-serif"
    fontSize: "12px"
    fontWeight: 500
    lineHeight: 1
  mono:
    fontFamily: "Consolas, Cascadia Mono, ui-monospace, Menlo, PingFang SC, Microsoft YaHei, monospace"
    fontSize: "13px"
    fontWeight: 400
    lineHeight: 1.9
rounded:
  tag: "3px"
  control: "6px"
  frame: "8px"
  window: "10px"
  pill: "50%"
spacing:
  gutter: "32px"
  gutter-mobile: "16px"
  container: "1180px"
  section: "104px"
  band: "88px"
components:
  button-primary:
    backgroundColor: "{colors.primary}"
    textColor: "{colors.on-primary}"
    rounded: "{rounded.control}"
    padding: "0 14px"
    height: "36px"
  button-primary-hover:
    backgroundColor: "{colors.primary-hover}"
  button-outline:
    backgroundColor: "{colors.page}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    padding: "0 14px"
    height: "36px"
  button-outline-hover:
    backgroundColor: "{colors.well}"
  button-ghost-copy:
    textColor: "{colors.ink-muted}"
    rounded: "{rounded.control}"
    padding: "0 8px"
    height: "28px"
  tag:
    backgroundColor: "{colors.page}"
    textColor: "{colors.ink-secondary}"
    typography: "{typography.label}"
    rounded: "{rounded.tag}"
    padding: "0 6px"
    height: "20px"
  tag-done:
    backgroundColor: "{colors.page}"
    textColor: "{colors.done-text}"
    rounded: "{rounded.tag}"
  tag-running:
    backgroundColor: "{colors.page}"
    textColor: "{colors.running-text}"
    rounded: "{rounded.tag}"
  tag-waiting:
    backgroundColor: "{colors.page}"
    textColor: "{colors.waiting-text}"
    rounded: "{rounded.tag}"
  tag-disconnected:
    backgroundColor: "{colors.page}"
    textColor: "{colors.disconnected-text}"
    rounded: "{rounded.tag}"
  codeline:
    backgroundColor: "{colors.well}"
    typography: "{typography.mono}"
    rounded: "{rounded.control}"
    padding: "4px 4px 4px 12px"
  frame:
    backgroundColor: "{colors.page}"
    rounded: "{rounded.frame}"
  table-head:
    backgroundColor: "{colors.panel}"
    textColor: "{colors.ink-muted}"
    height: "38px"
  title-bar:
    backgroundColor: "{colors.title-bar}"
    textColor: "{colors.ink}"
    height: "52px"
  nav-item-current:
    backgroundColor: "{colors.select}"
    textColor: "{colors.ink}"
    rounded: "{rounded.control}"
    height: "32px"
---

# Design System: xssh.io

Source of truth: `site/public/assets/site.css` (tokens on `:root`). After any change to `site.css` or `site.js`, bump `ASSET_V` in `site/src/html.ts`; both files are cached for a long time.

## Overview

**Creative North Star: "The App's Sibling"**

The site speaks the interface language of the desktop manager, xssh-desktop. The desktop manager uses gpui-kit 0.7's default light theme (shadcn "neutral"), with radius 6 / radius_lg 8 and Consolas as the Windows mono. The site is that theme on the web. The page is white, with hairline frames, grey panels for table heads and title bars, grey wells for terminals and code, and a near-black primary button. The only colour comes from the four outline status tags, drawn exactly as the app draws them. There are no hero illustrations and no dark terminal-hero landing. The visual proof is the product's own interface, rebuilt in HTML.

The site is light only (`color-scheme: light`, user decision 2026-10-04). Density is that of a calm desktop tool: 15px body, 13px mono, 12px labels and table heads, generous section spacing between dense framed panels. Depth is flat. The interactive replica of the desktop window is the only element with a shadow.

**Key Characteristics:**
- White page, hairline (#e5e5e5) borders, grey panels and wells; never a dark ground.
- Near-black primary button; every other button is white with a hairline border.
- Colour comes only from status: green done, cyan running, amber waiting, red disconnected, always as outline tags.
- System UI sans for everything, Consolas-class mono for commands and output.
- The desktop manager replica is the signature: a real, switchable window.

## Colors

A neutral grey scale with four status hues and no brand accent.

### Primary
- **Near-Black Ink** (primary): the one filled button per view (Copy in the message box, the main download) and the skip link. On hover it lifts to **Graphite** (primary-hover). Text on it is **Off-White** (on-primary).

### Status (the only hues)
- **Done Green**: line (done-line) on the tag border and the replica's connected dot; text (done-text) for the label and the "Copied" state; done-icon for the check marks in the secrets list.
- **Running Cyan**: running-line border, running-text label.
- **Waiting Amber**: waiting-line border, waiting-text label.
- **Disconnected Red**: disconnected-line border, disconnected-text label.

### Neutral
- **White Page** (page): page ground, frames, tag and button fill.
- **Panel Grey** (panel): table heads, terminal captions, the band section, the footer, the docs TOC, the replica sidebar, release headers.
- **Title-Bar Grey** (title-bar): the sticky header and the replica's window bar. Also the `theme-color`.
- **Well Grey** (well): terminals, code lines, inline code, session cards, and button/row hover.
- **Selection Grey** (select): the current nav item, the hover on ghost buttons, and grey tags.
- **Hairline** (hairline): every border and divider. Table row rules use it at 70% opacity.
- **Strong Hairline** (hairline-strong): link underlines, blockquote rule, quiet status lines.
- **Ink / Secondary Ink / Output Grey / Muted Ink**: body text, secondary copy and ledes, command output in wells, and labels/meta/prompts (`$`, `PS>`).

### Named Rules
**The Status-Only Colour Rule.** Hue appears only to show a session state or a success check. Decoration, links, headings and sections stay neutral.

**The One-Step-Darker Rule.** A status tag's border uses the app's colour; its text uses the next darker step of the same hue (green-500 border, green-700 text), so 12px labels pass contrast. Never set tag text in the border colour.

## Typography

**Body Font:** system UI sans (Segoe UI on Windows, -apple-system / PingFang SC on Apple, Microsoft YaHei / Noto Sans SC for Chinese)
**Mono Font:** Consolas (with Cascadia Mono, ui-monospace, Menlo; CJK falls back to PingFang SC / Microsoft YaHei)

**Character:** The fonts the app itself renders with. Nothing is self-hosted, and no web display face is used. Weight 600 on headings is the only emphasis.

### Hierarchy
- **Display** (600, 44px, 1.15, -0.02em; 32px at ≤720px): the home h1 only.
- **Page headline** (600, 32px, 1.2): inner page h1 (Download, Docs, 404 text).
- **Headline** (600, 28px, 1.25; 24px at ≤720px): section h2. Docs prose h2 is 22px with a hairline rule above it.
- **Title** (600, 16–18px): step titles, release headers, session name.
- **Lede** (400, 17px, 1.65, max 58ch): one paragraph under the h1, in secondary ink.
- **Body** (400, 15px, 1.6): running text. Docs prose is 1.7 leading at max 76ch.
- **Label** (500, 12–13px): field labels above inputs, table heads, terminal captions, tags. Normal case, no tracking.
- **Mono** (13px, 1.9 in terminals; 13.5px in code lines): commands, output, hashes, versions. Ligatures are off.

### Named Rules
**The One-Line Subtitle Rule.** Every subtitle or section description is one short sentence that does not wrap at the default desktop width. This is a standing user preference across xssh UIs.

**The Chinese Setting Rule.** On `:lang(zh)`, h1 and h2 use letter-spacing 0 and `word-break: keep-all`. The hero h1 uses 1.25 leading.

## Layout

A 1180px container with 32px gutters (16px at ≤720px). The home page runs: a two-column hero (1.1fr / 1fr, 56px gap), then a full-bleed panel band for the ssh-vs-xssh comparison, then framed sections at 104px top spacing (80px on mobile), then a footer on a panel with a hairline above.

- **Breakpoints:** 1000px stacks the hero, the keys section and docs into one column, makes the docs TOC a static panel, and hides two Hosts columns in the replica. At 720px: mobile gutters, a stacked message box with a full-width Copy, single-column comparison, and command/state tables without heads, as one-column rows. The replica's sidebar becomes a horizontal tab strip with the three interactive views, and its tables drop to their essential columns (Hosts: name + address; Audit: time, command, exit).
- **Touch:** `(pointer: coarse)` raises every control (nav links, buttons, ghost Copy, footer links, TOC links, replica tabs) to at least 44px.
- **Docs:** a 220px sticky TOC panel plus prose at max 880px (76ch lines).
- **Sticky header** is 52px. `scroll-padding-top` is 72px.

### Named Rules
**The Never-Cut-a-Command Rule.** A copyable command line scrolls horizontally on wide screens. On phones it wraps (`pre-wrap` + `break-all`). It is never truncated or clipped mid-token.

## Elevation & Depth

The system is flat. Depth comes from tone: a white page, panels at #fafafa, wells at #f5f5f5, and hairline borders between them. There is exactly one shadow.

### Shadow Vocabulary
- **Window shadow** (`box-shadow: 0 1px 2px rgba(0,0,0,0.04), 0 16px 40px -16px rgba(0,0,0,0.16)`): only on the desktop manager replica, so that it reads as a window sitting on the page.

### Named Rules
**The One-Window Rule.** No gradients, glows or other shadows. Only the replica window casts a shadow, and it is soft.

## Shapes

Small, quiet corners from the app theme: 3px on tags and status lines, 6px on controls (buttons, nav items, code wells, inline panels), 8px on frames (tables, terminals, the session panel, the message box, release cards, the docs TOC), 10px on the replica window. Install-step numbers are 28px circles. Borders are always 1px hairlines. The dashed border is reserved for the alternative-step badge.

## Components

### Buttons
Restrained, and drawn like the app's.
- **Shape:** gentle corners (6px), 36px tall, 14px horizontal padding, 500 weight at 14px.
- **Primary:** near-black fill, off-white text. Use one per view.
- **Outline:** white fill with a hairline border. Hover fills to well grey.
- **Ghost Copy:** 28px, transparent, muted text. Hover gives a selection-grey fill. After copying, it swaps to a check and "Copied" in done-text for a moment, announced through the polite `[data-live]` region.
- **Transitions:** 0.15s on background, colour and border (ease `cubic-bezier(0.16, 1, 0.3, 1)`), none under reduced motion.

### Status Tags
The app's outline tags: 20px tall (18px inside the replica), 3px corners, white fill, a 1px border in the status colour, 12px/500 text one step darker. A neutral tag has a hairline border and secondary ink. A grey tag has a selection-grey fill and no border. Inside terminals, `[xssh …]` status lines render as the same outline tags (`.st`), so the output reads the way the app shows a session state.

### Frames, Tables and Wells
- **Frame:** white, a 1px hairline border, 8px corners, overflow clipped.
- **Table:** a frame with a 38px panel-grey head (12px/500 muted), rows of at least 48px separated by 70% hairlines, and a well-grey hover. Command and state lists are tables, not cards.
- **Terminal:** a frame with a 38px panel-grey caption over a well-grey `pre` (13px mono, 1.9 leading). Prompts are muted and output is output-grey.
- **Code line:** a single well-grey row (6px corners) holding a mono command and a ghost Copy.

### Navigation
The header is the app's title bar: #f8f8f8, a hairline below, sticky, 52px, with the icon + "xssh" + version tag at left. At right: Docs / Download / GitHub, then the language switch as an outlined item. Items are 32px, 6px corners, secondary ink. Hover fills with translucent selection grey. The current page gets a selection-grey fill and 500 weight. At ≤720px, the GitHub item and the version tag are hidden.

### Message Box (hero)
A framed 8px box that holds the agent message (14px mono, wraps anywhere) and the black primary Copy. It stacks with a full-width Copy at ≤720px. The PowerShell code line and the zip download button sit below, each with a small label above.

### Session Panel (hero)
Drawn like the app's session detail: the session name (16px/600) and its state tag, a 12px muted meta line, then an inset well with real xssh lines whose status lines render as tags.

### Desktop Manager Replica (signature)
An HTML rebuild of xssh-desktop. It has a 36px title bar with window controls, a 172px panel-grey sidebar (Hosts / Sessions / Audit buttons, other items static, and a connected-status foot), and a main pane with a title, search and tables. The sidebar buttons switch views (`data-view` / `data-pane`). That is the signature interaction.
- It speaks the page's language, like the bilingual app: English at `/`, Chinese at `/zh/` (`data-lang` on `.app`; English widens the hosts actions column). Its sidebar foot shows the app's language switch, current language highlighted.
- Its data is sample data from documentation IP ranges only (192.0.2.x, 198.51.100.x, 203.0.113.x). It is labelled as an interface preview. Never put real hosts, IPs or names in it.
- At ≤720px the sidebar becomes a tab strip, and tables drop columns (see Layout).

### Install Steps
A numbered list with 28px circular badges (panel fill, hairline). An alternative step is marked with a dashed badge reading "或" / "or", never a number.

### Share Cards
`site/public/og-en.png` and `og-zh.png` (1200×630) are screenshots of `site/og/en.html` and `site/og/zh.html`, served through the site's own stylesheet. Each card has a 72px title bar (icon, xssh, xssh.io), the h1 at 64px, one line of description, a command line, and the status lines as tags. Re-render the cards when the h1 changes.

## Language

English at `/`, Chinese at `/zh/`. A `/zh/` URL is always served (explicit choice). An unprefixed URL entered from outside the site redirects to `/zh/` when the `lang` cookie, or on a first visit the browser's Accept-Language, is Chinese. Pages record the viewed language in the cookie, so the switch works without JS. Bots without Accept-Language get English.

## Do's and Don'ts

### Do:
- **Do** keep the page white and light only. Use panel grey for heads and bands, and well grey for code and terminals.
- **Do** frame content with 1px hairlines and 8px corners. Use tables (grey head, row rules) for lists.
- **Do** draw every status as an outline tag: app-colour border, text one step darker.
- **Do** use the near-black primary for the single main action. Every other button is white with a hairline.
- **Do** keep subtitles to one short sentence that does not wrap at the default width.
- **Do** wrap copyable commands on phones (break-all) rather than cutting them.
- **Do** mark an alternative install step with a dashed "或 / or" badge.
- **Do** raise every control to 44px under `(pointer: coarse)`.
- **Do** bump `ASSET_V` in `site/src/html.ts` after any CSS or JS change.

### Don't:
- **Don't** add a dark mode, dark hero or dark terminal ground.
- **Don't** use gradients, glows, or any shadow other than the replica's soft window shadow.
- **Don't** add hues for decoration. Colour means session state.
- **Don't** set tag text in the border colour, or fill status tags.
- **Don't** self-host or import a display or mono web font. Use the system UI sans and Consolas-class mono.
- **Don't** put real hosts, IPs or user names in the replica or any sample. Use documentation ranges.
- **Don't** mix languages inside the replica. Every label it shows comes from the app's own Chinese or English text.
