//! What a device says about itself in the clear: its name, maker, model and kind from
//! the WPS and Wi-Fi Direct elements of its probes, association requests and beacons,
//! an AP's own name from the vendor elements enterprise APs add, and a fingerprint of
//! how a client builds its requests. All of it is broadcast; nothing here asks.

use crate::dot11::{each_ie, Caps};
use crate::taxonomy;

const WPS: [u8; 4] = [0x00, 0x50, 0xf2, 0x04];
const P2P: [u8; 4] = [0x50, 0x6f, 0x9a, 0x09];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Ident {
    pub name: String,
    pub manufacturer: String,
    pub model: String,
    pub kind: Option<&'static str>,
    /// The platform its vendor elements give away.
    pub platform: Option<&'static str>,
}

impl Ident {
    pub fn is_empty(&self) -> bool {
        self.name.is_empty()
            && self.manufacturer.is_empty()
            && self.model.is_empty()
            && self.kind.is_none()
            && self.platform.is_none()
    }

    /// Fill what is still missing from `other`.
    pub fn merge(&mut self, other: Ident) {
        for (mine, theirs) in [
            (&mut self.name, other.name),
            (&mut self.manufacturer, other.manufacturer),
            (&mut self.model, other.model),
        ] {
            if mine.is_empty() {
                *mine = theirs;
            }
        }
        self.kind = self.kind.or(other.kind);
        self.platform = self.platform.or(other.platform);
    }
}

/// A string field up to its first NUL, trimmed.
fn text(v: &[u8]) -> String {
    let end = v.iter().position(|b| *b == 0).unwrap_or(v.len());
    String::from_utf8_lossy(&v[..end]).trim().to_string()
}

fn be16(v: &[u8], at: usize) -> Option<u16> {
    v.get(at..at + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// The kind a WPS primary device type names, by category and subcategory.
pub fn kind(category: u16, sub: u16) -> Option<&'static str> {
    Some(match (category, sub) {
        (1, 2) => "Server",
        (1, 5 | 8 | 10) => "Laptop",
        (1, 9) => "Tablet",
        (1, _) => "Computer",
        (2, _) => "Input Device",
        (3, 2) => "Scanner",
        (3, _) => "Printer",
        (4, _) => "Camera",
        (5, _) => "Storage",
        (6, 1) => "Access Point",
        (6, _) => "Router",
        (7, 1) => "TV",
        (7, 3) => "Projector",
        (7, 4) => "Monitor",
        (7, _) => "Display",
        (8, 4) => "Set-top Box",
        (8, _) => "Media Player",
        (9, 5) => "Handheld Console",
        (9, _) => "Game Console",
        (10, _) => "Phone",
        (11, 2) => "Speaker",
        (11, 4 | 5) => "Headphones",
        (11, _) => "Audio",
        (12, _) => "Dock",
        _ => return None,
    })
}

/// A primary device type: category, OUI, subcategory.
fn device_type(v: &[u8]) -> Option<&'static str> {
    kind(be16(v, 0)?, be16(v, 6)?)
}

/// WPS attributes, big-endian type and length, past the element's OUI and type.
pub fn wps_attrs(mut v: &[u8], mut visit: impl FnMut(u16, &[u8])) {
    while v.len() >= 4 {
        let kind = u16::from_be_bytes([v[0], v[1]]);
        let len = usize::from(u16::from_be_bytes([v[2], v[3]]));
        let Some(val) = v.get(4..4 + len) else { return };
        visit(kind, val);
        v = &v[4 + len..];
    }
}

/// Name, maker, model and kind from WPS attributes.
pub fn wps(v: &[u8]) -> Ident {
    let mut id = Ident::default();
    wps_attrs(v, |kind, val| match kind {
        0x1011 => id.name = text(val),
        0x1021 => id.manufacturer = text(val),
        0x1023 => id.model = text(val),
        0x1054 => id.kind = device_type(val),
        _ => {}
    });
    id
}

