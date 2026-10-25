//! 连接目标解析：命令行与凭据来源（密码只走环境变量，不落命令行）。

use std::path::PathBuf;

/// 一次 SSH 会话的连接参数。
#[derive(Clone, Debug)]
pub struct SshTarget {
    /// 登录用户名
    pub user: String,
    /// 主机地址
    pub host: String,
    /// 端口（默认 22）
    pub port: u16,
    /// 私钥文件（可选；与密码二选一，都没有则尝试 SSH Agent）
    pub key_path: Option<PathBuf>,
    /// 密码（来自环境变量 RULINK_PASSWORD）
    pub password: Option<String>,
}

impl SshTarget {
    /// 从命令行参数解析连接目标。
    ///
    /// 用法：`rulink [user@]host[:port] [-i keyfile]`
    /// 密码经 `RULINK_PASSWORD` 环境变量传入。
    pub fn from_args() -> anyhow::Result<Self> {
        let mut positional: Option<String> = None;
        let mut key_path: Option<PathBuf> = None;
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-i" | "--identity" => {
                    let path = args
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("-i 需要跟随一个私钥文件路径"))?;
                    key_path = Some(PathBuf::from(path));
                }
                "-h" | "--help" => {
                    anyhow::bail!(
                        "用法: rulink [user@]host[:port] [-i keyfile]\n密码通过环境变量 RULINK_PASSWORD 提供"
                    );
                }
                other => {
                    if other.starts_with('-') {
                        anyhow::bail!("未知参数: {other}");
                    }
                    if positional.is_some() {
                        anyhow::bail!("只接受一个连接目标");
                    }
                    positional = Some(other.to_string());
                }
            }
        }

        let raw = positional.ok_or_else(|| {
            anyhow::anyhow!(
                "用法: rulink [user@]host[:port] [-i keyfile]（密码经 RULINK_PASSWORD）"
            )
        })?;

        let (user, rest) = match raw.split_once('@') {
            Some((user, rest)) => (user.to_string(), rest.to_string()),
            None => (
                std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_else(|_| "root".to_string()),
                raw.clone(),
            ),
        };

        let (host, port) = match rest.rsplit_once(':') {
            Some((host, port_str)) => {
                let port = port_str
                    .parse::<u16>()
                    .map_err(|_| anyhow::anyhow!("端口无法解析: {port_str}"))?;
                (host.to_string(), port)
            }
            None => (rest, 22),
        };

        Ok(Self {
            user,
            host,
            port,
            key_path,
            password: std::env::var("RULINK_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }

    /// known_hosts 记录用的展示地址（host:port）。
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}
