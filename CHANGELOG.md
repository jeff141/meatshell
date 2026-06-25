# Changelog / 更新日志

All notable changes are documented here. 本文件记录所有重要变更。
中英对照（English first, 中文在后）.

## [0.4.18] - 2026-06-26

### Added / 新增

- **Windows 11 圆角窗口 + 投影 (#162 / #166)。** 自绘标题栏的无边框窗口此前是直角、无阴影,
  不符合 Win11 风格。现用 DWM 给它补上系统**圆角**(DWMWA_WINDOW_CORNER_PREFERENCE)和**投影**
  (DwmExtendFrameIntoClientArea);Win10 自动忽略圆角属性,其他平台无影响。
  **Native rounded corners + drop shadow on Windows 11 (#162 / #166).** The frameless window (custom
  title bar) had square corners and no shadow. DWM now gives it the system rounded corners and
  shadow; ignored on Windows 10, a no-op elsewhere.

- **macOS 上 app 快捷键改用 Cmd(⌘),释放 Ctrl 给终端 (#158)。** Mac 有 Ctrl 和 Cmd 两个键,之前
  app 快捷键全占了 Ctrl,导致 nano 里 ^X 等控制键发不到 shell。现按 macOS 习惯:查找 / 复制 /
  粘贴 / 历史 / 保存用 ⌘,物理 Ctrl 原样直达 shell;命令框的 Ctrl+A/E/K/U 行编辑保留 Ctrl;设置 →
  快捷键也显示 ⌘ / ⌃。Windows/Linux 不变。
  **macOS app shortcuts now use Cmd (⌘), freeing Ctrl for the terminal (#158).** macOS has both Ctrl
  and Cmd; app shortcuts all used Ctrl, so terminal control keys (^X in nano…) couldn't reach the
  shell. Now find / copy / paste / history / save use ⌘ and the physical Ctrl passes straight through;
  the command box's Ctrl+A/E/K/U line editing stays on Ctrl; Settings → Shortcuts shows ⌘ / ⌃.
  Windows/Linux unchanged.

### Fixed / 修复

- **SFTP 闲置一段时间后失效 (#160)。** SFTP 连接没有 keepalive,空闲时被 NAT / 防火墙 / 服务器空闲
  超时掐断,之后点目录"文件夹读取失败"、增删改全废。两条连接(终端 + SFTP)现都加 30s keepalive
  保活,真死了由 keepalive_max 关闭。
  **SFTP stopped working after sitting idle (#160).** The SFTP connection had no keepalive, so it was
  silently dropped by NAT / firewall / server idle timeouts; afterwards every operation failed. Both
  connections now send a 30 s keepalive, with keepalive_max still closing a genuinely dead one.

- **git clone / curl 输出在极窄列(~10)乱折行 (#163)。** 布局回流时 root.width 瞬间读成 ≈0,终端
  列数塌到下限 10 并立刻 resize 远程 PTY,正在跑的输出就按 10 列乱折。现给 PTY resize 加 150ms
  防抖,只应用静置后的尺寸,一闪而过的坏值不再发到服务器。
  **git clone / curl output wrapped at ~10 columns (#163).** A layout reflow momentarily reported a
  near-zero width, collapsing the terminal column count to its floor of 10 and resizing the remote
  PTY, which garbled in-flight output. PTY resizes are now debounced (150 ms) so only the settled
  size reaches the server.

- **设置 / 下载下拉菜单在资源面板停靠时错位。** 资源面板停右 / 上时齿轮 / 下载按钮会随工具栏位移,
  但两个下拉用的是固定坐标,会飘到资源面板上。现让下拉跟随按钮位移。
  **Settings / download dropdowns floated over the docked resource panel.** The gear / download
  buttons shift with the toolbar when the resource panel docks right / top, but the two dropdowns
  used fixed coordinates. They now follow their buttons.

- **浅色模式次要 / 弱化文字太浅。** 沉浸壁纸 + 浅色下,副标题、磁盘 / 路径标签、说明文字等灰得发飘、
  对比度差。浅色模式的 text-secondary / text-muted 已加深(深色模式不变)。
  **Faint secondary / muted text in light mode.** With the immersive wallpaper + light mode,
  subtitles, disk / path labels and hints were too pale on the bright background. Light-mode
  secondary / muted text is darkened (dark mode unchanged).

- **沉浸壁纸下,收起的资源面板展开按钮旁露出"黑块"。** 给展开按钮预留的 30px 空位没有背景,露出
  底下深色壁纸,浅色主题里就是一小块黑。现补上与按钮一致的磨砂背景。
  **A "black block" next to the collapsed resource-panel expand button in immersive mode.** The 30px
  gap reserved for the expand button had no background and showed the raw (dark) wallpaper; it now
  uses the same frosted background as the button.

### Security / 安全

- **记录 RUSTSEC-2026-0154 不可达,暂不升级 russh (#151)。** 该 DoS 在 russh 的 ssh-agent 帧解析,
  meatshell 完全不用 ssh-agent,漏洞代码路径在本二进制里不可达、不可利用;而唯一修复版
  (russh ≥ 0.60.3)会引入一堆**预发布**加密库(ed25519-dalek pre、aes-gcm rc…),在 SSH 客户端里
  用未审计的 rc 密码库风险更大。故 russh 暂留 0.49,等其依赖脱离 -rc 再迁移;新增 audit.toml 附完整理由。
  **Documented RUSTSEC-2026-0154 as unreachable; holding russh at 0.49 (#151).** The DoS is in russh's
  ssh-agent frame parsing, which meatshell never uses — the path is dead code here. The only patched
  russh (>= 0.60.3) drags in a stack of pre-release crypto crates, a worse trade than this unreachable
  DoS, so russh stays at 0.49 until its deps leave the -rc channel. Adds audit.toml with the full
  rationale.

## [0.4.17] - 2026-06-24

### Added / 新增

- **MFA / 验证码登录(JumpServer 等强制开启 MFA 的堡垒机)(#86)。** 这类堡垒机在
  keyboard-interactive 里先要密码、再要动态验证码;旧版对每个提示都回填密码,验证码那步
  必然失败(这正是"不支持 JumpServer"的真实原因)。现在密码挑战自动用已保存密码应答,
  其余挑战(MFA / 验证码)弹出「双重验证」对话框向你索取,回车即继续;终端与 SFTP 并发
  连接只问一次,输错码重连会重新弹框,而非静默重放旧码。
  **MFA / verification-code login on bastions that force MFA (JumpServer etc.) (#86).** Such
  bastions ask for the password then a one-time code over keyboard-interactive; the old code
  answered every prompt with the password, so the code step always failed. Now the password
  challenge is answered automatically and any other challenge pops a "Two-factor (MFA)" dialog
  showing the server's prompt; the shell and SFTP ask once, and a wrong code re-prompts on
  reconnect instead of being replayed.

- **纯键盘命令历史检索(#140)。** 命令框按 `Ctrl+R` 唤出历史并聚焦搜索框,`↑↓` 选择、
  回车执行、`Esc` 关闭并回到终端;终端里 `Ctrl+Shift+R` 跳过去(用 Shift 保留 shell 自身的
  反向搜索)。整个历史检索可全程不碰鼠标。
  **Keyboard-only command-history search (#140).** `Ctrl+R` in the command box opens the
  history with its search box focused; `↑↓` select, Enter runs, `Esc` closes and returns to
  the terminal. `Ctrl+Shift+R` jumps there from the terminal (Shift keeps the shell's own
  reverse-search).

- **会话选项:禁用 shell 集成(Windows / pwsh 服务端)(#140)。** 会话编辑「高级」里新增
  开关,勾上后跳过 cwd 跟随注入与远程资源监控——专为非 POSIX shell(Windows pwsh/cmd)
  准备,避免注入破坏 shell。
  **Session option: disable shell integration (Windows / pwsh server) (#140).** A new toggle
  in the session dialog's advanced section skips the cwd-follow hook and the resource monitor —
  for non-POSIX shells (Windows pwsh/cmd) where injecting them breaks the shell.

### Changed / 优化

- **空闲降耗:失焦停光标闪烁 + 后台暂停/降频系统采样(#127)。** 几个周期性定时器原先即便
  窗口在后台、终端空闲也照常触发整窗重绘。现在:光标在窗口失焦时停止闪烁(改为常亮);
  系统采样在最小化/被遮挡时暂停、仅失焦时降到 ~5s。实测后台空闲 CPU 从约 10% 降到接近 0。
  **Cut idle CPU: stop the cursor blink when unfocused, pause/throttle the system sampler in
  the background (#127).** Periodic timers used to repaint the whole window even backgrounded
  and idle. The cursor now stops blinking (shows solid) when the window is unfocused, and the
  sampler pauses when minimized/occluded and backs off to ~5 s when merely unfocused.

- **弹窗交互:Esc 关闭 + 关闭确认抢焦点(#140)。** 设置 / 关闭确认 / 凭据 / MFA 现在都能用
  `Esc` 关闭;关闭确认弹窗会抢走键盘焦点——回车/空格关闭、`Esc` 取消(「点叉 + 空格」一气
  呵成),终端背后不再被误输入。快捷键弹窗内容过多时可滚动。
  **Dialog interaction: Esc-to-close + focus the close-confirm dialog (#140).** Settings /
  close-confirm / credential / MFA dialogs now close with `Esc`; the close-confirm dialog grabs
  keyboard focus (Enter/Space close, `Esc` cancels) so X-then-Space closes it and the terminal
  no longer keeps receiving input behind it. The shortcuts dialog scrolls when it's too tall.

### Fixed / 修复

- **Windows / pwsh 服务端 shell 失效(回归,自 0.4.7)(#140)。** cwd 跟随注入的是 POSIX
  专用 hook,pwsh/cmd 既跑不了、也不回吐它等待的 OSC 7,导致客户端一直屏蔽终端输出、空白
  卡死(SFTP 是独立通道,所以不受影响)。现给屏蔽窗口加 1.2s 超时兜底:非 POSIX shell 也能
  正常显示;配合上面的「禁用 shell 集成」开关可做完全干净的处理。
  **Windows / pwsh server shell stopped working (regression since 0.4.7) (#140).** The
  cwd-follow setup injects a POSIX-only hook; a Windows pwsh/cmd shell can't run it and never
  echoes the OSC 7 the client waits for, so output stayed hidden and the terminal went blank.
  The suppression now has a 1.2 s timeout so a non-POSIX shell is usable again; pair it with the
  new "disable shell integration" toggle for a fully clean result.

- **修正 macOS 安装说明(#135)。** README 写的是 `tar -xzf …macos-*.tar.gz` + 裸 `meatshell`
  二进制,但实际发布产物是 `.zip` + `meatshell.app` 应用包,三条命令全对不上。已改为:解压
  `.zip` →(可选)移入 `/Applications` → 去 `com.apple.quarantine` 隔离属性 → `open`。
  **Fixed the macOS install instructions (#135).** The README said to `tar -xzf …macos-*.tar.gz`
  and run a bare `meatshell` binary, but the release artifact is a `.zip` containing
  `meatshell.app`. Updated to: unzip → optionally move to `/Applications` → clear
  `com.apple.quarantine` → `open`.

## [0.4.16] - 2026-06-23

### Added / 新增

- **沉浸式壁纸主题(可换壁纸 + 全局沉浸配色)。** 新增「设置 → 界面 → 壁纸」,提供
  macOS 风格的缩略图选择器:内置 **3 张**(简约·浅、简约·暗、**幻想3048**——赛博朋克合成波,
  星空 + 发光星球 + 霓虹网格,均为程序化绘制、无图片资源),也可「选择文件…」用自己的图片。
  选定后壁纸铺满整个窗口(含终端、侧栏、SFTP,以及独立的进程窗),各面板**磨砂半透**让壁纸
  透出,同时从壁纸提取主色**自动重着色强调色并微调背景**,深浅由壁纸亮度决定(内置款),
  自定义照片则交给主题开关手动控制可读性。**下个版本默认即「幻想3048 + 暗色」。**
  **Immersive wallpaper theming (custom wallpaper + global tinting).** Adds Settings →
  Interface → Wallpaper with a macOS-style thumbnail picker: **3 built-ins** (Meat Light,
  Meat Dark, and **Fantasy 3048** — a cyberpunk synthwave scene with a starfield, a glowing
  planet and a neon grid, all drawn procedurally with no image assets) plus a "Choose
  file…" option for your own image. The wallpaper fills the whole window (terminal,
  sidebars, SFTP and the detached process window), panels **frost translucently** to let it
  show through, the accent is **recoloured from the image's dominant colour** and surfaces
  are subtly tinted, with light/dark taken from the wallpaper's brightness (built-ins) while
  custom photos leave light/dark to the theme toggle for readability. **The next release
  ships with "Fantasy 3048 + dark" as the default.**

- **便携模式:配置改存到程序同目录的 `config/`(#141)。** 用户数据(`sessions.json`、
  加密密钥、`known_hosts`、`error.log`)现在优先存放在**可执行文件旁的 `config/` 文件夹**,
  整个程序可以随 U 盘携带,也不再往用户目录(`%APPDATA%`)里塞东西。当程序装在只读位置
  (如 Program Files / `/usr`)时,自动回退到原来的「按用户的系统配置目录」——这也是旧版本
  的存放位置,所以**老安装原样可用**。首次切到便携目录时,会把旧用户目录里的数据**复制**
  过去(只复制不删除、不覆盖已存在文件,作为兜底),升级用户不会丢失已保存的会话。
  **Portable mode: config moves to a `config/` folder next to the app (#141).** User data
  (`sessions.json`, the encryption key, `known_hosts`, `error.log`) is now stored, by
  preference, in a **`config/` folder beside the executable**, so the whole app can travel
  on a USB stick and stops cluttering the user profile (`%APPDATA%`). When the app is
  installed somewhere read-only (Program Files / `/usr`), it falls back to the per-user OS
  config dir — the same place older versions used, so **existing installs keep working
  untouched**. On the first launch that lands on the portable dir, data from the legacy
  per-user dir is **copied** over (copy-not-move, never overwriting, as a safety net) so
  upgrading users don't lose saved sessions.

- **终端内查找:Ctrl+F 唤出查找栏。** 在会话里按 Ctrl+F 即可弹出顶部查找栏(与右键菜单
  → 查找一致),输入即时高亮所有匹配,Esc 关闭;已在「设置 → 快捷键」中登记。
  **Find in terminal: Ctrl+F opens the find bar.** Press Ctrl+F in a session to bring up
  the find bar (same as right-click → Find); matches highlight as you type and Esc closes
  it. Now listed under Settings → Shortcuts.

- **面板可拖动吸附停靠(资源面板 + SFTP)。** 资源面板和 SFTP 面板现在都能拖到四条边
  (上/下/左/右):拖动面板手柄时,四条边浮现高亮放置区,松手即吸附到那条边。两个面板都
  可拖动调节大小;折叠后会缩成停靠边缘的一个小展开按钮(彻底隐藏面板)。**自适应:** 资源
  面板横向(上/下)停靠时,内部小组件自动改为横排;SFTP 竖向(左/右、窄)停靠时隐藏目录树,
  并随宽度**渐进隐藏「大小→时间」列**(名称快被挤成「…」时才让位),横向(上/下、宽)停靠
  则恒显示全部列。SFTP 工具栏左侧新增专用拖动手柄,密集控件下也能稳稳拖动。
  **Drag-to-dock panels (resource panel + SFTP).** Both the resource panel and the SFTP
  panel can now be dragged to any edge (top / bottom / left / right): dragging the
  panel's handle shows highlighted drop zones on all four edges, and releasing snaps it
  there. Both panels are drag-resizable and collapse to a small expand button on their
  docked edge (fully hiding the panel). **Responsive:** the resource panel lays its
  widgets out in a row when docked horizontally; the SFTP panel hides its directory tree
  when docked vertically (narrow) and progressively drops the **Size → Modified** columns
  as it narrows (only once the Name would elide to “…”), while a horizontal (wide) dock
  always shows every column. A dedicated drag grip was added to the SFTP toolbar so the
  panel is grabbable even though its toolbar is full of controls.

- **布局持久化。** 两个面板的停靠边与宽/高,以及父窗口大小,都会在退出时保存、下次启动
  恢复——可以保留你喜欢的窗口尺寸和面板布局。
  **Layout persistence.** Each panel's docked edge and size, plus the window size, are
  saved on exit and restored on the next launch — so your preferred window size and
  panel arrangement stick.

### Changed / 优化

- **历史命令的搜索框移到下拉框底部 (#131)。** 命令历史下拉向上展开,搜索框原先在顶部、
  位置随历史条数上下浮动、不好找;现在固定在下拉框**底部**(紧挨命令输入框),列表在其上方
  填充并可滚动——位置稳定、一眼可见,和 FinalShell 一致。
  **History search box moved to the bottom of the dropdown (#131).** The command-history
  dropdown opens upward; the search box used to sit at the top, drifting up and down with
  the number of entries and hard to find. It's now pinned to the **bottom** of the
  dropdown (right above the command input), with the scrollable list filling the space
  above it — a fixed, immediately visible spot, matching FinalShell.

- **SFTP 折叠按钮与资源面板统一,并保持在右侧。** 两个面板现在共用同一个展开按钮组件;
  SFTP 的控件本就在右侧,折叠后的展开按钮也随之停在右下/右上,不再突兀地跳到左边。
  **SFTP collapse button unified with the resource panel, kept on the right.** Both panels
  now share one expand-button component; since SFTP's controls live on the right, its
  collapsed expand button stays at the bottom-/top-right instead of jumping to the left.

### Fixed / 修复

- **含中文的行复制/查找列错位 (#132)。** 终端纯文本按「一字一字符」存储,而中文(CJK)字
  在网格上占两列;复制时把选区列号当作字符下标,导致丢失的字符数恰好等于选区前面的中文
  字数(如选「1pctl update password」实际只复制到「e password」)。现引入 unicode-width
  做「字符↔网格列」换算:复制所见即所得,在宽字形第二格起选也会整字纳入;查找高亮框(同源
  问题)改按网格列绘制,中文行之后也能精确罩住文字。
  **Copy & find column drift on lines with CJK glyphs (#132).** The terminal's plain text
  stores one char per glyph, but a wide (CJK) glyph spans two grid cells; copy treated a
  selection's column as a char index, dropping as many characters as there were wide glyphs
  before the selection (selecting “1pctl update password” yielded only “e password”). A
  char-to-column conversion (via unicode-width) makes copy WYSIWYG — anchoring on the
  second cell of a wide glyph still grabs the whole glyph — and find highlights (same root
  cause) now sit on grid columns so they line up after CJK.

- **macOS 欢迎页布局错位。** 欢迎页的标题、副标题、快速连接卡片在 macOS 上被拉开(标题与
  副标题间出现大空隙)。现在 Welcome 显式填满内容区、头部固定在顶部按自然高度排列,卡片填满
  其余空间。
  **macOS welcome-page layout spread apart.** The title, tagline and quick-connect card
  were spaced out on macOS (a large gap between the title and tagline). The Welcome view
  now explicitly fills the content area and the header is pinned to the top at its
  natural height, with the card filling the rest.

## [0.4.13] - 2026-06-21

### Fixed / 修复

- **堡垒机(JumpServer 等)密码登录“认证失败” (#86)。** 这类堡垒机默认只放行
  `keyboard-interactive` 认证、关闭 `password` 方法,旧版只尝试 `password`,因此直接
  “认证失败”——Xshell/MobaXterm/WindTerm 能登正是因为会自动回退。现在密码认证失败后会
  断开并重连一条全新连接,改用 `keyboard-interactive` 以密码应答服务器提示。注意:russh
  在一次失败的认证后无法在同一句柄上切换认证方法(会卡死),因此回退必须重连。已在真实的
  keyboard-interactive-only sshd 上验证登录成功。
  **Password login through bastions (JumpServer etc.) failed with “authentication
  failed” (#86).** Such bastions disable the `password` SSH method and only accept
  `keyboard-interactive`; the old code only tried `password`, so it failed outright —
  other clients get in because they fall back automatically. Now, on password-auth
  failure we disconnect and reconnect on a fresh handle, then authenticate via
  `keyboard-interactive`, answering each prompt with the password. (russh hangs if a
  second auth method is attempted on a handle whose first attempt already failed, so a
  reconnect is required.) Verified against a real keyboard-interactive-only sshd.

### Changed / 优化

- **设置·界面简约重做。** 右侧从竖排改为「分区 + 标签左·控件右」的紧凑布局(iOS 风
  开关、`[− 值 +]` 步进器、固定字号不随界面缩放放大,解决“字体过大”观感);仍为内嵌
  模态浮层,打开时遮罩吞鼠标 + 抢焦点吞键盘,禁止对主窗口的一切输入,卡片只能在窗口内拖动。
  **Redesigned Interface settings.** The right pane moves from stacked fields to a
  compact “section + label-left · control-right” layout (iOS-style switches,
  `[− value +]` steppers, fixed typography that ignores UI scale — fixing the
  “fonts too big” feel). Still an embedded modal overlay that blocks all input while
  open (veil swallows mouse, focus scope swallows keys); the card only drags within
  the window.

- **初始窗口放大到 1440×900。** 从 1200×760 提升到更舒适的默认尺寸,对齐同类客户端。
  **Larger default window, 1440×900.** Up from 1200×760, matching comparable clients.

- **Quieter startup logs.** Silenced fontdb's harmless "malformed font" warning for
  system fonts it can't parse but skips anyway (e.g. Windows' `mstmc.ttf`), and
  demoted the routine UI-font-selection line to `debug` — only an actual font-load
  failure still warns. `error.log` stays clean.
  **更安静的启动日志。** 屏蔽 fontdb 对无法解析(但会自动跳过)的系统字体发出的
  「malformed font」无害告警(如 Windows 的 `mstmc.ttf`),并把常规的界面字体选择日志
  降为 `debug`——只有真正的字体加载失败才会告警。`error.log` 保持干净。

### Added / 新增

- **侧栏可拖动调宽。** 在资源面板与主区之间加了可拖动分隔条,宽度可在 160–520px 间
  调节并持久化到配置(重启保留);折叠侧栏时分隔条自动隐藏,拖动期间禁用折叠动画以跟手。
  **Drag-resize the sidebar.** A draggable splitter sits between the resource panel and
  the main area; the width is adjustable within 160–520px and persisted to config
  (survives restart). The splitter hides when the sidebar is collapsed, and the
  collapse animation is disabled while dragging for 1:1 tracking.

- **进程监视独立窗口。** 进程监视从内嵌浮层提升为真正的独立 OS 窗口,可拖出主窗口、
  拖到第二块屏幕;无边框自绘标题栏 + 右下角缩放手柄,与主窗口实时共享同一份进程数据。
  **Detachable process-monitor window.** The process monitor is now a real top-level OS
  window that can be dragged outside the main window or onto a second monitor, with a
  frameless custom titlebar and a bottom-right resize grip; it shares one live process
  model with the main window.

- **Group quick commands, collapsible (#55).** Quick commands now take an optional
  group/folder name. Leaving it empty drops the command into the implicit
  "default" group. In the command-bar popup each group shows a header that can be
  clicked to collapse/expand it — same behaviour as the welcome page's quick-connect
  session groups. The manage dialog gained a "Group (optional)" field and shows the
  grouping.
  **快捷命令支持分组、可收起 (#55)。** 快捷命令新增可选的分组名,留空则归入隐式的
  「default」分组。命令栏弹窗里每个分组带标题,点击即可收起/展开——和欢迎页快速连接的
  会话分组体验一致。管理对话框新增「分组（可选）」输入框并按分组展示。

- **Full quick-command management, mirroring the session panel (#55).** Right-click
  a command — in the command-bar popup or the manage dialog — for Edit / Duplicate /
  Delete / Move to group, and right-click a group header for Rename / Delete (empty) /
  New group; the manage dialog also has a "+ New group" button. Same right-click
  model as the welcome page's quick-connect sessions. Groups start **collapsed** by
  default, and empty groups persist so you can pre-create folders.
  **快捷命令完整管理,对齐会话面板 (#55)。** 在命令栏弹窗或管理对话框里右键命令(编辑、
  复制、删除、移动到分组),右键分组标题(重命名、删除空分组、新建分组),管理对话框另有
  「+ 新建分组」按钮——与欢迎页快速连接会话的右键体验一致。分组**默认收起**,空分组会被
  保留以便预先建好文件夹。

## [0.4.12] - 2026-06-20

### Fixed / 修复

- **macOS 26 blank text — switch the default CJK UI font to one femtovg can render
  (#129, #108).** Root cause finally pinned: on some macOS 26 machines femtovg
  cannot rasterize the *modern* system CJK fonts (PingFang SC, Hiragino) — fontdb
  finds them but every glyph comes out blank — while the older Heiti/STHeiti/Songti
  faces render perfectly (verified per-font on an M2 / macOS 26). It was never the
  renderer (0.4.11's femtovg revert alone didn't help) nor font *loading* (fontdb
  loaded 900+ faces). The UI now prefers the reliably-rendering "Heiti SC" (a clean
  sans-serif that ships on every macOS), with STHeiti/Songti as further fallbacks
  and the embedded "Meatshell Mono" as a last resort so the window is never blank.
  A `MEATSHELL_UI_FONT="<family>"` env var can force any family without a rebuild.
  **修复 macOS 26 文字全白——默认中文界面字体改用 femtovg 能渲染的字体 (#129, #108)。**
  根因最终定位:部分 macOS 26 机器上 femtovg 无法栅格化*新版*系统中文字体(PingFang
  SC、Hiragino)——fontdb 能找到它们,但每个字形都画成空白;而老字体
  Heiti/STHeiti/Songti 渲染完全正常(已在 M2 / macOS 26 上逐字体实测)。既不是渲染器
  (0.4.11 单独退回 femtovg 没用),也不是字体*加载*(fontdb 加载了 900+ 个 face)。
  界面现在优先用稳定渲染的「Heiti SC」(所有 macOS 自带的干净黑体),STHeiti/Songti
  作为后备,内置「Meatshell Mono」兜底,确保窗口永不全白。可用环境变量
  `MEATSHELL_UI_FONT="<字体名>"` 免重编强制指定任意字体。

## [0.4.11] - 2026-06-20

### Fixed / 修复

- **macOS text-invisible regression — renderer no longer force-switched (#129, #108).**
  0.4.10 force-set the Skia renderer on macOS to work around femtovg failing to
  render text on macOS 26 (#108). That shipped unverified and broke a *different*
  set of Macs (Apple Silicon, macOS 26.5): Skia could not resolve the "PingFang SC"
  UI font, so all text vanished there instead (icons survived because they use an
  embedded font). The default now stays femtovg (known-good for the majority);
  Skia is still compiled in on macOS and can be opted into at launch with
  `SLINT_BACKEND=winit-skia` for machines where femtovg fails.
  **修复 macOS 文本全部消失的回退问题——不再强制切换渲染器 (#129, #108)。** 0.4.10 为
  绕过 macOS 26 上 femtovg 取字失败(#108),在 macOS 强制改用 Skia 渲染器;该改动
  未经真机验证就发布,反而弄坏了另一批 Mac(Apple Silicon / macOS 26.5):Skia 无法
  解析「PingFang SC」界面字体,导致这些机器上文字全部消失(图标因使用内嵌字体而正常)。
  现默认改回 femtovg(对绝大多数机器正常);macOS 仍编译 Skia,femtovg 失效的机器可在
  启动时用 `SLINT_BACKEND=winit-skia` 手动启用。

### Added / 新增

- **Cancel an in-progress upload, with remote cleanup (#100).** Uploads can now be
  cancelled like downloads; cancelling removes the half-written file on the remote
  so no partial junk is left behind.
  **上传也支持取消并清理远端半成品 (#100)。** 上传可像下载一样取消;取消会删除远端已
  写入的半成品文件,服务端不留垃圾。

- **Sponsor / donation link in the README.** Added a WeChat sponsor QR for anyone
  who'd like to support development.
  **README 增加赞助/捐赠入口。** 加入微信赞助二维码,欢迎支持项目开发。

### Changed / 优化

- **Silenced ICU4X segmentation-data log noise.** Suppressed the spurious ICU4X
  data-error warnings so they no longer clutter the log / error.log.
  **屏蔽 ICU4X 段落数据噪音日志。** 抑制无意义的 ICU4X data-error 警告,不再污染日志
  与 error.log。

## [0.4.10] - 2026-06-19

### Added / 新增

- **SFTP multi-select with one-archive download (#100).** Check multiple files in
  the SFTP panel and download them together: the selection is packed into a single
  `tar` on the remote (named after the first item, e.g. `11等文件.tar`), pulled in
  one transfer, then the temp is removed. Any download action (right-click, row,
  toolbar) packs the whole checked set when 2+ are checked; a single selection
  downloads as a plain file. Batch delete is also supported, and an empty folder
  is reported instead of creating an empty local directory.
  **SFTP 文件多选 + 打包下载 (#100)。** 在 SFTP 面板勾选多个文件即可一起下载:选中
  项在远端打包成单个 `tar`(以第一个文件命名,如 `11等文件.tar`),一次性下载后删除
  临时包。勾选 ≥2 项时,任意下载动作(右键/行内/工具栏)都打包整组;单选则按普通
  文件下载。同时支持批量删除;下载空文件夹会给出提示而非创建空目录。

- **Cancel an in-progress transfer (#100).** Each transfer row shows a cancel
  button while active or preparing; cancelling removes the partial local file and,
  for archive downloads, the remote temp archive — no junk left on either side.
  **可取消进行中的传输 (#100)。** 传输记录每行在下载中/准备中时显示取消按钮;取消会
  删除本地半成品文件,打包下载还会删除远端临时包,本地与服务端都不留垃圾。

- **Name port-forward rules (#100).** Port-forward rules can be given an optional
  name so they're easy to tell apart in the list.
  **端口转发规则可命名 (#100)。** 转发规则可设置可选名称,便于在列表中区分。

- **Global UI scale setting (#100 #117 #118).** A scale control in Interface
  settings zooms the whole UI (fonts, spacing, radii) from 80% to 200%.
  **界面整体缩放设置 (#100 #117 #118)。** 界面设置新增缩放控件,可将整个界面(字体、
  间距、圆角)从 80% 到 200% 缩放。

### Changed / 优化

- **Much faster downloads (#100).** Downloads now use a dedicated, pipelined SFTP
  channel that keeps many READ requests in flight at once (like uploads already
  did), hiding round-trip latency — large files and archive bundles download
  noticeably faster.
  **下载大幅提速 (#100)。** 下载改用专用、流水线化的 SFTP 通道,多个读请求并发在途
  (与上传一致),掩盖往返延迟 —— 大文件和打包包下载明显更快。

- **Switch directories during transfers.** SFTP transfers run on their own task,
  so listing and changing directories stays responsive while files move.
  **传输时仍可切换目录。** SFTP 传输在独立任务上运行,文件传输期间列目录、切换目录
  依然流畅。

### Fixed / 修复

- **macOS 26 (Tahoe): all UI text invisible (#108).** The default femtovg renderer
  failed CoreText font lookup on macOS 26, blanking every glyph including the
  embedded mono font. macOS now uses the Skia renderer (Windows/Linux unchanged).
  **macOS 26 (Tahoe) 界面文本全部消失 (#108)。** 默认 femtovg 渲染器在 macOS 26 上
  取字失败,所有文字(含内嵌等宽字体)消失。macOS 现改用 Skia 渲染器(Windows/Linux
  不变)。

- **Welcome session list now scrolls (#116).** When there are more sessions than
  fit, the welcome screen's session list scrolls instead of clipping.
  **欢迎页会话列表可滚动 (#116)。** 会话过多时,欢迎页的会话列表可滚动,不再被裁切。

## [0.4.9] - 2026-06-19

### Added / 新增

- **Searchable command-history dropdown (#101).** The command-history list is now
  filterable — type in the search box to narrow entries instantly, then click or
  press Enter to run the match.
  **命令历史下拉支持搜索 (#101)。** 历史列表新增搜索框,输入关键字即可实时过滤,
  点击或回车直接执行匹配项。

- **Readline keys in the command box + shortcuts reference (#103).** The command
  box now honours common readline bindings (Ctrl+A/E/K/U/W, Alt+B/F/D/Backspace,
  etc.) for fast inline editing; a keyboard-shortcuts reference panel is also
  added so users can discover available bindings at a glance.
  **命令输入框支持 Readline 快捷键 + 快捷键参考 (#103)。** 命令框现在支持常见
  readline 绑定(Ctrl+A/E/K/U/W、Alt+B/F/D/Backspace 等)进行快速行内编辑;
  另加快捷键参考面板,方便用户一览可用组合键。

- **Scroll arrows when tabs overflow (#122).** When open tabs exceed the tab bar
  width, left/right arrow buttons appear so users can scroll through the hidden
  tabs instead of losing access to them.
  **标签溢出时显示滚动箭头 (#122)。** 当打开的标签超出标签栏宽度时,左右箭头
  按钮出现,可滚动查看被遮挡的标签。

- **Slim scrollbar for the terminal output area (#103).** The terminal's vertical
  scrollbar is now a thin, auto-hiding overlay that doesn't eat into the column
  count, giving more screen real estate to the actual output.
  **终端输出区窄滚动条 (#103)。** 终端纵向滚动条改为细窄的自动隐藏覆盖层,
  不再占用列数,把更多屏幕空间留给实际输出。

### Fixed / 修复

- **Preserve the MOTD/banner when hiding the injected setup line (#98).** The
  previous approach stripped too aggressively and could swallow the server's
  MOTD/banner that arrives before the shell prompt; the matcher now only discards
  the single injected line, leaving the banner intact.
  **隐藏注入设置行时保留 MOTD/横幅 (#98)。** 之前的做法剥离过度,会把 shell 提示符
  之前到达的服务器 MOTD/横幅一并吞掉;现在匹配器仅丢弃注入的那一行,横幅原样保留。

- **Reserve space for toolbar icons + scroll overflowing tabs (#122).** The tab
  bar now leaves a right margin so the last tab's close button isn't hidden
  behind the toolbar icons; tabs that still overflow are scrollable.
  **为工具栏图标预留空间 + 溢出标签可滚动 (#122)。** 标签栏右侧留出余量,
  最后一个标签的关闭按钮不再被工具栏图标遮挡;仍然溢出的标签可滚动查看。

## [0.4.8] - 2026-06-18

### Added / 新增

- **Immersive frameless title bar.** On Windows/Linux the app draws its own
  themed title bar (app icon + name, minimize/maximize/close, draggable to move,
  double-click to maximize, edge/corner resize) instead of the OS chrome — so the
  top follows the light/dark theme instead of staying a mismatched native bar.
  macOS keeps its native decorations. (#119)
  **沉浸式无边框标题栏。** Windows/Linux 下自绘主题色标题栏(应用图标+名称、
  最小化/最大化/关闭、拖动移动、双击最大化、边角缩放),不再使用系统标题栏,顶部
  跟随明暗主题;macOS 保留原生标题栏。

### Fixed / 修复

- **htop/btop box-drawing and braille no longer render as tofu** on machines
  without Cascadia Mono installed (e.g. Win11 Home). The embedded font is now a
  uniquely-named family ("Meatshell Mono") so the OS can't substitute a
  glyph-poor fallback for it. (#114)
  **htop/btop 的线框和盲文字符不再显示为方块**(在未安装 Cascadia Mono 的机器上,
  如 Win11 家庭版)。内嵌字体改用独一无二的族名「Meatshell Mono」,系统无法再用
  缺字形的字体顶替它。
- **The injected setup line no longer leaks to the terminal on connect**, even
  when it wraps across the terminal width. Output is buffered until the hook's
  OSC sequence arrives, then everything up to it is discarded. (#98)
  **连接后不再出现注入的设置命令**,即使它按终端宽度换行也能正确隐藏。
- **Smooth scrollback across the live/scrolled boundary.** After shrinking then
  restoring the terminal (e.g. dragging the SFTP panel over it and back),
  scrolling back through history no longer jumps near the bottom. (#119)
  **回滚历史在实时/滚动边界处平滑。** 把 SFTP 面板拉上来盖住终端再放下后,往回翻
  历史时接近底部不再跳。
- **Fast drag-selection in the terminal works again.** A quick drag is no longer
  stolen by the Flickable, so selecting text by dragging fast still selects. (#119)
  **终端里快速拖动选择恢复正常。** 快速拖动不再被滚动容器抢走,快速拖选也能选中。
- **The Interface dialog's close button can't be dragged off-screen.** Its drag
  is clamped inside the window, so the modal dialog can no longer become
  unclosable. (#119)
  **「界面」设置对话框的关闭按钮不会被拖出屏幕。** 拖动被限制在窗口内,模态对话框
  不会再变得无法关闭。

## [0.4.7] - 2026-06-16

### Added / 新增

- **Host-key verification with a first-connect confirmation dialog.** On first
  contact a dialog shows the host, key type and SHA256 fingerprint; the key is
  remembered (a known_hosts file beside sessions.json) only after you trust it.
  A later key that differs is flagged as a possible MITM and needs re-confirming.
  Replaces the previous "accept any key" behaviour. (#109)
  **主机密钥校验 + 首次连接确认弹窗。** 首次连接会弹窗显示主机、密钥类型和 SHA256
  指纹,确认信任后才记住(known_hosts 文件,与 sessions.json 同目录);之后密钥若
  变化会作为疑似中间人攻击提示并要求重新确认。取代了原先「接受任意密钥」的行为。
- **Quick-connect login, Xshell-style.** New SSH/Telnet sessions now require a
  host. The username no longer defaults to `root`; if a session is missing its
  username and/or (password-auth) password, you're prompted for them on connect,
  with an optional "remember". Auto-naming uses `user@host`, or just the host
  when no username is given. (#110)
  **类 Xshell 的快速连接登录。** 新建 SSH/Telnet 会话需填主机;用户名不再默认
  `root`;会话缺用户名 和/或(密码认证)密码时,连接时弹窗补充,可勾选「记住」。
  自动命名用 `user@host`,无用户名时仅用主机名。
- **Commands typed in the terminal now join the command history.** Captured via
  the shell integration hook (bash/zsh), so the command box and ↑/↓ recall
  include what you ran in the terminal — passwords typed at prompts are never
  captured. (#113)
  **终端里直接敲的命令现在也进命令历史。** 通过 shell 集成钩子(bash/zsh)捕获,
  命令栏和 ↑/↓ 回溯都会包含;在提示符处输入的密码不会被捕获。

### Changed / 变更

- **Command history is de-duplicated, most-recent last.** Re-running a command
  moves it to the end instead of leaving duplicates; existing history is cleaned
  up on load. (#113)
  **命令历史全局去重,最近使用排在最后。** 重复执行只会把命令移到末尾而不再留重复
  项;已有历史在加载时清理一次。

### Fixed / 修复

- **The injected prompt-setup line no longer leaks to the terminal on connect.**
  When the echoed setup line was split across packets the matcher missed it,
  showing `test -z "$FISH_VERSION" && eval '…'`; output is now buffered until the
  line is complete so it's reliably stripped however it's chunked. (#98)
  **连接后不再出现注入的设置命令。** 该回显行被分包拆开时旧逻辑匹配不到,会显示
  `test -z "$FISH_VERSION" && eval '…'`;现在缓冲到该行完整再剥离,无论如何分块都能隐藏。
- **ZMODEM `sz a b c` now receives every file**, not just the first — ZEOF ends a
  file, not the session. (#109)
  **ZMODEM `sz a b c` 现在会接收每个文件**,而不只是第一个(ZEOF 表示单个文件结束,
  而非整个会话结束)。
- **A denied directory listing is handled gracefully.** Instead of spinning
  forever on a permission error, the panel stops loading and shows a clear
  "permission denied" message while keeping the current view. (#112)
  **目录无权限时优雅处理。** 不再卡在加载转圈,面板会停止加载并明确提示「权限不足」,
  同时保留当前视图。
- **IPv6 bind addresses are bracketed** for `-L`/`-D` port forwards
  (`[::1]:8080`). (#109/#105)
  **端口转发的 IPv6 绑定地址加方括号**(`[::1]:8080`),`-L`/`-D` 现在可用。

## [0.4.6] - 2026-06-14

### Fixed / 修复

- **Session-sync upload now works for drag-and-drop too.** Dropping a file onto
  the SFTP panel used a separate code path that skipped the session-sync mirror;
  now both the upload button and drag-and-drop mirror the file to every other
  online session, each into its own current SFTP directory. (Removed the
  temporary upload diagnostics added in 0.4.5.)
  **会话同步上传现在对「拖拽」也生效。** 拖文件到 SFTP 面板走的是另一条代码路径,
  之前漏掉了会话同步;现在上传按钮和拖拽都会把文件同步到其他在线会话(各进各自
  当前目录)。(移除了 0.4.5 加的临时上传诊断日志。)

## [0.4.5] - 2026-06-14

### Fixed / 修复

- **Session-sync upload now targets each session's own current directory.**
  Uploading from one session no longer reuses that session's path for the others
  (which failed when paths differed, e.g. /home/jeff vs /home/root); each session
  receives the file in its own current SFTP directory. (Includes temporary
  diagnostics to nail down a remaining report.)
  **会话同步上传改为各会话用自己的当前目录。** 从某会话上传不再把它的路径套用到
  其他会话(路径不同就会失败,如 /home/jeff 与 /home/root);每个会话都收到文件到
  它自己当前的 SFTP 目录。(含临时诊断日志以定位残留问题。)

## [0.4.4] - 2026-06-14

### Added / 新增

- **Session sync / broadcast input.** A new ⟳ toggle in the top-right bar
  mirrors keystrokes typed in any terminal to every online session
  (Xshell-style). Off by default, runtime-only. Settings → Session sync also
  adds "Sync file uploads during session sync": an upload is mirrored to the
  same path on each session (or that session's current SFTP dir if the path
  doesn't exist there).
  **会话同步 / 广播输入。** 右上角新增 ⟳ 开关,把任意终端里敲的键同步到所有在线
  会话(Xshell 风格);默认关闭、仅本次运行有效。设置 → 会话同步 还有「会话同步时
  文件上传同步」:上传会同步到各会话的相同路径(该路径不存在则用该会话当前 SFTP
  目录)。

- **Tooltips on the top-bar icons** (theme / download / settings / session sync).
  **右上角图标悬停提示**(切换主题 / 下载 / 设置 / 会话同步)。

### Fixed / 修复

- **Light-mode dialogs.** Inputs and buttons in the group / rename / quick-command
  dialogs no longer blend into the background under the light theme — Slint's
  std-widget palette now follows the app theme.
  **浅色模式对话框。** 分组 / 重命名 / 快捷命令等对话框里的输入框和按钮在浅色主题
  下不再与背景融为一体——std-widget 调色板现在跟随应用主题。

- **Empty session groups** now show a collapse chevron and can be expanded /
  collapsed, lining up with non-empty groups.
  **空会话分组** 现在也显示折叠箭头、可展开 / 收起,与非空分组对齐。

## [0.4.3] - 2026-06-14

### Fixed / 修复

- **Wide CJK glyphs are grid-aligned in the terminal.** With a Chinese path, the
  trailing `/` after `ll`, the cursor after `cd`, and the prompt `$` no longer
  overlap or drift away from the last CJK character — each wide character now
  occupies exactly its two terminal cells.
  **终端里的中文(宽字符)对齐到网格。** 中文路径下,`ll` 后目录名的 `/`、`cd`
  之后的光标、提示符 `$` 不再与中文末字重叠或拉开很远——每个宽字符现在正好
  占它的两个终端格。

## [0.4.1] - 2026-06-14

### Added / 新增

- **Run / copy / delete actions on command history (#96).** Each entry in the
  command-history dropdown now has run (▶), copy (⧉) and delete (🗑) buttons —
  run executes it immediately, copy puts it on the clipboard, delete removes it.
  **命令历史的运行 / 复制 / 删除 (#96)。** 历史下拉里每条记录新增 ▶ 运行、
  ⧉ 复制、🗑 删除按钮——运行即时执行,复制到剪贴板,删除移除该条。

- **Default-collapse settings for the sidebars (#78).** Settings → Interface →
  Sidebars adds two checkboxes to collapse the left resource panel and the bottom
  SFTP panel on startup — handy on low-spec jump hosts.
  **侧栏默认收起设置 (#78)。** 设置 → 界面 → 侧栏 新增两个复选框,可在启动时
  收起左侧资源面板和底部 SFTP 面板——适合低配跳板机。

## [0.4.0] - 2026-06-14

### Added / 新增

- **SSH port forwarding / tunnels (#56).** Per-session tunnels configured in the
  session dialog's Advanced section: local (-L), remote (-R) and dynamic
  (-D / SOCKS5). They auto-establish on connect and tear down on disconnect.
  **SSH 端口转发 / 隧道 (#56)。** 在会话对话框「高级」里按会话配置:本地 -L、
  远程 -R、动态 -D（SOCKS5）。连接时自动建立,断开时拆除。

- **Quick commands, command box & history (#55).** A command bar below the
  terminal: save named commands and click to run them, type into a command box
  (with an "all sessions" broadcast toggle), and recall history with ↑/↓.
  **快捷命令、命令输入框与历史 (#55)。** 终端下方的命令栏:保存命名命令点击即发、
  命令输入框（含「所有会话」群发开关）、↑/↓ 回溯历史。

- **Remote process monitor (#23).** The server-resource panel gains a "Processes"
  button that opens a read-only table (PID / user / CPU% / MEM% / command), sorted
  by CPU and refreshed live.
  **远端进程监控 (#23)。** 服务器资源面板新增「进程」按钮,打开只读进程表
  （PID / 用户 / CPU% / 内存% / 命令）,按 CPU 排序、实时刷新。

- **Encrypted private keys (#90).** Key auth now accepts a passphrase for
  encrypted private keys.
  **加密私钥 (#90)。** 私钥认证支持为加密私钥输入密码短语。

### Fixed / 修复

- **CJK rendering (#54).** Chinese (especially isolated punctuation like 、：（）)
  no longer renders as tofu in editable inputs or the terminal — editable fields
  and the window now use a CJK-capable font, and CJK terminal spans fall back
  correctly.
  **中文渲染 (#54)。** 输入框和终端里的中文（尤其「、：（）」这类孤立标点）不再
  显示为方块——可编辑控件与窗口改用支持 CJK 的字体,终端的 CJK 片段也正确回退。

- **SFTP cd-follow under zsh (#91).** The SFTP panel now follows `cd` under zsh
  (and other shells), not just bash — the cwd notification is registered via the
  shell's proper hook instead of bash's `PROMPT_COMMAND` only.
  **zsh 下 SFTP 跟随 cd (#91)。** SFTP 面板现在在 zsh（及其它 shell）下也能跟随
  `cd`,不再只支持 bash——按各 shell 正确的钩子注册 cwd 通知。

- **Compact, scrollable session dialog.** Proxy and port-forwarding settings are
  collapsed under an "Advanced" toggle, and the dialog scrolls instead of being
  clipped when it would exceed the window.
  **会话对话框更紧凑、可滚动。** 代理与端口转发收进「高级」折叠;对话框超出窗口
  时内部滚动而非被截断。

## [0.3.8] - 2026-06-12

### Added / 新增

- **Confirm before closing when there are active sessions (#88).** Double-clicking
  the title-bar icon (or X / Alt+F4) no longer silently drops live sessions — a
  confirm dialog appears; with no sessions the window closes as before.
  **有活动会话时关闭前先确认 (#88)。** 双击标题栏图标(或点 X / Alt+F4)不再静默
  断开正在进行的会话——会弹出确认框;没有会话时则照旧直接关闭。

- **"Always ask where to save" download option (#87).** Settings → Interface →
  Download adds a checkbox (default off); when on, every download prompts for the
  folder instead of using the preset.
  **「总是询问保存去何处」下载选项 (#87)。** 设置 → 界面 → 下载 新增复选框
  (默认关闭);勾选后每次下载都询问保存位置,而非直接用预设目录。

- **The transfers popup opens automatically when a download starts**, so progress
  is visible without opening it by hand.
  **下载开始时自动弹出传输面板**,无需手动打开即可看到进度。

- **Capped diagnostic log file (groundwork for #86).** Writes to
  `<config_dir>/error.log` at WARN and above — a single file capped at 5 MiB that
  auto-overwrites when full — so users can share what went wrong (e.g. a bastion
  disconnect reason) without setting RUST_LOG.
  **容量受限的诊断日志文件(为 #86 铺路)。** 写入 `<配置目录>/error.log`
  (WARN 及以上)——单文件、上限 5 MiB、满了自动覆盖——用户无需设置 RUST_LOG 即可
  把出错信息(如堡垒机断开原因)发来。

### Fixed / 修复

- **Settings checkboxes now persist visually after reopening the dialog (#87).**
  "Always ask where to save" and "SFTP follows cd" used a one-way binding, so
  reopening the settings dialog (without restarting) showed the stale state even
  though the value was saved; switched to a two-way binding.
  **设置里的复选框重新打开对话框后状态保持 (#87)。** 「总是询问保存去何处」和
  「SFTP 跟随 cd」原用单向绑定,不重启 app 时重开设置对话框会显示旧状态(尽管值
  已保存);改为双向绑定。

## [0.3.7] - 2026-06-12

### Added / 新增

- **Right-click anywhere in the SFTP file list (#84).** The list fills the panel
  height, so right-clicking the whitespace below the items works too; item
  actions grey out when nothing was hit, leaving new folder / new file / refresh.
  **SFTP 文件列表任意处可右键 (#84)。** 列表填满面板高度,条目下方空白也能右键;
  未点中条目时,条目相关操作置灰,仅保留 新建文件夹 / 新建文件 / 刷新。

- **Visual permissions dialog (#84).** Permissions opens a checkbox matrix
  (Owner/Group/Other × Read/Write/Execute) prefilled from the file's current
  mode, instead of typing an octal string.
  **可视化权限对话框 (#84)。** 「权限」打开勾选矩阵(所有者/组/其他 × 读取/写入/
  执行),按文件当前权限预填,不再手输八进制。

- **Open / Edit externally (#81).** New SFTP context-menu items hand the file to
  the OS default app (e.g. VS Code) for syntax highlighting / large files; edit
  mode watches the temp copy and re-uploads on change.
  **外部程序查看 / 编辑 (#81)。** SFTP 右键新增菜单项,把文件交给系统默认程序
  (如 VS Code)打开以获得语法高亮 / 处理大文件;编辑模式监听临时副本并自动重传。

- **Line numbers in the built-in editor (#81).**
  **内置编辑器加行号 (#81)。**

- **Upload a whole folder from the Upload button (#85).** The button now offers
  "Upload file" (multi-select) / "Upload folder".
  **上传按钮支持整个文件夹 (#85)。** 现在提供「上传文件」(可多选) /「上传文件夹」。

- **Linux ARM64 release build (#82)** and **AUR packaging scaffolding (#61).**
  **Linux ARM64 发布构建 (#82)** 与 **AUR 打包脚手架 (#61)。**

### Changed / 变更

- **Downloads default to the user's Downloads folder (#85)** instead of prompting
  every time; the settings button reads "Choose save path".
  **下载目录默认设为用户的「下载」文件夹 (#85)**,不再每次询问;设置按钮文案改为
  「选择保存路径」。

### Fixed / 修复

- **No more error under fish (#71).** The OSC 7 prompt injection is guarded with
  `test -z "$FISH_VERSION"`, so it's a no-op under fish (which emits OSC 7 on its
  own, keeping cd-follow working) and unchanged under bash/zsh/sh.
  **fish 下不再报错 (#71)。** OSC 7 提示符注入加了 `test -z "$FISH_VERSION"` 守卫,
  在 fish 下为空操作(fish 自带 OSC 7,cd 跟随照常),bash/zsh/sh 行为不变。

## [0.3.5] - 2026-06-12
- 我和claude都快冒烟了，不想写，让我摆烂一会吧。Claude and I are both about to explode with frustration. We don't want to write anymore. Let me just laze around for a while.

## [0.3.3] - 2026-06-11

### Added / 新增

- **In-app new-version notification (#48).** On startup a background thread
  checks the GitHub releases API; if a newer version exists, a dismissible
  top-centre banner offers a Download button (opens the release page). Purely
  informational — the app keeps working on the current version, never forced.
  **应用内新版本通知 (#48)。** 启动时后台线程查询 GitHub releases API,若有更
  新版本则在顶部居中显示一个可关闭的横幅,带"下载"按钮(打开 release 页)。纯
  提示性质——应用照常在当前版本运行,绝不强制更新。

- **Editable SFTP path bar with copy & paste-to-jump (#54).** The path bar is
  now an input: type or paste a path and press Enter to jump there, plus a copy
  button and a paste-and-jump button.
  **可编辑的 SFTP 路径栏,支持复制和粘贴跳转 (#54)。** 路径栏现在是输入框:输入
  或粘贴路径后回车即跳转,另有复制按钮和粘贴跳转按钮。

### Fixed / 修复

- **SFTP panel no longer gets stuck "loading" after manual navigation (#59).**
  The panel only follows a real cd now (cwd actually changed), not the OSC 7 that
  every prompt re-emits — so manual browsing isn't overridden and a later cd
  reloads correctly instead of hanging.
  **SFTP 面板手动导航后不再卡"加载中" (#59)。** 面板现在只跟随真正的 cd(cwd 确
  实变化),而非每个命令提示符都重发的 OSC 7——手动浏览不被覆盖,之后的 cd 也能
  正确重新加载而非卡住。

- **SFTP path bar renders Chinese instead of tofu (#54).** The embedded Cascadia
  Mono has no CJK glyphs and native TextInput doesn't glyph-fallback; editable
  inputs now use a CJK-capable system font.
  **SFTP 路径栏正常显示中文,不再是豆腐块 (#54)。** 嵌入的 Cascadia Mono 没有
  CJK 字形,而原生 TextInput 不做字形回退;可编辑输入现改用含 CJK 的系统字体。

## [0.3.2] - 2026-06-11

### Added / 新增

- **MSI installer for Windows (best-effort).** Built by cargo-wix with a
  WixUI_InstallDir wizard, so you can change the install location during setup.
  **Windows MSI 安装包(尽力而为)。** 由 cargo-wix 构建,带 WixUI_InstallDir
  向导,安装时可更改安装位置。

- **Explicit session folders (#41).** Groups are now first-class: create / rename
  / delete them, keep empty folders, and right-click a group header to manage it.
  Right-click a session to "move to" any group (incl. empty ones).
  **显式会话文件夹 (#41)。** 分组成为一等公民:可新建 / 重命名 / 删除,可保留空
  文件夹,右键分组标题进行管理;右键会话可"移动到"任意分组(含空文件夹)。

### Changed / 变更

- **Interface settings dialog** is now draggable (by its title bar), truly modal
  (background click no longer closes it), and its font controls no longer span
  the full pane.
  **「界面」设置对话框**现在可拖动(拖标题栏)、真模态(点背景不再关闭),字体控件
  也不再占满整个面板。

- **Session right-click menu reordered:** Edit / Duplicate / Delete above the
  divider, the "move to group" list below it with a header hint.
  **会话右键菜单重排:** 编辑 / 复制副本 / 删除在分割线上方,"移动到分组"列表带
  提示在下方。

### Fixed / 修复

- **Wide (CJK) characters no longer misalign the cursor (#60).** A wide glyph's
  blank continuation cell was being filled with a space, pushing the line and
  cursor one cell right per character; it now emits nothing.
  **宽(CJK)字符不再导致光标错位 (#60)。** 宽字形的空白延续格此前被补了空格,
  每个字符把行和光标右推一格;现在延续格不输出任何内容。

- **Download & settings popups close on click-outside.** A full-window backdrop
  under each popup closes it when you click outside.
  **下载/设置弹窗点击外部即关闭。** 每个弹窗下方铺一层全窗背景,点击外部即关闭。

- **Disk tooltip clears when the pointer leaves the panel**, and the OSC 7
  shell-integration command no longer leaves an extra blank prompt on connect.
  **磁盘 tooltip 在指针离开面板时消失**,OSC 7 shell 集成命令也不再在连接时留下
  多余的空提示符。

## [0.3.1] - 2026-06-10

### Security / 安全

- **Restrict sessions.json to owner-only on Unix (#34).** The config file holds
  (encrypted) credentials, so it is now written with mode 0600 — like
  secret.key — and other local accounts can't read it. Windows %APPDATA% is
  already owner-restricted by default ACLs.
  **将 sessions.json 限制为仅属主可读(Unix)(#34)。** 配置文件含(加密的)凭据,
  现在以 0600 权限写入(与 secret.key 一致),其它本地账户无法读取。Windows 的
  %APPDATA% 默认 ACL 已限制为属主。

### Build / 构建

- **Build macos-x86_64 by cross-compiling on Apple Silicon runners.** The
  dedicated Intel (macos-13) runners queue for ages and often time out, so the
  x86_64 Mac binary is now cross-compiled on a plentiful macos-14 runner.
  **在 Apple Silicon runner 上交叉编译 macos-x86_64。** 专用 Intel(macos-13)
  runner 排队极久且常超时,x86_64 Mac 二进制改为在充足的 macos-14 runner 上
  交叉编译。

## [0.3.0] - 2026-06-10

### Added / 新增

- **Interface settings — terminal font & size.** The gear menu's new "Interface"
  item opens a modal dialog (27% nav / 73% content) whose Font page lets you pick
  from the system's installed monospace fonts and set the size (8–32 px) with a
  live preview. Both apply immediately (cell size / cols / rows re-derive) and
  persist.
  **「界面」设置 —— 终端字体与字号。** 齿轮菜单新增「界面」项,打开模态对话框
  (27% 导航 / 73% 内容),「字体」页可从系统已安装的等宽字体中选择并设置字号
  (8–32 px),带实时预览。两者即时生效(cell 尺寸 / 列 / 行重新推导)并持久化。

- **Session folders / groups (#41).** Each session can belong to an optional
  group; Quick Connect shows folder headings (sorted; ungrouped under "default")
  that collapse when clicked. Right-click a session to move it to another group
  or duplicate it; right-click a group header to create a new group.
  **会话文件夹 / 分组 (#41)。** 每个会话可归入可选分组;「快速连接」按分组显示
  文件夹标题(排序;未分组归入「default」),点击标题可折叠。右键会话可移动到其它
  分组或复制副本;右键分组标题可新建分组。

- **Recursive SFTP folder transfer (#50).** Upload, download and delete whole
  directory trees: drag a folder onto the panel to upload, right-click a folder
  to download, and delete now removes non-empty directories too.
  **SFTP 文件夹递归传输 (#50)。** 可上传、下载、删除整个目录树:把文件夹拖到面板
  上传,右键文件夹下载,删除现在也能删非空目录。

- **Collapsible panels (#41).** Both the left sidebar and the SFTP panel can be
  minimized to reclaim screen space.
  **可折叠面板 (#41)。** 左侧栏与 SFTP 面板都可最小化以腾出屏幕空间。

- **Export / import connections (#46).** New "Export connections" / "Import
  connections" in the settings menu to migrate sessions between machines. The
  exported JSON keeps host/user/port in plaintext and obfuscates only the
  password with a built-in key, so it opens on any machine; key-auth sessions
  export the key path. Imports skip duplicates (host+user+port+kind).
  **导出 / 导入连接 (#46)。** 设置菜单新增「导出连接」「导入连接」,用于在多台
  机器间迁移会话。导出的 JSON 中 host/user/port 为明文,仅密码用内置 key 混淆,
  因此在任意机器都能打开;密钥认证的会话导出私钥路径。导入会跳过重复
  (host+user+port+kind)。

- **Pick SOCKS5 or HTTP proxy type in the session dialog (#46).** The proxy
  field is now a None / SOCKS5 / HTTP selector plus a `host:port` input. HTTP
  CONNECT was already supported by the backend but wasn't selectable in the UI.
  The stored proxy URL format is unchanged.
  **会话对话框选择 SOCKS5 或 HTTP 代理类型 (#46)。** 代理项改为 不使用 / SOCKS5 /
  HTTP 选择器加 `host:port` 输入框。HTTP CONNECT 后端早已支持,只是 UI 无法选择。
  存储的代理 URL 格式不变。

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

- **Drag-select no longer auto-scrolls within the visible area (#41).** Selecting
  text now only scrolls once the drag leaves the viewport edge, so the view no
  longer jumps and the selection no longer snaps to an edge row.
  **拖动选择在可见区内不再自动滚动 (#41)。** 现在仅当拖动离开视口边缘才滚动,
  视图不再乱跳、选区也不再吸附到边缘行。

- **Alt no longer clears the typed command (#43).** Slint encodes a lone
  modifier key as a C0 code point (Alt=0x12); pressing Alt (e.g. to Alt+Tab
  away) sent ESC+0x12 to the PTY, which bash/readline treated as Meta and
  discarded the input line. Bare modifier codes are now dropped, with a guard
  that preserves a real Ctrl+P..Ctrl+X.
  **按 Alt 不再清空已输入命令 (#43)。** Slint 把单独的修饰键编码成 C0 码位
  (Alt=0x12);按 Alt(如 Alt+Tab 切换)会向 PTY 发送 ESC+0x12,被 bash/readline
  当作 Meta 而丢弃输入行。现在丢弃单独的修饰键码位,并保留真实的 Ctrl+P..Ctrl+X。

- **Multi-line / backslash-continued commands now paste intact.** Pasted text
  kept its CRLF/LF line breaks, but the terminal expects CR for Enter, so CRLF
  made the shell see two breaks per line and end a `\`-continued command early.
  Pasted line endings are normalised to a single CR.
  **多行 / 反斜杠续行命令现在能完整粘贴。** 粘贴文本保留了 CRLF/LF 换行,而终端
  回车应为 CR,CRLF 会让 shell 每行看到两个换行、提前结束 `\` 续行命令。现在把
  粘贴的换行统一规范为单个 CR。

- **Session dialog no longer mis-lays-out when switching connection type.** The
  card had a fixed height, so Telnet/Serial (with fewer fields) left slack space
  that stretched inputs apart. The card height now follows its content.
  **切换连接类型时会话对话框不再排版错乱。** 卡片此前固定高度,Telnet/串口
  (字段更少)会留出空白把输入框撑开。现在卡片高度跟随内容。

- **Copy/paste works on Wayland sessions (#47).** arboard's default Linux
  backend is X11, which fails on Wayland (Debian sid / KDE) without XWayland.
  Enabled arboard's native `wayland-data-control` backend, and copy now uses
  set().wait() so the selection survives after the clipboard handle is dropped.
  **Wayland 会话下复制粘贴恢复可用 (#47)。** arboard 默认 Linux 后端是 X11,在
  无 XWayland 的 Wayland(Debian sid / KDE)下失效。启用 arboard 原生
  `wayland-data-control` 后端,复制改用 set().wait() 使选区在剪贴板句柄 drop 后
  仍然有效。

- **Hide the shell-integration command from the terminal.** meatshell injects a
  one-line `PROMPT_COMMAND` (OSC 7) on connect so the SFTP panel can follow the
  terminal's working directory. Its echo used to show up on every connect (and
  pollute shell history); the line now carries a leading space (kept out of
  history) and its echo is stripped from the output before display.
  **隐藏 shell 集成注入命令。** meatshell 连接时会注入一行 `PROMPT_COMMAND`
  (OSC 7),让 SFTP 面板跟随终端当前目录。此前它的回显每次连接都显示在终端
  (并污染命令历史);现在该行带前导空格(不进历史),回显也会在显示前被剥离。

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

- **Redact proxy credentials and zero them in memory (#32).** The HTTP/SOCKS
  proxy password is now wrapped in `Secret` (zeroed on drop) and ProxyConfig has
  a manual Debug that redacts auth, so credentials can't leak via {:?}/tracing
  or linger in core dumps.
  **代理凭据脱敏并在内存清零 (#32)。** HTTP/SOCKS 代理密码改用 `Secret` 包装
  (drop 时清零),ProxyConfig 手写 Debug 对凭据脱敏,使其无法经 {:?}/tracing
  泄露或残留于 core dump。

- **Validate HostName when importing ~/.ssh/config (#33).** Imported HostName
  values are now checked — IP literals and DNS hostnames accepted; shell
  metacharacters, whitespace and scheme prefixes rejected — and invalid entries
  are skipped with a warning.
  **导入 ~/.ssh/config 时校验 HostName (#33)。** 现在校验导入的 HostName(接受
  IP 字面量与 DNS 域名,拒绝 shell 元字符、空白与协议前缀),非法条目跳过并告警。

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

[0.3.8]: https://github.com/jeff141/meatshell/releases/tag/v0.3.8
[0.3.7]: https://github.com/jeff141/meatshell/releases/tag/v0.3.7
[0.3.3]: https://github.com/jeff141/meatshell/releases/tag/v0.3.3
[0.3.2]: https://github.com/jeff141/meatshell/releases/tag/v0.3.2
[0.3.1]: https://github.com/jeff141/meatshell/releases/tag/v0.3.1
[0.3.0]: https://github.com/jeff141/meatshell/releases/tag/v0.3.0
[0.2.2]: https://github.com/jeff141/meatshell/releases/tag/v0.2.2
