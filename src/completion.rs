//! 终端内命令补全：按正在输入的词匹配命令片段，像 IDE 的补全一样浮在光标下方。
//!
//! 这里只做纯文本部分（匹配打分、把前缀和命令合并）；按键状态跟踪与终端写入
//! 在 `app::completion`，避免这份逻辑依赖 `cx` 而无法单测。

/// 参与补全的一条候选：自己的片段或内置命令。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub name: String,
    pub command: String,
    pub note: String,
    /// true＝内置命令（只读），false＝用户保存的片段。
    pub builtin: bool,
    /// 在片段列表 / 内置命令表里的下标，接受补全时用来取命令原文。
    pub index: usize,
}

/// 当前词达到这个长度才浮出候选：一两个字符就弹会太吵。
pub const MIN_TOKEN_CHARS: usize = 2;
/// 浮层最多展示的候选数。
pub const MAX_MATCHES: usize = 8;

/// 文本里最后一个词（按空白 / 分隔符切分）。
pub fn last_token(text: &str) -> &str {
    text.rsplit(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')'))
        .next()
        .unwrap_or("")
}

/// 按输入词挑候选，返回 items 下标（已按匹配度排序）。
///
/// 打分顺序：名称前缀 → 名称包含 → 命令里某个词的前缀 → 命令包含 → 备注包含。
/// 命令词前面可能带管道/引号之类的符号，比较前先剥掉。`prefix` 是行内已输入的
/// 前文：命令开头正好接上前缀时排到最前，例如 `sudo apt in` 优先「安装软件包」
/// （`sudo apt install …`），而不是只碰巧某个词以 `in` 开头（如 `Invoke-WebRequest`）。
pub fn matches_after(items: &[CompletionItem], token: &str, prefix: &str) -> Vec<usize> {
    let token = token.trim();
    if token.chars().count() < MIN_TOKEN_CHARS {
        return Vec::new();
    }
    let needle = token.to_lowercase();
    let prefix_words: Vec<String> = prefix.split_whitespace().map(str::to_lowercase).collect();
    let mut scored: Vec<(i16, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| {
            item_score(item, &needle).map(|score| {
                let bonus = if continues_prefix(&item.command, &prefix_words) {
                    -8
                } else {
                    0
                };
                (i16::from(score) + bonus, slot)
            })
        })
        .collect();
    scored.sort_by(|(a_score, a_slot), (b_score, b_slot)| {
        a_score
            .cmp(b_score)
            .then_with(|| items[*a_slot].name.cmp(&items[*b_slot].name))
    });
    scored
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(_, slot)| slot)
        .collect()
}

/// 命令开头的若干词是否正好等于前缀末尾的词（大小写不敏感）。
fn continues_prefix(command: &str, prefix_words: &[String]) -> bool {
    let words: Vec<String> = command.split_whitespace().map(str::to_lowercase).collect();
    let max = prefix_words.len().min(words.len());
    (1..=max).any(|k| prefix_words[prefix_words.len() - k..] == words[..k])
}

fn item_score(item: &CompletionItem, needle: &str) -> Option<u8> {
    let name = item.name.to_lowercase();
    if name.starts_with(needle) {
        return Some(0);
    }
    if name.contains(needle) {
        return Some(1);
    }
    let command = item.command.to_lowercase();
    if command.split_whitespace().any(|word| {
        word.trim_start_matches(|c: char| !c.is_alphanumeric())
            .starts_with(needle)
    }) {
        return Some(2);
    }
    if command.contains(needle) {
        return Some(3);
    }
    if item.note.to_lowercase().contains(needle) {
        Some(4)
    } else {
        None
    }
}

/// 网格回显落后于按键时，行尾可能只有正在输入的词的前半截。
///
/// `before` 是终端网格里光标前的字符，`tracked` 是按按键累计的整行。远程 shell
/// 的回显要绕一圈才回来，刚敲完就按 Tab / Enter 时网格还没跟上；此时网格行尾正好
/// 是 `tracked` 的前缀（缺一个词以内的尾部）。终端输入按先后顺序入队，补全写入的
/// 退格仍在这些按键之后，所以这种“只差回显”的情况可以照常替换。
pub fn echo_lagging(before: &[char], tracked: &[char], token_chars: usize) -> bool {
    if before.is_empty() || tracked.is_empty() || token_chars == 0 || tracked.len() < token_chars {
        return false;
    }
    // 落后不超过一个词；整行都没回显时没有锚点，不冒险。
    let max_lag = token_chars.min(tracked.len());
    (1..=max_lag)
        .any(|lag| lag < tracked.len() && before.ends_with(&tracked[..tracked.len() - lag]))
}

