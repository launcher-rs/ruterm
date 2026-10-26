//! 凭据保险库：主机配置、应用设置与 known_hosts 统一存放在单文件加密数据库
//! （toasty ORM + turso 页级加密 AEGIS-256）。
//!
//! 解锁后端有两档（[`UnlockBackend`]）：
//! - **keyring**：主密钥为 32 随机字节的十六进制串，只保存在操作系统凭据管理器
//!   （Windows 凭据管理器 / macOS 钥匙串 / Linux libsecret）中，不落盘、不入库；
//!   首次运行时自动生成并写入。全局模式默认走这一档，启动零输入。
//! - **passphrase**：用户自定义主密码经 Argon2id 派生（盐明文存放
//!   `kdf-salt.bin`，是公开参数）。便携模式强制此档——库文件可随 `data/`
//!   拷往其他机器，凭口令解锁；全局模式亦可主动"设置主密码"迁移过来。
//!
//! 迁移旧文件（vault.db / sessions.json / settings.json / known_hosts）时
//! 改名保留 `.migrated`、绝不删除。turso 无 rekey 能力，更换主密钥走
//! "新建加密库 + 全量搬迁 + 旧库改名备份 + 替换"路径。

use std::path::Path;

use anyhow::{Context as _, Result};
use argon2::Argon2;
use keyring::Entry;
use toasty::stmt::{List, Query};
use toasty_driver_turso::{EncryptionOpts, Turso};
use tokio::sync::Mutex;
use turso::{Builder, Value};

use crate::paths::StorageMode;
use crate::sessions::{AuthMethod, HostConfig};

/// 凭据管理器中的服务名与账户名（keyring 档主密钥条目）。
const KEYRING_SERVICE: &str = "rulink";
const KEYRING_ACCOUNT: &str = "vault-key";

/// 新版单文件库：三张表全部由 toasty 建模，启动时 `push_schema` 幂等建表。
mod schema {
    use toasty::Model;

    use crate::sessions::HostConfig;

    /// 主机行；password 列为可选密文文本（库级加密下按原样存取，未保存则 NULL）。
    #[derive(Debug, Model)]
    #[table = "hosts"]
    pub struct Host {
        #[key]
        pub id: i64,
        pub name: String,
        pub host: String,
        pub port: i64,
        pub user: String,
        pub auth: String,
        pub key_path: String,
        pub favorite: bool,
        pub password: Option<String>,
    }

    impl Host {
        /// 转成 UI/连接层使用的主机配置。
        pub fn to_host_config(&self) -> HostConfig {
            HostConfig {
                id: self.id,
                name: self.name.clone(),
                host: self.host.clone(),
                port: self.port.clamp(1, u16::MAX as i64) as u16,
                user: self.user.clone(),
                auth: crate::vault::auth_from_tag(&self.auth),
                key_path: self.key_path.clone(),
                favorite: self.favorite,
            }
        }
    }

    /// 应用级设置（主题等），键值对。
    #[derive(Debug, Model)]
    #[table = "settings"]
    #[key(name)]
    pub struct Setting {
        pub name: String,
        pub value: String,
    }

    /// known_hosts 行：`[host]:port` → 指纹。
    #[derive(Debug, Model)]
    #[table = "known_hosts"]
    #[key(host_port)]
    pub struct KnownHost {
        pub host_port: String,
        pub fingerprint: String,
        pub algorithm: String,
    }
}

use schema::{Host as HostRow, KnownHost as KnownHostRow, Setting as SettingRow};

/// 解锁后端标记（写入数据目录 `unlock.txt`，非敏感）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnlockBackend {
    /// 系统凭据管理器里的随机密钥（全局模式默认，零输入）。
    Keyring,
    /// 用户主密码经 Argon2id 派生（便携模式强制；全局可选）。
    Passphrase,
}

impl UnlockBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            UnlockBackend::Keyring => "keyring",
            UnlockBackend::Passphrase => "passphrase",
        }
    }
}

