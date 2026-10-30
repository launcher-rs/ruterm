//! 单实例守卫：让一台机器上只跑一个 RuTerm 进程。
//!
//! 关窗口默认是隐藏到托盘（会话要在后台保活），于是很容易再双击一次 exe，
//! 结果两个进程读写同一个加密库、各自挂一个托盘图标，隐藏的那个再也找不回来。
//! 这里用「绑定 127.0.0.1 的一个回环端口」当进程间锁：抢到的人做主实例并监听
//! 敲门；抢不到的人递一个令牌、请主实例把窗口带到前台，然后自己退出。
//! 令牌要双向核对，端口被无关程序占着时宁可放弃守卫，也不让软件启动不了。

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

/// 回环锁端口：挑在常见服务区间之外，降低撞车概率。
const PORT: u16 = 47_122;
/// 握手口令：两边都核对，确认对面确实是另一个 RuTerm。
const TOKEN: &str = "ruterm-instance-v1";
/// 敲门与应答的等待上限：主实例卡死时不该把第二次启动也拖住。
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
    match TcpListener::bind(("127.0.0.1", PORT)) {
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
                log::warn!("回环端口 {PORT} 被非 RuTerm 程序占用，本次不做单实例守卫");
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
fn knock() -> bool {
    let Ok(stream) = TcpStream::connect(("127.0.0.1", PORT)) else {
        return false;
    };
    let _ = stream.set_write_timeout(Some(HANDSHAKE));
    let _ = stream.set_read_timeout(Some(HANDSHAKE));
    let Ok(mut writer) = stream.try_clone() else {
        return false;
    };
    if writeln!(writer, "{TOKEN}").is_err() {
        return false;
    }
    let mut line = String::new();
    let Ok(_) = BufReader::new(stream).read_line(&mut line) else {
        return false;
    };
    line.trim() == TOKEN
}

/// 主实例侧：把 std 监听换成 tokio 监听，交给应用执行器跑唤醒循环。
/// 每收到一次合法敲门就调用一次 `wake`（把主窗口带回前台）。
pub async fn serve<F: FnMut()>(listener: TcpListener, mut wake: F) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        log::warn!("单实例监听无法接入异步运行时，后续启动不会再唤醒本窗口");
        return;
    };
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let mut line = String::new();
        let mut reader = tokio::io::BufReader::new(&mut stream);
        if reader.read_line(&mut line).await.unwrap_or(0) == 0 || line.trim() != TOKEN {
            continue;
        }
        let _ = stream.write_all(format!("{TOKEN}\n").as_bytes()).await;
        let _ = stream.flush().await;
        wake();
    }
}
