use serde::{Deserialize, Serialize};

/// Protocol version, independent of the application version.
///
/// - `major` changes when old peers can no longer understand new messages.
/// - `minor` changes for backwards-compatible additions (new optional
///   capabilities); peers with the same `major` interoperate and negotiate
///   features through [`crate::Capabilities`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ProtocolVersion {
    pub major: u16,
    pub minor: u16,
}

impl ProtocolVersion {
    /// Whether two peers can talk to each other.
    pub const fn is_compatible_with(self, other: Self) -> bool {
        self.major == other.major
    }
}

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The version implemented by this build.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion { major: 0, minor: 3 };

/// First minor version whose clients understand `Displays` and can send
/// `SelectDisplay`. Older clients stop reading at a message they don't know.
pub const DISPLAY_CHOICE_MINOR: u16 = 2;

/// First minor version that has keyboard input: hosts understand `Text` and
/// `Key`, clients understand `Keyboard`.
pub const KEYBOARD_MINOR: u16 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_major_is_compatible() {
        let a = ProtocolVersion { major: 1, minor: 0 };
        assert!(a.is_compatible_with(ProtocolVersion { major: 1, minor: 7 }));
        assert!(!a.is_compatible_with(ProtocolVersion { major: 2, minor: 0 }));
    }
}
