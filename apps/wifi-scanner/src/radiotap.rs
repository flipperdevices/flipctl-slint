//! The radiotap header the driver puts in front of every captured frame: how it was
//! received (signal, rate, standard, width) rather than what it says.

/// The 802.11 generation a frame was sent with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Standard {
    #[default]
    Legacy,
    Ht,
    Vht,
    He,
}

impl Standard {
    pub fn name(self) -> &'static str {
        match self {
            Standard::Legacy => "11a/g",
            Standard::Ht => "11n",
            Standard::Vht => "11ac",
            Standard::He => "11ax",
        }
    }
}

/// What radiotap said about one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Phy {
    /// Where the 802.11 frame starts.
    pub len: usize,
    pub flags: u8,
    pub signal: Option<i8>,
    /// Frames of one A-MPDU share it and one preamble.
    pub ampdu: Option<u32>,
    pub standard: Standard,
    pub mcs: u8,
    pub nss: u8,
    /// MHz; below 20 for an HE resource unit.
    pub width: u16,
    /// Guard interval in nanoseconds.
    pub gi: u16,
    /// Mbit/s, 0 when unknown.
    pub rate: f32,
    /// Microseconds of preamble ahead of the data.
    pub preamble: f32,
}

pub const FLAG_FCS: u8 = 0x10;
pub const FLAG_BAD_FCS: u8 = 0x40;

/// Alignment and size of fields 0..27.
const FIELDS: [(usize, usize); 28] = [
    (8, 8),
    (1, 1),
    (1, 1),
    (2, 4),
    (2, 2),
    (1, 1),
    (1, 1),
    (2, 2),
    (2, 2),
    (2, 2),
    (1, 1),
    (1, 1),
    (1, 1),
    (1, 1),
    (2, 2),
    (2, 2),
    (1, 1),
    (1, 1),
    (4, 8),
    (1, 3),
    (4, 8),
    (2, 12),
    (8, 12),
    (2, 12),
    (2, 12),
    (2, 6),
    (1, 1),
    (2, 4),
];

/// Bits per subcarrier and coding rate, by MCS 0..11.
const MOD: [(f32, f32); 12] = [
    (1.0, 0.5),
    (2.0, 0.5),
    (2.0, 0.75),
    (4.0, 0.5),
    (4.0, 0.75),
    (6.0, 2.0 / 3.0),
    (6.0, 0.75),
    (6.0, 5.0 / 6.0),
    (8.0, 0.75),
    (8.0, 5.0 / 6.0),
    (10.0, 0.75),
    (10.0, 5.0 / 6.0),
];

/// Data subcarriers of an HT/VHT channel by width.
fn ht_sd(width: u16) -> f32 {
    match width {
        40 => 108.0,
        80 => 234.0,
        160 => 468.0,
        _ => 52.0,
    }
}

/// HE data subcarriers by the radiotap bandwidth/RU field: 20..160 MHz, then RUs.
const HE_SD: [f32; 11] =
    [234.0, 468.0, 980.0, 1960.0, 24.0, 48.0, 102.0, 234.0, 468.0, 980.0, 1960.0];
const HE_WIDTH: [u16; 11] = [20, 40, 80, 160, 2, 4, 8, 20, 40, 80, 160];

