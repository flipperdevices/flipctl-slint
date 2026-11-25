//! The networks around, from the scan results the kernel keeps: NetworkManager scans
//! on its own, and a rescan is one D-Bus call through nmcli. Reading the results
//! needs no privilege.

use crate::dot11::{each_ie, Mac};
use crate::ident::{self, Ident};
use crate::nl::Bss;

/// How a network protects itself, from its RSN and WPA elements and its privacy bit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Security {
    #[default]
    Open,
    /// Opportunistic wireless encryption: no password, but encrypted.
    Owe,
    Wep,
    Wpa,
    Wpa2,
    /// WPA2 and WPA3 both accepted.
    Wpa2Wpa3,
    Wpa3,
}

impl Security {
    pub fn name(self) -> &'static str {
        match self {
            Security::Open => "OPEN",
            Security::Owe => "OWE",
            Security::Wep => "WEP",
            Security::Wpa => "WPA",
            Security::Wpa2 => "WPA2",
            Security::Wpa2Wpa3 => "WPA2/3",
            Security::Wpa3 => "WPA3",
        }
    }
}

/// What the AP's WPS element says.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Wps {
    pub configured: bool,
    /// Too many wrong PINs: the AP refuses more for now.
    pub locked: bool,
    /// The push button is being pressed right now.
    pub push_button: bool,
}

/// A network, by its BSSID: one AP radio.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Network {
    pub bssid: Mac,
    pub ssid: String,
    /// The primary channel's centre, MHz.
    pub freq: u32,
    pub width: u16,
    /// The whole channel's centre, MHz.
    pub center: u32,
    pub signal: i32,
    /// The Flipper is joined to it, so its LAN is the Flipper's.
    pub joined: bool,
    pub security: Security,
    /// Key management, "PSK", "SAE", "EAP" and so on, as many as it offers.
    pub akm: Vec<&'static str>,
    /// Pairwise ciphers, "AES" or "TKIP" and so on.
    pub ciphers: Vec<&'static str>,
    pub group_cipher: Option<&'static str>,
    /// Management frame protection: offered, and required.
    pub pmf: (bool, bool),
    pub wps: Option<Wps>,
    /// The newest 802.11 amendment it speaks: "802.11ax" and so on.
    pub protocol: &'static str,
    pub streams: u8,
    /// The fastest PHY rate its streams and width allow, Mbit/s.
    pub max_rate: u32,
    /// Stations associated and channel load, 0..255, from its BSS Load element.
    pub stations: Option<u16>,
    pub load: Option<u8>,
    pub country: Option<String>,
    pub dtim: Option<u8>,
    pub beacon_ms: u32,
    /// How long the AP's clock has been running, seconds.
    pub uptime: u64,
    /// 802.11k, 802.11v, 802.11r.
    pub rrm: bool,
    pub btm: bool,
    pub ft: bool,
    /// Wi-Fi Agile Multiband.
    pub mbo: bool,
    /// Its SSID is hidden: empty or zeros in the beacon.
    pub hidden: bool,
    /// OUIs of its vendor elements, generic ones left out.
    pub vendors: Vec<[u8; 3]>,
    /// What its WPS and Wi-Fi Direct elements say it is.
    pub device: Ident,
    /// The name an enterprise AP gives itself.
    pub ap_name: Option<String>,
    /// Other radios of the same AP, as operating class and channel, from its Reduced
    /// Neighbor Report; and how many other APs that report lists.
    pub colocated: Vec<(u8, u8)>,
    pub neighbours: usize,
    /// Interworking: access network type, internet, venue group; and Hotspot 2.0.
    pub access: Option<u8>,
    pub internet: Option<bool>,
    pub venue: Option<u8>,
    pub passpoint: bool,
    /// EasyMesh: the Multi-AP element's role bits.
    pub multi_ap: Option<u8>,
    pub age_ms: u32,
}

impl Network {
    pub fn channel(&self) -> u32 {
        channel_of(self.freq)
    }
}

pub fn channel_of(freq: u32) -> u32 {
    match freq {
        2484 => 14,
        f if f < 3000 => (f.saturating_sub(2407)) / 5,
        f if f >= 5950 => (f - 5950) / 5,
        f => f.saturating_sub(5000) / 5,
    }
}

