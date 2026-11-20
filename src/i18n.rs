//! 界面文案多语言。
//!
//! 约定：**中文原文即翻译 key**。这样做的好处是未收录的词条会自动落回中文
//! （rgpui 的 `I18nManager::t` 找不到 key 就返回 key 本身），于是
//! 「翻译进度」与「可用性」解耦：漏翻只会露出中文，不会露出 `settings.title`
//! 这类占位符。英文词条表见 `assets/locales/en.json`（中文 → English，扁平一层）。
//!
//! 文案里的变量用命名占位符 `{name}`（rgpui 只支持命名替换，且 `format!`
//! 要求格式串是字面量），写法：`tr!(cx, "已断开 {title}", title = title)`。

use rgpui::App;
use rgpui::SharedString;
use rgpui::i18n::I18nManager;

/// 语言清单：(locale 代码, 该语言自署的名称)。
///
/// 名称恒用该语言本身书写，切语言时选项文字不跟着翻译，免得找不到自己要的语言。
pub const LANGUAGES: &[(&str, &str)] = &[("zh-CN", "简体中文"), ("en", "English")];

/// 基准语言：界面原文所用语言，也是词条缺失时的回退语言。
pub const BASE_LOCALE: &str = "zh-CN";

/// 英文 locale 代码（词条表按它注册）。
const EN_LOCALE: &str = "en";

/// 英文词条表，编译进二进制：发布包不需要额外带 locales 目录。
const EN_JSON: &str = include_str!("../assets/locales/en.json");

/// 安装全局 i18n 管理器并设定初始语言。
///
/// 必须在开窗口前调用一次：首帧就要按偏好语言渲染标题栏与菜单。
pub fn install(locale: &str, cx: &mut App) {
    let mut i18n = I18nManager::new(BASE_LOCALE).with_fallback(BASE_LOCALE);
    if let Err(error) = i18n.load_translations(EN_LOCALE, EN_JSON) {
        log::warn!("英文词条表解析失败，界面将保持中文: {error}");
    }
    i18n.set_locale(normalize(locale));
    cx.set_global(i18n);
}

/// 切换界面语言；尚未安装管理器时按新语言安装。
pub fn set_locale(locale: &str, cx: &mut App) {
    let locale = normalize(locale);
    if cx.try_global::<I18nManager>().is_some() {
        cx.global_mut::<I18nManager>().set_locale(locale);
    } else {
        install(locale, cx);
    }
}

/// 当前界面语言代码。
pub fn current_locale(cx: &App) -> &'static str {
    match cx.try_global::<I18nManager>() {
        // `set_locale` 只接受清单内的常量字符串，管理器里存的一定是其中之一。
        Some(i18n) => LANGUAGES
            .iter()
            .find(|(code, _)| *code == i18n.current_locale())
            .map(|(code, _)| *code)
            .unwrap_or(BASE_LOCALE),
        None => BASE_LOCALE,
    }
}

/// 把任意输入收敛到清单内的语言，未知代码按中文处理。
pub(crate) fn normalize(locale: &str) -> &'static str {
    LANGUAGES
        .iter()
        .find(|(code, _)| *code == locale)
        .map(|(code, _)| *code)
        .unwrap_or(BASE_LOCALE)
}

/// 取词条：命中当前语言表返回译文，否则原样返回 key（即中文）。
///
/// `cx` 传 `&App` 或 `&mut Context<T>` 都行（后者会向 `&App` 解引用）。
pub fn translate(cx: &App, key: &str, args: &[(&str, &str)]) -> SharedString {
    match cx.try_global::<I18nManager>() {
        Some(i18n) => SharedString::from(i18n.t(key, args)),
        None => SharedString::from(key),
    }
}

