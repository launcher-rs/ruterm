//! 单实例守卫：让同一个 *数据目录* 只有一个 RuTerm 进程。
//!
//! 关窗口默认是隐藏到托盘（会话要在后台保活），于是很容易再双击一次 exe，
//! 结果两个进程读写同一个加密库、各自挂一个托盘图标，隐藏的那个再也找不回来。
//! 这里用「绑定 127.0.0.1 的一个回环端口」当进程间锁：抢到的人做主实例并监听
//! 敲门；抢不到的人递一个令牌、请主实例把窗口带到前台，然后自己退出。
//!
//! 锁按「当前用户名 + 数据目录」划界，而不是按整个用户会话：
//! - 便携模式：不同文件夹里的两份拷贝各用各的数据，同时运行是有意为之，
//!   锁端口与令牌都不同，互不敲门、各起各的窗口；
//! - 常规安装（数据目录固定）或双击同一个便携文件夹（含藏进托盘后再双击）：
//!   数据目录相同，就互相唤醒，保证不会有两个进程同时读写同一个加密库。
//!
//! 端口按 guard key 哈希散开，不同数据目录有小概率散进同一槽：撞上时对面
//! 令牌核对不上（无关程序 / 另一份数据的 RuTerm），就向后试探下一个候选端口；
//! 候选全部被占才降级为「不加锁照常启动」。令牌带 guard key 双向核对，
//! 跨目录的敲门永远对不上，不会唤醒别人（别的目录）的窗口。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

/// 回环锁端口基数：挑在常见服务区间之外，降低撞车概率。
const DEFAULT_PORT: u16 = 47_122;
/// 按 guard key 哈希散开的端口数：不同数据目录大概率各用各的锁端口。
const PORT_SPREAD: u16 = 512;
/// 端口被占后向后试探的候选数：无关程序连占一段端口时不至于无限找。
const MAX_CANDIDATES: u16 = 16;

/// 当前用户名（Windows 用 USERNAME，Unix 用 USER）。
fn session_user() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default()
}

/// 守卫身份：用户名 + 数据目录。数据目录（安装模式的应用数据目录、
/// 便携模式的程序所在目录）才是「读写同一份数据」的真正边界。
fn guard_key(data_dir: &Path) -> String {
    format!("{}\0{}", session_user(), data_dir.display())
}

/// guard key 在散开区间内的稳定偏移。
fn hash_offset(guard_key: &str, spread: u64) -> u16 {
    let mut hasher = DefaultHasher::new();
    guard_key.hash(&mut hasher);
    (hasher.finish() % spread) as u16
}

/// 首选锁端口，可被 `RUTERM_INSTANCE_PORT` 覆盖；非法值（含 0——绑定随机端口
/// 会让每次启动拿不同的锁端口，守卫静默失效）回退按 guard key 派生的默认。
fn base_port(guard_key: &str) -> u16 {
    match std::env::var_os("RUTERM_INSTANCE_PORT") {
        Some(raw) => match raw.to_str().and_then(|s| s.parse::<u16>().ok()) {
            Some(port) if port != 0 => port,
            _ => {
                log::warn!("RUTERM_INSTANCE_PORT 非法（{raw:?}），回退按 guard key 派生的默认端口");
                DEFAULT_PORT + hash_offset(guard_key, u64::from(PORT_SPREAD))
            }
        },
        None => DEFAULT_PORT + hash_offset(guard_key, u64::from(PORT_SPREAD)),
    }
}

/// 第 attempt 个候选端口：在 1..=65535 内回绕（避开 0），首选之外向后挪。
fn candidate_port(base: u16, attempt: u16) -> u16 {
    (((u32::from(base) - 1) + u32::from(attempt)) % 65_535 + 1) as u16
}

/// 握手令牌：两边都核对，确认对面确实是同一用户会话里、用同一个数据目录
/// 的另一个 RuTerm。guard key 拼进令牌：跨目录的敲门（端口恰好撞上时）
/// 核对不上，本进程会换下一个候选端口而不是被「唤醒」。
fn token(guard_key: &str) -> Vec<u8> {
    let mut bytes = b"ruterm-instance-v2".to_vec();
    bytes.push(0);
    bytes.extend_from_slice(guard_key.as_bytes());
    bytes
}

