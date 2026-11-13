# RuTerm 设计目标与开发路线图

> 本文件是 RuTerm 的功能设计总纲：**列出目标 → 完成一项勾选一项**。
> 勾选图例：`[x]` 已完成 · `[~]` 进行中/部分完成 · `[ ]` 待办。
> 品牌定位与分阶段愿景见同目录 `ruterm.txt`。

## 一、当前进度快照

| 能力 | 状态 | 说明 |
|------|------|------|
| SSH 连接与终端 | `[x]` | russh + rgpui-term（外部 IO 后端），交互式 PTY |
| 文件浏览与传输（SFTP） | `[x]` | 边栏「文件」页（「视图」菜单 / `Ctrl+Shift+B`）；工具栏「上传」与拖入即传，行右键「下载到…」；复用当前 SSH 连接开 SFTP 子系统，分块搬运 + 节流进度；SFTP 通道走旁路集合，不污染终端输出（见 §二.7） |
| 端口转发（-L / -R / -D） | `[x]` | 边栏「转发」页（「终端」菜单 / `Ctrl+Shift+F`）；默认只列已保存规则，点「设置…」才展开添加 / 编辑表单；**先保存规则、再用开关启停**：条目按 `user@host:port` 存进加密库 `forwards` 键，列表行开关即时生效、可设「连接时自动开启」。本地（-L）监听 `127.0.0.1:<端口>` 经当前连接转到服务器视角的目标；远程（-R）向服务器申请 `127.0.0.1:<端口>` 经 SSH 回到本机；动态（-D）在本机提供 SOCKS5 代理、目标由客户端协商；监听都只绑回环地址，生效条数压成「转发」图标角标（见 §二.7） |
| 跳板机（ProxyJump） | `[x]` | 主机表单「跳板机」下拉选另一台已保存主机；先连跳板机，再经 `direct-tcpip` 通道 `connect_stream` 连目标（限一级，排除自环与链式）。逐跳各自 TOFU 校验；跳板机密码 / 口令 / 内嵌私钥只取自内存缓存或加密库，取不到给提示而不二次弹窗；绑定存加密库 `settings.jumps`（见 §二.7） |
| 本地终端（系统 Shell） | `[x]` | 侧边栏树顶「本地终端」是**可折叠分组**：子项为本机探测到的每个 shell（各取 exe 的系统默认图标，WSL 按发行版分条为 `WSL · Debian`），点一下即开对应终端标签；条目支持右键**重命名**、**起始目录**、**启动参数**与**环境变量**（均持久化到 `settings.local_shells`）；启动后台静默扫一次（含 WSL），「一键扫描」可在右键菜单 / 设置「终端」分页手动刷新；标签与 SSH 会话并存，进程退出自动收标签；见 §五 |
| 认证方式 | `[x]` | 密码 / 私钥 / Agent；私钥支持**本地文件**（`浏览…`选择、可填口令、加密私钥自动弹口令窗）与**内嵌到加密库**（表单「私钥来源」二选一，PEM 经字段加密存 `settings.host_keys`，不落明文文件）；口令与密码同走"默认内存 + 勾选入加密库"两档 |
| 主机指纹校验 | `[x]` | TOFU known_hosts **存入加密库**（`known_hosts` 表），不匹配 / 写入失败 / 查询失败一律拒绝（fail-closed，防 MITM） |
| 会话标签页 | `[x]` | 多标签、关闭、激活切换；同名标签（如同一主机重复打开）显示 `名称:2`、`名称:3`，序号打开时取最小空号、之后不随前面的标签关闭而重排，重开只补空号；标签栏在主区顶部（44px 栏高、160px 最小标签宽），**只放标签**，文件 / 转发入口在边栏竖向导航；**平铺式标签**：不描边、标签之间不留缝，活动标签用主题语义色 `tab_active` 打底 + 顶部 2px 强调线，非活动标签不显示 `×`（切过去再关）；**横向滚动**并把活动标签自动滚进可视区，`Ctrl+Tab` / `Ctrl+Shift+Tab` / `Ctrl+PageDown` / `Ctrl+PageUp` 循环切换；标签可**拖动排序**（阈值触发、缩略标签跟随光标、落点标签高亮，落到空白处＝移至最末） |
| 分屏 | `[x]` | 窗口级两标签配对：`Ctrl+Shift+D` 左右、`Ctrl+Shift+E` 上下（「视图」菜单同入口），同方向再按退出、反方向换向；点击任一侧即切换活动标签并聚焦终端；分隔条可拖拽、两侧最小 240px；切到配对外标签或关闭任一侧自动退出（见 §二.6） |
| 键宏 | `[x]` | 「终端」菜单「键宏…」/ `Ctrl+Shift+M` 打开管理器：每行一条宏（启用开关 / 名称 / 触发键 / 步数 / 使用次数）、搜索过滤、▶执行 / 编辑 / 两步确认删除；编辑表单按「名称 / 触发键 / 步骤（发送内容 + 步后延迟）」排布，转义 `\n` `\t` `\e` `\\`，底部可「导入… / 导出…」JSON。触发键**点一下录入框再按键即录制**（rgpui `HotkeyInput`，× 清空＝不绑键，无需手打组合键串）。触发键只在终端聚焦且无浮层时拦截（`intercept_keystrokes`，早于动作分发），命中后按步写入活动会话（复用同步输入的 Programmatic 通道，不回流镜像）；存加密库 `settings.macros`（见 §二.7） |
| 命令片段 | `[x]` | 「终端」菜单「命令片段…」/ `Ctrl+Shift+S`：两个页签——「我的片段」与「内置命令」。我的片段按名称 / 命令 / 备注搜索，每行可「插入」（只送文本）或「执行」（追加回车）到活动终端，另有新建 / 编辑 / 两步确认删除；表单含名称、单行命令与可选备注，校验空值 / 多行 / NUL / 超长。内置命令只读，先按平台分一级分类（Windows / Linux / Unix / 通用），再分系统信息、软件包、磁盘与内存、进程与服务、网络、文件与压缩、容器、维护等二级分组，管理器默认展示当前会话的平台并标出「（当前会话）」，上方「平台」按钮（Windows / Linux / Unix / 通用）可切换查看其它平台，覆盖更新、磁盘 / 内存、端口、服务、日志、文件查找、容器等常用操作，可插入 / 执行或「存为我的片段」再改参数。平台来源：SSH 取连接探测的 `uname -s`（`RemoteInfo::platform`），本地终端取 shell 类型（PowerShell / CMD→Windows、WSL→Linux、Git Bash→Unix），Telnet 与探测失败视为未识别（只见通用命令）；判定在 `src/platform.rs`。数据存加密库 `settings.snippets`（降级 `settings.json`），发送走 `Terminal::input`（Programmatic，不回流同步输入）。模型与单测在 `src/snippets.rs`、`src/snippet_presets.rs`，界面在 `src/app/snippets_ui.rs` |
| 命令历史 | `[x]` | 「终端」菜单「命令历史…」/ `Ctrl+Shift+H`：自动收录各会话执行过的命令（来源是 rgpui-term 的 block 追踪，只取终端回显过的命令行，无回显的密码输入与 vim 等程序里的回车不会误收），去重后新的在前、上限 300 条；支持搜索、插入 / 执行、存为片段与单条删除，清空需两步确认。程序化发送（片段 / 内置命令 / 历史 / 广播）由 `send_command_to_terminal` 直接补记，因为不产生 block。数据存加密库 `settings.history`（降级 `settings.json`）。模型与单测在 `src/history.rs`，界面在 `src/app/history_ops.rs`、`src/app/history_ui.rs` |
| 终端输入补全 | `[x]` | 终端里输入命令时自动弹出候选浮层：按片段名称前缀 / 名称包含 / 命令单词前缀 / 命令包含 / 备注打分，内置命令与「我的片段」共用候选，内置命令只保留当前会话平台的条目（未识别平台时只给通用命令），最多 8 条；`↑↓` 移动、`Tab` / `Enter` 只补全不执行、`Esc` 关闭。补全前核对终端网格光标前字符与跟踪输入一致，只回删当前词后写入命令，`merged_command` 去掉与前缀重复的词（`sudo apt ` + `sudo apt install <包名>` → `install <包名>`）；命令运行中（vim / ssh）、输入法组合中、有浮层时不出候选。按键在 `intercept_keystrokes` 截获，早于动作分发；设置「终端」分页可关闭（`inline_completion`，默认开启）。匹配与单测在 `src/completion.rs`，交互与渲染在 `src/app/completion.rs` |
| 侧边栏主机列表 | `[x]` | 分组（文件夹）树：折叠三角 + 文件夹/认证方式图标 + 组名 + 数量，★ 收藏置顶；**组内排序**支持手动（右键上移/下移/移到最前）、按名称、按最近连接三种模式，空白处右键切换；组与归属、折叠态、排序方式存加密库 `settings` 表 `sidebar` 键（JSON，不动 `hosts` 表结构）。新建/编辑/删除/移动/分组管理全部收进右键菜单，顶部不再放常驻大按钮；支持从 `~/.ssh/config` 一键导入（见 §二.7）。 |
| CLI 快速连接 | `[x]` | 带参数一次性连接；密码仅走环境变量 / GUI 临时输入 |
| 主题 | `[x]` | 明 / 暗切换（配色走 `cx.theme()` 语义色，与按钮/状态栏统一），标题栏「外观」菜单（当前项打勾）+ `Ctrl+T`，选择持久化到加密库 `settings` 表 |
| 底部状态栏 | `[x]` | 已实现并验证渲染（左"就绪"/地址/认证/远程信息，右"会话 N"）；连接成功后状态切到「已连接 地址」，远程基本信息经 exec 探测通道**每 5 秒刷新**，显示哪些条目可在「状态栏」分页勾选 |
| 凭据持久化 | `[x]` | 主机配置、应用设置、known_hosts 三类数据合并入**单个加密文件** `ruterm.db`（toasty 0.11 ORM + turso 0.7 AEGIS-256 页级加密，`hosts.password` 列再叠 **AES-256-GCM 字段加密**），全局模式位于 `~/.ruterm/`、便携模式位于 exe 旁 `data/`；解锁两档并存：默认 keyring 随机 32 字节主密钥（零输入），或用户**主密码**（argon2id 派生，可带往他机解锁）。库不可用时降级旧 `sessions.json` 且禁用密码保存，见 §四 |
| 便携 / 全局双模式 | `[x]` | 启动时只探测不创建：exe 同目录存在 `data/` → 便携模式（数据随 exe 走），否则全局模式 `~/.ruterm`；主密码为可选项（设置 / 更换 / 取消均在设置里），见 §四 |
| 自定义标题栏 | `[x]` | 应用自绘顶栏：品牌标识 + 「会话 / 视图 / 终端 / 凭据 / 外观 / 帮助」六档多级菜单（有快捷键的项右侧自动显示按键徽标，最近连接是「会话」下的二级菜单）+ 右侧口令档的 🔑/🔒 + 系统窗口控件；设置 / 侧栏折叠的常驻按钮已撤掉（走菜单与快捷键）；拖拽与窗口控制走 `WindowControlArea` 命中区，交互控件排在 Drag 区之外 |
| 设置界面 | `[x]` | 「会话 → 设置…」/「外观」菜单各分页入口打开设置弹窗（固定 640×470，切分页尺寸不变），**左侧竖排分页导航**（凭据库与主密码 / 外观 / 状态栏 / 终端 / 会话）+ 右侧定高滚动内容区，新设置项继续追加分页 |
| 品牌图标 | `[x]` | 从展示板切分 6 图标变体 + logo + 多尺寸 `.ico`；`build.rs`+winresource 嵌入 exe（任务栏/标题栏） |
| 系统托盘 | `[x]` | 品牌托盘图标 + 悬停提示（含会话数）+ 右键菜单（**动态列出当前会话**，● 活动 / ○ 其它，点击恢复窗口并切标签；显示主界面 / 退出），`App::set_tray_*` + `on_tray_menu_action`，标签增减/切换时自动刷新 |
| 单实例守卫 | `[x]` | 启动时抢 `127.0.0.1:47122` 的带令牌监听：抢到即主实例并后台守门；没抢到就敲门（写令牌 → 等回显 → 唤醒已藏起来的窗口 → 自己退出），避免关闭到托盘后再启动开出第二个进程抢同一份加密库、托盘叠两个图标；令牌对不上（端口被别程序占用）则照常启动，`src/instance.rs`（见 §八） |

