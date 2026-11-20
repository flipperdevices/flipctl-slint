//! What sysfs and the device tree say about the sockets and what is in them.
//!
//! A device's `product` and the like take the device's lock, and a device stuck in a
//! reset holds it, so only `describe` reads them, for the device the page shows.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub const DEVICES: &str = "/sys/bus/usb/devices";
const DEVICE_TREE: &str = "/sys/firmware/devicetree/base";
const TYPEC: &str = "/sys/class/typec";

pub fn read(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().trim_end_matches('\0').to_string())
        .unwrap_or_default()
}

/// A device's or a port's directory under /sys/bus/usb/devices.
pub fn device(name: impl AsRef<Path>) -> PathBuf {
    Path::new(DEVICES).join(name)
}

/// Devices plugged into a socket: not hubs, not wired inside the case, and not the
/// virtual ones USB/IP attaches.
pub fn sockets() -> Vec<String> {
    let Ok(dir) = std::fs::read_dir(DEVICES) else { return vec![] };
    let mut out: Vec<String> = dir
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| !name.contains(':') && !name.starts_with("usb"))
        .filter(|name| {
            let d = device(name);
            d.join("idVendor").exists()
                && read(&d.join("bDeviceClass")) != "09"
                && !std::fs::canonicalize(&d)
                    .map(|p| p.to_string_lossy().contains("vhci_hcd"))
                    .unwrap_or(false)
                && read(&port_dir(name).join("connect_type")) != "hardwired"
        })
        .collect();
    out.sort();
    out
}

/// The hub port directory a device is plugged into.
fn port_dir(name: &str) -> PathBuf {
    match name.rsplit_once('.') {
        Some((hub, n)) => device(format!("{hub}/{hub}:1.0/{hub}-port{n}")),
        None => {
            let (bus, n) = name.split_once('-').unwrap_or((name, ""));
            device(format!("usb{bus}/{bus}-0:1.0/usb{bus}-port{n}"))
        }
    }
}

/// Product, ID and speed of a device on this machine.
pub fn describe(port: &str) -> (String, String, String) {
    let d = device(port);
    let product = [read(&d.join("product")), read(&d.join("manufacturer"))]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown device".into());
    let ident = format!("{}:{}", read(&d.join("idVendor")), read(&d.join("idProduct")));
    let speed = read(&d.join("speed"));
    let shown = match speed.as_str() {
        "1.5" | "12" | "480" => format!("{speed}M"),
        _ => format!("{speed}M as 480M"),
    };
    (product, ident, shown)
}

fn phandle(value: u32) -> Option<PathBuf> {
    static MAP: OnceLock<HashMap<u32, PathBuf>> = OnceLock::new();
    let map = MAP.get_or_init(|| {
        let mut map = HashMap::new();
        let mut stack = vec![PathBuf::from(DEVICE_TREE)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    stack.push(path);
                } else if e.file_name() == "phandle" {
                    if let Ok(b) = std::fs::read(&path) {
                        if b.len() >= 4 {
                            map.insert(u32::from_be_bytes([b[0], b[1], b[2], b[3]]), dir.clone());
                        }
                    }
                }
            }
        }
        map
    });
    map.get(&value).cloned()
}

fn label(node: &Path) -> String {
    read(&node.join("label"))
}

/// The label printed beside the socket a device is in, as USB Analyzer finds it: the
/// hub's `port@N` graph endpoint leads to the connector node. A SuperSpeed port
/// reaches it through a mux and finds nothing, and its USB 2 `peer` answers.
pub fn socket_name(port: &str) -> String {
    static CACHE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(found) = cache.lock().unwrap().get(port) {
        return found.clone();
    }
    let found = find_socket_name(port).unwrap_or_default();
    cache.lock().unwrap().insert(port.to_string(), found.clone());
    found
}

fn find_socket_name(port: &str) -> Option<String> {
    if !port.contains('.') {
        return None;
    }
    let socket = port_dir(port);
    for place in [socket.clone(), socket.join("peer")] {
        let place = std::fs::canonicalize(place).ok()?;
        let name = place.file_name()?.to_string_lossy().into_owned();
        let (owner, n) = name.rsplit_once("-port")?;
        let node = device(owner).join("of_node");
        for base in [node.join("ports"), node.clone()] {
            let Ok(b) = std::fs::read(base.join(format!("port@{n}/endpoint/remote-endpoint")))
            else {
                continue;
            };
            if b.len() < 4 {
                continue;
            }
            let mut target = phandle(u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
            while let Some(t) = target {
                if t == Path::new(DEVICE_TREE) {
                    break;
                }
                if read(&t.join("compatible")).contains("-connector") && !label(&t).is_empty() {
                    return Some(label(&t));
                }
                target = t.parent().map(Path::to_path_buf);
            }
        }
    }
    None
}

/// The label of the Type-C connector the device port is behind.
pub fn pc_port_name() -> String {
    let Ok(dir) = std::fs::read_dir(TYPEC) else { return String::new() };
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
        .map(|p| label(&p.join("device/of_node/connector")))
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_port_is_found_on_its_hub_or_its_root_hub() {
        assert_eq!(port_dir("1-1.2"), PathBuf::from("/sys/bus/usb/devices/1-1/1-1:1.0/1-1-port2"));
        assert_eq!(
            port_dir("2-1.4.3"),
            PathBuf::from("/sys/bus/usb/devices/2-1.4/2-1.4:1.0/2-1.4-port3")
        );
        assert_eq!(port_dir("3-1"), PathBuf::from("/sys/bus/usb/devices/usb3/3-0:1.0/usb3-port1"));
    }
}
