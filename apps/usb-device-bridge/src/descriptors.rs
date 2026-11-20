//! USB descriptors: walked, and rewritten where the PC sees the device at another
//! speed than its own.

/// Each descriptor in a run of them, as its type and its bytes.
pub fn walk(raw: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
    let mut at = 0;
    std::iter::from_fn(move || {
        let n = *raw.get(at)? as usize;
        if n < 2 || at + n > raw.len() {
            return None;
        }
        let d = &raw[at..at + n];
        at += n;
        Some((d[1], d))
    })
}

/// The config descriptors after the device descriptor in sysfs `descriptors`.
pub fn configs(raw: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut at = 18;
    while at + 4 <= raw.len() {
        let total = u16::from_le_bytes([raw[at + 2], raw[at + 3]]) as usize;
        if total < 4 || at + total > raw.len() {
            break;
        }
        out.push(&raw[at..at + total]);
        at += total;
    }
    out
}

/// A SuperSpeed config as a high speed one: no companions, bulk at 512 bytes,
/// bMaxPower from 8mA units to 2mA ones.
pub fn downgrade_config(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for (kind, d) in walk(raw) {
        if kind == 0x30 || kind == 0x31 {
            continue;
        }
        let mut d = d.to_vec();
        if kind == 5 && d.len() >= 6 {
            let maxp = u16::from_le_bytes([d[4], d[5]]) & 0x7FF;
            let maxp = if d[3] & 3 == 2 { 512 } else { maxp.min(1024) };
            d[4..6].copy_from_slice(&maxp.to_le_bytes());
        }
        out.extend_from_slice(&d);
    }
    if out.len() >= 9 {
        let total = out.len() as u16;
        out[2..4].copy_from_slice(&total.to_le_bytes());
        out[8] = (u16::from(out[8]) * 4).min(250) as u8;
    }
    out
}

/// The device descriptor with the gadget's ep0, 64 bytes on every speed the gadget
/// runs, and a SuperSpeed device's bcdUSB brought down to 2.10.
pub fn patch_device(raw: &[u8], superspeed: bool) -> Vec<u8> {
    let mut d = raw.to_vec();
    if d.len() >= 8 {
        d[7] = 64;
    }
    if superspeed && d.len() >= 4 {
        d[2..4].copy_from_slice(&0x0210u16.to_le_bytes());
    }
    d
}

/// One alternate setting of an interface, with its endpoint descriptors.
#[derive(Debug, Clone, PartialEq)]
pub struct Interface {
    pub number: u8,
    pub alt: u8,
    pub class: u8,
    pub protocol: u8,
    pub endpoints: Vec<[u8; 7]>,
}

impl Interface {
    /// Mass storage over bulk-only transport, which states every transfer's length.
    pub fn bulk_only(&self) -> bool {
        self.class == 0x08 && self.protocol == 0x50
    }
}

pub fn interfaces(config: &[u8]) -> Vec<Interface> {
    let mut out: Vec<Interface> = Vec::new();
    for (kind, d) in walk(config) {
        if kind == 4 && d.len() >= 9 {
            out.push(Interface {
                number: d[2],
                alt: d[3],
                class: d[5],
                protocol: d[7],
                endpoints: vec![],
            });
        } else if kind == 5 && d.len() >= 7 {
            if let Some(i) = out.last_mut() {
                i.endpoints.push(d[..7].try_into().unwrap());
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A SuperSpeed disk's config: one interface, two bulk endpoints of 1024 bytes,
    /// each with a companion.
    fn superspeed_disk() -> Vec<u8> {
        let mut c = vec![9, 2, 44, 0, 1, 1, 0, 0x80, 112];
        c.extend([9, 4, 0, 0, 2, 8, 6, 0x50, 0]);
        c.extend([7, 5, 0x81, 2, 0, 4, 0]);
        c.extend([6, 0x30, 15, 0, 0, 0]);
        c.extend([7, 5, 0x02, 2, 0, 4, 0]);
        c.extend([6, 0x30, 15, 0, 0, 0]);
        c
    }

    #[test]
    fn a_superspeed_config_becomes_a_high_speed_one() {
        let d = downgrade_config(&superspeed_disk());
        assert_eq!(d.len(), 32);
        assert_eq!(&d[2..4], &32u16.to_le_bytes(), "wTotalLength follows");
        assert_eq!(d[8], 250, "448mA at 8mA units, capped at 500mA in 2mA units");
        let i = &interfaces(&d)[0];
        assert!(i.bulk_only());
        assert_eq!(i.endpoints, vec![[7, 5, 0x81, 2, 0, 2, 0], [7, 5, 0x02, 2, 0, 2, 0]]);
    }

    #[test]
    fn the_device_descriptor_gets_the_gadgets_ep0() {
        let mouse = [18, 1, 0x10, 0x01, 0, 0, 0, 8, 0x5d, 0x27, 0xa6, 0x0b, 0, 1, 0, 1, 0, 1];
        let d = patch_device(&mouse, false);
        assert_eq!(d[7], 64);
        assert_eq!(&d[2..4], &[0x10, 0x01], "a full speed device keeps its bcdUSB");
        assert_eq!(&patch_device(&mouse, true)[2..4], &[0x10, 0x02]);
    }

    #[test]
    fn configs_are_split_out_of_sysfs_descriptors() {
        let mut raw = vec![0u8; 18];
        raw.extend(superspeed_disk());
        raw.extend([9, 2, 9, 0, 0, 2, 0, 0x80, 50]);
        let found = configs(&raw);
        assert_eq!(found.len(), 2);
        assert_eq!(found[1][5], 2);
    }

    #[test]
    fn a_short_or_broken_run_ends_the_walk() {
        assert_eq!(walk(&[9, 2, 1]).count(), 0);
        assert_eq!(walk(&[0, 0, 7, 5]).count(), 0);
    }
}