### 入口速查（当前布局）

| 操作 | 入口 |
|------|------|
| 上传 / 下载文件 | 边栏「文件」页：工具栏「上传」· 拖入文件 · 行右键「下载到…」（`Ctrl+Shift+B` 切到文件页） |
| 端口转发 | 「终端」菜单 → 端口转发… · `Ctrl+Shift+F`（切到边栏「转发」页） |
| 设置 | 「会话」菜单 → 设置… · `Ctrl+,`；「外观」菜单可直达外观 / 状态栏 / 终端分页 |
| 最近连接 | 「会话」菜单 →「最近连接」**二级菜单**（最多 8 条，点一条即连）· 空工作区首页卡片同源 |
| 主机列表 / 文件页显隐 | 「视图」菜单 → 主机列表 / 文件浏览器（打勾即当前显示的页）· 或点最外缘图标轨 |
| 折叠 / 展开侧栏面板 | 「视图」菜单 · `Ctrl+B`（标题栏不再放该按钮；最外缘的图标轨始终保留） |
| 左右 / 上下分屏 | 「视图」菜单 → 左右分屏 / 上下分屏 · `Ctrl+Shift+D` / `Ctrl+Shift+E`（再次触发退出或换向） |
| 键宏 | 「终端」菜单 → 键宏… · `Ctrl+Shift+M`（Esc 关闭；触发键点录入框后按键录制） |
| 命令片段 | 「终端」菜单 → 命令片段… · `Ctrl+Shift+S`（Esc 关闭；插入不回车 / 执行追加回车；含内置命令页） |
| 命令历史 | 「终端」菜单 → 命令历史… · `Ctrl+Shift+H`（Esc 关闭；可插入 / 执行 / 存为片段） |
| 终端输入补全 | 终端里输入命令时自动弹出候选：`↑↓` 选择 · `Tab` / `Enter` 补全（不执行）· `Esc` 关闭 · 也可鼠标点选 |
| 明暗主题 | 「外观」菜单 · `Ctrl+T` |
| 解锁 / 锁定凭据库 | 「凭据」菜单；口令档未解锁时标题栏出现 🔑，已解锁时显示 🔒 |

> 传输与转发绑定「当前标签的会话」，因此常驻标题栏右侧；标签栏只负责标签本身，
> 避免标签过多横向滚动时把这两个入口挤出可视区。

---

## 二、UI / UX 目标（本次新增，主流 SSH 工具应具备）

### 1. 状态栏增强
- `[x]` 底部常驻状态栏：整体状态 + 活动会话地址 / 认证方式 + 会话计数。
  连接成功后整体状态从「正在连接 地址…」切到「已连接 地址」（只在文案没被别的提示覆盖时替换）。
- `[x]` 连接后展示**远程服务器基本信息**：主机名、操作系统及版本、内存使用、磁盘空间、uptime。
  - 实现：`src/remote.rs`（探测脚本 + `RemoteInfo` 解析/摘要，含单测）、`src/session.rs`
    （shell 启动后另开一条 **exec 通道**常驻跑 `while :; do <探测脚本>; printf 'end\n'; sleep 5; done`，
    8s 首块超时，客户端按 `end` 行切块解析，摘要经 `mpsc` 流式回传给 UI 写入
    `SessionTab.remote_info`）、`src/ssh.rs`（`SideChannels` 旁路通道集合）。
  - 刷新周期 `PROBE_INTERVAL = 5s`：只探一次会让负载 / 内存 / 磁盘永远显示连接那一刻的值
    （已实测：本地按 3× `yes` 压 CPU，状态栏负载从 0.03 涨到 1.24，`pkill yes` 后回落）。
    流中断或探测失败即停止刷新，已显示的条目保持最后一次的值。
  - 关键坑：russh 会把客户端自开通道（exec）的字节**同时**投递给 `Channel` 接收端与
    `Handler::data/extended_data/channel_close/exit_status`，不过滤就会把探测输出灌进终端、
    并在探测通道关闭时误判"远端退出"结束整个会话；故非 PTY 通道一律登记进 `SideChannels` 跳过。
  - 探测脚本按 POSIX sh 编写（`uname` / `/etc/os-release` / `/proc/meminfo` / `df -hP /` /
    `/proc/loadavg` / `/proc/uptime`，非 Linux 回退 `uptime -p`），缺项静默跳过，
    远端禁用 exec 时状态栏该条目直接不显示。
- `[x]` **可配置显示项**：设置弹窗新增「状态栏」分页（入口：设置 → 状态栏，或「外观」菜单 → 状态栏显示项…），
      分「会话信息」（连接地址 / 认证方式）与「远程主机信息」（系统 / 内存 / 磁盘 / 负载 / 运行）两组勾选，
      取消勾选即从状态栏移除，改动即时生效。
  - 实现：`StatusBarPrefs`（`src/app/prefs.rs`，`metrics` 用 `HashMap<id, bool>`，缺省视为显示，
    新增指标不必迁移旧数据）+ `Metric` 枚举（`src/remote.rs`，`id` 兼作持久化键与条目 id，
    `label` 供设置界面与状态栏共用）；`render_status_bar` 按勾选拼装，`RemoteInfo::item(metric)`
    缺项返回 `None` 时继续跳过。
  - 持久化沿用主题的双层路径：库内 `settings` 表 `status_bar` 键（JSON），库不可用时退回
    `settings.json`；旧 `settings.json` 迁移一并带上该键。读回时空串 / 损坏 JSON 回落默认全显示。

### 2. 侧边栏
- `[x]` 可拖拽调整宽度（rgpui `SplitPane::horizontal`，初始比例 0.42，最小宽度 260px：文件表格的五列放不下 160px）。
- `[x]` 折叠 / 展开：「视图」菜单 →「折叠 / 展开侧栏面板」或快捷键 `Ctrl+B`。标题栏原先的折叠按钮已随用户要求撤掉。
      原先折叠后主区顶部还会冒出一条 44px 的 `»` 展开栏，与当时的标题栏折叠按钮重复且白占终端高度，已删除。
- `[x]` **RustRover 式图标轨**：最外缘 42px 窄栏只画图标（主机 / 文件 / 转发），页名交给悬停 tooltip
      （`tooltip_show_delay` 300ms），当前页在外缘画强调色竖条，转发生效条数压成图标角标；
      窄栏独立于 `SplitPane`，面板收起时仍留在窗口外缘，主区能铺满整行；再点当前页图标＝收起面板。
- `[x]` WindTerm 式拖拽：拖到最小宽度即停住，分隔线仍可抓住往大拖；完全收起只交给「视图」菜单 / `Ctrl+B`，不做拖拽阈值自动折叠（此前自动折叠会在展开后拖动时因 `collapse` 前的旧比例经 mouse-down 触发 `on_resize` 而立刻重新收起，已移除）。
- `[x]` 窄宽度下主机行自适应：名称/地址 `text_ellipsis` 截断，认证方式徽标 `flex_shrink_0` 固定不被挤压。
- `[x]` 主机行右键菜单：**编辑 / 删除**（框架 `ContextMenuExt::context_menu` + `PopupMenu`，见本节末条：
      早期手搭 `absolute` 浮层因图层问题点不动，已换框架实现）。
- `[x]` 编辑复用新建表单：`open_form_at(Some(index))` 预填原值，保存原位替换（保留 `id` / `favorite`，不重新分配）。
- `[x]` 删除二次确认：居中模态卡片（半透明遮罩），提示"已保存的密码/口令将一并从加密库清除"；行内不再常驻删除按钮。
- `[x]` 新建/编辑主机改为**居中模态弹窗**（440px 卡片 + 遮罩，端口/用户名双列），不再挤占侧边栏窄面板；
      遮罩点击不关闭（防误丢已填内容），关闭仅走"取消"或标题栏 ×。
