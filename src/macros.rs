//! 键宏：把一段字节序列绑到组合键上，按一次等于替你逐字敲完。
//!
//! 模型（路线图「界面与交互 ⑦」）：名称 / 触发键（可空 = 只能从管理器执行）/
//! 步骤序列（文本 + 步后延迟毫秒）/ 启用开关 / 使用次数。原型没有录制功能，
//! 步骤一律手填，本模块只管数据与校验，界面见 `app/macros_ui.rs`。
//!
//! 约定：
//! - 触发键用 rgpui `Keystroke` 文法（`"ctrl-alt-1"`），经 `normalize_trigger`
//!   解析回 canonical 串存储；必须带 ctrl / alt（或 F 键），并避开应用已绑定
//!   与 shell 常用的组合（见 `RESERVED_TRIGGERS`）。
//! - 步骤文本按字面 UTF-8 发送，四个转义：`\n` = 回车（CR）、`\t` = Tab、
//!   `\e` = ESC、`\\` = 反斜杠本身；其余反斜杠原样保留。
//! - 执行复用命令广播的写入通道（`Terminal::input`，Programmatic 来源不会
//!   回流镜像中间件）；持久化进加密库 `settings` 表 `macros` 键，
//!   库不可用时降级 `settings.json`（`Settings::macros`）。

use rgpui::Keystroke;
use serde::{Deserialize, Serialize};

/// 应用已绑定（见 `main.rs` 的 `bind_keys`）、shell 常用控制字符与系统级
/// 组合键；键宏触发键不允许与之冲突，避免按下即抢走终端 / 系统的行为。
const RESERVED_TRIGGERS: &[&str] = &[
    // 应用快捷键（与 main.rs 的 bind_keys 对齐，新增全局键位记得同步这里）。
    "ctrl-b",
    "ctrl-shift-b",
    "ctrl-shift-l",
    "ctrl-shift-k",
    "ctrl-shift-r",
    "ctrl-t",
    "ctrl-n",
    "ctrl-shift-t",
    "ctrl-tab",
    "ctrl-shift-tab",
    "ctrl-pageup",
    "ctrl-pagedown",
    "ctrl-w",
    "ctrl-shift-w",
    "ctrl-f",
    "ctrl-shift-f",
    "ctrl-,",
    "ctrl-shift-d",
    "ctrl-shift-e",
    "ctrl-shift-m",
    "ctrl-shift-s",
    "ctrl-shift-h",
    // 终端上下文里被应用抢占的键（绑定分发先于宏匹配，收不到就永远不触发）。
    "ctrl-c",
    "ctrl-v",
    "ctrl-shift-c",
    "ctrl-shift-v",
    // 终端 / readline 常用控制字符，抢走会让 shell 没法用。
    "ctrl-a",
    "ctrl-d",
    "ctrl-e",
    "ctrl-h",
    "ctrl-i",
    "ctrl-j",
    "ctrl-k",
    "ctrl-l",
    "ctrl-m",
    "ctrl-o",
    "ctrl-p",
    "ctrl-q",
    "ctrl-r",
    "ctrl-s",
    "ctrl-u",
    "ctrl-y",
    "ctrl-z",
    // 系统级。
    "alt-f4",
    "alt-tab",
    "alt-escape",
];

/// 一步要发送的文本与发送完的等待毫秒数。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MacroStep {
    /// 字面文本（含 `\n` / `\t` / `\e` / `\\` 转义，执行时按 `to_bytes` 展开）。
    #[serde(default)]
    pub text: String,
    /// 发送完本步后等待的毫秒数（0 = 立即发下一步）。
    #[serde(default)]
    pub delay_ms: u64,
}

impl MacroStep {
    /// 按转义规则展开成实际发给终端的字节。
    pub fn to_bytes(&self) -> Vec<u8> {
        unescape_step(&self.text)
    }
}

