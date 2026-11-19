//! 单实例守卫：让一个用户会话里只跑一个 RuTerm 进程。
//!
//! 关窗口默认是隐藏到托盘（会话要在后台保活），于是很容易再双击一次 exe，
//! 结果两个进程读写同一个加密库、各自挂一个托盘图标，隐藏的那个再也找不回来。
//! 这里用「绑定 127.0.0.1 的一个回环端口」当进程间锁：抢到的人做主实例并监听
//! 敲门；抢不到的人递一个令牌、请主实例把窗口带到前台，然后自己退出。
//! 令牌要双向核对，端口被无关程序占着时宁可放弃守卫，也不让软件启动不了。
//!
//! 端口与令牌都按当前用户名派生：回环端口是机器全局资源，不派生的话
//! Windows 快速用户切换 / RDP 多会话场景下，另一名用户的启动会撞到
//! 我们的端口并被"唤醒"到别人的窗口，永远起不了自己的实例。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

/// 回环锁端口基数：挑在常见服务区间之外，降低撞车概率。
const DEFAULT_PORT: u16 = 47_122;
/// 按用户名哈希散开的端口数：不同用户大概率各用各的锁端口。
const PORT_SPREAD: u16 = 512;

/// 当前用户名（Windows 用 USERNAME，Unix 用 USER）。
fn session_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

/// 锁端口，可被 `RUTERM_INSTANCE_PORT` 覆盖；非法值回退默认并告警。
fn port() -> u16 {
    match std::env::var_os("RUTERM_INSTANCE_PORT") {
        Some(raw) => match raw.to_str().and_then(|s| s.parse::<u16>().ok()) {
            Some(port) => port,
            None => {
                log::warn!("RUTERM_INSTANCE_PORT 非法（{raw:?}），回退按用户派生的默认端口");
                default_port()
            }
        },
        None => default_port(),
    }
}

/// 无覆盖时的端口：基数 + 用户名哈希偏移。
fn default_port() -> u16 {
    let mut hasher = DefaultHasher::new();
    session_user().hash(&mut hasher);
    let offset = (hasher.finish() % u64::from(PORT_SPREAD)) as u16;
    DEFAULT_PORT + offset
}

/// 握手令牌：两边都核对，确认对面确实是同一用户会话里的另一个 RuTerm。
/// 用户名拼进令牌，跨用户的敲门（端口恰好撞上时）核对不上会干净失败。
fn token() -> Vec<u8> {
    let mut bytes = b"ruterm-instance-v1".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(session_user().as_bytes());
    bytes
}

/// 单次握手的等待上限：主实例卡死时不该把第二次启动也拖住。
const HANDSHAKE: Duration = Duration::from_millis(600);

pub enum Claim {
    /// 本进程是主实例：把这个监听交给应用循环，收到敲门就唤醒窗口。
    Primary(TcpListener),
    /// 已有实例在跑，且已请它把窗口带到前台 —— 本进程该退出了。
    Activated,
    /// 端口被无关程序占着：加不了锁，照常启动，只是不保证单实例。
    Unprotected,
}

/// 抢锁：成功返回主实例监听，否则说明为什么不做守卫。
pub fn claim() -> Claim {
    match TcpListener::bind(("127.0.0.1", port())) {
        Ok(listener) => {
            // 交给 tokio 之前必须切成非阻塞（`serve` 里会 from_std 接管）。
            match listener.set_nonblocking(true) {
                Ok(()) => Claim::Primary(listener),
                Err(error) => {
                    log::warn!("单实例监听无法转为非阻塞: {error}");
                    Claim::Unprotected
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            if knock() {
                Claim::Activated
            } else {
                log::warn!("回环端口被非 RuTerm 程序占用，本次不做单实例守卫");
                Claim::Unprotected
            }
        }
        Err(error) => {
            log::warn!("绑定单实例端口失败: {error}");
            Claim::Unprotected
        }
    }
}

/// 敲门：递令牌并等主实例回同一个令牌，回得上才认定对面是 RuTerm。
/// 连接、写入、读取都受总时上限约束；对面只发半截或一直不回就放弃。
fn knock() -> bool {
    let addr = SocketAddr::from(([127, 0, 0, 1], port()));
    let Ok(mut stream) = TcpStream::connect_timeout(&addr, HANDSHAKE) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(HANDSHAKE));
    let _ = stream.set_read_timeout(Some(HANDSHAKE));
    let token = token();
    if stream.write_all(&token).is_err() {
        return false;
    }
    let _ = stream.flush();
    let mut reply = vec![0u8; token.len()];
    if stream.read_exact(&mut reply).is_err() {
        return false;
    }
    reply == token
}

/// 主实例侧：把 std 监听换成 tokio 监听，交给应用执行器跑唤醒循环。
/// 每收到一次合法敲门就调用一次 `wake`（把主窗口带回前台）。
pub async fn serve<F: FnMut()>(listener: TcpListener, mut wake: F) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        log::warn!("单实例监听无法接入异步运行时，后续启动不会再唤醒本窗口");
        return;
    };
    loop {
        // accept 持续报错时不能变成忙等（错误立即返回会让这个循环吃满一个核）。
        let (mut stream, _) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(error) => {
                log::warn!("单实例监听 accept 失败: {error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        // 每个连接都必须在一个总时限内完成固定长度握手：读半截、慢速灌数据、
        // 连上不说话都不能把唤醒循环卡住。
        let token = token();
        let authenticated = tokio::time::timeout(HANDSHAKE, async {
            use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
            let mut received = vec![0u8; token.len()];
            stream.read_exact(&mut received).await.ok()?;
            if received != token {
                return None;
            }
            stream.write_all(&token).await.ok()?;
            stream.flush().await.ok()?;
            Some(())
        })
        .await
        .is_ok_and(|result| result.is_some());
        if authenticated {
            wake();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_PORT, PORT_SPREAD, default_port, token};

    #[test]
    fn port_offsets_by_user_and_stays_in_range() {
        let port = default_port();
        assert!((DEFAULT_PORT..DEFAULT_PORT + PORT_SPREAD).contains(&port));
    }

    #[test]
    fn token_is_user_scoped() {
        let token = token();
        assert!(token.starts_with(b"ruterm-instance-v1\x00"));
        // 含用户名主体（用户名可能为空，但分隔符必须在）。
        assert!(token.len() >= 19);
    }
}
