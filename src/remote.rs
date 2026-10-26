//! 远程主机基本信息探测：SSH 建立后经 exec 通道一次性采集，供底部状态栏展示。
//!
//! 探测脚本按 POSIX sh 语义编写，逐项输出 `key=value` 行；命令不可用或文件
//! 不存在时该项静默缺失，因此解析侧必须容忍乱序、缺项与杂行（stderr 噪声）。
//! 解析与展示不碰 IO，便于单测。

use std::collections::BTreeMap;

/// 探测命令：只读、无交互、单通道跑完即退出。
///
/// 键含义：`host` 主机名 / `os` 发行版名 / `sys` 内核名与版本 /
/// `mem` 已用与总内存 / `disk` 根分区已用与总量 / `load` 一分钟负载 / `up` 运行时长。
pub const PROBE_CMD: &str = r#"printf 'host=%s\n' "$(uname -n 2>/dev/null)"; printf 'sys=%s\n' "$(uname -s 2>/dev/null) $(uname -r 2>/dev/null)"; sed -n 's/^PRETTY_NAME=//p' /etc/os-release 2>/dev/null | head -n 1 | sed 's/^"\(.*\)"$/\1/' | { read -r v; [ -n "$v" ] && printf 'os=%s\n' "$v"; }; awk '/MemTotal/{t=$2} /MemAvailable/{a=$2} END{if(t!=""){printf "mem=%.1f/%.1fG\n",(t-a)/1048576,t/1048576}}' /proc/meminfo 2>/dev/null; df -hP / 2>/dev/null | awk 'NR==2{printf "disk=%s/%s(%s)\n",$3,$2,$5}'; printf 'load=%s\n' "$(cut -d' ' -f1 /proc/loadavg 2>/dev/null)"; if [ -r /proc/uptime ]; then awk '{s=int($1); printf "up=%dd%dh\n",int(s/86400),int(s%86400/3600)}' /proc/uptime; else uptime -p 2>/dev/null | { read -r v; [ -n "$v" ] && printf 'up=%s\n' "$v"; }; fi"#;

/// 一次探测得到的远程主机信息（缺项为 `None`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoteInfo {
    /// 原始 key=value 字段表，保留远端给出的原样文本。
    fields: BTreeMap<String, String>,
}

impl RemoteInfo {
    /// 解析探测输出：只认 `key=value` 行，其余（含 shell 报错）一律忽略。
    pub fn parse(raw: &str) -> Self {
        let mut fields = BTreeMap::new();
        for line in raw.lines() {
            let Some((key, value)) = line.trim().split_once('=') else {
                continue;
            };
            let value = value.trim();
            if !key.is_empty() && !value.is_empty() {
                fields.insert(key.to_string(), value.to_string());
            }
        }
        Self { fields }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(String::as_str)
    }

    /// 该次探测是否什么都没探到（远端禁用 exec 或全部缺项）。
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// 单项指标文案（含标签）；缺项返回 `None`，状态栏据此跳过该条目。
    pub fn item(&self, metric: Metric) -> Option<String> {
        match metric {
            // 发行版优先，缺 os-release 时回退内核标识，且不加标签。
            Metric::Os => self
                .get("os")
                .or_else(|| self.get("sys"))
                .map(str::to_string),
            other => self
                .get(other.key())
                .map(|value| format!("{} {value}", other.label())),
        }
    }
}

/// 状态栏可单独开关的远程指标。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Metric {
    Os,
    Mem,
    Disk,
    Load,
    Up,
}

impl Metric {
    /// 展示顺序。
    pub const ALL: [Metric; 5] = [Self::Os, Self::Mem, Self::Disk, Self::Load, Self::Up];

    /// 探测输出里的键名，同时用作设置持久化的稳定 id。
    pub fn id(self) -> &'static str {
        match self {
            Self::Os => "os",
            Self::Mem => "mem",
            Self::Disk => "disk",
            Self::Load => "load",
            Self::Up => "up",
        }
    }

    fn key(self) -> &'static str {
        self.id()
    }

    /// 条目标签（设置界面与状态栏共用）。
    pub fn label(self) -> &'static str {
        match self {
            Self::Os => "系统",
            Self::Mem => "内存",
            Self::Disk => "磁盘",
            Self::Load => "负载",
            Self::Up => "运行",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全部指标按展示顺序拼成一行（测试与"整行摘要"式断言用）。
    fn line(info: &RemoteInfo) -> Option<String> {
        let parts: Vec<String> = Metric::ALL.iter().filter_map(|m| info.item(*m)).collect();
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }

    #[test]
    fn parses_typical_linux_probe() {
        let raw = "host=rulink-demo\nsys=Linux 6.8.0-45-generic\nos=Ubuntu 24.04.1 LTS\n\
                   mem=1.2/7.7G\ndisk=23G/98G(24%)\nload=0.13\nup=3d4h\n";
        let info = RemoteInfo::parse(raw);
        assert_eq!(info.item(Metric::Os).as_deref(), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(
            line(&info).as_deref(),
            Some("Ubuntu 24.04.1 LTS · 内存 1.2/7.7G · 磁盘 23G/98G(24%) · 负载 0.13 · 运行 3d4h")
        );
    }

    #[test]
    fn ignores_noise_and_missing_items() {
        // stderr 混入的报错行、空值、未知键都不应成为条目。
        let raw = "df: /: not mounted\ncat: no such file\nload=\nfoo=bar\nup=0d1h\n";
        let info = RemoteInfo::parse(raw);
        assert_eq!(info.item(Metric::Load), None);
        assert_eq!(line(&info).as_deref(), Some("运行 0d1h"));
    }

    #[test]
    fn falls_back_to_kernel_when_no_os_release() {
        let info = RemoteInfo::parse("sys=FreeBSD 14.1-RELEASE\n");
        assert_eq!(
            info.item(Metric::Os).as_deref(),
            Some("FreeBSD 14.1-RELEASE")
        );
    }

    #[test]
    fn empty_probe_yields_no_items() {
        let info = RemoteInfo::parse("");
        assert!(info.is_empty());
        assert_eq!(line(&info), None);
    }
}
