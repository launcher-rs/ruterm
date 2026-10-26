# RuLink 设计目标与开发路线图

> 本文件是 RuLink 的功能设计总纲：**列出目标 → 完成一项勾选一项**。
> 勾选图例：`[x]` 已完成 · `[~]` 进行中/部分完成 · `[ ]` 待办。
> 品牌定位与分阶段愿景见同目录 `rulink.txt`。

## 一、当前进度快照

| 能力 | 状态 | 说明 |
|------|------|------|
| SSH 连接与终端 | `[x]` | russh + rgpui-term（外部 IO 后端），交互式 PTY |
| 本地终端（系统 Shell） | `[ ]` | 待接入 rgpui-term 本地 PTY 后端：cmd / PowerShell / pwsh7 / Git Bash / WSL，见 §五 |
| 认证方式 | `[x]` | 密码 / 私钥(IdentityFile) / Agent，连接时自动推断 |
| 主机指纹校验 | `[x]` | TOFU known_hosts，不匹配 / 写入失败一律拒绝（fail-closed，防 MITM） |
| 会话标签页 | `[x]` | 多标签、关闭、激活切换 |
| CLI 快速连接 | `[x]` | 带参数一次性连接；密码仅走环境变量 / GUI 临时输入 |
| 主题 | `[x]` | 明 / 暗切换（配色走 `cx.theme()` 语义色，与按钮/状态栏统一），☀/☾ 按钮 + `Ctrl+T`，选择持久化到 `settings.json` |
| 底部状态栏 | `[x]` | 已实现并验证渲染（左"就绪"/地址/认证/远程信息，右"会话 N"）；远程基本信息经 exec 探测通道填充 |
| 凭据持久化 | `[ ]` | 密码从不落盘；主机配置存 JSON（`~/.rulink/sessions.json`），见 §四 |
| 品牌图标 | `[x]` | 从展示板切分 6 图标变体 + logo + 多尺寸 `.ico`；`build.rs`+winresource 嵌入 exe（任务栏/标题栏） |
| 系统托盘 | `[x]` | 品牌托盘图标 + 悬停提示 + 右键菜单（显示主界面 / 退出），`App::set_tray_*` + `on_tray_menu_action` |

---

## 二、UI / UX 目标（本次新增，主流 SSH 工具应具备）

### 1. 状态栏增强
- `[x]` 底部常驻状态栏：整体状态 + 活动会话地址 / 认证方式 + 会话计数。
- `[x]` 连接后展示**远程服务器基本信息**：主机名、操作系统及版本、内存使用、磁盘空间、uptime。
  - 实现：`src/remote.rs`（探测脚本 + `RemoteInfo` 解析/摘要，含单测）、`src/session.rs`
    （shell 启动后另开一条 **exec 通道**一次性跑探测脚本，8s 超时，摘要经 `oneshot` 回传写入
    `SessionTab.remote_info`）、`src/ssh.rs`（`SideChannels` 旁路通道集合）。
  - 关键坑：russh 会把客户端自开通道（exec）的字节**同时**投递给 `Channel` 接收端与
    `Handler::data/extended_data/channel_close/exit_status`，不过滤就会把探测输出灌进终端、
    并在探测通道关闭时误判"远端退出"结束整个会话；故非 PTY 通道一律登记进 `SideChannels` 跳过。
  - 探测脚本按 POSIX sh 编写（`uname` / `/etc/os-release` / `/proc/meminfo` / `df -hP /` /
    `/proc/loadavg` / `/proc/uptime`，非 Linux 回退 `uptime -p`），缺项静默跳过，
    远端禁用 exec 时状态栏该条目直接不显示。
- `[ ]` **可配置显示项**：设置面板勾选状态栏要显示哪些条目（主机名/OS/内存/磁盘/uptime…）。

