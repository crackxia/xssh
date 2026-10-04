# xssh: SSH for agents
A daemon keeps connections and shells alive across calls. Output is ANSI-free; over 16KB
keeps head+tail and saves the full text (path shown). `--json` works anywhere.
xssh options go before `--`; everything after `--` is the remote command.

## Rules
- Do all remote work with xssh, never ssh/scp/sftp/rsync/sshpass/plink (they hang on prompts and
  lose state). Fall back only for what xssh cannot do, and tell the user.
- Start with `xssh host list`: saved aliases + notes. Never ask for an address or password it has.
- Unlisted server: `user@host[:port]`, an IP, or a ~/.ssh/config Host works as-is (key/agent auth).
  To save one with a password: `xssh host add ALIAS --host ADDR --user U --note '...' --ask-password`.
  Never ask for passwords in chat: password prompts go to the terminal or, without one, a desktop
  dialog for the user (blocks; cancel = exit 64).
- Exit 78 (host key changed, unknown under strict, or revoked): never work around it.
  `xssh host trust ALIAS` prints the presented fingerprint (read-only); give it to the user. Once they
  confirmed it with the server's owner: `xssh host trust ALIAS --fingerprint SHA256:...` (the user
  confirms again in the terminal/dialog; declined = exit 64; revoked keys are refused).
- Work over ~90s: `job start` + `job wait --timeout 90s` (tool calls time out, jobs do not).
- `exec` for one-off commands; `session` for state (cd/env/venv), prompts, REPLs, full-screen apps.
- Git Bash rewrites `/path` args to `C:/...`; xssh restores them. Where it cannot, it exits 64
  before running anything: rerun as `MSYS_NO_PATHCONV=1 xssh ...`.

## Hosts
    xssh host list [QUERY] | show H | rm H
    xssh host add H --host ADDR --user U [--ask-password] [--port P] [--key PATH] [--jump J1,user@J2:22] [--proxy-command 'CMD %h %p'] [--tag T] [--note '...'] [--expires 4h|7d|YYYY-MM-DD]
    xssh host edit H [--host ADDR] [--note ..] [--add-tag T] [--encoding gbk] [--legacy-algos] [--compression yes|no] [--host-key-policy tofu|strict] [--expires ..]
    xssh host set-password H [--sudo|--passphrase|--clear]   # prompts the user; login password also serves sudo unless --sudo set one
    xssh host test H                        # connects anew, replacing the pooled connection
    xssh host disconnect H                  # drop the pooled connection; sessions/forwards/jobs keep theirs
    xssh host import                        # from ~/.ssh/config (resolved like ssh)
    xssh host export [H|H1,H2|@tag]         # ssh_config text on stdout; only when the user asks
    xssh host prune [--unused 30d] [--dry-run]   # remove expired (+ unused) hosts and their secrets
    xssh secret set NAME                    # for --env-secret and {secret:NAME}
    xssh key gen K && xssh key deploy H K   # switch H to key auth
`--legacy-algos`: old devices (sha1 kex, CBC). `KEY-cert.pub` next to a key enables certificate auth.
In `host edit`, `''` clears a value; edits drop the pooled connection. `host list` marks `EXPIRED=`
(still connects) and `same_as=ALIAS` (same user@addr:port). Prune: run `--dry-run`, show the list,
remove only after the user agrees.

## exec
    xssh exec H -- 'cmd; cmd'               # H | H1,H2 | @tag | @all (parallel: one `=== H (exit N) ===` block each)
    xssh exec H --sudo -- systemctl restart nginx
    xssh exec H [--cwd DIR] [-e K=V] [--env-secret VAR=SECRET] [--timeout 100s] [--stdin | --stdin-file F] [--pty] -- cmd
Runs as `$SHELL -c` (not a login shell) in POSIX syntax; fish/csh hosts get bash or sh. Exit codes:
- remote exit code, or 124 = timeout (process group killed; the hint says if verified)
- 126 = sudo failed (reason given); 128+N = killed by signal N
- several hosts: the exit code they share, else 1
- a missing --cwd fails (exit 1, message) instead of running elsewhere
- `-e` values are visible in the remote `ps`; pass secrets with `--env-secret`
- `error[REMOTE] ... may have run`: the connection dropped after sending. Not retried; check the
  result before rerunning.

