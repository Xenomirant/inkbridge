use serde::{Deserialize, Serialize};

/// Video codecs a peer can produce (host) or consume (client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum VideoCodec {
    /// Baseline for every device: hardware decode is universal.
    H264,
    /// Optional: better quality per bit, not universally hardware-decoded.
    Hevc,
}

/// Physical link a session runs over. Used for link preference and shown to
/// the user; never sent anywhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TransportKind {
    Wifi,
    /// Android USB tethering (RNDIS/NCM) or iPad usbmuxd.
    Usb,
    /// Advanced option: adb port forwarding.
    Adb,
    Other,
}

/// Which pen axes the sender can measure (client) or inject (host).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)] // a flat feature set is the clearest wire form
pub struct PenFeatures {
    pub pressure: bool,
    pub tilt: bool,
    pub rotation: bool,
    pub hover: bool,
    pub eraser: bool,
    pub barrel_button: bool,
}

impl PenFeatures {
    /// Features usable by both sides.
    #[must_use]
    pub const fn intersect(self, other: Self) -> Self {
        Self {
            pressure: self.pressure && other.pressure,
            tilt: self.tilt && other.tilt,
            rotation: self.rotation && other.rotation,
            hover: self.hover && other.hover,
            eraser: self.eraser && other.eraser,
            barrel_button: self.barrel_button && other.barrel_button,
        }
    }
}

/// Capabilities advertised in the handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    /// In order of preference.
    pub video_codecs: Vec<VideoCodec>,
    /// Largest frame the client can decode / the host will send, in pixels:
    /// `max_width` limits the long side and `max_height` the short side,
    /// whichever way the picture is oriented (hardware decoders limited to
    /// 1920x1088 also take 1088x1920).
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u16,
    pub pen: PenFeatures,
}

impl Capabilities {
    /// The first codec in `self`'s preference order that `other` also supports.
    pub fn negotiate_codec(&self, other: &Self) -> Option<VideoCodec> {
        self.video_codecs
            .iter()
            .copied()
            .find(|c| other.video_codecs.contains(c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(codecs: &[VideoCodec]) -> Capabilities {
        Capabilities {
            video_codecs: codecs.to_vec(),
            max_width: 2560,
            max_height: 1600,
            max_fps: 60,
            pen: PenFeatures::default(),
        }
    }

    #[test]
    fn negotiates_first_common_codec_in_preference_order() {
        let host = caps(&[VideoCodec::Hevc, VideoCodec::H264]);
        let client = caps(&[VideoCodec::H264]);
        assert_eq!(host.negotiate_codec(&client), Some(VideoCodec::H264));
        assert_eq!(caps(&[VideoCodec::Hevc]).negotiate_codec(&client), None);
    }

    #[test]
    fn pen_features_intersect() {
        let a = PenFeatures {
            pressure: true,
            tilt: true,
            ..Default::default()
        };
        let b = PenFeatures {
            pressure: true,
            hover: true,
            ..Default::default()
        };
        assert_eq!(
            a.intersect(b),
            PenFeatures {
                pressure: true,
                ..Default::default()
            }
        );
    }
}
