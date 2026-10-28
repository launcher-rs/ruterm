//! `~/.ssh/config` 导入：只收具体别名，通配符与 `Match` 块整体跳过。
//!
//! 解析刻意做小：不支持 `Include` / `ProxyJump` / 环境变量插值，
//! 只把能直接变成 RuLink 主机的四项（HostName / User / Port / IdentityFile）读出来。
//! 同一别名在多个块里出现时保留第一个块（与 ssh 的 first-obtained-wins 语义接近）。

use std::path::PathBuf;

/// 一条可导入的主机配置。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SshConfigEntry {
    /// `Host` 后的具体别名（同时用作展示名）。
    pub alias: String,
    /// 实际地址；未写 `HostName` 时等于别名。
    pub host_name: String,
    /// 用户名；未写则由调用方取系统当前用户。
    pub user: Option<String>,
    pub port: u16,
    /// 私钥路径（已展开 `~/`）。
    pub identity_file: Option<PathBuf>,
}

/// 当前用户的 `~/.ssh/config` 路径。
pub fn config_path() -> PathBuf {
    home_dir().join(".ssh").join("config")
}

fn home_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 解析配置文件文本；空文本 / 只有通配符块时返回空表。
pub fn parse(text: &str) -> Vec<SshConfigEntry> {
    let mut entries = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let mut block: Option<Block> = None;

    for raw in text.lines() {
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = split_keyword(line);
        match key.to_ascii_lowercase().as_str() {
            "host" => {
                flush(&mut block, &mut entries, &mut seen);
                let aliases: Vec<String> = value
                    .split_whitespace()
                    .map(unquote)
                    // `!bad` 除外、含 `*` / `?` 的通配符不可直接连，跳过。
                    .filter(|pattern| !pattern.starts_with('!') && !pattern.contains(['*', '?']))
                    .map(str::to_string)
                    .collect();
                block = (!aliases.is_empty()).then_some(Block {
                    aliases,
                    host_name: None,
                    user: None,
                    port: None,
                    identity_file: None,
                });
            }
            // Match 块按条件生效，无法静态落成一台主机：其后的参数不再归入上一个 Host。
            "match" => {
                flush(&mut block, &mut entries, &mut seen);
                block = None;
            }
            "hostname" | "user" | "port" | "identityfile" => {
                let Some(block) = block.as_mut() else {
                    continue;
                };
                let value = unquote(value);
                if value.is_empty() {
                    continue;
                }
                match key.to_ascii_lowercase().as_str() {
                    "hostname" => set_once(&mut block.host_name, value.to_string()),
                    "user" => set_once(&mut block.user, value.to_string()),
                    "port" => {
                        if let Ok(port) = value.parse::<u16>()
                            && port != 0
                        {
                            set_once(&mut block.port, port);
                        }
                    }
                    _ => set_once(&mut block.identity_file, expand_tilde(value)),
                }
            }
            _ => {}
        }
    }
    flush(&mut block, &mut entries, &mut seen);
    entries
}

/// 正在累积的一个 `Host` 块（可能带多个别名）。
struct Block {
    aliases: Vec<String>,
    host_name: Option<String>,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<PathBuf>,
}

fn flush(block: &mut Option<Block>, entries: &mut Vec<SshConfigEntry>, seen: &mut Vec<String>) {
    let Some(block) = block.take() else {
        return;
    };
    for alias in block.aliases {
        let key = alias.to_ascii_lowercase();
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        let host_name = block.host_name.clone().unwrap_or_else(|| alias.clone());
        entries.push(SshConfigEntry {
            alias,
            host_name,
            user: block.user.clone(),
            port: block.port.unwrap_or(22),
            identity_file: block.identity_file.clone(),
        });
    }
}

/// 同一个块里重复写同一关键字时保留第一次的值（ssh 的行为）。
fn set_once<T>(slot: &mut Option<T>, value: T) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

/// `#` 注释：只在行首或空白之后开始，避免把路径里的 `#` 当成注释。
fn strip_comment(line: &str) -> &str {
    let mut after_space = true;
    for (index, character) in line.char_indices() {
        if character == '#' && after_space {
            return &line[..index];
        }
        after_space = character.is_whitespace();
    }
    line
}

/// 拆关键字与值：兼容 `Port 2222` 与 `Port=2222` 两种写法。
fn split_keyword(line: &str) -> (&str, &str) {
    match line.find(|character: char| character.is_whitespace() || character == '=') {
        Some(index) => {
            let rest = line[index..].trim_start_matches(|character: char| {
                character.is_whitespace() || character == '='
            });
            (&line[..index], rest)
        }
        None => (line, ""),
    }
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    if value.len() >= 2 && value.starts_with('"') && value.ends_with('"') {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// 展开开头的 `~/`；`~user/` 与相对路径原样保留（ssh 也按当前目录解析）。
fn expand_tilde(value: &str) -> PathBuf {
    match value
        .strip_prefix("~/")
        .or_else(|| value.strip_prefix("~\\"))
    {
        Some(rest) => home_dir().join(rest),
        None => PathBuf::from(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hosts_with_identity_file_and_comments() {
        let text = "\
# 个人跳板机
Host prod
    HostName 10.0.0.1
    User deploy
    Port 2222
    IdentityFile ~/.ssh/id_ed25519   # 主密钥

Host dev
    Hostname dev.example.com
    IdentityFile \"C:\\keys\\dev key\"
";
        let entries = parse(text);
        assert_eq!(entries.len(), 2);

        assert_eq!(entries[0].alias, "prod");
        assert_eq!(entries[0].host_name, "10.0.0.1");
        assert_eq!(entries[0].user.as_deref(), Some("deploy"));
        assert_eq!(entries[0].port, 2222);
        let key = entries[0].identity_file.as_ref().expect("应解析出私钥路径");
        assert!(key.is_absolute(), "~/ 应展开成绝对路径: {key:?}");
        assert!(key.ends_with(".ssh/id_ed25519") || key.ends_with(".ssh\\id_ed25519"));

        assert_eq!(entries[1].alias, "dev");
        assert_eq!(entries[1].host_name, "dev.example.com");
        assert_eq!(entries[1].user, None);
        assert_eq!(entries[1].port, 22);
        assert_eq!(
            entries[1].identity_file.as_deref(),
            Some(std::path::Path::new("C:\\keys\\dev key"))
        );
    }

    #[test]
    fn skips_wildcards_match_blocks_and_duplicate_aliases() {
        let text = "\
Host *
    User root

Host *.internal !skip
    User ops

Host web
    HostName web1

Host web
    HostName web2

Match host db user root
    Port 2200
";
        let entries = parse(text);
        assert_eq!(
            entries.len(),
            1,
            "只有具体且首次出现的别名可导入: {entries:?}"
        );
        assert_eq!(entries[0].alias, "web");
        assert_eq!(entries[0].host_name, "web1", "同名块保留第一份");
    }

    #[test]
    fn supports_equals_syntax_and_defaults_hostname_to_alias() {
        let entries = parse("Host=build\n  HostName=build.lan\n  Port=2200\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].host_name, "build.lan");
        assert_eq!(entries[0].port, 2200);

        let bare = parse("Host only\n");
        assert_eq!(bare[0].host_name, "only");
        assert_eq!(bare[0].port, 22);
    }

    #[test]
    fn empty_and_keyword_only_text_yields_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("User root\nPort 22\n").is_empty());
    }
}
