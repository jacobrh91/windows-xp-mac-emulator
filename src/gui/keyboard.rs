//! The PC keyboard as Windows describes it: each virtual key's scan code (set 1) and
//! whether it is an "extended" key, which go into the lParam of key messages; and the names
//! GetKeyNameText gives keys.

/// (scan code, extended) for a virtual key; None for keys without one.
pub fn scan_code(vk: u32) -> Option<(u32, bool)> {
    const LETTERS: [u32; 26] = [
        0x1E, 0x30, 0x2E, 0x20, 0x12, 0x21, 0x22, 0x23, 0x17, 0x24, 0x25, 0x26, 0x32, 0x31, 0x18,
        0x19, 0x10, 0x13, 0x1F, 0x14, 0x16, 0x2F, 0x11, 0x2D, 0x15, 0x2C,
    ];
    const NUMPAD: [u32; 10] = [0x52, 0x4F, 0x50, 0x51, 0x4B, 0x4C, 0x4D, 0x47, 0x48, 0x49];
    Some(match vk {
        0x41..=0x5A => (LETTERS[(vk - 0x41) as usize], false),
        0x31..=0x39 => (vk - 0x31 + 0x02, false),
        0x30 => (0x0B, false),
        0x70..=0x79 => (vk - 0x70 + 0x3B, false), // F1-F10
        0x7A => (0x57, false),
        0x7B => (0x58, false),
        0x60..=0x69 => (NUMPAD[(vk - 0x60) as usize], false),
        0x1B => (0x01, false),
        0xBD => (0x0C, false), // -
        0xBB => (0x0D, false), // =
        0x08 => (0x0E, false),
        0x09 => (0x0F, false),
        0xDB => (0x1A, false), // [
        0xDD => (0x1B, false), // ]
        0x0D => (0x1C, false),
        0x11 | 0xA2 => (0x1D, false),
        0xA3 => (0x1D, true),
        0xBA => (0x27, false), // ;
        0xDE => (0x28, false), // '
        0xC0 => (0x29, false), // `
        0x10 | 0xA0 => (0x2A, false),
        0xDC => (0x2B, false), // \
        0xBC => (0x33, false), // ,
        0xBE => (0x34, false), // .
        0xBF => (0x35, false), // /
        0xA1 => (0x36, false),
        0x6A => (0x37, false), // numpad *
        0x12 | 0xA4 => (0x38, false),
        0xA5 => (0x38, true),
        0x20 => (0x39, false),
        0x14 => (0x3A, false),
        0x90 => (0x45, true),
        0x91 => (0x46, false),
        0x6D => (0x4A, false), // numpad -
        0x6B => (0x4E, false), // numpad +
        0x6E => (0x53, false), // numpad .
        0x6F => (0x35, true),  // numpad /
        0x24 => (0x47, true),
        0x26 => (0x48, true),
        0x21 => (0x49, true),
        0x25 => (0x4B, true),
        0x27 => (0x4D, true),
        0x23 => (0x4F, true),
        0x28 => (0x50, true),
        0x22 => (0x51, true),
        0x2D => (0x52, true),
        0x2E => (0x53, true),
        0x5D => (0x5D, true),
        0x13 => (0x45, false), // pause
        _ => return None,
    })
}

/// The virtual key for a scan code: MapVirtualKey's MAPVK_VSC_TO_VK (generic modifiers) or,
/// with `sided`, MAPVK_VSC_TO_VK_EX (VK_LSHIFT and the like).
pub fn vk_of_scan(scan: u32, extended: bool, sided: bool) -> u32 {
    let generic = |vk: u32| match vk {
        0xA0 | 0xA1 => 0x10,
        0xA2 | 0xA3 => 0x11,
        0xA4 | 0xA5 => 0x12,
        v => v,
    };
    // Prefer the plain keys (letters, digits, the editing block) over numpad twins.
    let candidates = (0x08..=0xDEu32).filter(|&vk| !(0x10..=0x12).contains(&vk));
    let mut fallback = 0;
    for vk in candidates {
        if let Some((s, ext)) = scan_code(vk)
            && s == scan
        {
            if ext == extended {
                return if sided { vk } else { generic(vk) };
            }
            if fallback == 0 {
                fallback = if sided { vk } else { generic(vk) };
            }
        }
    }
    fallback
}