- `[x]` 弹窗**高度随窗口封顶**：分组 / 跳板机并排一行，私钥区块只在「私钥」认证下渲染，
      「保存口令」勾选与「私钥口令」标签同排（此前常驻占位把 Agent / 密码表单撑出默认窗口）；
      整卡最大高度取视口高度减 48px，仍超出时字段区自身滚动，标题与底部按钮始终可见。
      加密库不可用时保存口令处只显示"口令仅本次内存持有"。
- `[x]` 折叠后保留窄图标轨道（RustRover 式）：轨道常驻窗口外缘，悬停出页名 tooltip，点图标展开对应页；
      面板收起时终端铺满整行，轨道不再随面板一起消失。
- `[x]` **分组（文件夹）树**：主机列表按分组树组织（折叠三角 + 文件夹图标 + 组名 + 数量），
      右键可**新建分组… / 重命名分组… / 删除分组**，主机行右键「移动到分组」子项带 ✓ 当前归属；
      删除分组只退回归属到「未分组」，**绝不删主机**。折叠态、组顺序、归属均持久化。
  - 实现：`SidebarPrefs`（`src/app/prefs.rs`：`groups` 顺序 + `collapsed` + `assign: HashMap<host_id, group>`，
    含 `add/rename/remove/toggle/assign_to/group_of/forget` 与 JSON 往返单测）；`rename` 同步改写归属，
    `group_of` 校验组仍存在（悬空归属视为未分组），`remove` 只清归属。
  - 持久化刻意**不给 `hosts` 表加列**：整体序列化成 JSON 存加密库 `settings` 表 `sidebar` 键
    （降级时 `settings.json`）。加列要在加密库上做真实 ALTER，而分组只是展示层元数据，代价与风险都低得多。
  - 搜索态：空分组隐藏、折叠强制展开（否则命中的主机看不见）；仅在搜索无命中时留一行「无匹配主机」。
    「未分组」是兜底桶，**里面没有主机时整行不渲染**（否则刚建完分组、一台主机都没加时，它会和空的新组
    并排出现，像凭空多了一个组）；自定义空组保留，那是用户有意建的容器。
    首启动（没主机也没建组）不再画空壳「未分组」，也不再塞「暂无主机…」提示——树顶已有本机 shell 列表，
    空着比一排灰字干净，新建入口仍在空白处右键与「会话」菜单。
- `[~]` 主机**跨分组拖拽**移动（当前用右键「移动到分组」，拖拽待做）。
- `[x]` **本地终端并入主机列表**：树顶内置「本地终端」**可折叠分组**（折叠三角 + 终端图标 + 名称 + 数量），
      子项是探测到的每个本机 shell（PowerShell 7 / Windows PowerShell / Git Bash / 命令提示符 / WSL），
      每项左边是该 exe 的**系统默认图标**，点一下直接开对应终端标签。
      它不入凭据库，故不参与分组/收藏/删除；搜索按「本地终端」与各 shell 名匹配，命中即展开只留有命中的子项。
      折叠态存进 `sidebar` 键的 `local_collapsed` 字段，收起来后重启仍是收起的。
      右键菜单：一键扫描本机 shell… / 新建主机… / 新建分组…；「会话」菜单 → 本地终端（`Ctrl+Shift+T`）开列表第一项。
- `[x]` **新建入口收敛**：侧边栏顶部常驻的「+ 新建主机」大按钮已去掉，顶部空间整块留给列表；
      新建走**主机行 / 分组行 / 列表空白处**右键菜单，或标题栏「会话」菜单「新建主机…」（`Ctrl+N`）。
- `[x]` **主机行右键菜单改用框架 `PopupMenu`**：手搭的 `absolute` 浮层留在普通图层，会被后画的兄弟节点压住
      hitbox，菜单项收不到 mouse-up（鼠标能悬停、点击只关菜单），改用框架的 `deferred + anchored + with_priority(1)`
      独立图层后正常，并顺带拿到阴影 / 分隔线 / 图标 / ✓ / 危险色 / 贴边收敛。
      空白处的菜单必须是行的**兄弟**节点而非祖先——`ContextMenu` 包装器在子节点之后注册自己的 hitbox，嵌套会一次弹两层。
- `[~]` **分组内排序**：三种模式已落地——手动、按名称（大小写不敏感）、按最近连接（未连过的回落手动序）。
      手动模式下主机行右键「上移一位 / 下移一位 / 移到组内最前」，排序序列存 `sidebar` 键的 `order` 字段
      （跨组一条序列，渲染时只取组内相对次序）；排序方式存同键 `sort` 字段，空白处右键「排序方式」切换。
      收藏与非收藏各自成桶，★ 恒在桶上方，互不穿插；非手动模式下移序项置灰。余下：拖拽定序。

### 3. 主题
- `[x]` 明 / 暗主题切换（rgpui `Theme::change(ThemeMode)` + 配色统一走 `cx.theme()` 语义色），标题栏「外观」菜单（当前项打勾）+ `Ctrl+T`。
- `[x]` 启动项记忆上次主题选择（持久化到加密库 `settings` 表的 `theme` 项，库打开/解锁完成后自动应用；
      库不可用时回退读写 `settings.json`）。
- `[x]` 终端配色方案：设置「终端」分页下拉切换 rgpui-term `ThemeManager` 的 **9 套内置预设**
      （One Dark/Light、Solarized Dark/Light、Dracula、Nord、Gruvbox Dark/Light、GitHub Light），
      改动即刻刷到所有已开终端并写入加密库 `settings` 表 `terminal` 键（降级时 `settings.json`）。
      另有一套「自定义」：`TerminalColors`（前景 / 背景 / 8 普通色 / 8 明亮色，均存 `#RRGGBB`）
      经 `TerminalThemeConfig::to_theme` 注册成 `ThemeManager` 的「自定义」方案；
      编辑器 18 个十六进制输入框实时预览色块并即时刷到已开终端，非法格式不提交、
      「恢复 One Dark」一键复位；旧版 JSON 缺 `custom` 时按 One Dark 补齐，坏值由 `clamp` 修复。
      实现见 `src/app/appearance.rs` 的 `render_custom_colors` / `commit_custom_color` / `register_custom_theme`。
- `[~]` 终端字号（10–32 px）与行高（1.0–2.0）步进调节，与配色同分页、同持久化；
      新建终端经 `TerminalView::new_with_style` 取当前偏好，已开的经 `text_style_mut()` 原地刷新。
      终端字体族沿用默认等宽（`FiraCode Nerd Font` + 平台回退），UI 字号未开放。

### 4. 国际化（i18n）
- `[ ]` 接入 rgpui `I18nManager`，抽取界面文案为 key。
- `[ ]` 首批语言：中文 / English；语言切换即时生效并持久化。
- `[ ]` RTL 基线（框架已支持 `TextDirection`，暂只保证 LTR 正确）。

### 5. 自定义标题栏
- `[x]` 应用自绘标题栏替换系统标题栏（`TitlebarOptions::appears_transparent` + `app_owns_titlebar_drag`，
      品牌标识在左）。**不用** rgpui `TitleBar` 组件：它把 children 整体塞进 `WindowControlArea::Drag` 命中区，
      而 Windows `WM_NCHITTEST` 取**第一个**命中的控制区（父级 Drag 先入表），栏内按钮一律被判定为 caption，
      hover 正常但 click 永远不触发。改为自绘一行，把拖拽区拆成"品牌段 + 中间空档"两个兄弟节点，
      交互控件落在 Drag 区之外（实测：拖拽移动、栏内按钮点击均正常）。
- `[x]` 保留窗口拖动、双击最大化、系统按钮：拖拽段登记 `WindowControlArea::Drag`（HTCAPTION，双击最大化由系统处理），
      最小化 / 最大化 / 关闭分别登记 `Min` / `Max` / `Close`（HTMINBUTTON / HTMAXBUTTON / HTCLOSE），
      关闭仍经 `on_window_should_close` → 隐藏到托盘。
- `[x]` **标题栏多级菜单**：按域拆成六档，避免「会话」一栏堆到底——
      「会话」（新建主机 `Ctrl+N` · 导入 `~/.ssh/config` · **最近连接 ▸ 二级菜单** · 本地终端 ·
      设置… `Ctrl+,` · 隐藏到托盘 · 退出）·
      「视图」（主机列表 / 文件浏览器显隐打勾 · 折叠 / 展开侧栏 `Ctrl+B` · 左右 / 上下分屏 · 标签切换）·
      「终端」（终端内查找 `Ctrl+F` · 同步输入 · 命令发到所有会话 · 键宏 `Ctrl+Shift+M` · 端口转发）·
      「凭据」（解锁 · 主密码与凭据库设置 · 锁定 · 末尾只读显示当前档位）·
      「外观」（暗色 / 亮色打勾单选 + 切换明暗主题 `Ctrl+T` · 外观设置 · 状态栏显示项 · 终端外观分页入口）·
      「帮助」（只留版本号）。
      菜单用 `Button::dropdown_menu` + `PopupMenu`，构建器是 `Fn`，实体句柄需在闭包内重新 `clone` 再交给各项 `on_click`；
      **二级菜单**用 `PopupMenu::submenu(label, window, cx, builder)`，同样因为 builder 每次重渲染都会再跑一遍，
      捕获的列表要在闭包内 `clone()` 出副本再 `fold` 成条目。
- `[x]` **快捷键随菜单项原生显示**：带键的命令声明为 action（`actions!(ruterm, [ToggleSidebar, ToggleFilePanel, ToggleTheme, NewHost, NewLocalTerminal, OpenSettings, OpenForward, NextTab, PrevTab])`），
      `main()` 里 `cx.bind_keys` 绑定 `Ctrl+B` / `Ctrl+T` / `Ctrl+N` / `Ctrl+Shift+T` / `Ctrl+,` /
      `Ctrl+Shift+B` / `Ctrl+Shift+F` / `Ctrl+Tab` / `Ctrl+Shift+Tab`，根元素用 `.on_action(cx.listener(...))` 接收；
      菜单项改挂 `.action(Box::new(X))`，rgpui 的 `PopupMenu::render_key_binding` 会按该 action 的键绑定
      在项右侧自动渲染按键徽标（`Kbd`），因此标签里不再手写 `(Ctrl+B)`，「帮助」菜单里那份纯文字快捷键清单也删掉了。
      点击仍生效：`PopupMenuItem` 只有 `on_click` 没有 shortcut 字段，但 `confirm()` 里 handler 优先于 action，
      单独挂 action 一样能派发（实测菜单点击与键盘两条通路都通）。
