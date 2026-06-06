# Changelog / 更新日志

All notable changes are documented here. 本文件记录所有重要变更。
中英对照（English first, 中文在后）.

## [Unreleased]

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
  bottom (recent) rows stay visible.
  **上拉 SFTP 面板不再清空终端输出 (#18)。** vt100 缩小时从底部截断,丢掉最近输出;
  现在缩小前把顶部行存入回滚区并滚动,使底部(最近)行保持可见。

### Security / 安全

- **Stop logging raw keystroke bytes (#15).** Debug logs recorded the hex of SSH
  input, which could include passwords; now they record only the byte length.
  **不再记录原始按键字节 (#15)。** debug 日志原本记录 SSH 输入的十六进制(可能含
  密码),现在只记录字节长度。

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
