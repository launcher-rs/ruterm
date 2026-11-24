//! 命令历史：只收终端回显出的命令行，新的在前；整体序列化成 JSON 存库。
//!
//! 来源是 rgpui-term 的 block 追踪（回车时从网格取当前行并剥掉提示符），
//! 因此无回显的密码输入、vim 里敲的回车都不会被当成命令。

use serde::{Deserialize, Serialize};

/// 最多保留多少条：再多的历史列表和库内 JSON 都开始碍事。
pub const MAX_HISTORY: usize = 300;
/// 单条命令长度上限，防止误粘贴整篇日志占满设置项。
pub const MAX_COMMAND_BYTES: usize = 4096;

/// 命令历史（新 → 旧）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandHistory {
    entries: Vec<String>,
}

impl CommandHistory {
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 记一条命令：与已有条目相同就移到最前，返回是否真的有变化。
    pub fn push(&mut self, command: &str) -> bool {
        let Some(command) = sanitize(command) else {
            return false;
        };
        if self.entries.first() == Some(&command) {
            return false;
        }
        self.entries.retain(|entry| entry != &command);
        self.entries.insert(0, command);
        self.entries.truncate(MAX_HISTORY);
        true
    }

    /// 删除一条；下标越界返回 false。
    pub fn remove(&mut self, index: usize) -> bool {
        if index >= self.entries.len() {
            return false;
        }
        self.entries.remove(index);
        true
    }

    /// 清空历史；本来就是空的时候返回 false。
    pub fn clear(&mut self) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        self.entries.clear();
        true
    }

    /// 序列化；JSON 里同样新的在前。
    pub fn to_json(&self) -> String {
        serde_json::to_string(&self.entries).unwrap_or_default()
    }

    /// 明文降级路径专用序列化：先剔除疑似内联凭据的条目。
    ///
    /// 加密库里有页级 + 字段两层加密，内联了密码的命令可以放心保留；但降级
    /// 落到 settings.json 是明文，宁可历史断档也不把密文变明文。判定刻意
    /// 从宽（这是兜底不是分类器），误杀 `-p 2222` 这类端口写法可接受——
    /// 真正常用且无害的命令下次照常进历史。
    pub fn to_json_scrubbed(&self) -> String {
        let scrubbed: Vec<&String> = self
            .entries
            .iter()
            .filter(|entry| !looks_like_embedded_secret(entry))
            .collect();
        serde_json::to_string(&scrubbed).unwrap_or_default()
    }

    /// 解析库内 / settings.json 的历史；坏数据与不合格条目直接丢弃。
    pub fn from_json(text: &str) -> Self {
        let parsed: Vec<String> = serde_json::from_str(text).unwrap_or_default();
        let mut history = Self::default();
        for command in parsed {
            let Some(command) = sanitize(&command) else {
                continue;
            };
            if history.entries.contains(&command) {
                continue;
            }
            history.entries.push(command);
            if history.entries.len() >= MAX_HISTORY {
                break;
            }
        }
        history
    }
}

/// 过滤空行、控制字符、超长命令、密码提示回显与纯提示符行。
fn sanitize(command: &str) -> Option<String> {
    let command = command.trim();
    if command.is_empty() || command.len() > MAX_COMMAND_BYTES {
        return None;
    }
    // 行内 Tab 是合法 shell 输入（对齐的多列粘贴、`awk\t'{print}'`），
    // 不该因为它是控制字符就丢掉；回车 / ESC / NUL 这类照旧拒绝。
    if command.chars().any(|c| c.is_control() && c != '\t') {
        return None;
    }
    if looks_like_password_prompt(command) || looks_like_prompt(command) {
        return None;
    }
    Some(command.to_string())
}

/// 没敲命令就按下回车时，block 追踪可能把整行提示符当成命令
///（如 `(base) root@debian:~#`）；这类行收进历史只是噪音。
fn looks_like_prompt(command: &str) -> bool {
    let Some(last) = command.chars().last() else {
        return false;
    };
    if last == '❯' || last == '➜' {
        return true;
    }
    if !matches!(last, '$' | '#' | '%' | '>') {
        return false;
    }
    // bash / zsh 的 `user@host:path$` / `root@host:~#`。
    if let Some(token) = command.split_whitespace().last()
        && token.contains('@')
        && token.contains(':')
    {
        return true;
    }
    // PowerShell 的 `PS C:\Users>`。
    command.starts_with("PS ") && last == '>'
}

/// `sudo` 之类的密码提示没有回显，block 里只会留下提示符本身，收进历史徒增噪音。
fn looks_like_password_prompt(command: &str) -> bool {
    let lower = command.to_lowercase();
    (lower.ends_with(':') || lower.ends_with('：'))
        && (lower.contains("password")
            || lower.contains("passphrase")
            || command.contains("密码")
            || command.contains("口令"))
}

