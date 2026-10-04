# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Stack

Website (`site/`): delegated. Cloudflare Workers via the `cf` CLI (Vite + Cloudflare plugin, `cloudflare.config.ts`), static pages built by Vite, R2 for release archives, D1 for download counts, KV for the latest-release manifest, Cloudflare Web Analytics. Domain xssh.io (Cloudflare Registrar, zone in the cf CLI account).

## Users

1. AI coding agents (Claude Code, Codex, Gemini CLI, Cursor...) that must operate remote servers for a user. They read `xssh guide` / the installed skill, not web pages; machine-readable docs (llms.txt, raw guide) serve them.
2. Developers who run those agents and decide what the agent is allowed to use. They evaluate xssh, download it, install the skill, and store server passwords themselves.

## Product Purpose

xssh is an SSH client built for AI agents: one Rust binary (`xssh`) plus a desktop manager (`xssh-desktop`). Agents get saved hosts with notes, persistent shells across tool calls, prompt detection, full-screen app driving (including a remote Claude Code), long-running jobs, atomic file edits, rsync-like copy, port forwards, health/log queries, and stable exit codes with next-step hints. Success: an agent completes real server work without hanging on prompts, losing state, or seeing secrets.

## Positioning

Plain ssh is built for a human at a terminal; an agent calling it per tool call hangs on prompts, loses cwd/env, cannot drive TUIs, and must be handed passwords. xssh keeps connections and sessions in a daemon, reports structured state (`[xssh N STATE exit=E prompt=KIND]`), answers sudo prompts from the OS keyring, and keeps secrets out of the agent's context. Output and docs are written for agents (terse, exact, ANSI-free), not humans.

## Operating Context

- Agent tool calls time out (~2 min) and keep no shell state between calls; jobs and persistent sessions exist for that.
- Several agents share one daemon and its sessions.
- Humans enter passwords and confirm host keys (terminal prompt or desktop dialog); agents never see them.
- Remote targets: Linux, macOS, FreeBSD (POSIX shell). Local OS for the current release: Windows x86_64 only.
- Install = unzip; data lives in `data/` next to the exe (portable). `xssh path add` puts it on PATH; `xssh guide --install-skill [--agent ...]` installs the skill for 13 agent CLIs.

## Capabilities and Constraints

- Current release: 0.2.0, Windows x86_64 zip (`xssh.exe`, `xssh-desktop.exe`, README). No macOS/Linux builds published yet.
- License: MIT (open source). Public repository: https://github.com/crackxia/xssh
- Free. No pricing, accounts, or telemetry in the product.
- Agent manual: `docs/guide.md` (= `xssh guide` = skill body).

## Brand Commitments

- Name: xssh (lowercase). Icon: `assets/icon/xssh.svg` (PNG/ICO renders beside it).
- Voice: precise, terse, factual; written for agents first. No hype, no invented numbers.
- Languages: English (default) and Simplified Chinese.

## Evidence on Hand

- Real CLI output formats, exit codes and commands in `docs/guide.md` and README.md.
- No testimonials, customer logos, benchmarks, download numbers, or press. Do not fabricate any.

## Product Principles

1. The agent is the user of the CLI; every word of output and docs earns its tokens.
2. Secrets stay with the human; agents get capabilities, not credentials.
3. Never hang, never guess: every state and failure is reported with an exact next step.
4. Safety over convenience: host key changes are never bypassed automatically.
