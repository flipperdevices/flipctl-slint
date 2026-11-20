//! The Flipper's ports by the names printed on its case, which the device tree
//! carries as each connector's `label`.

use std::path::PathBuf;

const TYPEC: &str = "/sys/class/typec";

/// The case's name for the USB-C port a PC plugs into, such as "USB-C1": the first
/// Type-C port whose connector is labelled. Empty when the device tree names none.
pub fn device_port_name() -> String {
    let Ok(dir) = std::fs::read_dir(TYPEC) else { return String::new() };
    // port0, not port0-partner or port0-cable
    let mut ports: Vec<PathBuf> = dir
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().chars().all(|c| c.is_ascii_alphanumeric()))
        })
        .collect();
    ports.sort();
    ports
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(p.join("device/of_node/connector/label")).ok())
        .map(|l| l.trim().trim_end_matches('\0').to_string())
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}
