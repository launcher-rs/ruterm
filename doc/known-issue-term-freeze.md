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

## 8. 复盘：为什么第一轮排查没找到根因

根因并不在 rgpui / alacritty 一侧，而在 russh 会话任务中一次阻塞的
`chan.send().await`。第一轮排查耗时很久却没有定位，主要不是信息不足，
而是排查方法有偏差。按影响排序：

### 8.1 把「复现不出来」当成「假设不成立」，跳过了变量最少的实验

最该做的是纯下行洪泛（`seq 1 1000`、`cat` 大文件、`yes`）——超过 100 条下行
报文就会暴露，不需要粘贴、不需要删除，十秒内就能复现。实际却直接去复刻
用户描述的操作序列（粘贴 + 连按退格 + 截图），这条链路同时混着上行合批、
远端回显、终端锁和 UI 渲染好几个变量，卡住也说不清是哪一段。

**教训：先用最小变量实验拆分，再复刻现象；现象不是复现步骤。**

### 8.2 把决定性的一步留成了「建议的下一步」

文档里写过「卡死时抓全线程栈、记录 CPU 占用」，但当时没有执行。真正定性的
证据其实很便宜：进程 `Responding=True`、CPU 低、远端 shell 空闲、TCP
`Recv-Q/Send-Q` 为 0。用 `tasklist` + 全线程栈，五分钟就能确认
**UI 线程没有死锁，停掉的是 SSH 会话任务**。没有拿现场证据，后续所有推理
都建立在对「窗口卡死」的猜测上。

### 8.3 锚定效应 + 误读了「在源码里修」

此前确实修掉了一个 rgpui-term 的 lease 自锁，之后所有「卡死」都被套进同一个
模型：UI 线程在等 `FairMutex`。用户说「有问题直接在源码中修复」是条件句
（如果问题在源码里，就改源码），却被理解成「bug 就在 rgpui 源码」，
搜索范围被锁死在 `D:\code\rgpui`。

### 8.4 误读了「删到某个字符」这条关键线索

当时把它当成「与内容相关」（怀疑转义序列、换行重绘、某个 CSI 让解析器
卡死），实际是「与计数相关」：逐字退格时远端每键回显 1～5 字节的小包，
删到那个位置正好用满 100 条队列配额。甚至已经观察到「冻结点偏确定性、
不像随机竞态」——**确定性阈值 + 固定停点 = 计数器/队列满**，这是最经典的
排查信号，却被推向了「特定字节流内容」。把冻结点到文本末尾的字节数/报文数
量化，再对照 russh 默认的 `channel_buffer_size = 100`，答案就在眼前。

### 8.5 复现没有通过标准，用「截图看起来正常」代替了结论

smoke 钩子连发 400 次退格时，只确认了按键送到了 UI 输入队列、中间某张截图
文本在减少，并没有逐条核对「最后几十次按键是否仍有回显」。键位投递成功不等于
SSH 通道写成功，判定条件恰好漏掉了后半段。

### 8.6 信任了「很久没改、一直在跑」的 IO 边界层

russh 的通道封装一直没有改、SSH 也一直能用，于是被当作可信基础设施，
没去读它的收发循环。而这类潜伏 bug 恰恰最常藏在这种长期稳定、无人细看的
边界代码里，一旦出现新的负载特征（大量小报文的下行回显）就被触发。

### 8.7 观测点选错了层

`RUTERM_TRACE_EXTERNAL` 记录的是「上行泵 / 输入合批 / resize」，真正有效的
信号却是「下行 chunk 计数停在 98」——也就是 **handler 回调是否持续返回、
会话任务是否还活着**。日志应按「如果它挂了，哪几个计数会停止增长」设计，
而不是只记录原本怀疑的路径。

### 8.8 如果重来，排查顺序

1. 先判定「谁停摆」：进程是否 `Responding`、CPU 占用、全线程栈；不要从
   「窗口没反应」直接推到「UI 线程死锁」。
2. 用纯下行洪泛 / 纯按键 / 延迟下行做最小变量拆分。
3. 对确定性冻结点先量化：距末尾的字节数、报文数，和常见默认阈值对照。
4. 读 IO 边界库的收发循环，尤其是「在会话任务里 `await` 的有界通道」。
5. 每次复现都定义可判定的通过标准（最后 N 次按键仍有回显），不靠截图观感。

可复用的通用准则：**症状描述是用户感受，不是诊断结论；先拿客观证据
（进程状态、线程栈、计数）再选假设，别把上一轮修好的 bug 当成这一轮的模型。**
