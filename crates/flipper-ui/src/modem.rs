//! The 5G Modem pages, as plain data: what ModemManager says about each modem, and
//! what the Network menu and the modem's own screen make of it.
//!
//! Nothing here talks to ModemManager; `mm` does, and fills in `Modem`. Nothing here
//! draws either: the binary maps `Line` onto the detail screen's rows. What is left
//! is every decision the pages make -- which modem is which, what state to say, how
//! strong the signal is, which band a channel is in -- and those are tested here
//! against what the RM520N-GL on the test unit reported.
//!
//! The plan these follow is the docs site's `cpu-software/flipctl/5g-modem-app`.

use crate::font;
use crate::simsel::Card;
use crate::theme::{count, metric, PANEL_W};

/// ModemManager's numbers for the things this page reads, from its API headers.
pub mod mm {
    /// `MMModemState`.
    pub const STATE_FAILED: i32 = -1;
    pub const STATE_LOCKED: i32 = 2;
    pub const STATE_DISABLED: i32 = 3;
    pub const STATE_DISABLING: i32 = 4;
    pub const STATE_ENABLED: i32 = 6;
    pub const STATE_CONNECTED: i32 = 11;

    /// `MMModemStateFailedReason`.
    pub const FAILED_SIM_MISSING: u32 = 2;
    pub const FAILED_SIM_ERROR: u32 = 3;
    pub const FAILED_ESIM_WITHOUT_PROFILES: u32 = 5;

    /// `MMModemAccessTechnology`, a bit mask.
    pub const TECH_UMTS: u32 = 1 << 5;
    pub const TECH_HSDPA: u32 = 1 << 6;
    pub const TECH_HSUPA: u32 = 1 << 7;
    pub const TECH_HSPA: u32 = 1 << 8;
    pub const TECH_HSPA_PLUS: u32 = 1 << 9;
    pub const TECH_LTE: u32 = 1 << 14;
    pub const TECH_5GNR: u32 = 1 << 15;

    /// `MMModem3gppRegistrationState`.
    pub const REG_IDLE: u32 = 0;
    pub const REG_DENIED: u32 = 3;
    pub const REG_UNKNOWN: u32 = 4;
    pub const REG_ROAMING: u32 = 5;
    pub const REG_ROAMING_SMS_ONLY: u32 = 7;
    pub const REG_ROAMING_CSFB_NOT_PREFERRED: u32 = 10;

    /// `MMCellType`.
    pub const CELL_UMTS: u32 = 3;
    pub const CELL_LTE: u32 = 5;
    pub const CELL_5GNR: u32 = 6;

    /// `MMModemLock`, what the SIM wants before it will work.
    pub const LOCK_NONE: u32 = 1;
    pub const LOCK_SIM_PIN: u32 = 2;
    pub const LOCK_SIM_PUK: u32 = 4;

    /// `MMModemPortType`, by name. 1 is MM's own "unknown".
    pub fn port_type(t: u32) -> &'static str {
        match t {
            2 => "net",
            3 => "at",
            4 => "qcdm",
            5 => "gps",
            6 => "qmi",
            7 => "mbim",
            8 => "audio",
            9 => "ignored",
            _ => "unknown",
        }
    }
}

/// LTE signal, from ModemManager's `Signal.Lte`. Each value is `None` when the modem
/// did not report it, which ModemManager also spells `-3276.8`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Lte {
    pub rssi: Option<f64>,
    pub rsrp: Option<f64>,
    pub rsrq: Option<f64>,
    pub snr: Option<f64>,
}

/// 5G signal, from `Signal.Nr5g`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Nr {
    pub rsrp: Option<f64>,
    pub rsrq: Option<f64>,
    pub snr: Option<f64>,
}

/// 3G signal, from `Signal.Umts`.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Umts {
    pub rssi: Option<f64>,
    pub rscp: Option<f64>,
    pub ecio: Option<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Rat {
    #[default]
    Other,
    Umts,
    Lte,
    Nr,
}

/// One cell from ModemManager's `GetCellInfo`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Cell {
    pub rat: Rat,
    pub serving: bool,
    /// The downlink channel: UARFCN, EARFCN or NR-ARFCN for the cell's `rat`.
    pub channel: Option<u32>,
    /// Physical cell ID. MM gives it as a hex string; this is the number.
    pub pci: Option<u32>,
    /// Cell identity and tracking area, as MM gives them: hex strings.
    pub ci: String,
    pub tac: String,
    pub rsrp: Option<f64>,
    pub rsrq: Option<f64>,
    pub sinr: Option<f64>,
}

/// The SIM in use, from MM's `Sim` object.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Sim {
    pub iccid: String,
    pub imsi: String,
    /// MCC/MNC the SIM belongs to, and the name it carries, often empty.
    pub operator_id: String,
    pub operator_name: String,
    /// The eUICC's ID, on an eSIM.
    pub eid: String,
}

/// The data connection, from MM's bearer. MM-only, so no NetworkManager is asked:
/// the bearer has the interface, the addresses, the APN and the byte counters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bearer {
    pub connected: bool,
    pub iface: String,
    pub apn: String,
    /// `address/prefix`.
    pub ipv4: String,
    pub ipv4_gateway: String,
    pub ipv6: String,
    pub dns: Vec<String>,
    pub mtu: Option<u32>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    /// Seconds since the connection came up.
    pub duration: u32,
}

/// The four receive paths' readings for one radio, from Quectel's `AT+QRSRP`,
/// `AT+QRSRQ` and `AT+QSINR`, in the modem's order: PRX, DRX, RX2, RX3. `None` is
/// the modem's `-32768`: no reading on that path.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Antennas {
    /// The radio the readings are for, as the modem names it: `LTE`, `NR5G`.
    pub rat: String,
    pub rsrp: [Option<i32>; 4],
    pub rsrq: [Option<i32>; 4],
    pub sinr: [Option<i32>; 4],
}

/// The receive paths, in the order the modem reports them.
pub const PATHS: [&str; 4] = ["PRX", "DRX", "RX2", "RX3"];

/// One reply's lines, `<prefix> <p0>,<p1>,<p2>,<p3>,<rat>`, as `(rat, values)`. A
/// modem on 5G NSA can answer with a line for each radio.
pub fn parse_paths(reply: &str, prefix: &str) -> Vec<(String, [Option<i32>; 4])> {
    reply
        .lines()
        .filter_map(|line| line.trim().strip_prefix(prefix))
        .filter_map(|rest| {
            let fields: Vec<&str> = rest.split(',').map(str::trim).collect();
            let [a, b, c, d, rat] = fields.as_slice() else { return None };
            let value = |v: &str| v.parse::<i32>().ok().filter(|&v| v != -32768);
            Some((rat.to_string(), [value(a), value(b), value(c), value(d)]))
        })
        .collect()
}

/// The three replies put together, one set per radio, in the order of the RSRP reply.
pub fn antennas(rsrp: &str, rsrq: &str, sinr: &str) -> Vec<Antennas> {
    let (q, s) = (parse_paths(rsrq, "+QRSRQ:"), parse_paths(sinr, "+QSINR:"));
    let find = |set: &[(String, [Option<i32>; 4])], rat: &str| {
        set.iter().find(|(r, _)| r == rat).map(|(_, v)| *v).unwrap_or_default()
    };
    parse_paths(rsrp, "+QRSRP:")
        .into_iter()
        .map(|(rat, rsrp)| Antennas { rsrq: find(&q, &rat), sinr: find(&s, &rat), rat, rsrp })
        .collect()
}

/// One modem as ModemManager describes it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modem {
    /// The USB (or PCIe) device in sysfs, MM's `Physdev`. Where the modem is plugged
    /// in, which is what names it: see `port`.
    pub physdev: String,
    pub model: String,
    /// MM's `EquipmentIdentifier`, the IMEI on a 3GPP modem.
    pub imei: String,
    pub state: i32,
    pub failed_reason: u32,
    pub access_tech: u32,
    /// MM's own `SignalQuality`, used only when there is no RSRP to go on.
    pub quality: Option<u32>,
    pub operator_name: String,
    pub operator_code: String,
    pub registration: u32,
    pub lte: Lte,
    pub nr: Nr,
    pub umts: Umts,
    /// Empty until the screen has asked, and while the bus policy refuses to let
    /// flipctl ask (systemd/flipctl-modemmanager.conf).
    pub cells: Vec<Cell>,

    // What the device page shows.
    pub manufacturer: String,
    pub revision: String,
    pub hardware_revision: String,
    pub carrier_config: String,
    pub carrier_revision: String,
    pub plugin: String,
    pub primary_port: String,
    /// `name (type)`, in MM's order.
    pub ports: Vec<String>,
    pub drivers: Vec<String>,
    /// The USB link in Mbit/s, from sysfs: 5000 is USB 3, 480 USB 2.
    pub usb_speed: Option<u32>,

    /// The SIM's own phone number, when it stores one: MM's `OwnNumbers`.
    pub own_numbers: Vec<String>,
    pub unlock_required: u32,
    /// Tries left, by lock: `(MMModemLock, tries)`.
    pub unlock_retries: Vec<(u32, u32)>,
    /// Read only while a screen holds focus, like the cells.
    pub sim: Option<Sim>,
    pub bearer: Option<Bearer>,
    /// Read only while the Signal page is open, through MM's `Command()`.
    pub antennas: Vec<Antennas>,
    /// Why there are no antenna readings, when there are none for a reason.
    pub antennas_note: String,
}

