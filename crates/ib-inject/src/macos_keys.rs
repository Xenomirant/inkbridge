//! macOS keyboard injection with `CGEvent` (needs the same Accessibility
//! permission as the pen).
//!
//! Text is typed as characters (`CGEventKeyboardSetUnicodeString`), so it
//! doesn't depend on the Mac's keyboard layout and any language works.
//! Keys and shortcuts are virtual key codes. A character in a shortcut uses
//! its key on the US layout: macOS matches Command shortcuts through a
//! Latin layout even while a Cyrillic or other non-Latin layout is active.
//! (On a Latin layout that moves letters, such as AZERTY, a shortcut letter
//! lands on the US position.)
//!
//! The Windows key of the tablet's key row is Command here. Pressed alone it
//! opens Spotlight (Command-Space), as it opens Start on Windows.

use ib_protocol::{Key, Modifiers};

use crate::InjectError;

/// One key event: a virtual key code, down or up, with the modifier flags
/// (`CGEventFlags` bits) the event carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stroke {
    pub code: u16,
    pub down: bool,
    pub flags: u64,
}

/// What typing a piece of text takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TextPiece {
    /// A key press and release (Enter, Tab: apps expect the keys).
    Key(u16),
    /// One character as UTF-16 (one or two units).
    Char(Vec<u16>),
}

// `CGEventFlags` bits.
const SHIFT: u64 = 0x0002_0000;
const CONTROL: u64 = 0x0004_0000;
const OPTION: u64 = 0x0008_0000;
const COMMAND: u64 = 0x0010_0000;
const NUMERIC_PAD: u64 = 0x0020_0000;
const SECONDARY_FN: u64 = 0x0080_0000;

// Virtual key codes (`kVK_*` in HIToolbox's Events.h).
pub(crate) const RETURN: u16 = 0x24;
pub(crate) const TAB: u16 = 0x30;
const SPACE: u16 = 0x31;
const BACKSPACE: u16 = 0x33;
const ESCAPE: u16 = 0x35;
const COMMAND_KEY: u16 = 0x37;
const SHIFT_KEY: u16 = 0x38;
const OPTION_KEY: u16 = 0x3A;
const CONTROL_KEY: u16 = 0x3B;
const FORWARD_DELETE: u16 = 0x75;
const HOME: u16 = 0x73;
const PAGE_UP: u16 = 0x74;
const END: u16 = 0x77;
const PAGE_DOWN: u16 = 0x79;
const LEFT: u16 = 0x7B;
const RIGHT: u16 = 0x7C;
const DOWN: u16 = 0x7D;
const UP: u16 = 0x7E;
const F_KEYS: [u16; 12] = [
    0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F,
];

/// The pieces that type `text`, one character per event: some apps read
/// only the first character of an event that carries several.
pub(crate) fn text_pieces(text: &str) -> Vec<TextPiece> {
    text.chars()
        .filter_map(|c| match c {
            '\n' => Some(TextPiece::Key(RETURN)),
            '\t' => Some(TextPiece::Key(TAB)),
            '\r' => None,
            c => {
                let mut units = [0u16; 2];
                Some(TextPiece::Char(c.encode_utf16(&mut units).to_vec()))
            }
        })
        .collect()
}