/// 主机密钥信任判定结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// 指纹与库中记录一致。
    Matched,
    /// 有记录但指纹不符——必须拒绝连接（fail-closed）。
    Mismatch,
    /// 首次接触，已写入新记录并信任。
    New,
}

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

/// 从凭据管理器读取 keyring 档主密钥；没有条目时生成随机密钥并写入。
///
/// 若已有条目但格式非法，直接报错而不是覆盖——覆盖会让旧库永久无法解密。
pub fn load_or_create_master_key() -> Result<String> {
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

/// 读取（或首次生成）Argon2 派生盐；盐是公开参数，明文存放即可。
pub fn load_or_create_salt(path: &Path) -> Result<[u8; 16]> {
    if let Ok(bytes) = std::fs::read(path)
        && bytes.len() == 16
    {
        let mut salt = [0u8; 16];
        salt.copy_from_slice(&bytes);
        return Ok(salt);
    }
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).context("生成派生盐失败")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, salt).with_context(|| format!("写入 {}", path.display()))?;
    Ok(salt)
}

/// 主密码 → 32 字节主密钥（Argon2id，`Params::DEFAULT` 参数；
/// 强度依赖口令本身——README 明示）。
pub fn derive_key(salt: &[u8; 16], password: &str) -> Result<String> {
    let hasher = Argon2::default();
    let mut derived = [0u8; 32];
    hasher
        .hash_password_into(password.as_bytes(), salt, &mut derived)
        .map_err(|error| anyhow::anyhow!("主密码派生失败: {error}"))?;
    Ok(derived.iter().map(|b| format!("{b:02x}")).collect())
}

/// 更换主密码时强制生成新盐并写盘（旧盐不得复用，保证新密钥随新口令变化）。
pub fn rotate_salt(path: &Path) -> Result<[u8; 16]> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).context("生成派生盐失败")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, salt).with_context(|| format!("写入 {}", path.display()))?;
    Ok(salt)
}

/// 读取数据目录中的解锁后端标记；文件缺失按 keyring 档处理。
pub fn read_unlock_marker(mode: &StorageMode) -> UnlockBackend {
    match std::fs::read_to_string(mode.unlock_marker_path()) {
        Ok(text) if text.trim() == "passphrase" => UnlockBackend::Passphrase,
        _ => UnlockBackend::Keyring,
    }
}

/// 写入解锁后端标记（明文，非敏感）。
pub fn write_unlock_marker(mode: &StorageMode, backend: UnlockBackend) -> Result<()> {
    mode.ensure_dir()
        .with_context(|| format!("无法创建 {}", mode.dir().display()))?;
    std::fs::write(mode.unlock_marker_path(), backend.as_str())
        .with_context(|| format!("写入 {}", mode.unlock_marker_path().display()))?;
    Ok(())
}

/// 加密凭据库句柄（toasty Db 内部自带连接池，Mutex 仅收敛共享语义）。
pub struct Vault {
    db: Mutex<toasty::Db>,
}

impl Vault {
    /// 用给定主密钥打开（或创建）mode 数据目录下的单文件加密库并建表。
    pub async fn open(mode: &StorageMode, hexkey: String) -> Result<Self> {
        anyhow::ensure!(
            valid_hex_key(&hexkey),
            "主密钥格式无效（应为 64 位十六进制）"
        );
        mode.ensure_dir()
            .with_context(|| format!("无法创建 {}", mode.dir().display()))?;
        let path = mode.db_path().to_string_lossy().to_string();
        let driver = Turso::file(&path).experimental_encryption(EncryptionOpts {
            cipher: "aegis256".to_string(),
            hexkey,
        });
        let mut db = toasty::Db::builder()
            .models(toasty::models!(HostRow, SettingRow, KnownHostRow))
            .build(driver)
            .await
            .context("无法打开加密凭据库（主密码或库文件可能不正确）")?;
        // CREATE TABLE 不带 IF NOT EXISTS：先探测 hosts 表是否已存在，
        // 缺表才建（库能成功打开本身已证明密钥正确）。
        let hosts_ready = Query::<List<HostRow>>::all()
            .exec(&mut db as &mut dyn toasty::Executor)
            .await
            .is_ok();
        if !hosts_ready {
            db.push_schema()
                .await
                .context("凭据库建表失败（若升级后仍报错，请检查库文件完整性）")?;
        }
        Ok(Self { db: Mutex::new(db) })
    }