/// The centre frequency of channel `chan` in the band `freq` is in.
fn freq_of(chan: u32, freq: u32) -> u32 {
    if freq < 3000 {
        2407 + 5 * chan
    } else if freq >= 5950 {
        5950 + 5 * chan
    } else {
        5000 + 5 * chan
    }
}

fn cipher(suite: &[u8]) -> Option<&'static str> {
    if suite.len() < 4 || !matches!(suite[..3], [0x00, 0x0f, 0xac] | [0x00, 0x50, 0xf2]) {
        return None;
    }
    Some(match suite[3] {
        1 => "WEP40",
        2 => "TKIP",
        4 => "AES",
        5 => "WEP104",
        8 => "GCMP",
        9 => "GCMP-256",
        10 => "CCMP-256",
        _ => return None,
    })
}

fn akm(suite: &[u8]) -> Option<&'static str> {
    if suite.len() < 4 || !matches!(suite[..3], [0x00, 0x0f, 0xac] | [0x00, 0x50, 0xf2]) {
        return None;
    }
    Some(match suite[3] {
        1 | 5 | 11 | 12 | 13 => "EAP",
        2 | 6 => "PSK",
        3 => "FT-EAP",
        4 => "FT-PSK",
        8 | 24 => "SAE",
        9 | 25 => "FT-SAE",
        18 => "OWE",
        _ => return None,
    })
}

/// The suite lists of an RSN element, or of the old WPA vendor element past its OUI.
fn suites(n: &mut Network, v: &[u8], rsn: bool) {
    let u16_at =
        |at: usize| v.get(at..at + 2).map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])));
    n.group_cipher = v.get(2..6).and_then(cipher);
    let mut at = 6;
    for list in 0..2 {
        let count = u16_at(at).unwrap_or(0);
        at += 2;
        for i in 0..count {
            let s = v.get(at + 4 * i..at + 4 * i + 4).unwrap_or(&[]);
            match list {
                0 => n.ciphers.extend(cipher(s).filter(|c| !n.ciphers.contains(c))),
                _ => n.akm.extend(akm(s).filter(|a| !n.akm.contains(a))),
            }
        }
        at += 4 * count;
    }
    if rsn {
        if let Some(cap) = v.get(at..at + 2) {
            n.pmf = (cap[0] & 0x80 != 0, cap[0] & 0x40 != 0);
        }
    }
}

/// The WPS element's state; its names are the device's, see `ident`.
fn wps(v: &[u8]) -> Wps {
    let mut w = Wps::default();
    ident::wps_attrs(v, |kind, val| match kind {
        0x1044 => w.configured = val.first() == Some(&2),
        0x1057 => w.locked = val.first() == Some(&1),
        0x1012 => w.push_button = val.get(..2) == Some(&[0, 4]),
        _ => {}
    });
    w
}

/// Co-located radios and the count of other APs in a Reduced Neighbor Report.
fn rnr(n: &mut Network, mut v: &[u8]) {
    while v.len() >= 4 {
        let hdr = u16::from_le_bytes([v[0], v[1]]);
        let count = usize::from(hdr >> 4 & 0xf) + 1;
        let len = usize::from(hdr >> 8);
        let (class, chan) = (v[2], v[3]);
        let Some(infos) = v.get(4..4 + count * len) else { return };
        // Where the BSS parameters sit, by the length of a TBTT information field
        let params = match len {
            2 => Some(1),
            6 => Some(5),
            8 => Some(7),
            12.. => Some(11),
            _ => None,
        };
        for info in infos.chunks_exact(len.max(1)) {
            let colocated = params.and_then(|p| info.get(p)).is_some_and(|b| b & 0x40 != 0);
            if colocated {
                if !n.colocated.contains(&(class, chan)) {
                    n.colocated.push((class, chan));
                }
            } else {
                n.neighbours += 1;
            }
        }
        v = &v[4 + count * len..];
    }
}

/// Spatial streams in an HT MCS set or a VHT/HE MCS map.
fn ht_streams(mcs: &[u8]) -> u8 {
    mcs.iter().take(4).filter(|b| **b != 0).count() as u8
}

fn map_streams(map: u16) -> u8 {
    (0..8).filter(|i| map >> (2 * i) & 3 != 3).count() as u8
}

