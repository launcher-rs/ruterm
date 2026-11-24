//! 内置常用命令：按平台与子分类分组、只读展示；用户可以「存为我的片段」再改成自己的。
//!
//! 平台决定这条命令能不能在当前会话里用（补全据此过滤），子分类只影响展示顺序。
//! 名称 / 备注按中文键走 i18n，命令本身原样发送。

use crate::platform::Platform;

/// 一个内置命令条目。
pub struct PresetSnippet {
    /// 适用的平台。可以有多项：`df`、`tar` 这类 POSIX 通用命令同时标 Linux 与 Unix。
    pub platforms: &'static [Platform],
    /// 子分类，取值必须在 [`PRESET_GROUPS`] 里。
    pub group: &'static str,
    pub name: &'static str,
    pub command: &'static str,
    pub note: &'static str,
}

/// 子分类展示顺序。
pub const PRESET_GROUPS: &[&str] = &[
    "系统信息",
    "软件包",
    "环境搭建",
    "磁盘与内存",
    "进程",
    "服务",
    "用户与权限",
    "网络",
    "文件与压缩",
    "文本与日志",
    "定时任务",
    "容器",
    "维护",
];

impl PresetSnippet {
    /// 表内归属平台，仅测试用；列表按 [`Self::supports`] 过滤，
    /// 所以同时支持 Linux / Unix 的命令会在两边的平台页都出现。
    #[cfg(test)]
    pub fn platform(&self) -> Platform {
        self.platforms[0]
    }

    /// 这条命令能否在给定平台使用。
    pub fn supports(&self, platform: Platform) -> bool {
        self.platforms.contains(&platform)
    }

    /// 系统版本差异导致命令不可用时的替代做法。
    ///
    /// 只做提示，不改变发送内容；用户在提示里看到 "没有 X" 时知道该换什么命令。
    pub fn fallback(&self) -> Option<&'static str> {
        let platform = *self.platforms.first()?;
        Some(match (platform, self.name) {
            (Platform::Linux, "系统更新") => {
                "非 Debian 系：sudo dnf upgrade -y（Fedora / RHEL）"
            }
            (Platform::Linux, "安装软件包") => {
                "非 Debian 系：dnf install -y <包名>，Arch 用 pacman -S <包名>"
            }
            (Platform::Linux, "卸载软件包") => {
                "非 Debian 系：dnf remove <包名> 或 pacman -Rns <包名>"
            }
            (Platform::Linux, "搜索软件包") => {
                "非 Debian 系：dnf search <关键词> 或 pacman -Ss <关键词>"
            }
            (Platform::Linux, "查看系统版本") => "没有 /etc/os-release 时：cat /etc/issue",
            (Platform::Linux, "开机时间") => "老系统没有 uptime -s：改用 who -b",
            (Platform::Linux, "服务状态") => "没有 systemd：sudo service <服务名> status",
            (Platform::Linux, "启动服务") => "没有 systemd：sudo service <服务名> start",
            (Platform::Linux, "停止服务") => "没有 systemd：sudo service <服务名> stop",
            (Platform::Linux, "重启服务") => "没有 systemd：sudo service <服务名> restart",
            (Platform::Linux, "开机自启") => "没有 systemd：sudo update-rc.d <服务名> defaults",
            (Platform::Linux, "取消自启") => {
                "没有 systemd：sudo update-rc.d -f <服务名> remove"
            }
            (Platform::Linux, "失败的单元") => "没有 systemd：用 /var/log 下的日志排查",
            (Platform::Linux, "运行中的服务") => "没有 systemd：sudo service --status-all",
            (Platform::Linux, "跟踪服务日志") => {
                "没有 journalctl：sudo tail -f /var/log/syslog"
            }
            (Platform::Linux, "监听端口") => "没有 ss：sudo netstat -tlnp（需 net-tools）",
            (Platform::Linux, "查看本机地址") => "没有 ip：ifconfig -a（需 net-tools）",
            (Platform::Linux, "路由表") => "没有 ip：route -n（需 net-tools）",
            (Platform::Linux, "连接列表") => "没有 ss：sudo netstat -tunap",
            (Platform::Linux, "DNS 解析") => "没有 getent：改用 nslookup <域名>",
            (Platform::Linux, "防火墙状态") => {
                "没有 ufw：firewalld 用 firewall-cmd --state，iptables 用 iptables -L -n"
            }
            (Platform::Linux, "开放端口") => {
                "没有 ufw：firewall-cmd --add-port=<端口>/tcp --permanent 再 --reload"
            }
            (Platform::Linux, "抓包") => "没有 tcpdump：sudo apt install tcpdump",
            (Platform::Linux, "系统错误日志") => {
                "没有 journalctl：sudo tail -n 100 /var/log/syslog"
            }
            (Platform::Linux, "按时间看日志") => {
                "没有 journalctl：sudo tail -n 100 /var/log/syslog"
            }
            (Platform::Linux, "SSH 登录日志") => "没有 journalctl：grep sshd /var/log/auth.log",
            (Platform::Linux, "格式化 JSON") => "没有 jq：sudo apt install jq",
            (Platform::Linux, "cron 执行日志") => "没有 journalctl：grep CRON /var/log/syslog",
            (Platform::Linux, "清理旧日志") => "没有 journalctl：手动清理 /var/log 下的旧文件",
            (Platform::Linux, "磁盘健康") => "没有 smartctl：sudo apt install smartmontools",
            (Platform::Linux, "进程打开的文件") => "没有 lsof：sudo apt install lsof",
            (Platform::Linux, "安装编译工具链") => {
                "非 Debian 系：sudo dnf groupinstall \"Development Tools\""
            }
            (Platform::Linux, "安装 Docker") => {
                "脚本失败时：按官方文档添加 apt 源后安装 docker-ce"
            }
            (Platform::Linux, "安装 Compose 插件") => {
                "旧源没有此包：改用 sudo apt install docker-compose（命令是 docker-compose）"
            }
            (Platform::Linux, "Docker 免 sudo") => "仍提示权限不足：退出并重新登录",
            (Platform::Linux, "安装 Node.js LTS") => "脚本下载失败：改用 nvm 或发行版自带 nodejs",
            (Platform::Linux, "安装 Rust 工具链") => {
                "找不到 curl：先装 curl，或从官网下载 rustup-init"
            }
            (Platform::Linux, "公网 IP") => "curl 不可用：wget -qO- https://ifconfig.me",
            (Platform::Linux, "结束端口占用") => {
                "没有 fuser：先用 sudo ss -tlnp 找到 PID，再 kill"
            }
            (Platform::Linux, "测试端口") => {
                "没有 nc：用 curl -v telnet://<主机>:<端口>，或 bash 的 /dev/tcp"
            }
            (Platform::Linux, "按端口找进程") => {
                "没有 lsof：Linux 用 sudo ss -tlnp | grep :<端口>"
            }
            (Platform::Linux, "解压 zip") => {
                "没有 unzip：Linux 先装 unzip，macOS 用 ditto -x -k <文件.zip> <目标目录>"
            }
            (Platform::Linux, "同步目录") => {
                "没有 rsync：Linux 装 rsync，macOS 用 brew install rsync"
            }
            (Platform::Linux, "上传公钥") => {
                "没有 ssh-copy-id：ssh <用户>@<主机> 'mkdir -p ~/.ssh && cat >> ~/.ssh/authorized_keys' < ~/.ssh/id_ed25519.pub"
            }
            (Platform::Unix, "DNS 解析") => "没有 dig：改用 nslookup <域名>",
            (Platform::Unix, "查看系统日志") => "日志太大时用 --predicate 过滤进程名",
            _ => return None,
        })
    }
}

