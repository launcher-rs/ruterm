# RuLink 设计目标与开发路线图

> 本文件是 RuLink 的功能设计总纲：**列出目标 → 完成一项勾选一项**。
> 勾选图例：`[x]` 已完成 · `[~]` 进行中/部分完成 · `[ ]` 待办。
> 品牌定位与分阶段愿景见同目录 `rulink.txt`。

## 一、当前进度快照

| 能力 | 状态 | 说明 |
|------|------|------|
| SSH 连接与终端 | `[x]` | russh + rgpui-term（外部 IO 后端），交互式 PTY |
| 本地终端（系统 Shell） | `[x]` | 侧边栏树顶「本地终端」是**可折叠分组**：子项为本机探测到的每个 shell（各取 exe 的系统默认图标），点一下即开对应终端标签；启动后台静默扫一次（含 WSL），「一键扫描」可在右键菜单 / 设置「终端」分页手动刷新；标签与 SSH 会话并存，进程退出自动收标签；profile 持久化、起始目录、环境变量见 §五 |
| 认证方式 | `[x]` | 密码 / 私钥(IdentityFile，表单可填口令、`浏览…`选择密钥文件；加密私钥自动弹口令窗) / Agent；口令与密码同走"默认内存 + 勾选入加密库"两档 |
| 主机指纹校验 | `[x]` | TOFU known_hosts **存入加密库**（`known_hosts` 表），不匹配 / 写入失败 / 查询失败一律拒绝（fail-closed，防 MITM） |
| 会话标签页 | `[x]` | 多标签、关闭、激活切换；标签栏在主区顶部（44px 栏高、160px 最小标签宽），不放标题栏；**平铺式标签**：不描边、标签之间不留缝，活动标签用主题语义色 `tab_active` 打底 + 顶部 2px 强调线，非活动标签不显示 `×`（切过去再关） |
| 侧边栏主机列表 | `[x]` | 分组（文件夹）树：折叠三角 + 文件夹/认证方式图标 + 组名 + 数量，★ 收藏置顶；组与归属、折叠态存加密库 `settings` 表 `sidebar` 键（JSON，不动 `hosts` 表结构）。新建/编辑/删除/移动/分组管理全部收进右键菜单，顶部不再放常驻大按钮 |
| CLI 快速连接 | `[x]` | 带参数一次性连接；密码仅走环境变量 / GUI 临时输入 |
| 主题 | `[x]` | 明 / 暗切换（配色走 `cx.theme()` 语义色，与按钮/状态栏统一），标题栏「外观」菜单（当前项打勾）+ `Ctrl+T`，选择持久化到加密库 `settings` 表 |
| 底部状态栏 | `[x]` | 已实现并验证渲染（左"就绪"/地址/认证/远程信息，右"会话 N"）；远程基本信息经 exec 探测通道填充，显示哪些条目可在「状态栏」分页勾选 |
| 凭据持久化 | `[x]` | 主机配置、应用设置、known_hosts 三类数据合并入**单个加密文件** `rulink.db`（toasty 0.11 ORM + turso 0.7 AEGIS-256 页级加密，`hosts.password` 列再叠 **AES-256-GCM 字段加密**），全局模式位于 `~/.rulink/`、便携模式位于 exe 旁 `data/`；解锁两档并存：默认 keyring 随机 32 字节主密钥（零输入），或用户**主密码**（argon2id 派生，可带往他机解锁）。库不可用时降级旧 `sessions.json` 且禁用密码保存，见 §四 |
| 便携 / 全局双模式 | `[x]` | 启动时只探测不创建：exe 同目录存在 `data/` → 便携模式（数据随 exe 走），否则全局模式 `~/.rulink`；主密码为可选项（设置 / 更换 / 取消均在设置里），见 §四 |
| 自定义标题栏 | `[x]` | 应用自绘顶栏：品牌标识 + 「会话 / 凭据 / 外观 / 帮助」多级菜单（有快捷键的项右侧自动显示按键徽标）+ ⚙/🔑/🔒/`«` 快捷按钮 + 系统窗口控件；拖拽与窗口控制走 `WindowControlArea` 命中区，交互控件排在 Drag 区之外 |
| 设置界面 | `[x]` | ⚙ 打开设置弹窗（固定 640×470，切分页尺寸不变），**左侧竖排分页导航**（凭据库与主密码 / 外观 / 状态栏 / 终端）+ 右侧定高滚动内容区，新设置项继续追加分页 |
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
- `[x]` **可配置显示项**：设置弹窗新增「状态栏」分页（入口：⚙ → 状态栏，或「外观」菜单 → 状态栏显示项…），
      分「会话信息」（连接地址 / 认证方式）与「远程主机信息」（系统 / 内存 / 磁盘 / 负载 / 运行）两组勾选，
      取消勾选即从状态栏移除，改动即时生效。
  - 实现：`StatusBarPrefs`（`src/main.rs`，`metrics` 用 `HashMap<id, bool>`，缺省视为显示，
    新增指标不必迁移旧数据）+ `Metric` 枚举（`src/remote.rs`，`id` 兼作持久化键与条目 id，
    `label` 供设置界面与状态栏共用）；`render_status_bar` 按勾选拼装，`RemoteInfo::item(metric)`
    缺项返回 `None` 时继续跳过。
  - 持久化沿用主题的双层路径：库内 `settings` 表 `status_bar` 键（JSON），库不可用时退回
    `settings.json`；旧 `settings.json` 迁移一并带上该键。读回时空串 / 损坏 JSON 回落默认全显示。