/// Name and kind from Wi-Fi Direct attributes: one byte of type, two of length, little
/// endian. A frame's P2P elements are one list split across them, so they come joined.
pub fn p2p(mut v: &[u8]) -> Ident {
    let mut id = Ident::default();
    while v.len() >= 3 {
        let len = usize::from(u16::from_le_bytes([v[1], v[2]]));
        let Some(val) = v.get(3..3 + len) else { break };
        // Device info: address (6), config methods (2), primary type (8), secondary types
        if v[0] == 13 && val.len() >= 17 {
            id.kind = device_type(&val[8..16]);
            let name_at = 17 + 8 * usize::from(val[16]);
            wps_attrs(val.get(name_at..).unwrap_or(&[]), |kind, name| {
                if kind == 0x1011 {
                    id.name = text(name);
                }
            });
        }
        v = &v[3 + len..];
    }
    id
}

/// What a frame's elements say about the device that sent it.
pub fn from_ies(ies: &[u8]) -> Ident {
    let mut id = Ident::default();
    let mut direct = Vec::new();
    let mut has_wps = false;
    each_ie(ies, |n, v| {
        if n != 221 || v.len() < 4 {
            return;
        }
        if v[..4] == WPS {
            has_wps = true;
            id.merge(wps(&v[4..]));
        } else if v[..4] == P2P {
            direct.extend_from_slice(&v[4..]);
        }
        match v[..3] {
            [0x00, 0x17, 0xf2] => id.platform = Some("Apple"),
            [0x00, 0x00, 0xf0] => id.platform = id.platform.or(Some("Samsung")),
            _ => {}
        }
    });
    // Windows probes for Wi-Fi Direct and WPS together, with neither filled in
    if has_wps && !direct.is_empty() && id.platform.is_none() {
        id.platform = Some("Windows");
    }
    if !direct.is_empty() {
        id.merge(p2p(&direct));
    }
    id
}

