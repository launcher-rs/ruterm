//! 终端内命令补全：按正在输入的词匹配命令片段，像 IDE 的补全一样浮在光标下方。
//!
//! 这里只做纯文本部分（判定词在命令行里的位置、匹配打分、把前缀和命令合并）；
//! 按键状态跟踪与终端写入在 `app::completion`，避免这份逻辑依赖 `cx` 而无法单测。

/// 候选的来源，决定浮层上的徽标。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompletionKind {
    /// 用户自己保存的片段。
    Snippet,
    /// 内置命令表里的命令。
    Command,
    /// 从接得上前文的命令里抽出的选项（参数位输入 `-` 时）。
    Flag,
}

/// 参与补全的一条候选：自己的片段、内置命令，或命令里用过的选项。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CompletionItem {
    pub name: String,
    /// 接受补全时写进终端的文本；选项候选就是选项本身。
    pub command: String,
    pub note: String,
    /// 预计算的小写形式：匹配在每次按键时全表扫描，现场 `to_lowercase`
    /// 会对每个条目分配 2-3 个堆字符串，构造时算一次就能省掉。
    pub name_lower: String,
    pub command_lower: String,
    pub note_lower: String,
    pub kind: CompletionKind,
}

impl CompletionItem {
    pub fn new(name: String, command: String, note: String, kind: CompletionKind) -> Self {
        Self {
            name_lower: name.to_lowercase(),
            command_lower: command.to_lowercase(),
            note_lower: note.to_lowercase(),
            name,
            command,
            note,
            kind,
        }
    }
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

/// 正在输入的词在命令行里的角色。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TokenSlot {
    /// 命令段的第一个词：填的是命令本身。
    Command,
    /// 命令段其后的词：填的是参数，不能再拿它去撞命令名。
    Argument,
}

/// 正在输入的词落在命令位还是参数位。
///
/// `before_token` 是光标前、不含正在输入那个词的文本：按 `; | & ( )` 切成命令段，
/// 词落在段首就是命令位。`cd find` 里的 `find` 在 `cd` 之后，是参数位，
/// 不该再因为片段库里有 `sudo find / -name …` 就把它当命令提示出来。
fn token_slot(before_token: &str) -> TokenSlot {
    if segment_words(before_token).is_empty() {
        TokenSlot::Command
    } else {
        TokenSlot::Argument
    }
}

/// `before_token` 里当前命令段已经写下的词（不含正在输入的词）。
fn segment_words(before_token: &str) -> Vec<&str> {
    before_token
        .rsplit([';', '|', '&', '(', ')'])
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect()
}

/// 命令开头的若干词是否与段末尾的词一一对齐，返回对齐的词数。
///
/// 只认「对齐到段尾」的情况，也就是候选命令正好接在正在输入的词前面：
/// 段末尾是 `cd` 时，`sudo find / -name …` 对不上（开头是 `sudo`），
/// 段末尾是 `sudo apt` 时 `sudo apt install …` 对得上（2 个词）。
fn aligned_word_count(command: &str, segment: &[&str]) -> Option<usize> {
    let words: Vec<&str> = command.split_whitespace().collect();
    (1..=segment.len().min(words.len())).rev().find(|&k| {
        words[..k]
            .iter()
            .zip(&segment[segment.len() - k..])
            .all(|(head, typed)| head.eq_ignore_ascii_case(typed))
    })
}

/// 参数位候选：命令接得上前文，且紧接正在输入的词的那个词以它开头。
///
/// 词为空（刚敲完空格）时列出这条命令后面的所有写法，等于「接着能填什么」的
/// 速查；片段本身的顺序保留（我的片段在前）。
fn continuations(items: &[CompletionItem], token: &str, segment: &[&str]) -> Vec<usize> {
    let needle = token.to_lowercase();
    items
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| {
            let words: Vec<&str> = item.command.split_whitespace().collect();
            let aligned = aligned_word_count(&item.command, segment)?;
            let next = words.get(aligned)?.to_lowercase();
            next.starts_with(&needle).then_some(slot)
        })
        .take(MAX_MATCHES)
        .collect()
}

