//! 内置常用命令：按平台与子分类分组、只读展示；用户可以「存为我的片段」再改成自己的。
//!
//! 平台决定这条命令能不能在当前会话里用（补全据此过滤），子分类只影响展示顺序。
//! 名称 / 备注按中文键走 i18n，命令本身原样发送。

use crate::platform::Platform;

/// 一个内置命令条目。
pub struct PresetSnippet {
    /// 适用的平台，第一个同时作为列表里的归属平台。
    /// 可以有多项：`df`、`tar` 这类 POSIX 通用命令同时标 Linux 与 Unix。
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
    "磁盘与内存",
    "进程与服务",
    "网络",
    "文件与压缩",
    "容器",
    "维护",
];

impl PresetSnippet {
    /// 列表里归属的平台（多平台命令只出现一次，挂在第一个平台下）。
    pub fn platform(&self) -> Platform {
        self.platforms[0]
    }

    /// 这条命令能否在给定平台使用。
    pub fn supports(&self, platform: Platform) -> bool {
        self.platforms.contains(&platform)
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
    // ---- Windows（PowerShell） ----
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
        name: "已安装软件",
        command: "Get-ItemProperty 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' | Select-Object DisplayName, DisplayVersion | Sort-Object DisplayName",
        note: "读取卸载注册表项，只含系统级安装",
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
        group: "进程与服务",
        name: "服务状态",
        command: "Get-Service <服务名>",
        note: "查看 Windows 服务状态",
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
        name: "查看 IP 配置",
        command: "Get-NetIPConfiguration",
        note: "网卡、地址、网关与 DNS",
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
        platforms: &[Platform::Linux, Platform::Unix],
        group: "系统信息",
        name: "查看定时任务",
        command: "crontab -l",
        note: "当前用户的 crontab",
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
        platforms: &[Platform::Linux, Platform::Unix],
        group: "磁盘与内存",
        name: "查看磁盘余量",
        command: "df -h",
        note: "按挂载点列出磁盘使用率",
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
        group: "进程与服务",
        name: "服务状态",
        command: "systemctl status <服务名>",
        note: "查看服务是否在运行",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "进程与服务",
        name: "重启服务",
        command: "sudo systemctl restart <服务名>",
        note: "修改配置后重启服务",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "进程与服务",
        name: "跟踪服务日志",
        command: "journalctl -u <服务名> -f",
        note: "实时跟踪服务日志，Ctrl+C 退出",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "监听端口",
        command: "sudo ss -tlnp",
        note: "列出 TCP 监听端口和对应进程",
    },
    PresetSnippet {
        platforms: &[Platform::Linux, Platform::Unix],
        group: "网络",
        name: "测试网络连通",
        command: "ping -c 4 1.1.1.1",
        note: "发 4 个包测试外网连通性",
    },
    PresetSnippet {
        platforms: &[Platform::Linux],
        group: "网络",
        name: "查看本机地址",
        command: "ip -brief addr",
        note: "各网卡的 IPv4 / IPv6 地址",
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
        group: "维护",
        name: "清理悬空资源",
        command: "docker system prune -f",
        note: "删除已停止容器、悬空镜像与未使用网络（有数据卷的容器不受影响）",
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
        // 每个会话平台都要有专属命令；Unix 例外，它的命令与 Linux 共用（不会单独成组）。
        for platform in [Platform::Windows, Platform::Linux, Platform::Cross] {
            assert!(
                PRESETS
                    .iter()
                    .any(|preset: &PresetSnippet| preset.platform() == platform),
                "平台没有命令：{platform:?}"
            );
        }
        assert!(
            PRESETS
                .iter()
                .any(|preset: &PresetSnippet| preset.supports(Platform::Unix)),
            "Unix 没有任何可用命令"
        );
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
