//! RDP 的按键编码：Windows Set 1 扫描码（`0xE0xx` 表示扩展键）。
//!
//! rgpui 的 `Keystroke` 只给键名与字符，没有平台扫描码，所以这里按 US 布局
//! 反查物理键。查不到的一律走 IronRDP 的 Unicode 通道：中文、重音字符、
//! 以及本地布局上需要 Shift 的符号（`!@#$…`）——后者按物理键发会丢掉 Shift。

/// 具名键 → 扫描码。`0xE0xx` 是扩展键（方向键、Home/End 等）。
pub fn named_scancode(name: &str) -> Option<u16> {
    let scancode = match name.to_ascii_lowercase().as_str() {
        "escape" | "esc" => 0x01,
        "backspace" => 0x0e,
        "tab" => 0x0f,
        "enter" | "return" => 0x1c,
        "space" => 0x39,
        "capslock" | "caps_lock" => 0x3a,
        "numlock" | "num_lock" => 0x45,
        "scrolllock" | "scroll_lock" => 0x46,
        "shift" => 0x2a,
        "control" | "ctrl" => 0x1d,
        "alt" | "option" => 0x38,
        "super" | "cmd" | "win" | "platform" => 0xe05b,
        "printscreen" | "print_screen" => 0xe037,
        "insert" | "ins" => 0xe052,
        "delete" | "del" => 0xe053,
        "home" => 0xe047,
        "end" => 0xe04f,
        "pageup" | "page_up" => 0xe049,
        "pagedown" | "page_down" => 0xe051,
        "left" => 0xe04b,
        "right" => 0xe04d,
        "up" => 0xe048,
        "down" => 0xe050,
        "menu" | "contextmenu" => 0xe05d,
        function if function.len() >= 2 && function.len() <= 3 => {
            let digit = function.strip_prefix('f')?.parse::<u8>().ok()?;
            match digit {
                1..=10 => 0x3b + u16::from(digit - 1),
                11 => 0x57,
                12 => 0x58,
                13..=24 => 0x64 + u16::from(digit - 13),
                _ => return None,
            }
        }
        _ => return None,
    };
    Some(scancode)
}

/// 字符 → 扫描码。只收 US 布局上「不按 Shift 就能打出」的键。
///
/// 大写字母按小写处理：Shift 的真实按下事件已经单独发过去了，服务端自己合成
/// 大小写；这里再套一层 Shift 就成了双击。
pub fn char_scancode(ch: char) -> Option<u16> {
    let scancode = match ch.to_ascii_lowercase() {
        'a' => 0x1e,
        'b' => 0x30,
        'c' => 0x2e,
        'd' => 0x20,
        'e' => 0x12,
        'f' => 0x21,
        'g' => 0x22,
        'h' => 0x23,
        'i' => 0x17,
        'j' => 0x24,
        'k' => 0x25,
        'l' => 0x26,
        'm' => 0x32,
        'n' => 0x31,
        'o' => 0x18,
        'p' => 0x19,
        'q' => 0x10,
        'r' => 0x13,
        's' => 0x1f,
        't' => 0x14,
        'u' => 0x16,
        'v' => 0x2f,
        'w' => 0x11,
        'x' => 0x2d,
        'y' => 0x15,
        'z' => 0x2c,
        '0' => 0x0b,
        '1' => 0x02,
        '2' => 0x03,
        '3' => 0x04,
        '4' => 0x05,
        '5' => 0x06,
        '6' => 0x07,
        '7' => 0x08,
        '8' => 0x09,
        '9' => 0x0a,
        '-' => 0x0c,
        '=' => 0x0d,
        '[' => 0x1a,
        ']' => 0x1b,
        '\\' => 0x2b,
        ';' => 0x27,
        '\'' => 0x28,
        '`' => 0x29,
        ',' => 0x33,
        '.' => 0x34,
        '/' => 0x35,
        _ => return None,
    };
    Some(scancode)
}

/// 真实按下 / 抬起事件 → 扫描码。
///
/// `key_char` 与 `key` 不一致（本地布局的映射字符、输入法字符）时不发扫描码，
/// 由 `event_char` 交给 Unicode 通道，否则远端会打成另一个字。
pub fn event_scancode(key: &str, key_char: Option<&str>) -> Option<u16> {
    if let Some(scancode) = named_scancode(key) {
        return Some(scancode);
    }
    if let Some(ch) = key_char.and_then(|text| text.chars().next()) {
        let lowered = key.to_ascii_lowercase();
        if lowered.len() == 1 && ch.to_ascii_lowercase().to_string() == lowered {
            return char_scancode(ch);
        }
        return None;
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) => char_scancode(ch),
        _ => None,
    }
}

/// 走 Unicode 通道的字符：没有扫描码、但确实要打给远端的可打印字符。
pub fn event_char(key: &str, key_char: Option<&str>) -> Option<char> {
    if named_scancode(key).is_some() {
        return None;
    }
    if let Some(ch) = key_char.and_then(|text| text.chars().next()) {
        return (!ch.is_control() && char_scancode(ch).is_none()).then_some(ch);
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_control() && char_scancode(ch).is_none() => Some(ch),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_cover_navigation_and_modifiers() {
        assert_eq!(named_scancode("escape"), Some(0x01));
        assert_eq!(named_scancode("enter"), Some(0x1c));
        assert_eq!(named_scancode("left"), Some(0xe04b));
        assert_eq!(named_scancode("PageDown"), Some(0xe051));
        assert_eq!(named_scancode("f1"), Some(0x3b));
        assert_eq!(named_scancode("f10"), Some(0x44));
        assert_eq!(named_scancode("f11"), Some(0x57));
        assert_eq!(named_scancode("f12"), Some(0x58));
        assert_eq!(named_scancode("f13"), Some(0x64));
        assert_eq!(named_scancode("nonsense"), None);
        assert_eq!(named_scancode("f99"), None);
    }

    #[test]
    fn chars_map_to_us_layout_scancodes() {
        assert_eq!(char_scancode('a'), Some(0x1e));
        assert_eq!(char_scancode('A'), Some(0x1e));
        assert_eq!(char_scancode('z'), Some(0x2c));
        assert_eq!(char_scancode('0'), Some(0x0b));
        assert_eq!(char_scancode('9'), Some(0x0a));
        assert_eq!(char_scancode('\\'), Some(0x2b));
        assert_eq!(char_scancode('/'), Some(0x35));
        // Shift 上的符号与中文没有 US 布局扫描码：交给 Unicode 通道。
        assert_eq!(char_scancode('!'), None);
        assert_eq!(char_scancode('中'), None);
        assert_eq!(char_scancode('é'), None);
    }

    #[test]
    fn event_scancode_prefers_physical_key_and_drops_layout_remaps() {
        assert_eq!(event_scancode("shift", None), Some(0x2a));
        assert_eq!(event_scancode("a", Some("A")), Some(0x1e));
        assert_eq!(event_scancode("a", None), Some(0x1e));
        // 本地布局把某个键映射成别的字符：扫描码会打错字，改走 Unicode。
        assert_eq!(event_scancode("e", Some("é")), None);
        assert_eq!(event_char("e", Some("é")), Some('é'));
        // Shift 上的符号同样走 Unicode。
        assert_eq!(event_scancode("!", Some("!")), None);
        assert_eq!(event_char("!", Some("!")), Some('!'));
        assert_eq!(event_char("中", Some("中")), Some('中'));
        assert_eq!(event_char("a", Some("A")), None);
        assert_eq!(event_char("escape", None), None);
        assert_eq!(event_scancode("", None), None);
    }
}