## session
    xssh session open H --name N [--persist] [--respond 'REGEX=>TEXT']
    xssh session run N -- 'cmd'             # output + exit code; cwd/env persist
    xssh session run N --no-wait -- cmd     # then: xssh session wait N --timeout 90s (exit = its code)
    xssh session send N 'text' --enter      # answer prompts, drive REPLs/TUIs; --paste for multi-line text
    xssh session send N --keys ctrl-c       # enter tab shift-tab esc up down left right home end pgup pgdn
                                            # f1-f12 ctrl-X alt-X ctrl-left backspace; down*3 = 3x down;
                                            # comma, star = literal , *
    xssh session read N [--timeout 30s]     # new output since your last call
    xssh session expect N 'REGEX' --timeout 60s
    xssh session screen N | list [H] | log N [--tail 200] | close N
Each result ends with `[xssh N STATE exit=E prompt=KIND]`:
- STATE:
  - `done`: finished.
  - `waiting prompt=password|confirm|input|repl|pager|shell`: answer with `send`.
  - `running` / `quiet`: still working; `read` or `wait` again.
  - `tui`: a full-screen app.
  - `disconnected`: the connection dropped (exit 69).
- Exit code: the command's own once `done`; 0 at a `repl` prompt or in an alternate-screen app
  (vim, top); otherwise 124 = unfinished (running, or at a password/confirm/input/pager/shell prompt)
  or a wait timed out.
- `run` needs a shell prompt, else exit 75 with the reason. At REPLs and questions use `send`.
- Nested shells (`sudo -i`, `su -`, `ssh h`, `docker exec -it c bash`): `run` returns 124
  prompt=shell; later `run`s execute inside; `run N -- exit` returns to the outer shell.
- sudo prompts are auto-filled. Other prompts: `--respond`, where TEXT may use {password},
  {sudo_password}, {secret:NAME}.
- `--persist`: the shell lives in remote tmux, survives disconnects and daemon restarts, and is
  reattached automatically. Without it a session ends with its connection or the daemon.
- Sessions are shared by all local agents; each agent has its own read position (`--as NAME` or
  XSSH_AGENT when several share one process). Only drive sessions you opened; exit 75
  `busy: agent 'X' ...` means another agent holds it.
- Lost context: `session list`, `session log N`, `xssh audit --tail 30`.

## Full-screen apps (vim, top, Claude Code, codex)
Output = lines scrolled off since your last call + the current screen (or `screen unchanged`).
`«x»` marks the highlighted item; `[× N identical rows]` stands for repeats. Spinners and counters
are ignored; "esc to interrupt"-style markers mean busy.
    xssh session run N -- claude            # returns once the TUI is up
    xssh session send N 'task' --enter --timeout 10m   # returns when stable and not busy
    xssh session send N --keys down,enter
    xssh session wait N --until REGEX | --gone REGEX | --stable 3s [--timeout 5m]
Wait flags also work on `send`/`read`; on timeout: exit 124 plus the screen. Quit the app before the
next `run` (/exit, q, :q). After ctrl-c check the screen; some apps need a second ctrl-c. The footer
shows `done` once the shell is back.

## job (survives disconnects and daemon restarts)
    xssh job start H [-q] [--sudo] [--cwd D] [--name N] [-e K=V] [--env-secret VAR=SECRET] -- cmd   # one host; -q: id only
    xssh job wait ID --timeout 90s          # exit = the job's; 124 = still running
    xssh job logs ID [--offset N]           # N = next_offset from the previous call (first call: omit)
    xssh job status|kill|rm ID ; xssh job list [--host H] [--all] ; xssh job prune [--older-than 7d]
Logs are in ~/.xssh/jobs/ on the host. Job `-e`/`--env-secret` values never appear in ps.
`state=lost`: the job's pid was reused; xssh never signals it.

## Files
    xssh file read H PATH [--offset LINE --limit N] [--sudo]   # numbered lines; any file size
    xssh file edit H PATH --old 'exact text' --new 'text' [--old-file F] [--new-file F] [--replace-all] [--sudo]
    xssh file write H PATH --stdin | --content S | --from-file F [--mode 600] [--mkdirs] [--expect-sha256 SHA] [--sudo]
    xssh file ls H DIR ; xssh file stat H PATH
