# AGENTS.md — meatshell

Rust + Slint SSH/terminal 客户端（v0.2.4），替代 FinalShell。

## 常用命令

```bash
cargo check                    # 改 .slint 后最快反馈
cargo fmt && cargo check && cargo test   # 本地完整检查
cargo clippy                   # lint
MEATSHELL_STYLE=fluent cargo run          # 指定 UI style 构建
```

> Windows 应用，Linux 只可 `cargo check` + `cargo test`。运行和 CI 在 Windows 上验证。

## 关键约束（新会话易踩坑）

### 🔴 从不调用 ImmDisableIME
`main.rs` 注释详细说明了历史：早期为了修 vim `:q!` 副作用禁用了 IME，结果导致**完全无法输入中文**。当前中文输入通过 `terminal_view.slint` 中隐藏的 `ime-input` TextInput 驱动，vim 副作用由 `app::on_send_key` 的 C0 marker + 3 层 Backspace 过滤器处理。**任何时候都不要加 `ImmDisableIME`。**

### 🔴 跨线程 UI 更新必须走 `slint::invoke_from_event_loop`
Slint 事件循环单线程。tokio 任务中不能直接改 Slint 属性。

### 🟡 `check_server_key` 接受任意服务端密钥
当前等同于 `StrictHostKeyChecking=no`。known_hosts 校验在路线图中。

### 🟡 会话密码明文 JSON 存储
`~/.config/meatshell/sessions.json`，OS 钥匙串集成在路线图中。密码缓冲区使用 `zeroize` crate 在 drop 时清零。

### Linux 构建依赖
CI 需要：`libfontconfig1-dev libfreetype6-dev libxcb1-dev libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libgl1-mesa-dev libegl1-mesa-dev libgtk-3-dev`

## UI Style

```bash
# 构建时（默认 "native"）：
MEATSHELL_STYLE=fluent cargo build
# 运行时覆盖（优先级高于配置存储）：
SLINT_STYLE=fluent ./meatshell
```

默认运行时 style 从 `config::ConfigStore::style()` 读取（初始 "fluent"）。环境变量 `SLINT_STYLE` 优先级最高。

## 国际化（i18n）

两层机制，必须保持同步：

| 层 | 机制 | 适用 |
|---|---|---|
| Static Slint 文本 | `@tr("English")` + `lang/*.po` | UI 标签 |
| Dynamic Rust 文本 | `i18n::t(zh_str, en_str)` | status/error/format! |

源语言 msgid 是**英文**。`lang/` 下含 `en/` 和 `zh/` 两个翻译目录。切换：`slint::select_bundled_translation("zh")` / `i18n::set_language("zh")`。首次 window 创建后需重新调用 `i18n::apply_to_slint()`。

## 架构速览

```
Slint UI (单线程)
  ├── callbacks → SessionHandle::send_raw/resize → SSH channel
  ├── SSH 事件泵 (tokio) → SessionEvent → invoke_from_event_loop → UI
  ├── SFTP worker (tokio, 独立 SSH 连接) → SessionEvent → UI
  ├── 远端资源监控 (SSH exec channel, 每2s)
  └── 1Hz 定时器 → SystemSampler → 侧边栏
```

### 模块

| 文件 | 职责 |
|---|---|
| `src/app.rs` | UI 状态机，标签页/SessionHandle 映射，vt100 TermBuffer |
| `src/ssh.rs` | SSH worker: PTY, 认证, OSC 7 CWD, 远端 /proc 监控 |
| `src/sftp.rs` | SFTP worker: 独立 SSH, 上传/下载/删除/编辑回传 |
| `src/config.rs` | 会话 JSON 持久化, 原子写入 (tmp + rename) |
| `src/system.rs` | 本机 CPU/内存/交换/网络/磁盘 (1Hz) |
| `src/proxy.rs` | SOCKS5 + HTTP CONNECT 代理 (per-session / ALL_PROXY) |
| `src/i18n.rs` | 运行时中英文切换 |
| `src/ssh_config.rs` | `~/.ssh/config` 导入器 |
| `ui/*.slint` | 见 README 项目布局 |

### Build 产物
- 非 debug 模式启用 `#![windows_subsystem = "windows"]`（无控制台窗口）
- release: `lto = "thin"` + `codegen-units = 1` + `strip = "symbols"` + `panic = "abort"`
- Windows 构建嵌入 `assets/meatshell.ico`

## 配置文件路径

| 平台 | 路径 |
|---|---|
| Windows | `%APPDATA%/meatshell/sessions.json` |
| Linux | `~/.config/meatshell/sessions.json` |
| macOS | `~/Library/Application Support/meatshell/sessions.json` |