- `[x]` 设置入口收进标题栏菜单：「会话 → 设置…」（`Ctrl+,`）打开**分页式**设置弹窗（凭据库与主密码 / 外观 /
      状态栏 / 终端 / 会话），「外观」菜单另有各分页直达项；🔑 / 🔒 随锁定态切换，
      侧边栏头部品牌行与常驻 ⚙ 按钮移除。菜单项可**直达指定分页**（弹窗已开时只换分页，不重复创建）。
- `[x]` **右侧按钮带分层**：内容级操作（传输 / 转发）已迁到边栏导航与菜单里，标题栏右带只剩口令档才出现的
      🔑/🔒，与整高整宽的系统窗口按钮之间加 1px 竖分隔线；图标一律用 rgpui 矢量图标而非文本字形
      （图标集里没有钥匙/锁，这两个仍用 emoji）。转发 / 文件页入口绑定的是"当前会话"，属窗口级操作，
      但放在标签栏一多就会被横向滚动挤出视野，故收进边栏竖向导航；转发生效条数压成「转发」图标角标
      （见 §二.2 与 §八）。侧栏折叠与设置同样从前两条撤出（见 §二.2 与上条），标题栏只留全局操作。
- `[ ]` ~~会话标签并入标题栏（类 WindTerm 的"标签即标题栏"）~~：**已实现又撤回**——用户明确要求终端标签不要放在
      自定义标题栏上，标签栏回到主区顶部（44px 行高、平铺无描边、活动标签顶部强调线、仅活动标签显示 `×`）。
      标题栏只留品牌 / 菜单 / 会话操作按钮（传输、转发）与窗口控件，中间整段仍是拖拽区。技术上标签条与 Drag 段必须互为
      **兄弟**节点（父级 Drag 会吞掉标签点击，同 §二.5 首条 `TitleBar` 未采用的原因），若日后重提可直接复用该结论。
- `[ ]` Windows 下与 Mica 材质 / 圆角协调（当前保留 `WS_THICKFRAME`，DWM 仍绘制圆角）。

### 6. 分屏窗口
- `[x]` 水平 / 垂直分屏（一个窗口内并排多个终端会话）：窗口级两标签配对，`Ctrl+Shift+D` 左右、
      `Ctrl+Shift+E` 上下，「视图」菜单同样入口。固定有序（左/上为配对的前一个标签），不随焦点交换位置；
      同方向再次触发退出分屏、反方向触发即换向。切换/新建到配对之外的标签、关闭任一侧标签都会自动退出分屏。
      焦点切换靠点击面板（点击即激活对应标签并聚焦其终端），状态栏与标签栏同步。
- `[x]` 复用 rgpui `SplitPane` 作为布局基座：分隔条可拖拽（拖动时终端跟随重排宽度/高度），
      两侧最小 240px；`DockArea` 多区停靠暂不做。
- `[ ]` 单屏最大化、拖拽重排（把一个面板拖成新分屏/交换位置）。

### 7. 其它主流能力
- `[x]` 主机列表搜索：侧边栏顶部搜索框，按展示名 / `user@host:port` 子串实时过滤（无匹配显示"无匹配主机"）。
- `[x]` 主机收藏：每行 ★/☆ 切换按钮（`stop_propagation` 不触发连接），收藏主机稳定排序置顶，`favorite` 字段持久化到加密库 `hosts` 表。
- `[x]` **设置弹窗（左侧竖排分页）**（「会话」菜单 → 设置…，或「外观」菜单各分页入口）：「凭据库与主密码」分页显示数据目录与解锁方式、
      设置 / 更换 / 取消主密码、锁定凭据库；「外观」分页放明暗主题；「状态栏」分页勾选底部条目；
      「终端」分页调字号 / 行高与配色方案（见 §二.3）。
      弹窗为固定尺寸，切分页不再忽大忽小，内容超出时由分页自身滚动。
      新增设置项继续追加分页即可，主密码相关操作不再散落在侧边栏。已设主密码时标题栏额外出现
      🔒，点击即锁定库（丢弃句柄与内存密钥，需口令重新解锁）。
- `[~]` 设置项扩展：状态栏显示项（见 §二.1）、终端字号 / 行高 / 配色方案与自定义 16 色（见 §二.3）已完成；
      UI 字体等继续按分页追加（默认 shell 已明确不做，见 §五.3）。
- `[~]` 主机分组（已完成，见 §二.2）、组内排序（已完成：手动 / 按名称 / 按最近连接，见 §二.2），
      **最近连接**已完成：「会话」菜单 →「最近连接」**二级菜单**列出最多 8 条最近成功连接的主机（新→旧、去重、
      主机删除后自动消失），点击走与侧栏一致的取密 / 连接流程；id 列表存 `settings.recent`，不落凭据。
      余量：拖拽定序（待做）。
- `[x]` 断线提示 + 手动 / 自动重连、连接超时：终端 `CloseTerminal` 后保留标签与回滚内容，顶部浮出
      「远程通道已关闭」横幅，回车 / 「重新连接」在原标签内重建会话；`channel_eof/close/exit_status`
      经 Handler 信号结束 `handle.await` 并释放连接，关标签也会主动断开 SSH。
      失败收场时横幅第二行直接给出断开原因（多行压一行、过长截断），「复制错误」把未截断全文写进剪贴板并在状态栏回执。
      连接意外中断（网络断开等，表现为会话任务报错）时按 **1 / 2 / 5 / 10 / 20 秒退避自动重连**，
      最多连续 5 次，横幅显示进度并可取消；会话稳定 30 秒后计数重置；远端主动 `exit`、首次连接失败、
      凭据被拒均不自动重连（避免把写错的地址或坏凭据反复重试）。建连 → 认证 → 开 shell 整体有
      **连接超时**（默认 15 秒，设置「会话」分页可调 5–120 秒），超时明确报错而不是无限等待。
  - 实现：`SessionPrefs`（`src/app/prefs.rs`：`auto_reconnect` + `connect_timeout`，双层持久化到
    加密库 `settings` 表 `session` 键 / 降级 `settings.json`，越界值夹回区间并有单测）；
    `session.rs` 用 `tokio::time::timeout` 包住建连与认证，shell 开启时经 `ready` oneshot 通知 UI
    记录 `connected_at`；`SessionTab` 记录 `auto_retry_attempt / auto_retry_pending / connected_at`，
    到点任务用 `background_executor().timer` 等待，并以终端实体身份 + `ended` 校验防止迟到任务误重连。
- `[x]` **键宏**：「终端」菜单「键宏…」/ `Ctrl+Shift+M` 打开管理器（列表 ⇄ 编辑表单，640×500 居中弹窗）。
      列表每行：启用开关、名称（留空显示「未命名宏」）、`触发 ctrl-alt-1 · N 步 · 用 M 次`，右侧 ▶执行 / 编辑 /
      两步确认删除；顶部搜索框按名称与触发键过滤，底部「导入… / 导出…」走系统文件选择器（默认 `ruterm-macros.json`）。
      编辑表单：名称 / 触发键（可留空＝不绑键）/ 步骤列表（发送内容 + 步后延迟毫秒，可增删），
      转义 `\n` 回车、`\t` Tab、`\e` ESC、`\\` 反斜杠。触发键要求带 Ctrl / Alt 或为 F1–F12，
      规范化成小写 canonical 串再存，保留键表（应用快捷键与终端控制字符）与重键在保存时拒绝并就地报错。
      触发键录入用 rgpui `components::hotkey_input::HotkeyInput`（**点一下录入框、按下组合键即录制**，
      × 清空＝不绑键，同设置里快捷键的交互），不再要求手打 `ctrl-alt-1` 串；录制态由
      `intercept_macro_key` 顶部分支接管：正在 `is_recording()` 时把按键 `stop_propagation` 后直接喂
      `capture_keystroke`，所以录制 `Ctrl+T` 这类全局键不会被动作分发抢走、Esc 只退出录制不关弹窗；
      录到的 `HotkeyValue` 还原成 `Keystroke` 走同一条 `canonical_trigger` / `normalize_trigger` 校验，
      手输与录制两条路产出同一个存储串。
      触发走 `App::intercept_keystrokes`（在动作分发之前），仅当活动终端持有焦点且无浮层时命中；
      命中后 `stop_propagation` 并按步 `Terminal::input`（与同步输入同一条 Programmatic 通道，不回流镜像），
      步间用后台定时器等待延迟。模型与单测在 `src/macros.rs`，界面在 `src/app/macros_ui.rs`，数据存加密库
      `settings.macros`（降级 `settings.json`）。已知边界：暂不支持录制（宏内容）、循环、条件与按会话绑定。
      面向使用者的建法与五个案例见 README「键宏使用示例」。
- `[~]` 命令片段库、快捷命令、多主机同时执行：**命令片段与内置命令已完成**（「终端」菜单 / `Ctrl+Shift+S`，
      我的片段：名称 + 单行命令 + 可选备注，搜索、插入 / 执行、编辑与两步删除；内置命令：按平台
      （Windows / Linux / Unix / 通用）一级分类、系统信息 / 软件包 / 网络 / 容器等二级分组，默认看当前
      会话平台、可切换查看其它平台，只读并可「存为我的片段」。数据存加密库 `settings.snippets`，
      见 §一快照）；**命令历史已完成**（`Ctrl+Shift+H`，从终端回显收集、300 条上限、搜索 / 复用 / 存片段，
      存 `settings.history`，见 §一快照）；**终端输入补全已完成**（输入时自动弹候选，`↑↓` 选择、
      `Tab` / `Enter` 只补全、`Esc` 关闭，内置命令按当前会话平台过滤、与我的片段共用，见 §一快照）；
      多主机同时执行已有「命令发到所有会话」（见 P0 同步输入）。
      余量：片段变量替换、多行脚本、按会话绑定、导入导出。
