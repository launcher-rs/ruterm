//! 连接目标解析：命令行快速连接与 GUI 主机账号的统一形态。

use std::path::PathBuf;

use crate::sessions::{AuthMethod, HostConfig};

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
    /// 内嵌私钥的 PEM 文本（取自加密库，仅内存持有；优先于 `key_path`）
    pub key_data: Option<String>,
    /// 密码（来自环境变量 RUTERM_PASSWORD 或 GUI 临时输入/加密库）
    pub password: Option<String>,
    /// 加密私钥的口令（仅私钥认证且密钥有口令时使用；绝不入命令行）
    pub passphrase: Option<String>,
    /// 跳板机链（最内层在这里，向内嵌套）；None 表示直连。
    ///
    /// 每一跳只额外建立一条 `direct-tcpip` 通道，SSH 跑在通道之上：
    /// 跳板机本身不需要端口转发规则，但它的 sshd 必须允许 TCP 转发。
    pub jump: Option<Box<SshTarget>>,
}

impl SshTarget {
    /// 从命令行参数解析连接目标。
    ///
    /// 用法：`ruterm [user@]host[:port] [-i keyfile]`
    /// 密码经 `RUTERM_PASSWORD` 环境变量传入。
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
                        "用法: ruterm [user@]host[:port] [-i keyfile]\n密码通过环境变量 RUTERM_PASSWORD 提供"
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
                "用法: ruterm [user@]host[:port] [-i keyfile]（密码经 RUTERM_PASSWORD）"
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
            key_data: None,
            passphrase: None,
            jump: None,
            password: std::env::var("RUTERM_PASSWORD")
                .ok()
                .filter(|s| !s.is_empty()),
        })
    }

    /// known_hosts 记录用的展示地址（host:port）。
    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 由主机账号 + 连接时临时输入的密码构造连接目标。
    ///
    /// 密码仅在内存中传递；加密库只存勾选"保存"后的密文；`key_path` 仅私钥认证时使用。
    /// 内嵌私钥由 [`with_key_data`](Self::with_key_data) 补充；私钥口令经
    /// [`with_passphrase`](Self::with_passphrase) 补充。
    pub fn from_host(host: &HostConfig, password: Option<String>) -> Self {
        let key_path = host.key_path.trim();
        Self {
            user: host.user.clone(),
            host: host.host.clone(),
            port: host.port,
            key_path: if host.auth == AuthMethod::IdentityFile && !key_path.is_empty() {
                Some(PathBuf::from(key_path))
            } else {
                None
            },
            key_data: None,
            password,
            passphrase: None,
            jump: None,
        }
    }

    /// 挂上跳板机链（参数是**最内层**跳板机，它自己可以再带 `jump`）。
    pub fn with_jump(mut self, jump: Option<SshTarget>) -> Self {
        self.jump = jump.map(Box::new);
        self
    }

    /// 跳板链，从最外层到最内层（按"先连谁"排序），不含最终目标。
    ///
    /// `target.jump` 是最近一跳，`jump.jump` 是它的跳板机，因此收集后需要反转。
    pub fn jump_chain(&self) -> Vec<SshTarget> {
        let mut chain = Vec::new();
        let mut cursor = self.jump.as_deref();
        while let Some(jump) = cursor {
            chain.push(jump.clone());
            cursor = jump.jump.as_deref();
        }
        chain.reverse();
        chain
    }

    /// 补充内嵌私钥（PEM 文本，仅内存传递）。
    pub fn with_key_data(mut self, data: Option<String>) -> Self {
        self.key_data = data;
        self
    }

    /// 补充加密私钥的口令（仅内存传递）。
    pub fn with_passphrase(mut self, passphrase: Option<String>) -> Self {
        self.passphrase = passphrase;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(auth: AuthMethod, key_path: &str, key_inline: bool) -> HostConfig {
        HostConfig {
            id: 1,
            name: "测试机".into(),
            host: "example.com".into(),
            port: 22,
            user: "dev".into(),
            auth,
            key_path: key_path.into(),
            key_inline,
            favorite: false,
            proxy_jump: None,
        }
    }

    /// 内嵌私钥的主机没有路径：不能构造出一个空路径去让加载器报错。
    #[test]
    fn inline_key_host_has_no_key_path() {
        let target = SshTarget::from_host(&host(AuthMethod::IdentityFile, "", true), None);
        assert!(target.key_path.is_none());
        // 非私钥认证时即便残留路径也不启用。
        let target = SshTarget::from_host(&host(AuthMethod::Password, "C:/k", false), None);
        assert!(target.key_path.is_none());
        let target = SshTarget::from_host(&host(AuthMethod::IdentityFile, "C:/k ", false), None);
        assert_eq!(target.key_path, Some(PathBuf::from("C:/k")));
    }

    #[test]
    fn key_data_travels_with_the_target() {
        let target = SshTarget::from_host(&host(AuthMethod::IdentityFile, "", true), None)
            .with_key_data(Some("PEM".into()))
            .with_passphrase(Some("pp".into()));
        assert_eq!(target.key_data.as_deref(), Some("PEM"));
        assert_eq!(target.passphrase.as_deref(), Some("pp"));
    }

    fn plain(name: &str) -> SshTarget {
        let mut config = host(AuthMethod::Agent, "", false);
        config.host = name.into();
        config.user = "u".into();
        SshTarget::from_host(&config, None)
    }

    /// 链从最外层开始：目标挂 jump1、jump1 挂 jump2，就先连 jump2。
    #[test]
    fn jump_chain_orders_outermost_first() {
        let target = plain("target").with_jump(Some(plain("jump1").with_jump(Some(plain("jump2")))));
        let chain: Vec<String> = target
            .jump_chain()
            .iter()
            .map(|node| node.host.clone())
            .collect();
        assert_eq!(chain, ["jump2", "jump1"]);
        // 直连没有跳板链。
        assert!(plain("solo").jump_chain().is_empty());
    }
}
