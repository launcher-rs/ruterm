# RuTerm

Rust 编写的桌面远程连接工具：SSH 终端、本地 Shell、SFTP 文件管理、端口转发和远程桌面集中在一个窗口里。基于 [rgpui](https://crates.io/crates/rgpui) 与 [rgpui-term](https://crates.io/crates/rgpui-term) 构建。

## 主要功能

- **SSH 会话**：russh + 交互式 PTY（`xterm-256color`），支持 Agent、私钥、密码三种认证；私钥可引用本地文件，也可整份加密存入凭据库。
- **本地终端**：自动探测 PowerShell 7 / Windows PowerShell / Git Bash / cmd / WSL（按发行版分条），可自定义起始目录、启动参数和环境变量。
- **Telnet**：最小 IAC 协商、NAWS 窗口尺寸上报，用于 BBS / 网络设备。
- **多标签与分屏**：标签拖动排序、横向滚动、左右 / 上下分屏；断线保留终端内容，可在原标签内回车重连，并按退避策略自动重连。
- **SFTP 文件管理**：双栏浏览本机与远端目录，支持上传、下载、拖放、新建、重命名、删除；上传先写同目录暂存文件，校验完成后再提交，下载半途失败不会留下残缺文件。
- **端口转发与跳板机**：本地 `-L`、远程 `-R`、动态 SOCKS5 `-D`；跳板机经 `direct-tcpip` 逐跳连接，逐跳校验主机指纹。
- **远程桌面**：RDP（IronRDP）与 VNC（RFB），支持剪贴板文本同步、拖放上传、动态分辨率（RDP）、失焦自动释放按键。
- **终端查找与键宏**：`Ctrl+F` 查找含回滚历史；键宏可把组合键映射为多步文本（每步可带延迟）。
- **同步输入**：把当前终端的按键与粘贴实时写入其它在线会话，也可把一条命令发送到全部会话。
- **主机管理**：分组树、收藏置顶、组内排序、最近连接、局域网发现、`~/.ssh/config` 导入、主机配置 JSON 导入 / 导出。
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

需要 Rust 2024 edition 工具链（Cargo 1.85+）。主要面向 Windows，同时保留 Linux / macOS 构建路径。

```bash
cargo run --release      # 启动 GUI
cargo test               # 运行测试
```

命令行快速连接：

```bash
ruterm [user@]host[:port] [-i keyfile]
```

用户名缺省取 `$USER` / `$USERNAME`，端口缺省 22；密码通过环境变量 `ruterm_PASSWORD` 提供，不接受命令行密码参数。不带参数启动即进入主界面。

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
  sessions.rs     JSON 降级存储与设置持久化
vendor/           少量上游依赖的本地修补版本（保留原许可证）
```

完整设计说明与路线图见 [`doc/design-roadmap.md`](doc/design-roadmap.md)。

## 许可证

本项目使用 [Apache-2.0](LICENSE)。

图标来自 [Lucide](https://lucide.dev)，按 ISC 许可证使用，见 [`assets/lucide/LICENSE`](assets/lucide/LICENSE)。`vendor/` 下的第三方代码保留各自原始的 Apache-2.0 / MIT 许可证文件。
