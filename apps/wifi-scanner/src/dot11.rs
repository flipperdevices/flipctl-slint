//! The 802.11 header and the parts of management frames that are sent in the clear:
//! who a frame is between, its flags, its traffic class, and what a station says it
//! can do when it joins. Data payloads are encrypted and never looked at.

pub type Mac = [u8; 6];

pub const MGMT: u8 = 0;
pub const CTRL: u8 = 1;
pub const DATA: u8 = 2;

pub const ASSOC_REQ: u8 = 0;
pub const ASSOC_RESP: u8 = 1;
pub const REASSOC_REQ: u8 = 2;
pub const REASSOC_RESP: u8 = 3;
pub const PROBE_REQ: u8 = 4;
pub const BEACON: u8 = 8;
pub const DISASSOC: u8 = 10;
pub const DEAUTH: u8 = 12;

/// The header of one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Header {
    pub kind: u8,
    pub sub: u8,
    /// ToDS and FromDS.
    pub ds: u8,
    pub retry: bool,
    /// Power management: the sender is going to sleep after this frame.
    pub pm: bool,
    pub protected: bool,
    pub a1: Mac,
    pub a2: Option<Mac>,
    pub a3: Option<Mac>,
    /// TID of a QoS data frame.
    pub tid: Option<u8>,
    /// Where the body starts, for a management frame.
    pub body: usize,
}

fn mac(b: &[u8], at: usize) -> Option<Mac> {
    b.get(at..at + 6).map(|s| s.try_into().expect("6 bytes"))
}

/// Parse the header of a frame of at least 10 bytes.
pub fn header(b: &[u8]) -> Option<Header> {
    let a1 = mac(b, 4)?;
    let (fc0, fc1) = (b[0], b[1]);
    let kind = fc0 >> 2 & 3;
    let sub = fc0 >> 4;
    let mut h = Header {
        kind,
        sub,
        ds: fc1 & 3,
        retry: fc1 & 0x08 != 0,
        pm: fc1 & 0x10 != 0,
        protected: fc1 & 0x40 != 0,
        a1,
        a2: mac(b, 10),
        a3: mac(b, 16),
        ..Header::default()
    };
    // A control frame's transmitter can carry the bandwidth-signalling bit
    if kind == CTRL {
        if let Some(a2) = h.a2.as_mut() {
            a2[0] &= 0xfe;
        }
        h.a3 = None;
    }
    h.body = match kind {
        MGMT => 24,
        DATA => {
            let mut at = 24 + if h.ds == 3 { 6 } else { 0 };
            if sub & 8 != 0 {
                h.tid = b.get(at).map(|q| q & 0x0f);
                at += 2;
            }
            at
        }
        _ => b.len(),
    };
    Some(h)
}

/// Access category of a TID: 0 background, 1 best effort, 2 video, 3 voice.
pub fn access_category(tid: u8) -> usize {
    match tid & 7 {
        1 | 2 => 0,
        0 | 3 => 1,
        4 | 5 => 2,
        _ => 3,
    }
}

pub fn is_group(m: &Mac) -> bool {
    m[0] & 1 != 0
}

/// A locally administered address, which phones pick at random per network.
pub fn is_local(m: &Mac) -> bool {
    m[0] & 2 != 0
}

pub fn fmt(m: &Mac) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// Visit each element of a run of information elements.
pub fn each_ie(mut b: &[u8], mut visit: impl FnMut(u8, &[u8])) {
    while b.len() >= 2 {
        let (id, len) = (b[0], usize::from(b[1]));
        if 2 + len > b.len() {
            return;
        }
        visit(id, &b[2..2 + len]);
        b = &b[2 + len..];
    }
}

/// What a station says it can do, from its association request.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    pub ht: bool,
    pub vht: bool,
    pub he: bool,
    pub eht: bool,
    /// Spatial streams it can receive.
    pub nss: u8,
    /// The widest channel it supports, MHz.
    pub width: u16,
    pub band_2g: bool,
    pub band_5g: bool,
    pub band_6g: bool,
    /// 802.11k radio measurement.
    pub rrm: bool,
    /// 802.11v BSS transition.
    pub btm: bool,
    /// 802.11r fast transition.
    pub ft: bool,
    /// Management frame protection, from the RSN capabilities.
    pub pmf: bool,
    /// It asks for Target Wake Time, which battery devices do.
    pub twt: bool,
    /// Beacons it may sleep through, from the request's fixed fields.
    pub listen: u16,
    /// OUIs of its vendor elements, generic ones left out.
    pub vendors: Vec<[u8; 3]>,
}