/// The character a virtual key types, unshifted (MapVirtualKey's MAPVK_VK_TO_CHAR).
pub fn char_of(vk: u32) -> u32 {
    match vk {
        0x30..=0x39 | 0x41..=0x5A => vk,
        0x60..=0x69 => vk - 0x60 + b'0' as u32,
        0x20 => b' ' as u32,
        0x0D => b'\r' as u32,
        0x08 => 8,
        0x09 => 9,
        0x1B => 0x1B,
        0xBD => b'-' as u32,
        0xBB => b'=' as u32,
        0xDB => b'[' as u32,
        0xDD => b']' as u32,
        0xBA => b';' as u32,
        0xDE => b'\'' as u32,
        0xC0 => b'`' as u32,
        0xDC => b'\\' as u32,
        0xBC => b',' as u32,
        0xBE => b'.' as u32,
        0xBF | 0x6F => b'/' as u32,
        0x6A => b'*' as u32,
        0x6D => b'-' as u32,
        0x6B => b'+' as u32,
        0x6E => b'.' as u32,
        _ => 0,
    }
}

/// GetKeyNameText's name for a key, from the scan code and extended flag of a key lParam.
pub fn key_name(scan: u32, extended: bool) -> Option<String> {
    let vk = vk_of_scan(scan, extended, true);
    let name = match vk {
        0x41..=0x5A | 0x30..=0x39 => return Some(char::from_u32(vk)?.to_string()),
        0x70..=0x7B => return Some(format!("F{}", vk - 0x6F)),
        0x60..=0x69 => return Some(format!("Num {}", vk - 0x60)),
        0x08 => "Backspace",
        0x09 => "Tab",
        0x0D if extended => "Num Enter",
        0x0D => "Enter",
        0x13 => "Pause",
        0x14 => "Caps Lock",
        0x1B => "Esc",
        0x20 => "Space",
        0x21 => "Page Up",
        0x22 => "Page Down",
        0x23 => "End",
        0x24 => "Home",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        0x2D => "Insert",
        0x2E => "Delete",
        0x5D => "Application",
        0x6A => "Num *",
        0x6B => "Num +",
        0x6D => "Num -",
        0x6E => "Num Del",
        0x6F => "Num /",
        0x90 => "Num Lock",
        0x91 => "Scroll Lock",
        0xA0 => "Shift",
        0xA1 => "Right Shift",
        0xA2 => "Ctrl",
        0xA3 => "Right Ctrl",
        0xA4 => "Alt",
        0xA5 => "Right Alt",
        _ => {
            return char::from_u32(char_of(vk))
                .filter(|c| c.is_ascii_graphic())
                .map(|c| c.to_string());
        }
    };
    Some(name.to_string())
}

/// The lParam of WM_KEYDOWN / WM_KEYUP: repeat count 1, scan code, extended flag, the
/// Alt context bit, whether the key was already down, and the transition (released).
pub fn key_lparam(vk: u32, extended: bool, down: bool, was_down: bool, alt: bool) -> u32 {
    let scan = scan_code(vk).map_or(0, |s| s.0);
    1 | scan << 16
        | (extended as u32) << 24
        | (alt as u32) << 29
        | ((was_down || !down) as u32) << 30
        | (!down as u32) << 31
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_codes_and_names_round_trip() {
        assert_eq!(scan_code(0x4D), Some((0x32, false)), "M");
        assert_eq!(vk_of_scan(0x32, false, false), 0x4D);
        assert_eq!(key_name(0x32, false).as_deref(), Some("M"));
        assert_eq!(key_name(0x1D, false).as_deref(), Some("Ctrl"));
        assert_eq!(key_name(0x48, true).as_deref(), Some("Up"));
        assert_eq!(
            key_name(0x48, false).as_deref(),
            Some("Num 8"),
            "the numpad twin of Up"
        );
        assert_eq!(vk_of_scan(0x2A, false, true), 0xA0, "left shift, sided");
        assert_eq!(vk_of_scan(0x2A, false, false), 0x10);
    }

    #[test]
    fn key_up_lparam_has_the_previous_state_and_transition_bits() {
        let down = key_lparam(0x4D, false, true, false, false);
        assert_eq!(down, 0x0032_0001);
        let repeat = key_lparam(0x4D, false, true, true, false);
        assert_eq!(repeat >> 30, 1, "auto-repeat: was already down");
        let up = key_lparam(0x4D, false, false, true, false);
        assert_eq!(up >> 30, 3, "WM_KEYUP: previous state and transition set");
        assert_eq!(
            key_lparam(0x25, true, true, false, false) >> 24 & 1,
            1,
            "arrows are extended"
        );
    }
}