### 2. 侧边栏
- `[x]` 可拖拽调整宽度（rgpui `SplitPane::horizontal`，初始比例 0.25，最小宽度 160px）。
- `[x]` 折叠 / 展开：侧边栏头部 `«` 折叠按钮 + 折叠后主区顶部 `»` 展开栏 + 快捷键 `Ctrl+B`（不再用分隔线中间的双箭头）。
- `[x]` WindTerm 式拖拽：拖到最小宽度即停住，分隔线仍可抓住往大拖；完全收起只交给 `«` / `Ctrl+B`，不做拖拽阈值自动折叠（此前自动折叠会在展开后拖动时因 `collapse` 前的旧比例经 mouse-down 触发 `on_resize` 而立刻重新收起，已移除）。
- `[x]` 窄宽度下主机行自适应：名称/地址 `text_ellipsis` 截断，认证方式 / 删除按钮 `flex_shrink_0` 固定不被挤压。
- `[ ]` 折叠后保留窄图标轨道，悬停可临时展开（现折叠为完全收起 + `»` 展开栏；窄轨道悬停展开可后续按需增强）。

### 3. 主题
- `[x]` 明 / 暗主题切换（rgpui `Theme::change(ThemeMode)` + 配色统一走 `cx.theme()` 语义色），侧边栏 ☀/☾ 按钮 + `Ctrl+T`。
- `[x]` 启动项记忆上次主题选择（持久化到 `~/.rulink/settings.json`，下次启动自动应用）。
- `[ ]` 终端配色方案（多套预设，可自定义前景/背景/16 色）。
- `[ ]` 自定义字体与字号、行高（终端 + UI）。

### 4. 国际化（i18n）
- `[ ]` 接入 rgpui `I18nManager`，抽取界面文案为 key。
- `[ ]` 首批语言：中文 / English；语言切换即时生效并持久化。
- `[ ]` RTL 基线（框架已支持 `TextDirection`，暂只保证 LTR 正确）。

### 5. 自定义标题栏
- `[ ]` 用 rgpui `TitleBar` 替换系统标题栏，实现品牌化顶栏（标签页并入标题栏，类 WindTerm）。
- `[ ]` 保留窗口拖动、双击最大化、系统按钮（最小化/最大化/关闭）。
- `[ ]` Windows 下与 Mica 材质 / 圆角协调。

### 6. 分屏窗口
- `[ ]` 水平 / 垂直分屏（一个窗口内并排多个终端会话）。
- `[ ]` 分屏焦点切换（快捷键）、单屏最大化、拖拽重排。
- `[ ]` 复用 rgpui `SplitPane` / `DockArea` 作为布局基座。

### 7. 其它主流能力
- `[x]` 主机列表搜索：侧边栏顶部搜索框，按展示名 / `user@host:port` 子串实时过滤（无匹配显示"无匹配主机"）。
- `[x]` 主机收藏：每行 ★/☆ 切换按钮（`stop_propagation` 不触发连接），收藏主机稳定排序置顶，`favorite` 字段持久化到 `sessions.json`（`#[serde(default)]` 兼容旧数据）。
- `[ ]` 主机分组、最近连接。
- `[ ]` 断线提示 + 手动 / 自动重连、连接超时设置。
- `[ ]` 命令片段库、快捷命令、多主机同时执行。
- `[ ]` 终端日志记录与导出、会话保存 / 恢复。
- `[ ]` `~/.ssh/config` 导入与解析。

### 8. 系统托盘
- `[x]` 品牌托盘图标 + 悬停提示 + 右键菜单（显示主界面 / 退出），经 `App::set_tray_icon/tooltip/menu` + `on_tray_menu_action`。
- `[x]` 关闭到托盘（点窗口 X 经 `Window::on_window_should_close` 返回 false 改调 `hide_window()`，会话后台保活；托盘"显示主界面"经 `WindowHandle::update(..).activate_window()` 恢复，"退出"经 `cx.quit()` 直接投 `WM_QUIT` 真正结束）。
- `[ ]` 托盘菜单动态列出当前活动会话，点击可跳转到对应标签。
- `[ ]` 托盘图标角标 / tooltip 反映会话数或连接状态。

