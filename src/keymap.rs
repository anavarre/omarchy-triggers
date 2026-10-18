use evdev::Key;

/// US layout: key -> (unshifted, shifted).
const KEYS: &[(Key, char, char)] = &[
    (Key::KEY_A, 'a', 'A'), (Key::KEY_B, 'b', 'B'), (Key::KEY_C, 'c', 'C'),
    (Key::KEY_D, 'd', 'D'), (Key::KEY_E, 'e', 'E'), (Key::KEY_F, 'f', 'F'),
    (Key::KEY_G, 'g', 'G'), (Key::KEY_H, 'h', 'H'), (Key::KEY_I, 'i', 'I'),
    (Key::KEY_J, 'j', 'J'), (Key::KEY_K, 'k', 'K'), (Key::KEY_L, 'l', 'L'),
    (Key::KEY_M, 'm', 'M'), (Key::KEY_N, 'n', 'N'), (Key::KEY_O, 'o', 'O'),
    (Key::KEY_P, 'p', 'P'), (Key::KEY_Q, 'q', 'Q'), (Key::KEY_R, 'r', 'R'),
    (Key::KEY_S, 's', 'S'), (Key::KEY_T, 't', 'T'), (Key::KEY_U, 'u', 'U'),
    (Key::KEY_V, 'v', 'V'), (Key::KEY_W, 'w', 'W'), (Key::KEY_X, 'x', 'X'),
    (Key::KEY_Y, 'y', 'Y'), (Key::KEY_Z, 'z', 'Z'),
    (Key::KEY_1, '1', '!'), (Key::KEY_2, '2', '@'), (Key::KEY_3, '3', '#'),
    (Key::KEY_4, '4', '$'), (Key::KEY_5, '5', '%'), (Key::KEY_6, '6', '^'),
    (Key::KEY_7, '7', '&'), (Key::KEY_8, '8', '*'), (Key::KEY_9, '9', '('),
    (Key::KEY_0, '0', ')'),
    (Key::KEY_MINUS, '-', '_'), (Key::KEY_EQUAL, '=', '+'),
    (Key::KEY_LEFTBRACE, '[', '{'), (Key::KEY_RIGHTBRACE, ']', '}'),
    (Key::KEY_BACKSLASH, '\\', '|'), (Key::KEY_SEMICOLON, ';', ':'),
    (Key::KEY_APOSTROPHE, '\'', '"'), (Key::KEY_GRAVE, '`', '~'),
    (Key::KEY_COMMA, ',', '<'), (Key::KEY_DOT, '.', '>'),
    (Key::KEY_SLASH, '/', '?'), (Key::KEY_SPACE, ' ', ' '),
];

pub fn to_char(key: Key, shift: bool, caps_lock: bool) -> Option<char> {
    let &(_, lower, upper) = KEYS.iter().find(|(k, _, _)| *k == key)?;
    let shifted = if lower.is_ascii_alphabetic() { shift ^ caps_lock } else { shift };
    Some(if shifted { upper } else { lower })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_shift_and_caps() {
        assert_eq!(to_char(Key::KEY_A, false, false), Some('a'));
        assert_eq!(to_char(Key::KEY_A, true, false), Some('A'));
        assert_eq!(to_char(Key::KEY_A, true, true), Some('a'));
        assert_eq!(to_char(Key::KEY_SEMICOLON, false, true), Some(';'));
        assert_eq!(to_char(Key::KEY_1, true, false), Some('!'));
        assert_eq!(to_char(Key::KEY_ENTER, false, false), None);
    }
}