// ── Where it is plugged in ────────────────────────────────────────────────────

/// A modem's port, as the Network menu names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Port {
    pub name: String,
    /// The port on an external hub, for telling two modems on one hub apart.
    pub sub: Option<u32>,
    /// Menu order: the M.2 slot first, then the sockets left to right.
    pub order: u8,
}

/// Name the port a modem is plugged into, from its sysfs path.
///
/// The table is this board's, from rk3576-flipper-one.dtsi: the onboard hub sits on
/// root port 1 of the controller at 23400000, with the Wi-Fi on its port 1, USB-A
/// on 2, USB-C2 on 3 and the M.2 slot on 4. USB-C1 is the other controller. The bus
/// number is not part of it, so a modem keeps its name whether it came up at USB 3
/// (`2-1.4`) or USB 2 (`1-1.4`), and MM's own number (`Modem/0`), which changes on
/// every reset, plays no part.
///
/// A modem on PCIe can only be in the M.2 slot, which is the one PCIe lane.
pub fn port(physdev: &str) -> Port {
    let named = |name: &str, sub: Option<u32>, order: u8| Port { name: name.into(), sub, order };
    if physdev.contains("/pci") {
        return named("M.2", None, 0);
    }
    // The device's own directory is the last part of the path, `<bus>-<port>.<port>..`.
    let leaf = physdev.rsplit('/').next().unwrap_or_default();
    let chain: Vec<u32> = leaf
        .split_once('-')
        .map(|(_, ports)| ports.split('.').filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default();
    if physdev.contains("/23400000.usb/") && chain.first() == Some(&1) {
        let sub = chain.get(2).copied();
        return match chain.get(1) {
            Some(4) => named("M.2", sub, 0),
            Some(2) => named("USB-A", sub, 1),
            Some(3) => named("USB-C2", sub, 3),
            _ => named(&format!("USB {leaf}"), None, 9),
        };
    }
    if physdev.contains("/23000000.usb/") {
        return named("USB-C1", chain.get(1).copied(), 2);
    }
    named(&format!("USB {leaf}"), None, 9)
}

// ── The Network menu ──────────────────────────────────────────────────────────

/// One `5G Modem` entry in the Network menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// What the entry opens: the port name, or empty when there is no modem.
    pub key: String,
    /// The port, drawn after the label. Empty unless there is more than one modem.
    pub tag: String,
    pub status: String,
}

/// The modems in menu order, each with its port name.
///
/// Two MM entries with one IMEI are one modem: MM lists the RM520N-GL twice in PCIe
/// mode, once for each side. The one further along is kept, since that is the side
/// carrying the connection. Two modems on one external hub share a port name and get
/// their hub port after it.
pub fn ordered(modems: &[Modem]) -> Vec<(String, &Modem)> {
    let mut kept: Vec<&Modem> = Vec::new();
    for m in modems {
        match kept.iter_mut().find(|k| !m.imei.is_empty() && k.imei == m.imei) {
            Some(k) if m.state > k.state => *k = m,
            Some(_) => {}
            None => kept.push(m),
        }
    }
    let mut ports: Vec<(Port, &Modem)> = kept.into_iter().map(|m| (port(&m.physdev), m)).collect();
    ports.sort_by(|(a, am), (b, bm)| {
        (a.order, &a.name, a.sub, &am.physdev).cmp(&(b.order, &b.name, b.sub, &bm.physdev))
    });
    let shared = |name: &str| ports.iter().filter(|(p, _)| p.name == name).count() > 1;
    ports
        .iter()
        .map(|(p, m)| {
            let name = match p.sub {
                Some(sub) if shared(&p.name) => format!("{} {sub}", p.name),
                _ => p.name.clone(),
            };
            (name, *m)
        })
        .collect()
}

/// The Network menu's `5G Modem` entries: one when there is no modem or one modem,
/// with no port name, and one per modem with its port name when there are more.
///
/// `blocked` says whether the M.2 slot's rfkill switch is on; it belongs to that
/// slot alone.
pub fn entries(modems: &[Modem], blocked: bool) -> Vec<Entry> {
    let ordered = ordered(modems);
    if ordered.is_empty() {
        return vec![Entry { key: String::new(), tag: String::new(), status: "No modem".into() }];
    }
    let several = ordered.len() > 1;
    ordered
        .into_iter()
        .map(|(name, m)| Entry {
            tag: if several { name.clone() } else { String::new() },
            status: status(m, blocked && name == "M.2"),
            key: name,
        })
        .collect()
}

/// What the menu entry says on its right.
pub fn status(m: &Modem, blocked: bool) -> String {
    use mm::*;
    if blocked {
        return "Disabled".into();
    }
    let registered = !matches!(m.registration, REG_IDLE | REG_DENIED | REG_UNKNOWN);
    match m.state {
        STATE_FAILED => match m.failed_reason {
            FAILED_SIM_MISSING | FAILED_ESIM_WITHOUT_PROFILES => "No SIM".into(),
            FAILED_SIM_ERROR => "SIM error".into(),
            _ => "Modem failed".into(),
        },
        STATE_LOCKED => "SIM locked".into(),
        // MM's own off, without the switch: NetworkManager's WWAN radio, or mmcli.
        STATE_DISABLED | STATE_DISABLING => "Off".into(),
        STATE_CONNECTED => operator(m).unwrap_or_else(|| "Connected".into()),
        STATE_ENABLED if !registered => "No service".into(),
        _ if m.registration == REG_DENIED => "No service".into(),
        _ => "Connecting...".into(),
    }
}

/// The operator's name as the network sends it, or its MCC/MNC when it sends none.
fn operator(m: &Modem) -> Option<String> {
    let name = font::ascii(m.operator_name.trim());
    if !name.is_empty() {
        return Some(name);
    }
    let code = m.operator_code.trim();
    (code.len() > 3).then(|| format!("{}/{}", &code[..3], &code[3..]))
}

fn roaming(m: &Modem) -> bool {
    use mm::*;
    matches!(m.registration, REG_ROAMING | REG_ROAMING_SMS_ONLY | REG_ROAMING_CSFB_NOT_PREFERRED)
}

// ── Connection type ───────────────────────────────────────────────────────────

/// What the modem is connected over, from MM's access technologies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tech {
    /// 5G only.
    Sa,
    /// 5G alongside an LTE anchor (EN-DC). MM reports both bits.
    Nsa,
    Lte,
    HspaPlus,
    Hspa,
    Umts,
}

impl Tech {
    pub fn from_bits(bits: u32) -> Option<Self> {
        use mm::*;
        let lte = bits & TECH_LTE != 0;
        let nr = bits & TECH_5GNR != 0;
        Some(match () {
            _ if nr && lte => Tech::Nsa,
            _ if nr => Tech::Sa,
            _ if lte => Tech::Lte,
            _ if bits & TECH_HSPA_PLUS != 0 => Tech::HspaPlus,
            _ if bits & (TECH_HSPA | TECH_HSDPA | TECH_HSUPA) != 0 => Tech::Hspa,
            _ if bits & TECH_UMTS != 0 => Tech::Umts,
            _ => return None,
        })
    }

    /// The modem screen's line. Worded as the iPhone does: 5G+ is standalone 5G.
    pub fn long(self) -> &'static str {
        match self {
            Tech::Sa => "5G SA (Standalone 5G+)",
            Tech::Nsa => "5G NSA (5G + 4G LTE core)",
            Tech::Lte => "4G (LTE)",
            Tech::HspaPlus => "3G HSPA+ (WCDMA)",
            Tech::Hspa => "3G HSPA (WCDMA)",
            Tech::Umts => "3G UMTS (WCDMA)",
        }
    }

    /// The status bar's word: the same as the signal block's, so the two agree.
    pub fn badge(self) -> &'static str {
        short(self)
    }
}

