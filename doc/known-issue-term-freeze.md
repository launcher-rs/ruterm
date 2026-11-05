# 已知问题：粘贴多行命令后连续退格导致整个应用卡死（未解决）

> 状态：**未解决**。已排除若干方向，仓库里保留了复现钩子与链路追踪开关，供后续定位。
> 最后更新：2026-11-05

## 1. 现象

1. 通过 SSH 连上一台 Linux 主机（bash 交互 shell）；
2. 粘贴一段多行命令（bracketed paste，一次性进入输入缓冲、不执行）：

```
curl https://openrouter.ai/api/v1/chat/completions \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer sk-or-v1-<很长的 token>" \
  -d '{
    "model": "anthropic/claude-3.5-sonnet",
    "messages": [{"role": "user", "content": "How many r`s are in the word `strawberry?`"}]
  }'
```

3. 不想执行，改用 Backspace 从末尾往前删；
4. 删到大约只剩 `"content": "How many r`s a` 时，**整个 RuTerm 窗口无响应**：
   不重绘、按键无反应，只能杀进程。

用户描述这是「原来就存在的 bug」，与右键菜单 / Ctrl+V 改动无关；卡死点看起来与
**剩余文本的某个位置**相关，偏确定性，不像纯随机竞态。

## 2. 环境

- Windows 10 x64；RuTerm（`D:\code\ruterm`），GUI 框架 rgpui 本地源码（`D:\code\rgpui`）
- 终端：rgpui-term（外接 IO 后端） + `alacritty_terminal 0.26.0`
- `Cargo.toml` 的 `[patch.crates-io]` 把 `rgpui / rgpui-platform / rgpui-term`
  指向 `../rgpui/crates/*`：发布版 1.6.0 缺少源码里的若干修复
- 复现用 Docker 测试机：`ruterm-test-ssh`，`localhost:2222`，用户 `test`，密码 `test123`
  （Alpine + bash 5.3.9，终端尺寸约 75×35）

## 3. 已修复 / 已排除

### 3.1 rgpui-term 解析线程 lease 自锁（源码已修，不是本问题的原因）

`crates/rgpui-term/src/external.rs:216-224`：旧代码先 `terminal.lease()`（拿到 `next`），
`try_lock_unfair()` 失败时退回 `terminal.lock()`；而 `lock()` 会再次锁 `next`，
parking_lot 不可重入 → 解析线程永久自锁，之后 UI 线程任何 fair `lock()` 都会永久阻塞。

当前源码已改为 `lock_unfair()`，并有回归测试 `feed_survives_contention_with_ui_lock`
（`external.rs:398`）。用「UI 强占 data 锁 250ms + 连续喂 30 批字节」验证过该路径不再死锁。
切到本地源码后，用户在同类场景仍会卡死，**说明还有别的原因**。

### 3.2 rgpui-windows activate 误销毁窗口（已修）

`Platform::activate/hide` 曾用临时 `WindowsWindow(inner)` 包装调用，而
`impl Drop for WindowsWindow` 会 `DestroyWindow`，导致窗口一激活就消失。
已把逻辑移到 `WindowsWindowInner::{activate_window, hide_window}`，平台层只拿 inner 调用
（rgpui 提交 `d33ef7c5`）。

### 3.3 本地复现尝试（均未触发卡死）

- 内部 smoke 钩子（见 §4）通过真实按键分发路径 `WindowHandle::update →
  window.dispatch_keystroke(Keystroke::parse("backspace"))` 连发 400 次（8ms 间隔）；
  截图确认文本确实进入终端并逐步删除，进程无卡顿。
- 使用与用户接近的 payload（含长 token，强制超过 75 列换行）：仍不卡。
- 直接调 `Terminal::input_with_origin`（绕过按键分发）在旧版 crate 上也不卡：
  该路径不经过 focus/dispatch/draw，不能作为复现手段。

### 3.4 尚未覆盖的场景

- 终端里先有大量输出（shell 提示符在**屏幕最后一行**）再粘贴、删除；
- 远端回显有明显 RTT（本机 Docker 往返 <1ms，用户是局域网/远端主机）；
- 真实 Windows 消息路径（自动重复、`is_held`、IME input handler）；
- release 构建下的时序差异。

## 4. 仓库内的诊断设施（均为环境变量开关，发布前应删除）

| 变量 | 位置 | 作用 |
| --- | --- | --- |
| `RUTERM_SMOKE_BACKSPACE=<n>` | `src/app/connect.rs:487` | 连上 6s 后自动粘贴 payload，再经真实按键路径发 n 次 Backspace；`0` 则只粘贴 |
| `RUTERM_SMOKE_DELAY_MS=<ms>` | 同上 | 每次按键间隔，默认 8ms |
| `RUTERM_SMOKE_PREFILL=<n>` | 同上 | 粘贴前先执行 `seq 1 <n>`，把提示符顶到屏幕底部（**新增，尚未编译验证**） |
| `RUTERM_TEST_DOWNLINK_DELAY_MS=<ms>` | `src/ssh.rs:411` | 在 SSH 下行回调里人为加延迟，模拟远端 RTT（**新增，尚未编译验证**） |
| `RUTERM_TRACE_EXTERNAL=1` | `src/session.rs:300,1326-1410` | 打印上行泵 / 输入合批 / resize 的收发日志 |

