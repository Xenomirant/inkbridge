//! Linux Wayland sessions: screen sharing (and, without uinput, pointer
//! control) through the desktop portal.
//!
//! The desktop asks once ("Share your screen with Inkbridge?"); the restore
//! token saved in the config directory skips the dialog on later starts
//! until the user revokes it in the desktop settings.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use ib_capture::{PipeWireScreenSource, ScreenSource};
use ib_inject::{PenSink, PortalPointerSink};

/// The granted desktop share for this process (set once at startup).
pub struct WaylandDesktop {
    grant: ib_portal::Grant,
    capture: PipeWireScreenSource,
}

impl std::fmt::Debug for WaylandDesktop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WaylandDesktop").finish_non_exhaustive()
    }
}

static DESKTOP: OnceLock<WaylandDesktop> = OnceLock::new();

/// True in a Wayland desktop session.
pub fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
}

/// Tokens are per portal kind: a screen-cast-only token can't restore a
/// remote-desktop session and vice versa.
fn token_path(dir: &Path, pointer: bool) -> PathBuf {
    dir.join(if pointer {
        "wayland-remote-desktop-token"
    } else {
        "wayland-screencast-token"
    })
}

/// Written when the desktop was asked for touch and didn't grant it, so it
/// isn't asked on every start.
const NO_TOUCH_MARKER: &str = "wayland-no-touch";

/// Touch control in a remote-desktop grant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Touch {
    Granted,
    /// The desktop offers it, but this grant doesn't include it.
    Missing,
    /// The desktop (or its portal) has no touch control.
    Unavailable,
}

impl Touch {
    fn of(grant: &ib_portal::Grant) -> Self {
        if grant
            .pointer
            .as_ref()
            .is_some_and(ib_portal::Pointer::has_touch)
        {
            Self::Granted
        } else if grant.touch_offered {
            Self::Missing
        } else {
            Self::Unavailable
        }
    }
}

/// Whether to drop a restored grant and show the dialog again for touch.
fn ask_again_for_touch(restored: bool, touch: Touch, declined_before: bool) -> bool {
    restored && touch == Touch::Missing && !declined_before
}

/// Asks the desktop for the screen (and pointer control if `pointer`),
/// reusing and refreshing the saved restore token. Shows the desktop's
/// dialog the first time.
pub fn connect(dir: &Path, pointer: bool) -> anyhow::Result<&'static WaylandDesktop> {
    if let Some(d) = DESKTOP.get() {
        return Ok(d);
    }
    let path = token_path(dir, pointer);
    let saved = std::fs::read_to_string(&path)
        .ok()
        .map(|t| t.trim().to_owned());
    if saved.is_none() {
        println!("Your desktop will ask to share the screen with Inkbridge (only this once).");
    }
    let restored = saved.is_some();
    let (mut stream, mut grant) = ib_portal::request(ib_portal::Request {
        restore_token: saved,
        pointer,
    })?;
    let declined = dir.join(NO_TOUCH_MARKER);
    if ask_again_for_touch(restored, Touch::of(&grant), declined.exists()) {
        // A permission saved before touch support covers only the pointer:
        // ask once more so finger gestures (pinch-zoom) reach apps.
        println!("Your desktop will ask again, now also for touch control (only this once).");
        drop(stream);
        grant.close();
        (stream, grant) = ib_portal::request(ib_portal::Request {
            restore_token: None,
            pointer,
        })?;
        if Touch::of(&grant) != Touch::Granted {
            std::fs::create_dir_all(dir)?;
            std::fs::write(&declined, "")?;
        }
    }
    if pointer {
        tracing::info!(touch = ?Touch::of(&grant), "desktop remote control granted");
    }
    if let Some(t) = &grant.restore_token {
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, t)?;
    }
    let capture = PipeWireScreenSource::new(stream)?;
    let desktop = WaylandDesktop { grant, capture };
    Ok(DESKTOP.get_or_init(|| desktop))
}

/// The shared desktop, if [`connect`] succeeded.
pub fn desktop() -> Option<&'static WaylandDesktop> {
    DESKTOP.get()
}

impl WaylandDesktop {
    /// A capture source for one tablet session (shares the stream).
    pub fn capture(&self) -> Box<dyn ScreenSource> {
        Box::new(self.capture.clone())
    }

    /// Desktop pixels → the portal's logical coordinates.
    pub fn logical_scale(&self) -> f64 {
        self.capture.logical_scale()
    }

    /// The no-root pen fallback (pointer only), if pointer control was granted.
    pub fn pointer_sink(&self) -> Option<Box<dyn PenSink>> {
        let pointer = self.grant.pointer.clone()?;
        Some(Box::new(PortalPointerSink::new(
            pointer,
            self.capture.logical_scale(),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::{Touch, ask_again_for_touch};

    #[test]
    fn asks_again_only_for_an_old_grant_without_touch() {
        assert!(ask_again_for_touch(true, Touch::Missing, false));
        assert!(
            !ask_again_for_touch(false, Touch::Missing, false),
            "just asked"
        );
        assert!(
            !ask_again_for_touch(true, Touch::Granted, false),
            "has touch"
        );
        assert!(
            !ask_again_for_touch(true, Touch::Unavailable, false),
            "not offered"
        );
        assert!(
            !ask_again_for_touch(true, Touch::Missing, true),
            "declined before"
        );
    }
}