    /// 在指定文件路径打开加密库（更换主密码的暂存库 `rulink.db.new` 用）。
    pub async fn open_at(path: &str, hexkey: String) -> Result<Self> {
        anyhow::ensure!(
            valid_hex_key(&hexkey),
            "主密钥格式无效（应为 64 位十六进制）"
        );
        let driver = Turso::file(path).experimental_encryption(EncryptionOpts {
            cipher: "aegis256".to_string(),
            hexkey,
        });
        let db = toasty::Db::builder()
            .models(toasty::models!(HostRow, SettingRow, KnownHostRow))
            .build(driver)
            .await
            .context("无法创建新加密库")?;
        db.push_schema().await.context("新加密库建表失败")?;
        Ok(Self { db: Mutex::new(db) })
    }

    /// 主机总数（测试与诊断用）。
    #[allow(dead_code)]
    pub async fn count(&self) -> Result<i64> {
        Ok(self.load_hosts().await?.len() as i64)
    }

    /// 读取全部主机（不含密码列）。
    pub async fn load_hosts(&self) -> Result<Vec<HostConfig>> {
        let mut db = self.db.lock().await;
        let rows = Query::<List<HostRow>>::all()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("读取凭据库失败")?;
        Ok(rows.iter().map(|row| row.to_host_config()).collect())
    }

    /// 批量导入（一次性迁移用）。
    pub async fn import_hosts(&self, hosts: &[HostConfig]) -> Result<()> {
        for host in hosts {
            self.upsert(host).await?;
        }
        Ok(())
    }

    /// 保存主机配置；已保存的密码列保持原样（本方法永不写密码）。
    pub async fn upsert(&self, host: &HostConfig) -> Result<()> {
        let existing = self.password_of(host.id).await?;
        self.write_host(host, existing.as_deref()).await
    }

    /// 写入或清除某主机的保存密码（password=None 表示清除）。
    pub async fn set_password(&self, id: i64, password: Option<&str>) -> Result<()> {
        let mut db = self.db.lock().await;
        let row = Query::<List<HostRow>>::all()
            .filter(HostRow::fields().id().eq(id))
            .first()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("读取主机行失败")?;
        drop(db);
        let Some(row) = row else { return Ok(()) };
        let host = row.to_host_config();
        self.write_host(&host, password).await
    }

    /// 读取某主机已保存的密码（未保存则为 None）。
    pub async fn password_of(&self, id: i64) -> Result<Option<String>> {
        let mut db = self.db.lock().await;
        let row = Query::<List<HostRow>>::all()
            .filter(HostRow::fields().id().eq(id))
            .first()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("读取保存密码失败")?;
        Ok(row.and_then(|row| row.password))
    }

    /// 主机行落库（upsert by id；password 由调用方决定，None 写 NULL）。
    async fn write_host(&self, host: &HostConfig, password: Option<&str>) -> Result<()> {
        let mut db = self.db.lock().await;
        HostRow::upsert_by_id(host.id)
            .name(host.name.as_str())
            .host(host.host.as_str())
            .port(host.port as i64)
            .user(host.user.as_str())
            .auth(auth_tag(host.auth))
            .key_path(host.key_path.as_str())
            .favorite(host.favorite)
            .password(password.map(str::to_string))
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("写入凭据库失败")?;
        Ok(())
    }

    /// 删除主机（连带其密码行）。
    pub async fn remove(&self, id: i64) -> Result<()> {
        let mut db = self.db.lock().await;
        Query::<List<HostRow>>::all()
            .filter(HostRow::fields().id().eq(id))
            .delete()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("删除凭据库记录失败")?;
        Ok(())
    }

    /// 读取设置项（缺失为 None）。
    pub async fn setting(&self, name: &str) -> Result<Option<String>> {
        let mut db = self.db.lock().await;
        let row = Query::<List<SettingRow>>::all()
            .filter(SettingRow::fields().name().eq(name.to_string()))
            .first()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("读取设置失败")?;
        Ok(row.map(|row| row.value))
    }

