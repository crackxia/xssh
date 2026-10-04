import { GUIDE_MD } from "./pages/docs";
import { latest, type Manifest } from "./releases";

const ORIGIN = "https://xssh.io";

export function latestJson(m: Manifest) {
	const r = latest(m);
	return {
		version: r.version,
		date: r.date,
		files: r.files.map((f) => ({ ...f, url: `${ORIGIN}/dl/${r.version}/${f.target}` })),
		notes: r.notes.en,
	};
}

/** https://llmstxt.org: what an agent needs to install and start using xssh. */
export function llmsTxt(m: Manifest): string {
	const r = latest(m);
	return `# xssh

> SSH client for AI agents. Saved hosts with notes, shells that persist across tool calls, prompt and full-screen app handling, long jobs, atomic file edits, rsync-like copy, port forwards, stable exit codes. Use it instead of ssh/scp/sftp/rsync for all remote server work. Passwords stay with the human.

Latest: ${r.version} (${r.date}), Windows x86_64. Remote hosts: Linux, macOS, FreeBSD (POSIX shell).

## Install (Windows)

1. \`xssh --version\` prints ${r.version} or newer: go to step 3.
2. PowerShell: \`irm ${ORIGIN}/install.ps1 | iex\`
   From bash: \`powershell -NoProfile -Command "irm ${ORIGIN}/install.ps1 | iex"\`
   It checks the SHA-256, installs to %LOCALAPPDATA%\\Programs\\xssh (an existing data\\ folder is kept) and adds that folder to the user PATH. Shells already open keep the old PATH: until your next session call \`$env:LOCALAPPDATA\\Programs\\xssh\\xssh.exe\` (bash: \`"$LOCALAPPDATA/Programs/xssh/xssh.exe"\`).
   Manual: download ${ORIGIN}/dl/latest/windows-x86_64, compare its SHA-256 with ${ORIGIN}/latest.json, unzip anywhere, run \`xssh path add\`.
3. Install your skill: \`xssh guide --install-skill --agent ID\`, ID = claude-code | codex | gemini-cli | github-copilot | cursor | opencode | windsurf | amp | qwen-code | kiro | trae | roo | agents. It loads in your next session; now read \`xssh guide\`.
4. Follow \`xssh guide\`. Start with \`xssh host list\`.
5. Tell the user how to add a password server: \`xssh host add ALIAS --host ADDR --user USER --ask-password\` (they type the password in a terminal or dialog). Never ask for passwords in chat.

## Docs

- [Agent manual](${ORIGIN}/guide.md): commands, result formats, exit codes (= \`xssh guide\`)
- [Full text](${ORIGIN}/llms-full.txt): this file plus the manual
- [Release manifest](${ORIGIN}/latest.json): version, file, size, sha256, url
- [Source](https://github.com/crackxia/xssh): MIT
`;
}

export function llmsFull(m: Manifest): string {
	return `${llmsTxt(m)}\n---\n\n${GUIDE_MD}`;
}

export const INSTALL_PS1 = `# xssh installer (${ORIGIN}/install.ps1)
# Installs the latest xssh for Windows x64 into %LOCALAPPDATA%\\Programs\\xssh,
# keeps an existing data\\ folder, verifies the SHA-256, and adds the folder to the user PATH.
# Usage: irm ${ORIGIN}/install.ps1 | iex
& {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
  if ($env:PROCESSOR_ARCHITECTURE -ne 'AMD64' -and $env:PROCESSOR_ARCHITEW6432 -ne 'AMD64') {
    Write-Warning "xssh: this is a Windows x64 build; on $($env:PROCESSOR_ARCHITECTURE) it runs under emulation if available."
  }
  $m = Invoke-RestMethod '${ORIGIN}/latest.json'
  $f = $m.files | Where-Object { $_.target -eq 'windows-x86_64' } | Select-Object -First 1
  if (-not $f) { throw 'xssh: no Windows x64 build in latest.json' }
  $dir = Join-Path $env:LOCALAPPDATA 'Programs\\xssh'
  $tmp = Join-Path ([IO.Path]::GetTempPath()) ('xssh-' + [Guid]::NewGuid().ToString('N'))
  New-Item -ItemType Directory -Force $tmp | Out-Null
  try {
    $zip = Join-Path $tmp $f.name
    Write-Host "xssh $($m.version): downloading $($f.name)"
    Invoke-WebRequest $f.url -OutFile $zip -UseBasicParsing
    $hash = (Get-FileHash $zip -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($hash -ne $f.sha256) { throw "xssh: SHA-256 mismatch: got $hash, expected $($f.sha256)" }
    Expand-Archive $zip -DestinationPath $tmp -Force
    $src = Get-ChildItem $tmp -Directory | Where-Object { Test-Path (Join-Path $_.FullName 'xssh.exe') } | Select-Object -First 1
    if (-not $src) { throw 'xssh: xssh.exe not found in the archive' }
    New-Item -ItemType Directory -Force $dir | Out-Null
    try {
      Copy-Item (Join-Path $src.FullName '*') $dir -Recurse -Force
    } catch {
      throw "xssh: cannot replace files in $dir (close xssh-desktop and retry): $($_.Exception.Message)"
    }
  } finally {
    Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
  }
  $exe = Join-Path $dir 'xssh.exe'
  & $exe path add | Out-Host
  & $exe --version
  Write-Host "Installed to $dir"
  Write-Host "Next: xssh guide --install-skill --agent claude-code   (or another agent id, or all)"
  Write-Host "Open a new terminal for PATH; until then use: $exe"
}
`;
