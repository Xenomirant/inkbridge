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

/// Below 32768, outside the ranges Windows (49152+), Linux (32768+) and WSL hand
/// out automatically, so it is rarely taken; `Host` falls back if it is.
const DEFAULT_PORT: u16 = 27840;

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
            "-h" | "--help" => {
                println!("inkbridge-host [--port N] [--pair] [--fake-sink]");
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(args)
}

fn print_offer(offer: &PairingOffer) {
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
    let bind = SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), args.port);
    let config = HostConfig {
        host_name,
        bind,
        region,
        pen_features: features,
    };
    let host = Host::new(identity, config, store, pen_tx.clone())?;
    tracing::info!(addr = %host.local_addr()?, ?region, config = %dir.display(), "host listening");

    if args.force_pairing || host.paired_count().await == 0 {
        print_offer(&host.open_pairing().await?);
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
