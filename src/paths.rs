//! 数据目录与存储模式判定：便携模式（exe 旁 `data/`）与全局模式（`~/.ruterm`）。
//!
//! 便携判定**只探测不创建**：`data/` 目录存在即便携，用户手动创建即切换；
//! 不存在则维持全局模式，绝不在别人机器的工作目录里悄悄落文件。
//! 所有本地存储路径（单文件库、盐、解锁标记、旧文件迁移）统一从 [`StorageMode`] 取。

use std::path::{Path, PathBuf};

/// 新版单文件加密库（toasty + turso 页级加密）。
pub const DB_FILE: &str = "ruterm.db";
/// argon2 主密码派生的随机盐（公开值，非秘密）。
pub const SALT_FILE: &str = "kdf-salt.bin";
/// 解锁后端标记（`keyring` / `passphrase`），非敏感，明文存放。
pub const UNLOCK_FILE: &str = "unlock.txt";
/// 界面语言启动镜像（`zh-CN` / `en`），非敏感；开库后以库内值为准。
pub const LOCALE_FILE: &str = "locale.txt";
/// 明暗主题启动镜像：加密库锁定时也能在首帧应用上次的界面主题。
pub const THEME_FILE: &str = "theme.txt";

/// 旧版存储文件名（一次性迁移的读取对象）。
pub const LEGACY_VAULT_FILE: &str = "vault.db";
pub const LEGACY_SESSIONS_FILE: &str = "sessions.json";
pub const LEGACY_SETTINGS_FILE: &str = "settings.json";
pub const LEGACY_KNOWN_HOSTS_FILE: &str = "known_hosts";

/// 存储模式：数据目录的位置由启动时探测一次性确定。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StorageMode {
    /// 便携模式：数据全部放 exe 同目录的 `data/` 下，可随 exe 拷贝。
    Portable(PathBuf),
    /// 全局模式：`~/.ruterm`（Windows 取 `%USERPROFILE%`，其他平台取 `$HOME`）。
    Global(PathBuf),
}

impl StorageMode {
    /// 探测存储模式：exe 旁存在 `data/` 目录 → 便携；否则全局。
    pub fn detect() -> Self {
        if let Some(portable) = portable_dir_if_exists() {
            return StorageMode::Portable(portable);
        }
        StorageMode::Global(global_dir())
    }

    /// 可启用的便携数据目录（exe 同目录的 `data/`）。
    pub fn portable_dir() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        Some(exe.parent()?.join("data"))
    }

    /// 数据目录。
    pub fn dir(&self) -> &Path {
        match self {
            StorageMode::Portable(dir) | StorageMode::Global(dir) => dir,
        }
    }

    /// 是否便携模式（便携模式强制主密码解锁）。
    pub fn is_portable(&self) -> bool {
        matches!(self, StorageMode::Portable(_))
    }

    /// 单文件库路径。
    pub fn db_path(&self) -> PathBuf {
        self.dir().join(DB_FILE)
    }

    /// argon2 盐路径。
    pub fn salt_path(&self) -> PathBuf {
        self.dir().join(SALT_FILE)
    }

    /// 解锁后端标记路径。
    pub fn unlock_marker_path(&self) -> PathBuf {
        self.dir().join(UNLOCK_FILE)
    }

    /// 旧文件路径（迁移读取 / 降级回退用）。
    pub fn legacy_path(&self, file: &str) -> PathBuf {
        self.dir().join(file)
    }

    /// 数据目录不存在时创建（首次落库前调用）。
    pub fn ensure_dir(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(self.dir())
    }
}

/// exe 同目录的 `data/`（仅当它已存在时返回）。
fn portable_dir_if_exists() -> Option<PathBuf> {
    let candidate = StorageMode::portable_dir()?;
    candidate.is_dir().then_some(candidate)
}

/// `~/.ruterm` 全局目录。
fn global_dir() -> PathBuf {
    let home = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".ruterm")
}