### 2. 侧边栏
- `[x]` 可拖拽调整宽度（rgpui `SplitPane::horizontal`，初始比例 0.25，最小宽度 160px）。
- `[x]` 折叠 / 展开：只留标题栏的折叠按钮（图标随状态在 `PanelLeftClose` / `PanelLeftOpen` 间翻转）+ 快捷键 `Ctrl+B`。
      原先折叠后主区顶部还会冒出一条 44px 的 `»` 展开栏，与标题栏按钮重复且白占终端高度，已删除。
- `[x]` WindTerm 式拖拽：拖到最小宽度即停住，分隔线仍可抓住往大拖；完全收起只交给 `«` / `Ctrl+B`，不做拖拽阈值自动折叠（此前自动折叠会在展开后拖动时因 `collapse` 前的旧比例经 mouse-down 触发 `on_resize` 而立刻重新收起，已移除）。
- `[x]` 窄宽度下主机行自适应：名称/地址 `text_ellipsis` 截断，认证方式徽标 `flex_shrink_0` 固定不被挤压。
- `[x]` 主机行右键菜单：**编辑 / 删除**（框架 `ContextMenuExt::context_menu` + `PopupMenu`，见本节末条：
      早期手搭 `absolute` 浮层因图层问题点不动，已换框架实现）。
- `[x]` 编辑复用新建表单：`open_form_at(Some(index))` 预填原值，保存原位替换（保留 `id` / `favorite`，不重新分配）。
- `[x]` 删除二次确认：居中模态卡片（半透明遮罩），提示"已保存的密码/口令将一并从加密库清除"；行内不再常驻删除按钮。
- `[x]` 新建/编辑主机改为**居中模态弹窗**（440px 卡片 + 遮罩，端口/用户名双列），不再挤占侧边栏窄面板；
      遮罩点击不关闭（防误丢已填内容），关闭仅走"取消"或标题栏 ×。
- `[x]` 弹窗**高度稳定**：私钥区块（路径/口令/保存勾选）常驻布局，非私钥认证时 `invisible`
      保留占位（绘制与命中整体跳过），切换认证方式弹窗不再忽大忽小；加密库不可用时勾选行
      置灰并提示"仅本次内存持有"。
- `[ ]` 折叠后保留窄图标轨道，悬停可临时展开（现折叠为完全收起，展开只走标题栏按钮 / `Ctrl+B`；窄轨道悬停展开可后续按需增强）。
- `[x]` **分组（文件夹）树**：主机列表按分组树组织（折叠三角 + 文件夹图标 + 组名 + 数量），
      右键可**新建分组… / 重命名分组… / 删除分组**，主机行右键「移动到分组」子项带 ✓ 当前归属；
      删除分组只退回归属到「未分组」，**绝不删主机**。折叠态、组顺序、归属均持久化。
  - 实现：`SidebarPrefs`（`src/main.rs`：`groups` 顺序 + `collapsed` + `assign: HashMap<host_id, group>`，
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
      右键菜单：一键扫描本机 shell… / 新建主机… / 新建分组…；「会话」菜单 → 本地终端（`Ctrl+Shift+T`）开列表第一项。
