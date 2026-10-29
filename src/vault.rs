//! 凭据保险库：主机配置、应用设置与 known_hosts 统一存放在单文件加密数据库
//! （toasty ORM + turso 页级加密 AEGIS-256）。
//!
//! 密码列另有**应用层字段加密**（AES-256-GCM）：密钥由主密钥域分离派生
//! （`SHA-256(主密钥 ‖ "ruterm-field-key-v1")`），每条密码随机 12 字节 nonce，
//! 列内存 `base64(nonce ‖ 密文 ‖ 标签)`。即使库文件页级加密被绕过或某个页面
//! 被明文导出，密码列依然是不可读的密文；库级加密负责整体防护，字段加密
//! 负责"最敏感的那一列"。
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

use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::paths::StorageMode;
use crate::sessions::{AuthMethod, HostConfig, ProtocolKind};
use anyhow::{Context as _, Result};
use argon2::Argon2;
use keyring::Entry;
use toasty::stmt::{List, Query};
use toasty_driver_turso::{EncryptionOpts, Turso};
use tokio::sync::Mutex;
use turso::{Builder, Value};

/// 凭据管理器中的服务名与账户名（keyring 档主密钥条目）。
const KEYRING_SERVICE: &str = "ruterm";
const KEYRING_ACCOUNT: &str = "vault-key";

/// 内嵌私钥的存储位：settings 表里的一行 JSON（host_id → 字段加密的 PEM）。
///
/// 不另开表：toasty 的建表语句不支持 `IF NOT EXISTS`，给已有库加表需要手工迁移；
/// 私钥本身按字段加密（与密码列同一套 AES-GCM），放在设置行里不降低保护强度。
pub const HOST_KEYS_SETTING: &str = "host_keys";

/// 新版单文件库：三张表全部由 toasty 建模，启动时 `push_schema` 幂等建表。
mod schema {
    use toasty::Model;

    use crate::sessions::{HostConfig, ProtocolKind};

    /// 主机行；password 列存字段加密后的密文（见模块头的说明），未保存则 NULL。
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
                protocol: ProtocolKind::Ssh,
                id: self.id,
                name: self.name.clone(),
                host: self.host.clone(),
                port: self.port.clamp(1, u16::MAX as i64) as u16,
                user: self.user.clone(),
                auth: crate::vault::auth_from_tag(&self.auth),
                key_path: self.key_path.clone(),
                key_inline: false,
                favorite: self.favorite,
                // 跳板机映射存在 settings 表，加载后由 UI 层回填。
                proxy_jump: None,
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

/// 密码列的应用层字段加密（AES-256-GCM）。
///
/// 密钥由主密钥**域分离**派生，与页级加密用的密钥不同；密文自带随机 nonce，
/// 同一密码两次写入的密文也不同。既不依赖库级加密是否真的生效，也不必
/// 担心单页导出泄露明文。
mod field {
    use aes_gcm::{
        Aes256Gcm, KeyInit, Nonce,
        aead::{Aead, consts::U12},
    };
    use anyhow::{Context as _, Result};
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use sha2::{Digest as _, Sha256};

    /// 派生字段密钥用的域分离标签（改动它等于换了一把钥匙，旧密文将解不开）。
    const FIELD_KEY_INFO: &[u8] = b"ruterm-field-key-v1";

    /// GCM nonce 长度（字节）。
    const NONCE_LEN: usize = 12;

    type FieldNonce = Nonce<U12>;

    /// 字节切片 → GCM nonce。
    fn nonce_of(bytes: &[u8]) -> Result<FieldNonce> {
        FieldNonce::try_from(bytes).map_err(|_| anyhow::anyhow!("nonce 长度不合法"))
    }

    /// 主密钥（十六进制串）→ 字段密钥（32 字节）。
    pub fn derive_field_key(hexkey: &str) -> Result<[u8; 32]> {
        anyhow::ensure!(
            super::valid_hex_key(hexkey),
            "主密钥格式无效（应为 64 位十六进制）"
        );
        let mut master = [0u8; 32];
        for (byte, pair) in master.iter_mut().zip(hexkey.as_bytes().chunks(2)) {
            *byte = u8::from_str_radix(std::str::from_utf8(pair).context("主密钥编码异常")?, 16)
                .context("主密钥十六进制解析失败")?;
        }
        let mut hash = Sha256::new();
        hash.update(master);
        hash.update(FIELD_KEY_INFO);
        let digest = hash.finalize();
        let mut key = [0u8; 32];
        key.copy_from_slice(&digest);
        Ok(key)
    }

    /// 明文密码 → 入库字符串（`base64(nonce ‖ 密文 ‖ 标签)`）。
    pub fn encrypt(field_key: &[u8; 32], plaintext: &str) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(field_key)
            .map_err(|error| anyhow::anyhow!("字段加密初始化失败: {error}"))?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).context("生成字段加密随机数失败")?;
        let mut blob = Vec::with_capacity(NONCE_LEN + plaintext.len() + 16);
        blob.extend_from_slice(&nonce);
        blob.extend(
            cipher
                .encrypt(&nonce_of(&nonce)?, plaintext.as_bytes())
                .map_err(|error| anyhow::anyhow!("密码字段加密失败: {error}"))?,
        );
        Ok(STANDARD.encode(blob))
    }

    /// 入库字符串 → 明文密码；密钥不符或数据被篡改一律 Err（绝不降级返回原样）。
    pub fn decrypt(field_key: &[u8; 32], stored: &str) -> Result<String> {
        let cipher = Aes256Gcm::new_from_slice(field_key)
            .map_err(|error| anyhow::anyhow!("字段加密初始化失败: {error}"))?;
        let blob = STANDARD
            .decode(stored)
            .map_err(|error| anyhow::anyhow!("密码字段格式无效: {error}"))?;
        anyhow::ensure!(blob.len() > NONCE_LEN + 16, "密码字段长度不足，疑为损坏");
        let (nonce, ciphertext) = blob.split_at(NONCE_LEN);
        let plaintext = cipher
            .decrypt(&nonce_of(nonce)?, ciphertext)
            .map_err(|_| anyhow::anyhow!("密码字段解密失败（主密钥不匹配或数据被篡改）"))?;
        String::from_utf8(plaintext).context("密码字段不是合法文本")
    }
}

