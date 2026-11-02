//! 局域网主机发现：扫本机各网卡的 /24，探测常见远程端口。
//!
//! 不引入额外依赖：网卡地址从 `arp -a` 的 Interface 行解析（Windows 每块网卡
//! 一段），再按默认路由兜底；端口用短超时 TCP connect 并发探测。
//! 发现结果只用于界面提示，不自动写库——是否加入主机列表由用户点选。

use std::collections::BTreeSet;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, bail};
use tokio::net::{TcpStream, UdpSocket};
use tokio::sync::Semaphore;

/// 单个端口的探测超时：局域网内开放端口毫秒级返回，过滤掉的端口才吃满超时。
const PROBE_TIMEOUT: Duration = Duration::from_millis(350);
/// 同时探测的主机数。
const HOST_CONCURRENCY: usize = 64;
/// 最多扫描几个网段（VMware 主机常有多块虚拟网卡，但再多就明显拖慢）。
const MAX_SUBNETS: usize = 4;

/// 一台被发现的主机及其开放的远程服务。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiscoveredHost {
    pub ip: Ipv4Addr,
    pub ssh: bool,
    pub telnet: bool,
    pub rdp: bool,
    pub vnc: bool,
}

impl DiscoveredHost {
    /// 服务列表（界面副标题）。
    pub fn services(&self) -> String {
        let mut names = Vec::new();
        if self.rdp {
            names.push("RDP");
        }
        if self.vnc {
            names.push("VNC");
        }
        if self.ssh {
            names.push("SSH");
        }
        if self.telnet {
            names.push("Telnet");
        }
        names.join(" · ")
    }
}

/// 扫描本机所在网段：返回按 IP 排序、至少开了一个常见端口的主机。
///
/// 没有可用私有网段（例如只有公网地址）时返回空列表而非报错。
pub async fn scan_lan() -> Result<Vec<DiscoveredHost>> {
    let mut prefixes = interface_prefixes().await;
    if prefixes.is_empty() {
        bail!("没有可扫描的私有网段");
    }
    prefixes.truncate(MAX_SUBNETS);

    let mut targets = BTreeSet::new();
    for (a, b, c) in &prefixes {
        for last in 1..=254u8 {
            targets.insert(Ipv4Addr::new(*a, *b, *c, last));
        }
    }
    // 邻居表里已经出现过的地址也补进来：它们可能在其它网段（如 VMware NAT），
    // 而且多半正开着。
    let neighbors = tokio::task::spawn_blocking(neighbor_addresses)
        .await
        .unwrap_or_default();
    targets.extend(neighbors);

    let permits = Arc::new(Semaphore::new(HOST_CONCURRENCY));
    let mut tasks = Vec::with_capacity(targets.len());
    for ip in targets {
        let permits = permits.clone();
        tasks.push(tokio::spawn(async move {
            let _permit = permits.acquire().await.ok()?;
            probe(ip).await
        }));
    }

    let mut hosts = Vec::new();
    for task in tasks {
        if let Ok(Some(host)) = task.await {
            hosts.push(host);
        }
    }
    hosts.sort_by_key(|host| host.ip);
    Ok(hosts)
}

/// 探测四个常见远程端口；都没有响应时视为普通主机，不返回。
async fn probe(ip: Ipv4Addr) -> Option<DiscoveredHost> {
    let (ssh, telnet, rdp, vnc) = tokio::join!(
        port_open(ip, 22),
        port_open(ip, 23),
        port_open(ip, 3389),
        port_open(ip, 5900),
    );
    if !(ssh || telnet || rdp || vnc) {
        return None;
    }
    Some(DiscoveredHost {
        ip,
        ssh,
        telnet,
        rdp,
        vnc,
    })
}

async fn port_open(ip: Ipv4Addr, port: u16) -> bool {
    matches!(
        tokio::time::timeout(
            PROBE_TIMEOUT,
            TcpStream::connect(SocketAddr::from((ip, port)))
        )
        .await,
        Ok(Ok(_))
    )
}