- `[ ]` 终端日志记录与导出、会话保存 / 恢复。
- `[x]` `~/.ssh/config` 导入与解析：`src/ssh_config.rs` 逐行读 `Host` 块，取 `HostName` / `User` / `Port` /
      `IdentityFile`（支持 `Key=value` 语法、`#` 注释仅在行首或空白后生效、`~` 展开），通配符别名（`*` / `?`）
      与 `!` 排除项、`Match` 块整体跳过，同一别名只取第一块；每个具体别名生成一台主机、展示名即别名，
      `HostName` 缺省回落到别名、端口缺省 22、用户回落到 `USER`/`USERNAME`（再退 `root`）。
      导入入口有两处：「会话」菜单与侧边栏列表空白处右键；按 `user@host:port`（忽略大小写）或同名去重，
      私钥文件在本机不存在时改用 Agent 认证并计入提示，避免导入后立刻连不上。解析器 4 组单测覆盖注释、
      通配符、`Match`、重复别名、等号语法与空文件。
- `[x]` **SFTP 上传 / 下载**：边栏「文件」页（「视图」菜单 / `Ctrl+Shift+B`）里工具栏「上传」或拖入文件即传（拖动时面板底部浮一条「松开即上传 N 个文件到 <目录>」，悬在文件夹行上即改传到该子目录），行右键「下载到…」，
      借用当前标签的 SSH 连接
      开 SFTP 子系统（不新建连接、不再认证）。`SessionCommand::Transfer`（`session.rs`）让会话任务在收场
      `select!` 循环里服务传输指令，连接存活改用每秒 `handle.is_closed()` 轮询（`Handle` 不能再当
      Future 消费）；`transfer.rs` 负责 64 KiB 分块搬运、~10 次/秒进度节流、`~` 展开与「上传到目录
      自动补文件名」，下载失败删除残缺本地文件；SFTP 通道 id 登记进 `SideChannels`，字节与控制消息
      不会被灌进终端。弹窗显示字节进度条与百分比，错误同时写弹窗与状态栏；关标签时弹窗一起关闭。
      `client_config()` 把在途写入提到 32 个 × 64 KiB 分片（读取并发 8），吃满 OpenSSH 默认 2 MiB
      通道窗口；单请求超时 120 秒，超时报错点名该请求，不再退化成含糊的「传输失败」。
      余量：目录浏览 / 递归传输、断点续传、传输队列与速度限制。
- `[x]` **端口转发（-L / -R / -D，规则可保存）**：标题栏「⇆ 转发」按钮（也可走「会话」菜单 / `Ctrl+Shift+F`）打开弹窗。
      WindTerm 式的「先保存、再开关」：弹窗默认只列「已保存的转发」，点底部「设置…」才展开新增 / 编辑表单
      （避免每次进弹窗都对着表单误改已有规则）；表单填方向（本地 / 远程 / 动态）、备注名、端口与目标地址，
      点「保存规则」回到列表；列表每行一个开关即时启停（关闭会连同该规则的活动连接一起掐断），也可勾
      「连接时自动开启」随连接拉起，不必每次重填。规则按 `user@host:port` 存进加密库 `settings` 表的 `forwards` 键
      （库不可用时退回 `settings.json`），同一方向同一监听端口视为同一条、原位覆盖；表单名字留空再保存
      同一条规则时沿用旧备注；删除规则会先停掉再移除。关掉弹窗后标题栏按钮仍显示生效条数（`⇆ 转发 N`）。
      - 本地（-L）：监听 `127.0.0.1:<本地端口>`，连接经当前连接以 `direct-tcpip` 通道转到**服务器视角**的目标地址
        （内网 Web、数据库、跳板机皆可）。
      - 远程（-R）：向服务器申请 `127.0.0.1:<端口>` 监听（`tcpip-forward`），服务器 accept 后以
        `forwarded-tcpip` 通道回传，再拨本机目标，适合把本地服务临时暴露给远端。服务器侧同样只请求回环地址，
        不开放到 LAN / 公网；`Handler::server_channel_open_forwarded_tcpip` 里 `reply.accept()` 后登记进
        `SideChannels` 并把通道经 `mpsc` 递回会话任务。
      - 动态（-D）：监听 `127.0.0.1:<端口>`，每条连接先在自己的任务里做 RFC 1928 握手（仅无认证 + CONNECT，
        ATYP 支持 IPv4 / 域名 / IPv6），拿到目标后把套接字递回会话任务开 `direct-tcpip`；**成功应答
        (`0x00 0x00`) 必须等通道打开再回**，否则客户端会在通道就绪前就开始灌数据。目标不可达回 `0x05`、
        非 CONNECT 回 `0x07`、地址类型不支持回 `0x08`，让 curl / 浏览器立刻报错；握手全程 10s 超时，
        慢客户端不会卡住 accept 循环。域名原样交给 sshd 解析，等价于「在服务器侧解析内网域名」。
      - 会话层：`SessionCommand::{ForwardStart, ForwardStop}` 在会话 `select!` 循环里处理；监听任务
        accept 到本地连接后只把套接字递进来，由持有 `Handle` 的会话任务开通道（`Handle` 不可克隆、
        也不是 `Sync`，只能在会话任务内使用）。每条规则是一个 `ActiveForward { listener, pumps }`，
        `pumps` 用 `JoinSet` 管理该规则的活动连接；停用时 abort 监听并 `shutdown` 连接，不会出现"监听关了、流量照旧"。
        远程转发停用时先 `cancel_tcpip_forward` 让服务器撤掉监听，再收本地泵。
        本地 / 动态转发的 accept 结果用 `AcceptedForward::{Direct, Socks}` 区分，SOCKS 分支额外携带协商出的目标。
      - 安全 / 正确性：三种转发都只绑 / 只申请 `127.0.0.1`，不把端口暴露到局域网——SOCKS5 无认证，
        更没有理由开放；direct-tcpip 与 forwarded-tcpip 通道 id 一律登记进 `SideChannels`，否则其字节会灌进终端、
        close/EOF 还会被误判为"会话结束"；开通道有 10s 超时，远端拒绝或超时只写日志 / 状态栏，不影响交互会话。
        服务端 sshd 必须允许 TCP 转发（`AllowTcpForwarding yes`，部分镜像默认 `no`）：被禁时本地端口能连上，
        但通道随即被拒、连接立刻断开，验证时先用 `curl` 打通一次再交付。
- `[x]` **跳板机（ProxyJump）**：主机表单「跳板机」下拉（默认「直连（无跳板机）」）选另一台已保存主机；保存后
      目标与跳板机的绑定存进加密库 `settings` 表的 `jumps` 键（`{目标 id: 跳板机 id}`，不给 `hosts` 表加列，
      免 ALTER；库不可用时降级为「只给直连」，因为降级模式没有稳定 id）。
      - 连接：`SshTarget.jump: Option<Box<SshTarget>>` + `jump_chain()`（**从最外层开始**）。会话任务按链逐跳建连：
        第一跳走 `russh::client::connect`，后续每跳用上一跳 `Handle::channel_open_direct_tcpip(...)` 开通道，
        再把 `channel.into_stream()` 交给 `client::connect_stream` 做一次完整的 SSH 握手；到最后一跳才开 PTY / shell。
        终端先打印「经跳板机连接：A → B」一行，让用户看见实际路径。
      - 存活：每跳的 `Handle` 与 `SideChannels` 留在会话任务里（`hops`），跳板机的 direct-tcpip 通道 id 登记进该跳的
        旁路集合，字节不会漏进终端、也不能被当成会话结束信号；目标跳的 handler 才拿会话的 `ended` / `forwarded` 通道。
      - 指纹：每一跳各自构造 `SshTerminalHandler`，按该跳的 `host:port` 独立做 TOFU（写加密库 known_hosts），
        任一跳指纹不符即整链拒绝——跳板机不比直连更可信。
      - 凭据：跳板机的密码 / 口令 / 内嵌私钥只从**本次运行的内存缓存或加密库**取（点击目标时按跳板机 id 查库并预热缓存），
        绝不为了跳板机二次弹窗、也不落明文；取不到时状态栏提示「请先单独连接它并勾选「保存密码」」。
      - 限制：只支持一级跳板（下拉排除目标自己与已挂跳板机的主机，保存时再校验一次），删除跳板机时自动解除引用、
        把原目标改回直连。
      - 新增 `AuthMethod` / 转发逻辑零改动：跳板链只影响建连阶段，后续交互、SFTP、端口转发都作用在最后一跳的 `Handle` 上。

### 8. 系统托盘
- `[x]` 品牌托盘图标 + 悬停提示 + 右键菜单（显示主界面 / 退出），经 `App::set_tray_icon/tooltip/menu` + `on_tray_menu_action`。
- `[x]` 关闭到托盘（点窗口 X 经 `Window::on_window_should_close` 返回 false 改调 `hide_window()`，会话后台保活；托盘"显示主界面"经 `WindowHandle::update(..).activate_window()` 恢复，"退出"经 `cx.quit()` 直接投 `WM_QUIT` 真正结束）。
- `[x]` 托盘菜单动态列出当前活动会话，点击可跳转到对应标签：`sync_tray` 在标签新增 / 关闭 / 切换时重建菜单，
      条目形如 `● user@host`（活动）/ `○ user@host`（其它），id 为 `tab:<id>`；点击后先 `activate_window()`
      再 `activate_tab()`，窗口隐藏时也能一步回到目标会话。