/// `-y` / `--all` / `-k5` 这样以 `-` 开头、紧跟字母数字的词才算选项；
/// 单独的 `-` 与 `--`（读 stdin / 选项结束标记）不算。
fn is_flag(word: &str) -> bool {
    let body = word.trim_start_matches('-');
    word.starts_with('-')
        && word.len() > body.len()
        && body.chars().next().is_some_and(|c| c.is_alphanumeric())
}

/// 参数位输入 `-` 时的候选：从接得上前文的命令里，取接上位置之后出现的选项。
///
/// 候选没有选项表可依，就按片段库里出现的次数排；小写用于比较与去重，
/// 写回终端的仍是片段里的原大小写。
fn flag_options(items: &[CompletionItem], token: &str, segment: &[&str]) -> Vec<CompletionItem> {
    let needle = token.to_lowercase();
    let mut found: Vec<(String, u32, String)> = Vec::new();
    for item in items {
        let words: Vec<&str> = item.command.split_whitespace().collect();
        let Some(aligned) = aligned_word_count(&item.command, segment) else {
            continue;
        };
        for word in &words[aligned..] {
            if !is_flag(word) || !word.to_lowercase().starts_with(&needle) {
                continue;
            }
            match found
                .iter_mut()
                .find(|(flag, _, _)| flag.to_lowercase() == word.to_lowercase())
            {
                Some((_, count, _)) => *count += 1,
                None => found.push((word.to_string(), 1, item.command.clone())),
            }
        }
    }
    found.sort_by(|(a_flag, a_count, _), (b_flag, b_count, _)| {
        b_count
            .cmp(a_count)
            .then_with(|| a_flag.to_lowercase().cmp(&b_flag.to_lowercase()))
    });
    found
        .into_iter()
        .take(MAX_MATCHES)
        .map(|(flag, _, origin)| {
            CompletionItem::new(flag.clone(), flag, origin, CompletionKind::Flag)
        })
        .collect()
}

/// 按词在命令行里的位置给候选：命令位整库模糊搜索，参数位只接受接得上前文的
/// 续写与选项，参数填的是路径 / 包名之类的东西，命中不了就完全不弹（Tab 交回
/// shell 自己的补全）。
pub fn complete(items: &[CompletionItem], token: &str, before_token: &str) -> Vec<CompletionItem> {
    let token = token.trim();
    let segment = segment_words(before_token);
    let slots = match token_slot(before_token) {
        TokenSlot::Command => fuzzy_matches(items, token),
        TokenSlot::Argument if token.starts_with('-') => {
            return flag_options(items, token, &segment);
        }
        TokenSlot::Argument => continuations(items, token, &segment),
    };
    slots.into_iter().map(|slot| items[slot].clone()).collect()
}