---

## 三、架构基座复用（来自 rgpui）

RuLink 不自造 UI 轮子，优先复用 rgpui 现成子系统：

| 需求 | 复用组件 |
|------|----------|
| 状态栏 | `components::status_bar::{StatusBar, StatusBarItem}`（已用） |
| 侧边栏可拖拽宽度 / 折叠 | `SplitPane`（已用）/ `TaskPane` 自动隐藏轨道（备用） |
| 分屏 / 停靠 | `SplitPane`、`DockArea` |
| 自定义标题栏 | `title_bar::TitleBar` |
| 主题切换 | `theme::Theme::change` |
| 系统托盘 | `tray::{TrayMenuItem, TrayIconEvent}` + `App::set_tray_*`（已用） |
| 国际化 | `i18n::I18nManager` |
| 命令注册 / 快捷键 | `commands::CommandRegistry`、`key_context` |
| 布局约束范式 | `examples/showcase/src/bin/app_suite.rs`（flex_grow + min_h(0) 有界布局范式，状态栏布局修复即据此） |

---

## 四、凭据与安全存储（重点，用户明确诉求：讨厌反复输入密码）

### 目标
- `[ ]` 主机配置、用户名、私钥路径、（可选）密码 / 证书 passphrase 迁入**结构化存储**（SQLite），
      取代当前 JSON 文件；支持分组、备注、导入导出、备份恢复。
- `[ ]` 连接时**免重复输入**：优先用 Agent / 私钥；确需密码时安全取出。

### 安全约束（不可妥协，沿用现有基线并加强）
- `[ ]` **密码 / passphrase 绝不以明文入库、入日志、入命令行**。
- `[ ]` 敏感字段加密：优先对接**操作系统凭据库**
      （Windows Credential Manager / macOS Keychain / Linux Secret Service），
      SQLite 只存密文或不存、仅存对 keyring 条目的引用。
- `[ ]` 若退化为本地加密库文件：主密钥来自 keyring 或用户解锁口令（KDF 派生），
      算法用成熟 AEAD（如 AES-256-GCM），口令不落盘。
- `[ ]` known_hosts 指纹校验保持 fail-closed；指纹冲突必须拒绝连接。
- `[ ]` 提供"仅本次会话内存持有密码"与"持久保存"两档，默认前者。

### 迁移策略
- `[ ]` 一次性从 `sessions.json` 迁移到 SQLite；保留导入 / 导出（含加密导出包）。
- `[ ]` 迁移前不删除旧文件，验证无误后再切换。

---

## 五、本地终端（系统 Shell 会话）

> RuLink 目前是纯 SSH 客户端。目标是同时成为**本地 + 远程统一终端**：
> 一个应用里既能开 SSH 会话，也能开本机 shell 标签。
> 基座已具备：`rgpui-term` 除外接 IO 后端（`TerminalBuilder::new_external`）外，
> 还有本地 PTY 后端 `TerminalBuilder::new(working_directory, shell, env, max_scroll_history_lines, window_id, cx)`，
> `shell: Option<...>` 为 `None` 时用系统默认 shell。SSH 侧只需继续用外接后端，两条路径共用 `TerminalView`。

### 1. Windows 本地 shell（首批）
- `[ ]` **cmd.exe**：`cmd.exe`（默认回退 shell）。
- `[ ]` **Windows PowerShell 5.x**：`powershell.exe`（系统自带，无需安装）。
- `[ ]` **PowerShell 7**：`pwsh.exe`（用户自行安装；探测 PATH 与常见安装位置）。
- `[ ]` **Git Bash**：`bash.exe`（探测 `git\bin\bash.exe` / `git\usr\bin\bash.exe` 注册表与 PATH）。
- `[ ]` **WSL**：`wsl.exe`（可带 `-d <发行版>` 列出已安装发行版供选择）。
- `[ ]` Windows 控制台 PTY ConPTY 适配（`ConPTY` 作为 `portable-pty` 后端落地路径），确认 Ctrl+C / 尺寸变化 / ANSI 序列正常。