/// The top PHY rate, Mbit/s: the highest MCS of a protocol at this width and streams,
/// with the short guard interval.
fn max_rate(protocol: &str, width: u16, streams: u8) -> u32 {
    let per_stream: f32 = match (protocol, width) {
        ("802.11be" | "802.11ax", 20) => 143.4,
        ("802.11be" | "802.11ax", 40) => 286.8,
        ("802.11be" | "802.11ax", 80) => 600.5,
        ("802.11be" | "802.11ax", _) => 1201.0,
        ("802.11ac", 20) => 86.7,
        ("802.11ac", 40) => 200.0,
        ("802.11ac", 80) => 433.3,
        ("802.11ac", _) => 866.7,
        ("802.11n", 40) => 150.0,
        ("802.11n", _) => 72.2,
        ("802.11b", _) => return 11,
        _ => return 54,
    };
    (per_stream * f32::from(streams.max(1))).round() as u32
}

/// Name, width, centre, security and the rest from a BSS's elements.
pub fn network(bss: &Bss) -> Network {
    let freq = bss.freq;
    let mut n = Network {
        bssid: bss.bssid,
        freq,
        width: 20,
        center: freq,
        signal: bss.signal,
        joined: bss.joined,
        beacon_ms: u32::from(bss.interval) * 1024 / 1000,
        uptime: bss.tsf / 1_000_000,
        age_ms: bss.age_ms,
        ..Network::default()
    };
    let (mut rsn, mut wpa1, mut ofdm, mut eht) = (false, false, false, false);
    let (mut ht_on, mut vht_on, mut he_on) = (false, false, false);
    let mut ht: Option<(u8, bool)> = None;
    let mut vht: Option<(u8, u8, u8)> = None;
    let mut he6: Option<(u8, u8, u8)> = None;
    each_ie(&bss.ies, |id, v| match id {
        0 => {
            n.hidden = v.iter().all(|b| *b == 0);
            n.ssid = if n.hidden { String::new() } else { String::from_utf8_lossy(v).into_owned() };
        }
        1 | 50 => ofdm |= v.iter().any(|r| !matches!(r & 0x7f, 2 | 4 | 11 | 22)),
        5 if v.len() >= 2 => n.dtim = Some(v[1]),
        7 if v.len() >= 2 => {
            let cc = String::from_utf8_lossy(&v[..2]).into_owned();
            n.country = cc.chars().all(|c| c.is_ascii_alphabetic()).then_some(cc);
        }
        11 if v.len() >= 3 => {
            n.stations = Some(u16::from_le_bytes([v[0], v[1]]));
            n.load = Some(v[2]);
        }
        45 if v.len() >= 7 => {
            ht_on = true;
            n.streams = n.streams.max(ht_streams(&v[3..7]));
        }
        48 => {
            rsn = true;
            suites(&mut n, v, true);
        }
        54 => n.ft = true,
        70 => n.rrm = true,
        107 if !v.is_empty() => {
            n.access = Some(v[0] & 0x0f);
            n.internet = Some(v[0] & 0x10 != 0);
            n.venue = v.get(1).copied().filter(|_| v.len() == 3 || v.len() == 9);
        }
        133 => n.ap_name = n.ap_name.take().or(ident::ap_name(id, v)),
        201 => rnr(&mut n, v),
        127 if v.len() >= 3 => n.btm = v[2] & 0x08 != 0,
        191 if v.len() >= 6 => {
            vht_on = true;
            n.streams = n.streams.max(map_streams(u16::from_le_bytes([v[4], v[5]])));
        }
        221 if v.len() >= 4 => match (&v[..3], v[3]) {
            ([0x00, 0x50, 0xf2], 1) => {
                wpa1 = true;
                if !rsn {
                    suites(&mut n, &v[4..], false);
                }
            }
            ([0x00, 0x50, 0xf2], 4) => n.wps = Some(wps(&v[4..])),
            ([0x50, 0x6f, 0x9a], 0x16) => n.mbo = true,
            ([0x50, 0x6f, 0x9a], 0x10) => n.passpoint = true,
            ([0x50, 0x6f, 0x9a], 0x1b) => {
                // Multi-AP extension: attribute 6, one byte of role bits
                let mut at = 4;
                while at + 2 <= v.len() {
                    if v[at] == 6 && at + 2 < v.len() {
                        n.multi_ap = Some(v[at + 2]);
                    }
                    at += 2 + usize::from(v[at + 1]);
                }
            }
            ([0x00, 0x50, 0xf2], _) | ([0x50, 0x6f, 0x9a], _) => {}
            (oui, _) => {
                if n.ap_name.is_none() {
                    n.ap_name = ident::ap_name(id, v);
                }
                let oui = [oui[0], oui[1], oui[2]];
                if !n.vendors.contains(&oui) {
                    n.vendors.push(oui);
                }
            }
        },
        61 if v.len() >= 2 => ht = Some((v[1] & 3, v[1] & 4 != 0)),
        192 if v.len() >= 3 => vht = Some((v[0], v[1], v[2])),
        255 if v.len() >= 20 && v[0] == 35 => {
            he_on = true;
            n.streams = n.streams.max(map_streams(u16::from_le_bytes([v[18], v[19]])));
        }
        255 if !v.is_empty() && v[0] == 108 => eht = true,
        255 if v.len() >= 7 && v[0] == 36 => {
            // HE operation: params (3), BSS colour (1), basic MCS (2), then optional parts
            let params = u32::from(v[1]) | u32::from(v[2]) << 8 | u32::from(v[3]) << 16;
            let mut at = 7;
            if params & 1 << 14 != 0 {
                at += 3;
            }
            if params & 1 << 15 != 0 {
                at += 1;
            }
            if params & 1 << 17 != 0 && v.len() >= at + 4 {
                he6 = Some((v[at + 1] & 3, v[at + 2], v[at + 3]));
            }
        }
        _ => {}
    });
    n.security = if rsn {
        let sae = n.akm.iter().any(|a| a.ends_with("SAE"));
        let older = n.akm.iter().any(|a| a.ends_with("PSK") || a.ends_with("EAP"));
        match (sae, older) {
            _ if n.akm == ["OWE"] => Security::Owe,
            (true, true) => Security::Wpa2Wpa3,
            (true, false) => Security::Wpa3,
            _ => Security::Wpa2,
        }
    } else if wpa1 {
        Security::Wpa
    } else if bss.capability & 0x10 != 0 {
        Security::Wep
    } else {
        Security::Open
    };
    n.protocol = match (eht, he_on, vht_on, ht_on) {
        (true, ..) => "802.11be",
        (_, true, ..) => "802.11ax",
        (_, _, true, _) => "802.11ac",
        (.., true) => "802.11n",
        _ if freq > 3000 => "802.11a",
        _ if ofdm => "802.11g",
        _ => "802.11b",
    };
    n.device = ident::from_ies(&bss.ies);
    // A MikroTik's radio name is its MAC until someone names it
    let own = n.bssid.iter().map(|b| format!("{b:02X}")).collect::<String>();
    n.ap_name = n.ap_name.take().filter(|name| !name.eq_ignore_ascii_case(&own));
    n.streams = n.streams.max(1);
    widths(&mut n, ht, vht, he6);
    n.max_rate = max_rate(n.protocol, n.width, n.streams);
    n
}