fn ofdm(sd: f32, mcs: u8, nss: u8, symbol_us: f32) -> f32 {
    match MOD.get(usize::from(mcs)) {
        Some((bits, r)) if nss > 0 => sd * bits * r * f32::from(nss) / symbol_us,
        _ => 0.0,
    }
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Parse the header at the start of `b`, or `None` when it is not one.
pub fn parse(b: &[u8]) -> Option<Phy> {
    if b.len() < 8 || b[0] != 0 {
        return None;
    }
    let len = usize::from(u16_at(b, 2));
    if len < 8 || len > b.len() {
        return None;
    }
    let present = u32_at(b, 4);
    let mut at = 8;
    let mut word = present;
    while word & 0x8000_0000 != 0 {
        if at + 4 > len {
            return None;
        }
        word = u32_at(b, at);
        at += 4;
    }
    let mut phy = Phy { len, width: 20, gi: 800, preamble: 20.0, ..Phy::default() };
    for bit in 0..28 {
        if present >> bit & 1 == 0 {
            continue;
        }
        let (align, size) = FIELDS[bit];
        at = (at + align - 1) & !(align - 1);
        if at + size > len {
            break;
        }
        let f = &b[at..at + size];
        match bit {
            1 => phy.flags = f[0],
            2 => {
                phy.rate = f32::from(f[0]) * 0.5;
                // DSSS/CCK rates carry the long preamble
                if matches!(f[0], 2 | 4 | 11 | 22) {
                    phy.preamble = 192.0;
                }
            }
            5 => phy.signal = Some(f[0] as i8),
            19 => {
                let (fl, mcs) = (f[1], f[2]);
                phy.standard = Standard::Ht;
                phy.width = if fl & 3 == 1 { 40 } else { 20 };
                phy.gi = if fl & 4 != 0 { 400 } else { 800 };
                phy.mcs = mcs % 8;
                phy.nss = mcs / 8 + 1;
                let symbol = if fl & 4 != 0 { 3.6 } else { 4.0 };
                phy.rate = ofdm(ht_sd(phy.width), phy.mcs, phy.nss, symbol);
                phy.preamble = 36.0;
            }
            20 => phy.ampdu = Some(u32_at(f, 0)),
            21 => {
                let (fl, bw, mn) = (f[2], f[3], f[4]);
                phy.standard = Standard::Vht;
                phy.width = match bw {
                    0 => 20,
                    1..=3 => 40,
                    4..=10 => 80,
                    11..=25 => 160,
                    _ => 20,
                };
                phy.gi = if fl & 4 != 0 { 400 } else { 800 };
                phy.mcs = mn >> 4;
                phy.nss = mn & 15;
                let symbol = if fl & 4 != 0 { 3.6 } else { 4.0 };
                phy.rate = ofdm(ht_sd(phy.width), phy.mcs, phy.nss, symbol);
                phy.preamble = 40.0;
            }
            23 => {
                let (d1, d3, d5, d6) = (u16_at(f, 0), u16_at(f, 4), u16_at(f, 8), u16_at(f, 10));
                phy.standard = Standard::He;
                let ru = usize::from(d5 & 15);
                phy.gi = [800, 1600, 3200, 3200][usize::from(d5 >> 4 & 3)];
                phy.mcs = (d3 >> 8 & 15) as u8;
                phy.nss = ((d6 & 15) as u8).max(1);
                if let Some(w) = HE_WIDTH.get(ru) {
                    phy.width = *w;
                }
                if d1 & 0x20 != 0 && ru < HE_SD.len() {
                    phy.rate = ofdm(HE_SD[ru], phy.mcs, phy.nss, 12.8 + f32::from(phy.gi) / 1000.0);
                }
                phy.preamble = 48.0;
            }
            _ => {}
        }
        at += size;
    }
    Some(phy)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A header with flags, signal and a VHT field, as mt76 lays it out.
    fn vht_header(mcs: u8, nss: u8, bw: u8) -> Vec<u8> {
        // present: flags (1), antenna signal (5), VHT (21)
        let present: u32 = 1 << 1 | 1 << 5 | 1 << 21;
        let mut b = vec![0u8, 0, 0, 0];
        b.extend_from_slice(&present.to_le_bytes());
        b.push(0x10); // flags at 8: FCS included
        b.push(-48i8 as u8); // signal at 9
        b.extend_from_slice(&[0, 0, 0, bw, mcs << 4 | nss, 0, 0, 0, 0, 0, 0, 0]);
        let len = b.len() as u16;
        b[2..4].copy_from_slice(&len.to_le_bytes());
        b
    }

    #[test]
    fn vht_rate_width_and_signal() {
        let phy = parse(&vht_header(9, 2, 4)).unwrap();
        assert_eq!(phy.standard, Standard::Vht);
        assert_eq!((phy.mcs, phy.nss, phy.width), (9, 2, 80));
        assert_eq!(phy.signal, Some(-48));
        assert_eq!(phy.flags & FLAG_FCS, FLAG_FCS);
        // 80 MHz, 2 streams, MCS 9, long GI: 780 Mbit/s
        assert!((phy.rate - 780.0).abs() < 1.0, "{}", phy.rate);
    }

    #[test]
    fn a_short_or_foreign_header_is_refused() {
        assert_eq!(parse(&[0, 0, 4]), None);
        assert_eq!(parse(&[1, 0, 8, 0, 0, 0, 0, 0]), None);
        let mut b = vht_header(0, 1, 0);
        b[2] = 200;
        assert_eq!(parse(&b), None);
    }

    #[test]
    fn legacy_cck_has_the_long_preamble() {
        let present: u32 = 1 << 2;
        let mut b = vec![0u8, 0, 9, 0];
        b.extend_from_slice(&present.to_le_bytes());
        b.push(2); // 1 Mbit/s
        let phy = parse(&b).unwrap();
        assert_eq!(phy.rate, 1.0);
        assert_eq!(phy.preamble, 192.0);
        assert_eq!(phy.standard, Standard::Legacy);
    }
}
