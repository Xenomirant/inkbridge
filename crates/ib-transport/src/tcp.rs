//! TLS over TCP: the fallback transport.
//!
//! Used where QUIC can't get through (networks that block UDP) and on links
//! that carry only TCP, such as `adb reverse` over USB (a Mac can't use the
//! tablet's RNDIS tethering). Same certificates, same pinning, TLS 1.3.
//!
//! A session is three TLS connections, so the pen never waits behind video
//! inside one TCP stream: control, video and pen. Each starts with a
//! preamble, the channel (1 byte) and a session token (16 bytes chosen by
//! the tablet) that groups the three; all three must present the same
//! client certificate. The tablet opens all of them (it may be behind a
//! firewall or NAT the host can't dial through).
//!
//! The pen connection carries pen batches and, both ways, a ping every
//! second: it measures the round trip and ends the session when the peer
//! has been silent for [`IDLE_TIMEOUT`] (QUIC's idle timeout).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ib_protocol::{MAX_MESSAGE_LEN, PenBatch, decode, encode};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::{TlsAcceptor, TlsConnector, TlsStream};

use crate::framing::Control;
use crate::stream::{Closed, RecvHalf, SendHalf, Tls};
use crate::{Fingerprint, TransportError};

const TOKEN_LEN: usize = 16;
type Token = [u8; TOKEN_LEN];

/// Which connection of the session this is (first preamble byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Channel {
    Control = 0,
    Video = 1,
    Pen = 2,
}

impl Channel {
    fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Control),
            1 => Some(Self::Video),
            2 => Some(Self::Pen),
            _ => None,
        }
    }
}

/// Time for a tablet to finish its TLS handshakes and preambles.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// Connections that completed the handshake but whose session isn't
/// complete yet, at most (each costs a socket).
const MAX_PENDING: usize = 32;
const PING_EVERY: Duration = Duration::from_secs(1);
/// Silence on the pen connection after which the session is over.
pub(crate) const IDLE_TIMEOUT: Duration = Duration::from_secs(5);
/// Kernel send buffer of the video connection: like QUIC's send window, a
/// stalled link blocks the video writer (and the pipeline skips frames)
/// instead of queueing seconds of video.
const VIDEO_SEND_BUFFER: usize = 128 * 1024;
/// Pen frames waiting to be written; when full, new samples are dropped
/// (stale pen samples are worse than lost ones).
const PEN_QUEUE: usize = 64;

/// Pen-connection frame kinds: `[kind u8][len u32 BE][body]`.
const FRAME_PEN: u8 = 0;
const FRAME_PING: u8 = 1;
const FRAME_PONG: u8 = 2;

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut f = Vec::with_capacity(5 + body.len());
    f.push(kind);
    f.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_be_bytes());
    f.extend_from_slice(body);
    f
}

/// An established TCP session (the three connections).
#[derive(Debug)]
pub(crate) struct TcpSession {
    pub(crate) remote: SocketAddr,
    control: Mutex<Option<Tls>>,
    video: Mutex<Option<Tls>>,
    pen_out: mpsc::Sender<Vec<u8>>,
    pen_in: tokio::sync::Mutex<mpsc::Receiver<PenBatch>>,
    rtt_us: Arc<AtomicU64>,
    pub(crate) closed: Closed,
}

impl TcpSession {
    fn start(remote: SocketAddr, control: Tls, video: Tls, pen: Tls) -> Arc<Self> {
        let closed = Closed::new();
        let (pen_out, out_rx) = mpsc::channel(PEN_QUEUE);
        let (in_tx, pen_in) = mpsc::channel(PEN_QUEUE);
        let rtt_us = Arc::new(AtomicU64::new(0));
        let (reader, writer) = tokio::io::split(pen);
        let epoch = Instant::now();
        let heard = Arc::new(AtomicU64::new(0));
        tokio::spawn(write_pen(writer, out_rx, closed.clone()));
        tokio::spawn(read_pen(
            reader,
            in_tx,
            pen_out.clone(),
            rtt_us.clone(),
            heard.clone(),
            epoch,
            closed.clone(),
        ));
        tokio::spawn(keep_alive(pen_out.clone(), heard, epoch, closed.clone()));
        Arc::new(Self {
            remote,
            control: Mutex::new(Some(control)),
            video: Mutex::new(Some(video)),
            pen_out,
            pen_in: tokio::sync::Mutex::new(pen_in),
            rtt_us,
            closed,
        })
    }

