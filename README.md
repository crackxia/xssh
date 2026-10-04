<img src="assets/icon/xssh.png" width="96" alt="xssh">

# xssh

English · [简体中文](README.zh-CN.md) · [xssh.io](https://xssh.io)

An SSH client built for AI agents (Claude Code, Codex, Gemini CLI, Cursor...). One Rust binary.

Plain `ssh` is made for a human at a terminal. An agent calling it once per tool call hangs on
prompts, loses `cd`/env between calls, cannot drive full-screen programs and has to be handed
passwords. xssh keeps connections and shells alive in a background daemon, reports every state
in a fixed machine-readable form, answers sudo prompts from the OS keyring, and keeps secrets out
of the agent's context.

```
$ xssh session run w -- 'cd /srv/app && git pull'
Already up to date.
[xssh w done exit=0]
```

## Features

- **Hosts and credentials.** Hosts live in `hosts.toml` (no secrets) with notes, tags and learned
  OS facts; passwords and passphrases live in the OS keyring (Windows Credential Manager, macOS
  Keychain, Linux Secret Service), else an encrypted file. Secrets never appear in output, logs,
  the audit trail or transcripts. Humans enter them in a terminal prompt or a desktop dialog.
- **State across calls.** The first call starts a daemon that holds the connection pool and PTY
  sessions. `session open --persist` keeps the shell in remote tmux, so it survives network drops
  and daemon restarts with cwd, env and running programs intact. Dead pooled connections are
  replaced transparently.
- **Interactive automation.** `session run` returns exact output and exit code while keeping
  shell state; completion is detected by invisible shell-hook markers, independent of the prompt
  style. Prompts are classified (password / confirm / input / repl / pager / shell); sudo, sudo-rs
  and doas passwords are filled in automatically; nested shells (`sudo -i`, `su -`, `ssh`,
  `docker exec -it`) keep working.
- **Full-screen programs and remote agents.** Alternate-screen apps, redrawing apps (top) and
  inline TUIs (a remote Claude Code) are recognised; reads return the lines scrolled off plus the
  current screen, with the selected item marked and busy/stable detection.
- **Long jobs.** `job start/wait/logs/kill` run detached on the host (setsid + nohup), unaffected
  by agent tool timeouts; `--env-secret` values never reach the command line or `ps`.
- **Files.** `file read/write/edit` with sudo, atomic writes, backups with retention, sha256
  guards against overwriting someone else's change, CRLF/GBK files, `{secret:NAME}` placeholders.
  `xssh cp` copies local↔host and host↔host rsync-style: skips unchanged files, resumes large
  ones, keeps mtimes and modes, `--exclude/--delete/--dry-run/--verify/--sudo`.
- **Diagnostics.** Connect failures are classified (nothing listening, timeout, firewall, not SSH,
  a local proxy/TUN intercepting TCP) and carry the host's last successful time and address. Auth
  failures list untried local keys. `status` / `perf` / `diag` / `logs` for Linux, macOS, FreeBSD.
- **OpenSSH compatible.** Reads `~/.ssh/config` the OpenSSH way (first match wins, Include,
  Match, ProxyJump chains, ProxyCommand, certificates, HostKeyAlias); `host import` / `host
  export`; legacy algorithms per host for old devices.
- **Safety.** Host keys: trust on first use, hard failure on change, `@revoked` support; trusting
  a changed key needs a human. Keyboard-interactive answers only password prompts, never OTP.
  On Windows the data folder and the daemon's named pipe are owner-only.
- **Also.** Port forwards (`-L`, `-R` with reconnect, `-D` SOCKS5), parallel exec on many hosts,
  key generation and deployment, audit log, `--json` everywhere, stable exit codes.
- **Desktop manager** `xssh-desktop` ([gpui-kit](https://github.com/longbridge/gpui-kit)): hosts and
  credentials, live view of agent sessions, forwards, jobs, audit log, daemon control, PATH and
  agent-skill installation. It is only a client of the daemon: closing it never affects agents.

## Install

Windows x86_64: download the zip from [xssh.io](https://xssh.io) and unzip it anywhere. The folder
is the installation; data is kept in `data/` beside the executable.

```
xssh path add                    # put this folder on the user PATH (new terminals)
xssh guide --install-skill       # install the agent skill for Claude Code
xssh guide --install-skill --agent all   # every agent CLI found on this machine
```

Supported agent CLIs: Claude Code, Codex, Gemini CLI, GitHub Copilot, Cursor, OpenCode, Windsurf,
Amp, Qwen Code, Kiro, Trae, Roo Code, and `~/.agents` (Cline and others).

Remote hosts: any Unix-like system with a POSIX shell (Linux, macOS, FreeBSD).

## Quick start

```
xssh host add web1 --host 10.0.0.5 --user deploy --ask-password   # the human types the password
xssh exec web1 -- uptime
xssh session open web1 --name w
xssh session run w -- 'cd /srv/app && git pull'
xssh status web1
xssh guide                       # the full manual, written for agents
```

[`docs/guide.md`](docs/guide.md) is the agent manual (the same text as `xssh guide` and the
installed skill).

## Build from source

```
cargo build                   # CLI stack (default members; no GPUI)
cargo desktop                 # build and run the desktop app
cargo test
cargo build --profile dist    # release build: thin LTO, codegen-units=1
```

Rust 2024 edition. On Windows, russh uses the `ring` backend (aws-lc-rs would need NASM) and
linking uses `rust-lld` (`.cargo/config.toml`).

| Crate | Role |
|---|---|
| `crates/core` | errors, data-dir layout, daemon protocol, local IPC, text utilities |
| `crates/store` | hosts, config, secret store, audit log, job records |
| `crates/engine` | SSH connections, sessions, exec, transfer, jobs, probes, forwards, daemon |
| `crates/cli` | the `xssh` binary (also hosts the daemon) |
| `crates/desktop` | `xssh-desktop` (GPUI); depends only on core and store |

`tests/testbed/Dockerfile` is a disposable sshd container for end-to-end tests.

## Data folder

Default: `data/` next to the executable (portable); override with `--home DIR` or `XSSH_HOME`
(required when the program folder is not writable). Each installation gets its own keyring
namespace from `data/instance.id`.

| File | Contents |
|---|---|
| `config.toml` | optional settings (output limits, timeouts, host key policy, auto-responses), see `xssh config` |
| `hosts.toml` | host inventory |
| `known_hosts` | xssh's host keys (`~/.ssh/known_hosts` is also consulted) |
| `audit.jsonl` | audit log (`xssh audit`) |
| `sessions/` | redacted session transcripts |
| `outputs/` | full text of truncated outputs |

## Limitations

- "Secrets invisible to the agent" means xssh never reveals them. An agent running as the same OS
  user could read the keyring directly; this is not a hard security boundary. Redaction matches
  the literal secret only.
- `session run` and exec wrappers need a POSIX-style shell (bash/zsh/sh/dash/ksh) on the host;
  fish and csh login shells are handled by switching to bash or sh.
- Published builds: Windows x86_64 only for now.
- macOS/FreeBSD probe parsing is written from sample output and not yet verified on real hosts.

## License

[MIT](LICENSE)
