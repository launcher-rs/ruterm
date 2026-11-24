//! 目录线索解析：从用户键入识别 cd 行、解析 OSC 7 上报、按 POSIX 规则拼路径。
//!
//! 纯字符串/字节逻辑，不依赖 SSH 连接类型，可独立单测。

/// 逐字节跟踪用户键入：把终端的按键流还原成一行命令，识别 cd / pushd。
///
/// 遇到不可还原的编辑（方向键、Tab 补全、Ctrl+A 等）就把整行标成"不可信"，
/// 宁可漏报也不误报——面板跳错目录比不跟更糟。
#[derive(Default)]
pub(super) struct CwdKeystrokes {
    buf: Vec<u8>,
    skip: bool,
}

impl CwdKeystrokes {
    pub(super) fn feed(&mut self, bytes: &[u8]) -> Option<String> {
        let mut done = None;
        for &b in bytes {
            match b {
                0x0d | 0x0a => {
                    if !self.skip
                        && let Ok(line) = std::str::from_utf8(&self.buf)
                        && let Some(arg) = cd_arg_from_line(line)
                    {
                        log::debug!("[CwdKeystrokes] 检测到 cd: {}", arg);
                        done = Some(arg);
                    } else if !self.skip
                        && let Ok(line) = std::str::from_utf8(&self.buf)
                    {
                        log::debug!("[CwdKeystrokes] 行结束但未检测到 cd: {:?}", line);
                    }
                    self.buf.clear();
                    self.skip = false;
                }
                0x03 | 0x15 => {
                    // Ctrl+C / Ctrl+U：清空缓冲区，整行作废。
                    self.buf.clear();
                    self.skip = false;
                }
                0x17 => {
                    // Ctrl+W：删除上一个词；只按 ASCII 空格切词，
                    // 遇到 UTF-8 多字节字符会留下残缺字节，finalize 时 from_utf8 失败 → 整行跳过。
                    while self.buf.last().is_some_and(|&b| b == b' ') {
                        self.buf.pop();
                    }
                    while self.buf.last().is_some_and(|&b| b != b' ') {
                        self.buf.pop();
                    }
                }
                0x7f | 0x08 => {
                    self.buf.pop();
                }
                0x1b | 0x09 => {
                    // ESC（方向键 / 历史 / 粘贴括号）或 Tab（补全）：行内容已不可还原。
                    self.skip = true;
                }
                b if b < 0x20 => {
                    // 其他控制字符（Ctrl+A/E/B/F 等）：行编辑已偏离缓冲区。
                    self.skip = true;
                }
                _ => {
                    if self.buf.len() < 4096 {
                        self.buf.push(b);
                    } else {
                        self.skip = true;
                    }
                }
            }
        }
        done
    }
}

/// 从一行命令里提取 cd / pushd 的目标路径（空串＝回主目录）。
///
/// 仅处理最简情形：行首命令是 `cd` 或 `pushd`，参数在第一个分号/管道/逻辑与/注释前，
/// 成对引号会被剥掉；其余情形（含变量替换、通配、~user、pushd 无参等）一律返回 None，
/// 由校验任务保守地跳过，避免面板跳到错误目录。
fn cd_arg_from_line(line: &str) -> Option<String> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let (cmd, rest) = match line.split_once(char::is_whitespace) {
        Some((cmd, rest)) => (cmd, rest.trim()),
        None => (line, ""),
    };
    if cmd != "cd" && cmd != "pushd" {
        return None;
    }
    // pushd 无参 = 交换栈顶，无法跟踪；cd 无参 = 回主目录。
    if cmd == "pushd" && rest.is_empty() {
        return None;
    }
    let first = rest.split([';', '|', '&', '#']).next()?.trim();
    let mut arg = first;
    if arg.len() >= 2
        && let Some(stripped) = strip_matching_quotes(arg)
    {
        arg = stripped;
    }
    // 丢弃前导选项（-L / -P / -e ...），但保留裸 "-"（回 OLDPWD）。
    while arg.starts_with('-') && arg != "-" && arg.len() > 1 {
        match arg.split_once(char::is_whitespace) {
            Some((_, tail)) => arg = tail.trim(),
            None => return None,
        }
    }
    if arg.contains('$')
        || arg.contains('`')
        || arg.contains('*')
        || arg.contains('?')
        || (arg.starts_with('~') && arg != "~" && !arg.starts_with("~/"))
    {
        return None;
    }
    Some(arg.to_string())
}

fn strip_matching_quotes(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let first = bytes.first()?;
    let last = bytes.last()?;
    if (first == &b'"' && last == &b'"') || (first == &b'\'' && last == &b'\'') {
        Some(&s[1..s.len() - 1])
    } else {
        None
    }
}

/// 把相对路径按 POSIX 规则词法拼接到 base（base 必须绝对），
/// 不查文件系统：`..` 弹栈、`.` 跳过、空段忽略。cd 失败时校验任务会拒收，
/// 所以这里的"乐观拼接"不会污染面板。
pub(super) fn lex_join(base: &str, rel: &str) -> String {
    let mut stack: Vec<&str> = base
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    for part in rel.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                stack.pop();
            }
            other => stack.push(other),
        }
    }
    if stack.is_empty() {
        "/".to_string()
    } else {
        let mut out = String::with_capacity(stack.iter().map(|s| s.len() + 1).sum());
        out.push('/');
        for (i, seg) in stack.iter().enumerate() {
            if i > 0 {
                out.push('/');
            }
            out.push_str(seg);
        }
        out
    }
}

