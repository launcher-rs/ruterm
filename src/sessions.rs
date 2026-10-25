//! 主机账号管理：本地持久化的会话列表（~/.rulink/sessions.json）。
//!
//! 凭据策略：密码不入库，连接时临时输入；私钥只存路径。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 认证方式。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    /// SSH Agent（默认，免密免存）
    Agent,
    /// 私钥文件（存路径）
    IdentityFile,
    /// 密码（每次连接时弹窗输入，不落盘）
    Password,
}

/// 一台可连接的主机账号。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostConfig {
    /// 展示名
    pub name: String,
    /// 主机地址
    pub host: String,
    /// 端口
    pub port: u16,
    /// 登录用户
    pub user: String,
    /// 认证方式
    pub auth: AuthMethod,
    /// 私钥路径（仅 IdentityFile 使用）
    #[serde(default)]
    pub key_path: String,
    /// 是否收藏（收藏主机在列表置顶，★ 标记）
    #[serde(default)]
    pub favorite: bool,
}

impl HostConfig {
    /// 列表副标题（user@host:port）。
    pub fn addr(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    /// 表单字段基本校验，返回首个错误描述。
    pub fn validate(&self) -> Option<String> {
        if self.host.trim().is_empty() {
            return Some("主机地址不能为空".to_string());
        }
        if self.user.trim().is_empty() {
            return Some("用户名不能为空".to_string());
        }
        if self.port == 0 {
            return Some("端口无效".to_string());
        }
        if self.auth == AuthMethod::IdentityFile && self.key_path.trim().is_empty() {
            return Some("请填写私钥路径".to_string());
        }
        None
    }
}

/// 应用级设置（与主机列表分开存，避免动到 sessions.json 的数组格式）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// 主题模式："dark" / "light"；缺省按暗色。
    #[serde(default)]
    pub theme: String,
}

impl Settings {
    /// 是否暗色（非 "light" 一律视为暗色）。
    pub fn dark(&self) -> bool {
        self.theme != "light"
    }
}

/// 会话仓库：整表读写 JSON 文件。
pub struct SessionStore {
    path: PathBuf,
    /// 应用级设置文件路径（settings.json）。
    settings_path: PathBuf,
}

impl SessionStore {
    /// 使用 ~/.rulink/sessions.json 构造仓库。
    pub fn new() -> Self {
        let home = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        let dir = home.join(".rulink");
        Self {
            path: dir.join("sessions.json"),
            settings_path: dir.join("settings.json"),
        }
    }

    /// 读取应用级设置；文件缺失或损坏时返回默认值（暗色）。
    pub fn load_settings(&self) -> Settings {
        match std::fs::read_to_string(&self.settings_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// 保存主题选择（"dark" / "light"）；失败仅记日志。
    pub fn save_theme(&self, dark: bool) {
        let settings = Settings {
            theme: if dark { "dark" } else { "light" }.to_string(),
        };
        if let Some(parent) = self.settings_path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("无法创建 {}（{error}），设置不持久化", parent.display());
            return;
        }
        match serde_json::to_string_pretty(&settings) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&self.settings_path, text) {
                    log::warn!("写入 settings.json 失败: {error}");
                }
            }
            Err(error) => log::warn!("序列化设置失败: {error}"),
        }
    }

    /// 读取主机列表；文件缺失或损坏时返回空列表（损坏文件记日志保留现场）。
    pub fn load(&self) -> Vec<HostConfig> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
                log::warn!("sessions.json 解析失败（{error}），本次以空列表启动");
                Vec::new()
            }),
            Err(_) => Vec::new(),
        }
    }

    /// 整表写回；失败仅记日志（不阻断 UI 操作）。
    pub fn save(&self, hosts: &[HostConfig]) {
        if let Some(parent) = self.path.parent()
            && let Err(error) = std::fs::create_dir_all(parent)
        {
            log::warn!("无法创建 {}（{error}），会话不持久化", parent.display());
            return;
        }
        match serde_json::to_string_pretty(hosts) {
            Ok(text) => {
                if let Err(error) = std::fs::write(&self.path, text) {
                    log::warn!("写入 sessions.json 失败: {error}");
                }
            }
            Err(error) => log::warn!("序列化会话列表失败: {error}"),
        }
    }
}