- `[x]` **新建入口收敛**：侧边栏顶部常驻的「+ 新建主机」大按钮已去掉，顶部空间整块留给列表；
      新建走**主机行 / 分组行 / 列表空白处**右键菜单，或标题栏「会话」菜单「新建主机…」（`Ctrl+N`）。
- `[x]` **主机行右键菜单改用框架 `PopupMenu`**：手搭的 `absolute` 浮层留在普通图层，会被后画的兄弟节点压住
      hitbox，菜单项收不到 mouse-up（鼠标能悬停、点击只关菜单），改用框架的 `deferred + anchored + with_priority(1)`
      独立图层后正常，并顺带拿到阴影 / 分隔线 / 图标 / ✓ / 危险色 / 贴边收敛。
      空白处的菜单必须是行的**兄弟**节点而非祖先——`ContextMenu` 包装器在子节点之后注册自己的 hitbox，嵌套会一次弹两层。
- `[ ]` **分组内排序**：组内支持手动拖拽定序与按名称 / 最近使用自动排序两种模式，排序结果持久化。

### 3. 主题
- `[x]` 明 / 暗主题切换（rgpui `Theme::change(ThemeMode)` + 配色统一走 `cx.theme()` 语义色），标题栏「外观」菜单（当前项打勾）+ `Ctrl+T`。
- `[x]` 启动项记忆上次主题选择（持久化到加密库 `settings` 表的 `theme` 项，库打开/解锁完成后自动应用；
      库不可用时回退读写 `settings.json`）。
- `[~]` 终端配色方案：设置「终端」分页下拉切换 rgpui-term `ThemeManager` 的 **9 套内置预设**
      （One Dark/Light、Solarized Dark/Light、Dracula、Nord、Gruvbox Dark/Light、GitHub Light），
      改动即刻刷到所有已开终端并写入加密库 `settings` 表 `terminal` 键（降级时 `settings.json`）。
      自定义前景 / 背景 / 16 色（`TerminalThemeConfig` → `ThemeManager::add_theme("Custom")`）待做。
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
- `[x]` **标题栏多级菜单**：「会话」（新建主机 `Ctrl+N` / 折叠侧栏 `Ctrl+B` / 设置… `Ctrl+,` · 隐藏到托盘 · 退出）·
      「凭据」（解锁 · 主密码与凭据库设置 · 锁定 · 末尾只读显示当前档位）·
      「外观」（暗色 / 亮色打勾单选 + 切换明暗主题 `Ctrl+T` · 外观设置 · 状态栏显示项 · 终端外观分页入口）·
      「帮助」（只留版本号）。
      菜单用 `Button::dropdown_menu` + `PopupMenu`，构建器是 `Fn`，实体句柄需在闭包内重新 `clone` 再交给各项 `on_click`。
- `[x]` **快捷键随菜单项原生显示**：四个命令声明为 action（`actions!(rulink, [ToggleSidebar, ToggleTheme, NewHost, OpenSettings])`），
      `main()` 里 `cx.bind_keys` 绑定 `Ctrl+B` / `Ctrl+T` / `Ctrl+N` / `Ctrl+,`，根元素用 `.on_action(cx.listener(...))` 接收；
      菜单项改挂 `.action(Box::new(X))`，rgpui 的 `PopupMenu::render_key_binding` 会按该 action 的键绑定
      在项右侧自动渲染按键徽标（`Kbd`），因此标签里不再手写 `(Ctrl+B)`，「帮助」菜单里那份纯文字快捷键清单也删掉了。
      点击仍生效：`PopupMenuItem` 只有 `on_click` 没有 shortcut 字段，但 `confirm()` 里 handler 优先于 action，
      单独挂 action 一样能派发（实测菜单点击与键盘两条通路都通）。
- `[x]` 设置入口收进标题栏：⚙ 打开**分页式**设置弹窗（凭据库与主密码 / 外观 / 状态栏），🔑 / 🔒 随锁定态切换，
      侧边栏头部品牌行与 ⚙/🔒 按钮移除。菜单项可**直达指定分页**（弹窗已开时只换分页，不重复创建）。
