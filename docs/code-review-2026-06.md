# 代码评审报告 — meatshell v0.2.4

日期：2026-06-07
范围：全部 9 个源文件（~5400 行 Rust + build.rs）

---

## 🐛 Bug（影响功能的错误）

### 1. macOS 剪贴板读取命令错误 — `pbread` → `pbpaste`

**文件**: `src/app.rs:333`

```rust
("pbread", &[])
```

`pbread` 不存在。macOS 的剪贴板读取命令是 `pbpaste`（写入是 `pbcopy`）。当前代码导致 macOS 上 `Ctrl+Shift+V` 粘贴总是静默失败，`linux_clipboard_read` 函数会报 `failed to run pbread` 错误。

**修复**: 将 `"pbread"` 改为 `"pbpaste"`。

---

## ⚠️ 设计/代码质量问题

### 2. config store 默认 style 与 build.rs 不一致

**文件**: `src/config.rs:246` vs `build.rs:13`

`ConfigStore::style()` 无配置时返回 `"fluent"`，但 `build.rs` 中 `MEATSHELL_STYLE` 未设置时编译的是 `"native"`。运行时 Slint 根据 `SLINT_STYLE` 环境变量选择 style，如果没有设置则回到代码设置的值。

运行时逻辑：`main.rs:24-32` 设置了 `SLINT_STYLE` 为 config 的值（默认 `"fluent"`），所以运行时行为正确。但 **build 产物默认只包含 `native` style**，除非用 `MEATSHELL_STYLE=fluent` 构建。如果用户用普通 `cargo build`，运行时 config 的 `"fluent"` 会请求一个未编译的 style，Slint 会 fallback。

### 3. `format_size` 以 1024 为进制但输出标签为 KB/MB/GB

**文件**: `src/ssh.rs:50-60`

以 1024 为进制（`value /= 1024.0`），所以实际上是 KiB/MiB/GiB，但标签为 `KB`/`MB`/`GB`。大多数终端工具都这么做（包括 `ls -lh` 等），技术上不正确但不影响功能。

### 4. SSH/SFTP 两个 handler 的 `check_server_key` 重复

**文件**: `src/ssh.rs:696-699`, `src/sftp.rs:829-832`

`ClientHandler` 和 `SftpClientHandler` 都有相同的 `check_server_key` 实现——返回 `Ok(true)`。未来 known_hosts 校验实现时需要修改两处。

### 5. `Session::new_empty()` 默认 user 为 "root"

**文件**: `src/config.rs:116`

新会话默认用户为 `"root"`。简化了常用场景，但如果用户直接创建通用会话可能忘记修改。

---

## 🔶 潜在风险

### 6. 远端资源监控分隔符被远程输出包含

**文件**: `src/ssh.rs:402`

监控命令用 `__MSTICK__` 作为分块分隔符。如果远程系统输出（`/proc/net/dev` 内容、`df` 输出等）恰好包含该字符串，解析会错位。概率极低。

### 7. `ALL_PROXY` vs `all_proxy` 优先级顺序

**文件**: `src/proxy.rs:42-48`

代码先检查 `ALL_PROXY`（大写）再检查 `all_proxy`（小写），这与 `curl`/`wget` 等工具的做法相反。如果设置了冲突的变量，行为与用户预期可能不同。

### 8. 编辑会话时无法清空密码

**文件**: `src/app.rs:1077-1085`

编辑会话时密码字段为空保留旧密码。用户无法通过 UI 删除一个已保存的密码（置空）。

---

## 🧹 代码异味

### 9. macOS 拖放文件上传不支持

**文件**: `src/app.rs:744-894`

`DroppedFile` 事件处理函数被 `#[cfg(not(windows))]` 关闭，Linux 和 macOS 上都不能拖放上传文件。

### 10. SFTP upload 刷新目录使用原有 session

**文件**: `src/sftp.rs:368-372`

上传完成后调用 `list_dir_impl(&sftp, &remote_dir)` 刷新目录列表。使用主 sftp session 是正确的，但上传期间目录可能已变化，导致显示的条目与上传操作的最终结果不完全一致。这是小问题。

---

## 总结

| 分类 | 数量 | 严重程度 |
|------|------|----------|
| 🐛 Bug | 1 | 中（macOS 粘贴静默失败） |
| ⚠️ 设计问题 | 4 | 低 |
| 🔶 潜在风险 | 3 | 低 |
| 🧹 代码异味 | 2 | 低 |

**最值得优先修复**：
1. `pbread` → `pbpaste`（如果 macOS 是目标平台）
2. build.rs 默认 style 同步问题（如果经常用默认构建）
3. 未来 known_hosts 实现时抽象两个 handler 的公共逻辑
