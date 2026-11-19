//! Length-prefixed messages on the reliable control stream.
//!
//! Frame: `u32` big-endian payload length, then a postcard-encoded
//! [`ControlMessage`]. Lengths above [`MAX_MESSAGE_LEN`] abort the stream
//! before any allocation.
//!
//! [`Control`] can be [`split`](Control::split) into independent halves so
//! one task can send while another receives.

use ib_protocol::{ControlMessage, MAX_MESSAGE_LEN, decode, encode};

use crate::TransportError;
use crate::stream::{RecvHalf, SendHalf};

/// The bidirectional control stream of a session.
#[derive(Debug)]
pub struct Control {
    tx: ControlSender,
    rx: ControlReceiver,
}

/// Sending half of the control stream.
#[derive(Debug)]
pub struct ControlSender {
    send: SendHalf,
}

/// Receiving half of the control stream.
#[derive(Debug)]
pub struct ControlReceiver {
    recv: RecvHalf,
}

impl Control {
    pub(crate) fn new(send: SendHalf, recv: RecvHalf) -> Self {
        Self {
            tx: ControlSender { send },
            rx: ControlReceiver { recv },
        }
    }

    pub async fn send(&mut self, msg: &ControlMessage) -> Result<(), TransportError> {
        self.tx.send(msg).await
    }

    pub async fn recv(&mut self) -> Result<ControlMessage, TransportError> {
        self.rx.recv().await
    }

    /// Flushes and closes the sending half.
    pub async fn finish(mut self) -> Result<(), TransportError> {
        self.close_send().await;
        Ok(())
    }

    /// See [`ControlSender::close_send`].
    pub async fn close_send(&mut self) {
        self.tx.close_send().await;
    }

    /// Splits into independently usable halves.
    pub fn split(self) -> (ControlSender, ControlReceiver) {
        (self.tx, self.rx)
    }
}

impl ControlSender {
    pub async fn send(&mut self, msg: &ControlMessage) -> Result<(), TransportError> {
        let body = encode(msg)?;
        let len = u32::try_from(body.len()).map_err(|_| TransportError::TooLong(body.len()))?;
        // One write call per frame keeps header and body in the same packet.
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(&body);
        self.send.write_all(&frame).await?;
        Ok(())
    }

    /// Finishes the sending half and waits until the peer has received
    /// everything (or gone away). Call before closing a connection right
    /// after a final message, such as a rejection, so it isn't lost.
    pub async fn close_send(&mut self) {
        self.send.finish().await;
    }
}

impl ControlReceiver {
    pub async fn recv(&mut self) -> Result<ControlMessage, TransportError> {
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