/// 疑似「命令行里直接带了凭据」：`mysql -uroot -p'xx'`、`--password=xx`、
/// `PGPASSWORD=xx psql`、`sshpass -p xx`、`http://user:pass@host`。
/// 仅供明文降级序列化兜底，判定从宽。
fn looks_like_embedded_secret(command: &str) -> bool {
    let lower = command.to_lowercase();
    let tokens: Vec<&str> = lower.split_whitespace().collect();
    for token in &tokens {
        // 环境变量 / 长选项式：VAR=值，VAR 以 password/passwd/pwd/token/secret 结尾
        //（PGPASSWORD=、MYSQL_PWD=、--password=、GIT_TOKEN=…）；等号后必须有值。
        if let Some((name, value)) = token.split_once('=')
            && !value.is_empty()
            && (name.ends_with("password")
                || name.ends_with("passwd")
                || name.ends_with("pwd")
                || name.ends_with("token")
                || name.ends_with("secret"))
        {
            return true;
        }
        // 同 token 内联的短选项：`-p'xx'` / `-pxx`。`ssh -p 2222` 这类端口写法
        // -p 是独立 token，落不到这里；`-p2222` 会被误杀，可接受。
        if token.len() > 2 && token.starts_with("-p") && !token.starts_with("--") {
            return true;
        }
        // URL 内嵌凭据：scheme://user:pass@host（RFC 3986 userinfo）。
        if let Some((_, rest)) = token.split_once("://")
            && let Some(at) = rest.split('/').next().and_then(|a| a.rfind('@'))
            && rest[..at].contains(':')
        {
            return true;
        }
    }
    // 分写的 `sshpass -p xx`：-p 与密码各占一个 token。
    tokens.windows(2).any(|pair| {
        (pair[0] == "sshpass" || pair[0].ends_with("sshpass")) && pair[1].starts_with("-p")
    })
}

#[cfg(test)]
mod tests {
    use super::{CommandHistory, MAX_COMMAND_BYTES, MAX_HISTORY};

    #[test]
    fn push_trims_dedupes_and_moves_to_front() {
        let mut history = CommandHistory::default();
        assert!(history.push("  ls -la  "));
        assert!(history.push("df -h"));
        assert_eq!(history.entries(), ["df -h", "ls -la"]);

        // 再执行旧命令：移到最前，而不是留下两份。
        assert!(history.push("ls -la"));
        assert_eq!(history.entries(), ["ls -la", "df -h"]);
        // 与最新一条相同：不产生变化。
        assert!(!history.push("ls -la"));
        // 空行不做记录。
        assert!(!history.push("   "));
    }

    #[test]
    fn push_rejects_control_chars_and_password_prompts() {
        let mut history = CommandHistory::default();
        assert!(!history.push("echo hi\x1b[D"));
        assert!(!history.push("Password:"));
        assert!(!history.push("[sudo] password for user:"));
        assert!(!history.push("请输入密码："));
        // 行尾的回车 / 空格只是被裁掉，不算控制字符。
        assert!(history.push("echo hi\r\n"));
        assert_eq!(history.entries(), ["echo hi"]);
        // 正常命令里出现 password 不受影响。
        assert!(history.push("grep password /etc/app.conf"));
        assert_eq!(history.entries().len(), 2);
    }

    #[test]
    fn history_is_capped() {
        let mut history = CommandHistory::default();
        for index in 0..(MAX_HISTORY + 20) {
            history.push(&format!("echo {index}"));
        }
        assert_eq!(history.entries().len(), MAX_HISTORY);
        assert_eq!(history.entries()[0], format!("echo {}", MAX_HISTORY + 19));
    }

    #[test]
    fn prompt_only_lines_are_rejected() {
        let mut history = CommandHistory::default();
        assert!(!history.push("(base) root@debian:~#"));
        assert!(!history.push("user@host:/var/log$"));
        assert!(!history.push("PS C:\\Users\\user>"));
        assert!(!history.push("❯"));
        assert!(history.is_empty());

        // 真正的命令照常记录：@ / : 只出现在参数里、行尾不是提示符。
        assert!(history.push("ssh root@host"));
        assert!(history.push("scp ./app root@host:/opt/app"));
        assert!(history.push("git log --format='%h %s'"));
        assert_eq!(history.entries().len(), 3);
    }

    #[test]
    fn long_commands_are_rejected() {
        let mut history = CommandHistory::default();
        assert!(!history.push(&"x".repeat(MAX_COMMAND_BYTES + 1)));
        assert!(history.push(&"x".repeat(MAX_COMMAND_BYTES)));
    }

    #[test]
    fn scrubbed_json_drops_commands_with_embedded_secrets() {
        let mut history = CommandHistory::default();
        for command in [
            "mysql -uroot -p's3cret' -h db",
            "pg_dump --password=abc postgres://u@h/d",
            "PGPASSWORD=abc psql -h db",
            "GIT_TOKEN=abc git push",
            "sshpass -p hunter2 scp f host:/f",
            "curl http://user:pw@example.com/x",
            // 端口 / 普通词 / 无凭据命令不误杀。
            "ssh -p 2222 root@host",
            "grep password /etc/app.conf",
            "mysql -h db -P 3306",
            "ls -la",
        ] {
            history.push(command);
        }
        let restored = CommandHistory::from_json(&history.to_json_scrubbed());
        assert_eq!(
            restored.entries(),
            [
                "ls -la",
                "mysql -h db -P 3306",
                "grep password /etc/app.conf",
                "ssh -p 2222 root@host"
            ]
        );
        // 不清洗的完整序列化保持全部条目（加密库路径用）。
        assert_eq!(CommandHistory::from_json(&history.to_json()), history);
    }

    #[test]
    fn remove_clear_and_json_roundtrip() {
        let mut history = CommandHistory::default();
        history.push("apt update");
        history.push("df -h");
        assert!(history.remove(1));
        assert!(!history.remove(5));
        assert_eq!(history.entries(), ["df -h"]);

        let restored = CommandHistory::from_json(&history.to_json());
        assert_eq!(restored, history);
        assert!(history.clear());
        assert!(!history.clear());
        assert!(CommandHistory::from_json("").is_empty());
        assert!(CommandHistory::from_json("{ not json").is_empty());

        // 手工写坏的数据：坏条目与重复条目丢弃，顺序保持新在前。
        let parsed = CommandHistory::from_json(r#"["df -h","Password:","","df -h","ls"]"#);
        assert_eq!(parsed.entries(), ["df -h", "ls"]);
    }
}
