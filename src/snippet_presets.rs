//! 内置常用命令：按平台分组、只读展示；用户可以「存为我的片段」再改成自己的。

/// 一个内置命令条目。名称 / 说明按中文键走 i18n，命令本身原样发送。
pub struct PresetSnippet {
    pub category: &'static str,
    pub name: &'static str,
    pub command: &'static str,
    pub note: &'static str,
}

/// 分类顺序即展示顺序。
pub const PRESET_CATEGORIES: &[&str] = &["Linux（Debian / Ubuntu）", "Windows（PowerShell）", "Docker"];

pub const PRESETS: &[PresetSnippet] = &[
    // ---- Linux（Debian / Ubuntu） ----
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "系统更新",
        command: "sudo apt update && sudo apt upgrade -y",
        note: "更新软件包索引并升级已安装的软件",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "安装软件包",
        command: "sudo apt install <包名>",
        note: "把 <包名> 换成要安装的软件，回车前可再确认",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "查看磁盘余量",
        command: "df -h",
        note: "按挂载点列出磁盘使用率",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "目录占用排行",
        command: "du -sh * 2>/dev/null | sort -hr | head -20",
        note: "当前目录下占用最大的前 20 项",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "内存与进程",
        command: "free -h && ps aux --sort=-%mem | head -10",
        note: "内存概览与占用最高的进程",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "监听端口",
        command: "sudo ss -tlnp",
        note: "列出 TCP 监听端口和对应进程",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "服务状态",
        command: "systemctl status <服务名>",
        note: "查看服务是否在运行",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "重启服务",
        command: "sudo systemctl restart <服务名>",
        note: "修改配置后重启服务",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "跟踪服务日志",
        command: "journalctl -u <服务名> -f",
        note: "实时跟踪服务日志，Ctrl+C 退出",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "测试网络连通",
        command: "ping -c 4 1.1.1.1",
        note: "发 4 个包测试外网连通性",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "查看本机地址",
        command: "ip -brief addr",
        note: "各网卡的 IPv4 / IPv6 地址",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "查找文件",
        command: "sudo find / -name '<文件名>' 2>/dev/null",
        note: "从根目录按名称查找文件",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "解压 tar.gz",
        command: "tar -xzvf <归档.tar.gz>",
        note: "解压到当前目录",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "打包目录",
        command: "tar -czvf <归档.tar.gz> <目录>",
        note: "把目录压缩成 tar.gz",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "查看系统版本",
        command: "cat /etc/os-release",
        note: "发行版与版本号",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "最近登录",
        command: "last -n 10",
        note: "最近 10 次登录记录",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[0],
        name: "查看定时任务",
        command: "crontab -l",
        note: "当前用户的 crontab",
    },
    // ---- Windows（PowerShell） ----
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "系统版本",
        command: "Get-CimInstance Win32_OperatingSystem | Select-Object Caption, Version, LastBootUpTime",
        note: "系统名称、版本与上次启动时间",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "磁盘余量",
        command: "Get-Volume | Select-Object DriveLetter, FileSystemLabel, @{n='FreeGB';e={[math]::Round($_.SizeRemaining/1GB,1)}}",
        note: "各卷的剩余空间（GB）",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "内存占用前 10",
        command: "Get-Process | Sort-Object WorkingSet64 -Descending | Select-Object -First 10 Name, @{n='MemoryMB';e={[math]::Round($_.WorkingSet64/1MB,1)}}",
        note: "按工作集列出内存占用最高的进程",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "监听端口",
        command: "Get-NetTCPConnection -State Listen | Sort-Object LocalPort | Select-Object LocalAddress, LocalPort, OwningProcess",
        note: "TCP 监听端口与对应进程 ID",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "测试端口",
        command: "Test-NetConnection <主机> -Port <端口>",
        note: "如 Test-NetConnection example.com -Port 443",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "服务状态",
        command: "Get-Service <服务名>",
        note: "查看 Windows 服务状态",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "查看 IP 配置",
        command: "Get-NetIPConfiguration",
        note: "网卡、地址、网关与 DNS",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "查看路由",
        command: "Get-NetRoute -DestinationPrefix '0.0.0.0/0'",
        note: "默认路由",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "查找文件",
        command: "Get-ChildItem -Path C:\\ -Recurse -Filter '<文件名>' -ErrorAction SilentlyContinue",
        note: "从 C 盘按名称递归查找，权限错误自动忽略",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "下载文件",
        command: "Invoke-WebRequest -Uri <URL> -OutFile <文件名>",
        note: "把 URL 下载到当前目录",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "查看环境变量",
        command: "Get-ChildItem Env: | Sort-Object Name",
        note: "按名称排序的全部环境变量",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[1],
        name: "已安装软件",
        command: "Get-ItemProperty 'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*' | Select-Object DisplayName, DisplayVersion | Sort-Object DisplayName",
        note: "读取卸载注册表项，只含系统级安装",
    },
    // ---- Docker ----
    PresetSnippet {
        category: PRESET_CATEGORIES[2],
        name: "运行中的容器",
        command: "docker ps",
        note: "只看当前在运行的容器",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[2],
        name: "全部容器",
        command: "docker ps -a",
        note: "包含已停止的容器",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[2],
        name: "跟踪容器日志",
        command: "docker logs -f <容器>",
        note: "实时跟踪日志，Ctrl+C 退出",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[2],
        name: "进入容器",
        command: "docker exec -it <容器> sh",
        note: "以 sh 进入容器，没有 sh 时换成 bash",
    },
    PresetSnippet {
        category: PRESET_CATEGORIES[2],
        name: "清理悬空资源",
        command: "docker system prune -f",
        note: "删除已停止容器、悬空镜像与未使用网络（有数据卷的容器不受影响）",
    },
];

#[cfg(test)]
mod tests {
    use super::{PRESETS, PRESET_CATEGORIES, PresetSnippet};
    use crate::snippets;

    #[test]
    fn presets_are_valid_single_line_snippets() {
        assert!(!PRESETS.is_empty());
        for preset in PRESETS {
            assert!(
                PRESET_CATEGORIES.contains(&preset.category),
                "未知分类 {}",
                preset.category
            );
            assert!(
                snippets::validate(preset.name, preset.command, preset.note).is_ok(),
                "内置命令不合法：{}",
                preset.name
            );
            assert!(!preset.command.chars().any(|c| matches!(c, '\r' | '\n' | '\0')));
            assert!(!preset.note.chars().any(|c| matches!(c, '\r' | '\n')));
        }
    }

    #[test]
    fn preset_names_are_unique_per_category() {
        // 同一概念在不同平台上允许同名（如「服务状态」），同一分类内不允许重复。
        let mut seen = std::collections::HashSet::new();
        for preset in PRESETS {
            assert!(
                seen.insert((preset.category, preset.name)),
                "内置命令重名：{} / {}",
                preset.category,
                preset.name
            );
        }
    }

    #[test]
    fn every_category_has_commands() {
        for category in PRESET_CATEGORIES {
            assert!(
                PRESETS
                    .iter()
                    .any(|preset: &PresetSnippet| preset.category == *category),
                "分类没有命令：{category}"
            );
        }
    }
}
