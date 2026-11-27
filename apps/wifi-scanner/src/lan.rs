//! Names for clients from the LAN side, when the Flipper is on the same LAN.
//!
//! Passive by design: a client's address comes only from the Flipper's own neighbour
//! table, and the only questions asked are about that one address, a reverse DNS
//! lookup and one mDNS reverse query. Nothing is swept or probed.

use std::collections::{HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::dot11::Mac;

/// How often the neighbour table is read again.
const EVERY: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Known {
    pub ip: Option<Ipv4Addr>,
    pub name: Option<String>,
}

#[derive(Default)]
struct Shared {
    /// Clients the app wants names for.
    wanted: HashSet<Mac>,
    known: HashMap<Mac, Known>,
    /// The default gateway's MAC: traffic to it is the internet's.
    gateway: Option<Mac>,
    stop: bool,
}

/// The neighbour table watched, and names looked up for the clients asked about.
pub struct Lan {
    shared: Arc<Mutex<Shared>>,
}

impl Lan {
    pub fn start() -> Self {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let worker = shared.clone();
        std::thread::spawn(move || run(&worker));
        Self { shared }
    }

    pub fn want(&self, clients: impl IntoIterator<Item = Mac>) {
        self.shared.lock().unwrap().wanted.extend(clients);
    }

    pub fn get(&self, mac: &Mac) -> Known {
        self.shared.lock().unwrap().known.get(mac).cloned().unwrap_or_default()
    }

    pub fn gateway(&self) -> Option<Mac> {
        self.shared.lock().unwrap().gateway
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        self.shared.lock().unwrap().stop = true;
    }
}

fn run(shared: &Mutex<Shared>) {
    let mut asked: HashSet<Ipv4Addr> = HashSet::new();
    loop {
        let table = crate::nl::neighbours().unwrap_or_default();
        let gateway_ip = default_gateway();
        let todo: Vec<(Mac, Ipv4Addr)> = {
            let mut s = shared.lock().unwrap();
            if s.stop {
                return;
            }
            s.gateway =
                gateway_ip.and_then(|g| table.iter().find(|(_, ip)| *ip == g).map(|(m, _)| *m));
            let mut todo = Vec::new();
            for (mac, ip) in &table {
                if !s.wanted.contains(mac) {
                    continue;
                }
                s.known.entry(*mac).or_default().ip = Some(*ip);
                if asked.insert(*ip) {
                    todo.push((*mac, *ip));
                }
            }
            todo
        };
        for (mac, ip) in todo {
            if let Some(name) = reverse_dns(ip).or_else(|| mdns_reverse(ip)) {
                shared.lock().unwrap().known.entry(mac).or_default().name = Some(name);
            }
        }
        std::thread::sleep(EVERY);
    }
}

/// The IPv4 default gateway, from the kernel's routing table.
fn default_gateway() -> Option<Ipv4Addr> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    routes.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() > 2 && f[1] == "00000000" {
            u32::from_str_radix(f[2], 16).ok().map(|g| Ipv4Addr::from(g.swap_bytes()))
        } else {
            None
        }
    })
}

/// The address's name on the default resolver, if it has one.
fn reverse_dns(ip: Ipv4Addr) -> Option<String> {
    let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    sa.sin_family = libc::AF_INET as libc::sa_family_t;
    sa.sin_addr.s_addr = u32::from(ip).to_be();
    let mut host = [0 as libc::c_char; 256];
    let got = unsafe {
        libc::getnameinfo(
            std::ptr::addr_of!(sa).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
            host.as_mut_ptr(),
            host.len() as libc::socklen_t,
            std::ptr::null_mut(),
            0,
            libc::NI_NAMEREQD,
        )
    };
    if got != 0 {
        return None;
    }
    let name = unsafe { std::ffi::CStr::from_ptr(host.as_ptr()) }.to_string_lossy().into_owned();
    Some(short(&name))
}