### 2. 其他平台本地 shell（顺带，非 Windows 用户同样受益）
- `[ ]` Linux/macOS：探测 `$SHELL`，回退 `bash` / `zsh` / `sh`。

### 3. 会话形态与 UI
- `[ ]` 侧边栏新增"本地终端"分区（与主机列表并列），或顶部"+ 新建会话"下拉选择目标类型（SSH / 本地 shell）。
- `[ ]` 标签标题区分来源：本地会话显示 `shell 名 + 当前目录`，远程会话保持 `user@host`（现有 `SessionTab.addr` 语义扩展为"连接描述"）。
- `[ ]` 默认 shell 可设置（`settings.json` 增 `default_local_shell`），启动时可记忆上次使用的 profile。
- `[ ]` 每个本地会话可选起始工作目录（含"打开当前目录的终端"入口）。

### 4. Shell Profile（类 Windows Terminal 的 profile 概念）
- `[ ]` `sessions.json` 增本地条目类型（或独立 `profiles.json`）：名称、可执行路径、参数数组、起始目录、环境变量、图标/配色。
- `[ ]` 保留与收藏、搜索、置顶复用现有主机列表机制（★ 收藏 + 搜索过滤天然适用）。
- `[ ]` 从系统自动发现 profile（注册表 / PATH / `wsl -l`），一键添加。

### 5. 架构约束（避免做成 SSH 专用）
- `[ ]` 抽出 `SessionBackend` 抽象（`External`（russh）/ `LocalPty`），UI 层只认 `TerminalView` + 会话生命周期，不再假设"有 target/host"。
- `[ ]` 状态栏本地会话条目显示 shell 与运行时长，不显示地址/认证方式。
- `[ ]` 基座已具备，一期只需调用：`TerminalBuilder::new(working_directory, shell, env, max_scroll_history_lines, window_id, cx) -> Task<Result<TerminalBuilder>>`（`shell` 为 `None` 时走 `default_shell_command()`），拿到 builder 后与 SSH 路径完全同构（`cx.new(|cx| builder.subscribe(cx))` + `TerminalView::new`）。
- `[ ]` 保持 UI 线程零阻塞：本地 PTY 读写走 rgpui-term 内部线程，应用侧只持有句柄。

### 6. 分期落地（慢慢实现）
- 一期：`TerminalBuilder::new` 直连默认 shell，单标签跑通 cmd + PowerShell 5。
- 二期：profile 自动发现（pwsh7 / Git Bash / WSL）+ 侧边栏"本地终端"入口 + 新建会话选择器。
- 三期：profile 持久化、起始目录、环境变量、默认 shell 设置，与 SSH 共用分屏/标签/搜索能力。

---

## 六、实施优先级建议

1. ~~收尾状态栏（远程信息 exec 通道）~~（已完成：连接后经 exec 探测写入 `remote_info`，见 §二.1）。
2. ~~侧边栏折叠 + 主题切换~~（已完成：`Ctrl+B` 折叠、`Ctrl+T` / ☀☾ 明暗切换）。
3. 凭据 SQLite + keyring（安全核心，尽早定型，越晚迁移成本越高）。
4. 自定义标题栏 + 分屏（结构性改动，成批做）。
5. i18n（文案抽取宜在功能稳定后统一做，避免反复）。
6. **本地终端**（§五：cmd / PowerShell / pwsh7 / Git Bash / WSL）——基座已在 rgpui-term，一期只是接本地 PTY，投入小、日常价值高，可与凭据存储并行推进。
7. SFTP / 端口转发 / 跳板机（`rulink.txt` 第二阶段）。

> 每完成一项，回到本文件把对应方框勾成 `[x]`，并在 §一快照表更新状态。