// ── Signal strength ───────────────────────────────────────────────────────────

/// The RSRP that is 0% and the one that is 100%. Linear between, so each of the
/// screen's ten bars is 5 dB. -125 dBm is about where an LTE link stops working,
/// -75 dBm is as good as it gets in practice.
const RSRP_RANGE: (f64, f64) = (-125.0, -75.0);
/// The same for 3G's RSCP, which sits about 10 dB higher for the same link.
const RSCP_RANGE: (f64, f64) = (-115.0, -65.0);

/// How strong the signal is, 0..=100, or `None` when there is no reading at all.
///
/// From RSRP, the reference signal's own power, rather than MM's `SignalQuality`:
/// that follows RSSI, which is everything in the channel including neighbours, noise
/// and the cell's own traffic, so it climbs when the cell gets busier. In 5G NSA the
/// 4G anchor is what keeps the connection, so its RSRP is the one used. MM's number
/// stands in when there is no RSRP, which is the case until `Signal.Setup` has run.
pub fn percent(m: &Modem) -> Option<u8> {
    let tech = Tech::from_bits(m.access_tech)?;
    let scale = |v: f64, (lo, hi): (f64, f64)| ((v - lo) / (hi - lo) * 100.0).clamp(0.0, 100.0);
    let measured = match tech {
        Tech::Nsa | Tech::Lte => m.lte.rsrp.map(|v| scale(v, RSRP_RANGE)),
        Tech::Sa => m.nr.rsrp.map(|v| scale(v, RSRP_RANGE)),
        Tech::HspaPlus | Tech::Hspa | Tech::Umts => m.umts.rscp.map(|v| scale(v, RSCP_RANGE)),
    };
    measured.map(|p| p.round() as u8).or(m.quality.map(|q| q.min(100) as u8))
}

/// Filled bars out of `of`, rounding up so any signal at all lights one.
pub fn bars(percent: Option<u8>, of: u8) -> u8 {
    percent.map_or(0, |p| (u32::from(p) * u32::from(of)).div_ceil(100) as u8)
}

// ── Bands ─────────────────────────────────────────────────────────────────────

/// A band and the frequency it is known by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Band {
    /// `B1`, `n78`.
    pub name: String,
    /// `2100`, in MHz.
    pub mhz: u16,
}

impl Band {
    pub fn label(&self) -> String {
        format!("{} {} MHz", self.name, self.mhz)
    }
}

/// LTE: band, first downlink EARFCN, last, the band's lowest downlink frequency in
/// tenths of a MHz, and the name it goes by. From 3GPP TS 36.101 table 5.7.3-1; the
/// bands the RM520N-GL has and the common ones beside them.
#[rustfmt::skip]
const EUTRA: &[(u16, u32, u32, u32, u16)] = &[
    (1, 0, 599, 21100, 2100),        (2, 600, 1199, 19300, 1900),
    (3, 1200, 1949, 18050, 1800),    (4, 1950, 2399, 21100, 1700),
    (5, 2400, 2649, 8690, 850),      (7, 2750, 3449, 26200, 2600),
    (8, 3450, 3799, 9250, 900),      (12, 5010, 5179, 7290, 700),
    (13, 5180, 5279, 7460, 700),     (14, 5280, 5379, 7580, 700),
    (17, 5730, 5849, 7340, 700),     (18, 5850, 5999, 8600, 800),
    (19, 6000, 6149, 8750, 800),     (20, 6150, 6449, 7910, 800),
    (25, 8040, 8689, 19300, 1900),   (26, 8690, 9039, 8590, 850),
    (28, 9210, 9659, 7580, 700),     (29, 9660, 9769, 7170, 700),
    (30, 9770, 9869, 23500, 2300),   (32, 9920, 10359, 14520, 1500),
    (34, 36200, 36349, 20100, 2000), (38, 37750, 38249, 25700, 2600),
    (39, 38250, 38649, 18800, 1900), (40, 38650, 39649, 23000, 2300),
    (41, 39650, 41589, 24960, 2500), (42, 41590, 43589, 34000, 3500),
    (43, 43590, 45589, 36000, 3700), (46, 46790, 54539, 51500, 5200),
    (48, 55240, 56739, 35500, 3500), (66, 66436, 67335, 21100, 1700),
    (71, 68586, 68935, 6170, 600),
];

/// 5G: band, lowest and highest downlink frequency in MHz, and the name it goes by,
/// from 3GPP TS 38.104 table 5.2-1. A channel number does not say which band it was
/// sent in where bands overlap, so the order here decides: the band more networks
/// use comes first (n78 before n77, n1 before n65 and n66, n28 before n14).
#[rustfmt::skip]
const NR: &[(u16, u32, u32, u16)] = &[
    (78, 3300, 3800, 3500), (77, 3300, 4200, 3700), (79, 4400, 5000, 4700),
    (1, 2110, 2170, 2100),  (3, 1805, 1880, 1800),  (7, 2620, 2690, 2600),
    (8, 925, 960, 900),     (20, 791, 821, 800),    (28, 758, 803, 700),
    (5, 869, 894, 850),     (2, 1930, 1990, 1900),  (25, 1930, 1995, 1900),
    (66, 2110, 2200, 1700), (70, 1995, 2020, 2000), (71, 617, 652, 600),
    (12, 729, 746, 700),    (13, 746, 756, 700),    (14, 758, 768, 700),
    (18, 860, 875, 800),    (26, 859, 894, 850),    (29, 717, 728, 700),
    (30, 2350, 2360, 2300), (38, 2570, 2620, 2600), (40, 2300, 2400, 2300),
    (41, 2496, 2690, 2500), (48, 3550, 3700, 3500), (75, 1432, 1517, 1500),
    (76, 1427, 1432, 1500),
];

/// 3G: band, first and last downlink UARFCN, and the name it goes by, from 3GPP TS
/// 25.101 table 5.2. The five the RM520N-GL has.
#[rustfmt::skip]
const UTRA: &[(u16, u32, u32, u16)] = &[
    (1, 10562, 10838, 2100), (2, 9662, 9938, 1900), (4, 1537, 1738, 1700),
    (5, 4357, 4458, 850),    (8, 2937, 3088, 900),
];

/// The band a cell's downlink channel is in.
pub fn band(cell: &Cell) -> Option<Band> {
    let ch = cell.channel?;
    let found = |name: String, mhz: u16| Some(Band { name, mhz });
    match cell.rat {
        Rat::Lte => EUTRA
            .iter()
            .find(|(_, lo, hi, _, _)| (*lo..=*hi).contains(&ch))
            .and_then(|(b, _, _, _, mhz)| found(format!("B{b}"), *mhz)),
        Rat::Umts => UTRA
            .iter()
            .find(|(_, lo, hi, _)| (*lo..=*hi).contains(&ch))
            .and_then(|(b, _, _, mhz)| found(format!("B{b}"), *mhz)),
        Rat::Nr => {
            let f = nr_mhz(ch)?;
            NR.iter()
                .find(|(_, lo, hi, _)| (f64::from(*lo)..=f64::from(*hi)).contains(&f))
                .and_then(|(b, _, _, mhz)| found(format!("n{b}"), *mhz))
        }
        Rat::Other => None,
    }
}

/// The frequency of an NR-ARFCN, in MHz, from 3GPP TS 38.104 section 5.4.2.1.
fn nr_mhz(n: u32) -> Option<f64> {
    let (step_khz, base_mhz, base_n) = match n {
        0..=599_999 => (5.0, 0.0, 0),
        600_000..=2_016_666 => (15.0, 3000.0, 600_000),
        2_016_667..=3_279_165 => (60.0, 24_250.08, 2_016_667),
        _ => return None,
    };
    Some(base_mhz + step_khz / 1000.0 * f64::from(n - base_n))
}

/// The bands the modem is being served on, as one line: the 4G or 3G cell, and the
/// 5G cell beside it in NSA. Only what MM reports as serving: MM does not report
/// LTE carrier aggregation, and its neighbour list cannot tell a secondary carrier
/// from a neighbouring cell, so nothing is guessed from it.
pub fn bands(m: &Modem) -> Option<String> {
    let mut serving: Vec<&Cell> = m.cells.iter().filter(|c| c.serving).collect();
    // The anchor first, then 5G.
    serving.sort_by_key(|c| c.rat == Rat::Nr);
    let mut named: Vec<String> = Vec::new();
    for b in serving.iter().filter_map(|c| band(c)) {
        let label = b.label();
        if !named.contains(&label) {
            named.push(label);
        }
    }
    (!named.is_empty()).then(|| named.join(" + "))
}

