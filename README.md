# RuLink 拾连

面向开发者的开源远程连接工具（SSH 客户端），基于 [rgpui](https://crates.io/crates/rgpui) +
[rgpui-term](https://crates.io/crates/rgpui-term) 外接 IO 后端构建。

GUI 会话管理器形态：左侧持久化的主机账号列表，点账号即连；右侧多标签终端。
也兼容一次性命令行快速连接（带目标参数时启动即连上该主机）。

## 功能现状

- **SSH 会话**：russh 客户端 + 交互式 PTY（`xterm-256color`），网络 IO 全在 tokio 运行时，UI 线程零阻塞。
- **三种认证**：SSH Agent / 私钥文件（只存路径）/ 密码（连接时临时输入，绝不落盘）。
- **主机指纹校验**：TOFU known_hosts（`~/.rulink/known_hosts`），指纹不符或写盘失败一律拒绝连接（fail-closed，防中间人）。
- **多标签会话**：打开、切换、关闭；远端 `exit`、认证失败、断线都会自动移除标签。
- **主机管理**：新建/删除表单、搜索框实时过滤（按展示名或 `user@host:port`）、★ 收藏置顶并持久化。
- **布局**：侧边栏可拖拽宽度（WindTerm 式，拖到最小宽度停住）、`«` / `»` 折叠栏。
- **主题**：明 / 暗切换，配色全部取 rgpui 全局 `Theme` 语义色，选择持久化。
- **状态栏**：底部常驻，左侧整体状态 + 活动会话地址/认证方式，右侧会话计数。
- **系统托盘**：品牌图标 + 悬停提示 + 右键菜单（显示主界面 / 退出）；点窗口关闭按钮隐藏到托盘，会话后台保活。
- **品牌图标**：多尺寸 `.ico` 经 `build.rs` + winresource 嵌入 exe（文件 / 任务栏 / 标题栏）。

## 快捷键

| 按键 | 动作 |
|------|------|
| `Ctrl+B` | 折叠 / 展开侧边栏 |
| `Ctrl+T` | 切换明暗主题 |

## 构建与运行

```bash
cargo run                 # 启动 GUI 会话管理器
cargo run --release       # 发布构建
cargo build --release     # 产出可执行文件
```

要求 Rust 2024 edition 工具链（`Cargo.toml` 中 `edition = "2024"`）。

### 命令行快速连接

```bash
rulink [user@]host[:port] [-i keyfile]
```

- 用户名缺省时取 `$USER` / `$USERNAME`，再回退 `root`；端口缺省 22。
- 密码认证通过环境变量提供：`RULINK_PASSWORD=xxx rulink user@host`（命令行不接受密码参数）。
- 不带任何参数即进入 GUI 主界面。

## 本地文件

| 路径 | 内容 |
|------|------|
| `~/.rulink/sessions.json` | 主机账号列表（含 `favorite` 标记，**不含任何密码**） |
| `~/.rulink/settings.json` | 应用级设置（当前为主题模式 `dark` / `light`） |
| `~/.rulink/known_hosts` | 主机密钥指纹（TOFU，与系统 OpenSSH 文件隔离） |

Windows 下 `~` 取 `%USERPROFILE%`，其他平台取 `$HOME`。

## 依赖

| Crate | 版本 | 用途 |
|-------|------|------|
| `rgpui` | 1.6.0 | GUI 框架（启用 `tokio` 特性：异步运行时集成） |
| `rgpui-platform` | 1.6.0 | 平台应用入口（`rgpui_platform::application()`） |
| `rgpui-term` | 1.6.0 | 终端组件（本项目用外接 IO 后端 `TerminalBuilder::new_external`） |
| `russh` | 0.64 | SSH 客户端（`flate2` / `ring` / `rsa`，关闭默认特性） |
| `tokio` | 1 | 运行时与网络（rt-multi-thread、net、io-util、time 等） |
| `serde` / `serde_json` | 1 | 会话与设置持久化 |
| `winresource` | 0.1 | 仅 Windows：构建期嵌入 exe 图标资源 |

rgpui 三个 crate 走 crates.io 正式版本。本地联调 rgpui 时可在 `[patch]` 或临时把依赖指回 `path = "../rgpui/crates/..."`（工作区标签 `v1.6.0` 与发布版本一致）。

## 模块结构

```
src/
  main.rs       # UI：RuLinkApp（侧边栏、标签栏、终端、状态栏、托盘）+ 应用入口
  session.rs    # 会话桥接：rgpui-term 外接 IO ⇄ russh 双向字节流（上行泵线程 + 转发任务）
  ssh.rs        # SSH 引擎：russh client::Handler、TOFU 指纹校验
  config.rs     # 连接目标：命令行解析 + 主机账号 → SshTarget
  sessions.rs   # 主机账号与设置的 JSON 持久化（SessionStore）
```

数据流：下行 `SshTerminalHandler::data` → `FeedHandle::feed` 灌入终端；
上行终端阻塞通道 → 专用泵线程 → tokio 通道 → 写入 russh 通道（按键 / 应答 / 尺寸变化）。

## 路线图

完整设计目标与逐项进度见 [`doc/design-roadmap.md`](doc/design-roadmap.md)（勾选式清单）。品牌定位与分阶段愿景见 `doc/rulink.txt`。

近期重点：

- **本地终端（系统 Shell）** —— 计划中，尚未实现。目标是本地 + 远程统一终端：
  在 Windows 接入 **cmd**、**Windows PowerShell 5.x**、**PowerShell 7 (pwsh)**、**Git Bash**、**WSL**（`wsl -d <发行版>`），
  Linux/macOS 探测 `$SHELL`。基座已具备（`rgpui-term` 的本地 PTY 后端 `TerminalBuilder::new`，可指定 shell / 工作目录 / 环境变量），
  分三期落地：跑通默认 shell 单标签 → profile 自动发现 + 侧边栏"本地终端"入口 → profile 持久化与默认 shell 设置。
  同时抽出 `SessionBackend` 抽象（`External` / `LocalPty`），UI 层不再假设"一定有 host"。
- **凭据与安全存储**：主机配置迁入 SQLite，敏感字段对接操作系统凭据库（Windows Credential Manager / Keychain / Secret Service）；密码绝不以明文入库、入日志。
- **自定义标题栏 + 分屏**：`TitleBar` 品牌化顶栏（标签并入标题栏）、`SplitPane` / `DockArea` 多会话分屏。
- **状态栏远程信息**：SSH exec 通道探测远端 OS / 内存 / 磁盘 / uptime，填入现有 `remote_info` 占位。
- **i18n**：接入 rgpui `I18nManager`，首批中文 / English。
- **SFTP / 端口转发 / 跳板机**：第二阶段能力。

## 许可

与 rgpui 一致采用 Apache-2.0；本项目暂未在 `Cargo.toml` 声明 `license` 字段，正式对外发布前需补齐。