/// 从凭据管理器读取 keyring 档主密钥；没有条目时生成随机密钥并写入。
///
/// 若已有条目但格式非法，直接报错而不是覆盖——覆盖会让旧库永久无法解密。
pub fn load_or_create_master_key() -> Result<String> {
    let entry = Entry::new(KEYRING_SERVICE, KEYRING_ACCOUNT).context("无法访问系统凭据管理器")?;
    match entry.get_password() {
        Ok(hex) if valid_hex_key(&hex) => Ok(hex),
        Ok(_) => anyhow::bail!(
            "系统凭据管理器中的 ruterm 主密钥条目格式无效，为避免旧库无法解密，已拒绝自动覆盖"
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

/// 生成新的派生盐（只生成，不落盘）。
///
/// 换主密码时必须等新库就位后才写盘：先覆盖盐的话，一旦搬迁失败，
/// 旧口令就再也派生不出旧密钥了。
pub fn generate_salt() -> Result<[u8; 16]> {
    let mut salt = [0u8; 16];
    getrandom::fill(&mut salt).context("生成派生盐失败")?;
    Ok(salt)
}

/// 写入派生盐（公开参数，明文存放）。
pub fn write_salt(path: &Path, salt: &[u8; 16]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, salt).with_context(|| format!("写入 {} 失败", path.display()))
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

/// 加密凭据库句柄。
///
/// toasty 的连接池（depool `Runtime::Tokio1` + 巡检 `tokio::spawn`）以及 turso 的
/// 异步 IO **都要求运行在 tokio 运行时上下文里**，而 rgpui 的 `cx.spawn` 把 future
/// 放在主线程轮询、`background_spawn` 放在自己的线程池上——两者都不是 tokio。
/// 因此句柄自带运行时 `Handle`，所有库操作统一 `spawn` 到运行时执行，
/// 调用方可以从任意上下文 await（russh 会话内即已在运行时上）。
pub struct Vault {
    db: Arc<Mutex<toasty::Db>>,
    rt: tokio::runtime::Handle,
    /// password 列的字段密钥（主密钥域分离派生，仅存在于进程内存）。
    field_key: [u8; 32],
}

/// 一次库操作：在运行时任务里取得库句柄。
type DbCell = Arc<Mutex<toasty::Db>>;

impl Vault {
    /// 用给定主密钥打开（或创建）mode 数据目录下的单文件加密库并建表。
    pub async fn open(
        mode: &StorageMode,
        hexkey: String,
        rt: tokio::runtime::Handle,
    ) -> Result<Self> {
        anyhow::ensure!(
            valid_hex_key(&hexkey),
            "主密钥格式无效（应为 64 位十六进制）"
        );
        let field_key = field::derive_field_key(&hexkey)?;
        mode.ensure_dir()
            .with_context(|| format!("无法创建 {}", mode.dir().display()))?;
        let path = mode.db_path().to_string_lossy().to_string();
        let db = rt
            .spawn(connect(path, hexkey, false))
            .await
            .context("凭据库任务意外中止")??;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            rt,
            field_key,
        })
    }

    /// 在指定文件路径打开加密库（更换主密码的暂存库 `ruterm.db.new` 用）。
    pub async fn open_at(path: &str, hexkey: String, rt: tokio::runtime::Handle) -> Result<Self> {
        anyhow::ensure!(
            valid_hex_key(&hexkey),
            "主密钥格式无效（应为 64 位十六进制）"
        );
        let field_key = field::derive_field_key(&hexkey)?;
        let db = rt
            .spawn(connect(path.to_string(), hexkey, true))
            .await
            .context("凭据库任务意外中止")??;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
            rt,
            field_key,
        })
    }

    /// 本库所属运行时（换库时把文件操作也派发上去用）。
    pub fn runtime(&self) -> tokio::runtime::Handle {
        self.rt.clone()
    }

    /// 把一段异步工作投递到运行时执行。
    async fn dispatch<R, F>(&self, f: F) -> Result<R>
    where
        F: Future<Output = Result<R>> + Send + 'static,
        R: Send + 'static,
    {
        self.rt
            .spawn(f)
            .await
            .context("凭据库任务意外中止（运行时可能已关闭）")?
    }

    /// 在一次运行时任务里持有库句柄执行操作。
    async fn job<R, F>(&self, make: impl FnOnce(DbCell) -> F) -> Result<R>
    where
        F: Future<Output = Result<R>> + Send + 'static,
        R: Send + 'static,
    {
        self.dispatch(make(self.db.clone())).await
    }

    /// 主机总数（测试与诊断用）。
    #[allow(dead_code)]
    pub async fn count(&self) -> Result<i64> {
        Ok(self.load_hosts().await?.len() as i64)
    }

    /// 读取全部主机（不含密码列；内嵌私钥只回填 `key_inline` 标记）。
    pub async fn load_hosts(&self) -> Result<Vec<HostConfig>> {
        self.job(|cell| async move {
            let mut db = cell.lock().await;
            let rows = Query::<List<HostRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("读取凭据库失败")?;
            let stored = stored_host_keys(&mut db).await?;
            Ok(rows
                .iter()
                .map(|row| {
                    let mut host = row.to_host_config();
                    host.key_inline = host.id != 0 && stored.contains_key(&host.id);
                    host
                })
                .collect())
        })
        .await
    }

    /// 读取全部内嵌私钥（PEM 明文，仅进程内存）。
    ///
    /// 单条解密失败只跳过该条（记日志）：一把坏钥匙不该让整个列表打不开。
    pub async fn load_host_keys(&self) -> Result<BTreeMap<i64, String>> {
        let field_key = self.field_key;
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let stored = stored_host_keys(&mut db).await?;
            let mut keys = BTreeMap::new();
            for (id, sealed) in stored {
                match field::decrypt(&field_key, &sealed) {
                    Ok(pem) => {
                        keys.insert(id, pem);
                    }
                    Err(error) => log::warn!("内嵌私钥 {id} 解密失败，已跳过: {error}"),
                }
            }
            Ok(keys)
        })
        .await
    }

    /// 写入或清除某主机的内嵌私钥（None 表示清除）。
    pub async fn set_host_key(&self, id: i64, pem: Option<&str>) -> Result<()> {
        let (pem, field_key) = (pem.map(str::to_string), self.field_key);
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let mut stored = stored_host_keys(&mut db).await?;
            match pem {
                Some(pem) => {
                    stored.insert(id, field::encrypt(&field_key, &pem)?);
                }
                None => {
                    stored.remove(&id);
                }
            }
            put_host_keys(&mut db, &stored).await
        })
        .await
    }

    /// 批量导入（一次性迁移用）。
    pub async fn import_hosts(&self, hosts: &[HostConfig]) -> Result<()> {
        for host in hosts {
            self.upsert(host).await?;
        }
        Ok(())
    }

    /// 保存主机配置；已保存的密码列保持原样（本方法永不改密码）。
    pub async fn upsert(&self, host: &HostConfig) -> Result<()> {
        let (host, field_key) = (host.clone(), self.field_key);
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let stored = fetch_host(&mut db, host.id, &field_key)
                .await?
                .and_then(|(_, secret)| secret);
            put_host(&mut db, &host, stored.as_deref(), &field_key).await
        })
        .await
    }

    /// 写入或清除某主机的保存密码（password=None 表示清除）。
    pub async fn set_password(&self, id: i64, password: Option<&str>) -> Result<()> {
        let (password, field_key) = (password.map(str::to_string), self.field_key);
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let Some((host, _)) = fetch_host(&mut db, id, &field_key).await? else {
                return Ok(());
            };
            put_host(&mut db, &host, password.as_deref(), &field_key).await
        })
        .await
    }

    /// 读取某主机已保存的密码（未保存则为 None）。
    pub async fn password_of(&self, id: i64) -> Result<Option<String>> {
        let field_key = self.field_key;
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            Ok(fetch_host(&mut db, id, &field_key)
                .await?
                .and_then(|(_, secret)| secret))
        })
        .await
    }

    /// 删除主机（连带其密码行与内嵌私钥）。
    pub async fn remove(&self, id: i64) -> Result<()> {
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            Query::<List<HostRow>>::all()
                .filter(HostRow::fields().id().eq(id))
                .delete()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("删除凭据库记录失败")?;
            let mut stored = stored_host_keys(&mut db).await?;
            if stored.remove(&id).is_some() {
                put_host_keys(&mut db, &stored).await?;
            }
            Ok(())
        })
        .await
    }

    /// 读取设置项（缺失为 None）。
    pub async fn setting(&self, name: &str) -> Result<Option<String>> {
        let name = name.to_string();
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            raw_setting(&mut db, &name).await
        })
        .await
    }

    /// 写入设置项（存在则覆盖）。
    pub async fn set_setting(&self, name: &str, value: &str) -> Result<()> {
        let (name, value) = (name.to_string(), value.to_string());
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            put_setting(&mut db, &name, &value).await
        })
        .await
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
        let (host_port, fingerprint, algorithm) = (
            host_port.to_string(),
            fingerprint.to_string(),
            algorithm.to_string(),
        );
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let existing = Query::<List<KnownHostRow>>::all()
                .filter(KnownHostRow::fields().host_port().eq(host_port.clone()))
                .first()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("读取 known_hosts 失败")?;
            match existing {
                Some(row) if row.fingerprint == fingerprint => Ok(Trust::Matched),
                Some(_) => Ok(Trust::Mismatch),
                None => put_known_host(&mut db, &host_port, &fingerprint, &algorithm)
                    .await
                    .map(|()| Trust::New),
            }
        })
        .await
    }

    /// 批量写入 known_hosts（一次性迁移与换库搬迁用，已存在则覆盖）。
    pub async fn record_known_hosts(&self, entries: Vec<(String, String, String)>) -> Result<()> {
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            for (host_port, fingerprint, algorithm) in entries {
                put_known_host(&mut db, &host_port, &fingerprint, &algorithm).await?;
            }
            Ok(())
        })
        .await
    }

    /// 导出本库全部数据（换主密钥时搬迁用）。
    pub async fn export_all(&self) -> Result<VaultDump> {
        let field_key = self.field_key;
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            let hosts = Query::<List<HostRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出主机失败")?;
            let mut dumped = Vec::with_capacity(hosts.len());
            for row in hosts.iter() {
                let secret = row
                    .password
                    .as_deref()
                    .map(|stored| field::decrypt(&field_key, stored))
                    .transpose()
                    .context("导出密码失败")?;
                dumped.push((row.to_host_config(), secret));
            }
            let settings = Query::<List<SettingRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出设置失败")?;
            let stored = stored_host_keys(&mut db).await?;
            let mut host_keys = Vec::with_capacity(stored.len());
            for (id, sealed) in stored {
                let pem = field::decrypt(&field_key, &sealed)
                    .with_context(|| format!("导出主机 {id} 的内嵌私钥失败"))?;
                host_keys.push((id, pem));
            }
            let known = Query::<List<KnownHostRow>>::all()
                .exec(&mut *db as &mut dyn toasty::Executor)
                .await
                .context("导出 known_hosts 失败")?;
            Ok(VaultDump {
                hosts: dumped,
                host_keys,
                // 内嵌私钥表另存为明文形态（上面已解密），原始密文行不随设置搬迁。
                settings: settings
                    .iter()
                    .filter(|s| s.name != HOST_KEYS_SETTING)
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
        })
        .await
    }

    /// 导入另一库导出的全部数据（换主密钥后写入新库）。
    pub async fn import_dump(&self, dump: &VaultDump) -> Result<()> {
        let (hosts, host_keys, settings, known_hosts, field_key) = (
            dump.hosts.clone(),
            dump.host_keys.clone(),
            dump.settings.clone(),
            dump.known_hosts.clone(),
            self.field_key,
        );
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            for (host, password) in &hosts {
                put_host(&mut db, host, password.as_deref(), &field_key).await?;
            }
            if !host_keys.is_empty() {
                // 换库必然换字段密钥：旧库里的密文搬过来解不开，必须用新密钥重加密。
                let mut stored = BTreeMap::new();
                for (id, pem) in &host_keys {
                    stored.insert(*id, field::encrypt(&field_key, pem)?);
                }
                put_host_keys(&mut db, &stored).await?;
            }
            for (name, value) in &settings {
                put_setting(&mut db, name, value).await?;
            }
            for (host_port, fingerprint, algorithm) in &known_hosts {
                put_known_host(&mut db, host_port, fingerprint, algorithm).await?;
            }
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
impl Vault {
    /// 口令列的入库原文（不经字段解密），测试用。
    async fn raw_password_column(&self, id: i64) -> Result<Option<String>> {
        self.job(move |cell| async move {
            let mut db = cell.lock().await;
            raw_password_of(&mut db, id).await
        })
        .await
    }
}

/// 打开（并按需建表）加密库——必须在 tokio 运行时任务内执行。
/// `fresh` 为 true 时直接建表（暂存新库），否则先探测 `hosts` 表是否已存在
/// （toasty 生成的 `CREATE TABLE` 不带 `IF NOT EXISTS`）。
async fn connect(path: String, hexkey: String, fresh: bool) -> Result<toasty::Db> {
    let driver = Turso::file(&path).experimental_encryption(EncryptionOpts {
        cipher: "aegis256".to_string(),
        hexkey,
    });
    let mut db = toasty::Db::builder()
        .models(toasty::models!(HostRow, SettingRow, KnownHostRow))
        .build(driver)
        .await
        .context("无法打开加密凭据库（主密码或库文件可能不正确）")?;
    if fresh {
        db.push_schema().await.context("新加密库建表失败")?;
        return Ok(db);
    }
    // 库能成功打开本身已证明密钥正确：查询报"缺表"才建表。
    let hosts_ready = Query::<List<HostRow>>::all()
        .exec(&mut db as &mut dyn toasty::Executor)
        .await
        .is_ok();
    if !hosts_ready {
        db.push_schema()
            .await
            .context("凭据库建表失败（若升级后仍报错，请检查库文件完整性）")?;
    }
    Ok(db)
}

/// 主机行落库（upsert by id；password=None 写 NULL）。
///
/// 传入的是**明文**口令，写库前就地字段加密；调用方拿不到密文，也就无从日志。
async fn put_host(
    db: &mut toasty::Db,
    host: &HostConfig,
    password: Option<&str>,
    field_key: &[u8; 32],
) -> Result<()> {
    let sealed = password
        .map(|plain| field::encrypt(field_key, plain))
        .transpose()?;
    HostRow::upsert_by_id(host.id)
        .name(host.name.as_str())
        .host(host.host.as_str())
        .port(host.port as i64)
        .user(host.user.as_str())
        .auth(auth_tag(host.auth))
        .key_path(host.key_path.as_str())
        .favorite(host.favorite)
        .password(sealed)
        .exec(db as &mut dyn toasty::Executor)
        .await
        .context("写入凭据库失败")?;
    Ok(())
}

/// 读取单行主机：配置 + 已解密口令（行不存在为 None）。
async fn fetch_host(
    db: &mut toasty::Db,
    id: i64,
    field_key: &[u8; 32],
) -> Result<Option<(HostConfig, Option<String>)>> {
    let row = host_row(db, id).await?;
    Ok(match row {
        Some(row) => {
            let secret = row
                .password
                .as_deref()
                .map(|stored| field::decrypt(field_key, stored))
                .transpose()?;
            Some((row.to_host_config(), secret))
        }
        None => None,
    })
}

/// 单行主机原始读取（口令列保持库中形态，不解密）。
async fn host_row(db: &mut toasty::Db, id: i64) -> Result<Option<HostRow>> {
    let row = Query::<List<HostRow>>::all()
        .filter(HostRow::fields().id().eq(id))
        .first()
        .exec(db as &mut dyn toasty::Executor)
        .await
        .context("读取主机行失败")?;
    Ok(row)
}

/// 测试用：直接读出口令列的入库字符串，用来证明列里绝不是明文。
#[cfg(test)]
async fn raw_password_of(db: &mut toasty::Db, id: i64) -> Result<Option<String>> {
    Ok(host_row(db, id).await?.and_then(|row| row.password))
}

async fn put_setting(db: &mut toasty::Db, name: &str, value: &str) -> Result<()> {
    SettingRow::upsert_by_name(name.to_string())
        .value(value)
        .exec(db as &mut dyn toasty::Executor)
        .await
        .context("写入设置失败")?;
    Ok(())
}

/// 设置行原始值（缺失为 None；不做任何解密）。
async fn raw_setting(db: &mut toasty::Db, name: &str) -> Result<Option<String>> {
    let row = Query::<List<SettingRow>>::all()
        .filter(SettingRow::fields().name().eq(name))
        .first()
        .exec(db as &mut dyn toasty::Executor)
        .await
        .context("读取设置失败")?;
    Ok(row.map(|row| row.value))
}

/// 内嵌私钥表（host_id → 字段密文）。
///
/// 表损坏时记日志并按空表处理：私钥解不出来应表现为"这台主机没有内嵌私钥"，
/// 而不是让整个主机列表/删除操作一起失败。
async fn stored_host_keys(db: &mut toasty::Db) -> Result<BTreeMap<i64, String>> {
    let Some(raw) = raw_setting(db, HOST_KEYS_SETTING).await? else {
        return Ok(BTreeMap::new());
    };
    if raw.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    match serde_json::from_str(&raw) {
        Ok(map) => Ok(map),
        Err(error) => {
            log::warn!("内嵌私钥表解析失败（{error}），本次按空表处理");
            Ok(BTreeMap::new())
        }
    }
}

async fn put_host_keys(db: &mut toasty::Db, keys: &BTreeMap<i64, String>) -> Result<()> {
    let text = serde_json::to_string(keys).context("序列化内嵌私钥表失败")?;
    put_setting(db, HOST_KEYS_SETTING, &text).await
}

async fn put_known_host(
    db: &mut toasty::Db,
    host_port: &str,
    fingerprint: &str,
    algorithm: &str,
) -> Result<()> {
    KnownHostRow::upsert_by_host_port(host_port.to_string())
        .fingerprint(fingerprint)
        .algorithm(algorithm)
        .exec(db as &mut dyn toasty::Executor)
        .await
        .context("写入 known_hosts 失败")?;
    Ok(())
}

/// 两库之间搬迁用的全量快照（仅存在于进程内存，含口令与内嵌私钥明文——字段
/// 解密后的原生形态；绝不写日志、绝不落盘）。
pub struct VaultDump {
    pub hosts: Vec<(HostConfig, Option<String>)>,
    /// 内嵌私钥（host_id → PEM 明文）。
    pub host_keys: Vec<(i64, String)>,
    pub settings: Vec<(String, String)>,
    pub known_hosts: Vec<(String, String, String)>,
}

// ---------------------------------------------------------------------------
// 旧文件一次性迁移
// ---------------------------------------------------------------------------

/// 用旧版 hand-SQL 加密库（keyring 主密钥）读出 hosts 行（含密码列）。
/// 库损坏/密钥不对返回 Err（调用方据此回退改名，不动数据）。
async fn read_legacy_vault(
    path: PathBuf,
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
                protocol: ProtocolKind::Ssh,
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
                key_inline: false,
                favorite: matches!(row.get_value(7), Ok(Value::Integer(v)) if v != 0),
                proxy_jump: None,
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
        // 新库里已有主机 = 上次已导入、只是旧库文件当时没改走（句柄未释放）：
        // 跳过导入，免得用旧数据覆盖用户这段时间的新增/修改，仅再试着归档。
        let already_imported = vault.count().await.unwrap_or(0) > 0;
        // 先读原路径、成功后才改名：中途失败时旧库原样保留，不破坏现场。
        // 旧库走 turso 直连，同样必须在运行时上下文里执行。
        let read = vault
            .dispatch(read_legacy_vault(
                legacy_db_path.clone(),
                keyring_hexkey.to_string(),
            ))
            .await;
        match read {
            Ok(hosts) => {
                if already_imported {
                    log::warn!("凭据库已有数据，跳过旧 vault.db 导入（仅归档旧文件）");
                } else {
                    for (host, password) in &hosts {
                        vault.upsert(host).await?;
                        if password.is_some() {
                            vault.set_password(host.id, password.as_deref()).await?;
                        }
                    }
                }
                archive_legacy(vault, &legacy_db_path);
                migrated_vault = !already_imported;
            }
            Err(error) => {
                // 旧库打不开（密钥不符/损坏）：不动文件，交由上层保持现有行为。
                log::warn!("读取旧 vault.db 失败: {error}");
                return Err(error);
            }
        }
    }

    // settings.json：theme / status_bar / terminal / session → Setting
    let settings_path = mode.legacy_path(crate::paths::LEGACY_SETTINGS_FILE);
    if settings_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&settings_path)
            && let Ok(settings) = serde_json::from_str::<crate::sessions::Settings>(&text)
        {
            vault.set_setting("theme", &settings.theme).await?;
            for (key, value) in [
                ("status_bar", &settings.status_bar),
                ("terminal", &settings.terminal),
                ("session", &settings.session),
                ("local_shells", &settings.local_shells),
            ] {
                if !value.is_empty() {
                    vault.set_setting(key, value).await?;
                }
            }
        }
        rename_preserving(&settings_path);
    }

    // known_hosts 文件 → 表
    let kh_path = mode.legacy_path(crate::paths::LEGACY_KNOWN_HOSTS_FILE);
    if kh_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&kh_path) {
            vault
                .record_known_hosts(parse_legacy_known_hosts(&text))
                .await?;
        }
        rename_preserving(&kh_path);
    }
    Ok(migrated_vault)
}

