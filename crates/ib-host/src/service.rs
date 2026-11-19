//! The host as a service: everything `inkbridge-host` starts, as a library
//! call, so the command line and the tray app run the same thing.
//!
//! [`Service::start`] sets up the OS integrations, the pen thread and the
//! network host and runs it in the background; [`Service::status`] follows
//! what it does; [`Service::shutdown`] stops it and releases the pen.

use std::net::{Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use ib_inject::{FakePenSink, PenSink};
use ib_session::PairingStore;
use ib_transport::PairingOffer;

use crate::pen_thread::{self, PenInput, SinkFactory};
use crate::{Host, HostConfig, HostStatus, platform, storage};

/// How to start the host. The defaults are the plain `inkbridge-host`.
#[derive(Debug, Clone)]
pub struct ServiceOptions {
    pub port: u16,
    /// Display to show, numbered from 1 as `--list-displays` prints them.
    pub display: Option<usize>,
    /// Advanced: keep `adb reverse` set up for tablets on a USB cable.
    pub adb: bool,
    /// Log pen events instead of injecting them (testing).
    pub fake_sink: bool,
}

impl Default for ServiceOptions {
    fn default() -> Self {
        Self {
            port: ib_protocol::DEFAULT_PORT,
            display: None,
            adb: false,
            fake_sink: false,
        }
    }
}

/// A running host.
pub struct Service {
    host: Arc<Host>,
    pen_tx: std::sync::mpsc::Sender<PenInput>,
    pen_thread: Option<std::thread::JoinHandle<()>>,
    task: tokio::task::JoinHandle<()>,
    config_dir: PathBuf,
    name: String,
}

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl Service {
    /// Starts the host. Call from inside a Tokio runtime.
    pub fn start(options: &ServiceOptions) -> anyhow::Result<Self> {
        platform::init_process();
        // After a crash or restart the driver's monitor may be left on the
        // desktop; it belongs there only while a tablet is an extra screen.
        #[cfg(windows)]
        if !platform::extend_wanted()
            && let Err(e) = ib_capture::detach_virtual_monitor()
        {
            tracing::warn!("virtual display: {e}");
        }

        let dir = storage::config_dir();
        let identity = storage::load_or_create_identity(&dir)?;
        let store = PairingStore::load(&dir.join("paired-devices.txt"))?;
        #[cfg(all(target_os = "linux", feature = "wayland"))]
        if crate::wayland::is_wayland_session() && !options.fake_sink {
            // Pointer control is only needed when the pressure pen (uinput)
            // isn't set up; asking for less keeps the desktop's dialog simple.
            let probe = ib_geometry::Region {
                x: 0,
                y: 0,
                width: 1920,
                height: 1080,
            };
            let uinput = ib_inject::UinputPenSink::new(probe).is_ok();
            if let Err(e) = crate::wayland::connect(&dir, !uinput) {
                tracing::warn!("Wayland screen sharing unavailable: {e:#}");
            }
        }
        let region = match options.display {
            Some(number) => platform::select_display(number)?,
            None => platform::primary_display(),
        };
        platform::explain_permissions();

        let fake = options.fake_sink;
        let make_sink: SinkFactory = if fake {
            Box::new(|| Ok(Box::new(LoggingSink(FakePenSink::default())) as Box<dyn PenSink>))
        } else {
            Box::new(move || platform::pen_sink(region))
        };
        // Advertise what the backend actually delivers (probe it once).
        let (pen_features, keyboard) = if fake {
            let sink = FakePenSink::default();
            (sink.features(), sink.keyboard())
        } else {
            platform::pen_sink(region).map_or_else(
                |e| {
                    tracing::warn!("{e}");
                    (ib_protocol::PenFeatures::default(), false)
                },
                |s| (s.features(), s.keyboard()),
            )
        };
        let (pen_tx, pen_thread) = pen_thread::spawn(make_sink, region)?;

        let name = platform::host_name();
        let config = HostConfig {
            host_name: name.clone(),
            bind: SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), options.port),
            region,
            pen_features,
            keyboard,
            video: platform::video_supported(),
        };
        let host = Host::new(identity, config, store, pen_tx.clone())?;
        tracing::info!(addr = %host.local_addr()?, ?region, config = %dir.display(), "host listening");
        if options.adb {
            let adb = crate::adb::find_adb().ok_or_else(|| {
                anyhow::anyhow!(
                    "adb not found. Install Android's platform tools and put adb on the PATH \
                     (or set ANDROID_HOME)."
                )
            })?;
            tracing::info!(adb = %adb.display(), "tablets on a USB cable with USB debugging connect through adb");
            crate::adb::keep_reversed(adb, host.local_addr()?.port());
        }
        let task = tokio::spawn(host.clone().run(|name| {
            tracing::info!(device = name, "paired; pairing is closed");
        }));
        Ok(Self {
            host,
            pen_tx,
            pen_thread: Some(pen_thread),
            task,
            config_dir: dir,
            name,
        })
    }

    /// The network host (pairing, paired tablets, the session).
    pub fn host(&self) -> &Arc<Host> {
        &self.host
    }

    /// Follows the host's status.
    pub fn status(&self) -> tokio::sync::watch::Receiver<HostStatus> {
        self.host.status()
    }

    /// This computer's name as tablets show it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where the host keeps its identity and pairings.
    pub fn config_dir(&self) -> &std::path::Path {
        &self.config_dir
    }

    /// Opens pairing; the offer is what the QR code shows.
    pub async fn open_pairing(&self) -> anyhow::Result<PairingOffer> {
        self.host.open_pairing().await
    }

    /// Stops the host and releases the pen.
    pub fn shutdown(mut self) {
        self.task.abort();
        let _ = self.pen_tx.send(PenInput::Shutdown);
        if let Some(t) = self.pen_thread.take() {
            let _ = t.join();
        }
    }
}