    fn take(slot: &Mutex<Option<Tls>>) -> Result<Tls, TransportError> {
        slot.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or(TransportError::Tcp("channel already in use".into()))
    }

    /// The control channel (once; either side).
    pub(crate) fn control(&self) -> Result<Control, TransportError> {
        let (r, w) = tokio::io::split(Self::take(&self.control)?);
        Ok(Control::new(
            SendHalf::Tcp(w, self.closed.clone()),
            RecvHalf::Tcp(r, self.closed.clone()),
        ))
    }

    /// Host: the video channel's sending side.
    pub(crate) fn video_sender(&self) -> Result<crate::VideoSender, TransportError> {
        let (_, w) = tokio::io::split(Self::take(&self.video)?);
        Ok(crate::VideoSender::new(SendHalf::Tcp(
            w,
            self.closed.clone(),
        )))
    }

    /// Tablet: the video channel's receiving side.
    pub(crate) fn video_receiver(&self) -> Result<crate::VideoReceiver, TransportError> {
        let (r, _) = tokio::io::split(Self::take(&self.video)?);
        Ok(crate::VideoReceiver::new(RecvHalf::Tcp(
            r,
            self.closed.clone(),
        )))
    }

    pub(crate) fn send_pen(&self, batch: &PenBatch) -> Result<(), TransportError> {
        if self.closed.is_closed() {
            return Err(TransportError::Closed);
        }
        // A full queue means the link is stalled: drop, as a datagram would be.
        let _ = self.pen_out.try_send(frame(FRAME_PEN, &encode(batch)?));
        Ok(())
    }

    pub(crate) async fn recv_pen(&self) -> Result<PenBatch, TransportError> {
        let mut rx = self.pen_in.lock().await;
        tokio::select! {
            batch = rx.recv() => batch.ok_or(TransportError::Closed),
            () = self.closed.wait() => Err(TransportError::Closed),
        }
    }

    pub(crate) fn rtt(&self) -> Duration {
        Duration::from_micros(self.rtt_us.load(Ordering::Relaxed))
    }

    pub(crate) fn close(&self) {
        self.closed.close();
    }
}

impl Drop for TcpSession {
    /// The last handle is gone: stop the pen tasks and close the sockets.
    fn drop(&mut self) {
        self.closed.close();
    }
}

fn micros_since(epoch: Instant) -> u64 {
    u64::try_from(epoch.elapsed().as_micros()).unwrap_or(u64::MAX)
}

async fn write_pen(
    mut w: tokio::io::WriteHalf<Tls>,
    mut frames: mpsc::Receiver<Vec<u8>>,
    closed: Closed,
) {
    loop {
        let f = tokio::select! {
            f = frames.recv() => f,
            () = closed.wait() => None,
        };
        let Some(f) = f else { break };
        if closed
            .guard(async {
                w.write_all(&f).await?;
                w.flush().await
            })
            .await
            .is_err()
        {
            break;
        }
    }
    let _ = w.shutdown().await;
}

