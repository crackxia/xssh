<img src="assets/icon/xssh.png" width="96" alt="xssh">

# xssh

[English](README.md) · 简体中文 · [xssh.io](https://xssh.io)

专门给 AI agent（Claude Code 等）使用的 SSH 客户端，Rust 实现，单一二进制。

- **主机与凭据管理**：主机信息存 `hosts.toml`（不含任何秘密），密码/passphrase 存 OS keyring（Windows 凭据管理器 / macOS Keychain / Linux Secret Service），无 keyring 时回退到加密文件。凭据永远不会出现在输出、日志、审计或转录中。临时主机 `--expires` + `host prune`；`host export` 输出 ssh_config；同一 user@地址:端口 重复保存时提示（`same_as=`）。
- **跨调用保持连接与会话**：首次调用自动拉起后台 daemon（空闲自动退出），持有连接池和 PTY 会话；`session open --persist` 把 shell 托管在远端 tmux 中，断网和 daemon 重启后自动重连，cwd/环境/运行中的程序都保留。升级 xssh 时正在使用的 daemon 不会被强制重启。池中连接失效时自动换新连接（通道未打开、命令未发出才重试）；`host test` 总是新建连接，`host disconnect` 只丢弃一个主机的池连接；`daemon stop/restart` 在有其他会话/转发时拒绝（`--force` 强制）。
- **交互自动化**：`session run` 精确返回输出与退出码并保持 shell 状态；命令结束靠 shell 钩子注入的不可见标记判断，与提示符样式无关（starship、busybox、自定义 PS1 都可靠）；fish/csh 登录 shell 自动改用 bash；`sudo -i`/`su -`/`ssh`/`docker exec` 嵌套 shell 可继续 `run`；提示符识别（password / confirm / input / repl / pager / shell）；sudo / sudo-rs / doas 密码自动代填，失败时给出具体原因；自定义应答规则。
- **全屏程序与远程 agent**：自动识别 alt-screen、持续重绘（top）和 Ink 类行内 TUI（如远程运行的 Claude Code）；返回"滚出屏幕的新行 + 当前屏幕"，屏幕未变只回一行；选中项用 `«…»` 标出；忽略 spinner/计时器判断"稳定"，识别 "esc to interrupt" 等忙碌标志；`--until/--gone/--stable` 等待条件；按键逐个发送、文本与回车分开发送、`--paste` 括号粘贴。
- **编码**：远端非 UTF-8（GBK）自动识别；非 UTF-8 locale 的主机会话自动切到 UTF-8 locale；Windows 下管道输出跟随控制台代码页，`--json` 转义非 ASCII，中文不乱码。
- **长任务**：`job start/wait/logs/kill/prune`，远端 setsid+nohup 执行，不受 agent 工具超时限制；记录进程启动时间，PID 被复用时绝不误杀；`--env-secret` 传入的秘密不出现在命令行和 `ps` 中。
- **文件**：`file read/write/edit`（与 Claude Code Read/Edit 语义一致，支持 sudo、原子写入、自动备份与保留策略、改动前 sha256 校验防止覆盖别人的修改、CRLF/GBK 文件、任意大小文件按行切片读取）；`xssh cp` 统一传输：本地↔主机、主机↔主机（同主机在远端直接复制），rsync 式跳过未变文件、临时文件+改名原子写入、大文件断点续传、保留 mtime 与目录权限、`--exclude/--delete/--dry-run/--links/--verify`、`--sudo` 读写 root 路径，返回变更清单和 sha256。`file write/edit --secrets` 把 `{secret:NAME}` 替换为已存秘密，输出显示 `***`。
- **多会话**：任意数量的命名会话并行（各自保持 cwd/env/程序），多个 agent 共享同一 daemon；服务器限制单连接通道数（sshd `MaxSessions`）时自动追加连接。
- **状态与性能**：`status` / `perf` / `diag` / `logs`，支持 Linux、macOS、FreeBSD，输出结构化且精简，附带诊断发现与建议命令。
- **SSH 兼容**：按 OpenSSH 规则解析 `~/.ssh/config`（首个匹配生效、Include、Match、ProxyJump 链、ProxyCommand、证书认证、HostKeyAlias 等），未保存的 `user@host` 可直接使用；老设备可按主机开启 sha1/CBC 等旧算法；可选压缩；跳板机环路检测。
- **连接诊断**：区分端口无监听、超时、防火墙拦截、非 SSH 端口、本地代理/TUN 接管（向不可路由的 192.0.2.1 建连成功即判定）；失败提示附上次成功的时间与地址（地址已变 = 记录过时）；认证失败列出本机未尝试的私钥。
- **安全**：host key TOFU + 同类型密钥变更硬失败、支持 `@revoked`，`host trust` 需用户核对指纹；无终端时（如 Claude Code 的 `!`）密码录入与指纹确认改用系统对话框（Windows CredUI/MessageBox，macOS osascript，Linux zenity）；keyboard-interactive 只用密码回答密码提示（不会误答 OTP）；Windows 下数据目录与命名管道仅本人可访问，客户端校验管道属主；审计日志不记录密码提示下输入的内容。
- **其他**：端口转发（`-L`/`-R` 自动重连/`-D` SOCKS5）、多主机并发执行、密钥生成与部署、审计日志、`--json` 输出、稳定退出码。
- **桌面管理程序** `xssh-desktop`（[gpui-kit](https://github.com/longbridge/gpui-kit)）：给人用的图形界面，见下文。

## 桌面管理程序

| 页面 | 功能 |
|---|---|
| 主机 | 列表与搜索；添加/编辑/删除（改名时凭据随之迁移）；保存或删除登录密码、sudo 密码、私钥口令（存入系统凭据管理器）；连接测试；主机密钥变化时由人核实后确认信任 |
| 会话 | agent 打开的所有会话：状态、空闲时长、正在运行的程序、最近输入；只读查看实时屏幕和会话日志；关闭会话；查看已关闭会话的日志 |
| 端口转发 | 列表、添加 `-L`/`-R`、停止 |
| 后台任务 | `job start` 启动的任务；查询状态、查看日志、终止 |
| 审计日志 | 最近 5000 条远程操作，可筛选 |
| 守护进程 | 状态、SSH 连接、启动/停止/重启（有确认）、守护进程日志 |
| 集成 | 把 xssh 所在文件夹放到 PATH 最前面（Windows 写当前用户的 `HKCU\Environment\Path`，保留原值类型并广播 `WM_SETTINGCHANGE`；Unix 写登录脚本里带标记的一段），并按新进程的 PATH 顺序检查 `xssh` 实际解析到哪个文件，被其他副本覆盖时给出提示；为 Claude Code、Codex、Gemini CLI、GitHub Copilot、Cursor、OpenCode、Windsurf、Amp、Qwen Code、Kiro、Trae、Roo Code、`~/.agents`（Cline 等）安装 / 更新 / 移除 xssh skill，自动检测本机装了哪些工具 |

界面支持简体中文和英文：首次启动跟随系统语言，侧栏底部可随时切换，选择保存在数据目录的 `desktop.json` 中。

窗口无边框、标题栏自绘（gpui-kit `TitleBar`）：拖动、双击最大化、边缘缩放以及 Windows 11 最大化按钮上的贴靠布局都由系统照常处理。

它和 CLI 一样只是守护进程的客户端：

- 关闭窗口不影响守护进程，其中的会话、转发照常运行。
- 以“观察者”身份轮询：不会让本该空闲退出的守护进程一直运行；查看会话屏幕用 `session_peek`，不刷新会话的空闲计时，也不影响 agent 的读取进度。
- 版本不同也不会自动重启守护进程（那会关闭 agent 的会话），只在界面上提示，由人决定是否重启。
- 需要守护进程时调用同目录（或 PATH 中）的 `xssh daemon start`，启动逻辑只有 CLI 一份。
- `xssh-desktop --home DIR` 或 `XSSH_HOME` 指定数据目录，规则与 CLI 相同。
- 所有列表（主机、会话、会话日志、转发、任务、审计、守护进程日志）都是虚拟化渲染（GPUI `uniform_list`），只绘制可见的行；几万行的会话日志和上千条审计记录也能流畅滚动。日志、审计等大文件在后台线程读取，不阻塞界面。

## 代码结构

Cargo workspace，按依赖方向分层（上层只依赖下层）：

```
crates/
  core/     xssh-core    错误类型、home 目录布局、守护进程协议（请求与跨 IPC 的数据类型）、本地 IPC、文本工具、会话日志格式
  store/    xssh-store   本地状态：hosts.toml、config、密钥存储（keyring / 加密文件）、审计日志、任务记录
  engine/   xssh-engine  SSH 连接、会话、exec、文件传输、任务、采集、端口转发、守护进程
  cli/      xssh         命令行（二进制 xssh），也是守护进程的载体
  desktop/  xssh-desktop 桌面程序（GPUI），只依赖 core + store，不含任何 SSH 代码
```

`assets/icon/`：应用图标。`xssh.svg` 是源文件，`xssh.ico`（16–256 px 多尺寸）与 PNG 由它渲染生成；`xssh.rc` 经 build.rs（`embed-resource`）编进两个 Windows 可执行文件，作为资源 1——Explorer、任务栏和 GPUI 窗口标题栏都用它。

core 里 russh 相关的错误转换在 `ssh` feature 后面，只有 engine 打开它，所以 desktop 的依赖树里没有 russh。

## 构建

```
cargo build                   # CLI 及其依赖（默认成员，不编译 GPUI）
cargo desktop                 # 编译并运行桌面程序（= cargo run -p xssh-desktop）
cargo desktop-build           # 只编译桌面程序
cargo all                     # 整个 workspace
cargo test                    # 单元测试
cargo build --release         # 本地 release：不做 LTO，改完代码重编很快
cargo build --profile dist    # 发布版：thin LTO + codegen-units=1，体积更小，编译更慢
```

编译速度相关设置：

- `.cargo/config.toml`：Windows 下用 rustup 自带的 `rust-lld` 链接，比 MSVC `link.exe` 快得多（桌面程序要链接几百个 GPUI crate）。
- dev profile：依赖不生成调试信息，本项目 crate 只保留行号表；GPUI 的布局、文本、光栅化 crate 用 `opt-level = 3`（只在首次编译时多花时间，之后走缓存，界面才不卡）。
- 分层后改哪一层只重编这一层和它上面的层：改桌面程序不碰 CLI，改 engine 不碰桌面程序。
- 守护进程版本号（build id）由 cli 的 build.rs 生成，只在 core/store/engine/cli 源码或 Cargo.lock 变化时更新，只改桌面程序不会导致 agent 的守护进程被重启。

Windows 上 russh 使用 `ring` 加密后端（默认的 aws-lc-rs 需要 NASM）。

## 快速开始

```
xssh host add web1 --host 10.0.0.5 --user deploy --ask-password   # 人工在终端录入密码
xssh exec web1 -- uptime
xssh session open web1 --name w
xssh session run w -- 'cd /srv/app && git pull'
xssh status web1
xssh guide                     # 面向 agent 的完整手册
xssh guide --install-skill     # 安装为 Claude Code skill (~/.claude/skills/xssh)
xssh guide --install-skill --agent codex --agent gemini-cli   # 其他 agent；--agent all = 本机检测到的全部
xssh path add                  # 把本安装放到用户 PATH 最前（新开的终端生效）
```

密码由用户本人通过 `xssh host set-password <alias>` 录入（终端隐藏输入，无终端时弹对话框），不要发给 agent。

## 目录

默认 home：**程序所在目录下的 `data`**（便携模式，整个文件夹就是一份安装；Unix 下软链接会解析到真实二进制所在目录）。`xssh` 与 `xssh-desktop` 放在同一目录即共用数据。可用 `--home` / `XSSH_HOME` 覆盖；程序目录不可写（如 `Program Files`、`/usr/local/bin`）时必须指定。

- 每份安装首次运行时生成 `data/instance.id`，keyring 条目放在 `xssh-<id>` 名下：不同安装的凭据互不干扰，整个文件夹移动后凭据仍可用。
- 开发时 `target/debug/data` 与 `target/release/data` 是两份独立数据，`cargo clean` 会一并删除；调试建议用 `--home` 指向固定目录。

| 文件 | 内容 |
|---|---|
| `config.toml` | 可选全局配置（输出上限、超时、host key 策略、自动应答规则等），见 `xssh config` |
| `hosts.toml` | 主机清单 |
| `known_hosts` | xssh 自己的 host key 记录（同时参考 `~/.ssh/known_hosts`） |
| `audit.jsonl` | 审计日志（`xssh audit`） |
| `sessions/` | 会话转录（已脱敏） |
| `outputs/` | 被截断输出的完整内容 |
| `instance.id` | 本安装的 id（keyring 命名空间） |
| `run/` | daemon 的 token、pid、日志；Windows 下 daemon 从此处的 exe 副本运行，避免锁住安装的二进制 |

## 已知限制

- 凭据"对 agent 不可见"指 xssh 自身不泄露；与 xssh 同一 OS 用户运行的 agent 理论上可直接读取 keyring，不是强安全边界。输出脱敏只匹配原文，base64 等变形后的秘密无法识别。
- `session run` 与 exec 包装依赖 POSIX 风格 shell（bash/zsh/sh/dash/ksh）；fish/csh 登录 shell 下部分功能受限。
- macOS / FreeBSD 的采集解析基于样例数据编写，尚未在真机验证。
- 远端必须是类 Unix 系统。
- Windows 下在 Git Bash 里调用时，以 `/` 开头的参数会被 MSYS 改写为 `C:/...`；xssh 会在远端路径和命令参数中自动还原（stderr 提示一行），无法还原的会报错而不会误执行，此时在该条命令前加 `MSYS_NO_PATHCONV=1`。

## 许可证

[MIT](LICENSE)
