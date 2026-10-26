# RuLink 拾连

面向开发者的开源远程连接工具（SSH 客户端），基于 [rgpui](https://crates.io/crates/rgpui) +
[rgpui-term](https://crates.io/crates/rgpui-term) 外接 IO 后端构建。

GUI 会话管理器形态：左侧持久化的主机账号列表，点账号即连；右侧多标签终端。
也兼容一次性命令行快速连接（带目标参数时启动即连上该主机）。

## 功能现状

- **SSH 会话**：russh 客户端 + 交互式 PTY（`xterm-256color`），网络 IO 全在 tokio 运行时，UI 线程零阻塞。
- **三种认证**：SSH Agent / 私钥文件（只存路径；可选手输或"浏览…"系统文件对话框选取；加密私钥可在表单直填口令或连接时自动弹口令窗）/ 密码。口令与密码均可勾选"保存"，加密存入本地凭据库，默认仅本次内存持有。
- **主机指纹校验**：TOFU known_hosts 直接存入加密库的 `known_hosts` 表；指纹不符、写入或查询失败一律拒绝连接（fail-closed，防中间人）。
- **多标签会话**：打开、切换、关闭；远端 `exit`、认证失败、断线都会自动移除标签。
- **主机管理**：新建/编辑/删除、搜索框实时过滤（按展示名或 `user@host:port`）、★ 收藏置顶并持久化；编辑与删除收在主机行**右键菜单**，删除弹二次确认（已保存的密码/口令一并从加密库清除）；新建/编辑为**居中弹窗**表单（切换认证方式高度恒定），不再挤在侧边栏。
- **布局**：侧边栏可拖拽宽度（WindTerm 式，拖到最小宽度停住）、`«` / `»` 折叠栏。
- **自定义标题栏**：应用自绘顶栏（`appears_transparent` + `app_owns_titlebar_drag`），左侧品牌标识与锁定提示，
  中间**多级菜单**「会话 / 凭据 / 外观 / 帮助」把设置项按域分散，右侧 ⚙ / 🔑 / 🔒 / `«` 快捷按钮，
  最右为系统窗口控件（最小化 / 最大化 / 关闭）。拖拽与窗口控件走 `WindowControlArea` 命中区，
  交互控件排在 Drag 区**之外**（Windows 的 `WM_NCHITTEST` 只认第一个命中的控制区，父级 Drag 会吞掉栏内按钮点击）。
- **设置界面**：⚙ 或菜单进入，按 **Tab 分页**——「凭据库与主密码」/「外观」，后续设置项继续加分页即可。
- **主题**：明 / 暗切换，配色全部取 rgpui 全局 `Theme` 语义色，选择持久化到加密库。
- **凭据存储**：主机配置、应用设置、known_hosts 合并为**一个加密文件** `rulink.db`
  （toasty ORM + turso AEGIS-256 页级加密），不再有零散的 JSON / 文本档。
  密码 / 私钥口令另有**第二层字段加密**：`hosts.password` 列存 AES-256-GCM 密文
  （字段密钥由主密钥域分离派生，每次写入随机 nonce），即使库文件被离线复制也只多一道锁。
- **便携 / 全局双模式**：exe 同目录**存在** `data/` 文件夹即为便携模式（全部数据随 exe 走，U 盘可带），
  否则为全局模式 `~/.rulink`。判定只探测、不自动创建目录。
- **两档解锁**：默认（全局与便携皆同）用系统凭据管理器里的随机主密钥，启动零输入、开箱即用；
  主密码（argon2id 派生）为**可选增强**，在 ⚙ 设置里随时**设置 / 更换 / 取消**：设置后每次启动需输入口令，
  库文件可凭同一口令拷到另一台机器解锁；取消则换回免密档（库重新绑定本机）。已设主密码时标题栏出现
  🔒，一键锁定凭据库（锁定后只剩 🔑 解锁入口）。⚠️ 忘记主密码 = 数据不可恢复（无任何后门），请牢记。
- **状态栏**：底部常驻，左侧整体状态 + 活动会话地址/认证方式 + 远程主机信息，右侧会话计数。
  连接后经 exec 通道一次性探测远端 OS / 内存 / 磁盘 / 负载 / uptime（探测失败则该条目不显示）。
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
- 快速连接同样在 GUI 窗口内进行：已设置主密码时，若要读取库内已保存的凭据，先在主界面完成解锁
  （未解锁时指纹校验回退旧 `known_hosts` 文本，行为与降级模式一致）。

## 本地文件

数据目录由启动模式决定：**exe 同目录存在 `data/` 文件夹 → 便携模式**（下表路径换成 `<exe 目录>/data/`），
否则全局模式 `~/.rulink`。判定只探测不创建——想切换便携，手动在 exe 旁建一个空 `data/` 即可。

| 路径 | 内容 |
|------|------|
| `rulink.db` | **唯一数据文件**（toasty ORM + turso，AEGIS-256 页级加密）：`hosts`（主机配置 + 可选保存的密码/口令）、`settings`（主题等应用设置）、`known_hosts`（主机指纹）三张表 |
| `kdf-salt.bin` | argon2 派生主密钥用的随机盐（**公开参数，非秘密**）；更换主密码时重新生成 |
| `unlock.txt` | 解锁后端标记（`keyring` / `passphrase`），非敏感明文，用于决定启动是否弹解锁窗 |
| `vault.db` → `vault.db.migrated` | 旧版手写 SQL 加密库；首启读出迁入 `rulink.db` 后改名保留 |
| `sessions.json` → `sessions.json.migrated` | 旧版主机构档；迁入加密库后改名保留（加密库不可用时才继续读写原文件） |
| `settings.json` → `settings.json.migrated` | 旧版应用设置（主题）；迁入 `settings` 表后改名保留 |
| `known_hosts` → `known_hosts.migrated` | 旧版指纹文本；迁入 `known_hosts` 表后改名保留 |
| `rulink.db.keyring-bak` | 设置 / 更换 / 取消主密码时被替换下的旧库备份（数据校验通过后自动清理） |

Windows 下 `~` 取 `%USERPROFILE%`，其他平台取 `$HOME`。

**主密码须知**：主密码是**可选**加固项——不设也能正常用（默认走本机凭据管理器免密解锁，状态栏会给出建议）。
在 ⚙ 设置 →「凭据库与主密码」里可随时**设置 / 更换 / 取消**：设置或更换都需先输入并确认新口令
（已是口令档时还要先验证当前主密码，防止托盘常驻时被他人直接改密钥）；取消则把库密钥换回凭据管理器里的
随机密钥，此后启动不再输入口令。口令错误可重试，连续失败想放弃就点"退出应用"——未解锁的运行不落任何秘密。
**忘了主密码数据无法找回**（Argon2 无后门）；免密档（keyring）下拷贝 `rulink.db` 到其他机器无法解密，
需要随身携带就把主密码设上。

## 依赖

| Crate | 版本 | 用途 |
|-------|------|------|
| `rgpui` | 1.6.0 | GUI 框架（启用 `tokio` 特性：异步运行时集成） |
| `rgpui-platform` | 1.6.0 | 平台应用入口（`rgpui_platform::application()`） |
| `rgpui-term` | 1.6.0 | 终端组件（本项目用外接 IO 后端 `TerminalBuilder::new_external`） |
| `russh` | 0.64 | SSH 客户端（`flate2` / `ring` / `rsa`，关闭默认特性） |
| `tokio` | 1 | 运行时与网络（rt-multi-thread、net、io-util、time 等） |
| `serde` / `serde_json` | 1 | 降级路径（`sessions.json` / `settings.json`）持久化 |
| `toasty` / `toasty-driver-turso` | 0.11 | ORM：声明式模型 + 自动 CRUD，驱动走 turso 并暴露页级加密参数 |
| `turso` | 0.7.2 | 纯 Rust SQLite（与驱动同版本；仅旧库一次性迁移时直连读取） |
| `argon2` | 0.5 | 主密码 → 主密钥派生（argon2id） |
| `keyring` | 4.2 | keyring 档主密钥存取（Windows Credential Manager / Keychain / Secret Service） |
| `getrandom` | 0.4 | 主密钥与盐的随机生成 |
| `winresource` | 0.1 | 仅 Windows：构建期嵌入 exe 图标资源 |

rgpui 三个 crate 走 crates.io 正式版本。本地联调 rgpui 时可在 `[patch]` 或临时把依赖指回 `path = "../rgpui/crates/..."`（工作区标签 `v1.6.0` 与发布版本一致）。

## 模块结构

```
src/
  main.rs       # UI：RuLinkApp（侧边栏、标签栏、终端、状态栏、托盘）+ 解锁弹窗 + 设置弹窗（主密码、主题）+ 应用入口
  session.rs    # 会话桥接：rgpui-term 外接 IO ⇄ russh 双向字节流（上行泵线程 + 转发任务）+ exec 远程信息探测
  ssh.rs        # SSH 引擎：russh client::Handler、指纹校验（优先查加密库，降级回文本文件）、旁路通道过滤
  remote.rs     # 远程主机信息：POSIX 探测脚本 + key=value 解析/状态栏摘要（含单测）
  config.rs     # 连接目标：命令行解析 + 主机账号 → SshTarget
  paths.rs      # 存储模式判定（便携 `data/` 或全局 `~/.rulink`）与所有数据文件路径
  sessions.rs   # 降级路径：旧版主机/设置 JSON（SessionStore）+ 迁移后改名标记
  vault.rs      # 单文件加密库：toasty 模型（hosts/settings/known_hosts）+ keyring / argon2 主密码 + 旧文件一次性迁移
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
- ~~**凭据与安全存储**~~：已完成 —— 主机 / 设置 / known_hosts 合并为单个 toasty + turso 加密库
  （AEGIS-256 页级加密），密码字段再叠一层 AES-256-GCM 字段加密，解锁支持系统凭据管理器随机密钥与
  用户主密码（argon2id）两档，主密码可在设置里
  设置 / 更换 / 取消，并支持便携 / 全局双模式；
  密码默认仅本次内存持有，勾选"保存密码"才入库。余量：分组 / 备注、导入导出界面、备份恢复。
- **自定义标题栏 + 分屏**：标题栏已落地（品牌标识 + 多级菜单 + 窗口控件）；余下**会话标签并入标题栏**
  （类 WindTerm）与 `SplitPane` / `DockArea` 多会话分屏。
- **i18n**：接入 rgpui `I18nManager`，首批中文 / English。
- **SFTP / 端口转发 / 跳板机**：第二阶段能力。

## 许可

与 rgpui 一致采用 Apache-2.0；本项目暂未在 `Cargo.toml` 声明 `license` 字段，正式对外发布前需补齐。
