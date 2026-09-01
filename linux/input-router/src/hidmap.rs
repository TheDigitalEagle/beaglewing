//! Linux evdev keycode -> USB HID usage mapping.
//!
//! Uses raw numeric Linux keycodes (input-event-codes.h) so the same table
//! serves both raw evdev capture and the InputCapture portal path, which
//! also delivers evdev keycodes.

/// Map a Linux keycode to a HID usage id (page 0x07).
/// Modifiers map to 0xE0..0xE7. Returns None for unmapped keys.
pub fn keycode_to_usage(code: u16) -> Option<u8> {
    Some(match code {
        1 => 0x29,   // ESC
        2..=10 => (code - 2) as u8 + 0x1e, // 1..9
        11 => 0x27,  // 0
        12 => 0x2d,  // -
        13 => 0x2e,  // =
        14 => 0x2a,  // Backspace
        15 => 0x2b,  // Tab
        16 => 0x14,  // q
        17 => 0x1a,  // w
        18 => 0x08,  // e
        19 => 0x15,  // r
        20 => 0x17,  // t
        21 => 0x1c,  // y
        22 => 0x18,  // u
        23 => 0x0c,  // i
        24 => 0x12,  // o
        25 => 0x13,  // p
        26 => 0x2f,  // [
        27 => 0x30,  // ]
        28 => 0x28,  // Enter
        29 => 0xe0,  // LCtrl
        30 => 0x04,  // a
        31 => 0x16,  // s
        32 => 0x07,  // d
        33 => 0x09,  // f
        34 => 0x0a,  // g
        35 => 0x0b,  // h
        36 => 0x0d,  // j
        37 => 0x0e,  // k
        38 => 0x0f,  // l
        39 => 0x33,  // ;
        40 => 0x34,  // '
        41 => 0x35,  // `
        42 => 0xe1,  // LShift
        43 => 0x31,  // backslash
        44 => 0x1d,  // z
        45 => 0x1b,  // x
        46 => 0x06,  // c
        47 => 0x19,  // v
        48 => 0x05,  // b
        49 => 0x11,  // n
        50 => 0x10,  // m
        51 => 0x36,  // ,
        52 => 0x37,  // .
        53 => 0x38,  // /
        54 => 0xe5,  // RShift
        55 => 0x55,  // KP *
        56 => 0xe2,  // LAlt
        57 => 0x2c,  // Space
        58 => 0x39,  // CapsLock
        59..=68 => (code - 59) as u8 + 0x3a, // F1..F10
        69 => 0x53,  // NumLock
        70 => 0x47,  // ScrollLock
        71..=73 => (code - 71) as u8 + 0x5f, // KP 7 8 9
        74 => 0x56,  // KP -
        75..=77 => (code - 75) as u8 + 0x5c, // KP 4 5 6
        78 => 0x57,  // KP +
        79..=81 => (code - 79) as u8 + 0x59, // KP 1 2 3
        82 => 0x62,  // KP 0
        83 => 0x63,  // KP .
        86 => 0x64,  // ISO extra key (102nd)
        87 => 0x44,  // F11
        88 => 0x45,  // F12
        96 => 0x58,  // KP Enter
        97 => 0xe4,  // RCtrl
        98 => 0x54,  // KP /
        99 => 0x46,  // PrintScreen/SysRq
        100 => 0xe6, // RAlt
        102 => 0x4a, // Home
        103 => 0x52, // Up
        104 => 0x4b, // PageUp
        105 => 0x50, // Left
        106 => 0x4f, // Right
        107 => 0x4d, // End
        108 => 0x51, // Down
        109 => 0x4e, // PageDown
        110 => 0x49, // Insert
        111 => 0x4c, // Delete
        119 => 0x48, // Pause
        125 => 0xe3, // LMeta (Super)
        126 => 0xe7, // RMeta
        127 => 0x65, // Menu/Compose
        _ => return None,
    })
}

/// Mouse button evdev codes -> protocol button bit.
pub fn button_to_bit(code: u16) -> Option<u8> {
    Some(match code {
        0x110 => 0x01, // BTN_LEFT
        0x111 => 0x02, // BTN_RIGHT
        0x112 => 0x04, // BTN_MIDDLE
        _ => return None,
    })
}

pub fn is_modifier_usage(usage: u8) -> bool {
    (0xe0..=0xe7).contains(&usage)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_digits_rows() {
        assert_eq!(keycode_to_usage(30), Some(0x04)); // a
        assert_eq!(keycode_to_usage(50), Some(0x10)); // m
        assert_eq!(keycode_to_usage(2), Some(0x1e));  // 1
        assert_eq!(keycode_to_usage(11), Some(0x27)); // 0
    }

    #[test]
    fn modifiers_and_ranges() {
        assert_eq!(keycode_to_usage(29), Some(0xe0));
        assert_eq!(keycode_to_usage(126), Some(0xe7));
        assert!(is_modifier_usage(0xe3));
        assert!(!is_modifier_usage(0x04));
        assert_eq!(keycode_to_usage(59), Some(0x3a)); // F1
        assert_eq!(keycode_to_usage(68), Some(0x43)); // F10
        assert_eq!(keycode_to_usage(88), Some(0x45)); // F12
        assert_eq!(keycode_to_usage(71), Some(0x5f)); // KP7
        assert_eq!(keycode_to_usage(82), Some(0x62)); // KP0
    }

    #[test]
    fn unmapped_returns_none() {
        assert_eq!(keycode_to_usage(240), None);
        assert_eq!(button_to_bit(0x113), None); // BTN_SIDE unsupported for now
    }

    #[test]
    fn buttons() {
        assert_eq!(button_to_bit(0x110), Some(0x01));
        assert_eq!(button_to_bit(0x111), Some(0x02));
        assert_eq!(button_to_bit(0x112), Some(0x04));
    }
}