/// 本机各私有网段的前三段（/24）。默认路由所在网段排最前，
/// 再用 `arp -a` 的 Interface 行补齐多网卡（虚拟网卡多时也不会把真正的局域网挤掉）。
async fn interface_prefixes() -> Vec<(u8, u8, u8)> {
    let mut found: Vec<(u8, u8, u8)> = Vec::new();
    let mut push = |ip: Ipv4Addr| {
        if is_private(ip) {
            let octets = ip.octets();
            let prefix = (octets[0], octets[1], octets[2]);
            if !found.contains(&prefix) {
                found.push(prefix);
            }
        }
    };
    if let Some(ip) = default_local_ip().await {
        push(ip);
    }
    let arp = tokio::task::spawn_blocking(|| {
        let output = std::process::Command::new("arp").arg("-a").output().ok()?;
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .ok()
    .flatten();
    if let Some(text) = arp {
        for ip in interface_addresses(&text) {
            push(ip);
        }
    }
    found
}

/// 解析 `arp -a` 的接口行：形如 `Interface: 192.168.31.5 --- 0x5`
/// （中文系统是 `接口: 192.168.31.5 --- 0x5`，判据用 `---`）。
fn interface_addresses(text: &str) -> Vec<Ipv4Addr> {
    let mut addresses = Vec::new();
    for line in text.lines() {
        if !line.contains("---") {
            continue;
        }
        if let Some(ip) = line
            .split_whitespace()
            .find_map(|token| token.parse::<Ipv4Addr>().ok())
        {
            addresses.push(ip);
        }
    }
    addresses
}

/// 解析邻居表里出现的 IPv4（各平台格式都按空白分隔，取第一个能解析的字段）。
fn neighbor_addresses() -> Vec<Ipv4Addr> {
    let text = std::process::Command::new("arp")
        .arg("-a")
        .output()
        .ok()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let mut addresses = Vec::new();
    for line in text.lines() {
        if line.contains("---") {
            continue;
        }
        if let Some(ip) = line
            .split_whitespace()
            .find_map(|token| token.parse::<Ipv4Addr>().ok())
            && is_private(ip)
        {
            addresses.push(ip);
        }
    }
    addresses
}

/// 默认路由出口的本机地址：UDP connect 不发包，只让系统选源地址。
async fn default_local_ip() -> Option<Ipv4Addr> {
    for target in ["8.8.8.8:80", "1.1.1.1:80"] {
        let Ok(socket) = UdpSocket::bind("0.0.0.0:0").await else {
            continue;
        };
        if socket.connect(target).await.is_err() {
            continue;
        }
        if let Ok(local) = socket.local_addr()
            && let IpAddr::V4(ip) = local.ip()
        {
            return Some(ip);
        }
    }
    None
}

fn is_private(ip: Ipv4Addr) -> bool {
    ip.is_private()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_interface_lines() {
        let text = "\
Interface: 192.168.31.5 --- 0x8
  Internet Address      Physical Address      Type
  192.168.31.1          00-11-22-33-44-55     dynamic
Interface: 192.168.56.1 --- 0x11
  192.168.56.255        ff-ff-ff-ff-ff-ff     static";
        assert_eq!(
            interface_addresses(text),
            vec![
                Ipv4Addr::new(192, 168, 31, 5),
                Ipv4Addr::new(192, 168, 56, 1)
            ]
        );
    }

    #[test]
    fn parses_localized_interface_lines() {
        let text = "接口: 192.168.31.3 --- 0x3";
        assert_eq!(
            interface_addresses(text),
            vec![Ipv4Addr::new(192, 168, 31, 3)]
        );
    }

    #[test]
    fn services_are_listed_in_priority_order() {
        let host = DiscoveredHost {
            ip: Ipv4Addr::new(192, 168, 31, 6),
            ssh: true,
            telnet: false,
            rdp: true,
            vnc: true,
        };
        assert_eq!(host.services(), "RDP · VNC · SSH");
    }

    #[test]
    fn private_ranges_only() {
        assert!(is_private(Ipv4Addr::new(192, 168, 1, 2)));
        assert!(is_private(Ipv4Addr::new(10, 0, 0, 2)));
        assert!(!is_private(Ipv4Addr::new(8, 8, 8, 8)));
    }
}
