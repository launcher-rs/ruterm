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

    /// 状态栏摘要：发行版优先于内核标识，其余按固定顺序以 ` · ` 连接。
    ///
    /// 全部缺项（远端禁用 exec 或非 Linux 无相应文件）时返回 `None`，
    /// 状态栏据此不显示远程条目。
    pub fn summary(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(name) = self.get("os").or(self.get("sys")) {
            parts.push(name.to_string());
        }
        for (key, label) in [
            ("mem", "内存"),
            ("disk", "磁盘"),
            ("load", "负载"),
            ("up", "运行"),
        ] {
            if let Some(value) = self.get(key) {
                parts.push(format!("{label} {value}"));
            }
        }
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_linux_probe() {
        let raw = "host=rulink-demo\nsys=Linux 6.8.0-45-generic\nos=Ubuntu 24.04.1 LTS\n\
                   mem=1.2/7.7G\ndisk=23G/98G(24%)\nload=0.13\nup=3d4h\n";
        let info = RemoteInfo::parse(raw);
        assert_eq!(info.get("os"), Some("Ubuntu 24.04.1 LTS"));
        assert_eq!(
            info.summary().as_deref(),
            Some("Ubuntu 24.04.1 LTS · 内存 1.2/7.7G · 磁盘 23G/98G(24%) · 负载 0.13 · 运行 3d4h")
        );
    }

    #[test]
    fn ignores_noise_and_missing_items() {
        // stderr 混入的报错行、空值、未知键都不应进入摘要。
        let raw = "df: /: not mounted\ncat: no such file\nload=\nfoo=bar\nup=0d1h\n";
        let info = RemoteInfo::parse(raw);
        assert_eq!(info.summary().as_deref(), Some("运行 0d1h"));
    }

    #[test]
    fn falls_back_to_kernel_when_no_os_release() {
        let info = RemoteInfo::parse("sys=FreeBSD 14.1-RELEASE\n");
        assert_eq!(info.summary().as_deref(), Some("FreeBSD 14.1-RELEASE"));
    }

    #[test]
    fn empty_probe_yields_no_summary() {
        assert_eq!(RemoteInfo::parse("").summary(), None);
    }
}