/// 旧文件归档：改名 `.migrated` 保留现场，**后台**重试。
///
/// turso 连接的回收由运行时异步完成，Windows 上句柄可能滞留数秒；前台等待会拖住
/// 开库后的首屏，故丢给运行时慢慢改。目标已存在时不动原文件；改不动也只保留原样，
/// 下次启动会再试（届时新库已有数据，导入会被跳过，不会覆盖用户的新改动）。
fn archive_legacy(vault: &Vault, path: &Path) {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    let target = path.with_file_name(format!("{file_name}.migrated"));
    if target.exists() {
        log::warn!(
            "{} 已存在，保留 {} 不重复改名",
            target.display(),
            path.display()
        );
        return;
    }
    let from = path.to_path_buf();
    let display = path.display().to_string();
    let background = vault.rt.clone();
    vault.rt.spawn(async move {
        // 最长约 30 秒：足够覆盖连接回收，又不必让用户等待。
        match rename_db_bundle(&background, from, target, 300).await {
            Ok(()) => log::info!("旧库已归档为 .migrated: {display}"),
            Err(error) => log::warn!(
                "迁移后改名 {display} 失败: {error}；数据已入库，若有程序（如数据库管理器）\
                 打开该文件，关闭后下次启动会自动继续归档"
            ),
        }
    });
}

