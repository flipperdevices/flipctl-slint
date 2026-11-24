//! USB IP: a device served to the network, and the ones other Flippers serve.
//!
//! A device sent to USB IP is served by USB/IP on port 3240 (flipctl's usbip-share
//! claim) and announced by a UDP broadcast on port 3241 every second. The beacons
//! heard are the remote devices the page lists; one is attached through flipctl's
//! usbip-attach claim.
//!
//! A broadcast stays on its LAN, so a Transmitter behind a VPN or a router is never
//! heard. For those the page takes addresses, and each Transmitter is asked what it
//! serves with USB/IP's own device list, as `usbip list -r` asks: only port 3240 has to
//! reach it, and any usbipd answers, a Linux PC's included.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpStream, UdpSocket};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::json;

use crate::sysfs::{describe, device, read};

pub const BEACON_PORT: u16 = 3241;
const USBIP_PORT: u16 = 3240;
/// How long a beacon counts as heard.
const FRESH: Duration = Duration::from_secs(3);
/// How often a typed address is asked, and how long it has to answer.
const ASK_EVERY: Duration = Duration::from_secs(1);
const ASK_WAIT: Duration = Duration::from_millis(1500);

/// This Flipper's address on its default route. Connecting a UDP socket picks the
/// route and sends nothing; 192.0.2.1 is reserved for documentation.
pub fn own_address() -> String {
    UdpSocket::bind(("0.0.0.0", 0))
        .and_then(|s| s.connect(("192.0.2.1", 9)).and_then(|_| s.local_addr()))
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "-".into())
}

/// An address as /proc/net/tcp prints it: hex, each 32-bit word in host order.
pub fn proc_address(field: &str) -> Option<IpAddr> {
    let hex = field.split(':').next()?;
    let bytes: Vec<u8> = (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect::<Option<_>>()?;
    let mut raw: Vec<u8> = bytes.chunks(4).flat_map(|w| w.iter().rev().copied()).collect();
    if raw.len() == 16 && raw[..10].iter().all(|b| *b == 0) && raw[10..12] == [0xff, 0xff] {
        raw.drain(..12);
    }
    match raw.len() {
        4 => Some(IpAddr::V4(Ipv4Addr::new(raw[0], raw[1], raw[2], raw[3]))),
        16 => Some(IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(raw).ok()?))),
        _ => None,
    }
}

/// Who is connected to USB/IP here: the established connection on its port, which
/// usbip-host keeps in the kernel once a receiver has attached.
fn usbip_peer() -> Option<IpAddr> {
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        for line in read(std::path::Path::new(table)).lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() > 3 && f[3] == "01" && f[1].ends_with(&format!(":{USBIP_PORT:04X}")) {
                if let Some(ip) = proc_address(f[2]) {
                    return Some(ip);
                }
            }
        }
    }
    None
}

/// Serve one device by USB/IP and announce it, until stopped.
pub struct Transmitter {
    pub port: String,
    state: Arc<Mutex<String>>,
    held: Arc<Mutex<Option<UnixStream>>>,
    stopping: Arc<AtomicBool>,
}

