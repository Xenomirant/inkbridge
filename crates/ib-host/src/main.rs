//! `inkbridge-host`: command-line host for M1 (pen input, no video yet).
//!
//! Usage: `inkbridge-host [--port N] [--pair] [--display N] [--extend] [--fake-sink]`,
//! `--list-paired`, `--forget N|all`, `--adb` (USB through adb reverse)
//! - `--pair`: open pairing even if tablets are already paired;
//! - `--display N`: show and draw on display N of `--list-displays`;
//! - `--extend`: macOS: the tablet is an extra screen, not a copy of one;
//!   with `--sharp` it is shaped so the tablet shows it pixel for pixel;
//! - `--fake-sink`: log pen events instead of injecting them (testing).

use std::net::{Ipv6Addr, SocketAddr};

use ib_host::pen_thread::{self, SinkFactory};
use ib_host::{Host, HostConfig, platform, storage};
use ib_inject::{FakePenSink, PenSink};
use ib_session::PairingStore;
use ib_transport::PairingOffer;
use tracing_subscriber::EnvFilter;

/// `Host` falls back to a free port if this one is taken.
const DEFAULT_PORT: u16 = ib_protocol::DEFAULT_PORT;

struct Args {
    port: u16,
    force_pairing: bool,
    fake_sink: bool,
    /// Display to show, numbered from 1 as `--list-displays` prints them.
    display: Option<usize>,
    /// Advanced: keep `adb reverse` set up so a tablet on a USB cable
    /// connects through it.
    adb: bool,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        port: DEFAULT_PORT,
        force_pairing: false,
        fake_sink: false,
        display: None,
        adb: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => {
                args.port = it
                    .next()
                    .ok_or_else(|| anyhow::anyhow!("--port needs a value"))?
                    .parse()?;
            }
            "--pair" => args.force_pairing = true,
            "--adb" => args.adb = true,
            "--display" => {
                args.display = Some(
                    it.next()
                        .ok_or_else(|| anyhow::anyhow!("--display needs a number"))?
                        .parse()?,
                );
            }
            "--extend" => {
                anyhow::ensure!(
                    platform::extend_supported(),
                    "the tablet as an extra screen needs macOS, or on Windows the virtual display driver (`--install-virtual-display`)"
                );
                platform::set_extend_wanted(true);
            }
            "--sharp" => platform::set_sharp_wanted(true),
            "--list-displays" => {
                println!("{}", platform::describe_displays());
                std::process::exit(0);
            }
            "--list-paired" => std::process::exit(run_list_paired()),
            "--forget" => {
                let which = it.next().ok_or_else(|| {
                    anyhow::anyhow!("--forget needs a number from --list-paired, or all")
                })?;
                std::process::exit(run_forget(&which));
            }
            "--fake-sink" => args.fake_sink = true,
            #[cfg(windows)]
            "--install-virtual-display" => std::process::exit(report(
                ib_host::vdd_setup::check_platform().and_then(|()| ib_host::vdd_setup::install()),
            )),
            #[cfg(windows)]
            "--detach-virtual-display" => std::process::exit(report(
                ib_capture::detach_virtual_monitor().map_err(Into::into),
            )),
            #[cfg(windows)]
            "--remove-virtual-display" => std::process::exit(report(ib_host::vdd_setup::remove())),
            "--fix-usb-routing" => std::process::exit(run_routing(true)),
            "--undo-usb-routing" => std::process::exit(run_routing(false)),
            #[cfg(all(target_os = "linux", feature = "wayland", feature = "vaapi"))]
            "--self-test" => {
                let report = it
                    .next()
                    .map_or_else(|| "inkbridge-selftest.txt".into(), std::path::PathBuf::from);
                // Own thread: the portal blocks while the desktop's dialog is open.
                let code = std::thread::spawn(move || ib_host::selftest::run(&report))
                    .join()
                    .map_err(|_| anyhow::anyhow!("self-test panicked"))??;
                std::process::exit(code);
            }
            "--setup-uinput" => std::process::exit(run_uinput(true)),
            "--remove-uinput-setup" => std::process::exit(run_uinput(false)),
            "-h" | "--help" => {
                println!(
                    "inkbridge-host [--port N] [--pair] [--display N] [--extend] [--fake-sink]\n\
                     inkbridge-host --extend [--sharp]   macOS: use the tablet as an extra screen (also in the tablet's Screen menu);\n\
                                                         --sharp: pixel for pixel on the tablet, with thin bars\n\
                     inkbridge-host --list-displays      number the connected displays for --display\n\
                     inkbridge-host --adb                USB without tethering (macOS, or tethering off): needs adb and USB debugging\n\
                     inkbridge-host --list-paired        list the paired tablets (forgotten after 90 days unused)\n\
                     inkbridge-host --forget N|all       unpair tablet N of --list-paired, or every tablet\n\
                     inkbridge-host --install-virtual-display  Windows: let the tablet be an extra screen (third-party driver, asks for admin once)\n\
                     inkbridge-host --remove-virtual-display   Windows: undo --install-virtual-display\n\
                     inkbridge-host --detach-virtual-display   Windows: take the virtual monitor off the desktop (if it was left on)\n\
                     inkbridge-host --fix-usb-routing    keep internet traffic off USB-tethered tablets (asks for admin once)\n\
                     inkbridge-host --undo-usb-routing   restore Windows' automatic routing for them\n\
                     inkbridge-host --setup-uinput       Linux: allow pen pressure via a virtual tablet (asks for admin once)\n\
                     inkbridge-host --remove-uinput-setup  Linux: undo --setup-uinput\n\
                     inkbridge-host --self-test [REPORT]  Linux: check encoder, screen sharing and pointer; write a report"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(args)
}

/// Prints an error, if any; the exit code for it.
#[cfg_attr(not(windows), allow(dead_code))]
fn report(result: anyhow::Result<()>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

fn paired_store() -> anyhow::Result<PairingStore> {
    Ok(PairingStore::load(
        &storage::config_dir().join("paired-devices.txt"),
    )?)
}

/// `--list-paired`: the paired tablets, numbered for `--forget`.
fn run_list_paired() -> i32 {
    let store = match paired_store() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e:#}");
            return 1;
        }
    };
    if store.devices().is_empty() {
        println!("No paired tablets.");
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    for (i, d) in store.devices().iter().enumerate() {
        let days = now.saturating_sub(d.last_used) / (24 * 60 * 60);
        let when = match days {
            0 => "today".to_owned(),
            1 => "yesterday".to_owned(),
            n => format!("{n} days ago"),
        };
        println!("{}: {} (last used {when})", i + 1, d.name);
    }
    0
}

/// `--forget N|all`: unpairs tablets; they need the QR code again.
fn run_forget(which: &str) -> i32 {
    let result = paired_store().and_then(|mut store| {
        if which == "all" {
            store.clear()?;
            println!("Forgot every paired tablet.");
            return Ok(());
        }
        let device = which
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| store.devices().get(i).cloned())
            .ok_or_else(|| anyhow::anyhow!("no paired tablet {which}; see --list-paired"))?;
        store.remove(device.fingerprint)?;
        println!(
            "Forgot {}. It needs the QR code to connect again.",
            device.name
        );
        Ok(())
    });
    match result {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

fn run_uinput(enable: bool) -> i32 {
    match ib_host::platform::set_uinput_access(enable) {
        Ok(()) if enable => {
            println!(
                "Done. Log out and back in once (or replug nothing: usually immediate); pen pressure is now available."
            );
            0
        }
        Ok(()) => {
            println!("Removed. Inkbridge falls back to features that need no special access.");
            0
        }
        Err(e) => {
            eprintln!("{e:#}");
            1
        }
    }
}

fn run_routing(fix: bool) -> i32 {
    match ib_host::platform::set_usb_routing(fix) {
        Ok(()) => {
            println!(
                "{}",
                if fix {
                    "Done: USB-tethered tablets no longer carry this PC's internet traffic. Undo with --undo-usb-routing."
                } else {
                    "Done: Windows chooses routes for USB-tethered tablets automatically again."
                }
            );
            0
        }
        Err(e) => {
            eprintln!("Could not change routing: {e:#}");
            1
        }
    }
}

fn show_offer(offer: &PairingOffer, dir: &std::path::Path, host_name: &str) {
    let uri = offer.to_uri();
    match qrcode::QrCode::new(uri.as_bytes()) {
        Ok(code) => {
            let art = code
                .render::<qrcode::render::unicode::Dense1x2>()
                .quiet_zone(true)
                .build();
            println!("\nScan with the Inkbridge tablet app to pair:\n{art}");
        }
        Err(e) => tracing::warn!("could not render QR code: {e}"),
    }
    println!(
        "Pairing code: {uri}\nIt works once, for the next {} minutes; run with --pair for a new one.\n",
        ib_host::server::PAIRING_VALID.as_secs() / 60
    );
    // A real image scans far more reliably than terminal characters.
    match ib_host::pairing_page::write(dir, offer, host_name) {
        Ok(page) => {
            if let Err(e) = ib_host::pairing_page::open(&page) {
                tracing::warn!("could not open {}: {e}", page.display());
            }
            println!(
                "The QR code is also open in your browser: {}",
                page.display()
            );
        }
        Err(e) => tracing::warn!("could not write the pairing page: {e}"),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let args = parse_args()?;
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
    if ib_host::wayland::is_wayland_session() && !args.fake_sink {
        // Pointer control is only needed when the pressure pen (uinput)
        // isn't set up; asking for less keeps the desktop's dialog simple.
        let probe = ib_geometry::Region {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        };
        let uinput = ib_inject::UinputPenSink::new(probe).is_ok();
        if let Err(e) = ib_host::wayland::connect(&dir, !uinput) {
            tracing::warn!("Wayland screen sharing unavailable: {e:#}");
        }
    }
    let region = match args.display {
        Some(number) => platform::select_display(number)?,
        None => platform::primary_display(),
    };
    platform::explain_permissions();

    let make_sink: SinkFactory = if args.fake_sink {
        Box::new(|| Ok(Box::new(LoggingSink(FakePenSink::default())) as Box<dyn PenSink>))
    } else {
        Box::new(move || platform::pen_sink(region))
    };
    // Advertise what the backend actually delivers (probe it once).
    let (features, keyboard) = if args.fake_sink {
        let fake = FakePenSink::default();
        (fake.features(), fake.keyboard())
    } else {
        platform::pen_sink(region).map_or_else(
            |e| {
                tracing::warn!("{e}");
                (ib_protocol::PenFeatures::default(), false)
            },
            |s| (s.features(), s.keyboard()),
        )
    };
    let (pen_tx, pen_handle) = pen_thread::spawn(make_sink, region)?;

    let host_name = platform::host_name();
    let host_label = host_name.clone();
    let bind = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), args.port);
    let config = HostConfig {
        host_name,
        bind,
        region,
        pen_features: features,
        keyboard,
        video: platform::video_supported(),
    };
    let host = Host::new(identity, config, store, pen_tx.clone())?;
    tracing::info!(addr = %host.local_addr()?, ?region, config = %dir.display(), "host listening");
    if args.adb {
        let adb = ib_host::adb::find_adb().ok_or_else(|| {
            anyhow::anyhow!(
                "--adb: adb not found. Install Android's platform tools and put adb on the PATH \
                 (or set ANDROID_HOME)."
            )
        })?;
        tracing::info!(adb = %adb.display(), "--adb: tablets on a USB cable with USB debugging connect through it");
        ib_host::adb::keep_reversed(adb, host.local_addr()?.port());
    }

    if args.force_pairing || host.paired_count().await == 0 {
        show_offer(&host.open_pairing().await?, &dir, &host_label);
    } else {
        println!("Tablets already paired; run with --pair to pair another one.");
    }

    let run = host
        .clone()
        .run(|name| println!("Paired with {name}. Pairing is now closed."));
    tokio::select! {
        () = run => {}
        _ = tokio::signal::ctrl_c() => println!("Shutting down."),
    }
    let _ = pen_tx.send(pen_thread::PenInput::Shutdown);
    let _ = pen_handle.join();
    Ok(())
}

/// Fake sink that logs each injected event (for `--fake-sink`).
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