    /// 写入设置项（存在则覆盖）。
    pub async fn set_setting(&self, name: &str, value: &str) -> Result<()> {
        let mut db = self.db.lock().await;
        SettingRow::upsert_by_name(name.to_string())
            .value(value)
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("写入设置失败")?;
        Ok(())
    }

    /// known_hosts 判定（fail-closed）：命中比对、不符拒绝、缺失写入并信任。
    ///
    /// 库内任何读取/写入失败都返回 Err——调用方必须拒绝连接，
    /// 绝不能把"查询失败"降级为"放行"。
    pub async fn check_or_record_known_host(
        &self,
        host_port: &str,
        fingerprint: &str,
        algorithm: &str,
    ) -> Result<Trust> {
        let mut db = self.db.lock().await;
        let existing = Query::<List<KnownHostRow>>::all()
            .filter(KnownHostRow::fields().host_port().eq(host_port.to_string()))
            .first()
            .exec(&mut *db as &mut dyn toasty::Executor)
            .await
            .context("读取 known_hosts 失败")?;
        match existing {
            Some(row) if row.fingerprint == fingerprint => Ok(Trust::Matched),
            Some(_) => Ok(Trust::Mismatch),
            None => {
                KnownHostRow::upsert_by_host_port(host_port.to_string())
                    .fingerprint(fingerprint)
                    .algorithm(algorithm)
                    .exec(&mut *db as &mut dyn toasty::Executor)
                    .await
                    .context("写入 known_hosts 失败")?;
                Ok(Trust::New)
            }
        }
    }

    /// 导出本库全部数据（换主密钥时搬迁用）。
    pub async fn export_all(&self) -> Result<VaultDump> {
        let hosts = {
            let mut db = self.db.lock().await;
            Query::<List<HostRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出主机失败")?
        };
        let mut dumped = Vec::new();
        for row in hosts {
            dumped.push((row.to_host_config(), row.password));
        }
        let settings = {
            let mut db = self.db.lock().await;
            Query::<List<SettingRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出设置失败")?
        };
        let known = {
            let mut db = self.db.lock().await;
            Query::<List<KnownHostRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出 known_hosts 失败")?
        };
        Ok(VaultDump {
            hosts: dumped,
            settings: settings
                .iter()
                .map(|s| (s.name.clone(), s.value.clone()))
                .collect(),
            known_hosts: known
                .iter()
                .map(|k| {
                    (
                        k.host_port.clone(),
                        k.fingerprint.clone(),
                        k.algorithm.clone(),
                    )
                })
                .collect(),
        })
    }

    /// 导入另一库导出的全部数据（换主密钥后写入新库）。
    pub async fn import_dump(&self, dump: &VaultDump) -> Result<()> {
        for (host, password) in &dump.hosts {
            self.write_host(host, password.as_deref()).await?;
        }
        for (name, value) in &dump.settings {
            self.set_setting(name, value).await?;
        }
        for (host_port, fingerprint, algorithm) in &dump.known_hosts {
            let mut db = self.db.lock().await;
            KnownHostRow::upsert_by_host_port(host_port.to_string())
                .fingerprint(fingerprint.as_str())
                .algorithm(algorithm.as_str())
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导入 known_hosts 失败")?;
        }
        Ok(())
    }
}

/// 两库之间搬迁用的全量快照（仅存在于进程内存，含密码明文——库级加密下
/// 解密后的原生形态；绝不写日志、绝不落盘）。
pub struct VaultDump {
    pub hosts: Vec<(HostConfig, Option<String>)>,
    pub settings: Vec<(String, String)>,
    pub known_hosts: Vec<(String, String, String)>,
}

// ---------------------------------------------------------------------------
// 旧文件一次性迁移
// ---------------------------------------------------------------------------