/// 单次握手的等待上限：主实例卡死时不该把第二次启动也拖住。
const HANDSHAKE: Duration = Duration::from_millis(600);

#[derive(Debug)]
pub enum Claim {
    /// 本进程是主实例：把这个监听交给应用循环，收到敲门就唤醒窗口。
    Primary(TcpListener),
    /// 同数据目录已有实例在跑，且已请它把窗口带到前台 —— 本进程该退出了。
    Activated,
    /// 候选端口都被无关程序占着：加不了锁，照常启动，只是不保证单实例。
    Unprotected,
}

/// 抢锁：按数据目录派生身份，从派生端口起最多试探 `MAX_CANDIDATES` 个候选；
/// 成功返回主实例监听，全部占尽才说明为什么不做守卫。
pub fn claim(data_dir: &Path) -> Claim {
    let key = guard_key(data_dir);
    let base = base_port(&key);
    for attempt in 0..MAX_CANDIDATES {
        let port = candidate_port(base, attempt);
        match TcpListener::bind(("127.0.0.1", port)) {
            // 监听保持阻塞语义：接管方是普通线程（`spawn_knock_listener`），
            // 不再经 tokio from_std，也就不需要提前转非阻塞。
            Ok(listener) => return Claim::Primary(listener),
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if knock(&key, port) {
                    return Claim::Activated;
                }
                // 令牌对不上：这个端口属于无关程序或另一份数据的 RuTerm，
                // 向后试探下一个候选。
            }
            Err(error) => {
                log::warn!("绑定单实例端口 {port} 失败: {error}");
            }
        }
    }
    log::warn!("单实例候选端口全部被占，本次不做单实例守卫");
    Claim::Unprotected
}

/// 敲门：递令牌并等主实例回同一个令牌，回得上才认定对面是同一数据目录的 RuTerm。
///
/// 主实例可能刚绑好端口、`serve` 还没跑起来（抢锁与建窗之间有几百毫秒）：
/// 对同一端口做几轮短退避，避免同目录并发启动时认不出对面、错误地再起一份。
/// 整个过程只发生在端口被占这条少见路径上，上限约 2.7 秒。
fn knock(guard_key: &str, port: u16) -> bool {
    let token = token(guard_key);
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    for backoff_ms in [0u64, 300, 600] {
        if backoff_ms > 0 {
            std::thread::sleep(Duration::from_millis(backoff_ms));
        }
        let Ok(mut stream) = TcpStream::connect_timeout(&addr, HANDSHAKE) else {
            return false;
        };
        let _ = stream.set_write_timeout(Some(HANDSHAKE));
        let _ = stream.set_read_timeout(Some(HANDSHAKE));
        if stream.write_all(&token).is_err() {
            continue;
        }
        let _ = stream.flush();
        let mut reply = vec![0u8; token.len()];
        if stream.read_exact(&mut reply).is_ok() && reply == token {
            return true;
        }
    }
    false
}

