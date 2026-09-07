//! What the status bar's signal badge is reading, and from where.
//!
//! The badge has one number and three ways to fail to get it, so this prints each
//! step rather than the answer: which interface was picked, whether the cheap file
//! exists, and what nl80211 says when it does not.
//!
//!     cargo run -p flipper-ui --example wifi_signal

fn main() {
    let mut found = false;
    for entry in std::fs::read_dir("/sys/class/net").expect("no /sys/class/net").flatten() {
        let dir = entry.path();
        if !dir.join("wireless").is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let carrier = std::fs::read_to_string(dir.join("carrier")).unwrap_or_default();
        let state = std::fs::read_to_string(dir.join("operstate")).unwrap_or_default();
        let index = std::fs::read_to_string(dir.join("ifindex")).unwrap_or_default();
        println!(
            "interface {name}: carrier={} operstate={} ifindex={}",
            carrier.trim(),
            state.trim(),
            index.trim()
        );
        found = true;
        println!(
            "  /proc/net/wireless: {}",
            if std::path::Path::new("/proc/net/wireless").exists() { "present" } else { "absent" }
        );
        match flipper_ui::nl80211::signal_percent(&name) {
            Some(pct) => println!("  nl80211 signal_percent: {pct}"),
            None => println!("  nl80211 signal_percent: None  <- the badge draws 0 for this"),
        }
    }
    if !found {
        println!("no wireless interface with a /sys/class/net/*/wireless directory");
    }
}
