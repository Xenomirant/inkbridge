//! Linux keyboard mapping: tablet keys and text to evdev key codes, shared
//! by the uinput keyboard and the portal (which also takes evdev codes).
//!
//! Linux injects keys, not characters: the desktop's active layout turns
//! them into text. Keys and shortcuts work on any layout (a shortcut letter
//! uses its key on the US layout, as on macOS). Text is typed for the
//! characters of the US layout only, which matches the text on a US-like
//! layout; other characters (accents, Cyrillic, CJK) are skipped. Typing
//! any character needs a different route (an input method, or the
//! clipboard), not done yet.
//!
//! The Windows key of the tablet's key row is Super; alone it opens the
//! GNOME overview, as Start opens on Windows.

use evdev::KeyCode;
use ib_protocol::{Key, Modifiers};

/// One key event: an evdev key code, down or up.
pub(crate) type Stroke = (KeyCode, bool);

/// Every key the mappings below can produce (for the virtual keyboard's
/// capabilities).
pub(crate) fn all_keys() -> Vec<KeyCode> {
    let mut keys: Vec<KeyCode> = US_PLAIN.iter().map(|&(_, k)| k).collect();
    keys.extend([
        KeyCode::KEY_ENTER,
        KeyCode::KEY_BACKSPACE,
        KeyCode::KEY_DELETE,
        KeyCode::KEY_TAB,
        KeyCode::KEY_ESC,
        KeyCode::KEY_UP,
        KeyCode::KEY_DOWN,
        KeyCode::KEY_LEFT,
        KeyCode::KEY_RIGHT,
        KeyCode::KEY_HOME,
        KeyCode::KEY_END,
        KeyCode::KEY_PAGEUP,
        KeyCode::KEY_PAGEDOWN,
        KeyCode::KEY_LEFTSHIFT,
        KeyCode::KEY_LEFTCTRL,
        KeyCode::KEY_LEFTALT,
        KeyCode::KEY_LEFTMETA,
    ]);
    keys.extend(F_KEYS);
    keys
}

const F_KEYS: [KeyCode; 12] = [
    KeyCode::KEY_F1,
    KeyCode::KEY_F2,
    KeyCode::KEY_F3,
    KeyCode::KEY_F4,
    KeyCode::KEY_F5,
    KeyCode::KEY_F6,
    KeyCode::KEY_F7,
    KeyCode::KEY_F8,
    KeyCode::KEY_F9,
    KeyCode::KEY_F10,
    KeyCode::KEY_F11,
    KeyCode::KEY_F12,
];

/// Characters typed without Shift on the US layout, and their keys.
const US_PLAIN: &[(char, KeyCode)] = &[
    ('a', KeyCode::KEY_A),
    ('b', KeyCode::KEY_B),
    ('c', KeyCode::KEY_C),
    ('d', KeyCode::KEY_D),
    ('e', KeyCode::KEY_E),
    ('f', KeyCode::KEY_F),
    ('g', KeyCode::KEY_G),
    ('h', KeyCode::KEY_H),
    ('i', KeyCode::KEY_I),
    ('j', KeyCode::KEY_J),
    ('k', KeyCode::KEY_K),
    ('l', KeyCode::KEY_L),
    ('m', KeyCode::KEY_M),
    ('n', KeyCode::KEY_N),
    ('o', KeyCode::KEY_O),
    ('p', KeyCode::KEY_P),
    ('q', KeyCode::KEY_Q),
    ('r', KeyCode::KEY_R),
    ('s', KeyCode::KEY_S),
    ('t', KeyCode::KEY_T),
    ('u', KeyCode::KEY_U),
    ('v', KeyCode::KEY_V),
    ('w', KeyCode::KEY_W),
    ('x', KeyCode::KEY_X),
    ('y', KeyCode::KEY_Y),
    ('z', KeyCode::KEY_Z),
    ('1', KeyCode::KEY_1),
    ('2', KeyCode::KEY_2),
    ('3', KeyCode::KEY_3),
    ('4', KeyCode::KEY_4),
    ('5', KeyCode::KEY_5),
    ('6', KeyCode::KEY_6),
    ('7', KeyCode::KEY_7),
    ('8', KeyCode::KEY_8),
    ('9', KeyCode::KEY_9),
    ('0', KeyCode::KEY_0),
    ('-', KeyCode::KEY_MINUS),
    ('=', KeyCode::KEY_EQUAL),
    ('[', KeyCode::KEY_LEFTBRACE),
    (']', KeyCode::KEY_RIGHTBRACE),
    ('\\', KeyCode::KEY_BACKSLASH),
    (';', KeyCode::KEY_SEMICOLON),
    ('\'', KeyCode::KEY_APOSTROPHE),
    ('`', KeyCode::KEY_GRAVE),
    (',', KeyCode::KEY_COMMA),
    ('.', KeyCode::KEY_DOT),
    ('/', KeyCode::KEY_SLASH),
    (' ', KeyCode::KEY_SPACE),
];