/// 用旧版 hand-SQL 加密库（keyring 主密钥）读出 hosts 行（含密码列）。
/// 库损坏/密钥不对返回 Err（调用方据此回退改名，不动数据）。
async fn read_legacy_vault(
    path: &Path,
    hexkey: String,
) -> Result<Vec<(HostConfig, Option<String>)>> {
    let db = Builder::new_local(path.to_string_lossy().as_ref())
        .experimental_encryption(true)
        .with_encryption(EncryptionOpts {
            cipher: "aegis256".to_string(),
            hexkey,
        })
        .build()
        .await
        .context("打开旧 vault.db 失败")?;
    let conn = db.connect().context("旧 vault.db 连接失败")?;
    let mut rows = conn
        .query(
            "SELECT id, name, host, port, user, auth, key_path, favorite, password FROM hosts ORDER BY id",
            (),
        )
        .await
        .context("读取旧 vault.db 失败")?;
    let mut hosts = Vec::new();
    while let Some(row) = rows.next().await? {
        let password = match row.get_value(8) {
            Ok(Value::Text(text)) => Some(text),
            _ => None,
        };
        hosts.push((
            HostConfig {
                id: match row.get_value(0) {
                    Ok(Value::Integer(v)) => v,
                    _ => 0,
                },
                name: text_of(&row, 1),
                host: text_of(&row, 2),
                port: match row.get_value(3) {
                    Ok(Value::Integer(v)) => v.clamp(1, u16::MAX as i64) as u16,
                    _ => 22,
                },
                user: text_of(&row, 4),
                auth: auth_from_tag(&text_of(&row, 5)),
                key_path: text_of(&row, 6),
                favorite: matches!(row.get_value(7), Ok(Value::Integer(v)) if v != 0),
            },
            password,
        ));
    }
    Ok(hosts)
}

fn text_of(row: &turso::Row, idx: usize) -> String {
    match row.get_value(idx) {
        Ok(Value::Text(text)) => text,
        _ => String::new(),
    }
}

/// 解析旧 known_hosts 文本行：`[host]:port fingerprint [algorithm]`。
fn parse_legacy_known_hosts(text: &str) -> Vec<(String, String, String)> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(host_port), Some(fingerprint)) = (fields.next(), fields.next()) else {
            continue;
        };
        entries.push((
            host_port.to_string(),
            fingerprint.to_string(),
            fields.next().unwrap_or("unknown").to_string(),
        ));
    }
    entries
}

/// 把旧 vault.db / settings.json / known_hosts 一次性迁入新单文件库；
/// 成功项逐个改名 `.migrated` 保留现场。返回是否迁移过旧 vault.db。
pub async fn migrate_legacy(
    vault: &Vault,
    mode: &StorageMode,
    keyring_hexkey: &str,
) -> Result<bool> {
    let mut migrated_vault = false;
    let legacy_db_path = mode.legacy_path(crate::paths::LEGACY_VAULT_FILE);
    if legacy_db_path.exists() {
        // 先读原路径、成功后才改名：中途失败时旧库原样保留，不破坏现场。
        match read_legacy_vault(&legacy_db_path, keyring_hexkey.to_string()).await {
            Ok(hosts) => {
                for (host, password) in &hosts {
                    vault.upsert(host).await?;
                    if password.is_some() {
                        vault.set_password(host.id, password.as_deref()).await?;
                    }
                }
                rename_preserving(&legacy_db_path);
                migrated_vault = true;
            }
            Err(error) => {
                // 旧库打不开（密钥不符/损坏）：不动文件，交由上层保持现有行为。
                log::warn!("读取旧 vault.db 失败: {error}");
                return Err(error);
            }
        }
    }

    // settings.json：theme → Setting("theme")
    let settings_path = mode.legacy_path(crate::paths::LEGACY_SETTINGS_FILE);
    if settings_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&settings_path)
            && let Ok(settings) = serde_json::from_str::<crate::sessions::Settings>(&text)
        {
            vault.set_setting("theme", &settings.theme).await?;
        }
        rename_preserving(&settings_path);
    }

    // known_hosts 文件 → 表
    let kh_path = mode.legacy_path(crate::paths::LEGACY_KNOWN_HOSTS_FILE);
    if kh_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&kh_path) {
            for (host_port, fingerprint, algorithm) in parse_legacy_known_hosts(&text) {
                let mut db = vault.db.lock().await;
                KnownHostRow::upsert_by_host_port(host_port)
                    .fingerprint(fingerprint.as_str())
                    .algorithm(algorithm.as_str())
                    .exec(&mut *db as &mut dyn toasty::Executor)
                    .await
                    .context("导入 known_hosts 失败")?;
            }
        }
        rename_preserving(&kh_path);
    }
    Ok(migrated_vault)
}

