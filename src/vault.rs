//! 凭据保险库：主机配置与密码统一存放在本地 turso 加密数据库（AEGIS-256 页级加密）。
//!
//! 主密钥为 32 随机字节的十六进制串，只保存在操作系统凭据管理器
//! （Windows 凭据管理器 / macOS 钥匙串 / Linux libsecret）中，不落盘、不入库；
//! 首次运行时自动生成并写入。迁移旧 sessions.json 时改名保留、绝不删除。

use std::path::Path;

use anyhow::{Context as _, Result};
use keyring::Entry;
use tokio::sync::Mutex;
use turso::{Builder, EncryptionOpts, Value};

use crate::sessions::{AuthMethod, HostConfig};

/// 凭据管理器中的服务名与账户名（主密钥条目）。
const KEYRING_SERVICE: &str = "rulink";
const KEYRING_ACCOUNT: &str = "vault-key";

/// 加密库文件名（位于 ~/.rulink/ 下）。
const VAULT_FILE: &str = "vault.db";

/// 建表语句：password 允许 NULL（未勾选保存的主机）。
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS hosts (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    host TEXT NOT NULL,
    port INTEGER NOT NULL,
    user TEXT NOT NULL,
    auth TEXT NOT NULL,
    key_path TEXT NOT NULL DEFAULT '',
    favorite INTEGER NOT NULL DEFAULT 0,
    password TEXT
)";

/// 认证方式与库内标签字符串的互转。
fn auth_tag(auth: AuthMethod) -> &'static str {
    match auth {
        AuthMethod::Agent => "agent",
        AuthMethod::IdentityFile => "key",
        AuthMethod::Password => "password",
    }
}

fn auth_from_tag(tag: &str) -> AuthMethod {
    match tag {
        "key" => AuthMethod::IdentityFile,
        "password" => AuthMethod::Password,
        _ => AuthMethod::Agent,
    }
}

/// 主密钥必须是 64 位十六进制（32 字节）。
fn valid_hex_key(hex: &str) -> bool {
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 从凭据管理器读取主密钥；没有条目时生成随机密钥并写入。
///
/// 若已有条目但格式非法，直接报错而不是覆盖——覆盖会让旧库永久无法解密。
fn load_or_create_master_key() -> Result<String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("无法访问系统凭据管理器")?;
    match entry.get_password() {
        Ok(hex) if valid_hex_key(&hex) => Ok(hex),
        Ok(_) => anyhow::bail!(
            "系统凭据管理器中的 rulink 主密钥条目格式无效，为避免旧库无法解密，已拒绝自动覆盖"
        ),
        Err(keyring::Error::NoEntry) => {
            let mut buf = [0u8; 32];
            getrandom::fill(&mut buf).context("生成随机主密钥失败")?;
            let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
            entry
                .set_password(&hex)
                .context("写入主密钥到系统凭据管理器失败")?;
            Ok(hex)
        }
        Err(error) => Err(anyhow::Error::new(error).context("读取主密钥失败")),
    }
}

/// 行读取辅助：按列序取文本/整数，缺值按空串/0 处理。
fn text_of(row: &turso::Row, idx: usize) -> String {
    match row.get_value(idx) {
        Ok(Value::Text(text)) => text,
        _ => String::new(),
    }
}

fn int_of(row: &turso::Row, idx: usize) -> i64 {
    match row.get_value(idx) {
        Ok(Value::Integer(value)) => value,
        _ => 0,
    }
}

/// 加密凭据库连接句柄（内部 Mutex 保证可跨异步任务共享）。
pub struct Vault {
    /// 保持 Database 存活（Connection 依赖其生命周期内的引用语义）。
    _db: turso::Database,
    conn: Mutex<turso::Connection>,
}

impl Vault {
    /// 打开（或创建）dir 下的加密库，确保 schema 就绪。
    pub async fn open(dir: &Path) -> Result<Self> {
        let hexkey = load_or_create_master_key()?;
        std::fs::create_dir_all(dir).with_context(|| format!("无法创建 {}", dir.display()))?;
        let path = dir.join(VAULT_FILE).to_string_lossy().to_string();
        let db = Builder::new_local(&path)
            .experimental_encryption(true)
            .with_encryption(EncryptionOpts {
                cipher: "aegis256".to_string(),
                hexkey,
            })
            .build()
            .await
            .context("无法打开加密凭据库（主密钥或库文件可能已损坏）")?;
        let conn = db.connect().context("凭据库连接建立失败")?;
        conn.execute(SCHEMA, ()).await.context("凭据库建表失败")?;
        Ok(Self {
            _db: db,
            conn: Mutex::new(conn),
        })
    }

