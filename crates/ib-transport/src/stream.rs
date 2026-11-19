//! Byte streams under the framed channels: a QUIC stream, or one half of a
//! TLS-over-TCP connection (the fallback, see [`crate::tcp`]).
//!
//! Closing a QUIC connection ends all its streams at once. A TCP session
//! has one [`Closed`] signal shared by its connections for the same effect:
//! a read or write blocked on a TCP half returns [`TransportError::Closed`]
//! as soon as the session is closed.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf};
use tokio::net::TcpStream;
use tokio_rustls::TlsStream;

use crate::TransportError;

pub(crate) type Tls = TlsStream<TcpStream>;

/// Set once when a TCP session ends; everything blocked on it wakes up.
#[derive(Debug, Clone)]
pub(crate) struct Closed(Arc<tokio::sync::watch::Sender<bool>>);

impl Closed {
    pub(crate) fn new() -> Self {
        Self(Arc::new(tokio::sync::watch::Sender::new(false)))
    }

    pub(crate) fn close(&self) {
        self.0.send_replace(true);
    }

    pub(crate) fn is_closed(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolves once the session is closed.
    pub(crate) async fn wait(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(|closed| *closed).await;
    }

    /// Runs `io` unless the session closes first.
    pub(crate) async fn guard<T>(
        &self,
        io: impl Future<Output = std::io::Result<T>>,
    ) -> Result<T, TransportError> {
        if self.is_closed() {
            return Err(TransportError::Closed);
        }
        tokio::select! {
            r = io => r.map_err(|e| {
                // A broken TCP connection ends the whole session, as a lost
                // QUIC connection would.
                self.close();
                TransportError::Io(e)
            }),
            () = self.wait() => Err(TransportError::Closed),
        }
    }
}

/// Writing half of a channel.
#[derive(Debug)]
pub(crate) enum SendHalf {
    Quic(quinn::SendStream),
    Tcp(WriteHalf<Tls>, Closed),
}

/// Reading half of a channel.
#[derive(Debug)]
pub(crate) enum RecvHalf {
    Quic(quinn::RecvStream),
    Tcp(ReadHalf<Tls>, Closed),
}

impl SendHalf {
    /// Writes all of `buf` and pushes it onto the network.
    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> Result<(), TransportError> {
        match self {
            Self::Quic(s) => Ok(s.write_all(buf).await?),
            Self::Tcp(w, closed) => {
                closed
                    .guard(async {
                        w.write_all(buf).await?;
                        // TLS keeps records until flushed.
                        w.flush().await
                    })
                    .await
            }
        }
    }

    /// Ends the sending side once the peer has everything sent so far.
    pub(crate) async fn finish(&mut self) {
        match self {
            Self::Quic(s) => {
                let _ = s.finish();
                let _ = s.stopped().await;
            }
            Self::Tcp(w, closed) => {
                let _ = closed.guard(w.shutdown()).await;
            }
        }
    }
}

impl RecvHalf {
    pub(crate) async fn read_exact(&mut self, buf: &mut [u8]) -> Result<(), TransportError> {
        match self {
            Self::Quic(r) => Ok(r.read_exact(buf).await?),
            Self::Tcp(r, closed) => closed.guard(r.read_exact(buf)).await.map(|_| ()),
        }
    }
}