- `[x]` **右侧按钮带分层**：内容级操作（`IconName::PanelLeftClose/Open` 折叠侧栏、`IconName::Settings` 设置、
      口令档才有 🔑/🔒）用 `Button::compact().ghost()` 的 28px 圆角块，与贴右缘、整高整宽的
      系统窗口按钮之间加 1px 竖分隔线；图标一律用 rgpui 矢量图标而非 `⚙` / `«` 文本字形
      （图标集里没有钥匙/锁，这两个仍用 emoji）。侧栏按钮改为 `toggle_collapse`，折叠后图标翻转成 `PanelLeftOpen`，
      原实现只会 `collapse`，折叠后再点没反应。
- `[ ]` ~~会话标签并入标题栏（类 WindTerm 的"标签即标题栏"）~~：**已实现又撤回**——用户明确要求终端标签不要放在
      自定义标题栏上，标签栏回到主区顶部（44px 行高、平铺无描边、活动标签顶部强调线、仅活动标签显示 `×`）。
      标题栏只留品牌 / 菜单 / 快捷按钮 / 窗口控件，中间整段仍是拖拽区。技术上标签条与 Drag 段必须互为
      **兄弟**节点（父级 Drag 会吞掉标签点击，同 §二.5 首条 `TitleBar` 未采用的原因），若日后重提可直接复用该结论。
- `[ ]` Windows 下与 Mica 材质 / 圆角协调（当前保留 `WS_THICKFRAME`，DWM 仍绘制圆角）。

### 6. 分屏窗口
- `[ ]` 水平 / 垂直分屏（一个窗口内并排多个终端会话）。
- `[ ]` 分屏焦点切换（快捷键）、单屏最大化、拖拽重排。
- `[ ]` 复用 rgpui `SplitPane` / `DockArea` 作为布局基座。

### 7. 其它主流能力
- `[x]` 主机列表搜索：侧边栏顶部搜索框，按展示名 / `user@host:port` 子串实时过滤（无匹配显示"无匹配主机"）。
- `[x]` 主机收藏：每行 ★/☆ 切换按钮（`stop_propagation` 不触发连接），收藏主机稳定排序置顶，`favorite` 字段持久化到加密库 `hosts` 表。
- `[x]` **设置弹窗（左侧竖排分页）**（标题栏 ⚙ 或菜单进入）：「凭据库与主密码」分页显示数据目录与解锁方式、
      设置 / 更换 / 取消主密码、锁定凭据库；「外观」分页放明暗主题；「状态栏」分页勾选底部条目；
      「终端」分页调字号 / 行高与配色方案（见 §二.3）。
      弹窗为固定尺寸，切分页不再忽大忽小，内容超出时由分页自身滚动。
      新增设置项继续追加分页即可，主密码相关操作不再散落在侧边栏。已设主密码时标题栏额外出现
      🔒，点击即锁定库（丢弃句柄与内存密钥，需口令重新解锁）。
- `[~]` 设置项扩展：状态栏显示项（见 §二.1）、终端字号 / 行高 / 配色方案（见 §二.3）已完成，
      默认 shell、UI 字体、终端自定义 16 色等继续按分页追加。