/// 主实例侧：起一个普通线程跑等敲门循环。
///
/// 不用 tokio 任务而是 std 线程，而且要抢完锁就立刻起（建窗之前）：
/// 若等应用循环建好窗口才监听，用户快速双击时第二个进程敲门敲不到人，
/// 会认不出同目录已有实例、错误地再起一份。每次合法敲门（同数据目录的
/// 令牌核对成功）回令牌并调用 `wake`——`wake` 要能跨线程（转递给主线程
/// 把主窗口带回前台）。
pub fn spawn_knock_listener<F: Fn() + Send + 'static>(
    listener: TcpListener,
    data_dir: &Path,
    wake: F,
) {
    let token = token(&guard_key(data_dir));
    std::thread::spawn(move || {
        loop {
            // accept 持续报错时不能变成忙等（错误立即返回会让这个线程吃满一个核）。
            let (mut stream, _) = match listener.accept() {
                Ok(accepted) => accepted,
                Err(error) => {
                    log::warn!("单实例监听 accept 失败: {error}");
                    std::thread::sleep(Duration::from_millis(200));
                    continue;
                }
            };
            // 每个连接都受 socket 超时约束：读半截、连上不说话、慢速灌数据
            // 都只会占住这个线程几百毫秒。
            let _ = stream.set_read_timeout(Some(HANDSHAKE));
            let _ = stream.set_write_timeout(Some(HANDSHAKE));
            let mut received = vec![0u8; token.len()];
            let authenticated = stream.read_exact(&mut received).is_ok()
                && received == token
                && stream.write_all(&token).is_ok()
                && stream.flush().is_ok();
            if authenticated {
                wake();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Claim, DEFAULT_PORT, MAX_CANDIDATES, PORT_SPREAD, base_port, candidate_port, claim,
        spawn_knock_listener, token,
    };

    fn test_key(tag: &str) -> String {
        format!("guard-test-{tag}")
    }

    #[test]
    fn port_is_stable_per_key_and_stays_in_range() {
        let key = test_key("range");
        assert_eq!(base_port(&key), base_port(&key));
        assert!((DEFAULT_PORT..DEFAULT_PORT + PORT_SPREAD).contains(&base_port(&key)));
    }

    #[test]
    fn candidate_ports_wrap_away_from_zero() {
        for attempt in 0..MAX_CANDIDATES {
            let port = candidate_port(65_535, attempt);
            assert!((1..=65_535).contains(&port));
        }
        // 相邻候选互不相同。
        let ports: Vec<u16> = (0..MAX_CANDIDATES)
            .map(|a| candidate_port(47_122, a))
            .collect();
        let mut sorted = ports.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), ports.len());
    }

    #[test]
    fn token_carries_the_guard_key() {
        let key = test_key("token");
        let bytes = token(&key);
        assert!(bytes.starts_with(b"ruterm-instance-v2\x00"));
        assert!(bytes.ends_with(key.as_bytes()));
    }

    #[test]
    fn base_port_taken_falls_to_next_candidate() {
        // 无关程序占住首选端口：claim 应向后试探拿到另一个端口，而不是无守卫。
        let dir = std::env::temp_dir().join("ruterm-instance-taken");
        let taken_base = base_port(&super::guard_key(&dir));
        let squatter = std::net::TcpListener::bind(("127.0.0.1", taken_base))
            .expect("占住首选端口（偶发端口冲突则跳过）");
        match claim(&dir) {
            Claim::Primary(listener) => {
                assert_ne!(listener.local_addr().unwrap().port(), taken_base);
            }
            other => panic!("应向后试探拿到主实例，实际 {other:?}"),
        }
        drop(squatter);
    }

    #[test]
    fn second_claim_of_same_dir_activates_and_wakes() {
        let dir = std::env::temp_dir().join("ruterm-instance-same-dir");
        let Claim::Primary(listener) = claim(&dir) else {
            panic!("第一次 claim 应拿到主实例");
        };
        let wakes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = wakes.clone();
        // 真实进程里监听线程是抢完锁立刻起的（建窗之前），这里同样先起再敲。
        spawn_knock_listener(listener, &dir, move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        // 第二次启动（同数据目录）：knock 的退避会等 accept 就位。
        match claim(&dir) {
            Claim::Activated => {}
            other => panic!("同目录第二次 claim 应 Activated，实际 {other:?}"),
        }
        for _ in 0..100 {
            if wakes.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert_eq!(wakes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn different_dirs_keep_their_own_guard() {
        // 便携语义的核心回归：两个不同数据目录各自抢到锁、各起各的，
        // 后一个绝不能被前一个「唤醒」。
        let dir_a = std::env::temp_dir().join("ruterm-instance-dir-a");
        let dir_b = std::env::temp_dir().join("ruterm-instance-dir-b");
        let a = claim(&dir_a);
        assert!(matches!(a, Claim::Primary(_)));
        match claim(&dir_b) {
            Claim::Primary(_) => {}
            other => panic!("不同数据目录应各自抢锁，实际 {other:?}"),
        }
        drop(a);
    }
}
