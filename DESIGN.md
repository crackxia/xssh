# Design: xssh.io

Source of truth: `site/public/assets/site.css` (tokens on `:root`). Derived from the app icon `assets/icon/xssh.svg`: navy tile, two weaving strokes (cyan→blue, lavender→indigo), one lit teal cursor (`x_`).

## Principles
- Quiet navy space; only commands, status lines and the primary action are lit.
- Colours come from the icon only. No illustration besides the icon.
- Dark only (`color-scheme: dark`): matches the icon and the terminal context.

## Colour
| Token | Value | Use |
|---|---|---|
| `--bg` | #090e22 | page |
| `--bg-2` | #0c1330 | alternating band |
| `--panel` / `--panel-2` | #111a3c / #172150 | release card, ask box / buttons |
| `--well` | #060a1a | terminals, code lines |
| `--line` / `--line-2` | indigo 12% / 22% | hairlines, borders |
| `--fg` / `--fg-2` / `--fg-3` | #e8ecf9 / #a7b1d4 / #8590b8 | text, secondary (navy-tinted, ≥4.5:1), labels |
| `--cyan` | #67e8f9 | links, running |
| `--lav` | #c4b5fd | waiting/prompt |
| `--sky` | #93c5fd | tui |
| `--teal` / `--teal-hi` | #2dd4bf / #5eead4 | cursor, primary button, done |
| `--on-teal` | #04211d | text on teal |

## Type
- Schibsted Grotesk (variable, self-hosted latin + latin-ext) for UI and display; CJK falls back to PingFang SC / Microsoft YaHei.
- Commit Mono 400/600 (subset: ASCII, Latin-1, punctuation, arrows, box drawing) for code, commands, status lines, versions only.
- h1 clamp(2.5rem, 5vw, 3.9rem), weight 700, tracking -0.03em; h2 clamp(1.75rem, 3vw, 2.35rem). Chinese: tracking 0, `word-break: keep-all`, looser leading.

## Shape & depth
- Radius 14px (`--r`), 10px (`--r-s`), 18px release card: echoes the icon squircle.
- Shadows only on the hero terminal and ask box (offset, soft blur).

## Components
- `.cursor`: solid teal bar after the h1 (and 404), blinks 5× then stays; none under reduced motion. No glow, no gradients, flat navy ground.
- `.ask-box`: agent message + teal Copy (primary action).
- `.codeline`: one-line command in a well + ghost Copy.
- `.term`: terminal panel; `$` teal, output `--fg-3`, `[xssh …]` lines coloured by state.
- `.btn` / `.btn.primary`, `.copy` (swaps to check + "Copied" for 1.6s).
- Lists use hairline rows (`.cmds`, `.states`), not cards. Command names: `xssh` in `--fg-3`, subcommand in `--fg`.
- Touch (`pointer: coarse`): every control ≥44px tall.
- Copy feedback is announced through a polite live region (`[data-live]`).

## Layout
- Container 1180px, gutter 32px (16px ≤720px). Breakpoints 1000px (stack hero/keys/docs), 720px (mobile).
- Docs: 220px sticky TOC + prose ≤76ch; TOC becomes a panel above content on narrow screens.

## Language
- English at `/`, Chinese at `/zh/`. A `/zh/` URL is always served (explicit choice). An unprefixed URL entered from outside the site redirects to `/zh/` when the `lang` cookie, or on a first visit the browser's Accept-Language, is Chinese. Pages record the viewed language in the cookie, so the switch works without JS. Bots without Accept-Language get English.

## Share cards
- `site/public/og-en.png`, `og-zh.png` (1200×630): rendered by headless Chrome from an HTML card using the site fonts and `icon.svg` (navy ground, h1 + teal cursor, status line, teal base rule). Re-render when the h1 changes.
