# rgpui-term（RuTerm 本地开发副本）

> **来源**：crates.io `rgpui-term 1.8.2` 的源码，依赖已展开成 registry 版本，
> 因而可作为独立 crate 编译/测试，不牵连上游 rgpui workspace。
>
> **为何在这里**：上游发版周期长，终端的滚动/输入问题先在 ruterm 内快速迭代，
> 稳定后再整体回馈到 `launcher-rs/rgpui` 的 `crates/rgpui-term`。
>
> **本地改动**（相对 1.8.2）：拖滚动条出界松手后解析线程卡死、Ctrl+C 看似失效
> 的修复（`end_scrollbar_drag` 统一清 `ui_dragging` + `external.rs` 拖动暂停 2s
> 兜底 + 回归测试）。上游对应分支 `fix/term-drag-stuck-ui-flag`（提交 059b4a0）。
>
> **开发**：`cargo test --manifest-path vendor/rgpui-term/Cargo.toml --lib`
> 可独立跑测试。主工程通过根 `Cargo.toml` 的 `[patch.crates-io]` 使用本副本。
> 回馈上游后：删掉根 `Cargo.toml` 的 patch 段、把 rgpui-term 版本提到修复版，
> 并删除本目录。

[rgpui](https://crates.io/crates/rgpui) 的终端组件：把 Alacritty 的终端仿真内核
（`alacritty_terminal`）接到 rgpui 的 GPU 渲染与事件循环上。既能起本地 PTY 跑
shell，也能把**任意外部字节流**（SSH / Telnet / 串口 / 测试注入）接成终端。

## 功能

- **终端仿真**：VT100/xterm 状态机、256 色与 TrueColor、同步输出（synchronized
  update）、滚动历史、鼠标上报、OSC 标题/调色板/响铃
- **两种字节来源**：
  - 本地 PTY —— [`TerminalBuilder::new`]（进程 + 读写线程）
  - 外接 IO —— [`TerminalBuilder::new_external`]（无进程，由传输层喂字节，见下）
- **内容搜索**：[`Terminal::search`]（正则）与 [`Terminal::search_literal`]
  （字面量，元字符按原样匹配）；`search_next` / `search_previous` 覆盖全部滚动历史
  并自动回绕，匹配高亮复用选区几何；结果状态用 [`Terminal::search_status`]
  或 `Event::SearchUpdated` 取得，区分「正则非法 / 无匹配 / 命中」
- **OSC 8 超链接**：链接单元格着色 + 下划线，`Ctrl`/`Cmd` + 点击发
  `Event::HyperlinkClick(uri)`，由应用侧决定白名单与打开方式
- **中文/日文输入（IME）**：组合期间候选文本留在终端内预览（下划线），确认后才按
  UTF-16 区间上屏给 PTY
- **选区与剪贴板**：拖拽选择、`SelectAll` / `Copy` / `Paste`
- **代码块跟踪**：`Block` / `BlockTracker`（命令输出分块，供跳转与折叠）
- **中间件**：`TerminalMiddleware` 在输入抵达 PTY 前拦截改写
- **主题**：`TerminalConfig` / `ThemeManager`，可运行时换肤
- **渲染**：`TerminalElement`（低层元素）与 `TerminalView`（带焦点/滚动条的实体组件）

## 快速上手（本地 PTY）

```rust
use rgpui_term::{Terminal, TerminalBuilder, TerminalView};

// TerminalBuilder::new 返回 Task，内部拉起 shell 进程
let builder = TerminalBuilder::new(None, None, Default::default(), None, window_id, cx)
    .await?;
let terminal = cx.new(|cx| builder.subscribe(cx));
let view = cx.new(|cx| TerminalView::new(terminal, window, cx));
```

## 外接 IO 后端（无本地进程）

适合「远端才是真终端」的场景。应用持有两个句柄：下行注入、上行转发。

```rust
use rgpui_term::{PtyBoundMessage, TerminalBuilder};

let mut builder = TerminalBuilder::new_external(None)?;
let handles = builder.take_external_handles().expect("外联终端必有句柄");
// 传输层（SSH channel 等）在别处：
//   远端回字节 → handles.feed.feed(bytes)
//   终端要写字节 / 尺寸变化 → handles.rx.recv() 拿到 PtyBoundMessage::{Input, Resize}
let terminal = cx.new(|cx| builder.subscribe(cx));
```

`feed()` 返回 `false` 表示会话已结束（通道无接收端），传输层据此关闭远端连接；
远端断开时调用 `feed.end_session()`，UI 侧与本地进程退出表现一致。

可运行示例：`cargo run -p rgpui_term_basic --bin external_io`（模拟一条远端会话，
双向通道都跑通）。

## 示例

| 示例 | 说明 |
|------|------|
| `cargo run -p rgpui_term_basic` | 多标签/多窗格终端：搜索条、OSC 8、IME、主题、持久化 |
| `cargo run -p rgpui_term_basic --bin external_io` | 外接 IO 后端（无 PTY） |
| `cargo run -p rgpui_term_integration` | 把终端嵌进应用布局 |

## 依赖

- `rgpui`（核心框架）
- `alacritty_terminal`（终端仿真内核 + `vte` 状态机）

## 平台说明

需要本地 PTY 的路径依赖平台的进程与设备接口；`new_external` 不产生进程，理论上可跑在
任何能编译 `rgpui` 的平台，但 Web/WASM 目标当前未纳入终端的验证矩阵。
