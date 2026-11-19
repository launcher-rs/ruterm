# RuTerm

[![CI](https://github.com/launcher-rs/ruterm/actions/workflows/ci.yml/badge.svg)](https://github.com/launcher-rs/ruterm/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/launcher-rs/ruterm)](https://github.com/launcher-rs/ruterm/releases)
[![License](https://img.shields.io/badge/license-Apache--2.0-blue)](LICENSE)

Rust 编写的桌面远程连接工具：SSH 终端、本地 Shell、SFTP 文件管理、端口转发和远程桌面集中在一个窗口里。基于 [rgpui](https://crates.io/crates/rgpui) 与 [rgpui-term](https://crates.io/crates/rgpui-term) 构建。

## 主要功能

- **SSH 会话**：russh + 交互式 PTY（`xterm-256color`），支持 Agent、私钥、密码三种认证；私钥可引用本地文件，也可整份加密存入凭据库。
- **本地终端**：自动探测 PowerShell 7 / Windows PowerShell / Git Bash / cmd / WSL（按发行版分条），可自定义起始目录、启动参数和环境变量。
- **Telnet**：最小 IAC 协商、NAWS 窗口尺寸上报，用于 BBS / 网络设备。
- **多标签与分屏**：标签拖动排序、横向滚动、左右 / 上下分屏，同一主机开多个会话时自动在标签名后标出 `:2`、`:3` 加以区分（序号取最小空号，前面的会话关掉后已有标签的序号不变）；断线保留终端内容，可在原标签内回车重连，并按退避策略自动重连。
- **SFTP 文件管理**：双栏浏览本机与远端目录，支持上传、下载、拖放、新建、重命名、删除；上传先写同目录暂存文件，校验完成后再提交，下载半途失败不会留下残缺文件。连续下载多个文件不必反复挑位置——行右键「下载」直接落到上次的保存目录（同名自动加序号，完成提示写全路径），要换目录用「下载到…（换目录）」。
- **文件面板跟随终端目录**：面板底栏的「定位」一次跳到终端当前目录、「跟随」则持续跟。连上 bash / zsh 会话时会自动追加一条提示符钩子，用 OSC 7 上报 `$PWD`（保留你原有的 `PROMPT_COMMAND`，可在「设置 → 终端」关闭）；探测不到 shell 时退回识别命令行的 `cd`。两条线索都要经 SFTP 确认确实是目录才动面板，按钮提示会显示当前识别到的路径。
- **端口转发与跳板机**：本地 `-L`、远程 `-R`、动态 SOCKS5 `-D`；跳板机经 `direct-tcpip` 逐跳连接，逐跳校验主机指纹。
- **远程桌面**：RDP（IronRDP）与 VNC（RFB），支持剪贴板文本同步、拖放上传、动态分辨率（RDP）、失焦自动释放按键。
- **终端查找、键宏与命令片段**：`Ctrl+F` 查找含回滚历史；键宏可把组合键映射为多步文本（每步可带延迟）；命令片段库内置约 200 条 Windows、Linux / Unix 与 Docker 常用命令，平台作一级分类，系统信息 / 软件包 / 环境搭建 / 进程 / 服务 / 用户与权限 / 网络 / 文件与压缩 / 文本与日志 / 定时任务 / 容器 / 维护作二级分组，默认展示当前会话的平台、也可一键切换查看其它平台，还可保存自己的单行命令，一键插入或执行到当前会话。常见开发环境（build-essential、常用命令行工具、Node.js、Rust、Docker、Homebrew、WSL 等）与难记的一行命令都收在其中；版本差异导致命令不可用时，提示里会给出替代做法（如没有 systemd 改用 `service`、没有 `ss` 改用 `netstat`、非 Debian 系改用 `dnf` / `pacman`），并收录了对应的兼容命令。右侧竖排导航轨的「命令片段」按钮（或 `Ctrl+Shift+S`）开关片段面板：面板分「我的片段 / 内置命令」两页，内置页默认对齐当前会话平台、也可切换其它平台，两页都可搜索、单击插入、▶ 执行；我的片段底部可「导入… / 导出…」JSON（按名称与命令去重，方便分享或迁移），右键可编辑或删除，新建 / 编辑直接在面板内联完成；内置命令右键可存为我的片段。
- **输入即补全**：在终端里敲命令时自动弹出候选浮层，按正在输入的那个词在命令行里的位置给候选——命令位整库搜索（片段名称、命令与备注都算，敲「磁盘」能找到 `df -h`），参数位只接得上前文的写法（`git re` → `git remote add …`、`sudo apt up` → `sudo apt update …`、`git -` → 该命令用过的 `-m` / `--amend`），对不上的参数不提示（`cd find` 不会冒出 `find` 命令，Tab 交给 shell 自己的路径补全）。内置命令只提示当前会话所属平台的（Linux SSH 里不会冒出 PowerShell / 注册表命令，识别不出平台时只提示 Docker 等通用命令）；**执行过的命令也是候选**——`git commit -m "fix login crash"` 跑过一次后，下次只敲 `git commit`、甚至只敲参数里的 `login`，都能把上次那条整条捞回来（浮层标「历史」，按最近使用排最前，`Tab` 一次补齐整条，已经存成片段或内置命令的同一条不会重复出现）；`↑↓` 选择、`Tab` / `Enter` 补全、`Esc` 关闭。补全只替换正在输入的那个词，不执行命令，再按一次回车才运行。嫌它碍事时，去「终端 → 输入提示」选「关闭输入提示」（勾会跟着走），要回来选「输入时弹出候选」；「设置 → 终端」的复选框同步的是同一项。
- **命令历史**：自动记录终端里执行过的命令（无回显的密码输入不会被记录），右侧竖排导航轨的「历史」按钮开关历史面板，可搜索、插入、重跑或一键存为命令片段（`Ctrl+Shift+H` 同效）；「终端 → 命令历史…」仍提供完整管理弹窗。
- **右侧停靠区**：命令片段在上、命令历史在下共用一块停靠区，中间分隔条可上下拖动调整占比，拖左缘可改宽度；两个分区各自独立展开，全部收起时停靠区隐藏。还没有保存过片段时，打开片段面板会直接落在「内置命令」页，不会是一块空地。
- **同步输入**：把当前终端的按键与粘贴实时写入其它在线会话，也可把一条命令发送到全部会话。
- **主机管理**：分组树、收藏置顶、手动 / 按名称 / 按最近连接排序、最近连接、局域网发现、`~/.ssh/config` 导入、主机配置 JSON 导入 / 导出。
- **主题与语言**：明暗主题、中文 / English 界面，偏好随凭据库持久化。
- **单实例与系统托盘**：关窗可隐藏到托盘继续保活；重复启动会把已有窗口带到前台。

## 安全与数据

主机配置、应用设置与主机指纹存放在单个加密库 `ruterm.db`（toasty + turso，AEGIS-256 页级加密）。密码等字段再经 AES-256-GCM 单独加密；未勾选「保存密码」时，密码只在本次运行的内存中持有。

解锁方式二选一：

- **系统凭据管理器**（默认）：随机主密钥存入 Windows Credential Manager / macOS Keychain / Secret Service，启动免输入。
- **主密码**：argon2id 派生密钥，可把加密库带到其它机器解锁；忘记主密码无法找回。

主机指纹采用 TOFU 策略，首次连接记录，之后不匹配即拒绝连接。

数据目录在启动时决定：可执行文件同目录存在 `data/` 时为便携模式，否则使用全局目录 `~/.ruterm`（Windows 为 `%USERPROFILE%\.ruterm`）。该判定只探测不创建，想切换便携模式时手动建一个空 `data/` 即可。

远程文件默认作为**文本**在系统编辑器中打开，不会按扩展名执行；下载内容放在独立临时目录中。

## 构建与运行

需要 Rust 1.88+（edition 2024；代码用到 let-chains 与 `slice::as_chunks`）。主要面向 Windows，同时保留 Linux / macOS 构建路径。

```bash
cargo run --release      # 启动 GUI
cargo test               # 运行测试
```

命令行快速连接：

```bash
ruterm [user@]host[:port] [-i keyfile]
ruterm -h | --help       # 用法说明
ruterm -V | --version    # 打印版本号
```

用户名缺省取 `$USER` / `$USERNAME`，端口缺省 22；密码通过环境变量 `RUTERM_PASSWORD` 提供，不接受命令行密码参数。不带参数启动即进入主界面。

## 常用快捷键

| 按键 | 动作 |
|------|------|
| `Ctrl+N` | 新建主机 |
| `Ctrl+Shift+T` | 新建本地终端 |
| `Ctrl+Shift+R` | 新建远程桌面 |
| `Ctrl+Tab` / `Ctrl+Shift+Tab` | 下一个 / 上一个标签 |
| `Ctrl+Shift+W` | 关闭当前标签 |
| `Ctrl+Shift+D` / `Ctrl+Shift+E` | 左右 / 上下分屏 |
| `Ctrl+Shift+B` / `Ctrl+Shift+L` / `Ctrl+Shift+F` | 文件 / 本机 / 转发面板 |
| `Ctrl+Shift+M` | 键宏管理器 |
| `Ctrl+Shift+S` | 命令片段面板（右侧） |
| `Ctrl+Shift+H` | 命令历史面板（右侧） |
| `Ctrl+F` | 终端内查找 |
| `Ctrl+T` | 切换明暗主题 |
| `Ctrl+,` | 设置 |

终端内的 `Ctrl+W`、`Tab`、`Shift+Tab` 原样送给 shell，不被应用快捷键拦截。

## 项目结构

```
src/
  main.rs         应用入口、动作绑定、窗口与托盘
  app/            UI 与状态（主机、标签、文件、转发、设置、远程桌面）
  session.rs      SSH / Telnet 会话生命周期
  transfer.rs     SFTP 传输（暂存、校验、提交）
  vault.rs        加密库、主密码、known_hosts
  desktop/        RDP / VNC 后端
  local_shell.rs  本机 Shell 探测与启动
  snippets.rs     命令片段模型与校验
  snippet_presets.rs 内置常用命令
  completion.rs   终端输入补全的匹配与合并
  history.rs      命令历史模型
  sessions.rs     JSON 降级存储与设置持久化
vendor/           少量上游依赖的本地修补版本（保留原许可证）
```

完整设计说明与路线图见 [`docs/design-roadmap.md`](docs/design-roadmap.md)，各版本的变化见 [`CHANGELOG.md`](CHANGELOG.md)。

## 许可证

本项目使用 [Apache-2.0](LICENSE)。