/// Characters typed with Shift on the US layout, and the unshifted
/// character of the same key.
const US_SHIFTED: &[(char, char)] = &[
    ('!', '1'),
    ('@', '2'),
    ('#', '3'),
    ('$', '4'),
    ('%', '5'),
    ('^', '6'),
    ('&', '7'),
    ('*', '8'),
    ('(', '9'),
    (')', '0'),
    ('_', '-'),
    ('+', '='),
    ('{', '['),
    ('}', ']'),
    ('|', '\\'),
    (':', ';'),
    ('"', '\''),
    ('~', '`'),
    ('<', ','),
    ('>', '.'),
    ('?', '/'),
];

/// The US-layout key that types `c`, and whether Shift is part of it.
fn us_key(c: char) -> Option<(KeyCode, bool)> {
    let plain = |c: char| {
        US_PLAIN
            .iter()
            .find(|(k, _)| *k == c)
            .map(|&(_, code)| code)
    };
    if let Some(code) = plain(c) {
        return Some((code, false));
    }
    if c.is_ascii_uppercase() {
        return plain(c.to_ascii_lowercase()).map(|code| (code, true));
    }
    let &(_, base) = US_SHIFTED.iter().find(|(k, _)| *k == c)?;
    plain(base).map(|code| (code, true))
}

/// The key for a tablet key that has no character.
fn named_key(key: Key) -> Option<KeyCode> {
    Some(match key {
        Key::Enter => KeyCode::KEY_ENTER,
        Key::Backspace => KeyCode::KEY_BACKSPACE,
        Key::Delete => KeyCode::KEY_DELETE,
        Key::Tab => KeyCode::KEY_TAB,
        Key::Escape => KeyCode::KEY_ESC,
        Key::Up => KeyCode::KEY_UP,
        Key::Down => KeyCode::KEY_DOWN,
        Key::Left => KeyCode::KEY_LEFT,
        Key::Right => KeyCode::KEY_RIGHT,
        Key::Home => KeyCode::KEY_HOME,
        Key::End => KeyCode::KEY_END,
        Key::PageUp => KeyCode::KEY_PAGEUP,
        Key::PageDown => KeyCode::KEY_PAGEDOWN,
        Key::Meta => KeyCode::KEY_LEFTMETA,
        Key::Function(n @ 1..=12) => F_KEYS[usize::from(n) - 1],
        Key::Function(_) | Key::Char(_) => return None,
    })
}

/// Modifier keys in press order.
fn modifier_keys(modifiers: Modifiers) -> Vec<KeyCode> {
    [
        (Modifiers::CTRL, KeyCode::KEY_LEFTCTRL),
        (Modifiers::ALT, KeyCode::KEY_LEFTALT),
        (Modifiers::SHIFT, KeyCode::KEY_LEFTSHIFT),
        (Modifiers::META, KeyCode::KEY_LEFTMETA),
    ]
    .into_iter()
    .filter(|&(m, _)| modifiers.contains(m))
    .map(|(_, k)| k)
    .collect()
}

/// `key` pressed and released inside `held` (pressed first, released in
/// reverse order).
fn press(key: KeyCode, held: &[KeyCode]) -> Vec<Stroke> {
    let mut s: Vec<Stroke> = held.iter().map(|&k| (k, true)).collect();
    s.push((key, true));
    s.push((key, false));
    s.extend(held.iter().rev().map(|&k| (k, false)));
    s
}

