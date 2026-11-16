//! `inkbridge-host`: command-line host for M1 (pen input, no video yet).
//!
//! Usage: `inkbridge-host [--port N] [--pair] [--fake-sink]`
//! - `--pair`: open pairing even if tablets are already paired;
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
}

fn parse_args() -> anyhow::Result<Args> {
    let mut args = Args {
        port: DEFAULT_PORT,
        force_pairing: false,
        fake_sink: false,
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
            "--fake-sink" => args.fake_sink = true,
            "--fix-usb-routing" => std::process::exit(run_routing(true)),
            "--undo-usb-routing" => std::process::exit(run_routing(false)),
            "-h" | "--help" => {
                println!(
                    "inkbridge-host [--port N] [--pair] [--fake-sink]\n\
                     inkbridge-host --fix-usb-routing    keep internet traffic off USB-tethered tablets (asks for admin once)\n\
                     inkbridge-host --undo-usb-routing   restore Windows' automatic routing for them"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(args)
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
    println!("Pairing code: {uri}\n");
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

    let dir = storage::config_dir();
    let identity = storage::load_or_create_identity(&dir)?;
    let store = PairingStore::load(&dir.join("paired-devices.txt"))?;
    let region = platform::primary_display();

    let make_sink: SinkFactory = if args.fake_sink {
        Box::new(|| Ok(Box::new(LoggingSink(FakePenSink::default())) as Box<dyn PenSink>))
    } else {
        Box::new(ib_inject::platform_sink)
    };
    let features = if args.fake_sink {
        FakePenSink::default().features()
    } else {
        // Advertise what the backend supports; Windows supports everything.
        ib_protocol::PenFeatures {
            pressure: true,
            tilt: true,
            rotation: true,
            hover: true,
            eraser: true,
            barrel_button: true,
        }
    };
    let (pen_tx, pen_handle) = pen_thread::spawn(make_sink, region)?;

    let host_name = std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "Inkbridge host".into());
    let host_label = host_name.clone();
    let bind = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), args.port);
    let config = HostConfig {
        host_name,
        bind,
        region,
        pen_features: features,
        video: ib_capture::platform_source().is_ok(),
    };
    let host = Host::new(identity, config, store, pen_tx.clone())?;
    tracing::info!(addr = %host.local_addr()?, ?region, config = %dir.display(), "host listening");

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
}