// ── The main screen ───────────────────────────────────────────────────────────

/// How a run of text is drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Ink,
    /// The dim tone: the modem's name on the first item.
    Dim,
}

/// A run of text on the main screen, placed here and only drawn by the component.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    /// The left edge, or the right edge when `right` is set.
    pub x: i32,
    /// The top of the 12px row it sits in.
    pub y: i32,
    pub text: String,
    /// The item it belongs to, so it can invert with that item's press.
    pub item: usize,
    pub tone: Tone,
    pub right: bool,
}

/// One selectable item: the band the selector frames.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub y: i32,
    pub h: i32,
    pub what: What,
}

/// What an item is, which decides what Ok, Info and the soft keys do on it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum What {
    /// `5G Modem .... model .... ON/OFF`: Ok flips the switch, Info opens the device page.
    Radio,
    /// `SIM Card .... Physical`: Ok and Info open the SIM page.
    Sim,
    /// The operator and signal block: Ok opens the signal page.
    Signal,
    /// The data connection block: Ok opens the network page.
    Network,
}

impl What {
    /// Whether the Info soft key does anything here.
    pub fn has_info(self) -> bool {
        matches!(self, What::Radio | What::Sim)
    }
}

/// Where the signal bars go and how many are lit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BarsAt {
    pub x: i32,
    /// The top of their row; they stand on its bottom edge.
    pub y: i32,
    pub lit: u8,
}

/// The main screen, laid out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MainView {
    pub items: Vec<Item>,
    pub runs: Vec<Run>,
    /// 1px rules between the blocks, by y.
    pub dividers: Vec<i32>,
    pub bars: Option<BarsAt>,
    /// A message alone on the screen: no modem, or one restarting.
    pub message: String,
}

/// Bars in the signal block, and their geometry.
pub const BARS: u8 = count::MODEM_BARS as u8;
const BARS_W: i32 =
    count::MODEM_BARS * metric::MODEM_BAR_W + (count::MODEM_BARS - 1) * metric::MODEM_BAR_GAP;

/// The two speeds of the data connection, bytes a second, when known.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rates {
    pub down: Option<f64>,
    pub up: Option<f64>,
}

/// The three steps the signal block names RSRP with.
pub fn rsrp_word(dbm: f64) -> &'static str {
    match dbm {
        v if v >= -90.0 => "excellent",
        v if v >= -105.0 => "good",
        _ => "weak",
    }
}

/// The three steps the signal block names SNR with.
pub fn snr_word(db: f64) -> &'static str {
    match db {
        v if v >= 20.0 => "excellent",
        v if v >= 10.0 => "good",
        _ => "poor",
    }
}

/// The network in the signal block: 5G+ is standalone 5G, as the iPhone says it.
pub fn short(tech: Tech) -> &'static str {
    match tech {
        Tech::Sa => "5G+",
        Tech::Nsa => "5G",
        Tech::Lte => "4G",
        Tech::HspaPlus | Tech::Hspa | Tech::Umts => "3G",
    }
}

/// A speed for the network block: `980 B/s`, `13 KB/s`, `1.4 MB/s`.
pub fn rate(bytes_per_s: f64) -> String {
    match bytes_per_s.max(0.0) {
        v if v < 1000.0 => format!("{v:.0} B/s"),
        v if v < 100_000.0 => format!("{:.0} KB/s", v / 1000.0),
        v if v < 10_000_000.0 => format!("{:.1} MB/s", v / 1_000_000.0),
        v => format!("{:.0} MB/s", v / 1_000_000.0),
    }
}

/// An amount of data: `980 B`, `27 KB`, `14.2 MB`, `3.1 GB`.
pub fn size(bytes: u64) -> String {
    let b = bytes as f64;
    match bytes {
        0..=999 => format!("{bytes} B"),
        1_000..=99_999 => format!("{:.1} KB", b / 1e3),
        100_000..=999_999 => format!("{:.0} KB", b / 1e3),
        1_000_000..=999_999_999 => format!("{:.1} MB", b / 1e6),
        _ => format!("{:.1} GB", b / 1e9),
    }
}

fn width(text: &str) -> i32 {
    i32::from(font::ROW.text_width(text))
}

/// Cut `text` to `room` pixels in the row face, ending in `..` when it had to.
fn cut(text: &str, room: i32) -> String {
    if width(text) <= room {
        return text.to_string();
    }
    let mut out = String::new();
    for c in text.chars() {
        let mut probe = out.clone();
        probe.push(c);
        if width(&probe) + width("..") > room {
            break;
        }
        out = probe;
    }
    out + ".."
}

/// The main screen for `m`, top to bottom: the radio switch, the SIM, the operator
/// and signal block, and the data connection block. Every position is worked out
/// here, so the component draws and measures nothing.
///
/// `m2` is whether this is the M.2 slot, the only one with an rfkill switch;
/// `blocked` whether that switch is on, which leaves the first two items only.
/// `seen` is whether this screen has shown its modem before: one that goes away
/// from under an open screen is being reset or switched to the other SIM. `card` is
/// which card the board's selector is on: `None` off the M.2 slot, which is the only
/// one behind it.
pub fn main_view(
    m: Option<&Modem>,
    m2: bool,
    blocked: bool,
    seen: bool,
    card: Option<Card>,
    rates: Rates,
) -> MainView {
    let Some(m) = m else {
        let say = if seen { "Restarting modem" } else { "No modem found" };
        return MainView { message: say.into(), ..MainView::default() };
    };
    let row = metric::ITEM_H;
    let left = metric::MARGIN_H;
    let right = PANEL_W as i32 - metric::MARGIN_H;
    let gap = metric::MARGIN_H;
    let mut v = MainView::default();
    let mut y = metric::SUBMENU_CONTAINER_Y;
    let put =
        |v: &mut MainView, x: i32, y: i32, text: &str, item: usize, tone: Tone, at_right: bool| {
            v.runs.push(Run { x, y, text: text.into(), item, tone, right: at_right });
        };

    // 1: the radio. The switch only where there is one to throw.
    let label = "5G Modem";
    put(&mut v, left, y, label, 0, Tone::Ink, false);
    let switch = if !m2 {
        ""
    } else if blocked {
        "OFF"
    } else {
        "ON"
    };
    put(&mut v, right, y, switch, 0, Tone::Ink, true);
    let name = format!("{} {}", m.manufacturer.trim(), m.model.trim()).trim().to_string();
    let name_x = left + width(label) + gap;
    let room = right - width(switch) - gap - name_x;
    put(&mut v, name_x, y, &cut(&font::ascii(&name), room), 0, Tone::Dim, false);
    v.items.push(Item { y, h: row, what: What::Radio });
    y += row;

    // 2: the SIM, by which card the board's selector is on.
    put(&mut v, left, y, "SIM Card", 1, Tone::Ink, false);
    put(&mut v, right, y, &format!("{} >", card.map_or("SIM", Card::name)), 1, Tone::Ink, true);
    v.items.push(Item { y, h: row, what: What::Sim });
    y += row;
    if blocked {
        return v;
    }

    // 3: operator and signal.
    v.dividers.push(y);
    y += 1;
    let tech = Tech::from_bits(m.access_tech);
    let p = percent(m);
    let pct = p.map_or("--%".to_string(), |p| format!("{p}%"));
    put(&mut v, right, y, &pct, 2, Tone::Ink, true);
    let bars_x = right - width(&pct) - gap / 2 - BARS_W;
    v.bars = Some(BarsAt { x: bars_x, y, lit: bars(p, BARS) });
    put(&mut v, bars_x - gap / 2, y, tech.map_or("--", short), 2, Tone::Ink, true);
    let (strength, quality) = match tech {
        Some(Tech::Sa) => (signal_line("RSRP", m.nr.rsrp, true), snr_line(m.nr.snr)),
        Some(Tech::HspaPlus | Tech::Hspa | Tech::Umts) => (
            signal_line("RSCP", m.umts.rscp, false),
            format!("Ec/Io {}", m.umts.ecio.map_or("--".into(), |v| format!("{v:.1} dB"))),
        ),
        _ => (signal_line("RSRP", m.lte.rsrp, true), snr_line(m.lte.snr)),
    };
    put(&mut v, right, y + row, &strength, 2, Tone::Ink, true);
    put(&mut v, right, y + 2 * row, &quality, 2, Tone::Ink, true);
    let who = match (m.state, operator(m)) {
        (mm::STATE_CONNECTED, Some(name)) => name,
        _ if tech.is_some() => operator(m).unwrap_or_else(|| status(m, false)),
        _ => status(m, false),
    };
    let who = if roaming(m) { format!("{who} (roaming)") } else { who };
    let wide = width(&strength).max(width(&quality));
    put(
        &mut v,
        left,
        y,
        &cut(&who, bars_x - width(tech.map_or("--", short)) - gap - left),
        2,
        Tone::Ink,
        false,
    );
    if let Some(number) = m.own_numbers.first() {
        put(&mut v, left, y + row, &cut(number, right - wide - gap - left), 2, Tone::Ink, false);
    }
    v.items.push(Item { y, h: 3 * row, what: What::Signal });
    y += 3 * row;

    // 4: the data connection.
    v.dividers.push(y);
    y += 1;
    let lines = match m.bearer.as_ref().filter(|b| b.connected) {
        Some(b) => {
            let speeds = [("Down", rates.down), ("Up", rates.up)]
                .map(|(dir, r)| r.map(|r| format!("{dir} {}", rate(r))));
            let wide = speeds.iter().flatten().map(|s| width(s)).max().unwrap_or(0);
            for (i, s) in speeds.iter().enumerate() {
                if let Some(s) = s {
                    put(&mut v, right, y + i as i32 * row, s, 3, Tone::Ink, true);
                }
            }
            let room = right - wide - gap - left;
            let mut lines = Vec::new();
            if !b.ipv4.is_empty() {
                lines.push(format!("IPv4 {}", b.ipv4.split('/').next().unwrap_or_default()));
            }
            if !b.ipv6.is_empty() {
                lines.push(format!("IPv6 {}", b.ipv6.split('/').next().unwrap_or_default()));
            }
            if !b.apn.is_empty() {
                lines.push(format!("APN {}", b.apn));
            }
            lines.into_iter().take(3).map(|l| cut(&l, room)).collect::<Vec<_>>()
        }
        None => vec!["Not connected".to_string()],
    };
    let n = (lines.len() as i32).max(2);
    for (i, l) in lines.iter().enumerate() {
        put(&mut v, left, y + i as i32 * row, l, 3, Tone::Ink, false);
    }
    v.items.push(Item { y, h: n * row, what: What::Network });
    v
}

