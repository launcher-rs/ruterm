//! 命令片段库：名称 + 单行命令 + 可选备注，存加密库 `settings.snippets`。
//!
//! 数据只描述"发什么"；插入还是执行由界面决定（执行在末尾补一个回车）。
//! 校验文案拿不到 `cx`，统一给中文原文，界面展示前再经词条表翻译。

use serde::{Deserialize, Serialize};

/// 片段条数上限：列表可搜索，超出多半是误导入。
pub const MAX_SNIPPETS: usize = 500;
/// 名称长度上限（按字符计）。
pub const MAX_NAME_CHARS: usize = 80;
/// 单条命令上限：一条片段不该长时间占住终端输入通道。
pub const MAX_COMMAND_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSnippet {
    /// 展示名，搜索也匹配它。
    pub name: String,
    /// 原样写给终端的命令（单行）。
    pub command: String,
    /// 备注，仅用于检索与辨认，不发送。
    #[serde(default)]
    pub note: String,
}

impl CommandSnippet {
    /// 名称是否可用；留空时界面显示「未命名片段」，不把界面语言写进数据。
    pub fn has_name(&self) -> bool {
        !self.name.trim().is_empty()
    }
}

/// 校验并规范化表单输入。
pub fn validate(name: &str, command: &str, note: &str) -> Result<CommandSnippet, &'static str> {
    let name = name.trim();
    if name.is_empty() {
        return Err("片段名称不能为空");
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err("片段名称过长（最多 80 个字符）");
    }
    if command.trim().is_empty() {
        return Err("命令内容不能为空");
    }
    if command.contains(['\r', '\n']) {
        return Err("命令片段仅支持单行命令");
    }
    if command.contains('\0') {
        return Err("命令内容包含无法发送的字符");
    }
    if command.len() > MAX_COMMAND_BYTES {
        return Err("命令内容过长（最多 16 KB）");
    }
    Ok(CommandSnippet {
        name: name.to_string(),
        command: command.to_string(),
        note: note.trim().to_string(),
    })
}

/// 解析存储的 JSON；损坏或含坏条目时只保留可用片段。
pub fn from_json(text: &str) -> Vec<CommandSnippet> {
    let Ok(list) = serde_json::from_str::<Vec<CommandSnippet>>(text) else {
        return Vec::new();
    };
    list.into_iter()
        .filter(|snippet| {
            snippet.has_name()
                && !snippet.command.trim().is_empty()
                && !snippet.command.contains(['\r', '\n', '\0'])
                && snippet.command.len() <= MAX_COMMAND_BYTES
        })
        .take(MAX_SNIPPETS)
        .collect()
}

pub fn to_json(snippets: &[CommandSnippet]) -> String {
    serde_json::to_string(snippets).unwrap_or_else(|_| "[]".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_trims_labels_and_keeps_command() {
        let snippet =
            validate("  重启 nginx  ", " sudo systemctl restart nginx ", " 运维 ").unwrap();
        assert_eq!(snippet.name, "重启 nginx");
        assert_eq!(snippet.command, " sudo systemctl restart nginx ");
        assert_eq!(snippet.note, "运维");
    }

    #[test]
    fn validate_rejects_blank_multiline_and_oversized_inputs() {
        assert_eq!(validate(" ", "ls", "").unwrap_err(), "片段名称不能为空");
        assert_eq!(validate("x", "  ", "").unwrap_err(), "命令内容不能为空");
        assert_eq!(
            validate("x", "a\nb", "").unwrap_err(),
            "命令片段仅支持单行命令"
        );
        assert_eq!(
            validate("x", "a\rb", "").unwrap_err(),
            "命令片段仅支持单行命令"
        );
        assert_eq!(
            validate("x", "a\0b", "").unwrap_err(),
            "命令内容包含无法发送的字符"
        );
        assert_eq!(
            validate(&"x".repeat(MAX_NAME_CHARS + 1), "ls", "").unwrap_err(),
            "片段名称过长（最多 80 个字符）"
        );
        assert_eq!(
            validate("x", &"a".repeat(MAX_COMMAND_BYTES + 1), "").unwrap_err(),
            "命令内容过长（最多 16 KB）"
        );
    }

    #[test]
    fn json_roundtrip_drops_bad_entries() {
        let good = CommandSnippet {
            name: "看磁盘".into(),
            command: "df -h".into(),
            note: "容量".into(),
        };
        let list = vec![
            good.clone(),
            CommandSnippet {
                name: " ".into(),
                command: "rm -rf /".into(),
                note: "".into(),
            },
            CommandSnippet {
                name: "多行".into(),
                command: "echo a\necho b".into(),
                note: "".into(),
            },
        ];
        assert_eq!(from_json(&to_json(&list)), vec![good]);
        assert!(from_json("not json").is_empty());
        assert!(from_json("").is_empty());
    }

    #[test]
    fn from_json_caps_entry_count() {
        let list: Vec<_> = (0..MAX_SNIPPETS + 5)
            .map(|index| CommandSnippet {
                name: format!("s{index}"),
                command: "true".into(),
                note: String::new(),
            })
            .collect();
        assert_eq!(from_json(&to_json(&list)).len(), MAX_SNIPPETS);
    }
}