async fn read_pen(
    mut r: tokio::io::ReadHalf<Tls>,
    batches: mpsc::Sender<PenBatch>,
    replies: mpsc::Sender<Vec<u8>>,
    rtt_us: Arc<AtomicU64>,
    heard: Arc<AtomicU64>,
    epoch: Instant,
    closed: Closed,
) {
    let mut head = [0u8; 5];
    while closed.guard(r.read_exact(&mut head)).await.is_ok() {
        let len = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        if len > MAX_MESSAGE_LEN {
            break;
        }
        let mut body = vec![0u8; len];
        if closed.guard(r.read_exact(&mut body)).await.is_err() {
            break;
        }
        heard.store(micros_since(epoch), Ordering::Relaxed);
        match head[0] {
            FRAME_PEN => {
                if let Ok(batch) = decode::<PenBatch>(&body) {
                    let _ = batches.try_send(batch);
                }
            }
            FRAME_PING => {
                let _ = replies.try_send(frame(FRAME_PONG, &body));
            }
            FRAME_PONG => {
                if let Ok(sent) = <[u8; 8]>::try_from(body.as_slice()) {
                    let rtt = micros_since(epoch).saturating_sub(u64::from_be_bytes(sent));
                    let old = rtt_us.load(Ordering::Relaxed);
                    // Smoothed like TCP's estimate (7/8 old, 1/8 new).
                    let smoothed = if old == 0 { rtt } else { (old * 7 + rtt) / 8 };
                    rtt_us.store(smoothed, Ordering::Relaxed);
                }
            }
            _ => {} // unknown frames are skipped: room for later additions
        }
    }
    closed.close();
}

async fn keep_alive(
    out: mpsc::Sender<Vec<u8>>,
    heard: Arc<AtomicU64>,
    epoch: Instant,
    closed: Closed,
) {
    let mut tick = tokio::time::interval(PING_EVERY);
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            () = closed.wait() => return,
        }
        let now = micros_since(epoch);
        let silent = Duration::from_micros(now.saturating_sub(heard.load(Ordering::Relaxed)));
        if silent > IDLE_TIMEOUT {
            tracing::debug!("TCP session silent for {silent:?}, closing");
            closed.close();
            return;
        }
        let _ = out.try_send(frame(FRAME_PING, &now.to_be_bytes()));
    }
}

fn tcp_error(e: impl std::fmt::Display) -> TransportError {
    TransportError::Tcp(e.to_string())
}

/// Tablet side: opens the three connections of a session to `addr`.
pub(crate) async fn connect(
    addr: SocketAddr,
    config: Arc<rustls::ClientConfig>,
) -> Result<Arc<TcpSession>, TransportError> {
    let mut token: Token = [0; TOKEN_LEN];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut token)
        .map_err(|_| tcp_error("no random numbers"))?;
    let connector = TlsConnector::from(config);
    let open = |channel: Channel| {
        let connector = connector.clone();
        async move {
            let tcp = TcpStream::connect(addr).await?;
            tcp.set_nodelay(true)?;
            let name = ServerName::try_from("inkbridge").map_err(tcp_error)?;
            let mut tls = connector.connect(name, tcp).await?;
            let mut preamble = [0u8; 1 + TOKEN_LEN];
            preamble[0] = channel as u8;
            preamble[1..].copy_from_slice(&token);
            tls.write_all(&preamble).await?;
            tls.flush().await?;
            Ok::<Tls, TransportError>(TlsStream::Client(tls))
        }
    };
    // Control first: a refused or wrong host fails before more connections.
    let control = open(Channel::Control).await?;
    let (video, pen) = tokio::try_join!(open(Channel::Video), open(Channel::Pen))?;
    Ok(TcpSession::start(addr, control, video, pen))
}

/// Connections of a session that isn't complete yet.
#[derive(Default)]
struct Partial {
    control: Option<Tls>,
    video: Option<Tls>,
    pen: Option<Tls>,
    since: Option<Instant>,
}

type PendingMap = HashMap<(Fingerprint, Token), Partial>;