fn signal_line(name: &str, v: Option<f64>, words: bool) -> String {
    match v {
        Some(v) if words => format!("{name} {v:.0} dBm ({})", rsrp_word(v)),
        Some(v) => format!("{name} {v:.0} dBm"),
        None => format!("{name} --"),
    }
}

fn snr_line(v: Option<f64>) -> String {
    v.map_or("SNR --".into(), |v| format!("SNR {v:.1} dB ({})", snr_word(v)))
}

// ── The pages behind it ───────────────────────────────────────────────────────

/// One line of a page opened from the main screen, drawn on the detail screen.
#[derive(Clone, Debug, PartialEq)]
pub enum Line {
    /// A label in the bold face, heading what follows.
    Head(String),
    /// A label on the left and a value on the right.
    Pair { label: String, value: String },
    /// One line of text across the screen.
    Text(String),
    /// A message alone in the middle of an otherwise empty screen.
    Centered(String),
    /// A line the cursor sits on and Ok acts on: switching the SIM.
    Action(String),
}

/// The pages the main screen opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Page {
    Main,
    /// Info on the radio item.
    Device,
    /// Ok or Info on the SIM item.
    Sim,
    /// Ok on the signal block.
    Signal,
    /// Ok on the data connection block.
    Network,
    /// The Config soft key. A placeholder for now.
    Config,
}

impl Page {
    pub fn title(self) -> &'static str {
        match self {
            Page::Main => "",
            Page::Device => "Device",
            Page::Sim => "SIM Card",
            Page::Signal => "Signal",
            Page::Network => "Network",
            Page::Config => "Config",
        }
    }
}

fn pair(label: &str, value: &str) -> Line {
    Line::Pair {
        label: label.into(),
        value: if value.is_empty() { "-".into() } else { value.into() },
    }
}

fn dbm(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.0} dBm"))
}

fn db(v: Option<f64>) -> String {
    v.map_or("-".into(), |v| format!("{v:.1} dB"))
}

/// A page's lines. `card` is the card the board's selector is on, for the SIM page,
/// and `rates` the data connection's speed, for the network page.
pub fn page(page: Page, m: Option<&Modem>, card: Option<Card>, rates: Rates) -> Vec<Line> {
    let Some(m) = m else {
        return vec![Line::Centered("No modem found".into())];
    };
    match page {
        Page::Main => Vec::new(),
        Page::Device => device(m),
        Page::Sim => sim_page(m, card),
        Page::Signal => signal(m),
        Page::Network => network(m, rates),
        Page::Config => vec![Line::Centered("No settings yet".into())],
    }
}

fn device(m: &Modem) -> Vec<Line> {
    let link = match (m.physdev.contains("/pci"), m.usb_speed) {
        (true, _) => "PCIe".to_string(),
        (false, Some(s)) if s >= 5000 => format!("USB 3, {}", port(&m.physdev).name),
        (false, Some(_)) => format!("USB 2, {}", port(&m.physdev).name),
        (false, None) => port(&m.physdev).name,
    };
    let mut out = vec![
        pair("Manufacturer", &m.manufacturer),
        pair("Model", &m.model),
        pair("Firmware", &m.revision),
        pair("Hardware", &m.hardware_revision),
        pair("IMEI", &m.imei),
        pair("Connected", &link),
        pair("Carrier config", &m.carrier_config),
        pair("Config version", &m.carrier_revision),
        pair("MM plugin", &m.plugin),
        pair("Primary port", &m.primary_port),
        pair("Drivers", &m.drivers.join(", ")),
        Line::Head("Ports".into()),
    ];
    out.extend(m.ports.iter().map(|p| Line::Text(p.clone())));
    out
}

/// The words on the SIM page's first line, which moves the modem to the other card.
pub fn switch_label(card: Card) -> &'static str {
    match card {
        Card::Physical => "Switch to eSIM",
        Card::Esim => "Switch to physical SIM",
    }
}

fn sim_page(m: &Modem, card: Option<Card>) -> Vec<Line> {
    let pin = match m.unlock_required {
        mm::LOCK_NONE => "Not required".to_string(),
        mm::LOCK_SIM_PIN => "PIN required".to_string(),
        mm::LOCK_SIM_PUK => "PUK required".to_string(),
        0 => "-".to_string(),
        other => format!("Locked ({other})"),
    };
    let tries =
        |lock: u32| m.unlock_retries.iter().find(|(l, _)| *l == lock).map(|(_, n)| n.to_string());
    // The switch first, where there is one: the line the cursor is on.
    let mut out: Vec<Line> =
        card.map(|c| Line::Action(switch_label(c).into())).into_iter().collect();
    out.push(pair("In use", card.map_or("SIM", Card::name)));
    out.push(pair("PIN", &pin));
    if let Some(n) = tries(mm::LOCK_SIM_PIN) {
        out.push(pair("PIN tries left", &n));
    }
    if let Some(n) = tries(mm::LOCK_SIM_PUK) {
        out.push(pair("PUK tries left", &n));
    }
    match &m.sim {
        Some(s) => {
            out.push(pair("ICCID", &s.iccid));
            out.push(pair("IMSI", &s.imsi));
            out.push(pair("Operator", &s.operator_id));
            if !s.operator_name.is_empty() {
                out.push(pair("SIM name", &s.operator_name));
            }
            if !s.eid.is_empty() {
                out.push(pair("EID", &s.eid));
            }
        }
        None if m.state == mm::STATE_FAILED => out.push(Line::Text("No SIM card".into())),
        None => out.push(Line::Text("Reading the SIM..".into())),
    }
    out.push(pair("Phone number", m.own_numbers.first().map_or("", |s| s.as_str())));
    out
}

