# 已知问题：粘贴多行命令后连续退格导致终端停摆（已解决）

> 状态：**已解决**（2026-11-05）。
> 根因是 russh 会话通道的**有界接收队列没人消费**，与 rgpui-term 的锁、
> alacritty 解析器无关。修复在 `src/session.rs`：终端通道只保留写半边，
> 读半边直接丢弃。

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
4. 删到大约只剩 `"content": "How many r`s a` 时，终端不再刷新，按键也无反应。

本机复现确认：此时 **RuTerm 进程本身仍然响应**（`Responding=True`、CPU 低），
停掉的是这条 SSH 会话——既收不到下行字节，也发不出上行按键。用户只看终端时
的感受就是「整个窗口卡死」。

## 2. 本地复现

用仓库里的 smoke 钩子（§6）连接 Docker 测试机，按真实按键分发路径连发 Backspace：

```bash
taskkill //IM ruterm.exe //F
RUTERM_PASSWORD=test123 \
RUTERM_SMOKE_BACKSPACE=340 RUTERM_SMOKE_PREFILL=200 RUTERM_TEST_DOWNLINK_DELAY_MS=30 \
RUTERM_TRACE_EXTERNAL=1 \
./target/debug/ruterm.exe root@localhost:2223 > target/freeze.log 2>&1
```

修复前实测：

- `RUTERM_TRACE_EXTERNAL` 的下行计数停在 **98 个 chunk**；
- 最后一条上行日志是 `uplink input batch ending at #179 ... write start`，
  没有对应的 `write done`；后续 Backspace 仍被 UI 接收，但全部堆在通道里；
- 远端 shell 停在 `ppoll` 空闲状态，容器内 TCP `Recv-Q/Send-Q` 都是 0；
- 结论：两端都没在干活，是**客户端 SSH 会话任务自己堵死**，不是远端流控、
  不是 UI 死锁、不是解析器死循环。

## 3. 根因

russh 0.64.1 处理 `CHANNEL_DATA` 时（`client/encrypted.rs`）：

```rust
if let Some(chan) = self.channels.get(&channel_num) {
    // 把每条下行报文复制一份投递进通道的“应用侧”有界接收队列
    let _ = chan.send(ChannelMsg::Data { data: data.clone() }).await;
}
client.data(channel_num, &data, self).await
```

`channel_open_session` 创建的接收队列容量是 `Config::channel_buffer_size`，
默认 **100** 条。RuTerm 的下行一直走 `Handler::data`（`src/ssh.rs` →
`FeedHandle`），终端通道的 `ChannelReadHalf` 从未被 `wait()` / 读取，
于是第 100 条报文到达后：

- 会话任务永久阻塞在 `chan.send(...).await`；
- 不再投递下行 → 终端不再刷新；
- 不再消费上行消息队列 → `Channel::data_bytes` 跟着挂起 → 按键发不出去；
- rgpui UI 线程不参与这条链路，所以窗口本身仍可响应。

「卡死点与剩余文本位置相关」是假象：真正的阈值是**下行报文条数**。逐字退格时
bash 回显大量 1～5 字节的小包，删到那个位置时正好把 100 条配额用尽。

## 4. 修复

```rust
// src/session.rs（开 shell 之后）
let (channel_read, channel) = channel.split();
drop(channel_read);

tokio::spawn(forward_uplink(channel, ui_rx, raw_rx, cwd_hint_tx));
```

读半边关闭后，russh 的 `send` 立即返回错误（调用处本来就是 `let _ =`），
会话任务不再阻塞；下行照旧由 `Handler::data` 投递，EOF / Close / ExitStatus
也仍由 handler 回调处理，行为不变。

## 5. 验证

| 场景 | 修复前 | 修复后 |
| --- | --- | --- |
| `seq 1 200` + 340 次退格 | 98 个下行 chunk 后停摆 | 131 chunk、344 条上行全部 `write done` |
| bash 5.3.9 + `seq 1 2000` + 340 次退格 | — | 528 chunk、342 批次全部完成 |
| bash 5.3.9 + `seq 1 5000` + 340 次退格 | — | 1137 chunk、342 批次全部完成，进程 `Responding=True` |
| `cargo test --bin ruterm` | — | 122 passed |

修复后同一套操作可以持续产生上千个下行报文，不再有 100 条上限，也没有积压。

## 6. 复现与诊断设施（保留，发布前删除）

| 变量 | 位置 | 作用 |
| --- | --- | --- |
| `RUTERM_SMOKE_BACKSPACE=<n>` | `src/app/connect.rs` | 连上 6s 后自动粘贴 payload，再经真实按键路径发 n 次 Backspace；`0` 则只粘贴 |
| `RUTERM_SMOKE_DELAY_MS=<ms>` | 同上 | 每次按键间隔，默认 8ms |
| `RUTERM_SMOKE_PREFILL=<n>` | 同上 | 粘贴前先执行 `seq 1 <n>`，把提示符顶到屏幕底部、制造大量下行报文 |
| `RUTERM_TEST_DOWNLINK_DELAY_MS=<ms>` | `src/ssh.rs` | 在 SSH 下行回调里人为加延迟，拉大按键与回显的交叠窗口 |
| `RUTERM_TRACE_EXTERNAL=1` | `src/session.rs` / `src/ssh.rs` | 打印上行泵、输入合批、resize、下行字节与解析批次日志 |

复现命令见 §2；截图脚本：`target/shot_ruterm.ps1` → `target/ruterm_shot.png`。
单实例锁占用 TCP 47122，重跑前先结束旧进程。

另有一处「上行输入合批」（`src/session.rs` 的 `forward_uplink`）：把连续的多条
`PtyBoundMessage::Input` 合并到 64KB 再写通道。它和本问题无关，但能减少上行消息
数量，保留即可。

## 7. 历史排查记录（均已排除）

- **rgpui-term 解析线程 lease 自锁**：`external.rs` 里持 `next` 再走 `lock()`
  会自锁；已改为 `lock_unfair()` 并补了回归测试 `feed_survives_contention_with_ui_lock`。
- **rgpui-windows activate 误销毁窗口**：已修（rgpui `d33ef7c5`）。
- **`Terminal::sync` 等锁卡 UI**：当前实现用 `try_lock_unfair` 拿不到就跳过本帧，
  不会阻塞；应用侧订阅者与中间件也不取公平锁。
- **`Entity::read` 重入**：`TitleChanged` 订阅读取终端实体在 update 期间会 panic
  （double lease），不会静默阻塞。
- **alacritty 解析器死循环 / 上行写阻塞 / 远端流控**：本地抓到的下行字节只有
  普通 CSI，远端 shell 空闲、TCP 队列为 0，均与现象不符。