impl Caps {
    pub fn generation(&self) -> &'static str {
        match (self.eht, self.he, self.vht, self.ht) {
            (true, ..) => "Wi-Fi 7",
            (_, true, ..) if self.band_6g => "Wi-Fi 6E",
            (_, true, ..) => "Wi-Fi 6",
            (_, _, true, _) => "Wi-Fi 5",
            (.., true) => "Wi-Fi 4",
            _ => "11a/b/g",
        }
    }
}

/// Microsoft (WMM, WPS) and the Wi-Fi Alliance (P2P, MBO): everyone sends these.
const GENERIC_OUIS: [[u8; 3]; 2] = [[0x00, 0x50, 0xf2], [0x50, 0x6f, 0x9a]];

/// Streams in a 16-bit VHT/HE MCS map: two bits each, 3 is "not supported".
fn map_streams(map: u16) -> u8 {
    (0..8).filter(|i| map >> (2 * i) & 3 != 3).count() as u8
}

/// The capabilities in an association or reassociation request's body.
pub fn caps(body: &[u8], reassoc: bool) -> Caps {
    let fixed = if reassoc { 10 } else { 4 };
    let mut c = Caps { width: 20, ..Caps::default() };
    if let Some(l) = body.get(2..4) {
        c.listen = u16::from_le_bytes([l[0], l[1]]);
    }
    let Some(ies) = body.get(fixed..) else { return c };
    each_ie(ies, |id, v| match id {
        45 if v.len() >= 7 => {
            c.ht = true;
            if u16::from_le_bytes([v[0], v[1]]) & 2 != 0 {
                c.width = c.width.max(40);
            }
            c.nss = c.nss.max(v[3..7].iter().filter(|b| **b != 0).count() as u8);
        }
        191 if v.len() >= 6 => {
            c.vht = true;
            c.width = c.width.max(80);
            if u32::from_le_bytes([v[0], v[1], v[2], v[3]]) >> 2 & 3 != 0 {
                c.width = 160;
            }
            c.nss = c.nss.max(map_streams(u16::from_le_bytes([v[4], v[5]])));
        }
        48 if v.len() >= 2 => {
            // RSN: version, group cipher, pairwise and AKM lists, then capabilities
            let mut at = 6;
            for _ in 0..2 {
                let n =
                    v.get(at..at + 2).map_or(0, |n| usize::from(u16::from_le_bytes([n[0], n[1]])));
                at += 2 + 4 * n;
            }
            if let Some(cap) = v.get(at..at + 2) {
                c.pmf = cap[0] & 0x80 != 0 || cap[0] & 0x40 != 0;
            }
        }
        54 => c.ft = true,
        59 => {
            for class in v {
                match class {
                    81..=84 => c.band_2g = true,
                    115..=130 => c.band_5g = true,
                    131..=137 => c.band_6g = true,
                    _ => {}
                }
            }
        }
        70 => c.rrm = true,
        127 if v.len() >= 3 => c.btm = v[2] & 0x08 != 0,
        221 if v.len() >= 3 => {
            let oui = [v[0], v[1], v[2]];
            if !GENERIC_OUIS.contains(&oui) && !c.vendors.contains(&oui) {
                c.vendors.push(oui);
            }
        }
        255 if !v.is_empty() => match v[0] {
            // HE capabilities: ext id, 6 MAC, 11 PHY, then the 80 MHz MCS map
            35 if v.len() >= 20 => {
                c.he = true;
                c.twt = v[1] & 0x02 != 0;
                let phy0 = v[7];
                if phy0 & 0x0c != 0 {
                    c.width = c.width.max(if phy0 & 0x08 != 0 { 160 } else { 80 });
                }
                c.nss = c.nss.max(map_streams(u16::from_le_bytes([v[18], v[19]])));
            }
            108 => c.eht = true,
            _ => {}
        },
        _ => {}
    });
    if c.nss == 0 {
        c.nss = 1;
    }
    c
}

/// The AP a reassociating station is coming from.
pub fn reassoc_from(body: &[u8]) -> Option<Mac> {
    mac(body, 4)
}