fn signal(m: &Modem) -> Vec<Line> {
    let tech = Tech::from_bits(m.access_tech);
    let mut out = vec![
        pair("Network", tech.map_or("Not registered", Tech::long)),
        pair("Operator", &operator(m).unwrap_or_default()),
        pair("Bands", &bands(m).unwrap_or_default()),
    ];
    if let Some(c) = m.cells.iter().find(|c| c.serving && c.rat != Rat::Nr) {
        out.push(Line::Head("Serving cell".into()));
        out.push(pair("Band", &band(c).map(|b| b.label()).unwrap_or_default()));
        out.push(pair("Channel", &c.channel.map(|ch| ch.to_string()).unwrap_or_default()));
        out.push(pair("PCI", &c.pci.map(|p| p.to_string()).unwrap_or_default()));
        out.push(pair("Cell ID", &c.ci));
        out.push(pair("TAC", &c.tac));
    }
    if m.lte != Lte::default() {
        out.push(Line::Head("4G".into()));
        out.push(pair("RSRP", &dbm(m.lte.rsrp)));
        out.push(pair("RSRQ", &db(m.lte.rsrq)));
        out.push(pair("RSSI", &dbm(m.lte.rssi)));
        out.push(pair("SNR", &db(m.lte.snr)));
    }
    if m.nr != Nr::default() {
        out.push(Line::Head("5G".into()));
        out.push(pair("RSRP", &dbm(m.nr.rsrp)));
        out.push(pair("RSRQ", &db(m.nr.rsrq)));
        out.push(pair("SNR", &db(m.nr.snr)));
    }
    if m.umts != Umts::default() {
        out.push(Line::Head("3G".into()));
        out.push(pair("RSCP", &dbm(m.umts.rscp)));
        out.push(pair("Ec/Io", &db(m.umts.ecio)));
        out.push(pair("RSSI", &dbm(m.umts.rssi)));
    }
    let neighbours: Vec<&Cell> = m.cells.iter().filter(|c| !c.serving).collect();
    if !neighbours.is_empty() {
        out.push(Line::Head("Neighbour cells".into()));
        for c in neighbours {
            let b = band(c).map_or("-".into(), |b| b.name);
            let pci = c.pci.map_or("-".into(), |p| p.to_string());
            out.push(pair(&format!("{b} PCI {pci}"), &dbm(c.rsrp)));
        }
    }
    out.extend(antenna_lines(m));
    out
}

/// Which connector each receive path is on, in the band `b`, in the order the
/// modem reports the paths (PRX, DRX, RX2, RX3): `Some(n)` for ANTn, `None` for a
/// path that has no connector in that band. From the RM520N-GL's connector table,
/// Table 32 of Quectel's RM520N Series Hardware Design V1.3, as written up in
/// docs/testing/RM520N-GL.md. RX2 and RX3 are taken to be its third and fourth
/// receive paths.
///
/// - Low bands, under 1 GHz: two paths only, primary on ANT0, diversity on ANT3.
/// - 5G n77, n78, n79 and LTE B42, B43, B48 (3.3-5 GHz): primary on ANT2, diversity
///   on ANT0, MIMO on ANT1 and ANT3.
/// - Every other band: primary on ANT0, diversity on ANT3, MIMO on ANT1 and ANT2.
pub fn connectors(b: &Band) -> [Option<u8>; 4] {
    match b.name.as_str() {
        "n77" | "n78" | "n79" | "B42" | "B43" | "B48" => [Some(2), Some(0), Some(1), Some(3)],
        _ if b.mhz < 1000 => [Some(0), Some(3), None, None],
        _ => [Some(0), Some(3), Some(1), Some(2)],
    }
}

/// The Signal page's per-antenna section: by connector, ANT0 to ANT3 in the order
/// they sit on the module, each with the receive path it carries in the band in use,
/// so a reading leads to the antenna to look at.
fn antenna_lines(m: &Modem) -> Vec<Line> {
    let mut out = vec![Line::Head("Antennas".into())];
    if m.antennas.is_empty() {
        let why = if m.antennas_note.is_empty() { "Reading.." } else { m.antennas_note.as_str() };
        out.push(Line::Text(why.into()));
        return out;
    }
    let numbers = |set: &Antennas, i: usize| match (set.rsrp[i], set.rsrq[i], set.sinr[i]) {
        (None, None, None) => "no reading".to_string(),
        (p, q, s) => {
            let n = |v: Option<i32>| v.map_or("-".into(), |v| v.to_string());
            format!("{} / {} / {}", n(p), n(q), n(s))
        }
    };
    for set in &m.antennas {
        let (rat, cell_rat) = match set.rat.as_str() {
            "NR5G" => ("5G", Rat::Nr),
            "LTE" => ("4G", Rat::Lte),
            other => (other, Rat::Other),
        };
        let b = m.cells.iter().find(|c| c.serving && c.rat == cell_rat).and_then(band);
        let head = b.as_ref().map_or(rat.to_string(), |b| format!("{rat} {}", b.name));
        out.push(pair(&head, "RSRP / RSRQ / SINR"));
        match b.as_ref().map(connectors) {
            Some(on) => {
                for ant in 0..4u8 {
                    match on.iter().position(|&c| c == Some(ant)) {
                        Some(i) => {
                            out.push(pair(&format!("ANT{ant} {}", PATHS[i]), &numbers(set, i)))
                        }
                        None => out.push(pair(&format!("ANT{ant}"), "not used on this band")),
                    }
                }
            }
            // No band to go on, so no connectors: the paths as the modem names them.
            None => {
                for (i, name) in PATHS.iter().enumerate() {
                    out.push(pair(name, &numbers(set, i)));
                }
            }
        }
    }
    out
}

