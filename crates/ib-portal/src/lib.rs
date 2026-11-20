//! Linux Wayland desktop access through xdg-desktop-portal (GNOME, KDE,
//! wlroots…).
//!
//! On Wayland an application can't read the screen or move the pointer by
//! itself; the desktop asks the user once and then hands out:
//! - a **PipeWire stream** of the chosen monitor (screen cast), and
//! - optionally **pointer control** (remote desktop) on the same session, so
//!   one dialog covers both.
//!
//! With a **restore token** (persist mode "until revoked"), later sessions
//! reuse that consent without showing the dialog again.
//!
//! The portal is asynchronous D-Bus; this crate runs it on its own thread
//! and gives the rest of the host plain synchronous handles. Pointer events
//! are queued (never block the pen thread) and delivered in order.

#![cfg(target_os = "linux")]

use std::os::fd::OwnedFd;
use std::sync::{Mutex, OnceLock, mpsc};

use ashpd::desktop::remote_desktop::{
    DeviceType, NotifyPointerAxisOptions, NotifyPointerButtonOptions,
    NotifyPointerMotionAbsoluteOptions, NotifyTouchDownOptions, NotifyTouchMotionOptions,
    NotifyTouchUpOptions, RemoteDesktop, SelectDevicesOptions, StartOptions,
};
use ashpd::desktop::screencast::{
    CursorMode, OpenPipeWireRemoteOptions, Screencast, SelectSourcesOptions, SourceType,
    StartCastOptions,
};
use ashpd::desktop::{CreateSessionOptions, PersistMode, Session};
use tokio::sync::mpsc as amp;

/// Linux evdev code of the left mouse button.
pub const BTN_LEFT: i32 = 0x110;

#[derive(Debug, thiserror::Error)]
pub enum PortalError {
    /// The user declined the dialog (or closed it).
    #[error("the desktop did not allow screen sharing")]
    Denied,
    /// No portal, or the portal lacks a needed interface/version.
    #[error("desktop portal unavailable: {0}")]
    Unavailable(String),
}

fn portal_err(e: ashpd::Error) -> PortalError {
    match e {
        ashpd::Error::Response(_) => PortalError::Denied,
        other => PortalError::Unavailable(other.to_string()),
    }
}

/// What to ask the desktop for.
#[derive(Debug, Clone, Default)]
pub struct Request {
    /// Token from a previous [`Grant::restore_token`]; skips the dialog
    /// while the user hasn't revoked it.
    pub restore_token: Option<String>,
    /// Also ask for pointer control (needed when uinput isn't set up).
    pub pointer: bool,
}

/// The monitor stream granted by the desktop.
#[derive(Debug)]
pub struct Stream {
    /// PipeWire connection restricted to this stream (hand to PipeWire).
    pub fd: OwnedFd,
    pub node_id: u32,
    /// Size and position in the compositor's logical layout, if reported.
    pub size: Option<(i32, i32)>,
    pub position: Option<(i32, i32)>,
}

/// The granted access. Dropping it ends the portal session, which stops
/// the stream and pointer control.
#[derive(Debug)]
pub struct Grant {
    /// Save this and pass it in the next [`Request`].
    pub restore_token: Option<String>,
    /// Present when pointer control was requested and granted.
    pub pointer: Option<Pointer>,
    /// The desktop offers touch control, whether or not it was granted (a
    /// restored grant keeps the devices it was first given).
    pub touch_offered: bool,
    session: SessionThread,
}

impl Grant {
    /// Ends the portal session and waits until the desktop has closed it.
    /// Dropping the grant also ends it, without waiting; a new request made
    /// right after a drop can be refused while the old session closes.
    pub fn close(mut self) {
        if let Some(stop) = self.session.stop.take() {
            let _ = stop.send(());
        }
        if let Some(done) = self.session.done.take() {
            // Returns once the task has ended (the sender is dropped).
            let _ = done.into_inner().map(|rx| rx.recv());
        }
    }
}