/// 一条键宏。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyMacro {
    /// 展示名（列表与状态栏提示用，可重复）。
    #[serde(default)]
    pub name: String,
    /// 触发键 canonical 串（如 `ctrl-alt-1`）；None = 不绑键，仅从管理器执行。
    #[serde(default)]
    pub trigger: Option<String>,
    /// 禁用后触发键不拦截（管理器里仍可手动执行）。
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 使用次数（触发 / 手动执行都计一次，仅统计）。
    #[serde(default)]
    pub uses: u64,
    /// 按顺序发送的步骤。
    #[serde(default)]
    pub steps: Vec<MacroStep>,
}

fn default_true() -> bool {
    true
}

impl KeyMacro {
    /// 步数（列表「步数」列）。
    pub fn step_count(&self) -> usize {
        self.steps.len()
    }

    /// 该键宏是否命中这次按键；未绑键 / 已禁用一律不命中。
    pub fn matches_keystroke(&self, keystroke: &Keystroke) -> bool {
        if !self.enabled {
            return false;
        }
        let Some(trigger) = self
            .trigger
            .as_deref()
            .and_then(|raw| Keystroke::parse(raw).ok())
        else {
            return false;
        };
        // 与键映射同规则：修饰键与键名都相等才算命中（key_char 是 IME 细节，不参与）。
        trigger.modifiers == keystroke.modifiers && trigger.key == keystroke.key
    }
}

/// 校验并规范化触发键；空串 = 不绑键。
///
/// 返回 canonical 串（如 `ctrl-alt-1`）或给表单显示的原因。
pub fn normalize_trigger(raw: &str) -> Result<Option<String>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let keystroke =
        Keystroke::parse(raw).map_err(|_| "格式不对：应为 ctrl-alt-1 这样的组合键".to_string())?;
    // `Keystroke::parse` 对 "ctrl-" 这类残缺输入会给空键名，这里显式拦下。
    if keystroke.key.is_empty() {
        return Err("格式不对：应为 ctrl-alt-1 这样的组合键".into());
    }
    let canonical = canonical_trigger(&keystroke);

    // 只按 shift（或 fn）的普通键会在打字时误触发；F 键除外（本身不产字符）。
    let is_fkey = {
        let rest = &keystroke.key.as_bytes()[1..];
        keystroke.key.starts_with('f')
            && (1..=2).contains(&rest.len())
            && rest.iter().all(u8::is_ascii_digit)
    };
    if !is_fkey && !keystroke.modifiers.control && !keystroke.modifiers.alt {
        return Err("必须带 Ctrl 或 Alt（或用 F1–F12）".into());
    }
    if RESERVED_TRIGGERS.contains(&canonical.as_str()) {
        return Err("该组合已被应用或终端占用".into());
    }
    Ok(Some(canonical))
}

/// 触发键的存储形态：修饰键按固定顺序 + 小写键名（`Keystroke` 的 Display
/// 会按展示习惯把键名大写，不能当存储串用）。
pub fn canonical_trigger(keystroke: &Keystroke) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if keystroke.modifiers.control {
        parts.push("ctrl");
    }
    if keystroke.modifiers.alt {
        parts.push("alt");
    }
    if keystroke.modifiers.shift {
        parts.push("shift");
    }
    if keystroke.modifiers.platform {
        parts.push("cmd");
    }
    if keystroke.modifiers.function {
        parts.push("fn");
    }
    parts.push(&keystroke.key);
    parts.join("-")
}

/// 把整份键宏列表序列化成 JSON（存加密库 / settings.json / 导出共用）。
pub fn to_json(macros: &[KeyMacro]) -> String {
    serde_json::to_string_pretty(macros).unwrap_or_else(|_| "[]".to_string())
}

/// 解析整份键宏列表；空串 / 损坏 JSON 返回空表（坏宏由字段默认值兜底）。
pub fn from_json(text: &str) -> Vec<KeyMacro> {
    serde_json::from_str(text).unwrap_or_default()
}

/// 键宏总数上限：与片段库同思路，防一份异常备份把列表撑爆。
pub const MAX_MACROS: usize = 100;

/// 导入合并的统计：收下、触发键重复跳过、超出总数上限丢弃。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub accepted: usize,
    pub duplicates: usize,
    pub over_limit: usize,
}