/// 某会话平台可用的内置命令（下标 + 条目，保持表内顺序）。
///
/// 会话平台未知（Telnet、探测失败的 SSH）时只给跨平台命令：宁可少提示，
/// 也不要把 PowerShell 命令塞进 Linux shell，反之亦然。
pub fn presets_for(
    platform: Option<Platform>,
) -> impl Iterator<Item = (usize, &'static PresetSnippet)> {
    PRESETS
        .iter()
        .enumerate()
        .filter(move |(_, preset)| match platform {
            // 通用命令（Docker）在任何已识别的平台上都可用。
            Some(platform) => preset.supports(platform) || preset.supports(Platform::Cross),
            None => preset.supports(Platform::Cross),
        })
}

pub const PRESETS: &[PresetSnippet] = &[
    // ---- Windows（PowerShell）----
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "系统版本",
        command: "Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, LastBootUpTime",
        note: "系统名称、版本与上次启动时间",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "硬件概览",
        command: "Get-CimInstance Win32_ComputerSystem | Select-Object Manufacturer, Model, TotalPhysicalMemory",
        note: "厂商、型号与物理内存",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "查看环境变量",
        command: "Get-ChildItem Env: | Sort-Object Name",
        note: "按名称排序的全部环境变量",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "当前用户",
        command: "whoami",
        note: "当前登录账号与域",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "最近补丁",
        command: "Get-HotFix | Sort-Object InstalledOn -Descending | Select-Object -First 10 HotFixID, Description, InstalledOn",
        note: "最近安装的 10 个系统补丁",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "激活状态",
        command: "slmgr /xpr",
        note: "弹窗显示激活类型与到期时间",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "系统信息",
        name: "电池健康报告",
        command: "powercfg /batteryreport",
        note: "在当前目录生成 HTML，对比设计容量与当前容量",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "已安装软件",
        command: "Get-ItemProperty 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' | Select-Object DisplayName, DisplayVersion | Sort-Object DisplayName",
        note: "读取卸载注册表项，只含系统级安装",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "搜索软件",
        command: "winget search <关键词>",
        note: "在 winget 源里按名称搜索",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "安装软件",
        command: "winget install <包名>",
        note: "包名可先用「搜索软件」查",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "升级全部软件",
        command: "winget upgrade --all",
        note: "升级所有可升级的 winget 软件",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "卸载软件",
        command: "winget uninstall <包名>",
        note: "卸载 winget 安装的软件",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "软件包",
        name: "已安装软件（winget）",
        command: "winget list <关键词>",
        note: "列出已装软件，留空看全部、带关键词过滤",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 WSL",
        command: "wsl --install",
        note: "一条命令装好 WSL 与默认发行版，装完重启",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "WSL 发行版",
        command: "wsl -l -v",
        note: "查看已安装的发行版、版本与运行状态",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 Git",
        command: "winget install --id Git.Git -e",
        note: "装上 Git 与 Git Bash",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 Node.js LTS",
        command: "winget install --id OpenJS.NodeJS.LTS -e",
        note: "安装长期支持版 Node.js 与 npm",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 Python",
        command: "winget install --id Python.Python.3.12 -e",
        note: "版本号可换成要装的 3.x",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 VS Code",
        command: "winget install --id Microsoft.VisualStudioCode -e",
        note: "编辑器本体，命令行入口是 code",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 C++ 编译工具",
        command: "winget install --id Microsoft.VisualStudio.2022.BuildTools -e",
        note: "装时勾选「使用 C++ 的桌面开发」，才有 cl.exe",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 PowerShell 7",
        command: "winget install --id Microsoft.PowerShell -e",
        note: "新版跨平台 PowerShell，命令是 pwsh",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "环境搭建",
        name: "安装 Docker Desktop",
        command: "winget install --id Docker.DockerDesktop -e",
        note: "需要开启虚拟化，装完重启",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "磁盘与内存",
        name: "磁盘余量",
        command: "Get-Volume | Select-Object DriveLetter, FileSystemLabel, @{n='FreeGB';e={[math]::Round($_.SizeRemaining/1GB,1)}}",
        note: "各卷的剩余空间（GB）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "磁盘与内存",
        name: "内存占用前 10",
        command: "Get-Process | Sort-Object WorkingSet64 -Descending | Select-Object -First 10 Name, @{n='MemoryMB';e={[math]::Round($_.WorkingSet64/1MB,1)}}",
        note: "按工作集列出内存占用最高的进程",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "磁盘与内存",
        name: "物理磁盘健康",
        command: "Get-PhysicalDisk | Select-Object FriendlyName, MediaType, HealthStatus, OperationalStatus, Size",
        note: "磁盘类型、健康状态与容量",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "磁盘与内存",
        name: "磁盘序列号",
        command: "Get-Disk | Select-Object Number, FriendlyName, SerialNumber",
        note: "报修或查保修时需要",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "磁盘与内存",
        name: "大文件排行",
        command: "Get-ChildItem -Path C:\\ -Recurse -File -ErrorAction SilentlyContinue | Sort-Object Length -Descending | Select-Object -First 20 FullName, @{n='MB';e={[math]::Round($_.Length/1MB,1)}}",
        note: "从 C 盘找最大的 20 个文件，耗时较长",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "进程",
        name: "资源占用排行",
        command: "Get-Process | Sort-Object CPU -Descending | Select-Object -First 15 Id, ProcessName, CPU, @{n='MemoryMB';e={[math]::Round($_.WorkingSet64/1MB,1)}}",
        note: "按 CPU 时间排序的前 15 个进程",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "进程",
        name: "结束进程",
        command: "Stop-Process -Name <进程名> -Force",
        note: "按进程名强制结束（先用「资源占用排行」确认）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "进程",
        name: "进程命令行",
        command: "Get-CimInstance Win32_Process -Filter \"Name='<进程名>.exe'\" | Select-Object ProcessId, CommandLine",
        note: "查看进程的启动参数与路径",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "服务状态",
        command: "Get-Service <服务名>",
        note: "查看 Windows 服务状态",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "启动服务",
        command: "Start-Service <服务名>",
        note: "启动已停止的服务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "停止服务",
        command: "Stop-Service <服务名>",
        note: "停止正在运行的服务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "重启服务",
        command: "Restart-Service <服务名>",
        note: "改完配置后重启服务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "设为自动启动",
        command: "Set-Service <服务名> -StartupType Automatic",
        note: "让服务随系统启动",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "服务",
        name: "自动启动服务列表",
        command: "Get-Service | Where-Object StartType -eq 'Automatic' | Sort-Object Name | Select-Object Status, Name, DisplayName",
        note: "所有设为自动启动的服务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "用户与权限",
        name: "本地用户列表",
        command: "Get-LocalUser | Select-Object Name, Enabled, LastLogon",
        note: "本机账号、启用状态与最后登录",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "用户与权限",
        name: "管理员组成员",
        command: "Get-LocalGroupMember -Group Administrators",
        note: "本机管理员组里的账号",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "用户与权限",
        name: "新建本地用户",
        command: "New-LocalUser -Name <用户名> -Password (Read-Host -AsSecureString '密码')",
        note: "交互输入密码，明文不落历史",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "用户与权限",
        name: "加入管理员组",
        command: "Add-LocalGroupMember -Group Administrators -Member <用户名>",
        note: "把账号加进本机管理员组",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "查看 IP 配置",
        command: "Get-NetIPConfiguration",
        note: "网卡、地址、网关与 DNS",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "监听端口",
        command: "Get-NetTCPConnection -State Listen | Sort-Object LocalPort | Select-Object LocalAddress, LocalPort, OwningProcess",
        note: "TCP 监听端口与对应进程 ID",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "测试端口",
        command: "Test-NetConnection <主机> -Port <端口>",
        note: "如 Test-NetConnection example.com -Port 443",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "查看路由",
        command: "Get-NetRoute -DestinationPrefix '0.0.0.0/0'",
        note: "默认路由",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "DNS 解析",
        command: "Resolve-DnsName <域名>",
        note: "用 Windows DNS 客户端解析域名",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "刷新 DNS 缓存",
        command: "Clear-DnsClientCache",
        note: "域名变更后清掉本机缓存",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "查看 ARP",
        command: "arp -a",
        note: "本机 ARP 邻居表",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "按端口找进程",
        command: "Get-NetTCPConnection -LocalPort <端口> | Select-Object LocalAddress, LocalPort, State, OwningProcess",
        note: "端口被哪个进程占用",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "WiFi 密码",
        command: "netsh wlan show profile name=\"<WiFi名称>\" key=clear",
        note: "查看已保存网络的明文密码（需管理员）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "网络",
        name: "公网 IP",
        command: "(Invoke-RestMethod https://ifconfig.me)",
        note: "查本机出口 IP（需要外网）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "查找文件",
        command: "Get-ChildItem -Path C:\\ -Recurse -Filter '<文件名>' -ErrorAction SilentlyContinue",
        note: "从 C 盘按名称递归查找，权限错误自动忽略",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "下载文件",
        command: "Invoke-WebRequest -Uri <URL> -OutFile <文件名>",
        note: "把 URL 下载到当前目录",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "目录大小",
        command: "Get-ChildItem <目录> -Recurse -File | Measure-Object Length -Sum | Select-Object @{n='MB';e={[math]::Round($_.Sum/1MB,1)}}",
        note: "递归统计目录总大小",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "压缩目录",
        command: "Compress-Archive -Path <目录> -DestinationPath <归档.zip> -Force",
        note: "覆盖同名 zip",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "解压 zip",
        command: "Expand-Archive -Path <文件.zip> -DestinationPath <目标目录> -Force",
        note: "解压到指定目录",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "文件哈希",
        command: "Get-FileHash <文件> -Algorithm SHA256",
        note: "校验下载文件完整性",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "最近修改的文件",
        command: "Get-ChildItem -Recurse -File | Sort-Object LastWriteTime -Descending | Select-Object -First 20 FullName, LastWriteTime",
        note: "当前目录最近改动的 20 个文件",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文件与压缩",
        name: "镜像目录",
        command: "robocopy <源目录> <目标目录> /E /COPYALL /R:1 /W:1",
        note: "robocopy 参数难记：复制子目录、权限并少重试",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文本与日志",
        name: "实时跟踪日志",
        command: "Get-Content <文件> -Wait -Tail 50",
        note: "先显示末 50 行再持续追加",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文本与日志",
        name: "搜索文本",
        command: "Select-String -Path <文件> -Pattern '<关键词>'",
        note: "按关键词查找并显示行号",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文本与日志",
        name: "系统日志错误",
        command: "Get-WinEvent -FilterHashtable @{LogName='System'; Level=2} -MaxEvents 20 | Select-Object TimeCreated, Id, ProviderName, Message",
        note: "最近 20 条系统错误",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "文本与日志",
        name: "统计行数",
        command: "(Get-Content <文件> | Measure-Object -Line).Lines",
        note: "文件行数",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "定时任务",
        name: "计划任务列表",
        command: "Get-ScheduledTask | Select-Object TaskPath, TaskName, State",
        note: "所有计划任务及状态",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "定时任务",
        name: "任务详情",
        command: "Get-ScheduledTask -TaskName <任务名> | Format-List *",
        note: "触发器、动作与运行账号",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "定时任务",
        name: "立即运行任务",
        command: "Start-ScheduledTask -TaskName <任务名>",
        note: "手动触发一次计划任务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "定时任务",
        name: "禁用任务",
        command: "Disable-ScheduledTask -TaskName <任务名>",
        note: "暂停计划任务",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "维护",
        name: "系统文件检查",
        command: "sfc /scannow",
        note: "扫描并修复系统文件（需管理员）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "维护",
        name: "映像修复",
        command: "DISM /Online /Cleanup-Image /RestoreHealth",
        note: "修复组件存储（需管理员，耗时较长）",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "维护",
        name: "重启资源管理器",
        command: "Stop-Process -Name explorer -Force",
        note: "任务栏或桌面卡住时重启外壳",
    },
    PresetSnippet {
        platforms: &[Platform::Windows],
        group: "维护",
        name: "启动项列表",
        command: "Get-CimInstance Win32_StartupCommand | Select-Object Name, Command, Location, User",
        note: "开机自启的程序与位置",
    },
    // ---- Linux / Unix 共用（POSIX）----
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "系统信息",
        name: "内核与架构",
        command: "uname -a",
        note: "内核版本、主机名与架构",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "系统信息",
        name: "运行时长与负载",
        command: "uptime",
        note: "运行时长与 1/5/15 分钟平均负载",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "系统信息",
        name: "当前用户",
        command: "whoami",
        note: "当前登录账号",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "磁盘与内存",
        name: "查看磁盘余量",
        command: "df -h",
        note: "按挂载点列出磁盘使用率",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "磁盘与内存",
        name: "目录大小",
        command: "du -sh <目录>",
        note: "只看目录总占用",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "进程",
        name: "进程列表",
        command: "ps aux | sort -nrk 3 | head -15",
        note: "按 CPU 占用排出前 15 个进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "进程",
        name: "结束进程",
        command: "kill -9 <PID>",
        note: "强制结束（先用「进程列表」确认 PID）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "进程",
        name: "按名称结束",
        command: "pkill -f <进程名>",
        note: "匹配完整命令行后结束进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "用户与权限",
        name: "用户所属组",
        command: "groups <用户名>",
        note: "该用户加入的所有组",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "用户与权限",
        name: "修改密码",
        command: "passwd",
        note: "修改当前用户密码（交互输入）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "网络",
        name: "测试网络连通",
        command: "ping -c 4 1.1.1.1",
        note: "发 4 个包测试外网连通性",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "网络",
        name: "测试端口",
        command: "nc -zv <主机> <端口>",
        note: "探测端口是否开放（需 netcat）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "网络",
        name: "按端口找进程",
        command: "lsof -i :<端口>",
        note: "哪个进程占用了端口（需 lsof）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "查找文件",
        command: "sudo find / -name '<文件名>' 2>/dev/null",
        note: "从根目录按名称查找文件",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "解压 tar.gz",
        command: "tar -xzvf <归档.tar.gz>",
        note: "解压到当前目录",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "打包目录",
        command: "tar -czvf <归档.tar.gz> <目录>",
        note: "把目录压缩成 tar.gz",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "解压 zip",
        command: "unzip <文件.zip> -d <目标目录>",
        note: "解压到指定目录（需 unzip）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "解压 tar.xz",
        command: "tar -xJvf <归档.tar.xz>",
        note: ".xz 用大写 J，.gz 用 z",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文件与压缩",
        name: "同步目录",
        command: "rsync -avz --progress <源目录>/ <用户>@<主机>:<目标目录>/",
        note: "增量同步到远端；尾部斜杠表示同步目录内容",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文本与日志",
        name: "实时跟踪日志",
        command: "tail -f <日志文件>",
        note: "实时追加输出，Ctrl+C 退出",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文本与日志",
        name: "搜索文本",
        command: "grep -rn '<关键词>' <目录>",
        note: "递归搜索并显示行号",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文本与日志",
        name: "排序去重计数",
        command: "sort <文件> | uniq -c | sort -nr",
        note: "按出现次数从高到低排列",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "文本与日志",
        name: "统计行数",
        command: "wc -l <文件>",
        note: "文件行数",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "定时任务",
        name: "查看定时任务",
        command: "crontab -l",
        note: "当前用户的 crontab",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "定时任务",
        name: "编辑定时任务",
        command: "crontab -e",
        note: "用默认编辑器修改当前用户任务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "环境搭建",
        name: "生成 SSH 密钥",
        command: "ssh-keygen -t ed25519 -C \"<邮箱>\"",
        note: "Ed25519 更短更安全，默认写到 ~/.ssh",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "环境搭建",
        name: "上传公钥",
        command: "ssh-copy-id <用户>@<主机>",
        note: "把本机公钥追加到远端，之后免密码登录",
    },
    // ---- Linux（Debian / Ubuntu）----
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "系统信息",
        name: "查看系统版本",
        command: "cat /etc/os-release",
        note: "发行版与版本号",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "系统信息",
        name: "最近登录",
        command: "last -n 10",
        note: "最近 10 次登录记录",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "系统信息",
        name: "CPU 信息",
        command: "lscpu",
        note: "核心数、频率与虚拟化支持",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "系统信息",
        name: "开机时间",
        command: "uptime -s",
        note: "系统最近一次启动的时间点",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "系统更新",
        command: "sudo apt update && sudo apt upgrade -y",
        note: "更新软件包索引并升级已安装的软件",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "安装软件包",
        command: "sudo apt install <包名>",
        note: "把 <包名> 换成要安装的软件，回车前可再确认",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "安装软件（RHEL / CentOS）",
        command: "sudo dnf install -y <包名>",
        note: "Fedora / RHEL 8+；老版本把 dnf 换成 yum",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "安装软件（Arch）",
        command: "sudo pacman -S <包名>",
        note: "Arch / Manjaro",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "卸载软件包",
        command: "sudo apt remove <包名>",
        note: "保留配置文件；彻底删除换成 purge",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "搜索软件包",
        command: "apt search <关键词>",
        note: "按名称与描述搜索",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "包详情",
        command: "apt show <包名>",
        note: "版本、依赖与来源",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "清理无用依赖",
        command: "sudo apt autoremove -y && sudo apt clean",
        note: "删除不再需要的依赖与下载缓存",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "包内文件列表",
        command: "dpkg -L <包名>",
        note: "这个包装了哪些文件",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "软件包",
        name: "文件属于哪个包",
        command: "dpkg -S <文件路径>",
        note: "反查命令或文件来自哪个包",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装编译工具链",
        command: "sudo apt update && sudo apt install -y build-essential",
        note: "gcc / g++ / make，新机器开发环境第一步",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装常用命令行工具",
        command: "sudo apt install -y git curl wget unzip vim htop",
        note: "新机器常备的一组工具",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 Python 开发环境",
        command: "sudo apt install -y python3 python3-pip python3-venv",
        note: "含 pip 与虚拟环境 venv",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 Node.js LTS",
        command: "curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - && sudo apt install -y nodejs",
        note: "用 NodeSource 官方源装 LTS",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 Rust 工具链",
        command: "curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh",
        note: "装好 rustc / cargo，重开 shell 生效",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 Docker",
        command: "curl -fsSL https://get.docker.com | sudo sh",
        note: "Docker 官方一键安装脚本",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "Docker 免 sudo",
        command: "sudo usermod -aG docker \"$USER\"",
        note: "加入 docker 组，重新登录后生效",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 Compose 插件",
        command: "sudo apt install -y docker-compose-plugin",
        note: "装好后用 docker compose 子命令",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "安装 JDK",
        command: "sudo apt install -y openjdk-21-jdk",
        note: "用 java -version 验证",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "设置时区",
        command: "sudo timedatectl set-timezone Asia/Shanghai",
        note: "日志时间不对时先改这里",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "时间同步",
        command: "sudo timedatectl set-ntp true",
        note: "开启 NTP 自动校时",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "环境搭建",
        name: "修改主机名",
        command: "sudo hostnamectl set-hostname <主机名>",
        note: "改完重新登录，提示符才会变",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "磁盘与内存",
        name: "目录占用排行",
        command: "du -sh * 2>/dev/null | sort -hr | head -20",
        note: "当前目录下占用最大的前 20 项",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "磁盘与内存",
        name: "内存与进程",
        command: "free -h && ps aux --sort=-%mem | head -10",
        note: "内存概览与占用最高的进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "磁盘与内存",
        name: "块设备与文件系统",
        command: "lsblk -f",
        note: "磁盘、分区、文件系统与挂载点",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "磁盘与内存",
        name: "inode 使用",
        command: "df -i",
        note: "小文件很多时排查 inode 耗尽",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "磁盘与内存",
        name: "大文件查找",
        command: "sudo find / -xdev -type f -size +100M -exec ls -lh {} + 2>/dev/null | sort -k5 -hr | head -20",
        note: "全盘找大于 100 MB 的文件",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "进程",
        name: "进程树",
        command: "ps -ef --forest",
        note: "查看父子进程关系",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "进程",
        name: "进程打开的文件",
        command: "sudo lsof -p <PID>",
        note: "查进程占用的文件、端口与目录（需 lsof）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "服务状态",
        command: "systemctl status <服务名>",
        note: "查看服务是否在运行",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "服务状态（无 systemd）",
        command: "sudo service <服务名> status",
        note: "老系统或容器里没有 systemctl 时用",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "重启服务",
        command: "sudo systemctl restart <服务名>",
        note: "修改配置后重启服务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "启动服务",
        command: "sudo systemctl start <服务名>",
        note: "启动服务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "停止服务",
        command: "sudo systemctl stop <服务名>",
        note: "停止服务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "开机自启",
        command: "sudo systemctl enable <服务名>",
        note: "让服务随系统启动",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "取消自启",
        command: "sudo systemctl disable <服务名>",
        note: "取消开机自动启动",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "失败的单元",
        command: "systemctl --failed",
        note: "列出启动失败的服务与单元",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "运行中的服务",
        command: "systemctl list-units --type=service --state=running",
        note: "当前正在运行的服务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "服务",
        name: "跟踪服务日志",
        command: "journalctl -u <服务名> -f",
        note: "实时跟踪服务日志，Ctrl+C 退出",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "用户列表",
        command: "cut -d: -f1,3,7 /etc/passwd",
        note: "用户名、UID 与登录 shell",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "新建用户",
        command: "sudo useradd -m -s /bin/bash <用户名>",
        note: "创建家目录并指定 bash",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "加入 sudo 组",
        command: "sudo usermod -aG sudo <用户名>",
        note: "Debian/Ubuntu 的 sudo 组；重新登录生效",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "查看 sudo 权限",
        command: "sudo -l",
        note: "当前用户能执行哪些 sudo 命令",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "修改权限",
        command: "chmod 755 <文件>",
        note: "rwxr-xr-x；数字权限不好记",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "用户与权限",
        name: "修改属主",
        command: "sudo chown -R <用户>:<组> <目录>",
        note: "递归修改文件所有者与组",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "监听端口",
        command: "sudo ss -tlnp",
        note: "列出 TCP 监听端口和对应进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "监听端口（无 ss）",
        command: "sudo netstat -tlnp",
        note: "需要 net-tools；老系统用的替代命令",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "查看本机地址",
        command: "ip -brief addr",
        note: "各网卡的 IPv4 / IPv6 地址",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "路由表",
        command: "ip route",
        note: "默认网关与网段路由",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "DNS 解析",
        command: "getent hosts <域名>",
        note: "按系统解析顺序查地址",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "DNS 解析（无 getent）",
        command: "nslookup <域名>",
        note: "getent 不可用时的替代",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "连接列表",
        command: "ss -tunap",
        note: "所有 TCP / UDP 连接与进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "公网 IP",
        command: "curl -s https://ifconfig.me",
        note: "查本机出口 IP（需要外网）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "结束端口占用",
        command: "sudo fuser -k <端口>/tcp",
        note: "确认后再用，会直接杀掉占用进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "防火墙状态",
        command: "sudo ufw status verbose",
        note: "Ubuntu 防火墙规则",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "开放端口",
        command: "sudo ufw allow <端口>/tcp",
        note: "放行一个 TCP 端口",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "抓包",
        command: "sudo tcpdump -i any -n port <端口>",
        note: "实时抓指定端口的包，Ctrl+C 停止",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "SSH 本地转发",
        command: "ssh -L <本地端口>:localhost:<远端端口> <用户>@<主机>",
        note: "把远端端口映射到本机",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文件与压缩",
        name: "文件哈希",
        command: "sha256sum <文件>",
        note: "校验下载文件完整性",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文件与压缩",
        name: "最近修改的文件",
        command: "find . -type f -printf '%T@ %p\\n' | sort -nr | head -20",
        note: "当前目录最近改动的 20 个文件",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "系统错误日志",
        command: "journalctl -p err -b --no-pager",
        note: "本次启动以来的错误级日志",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "查看 syslog",
        command: "sudo tail -n 100 /var/log/syslog",
        note: "没有 journalctl 时；RHEL 系看 /var/log/messages",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "SSH 登录日志",
        command: "journalctl -u ssh --since today --no-pager",
        note: "今天谁登录过；服务名也可能是 sshd",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "按时间看日志",
        command: "journalctl --since '1 hour ago' --no-pager",
        note: "时间表达式不好记，这里看最近一小时",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "替换文本",
        command: "sed -i.bak 's/<旧>/<新>/g' <文件>",
        note: "原地替换并保留 .bak 备份",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "文本与日志",
        name: "格式化 JSON",
        command: "jq . <文件>",
        note: "美化并高亮 JSON（需 jq）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "定时任务",
        name: "每天凌晨执行",
        command: "(crontab -l 2>/dev/null; echo \"0 3 * * * <命令>\") | crontab -",
        note: "追加一条每天 03:00 执行的任务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "定时任务",
        name: "systemd 定时器",
        command: "systemctl list-timers --all",
        note: "比 cron 更现代的定时方式",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "定时任务",
        name: "cron 执行日志",
        command: "journalctl -u cron --since today --no-pager",
        note: "今天 cron 的执行记录（部分发行版是 crond）",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "维护",
        name: "清理旧日志",
        command: "sudo journalctl --vacuum-time=7d",
        note: "只保留最近 7 天的日志",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "维护",
        name: "开机耗时",
        command: "systemd-analyze blame",
        note: "按耗时排列启动单元",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "维护",
        name: "磁盘健康",
        command: "sudo smartctl -a /dev/<磁盘>",
        note: "查看 SMART 信息（需 smartmontools）",
    },
    // ---- Unix（macOS / BSD / Git Bash）----
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "系统信息",
        name: "macOS 版本",
        command: "sw_vers",
        note: "产品版本与构建号",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "网络",
        name: "DNS 解析",
        command: "dig +short <域名>",
        note: "直接看解析结果（需 dig）",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "网络",
        name: "查看本机地址",
        command: "ifconfig | grep 'inet '",
        note: "macOS / BSD 的网卡地址；Linux 用 ip addr",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "文件与压缩",
        name: "文件哈希",
        command: "shasum -a 256 <文件>",
        note: "macOS 默认没有 sha256sum",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "文本与日志",
        name: "替换文本",
        command: "sed -i '' 's/<旧>/<新>/g' <文件>",
        note: "macOS 的 -i 必须带空参数，与 Linux 不同",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "文本与日志",
        name: "查看系统日志",
        command: "log show --last 1h",
        note: "macOS 统一日志，查最近一小时",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "服务",
        name: "launchd 服务",
        command: "launchctl list | grep <关键词>",
        note: "macOS 的 launchd 服务列表",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "维护",
        name: "刷新 DNS 缓存",
        command: "sudo dscacheutil -flushcache; sudo killall -HUP mDNSResponder",
        note: "仅 macOS 需要这样刷新",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "维护",
        name: "显示隐藏文件",
        command: "defaults write com.apple.finder AppleShowAllFiles -bool true && killall Finder",
        note: "Finder 里显示 . 开头的文件；把 true 换成 false 恢复",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "环境搭建",
        name: "安装 Homebrew",
        command: "/bin/bash -c \"$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)\"",
        note: "macOS 包管理器，装完按提示把 brew 加进 PATH",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "环境搭建",
        name: "安装 Xcode 命令行工具",
        command: "xcode-select --install",
        note: "编译工具与系统头文件，Homebrew 常要先装",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "环境搭建",
        name: "brew 安装",
        command: "brew install <包名>",
        note: "先用 brew search 查包名",
    },
    PresetSnippet {
        platforms: &[Platform::Unix],
        group: "环境搭建",
        name: "brew 升级",
        command: "brew upgrade",
        note: "升级所有通过 brew 安装的软件",
    },
    // ---- 通用（Docker）----
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "运行中的容器",
        command: "docker ps",
        note: "只看当前在运行的容器",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "全部容器",
        command: "docker ps -a",
        note: "包含已停止的容器",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "跟踪容器日志",
        command: "docker logs -f <容器>",
        note: "实时跟踪日志，Ctrl+C 退出",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "进入容器",
        command: "docker exec -it <容器> sh",
        note: "以 sh 进入容器，没有 sh 时换成 bash",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "镜像列表",
        command: "docker images",
        note: "本地镜像与大小",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "容器资源占用",
        command: "docker stats",
        note: "容器 CPU / 内存 / 网络的实时占用",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "查看容器详情",
        command: "docker inspect <容器>",
        note: "JSON 形式的完整配置",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "compose 启动",
        command: "docker compose up -d",
        note: "按当前目录的 compose 文件后台启动",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "compose 停止",
        command: "docker compose down",
        note: "停止并移除容器与网络",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "compose 状态",
        command: "docker compose ps",
        note: "当前项目的容器状态",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "复制文件出容器",
        command: "docker cp <容器>:<容器内路径> <本机路径>",
        note: "参数顺序：容器在前、本机在后",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "容器",
        name: "复制文件进容器",
        command: "docker cp <本机路径> <容器>:<容器内路径>",
        note: "反向复制，顺序与本机相反",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "维护",
        name: "清理悬空资源",
        command: "docker system prune -f",
        note: "删除已停止容器、悬空镜像与未使用网络（有数据卷的容器不受影响）",
    },
    PresetSnippet {
        platforms: &[Platform::Cross],
        group: "维护",
        name: "清理未使用镜像",
        command: "docker image prune -a",
        note: "删除没有容器引用的镜像",
    },
];