/// One mDNS question, "who is this address", answered by the device itself if it runs
/// a responder.
fn mdns_reverse(ip: Ipv4Addr) -> Option<String> {
    let o = ip.octets();
    let qname = format!("{}.{}.{}.{}.in-addr.arpa", o[3], o[2], o[1], o[0]);
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.set_read_timeout(Some(Duration::from_millis(700))).ok()?;
    socket.send_to(&query(&qname), SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353)).ok()?;
    let mut buf = [0u8; 1500];
    for _ in 0..4 {
        let (n, _) = socket.recv_from(&mut buf).ok()?;
        if let Some(name) = answer_ptr(&buf[..n]) {
            return Some(short(&name));
        }
    }
    None
}

/// A PTR question for `name`, asking for a unicast answer.
fn query(name: &str) -> Vec<u8> {
    let mut q = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in name.split('.') {
        q.push(label.len() as u8);
        q.extend_from_slice(label.as_bytes());
    }
    q.extend_from_slice(&[0, 0, 12, 0x80, 1]);
    q
}

/// A name at `at`, following compression pointers.
fn name_at(msg: &[u8], mut at: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut end = None;
    for _ in 0..32 {
        let len = *msg.get(at)? as usize;
        if len == 0 {
            return Some((labels.join("."), end.unwrap_or(at + 1)));
        }
        if len & 0xc0 == 0xc0 {
            let ptr = (len & 0x3f) << 8 | *msg.get(at + 1)? as usize;
            end.get_or_insert(at + 2);
            at = ptr;
            continue;
        }
        labels.push(String::from_utf8_lossy(msg.get(at + 1..at + 1 + len)?).into_owned());
        at += 1 + len;
    }
    None
}

/// The first PTR answer of a DNS response.
fn answer_ptr(msg: &[u8]) -> Option<String> {
    if msg.len() < 12 || msg[2] & 0x80 == 0 {
        return None;
    }
    let questions = u16::from_be_bytes([msg[4], msg[5]]);
    let answers = u16::from_be_bytes([msg[6], msg[7]]);
    let mut at = 12;
    for _ in 0..questions {
        at = name_at(msg, at)?.1 + 4;
    }
    for _ in 0..answers {
        let (_, next) = name_at(msg, at)?;
        let kind = u16::from_be_bytes([*msg.get(next)?, *msg.get(next + 1)?]);
        let len = u16::from_be_bytes([*msg.get(next + 8)?, *msg.get(next + 9)?]) as usize;
        if kind == 12 {
            return name_at(msg, next + 10).map(|(n, _)| n);
        }
        at = next + 10 + len;
    }
    None
}

/// The host part of a name, without `.local` or the search domain.
fn short(name: &str) -> String {
    let name = name.trim_end_matches('.');
    name.split('.').next().unwrap_or(name).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ptr_answer_with_a_pointer() {
        // Header: response, 0 questions, 1 answer
        let mut m = vec![0, 0, 0x84, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        // Answer name "1.1.168.192.in-addr.arpa" spelled out
        for l in ["1", "1", "168", "192", "in-addr", "arpa"] {
            m.push(l.len() as u8);
            m.extend_from_slice(l.as_bytes());
        }
        m.push(0);
        m.extend_from_slice(&[0, 12, 0, 1, 0, 0, 0, 120]);
        let rdata: Vec<u8> = [&[5u8][..], b"pixel", &[5], b"local", &[0]].concat();
        m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        m.extend_from_slice(&rdata);
        assert_eq!(answer_ptr(&m).map(|n| short(&n)), Some("pixel".to_string()));
    }

    #[test]
    fn the_question_is_a_unicast_ptr() {
        let q = query("1.0.0.10.in-addr.arpa");
        assert_eq!(&q[q.len() - 4..], &[0, 12, 0x80, 1]);
        assert_eq!(q[12], 1);
    }
}