/// Fake sink that logs each injected event (`--fake-sink`).
struct LoggingSink(FakePenSink);

impl PenSink for LoggingSink {
    fn name(&self) -> &'static str {
        "logging-fake"
    }
    fn features(&self) -> ib_protocol::PenFeatures {
        self.0.features()
    }
    fn emit(
        &mut self,
        s: &ib_protocol::PenSample,
        at: ib_geometry::DesktopPoint,
    ) -> Result<(), ib_inject::InjectError> {
        tracing::info!(x = at.x, y = at.y, pressure = s.pressure, state = ?s.state, "pen");
        self.0.emit(s, at)
    }
    fn release(&mut self) -> Result<(), ib_inject::InjectError> {
        tracing::info!("pen released");
        self.0.release()
    }
    fn keyboard(&self) -> bool {
        true
    }
    fn type_text(&mut self, text: &str) -> Result<(), ib_inject::InjectError> {
        tracing::info!(text, "typed");
        self.0.type_text(text)
    }
    fn key(
        &mut self,
        key: ib_protocol::Key,
        modifiers: ib_protocol::Modifiers,
    ) -> Result<(), ib_inject::InjectError> {
        tracing::info!(?key, ?modifiers, "key");
        self.0.key(key, modifiers)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole host starts on a free port with a fake pen, reports no
    /// tablet, opens pairing on request and stops cleanly.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_service_starts_reports_and_stops() {
        let service = Service::start(&ServiceOptions {
            port: 0,
            fake_sink: true,
            ..ServiceOptions::default()
        })
        .unwrap();
        let mut status = service.status();
        assert_eq!(status.borrow().tablet, None);
        let offer = service.open_pairing().await.unwrap();
        assert!(offer.to_uri().starts_with("inkbridge:pair?"));
        status.changed().await.unwrap();
        assert!(status.borrow().pairing_open);
        service.host().close_pairing().await;
        assert!(!service.status().borrow().pairing_open);
        service.shutdown();
    }
}