/// 把候选命令接到已有前缀后面：前缀的后缀已经写过命令开头的若干词时（如
/// `sudo apt ` + `sudo apt install …`）不重复，否则会拼出 `sudo apt sudo apt …`。
pub fn merged_command(prefix: &str, command: &str) -> String {
    let prefix_words: Vec<&str> = prefix.split_whitespace().collect();
    let words: Vec<&str> = command.split_whitespace().collect();
    let max = prefix_words.len().min(words.len());
    for common in (1..=max).rev() {
        if prefix_words[prefix_words.len() - common..] == words[..common] {
            let rest = words[common..].join(" ");
            return if rest.is_empty() {
                command.trim().to_string()
            } else {
                rest
            };
        }
    }
    command.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::{
        CompletionItem, MAX_MATCHES, MIN_TOKEN_CHARS, echo_lagging, last_token, matches_after,
        merged_command,
    };

    fn item(name: &str, command: &str, note: &str) -> CompletionItem {
        CompletionItem {
            name: name.to_string(),
            command: command.to_string(),
            note: note.to_string(),
            builtin: true,
            index: 0,
        }
    }

    fn sample() -> Vec<CompletionItem> {
        vec![
            item(
                "系统更新",
                "sudo apt update && sudo apt upgrade -y",
                "更新索引并升级",
            ),
            item(
                "安装软件包",
                "sudo apt install <包名>",
                "把包名换成要装的软件",
            ),
            item("监听端口", "sudo ss -tlnp", "列出监听端口"),
            item("磁盘余量", "df -h", "按挂载点列出使用率"),
            item(
                "Docker 容器",
                "docker ps -a --format 'table {{.Names}}'",
                "列出全部容器",
            ),
        ]
    }

    #[test]
    fn short_token_shows_nothing() {
        let items = sample();
        assert!(matches_after(&items, "", "").is_empty());
        assert!(matches_after(&items, "a".repeat(MIN_TOKEN_CHARS - 1).as_str(), "").is_empty());
        assert!(!matches_after(&items, "ap", "").is_empty());
    }

    #[test]
    fn command_word_prefix_wins_over_note() {
        let items = sample();
        let hits = matches_after(&items, "apt", "");
        assert_eq!(hits.len(), 2);
        // 「安装软件包」与「系统更新」都在命令里出现 apt，按名称稳定排序。
        let names: Vec<&str> = hits.iter().map(|&i| items[i].name.as_str()).collect();
        assert_eq!(names, ["安装软件包", "系统更新"]);
    }

    #[test]
    fn name_and_note_are_searchable() {
        let items = sample();
        assert_eq!(matches_after(&items, "磁盘", ""), vec![3]);
        // 备注里也有「端口」，仍应命中同名条目。
        assert_eq!(matches_after(&items, "端口", ""), vec![2]);
        assert_eq!(matches_after(&items, "docker", ""), vec![4]);
    }

    #[test]
    fn limit_is_capped() {
        let items: Vec<CompletionItem> = (0..32)
            .map(|n| item(&format!("命令 {n:02}"), "echo hi", ""))
            .collect();
        assert_eq!(matches_after(&items, "命令", "").len(), MAX_MATCHES);
    }

    #[test]
    fn typed_prefix_ranks_continuation_first() {
        let items = vec![
            item(
                "下载文件",
                "Invoke-WebRequest -Uri <URL> -OutFile <文件名>",
                "下载文件到本机",
            ),
            item("安装软件包", "sudo apt install <包名>", ""),
        ];
        // 前缀 `sudo apt ` 正好接上 install 所在命令：即使两条都命中 `in`，也排在前面。
        assert_eq!(
            items[matches_after(&items, "in", "sudo apt ")[0]].name,
            "安装软件包"
        );
        // 没有前缀时按普通打分（命令词前缀）与名称排序，PowerShell 那条在前。
        assert_eq!(items[matches_after(&items, "in", "")[0]].name, "下载文件");
    }

    #[test]
    fn previous_word_query_skips_trailing_space() {
        // UI 在词太短时用前一个词查询：`docker p` 要能回退到 `docker`。
        let line = "docker p";
        let token = last_token(line);
        assert_eq!(token, "p");
        let before = line[..line.len() - token.len()].trim_end();
        assert_eq!(last_token(before), "docker");
        // 行尾只有一个空格时同样回退到最后一个完整词。
        let line = "sudo apt ";
        assert_eq!(last_token(line), "");
        assert_eq!(last_token(line.trim_end()), "apt");
    }

    #[test]
    fn last_token_splits_on_separators() {
        assert_eq!(last_token("sudo apt upd"), "upd");
        assert_eq!(last_token("cd /tmp && ls"), "ls");
        assert_eq!(last_token(""), "");
    }

    #[test]
    fn merged_command_skips_duplicate_leading_words() {
        assert_eq!(merged_command("sudo ", "sudo apt update"), "apt update");
        assert_eq!(
            merged_command("sudo apt ", "sudo apt install <包名>"),
            "install <包名>"
        );
        assert_eq!(merged_command("docker ", "docker ps -a"), "ps -a");
        assert_eq!(merged_command("cd /tmp && ", "ls -la"), "ls -la");
        assert_eq!(merged_command("", "df -h"), "df -h");
    }

    fn chars(text: &str) -> Vec<char> {
        text.chars().collect()
    }

    #[test]
    fn echo_lagging_accepts_only_partial_echo_of_the_tail() {
        // 词已完整回显：后面只差一个空格的回显，替换仍然安全。
        assert!(echo_lagging(&chars("root# docker "), &chars("docker p"), 1));
        // 网格停在词前：连词都没回显，只是看起来像，不能动。
        assert!(!echo_lagging(&chars("root# docker"), &chars("docker p"), 1));
        // 正在输入的词只回显了前半截。
        assert!(echo_lagging(
            &chars("root# sudo dock"),
            &chars("sudo docker"),
            6
        ));
        // 整行都没回显。
        assert!(!echo_lagging(&chars("root# "), &chars("sudo docker"), 6));
        // 命令词在行首、且一个字符都没回显：没有锚点。
        assert!(!echo_lagging(&chars("root#"), &chars("docker"), 6));
        // 网格内容与记录的行完全无关。
        assert!(!echo_lagging(&chars("root# sudo"), &chars("echo hi"), 5));
        assert!(!echo_lagging(&[], &chars("docker"), 6));
    }
}