/// Width and centre from the HT, VHT and 6 GHz HE operation elements.
fn widths(
    n: &mut Network,
    ht: Option<(u8, bool)>,
    vht: Option<(u8, u8, u8)>,
    he6: Option<(u8, u8, u8)>,
) {
    let freq = n.freq;
    if freq >= 5950 {
        if let Some((w, c0, c1)) = he6 {
            (n.width, n.center) = match w {
                1 => (40, freq_of(u32::from(c0), freq)),
                2 => (80, freq_of(u32::from(c0), freq)),
                3 if c1 != 0 => (160, freq_of(u32::from(c1), freq)),
                _ => (20, freq),
            };
        }
        return;
    }
    if let Some((offset, any)) = ht {
        if any && (offset == 1 || offset == 3) {
            n.width = 40;
            n.center = if offset == 1 { freq + 10 } else { freq - 10 };
        }
    }
    if let Some((w, s0, s1)) = vht {
        if freq > 3000 && w >= 1 {
            let (s0, s1) = (u32::from(s0), u32::from(s1));
            if w == 2 {
                (n.width, n.center) = (160, freq_of(s0, freq));
            } else if s1 != 0 && s1.abs_diff(s0) == 8 {
                (n.width, n.center) = (160, freq_of(s1, freq));
            } else {
                (n.width, n.center) = (80, freq_of(s0, freq));
            }
        }
    }
}

