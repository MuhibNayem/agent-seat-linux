/// X keysym for a named key.
pub(crate) fn keysym_for_key(name: &str) -> Option<u32> {
    let trimmed = name.trim();
    if trimmed.len() == 1 {
        let character = trimmed.chars().next()?;
        return Some(if character.is_ascii_uppercase() {
            character as u32
        } else {
            character.to_ascii_lowercase() as u32
        });
    }

    let normalized = trimmed.to_lowercase();
    let symbol = match normalized.as_str() {
        "return" | "enter" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" | "del" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" | "page_up" => 0xff55,
        "pagedown" | "page_down" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "shift" | "shiftleft" | "shift_left" => 0xffe1,
        "shiftright" | "shift_right" => 0xffe2,
        "control" | "ctrl" | "controlleft" | "control_left" => 0xffe3,
        "controlright" | "control_right" => 0xffe4,
        "alt" | "altleft" | "alt_left" | "option" => 0xffe9,
        "altright" | "alt_right" => 0xffea,
        "super" | "cmd" | "command" | "meta" | "superleft" | "super_left" => 0xffeb,
        "superright" | "super_right" => 0xffec,
        "space" => 0x0020,
        "capslock" | "caps_lock" => 0xffe5,
        _ => {
            if let Some(number) = normalized
                .strip_prefix('f')
                .and_then(|value| value.parse::<u32>().ok())
            {
                if (1..=24).contains(&number) {
                    return Some(0xffbe + number - 1);
                }
            }
            return None;
        }
    };
    Some(symbol)
}

#[cfg(test)]
mod tests {
    use super::keysym_for_key;

    #[test]
    fn common_keys_are_mapped() {
        assert_eq!(keysym_for_key("Enter"), Some(0xff0d));
        assert_eq!(keysym_for_key("Ctrl"), Some(0xffe3));
        assert_eq!(keysym_for_key("F12"), Some(0xffc9));
        assert_eq!(keysym_for_key("A"), Some(u32::from(b'A')));
        assert_eq!(keysym_for_key("unknown"), None);
    }
}