fn network(m: &Modem, rates: Rates) -> Vec<Line> {
    let Some(b) = m.bearer.as_ref().filter(|b| b.connected) else {
        return vec![Line::Centered("Not connected".into())];
    };
    let up = format!("{}h {:02}m", b.duration / 3600, b.duration / 60 % 60);
    vec![
        pair("Interface", &b.iface),
        pair("APN", &b.apn),
        pair("IPv4", &b.ipv4),
        pair("Gateway", &b.ipv4_gateway),
        pair("IPv6", &b.ipv6),
        pair("DNS", &b.dns.join(", ")),
        pair("MTU", &b.mtu.map(|v| v.to_string()).unwrap_or_default()),
        pair("Download", &rates.down.map(rate).unwrap_or_default()),
        pair("Upload", &rates.up.map(rate).unwrap_or_default()),
        pair("Received", &size(b.rx_bytes)),
        pair("Sent", &size(b.tx_bytes)),
        pair("Connected for", &up),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    const M2: &str = "/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.4";

    /// The RM520N-GL on the test unit, connected on Vodafone UK in NSA, with the
    /// values ModemManager reported on 2026-10-21.
    fn rm520() -> Modem {
        Modem {
            physdev: M2.into(),
            model: "RM520N-GL".into(),
            imei: "867922072001297".into(),
            state: mm::STATE_CONNECTED,
            access_tech: mm::TECH_LTE | mm::TECH_5GNR,
            quality: Some(44),
            operator_name: "vodafone UK".into(),
            operator_code: "23415".into(),
            registration: 1,
            lte: Lte { rssi: Some(-87.0), rsrp: Some(-120.0), rsrq: Some(-15.0), snr: Some(-5.8) },
            nr: Nr { rsrp: Some(-115.0), rsrq: Some(-20.0), snr: None },
            cells: vec![
                Cell { rat: Rat::Lte, serving: true, channel: Some(223), ..Cell::default() },
                Cell { rat: Rat::Lte, serving: false, channel: Some(6300), ..Cell::default() },
                Cell { rat: Rat::Lte, serving: false, channel: Some(2850), ..Cell::default() },
            ],
            ..Modem::default()
        }
    }

    fn at(physdev: &str, imei: &str) -> Modem {
        Modem { physdev: physdev.into(), imei: imei.into(), ..rm520() }
    }

    #[test]
    fn ports_are_named_by_socket_not_by_bus() {
        assert_eq!(port(M2).name, "M.2");
        // The same slot at USB 2 speed.
        assert_eq!(
            port("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb1/1-1/1-1.4").name,
            "M.2"
        );
        assert_eq!(
            port("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.2").name,
            "USB-A"
        );
        assert_eq!(
            port("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.3").name,
            "USB-C2"
        );
        assert_eq!(
            port("/sys/devices/platform/soc/23000000.usb/xhci-hcd.1.auto/usb4/4-1").name,
            "USB-C1"
        );
        assert_eq!(
            port("/sys/devices/platform/soc/22000000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0")
                .name,
            "M.2"
        );
        // Behind a hub on USB-A: the socket, and the hub's port for later.
        let hubbed =
            port("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.2/2-1.2.3");
        assert_eq!((hubbed.name.as_str(), hubbed.sub), ("USB-A", Some(3)));
    }

    #[test]
    fn one_modem_or_none_is_one_entry_without_a_port() {
        assert_eq!(
            entries(&[], false),
            [Entry { key: "".into(), tag: "".into(), status: "No modem".into() }]
        );
        assert_eq!(
            entries(&[rm520()], false),
            [Entry { key: "M.2".into(), tag: "".into(), status: "vodafone UK".into() }]
        );
    }

    #[test]
    fn several_modems_are_named_in_socket_order() {
        let usb_a = Modem {
            state: mm::STATE_FAILED,
            failed_reason: mm::FAILED_SIM_MISSING,
            ..at("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.2", "2")
        };
        let c2 = Modem {
            state: mm::STATE_FAILED,
            failed_reason: mm::FAILED_SIM_MISSING,
            ..at("/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.3", "3")
        };
        let shown: Vec<(String, String)> =
            entries(&[c2, usb_a, rm520()], false).into_iter().map(|e| (e.tag, e.status)).collect();
        assert_eq!(
            shown,
            [
                ("M.2".into(), "vodafone UK".into()),
                ("USB-A".into(), "No SIM".into()),
                ("USB-C2".into(), "No SIM".into()),
            ]
        );
    }

    #[test]
    fn two_modems_on_one_hub_get_their_hub_port() {
        let hub = "/sys/devices/platform/soc/23400000.usb/xhci-hcd.0.auto/usb2/2-1/2-1.2/2-1.2.";
        let names: Vec<String> =
            ordered(&[at(&format!("{hub}4"), "4"), at(&format!("{hub}1"), "1")])
                .into_iter()
                .map(|(n, _)| n)
                .collect();
        assert_eq!(names, ["USB-A 1", "USB-A 4"]);
    }

    /// PCIe mode: MM lists the same module twice, and it is one entry.
    #[test]
    fn one_imei_is_one_modem() {
        let usb_side = Modem { state: mm::STATE_ENABLED, ..rm520() };
        let pcie = Modem {
            physdev: "/sys/devices/platform/soc/22000000.pcie/pci0000:00/0000:00:00.0/0000:01:00.0"
                .into(),
            ..rm520()
        };
        let both = [usb_side, pcie];
        let kept = ordered(&both);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].1.physdev.contains("pcie"));
    }

    #[test]
    fn the_entry_says_what_is_wrong_first() {
        let m = rm520();
        assert_eq!(status(&m, true), "Disabled");
        let no_sim =
            Modem { state: mm::STATE_FAILED, failed_reason: mm::FAILED_SIM_MISSING, ..m.clone() };
        assert_eq!(status(&no_sim, false), "No SIM");
        assert_eq!(status(&Modem { state: mm::STATE_LOCKED, ..m.clone() }, false), "SIM locked");
        assert_eq!(
            status(&Modem { state: 7, registration: 2, ..m.clone() }, false),
            "Connecting..."
        );
        assert_eq!(status(&Modem { state: 8, ..m.clone() }, false), "Connecting...");
        assert_eq!(
            status(
                &Modem { state: mm::STATE_ENABLED, registration: mm::REG_IDLE, ..m.clone() },
                false
            ),
            "No service"
        );
        assert_eq!(status(&Modem { operator_name: "".into(), ..m }, false), "234/15");
    }

    #[test]
    fn technology_is_worded_as_agreed() {
        assert_eq!(Tech::from_bits(49152), Some(Tech::Nsa));
        assert_eq!(Tech::from_bits(16384), Some(Tech::Lte));
        assert_eq!(Tech::from_bits(mm::TECH_5GNR).map(Tech::long), Some("5G SA (Standalone 5G+)"));
        assert_eq!(Tech::from_bits(mm::TECH_HSPA_PLUS | mm::TECH_UMTS), Some(Tech::HspaPlus));
        assert_eq!(Tech::from_bits(0), None);
    }

    /// Bars and percentage come from RSRP, 5 dB a bar; in NSA from the 4G anchor.
    #[test]
    fn signal_follows_rsrp() {
        let m = rm520();
        assert_eq!(percent(&m), Some(10));
        assert_eq!(bars(percent(&m), 10), 1);
        let strong = Modem { lte: Lte { rsrp: Some(-80.0), ..m.lte }, ..m.clone() };
        assert_eq!((percent(&strong), bars(percent(&strong), 10)), (Some(90), 9));
        let dead = Modem { lte: Lte { rsrp: Some(-140.0), ..m.lte }, ..m.clone() };
        assert_eq!(bars(percent(&dead), 10), 0);
        // No Signal.Setup yet: MM's own number stands in.
        let unset = Modem { lte: Lte::default(), ..m.clone() };
        assert_eq!(percent(&unset), Some(44));
        // Not registered: nothing to show.
        assert_eq!(percent(&Modem { access_tech: 0, ..m }), None);
        // The status bar's five.
        assert_eq!(bars(Some(44), 5), 3);
    }

    #[test]
    fn channels_name_their_bands() {
        let lte = |ch| {
            band(&Cell { rat: Rat::Lte, serving: true, channel: Some(ch), ..Cell::default() })
                .map(|b| b.label())
        };
        assert_eq!(lte(223).as_deref(), Some("B1 2100 MHz"));
        assert_eq!(lte(6300).as_deref(), Some("B20 800 MHz"));
        assert_eq!(lte(2850).as_deref(), Some("B7 2600 MHz"));
        assert_eq!(lte(3501).as_deref(), Some("B8 900 MHz"));
        let nr = |ch| {
            band(&Cell { rat: Rat::Nr, serving: true, channel: Some(ch), ..Cell::default() })
                .map(|b| b.label())
        };
        // 3500 MHz and 3840 MHz.
        assert_eq!(nr(633_334).as_deref(), Some("n78 3500 MHz"));
        assert_eq!(nr(656_000).as_deref(), Some("n77 3700 MHz"));
        // 632.5 MHz.
        assert_eq!(nr(126_500).as_deref(), Some("n71 600 MHz"));
        let umts =
            band(&Cell { rat: Rat::Umts, serving: true, channel: Some(10700), ..Cell::default() });
        assert_eq!(umts.map(|b| b.label()).as_deref(), Some("B1 2100 MHz"));
    }

    /// The serving 4G cell only: B20 and B7 were aggregated, and MM listed them as
    /// neighbours, so they are not shown as bands in use.
    #[test]
    fn only_serving_cells_are_bands_in_use() {
        assert_eq!(bands(&rm520()).as_deref(), Some("B1 2100 MHz"));
        let mut nsa = rm520();
        nsa.cells.push(Cell {
            rat: Rat::Nr,
            serving: true,
            channel: Some(633_334),
            ..Cell::default()
        });
        assert_eq!(bands(&nsa).as_deref(), Some("B1 2100 MHz + n78 3500 MHz"));
        assert_eq!(bands(&Modem { cells: vec![], ..nsa }), None);
    }

    /// The test unit as the full screen sees it: connected, with a bearer.
    fn connected() -> Modem {
        Modem {
            manufacturer: "Quectel".into(),
            own_numbers: vec!["+447253012346".into()],
            bearer: Some(Bearer {
                connected: true,
                iface: "wwu1u4i4".into(),
                apn: "wap.vodafone.co.uk".into(),
                ipv4: "10.38.112.9/30".into(),
                ipv6: "2a01:4c8:1a3:9f02:5c3e:21ff:fe47:9a10/64".into(),
                ..Bearer::default()
            }),
            ..rm520()
        }
    }

    fn texts(v: &MainView, item: usize) -> Vec<&str> {
        v.runs
            .iter()
            .filter(|r| r.item == item && !r.text.is_empty())
            .map(|r| r.text.as_str())
            .collect()
    }

    #[test]
    fn the_main_screen_reads_top_to_bottom() {
        let rates = Rates { down: Some(13_400.0), up: Some(1_400_000.0) };
        let v = main_view(Some(&connected()), true, false, true, Some(Card::Physical), rates);
        let what: Vec<What> = v.items.iter().map(|i| i.what).collect();
        assert_eq!(what, [What::Radio, What::Sim, What::Signal, What::Network]);
        assert_eq!(texts(&v, 0), ["5G Modem", "ON", "Quectel RM520N-GL"]);
        assert_eq!(texts(&v, 1), ["SIM Card", "Physical >"]);
        assert_eq!(
            texts(&v, 2),
            [
                "10%",
                "5G",
                "RSRP -120 dBm (weak)",
                "SNR -5.8 dB (poor)",
                "vodafone UK",
                "+447253012346"
            ]
        );
        let net = texts(&v, 3);
        assert_eq!(net[..3], ["Down 13 KB/s", "Up 1.4 MB/s", "IPv4 10.38.112.9"]);
        // Too long for the room beside the speeds, so cut.
        assert!(net[3].starts_with("IPv6 2a01:4c8:") && net[3].ends_with(".."), "{}", net[3]);
        assert_eq!(net[4], "APN wap.vodafone.co.uk");
        // 10% is the first of six bars.
        assert_eq!(v.bars.map(|b| b.lit), Some(1));
    }

    /// Everything clears the soft keys, and on every row the left text stops short
    /// of the right text.
    #[test]
    fn the_main_screen_fits_the_panel() {
        let v = main_view(
            Some(&connected()),
            true,
            false,
            true,
            Some(Card::Esim),
            Rates { down: Some(9e6), up: Some(9e6) },
        );
        let soft_keys = crate::theme::PANEL_H as i32 - 14;
        let last = v.items.last().expect("items");
        assert!(last.y + last.h <= soft_keys, "the last item ends at {}", last.y + last.h);
        for r in v.runs.iter().filter(|r| !r.right) {
            let end = r.x + width(&r.text);
            for o in v.runs.iter().filter(|o| o.right && o.y == r.y) {
                assert!(end < o.x - width(&o.text), "{:?} runs into {:?}", r.text, o.text);
            }
            assert!(end <= PANEL_W as i32 - metric::MARGIN_H, "{:?} is too wide", r.text);
        }
        let bars = v.bars.expect("bars");
        let label = v.runs.iter().find(|r| r.text == "5G").expect("the network");
        assert!(label.x < bars.x, "the network label overlaps the bars");
    }

    #[test]
    fn a_blocked_modem_keeps_its_switch_and_sim_only() {
        let v =
            main_view(Some(&connected()), true, true, true, Some(Card::Physical), Rates::default());
        assert_eq!(v.items.len(), 2);
        assert_eq!(texts(&v, 0)[1], "OFF");
    }

    #[test]
    fn a_missing_modem_is_restarting_once_it_has_been_seen() {
        assert_eq!(
            main_view(None, true, false, false, Some(Card::Physical), Rates::default()).message,
            "No modem found"
        );
        assert_eq!(
            main_view(None, true, false, true, Some(Card::Physical), Rates::default()).message,
            "Restarting modem"
        );
    }

    #[test]
    fn no_data_connection_says_so() {
        let v =
            main_view(Some(&rm520()), true, false, true, Some(Card::Physical), Rates::default());
        assert_eq!(texts(&v, 3), ["Not connected"]);
    }

    #[test]
    fn signal_words_have_three_steps() {
        assert_eq!(
            [-80.0, -90.0, -100.0, -105.0, -117.0].map(rsrp_word),
            ["excellent", "excellent", "good", "good", "weak"]
        );
        assert_eq!(
            [25.0, 20.0, 12.0, 10.0, 2.4].map(snr_word),
            ["excellent", "excellent", "good", "good", "poor"]
        );
    }

    #[test]
    fn totals_read_in_their_own_unit() {
        assert_eq!(
            [980, 27_060, 450_000, 14_200_000, 3_100_000_000].map(size),
            ["980 B", "27.1 KB", "450 KB", "14.2 MB", "3.1 GB"]
        );
    }

    #[test]
    fn speeds_read_in_their_own_unit() {
        assert_eq!(
            [980.0, 13_400.0, 1_400_000.0, 87_000_000.0].map(rate),
            ["980 B/s", "13 KB/s", "1.4 MB/s", "87 MB/s"]
        );
    }

    #[test]
    fn the_signal_page_names_the_serving_cell() {
        let mut m = connected();
        m.cells[0].pci = Some(332);
        let lines = page(Page::Signal, Some(&m), Some(Card::Physical), Rates::default());
        assert!(lines.contains(&pair("Band", "B1 2100 MHz")), "{lines:?}");
        assert!(lines.contains(&pair("PCI", "332")), "{lines:?}");
        assert!(lines.contains(&Line::Head("Neighbour cells".into())));
    }

    /// On the M.2 slot the SIM page opens on the switch; off it there is none.
    #[test]
    fn the_sim_page_leads_with_the_switch() {
        let on_m2 = page(Page::Sim, Some(&connected()), Some(Card::Physical), Rates::default());
        assert_eq!(on_m2[0], Line::Action("Switch to eSIM".into()));
        let on_esim = page(Page::Sim, Some(&connected()), Some(Card::Esim), Rates::default());
        assert_eq!(on_esim[0], Line::Action("Switch to physical SIM".into()));
        let usb = page(Page::Sim, Some(&connected()), None, Rates::default());
        assert!(!usb.iter().any(|l| matches!(l, Line::Action(_))));
    }

    /// The replies the RM520N-GL gave on 2026-10-24, on B20: two paths only.
    #[test]
    fn antenna_replies_parse() {
        let sets = antennas(
            "+QRSRP: -114,-115,-32768,-32768,LTE",
            "+QRSRQ: -12,-13,-32768,-32768,LTE",
            "+QSINR: 2,0,-32768,-32768,LTE",
        );
        assert_eq!(
            sets,
            [Antennas {
                rat: "LTE".into(),
                rsrp: [Some(-114), Some(-115), None, None],
                rsrq: [Some(-12), Some(-13), None, None],
                sinr: [Some(2), Some(0), None, None],
            }]
        );
        // Two radios in NSA, one line each; and noise around the reply.
        let nsa = parse_paths(
            "\r\n+QRSRP: -100,-101,-102,-103,LTE\r\n+QRSRP: -90,-91,-92,-93,NR5G\r\nOK",
            "+QRSRP:",
        );
        assert_eq!(nsa.len(), 2);
        assert_eq!(nsa[1].0, "NR5G");
        assert!(parse_paths("ERROR", "+QRSRP:").is_empty());
    }

    /// By connector, in the order they sit on the module. On a low band the two
    /// connectors without a path say so, rather than looking like broken antennas.
    #[test]
    fn unused_paths_on_a_low_band_say_so() {
        let mut m = connected();
        m.cells =
            vec![Cell { rat: Rat::Lte, serving: true, channel: Some(6300), ..Cell::default() }];
        m.antennas = antennas(
            "+QRSRP: -114,-115,-32768,-32768,LTE",
            "+QRSRQ: -12,-13,-32768,-32768,LTE",
            "+QSINR: 2,0,-32768,-32768,LTE",
        );
        let lines = page(Page::Signal, Some(&m), Some(Card::Physical), Rates::default());
        let ants: Vec<&Line> =
            lines.iter().skip_while(|l| **l != Line::Head("Antennas".into())).collect();
        assert_eq!(
            ants[1..],
            [
                &pair("4G B20", "RSRP / RSRQ / SINR"),
                &pair("ANT0 PRX", "-114 / -12 / 2"),
                &pair("ANT1", "not used on this band"),
                &pair("ANT2", "not used on this band"),
                &pair("ANT3 DRX", "-115 / -13 / 0"),
            ]
        );
    }

    /// Each band group puts the paths on its own connectors.
    #[test]
    fn paths_map_to_connectors_by_band() {
        let b = |name: &str, mhz| Band { name: name.into(), mhz };
        assert_eq!(connectors(&b("B20", 800)), [Some(0), Some(3), None, None]);
        assert_eq!(connectors(&b("B1", 2100)), [Some(0), Some(3), Some(1), Some(2)]);
        assert_eq!(connectors(&b("n41", 2500)), [Some(0), Some(3), Some(1), Some(2)]);
        assert_eq!(connectors(&b("n78", 3500)), [Some(2), Some(0), Some(1), Some(3)]);
        assert_eq!(connectors(&b("B42", 3500)), [Some(2), Some(0), Some(1), Some(3)]);
    }

    /// The RM520N-GL's ports as `busctl` showed the numbers and `mmcli` the names.
    #[test]
    fn port_types_are_mms() {
        let named: Vec<&str> = [6, 9, 5, 3, 3, 2].map(mm::port_type).to_vec();
        assert_eq!(named, ["qmi", "ignored", "gps", "at", "at", "net"]);
    }

    #[test]
    fn every_page_has_something_to_say() {
        for p in [Page::Device, Page::Sim, Page::Signal, Page::Network, Page::Config] {
            assert!(
                !page(p, Some(&connected()), Some(Card::Physical), Rates::default()).is_empty(),
                "{p:?}"
            );
        }
        assert_eq!(
            page(Page::Network, Some(&rm520()), Some(Card::Physical), Rates::default()),
            [Line::Centered("Not connected".into())]
        );
    }
}