/// The strokes for `key` with `modifiers` held; `None` if it can't be typed.
pub(crate) fn key_strokes(key: Key, modifiers: Modifiers) -> Option<Vec<Stroke>> {
    let mut held = modifier_keys(modifiers);
    let code = match key {
        Key::Char(c) => {
            let (code, shift) = us_key(c)?;
            if shift && !held.contains(&KeyCode::KEY_LEFTSHIFT) {
                held.push(KeyCode::KEY_LEFTSHIFT);
            }
            code
        }
        other => named_key(other)?,
    };
    // Super with Super held would press it twice.
    held.retain(|&k| k != code);
    Some(press(code, &held))
}

/// The strokes that type `text`, and how many characters had no key.
pub(crate) fn text_strokes(text: &str) -> (Vec<Stroke>, usize) {
    let mut strokes = Vec::new();
    let mut skipped = 0;
    for c in text.chars() {
        match c {
            '\n' => strokes.extend(press(KeyCode::KEY_ENTER, &[])),
            '\t' => strokes.extend(press(KeyCode::KEY_TAB, &[])),
            '\r' => {}
            c => match us_key(c) {
                Some((code, true)) => strokes.extend(press(code, &[KeyCode::KEY_LEFTSHIFT])),
                Some((code, false)) => strokes.extend(press(code, &[])),
                None => skipped += 1,
            },
        }
    }
    (strokes, skipped)
}

pub(crate) fn unknown_key(key: Key) -> crate::InjectError {
    crate::InjectError::Rejected(format!("no key for {key:?} on Linux"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIFT: KeyCode = KeyCode::KEY_LEFTSHIFT;
    const CTRL: KeyCode = KeyCode::KEY_LEFTCTRL;

    #[test]
    fn text_types_us_characters_with_shift_where_needed() {
        let (s, skipped) = text_strokes("aB!\n");
        assert_eq!(skipped, 0);
        assert_eq!(
            s,
            vec![
                (KeyCode::KEY_A, true),
                (KeyCode::KEY_A, false),
                (SHIFT, true),
                (KeyCode::KEY_B, true),
                (KeyCode::KEY_B, false),
                (SHIFT, false),
                (SHIFT, true),
                (KeyCode::KEY_1, true),
                (KeyCode::KEY_1, false),
                (SHIFT, false),
                (KeyCode::KEY_ENTER, true),
                (KeyCode::KEY_ENTER, false),
            ]
        );
    }

    #[test]
    fn characters_without_a_us_key_are_skipped() {
        let (s, skipped) = text_strokes("é Ж\r");
        assert_eq!(skipped, 2);
        assert_eq!(s, press(KeyCode::KEY_SPACE, &[]));
    }

    #[test]
    fn shortcuts_hold_modifiers_around_the_key() {
        assert_eq!(
            key_strokes(Key::Char('c'), Modifiers::CTRL),
            Some(vec![
                (CTRL, true),
                (KeyCode::KEY_C, true),
                (KeyCode::KEY_C, false),
                (CTRL, false),
            ])
        );
        // A shifted character adds Shift once, even when Shift is held.
        let s = key_strokes(Key::Char('Z'), Modifiers::CTRL.with(Modifiers::SHIFT)).unwrap();
        assert_eq!(s.iter().filter(|(k, down)| *k == SHIFT && *down).count(), 1);
    }

    #[test]
    fn named_keys_and_meta() {
        assert_eq!(
            key_strokes(Key::Function(5), Modifiers::NONE),
            Some(press(KeyCode::KEY_F5, &[]))
        );
        assert_eq!(key_strokes(Key::Function(13), Modifiers::NONE), None);
        // Super alone (opens the overview), not pressed twice with META held.
        assert_eq!(
            key_strokes(Key::Meta, Modifiers::META),
            Some(press(KeyCode::KEY_LEFTMETA, &[]))
        );
    }

    #[test]
    fn every_produced_key_is_declared() {
        let declared = all_keys();
        let mut produced: Vec<KeyCode> = (32u8..127)
            .map(char::from)
            .flat_map(|c| text_strokes(&c.to_string()).0)
            .map(|(k, _)| k)
            .collect();
        for key in [Key::Enter, Key::Meta, Key::PageDown, Key::Function(12)] {
            produced.extend(
                key_strokes(key, Modifiers(15))
                    .unwrap()
                    .into_iter()
                    .map(|(k, _)| k),
            );
        }
        for k in produced {
            assert!(declared.contains(&k), "{k:?} not declared");
        }
    }
}
