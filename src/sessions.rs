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
    /// 加密库中的主键（旧 JSON 数据迁移前为 0，入库时分配）
    #[serde(default)]
    pub id: i64,
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
    /// 私钥是否内嵌在加密库（不落 JSON；读取时按库内是否存在私钥回填）。
    #[serde(skip)]
    pub key_inline: bool,
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
        if self.auth == AuthMethod::IdentityFile
            && !self.key_inline
            && self.key_path.trim().is_empty()
        {
            return Some("请填写私钥路径，或改为内嵌私钥".to_string());
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
    /// 状态栏显示项偏好（序列化成 JSON 字符串；空串表示未设置）。
    #[serde(default)]
    pub status_bar: String,
    /// 终端外观偏好（字号 / 行高 / 配色方案，序列化成 JSON 字符串）。
    #[serde(default)]
    pub terminal: String,
    /// 会话行为偏好（自动重连 / 连接超时，序列化成 JSON 字符串）。
    #[serde(default)]
    pub session: String,
    /// 侧边栏分组（文件夹）偏好（序列化成 JSON 字符串）。
    #[serde(default)]
    pub sidebar: String,
    /// 本机 shell 自定义展示名（序列化成 JSON 字符串）。
    #[serde(default)]
    pub local_shells: String,
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
    /// 按存储模式构造旧 JSON 仓库（便携=data/，全局=~/.rulink；降级路径保留）。
    pub fn new(mode: &crate::paths::StorageMode) -> Self {
        let dir = mode.dir();
        Self {
            path: dir.join(crate::paths::LEGACY_SESSIONS_FILE),
            settings_path: dir.join(crate::paths::LEGACY_SETTINGS_FILE),
        }
    }

    /// 读取应用级设置；文件缺失或损坏时返回默认值（暗色）。
    pub fn load_settings(&self) -> Settings {
        match std::fs::read_to_string(&self.settings_path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Settings::default(),
        }
    }

    /// 就地改一项设置后整体回写（读-改-写，避免各设置项互相覆盖）。
    pub fn update_settings(&self, f: impl FnOnce(&mut Settings)) {
        let mut settings = self.load_settings();
        f(&mut settings);
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

    /// 保存主题选择（"dark" / "light"）；失败仅记日志。
    pub fn save_theme(&self, dark: bool) {
        self.update_settings(|settings| {
            settings.theme = if dark { "dark" } else { "light" }.to_string();
        });
    }

    /// 降级模式（无加密库）下保存状态栏偏好。
    pub fn save_status_bar(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.status_bar = json);
    }

    /// 降级模式（无加密库）下保存终端外观偏好。
    pub fn save_terminal(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.terminal = json);
    }

    /// 降级模式（无加密库）下保存会话行为偏好。
    pub fn save_session(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.session = json);
    }

    /// 降级模式（无加密库）下保存侧边栏分组偏好。
    pub fn save_sidebar(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.sidebar = json);
    }

    /// 降级模式（无加密库）下保存本机 shell 自定义名。
    pub fn save_local_shells(&self, json: &str) {
        let json = json.to_string();
        self.update_settings(|settings| settings.local_shells = json);
    }

    /// 迁移完成后把 sessions.json 改名为 sessions.json.migrated（保留现场，不删除）。
    pub fn mark_migrated(&self) {
        if !self.path.exists() {
            return;
        }
        let target = self.path.with_extension("json.migrated");
        if target.exists() {
            log::warn!(
                "{} 已存在，保留 {} 不重复改名",
                target.display(),
                self.path.display()
            );
            return;
        }
        if let Err(error) = std::fs::rename(&self.path, &target) {
            log::warn!("迁移 sessions.json 到加密库后改名失败: {error}");
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