#[cfg(test)]
mod tests {
    use super::{PRESET_GROUPS, PRESETS, PresetSnippet, presets_for};
    use crate::platform::Platform;
    use crate::snippets;

    #[test]
    fn presets_are_valid_single_line_snippets() {
        assert!(!PRESETS.is_empty());
        for preset in PRESETS {
            assert!(
                !preset.platforms.is_empty(),
                "内置命令没有平台：{}",
                preset.name
            );
            assert!(
                PRESET_GROUPS.contains(&preset.group),
                "未知子分类 {}",
                preset.group
            );
            assert!(
                snippets::validate(preset.name, preset.command, preset.note).is_ok(),
                "内置命令不合法：{}",
                preset.name
            );
            assert!(
                !preset
                    .command
                    .chars()
                    .any(|c| matches!(c, '\r' | '\n' | '\0'))
            );
            assert!(!preset.note.chars().any(|c| matches!(c, '\r' | '\n')));
            if let Some(fallback) = preset.fallback() {
                assert!(
                    !fallback.trim().is_empty(),
                    "内置命令替代提示为空：{}",
                    preset.name
                );
                assert!(
                    !fallback.contains(['\r', '\n']),
                    "内置命令替代提示必须单行：{}",
                    preset.name
                );
            }
        }
    }