/// 文件换名（带重试，每 100ms 一次）：turso/toasty 的连接由 tokio 运行时异步回收，
/// Windows 上句柄可能滞留片刻，立刻改名会撞上"文件被占用"；休眠必须在运行时内进行。
pub(crate) async fn rename_with_retry(
    rt: &tokio::runtime::Handle,
    from: PathBuf,
    to: PathBuf,
    attempts: u32,
) -> Result<(), String> {
    let (from_desc, to_desc) = (from.display().to_string(), to.display().to_string());
    let moved = rt
        .spawn(async move {
            let mut last: Option<std::io::Error> = None;
            for _ in 0..attempts {
                match std::fs::rename(&from, &to) {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        last = Some(error);
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
            Err(last.unwrap_or_else(|| std::io::Error::other("文件仍被占用")))
        })
        .await
        .map_err(|error| format!("换名任务意外中止: {error}"))?;
    moved.map_err(|error| format!("换名 {from_desc} → {to_desc} 失败: {error}"))
}

/// 连同 SQLite 的 `-wal` / `-shm` 边文件一起换名：边文件按主文件名配对，
/// 只挪走主文件会让旧库的 WAL 挂到新库上（密钥不同），必须整套搬走。
pub(crate) async fn rename_db_bundle(
    rt: &tokio::runtime::Handle,
    base: PathBuf,
    target: PathBuf,
    attempts: u32,
) -> Result<(), String> {
    // 先搬边文件再搬主文件：只要 WAL 还留在原处，新库就位后就会把旧库的 WAL
    // 当成自己的挂上去（页号对不上直接毁数据），所以边文件搬不动就整体作罢。
    let sides = ["-wal", "-shm"]
        .iter()
        .filter_map(|suffix| {
            let name = base.file_name()?.to_str()?;
            let side = base.with_file_name(format!("{name}{suffix}"));
            side.exists().then(|| {
                let target_name = target.file_name().and_then(|n| n.to_str()).unwrap_or("db");
                (
                    side,
                    target.with_file_name(format!("{target_name}{suffix}")),
                )
            })
        })
        .collect::<Vec<(PathBuf, PathBuf)>>();
    for (side, side_target) in sides {
        rename_with_retry(rt, side, side_target, attempts)
            .await
            .map_err(|error| format!("{error}（边文件未让位，主文件保持不动）"))?;
    }
    rename_with_retry(rt, base, target, attempts).await
}

/// 删除整套库文件（主文件 + WAL/SHM 边文件）。
///
/// 只用于已确认无用的残留：暂存库的上一轮草稿，或换密钥成功后的旧库备份。
pub(crate) fn remove_db_bundle(base: &Path) {
    let Some(name) = base.file_name().and_then(|n| n.to_str()) else {
        return;
    };
    for suffix in ["", "-wal", "-shm"] {
        let path = if suffix.is_empty() {
            base.to_path_buf()
        } else {
            base.with_file_name(format!("{name}{suffix}"))
        };
        if path.exists()
            && let Err(error) = std::fs::remove_file(&path)
        {
            log::warn!("删除 {} 失败: {error}", path.display());
        }
    }
}

/// 改名保留（同步版：用于从未被打开过的普通文本/JSON 文件）。
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

    /// 每次调用得到唯一临时目录名（测试结束自行删除）。
    fn temp_dir(prefix: &str) -> PathBuf {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        std::env::temp_dir().join(format!(
            "{prefix}-{}",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ))
    }

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
            "ruterm-vault-test-{}",
            suffix
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        let mode = StorageMode::Global(dir.clone());
        let hexkey = "0f".repeat(32);
        let rt = tokio::runtime::Handle::current();
        let vault = Vault::open(&mode, hexkey.clone(), rt.clone())
            .await
            .expect("打开加密库失败");

        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 3,
            name: "测试机".into(),
            host: "example.com".into(),
            port: 2222,
            user: "dev".into(),
            auth: AuthMethod::IdentityFile,
            key_path: "C:/keys/id_ed25519".into(),
            key_inline: false,
             favorite: true,
            proxy_jump: None,
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
        let wrong = Vault::open(&mode, "1f".repeat(32), rt.clone()).await;
        assert!(wrong.is_err(), "错误主密钥不应打开数据库");
        vault.remove(3).await.unwrap();
        assert_eq!(vault.count().await.unwrap(), 0);
        drop(vault);
        let reopened = Vault::open(&mode, hexkey, rt.clone())
            .await
            .expect("重开失败");
        assert_eq!(reopened.load_hosts().await.unwrap().len(), 0);
        assert_eq!(
            reopened.setting("theme").await.unwrap().as_deref(),
            Some("light")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 换主密钥的文件搬迁全流程（新建暂存库 → 全量导入 → 旧库让位 → 新库就位）：
    /// 主机、已存密码、设置与 known_hosts 必须一项不少——曾出现"设置主密码后主机消失"。
    #[tokio::test]
    async fn rekey_file_swap_preserves_everything() {
        let dir = temp_dir("ruterm-rekey-test");
        let mode = StorageMode::Global(dir.clone());
        let rt = tokio::runtime::Handle::current();
        let vault = Vault::open(&mode, "0a".repeat(32), rt.clone())
            .await
            .expect("打开旧密钥库失败");
        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 7,
            name: "搬迁机".into(),
            host: "10.0.0.7".into(),
            port: 22,
            user: "ops".into(),
            auth: AuthMethod::Password,
            key_path: String::new(),
            key_inline: false,
             favorite: false,
            proxy_jump: None,
        };
        vault.upsert(&host).await.unwrap();
        vault.set_password(7, Some("pw-7")).await.unwrap();
        vault.set_host_key(7, Some("INLINE-PEM-7")).await.unwrap();
        vault.set_setting("theme", "dark").await.unwrap();
        assert_eq!(
            vault
                .check_or_record_known_host("10.0.0.7:22", "fp-7", "ssh-ed25519")
                .await
                .unwrap(),
            Trust::New
        );

        let dump = vault.export_all().await.unwrap();
        let db_path = mode.db_path();
        let staging = db_path.with_file_name("ruterm.db.new");
        remove_db_bundle(&staging);
        let fresh = Vault::open_at(&staging.to_string_lossy(), "7d".repeat(32), rt.clone())
            .await
            .expect("建暂存库失败");
        fresh.import_dump(&dump).await.unwrap();
        drop(fresh);
        drop(vault);

        let backup = db_path.with_file_name("ruterm.db.keyring-bak");
        rename_db_bundle(&rt, db_path.clone(), backup.clone(), 100)
            .await
            .expect("旧库让位失败");
        rename_db_bundle(&rt, staging.clone(), db_path.clone(), 100)
            .await
            .expect("新库就位失败");

        let reopened = Vault::open(&mode, "7d".repeat(32), rt.clone())
            .await
            .expect("用新密钥重开失败");
        let hosts = reopened.load_hosts().await.unwrap();
        assert_eq!(hosts.len(), 1, "搬迁后主机数量不符");
        assert_eq!(hosts[0].id, 7);
        assert_eq!(hosts[0].name, "搬迁机");
        assert_eq!(
            reopened.password_of(7).await.unwrap().as_deref(),
            Some("pw-7"),
            "搬迁后密码丢失"
        );
        assert_eq!(
            reopened
                .load_host_keys()
                .await
                .unwrap()
                .get(&7)
                .map(String::as_str),
            Some("INLINE-PEM-7"),
            "搬迁后内嵌私钥丢失（换库必须用新字段密钥重加密）"
        );
        assert!(hosts[0].key_inline, "搬迁后内嵌私钥标记丢失");
        assert_eq!(
            reopened.setting("theme").await.unwrap().as_deref(),
            Some("dark")
        );
        assert_eq!(
            reopened
                .check_or_record_known_host("10.0.0.7:22", "fp-7", "ssh-ed25519")
                .await
                .unwrap(),
            Trust::Matched
        );
        // 新库确认无误后才清理备份与暂存残留。
        assert!(!staging.exists());
        remove_db_bundle(&backup);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 简易轮询执行器：刻意**不在** tokio 上下文里 poll future，
    /// 用来复现 rgpui 主线程（`cx.spawn`）await 凭据库操作的场景。
    fn bare_block_on<F: Future>(fut: F) -> F::Output {
        use std::task::{Context, Poll, Wake};

        struct ParkWaker(std::thread::Thread);

        impl Wake for ParkWaker {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }

        let waker = std::task::Waker::from(Arc::new(ParkWaker(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut fut = std::pin::pin!(fut);
        let start = std::time::Instant::now();
        loop {
            if let Poll::Ready(value) = fut.as_mut().poll(&mut cx) {
                return value;
            }
            assert!(
                start.elapsed() < std::time::Duration::from_secs(30),
                "凭据库操作迟迟没有进展"
            );
            std::thread::park_timeout(std::time::Duration::from_millis(20));
        }
    }

    /// 回归：库操作必须能在非 tokio 线程上 await（rgpui 的 `cx.spawn` 在主线程
    /// 轮询、`background_spawn` 在自己的线程池），否则撞上
    /// "there is no reactor running, must be called from the context of a Tokio 1.x runtime"。
    #[test]
    fn vault_operations_work_off_runtime() {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = rt.handle().clone();
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let tag: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("ruterm-offrt-test-{tag}"));
        let mode = StorageMode::Global(dir.clone());
        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 1,
            name: "离线机".into(),
            host: "h.example".into(),
            port: 22,
            user: "u".into(),
            auth: AuthMethod::Password,
            key_path: String::new(),
            key_inline: false,
             favorite: false,
            proxy_jump: None,
        };
        std::thread::scope(|scope| {
            scope.spawn(move || {
                assert!(
                    tokio::runtime::Handle::try_current().is_err(),
                    "测试线程本身不该有运行时上下文"
                );
                let opened = bare_block_on(async {
                    let vault = Vault::open(&mode, "0e".repeat(32), handle.clone()).await?;
                    vault.upsert(&host).await?;
                    vault.set_password(1, Some("pw")).await?;
                    vault.set_setting("theme", "light").await?;
                    let trust = vault
                        .check_or_record_known_host("[h.example]:22", "fp", "ssh-ed25519")
                        .await?;
                    anyhow::Ok((
                        vault.load_hosts().await?,
                        trust,
                        vault.password_of(1).await?,
                        vault.setting("theme").await?,
                    ))
                });
                let (hosts, trust, secret, theme) = opened.expect("非运行时上下文里操作凭据库失败");
                assert_eq!(hosts.len(), 1);
                assert_eq!(hosts[0].name, "离线机");
                assert_eq!(trust, Trust::New);
                assert_eq!(secret.as_deref(), Some("pw"));
                assert_eq!(theme.as_deref(), Some("light"));
            });
        });
        drop(rt);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一次性迁移：旧 hand-SQL vault.db（含密码）+ settings.json + known_hosts
    /// 全部进入新单文件库，旧文件改名 `.migrated` 保留。
    #[tokio::test]
    async fn legacy_files_migrate_into_new_vault() {
        let mut suffix = [0u8; 8];
        getrandom::fill(&mut suffix).unwrap();
        let tag: String = suffix.iter().map(|b| format!("{b:02x}")).collect();
        let dir = std::env::temp_dir().join(format!("ruterm-mig-test-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        let mode = StorageMode::Global(dir.clone());
        let hexkey = "0c".repeat(32);
        let rt = tokio::runtime::Handle::current();

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
        let vault = Vault::open(&mode, hexkey.clone(), rt.clone())
            .await
            .unwrap();
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

        // 旧文件全部改名保留，绝不删除。旧库归档在后台重试（句柄回收是异步的），
        // 这里轮询等待它完成。
        for _ in 0..100 {
            if dir.join("vault.db.migrated").exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
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
    fn field_key_is_derived_and_domain_separated() {
        let hexkey = "0123456789abcdef".repeat(4);
        let key = field::derive_field_key(&hexkey).unwrap();
        // 同一主密钥恒定派生出同一把字段密钥。
        assert_eq!(key, field::derive_field_key(&hexkey).unwrap());
        // 与页级加密密钥不同（域分离），换主密钥则换字段密钥。
        assert_ne!(&key[..], &hexkey.as_bytes()[..32]);
        assert_ne!(key, field::derive_field_key(&"00".repeat(32)).unwrap());
        assert!(field::derive_field_key("不是十六进制").is_err());
    }

    #[test]
    fn secret_field_encrypt_roundtrip_and_tamper_checks() {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let key = field::derive_field_key(&"0a".repeat(32)).unwrap();
        let other = field::derive_field_key(&"0b".repeat(32)).unwrap();
        let plain = "s3cret-密码";

        let sealed = field::encrypt(&key, plain).unwrap();
        // 列里存的既不是明文，也不是可辨识的 base64 明文片段。
        assert!(!sealed.contains(plain));
        assert!(!sealed.contains("s3cret"));
        assert_eq!(field::decrypt(&key, &sealed).unwrap(), plain);
        // 每次写入随机 nonce：同一口令两次密文不同，杜绝"两行相等=同密码"的旁路。
        assert_ne!(sealed, field::encrypt(&key, plain).unwrap());
        // 换钥匙、篡改字节、非法编码都必须失败，绝不返回原样或空串。
        assert!(field::decrypt(&other, &sealed).is_err());
        let mut bytes = STANDARD.decode(&sealed).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 0x01;
        assert!(field::decrypt(&key, &STANDARD.encode(bytes)).is_err());
        assert!(field::decrypt(&key, "短短短短").is_err());
        assert!(field::decrypt(&key, "!!!不是base64!!!").is_err());
    }

    /// 端到端：口令列落库的是密文，读出来的是明文；换主密钥搬迁后依旧如此。
    #[tokio::test]
    async fn password_column_never_holds_plaintext() {
        let dir = temp_dir("ruterm-field-test");
        let mode = StorageMode::Global(dir.clone());
        let rt = tokio::runtime::Handle::current();
        let old_key = "11".repeat(32);
        let vault = Vault::open(&mode, old_key.clone(), rt.clone())
            .await
            .unwrap();
        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 5,
            name: "密文机".into(),
            host: "5.example".into(),
            port: 22,
            user: "u".into(),
            auth: AuthMethod::Password,
            key_path: String::new(),
            key_inline: false,
             favorite: false,
            proxy_jump: None,
        };
        vault.upsert(&host).await.unwrap();
        vault.set_password(5, Some("明文口令")).await.unwrap();
        let stored = vault
            .raw_password_column(5)
            .await
            .unwrap()
            .expect("口令列应有值");
        assert!(!stored.contains("明文口令"));
        assert_eq!(
            vault.password_of(5).await.unwrap().as_deref(),
            Some("明文口令")
        );

        // 换主密钥 = 换字段密钥：必须解密后重新加密，旧密文带到新库解不开。
        let dump = vault.export_all().await.unwrap();
        let new_key = "22".repeat(32);
        let staging = mode.db_path().with_file_name("ruterm.db.new");
        remove_db_bundle(&staging);
        let fresh = Vault::open_at(&staging.to_string_lossy(), new_key.clone(), rt.clone())
            .await
            .unwrap();
        fresh.import_dump(&dump).await.unwrap();
        let fresh_stored = fresh.raw_password_column(5).await.unwrap().unwrap();
        assert!(!fresh_stored.contains("明文口令"));
        assert_ne!(fresh_stored, stored);
        assert_eq!(
            fresh.password_of(5).await.unwrap().as_deref(),
            Some("明文口令")
        );
        drop(fresh);
        drop(vault);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 内嵌私钥：读写往返、库里只有密文、标记随库回填、删除主机连带清除。
    #[tokio::test]
    async fn inline_key_roundtrip_never_plaintext() {
        let dir = temp_dir("ruterm-inline-key-test");
        let mode = StorageMode::Global(dir.clone());
        let rt = tokio::runtime::Handle::current();
        let vault = Vault::open(&mode, "0b".repeat(32), rt.clone())
            .await
            .expect("打开库失败");
        let host = HostConfig {
            protocol: ProtocolKind::Ssh,
            id: 9,
            name: "内嵌私钥机".into(),
            host: "9.example".into(),
            port: 22,
            user: "u".into(),
            auth: AuthMethod::IdentityFile,
            key_path: String::new(),
            key_inline: false,
             favorite: false,
            proxy_jump: None,
        };
        vault.upsert(&host).await.unwrap();

        let pem = "-----BEGIN OPENSSH PRIVATE KEY-----\nMARKER-INLINE-KEY-9\n-----END OPENSSH PRIVATE KEY-----";
        vault.set_host_key(9, Some(pem)).await.unwrap();
        assert_eq!(
            vault
                .load_host_keys()
                .await
                .unwrap()
                .get(&9)
                .map(String::as_str),
            Some(pem)
        );
        assert!(
            vault.load_hosts().await.unwrap()[0].key_inline,
            "有内嵌私钥的主机应标记 key_inline"
        );
        // 设置行里存的必须是字段密文。
        let raw = vault
            .setting(HOST_KEYS_SETTING)
            .await
            .unwrap()
            .expect("应有 host_keys 行");
        assert!(!raw.contains("MARKER-INLINE-KEY-9"));

        // 清除后标记跟着回落。
        vault.set_host_key(9, None).await.unwrap();
        assert!(vault.load_host_keys().await.unwrap().is_empty());
        assert!(!vault.load_hosts().await.unwrap()[0].key_inline);

        // 删除主机时内嵌私钥一并清掉，不留孤儿密文。
        vault.set_host_key(9, Some(pem)).await.unwrap();
        vault.remove(9).await.unwrap();
        assert!(vault.load_host_keys().await.unwrap().is_empty());
        drop(vault);
        let _ = std::fs::remove_dir_all(&dir);
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
