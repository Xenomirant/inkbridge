//! Prints detected USB tethering links: `cargo run -p ib-host --example usb_links`.
fn main() {
    let links = ib_host::usb::tether_links();
    if links.is_empty() {
        println!("no USB tethering link found");
    }
    for l in links {
        println!("{} -> tablet at {}", l.description, l.tablet);
    }
}