- `[~]` 主机分组（已完成，见 §二.2）、组内排序与最近连接（待做）。
- `[~]` 断线提示 + 手动重连（**已完成**：终端 `CloseTerminal` 后保留标签与回滚内容，顶部浮出
      「远程通道已关闭」横幅，回车 / 「重新连接」在原标签内重建会话；`channel_eof/close/exit_status`
      经 Handler 信号结束 `handle.await` 并释放连接，关标签也会主动断开 SSH。自动重连与连接超时仍需做）。
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
| 自定义标题栏 | `TitlebarOptions::appears_transparent` + `app_owns_titlebar_drag` + `WindowControlArea`（Drag/Min/Max/Close）；`title_bar::TitleBar` 因 Drag 命中区吞掉栏内按钮点击而未采用，改自绘一行 |
| 标题栏菜单 / 设置分页 | `menu::{PopupMenu, PopupMenuItem, DropdownMenu}`（`Button::dropdown_menu`）；菜单项挂 `.action(...)` 即由 `PopupMenu::render_key_binding` 自动在右侧渲染 `Kbd` 按键徽标（`PopupMenuItem` 本身无 shortcut 字段）；设置分页导航自绘（`h_flex` 行 + 选中项强调色竖条 + 粗体，不用 `tabs::TabBar`——它只有横向模式） |
| 主题切换 | `theme::Theme::change` |
| 终端配色 / 字号 | `rgpui-term`：`ThemeManager`（全局，9 套预设）+ `TextStyle`；新建走 `TerminalView::new_with_style`，已开终端经 `text_style_mut()` 原地刷新 |
| 系统托盘 | `tray::{TrayMenuItem, TrayIconEvent}` + `App::set_tray_*`（已用） |
| 国际化 | `i18n::I18nManager` |
| 命令注册 / 快捷键 | `actions!` 声明动作 + `cx.bind_keys`（App 级 `KeyBinding`）+ 根元素 `.on_action(cx.listener(...))`；`commands::CommandRegistry`、`key_context` 备用。局部覆盖用 `KeyBinding::new("tab", NoAction {}, Some("Terminal"))`：`Keymap::bindings_for_input` 按上下文深度降序解析，遇到 `NoAction` 即 `break`，可截掉框架在 `Root` 上下文里对同一按键的绑定（终端 Tab 补全即此法恢复） |
| 标签栏配色 | 主题语义色 `tab_bar` / `tab_active` / `tab_active_foreground` / `tab_foreground`（明暗两套自带区分），活动标签顶部叠 2px `primary` 强调线（不再整圈描边）；不硬编码颜色 |
| 主机行 / 分组行 / 空白处右键菜单 | `menu::context_menu::{ContextMenuExt::context_menu, PopupMenu, PopupMenuItem}`：包装器把元素转 `relative` 并在 `deferred(anchored().snap_to_window_with_margin(8px).with_priority(1))` 的**独立图层**渲染菜单。手搭 `absolute` 浮层不可行——它留在普通图层，被后画兄弟节点压住 hitbox，菜单项只吃得到 hover 吃不到 mouse-up。`context_menu` 要求元素同时是 `InteractiveElement + ParentElement + Styled`，ElementId 由内层 `.id()` 派生；包装器的 hitbox 注册在子节点**之后**，故两层菜单不能嵌套（一次右键弹两个），空白区菜单须作行的兄弟节点。`PopupMenuItem::on_click` 的闭包要求 `Send + Sync`，捕获 `Entity<T>` 后 `entity.update(cx, ..)` 即可拿到 `&mut RuLinkApp`（`Entity` 恒 `Send + Sync`，内部是 `PhantomData<fn(T)->T>`） |
| 布局约束范式 | `examples/showcase/src/bin/app_suite.rs`（flex_grow + min_h(0) 有界布局范式，状态栏布局修复即据此） |
| exe 的系统默认图标 | 无现成组件：`SHGetFileInfoW(SHGFI_ICON \| SHGFI_LARGEICON)` 取 `HICON` → `GetIconInfo` + `CreateDIBSection`（32bpp 顶向下）+ `DrawIconEx(DI_NORMAL)` → BGRA 转 RGBA（`alpha == 0` 的 RGB 归零）→ `png::Encoder` → `rgpui::Image::from_bytes(ImageFormat::Png, ..)`。`windows 0.62`（`Win32_UI_Shell` / `Win32_Storage_FileSystem` / `Win32_Graphics_Gdi` / `Win32_UI_WindowsAndMessaging`）+ `png 0.18`，无新增大件。另注：`extract_icon` 在 rsproxy 镜像上不可得，故手写 |
| 后台阻塞任务（扫描 shell） | **不能用 `tokio::task::spawn_blocking`**：rgpui 的 `cx.spawn` 跑在它自己的执行器上，没有 tokio reactor，直接 panic「there is no reactor running」。正确写法是先 `let runtime = rgpui::tokio::Tokio::handle(cx);` 拿到 `tokio::runtime::Handle`，再 `runtime.spawn_blocking(f).await` |

---

## 四、凭据与安全存储（重点，用户明确诉求：讨厌反复输入密码）

