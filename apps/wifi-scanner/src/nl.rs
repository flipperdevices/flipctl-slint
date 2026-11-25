//! nl80211 and rtnetlink, the kernel's own interfaces for what `iw` and `ip` do.
//!
//! Reading scan results and neighbours needs nothing. Setting the radio up needs
//! CAP_NET_ADMIN over its namespace, which the capture process has in the namespace
//! flipctl moves the radio into.

use std::io;
use std::time::Duration;

use flipper_ui::netlink::{
    self, each_attr, Socket, GENERIC, NLMSG_DONE, NLMSG_ERROR, NLMSG_HDR, NLM_F_DUMP, ROUTE,
};

const GENL_ID_CTRL: u16 = 16;
const GENL_HDR: usize = 4;
const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;
const NLM_F_ACK: u16 = 4;

const CMD_SET_WIPHY: u8 = 2;
const CMD_SET_INTERFACE: u8 = 6;
const CMD_GET_SCAN: u8 = 32;
const CMD_GET_SURVEY: u8 = 50;

const ATTR_IFINDEX: u16 = 3;
const ATTR_IFTYPE: u16 = 5;
const ATTR_MNTR_FLAGS: u16 = 23;
const ATTR_WIPHY_FREQ: u16 = 38;
const ATTR_BSS: u16 = 47;
const ATTR_SURVEY_INFO: u16 = 84;
const ATTR_CHANNEL_WIDTH: u16 = 159;
const ATTR_CENTER_FREQ1: u16 = 160;

const IFTYPE_MONITOR: u32 = 6;
const MNTR_FLAG_FCSFAIL: u16 = 1;
const MNTR_FLAG_CONTROL: u16 = 3;
const MNTR_FLAG_OTHER_BSS: u16 = 4;

const BSS_BSSID: u16 = 1;
const BSS_FREQUENCY: u16 = 2;
const BSS_TSF: u16 = 3;
const BSS_BEACON_INTERVAL: u16 = 4;
const BSS_CAPABILITY: u16 = 5;
const BSS_INFORMATION_ELEMENTS: u16 = 6;
const BSS_SIGNAL_MBM: u16 = 7;
const BSS_STATUS: u16 = 9;
const BSS_SEEN_MS_AGO: u16 = 10;

const SURVEY_FREQUENCY: u16 = 1;
const SURVEY_NOISE: u16 = 2;
const SURVEY_IN_USE: u16 = 3;
const SURVEY_TIME: u16 = 4;
const SURVEY_TIME_BUSY: u16 = 5;

const REPLY: Duration = Duration::from_secs(2);

fn u32_of(v: &[u8]) -> Option<u32> {
    v.get(..4).map(|b| u32::from_ne_bytes(b.try_into().expect("4 bytes")))
}

fn u64_of(v: &[u8]) -> Option<u64> {
    v.get(..8).map(|b| u64::from_ne_bytes(b.try_into().expect("8 bytes")))
}

/// Read until DONE, an error or an ACK, handing each message's payload to `visit`.
/// An error is returned as one; an ACK ends a request that has no answer.
fn read_all(socket: &Socket, mut visit: impl FnMut(&[u8])) -> io::Result<()> {
    let mut buf = vec![0u8; 65536];
    loop {
        let got = socket.recv(&mut buf)?;
        let mut at = 0;
        while at + NLMSG_HDR <= got {
            let len = u32::from_ne_bytes(buf[at..at + 4].try_into().expect("4 bytes")) as usize;
            let kind = u16::from_ne_bytes([buf[at + 4], buf[at + 5]]);
            if len < NLMSG_HDR || at + len > got {
                return Ok(());
            }
            let body = &buf[at + NLMSG_HDR..at + len];
            match kind {
                NLMSG_DONE => return Ok(()),
                NLMSG_ERROR => {
                    let errno =
                        body.get(..4).map_or(0, |e| i32::from_ne_bytes(e.try_into().expect("4")));
                    return if errno == 0 {
                        Ok(())
                    } else {
                        Err(io::Error::from_raw_os_error(-errno))
                    };
                }
                _ => visit(body),
            }
            at += netlink::align4(len);
        }
    }
}

/// An nl80211 socket, with the family resolved.
pub struct Nl80211 {
    socket: Socket,
    family: u16,
}