/// 百分号解码（`%XX` → 字节），非 UTF-8 序列用 lossy 还原；
/// 无效 `%` 序列原样保留，避免把用户路径里的 `%` 误吞。
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 3 <= bytes.len() {
            let hi = bytes[i + 1];
            let lo = bytes[i + 2];
            if let (Some(h), Some(l)) = (hex_digit(hi), hex_digit(lo)) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 解析 OSC 7 上报的 `file://host/path` 或 `file:///path`，返回解码后的绝对路径。
pub(crate) fn path_from_osc7(payload: &str) -> Option<String> {
    let rest = payload.strip_prefix("file://")?;
    let idx = rest.find('/')?;
    let path = &rest[idx..];
    if !path.starts_with('/') {
        return None;
    }
    Some(percent_decode(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_arg_plain_and_quoted() {
        assert_eq!(cd_arg_from_line("cd /tmp"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd  /var/log"), Some("/var/log".into()));
        assert_eq!(cd_arg_from_line("cd"), Some("".into()));
        assert_eq!(cd_arg_from_line("cd ~"), Some("~".into()));
        assert_eq!(cd_arg_from_line("cd ~/src"), Some("~/src".into()));
        assert_eq!(cd_arg_from_line("cd -"), Some("-".into()));
        assert_eq!(
            cd_arg_from_line("cd \"/mnt/c/My Files\""),
            Some("/mnt/c/My Files".into())
        );
        assert_eq!(cd_arg_from_line("cd '/var/log'"), Some("/var/log".into()));
    }

    #[test]
    fn cd_arg_strips_chain_and_options() {
        assert_eq!(cd_arg_from_line("cd /tmp && ls"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp; pwd"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp | grep"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd /tmp # comment"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd -L /tmp"), Some("/tmp".into()));
        assert_eq!(cd_arg_from_line("cd -L -P /tmp"), Some("/tmp".into()));
    }

    #[test]
    fn cd_arg_rejects_unresolvable() {
        assert_eq!(cd_arg_from_line("ls /tmp"), None);
        assert_eq!(cd_arg_from_line("cd $HOME"), None);
        assert_eq!(cd_arg_from_line("cd `pwd`"), None);
        assert_eq!(cd_arg_from_line("cd *"), None);
        assert_eq!(cd_arg_from_line("cd ~root"), None);
        assert_eq!(cd_arg_from_line("pushd"), None);
        assert_eq!(cd_arg_from_line(""), None);
    }

    #[test]
    fn lex_join_handles_relative_and_dotdot() {
        assert_eq!(lex_join("/home/user", "src"), "/home/user/src");
        assert_eq!(lex_join("/home/user", ".."), "/home");
        assert_eq!(lex_join("/home/user", "../.."), "/");
        assert_eq!(lex_join("/", "a/b"), "/a/b");
        assert_eq!(lex_join("/a/b", "./c"), "/a/b/c");
        assert_eq!(lex_join("/a/b", "c/../d"), "/a/b/d");
    }

    #[test]
    fn percent_decode_handles_utf8_and_invalid() {
        assert_eq!(percent_decode("/tmp"), "/tmp");
        assert_eq!(percent_decode("/a%20b"), "/a b");
        // 中文 UTF-8 路径：`中` = E4 B8 AD
        assert_eq!(percent_decode("/%E4%B8%AD"), "/中");
        // 无效百分号保留原样
        assert_eq!(percent_decode("/a%ZZb"), "/a%ZZb");
        assert_eq!(percent_decode("/a%2"), "/a%2");
    }

    #[test]
    fn path_from_osc7_parses_host_and_empty_host() {
        assert_eq!(
            path_from_osc7("file://server/tmp/logs"),
            Some("/tmp/logs".into())
        );
        assert_eq!(
            path_from_osc7("file:///home/user"),
            Some("/home/user".into())
        );
        assert_eq!(path_from_osc7("file://h/a%20b"), Some("/a b".into()));
        assert_eq!(path_from_osc7("not-a-uri"), None);
        assert_eq!(path_from_osc7("file://host"), None);
    }

    #[test]
    fn keystroke_tracker_detects_cd_and_ignores_escapes() {
        let mut t = CwdKeystrokes::default();
        assert_eq!(t.feed(b"cd /tmp\r"), Some("/tmp".into()));
        // 方向键（ESC）+ 回车：整行不可信
        assert_eq!(t.feed(b"cd /var\x1b[Ar"), None);
        assert_eq!(t.feed(b"\r"), None);
        // 退格正常处理
        assert_eq!(t.feed(b"cd /tmpx\x7f\r"), Some("/tmp".into()));
        // Ctrl+W 删词
        assert_eq!(t.feed(b"cd /tmp /var\x17\r"), Some("/tmp".into()));
        // Ctrl+C 清空
        assert_eq!(t.feed(b"garbage\x03"), None);
        assert_eq!(t.feed(b"cd /etc\r"), Some("/etc".into()));
    }
}