- `read` adds `[xssh eol=crlf|mixed no final newline encoding=gbk]` only when relevant; `--json` includes sha256.
- `edit` needs a unique match. An LF `--old` also matches CRLF files.
- `edit`, and `write --expect-sha256`, stop with exit 75 if the file changed meanwhile.
- Writes are atomic, follow symlinks and keep owner/mode. The old content is backed up to the remote
  ~/.xssh/backups (root's with --sudo; 0700; 5 per file, 30 days; path printed; restore:
  `exec H [--sudo] -- cp -p BACKUP PATH`). `--no-backup` when the old content holds secrets.
- Secrets: write `{secret:NAME}`, `{password}` or `{sudo_password}` in the text and add `--secrets`;
  xssh inserts the stored value, output shows `***`. Long text: `--old-file F`/`--new-file F` (edit),
  `--stdin`/`--from-file F` (write); F = local file, used verbatim.
- Max 64 MB per write (use `cp` for larger). A missing parent dir gives exit 66 (use `--mkdirs`).

## cp (rsync-like)
    xssh cp SRC... DST                      # HOST:PATH = saved host, else local; dirs recurse
    xssh cp ./app H:/srv/app/ --exclude node_modules/ --exclude '*.log' [--delete]
    xssh cp H1:/srv/db.dump H2:/tmp/        # host->host (relayed; on the same host it copies there)
    xssh cp a.conf H:/etc/app/ --sudo [--mode 644]
- Skips files with the same size+mtime; `-c` compares sha256 instead, `--force` copies all.
- Each file is written atomically. mtimes and modes are kept; large files resume.
- Other flags: `-n` dry run, `-l` keep symlinks, `--verify` re-hashes the destination, `--delete`
  (not with --sudo).
- `--exclude` patterns: `*.log`, `dir/` (directories only), `/x` (anchored at SRC), `**/x`.
- DST ending in `/`, an existing dir, or several SRCs: copy into it. `SRC/.` copies only the contents.
- Output:
  - summary: `copied N file(s), SIZE, K unchanged, D deleted, R resumed`
  - `sha256` for one file, or `tree sha256` for a directory
    (= `find . -type f | LC_ALL=C sort | xargs -d '\n' sha256sum | sha256sum`, run in DIR)
  - changes: `+` new, `~` replaced, `-` deleted (first 50); `[skipped] PATH (reason)`

## Health & logs
    xssh status H [--only cpu,mem,disk,net,proc,ports,services,docker] [--sudo]
    xssh diag H [--sudo]                    # findings + suggested commands
    xssh perf H --duration 30s --interval 2s   # blocks for the whole duration; longer: job start H -- vmstat 5 60
    xssh logs H [--unit U] [--since 15m] [--priority err] [--grep ERE] [--tail N]   # grep is case-insensitive; --grep alone searches 24h
    xssh logs H --file PATH [--grep ERE] [--tail N]
Without --sudo, `[note]` lines say what could not be seen.

## Forwards & misc
    xssh forward add H -L [bind:]PORT:HOST:PORT   # this machine -> HOST as seen from H
    xssh forward add H -R [bind:]PORT:HOST:PORT   # H's port -> HOST as seen from here (re-registered after drops)
    xssh forward add H -D PORT                    # SOCKS5 proxy here, traffic leaves from H
    xssh forward list | stop ID
    xssh audit --tail 20 ; xssh daemon status|stop|restart
    xssh path [add|rm]                      # which xssh new terminals run; add|rm edit the user PATH (ask first)
- `daemon stop|restart`: exit 75 while any non-`--persist` session or forward is open (they close for
  all agents); `--force` only when the user agrees. Never restart the daemon to fix one host.
- Text is UTF-8. GBK output is detected; `host edit --encoding gbk` also encodes your input.
- On Windows, `XSSH_OUTPUT_ENCODING=utf-8` forces UTF-8 on pipes.

## Exit codes
- 0 ok; otherwise the remote/job exit code is passed through, or:
- 64 usage; 66 not found.
- 69 connect failed or disconnected. The hint gives the cause and the last success (time, address).
  Run `host test H` once; if it still fails, ask the user (address changed? firewall?), then
  `host edit H --host NEW` if needed. No retry loops. A `disconnected` session: open a new one.
- 75 busy: session in use, `run` not at a prompt, file changed, or host refuses more channels.
- 77 auth (OTP/2FA prompts are not answered; use keys; the hint lists untried local keys);
  78 host key changed or revoked.
- 124 timeout / unfinished; 125 other; 126 sudo failed; 128+N killed by signal N.