/// One queued pointer event.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PointerCmd {
    MoveTo {
        x: f64,
        y: f64,
    },
    Button {
        code: i32,
        pressed: bool,
    },
    Axis {
        dx: f64,
        dy: f64,
        finish: bool,
    },
    /// Touch points (`slot` = finger), in the stream's logical coordinates.
    TouchDown {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchMotion {
        slot: u32,
        x: f64,
        y: f64,
    },
    TouchUp {
        slot: u32,
    },
}

/// Moves and clicks the desktop pointer (and touches, if granted) through
/// the portal. Cheap to clone;
/// calls queue the event and return immediately.
#[derive(Debug, Clone)]
pub struct Pointer {
    tx: amp::UnboundedSender<PointerCmd>,
    touch: bool,
}

impl Pointer {
    /// A pointer not connected to any desktop: events go to the returned
    /// receiver instead (tests, diagnostics).
    pub fn detached() -> (Self, amp::UnboundedReceiver<PointerCmd>) {
        let (tx, rx) = amp::unbounded_channel();
        (Self { tx, touch: true }, rx)
    }

    /// Whether the desktop granted touch control too (older portals and
    /// desktops may grant only the pointer).
    pub fn has_touch(&self) -> bool {
        self.touch
    }

    /// Absolute move, in the stream's logical coordinates (0,0 = the
    /// monitor's top-left corner).
    pub fn move_to(&self, x: f64, y: f64) -> bool {
        self.tx.send(PointerCmd::MoveTo { x, y }).is_ok()
    }

    /// Presses or releases an evdev button ([`BTN_LEFT`]…).
    pub fn button(&self, code: i32, pressed: bool) -> bool {
        self.tx.send(PointerCmd::Button { code, pressed }).is_ok()
    }

    /// Smooth scroll in logical pixels; `finish` ends the gesture (kinetic
    /// scrolling in apps that support it).
    pub fn axis(&self, dx: f64, dy: f64, finish: bool) -> bool {
        self.tx.send(PointerCmd::Axis { dx, dy, finish }).is_ok()
    }

    /// Puts finger `slot` down at a logical position.
    pub fn touch_down(&self, slot: u32, x: f64, y: f64) -> bool {
        self.tx.send(PointerCmd::TouchDown { slot, x, y }).is_ok()
    }

    /// Moves a finger that is down.
    pub fn touch_motion(&self, slot: u32, x: f64, y: f64) -> bool {
        self.tx.send(PointerCmd::TouchMotion { slot, x, y }).is_ok()
    }

    /// Lifts a finger.
    pub fn touch_up(&self, slot: u32) -> bool {
        self.tx.send(PointerCmd::TouchUp { slot }).is_ok()
    }
}

/// Owns the portal session task; dropping it closes the session.
#[derive(Debug)]
struct SessionThread {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    /// Disconnects when the session task has ended.
    done: Option<Mutex<mpsc::Receiver<()>>>,
}

impl Drop for SessionThread {
    fn drop(&mut self) {
        if let Some(s) = self.stop.take() {
            let _ = s.send(());
        }
    }
}

/// Asks the desktop for one monitor (cursor drawn into the frames) and,
/// if requested, pointer control. Blocks until the user answers the dialog
/// (or immediately, with a valid restore token).
pub fn request(req: Request) -> Result<(Stream, Grant), PortalError> {
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    // Dropped when the session task ends: `Grant::close` waits for that.
    let (done_tx, done_rx) = mpsc::channel::<()>();
    runtime()?.spawn(async move {
        let _done = done_tx;
        run(req, reply_tx, stop_rx).await;
    });
    let (stream, mut grant) = reply_rx
        .recv()
        .map_err(|_| PortalError::Unavailable("portal task ended".into()))??;
    grant.session.stop = Some(stop_tx);
    grant.session.done = Some(Mutex::new(done_rx));
    Ok((stream, grant))
}