/// Key code and the flags a real keyboard sends with it, for a key
/// without a character.
fn named_key(key: Key) -> Option<(u16, u64)> {
    Some(match key {
        Key::Enter => (RETURN, 0),
        Key::Backspace => (BACKSPACE, 0),
        Key::Delete => (FORWARD_DELETE, SECONDARY_FN),
        Key::Tab => (TAB, 0),
        Key::Escape => (ESCAPE, 0),
        Key::Up => (UP, SECONDARY_FN | NUMERIC_PAD),
        Key::Down => (DOWN, SECONDARY_FN | NUMERIC_PAD),
        Key::Left => (LEFT, SECONDARY_FN | NUMERIC_PAD),
        Key::Right => (RIGHT, SECONDARY_FN | NUMERIC_PAD),
        Key::Home => (HOME, SECONDARY_FN),
        Key::End => (END, SECONDARY_FN),
        Key::PageUp => (PAGE_UP, SECONDARY_FN),
        Key::PageDown => (PAGE_DOWN, SECONDARY_FN),
        // Command alone does nothing on a Mac; Spotlight is its Start menu.
        Key::Meta => (SPACE, 0),
        Key::Function(n @ 1..=12) => (F_KEYS[usize::from(n) - 1], SECONDARY_FN),
        Key::Function(_) | Key::Char(_) => return None,
    })
}

/// The US-layout key that types `c`, and whether Shift is part of it.
fn us_key(c: char) -> Option<(u16, bool)> {
    const PLAIN: &[(char, u16)] = &[
        ('a', 0x00),
        ('s', 0x01),
        ('d', 0x02),
        ('f', 0x03),
        ('h', 0x04),
        ('g', 0x05),
        ('z', 0x06),
        ('x', 0x07),
        ('c', 0x08),
        ('v', 0x09),
        ('b', 0x0B),
        ('q', 0x0C),
        ('w', 0x0D),
        ('e', 0x0E),
        ('r', 0x0F),
        ('y', 0x10),
        ('t', 0x11),
        ('1', 0x12),
        ('2', 0x13),
        ('3', 0x14),
        ('4', 0x15),
        ('6', 0x16),
        ('5', 0x17),
        ('=', 0x18),
        ('9', 0x19),
        ('7', 0x1A),
        ('-', 0x1B),
        ('8', 0x1C),
        ('0', 0x1D),
        (']', 0x1E),
        ('o', 0x1F),
        ('u', 0x20),
        ('[', 0x21),
        ('i', 0x22),
        ('p', 0x23),
        ('l', 0x25),
        ('j', 0x26),
        ('\'', 0x27),
        ('k', 0x28),
        (';', 0x29),
        ('\\', 0x2A),
        (',', 0x2B),
        ('/', 0x2C),
        ('n', 0x2D),
        ('m', 0x2E),
        ('.', 0x2F),
        (' ', SPACE),
        ('`', 0x32),
    ];
    const SHIFTED: &[(char, char)] = &[
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
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
        ('~', '`'),
    ];
    let find = |c: char| PLAIN.iter().find(|(k, _)| *k == c).map(|&(_, code)| code);
    if let Some(code) = find(c) {
        return Some((code, false));
    }
    if c.is_ascii_uppercase() {
        return find(c.to_ascii_lowercase()).map(|code| (code, true));
    }
    let &(_, base) = SHIFTED.iter().find(|(k, _)| *k == c)?;
    find(base).map(|code| (code, true))
}

/// The events for `key` with `modifiers` held: the modifier keys go down,
/// the key is pressed and released with their flags, and they come up in
/// reverse order. `None` if the key can't be typed.
pub(crate) fn key_strokes(key: Key, modifiers: Modifiers) -> Option<Vec<Stroke>> {
    let (code, extra, shift) = match key {
        Key::Char(c) => {
            let (code, shift) = us_key(c)?;
            (code, 0, shift)
        }
        named => {
            let (code, extra) = named_key(named)?;
            (code, extra, false)
        }
    };
    let mut held: Vec<(u16, u64)> = Vec::with_capacity(4);
    if modifiers.contains(Modifiers::CTRL) {
        held.push((CONTROL_KEY, CONTROL));
    }
    if modifiers.contains(Modifiers::ALT) {
        held.push((OPTION_KEY, OPTION));
    }
    if shift || modifiers.contains(Modifiers::SHIFT) {
        held.push((SHIFT_KEY, SHIFT));
    }
    if modifiers.contains(Modifiers::META) || key == Key::Meta {
        held.push((COMMAND_KEY, COMMAND));
    }
    let mut out = Vec::with_capacity(held.len() * 2 + 2);
    let mut flags = 0;
    for &(modifier, bit) in &held {
        flags |= bit;
        out.push(Stroke {
            code: modifier,
            down: true,
            flags,
        });
    }
    for down in [true, false] {
        out.push(Stroke {
            code,
            down,
            flags: flags | extra,
        });
    }
    for &(modifier, bit) in held.iter().rev() {
        flags &= !bit;
        out.push(Stroke {
            code: modifier,
            down: false,
            flags,
        });
    }
    Some(out)
}