impl Nl80211 {
    pub fn open() -> io::Result<Self> {
        let socket = Socket::open(GENERIC)?;
        socket.set_timeout(REPLY)?;
        let mut name = b"nl80211".to_vec();
        name.push(0);
        // The ACK ends the answer, which is one message and no DONE
        socket.send(&netlink::message(
            GENL_ID_CTRL,
            NLM_F_ACK,
            &[CTRL_CMD_GETFAMILY, 0, 0, 0],
            &[(CTRL_ATTR_FAMILY_NAME, &name)],
        ))?;
        let mut family = 0;
        read_all(&socket, |body| {
            each_attr(&body[GENL_HDR.min(body.len())..], |kind, v| {
                if kind == CTRL_ATTR_FAMILY_ID && v.len() >= 2 {
                    family = u16::from_ne_bytes([v[0], v[1]]);
                }
            });
        })?;
        if family == 0 {
            return Err(io::Error::other("no nl80211"));
        }
        Ok(Self { socket, family })
    }

    fn request(
        &self,
        cmd: u8,
        flags: u16,
        attrs: &[(u16, &[u8])],
        visit: impl FnMut(&[u8]),
    ) -> io::Result<()> {
        self.socket.send(&netlink::message(self.family, flags, &[cmd, 0, 0, 0], attrs))?;
        read_all(&self.socket, visit)
    }

    /// Monitor mode with bad-FCS, control and other-BSS frames passed up. The interface
    /// must be down.
    pub fn set_monitor(&self, ifindex: u32) -> io::Result<()> {
        let mut flags = Vec::new();
        for f in [MNTR_FLAG_FCSFAIL, MNTR_FLAG_CONTROL, MNTR_FLAG_OTHER_BSS] {
            flags.extend_from_slice(&4u16.to_ne_bytes());
            flags.extend_from_slice(&f.to_ne_bytes());
        }
        self.request(
            CMD_SET_INTERFACE,
            NLM_F_ACK,
            &[
                (ATTR_IFINDEX, &ifindex.to_ne_bytes()),
                (ATTR_IFTYPE, &IFTYPE_MONITOR.to_ne_bytes()),
                (ATTR_MNTR_FLAGS | 0x8000, &flags),
            ],
            |_| {},
        )
    }

    /// Tune to `freq` at `width` MHz around `center`.
    pub fn tune(&self, ifindex: u32, freq: u32, width: u16, center: u32) -> io::Result<()> {
        let kind: u32 = match width {
            40 => 2,
            80 => 3,
            160 => 5,
            _ => 1,
        };
        let center = if width <= 20 { freq } else { center };
        self.request(
            CMD_SET_WIPHY,
            NLM_F_ACK,
            &[
                (ATTR_IFINDEX, &ifindex.to_ne_bytes()),
                (ATTR_WIPHY_FREQ, &freq.to_ne_bytes()),
                (ATTR_CHANNEL_WIDTH, &kind.to_ne_bytes()),
                (ATTR_CENTER_FREQ1, &center.to_ne_bytes()),
            ],
            |_| {},
        )
    }

    /// The channel in use: active and busy milliseconds since the radio came up, and
    /// the noise floor.
    pub fn survey(&self, ifindex: u32) -> io::Result<Option<Survey>> {
        let mut found = None;
        self.request(
            CMD_GET_SURVEY,
            NLM_F_DUMP,
            &[(ATTR_IFINDEX, &ifindex.to_ne_bytes())],
            |body| {
                each_attr(&body[GENL_HDR.min(body.len())..], |kind, v| {
                    if kind != ATTR_SURVEY_INFO {
                        return;
                    }
                    let mut s = Survey::default();
                    let mut in_use = false;
                    each_attr(v, |k, x| match k {
                        SURVEY_FREQUENCY => s.freq = u32_of(x).unwrap_or(0),
                        SURVEY_NOISE => s.noise = x.first().map(|n| *n as i8),
                        SURVEY_IN_USE => in_use = true,
                        SURVEY_TIME => s.active_ms = u64_of(x).unwrap_or(0),
                        SURVEY_TIME_BUSY => s.busy_ms = u64_of(x).unwrap_or(0),
                        _ => {}
                    });
                    if in_use {
                        found = Some(s);
                    }
                });
            },
        )?;
        Ok(found)
    }