- `[x]` 托盘图标 tooltip 反映会话数：无会话时为 `RuTerm 拾连`，有会话时 `RuTerm 拾连 · N 个会话`。
- `[x]` **单实例守卫**（`src/instance.rs`）：关闭默认藏进托盘，于是"再点一次 exe"极易开出第二个进程——
      两个进程会同时打开同一份 toasty/turso 加密库（写冲突、锁库），托盘里还会叠出两个图标，用户找不到原先那个隐藏窗口。
      做法是**先占坑再敲门**：启动时尝试 `TcpListener::bind` `127.0.0.1:47122` 并置 `nonblocking`，
      抢到即主实例（`Claim::Primary`），交一份令牌口令给后台守门任务；`AddrInUse` 则当次实例（`Claim::Activated`），
      连上去写一行 `ruterm-instance-v1`、等对方原样回显（600ms 超时判失败），成功就打印"已在运行"后直接退出，
      由主实例 `window.activate_window()` 把窗口带回前台。回显对不上说明端口被**别的程序**占着，
      此时宁可用 `Claim::Unprotected` 正常启动，也不因为一个无关监听器把 RuTerm 锁在门外。
      关键坑：`tokio::net::TcpListener::from_std` 必须在 tokio 执行器里跑，`cx.spawn` 用的是 rgpui 自己的前台执行器、
      没有 IO reactor（首次实测即 panic「there is no reactor running」），所以守门任务用
      `rgpui::tokio::Tokio::spawn` 起在 tokio 池上，再用 `tokio::sync::mpsc` 无界通道把"唤醒" hop 回主线程更新窗口；
      `JoinHandle` 必须被 await 持有（Task 被丢弃会连带取消 tokio 侧的监听）。

---

## 三、架构基座复用（来自 rgpui）

RuTerm 不自造 UI 轮子，优先复用 rgpui 现成子系统：

| 需求 | 复用组件 |
|------|----------|
| 状态栏 | `components::status_bar::{StatusBar, StatusBarItem}`（已用） |
| 侧边栏可拖拽宽度 / 折叠 | `SplitPane`（已用）/ `TaskPane` 自动隐藏轨道（备用） |
| 分屏 / 停靠 | `SplitPane`、`DockArea` |
| 自定义标题栏 | `TitlebarOptions::appears_transparent` + `app_owns_titlebar_drag` + `WindowControlArea`（Drag/Min/Max/Close）；`title_bar::TitleBar` 因 Drag 命中区吞掉栏内按钮点击而未采用，改自绘一行 |
| 标题栏菜单 / 设置分页 | `menu::{PopupMenu, PopupMenuItem, DropdownMenu}`（`Button::dropdown_menu`）；菜单项挂 `.action(...)` 即由 `PopupMenu::render_key_binding` 自动在右侧渲染 `Kbd` 按键徽标（`PopupMenuItem` 本身无 shortcut 字段）；设置分页导航自绘（`h_flex` 行 + 选中项强调色竖条 + 粗体，不用 `tabs::TabBar`——它只有横向模式） |
| 主题切换 | `theme::Theme::change` |
| 终端配色 / 字号 | `rgpui-term`：`ThemeManager`（全局，9 套预设）+ `TextStyle`；新建走 `TerminalView::new_with_style`，已开终端经 `text_style_mut()` 原地刷新 |
| 系统托盘 | `tray::{TrayMenuItem, TrayIconEvent}` + `App::set_tray_*`（已用） |
| SFTP 传输 / 本地端口转发通道 | 都借用已认证的 SSH 连接：`handle.channel_open_session()` + `request_subsystem(true, "sftp")`，或 `handle.channel_open_direct_tcpip(远端主机, 端口, ..)`；两类通道 id 必须登记进 `SideChannels`（russh 会把客户端自开通道的字节同时投给 `Handler`，不登记就灌进终端、close 还会误判会话结束）。`Handle` 不可克隆且非 `Sync`，所以监听线程 accept 到的套接字经 `mpsc` 递回会话任务再开通道；双向搬运用 `tokio::io::copy_bidirectional`，每条转发用 `JoinSet` 归组，停止时 abort 监听并 `shutdown` 连接 |
| 端口转发监听 | `tokio::net::TcpListener` 只绑 `127.0.0.1`（不对局域网开放），accept 循环遇到发送端关闭即退出，监听随之释放；通道打开限时 10s，超时 / 远端拒绝只记日志，不影响交互会话 |
| SOCKS5 动态转发 | 每条连接在独立任务里握手（`read_exact_timeout` 全套 10s 上限），成功后把 `(TcpStream, target)` 经 `mpsc` 递回会话任务开 `direct-tcpip`；**成功应答在通道打开后才写**（`0x05 0x00 0x00 0x01 0 0 0 0 0 0`），失败回 `0x05` / `0x07` / `0x08`；拒绝前先把整条请求读完，否则未读数据导致 RST、客户端收不到应答 |
| 跳板机链式建连 | 第一跳 `russh::client::connect`，后续每跳 `previous.channel_open_direct_tcpip(target.host, port, "127.0.0.1", 0)` → `russh::client::connect_stream(channel.into_stream())`；跳板机通道 id 登记进该跳自己的 `SideChannels`，每跳各自 `SshTerminalHandler` 做 TOFU；跳板 `Handle` 必须活到会话结束（留在 `hops` 里），否则通道随句柄关闭 |
| 国际化 | `i18n::I18nManager` |
| 组合键录入（键宏触发键） | `components::hotkey_input::{HotkeyInput, HotkeyInputState, HotkeyValue}`：点一下进录制态、`capture_keystroke` 收键、`hotkey()` 取值、× 清空。该模块**不在 crate root 再导出**，要写全路径 `rgpui::components::hotkey_input::…`；录制期间必须由 `App::intercept_keystrokes`（早于动作分发）先吞掉按键，否则 `Ctrl+T` 这类全局键会先触发应用动作、Esc 会直接关掉弹窗。`HotkeyValue{modifiers,key}` 字段全 pub，可还原成 `Keystroke` 复用既有的规范化 / 校验函数，录制与手输因此产出同一个存储串。 |
| 单实例 / 唤醒已隐藏的窗口 | `std::net::TcpListener`（`127.0.0.1` 固定端口 + 令牌握手）绑定权当锁；后台侧 `rgpui::tokio::Tokio::spawn` + `tokio::net::TcpListener::from_std`，唤醒走 `Window::activate_window()`（托盘"显示主界面"复用同一条通路）。 |
| 命令注册 / 快捷键 | `actions!` 声明动作 + `cx.bind_keys`（App 级 `KeyBinding`）+ 根元素 `.on_action(cx.listener(...))`；`commands::CommandRegistry`、`key_context` 备用。局部覆盖用 `KeyBinding::new("tab", NoAction {}, Some("Terminal"))`：`Keymap::bindings_for_input` 按上下文深度降序解析，遇到 `NoAction` 即 `break`，可截掉框架在 `Root` 上下文里对同一按键的绑定（终端 Tab 补全即此法恢复） |
| 标签栏配色 | 主题语义色 `tab_bar` / `tab_active` / `tab_active_foreground` / `tab_foreground`（明暗两套自带区分），活动标签顶部叠 2px `primary` 强调线（不再整圈描边）；不硬编码颜色 |
| 主机行 / 分组行 / 空白处右键菜单 | `menu::context_menu::{ContextMenuExt::context_menu, PopupMenu, PopupMenuItem}`：包装器把元素转 `relative` 并在 `deferred(anchored().snap_to_window_with_margin(8px).with_priority(1))` 的**独立图层**渲染菜单。手搭 `absolute` 浮层不可行——它留在普通图层，被后画兄弟节点压住 hitbox，菜单项只吃得到 hover 吃不到 mouse-up。`context_menu` 要求元素同时是 `InteractiveElement + ParentElement + Styled`，ElementId 由内层 `.id()` 派生；包装器的 hitbox 注册在子节点**之后**，故两层菜单不能嵌套（一次右键弹两个），空白区菜单须作行的兄弟节点。`PopupMenuItem::on_click` 的闭包要求 `Send + Sync`，捕获 `Entity<T>` 后 `entity.update(cx, ..)` 即可拿到 `&mut RuTermApp`（`Entity` 恒 `Send + Sync`，内部是 `PhantomData<fn(T)->T>`） |
| 布局约束范式 | `examples/showcase/src/bin/app_suite.rs`（flex_grow + min_h(0) 有界布局范式，状态栏布局修复即据此） |
| exe 的系统默认图标 | 无现成组件：`SHGetFileInfoW(SHGFI_ICON \| SHGFI_LARGEICON)` 取 `HICON` → `GetIconInfo` + `CreateDIBSection`（32bpp 顶向下）+ `DrawIconEx(DI_NORMAL)` → BGRA 转 RGBA（`alpha == 0` 的 RGB 归零）→ `png::Encoder` → `rgpui::Image::from_bytes(ImageFormat::Png, ..)`。`windows 0.62`（`Win32_UI_Shell` / `Win32_Storage_FileSystem` / `Win32_Graphics_Gdi` / `Win32_UI_WindowsAndMessaging`）+ `png 0.18`，无新增大件。另注：`extract_icon` 在 rsproxy 镜像上不可得，故手写 |
| 后台阻塞任务（扫描 shell） | **不能用 `tokio::task::spawn_blocking`**：rgpui 的 `cx.spawn` 跑在它自己的执行器上，没有 tokio reactor，直接 panic「there is no reactor running」。正确写法是先 `let runtime = rgpui::tokio::Tokio::handle(cx);` 拿到 `tokio::runtime::Handle`，再 `runtime.spawn_blocking(f).await` |

---

## 四、凭据与安全存储（重点，用户明确诉求：讨厌反复输入密码）

### 目标
- `[x]` 主机配置、用户名、私钥路径、（可选）密码迁入**结构化存储**：**toasty 0.11 ORM**
      （声明式 `#[derive(Model)]`）+ 底层 turso 0.7 页级加密，单文件 `ruterm.db`；
      表 `hosts(id, name, host, port, user, auth, key_path, favorite, password)`、
      `settings(name, value)`、`known_hosts(host_port, fingerprint, algorithm)`。
      分组、备注、备份恢复为后续增量。
- `[x]` **一个文件装下所有本地数据**：`settings.json`（主题等）与 `known_hosts` 文本文件
      全部并入同一加密库；`.ruterm` 目录不再有第三个业务文件。
