# Changelog / 更新日志

All notable changes are documented here. 本文件记录所有重要变更。
中英对照（English first, 中文在后）.

## [Unreleased]

### Added / 新增

- **Confirmation prompt before deleting a remote file (#28).** SFTP delete is
  irreversible (there is no trash), so the context-menu *Delete* now asks for
  confirmation — showing the full path — before removing anything; a misclick
  no longer silently destroys a file.
  **删除远程文件前先确认 (#28)。** SFTP 删除不可撤销(没有回收站),右键菜单的
  「删除」现在会先弹出确认框(显示完整路径)再执行,误点不会再悄悄删掉文件。

- **Serial port sessions (#14, #17).** New session type for connecting to
  switches, routers and embedded devices over a serial console. Pick
  **Serial** in the session dialog and set the port (`COM3`, `/dev/ttyUSB0`),
  baud rate, data/stop bits, parity and flow control. The serial line reuses
  the full terminal pipeline (output, input, scrollback, copy/paste); SFTP and
  the resource monitor are not applicable and are hidden.
  **串口会话 (#14, #17)。** 新增串口会话类型,用于通过串口控制台连接交换机、
  路由器和嵌入式设备。在会话对话框选择 **串口**,填写串口号(`COM3`、
  `/dev/ttyUSB0`)、波特率、数据/停止位、校验位和流控。串口复用完整的终端管线
  (输出、输入、回滚、复制粘贴);SFTP 和资源监控不适用,已隐藏。

- **Telnet sessions (#17).** New session type for legacy gear that only speaks
  Telnet. Handles RFC 854 option negotiation (suppress-go-ahead / echo /
  window-size), strips IAC sequences from the stream, and tunnels through the
  same SOCKS5 / HTTP proxy as SSH when configured.
  **Telnet 会话 (#17)。** 新增 Telnet 会话类型,用于只支持 Telnet 的老旧设备。
  处理 RFC 854 选项协商(抑制 Go-Ahead / 回显 / 窗口大小),从数据流中剥离 IAC
  序列,并可经与 SSH 相同的 SOCKS5 / HTTP 代理隧道连接。

### Performance / 性能

- **Pipelined SFTP upload (#16).** Uploads now keep ~32 WRITE requests in flight
  on a dedicated SFTP channel instead of writing one chunk and waiting for each
  ack, hiding the round-trip latency that made transfers ~15x slower than `scp`.
  Out-of-order completion is safe (every chunk carries its absolute offset).
  **SFTP 上传流水线化 (#16)。** 上传改为在专用 SFTP 通道上保持约 32 个 WRITE 请求
  并发在途,而不是写一块等一块的 ack,消除了让传输比 `scp` 慢约 15 倍的往返延迟。
  乱序完成也安全(每块都带绝对偏移)。

### Fixed / 修复

- **Dragging the SFTP panel up no longer clears terminal output (#18).** vt100's
  shrink truncated the grid from the bottom, dropping the most recent output;
  before shrinking we now save the top rows to scrollback and scroll so the
  bottom (recent) rows stay visible. Two follow-ups: (1) the shrink now only
  scrolls off as many rows as needed to keep the cursor visible, so rapid
  up/down dragging on a not-yet-full screen no longer pushes the prompt into
  scrollback and strands the cursor at the top (also reported as #24); (2) drag-selection is now stored
  in absolute scrollback coordinates, so selecting from the top of the history
  down through several screens copies every line instead of losing everything
  above the final window when the view auto-scrolls.
  **上拉 SFTP 面板不再清空终端输出 (#18)。** vt100 缩小时从底部截断,丢掉最近输出;
  现在缩小前把顶部行存入回滚区并滚动,使底部(最近)行保持可见。两处后续修复:
  (1) 缩小时只滚走"保持光标可见所需"的行数,疯狂上下拖动未填满的屏幕时不再把
  提示符推进回滚区、光标卡在顶部;(2) 拖选改用绝对回滚坐标存储,从历史顶部往下
  跨多屏选择时能复制到每一行,而不是在视图自动滚动后丢掉最后一屏以上的内容。

### Security / 安全

- **Harden the remote resource monitor against a hostile server (#27).** The
  monitor runs a small loop over an SSH exec channel. It now (1) resets `PATH`
  to the standard system dirs so a server with a hijacked `PATH`/`BASH_ENV`
  can't shadow `awk`/`cat`/`df`/`sleep`; (2) caps the reassembly buffer at 1 MiB
  so a server that streams data without the sync marker can't exhaust memory;
  and (3) parses `/proc` and `df` output with saturating arithmetic and a
  64-row cap per sample, so crafted huge values or a flood of fake interfaces
  can't overflow-panic or swamp the sidebar.
  **加固远程资源监控以防恶意服务器 (#27)。** 监控通过 SSH exec 通道跑一个小循环。
  现在:(1) 重置 `PATH` 为标准系统目录,使被劫持 `PATH`/`BASH_ENV` 的服务器无法
  替换 `awk`/`cat`/`df`/`sleep`;(2) 重组缓冲上限 1 MiB,防止只发数据不发同步标记
  的服务器耗尽内存;(3) 解析 `/proc` 与 `df` 输出改用饱和运算并对每次采样限 64 行,
  使构造的超大数值或伪造网卡洪流无法触发溢出 panic 或拖垮侧栏。

- **Sanitize remote file names before saving downloads (#26).** SFTP downloads
  built the local path straight from the server-supplied name, so a malicious
  server could use path separators, shell-special characters or a Windows
  reserved device name (`CON`, `NUL`, `COM1`…) to write outside the chosen
  folder or hit a device. Downloads now run the name through `sanitize_filename`
  (already used by the open/edit flow), which also gained reserved-device-name
  and leading-whitespace handling.
  **保存下载前清洗远程文件名 (#26)。** SFTP 下载直接用服务器给的文件名拼本地路径,
  恶意服务器可借路径分隔符、shell 特殊字符或 Windows 保留设备名(`CON`、`NUL`、
  `COM1`…)写到目标目录之外或命中设备。现在下载会先经 `sanitize_filename`
  (查看/编辑流程已在用)清洗,并新增了保留设备名与前导空白的处理。

- **Stop logging raw keystroke bytes (#15).** Debug logs recorded the hex of SSH
  input, which could include passwords; now they record only the byte length.
  A follow-up found two more leak sites in the key handler: `send_key` logged
  the raw key string (`key={:?}`) at debug level, and the `[KEY_DIAG]` IME
  diagnostic logged each Shift-typed key's code point at **info** level (no
  `RUST_LOG` needed) — both could expose password characters. They now go
  through a `redact_key` helper that reveals only C0/C1 control codes (what the
  IME diagnostics actually need) and masks every printable character.
  **不再记录原始按键字节 (#15)。** debug 日志原本记录 SSH 输入的十六进制(可能含
  密码),现在只记录字节长度。后续又发现按键处理里还有两处泄露:`send_key` 以
  debug 级打印按键原文(`key={:?}`),`[KEY_DIAG]` IME 诊断更是以 **info 级**
  (无需 `RUST_LOG`)打印每个带 Shift 按键的码位——都可能暴露密码字符。现在统一
  经 `redact_key` 处理,只保留 C0/C1 控制码(IME 诊断真正需要的),可打印字符一律掩码。

## [0.2.3] - 2026-06-05

### Added / 新增

- **Proxy support for SSH / SFTP (#7).** Connections can tunnel through a
  **SOCKS5** (`socks5://`) or **HTTP CONNECT** (`http://`) proxy, with optional
  `user:pass@` credentials. Set it per session in the dialog, or leave it blank
  to use the `$ALL_PROXY` environment variable; empty = direct.
  **SSH / SFTP 代理支持 (#7)。** 连接可经 **SOCKS5**(`socks5://`)或
  **HTTP CONNECT**(`http://`)代理(支持 `user:pass@` 认证)。会话对话框里按需
  填写,留空则用 `$ALL_PROXY` 环境变量,再空则直连。

- **Import hosts from `~/.ssh/config` (#1).** The "Import ~/.ssh/config" action
  (in the settings menu) parses the standard SSH config (`Host` / `HostName` /
  `User` / `Port` / `IdentityFile`, wildcard `Host *` blocks skipped) and adds
  each host as a session, skipping duplicates. Hosts with an `IdentityFile`
  default to key auth.
  **从 `~/.ssh/config` 导入主机 (#1)。** 设置菜单里的「导入 ~/.ssh/config」解析
  标准 SSH 配置(`Host` / `HostName` / `User` / `Port` / `IdentityFile`,跳过
  `Host *` 通配块),将每个主机加为会话并跳过重复;带 `IdentityFile` 的默认用密钥。

- **GitHub Actions release workflow** building native binaries for Windows /
  Linux / macOS (arm64 + x86_64) on each `v*` tag.
  **GitHub Actions 发布工作流**,每个 `v*` 标签自动构建 Windows / Linux /
  macOS(arm64 + x86_64)三平台二进制。

### Fixed / 修复

- The full-width `＋` before "New session" rendered as a tofu box in English;
  switched to an ASCII `+`.
  英文下「New session」前的全角 `＋` 显示为豆腐块,改用 ASCII `+`。

- `install-linux.sh` now auto-detects the `meatshell` binary sitting next to it
  in a release package, so it works with no arguments (it previously defaulted to
  the source-tree `./target/release` path and failed for end users).
  `install-linux.sh` 现在自动识别发布包里同目录的 `meatshell`,无需传参即可使用
  (之前默认指向源码树的 `./target/release`,普通用户直接跑会报错)。

## [0.2.2] - 2026-06-05

### Security / 安全

- **Fix Windows command injection (#12)** — `open_with_os` no longer shells out
  via `cmd /C start`; it calls `ShellExecuteW` directly so a malicious remote
  file name (e.g. `foo&calc.exe`) can't inject commands. Added `sanitize_filename`
  as defence-in-depth.
  **修复 Windows 命令注入 (#12)** —— 打开文件不再经 `cmd /C start`，改用
  `ShellExecuteW` 直接打开，恶意远程文件名（如 `foo&calc.exe`）无法注入命令；
  并新增 `sanitize_filename` 清洗作为纵深防御。

- **Stop echoing the saved password when editing a session (#10)** — the field
  is left blank with a "leave blank to keep" hint; an empty field on save keeps
  the existing password.
  **编辑会话时不再回显已保存密码 (#10)** —— 密码框留空并提示「留空则不修改」，
  保存时为空则保留原密码。

- **Zero passwords in memory on drop (#8)** — passwords now use a `Secret` type
  (`zeroize`) that wipes its heap buffer on drop and redacts itself in logs; the
  on-disk JSON format is unchanged.
  **密码内存清零 (#8)** —— 密码改用 `Secret` 类型（`zeroize`），Drop 时清零堆
  内存、日志中脱敏；磁盘 JSON 格式不变。

### Added / 新增

- **Internationalization — Chinese / English with runtime switching (#9).**
  Static UI uses Slint `@tr` + bundled `.po`; dynamic Rust strings use a `t()`
  helper. Switch via the gear menu; the choice is persisted and the default
  follows the system locale.
  **国际化 —— 中 / 英双语，运行时实时切换 (#9)。** 静态界面用 Slint `@tr` +
  bundled `.po`；Rust 动态文本用 `t()`。设置菜单里切换，选择会持久化，首次启动
  跟随系统语言。

- **Private-key file picker** in the session dialog, plus `.pub` fallback (auto
  strips the suffix to load the matching private key) and uniform `/` path
  separators across platforms.
  **会话弹窗的私钥文件选择器**，并支持 `.pub` 容错（自动去后缀加载对应私钥）、
  路径分隔符统一为 `/`。

- **Linux desktop integration** — `assets/meatshell.desktop` + `install-linux.sh`
  and an `xdg_app_id` so the GNOME/Ubuntu dock shows the app icon on Wayland.
  **Linux 桌面集成** —— `assets/meatshell.desktop` + `install-linux.sh`，并设置
  `xdg_app_id`，使 Wayland 下 GNOME/Ubuntu 任务栏显示应用图标。

- **Screenshots in the README** (`docs/screenshots/`, sensitive info redacted).
  **README 增加截图**（`docs/screenshots/`，敏感信息已打码）。

[0.2.2]: https://github.com/jeff141/meatshell/releases/tag/v0.2.2
