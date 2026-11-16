//! The host → tablet video stream: one long-lived unidirectional QUIC
//! stream of length-prefixed [`VideoChunk`]s, separate from pen and control
//! traffic so video can never delay the pen.
//!
//! Ordering is reliable: the host must not drop single delta frames; when it
//! falls behind it skips to a new keyframe instead.

use ib_protocol::{MAX_MESSAGE_LEN, VideoChunk, decode, encode};

use crate::TransportError;

#[derive(Debug)]
pub struct VideoSender {
    send: quinn::SendStream,
}

#[derive(Debug)]
pub struct VideoReceiver {
    recv: quinn::RecvStream,
}

impl VideoSender {
    pub(crate) fn new(send: quinn::SendStream) -> Self {
        Self { send }
    }

    pub async fn send(&mut self, chunk: &VideoChunk) -> Result<(), TransportError> {
        let body = encode(chunk)?;
        let len = u32::try_from(body.len()).map_err(|_| TransportError::TooLong(body.len()))?;
        self.send.write_all(&len.to_be_bytes()).await?;
        self.send.write_all(&body).await?;
        Ok(())
    }
}

impl VideoReceiver {
    pub(crate) fn new(recv: quinn::RecvStream) -> Self {
        Self { recv }
    }

    pub async fn recv(&mut self) -> Result<VideoChunk, TransportError> {
        let mut len = [0u8; 4];
        self.recv.read_exact(&mut len).await?;
        let len = u32::from_be_bytes(len) as usize;
        if len > MAX_MESSAGE_LEN {
            return Err(TransportError::TooLong(len));
        }
        let mut body = vec![0u8; len];
        self.recv.read_exact(&mut body).await?;
        Ok(decode(&body)?)
    }
}
