//! `fake-tablet "<pairing code>" [--state-dir DIR]`
//!
//! Pairs with a host (first run) or reconnects (later runs), measures clock
//! offset/RTT, then draws a spiral with a pressure ramp and tilt at 240 Hz,
//! the way a real stylus would report it.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use ib_clock::{ClockSync, Exchange};
use ib_protocol::{
    Capabilities, ClientHello, ControlMessage, PROTOCOL_VERSION, PenBatch, PenFeatures, PenSample,
    PenState, Tool,
};
use ib_session::client_handshake;
use ib_transport::{ClientEndpoint, Connection, Control, Identity, PairingOffer};

const SAMPLE_HZ: u64 = 240;
const SAMPLES_PER_BATCH: usize = 4;

fn now_us(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn load_identity(dir: &PathBuf) -> anyhow::Result<Identity> {
    let (c, k) = (dir.join("cert.der"), dir.join("key.der"));
    if let (Ok(c), Ok(k)) = (std::fs::read(&c), std::fs::read(&k)) {
        return Ok(Identity::from_der(c, k));
    }
    let id = Identity::generate()?;
    std::fs::create_dir_all(dir)?;
    std::fs::write(c, id.cert_der())?;
    std::fs::write(k, id.to_pkcs8_der())?;
    Ok(id)
}

async fn connect_any(client: &ClientEndpoint, offer: &PairingOffer) -> anyhow::Result<Connection> {
    for addr in &offer.addrs {
        match tokio::time::timeout(
            Duration::from_secs(3),
            client.connect(*addr, offer.fingerprint),
        )
        .await
        {
            Ok(Ok(c)) => {
                println!("connected to {addr}");
                return Ok(c);
            }
            Ok(Err(e)) => println!("{addr}: {e}"),
            Err(_) => println!("{addr}: timed out"),
        }
    }
    anyhow::bail!("could not reach the host on any advertised address")
}

async fn sync_clock(ctl: &mut Control, start: Instant) -> anyhow::Result<ClockSync> {
    let mut sync = ClockSync::new(8);
    for _ in 0..8 {
        let t0 = now_us(start);
        ctl.send(&ControlMessage::Ping { t0_us: t0 }).await?;
        if let ControlMessage::Pong {
            t0_us,
            t1_us,
            t2_us,
        } = ctl.recv().await?
        {
            sync.add(Exchange {
                t0: t0_us,
                t1: t1_us,
                t2: t2_us,
                t3: now_us(start),
            });
        }
    }
    Ok(sync)
}

/// Spiral stroke: hover in, press with rising then falling pressure, lift, leave.
fn stroke(start: Instant) -> Vec<PenSample> {
    let n = 480; // 2 s of contact at 240 Hz
    let mut v = Vec::with_capacity(n + 40);
    let t0 = now_us(start);
    let period = 1_000_000 / SAMPLE_HZ;
    let mut push = |i: usize, state: PenState, pressure: f32| {
        let f = i as f32 / n as f32;
        let angle = f * std::f32::consts::TAU * 3.0;
        let r = 0.05 + 0.3 * f;
        v.push(PenSample {
            t_us: t0 + i as u64 * period,
            x: 0.5 + r * angle.cos() * 0.625, // 16:10 aspect correction
            y: 0.5 + r * angle.sin(),
            pressure,
            tilt_x: 30.0 * angle.cos(),
            tilt_y: 30.0 * angle.sin(),
            rotation: 0.0,
            state,
            tool: Tool::Pen,
            buttons: ib_protocol::Buttons::default(),
        });
    };
    for i in 0..20 {
        push(i, PenState::Hover, 0.0);
    }
    for i in 20..n {
        let f = (i - 20) as f32 / (n - 20) as f32;
        push(i, PenState::Contact, (f * std::f32::consts::PI).sin()); // 0 → 1 → 0
    }
    for i in n..n + 20 {
        push(i, PenState::Hover, 0.0);
    }
    push(n + 20, PenState::OutOfRange, 0.0);
    v
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let code = args
        .next()
        .context("usage: fake-tablet \"<pairing code>\" [--state-dir DIR]")?;
    let state_dir = match (args.next().as_deref(), args.next()) {
        (Some("--state-dir"), Some(d)) => PathBuf::from(d),
        _ => std::env::temp_dir().join("inkbridge-fake-tablet"),
    };
    let offer = PairingOffer::parse(&code)?;
    let identity = load_identity(&state_dir)?;
    let paired_marker = state_dir.join(format!("paired-{}", offer.fingerprint.to_hex()));

    let client = ClientEndpoint::new(identity)?;
    let conn = connect_any(&client, &offer).await?;
    let mut ctl = conn.open_control().await?;

    let hello = ClientHello {
        version: PROTOCOL_VERSION,
        device_name: "fake-tablet".into(),
        caps: Capabilities {
            video_codecs: vec![],
            max_width: 2560,
            max_height: 1600,
            max_fps: 0,
            pen: PenFeatures {
                pressure: true,
                tilt: true,
                rotation: false,
                hover: true,
                eraser: true,
                barrel_button: false,
            },
        },
        screen_width: 2560,
        screen_height: 1600,
    };
    let pair = (!paired_marker.exists()).then(|| (offer.secret.0, "fake-tablet".to_owned()));
    let host = client_handshake(&mut ctl, hello, pair).await?;
    std::fs::write(&paired_marker, b"")?;
    println!(
        "session with {} (region {:?}, pen {:?})",
        host.host_name, host.region, host.caps.pen
    );

    let start = Instant::now();
    let sync = sync_clock(&mut ctl, start).await?;
    println!(
        "clock offset {} µs, min RTT {} µs (QUIC RTT {:?})",
        sync.offset_us().unwrap_or_default(),
        sync.min_rtt_us().unwrap_or_default(),
        conn.rtt()
    );

    // Real-time playback: one batch every SAMPLES_PER_BATCH samples.
    let samples = stroke(start);
    let pen = conn.pen();
    let mut last_state = None;
    let mut ticker = tokio::time::interval(Duration::from_micros(
        1_000_000 / SAMPLE_HZ * SAMPLES_PER_BATCH as u64,
    ));
    for (seq, chunk) in samples.chunks(SAMPLES_PER_BATCH).enumerate() {
        ticker.tick().await;
        pen.send(&PenBatch {
            seq: u32::try_from(seq + 1)?,
            samples: chunk.to_vec(),
        })?;
        for s in chunk {
            if last_state != Some(s.state) {
                ctl.send(&ControlMessage::PenTransition(*s)).await?;
                last_state = Some(s.state);
            }
        }
    }
    ctl.send(&ControlMessage::Goodbye).await?;
    ctl.close_send().await;
    conn.close("done");
    println!("stroke sent ({} samples)", samples.len());
    Ok(())
}