    /// 主机总数（用于判断是否需要从旧 JSON 迁移）。
    pub async fn count(&self) -> Result<i64> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM hosts", ())
            .await
            .context("查询凭据库失败")?;
        match rows.next().await? {
            Some(row) => Ok(int_of(&row, 0)),
            None => Ok(0),
        }
    }

    /// 读取全部主机（不含密码列）。
    pub async fn load_hosts(&self) -> Result<Vec<HostConfig>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query(
                "SELECT id, name, host, port, user, auth, key_path, favorite FROM hosts ORDER BY id",
                (),
            )
            .await
            .context("读取凭据库失败")?;
        let mut hosts = Vec::new();
        while let Some(row) = rows.next().await? {
            hosts.push(HostConfig {
                id: int_of(&row, 0),
                name: text_of(&row, 1),
                host: text_of(&row, 2),
                port: int_of(&row, 3).clamp(1, u16::MAX as i64) as u16,
                user: text_of(&row, 4),
                auth: auth_from_tag(&text_of(&row, 5)),
                key_path: text_of(&row, 6),
                favorite: int_of(&row, 7) != 0,
            });
        }
        Ok(hosts)
    }

    /// 批量导入（一次性迁移用）。
    pub async fn import_hosts(&self, hosts: &[HostConfig]) -> Result<()> {
        for host in hosts {
            self.upsert(host).await?;
        }
        Ok(())
    }

    /// 保存主机配置；密码列保持原样（UPDATE 列表不含 password）。
    pub async fn upsert(&self, host: &HostConfig) -> Result<()> {
        let conn = self.conn.lock().await;
        let changed = conn
            .execute(
                "UPDATE hosts SET name = ?, host = ?, port = ?, user = ?, auth = ?, key_path = ?, favorite = ? WHERE id = ?",
                (
                    host.name.as_str(),
                    host.host.as_str(),
                    host.port as i64,
                    host.user.as_str(),
                    auth_tag(host.auth),
                    host.key_path.as_str(),
                    host.favorite as i64,
                    host.id,
                ),
            )
            .await
            .context("更新凭据库失败")?;
        if changed == 0 {
            conn.execute(
                "INSERT INTO hosts(id, name, host, port, user, auth, key_path, favorite) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    host.id,
                    host.name.as_str(),
                    host.host.as_str(),
                    host.port as i64,
                    host.user.as_str(),
                    auth_tag(host.auth),
                    host.key_path.as_str(),
                    host.favorite as i64,
                ),
            )
            .await
            .context("写入凭据库失败")?;
        }
        Ok(())
    }

    /// 删除主机（连带其密码行）。
    pub async fn remove(&self, id: i64) -> Result<()> {
        let conn = self.conn.lock().await;
        conn.execute("DELETE FROM hosts WHERE id = ?", (id,))
            .await
            .context("删除凭据库记录失败")?;
        Ok(())
    }

    /// 写入或清除某主机的保存密码。
    pub async fn set_password(&self, id: i64, password: Option<&str>) -> Result<()> {
        let conn = self.conn.lock().await;
        match password {
            Some(text) => {
                conn.execute("UPDATE hosts SET password = ? WHERE id = ?", (text, id))
                    .await?;
            }
            None => {
                conn.execute("UPDATE hosts SET password = NULL WHERE id = ?", (id,))
                    .await?;
            }
        }
        Ok(())
    }

    /// 读取某主机已保存的密码（未保存则为 None）。
    pub async fn password_of(&self, id: i64) -> Result<Option<String>> {
        let conn = self.conn.lock().await;
        let mut rows = conn
            .query("SELECT password FROM hosts WHERE id = ?", (id,))
            .await
            .context("读取保存密码失败")?;
        match rows.next().await? {
            Some(row) => match row.get_value(0) {
                Ok(Value::Text(text)) => Ok(Some(text)),
                _ => Ok(None),
            },
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_key_validation() {
        assert!(valid_hex_key(&"0a".repeat(32)));
        assert!(!valid_hex_key(&"0a".repeat(31)));
        assert!(!valid_hex_key(&"zz".repeat(32)));
        assert!(!valid_hex_key(""));
    }

    #[test]
    fn auth_tag_roundtrip() {
        for auth in [
            AuthMethod::Agent,
            AuthMethod::IdentityFile,
            AuthMethod::Password,
        ] {
            assert_eq!(auth_from_tag(auth_tag(auth)), auth);
        }
        assert_eq!(auth_from_tag("未知"), AuthMethod::Agent);
    }
}
