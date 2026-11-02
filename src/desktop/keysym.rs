//! X11 keysym：VNC 的按键事件用这套编码，而不是平台虚拟键码。
//!
//! 规则很省事：可打印 ASCII / Latin-1 的 keysym 就是码点本身；其余字符走
//! Unicode 区（`0x0100_0000 + 码点`），现代服务端（TigerVNC / x11vnc）都认。
//! 具名键按 rgpui `Keystroke.key` 的写法收，两边用同一套名字。

/// 左 Shift 的 keysym。
pub const SHIFT_L: u32 = 0xffe1;
/// 左 Control 的 keysym。
pub const CONTROL_L: u32 = 0xffe3;
/// 左 Alt 的 keysym。
pub const ALT_L: u32 = 0xffe9;
/// 左 Super（Windows 键）的 keysym。
pub const SUPER_L: u32 = 0xffeb;

/// 字符 → keysym。
pub fn char_keysym(ch: char) -> u32 {
    let code = ch as u32;
    if (0x20..=0x7e).contains(&code) || (0xa0..=0xff).contains(&code) {
        code
    } else {
        0x0100_0000 + code
    }
}

/// 具名键 → keysym；不认识的返回 `None`。
pub fn named_keysym(name: &str) -> Option<u32> {
    let keysym = match name.to_ascii_lowercase().as_str() {
        "space" => 0x0020,
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" | "ins" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" => 0xff55,
        "pagedown" | "page_down" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "pause" => 0xff13,
        "printscreen" | "print_screen" => 0xff61,
        "capslock" | "caps_lock" => 0xffe5,
        "numlock" | "num_lock" => 0xff7f,
        "menu" | "contextmenu" => 0xff67,
        "shift" => SHIFT_L,
        "control" | "ctrl" => CONTROL_L,
        "alt" | "option" => ALT_L,
        "super" | "cmd" | "win" | "platform" => SUPER_L,
        function if function.len() >= 2 && function.len() <= 3 => {
            let digit = function.strip_prefix('f')?.parse::<u8>().ok()?;
            if (1..=24).contains(&digit) {
                return Some(0xffbe + u32::from(digit - 1));
            }
            return None;
        }
        _ => return None,
    };
    Some(keysym)
}

/// 真实按下 / 抬起事件 → keysym（修饰键由服务端按事件顺序自己维护）。
///
/// 只翻译当前这一个键：上下行是一一对应的真实事件，不合成修饰键的按下 / 抬起来回。
pub fn event_keysym(key: &str, key_char: Option<&str>) -> Option<u32> {
    if let Some(keysym) = named_keysym(key) {
        return Some(keysym);
    }
    if let Some(ch) = key_char.and_then(|text| text.chars().next()) {
        // 与 key 只差大小写的字符按未按 Shift 的 keysym 发：Shift 的按下事件
        // 已经真实发过去了，服务端自己合成大小写；其余（输入法字符）原样发。
        let lowered = key.to_ascii_lowercase();
        if lowered.len() == 1 && ch.to_ascii_lowercase().to_string() == lowered {
            return Some(char_keysym(ch.to_ascii_lowercase()));
        }
        return Some(char_keysym(ch));
    }
    let mut chars = key.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if !ch.is_control() => Some(char_keysym(ch)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chars_map_to_ascii_latin1_or_unicode_keysyms() {
        assert_eq!(char_keysym('a'), 0x61);
        assert_eq!(char_keysym('~'), 0x7e);
        assert_eq!(char_keysym('é'), 0xe9);
        assert_eq!(char_keysym('中'), 0x0100_4e2d);
        assert_eq!(char_keysym('😀'), 0x0101_f600);
    }

    #[test]
    fn named_keys_cover_editing_and_function_rows() {
        assert_eq!(named_keysym("enter"), Some(0xff0d));
        assert_eq!(named_keysym("PageDown"), Some(0xff56));
        assert_eq!(named_keysym("f1"), Some(0xffbe));
        assert_eq!(named_keysym("f12"), Some(0xffc9));
        assert_eq!(named_keysym("f13"), Some(0xffca));
        assert_eq!(named_keysym("nonsense"), None);
        assert_eq!(named_keysym("f99"), None);
    }

    #[test]
    fn event_keysym_prefers_named_then_typed_char() {
        assert_eq!(event_keysym("shift", None), Some(SHIFT_L));
        assert_eq!(event_keysym("escape", None), Some(0xff1b));
        // Shift 的真实按下事件已经发过，这里发未 Shift 的 keysym，避免双 Shift。
        assert_eq!(event_keysym("a", Some("A")), Some(0x61));
        assert_eq!(event_keysym("a", None), Some(0x61));
        assert_eq!(event_keysym("中", Some("中")), Some(0x0100_4e2d));
        assert_eq!(event_keysym("", None), None);
    }
}
