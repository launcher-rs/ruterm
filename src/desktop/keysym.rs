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

/// 一次按键要发的事件序列 `(keysym, 是否按下)`。
///
/// 修饰键必须显式按下再松开，服务端才会把 Ctrl / Alt 算进组合键；
/// `key_char` 与 `key` 只差大小写时按 Shift 组合发，其它情况（输入法字符、
/// 非拉丁字符）直接发该字符的 keysym，避免多按一个 Shift 把字符打歪。
pub fn keystroke_keysyms(
    key: &str,
    key_char: Option<&str>,
    ctrl: bool,
    alt: bool,
    shift: bool,
    platform: bool,
) -> Vec<(u32, bool)> {
    let lowered = key.to_ascii_lowercase();
    let mut typed: Option<u32> = None;
    let mut shifted = shift;
    if let Some(text) = key_char.and_then(|text| text.chars().next()) {
        if lowered.len() == 1 && text.to_ascii_lowercase().to_string() == lowered {
            shifted |= text.is_uppercase();
            typed = Some(char_keysym(text.to_ascii_lowercase()));
        } else {
            // 输入法 / 组合字符：直接发这个字符，Shift 不再叠加。
            shifted = false;
            typed = Some(char_keysym(text));
        }
    }
    let keysym = typed
        .or_else(|| {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) => Some(char_keysym(ch)),
                _ => named_keysym(key),
            }
        })
        .unwrap_or_else(|| char_keysym(key.chars().next().unwrap_or(' ')));

    let mut modifiers = Vec::new();
    if ctrl {
        modifiers.push(CONTROL_L);
    }
    if alt {
        modifiers.push(ALT_L);
    }
    if platform {
        modifiers.push(SUPER_L);
    }
    if shifted {
        modifiers.push(SHIFT_L);
    }

    let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
    for keysym in &modifiers {
        events.push((*keysym, true));
    }
    events.push((keysym, true));
    events.push((keysym, false));
    for keysym in modifiers.iter().rev() {
        events.push((*keysym, false));
    }
    events
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
    fn ctrl_combo_presses_and_releases_modifier_around_key() {
        let events = keystroke_keysyms("c", Some("c"), true, false, false, false);
        assert_eq!(
            events,
            vec![(CONTROL_L, true), (0x63, true), (0x63, false), (CONTROL_L, false)]
        );
    }

    #[test]
    fn uppercase_char_implies_shift_without_double_shift() {
        let events = keystroke_keysyms("a", Some("A"), false, false, true, false);
        assert_eq!(
            events,
            vec![(SHIFT_L, true), (0x61, true), (0x61, false), (SHIFT_L, false)]
        );
    }

    #[test]
    fn ime_char_is_sent_directly() {
        let events = keystroke_keysyms("中", Some("中"), false, false, false, false);
        assert_eq!(events, vec![(0x0100_4e2d, true), (0x0100_4e2d, false)]);
    }

    #[test]
    fn named_key_without_char() {
        assert_eq!(
            keystroke_keysyms("enter", None, false, false, false, false),
            vec![(0xff0d, true), (0xff0d, false)]
        );
    }
}