impl Transmitter {
    pub fn start(port: &str) -> Transmitter {
        let (product, ident, _) = describe(port);
        let t = Transmitter {
            port: port.to_string(),
            state: Arc::new(Mutex::new("starting".into())),
            held: Arc::default(),
            stopping: Arc::default(),
        };
        let (port, state, held, stopping) =
            (t.port.clone(), t.state.clone(), t.held.clone(), t.stopping.clone());
        std::thread::spawn(move || {
            let claim = match flipctl_app::claim(&format!("usbip-share/{port}")) {
                Ok(c) => c,
                Err(e) => {
                    *state.lock().unwrap() = e;
                    return;
                }
            };
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            *held.lock().unwrap() = Some(claim);
            let hostname = read(std::path::Path::new("/proc/sys/kernel/hostname"));
            let beacon = json!({
                "usb-device-bridge": 1, "host": hostname, "busid": port, "product": product,
                "id": ident, "speed": read(&device(&port).join("speed")),
            })
            .to_string();
            let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else { return };
            let _ = socket.set_broadcast(true);
            while !stopping.load(Ordering::SeqCst) {
                let _ = socket.send_to(beacon.as_bytes(), ("255.255.255.255", BEACON_PORT));
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        t
    }

    pub fn state(&self) -> String {
        if self.held.lock().unwrap().is_none() {
            return self.state.lock().unwrap().clone();
        }
        // usbip-host: 1 available, 2 in use
        if read(&device(&self.port).join("usbip_status")) == "2" {
            return format!(
                "sent to {}",
                usbip_peer().map(|ip| ip.to_string()).unwrap_or("a receiver".into())
            );
        }
        "waiting for receiver".into()
    }

    pub fn stop(&self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.held.lock().unwrap().take();
    }
}

impl Drop for Transmitter {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A device another Flipper announces.
#[derive(Clone, Debug, PartialEq)]
pub struct Remote {
    pub ip: String,
    pub busid: String,
    pub product: String,
    pub ident: String,
    pub speed: String,
}

/// Each Transmitter's device by its address and busid, and when it was last heard.
type Heard = Arc<Mutex<HashMap<(String, String), (Remote, Instant)>>>;

/// The addresses typed in, and how many devices each served when last asked: `None`
/// for one that did not answer, absent for one not asked yet.
type Asked = Arc<Mutex<(Vec<Ipv4Addr>, HashMap<Ipv4Addr, Option<usize>>)>>;

/// The Transmitters heard on the LAN in the last few seconds, and the ones at the
/// addresses typed in.
pub struct Beacons {
    seen: Heard,
    asked: Asked,
    closed: Arc<AtomicBool>,
}

impl Beacons {
    pub fn listen() -> std::io::Result<Beacons> {
        let socket = UdpSocket::bind(("0.0.0.0", BEACON_PORT))?;
        socket.set_read_timeout(Some(Duration::from_millis(500)))?;
        let b = Beacons { seen: Arc::default(), asked: Arc::default(), closed: Arc::default() };
        let (seen, asked, closed) = (b.seen.clone(), b.asked.clone(), b.closed.clone());
        std::thread::spawn({
            let (seen, closed) = (seen.clone(), closed.clone());
            move || ask_loop(seen, asked, closed)
        });
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while !closed.load(Ordering::SeqCst) {
                let Ok((n, from)) = socket.recv_from(&mut buf) else { continue };
                let Ok(v) = serde_json::from_slice::<serde_json::Value>(&buf[..n]) else {
                    continue;
                };
                if v.get("usb-device-bridge").and_then(|x| x.as_i64()) != Some(1) {
                    continue;
                }
                let text = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
                let busid = text("busid");
                if busid.is_empty() {
                    continue;
                }
                let r = Remote {
                    ip: from.ip().to_string(),
                    busid: busid.clone(),
                    product: text("product"),
                    ident: text("id"),
                    speed: text("speed"),
                };
                seen.lock().unwrap().insert((r.ip.clone(), busid), (r, Instant::now()));
            }
        });
        Ok(b)
    }

    /// Ask the Transmitters at these addresses too, and nobody else.
    pub fn ask(&self, ips: &[Ipv4Addr]) {
        let mut asked = self.asked.lock().unwrap();
        asked.1.retain(|ip, _| ips.contains(ip));
        asked.0 = ips.to_vec();
    }

    /// How many devices `ip` served when last asked, `Some(None)` if it did not
    /// answer, `None` before the first answer.
    pub fn answer(&self, ip: Ipv4Addr) -> Option<Option<usize>> {
        self.asked.lock().unwrap().1.get(&ip).copied()
    }

    pub fn current(&self) -> Vec<Remote> {
        let mut found: Vec<Remote> = self
            .seen
            .lock()
            .unwrap()
            .values()
            .filter(|(_, at)| at.elapsed() < FRESH)
            .map(|(r, _)| r.clone())
            .collect();
        found.sort_by(|a, b| (&a.ip, &a.product).cmp(&(&b.ip, &b.product)));
        found
    }
}

impl Drop for Beacons {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// Ask each typed address what it serves, once a second and all at once, so one that
/// does not answer holds up none of the others. A device a beacon has named already
/// keeps its name: the list has only the IDs.
fn ask_loop(seen: Heard, asked: Asked, closed: Arc<AtomicBool>) {
    let mut names: HashMap<String, String> = HashMap::new();
    while !closed.load(Ordering::SeqCst) {
        let targets = asked.lock().unwrap().0.clone();
        let lists: Vec<(Ipv4Addr, Option<Vec<_>>)> = std::thread::scope(|scope| {
            let asking: Vec<_> =
                targets.iter().map(|&ip| scope.spawn(move || (ip, device_list(ip).ok()))).collect();
            asking.into_iter().filter_map(|t| t.join().ok()).collect()
        });
        for (ip, list) in lists {
            {
                let mut asked = asked.lock().unwrap();
                if asked.0.contains(&ip) {
                    asked.1.insert(ip, list.as_ref().map(Vec::len));
                }
            }
            for (busid, ident, speed) in list.unwrap_or_default() {
                let product =
                    names.entry(ident.clone()).or_insert_with(|| usb_name(&ident)).clone();
                let key = (ip.to_string(), busid.clone());
                let now = Instant::now();
                let mut seen = seen.lock().unwrap();
                match seen.get_mut(&key) {
                    Some((_, at)) => *at = now,
                    None => {
                        let r = Remote { ip: key.0.clone(), busid, product, ident, speed };
                        seen.insert(key, (r, now));
                    }
                }
            }
        }
        std::thread::sleep(ASK_EVERY);
    }
}

/// USB/IP's `OP_REQ_DEVLIST`: each exported device's busid, `vid:pid` and speed in
/// Mbit/s as sysfs writes it. Everything on the wire is big-endian.
fn device_list(ip: Ipv4Addr) -> std::io::Result<Vec<(String, String, String)>> {
    let mut s = TcpStream::connect_timeout(&SocketAddr::from((ip, USBIP_PORT)), ASK_WAIT)?;
    s.set_read_timeout(Some(ASK_WAIT))?;
    s.set_write_timeout(Some(ASK_WAIT))?;
    // version 1.1.1, OP_REQ_DEVLIST, status 0
    s.write_all(&[0x01, 0x11, 0x80, 0x05, 0, 0, 0, 0])?;
    let mut head = [0u8; 12];
    s.read_exact(&mut head)?;
    if head[2..4] != [0x00, 0x05] || head[4..8] != [0; 4] {
        return Err(std::io::Error::other("not a USB/IP device list"));
    }
    let count = u32::from_be_bytes(head[8..12].try_into().unwrap());
    let mut found = Vec::new();
    for _ in 0..count.min(64) {
        // path[256] busid[32] busnum devnum speed vid pid bcd, six class bytes
        let mut dev = [0u8; 312];
        s.read_exact(&mut dev)?;
        let mut interfaces = vec![0u8; usize::from(dev[311]) * 4];
        s.read_exact(&mut interfaces)?;
        if let Some(d) = parse_device(&dev) {
            found.push(d);
        }
    }
    Ok(found)
}

fn parse_device(dev: &[u8; 312]) -> Option<(String, String, String)> {
    let busid = &dev[256..288];
    let busid = String::from_utf8_lossy(&busid[..busid.iter().position(|b| *b == 0)?]);
    let speed = match u32::from_be_bytes(dev[296..300].try_into().ok()?) {
        1 => "1.5",
        2 => "12",
        3 => "480",
        5 => "5000",
        6 => "10000",
        _ => "",
    };
    let word = |at: usize| u16::from_be_bytes([dev[at], dev[at + 1]]);
    let ident = format!("{:04x}:{:04x}", word(300), word(302));
    Some((busid.into_owned(), ident, speed.into()))
}

/// A device's name from the USB ID database, as `lsusb` would print it, or its IDs.
fn usb_name(ident: &str) -> String {
    let fallback = format!("USB {ident}");
    let Some((vid, pid)) = ident.split_once(':') else { return fallback };
    let db = ["/usr/share/hwdata/usb.ids", "/usr/share/misc/usb.ids", "/var/lib/usbutils/usb.ids"]
        .iter()
        .find_map(|p| std::fs::read(p).ok());
    let Some(db) = db else { return fallback };
    let db = String::from_utf8_lossy(&db);
    let (mut vendor, mut product) = (None, None);
    for line in db.lines() {
        if vendor.is_none() {
            if let Some(name) = line.strip_prefix(vid).and_then(|r| r.strip_prefix("  ")) {
                vendor = Some(name);
            }
        } else if let Some(name) = line.strip_prefix('\t').and_then(|r| r.strip_prefix(pid)) {
            product = name.strip_prefix("  ");
            break;
        } else if !line.starts_with('\t') && !line.starts_with('#') {
            break;
        }
    }
    product.or(vendor).map(String::from).unwrap_or(fallback)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_net_tcp_addresses_decode() {
        assert_eq!(proc_address("7301A8C0:0CA8"), Some("192.168.1.115".parse().unwrap()));
        assert_eq!(
            proc_address("0000000000000000FFFF00007301A8C0:0CA8"),
            Some("192.168.1.115".parse().unwrap())
        );
        assert_eq!(proc_address("zz:0CA8"), None);
    }

    /// One device as usbipd sends it in a device list.
    #[test]
    fn a_listed_device_decodes() {
        let mut dev = [0u8; 312];
        dev[..10].copy_from_slice(b"/sys/x/1-1");
        dev[256..259].copy_from_slice(b"1-1");
        dev[296..300].copy_from_slice(&3u32.to_be_bytes());
        dev[300..302].copy_from_slice(&0x0951u16.to_be_bytes());
        dev[302..304].copy_from_slice(&0x1666u16.to_be_bytes());
        assert_eq!(parse_device(&dev), Some(("1-1".into(), "0951:1666".into(), "480".into())));
    }
}