/// The name an enterprise AP gives itself, in Cisco's element or a vendor's. Layouts
/// as Wireshark documents them.
pub fn ap_name(id: u8, v: &[u8]) -> Option<String> {
    let name = match id {
        // Cisco CCX: ten bytes, then the name in sixteen
        133 => text(v.get(10..26)?),
        221 if v.len() >= 5 => {
            let rest = &v[4..];
            match ([v[0], v[1], v[2]], v[3]) {
                // Cisco and Meraki, AP name v2
                ([0x00, 0x40, 0x96], 47) => text(rest),
                // Aruba and Huawei: a subtype, a byte, the name
                ([0x00, 0x0b, 0x86], _) if rest[0] == 3 => text(rest.get(2..)?),
                ([0x00, 0xe0, 0xfc], _) if rest[0] == 1 => text(rest.get(2..)?),
                // The name right after the type
                ([0x5c, 0x5b, 0x35], 1)
                | ([0x00, 0x15, 0x6d], 1)
                | ([0x84, 0x80, 0x94], 0)
                | ([0x48, 0xd0, 0x17], 2)
                | ([0x00, 0x13, 0x92], 3)
                | ([0xdc, 0x08, 0x56], 1)
                | ([0x00, 0x11, 0x74], 6)
                | ([0x3c, 0xb9, 0xa6], 1) => text(rest),
                // Aerohive: version, subtype, length, name
                ([0x00, 0x19, 0x77], 33) => {
                    let len = usize::from(*rest.get(2)?);
                    text(rest.get(3..3 + len)?)
                }
                // Extreme: seven bytes, length, name
                ([0x00, 0xa0, 0xf8], 1) => {
                    let len = usize::from(*rest.get(7)?);
                    text(rest.get(8..8 + len)?)
                }
                // Fortinet: a two-byte subtype 10, then type and length pairs, 1 the name
                ([0x00, 0x09, 0x0f], _) if v.get(3..5) == Some(&[10, 0]) => {
                    sub_elements(&v[5..], 1)?
                }
                // MikroTik: two bytes, then type and length pairs; 1 holds the name past ten
                ([0x00, 0x0c, 0x42], _) => {
                    let mut name = None;
                    walk_pairs(rest.get(2..)?, |t, val| {
                        if t == 1 && val.len() == 30 {
                            name = Some(text(&val[10..]));
                        }
                    });
                    name?
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    (!name.is_empty()).then_some(name)
}

fn walk_pairs(mut v: &[u8], mut visit: impl FnMut(u8, &[u8])) {
    while v.len() >= 2 {
        let len = usize::from(v[1]);
        let Some(val) = v.get(2..2 + len) else { return };
        visit(v[0], val);
        v = &v[2 + len..];
    }
}

fn sub_elements(v: &[u8], want: u8) -> Option<String> {
    let mut out = None;
    walk_pairs(v, |t, val| {
        if t == want && out.is_none() {
            out = Some(text(val));
        }
    });
    out
}

/// Makers that put one kind of device on Wi-Fi, see `makers.tsv`.
const MAKERS: &str = include_str!("../makers.tsv");

/// The kind `makers.tsv` gives a registered vendor name: the first prefix it starts
/// with, ignoring case, that ends at a word boundary.
pub fn maker_kind(name: &str) -> Option<&'static str> {
    let lower = name.to_ascii_lowercase();
    MAKERS.lines().filter(|l| !l.starts_with('#')).find_map(|line| {
        let (prefix, kind) = line.split_once('\t')?;
        let p = prefix.to_ascii_lowercase();
        let rest = lower.strip_prefix(&p)?;
        let boundary = rest.chars().next().is_none_or(|c| !c.is_ascii_alphanumeric())
            || !p.ends_with(|c: char| c.is_ascii_alphanumeric());
        boundary.then_some(kind)
    })
}

/// A kind guessed when a client does not say: by a maker that builds one kind of
/// device; by a random MAC, which phones use for each network by default and other
/// devices mostly do not; or by a radio built like smart-home gear, one stream on
/// 2.4 GHz only with no 11ac.
pub fn guess(caps: Option<&Caps>, maker: Option<&str>, random: bool) -> Option<&'static str> {
    if let Some(kind) = maker.and_then(maker_kind) {
        return Some(kind);
    }
    if random {
        return Some("Phone");
    }
    let c = caps?;
    (c.nss == 1 && !c.vht && !c.band_5g && !c.band_6g && !c.eht).then_some("IoT")
}

/// How a request is built, in the form wifi_taxonomy's database keys devices by:
/// `half` ("probe" or "assoc"), then its elements in order, vendor ones by OUI and
/// type, then its HT, VHT, TX power and extended capability fields and the WPS model
/// name. Devices of one model and software build it the same way whatever their MAC.
pub fn signature(half: &str, ies: &[u8]) -> String {
    let mut tags = Vec::new();
    let mut fields: [String; 9] = Default::default();
    let le32 = |v: &[u8], at: usize| {
        v.get(at..at + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    each_ie(ies, |n, v| {
        if n == 221 && v.len() >= 4 {
            tags.push(format!("221({:02x}{:02x}{:02x},{})", v[0], v[1], v[2], v[3]));
            if v[..4] == WPS {
                wps_attrs(&v[4..], |kind, model| {
                    if kind == 0x1023 && fields[8].is_empty() {
                        let safe: String = model
                            .iter()
                            .take(32)
                            .map(|b| if b.is_ascii_alphanumeric() { char::from(*b) } else { '_' })
                            .collect();
                        fields[8] = format!(",wps:{safe}");
                    }
                });
            }
            return;
        }
        tags.push(n.to_string());
        match n {
            45 => {
                if v.len() >= 2 {
                    fields[0] = format!(",htcap:{:04x}", u16::from_le_bytes([v[0], v[1]]));
                }
                if v.len() >= 3 {
                    fields[1] = format!(",htagg:{:02x}", v[2]);
                }
                // Printed as a short in the original, so only the low half survives
                if let Some(mcs) = le32(v, 3) {
                    fields[2] = format!(",htmcs:{:08x}", mcs & 0xffff);
                }
            }
            191 => {
                for (slot, at, name) in [(3, 0, "vhtcap"), (4, 4, "vhtrxmcs"), (5, 8, "vhttxmcs")] {
                    if let Some(x) = le32(v, at) {
                        fields[slot] = format!(",{name}:{x:08x}");
                    }
                }
            }
            33 if v.len() == 2 => {
                fields[6] = format!(",txpow:{:04x}", u16::from_le_bytes([v[0], v[1]]))
            }
            127 => {
                let hex: String = v.iter().map(|b| format!("{b:02x}")).collect();
                fields[7] = format!(",extcap:{hex}");
            }
            _ => {}
        }
    });
    format!("{half}:{}{}", tags.join(","), fields.concat())
}

/// FNV-1a 64, the hash the generated table keys signatures by.
pub fn print_hash(signature: &str) -> u64 {
    signature
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

/// Signature hashes of devices we have identified ourselves, in either half, with
/// their name and kind.
const OURS: &[(u64, &str, &str)] = &[];

/// A device's name and kind by the hash of its association request, else of its probe.
pub fn known(assoc: Option<u64>, probe: Option<u64>) -> Option<(&'static str, &'static str)> {
    let find = |table: &[(u64, u16)], h: u64| {
        table
            .binary_search_by_key(&h, |(k, _)| *k)
            .ok()
            .map(|i| taxonomy::NAMES[usize::from(table[i].1)])
    };
    let ours = |h: u64| OURS.iter().find(|(k, ..)| *k == h).map(|(_, n, k)| (*n, *k));
    let a = assoc.and_then(|h| ours(h).or_else(|| find(taxonomy::ASSOC, h)));
    a.or_else(|| probe.and_then(|h| ours(h).or_else(|| find(taxonomy::PROBE, h))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wps_attr(kind: u16, val: &[u8]) -> Vec<u8> {
        let mut v = kind.to_be_bytes().to_vec();
        v.extend_from_slice(&(val.len() as u16).to_be_bytes());
        v.extend_from_slice(val);
        v
    }

    #[test]
    fn wps_names_and_a_phone() {
        let mut v = wps_attr(0x1011, b"Pixel 8\0");
        v.extend(wps_attr(0x1021, b"Google"));
        v.extend(wps_attr(0x1054, &[0, 10, 0x00, 0x50, 0xf2, 0x04, 0, 5]));
        let id = wps(&v);
        assert_eq!(
            (id.name.as_str(), id.manufacturer.as_str(), id.kind),
            ("Pixel 8", "Google", Some("Phone"))
        );
    }

    #[test]
    fn a_printer_by_wifi_direct_split_over_two_elements() {
        let mut info = vec![0u8; 6];
        info.extend_from_slice(&[0x01, 0x88]);
        info.extend_from_slice(&[0, 3, 0x00, 0x50, 0xf2, 0x04, 0, 1]);
        info.push(0);
        info.extend(wps_attr(0x1011, b"HP OfficeJet"));
        let mut attrs = vec![13];
        attrs.extend_from_slice(&(info.len() as u16).to_le_bytes());
        attrs.extend(info);
        let (a, b) = attrs.split_at(10);
        let mut ies = Vec::new();
        for part in [a, b] {
            ies.extend_from_slice(&[221, (4 + part.len()) as u8]);
            ies.extend_from_slice(&P2P);
            ies.extend_from_slice(part);
        }
        let id = from_ies(&ies);
        assert_eq!((id.name.as_str(), id.kind), ("HP OfficeJet", Some("Printer")));
    }

    #[test]
    fn ap_names_from_cisco_and_aruba() {
        let mut ccx = vec![0u8; 10];
        ccx.extend_from_slice(b"AP-3F-EAST\0\0\0\0\0\0");
        ccx.extend_from_slice(&[4, 0, 0, 0]);
        assert_eq!(ap_name(133, &ccx).as_deref(), Some("AP-3F-EAST"));
        let mut aruba = vec![0x00, 0x0b, 0x86, 0x01, 0x03, 0x00];
        aruba.extend_from_slice(b"lobby-ap");
        assert_eq!(ap_name(221, &aruba).as_deref(), Some("lobby-ap"));
        assert_eq!(ap_name(221, &[0x00, 0x15, 0x6d, 0x02, b'x']), None);
    }

    #[test]
    fn a_signature_in_the_taxonomy_form() {
        // Rates, HT capabilities with two streams, extended capabilities, Apple, WPS model
        let mut ies =
            vec![1, 1, 0x82, 45, 7, 0x6f, 0x01, 0x17, 0xff, 0xff, 0, 0, 127, 2, 0x04, 0x00];
        ies.extend_from_slice(&[221, 5, 0x00, 0x17, 0xf2, 0x0a, 0x00]);
        let mut model = WPS.to_vec();
        model.extend(wps_attr(0x1023, b"Nexus 7"));
        ies.extend_from_slice(&[221, model.len() as u8]);
        ies.extend(model);
        assert_eq!(
            signature("probe", &ies),
            "probe:1,45,127,221(0017f2,10),221(0050f2,4),htcap:016f,htagg:17,htmcs:0000ffff,extcap:0400,wps:Nexus_7"
        );
        assert_eq!(from_ies(&ies).platform, Some("Apple"));
    }

    #[test]
    fn the_table_names_a_device_by_either_half() {
        // A Chromecast's association half; a probe hash never matches an association
        let sig = "assoc:0,1,48,50,127,221(0050f2,2),45,htcap:112c,htagg:03,htmcs:000000ff,extcap:0400080200000040";
        assert_eq!(known(Some(print_hash(sig)), None), Some(("Chromecast", "Media Player")));
        // One Broadcom radio in several devices, told apart only by OUI: no answer
        let shared = "assoc:0,1,48,50,45,221(001018,2),221(00904c,51),221(0050f2,2),htcap:110c,htagg:19,htmcs:000000ff";
        assert_eq!(known(Some(print_hash(shared)), None), None);
        assert_eq!(known(None, Some(print_hash(sig))), None);
        assert!(taxonomy::ASSOC.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn a_single_stream_2ghz_radio_reads_as_iot() {
        let c = Caps { ht: true, nss: 1, band_2g: true, ..Caps::default() };
        assert_eq!(guess(Some(&c), None, false), Some("IoT"));
        let phone = Caps { vht: true, nss: 2, ..c };
        assert_eq!(guess(Some(&phone), Some("Apple, Inc."), false), None);
        assert_eq!(guess(None, Some("Espressif Inc."), false), Some("IoT"));
        assert_eq!(guess(None, Some("Intel Corporate"), false), Some("Laptop"));
        assert_eq!(guess(None, None, true), Some("Phone"));
    }

    #[test]
    fn makers_match_whole_words_and_specific_lines_first() {
        assert_eq!(maker_kind("Sonos, Inc."), Some("Speaker"));
        assert_eq!(maker_kind("SonoSite, Inc."), None);
        assert_eq!(maker_kind("itelio GmbH"), None);
        assert_eq!(maker_kind("WiZ"), Some("IoT"));
        assert_eq!(maker_kind("Wizyoung Tech."), None);
        assert_eq!(maker_kind("LG Electronics (Mobile Communications)"), Some("Phone"));
        assert_eq!(maker_kind("LG Electronics"), Some("TV"));
        assert_eq!(maker_kind("Hewlett Packard Enterprise"), Some("Router"));
        assert_eq!(maker_kind("Apple, Inc."), None);
        // Every line names a kind the icons know
        for line in MAKERS.lines().filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let (_, kind) = line.split_once('\t').expect("prefix, tab, kind");
            assert!(crate::icons::known(kind), "{kind}");
        }
    }
}
