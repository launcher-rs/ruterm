//! 更新配置：应用专属信息在此注入，版本比较 / 镜像回退 / 下载解压由 rgpui-extra 负责。
//!
//! 与 rumark 共用同一套组件，发布资产的命名也已对齐（ruterm-<平台>.zip / .tar.gz），
//! 换发布平台时只需改这里，UI 与下载流程不用动。

use rgpui_extra::updater::{Config, Provider};

/// 当前程序版本（来自 Cargo.toml，release 工作流会校验 tag 与此一致）。
pub(crate) const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 实测可用的 GitHub 镜像前缀（2026-11-25）。
///
/// 参考腾讯云开发者社区的两篇汇总及 FreeVaults 持续更新列表，逐个请求
/// `api.github.com/repos/launcher-rs/ruterm/releases/latest` 与 Windows 安装包
/// 做了验证：
/// - `gh-proxy.com` / `gh-proxy.org`：API 返回合法 Release JSON，安装包返回 206 + ZIP 头；
/// - `ghfast.top` / `ghproxy.net`：安装包返回 206 + ZIP 头，但 API 返回 403，
///   检查阶段会自动跳过并回落到直连，下载阶段仍可用。
///
/// 其余候选（ghproxy.homeboyc.cn / ghp.ci / moeyy.cn / ghproxy.com 等）
/// 当前返回 403 / 404 / TLS 握手失败，不写入默认配置。
pub(crate) const RECOMMENDED_MIRRORS: &[&str] = &[
    "https://gh-proxy.com",
    "https://gh-proxy.org",
    "https://ghfast.top",
    "https://ghproxy.net",
];

/// 发布源：正式版归档在 GitHub Release，Gitee 只是仓库镜像、不带 Release 附件。
pub(crate) fn updater_provider() -> Provider {
    Provider::GitHub
}

pub(crate) use rgpui_extra::updater::{
    DEFAULT_MIRRORS, DownloadProgress, InstallOptions, ReleaseAsset, check, download_and_stage,
    load_staged_update, open_release_page, run_blocking, spawn_install_helper,
};

/// 构造更新配置。默认直连，只有偏好里显式开启镜像才走中转。
pub(crate) fn updater_config(use_mirrors: bool, custom_mirrors: Vec<String>) -> Config {
    let mut config = Config::new("launcher-rs/ruterm", "ruterm");
    config.provider = updater_provider();
    config.user_agent = "ruterm-updater/1.0".to_string();
    config.use_mirrors = use_mirrors;
    config.custom_mirrors = custom_mirrors;
    config
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_points_to_ruterm_releases_and_defaults_to_direct() {
        let config = updater_config(false, vec!["https://mirror.example".into()]);
        assert_eq!(
            config.api_url(),
            "https://api.github.com/repos/launcher-rs/ruterm/releases/latest"
        );
        assert_eq!(
            config.releases_page(),
            "https://github.com/launcher-rs/ruterm/releases/latest"
        );
        assert_eq!(config.candidate_mirrors(), vec![String::new()]);
        assert_eq!(CURRENT_VERSION, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn config_mirror_list_prefers_custom_then_builtin() {
        let config = updater_config(true, vec!["https://mirror.example/".into()]);
        let mirrors = config.candidate_mirrors();
        assert_eq!(mirrors[0], "https://mirror.example");
        assert!(mirrors.iter().any(|m| m.is_empty()), "直连必须作为兜底");
        assert!(mirrors.len() > 2, "内置镜像也应进入候选");
    }

    #[test]
    fn recommended_mirrors_are_first_and_direct_is_last_fallback() {
        let expected: Vec<String> = RECOMMENDED_MIRRORS
            .iter()
            .map(|mirror| (*mirror).to_string())
            .collect();
        let config = updater_config(true, expected.clone());
        let candidates = config.candidate_mirrors();
        assert_eq!(&candidates[..expected.len()], expected.as_slice());
        assert_eq!(candidates.last().map(String::as_str), Some(""));
        // 内置镜像与推荐镜像重叠时不能重复请求。
        assert_eq!(
            candidates
                .iter()
                .filter(|m| *m == "https://ghfast.top")
                .count(),
            1
        );
    }

    #[test]
    fn config_asset_name_matches_release_artifacts() {
        let config = updater_config(false, vec![]);
        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        assert_eq!(config.asset_name(), "ruterm-windows-x86_64.zip");
        #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
        assert_eq!(config.asset_name(), "ruterm-linux-x86_64.tar.gz");
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(config.asset_name(), "ruterm-macos-aarch64.tar.gz");
    }
}