/// Host side: accepts tablets' TCP sessions.
#[derive(Debug)]
pub(crate) struct TcpAcceptor {
    sessions: tokio::sync::Mutex<mpsc::Receiver<(Arc<TcpSession>, Fingerprint)>>,
    local: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for TcpAcceptor {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TcpAcceptor {
    pub(crate) fn bind(
        addr: SocketAddr,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<Self, TransportError> {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
        if addr.is_ipv6() {
            // Dual-stack, as for QUIC: Windows defaults to IPv6-only.
            socket.set_only_v6(false)?;
        }
        socket.set_reuse_address(true)?;
        socket.bind(&addr.into())?;
        socket.listen(64)?;
        socket.set_nonblocking(true)?;
        let listener = TcpListener::from_std(socket.into())?;
        let local = listener.local_addr()?;
        let (tx, rx) = mpsc::channel(4);
        let task = tokio::spawn(accept_loop(listener, TlsAcceptor::from(config), tx));
        Ok(Self {
            sessions: tokio::sync::Mutex::new(rx),
            local,
            task,
        })
    }

    pub(crate) fn local_addr(&self) -> SocketAddr {
        self.local
    }

    /// The next complete session and the tablet's fingerprint.
    pub(crate) async fn accept(&self) -> Option<(Arc<TcpSession>, Fingerprint)> {
        self.sessions.lock().await.recv().await
    }
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    sessions: mpsc::Sender<(Arc<TcpSession>, Fingerprint)>,
) {
    let pending: Arc<Mutex<PendingMap>> = Arc::default();
    let handshakes = Arc::new(tokio::sync::Semaphore::new(MAX_PENDING));
    loop {
        let (tcp, remote) = match listener.accept().await {
            Ok(c) => c,
            Err(e) => {
                // Out of sockets, for example: don't spin.
                tracing::debug!("TCP accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(permit) = handshakes.clone().try_acquire_owned() else {
            tracing::debug!(%remote, "too many TCP handshakes at once, refusing one");
            continue;
        };
        let (acceptor, pending, sessions) = (acceptor.clone(), pending.clone(), sessions.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let arrived = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake(&acceptor, tcp)).await;
            let (tls, peer, channel, token) = match arrived {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    tracing::debug!(%remote, "TCP connection refused: {e}");
                    return;
                }
                Err(_) => {
                    tracing::debug!(%remote, "TCP handshake timed out");
                    return;
                }
            };
            let complete = {
                let mut map = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                map.retain(|_, p| p.since.is_none_or(|t| t.elapsed() < HANDSHAKE_TIMEOUT));
                let p = map.entry((peer, token)).or_default();
                p.since.get_or_insert_with(Instant::now);
                match channel {
                    Channel::Control => p.control = Some(tls),
                    Channel::Video => p.video = Some(tls),
                    Channel::Pen => p.pen = Some(tls),
                }
                if p.control.is_some() && p.video.is_some() && p.pen.is_some() {
                    map.remove(&(peer, token))
                } else {
                    None
                }
            };
            if let Some(Partial {
                control: Some(control),
                video: Some(video),
                pen: Some(pen),
                ..
            }) = complete
            {
                let session = TcpSession::start(remote, control, video, pen);
                let _ = sessions.send((session, peer)).await;
            }
        });
    }
}

/// TLS handshake and preamble of one incoming connection.
async fn handshake(
    acceptor: &TlsAcceptor,
    tcp: TcpStream,
) -> Result<(Tls, Fingerprint, Channel, Token), TransportError> {
    tcp.set_nodelay(true)?;
    let mut tls = acceptor.accept(tcp).await?;
    let peer = tls
        .get_ref()
        .1
        .peer_certificates()
        .and_then(<[_]>::first)
        .map(|c| Fingerprint::of(c))
        .ok_or(TransportError::NoPeerCertificate)?;
    let mut preamble = [0u8; 1 + TOKEN_LEN];
    tls.read_exact(&mut preamble).await?;
    let channel = Channel::from_byte(preamble[0]).ok_or_else(|| tcp_error("unknown channel"))?;
    if channel == Channel::Video {
        socket2::SockRef::from(tls.get_ref().0).set_send_buffer_size(VIDEO_SEND_BUFFER)?;
    }
    let mut token: Token = [0; TOKEN_LEN];
    token.copy_from_slice(&preamble[1..]);
    Ok((TlsStream::Server(tls), peer, channel, token))
}