    /// What the last scan found, as the kernel keeps it.
    pub fn scan_dump(&self, ifindex: u32) -> io::Result<Vec<Bss>> {
        let mut out = Vec::new();
        self.request(
            CMD_GET_SCAN,
            NLM_F_DUMP,
            &[(ATTR_IFINDEX, &ifindex.to_ne_bytes())],
            |body| {
                each_attr(&body[GENL_HDR.min(body.len())..], |kind, v| {
                    if kind != ATTR_BSS {
                        return;
                    }
                    let mut bss = Bss::default();
                    each_attr(v, |k, x| match k {
                        BSS_BSSID if x.len() >= 6 => bss.bssid.copy_from_slice(&x[..6]),
                        BSS_FREQUENCY => bss.freq = u32_of(x).unwrap_or(0),
                        BSS_TSF => bss.tsf = u64_of(x).unwrap_or(0),
                        BSS_BEACON_INTERVAL => {
                            bss.interval =
                                x.get(..2).map_or(0, |b| u16::from_ne_bytes([b[0], b[1]]))
                        }
                        BSS_CAPABILITY => {
                            bss.capability =
                                x.get(..2).map_or(0, |b| u16::from_ne_bytes([b[0], b[1]]))
                        }
                        BSS_INFORMATION_ELEMENTS => bss.ies = x.to_vec(),
                        BSS_SIGNAL_MBM => bss.signal = u32_of(x).map_or(-100, |m| m as i32 / 100),
                        BSS_STATUS => bss.joined = true,
                        BSS_SEEN_MS_AGO => bss.age_ms = u32_of(x).unwrap_or(0),
                        _ => {}
                    });
                    if bss.freq != 0 {
                        out.push(bss);
                    }
                });
            },
        )?;
        Ok(out)
    }
}

/// One survey entry.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Survey {
    pub freq: u32,
    pub noise: Option<i8>,
    pub active_ms: u64,
    pub busy_ms: u64,
}

/// One scan result, as the kernel has it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bss {
    pub bssid: [u8; 6],
    pub freq: u32,
    pub signal: i32,
    pub ies: Vec<u8>,
    /// The Flipper is associated with it.
    pub joined: bool,
    pub age_ms: u32,
    /// Microseconds the AP's clock has run, which is about how long it has been up.
    pub tsf: u64,
    /// Time units (1024 us) between beacons.
    pub interval: u16,
    pub capability: u16,
}

const RTM_NEWLINK: u16 = 16;
const RTM_GETNEIGH: u16 = 30;
const IFF_UP: u32 = 1;
const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;

/// Bring an interface up, as `ip link set up`.
pub fn link_up(ifindex: u32) -> io::Result<()> {
    let socket = Socket::open(ROUTE)?;
    socket.set_timeout(REPLY)?;
    // ifinfomsg: family, pad, type, index, flags, change
    let mut info = vec![0u8; 16];
    info[4..8].copy_from_slice(&(ifindex as i32).to_ne_bytes());
    info[8..12].copy_from_slice(&IFF_UP.to_ne_bytes());
    info[12..16].copy_from_slice(&IFF_UP.to_ne_bytes());
    socket.send(&netlink::message(RTM_NEWLINK, NLM_F_ACK, &info, &[]))?;
    read_all(&socket, |_| {})
}

/// The neighbour table: each MAC this machine has an IPv4 address for.
pub fn neighbours() -> io::Result<Vec<([u8; 6], std::net::Ipv4Addr)>> {
    let socket = Socket::open(ROUTE)?;
    socket.set_timeout(REPLY)?;
    // ndmsg: family AF_INET, pads, ifindex, state, flags, type
    let mut nd = vec![0u8; 12];
    nd[0] = libc::AF_INET as u8;
    socket.send(&netlink::message(RTM_GETNEIGH, NLM_F_DUMP, &nd, &[]))?;
    let mut out = Vec::new();
    read_all(&socket, |body| {
        let Some(attrs) = body.get(12..) else { return };
        let (mut ip, mut mac) = (None, None);
        each_attr(attrs, |k, v| match k {
            NDA_DST if v.len() == 4 => ip = Some(std::net::Ipv4Addr::new(v[0], v[1], v[2], v[3])),
            NDA_LLADDR if v.len() == 6 => mac = Some(<[u8; 6]>::try_from(v).expect("6")),
            _ => {}
        });
        if let (Some(ip), Some(mac)) = (ip, mac) {
            if mac != [0; 6] {
                out.push((mac, ip));
            }
        }
    })?;
    Ok(out)
}

/// The interface index of `name` in this process's network namespace.
pub fn ifindex(name: &str) -> Option<u32> {
    let c = std::ffi::CString::new(name).ok()?;
    let i = unsafe { libc::if_nametoindex(c.as_ptr()) };
    (i != 0).then_some(i)
}