### 目标
- `[x]` 主机配置、用户名、私钥路径、（可选）密码迁入**结构化存储**：**toasty 0.11 ORM**
      （声明式 `#[derive(Model)]`）+ 底层 turso 0.7 页级加密，单文件 `rulink.db`；
      表 `hosts(id, name, host, port, user, auth, key_path, favorite, password)`、
      `settings(name, value)`、`known_hosts(host_port, fingerprint, algorithm)`。
      分组、备注、备份恢复为后续增量。
- `[x]` **一个文件装下所有本地数据**：`settings.json`（主题等）与 `known_hosts` 文本文件
      全部并入同一加密库；`.rulink` 目录不再有第三个业务文件。
- `[x]` **便携 / 全局双模式**（`src/paths.rs`）：启动时 `StorageMode::detect()` 只看 exe 同目录
      `data/` 是否存在（**只探测不创建**，绝不往别人机器的工作目录落文件）→ 存在即便携模式
      （数据全在 `data/rulink.db`，随 exe 拷贝）；否则全局模式（`~/.rulink/rulink.db`）。
- `[x]` **解锁两档并存**：
      - *keyring 档*（默认）：`getrandom` 生成 32 字节主密钥存系统凭据管理器，
        启动零输入、开箱即用（全局与便携皆同，便携状态下状态栏提示"建议设置主密码"但不拦路）。
      - *主密码档*（可选增强）：用户自定义口令经 **argon2id** 派生主密钥（盐 16 字节明文存
        `kdf-salt.bin`，属公开参数）；在 ⚙ 设置 →「凭据库与主密码」里可随时**设置 / 更换 / 取消**，
        口令档下每次启动输入口令解锁，库文件可凭同一口令带往其他机器。已设主密码时标题栏出现
        🔒，一键锁定凭据库（丢弃库句柄与内存密钥，回到解锁弹窗）。
      - *换密钥*：turso 无 rekey 能力，故实现为**新建加密库全量搬迁**——内存导出
        → 以新密钥打开暂存库 `rulink.db.new` → 导入 → 旧库改名 `rulink.db.keyring-bak`
        （占用则带时间戳）→ 换名上岗 → 写盐与 `unlock.txt` 后端标记 → 重开句柄校验主机数，
        确认数据完整才清理备份；任一步失败则旧库原样放回、盐还原。
- `[x]` **密码列二次加密（字段级）**：`hosts.password` 不再直接依赖页级加密，入库前用
      **AES-256-GCM**（`aes-gcm`）逐字段密封，字段密钥 = `SHA-256(主密钥 ‖ "rulink-field-key-v1")`
      域分离派生，密文格式 `base64(nonce12 ‖ 密文 ‖ tag16)`，每次写入随机 nonce（同一口令两次密文不同）。
      读取时密钥不符 / 标签校验失败一律报错，**绝不降级返回原样**；导出转储同样只输出明文之外的密文。
      好处：库文件即便被离线复制，攻击者仍需同时破解页级 AEGIS 与字段级 GCM 才拿得到口令。
- `[x]` 连接时**免重复输入**：优先用 Agent / 私钥；勾选过"保存"的主机自动从加密库取码直连。
- `[x]` **加密私钥口令（passphrase）**：连接时 russh 报 `KeyIsEncrypted` → 会话回传
      `PassphraseRequired` 信号 → UI 自动弹口令窗（同样支持"保存口令"入加密库）；
      主机表单亦可直接填口令——勾选"保存口令"入加密库，否则仅缓存于本次会话内存。
- `[x]` **私钥文件选择**：表单"浏览…"按钮经 `App::prompt_for_paths`（Windows 原生对话框）
      选择私钥路径，仍保留手动输入；编辑态口令永不回填。
- `[x]` **连接失败可读化**：shell 建立前的任何失败（连不上 / 认证被拒 / 私钥需口令）**保留标签页**，
      错误链拍平写入终端与状态栏，可阅读后手动关闭，不再一闪而过；被服务器拒绝的已存秘密
      自动清除库内条目与内存缓存（自愈），下次点击重新询问。
- `[x]` **失效凭据自愈**：`SecretRejected` 标记贯穿密码/公钥/Agent/口令解密四类拒绝点。