- `[x]` **便携 / 全局双模式**（`src/paths.rs`）：启动时 `StorageMode::detect()` 只看 exe 同目录
      `data/` 是否存在（**只探测不创建**，绝不往别人机器的工作目录落文件）→ 存在即便携模式
      （数据全在 `data/ruterm.db`，随 exe 拷贝）；否则全局模式（`~/.ruterm/ruterm.db`）。
- `[x]` **解锁两档并存**：
      - *keyring 档*（默认）：`getrandom` 生成 32 字节主密钥存系统凭据管理器，
        启动零输入、开箱即用（全局与便携皆同，便携状态下状态栏提示"建议设置主密码"但不拦路）。
      - *主密码档*（可选增强）：用户自定义口令经 **argon2id** 派生主密钥（盐 16 字节明文存
        `kdf-salt.bin`，属公开参数）；在「会话 → 设置…」→「凭据库与主密码」里可随时**设置 / 更换 / 取消**，
        口令档下每次启动输入口令解锁，库文件可凭同一口令带往其他机器。已设主密码时标题栏出现
        🔒，一键锁定凭据库（丢弃库句柄与内存密钥，回到解锁弹窗）。
      - *换密钥*：turso 无 rekey 能力，故实现为**新建加密库全量搬迁**——内存导出
        → 以新密钥打开暂存库 `ruterm.db.new` → 导入 → 旧库改名 `ruterm.db.keyring-bak`
        （占用则带时间戳）→ 换名上岗 → 写盐与 `unlock.txt` 后端标记 → 重开句柄校验主机数，
        确认数据完整才清理备份；任一步失败则旧库原样放回、盐还原。
- `[x]` **密码列二次加密（字段级）**：`hosts.password` 不再直接依赖页级加密，入库前用
      **AES-256-GCM**（`aes-gcm`）逐字段密封，字段密钥 = `SHA-256(主密钥 ‖ "ruterm-field-key-v1")`
      域分离派生，密文格式 `base64(nonce12 ‖ 密文 ‖ tag16)`，每次写入随机 nonce（同一口令两次密文不同）。
      读取时密钥不符 / 标签校验失败一律报错，**绝不降级返回原样**；导出转储同样只输出明文之外的密文。
      好处：库文件即便被离线复制，攻击者仍需同时破解页级 AEGIS 与字段级 GCM 才拿得到口令。
- `[x]` 连接时**免重复输入**：优先用 Agent / 私钥；勾选过"保存"的主机自动从加密库取码直连。
- `[x]` **加密私钥口令（passphrase）**：连接时 russh 报 `KeyIsEncrypted` → 会话回传
      `PassphraseRequired` 信号 → UI 自动弹口令窗（同样支持"保存口令"入加密库）；
      主机表单亦可直接填口令——勾选"保存口令"入加密库，否则仅缓存于本次会话内存。
- `[x]` **私钥文件选择**：表单"浏览…"按钮经 `App::prompt_for_paths`（Windows 原生对话框）
      选择私钥路径，仍保留手动输入；编辑态口令永不回填。
- `[x]` **私钥内嵌到加密库**：表单「私钥来源」二选一（本地文件 / 内嵌到加密库）。选内嵌后经「导入私钥…」
      读入 PEM 内容（≤64 KB，先用 `russh::keys::decode_secret_key` 校验可解析；加密私钥允许留口令），
      保存时写入 `settings` 表 `host_keys` 键（JSON：`{ "<host_id>": "<字段密文>" }`，逐条 AES-256-GCM）
      并只在内存 `inline_keys` 缓存解密后的原文；`hosts` 表结构不变（避免加密库 ALTER 迁移）。
      连接时优先用内存/库内 PEM，库未解锁或数据丢失则**明确报错**（不静默回退 Agent，防止认证方式被悄悄改变）；
      删除主机、切回本地文件认证都会清掉对应条目；换主密码时经 `export_all` 解密、`import_dump` 重加密。
  - 为什么放 `settings` 而非新表/新列：toasty 0.11 的 `push_schema` 不能对已存在的加密库补表补列；
    多行 JSON 适合"每台主机至多一把内嵌私钥"这种小规模元数据，且迁移零成本。
- `[x]` **连接失败可读化**：shell 建立前的任何失败（连不上 / 认证被拒 / 私钥需口令）**保留标签页**，
      错误链拍平写入终端与状态栏，可阅读后手动关闭，不再一闪而过；被服务器拒绝的已存秘密
      自动清除库内条目与内存缓存（自愈），下次点击重新询问。
- `[x]` **失效凭据自愈**：`SecretRejected` 标记贯穿密码/公钥/Agent/口令解密四类拒绝点。

### 安全约束（不可妥协，沿用现有基线并加强）
- `[x]` **密码 / passphrase / 内嵌私钥绝不以明文入库、入日志、入命令行**：密码列先经 AES-256-GCM 字段加密，
      再落入 AEGIS-256 页级加密库，磁盘上双层密文（含认证标签，端到端测试确认换密钥必然打不开、
      列内永不含明文）；内嵌私钥同样只以字段密文存在于 `settings.host_keys`（单测断言该设置行内
      不含 PEM 明文标记）；主密钥只在内存与
      系统凭据管理器 / 弹窗输入之间流动，从不写入文件、从不作为命令行参数传递。
- `[x]` keyring 档：主密钥经 **keyring** 对接操作系统凭据库（Windows Credential Manager，
      `service=ruterm / account=vault-key`），库文件与密钥分离，单独拷贝 `ruterm.db` 无法解密；
      条目内容非法时**拒绝自动覆盖**，以防旧库被锁死。
- `[x]` 主密码档：argon2id 派生，强度**取决于口令本身**；一旦忘记主密码，数据**不可恢复**
      （Argon2 无任何后门）——解锁弹窗与 README 均已明示。便携库可带往其他机器，
      在同一口令下正常解锁；未设主密码时库绑定本机凭据管理器，拷贝不可解（状态栏与设置里均有提示）。
- `[x]` 设置 / 更换 / 取消主密码前**必须验证当前主密码**（口令档）：应用可能在托盘里常驻数小时，
      不验证就能换密钥等于没有锁；校验用内存中的本次解锁密钥比对派生结果，口令本身不出界面。
- `[x]` known_hosts 指纹校验入加密库并保持 fail-closed：无记录 → 记入并信任（TOFU）；
      指纹不符 → 拒绝连接；**库查询/写入报错同样拒绝**（降级模式回退旧文本文件）。
- `[x]` 提供"仅本次会话内存持有密码"与"持久保存"两档，**默认前者**：密码弹窗带
      "☐ 保存密码（加密存入本地凭据库）"勾选，默认不勾选。

### 迁移策略
- `[x]` 一次性迁移到单文件库（`migrate_legacy`）：旧手写 SQL 的 `vault.db`（keyring 密钥）
      读出 hosts → 写入 toasty 新库；`sessions.json` 非空且新库为空时按序导入并分配主键；
      `settings.json` 的主题 / 状态栏 / 终端偏好写入 `settings` 表；`known_hosts` 文本逐行解析入表。
      导入 / 改名间新增的主机也会补发 id 入库。**导入 / 导出 UI 为后续增量。**
- `[x]` 迁移前不删除旧文件：**读取成功后**才逐个改名为 `.migrated`（`vault.db.migrated` /
      `sessions.json.migrated` / `settings.json.migrated` / `known_hosts.migrated`）保留现场；
      任一步失败则旧文件原样保留，整体降级回 JSON 路径并在状态栏提示。
- `[~]` 分组元数据已入库（加密库 `settings` 表 `sidebar` 键，见 §二.2，未动 `hosts` 表结构）；
      主机备注字段、导入导出界面、备份恢复待做。

---

## 五、本地终端（系统 Shell 会话）

> RuTerm 目前是纯 SSH 客户端。目标是同时成为**本地 + 远程统一终端**：
> 一个应用里既能开 SSH 会话，也能开本机 shell 标签。
> 基座已具备：`rgpui-term` 除外接 IO 后端（`TerminalBuilder::new_external`）外，
> 还有本地 PTY 后端 `TerminalBuilder::new(working_directory, shell, env, max_scroll_history_lines, window_id, cx)`，
> `shell: Option<...>` 为 `None` 时用系统默认 shell。SSH 侧只需继续用外接后端，两条路径共用 `TerminalView`。

### 1. Windows 本地 shell（首批）
- `[x]` **cmd.exe**：`cmd.exe`（`$COMSPEC` 回退 shell）。
- `[x]` **Windows PowerShell 5.x**：`powershell.exe`（PATH 找不到时按 `%SystemRoot%` 定位 `System32\WindowsPowerShell\v1.0\powershell.exe`）。
- `[x]` **PowerShell 7**：`pwsh.exe`（PATH 探测；常见安装位置扫描与「默认 shell」概念**不做**，见 §五.3）。
- `[x]` **Git Bash**：`git.exe` 同级的 `..\bin\bash.exe`，再兜 `%ProgramFiles%` / `%ProgramFiles(x86)%` /
      `%LOCALAPPDATA%` 下的 `Git\bin\bash.exe`，最后 PATH 里的 `bash`（`git_bash()`，`src/local_shell.rs`）。
- `[x]` **WSL（按发行版分条）**：`wsl.exe -l -q` 列出已安装发行版（UTF-16LE / UTF-8 两种输出都解，
      未安装时装的是本地化提示句，需过滤），每个发行版单独成条 `WSL · Debian` / `WSL · docker-desktop`，
      启动参数 `-d <发行版>` 经 `local_shell::command_line_with()` 拼成整条命令行交给 ConPTY 后端
      （`TerminalBuilder::new` 只收单一路径字符串，Windows 后端把它原样塞进 `CreateProcessW`）。
      枚举失败（`wsl.exe` 存在但调用出错）退回一条「WSL（默认发行版）」，确实没装发行版则不出现条目。
- `[x]` Windows 控制台 PTY 已确认可用：rgpui-term 内置 ConPTY（`portable-pty`）后端，实测
      ANSI 颜色 / 尺寸自适应 / Ctrl+C（`^C` 回显）/ Enter / 退格均正常，无需自行适配。
      - 探测拆两档（`src/local_shell.rs`）：`detect()` 只查文件、启动时同步调用；
        `scan()` = `detect()` + 起一次 `wsl.exe`，耗时且可能触发 WSL 服务启动，故放后台：
        启动后**静默补扫一次**（不写状态栏），「一键扫描」按钮/菜单项再手动刷新。
      - 去重保留先发现者（`pwsh` 优先于 `powershell`，Git Bash 不会因 PATH 里还有 `bash` 出现两条）。

