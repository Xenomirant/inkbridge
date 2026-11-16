//! The pairing QR code as a page in the default browser.
//!
//! Terminal-drawn QR codes are unreliable to scan (dark themes invert them,
//! fonts add gaps between rows), so the host also writes a crisp
//! black-on-white SVG inside a small HTML page and opens it.

use std::path::{Path, PathBuf};

use ib_transport::PairingOffer;
use qrcode::QrCode;
use qrcode::render::svg;

/// Writes the pairing page into `dir` and returns its path.
pub fn write(dir: &Path, offer: &PairingOffer, host_name: &str) -> anyhow::Result<PathBuf> {
    let uri = offer.to_uri();
    let code = QrCode::new(uri.as_bytes())?;
    let image = code
        .render::<svg::Color<'_>>()
        .min_dimensions(360, 360)
        .dark_color(svg::Color("#000000"))
        .light_color(svg::Color("#ffffff"))
        .quiet_zone(true)
        .build();
    let html = format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><title>Pair with Inkbridge</title>
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
 body {{ margin: 0; min-height: 100vh; display: grid; place-items: center; background: #fff; color: #111;
        font: 16px/1.5 system-ui, sans-serif; }}
 main {{ text-align: center; padding: 24px; }}
 svg {{ width: min(70vmin, 520px); height: auto; }}
 code {{ display: block; margin-top: 16px; font-size: 12px; color: #666; word-break: break-all; max-width: 520px; }}
</style></head>
<body><main>
<h1>Pair a tablet with {host}</h1>
<p>Open <b>Inkbridge</b> on the tablet → <b>Pair with a computer</b>, and scan this code.</p>
{image}
<p>This code works once. Close this page after pairing.</p>
<code>{uri}</code>
</main></body></html>"#,
        host = html_escape(host_name),
        uri = html_escape(&uri),
    );
    std::fs::create_dir_all(dir)?;
    let path = dir.join("pairing.html");
    std::fs::write(&path, html)?;
    Ok(path)
}

/// Opens a file with the user's default application.
pub fn open(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let mut cmd = {
        let mut c = std::process::Command::new("cmd");
        // `start ""` treats the first quoted argument as the window title.
        c.args(["/C", "start", ""]).arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    cmd.spawn().map(|_| ())
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use ib_transport::{Fingerprint, PairingSecret};

    #[test]
    fn page_contains_svg_and_escaped_uri() {
        let dir = std::env::temp_dir().join(format!("ib-pairpage-{}", std::process::id()));
        let offer = PairingOffer {
            addrs: vec!["192.168.1.2:27840".parse().unwrap()],
            fingerprint: Fingerprint([1; 32]),
            secret: PairingSecret([2; 16]),
        };
        let path = write(&dir, &offer, "PC <test>").unwrap();
        let html = std::fs::read_to_string(&path).unwrap();
        assert!(html.contains("<svg"));
        assert!(html.contains("PC &lt;test&gt;"));
        assert!(html.contains("v=1&amp;a=192.168.1.2:27840"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