/// 改名保留（`.migrated` 已存在则不动原文件，只警告）。
fn rename_preserving(path: &Path) {
    let target = path.with_file_name(format!(
        "{}.migrated",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("data")
    ));
    if target.exists() {
        log::warn!(
            "{} 已存在，保留 {} 不重复改名",
            target.display(),
            path.display()
        );
        return;
    }
    if let Err(error) = std::fs::rename(path, &target) {
        log::warn!("迁移后改名 {} 失败: {error}", path.display());
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

    /// 端到端：加密单文件库建表、主机 upsert 保密码、设置读写、known_hosts 三态。
    #[tokio::test]
    async fn vault_roundtrip_on_encrypted_file() {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let dir = std::env::temp_dir().join(format!(
            "rulink-vault-test-{}",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        let mode = StorageMode::Global(dir.clone());
        let hexkey = "0f".repeat(32);
        let vault = Vault::open(&mode, hexkey.clone())
            .await
            .expect("打开加密库失败");

        let host = HostConfig {
            id: 3,
            name: "测试机".into(),
            host: "example.com".into(),
            port: 2222,
            user: "dev".into(),
            auth: AuthMethod::IdentityFile,
            key_path: "C:/keys/id_ed25519".into(),
            favorite: true,
        };
        vault.upsert(&host).await.unwrap();
        vault.set_password(3, Some("s3cret")).await.unwrap();
        // 元信息更新不得冲掉已存密码。
        let mut changed = host.clone();
        changed.name = "测试机★".into();
        vault.upsert(&changed).await.unwrap();
        assert_eq!(
            vault.password_of(3).await.unwrap().as_deref(),
            Some("s3cret")
        );
        let hosts = vault.load_hosts().await.unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].name, "测试机★");
        assert_eq!(hosts[0].port, 2222);
        assert_eq!(hosts[0].auth, AuthMethod::IdentityFile);
        assert!(hosts[0].favorite);
        assert_eq!(vault.count().await.unwrap(), 1);

        vault.set_setting("theme", "light").await.unwrap();
        assert_eq!(
            vault.setting("theme").await.unwrap().as_deref(),
            Some("light")
        );

        // known_hosts：首次 New → 一致 Matched → 不符 Mismatch（fail-closed）。
        let trust = vault
            .check_or_record_known_host("[example.com]:2222", "aaa", "ssh-ed25519")
            .await
            .unwrap();
        assert_eq!(trust, Trust::New);
        let trust = vault
            .check_or_record_known_host("[example.com]:2222", "aaa", "ssh-ed25519")
            .await
            .unwrap();
        assert_eq!(trust, Trust::Matched);
        let trust = vault
            .check_or_record_known_host("[example.com]:2222", "bbb", "ssh-ed25519")
            .await
            .unwrap();
        assert_eq!(trust, Trust::Mismatch);

        // 换密钥重开必须失败（页级加密真实生效），同密钥重开数据仍在。
        let wrong = Vault::open(&mode, "1f".repeat(32)).await;
        assert!(wrong.is_err(), "错误主密钥不应打开数据库");
        vault.remove(3).await.unwrap();
        assert_eq!(vault.count().await.unwrap(), 0);
        drop(vault);
        let reopened = Vault::open(&mode, hexkey).await.expect("重开失败");
        assert_eq!(reopened.load_hosts().await.unwrap().len(), 0);
        assert_eq!(
            reopened.setting("theme").await.unwrap().as_deref(),
            Some("light")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次性迁移：旧 hand-SQL vault.db（含密码）+ settings.json + known_hosts
    /// 全部进入新单文件库，旧文件改名 `.migrated` 保留。
    #[tokio::test]
    async fn legacy_files_migrate_into_new_vault() {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let tag: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("rulink-mig-test-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mode = StorageMode::Global(dir.clone());
        let hexkey = "0c".repeat(32);

        // --- 造旧 vault.db（turso 0.7 手写 SCHEMA，keyring 同款密钥） ---
        let legacy_path = dir.join(crate::paths::LEGACY_VAULT_FILE);
        {
            let db = Builder::new_local(legacy_path.to_string_lossy().as_ref())
                .experimental_encryption(true)
                .with_encryption(EncryptionOpts {
                    cipher: "aegis256".to_string(),
                    hexkey: hexkey.clone(),
                })
                .build()
                .await
                .unwrap();
            let conn = db.connect().unwrap();
            conn.execute(
                "CREATE TABLE IF NOT EXISTS hosts (
                    id INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    host TEXT NOT NULL,
                    port INTEGER NOT NULL,
                    user TEXT NOT NULL,
                    auth TEXT NOT NULL,
                    key_path TEXT NOT NULL DEFAULT '',
                    favorite INTEGER NOT NULL DEFAULT 0,
                    password TEXT
                )",
                (),
            )
            .await
            .unwrap();
            conn.execute(
                "INSERT INTO hosts(id, name, host, port, user, auth, key_path, favorite, password)
                 VALUES (7, '旧主机', 'old.example.com', 22, 'root', 'password', '', 1, 'old-pass')",
                (),
            )
            .await
            .unwrap();
        }
        std::fs::write(
            dir.join(crate::paths::LEGACY_SETTINGS_FILE),
            r#"{"theme":"light"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(crate::paths::LEGACY_KNOWN_HOSTS_FILE),
            "[old.example.com]:22 deadbeef ssh-ed25519\n",
        )
        .unwrap();

        // --- 新库 + 迁移 ---
        let vault = Vault::open(&mode, hexkey.clone()).await.unwrap();
        let migrated = migrate_legacy(&vault, &mode, &hexkey).await.unwrap();
        assert!(migrated);

        let hosts = vault.load_hosts().await.unwrap();
        assert_eq!(hosts.len(), 1);
        assert_eq!(hosts[0].id, 7);
        assert_eq!(hosts[0].name, "旧主机");
        assert!(hosts[0].favorite);
        assert_eq!(
            vault.password_of(7).await.unwrap().as_deref(),
            Some("old-pass")
        );
        assert_eq!(
            vault.setting("theme").await.unwrap().as_deref(),
            Some("light")
        );
        let trust = vault
            .check_or_record_known_host("[old.example.com]:22", "deadbeef", "ssh-ed25519")
            .await
            .unwrap();
        assert_eq!(trust, Trust::Matched);

        // 旧文件全部改名保留，绝不删除。
        assert!(!legacy_path.exists());
        assert!(dir.join("vault.db.migrated").exists());
        assert!(dir.join("settings.json.migrated").exists());
        assert!(dir.join("known_hosts.migrated").exists());
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn derive_key_is_deterministic_hex64() {
        let salt = [7u8; 16];
        let a = derive_key(&salt, "正确马电池").unwrap();
        let b = derive_key(&salt, "正确马电池").unwrap();
        assert_eq!(a, b);
        assert!(valid_hex_key(&a));
        assert_ne!(a, derive_key(&salt, "错误马电池").unwrap());
        assert_ne!(a, derive_key(&[8u8; 16], "正确马电池").unwrap());
    }

    #[test]
    fn legacy_known_hosts_line_parsing() {
        let entries = parse_legacy_known_hosts(
            "[host.example]:2207 abc123 ssh-ed25519\n[other]:22 def456\n坏行\n",
        );
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "[host.example]:2207");
        assert_eq!(entries[0].1, "abc123");
        assert_eq!(entries[0].2, "ssh-ed25519");
        assert_eq!(entries[1].2, "unknown");
    }
}
