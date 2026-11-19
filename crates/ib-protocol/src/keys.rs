//! Keyboard input from the tablet (protocol 0.3).
//!
//! Typed text travels as text ([`crate::ControlMessage::Text`]), so any
//! language and the tablet's own keyboard layout work. Keys that have no
//! character (Enter, arrows, the Windows/Command key) and shortcuts travel
//! as a [`Key`] with [`Modifiers`].

use serde::{Deserialize, Serialize};

/// A key without a character, or a character used in a shortcut.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Key {
    Enter,
    Backspace,
    Delete,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// The Windows key or macOS Command key on its own (opens Start).
    Meta,
    /// F1..=F12.
    Function(u8),
    /// A character in a shortcut (`Ctrl` + `c`): the key that types it on
    /// the computer's layout. Letters are sent lowercase.
    Char(char),
}

/// Keys held down during a [`Key`], as bits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Modifiers(pub u8);

impl Modifiers {
    pub const NONE: Self = Self(0);
    pub const SHIFT: Self = Self(1);
    pub const CTRL: Self = Self(2);
    pub const ALT: Self = Self(4);
    /// Windows key / macOS Command.
    pub const META: Self = Self(8);

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }
}

/// Longest text accepted in one message; longer pastes are split.
pub const MAX_TEXT_LEN: usize = 4096;