/// One runtime for every portal call, kept for the life of the process:
/// ashpd caches its D-Bus connection process-wide, and that connection
/// stops working once the runtime it was created on shuts down (a second
/// request then waits forever). A dedicated thread drives it.
fn runtime() -> Result<&'static tokio::runtime::Handle, PortalError> {
    static RUNTIME: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    if let Some(h) = RUNTIME.get() {
        return Ok(h);
    }
    let (tx, rx) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("inkbridge-portal".into())
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => {
                    let _ = tx.send(Ok(rt.handle().clone()));
                    rt.block_on(std::future::pending::<()>());
                }
                Err(e) => {
                    let _ = tx.send(Err(PortalError::Unavailable(e.to_string())));
                }
            }
        })
        .map_err(|e| PortalError::Unavailable(e.to_string()))?;
    let handle = rx
        .recv()
        .map_err(|_| PortalError::Unavailable("portal thread ended".into()))??;
    // Two first calls at once would each start a thread; one handle wins
    // and the other runtime just idles.
    Ok(RUNTIME.get_or_init(|| handle))
}

type Reply = mpsc::SyncSender<Result<(Stream, Grant), PortalError>>;

async fn run(req: Request, reply: Reply, stop: tokio::sync::oneshot::Receiver<()>) {
    if req.pointer {
        match start_remote(&req).await {
            Ok((share, remote, session, mut rx)) => {
                let node = share.0.node_id;
                let _ = reply.send(Ok(share));
                // Deliver pointer events in order until stopped or all
                // `Pointer` handles are gone.
                tokio::select! {
                    () = pump(&remote, &session, node, &mut rx) => {}
                    _ = stop => {}
                }
                let _ = session.close().await;
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    } else {
        match start_cast(&req).await {
            Ok((share, session)) => {
                let _ = reply.send(Ok(share));
                let _ = stop.await;
                let _ = session.close().await;
            }
            Err(e) => {
                let _ = reply.send(Err(e));
            }
        }
    }
}

fn select_sources_options(req: &Request, persist: bool) -> SelectSourcesOptions {
    let o = SelectSourcesOptions::default()
        .set_cursor_mode(CursorMode::EMBEDDED)
        .set_sources(SourceType::MONITOR)
        .set_multiple(false);
    if persist {
        o.set_persist_mode(PersistMode::ExplicitlyRevoked)
            .set_restore_token(req.restore_token.as_deref())
    } else {
        o
    }
}

/// Node id, size and position of a granted stream.
type Geometry = (u32, Option<(i32, i32)>, Option<(i32, i32)>);

fn first_stream(streams: &[ashpd::desktop::screencast::Stream]) -> Result<Geometry, PortalError> {
    let s = streams
        .first()
        .ok_or_else(|| PortalError::Unavailable("the portal returned no stream".into()))?;
    Ok((s.pipe_wire_node_id(), s.size(), s.position()))
}

async fn start_cast(req: &Request) -> Result<((Stream, Grant), Session<Screencast>), PortalError> {
    let proxy = Screencast::new().await.map_err(portal_err)?;
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(portal_err)?;
    proxy
        .select_sources(&session, select_sources_options(req, true))
        .await
        .map_err(portal_err)?
        .response()
        .map_err(portal_err)?;
    let started = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(portal_err)?
        .response()
        .map_err(portal_err)?;
    let (node_id, size, position) = first_stream(started.streams())?;
    let fd = proxy
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(portal_err)?;
    let grant = Grant {
        restore_token: started.restore_token().map(str::to_owned),
        pointer: None,
        touch_offered: false,
        session: SessionThread {
            stop: None,
            done: None,
        },
    };
    let stream = Stream {
        fd,
        node_id,
        size,
        position,
    };
    Ok(((stream, grant), session))
}

type RemoteStart = (
    (Stream, Grant),
    RemoteDesktop,
    Session<RemoteDesktop>,
    amp::UnboundedReceiver<PointerCmd>,
);

async fn start_remote(req: &Request) -> Result<RemoteStart, PortalError> {
    let remote = RemoteDesktop::new().await.map_err(portal_err)?;
    let cast = Screencast::new().await.map_err(portal_err)?;
    let session = remote
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(portal_err)?;
    // Touch too where the desktop offers it (pinch-zoom in apps); asking
    // for a type it lacks could fail the whole request.
    let touch = remote
        .available_device_types()
        .await
        .unwrap_or(DeviceType::POINTER)
        & DeviceType::TOUCHSCREEN;
    let devices = DeviceType::POINTER | touch;
    // On a combined session the persist mode/token belong to SelectDevices.
    remote
        .select_devices(
            &session,
            SelectDevicesOptions::default()
                .set_devices(devices)
                .set_persist_mode(PersistMode::ExplicitlyRevoked)
                .set_restore_token(req.restore_token.as_deref()),
        )
        .await
        .map_err(portal_err)?
        .response()
        .map_err(portal_err)?;
    cast.select_sources(&session, select_sources_options(req, false))
        .await
        .map_err(portal_err)?
        .response()
        .map_err(portal_err)?;
    let started = remote
        .start(&session, None, StartOptions::default())
        .await
        .map_err(portal_err)?
        .response()
        .map_err(portal_err)?;
    if !started.devices().contains(DeviceType::POINTER) {
        return Err(PortalError::Denied);
    }
    let (node_id, size, position) = first_stream(started.streams())?;
    let fd = cast
        .open_pipe_wire_remote(&session, OpenPipeWireRemoteOptions::default())
        .await
        .map_err(portal_err)?;
    let (tx, rx) = amp::unbounded_channel();
    let share = (
        Stream {
            fd,
            node_id,
            size,
            position,
        },
        Grant {
            restore_token: started.restore_token().map(str::to_owned),
            pointer: Some(Pointer {
                tx,
                touch: started.devices().contains(DeviceType::TOUCHSCREEN),
            }),
            touch_offered: !touch.is_empty(),
            session: SessionThread {
                stop: None,
                done: None,
            },
        },
    );
    Ok((share, remote, session, rx))
}

async fn pump(
    remote: &RemoteDesktop,
    session: &Session<RemoteDesktop>,
    node: u32,
    rx: &mut amp::UnboundedReceiver<PointerCmd>,
) {
    let mut warned = false;
    while let Some(cmd) = rx.recv().await {
        let r = match cmd {
            PointerCmd::MoveTo { x, y } => {
                remote
                    .notify_pointer_motion_absolute(
                        session,
                        node,
                        x,
                        y,
                        NotifyPointerMotionAbsoluteOptions::default(),
                    )
                    .await
            }
            PointerCmd::Button { code, pressed } => {
                let state = if pressed {
                    ashpd::desktop::remote_desktop::KeyState::Pressed
                } else {
                    ashpd::desktop::remote_desktop::KeyState::Released
                };
                remote
                    .notify_pointer_button(
                        session,
                        code,
                        state,
                        NotifyPointerButtonOptions::default(),
                    )
                    .await
            }
            PointerCmd::Axis { dx, dy, finish } => {
                remote
                    .notify_pointer_axis(
                        session,
                        dx,
                        dy,
                        NotifyPointerAxisOptions::default().set_finish(finish),
                    )
                    .await
            }
            PointerCmd::TouchDown { slot, x, y } => {
                remote
                    .notify_touch_down(session, node, slot, x, y, NotifyTouchDownOptions::default())
                    .await
            }
            PointerCmd::TouchMotion { slot, x, y } => {
                remote
                    .notify_touch_motion(
                        session,
                        node,
                        slot,
                        x,
                        y,
                        NotifyTouchMotionOptions::default(),
                    )
                    .await
            }
            PointerCmd::TouchUp { slot } => {
                remote
                    .notify_touch_up(session, slot, NotifyTouchUpOptions::default())
                    .await
            }
        };
        if let Err(e) = r
            && !std::mem::replace(&mut warned, true)
        {
            tracing::warn!("portal rejected a pointer or touch event: {e}");
        }
    }
}