### 安全约束（不可妥协，沿用现有基线并加强）
- `[x]` **密码 / passphrase 绝不以明文入库、入日志、入命令行**：密码列先经 AES-256-GCM 字段加密，
      再落入 AEGIS-256 页级加密库，磁盘上双层密文（含认证标签，端到端测试确认换密钥必然打不开、
      列内永不含明文）；主密钥只在内存与
      系统凭据管理器 / 弹窗输入之间流动，从不写入文件、从不作为命令行参数传递。
- `[x]` keyring 档：主密钥经 **keyring** 对接操作系统凭据库（Windows Credential Manager，
      `service=rulink / account=vault-key`），库文件与密钥分离，单独拷贝 `rulink.db` 无法解密；
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

> RuLink 目前是纯 SSH 客户端。目标是同时成为**本地 + 远程统一终端**：
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
- `[x]` **WSL**：`wsl.exe -l -q` 列出已安装发行版（UTF-16LE / UTF-8 两种输出都解，未安装时装的是本地化提示句，
      需过滤），有发行版才出现条目，名称带数量：「WSL（2 个发行版）」。
      - **限制**：`rgpui_term::TerminalBuilder::new` 内部写死 `tty::Shell::new(program, Vec::new())`（参数恒为空），
        故只能起默认发行版，`wsl.exe -d <发行版>` 起不来；按发行版分条要等上游开放 shell 参数。
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
- `[ ]` 每个本地会话可选起始工作目录（当前固定为进程当前目录）。

### 4. Shell Profile（类 Windows Terminal 的 profile 概念）
- `[ ]` `hosts` 表增本地条目类型（或新增 `profiles` 表）：名称、可执行路径、参数数组、起始目录、环境变量、图标/配色。
- `[ ]` 保留与收藏、搜索、置顶复用现有主机列表机制（★ 收藏 + 搜索过滤天然适用）。
- `[x]` 从系统自动发现（PATH / 安装目录 / `wsl -l`）：`src/local_shell.rs` 的 `detect()` / `scan()`，
      结果直接作为侧边栏子项展示（尚未入表，也还不能自定义名称/参数/环境变量）。

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
- 三期：profile 持久化（自定义名称/参数/起始目录/环境变量）、每会话起始目录、环境变量，
  与 SSH 共用分屏/标签/搜索能力；按发行版分条 WSL 待 rgpui-term 开放 shell 参数。

---

## 六、实施优先级建议

1. ~~收尾状态栏（远程信息 exec 通道 + 可配置显示项）~~（已完成：连接后经 exec 探测写入 `remote_info`，
   条目多寡在设置「状态栏」分页勾选，见 §二.1）。
2. ~~侧边栏折叠 + 主题切换~~（已完成：`Ctrl+B` 折叠、`Ctrl+T` / 标题栏「外观」菜单明暗切换）。
3. ~~凭据 SQLite + keyring（安全核心，尽早定型，越晚迁移成本越高）~~（已完成：toasty ORM 单文件加密库
   `rulink.db`（hosts/settings/known_hosts 三表）+ keyring 或 argon2 主密码双档解锁 + 便携/全局双模式
   + 旧文件一次性迁移，见 §四；分组偏好亦已入 `settings` 表 `sidebar` 键（见 §二.2）；余量：备注、导入导出界面、备份恢复）。
4. ~~自定义标题栏~~（已完成：自绘顶栏 + 多级菜单（快捷键徽标）+ 窗口控件，见 §二.5；会话标签经用户确认
   保留在主区标签栏）+ 分屏（余量：`SplitPane` / `DockArea` 多会话分屏）。
5. i18n（文案抽取宜在功能稳定后统一做，避免反复）。
6. ~~**本地终端**一期~~（已完成：`TerminalBuilder::new` 直连本地 PTY，标签 + 状态栏 + 退出收标签；
   Git Bash / WSL 探测、一键扫描、系统图标、侧边栏分组列表均已完成，见 §五）；
   余量：profile 持久化（自定义名称/参数/起始目录/环境变量）、每会话起始目录。
7. SFTP / 端口转发 / 跳板机（`rulink.txt` 第二阶段）。

> 每完成一项，回到本文件把对应方框勾成 `[x]`，并在 §一快照表更新状态。