/// 命令位候选：整库模糊匹配，返回 items 下标（已按匹配度排序）。
///
/// 打分顺序：名称前缀 → 名称包含 → 命令里某个词的前缀 → 命令包含 → 备注包含。
/// 命令词前面可能带管道/引号之类的符号，比较前先剥掉。只有命令位走这里——
/// 「敲『磁盘』能找到 `df -h`」靠的就是这种宽松匹配，参数位不能用。
fn fuzzy_matches(items: &[CompletionItem], token: &str) -> Vec<usize> {
    let token = token.trim();
    if token.chars().count() < MIN_TOKEN_CHARS {
        return Vec::new();
    }
    let needle = token.to_lowercase();
    let mut scored: Vec<(i16, usize)> = items
        .iter()
        .enumerate()
        .filter_map(|(slot, item)| item_score(item, &needle).map(|score| (i16::from(score), slot)))
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

fn item_score(item: &CompletionItem, needle: &str) -> Option<u8> {
    // 小写形式在构造时算好（见 `CompletionItem::new`），这里直接用。
    if item.name_lower.starts_with(needle) {
        return Some(0);
    }
    if item.name_lower.contains(needle) {
        return Some(1);
    }
    if item.command_lower.split_whitespace().any(|word| {
        word.trim_start_matches(|c: char| !c.is_alphanumeric())
            .starts_with(needle)
    }) {
        return Some(2);
    }
    if item.command_lower.contains(needle) {
        return Some(3);
    }
    if item.note_lower.contains(needle) {
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
        CompletionItem, CompletionKind, MAX_MATCHES, MIN_TOKEN_CHARS, TokenSlot, complete,
        echo_lagging, fuzzy_matches, last_token, merged_command, token_slot,
    };

    fn item(name: &str, command: &str, note: &str) -> CompletionItem {
        CompletionItem::new(
            name.to_string(),
            command.to_string(),
            note.to_string(),
            CompletionKind::Command,
        )
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

    fn names(hits: &[CompletionItem]) -> Vec<String> {
        hits.iter().map(|hit| hit.name.clone()).collect()
    }

    #[test]
    fn short_token_shows_nothing() {
        let items = sample();
        assert!(fuzzy_matches(&items, "").is_empty());
        assert!(
            fuzzy_matches(&items, "a".repeat(MIN_TOKEN_CHARS - 1).as_str()).is_empty(),
            "命令位一两个字符就弹太吵"
        );
        assert!(!fuzzy_matches(&items, "ap").is_empty());
    }

    #[test]
    fn command_word_prefix_wins_over_note() {
        let items = sample();
        let hits = fuzzy_matches(&items, "apt");
        assert_eq!(hits.len(), 2);
        // 「安装软件包」与「系统更新」都在命令里出现 apt，按名称稳定排序。
        let names: Vec<&str> = hits.iter().map(|&i| items[i].name.as_str()).collect();
        assert_eq!(names, ["安装软件包", "系统更新"]);
    }

    #[test]
    fn name_and_note_are_searchable() {
        let items = sample();
        assert_eq!(fuzzy_matches(&items, "磁盘"), vec![3]);
        // 备注里也有「端口」，仍应命中同名条目。
        assert_eq!(fuzzy_matches(&items, "端口"), vec![2]);
        assert_eq!(fuzzy_matches(&items, "docker"), vec![4]);
    }

    #[test]
    fn limit_is_capped() {
        let items: Vec<CompletionItem> = (0..32)
            .map(|n| item(&format!("命令 {n:02}"), "echo hi", ""))
            .collect();
        assert_eq!(fuzzy_matches(&items, "命令").len(), MAX_MATCHES);
    }

    #[test]
    fn slot_follows_the_command_segment() {
        assert_eq!(token_slot(""), TokenSlot::Command);
        assert_eq!(token_slot("cd "), TokenSlot::Argument);
        assert_eq!(token_slot("sudo apt "), TokenSlot::Argument);
        // 分隔符之后是新的命令段，段首又是命令位。
        assert_eq!(token_slot("ls -la | "), TokenSlot::Command);
        assert_eq!(token_slot("cd /tmp && cd "), TokenSlot::Argument);
        assert_eq!(token_slot("echo hi;"), TokenSlot::Command);
    }

    #[test]
    fn argument_position_does_not_suggest_other_commands() {
        let items = vec![
            item("进入目录", "cd /var/log", "去看系统日志"),
            item(
                "按名查找",
                "sudo find / -name '<文件名>' 2>/dev/null",
                "全盘找文件",
            ),
        ];
        // `cd find`：`find` 是 cd 的参数，不该因为片段里有 `sudo find …` 就命中。
        assert!(complete(&items, "find", "cd ").is_empty());
        // 同一个词落在命令位，照旧整库搜索。
        assert_eq!(names(&complete(&items, "find", "")), ["按名查找"]);
        // 参数位也不许拿参数去撞别的命令名（`cd docker` 不等于要找 docker 命令）。
        assert!(complete(&items, "docker", "cd ").is_empty());
    }

    #[test]
    fn argument_position_only_takes_aligned_continuation() {
        let items = sample();
        // 段末尾正好是候选命令的开头：`sudo apt up` → `sudo apt update …`。
        assert_eq!(names(&complete(&items, "up", "sudo apt ")), ["系统更新"]);
        // 只对齐一个词的 `apt up` 接不上以 sudo 开头的片段。
        assert!(complete(&items, "up", "apt ").is_empty());
        // 命令位仍会把 Invoke-WebRequest 这类含 `in` 的排前面。
        let with_powershell = vec![
            item(
                "下载文件",
                "Invoke-WebRequest -Uri <URL> -OutFile <文件名>",
                "下载文件到本机",
            ),
            item("安装软件包", "sudo apt install <包名>", ""),
        ];
        assert_eq!(names(&complete(&with_powershell, "in", ""))[0], "下载文件");
        assert_eq!(
            names(&complete(&with_powershell, "in", "sudo apt "))[0],
            "安装软件包"
        );
    }

    #[test]
    fn empty_token_at_argument_lists_whats_next() {
        let items = sample();
        // 刚敲完 `sudo apt `：列出这条命令后面能接的写法。
        assert_eq!(
            names(&complete(&items, "", "sudo apt ")),
            ["系统更新", "安装软件包"]
        );
        // 没有接得上的命令就什么都不列。
        assert!(complete(&items, "", "cd ").is_empty());
    }

    #[test]
    fn subcommand_continues_its_command() {
        let items = vec![
            item("提交改动", "git commit -m '<说明>'", "提交到当前分支"),
            item("查看改动", "git status", "看工作区状态"),
            item("添加远程", "git remote add <名> <地址>", "接远程仓库"),
        ];
        assert_eq!(names(&complete(&items, "re", "git ")), ["添加远程"]);
        assert_eq!(names(&complete(&items, "s", "git ")), ["查看改动"]);
        // 子命令之后的参数不再去撞别的 git 命令：`git commit msg` 什么都不提示。
        assert!(complete(&items, "msg", "git commit ").is_empty());
    }

    #[test]
    fn dash_offers_options_of_the_command_in_position() {
        let items = vec![
            item("提交改动", "git commit -m '<说明>'", ""),
            item("打标签", "git tag -a -m '<说明>'", ""),
            item("改写上次提交", "git commit --amend", ""),
            item("忽略大小写比较", "git diff -i --no-index", ""),
        ];
        // 用得多的排前面，同频按字母；来源命令留在 note 里给浮层显示。
        let hits = complete(&items, "-", "git ");
        assert_eq!(
            hits.iter()
                .map(|hit| hit.command.as_str())
                .collect::<Vec<_>>(),
            ["-m", "--amend", "--no-index", "-a", "-i"]
        );
        assert_eq!(hits[0].note, "git commit -m '<说明>'");
        assert_eq!(hits[0].kind, CompletionKind::Flag);
        // `--` 只留长选项。
        assert_eq!(
            complete(&items, "--", "git ")
                .iter()
                .map(|hit| hit.command.as_str())
                .collect::<Vec<_>>(),
            ["--amend", "--no-index"]
        );
        // 命令对不上就没有选项可给。
        assert!(complete(&items, "-", "cd ").is_empty());
        // 单独的 `-` 与 `--` 本身不是选项。
        let bare = vec![item("过滤", "grep - pattern", "")];
        assert!(complete(&bare, "-", "grep ").is_empty());
    }

    #[test]
    fn trailing_space_hands_the_previous_words_to_the_matcher() {
        // UI 交给匹配层的只有「正在输入的词」和它之前的文本：`docker p` 里词是 `p`，
        // 前文 `docker ` 决定这是 docker 的参数位。
        let line = "docker p";
        let token = last_token(line);
        assert_eq!(token, "p");
        assert_eq!(
            token_slot(&line[..line.len() - token.len()]),
            TokenSlot::Argument
        );
        // 行尾只有一个空格时词为空，前文仍是整段命令；分隔符之后重新算命令位。
        assert_eq!(last_token("sudo apt "), "");
        assert_eq!(token_slot("sudo apt "), TokenSlot::Argument);
        assert_eq!(token_slot("cd /tmp && "), TokenSlot::Command);
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