/// 合并导入的键宏：触发键重复的跳过（运行时同名组合只有第一条生效，
/// 收下两条只会造成「保存成功却永远不触发」的困惑）；总数超上限的丢弃。
pub fn merge_imported(existing: Vec<KeyMacro>, imported: Vec<KeyMacro>) -> (Vec<KeyMacro>, ImportSummary) {
    let mut duplicates = 0usize;
    let mut accepted = Vec::new();
    let mut seen: std::collections::HashSet<String> = existing
        .iter()
        .filter_map(|macro_| macro_.trigger.clone())
        .collect();
    for macro_ in imported {
        let Some(trigger) = macro_.trigger.clone() else {
            accepted.push(macro_);
            continue;
        };
        if !seen.insert(trigger) {
            duplicates += 1;
            continue;
        }
        accepted.push(macro_);
    }
    let free = MAX_MACROS.saturating_sub(existing.len());
    let over_limit = accepted.len().saturating_sub(free);
    let accepted_count = accepted.len().min(free);
    let mut merged = existing;
    merged.extend(accepted.into_iter().take(free));
    (
        merged,
        ImportSummary {
            accepted: accepted_count,
            duplicates,
            over_limit,
        },
    )
}

/// 步骤文本 → 字节：`\n` = CR、`\t` = Tab、`\e` = ESC、`\\` = 反斜杠，
/// 未知转义原样保留（反斜杠 + 字符）。
pub fn unescape_step(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => out.push(b'\r'),
            Some('t') => out.push(b'\t'),
            Some('e') => out.push(0x1b),
            Some('\\') => out.push(b'\\'),
            Some(other) => {
                out.push(b'\\');
                let mut buf = [0u8; 4];
                out.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => out.push(b'\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(text: &str, delay_ms: u64) -> MacroStep {
        MacroStep {
            text: text.into(),
            delay_ms,
        }
    }

    #[test]
    fn unescape_step_maps_named_escapes_and_keeps_unknown() {
        assert_eq!(unescape_step("ls\\n"), b"ls\r");
        assert_eq!(unescape_step("a\\tb"), b"a\tb");
        assert_eq!(unescape_step("\\e[A"), b"\x1b[A");
        // 反斜杠自身与未知转义都原样保留，用户不会意外丢字。
        assert_eq!(unescape_step("C:\\\\path"), b"C:\\path");
        assert_eq!(unescape_step("\\x41"), b"\\x41");
        assert_eq!(unescape_step("尾随\\"), "尾随\\".as_bytes());
        assert_eq!(unescape_step("中文\\n"), "中文\r".as_bytes());
    }

    #[test]
    fn normalize_trigger_accepts_modified_and_fkeys() {
        assert_eq!(
            normalize_trigger("ctrl-alt-1").unwrap(),
            Some("ctrl-alt-1".into())
        );
        // 大写会被解析成 shift+小写；shift+alt 仍带 alt，允许。
        assert_eq!(normalize_trigger("alt-p").unwrap(), Some("alt-p".into()));
        assert_eq!(normalize_trigger("f5").unwrap(), Some("f5".into()));
        assert_eq!(
            normalize_trigger("ctrl-shift-f9").unwrap(),
            Some("ctrl-shift-f9".into())
        );
        // 空串 = 不绑键。
        assert_eq!(normalize_trigger("  ").unwrap(), None);
    }

    #[test]
    fn normalize_trigger_rejects_unsafe_and_reserved() {
        // 无 ctrl / alt 的普通键会在打字时误触发。
        assert!(normalize_trigger("a").is_err());
        assert!(normalize_trigger("shift-a").is_err());
        // 应用 / 终端 / 系统已占用的组合。
        assert!(normalize_trigger("ctrl-b").is_err());
        assert!(normalize_trigger("ctrl-c").is_err());
        assert!(normalize_trigger("alt-f4").is_err());
        assert!(
            normalize_trigger("ctrl-shift-m").is_err(),
            "管理器自己的键要保留"
        );
        // 键法不对（缺键名）。
        assert!(normalize_trigger("ctrl-").is_err());
    }

    #[test]
    fn macro_json_roundtrip_and_tolerate_bad_data() {
        let macros = vec![KeyMacro {
            name: "重启 nginx".into(),
            trigger: Some("ctrl-alt-r".into()),
            enabled: true,
            uses: 3,
            steps: vec![step("sudo systemctl restart nginx", 0), step("\\n", 200)],
        }];
        let restored = from_json(&to_json(&macros));
        assert_eq!(restored, macros);

        // 空串 / 损坏 JSON 回空表；缺字段按默认值补齐。
        assert_eq!(from_json(""), Vec::new());
        assert_eq!(from_json("{ not json"), Vec::new());
        let legacy: Vec<KeyMacro> =
            serde_json::from_str(r#"[{"name":"x"}]"#).expect("缺省字段应可解析");
        assert!(legacy[0].enabled);
        assert_eq!(legacy[0].trigger, None);
        assert!(legacy[0].steps.is_empty());
    }

    /// 触发键重复跳过、超上限丢弃、未绑键的照收。
    #[test]
    fn merge_imported_dedupes_by_trigger_and_caps_total() {
        let macro_ = |trigger: Option<&str>| KeyMacro {
            name: "m".into(),
            trigger: trigger.map(str::to_string),
            enabled: true,
            uses: 0,
            steps: Vec::new(),
        };
        let existing = vec![macro_(Some("ctrl-alt-1"))];
        let imported = vec![
            macro_(Some("ctrl-alt-1")), // 与现有重复：跳过。
            macro_(None),               // 未绑键：不参与判重，照收。
            macro_(Some("ctrl-alt-2")),
            macro_(Some("ctrl-alt-3")),
        ];
        let (merged, summary) = merge_imported(existing, imported);
        assert_eq!(summary.duplicates, 1);
        assert_eq!(summary.accepted, 3);
        assert_eq!(summary.over_limit, 0);
        let triggers: Vec<Option<String>> =
            merged.iter().map(|macro_| macro_.trigger.clone()).collect();
        assert_eq!(
            triggers,
            [
                Some("ctrl-alt-1".to_string()),
                None,
                Some("ctrl-alt-2".to_string()),
                Some("ctrl-alt-3".to_string()),
            ]
        );

        // 上限：现有 99 条、新来 2 条 → 收 1 丢 1。
        let existing: Vec<KeyMacro> = (0..MAX_MACROS - 1)
            .map(|index| macro_(Some(&format!("ctrl-alt-{index}"))))
            .collect();
        let (merged, summary) =
            merge_imported(existing, vec![macro_(None), macro_(Some("ctrl-alt-99"))]);
        assert_eq!(summary.accepted, 1);
        assert_eq!(summary.over_limit, 1);
        assert_eq!(merged.len(), MAX_MACROS);
    }

    #[test]
    fn matches_keystroke_requires_enabled_binding_and_exact_keys() {
        let keyctrlalt1 = Keystroke::parse("ctrl-alt-1").expect("合法键");
        let bound = KeyMacro {
            name: "m".into(),
            trigger: Some("ctrl-alt-1".into()),
            enabled: true,
            uses: 0,
            steps: vec![step("ls\\n", 0)],
        };
        assert!(bound.matches_keystroke(&keyctrlalt1));
        // 键同修饰不同、修饰同键不同都不命中。
        assert!(!bound.matches_keystroke(&Keystroke::parse("ctrl-1").unwrap()));
        assert!(!bound.matches_keystroke(&Keystroke::parse("ctrl-alt-2").unwrap()));
        // 禁用与未绑键的宏不命中。
        let disabled = KeyMacro {
            enabled: false,
            ..bound.clone()
        };
        assert!(!disabled.matches_keystroke(&keyctrlalt1));
        let unbound = KeyMacro {
            trigger: None,
            ..bound
        };
        assert!(!unbound.matches_keystroke(&keyctrlalt1));
    }
}