/// Every network, strongest first, the one an SSID is joined on ahead of the rest.
pub fn networks(found: &[Bss]) -> Vec<Network> {
    let mut nets: Vec<Network> = found.iter().map(network).collect();
    nets.sort_by_key(|n| (!n.joined, -n.signal));
    nets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bss(freq: u32, ies: &[u8]) -> Bss {
        Bss { bssid: [1, 2, 3, 4, 5, 6], freq, signal: -50, ies: ies.to_vec(), ..Bss::default() }
    }

    #[test]
    fn an_80_mhz_vht_network() {
        // SSID, HT operation (primary 104, above, any width), VHT (80, seg0 106)
        let mut ies = vec![0, 8];
        ies.extend_from_slice(b"Valhalla");
        ies.extend_from_slice(&[61, 22, 104, 0x05]);
        ies.extend_from_slice(&[0u8; 20]);
        ies.extend_from_slice(&[192, 5, 1, 106, 0, 0, 0]);
        let n = network(&bss(5520, &ies));
        assert_eq!(n.ssid, "Valhalla");
        assert_eq!((n.channel(), n.width, n.center), (104, 80, 5530));
    }

    #[test]
    fn a_40_mhz_2ghz_network_below() {
        let ies = [61, 22, 6, 0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        let n = network(&bss(2437, &ies));
        assert_eq!((n.channel(), n.width, n.center), (6, 40, 2427));
    }

    #[test]
    fn a_160_mhz_network_by_segments() {
        let ies = [192, 5, 1, 42, 50, 0, 0];
        let n = network(&bss(5180, &ies));
        assert_eq!((n.width, n.center), (160, 5250));
    }

    #[test]
    fn a_6ghz_network_from_he_operation() {
        // HE operation with the 6 GHz part: params bit 17, primary 37, 80 MHz, ccfs0 39
        let ies = [255, 12, 36, 0, 0, 0x02, 0, 0, 0, 37, 2, 39, 0, 0];
        let n = network(&bss(6135, &ies));
        assert_eq!((n.channel(), n.width, n.center), (37, 80, 6145));
    }

    #[test]
    fn the_joined_network_comes_first() {
        let mut a = bss(2412, &[]);
        a.signal = -80;
        a.joined = true;
        let b = bss(2437, &[]);
        let nets = networks(&[b, a]);
        assert!(nets[0].joined);
    }

    #[test]
    fn an_rnr_with_a_colocated_6ghz_radio_and_a_neighbour() {
        // Two entries of 13-byte TBTT info: class 131 channel 37 co-located, class 115
        // channel 36 not
        let mut ies = vec![201, 2 * (4 + 13)];
        for (class, chan, params) in [(131u8, 37u8, 0x40u8), (115, 36, 0)] {
            ies.extend_from_slice(&[0x00, 13, class, chan, 0]);
            ies.extend_from_slice(&[0; 6]);
            ies.extend_from_slice(&[0; 4]);
            ies.extend_from_slice(&[params, 0]);
        }
        let n = network(&bss(5180, &ies));
        assert_eq!((n.colocated.clone(), n.neighbours), (vec![(131, 37)], 1));
    }

    #[test]
    fn a_mikrotik_named_by_its_mac_has_no_ap_name() {
        let mut v = vec![221, 0, 0x00, 0x0c, 0x42, 0x00, 0x00, 0x00, 1, 30];
        v.extend_from_slice(&[0; 10]);
        let mut name = b"010203040506".to_vec();
        name.resize(20, 0);
        v.extend_from_slice(&name);
        v[1] = (v.len() - 2) as u8;
        assert_eq!(network(&bss(2412, &v)).ap_name, None);
        v[20..32].copy_from_slice(b"Hall-AP-2\0\0\0");
        assert_eq!(network(&bss(2412, &v)).ap_name.as_deref(), Some("Hall-AP-2"));
    }
}