/// The status of an association response, 0 being success.
pub fn assoc_status(body: &[u8]) -> Option<u16> {
    body.get(2..4).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

/// The reason of a deauthentication or disassociation, readable unless protected.
pub fn reason(body: &[u8]) -> Option<u16> {
    body.get(0..2).map(|s| u16::from_le_bytes([s[0], s[1]]))
}

#[cfg(test)]
mod tests {
    use super::*;

    const AP: Mac = [0xcc, 0x2d, 0xe0, 0xc2, 0x9e, 0xa0];
    const STA: Mac = [0xde, 0xad, 0x4b, 0x52, 0x55, 0x9a];
    const PEER: Mac = [0x88, 0xd8, 0x2e, 0x56, 0x91, 0x13];

    pub fn qos_data_to_ap(tid: u8) -> Vec<u8> {
        let mut b = vec![0x88, 0x01 | 0x08 | 0x10, 0, 0];
        b.extend_from_slice(&AP);
        b.extend_from_slice(&STA);
        b.extend_from_slice(&PEER);
        b.extend_from_slice(&[0, 0, tid, 0]);
        b.extend_from_slice(&[0u8; 40]);
        b
    }

    #[test]
    fn a_qos_data_frame_from_a_station() {
        let h = header(&qos_data_to_ap(6)).unwrap();
        assert_eq!((h.kind, h.ds), (DATA, 1));
        assert!(h.retry && h.pm);
        assert_eq!(h.a1, AP);
        assert_eq!(h.a2, Some(STA));
        assert_eq!(h.a3, Some(PEER));
        assert_eq!(h.tid, Some(6));
        assert_eq!(access_category(6), 3);
        assert_eq!(h.body, 26);
    }

    #[test]
    fn a_control_frame_drops_the_signalling_bit() {
        let mut b = vec![0x84, 0, 0, 0];
        b.extend_from_slice(&STA);
        let mut ta = AP;
        ta[0] |= 1;
        b.extend_from_slice(&ta);
        let h = header(&b).unwrap();
        assert_eq!(h.kind, CTRL);
        assert_eq!(h.a2, Some(AP));
        assert_eq!(h.a3, None);
    }

    #[test]
    fn capabilities_of_a_two_stream_wifi6_phone() {
        let mut body = vec![0x31, 0x04, 0x0a, 0x00];
        // HT: 40 MHz, two streams
        body.extend_from_slice(&[45, 26, 0x02, 0x00, 0x00, 0xff, 0xff, 0, 0]);
        body.extend_from_slice(&[0u8; 19]);
        // VHT: 80 MHz, MCS map two streams
        body.extend_from_slice(&[191, 12, 0, 0, 0, 0, 0xfa, 0xff, 0, 0, 0xfa, 0xff, 0, 0]);
        // Extended capabilities: BSS transition
        body.extend_from_slice(&[127, 3, 0, 0, 0x08]);
        // RM enabled, operating classes 81 and 128
        body.extend_from_slice(&[70, 5, 0, 0, 0, 0, 0, 59, 2, 81, 128]);
        // Vendor: WMM (generic), and one of a chip maker's
        body.extend_from_slice(&[221, 7, 0x00, 0x50, 0xf2, 2, 0, 1, 0]);
        body.extend_from_slice(&[221, 4, 0x00, 0x10, 0x18, 2]);
        // HE: ext 35, MAC 6, PHY 11 (80 MHz in 5 GHz), MCS map two streams
        let mut he = vec![35];
        he.extend_from_slice(&[0u8; 6]);
        he.extend_from_slice(&[0x04, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        he.extend_from_slice(&[0xfa, 0xff, 0xfa, 0xff]);
        body.push(255);
        body.push(he.len() as u8);
        body.extend_from_slice(&he);
        let c = caps(&body, false);
        assert!(c.ht && c.vht && c.he && !c.eht);
        assert_eq!((c.nss, c.width), (2, 80));
        assert!(c.btm && c.rrm && !c.ft);
        assert!(c.band_2g && c.band_5g && !c.band_6g);
        assert_eq!(c.vendors, vec![[0x00, 0x10, 0x18]]);
        assert_eq!(c.generation(), "Wi-Fi 6");
    }

    #[test]
    fn a_truncated_element_ends_the_walk() {
        let mut seen = Vec::new();
        each_ie(&[0, 2, b'a', b'b', 1, 9, 1], |id, v| seen.push((id, v.len())));
        assert_eq!(seen, vec![(0, 2)]);
    }
}