另有一处「上行输入合批」（`src/session.rs:1335` 附近）：把连续的多条
`PtyBoundMessage::Input` 合并到 64KB 再写 SSH 通道，byte 顺序不变。
**动机是怀疑每次按键单独写 SSH 会塞满通道队列，但该改动的收益未被证实**，
保留在此供后续参考；若不打算继续这条线，可以整段回退。

复现命令：

```bash
taskkill //IM ruterm.exe //F
RUTERM_PASSWORD=test123 \
RUTERM_SMOKE_BACKSPACE=300 RUTERM_SMOKE_PREFILL=100 RUTERM_TEST_DOWNLINK_DELAY_MS=30 \
RUTERM_TRACE_EXTERNAL=1 \
./target/debug/ruterm.exe test@localhost:2222 > target/freeze.log 2>&1
```

截图（DPI 感知）：`powershell -NoProfile -ExecutionPolicy Bypass -File target/shot_ruterm.ps1`
→ `target/ruterm_shot.png`。单实例锁占用 TCP 47122，重跑前先结束旧进程。

## 5. 关键代码路径与锁清单

UI 线程会阻塞在 `FairMutex<Term>` 上的位置：

- 每帧 prepaint：`crates/rgpui-term/src/terminal_element.rs:1480-1485`
  → `Terminal::set_size` + `Terminal::sync`；`sync` 用 `lock_unfair()`
  （`crates/rgpui-term/src/terminal.rs:882-903`），**只等 data 锁**。
  如果解析线程在 `parser.advance()` 里不返回，UI 会永久等下去 → 表现就是整个窗口卡死。
- 公平锁（先 `next` 再 `data`）：`input_with_origin` 的 Enter 分支
  （`terminal.rs:805-813`）、`select_all` / `get_all_text` / `hyperlink_at` /
  `ColorRequest` / `CursorBlinkingChange`（`terminal.rs:992,1113,1138,1448`）。
  `sync` 持 data 期间还会 `cx.emit(...)`（Wakeup/TitleChanged/SelectionsChanged），
  订阅者若再取公平锁，会与「解析线程持 next 等 data」互等。
- 解析线程：`external.rs:154-246`（lease + `lock_unfair`；同步超时分支
  `external.rs:172` 仍是 `terminal.lock()`）。

应用侧订阅：`src/app/tabs.rs:384`（TitleChanged/SearchUpdated，不取锁）、
`src/app/connect.rs:454`（CloseTerminal，收尾会话）。

## 6. 未排除的假设（按优先级）

1. **解析线程卡在 alacritty 解析器内部**（死循环或长阻塞），UI 因此在每帧 `sync`
   的 `lock_unfair` 上永久等待。触发字节流可能来自特定 shell（zsh/fish/带高亮插件）
   在换行边界重绘时的光标/擦除序列。
2. **公平锁互等**：`sync` 持 data 期间经 `cx.emit` 进入回调，回调再取公平锁；
   与解析线程的 lease 组合成环。需审计所有订阅者与 middleware 的 `on_event`。
3. **每帧 `set_size + sync` 的 resize 反馈环**：宽度/滚动历史变化导致
   Resize → reflow → 再次 Resize，表现为持续高 CPU（活锁）而非 0% 死锁。
4. **平台按键路径差异**：真实 Windows 消息带自动重复（`is_held`）、
   `prefer_character_input`，字符键可能走 `InputHandler::replace_text_in_range`
   （`terminal_element.rs:1942`）；smoke 钩子未覆盖这些分支。
5. **上行写阻塞**（ChatGPT 方向）：`forward_uplink` 的 `channel.data_bytes().await`
   若长时间挂起，会让上行泵积压，但理论上不阻塞 UI 线程；需要实测排除。

## 7. 建议的下一步

1. 卡死时抓**全线程栈**（最快定性）：
   ```
   procdump -ma <PID> target\freeze.dmp
   ```
   或用 Process Explorer 右键 → Create dump → 查看 UI 线程与
   `rgpui-term-external` 线程的栈。看 UI 是停在 `sync` 的 `lock_unfair`、
   实体 update、还是 draw/search。
2. 记录卡死时 CPU 占用：0% → 真死锁；100% → 活锁/死循环。
3. 若栈指向解析线程，在 `FeedHandle::feed` 处把下行字节落盘，拿到用户环境的
   原始回显流后用单测重放（可加超时断言），定位具体 CSI 序列。
4. 若要继续本地复现：先补测 §3.4 的「满屏输出 + 链路延迟 + 真实 Windows 消息」；
   再尝试用户所在 shell（zsh + 主题）在容器里运行。
