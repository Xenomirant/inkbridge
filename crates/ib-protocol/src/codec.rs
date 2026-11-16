//! Byte encoding of protocol messages (postcard: compact, varint-based).
//!
//! Framing (length prefixes on streams) is the transport's job; these
//! functions convert exactly one message.

use serde::{Serialize, de::DeserializeOwned};

/// Hard upper bound on an encoded message. Protects the receiver against
/// memory exhaustion from a hostile or broken peer. Video chunks are the
/// largest messages (a 4K keyframe at high bitrate stays well below this).
pub const MAX_MESSAGE_LEN: usize = 8 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    #[error("message too large: {len} bytes (max {MAX_MESSAGE_LEN})")]
    TooLarge { len: usize },
    #[error("serialisation failed: {0}")]
    Serialize(#[from] postcard::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("message too large: {len} bytes (max {MAX_MESSAGE_LEN})")]
    TooLarge { len: usize },
    #[error("malformed message: {0}")]
    Malformed(#[from] postcard::Error),
}

pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>, EncodeError> {
    let bytes = postcard::to_stdvec(msg)?;
    if bytes.len() > MAX_MESSAGE_LEN {
        return Err(EncodeError::TooLarge { len: bytes.len() });
    }
    Ok(bytes)
}

/// Decodes exactly one message; trailing bytes are an error, which catches
/// framing bugs early.
pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, DecodeError> {
    if bytes.len() > MAX_MESSAGE_LEN {
        return Err(DecodeError::TooLarge { len: bytes.len() });
    }
    let (msg, rest) = postcard::take_from_bytes(bytes)?;
    if rest.is_empty() {
        Ok(msg)
    } else {
        Err(DecodeError::Malformed(
            postcard::Error::DeserializeBadEncoding,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Buttons, Capabilities, ClientHello, ControlMessage, PROTOCOL_VERSION, PenBatch,
        PenFeatures, PenSample, PenState, Tool, VideoCodec,
    };
    use proptest::prelude::*;

    fn sample_strategy() -> impl Strategy<Value = PenSample> {
        (
            any::<u64>(),
            (0.0f32..=1.0, 0.0f32..=1.0, 0.0f32..=1.0),
            (-90.0f32..=90.0, -90.0f32..=90.0, 0.0f32..360.0),
            prop_oneof![
                Just(PenState::Hover),
                Just(PenState::Contact),
                Just(PenState::OutOfRange)
            ],
            prop_oneof![Just(Tool::Pen), Just(Tool::Eraser)],
            any::<u8>(),
        )
            .prop_map(
                |(t_us, (x, y, pressure), (tilt_x, tilt_y, rotation), state, tool, b)| PenSample {
                    t_us,
                    x,
                    y,
                    pressure,
                    tilt_x,
                    tilt_y,
                    rotation,
                    state,
                    tool,
                    buttons: Buttons(b),
                },
            )
    }

    proptest! {
        #[test]
        fn pen_batch_round_trips(seq in any::<u32>(), samples in prop::collection::vec(sample_strategy(), 0..=PenBatch::MAX_SAMPLES)) {
            let batch = PenBatch { seq, samples };
            let bytes = encode(&batch).unwrap();
            prop_assert_eq!(decode::<PenBatch>(&bytes).unwrap(), batch);
        }

        #[test]
        fn decoding_garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..512)) {
            let _ = decode::<ControlMessage>(&bytes);
            let _ = decode::<PenBatch>(&bytes);
        }
    }

    /// A full batch must fit in a single QUIC datagram (conservative 1200 B).
    #[test]
    fn full_pen_batch_fits_in_a_datagram() {
        let worst = PenSample {
            t_us: u64::MAX,
            x: 0.123_456,
            y: 0.654_321,
            pressure: 0.5,
            tilt_x: -45.5,
            tilt_y: 45.5,
            rotation: 359.9,
            state: PenState::Contact,
            tool: Tool::Eraser,
            buttons: Buttons(0xFF),
        };
        let batch = PenBatch {
            seq: u32::MAX,
            samples: vec![worst; PenBatch::MAX_SAMPLES],
        };
        let len = encode(&batch).unwrap().len();
        assert!(len <= 1200, "full batch is {len} bytes");
    }

    #[test]
    fn control_round_trip_and_trailing_bytes_rejected() {
        let msg = ControlMessage::ClientHello(ClientHello {
            version: PROTOCOL_VERSION,
            device_name: "Xiaoxin Pad 2024".into(),
            caps: Capabilities {
                video_codecs: vec![VideoCodec::H264],
                max_width: 2560,
                max_height: 1600,
                max_fps: 90,
                pen: PenFeatures {
                    pressure: true,
                    tilt: true,
                    ..Default::default()
                },
            },
            screen_width: 2560,
            screen_height: 1600,
        });
        let mut bytes = encode(&msg).unwrap();
        assert_eq!(decode::<ControlMessage>(&bytes).unwrap(), msg);
        bytes.push(0);
        assert!(decode::<ControlMessage>(&bytes).is_err());
    }
}