/// 取词宏：`tr!(cx, "中文原文")`，带变量时 `tr!(cx, "共 {n} 台", n = count)`。
///
/// 占位符名走 `ident`（与 `format!` 的 `名字 = 值` 写法一致），宏内用
/// `stringify!` 还原成 rgpui 需要的 `{名字}`。
#[macro_export]
macro_rules! tr {
    ($cx:expr, $key:literal) => {
        $crate::i18n::translate($cx, $key, &[])
    };
    ($cx:expr, $key:literal, $($name:ident = $value:expr),+ $(,)?) => {{
        let owned: Vec<String> = vec![$(format!("{}", $value)),*];
        let names: &[&str] = &[$(stringify!($name)),*];
        let args: Vec<(&str, &str)> = names
            .iter()
            .copied()
            .zip(owned.iter().map(|text| text.as_str()))
            .collect();
        $crate::i18n::translate($cx, $key, &args)
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_keeps_known_and_falls_back_to_base() {
        assert_eq!(normalize("en"), "en");
        assert_eq!(normalize("zh-CN"), "zh-CN");
        assert_eq!(normalize("fr"), BASE_LOCALE);
    }

    /// 词条表必须是「中文 key → 非空译文」且占位符两侧对齐，缺一条就会露半句英文。
    #[test]
    fn en_table_is_valid_and_placeholders_match() {
        let table: std::collections::BTreeMap<String, String> =
            serde_json::from_str(EN_JSON).expect("en.json 必须是扁平字符串表");
        for (key, value) in &table {
            assert!(!value.trim().is_empty(), "词条 {key:?} 译文为空");
            let extract = |text: &str| {
                let mut out = vec![];
                let mut rest = text;
                while let Some(start) = rest.find('{') {
                    let Some(end) = rest[start..].find('}') else {
                        break;
                    };
                    out.push(rest[start + 1..start + end].to_string());
                    rest = &rest[start + end + 1..];
                }
                out
            };
            let mut source = extract(key);
            let mut target = extract(value);
            source.sort();
            target.sort();
            assert_eq!(source, target, "词条 {key:?} 的占位符与译文不一致");
        }
    }

    /// 代码里每个 `tr!` / `translate` 的字符串 key 都必须被 en.json 收录：
    /// 漏一条就静默回退中文——英文界面直接露中文且无任何告警，只能靠扫源码兜住。
    ///
    /// 扫描规则：标记（`tr!(` / `translate(`）到第一个引号之间只允许出现
    /// 实参记号（cx / 逗号 / & / 标识符），否则视为「变量 key」跳过；
    /// 注释里的样例先剥离；i18n.rs 自身（含扫描器与文档样例）不扫。
    #[test]
    fn tr_keys_are_covered_by_en_table() {
        let table: std::collections::BTreeMap<String, String> =
            serde_json::from_str(EN_JSON).expect("en.json 必须是扁平字符串表");
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut sources = Vec::new();
        collect_rs_sources(&root, &mut sources);
        let mut missing: Vec<String> = Vec::new();
        for (path, text) in sources {
            for key in extract_tr_keys(&text) {
                if !table.contains_key(&key) {
                    missing.push(format!("{path}: {key}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "en.json 缺少以下 tr!/translate 词条：\n{}",
            missing.join("\n")
        );
    }

    /// 递归收集 src 下的 .rs 源码（跳过 i18n.rs：扫描器与文档样例都在那里），
    /// 并剥掉行注释（`//` 之后的内容），避免文档样例里的 `tr!(` 被误当成调用。
    fn collect_rs_sources(dir: &std::path::Path, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).expect("遍历 src 目录失败").flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_rs_sources(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && path.file_name().is_some_and(|name| name != "i18n.rs")
            {
                let text = std::fs::read_to_string(&path).expect("读取源码失败");
                let stripped = text
                    .lines()
                    .map(|line| match line.find("//") {
                        Some(at) => &line[..at],
                        None => line,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                out.push((path.display().to_string(), stripped));
            }
        }
    }

    /// 提取一段源码里所有 `tr!(` / `translate(` 标记后的首参字符串字面量。
    ///
    /// 标记前不能是标识符字符（`include_str!(` 里就藏着 `tr!(` 子串）；
    /// 字面量里的 `\\` 反转义成单个反斜杠（key 与 en.json 都是单反斜杠形态）。
    fn extract_tr_keys(text: &str) -> Vec<String> {
        let bytes = text.as_bytes();
        let mut keys = Vec::new();
        for marker in ["tr!(", "translate("] {
            let mut from = 0;
            while let Some(offset) = text[from..].find(marker) {
                let start = from + offset;
                let marker_end = start + marker.len();
                from = marker_end;
                // 前置边界：标记必须独立成词，不能是 `include_str!(` 这种子串。
                let boundary_ok = start == 0
                    || !bytes[start - 1].is_ascii_alphanumeric()
                    && bytes[start - 1] != b'_';
                if !boundary_ok {
                    continue;
                }
                let Some(quote) = text[marker_end..].find('"') else {
                    break;
                };
                let between = &text[marker_end..marker_end + quote];
                let plain_args = between.chars().all(|ch| {
                    ch.is_whitespace()
                        || ch.is_ascii_alphanumeric()
                        || matches!(ch, '_' | ',' | '&' | '\'')
                });
                if !plain_args {
                    // 首参不是字符串字面量（变量 / 表达式）：跳过这个调用。
                    continue;
                }
                let rest = &text[marker_end + quote + 1..];
                let Some(end) = rest.find('"') else {
                    break;
                };
                let raw = &rest[..end];
                // 只反转义 `\\`（key 里可能出现字面反斜杠，如「D:\ 换盘」）。
                let mut key = String::with_capacity(raw.len());
                let mut chars = raw.chars();
                while let Some(ch) = chars.next() {
                    if ch == '\\' && chars.clone().next() == Some('\\') {
                        chars.next();
                        key.push('\\');
                    } else {
                        key.push(ch);
                    }
                }
                keys.push(key);
            }
        }
        keys
    }
}