pub(crate) fn unknown_key(key: Key) -> InjectError {
    InjectError::Rejected(format!("{key:?} can't be typed on this computer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_typed_character_by_character_and_newlines_as_return() {
        let pieces = text_pieces("aЖ\r\n😀\t");
        assert_eq!(
            pieces,
            [
                TextPiece::Char(vec![u16::from(b'a')]),
                TextPiece::Char(vec![0x0416]),
                TextPiece::Key(RETURN),
                TextPiece::Char(vec![0xD83D, 0xDE00]),
                TextPiece::Key(TAB),
            ]
        );
    }

    #[test]
    fn a_shortcut_holds_its_modifiers_around_the_key() {
        // Command-Shift-4 (a screenshot of an area).
        let strokes = key_strokes(Key::Char('4'), Modifiers::META.with(Modifiers::SHIFT)).unwrap();
        let shift_cmd = SHIFT | COMMAND;
        assert_eq!(
            strokes,
            [
                Stroke {
                    code: SHIFT_KEY,
                    down: true,
                    flags: SHIFT
                },
                Stroke {
                    code: COMMAND_KEY,
                    down: true,
                    flags: shift_cmd
                },
                Stroke {
                    code: 0x15,
                    down: true,
                    flags: shift_cmd
                },
                Stroke {
                    code: 0x15,
                    down: false,
                    flags: shift_cmd
                },
                Stroke {
                    code: COMMAND_KEY,
                    down: false,
                    flags: SHIFT
                },
                Stroke {
                    code: SHIFT_KEY,
                    down: false,
                    flags: 0
                },
            ]
        );
    }

    #[test]
    fn characters_that_need_shift_get_it() {
        let strokes = key_strokes(Key::Char('?'), Modifiers::NONE).unwrap();
        assert_eq!(
            strokes[1],
            Stroke {
                code: 0x2C,
                down: true,
                flags: SHIFT
            }
        );
        let strokes = key_strokes(Key::Char('Z'), Modifiers::CTRL).unwrap();
        assert_eq!(
            strokes[2],
            Stroke {
                code: 0x06,
                down: true,
                flags: CONTROL | SHIFT
            }
        );
        assert!(key_strokes(Key::Char('Ж'), Modifiers::META).is_none());
    }

    #[test]
    fn the_command_key_alone_opens_spotlight() {
        let strokes = key_strokes(Key::Meta, Modifiers::META).unwrap();
        assert_eq!(strokes.len(), 4, "{strokes:?}");
        assert_eq!(
            strokes[1],
            Stroke {
                code: SPACE,
                down: true,
                flags: COMMAND
            }
        );
    }

    #[test]
    fn arrows_and_function_keys_carry_the_flags_of_a_real_keyboard() {
        let strokes = key_strokes(Key::Left, Modifiers::NONE).unwrap();
        assert_eq!(strokes[0].flags, SECONDARY_FN | NUMERIC_PAD);
        let strokes = key_strokes(Key::Function(12), Modifiers::NONE).unwrap();
        assert_eq!(
            strokes[0],
            Stroke {
                code: 0x6F,
                down: true,
                flags: SECONDARY_FN
            }
        );
        assert!(key_strokes(Key::Function(13), Modifiers::NONE).is_none());
    }
}