    #[test]
    fn preset_texts_and_fallbacks_have_english_entries() {
        let table: std::collections::BTreeMap<String, String> =
            serde_json::from_str(include_str!("../../assets/locales/en.json"))
                .expect("en.json 必须是扁平字符串表");
        for preset in PRESETS {
            for text in [preset.group, preset.name, preset.note] {
                assert!(table.contains_key(text), "内置命令缺少英文词条：{text}");
            }
            if let Some(fallback) = preset.fallback() {
                assert!(
                    table.contains_key(fallback),
                    "内置命令替代提示缺少英文词条：{fallback}"
                );
            }
        }
    }

    #[test]
    fn names_are_unique_within_platform_and_group() {
        // 同一概念在不同平台上允许同名（如「服务状态」），同一平台内不允许重复。
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(
                seen.insert((preset.platform(), preset.group, preset.name)),
                "内置命令重名：{:?} / {} / {}",
                preset.platform(),
                preset.group,
                preset.name
            );
        }
    }

    #[test]
    fn every_group_and_platform_has_commands() {
        for group in PRESET_GROUPS {
            assert!(
                PRESETS
                    .iter()
                    .any(|preset: &PresetSnippet| preset.group == *group),
                "子分类没有命令：{group}"
            );
        }
        // 每个可查看的平台页都要有内容：Unix 页会同时列出 Linux/Unix 共用命令。
        for platform in Platform::ALL {
            assert!(
                PRESETS.iter().any(|preset| preset.supports(platform)),
                "平台没有命令：{platform:?}"
            );
        }
    }

    /// 真实片段表上复核位置感知：`cd find` 是 cd 的参数，不该冒出 `sudo find …`。
    #[test]
    fn completion_stays_in_the_word_position() {
        let items: Vec<crate::completion::CompletionItem> = presets_for(Some(Platform::Linux))
            .map(|(_, preset)| {
                crate::completion::CompletionItem::new(
                    preset.name.to_string(),
                    preset.command.to_string(),
                    preset.note.to_string(),
                    crate::completion::CompletionKind::Command,
                )
            })
            .collect();
        let commands = |token: &str, before: &str| -> Vec<String> {
            crate::completion::complete(&items, token, before)
                .iter()
                .map(|item| item.command.clone())
                .collect()
        };
        // 参数位：不接前文的命令一条都不给，交给 shell 自己的补全。
        assert!(commands("find", "cd ").is_empty());
        assert!(commands("docker", "cd ").is_empty());
        // 同一个词落在命令位，照旧能找到 find / docker 的命令。
        assert!(
            commands("find", "")
                .iter()
                .any(|command| command.contains("find"))
        );
        // 参数位接得上前文时给续写与选项。
        assert!(
            commands("up", "sudo apt ")
                .iter()
                .all(|command| command.starts_with("sudo apt update"))
        );
        assert!(commands("-", "docker ").contains(&"-a".to_string()));
    }

    #[test]
    fn completion_never_crosses_platforms() {
        let linux: Vec<&str> = presets_for(Some(Platform::Linux))
            .map(|(_, preset)| preset.command)
            .collect();
        assert!(
            linux
                .iter()
                .any(|command| command.contains("ip -brief addr"))
        );
        assert!(!linux.iter().any(|command| command.contains("Get-")));
        assert!(!linux.iter().any(|command| command.contains("HKLM:")));
        // 跨平台命令（Docker）在 Linux 上仍然可用。
        assert!(linux.contains(&"docker ps"));

        let windows: Vec<&str> = presets_for(Some(Platform::Windows))
            .map(|(_, preset)| preset.command)
            .collect();
        assert!(
            windows
                .iter()
                .any(|command| command.contains("Get-NetIPConfiguration"))
        );
        assert!(!windows.iter().any(|command| command.contains("apt ")));
        assert!(
            !windows
                .iter()
                .any(|command| command.contains("ip -brief addr"))
        );

        // 平台未知：只剩通用命令。
        let unknown: Vec<&str> = presets_for(None)
            .map(|(_, preset)| preset.command)
            .collect();
        assert!(unknown.iter().all(|command| command.starts_with("docker ")));
        assert!(!unknown.is_empty());

        // Git Bash / macOS：拿到 POSIX 工具，拿不到 apt / systemctl。
        let unix: Vec<&str> = presets_for(Some(Platform::Unix))
            .map(|(_, preset)| preset.command)
            .collect();
        assert!(unix.contains(&"df -h"));
        assert!(!unix.iter().any(|command| command.contains("systemctl")));
    }
}
