//! Windows keyboard injection with `SendInput`.
//!
//! Text is typed as Unicode characters (`KEYEVENTF_UNICODE`), so it doesn't
//! depend on the computer's keyboard layout and any language works. Keys
//! and shortcuts are virtual keys; a character in a shortcut uses the key
//! that types it on the current layout (`VkKeyScanW`). Every press is sent
//! with its release in one `SendInput` call, so nothing stays held down.

use ib_protocol::{Key, Modifiers};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_UNICODE, VIRTUAL_KEY, VK_BACK, VK_CONTROL, VK_DELETE, VK_DOWN,
    VK_END, VK_ESCAPE, VK_F1, VK_HOME, VK_LEFT, VK_LWIN, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN,
    VK_RIGHT, VK_SHIFT, VK_TAB, VK_UP, VkKeyScanW,
};

use crate::InjectError;

fn key_input(vk: VIRTUAL_KEY, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: scan,
                dwFlags: flags,
                ..Default::default()
            },
        },
    }
}

/// Press and release of a virtual key.
fn tap(vk: VIRTUAL_KEY, extended: bool, out: &mut Vec<INPUT>) {
    let ext = if extended {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    out.push(key_input(vk, 0, ext));
    out.push(key_input(vk, 0, ext | KEYEVENTF_KEYUP));
}

/// The events that type `text`.
pub(crate) fn text_inputs(text: &str) -> Vec<INPUT> {
    let mut out = Vec::with_capacity(text.len() * 2);
    for c in text.chars() {
        match c {
            // Apps expect the Enter and Tab keys, not the characters.
            '\n' => tap(VK_RETURN, false, &mut out),
            '\t' => tap(VK_TAB, false, &mut out),
            '\r' => {}
            c => {
                let mut units = [0u16; 2];
                for &unit in c.encode_utf16(&mut units).iter() {
                    out.push(key_input(VIRTUAL_KEY(0), unit, KEYEVENTF_UNICODE));
                    out.push(key_input(
                        VIRTUAL_KEY(0),
                        unit,
                        KEYEVENTF_UNICODE | KEYEVENTF_KEYUP,
                    ));
                }
            }
        }
    }
    out
}

/// Virtual key (and whether it is an "extended" key) for a key without a
/// character. `None` for [`Key::Char`] and function keys out of range.
fn named_key(key: Key) -> Option<(VIRTUAL_KEY, bool)> {
    Some(match key {
        Key::Enter => (VK_RETURN, false),
        Key::Backspace => (VK_BACK, false),
        Key::Delete => (VK_DELETE, true),
        Key::Tab => (VK_TAB, false),
        Key::Escape => (VK_ESCAPE, false),
        Key::Up => (VK_UP, true),
        Key::Down => (VK_DOWN, true),
        Key::Left => (VK_LEFT, true),
        Key::Right => (VK_RIGHT, true),
        Key::Home => (VK_HOME, true),
        Key::End => (VK_END, true),
        Key::PageUp => (VK_PRIOR, true),
        Key::PageDown => (VK_NEXT, true),
        Key::Meta => (VK_LWIN, true),
        Key::Function(n @ 1..=12) => (VIRTUAL_KEY(VK_F1.0 + u16::from(n) - 1), false),
        Key::Function(_) | Key::Char(_) => return None,
    })
}

/// The key that types `c` on the current layout, and whether Shift is
/// part of it. Letters and digits fall back to their US keys.
fn char_key(c: char) -> Option<(VIRTUAL_KEY, bool)> {
    let mut units = [0u16; 2];
    if let [unit] = c.encode_utf16(&mut units) {
        // SAFETY: plain FFI call without pointers.
        let scan = unsafe { VkKeyScanW(*unit) };
        if scan != -1 {
            let [vk, shift_state] = scan.to_le_bytes();
            return Some((VIRTUAL_KEY(u16::from(vk)), shift_state & 1 != 0));
        }
    }
    let upper = c.to_ascii_uppercase();
    upper
        .is_ascii_alphanumeric()
        .then_some((VIRTUAL_KEY(upper as u16), false))
}

/// The events for `key` with `modifiers` held, or `None` if the key can't
/// be typed on this computer.
pub(crate) fn key_inputs(key: Key, modifiers: Modifiers) -> Option<Vec<INPUT>> {
    let (vk, extended, shift) = match key {
        Key::Char(c) => {
            let (vk, shift) = char_key(c)?;
            (vk, false, shift)
        }
        named => {
            let (vk, extended) = named_key(named)?;
            (vk, extended, false)
        }
    };
    let mut held: Vec<(VIRTUAL_KEY, bool)> = Vec::new();
    if modifiers.contains(Modifiers::CTRL) {
        held.push((VK_CONTROL, false));
    }
    if modifiers.contains(Modifiers::ALT) {
        held.push((VK_MENU, false));
    }
    if shift || modifiers.contains(Modifiers::SHIFT) {
        held.push((VK_SHIFT, false));
    }
    if modifiers.contains(Modifiers::META) && vk != VK_LWIN {
        held.push((VK_LWIN, true));
    }
    let flags = |extended: bool| {
        if extended {
            KEYEVENTF_EXTENDEDKEY
        } else {
            KEYBD_EVENT_FLAGS(0)
        }
    };
    let mut out = Vec::with_capacity(held.len() * 2 + 2);
    for &(m, ext) in &held {
        out.push(key_input(m, 0, flags(ext)));
    }
    tap(vk, extended, &mut out);
    for &(m, ext) in held.iter().rev() {
        out.push(key_input(m, 0, flags(ext) | KEYEVENTF_KEYUP));
    }
    Some(out)
}

pub(crate) fn unknown_key(key: Key) -> InjectError {
    InjectError::Rejected(format!("{key:?} can't be typed on this computer"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(inputs: &[INPUT]) -> Vec<(u16, u16, bool)> {
        inputs
            .iter()
            .map(|i| {
                // SAFETY: every record built here is a keyboard input.
                let k = unsafe { i.Anonymous.ki };
                (k.wVk.0, k.wScan, k.dwFlags.contains(KEYEVENTF_KEYUP))
            })
            .collect()
    }

    #[test]
    fn text_is_typed_as_characters_and_newlines_as_enter() {
        let inputs = text_inputs("aЖ\n😀");
        let k = keys(&inputs);
        // a, Ж: one unit each; Enter: a key; the emoji: two UTF-16 units.
        assert_eq!(k.len(), 2 + 2 + 2 + 4);
        assert_eq!(k[0], (0, u16::from(b'a'), false));
        assert_eq!(k[1], (0, u16::from(b'a'), true));
        assert_eq!(k[2], (0, 0x0416, false));
        assert_eq!(k[4], (VK_RETURN.0, 0, false));
        assert_eq!(k[6].1, 0xD83D);
    }

    #[test]
    fn a_shortcut_holds_its_modifiers_around_the_key() {
        let inputs = key_inputs(Key::Function(4), Modifiers::ALT).unwrap();
        assert_eq!(
            keys(&inputs),
            [
                (VK_MENU.0, 0, false),
                (VK_F1.0 + 3, 0, false),
                (VK_F1.0 + 3, 0, true),
                (VK_MENU.0, 0, true),
            ]
        );
    }

    #[test]
    fn the_windows_key_alone_is_one_tap() {
        let inputs = key_inputs(Key::Meta, Modifiers::META).unwrap();
        assert_eq!(keys(&inputs), [(VK_LWIN.0, 0, false), (VK_LWIN.0, 0, true)]);
        assert!(key_inputs(Key::Function(13), Modifiers::NONE).is_none());
    }
}