### 2. 其他平台本地 shell（顺带，非 Windows 用户同样受益）
- `[x]` Linux/macOS：探测 `$SHELL`，回退 `/bin/bash`。

### 3. 会话形态与 UI
- `[x]` 侧边栏树顶的**可折叠「本地终端」分组**（见 §二.2）：子项即各本机 shell，带系统默认图标，
      点一下开对应标签；右键菜单与设置「终端」分页都有「一键扫描」；标题栏「会话」菜单 →
      「本地终端…」（`Ctrl+Shift+T`）入口保留。它不入库，因此不受分组、收藏、删除影响。
- `[x]` 标签标题区分来源：本地会话显示 shell 名（`pwsh` / `powershell` / `cmd`），状态栏条目为
      `本地终端 <shell>` + `<shell> · <起始目录>`；远程会话保持 `user@host`（`SessionTab.addr`
      语义扩展为"连接描述"，`auth` 改为 `Option<AuthMethod>`——本地会话没有认证概念）。
- `[x]` 终端内容区加内边距（左右 12px / 上下 8px），底色取终端自身主题的 `background`，
      四周不会在浅色主题下显成一圈白框（`terminal_bg()`，随配色预设切换）。
- `[x]` **Tab 键送进 PTY**（修复 shell 补全失效）：rgpui 的 `Root` 把 `tab` / `shift-tab` 绑成
      `focus_next` / `focus_prev`（context `Root`），而按键绑定分发早于 `on_key_down`，终端元素
      永远收不到 `\x09`。在 `Terminal` 上下文用 `NoAction {}` 抢占——它比 `Root` 更深，
      `Keymap::bindings_for_input` 解析到 `NoAction` 即 `break`，Root 的绑定不再入选，按键原样
      落到 `TerminalView::on_key_down` → `try_keystroke` → PTY。实测 `rea` + Tab 补出 `.\README.md`。
- `[x]` ~~默认 shell 可设置~~ —— **明确不做**：入口就是列表本身，点哪个开哪个；
      多一层"默认值"只会让"列表第一个"和"默认项"两套概念打架（用户决策：简单的一个本地终端列表就行）。
      `Ctrl+Shift+T` / 菜单项无具体档案时开列表第一项（`detect()` 把 PowerShell 7 排最前）。
- `[x]` 分组折叠态只存内存（`local_group_collapsed`）：它不是库里的分组，不值得占一个持久化键。
- `[x]` **本地终端重命名**：shell 行右键「重命名…」弹小窗只改展示名（侧边栏与标签共用），
      留空或「恢复默认名称」回到探测名；映射存加密库 `settings` 表 `local_shells` 键
      （降级 `settings.json`，JSON `{"names": {"<id>": "<name>"}, "cwd": {"<id>": "<dir>"}}`），id 即 路径 + 参数，
      WSL 各发行版互不影响。
- `[x]` 每个本地会话可选起始工作目录：shell 行右键「起始目录…」弹窗（可「浏览…」调系统目录对话框），
      按 profile id 存 `settings.local_shells`；目录不存在时拒绝保存，事后被删则静默回落到进程当前目录；
      右键「恢复默认起始目录」一键清掉。
- `[x]` **自定义启动参数**：右键「启动参数…」单行弹窗，空格分隔、含空格的参数用双引号包住
      （`local_shell::split_args` 按引号分词，`join_args` 规范化回填）；启动时追加在档案自带的
      `-d <发行版>` 之后，经 `local_shell::command_line_with` 拼成整条命令行交给 ConPTY；
      非 Windows 后端不解析命令行，参数暂不生效。留空/「清除启动参数」即恢复。
- `[x]` **自定义环境变量**：右键「环境变量…」多行弹窗（6 行起、`auto_grow`），每行 `KEY=VALUE`，
      空行与 `#` 注释忽略、值里允许 `=`；键名含空白/为空时带行号拒绝保存并保持弹窗。
      解析后经 `TerminalBuilder::new` 的 `env` 参数注入（portable-pty 在父进程环境之上追加），
      存 `settings.local_shells` 的 `env` 字段；留空/「清除环境变量」即恢复。

### 4. Shell Profile（类 Windows Terminal 的 profile 概念）
- `[ ]` `hosts` 表增本地条目类型（或新增 `profiles` 表）：名称、可执行路径、参数数组、起始目录、环境变量、图标/配色。
- `[ ]` 保留与收藏、搜索、置顶复用现有主机列表机制（★ 收藏 + 搜索过滤天然适用）。
- `[x]` 从系统自动发现（PATH / 安装目录 / `wsl -l`）：`src/local_shell.rs` 的 `detect()` / `scan()`，
      结果直接作为侧边栏子项展示（尚未入表；展示名 / 起始目录 / 启动参数 / 环境变量均可右键配置并持久化，
      见 §五.3；入表与图标/配色自定义待做）。

### 5. 架构约束（避免做成 SSH 专用）
- `[ ]` 抽出 `SessionBackend` 抽象（`External`（russh）/ `LocalPty`），UI 层只认 `TerminalView` + 会话生命周期，不再假设"有 target/host"。
- `[~]` 状态栏本地会话条目显示 shell（与起始目录），不显示认证方式（`SessionTab.auth: Option<AuthMethod>`，
      `None` 时该条目整体不渲染）；运行时长待补。
- `[x]` 基座已具备，一期只需调用：`TerminalBuilder::new(working_directory, shell, env, max_scroll_history_lines, window_id, cx) -> Task<Result<TerminalBuilder>>`（`shell` 为 `None` 时走 `default_shell_command()`），拿到 builder 后与 SSH 路径完全同构（`cx.new(|cx| builder.subscribe(cx))` + `TerminalView::new`）。
      - 窗口 id 取法：`Window::window_id()` 对外不可用，用 `cx.active_window().map(|h| h.window_id().as_u64())`。
- `[x]` 保持 UI 线程零阻塞：本地 PTY 读写走 rgpui-term 内部线程，应用侧只持有句柄；
      进程拉起 / 通道握手在 `cx.spawn` 里 await，**成功才落标签**，失败只写状态栏不留空标签。
      shell 退出（`exit` / Ctrl+D / 进程被杀）经 `TerminalEvent::CloseTerminal` 订阅自动收标签。

### 6. 分期落地（慢慢实现）
- 一期：~~`TerminalBuilder::new` 直连默认 shell，单标签跑通 cmd + PowerShell 5~~（**已完成**，实测 pwsh 7.6.7：
  输入回显、`dir` 彩色表格、Ctrl+C、Tab 补全、`exit` 自动收标签均正常）。
- 二期：~~本地终端并入主机列表分组树~~（**已完成**：树顶「本地终端」分组列出各 shell，见 §二.2 / §五.3）
  + ~~profile 自动发现（Git Bash / WSL）~~（**已完成**，`src/local_shell.rs`）+ 新建会话选择器（§二.2）。
- 三期：~~按发行版分条 WSL~~（**已完成**：`wsl.exe -d <发行版>` 经整条命令行传递，见 §五.1）、
  ~~本地终端重命名~~（**已完成**，见 §五.3）、~~每会话起始目录~~（**已完成**，见 §五.3）、
  ~~profile 自定义启动参数 / 环境变量~~（**已完成**，见 §五.3）；
  余下 profile 入表（名称/路径/图标/配色）与 SSH 共用分屏/标签/搜索能力。

---

## 六、实施优先级建议

1. ~~收尾状态栏（远程信息 exec 通道 + 可配置显示项）~~（已完成：连接后经 exec 探测写入 `remote_info`，
   条目多寡在设置「状态栏」分页勾选，见 §二.1）。
2. ~~侧边栏折叠 + 主题切换~~（已完成：`Ctrl+B` 折叠、`Ctrl+T` / 标题栏「外观」菜单明暗切换）。
3. ~~凭据 SQLite + keyring（安全核心，尽早定型，越晚迁移成本越高）~~（已完成：toasty ORM 单文件加密库
   `ruterm.db`（hosts/settings/known_hosts 三表）+ keyring 或 argon2 主密码双档解锁 + 便携/全局双模式
   + 旧文件一次性迁移，见 §四；分组偏好亦已入 `settings` 表 `sidebar` 键（见 §二.2）；余量：备注、导入导出界面、备份恢复）。
4. ~~自定义标题栏~~（已完成：自绘顶栏 + 多级菜单（快捷键徽标）+ 窗口控件，见 §二.5；会话标签经用户确认
   保留在主区标签栏）+ ~~分屏~~（已完成：窗口级两标签 `SplitPane` 左右/上下分屏、点击切焦点、可拖分隔条，
   见 §二.6；余量：单屏最大化、拖拽重排、`DockArea` 多区停靠）。
5. i18n（文案抽取宜在功能稳定后统一做，避免反复）。
6. ~~**本地终端**一期~~（已完成：`TerminalBuilder::new` 直连本地 PTY，标签 + 状态栏 + 退出收标签；
   Git Bash / WSL 探测、一键扫描、系统图标、侧边栏分组列表、**WSL 按发行版分条**、**条目重命名**、
   **每会话起始目录**、**自定义启动参数与环境变量**均已完成，见 §五）。
7. ~~SFTP 上传 / 下载~~（已完成：复用 SSH 连接 + 边栏文件页，见 §二.7）；~~端口转发~~
   （已完成：本地（-L）、远程（-R）与动态 SOCKS（-D），规则按连接保存、列表开关启停、可设连接时自动开启，见 §二.7）；
   ~~跳板机（ProxyJump）~~（已完成：主机表单选跳板机、逐跳 `direct-tcpip` + `connect_stream`、逐跳 TOFU、凭据只取缓存 / 加密库，见 §二.7）。

> 每完成一项，回到本文件把对应方框勾成 `[x]`，并在 §一快照表更新状态。
