//! System information for the Settings and Network detail screens.
//!
//! The prototype gets all of this from its Node server, which shells out to `ip`,
//! `df`, `mmcli`, `qmicli` and `git`. Where the kernel exposes the same fact
//! directly this reads it directly: routes come from procfs and the disk from
//! `statvfs`, because spawning a process to learn the size of a filesystem is
//! silly. Where there is no interface but a tool, the tool is run, on a
//! background thread (see `watch`).

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

/// Run a command and return stdout, or None if it could not be run or failed.
///
/// Every failure means the same thing to a detail screen: the field is unknown and
/// shows as `--`. There is nothing useful to distinguish a missing binary from a
/// refused D-Bus call here.
///
/// Shared with the modules that ask the system the same way: the radios and the boot
/// menu's store discovery. It lived in three files before, identically.
pub(crate) fn output(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).stderr(Stdio::null()).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

fn read_i64(path: impl AsRef<Path>) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_str(path: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}

// ── Routing ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Route {
    /// "IPv4" or "IPv6", as the prototype labels them.
    pub family: &'static str,
    pub gateway: String,
    pub dev: String,
    pub metric: Option<i32>,
}

/// Default routes, newest kernel view.
///
/// The prototype runs `ip -4 route show default` and `ip -6 route show default`
/// and regexes `default via <gw> dev <dev> ... metric <n>`. Reading procfs
/// instead avoids two subprocesses on a 1-second poll: `/proc/net/route` carries
/// the v4 table and `/proc/net/ipv6_route` the v6 one, both in hex.
pub fn routes() -> Vec<Route> {
    let mut out = Vec::new();

    // /proc/net/route: Iface Destination Gateway Flags RefCnt Use Metric Mask ...
    // A default route is the one whose destination is 0.0.0.0. Values are
    // little-endian hex, which is why the octets come out reversed.
    if let Ok(text) = fs::read_to_string("/proc/net/route") {
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 8 || f[1] != "00000000" {
                continue;
            }
            let Ok(gw) = u32::from_str_radix(f[2], 16) else {
                continue;
            };
            let b = gw.to_le_bytes();
            out.push(Route {
                family: "IPv4",
                gateway: format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]),
                dev: f[0].to_string(),
                metric: f[6].parse().ok(),
            });
        }
    }

    // /proc/net/ipv6_route: dest plen src plen nexthop metric refcnt use flags dev
    // The default route has a zero destination and a zero prefix length.
    if let Ok(text) = fs::read_to_string("/proc/net/ipv6_route") {
        for line in text.lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[1] != "00" || f[0] != "00000000000000000000000000000000" {
                continue;
            }
            let Some(gw) = ipv6_from_hex(f[4]) else {
                continue;
            };
            // A default route with no next hop is not a gateway route.
            if gw == "::" {
                continue;
            }
            out.push(Route {
                family: "IPv6",
                gateway: gw,
                dev: f[9].to_string(),
                metric: i32::from_str_radix(f[5], 16).ok(),
            });
        }
    }
    out
}

/// 32 hex characters to a shortest-form IPv6 literal, per RFC 5952.
fn ipv6_from_hex(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut groups = [0u16; 8];
    for (i, g) in groups.iter_mut().enumerate() {
        *g = u16::from_str_radix(&hex[i * 4..i * 4 + 4], 16).ok()?;
    }
    Some(format_ipv6(&groups))
}

/// Collapse the longest run of zero groups, which is what makes an address
/// readable on a 256px panel.
pub fn format_ipv6(groups: &[u16; 8]) -> String {
    let mut best = (0usize, 0usize);
    let mut i = 0;
    while i < 8 {
        if groups[i] == 0 {
            let start = i;
            while i < 8 && groups[i] == 0 {
                i += 1;
            }
            if i - start > best.1 {
                best = (start, i - start);
            }
        } else {
            i += 1;
        }
    }
    // A single zero group is written out; only a run of two or more collapses.
    if best.1 < 2 {
        return groups.iter().map(|g| format!("{g:x}")).collect::<Vec<_>>().join(":");
    }
    let head: Vec<String> = groups[..best.0].iter().map(|g| format!("{g:x}")).collect();
    let tail: Vec<String> = groups[best.0 + best.1..].iter().map(|g| format!("{g:x}")).collect();
    format!("{}::{}", head.join(":"), tail.join(":"))
}

// ── Disk ───────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Disk {
    pub device: String,
    pub mounted: bool,
    pub used_gb: f32,
    pub total_gb: f32,
}

/// Usage of the filesystem on `device`.
///
/// The prototype runs `df -k <device>`. `statvfs` is the same numbers without the
/// subprocess, but it takes a path rather than a device, so the mount point comes
/// out of /proc/mounts first. Used is total minus free-to-root, matching how df
/// computes it, not total minus available.
/// Usage of the filesystem mounted at `point`.
pub fn disk_at(point: &str) -> Disk {
    let mut d = Disk { device: point.to_string(), ..Default::default() };
    let Ok(c) = std::ffi::CString::new(point) else {
        return d;
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return d;
    }
    let frsize = st.f_frsize as f64;
    let total = st.f_blocks as f64 * frsize;
    // Used is total minus free-to-root, which is how df computes it, rather than
    // total minus available.
    let used = (st.f_blocks - st.f_bfree) as f64 * frsize;
    // Decimal, as a desktop's own tools and the drive's label both use: 1 GB is
    // 10^9 bytes.
    const GB: f64 = 1e9;
    d.mounted = st.f_blocks > 0;
    d.total_gb = ((total / GB) * 10.0).round() as f32 / 10.0;
    d.used_gb = ((used / GB) * 10.0).round() as f32 / 10.0;
    d
}

/// The device backing a mount point, from /proc/mounts.
pub fn device_at(point: &str) -> Option<String> {
    let mounts = fs::read_to_string("/proc/mounts").ok()?;
    mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        let dev = f.next()?;
        let at = f.next()?;
        (at.replace("\\040", " ") == point).then(|| dev.to_string())
    })
}

/// The largest partition of `disk`, which is the data partition on a card whose
/// other partitions are boot and metadata.
///
/// The prototype hardcodes `/dev/mmcblk0p2`, which on this board is the 4MB
/// metadata partition rather than the 29.7GB data one, so a mounted card would be
/// reported as 4MB. Picking by size gets the partition a person means.
pub fn largest_partition(disk: &str) -> Option<String> {
    let base = disk.rsplit('/').next()?;
    let dir = format!("/sys/class/block/{base}");
    let mut best: Option<(u64, String)> = None;
    for entry in fs::read_dir(&dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with(base) {
            continue;
        }
        // Size is in 512-byte sectors.
        let Some(sectors) = read_i64(entry.path().join("size")) else {
            continue;
        };
        let sectors = sectors as u64;
        if best.as_ref().is_none_or(|(b, _)| sectors > *b) {
            best = Some((sectors, format!("/dev/{name}")));
        }
    }
    best.map(|(_, n)| n)
}

pub fn disk(device: &str) -> Disk {
    let mut d = Disk { device: device.to_string(), ..Default::default() };
    let Ok(mounts) = fs::read_to_string("/proc/mounts") else {
        return d;
    };
    let Some(point) = mounts.lines().find_map(|l| {
        let mut f = l.split_whitespace();
        (f.next()? == device).then(|| f.next().map(|s| s.to_string()))?
    }) else {
        return d;
    };

    // Mount points in /proc/mounts escape spaces as \040.
    let point = point.replace("\\040", " ");
    let Ok(c) = std::ffi::CString::new(point) else {
        return d;
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return d;
    }
    let frsize = st.f_frsize as f64;
    let total = st.f_blocks as f64 * frsize;
    let used = (st.f_blocks - st.f_bfree) as f64 * frsize;
    // Decimal, as a desktop's own tools and the drive's label both use: 1 GB is
    // 10^9 bytes.
    const GB: f64 = 1e9;
    d.mounted = true;
    // One decimal, as the prototype's toFixed(1) gives.
    d.total_gb = ((total / GB) * 10.0).round() as f32 / 10.0;
    d.used_gb = ((used / GB) * 10.0).round() as f32 / 10.0;
    d
}

// ── Battery ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Battery {
    pub available: bool,
    /// "Charging", "Discharging", "Full", ... straight from sysfs.
    pub status: String,
    pub capacity: Option<i32>,
    /// Volts, amps and watts, each to three decimals as the prototype reports.
    pub voltage: Option<f32>,
    pub current: Option<f32>,
    pub power: Option<f32>,
    /// Amp-hours.
    pub charge_now: Option<f32>,
    pub charge_full: Option<f32>,
    pub time_to_empty: Option<i64>,
    pub time_to_full: Option<i64>,
    /// Tenths of a degree Celsius, as sysfs reports it.
    pub temp: Option<i64>,
}

pub fn battery() -> Battery {
    let Some(p) = crate::status::battery_dir() else {
        return Battery::default();
    };
    let micro = |name: &str| read_i64(p.join(name)).map(|v| v as f32 / 1_000_000.0);
    let v = micro("voltage_now");
    let i = micro("current_now");
    Battery {
        available: true,
        status: read_str(p.join("status")).unwrap_or_else(|| "Unknown".into()),
        capacity: read_i64(p.join("capacity")).map(|v| v as i32),
        voltage: v,
        current: i,
        // power_now if the gauge reports it, else V*I, which is what the
        // prototype falls back to.
        power: micro("power_now").or_else(|| match (v, i) {
            (Some(v), Some(i)) => Some(v * i),
            _ => None,
        }),
        charge_now: micro("charge_now"),
        charge_full: micro("charge_full"),
        time_to_empty: read_i64(p.join("time_to_empty_now")),
        time_to_full: read_i64(p.join("time_to_full_now")),
        temp: read_i64(p.join("temp")),
    }
}

/// Seconds as the prototype's `fmtTime`: hours and minutes, or minutes and
/// seconds, or seconds.
pub fn fmt_time(sec: Option<i64>) -> String {
    let Some(sec) = sec.filter(|s| *s >= 0) else {
        return "--".into();
    };
    let (h, m, s) = (sec / 3600, (sec % 3600) / 60, sec % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

// ── Modem ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Modem {
    pub available: bool,
    pub model: String,
    pub operator: String,
    pub state: String,
    /// Access technologies as ModemManager reports them, e.g. ["lte"].
    pub access_tech: Vec<String>,
    pub signal_quality: Option<i32>,
    pub ip4: String,
    pub iface: String,
    /// Cell identity, from qmicli's serving-system and cell-location views.
    pub cell_id: String,
    pub tac: String,
    pub mcc: String,
    pub mnc: String,
    pub pci: String,
    pub earfcn: String,
    pub band: String,
    pub rsrp: String,
    pub rsrq: String,
    pub sinr: String,
    /// Negotiated link rates in Mbit/s.
    pub dl_mbps: Option<f32>,
    pub ul_mbps: Option<f32>,
}

/// The value of a `"key": ...` pair in a flat-ish JSON blob.
///
/// mmcli's `-J` output is nested, but every field this screen wants has a unique
/// key name, so a scan for the key beats carrying a JSON parser. Values are
/// returned verbatim minus surrounding quotes; `--` and empty string both become
/// empty, since mmcli uses `--` for "not applicable".
fn json_value(src: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":");
    let mut at = src.find(&needle)? + needle.len();
    let bytes = src.as_bytes();
    while at < bytes.len() && (bytes[at] as char).is_whitespace() {
        at += 1;
    }
    let rest = &src[at..];
    let raw = if rest.starts_with('"') {
        let end = rest[1..].find('"')? + 1;
        rest[1..end].to_string()
    } else {
        let end = rest.find([',', '}', ']', '\n']).unwrap_or(rest.len());
        rest[..end].trim().to_string()
    };
    (raw != "--" && !raw.is_empty() && raw != "null").then_some(raw)
}

/// `json_value`, but only searching after `scope`.
///
/// Needed because a few key names are not unique in mmcli's output: `value`
/// appears under both signal-quality and other blocks, and `address` under both
/// ipv4-config and ipv6-config. Anchoring on the parent key first is enough to
/// disambiguate without a real parser.
fn json_value_in(src: &str, scope: &str, key: &str) -> Option<String> {
    let at = src.find(&format!("\"{scope}\""))?;
    json_value(&src[at..], key)
}

/// The strings of a `"key": [...]` array.
fn json_array(src: &str, key: &str) -> Vec<String> {
    let needle = format!("\"{key}\":");
    let Some(at) = src.find(&needle) else {
        return Vec::new();
    };
    let rest = &src[at + needle.len()..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    let Some(close) = rest[open..].find(']') else {
        return Vec::new();
    };
    rest[open + 1..open + close]
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty() && s != "--")
        .collect()
}

/// The first capture of a pattern shaped `<label><ws>'<value>'`.
///
/// qmicli prints human-readable output, not JSON, so the prototype regexes it.
/// Every field it wants has the same shape, so one scan for the label followed by
/// the next quoted run replaces the regex engine.
fn quoted_after(src: &str, label: &str) -> String {
    let Some(at) = src.find(label) else {
        return String::new();
    };
    let rest = &src[at + label.len()..];
    let Some(open) = rest.find('\'') else {
        return String::new();
    };
    let Some(close) = rest[open + 1..].find('\'') else {
        return String::new();
    };
    rest[open + 1..open + 1 + close].to_string()
}

/// Negotiated speed of `iface`, from sysfs rather than ethtool.
fn link_mbps(iface: &str) -> Option<f32> {
    read_i64(format!("/sys/class/net/{iface}/speed")).map(|v| v as f32)
}

/// The modem NetworkManager can see, as a ModemManager index.
///
/// Asked rather than assumed: the old code read modem 0, which is the only modem on
/// this board and the wrong one, or none at all, anywhere else. NetworkManager is
/// asked first because it is the answer to "is there a modem at all" and costs one
/// call; the index comes from ModemManager, which is the only place it exists.
fn modem_index() -> Option<String> {
    use std::sync::{Mutex, OnceLock};
    static HELD: OnceLock<Mutex<Option<(std::time::Instant, Option<String>)>>> = OnceLock::new();
    let held = HELD.get_or_init(|| Mutex::new(None));
    if let Ok(slot) = held.lock() {
        if let Some((at, found)) = slot.as_ref() {
            if at.elapsed() < NM_FRESH {
                return found.clone();
            }
        }
    }
    let found = read_modem_index();
    if let Ok(mut slot) = held.lock() {
        *slot = Some((std::time::Instant::now(), found.clone()));
    }
    found
}

/// Which modem ModemManager has, asked of NetworkManager first.
///
/// Two subprocesses, so it is remembered for as long as the rest of what only NM
/// knows: a modem does not appear twice a second, and the page that shows one polls
/// twice a second.
fn read_modem_index() -> Option<String> {
    let devices = output(&["nmcli", "-t", "-f", "DEVICE,TYPE", "device"])?;
    let present = devices
        .lines()
        .filter_map(|l| l.split_once(':'))
        .any(|(_, kind)| matches!(kind, "gsm" | "cdma" | "wwan"));
    if !present {
        return None;
    }
    // `mmcli -L` lists object paths; the trailing number is what -m takes.
    let listed = output(&["mmcli", "-L"])?;
    listed
        .split_whitespace()
        .find(|w| w.contains("/Modem/"))
        .and_then(|path| path.rsplit('/').next())
        .map(str::to_string)
}

pub fn modem(qmi_dev: &str) -> Modem {
    let mut m = Modem::default();
    let Some(index) = modem_index() else {
        return m;
    };
    let Some(mm) = output(&["mmcli", "-m", &index, "-J"]) else {
        return m;
    };
    m.available = true;
    m.model = json_value(&mm, "model").unwrap_or_default();
    m.operator = json_value(&mm, "operator-name").unwrap_or_default();
    m.state = json_value(&mm, "state").unwrap_or_default();
    m.access_tech = json_array(&mm, "access-technologies");
    m.signal_quality = json_value_in(&mm, "signal-quality", "value").and_then(|v| v.parse().ok());

    // The newest bearer carries the active connection's addresses.
    if let Some(path) = json_array(&mm, "bearers").last() {
        if let Some(num) = path.rsplit('/').next() {
            if let Some(b) = output(&["mmcli", "-b", num, "-J"]) {
                m.ip4 = json_value_in(&b, "ipv4-config", "address").unwrap_or_default();
                m.iface = json_value(&b, "interface").unwrap_or_default();
            }
        }
    }
    if !m.iface.is_empty() {
        m.dl_mbps = link_mbps(&m.iface);
        m.ul_mbps = m.dl_mbps;
    }

    // Cell identity. These three calls are why the whole struct is refreshed on a
    // background thread: each opens the QMI device and takes ~100ms.
    let qmi = |arg: &str| {
        output(&["qmicli", "-d", qmi_dev, "--device-open-proxy", arg]).unwrap_or_default()
    };
    let ss = qmi("--nas-get-serving-system");
    m.cell_id = quoted_after(&ss, "3GPP cell ID:");
    m.tac = quoted_after(&ss, "LTE tracking area code:");
    m.mcc = quoted_after(&ss, "MCC:");
    m.mnc = quoted_after(&ss, "MNC:");

    let cl = qmi("--nas-get-cell-location-info");
    m.pci = quoted_after(&cl, "Serving Cell ID:");
    // The channel number's parenthetical is the band, e.g. "1850" (Band 3).
    if let Some(at) = cl.find("EUTRA Absolute RF Channel Number:") {
        let rest = &cl[at..];
        m.earfcn = quoted_after(rest, "EUTRA Absolute RF Channel Number:");
        if let (Some(o), Some(c)) = (rest.find('('), rest.find(')')) {
            if o < c {
                m.band = rest[o + 1..c].to_string();
            }
        }
    }

    let sig = qmi("--nas-get-signal-strength");
    m.rsrp = quoted_after(&sig, "RSRP:");
    m.rsrq = quoted_after(&sig, "RSRQ:");
    m.sinr = quoted_after(&sig, "SINR");
    m
}

/// Bars 0..=5 from a signal quality percentage, as the modem screen shows them.
pub fn signal_bars(quality: Option<i32>) -> i32 {
    match quality {
        None => 0,
        Some(q) => match q {
            0 => 0,
            1..=20 => 1,
            21..=40 => 2,
            41..=60 => 3,
            61..=80 => 4,
            _ => 5,
        },
    }
}

// ── Update ─────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpdateStatus {
    /// None while the check is still running, which is a distinct state from
    /// "checked, nothing available".
    pub checked: bool,
    pub available: bool,
    pub current_commit: String,
    /// One line per commit between HEAD and the remote branch.
    pub commits: Vec<String>,
    pub error: String,
}

/// Compare the deployed checkout against its remote branch.
///
/// Same three git calls the prototype's `getUpdateStatus` makes, and the same
/// three failure messages, because they are the ones that tell the difference
/// between "not a repo", "no internet" and "cannot compare".
pub fn update_check(repo: &str, branch: &str) -> UpdateStatus {
    let mut u = UpdateStatus { checked: true, ..Default::default() };
    let git = |args: &[&str]| {
        let mut v = vec!["git", "-C", repo];
        v.extend_from_slice(args);
        output(&v)
    };
    match git(&["log", "--oneline", "-1"]) {
        Some(s) => u.current_commit = s.trim().to_string(),
        None => {
            u.error = "Cannot read local repo".into();
            return u;
        }
    }
    if git(&["fetch", "origin", branch]).is_none() {
        u.error = "No internet".into();
        return u;
    }
    match git(&["log", "--oneline", &format!("HEAD..origin/{branch}")]) {
        Some(log) => {
            let lines: Vec<String> = log.lines().map(|l| l.to_string()).collect();
            u.available = !lines.is_empty();
            u.commits = lines;
        }
        None => u.error = "Cannot compare branches".into(),
    }
    u
}

/// Fast-forward the checkout, then restart the unit that runs it.
///
/// The restart is detached into a transient unit for the same reason the reboot
/// is: systemd tearing this process down must not kill the command mid-flight.
pub fn update_apply(repo: &str, branch: &str, unit: &str) -> Result<(), String> {
    let out = Command::new("git")
        .args(["-C", repo, "pull", "--ff-only", "origin", branch])
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().next().unwrap_or("pull failed").to_string());
    }
    crate::system::spawn_transient(&format!("systemctl daemon-reload && systemctl restart {unit}"));
    Ok(())
}

// ── Ethernet ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Iface {
    /// Kernel name, e.g. `end0` or `flipusb0`.
    pub name: String,
    /// True only with a carrier: an interface that is up but unplugged is not a
    /// connection, and the prototype's `isConnected` says the same.
    pub connected: bool,
    /// Negotiated link rate in Mbit/s, or None when down.
    pub speed: Option<i64>,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub ipv4: Vec<String>,
    pub ipv6: Vec<String>,
    /// NetworkManager's `ipv4.method`: auto, manual, shared, link-local.
    pub method: String,
    pub gateway: String,
    pub dns: Vec<String>,
    pub mac: String,
    pub mtu: Option<i64>,
}

/// The wired interfaces this machine actually has, in name order.
///
/// Found rather than listed. The prototype names `end0`, `end1` and `flipusb0`
/// outright, which is right for one board and wrong everywhere else: on a desktop
/// the port is `ens18` or `enp3s0`, and a fixed list shows an empty page.
///
/// What is wanted is the hardware, and `is_hardware` is exactly that question.
/// Wireless is left out because it has a page of its own, and the modem's wwan
/// because it is not an ethernet port.
pub fn eth_ifaces() -> Vec<String> {
    let mut names: Vec<String> = hardware_ifaces()
        .into_iter()
        .filter(|name| {
            !Path::new(&format!("/sys/class/net/{name}/wireless")).is_dir()
                && !name.starts_with("wwan")
        })
        .collect();
    names.sort();
    names
}

/// Every interface with hardware behind it, in name order.
///
/// `/sys/class/net/<name>/device` is the test: a bridge, a veth, a docker interface,
/// a VPN tun and loopback have no device, and a real port does. The idle screen and
/// the Ethernet page both start here, so neither can drift into listing a machine's
/// container plumbing as its network.
pub fn hardware_ifaces() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .filter(|e| e.path().join("device").exists())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

/// Display name, from `ifaceDisplayName`: `end0` reads as ETH0 and the USB gadget
/// as USB ETH, because "flipusb0" means nothing to a person holding the device.
pub fn iface_display_name(name: &str) -> String {
    if name.starts_with("flipusb") {
        return "USB ETH".into();
    }
    match name.strip_prefix("end") {
        Some(n) => format!("ETH{n}"),
        None => name.to_uppercase(),
    }
}

/// One nmcli call for every interface's addressing method.
///
/// `nmcli -t -f DEVICE,NAME connection show --active` pairs devices with
/// connection names, and the method is a per-connection setting, so this is two
/// calls total rather than two per interface.
fn ipv4_methods() -> Vec<(String, String)> {
    let Some(active) =
        output(&["nmcli", "-t", "-f", "DEVICE,NAME", "connection", "show", "--active"])
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in active.lines() {
        let Some((dev, conn)) = line.split_once(':') else {
            continue;
        };
        if dev.is_empty() || dev == "lo" {
            continue;
        }
        let method = output(&["nmcli", "-t", "-f", "ipv4.method", "connection", "show", conn])
            .and_then(|s| {
                s.lines().next().and_then(|l| l.split_once(':')).map(|(_, v)| v.trim().to_string())
            })
            .unwrap_or_default();
        out.push((dev.to_string(), method));
    }
    out
}

/// Addresses, gateways and resolvers for one interface, from
/// `nmcli -t device show <name>`.
///
/// This has to come from NetworkManager rather than the system: on a
/// systemd-resolved host /etc/resolv.conf names the local stub (127.0.0.53), not
/// the servers actually in use, so reading it reports the wrong DNS. NM knows the
/// real ones per interface, which is why the prototype asks it.
///
/// Returns (state, ipv4, ipv6, gateway4, dns). Addresses arrive with a prefix
/// length attached and are stripped, as `stripCidr` does.
fn nmcli_device(name: &str) -> Option<(String, Vec<String>, Vec<String>, String, Vec<String>)> {
    let text = output(&["nmcli", "-t", "device", "show", name])?;
    let mut state = String::new();
    let (mut v4, mut v6, mut dns) = (Vec::new(), Vec::new(), Vec::new());
    let mut gw4 = String::new();
    for line in text.lines() {
        // A value may itself contain a colon (an IPv6 address does), so split once
        // and keep the rest.
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let val = val.trim();
        if val.is_empty() {
            continue;
        }
        let bare = || val.split('/').next().unwrap_or(val).to_string();
        match key {
            "GENERAL.STATE" => state = val.to_string(),
            "IP4.GATEWAY" => gw4 = bare(),
            k if k.starts_with("IP4.ADDRESS") => v4.push(bare()),
            k if k.starts_with("IP6.ADDRESS") => v6.push(bare()),
            k if k.starts_with("IP4.DNS") || k.starts_with("IP6.DNS") => dns.push(val.to_string()),
            _ => {}
        }
    }
    Some((state, v4, v6, gw4, dns))
}

/// What only NetworkManager knows, kept for a while.
///
/// The method, the gateway and the resolvers come from NM, and every question costs a
/// process that NM and dbus answer: the Ethernet page polls twice a second, and with
/// two interfaces and two connections that was five `nmcli` runs every two seconds,
/// measured on the device as 8% of a core in NetworkManager and 9% in dbus.
///
/// None of those three changes on the timescale the page redraws at, so they are asked
/// for at most this often and remembered in between. Everything else on that page is
/// read from the kernel every tick, so addresses, counters and carrier stay live.
const NM_FRESH: std::time::Duration = std::time::Duration::from_secs(10);

type NmFacts = std::collections::HashMap<String, (String, String, Vec<String>)>;

fn nm_facts() -> NmFacts {
    use std::sync::{Mutex, OnceLock};
    static HELD: OnceLock<Mutex<Option<(std::time::Instant, NmFacts)>>> = OnceLock::new();
    let held = HELD.get_or_init(|| Mutex::new(None));
    let Ok(mut slot) = held.lock() else {
        return NmFacts::new();
    };
    if let Some((at, facts)) = slot.as_ref() {
        if at.elapsed() < NM_FRESH {
            return facts.clone();
        }
    }
    let methods = ipv4_methods();
    let mut facts = NmFacts::new();
    for name in eth_ifaces() {
        let (_, _, _, gateway, dns) = nmcli_device(&name).unwrap_or_default();
        let method = methods
            .iter()
            .find(|(dev, _)| *dev == name)
            .map(|(_, m)| m.clone())
            .unwrap_or_default();
        facts.insert(name, (method, gateway, dns));
    }
    *slot = Some((std::time::Instant::now(), facts.clone()));
    facts
}

pub fn ethernet() -> Vec<Iface> {
    let facts = nm_facts();
    let mut out = Vec::new();
    for name in eth_ifaces() {
        let name = name.as_str();
        let dir = format!("/sys/class/net/{name}");
        // carrier is 1 only with a live link. operstate alone says "up" for a
        // configured-but-unplugged port.
        let connected = read_i64(format!("{dir}/carrier")) == Some(1);
        // Addresses from the kernel rather than from NM: they are the same addresses,
        // they cost nothing to read, and an unmanaged interface has them either way,
        // which is the normal state for the USB gadget.
        let ipv4 = crate::status::ipv4_all(name);
        let ipv6 = crate::status::ipv6_all(name);
        let (method, gateway, dns) = facts.get(name).cloned().unwrap_or_default();
        out.push(Iface {
            connected,
            speed: connected.then(|| read_i64(format!("{dir}/speed"))).flatten(),
            rx_bytes: read_i64(format!("{dir}/statistics/rx_bytes")).unwrap_or(0) as u64,
            tx_bytes: read_i64(format!("{dir}/statistics/tx_bytes")).unwrap_or(0) as u64,
            ipv4,
            ipv6,
            method,
            gateway,
            dns,
            mac: read_str(format!("{dir}/address")).unwrap_or_default(),
            mtu: read_i64(format!("{dir}/mtu")),
            name: name.to_string(),
        });
    }
    out
}

/// A byte count as the prototype's `formatBytes`: three significant figures, with
/// the decimals dropped once the number is big enough not to need them.
///
/// Decimal steps, not the prototype's 1024: it divided by 1024 and labelled the
/// result KB, MB, GB, which reads as a different number from the one a desktop shows
/// for the same counter. The labels were always the decimal ones; now the arithmetic
/// matches them.
pub fn format_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut num = n as f64;
    let mut u = 0;
    while num >= 1000.0 && u < UNITS.len() - 1 {
        num /= 1000.0;
        u += 1;
    }
    let s = if num >= 100.0 || u == 0 {
        format!("{num:.0}")
    } else if num >= 10.0 {
        format!("{num:.1}")
    } else {
        format!("{num:.2}")
    };
    format!("{s} {}", UNITS[u])
}

/// A link rate as `formatSpeed`: megabits until it reaches a gigabit.
pub fn format_speed(mbps: Option<i64>) -> String {
    let Some(mb) = mbps.filter(|v| *v > 0) else {
        return String::new();
    };
    if mb >= 1000 {
        let gb = mb as f64 / 1000.0;
        if gb.fract() == 0.0 {
            format!("{gb:.0}Gb/s")
        } else {
            format!("{gb:.1}Gb/s")
        }
    } else {
        format!("{mb}Mb/s")
    }
}

/// NetworkManager's method name as the page labels it, from `formatDhcp`.
pub fn format_method(method: &str) -> String {
    match method {
        "auto" => "DHCP Client".into(),
        "manual" => "Static".into(),
        "shared" => "DHCP Server".into(),
        "link-local" => "Link-local".into(),
        "disabled" | "" => String::new(),
        other => other.into(),
    }
}

// ── System info ─────────────────────────────────────────────────────────────

/// What Settings > System info shows: what the machine is, what it runs, its
/// storage, its radios and their addresses, and how long it has been up.
///
/// All of it from files and two ioctls a user may make, with no tool run: it is read
/// every couple of seconds on a thread, and most of it never changes.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct System {
    pub model: String,
    pub hostname: String,
    pub soc: String,
    pub cpu: String,
    /// The serial U-Boot derives from the OTP, the one in the SSH banner and the
    /// Wi-Fi access point's SSID.
    pub serial: String,
    pub mem_used: u64,
    /// What is fitted, from the device tree, rather than what the kernel was left
    /// with: `MemTotal` is short by the firmware's carve-out and the kernel's own.
    pub mem_total: u64,
    pub os: String,
    pub build: String,
    pub git: String,
    pub profile: String,
    /// When the running profile was made, from its root's birth time.
    pub installed: String,
    pub kernel: String,
    pub kernel_built: String,
    pub ufs: Option<Ufs>,
    /// The card in the slot, or None with the slot empty.
    pub sd: Option<SdCard>,
    pub pack: Option<Pack>,
    /// What each radio is, as (kind, what): `("Wi-Fi", "MediaTek mt7921u")`.
    pub radios: Vec<(String, String)>,
    /// The hardware addresses, as (what, address).
    pub macs: Vec<(String, String)>,
    pub uptime: String,
}

/// The UFS the system runs from.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ufs {
    pub name: String,
    pub size: String,
    pub version: String,
    pub link: String,
    /// How much of its rated life is used, the worse of the two estimates.
    pub life: String,
    pub health: String,
}

pub fn system() -> System {
    let release = read_str("/etc/os-release").unwrap_or_default();
    let build = os_release_value(&release, "BUILD_ID").unwrap_or_default();
    let (mem_total, mem_used) = memory();
    let version = read_str("/proc/version").unwrap_or_default();
    System {
        model: dt_string("model"),
        hostname: read_str("/proc/sys/kernel/hostname").unwrap_or_default(),
        soc: soc(),
        cpu: cpu_summary(&clusters()),
        serial: read_cpuid().map(|id| cpu_serial(&id)).unwrap_or_default(),
        mem_used,
        mem_total,
        os: os_release_value(&release, "PRETTY_NAME")
            .map(|name| {
                // Debian's name carries the major version only; the point release is
                // in its own key and is the one worth reading.
                match os_release_value(&release, "DEBIAN_VERSION_FULL") {
                    Some(full) => name.replacen(
                        &format!(
                            " {} ",
                            os_release_value(&release, "VERSION_ID").unwrap_or_default()
                        ),
                        &format!(" {full} "),
                        1,
                    ),
                    None => name,
                }
            })
            .unwrap_or_default(),
        build,
        git: os_release_value(&release, "BUILD_GIT").unwrap_or_default(),
        profile: crate::status::booted_profile(),
        installed: birth("/").map(date).unwrap_or_default(),
        kernel: read_str("/proc/sys/kernel/osrelease").unwrap_or_default(),
        kernel_built: kernel_built(&version),
        ufs: ufs(),
        sd: sd_card(),
        pack: pack(),
        radios: radios(),
        macs: macs(),
        uptime: read_str("/proc/uptime")
            .and_then(|u| u.split_whitespace().next()?.parse::<f64>().ok())
            .map(|s| uptime(s as u64))
            .unwrap_or_default(),
    }
}

/// One key of an os-release file, unquoted.
pub fn os_release_value(text: &str, key: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let value = line.strip_prefix(key)?.strip_prefix('=')?;
        Some(value.trim().trim_matches('"').to_string())
    })
}

/// A device tree property as text, with its terminating NUL.
fn dt_string(name: &str) -> String {
    fs::read(Path::new("/proc/device-tree").join(name))
        .map(|b| String::from_utf8_lossy(&b).trim_end_matches('\0').to_string())
        .unwrap_or_default()
}

/// The SoC, from the last entry of the board's `compatible`: `rockchip,rk3576`.
fn soc() -> String {
    let compatible = dt_string("compatible");
    let Some((vendor, part)) = compatible.split('\0').last().and_then(|c| c.split_once(',')) else {
        return String::new();
    };
    let mut vendor = vendor.to_string();
    if let Some(first) = vendor.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    format!("{vendor} {}", part.to_uppercase())
}

/// ARM's part numbers for the cores a board like this carries.
fn core_name(part: u32) -> String {
    match part {
        0xd03 => "A53".into(),
        0xd04 => "A35".into(),
        0xd05 => "A55".into(),
        0xd07 => "A57".into(),
        0xd08 => "A72".into(),
        0xd09 => "A73".into(),
        0xd0a => "A75".into(),
        0xd0b => "A76".into(),
        0xd0d => "A77".into(),
        0xd41 => "A78".into(),
        0xd46 => "A510".into(),
        0xd47 => "A710".into(),
        0xd4d => "A715".into(),
        other => format!("{other:#x}"),
    }
}

/// One frequency domain: what its cores are, how many, and their top speed in kHz.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cluster {
    pub core: String,
    pub count: usize,
    pub max_khz: u64,
}

/// The cores by cpufreq policy, which is one per cluster on big.LITTLE.
fn clusters() -> Vec<Cluster> {
    let cpuinfo = read_str("/proc/cpuinfo").unwrap_or_default();
    let mut parts = std::collections::HashMap::new();
    let mut cpu = None;
    for line in cpuinfo.lines() {
        let Some((key, value)) = line.split_once(':') else { continue };
        match key.trim() {
            "processor" => cpu = value.trim().parse::<u32>().ok(),
            "CPU part" => {
                if let (Some(n), Ok(p)) =
                    (cpu, u32::from_str_radix(value.trim().trim_start_matches("0x"), 16))
                {
                    parts.insert(n, p);
                }
            }
            _ => {}
        }
    }
    let Ok(dir) = fs::read_dir("/sys/devices/system/cpu/cpufreq") else { return Vec::new() };
    let mut policies: Vec<_> = dir.flatten().map(|e| e.path()).collect();
    policies.sort();
    policies
        .iter()
        .filter_map(|p| {
            let cpus: Vec<u32> = read_str(p.join("affected_cpus"))?
                .split_whitespace()
                .filter_map(|c| c.parse().ok())
                .collect();
            let part = cpus.first().and_then(|c| parts.get(c)).copied()?;
            Some(Cluster {
                core: core_name(part),
                count: cpus.len(),
                max_khz: read_u64(p.join("cpuinfo_max_freq")).unwrap_or(0),
            })
        })
        .collect()
}

fn read_u64(path: impl AsRef<Path>) -> Option<u64> {
    read_str(path)?.parse().ok()
}

/// `4x A53 2.0GHz, 4x A72 2.2GHz`.
pub fn cpu_summary(clusters: &[Cluster]) -> String {
    clusters
        .iter()
        .map(|c| format!("{}x {} {:.1}GHz", c.count, c.core, c.max_khz as f64 / 1e6))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The OTP's cpuid, which the RK3576 keeps 16 bytes in at 0x0a. Readable by anyone.
fn read_cpuid() -> Option<[u8; 16]> {
    let otp = fs::read("/sys/bus/nvmem/devices/rockchip-otp0/nvmem").ok()?;
    otp.get(0x0a..0x1a)?.try_into().ok()
}

/// The serial U-Boot makes of the cpuid, as `rockchip_cpuid_set` does: a CRC-32 with
/// no complement over the odd bytes, then over the even ones seeded with the first,
/// printed high word first. Same as `rk3576_cpu_serial.sh` in the image.
pub fn cpu_serial(cpuid: &[u8; 16]) -> String {
    fn crc(mut crc: u32, bytes: impl Iterator<Item = u8>) -> u32 {
        for b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        crc
    }
    let low = crc(0, cpuid.iter().skip(1).step_by(2).copied());
    let high = crc(low, cpuid.iter().step_by(2).copied());
    format!("{high:08x}{low:08x}")
}

/// Installed and used, in bytes. Used is what is not available, the figure `free`
/// shows: cache that would be dropped on demand is not in use.
fn memory() -> (u64, u64) {
    let info = read_str("/proc/meminfo").unwrap_or_default();
    let kb = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key)?.split_whitespace().next()?.parse::<u64>().ok())
            .unwrap_or(0)
            * 1024
    };
    let usable = kb("MemTotal:");
    (installed().unwrap_or(usable), usable.saturating_sub(kb("MemAvailable:")))
}

/// The RAM the device tree describes: every `memory` node's ranges, summed.
fn installed() -> Option<u64> {
    let cells = |name: &str| {
        fs::read(Path::new("/proc/device-tree").join(name))
            .ok()
            .and_then(|b| Some(u32::from_be_bytes(b.get(..4)?.try_into().ok()?) as usize))
    };
    let (addr, size) = (cells("#address-cells").unwrap_or(2), cells("#size-cells").unwrap_or(2));
    let total: u64 = fs::read_dir("/proc/device-tree")
        .ok()?
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("memory"))
        .filter_map(|e| fs::read(e.path().join("reg")).ok())
        .map(|reg| reg_size(&reg, addr, size))
        .sum();
    (total > 0).then_some(total)
}

/// The sizes in a `reg` property: (address, size) pairs of big-endian cells.
pub fn reg_size(reg: &[u8], address_cells: usize, size_cells: usize) -> u64 {
    let pair = (address_cells + size_cells) * 4;
    if pair == 0 {
        return 0;
    }
    reg.chunks_exact(pair)
        .map(|entry| {
            entry[address_cells * 4..].chunks_exact(4).fold(0u64, |acc, cell| {
                acc << 32 | u64::from(u32::from_be_bytes(cell.try_into().expect("4 bytes")))
            })
        })
        .sum()
}

/// `0.7/8 GB`: used to a tenth, and what is fitted in whole gigabytes, both binary,
/// the units RAM is sold in. The firmware's couple of megabytes below the ranges the
/// tree describes is why the total is rounded rather than truncated.
pub fn memory_text(used: u64, installed: u64) -> String {
    format!("{:.1}/{}", used as f64 / GIB, installed_text(installed))
}

/// What is fitted, in whole binary gigabytes: `8 GB`.
pub fn installed_text(installed: u64) -> String {
    format!("{:.0} GB", (installed as f64 / GIB).round())
}

const GIB: f64 = (1u64 << 30) as f64;

/// The build date of the kernel: the last six words of /proc/version, which end in
/// `Mon Sep 28 11:35:23 BST 2026`, read as `Sep 28 2026`.
pub fn kernel_built(version: &str) -> String {
    let words: Vec<&str> = version.split_whitespace().collect();
    match words.len().checked_sub(6).map(|at| &words[at..]) {
        Some([_, month, day, _, _, year]) if year.parse::<u32>().is_ok() => {
            format!("{month} {day} {year}")
        }
        _ => String::new(),
    }
}

/// A path's birth time, in seconds. For the root of a profile that is when its
/// subvolume was made: when it was installed, or cloned from another.
fn birth(path: &str) -> Option<i64> {
    let c = std::ffi::CString::new(path).ok()?;
    let mut st: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: c is NUL-terminated and st is a statx the call fills in.
    let rc = unsafe { libc::statx(libc::AT_FDCWD, c.as_ptr(), 0, libc::STATX_BTIME, &mut st) };
    (rc == 0 && st.stx_mask & libc::STATX_BTIME != 0).then_some(st.stx_btime.tv_sec)
}

/// `2026-09-30`, in UTC.
pub fn date(secs: i64) -> String {
    // Howard Hinnant's days-to-civil, the inverse of `boot::days_from_civil`.
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `5m`, `3h 47m`, or `2d 4h` past a day.
pub fn uptime(secs: u64) -> String {
    let (d, h, m) = (secs / 86_400, secs / 3600 % 24, secs / 60 % 60);
    match (d, h) {
        (0, 0) => format!("{m}m"),
        (0, _) => format!("{h}h {m}m"),
        _ => format!("{d}d {h}h"),
    }
}

/// An SD card, from what the kernel read of its CID, CSD and SD Status registers.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SdCard {
    pub name: String,
    pub size: String,
    /// SD, SDHC or SDXC.
    pub kind: String,
    /// What its SD Status says it is rated for: `C10 U3 V30 A2`.
    pub classes: String,
    pub made: String,
    pub serial: String,
    pub oem: String,
    pub rev: String,
}

fn sd_card() -> Option<SdCard> {
    let card = fs::read_dir("/sys/bus/mmc/devices")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| read_str(p.join("type")).as_deref() == Some("SD"))?;
    let attr = |f: &str| read_str(card.join(f)).unwrap_or_default();
    let bytes = fs::read_dir(card.join("block"))
        .ok()
        .and_then(|mut d| d.next()?.ok())
        .and_then(|b| read_u64(b.path().join("size")))
        .unwrap_or(0)
        * 512;
    let id = |hex: &str| u32::from_str_radix(hex.trim_start_matches("0x"), 16).unwrap_or(0);
    Some(SdCard {
        name: format!("{} {}", sd_maker(id(&attr("manfid"))), attr("name")).trim().to_string(),
        size: format_bytes(bytes),
        kind: sd_kind(bytes).into(),
        classes: sd_classes(&attr("ssr")),
        made: sd_date(&attr("date")),
        serial: attr("serial"),
        oem: sd_oem(id(&attr("oemid"))),
        rev: format!("hw {} fw {}", attr("hwrev"), attr("fwrev")),
    })
}

/// The manufacturer IDs the SD Association hands out, for the makers a person is
/// likely to hold. The list is not published, so an unknown one stays a number.
pub fn sd_maker(id: u32) -> String {
    match id {
        0x01 => "Panasonic".into(),
        0x02 => "Toshiba".into(),
        0x03 => "SanDisk".into(),
        0x1b => "Samsung".into(),
        0x1d => "ADATA".into(),
        0x27 => "Phison".into(),
        0x28 => "Lexar".into(),
        0x31 => "Silicon Power".into(),
        0x41 => "Kingston".into(),
        0x74 => "Transcend".into(),
        0x76 => "Patriot".into(),
        0x82 => "Sony".into(),
        other => format!("{other:#04x}"),
    }
}

/// The two OEM bytes are ASCII when a maker set them, `SM` or `PH`.
pub fn sd_oem(id: u32) -> String {
    let pair = [(id >> 8) as u8, id as u8];
    if pair.iter().all(|b| b.is_ascii_graphic()) {
        String::from_utf8_lossy(&pair).into_owned()
    } else {
        format!("{id:#06x}")
    }
}

/// The family a capacity puts a card in: SDHC up to 32GB, SDXC up to 2TB.
pub fn sd_kind(bytes: u64) -> &'static str {
    match bytes {
        0..=2_147_483_648 => "SD",
        ..=34_359_738_368 => "SDHC",
        ..=2_199_023_255_552 => "SDXC",
        _ => "SDUC",
    }
}

/// `05/2023` as the kernel prints the CID's date, as `2023-05`.
pub fn sd_date(date: &str) -> String {
    match date.split_once('/') {
        Some((month, year)) => format!("{year}-{month:0>2}"),
        None => date.to_string(),
    }
}

/// The ratings in the SD Status register, 512 bits as hex, bit 511 first:
/// SPEED_CLASS in 447:440, UHS_SPEED_GRADE in 399:396, VIDEO_SPEED_CLASS in 391:384
/// and APP_PERF_CLASS in 339:336.
pub fn sd_classes(ssr: &str) -> String {
    let byte = |at: usize| {
        ssr.get(at * 2..at * 2 + 2).and_then(|h| u8::from_str_radix(h, 16).ok()).unwrap_or(0)
    };
    let mut out = Vec::new();
    match byte(8) {
        1 => out.push("C2".to_string()),
        2 => out.push("C4".into()),
        3 => out.push("C6".into()),
        4 => out.push("C10".into()),
        _ => {}
    }
    if let u @ 1.. = byte(14) >> 4 {
        out.push(format!("U{u}"));
    }
    if let v @ 1.. = byte(15) {
        out.push(format!("V{v}"));
    }
    if let a @ 1.. = byte(21) & 0x0f {
        out.push(format!("A{a}"));
    }
    if out.is_empty() {
        "-".into()
    } else {
        out.join(" ")
    }
}

/// The battery pack: what it is and how worn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Pack {
    pub technology: String,
    pub capacity: Option<i32>,
    pub status: String,
    /// Full-charge and design capacity, in microamp-hours.
    pub full: Option<u64>,
    pub design: Option<u64>,
    pub cycles: Option<u64>,
    pub health: String,
}

/// The pack, by type rather than by name: its driver has been called both bq28z610
/// and bq28z620.
fn pack() -> Option<Pack> {
    let dir = fs::read_dir("/sys/class/power_supply")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| read_str(p.join("type")).as_deref() == Some("Battery"))?;
    let text = |f: &str| read_str(dir.join(f)).unwrap_or_default();
    let num = |f: &str| read_u64(dir.join(f));
    Some(Pack {
        technology: text("technology"),
        capacity: read_str(dir.join("capacity")).and_then(|c| c.parse().ok()),
        status: text("status"),
        full: num("charge_full"),
        design: num("charge_full_design"),
        cycles: num("cycle_count"),
        health: text("health"),
    })
}

/// The UFS host, if there is one, and the disk it serves.
fn ufs() -> Option<Ufs> {
    let host = fs::read_dir("/sys/devices/platform/soc")
        .ok()?
        .flatten()
        .map(|e| e.path())
        .find(|p| p.file_name().is_some_and(|n| n.to_string_lossy().ends_with(".ufshc")))?;
    let string = |d: &str, f: &str| read_str(host.join(d).join(f)).unwrap_or_default();
    let hex =
        |d: &str, f: &str| u32::from_str_radix(string(d, f).trim_start_matches("0x"), 16).ok();
    // The data LUN is the largest disk the host serves: the others are boot LUNs of a
    // few megabytes.
    let size = fs::read_dir("/sys/block")
        .ok()?
        .flatten()
        .filter(|e| {
            fs::canonicalize(e.path().join("device"))
                .is_ok_and(|d| d.starts_with(fs::canonicalize(&host).unwrap_or_default()))
        })
        .filter_map(|e| read_u64(e.path().join("size")))
        .max()
        .unwrap_or(0)
        * 512;
    let life = [
        hex("health_descriptor", "life_time_estimation_a"),
        hex("health_descriptor", "life_time_estimation_b"),
    ]
    .into_iter()
    .flatten()
    .max();
    Some(Ufs {
        name: format!(
            "{} {}",
            string("string_descriptors", "manufacturer_name"),
            string("string_descriptors", "product_name")
        )
        .trim()
        .to_string(),
        size: format_bytes(size),
        version: hex("device_descriptor", "specification_version")
            .map(ufs_version)
            .unwrap_or_default(),
        link: ufs_link(&string("power_info", "gear"), &string("power_info", "lane")),
        life: life.map(ufs_life).unwrap_or_default(),
        health: hex("health_descriptor", "eol_info").map(ufs_eol).unwrap_or_default(),
    })
}

/// `wSpecVersion`, BCD: 0x0220 is UFS 2.2, 0x0310 is 3.1.
pub fn ufs_version(bcd: u32) -> String {
    format!("UFS {}.{}", bcd >> 8, (bcd >> 4) & 0xf)
}

/// `HS_GEAR3` on 2 lanes, as `HS-G3 x2`.
pub fn ufs_link(gear: &str, lanes: &str) -> String {
    match gear.strip_prefix("HS_GEAR") {
        Some(n) if !lanes.is_empty() => format!("HS-G{n} x{lanes}"),
        Some(n) => format!("HS-G{n}"),
        None => gear.to_string(),
    }
}

/// JEDEC's device life time estimate: 0x01 is 0-10% of rated life used, each step
/// another tenth, 0x0B past it.
pub fn ufs_life(code: u32) -> String {
    match code {
        1..=10 => format!("{}-{}%", (code - 1) * 10, code * 10),
        11 => "past rated life".into(),
        _ => "-".into(),
    }
}

/// `bPreEOLInfo`: how much of the reserved blocks are spent.
pub fn ufs_eol(code: u32) -> String {
    match code {
        1 => "normal".into(),
        2 => "warning".into(),
        3 => "critical".into(),
        _ => "-".into(),
    }
}

/// The USB device a sysfs class device sits on, for its manufacturer's name.
fn usb_maker(class_dev: &Path) -> String {
    let Ok(mut at) = fs::canonicalize(class_dev.join("device")) else { return String::new() };
    for _ in 0..4 {
        if let Some(maker) = read_str(at.join("manufacturer")) {
            return maker;
        }
        if !at.pop() {
            break;
        }
    }
    String::new()
}

fn driver_of(class_dev: &Path) -> String {
    fs::read_link(class_dev.join("device/driver"))
        .ok()
        .and_then(|d| Some(d.file_name()?.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// What each radio is: its maker and the driver that runs it.
fn radios() -> Vec<(String, String)> {
    let describe = |dev: &Path| format!("{} {}", usb_maker(dev), driver_of(dev)).trim().to_string();
    let mut out = Vec::new();
    for name in hardware_ifaces() {
        let dev = Path::new("/sys/class/net").join(&name);
        if dev.join("wireless").is_dir() {
            out.push(("Wi-Fi".to_string(), describe(&dev)));
        }
    }
    if let Ok(dir) = fs::read_dir("/sys/class/bluetooth") {
        for e in dir.flatten().filter(|e| !e.file_name().to_string_lossy().contains(':')) {
            out.push(("Bluetooth".to_string(), describe(&e.path())));
        }
    }
    if let Ok(dir) = fs::read_dir("/sys/class/usbmisc") {
        for e in dir.flatten() {
            let product = fs::canonicalize(e.path().join("device"))
                .ok()
                .and_then(|d| read_str(d.parent()?.join("product")));
            out.push(("Modem".to_string(), product.unwrap_or_else(|| describe(&e.path()))));
        }
    }
    out
}

/// Every hardware address: the network ports by the names the other screens give
/// them, and Bluetooth's.
///
/// A wireless card's own address rather than what it is using: NetworkManager
/// randomises the one it scans with, so `address` changes and says nothing about the
/// card.
fn macs() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = hardware_ifaces()
        .into_iter()
        .map(|name| {
            let dev = Path::new("/sys/class/net").join(&name);
            let wireless = dev.join("wireless").is_dir();
            let label = if wireless { "WIFI".to_string() } else { iface_display_name(&name) };
            let address = wireless
                .then(|| permanent_mac(&name))
                .flatten()
                .or_else(|| read_str(dev.join("address")))
                .unwrap_or_default();
            (label, address)
        })
        .collect();
    if let Some(bt) = bluetooth_mac() {
        out.push(("BT".to_string(), bt));
    }
    out
}

fn mac(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

/// The address burned into a network card, from `ETHTOOL_GPERMADDR`.
fn permanent_mac(iface: &str) -> Option<String> {
    #[repr(C)]
    struct PermAddr {
        cmd: u32,
        size: u32,
        data: [u8; 32],
    }
    #[repr(C)]
    struct Ifreq {
        name: [u8; libc::IFNAMSIZ],
        data: *mut PermAddr,
        _pad: [u8; 16],
    }
    const ETHTOOL_GPERMADDR: u32 = 0x20;
    let mut addr = PermAddr { cmd: ETHTOOL_GPERMADDR, size: 32, data: [0; 32] };
    let mut req = Ifreq { name: [0; libc::IFNAMSIZ], data: &mut addr, _pad: [0; 16] };
    let name = iface.as_bytes();
    if name.len() >= libc::IFNAMSIZ {
        return None;
    }
    req.name[..name.len()].copy_from_slice(name);
    // SAFETY: a datagram socket for the ioctl, closed below.
    let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return None;
    }
    // SAFETY: req names the interface and points at addr, both live across the call.
    let rc = unsafe { libc::ioctl(fd, libc::SIOCETHTOOL, &mut req) };
    // SAFETY: fd is ours.
    unsafe { libc::close(fd) };
    let len = (addr.size as usize).min(addr.data.len());
    (rc == 0 && len > 0 && addr.data[..len].iter().any(|&b| b != 0)).then(|| mac(&addr.data[..len]))
}

/// hci0's address, from `HCIGETDEVINFO`, which any user may ask: sysfs does not
/// carry it.
fn bluetooth_mac() -> Option<String> {
    const BTPROTO_HCI: libc::c_int = 1;
    const HCIGETDEVINFO: libc::c_ulong = 0x8004_48d3;
    // dev_id, then the name, then the address little-endian, then the rest of
    // hci_dev_info, which is ignored here.
    let mut info = [0u8; 128];
    // SAFETY: a raw HCI socket for the ioctl, closed below.
    let fd = unsafe {
        libc::socket(libc::AF_BLUETOOTH, libc::SOCK_RAW | libc::SOCK_CLOEXEC, BTPROTO_HCI)
    };
    if fd < 0 {
        return None;
    }
    // SAFETY: info is larger than hci_dev_info and its dev_id, 0, is hci0.
    let rc = unsafe { libc::ioctl(fd, HCIGETDEVINFO as _, info.as_mut_ptr()) };
    // SAFETY: fd is ours.
    unsafe { libc::close(fd) };
    let mut addr = info[10..16].to_vec();
    addr.reverse();
    (rc == 0 && addr.iter().any(|&b| b != 0)).then(|| mac(&addr))
}

#[cfg(test)]
mod system_tests {
    use super::*;

    /// The pair `rk3576_cpu_serial.sh` printed on a Flipper One, which is also what
    /// its SSH banner and its AP's SSID say.
    #[test]
    fn the_serial_is_the_one_u_boot_makes() {
        let id: [u8; 16] = [0x4e, 0x59, 0x31, 0x37, 0x48, 0, 0, 0, 0, 0, 0, 0, 0, 0x16, 0x14, 0x0c];
        assert_eq!(cpu_serial(&id), "45d2488f32838acd");
    }

    #[test]
    fn os_release_values_are_unquoted() {
        let text =
            "PRETTY_NAME=\"Debian GNU/Linux 13 (trixie)\"\nBUILD_ID=1136\nBUILD_GIT=\"x y\"\n";
        assert_eq!(os_release_value(text, "BUILD_ID").as_deref(), Some("1136"));
        assert_eq!(os_release_value(text, "BUILD_GIT").as_deref(), Some("x y"));
        assert_eq!(os_release_value(text, "BUILD").as_deref(), None);
    }

    #[test]
    fn the_cpu_is_summed_up_by_cluster() {
        let clusters = [
            Cluster { core: "A53".into(), count: 4, max_khz: 2_016_000 },
            Cluster { core: "A72".into(), count: 4, max_khz: 2_208_000 },
        ];
        assert_eq!(cpu_summary(&clusters), "4x A53 2.0GHz, 4x A72 2.2GHz");
    }

    /// This board's own node: 8GiB less the 2MiB the firmware keeps, which is
    /// still 8 on the screen.
    #[test]
    fn the_installed_ram_is_what_the_tree_describes() {
        let reg = [0, 0, 0, 0, 0x40, 0x20, 0, 0, 0, 0, 0, 1, 0xff, 0xe0, 0, 0];
        assert_eq!(reg_size(&reg, 2, 2), 0x1_ffe0_0000);
        assert_eq!(memory_text(700 << 20, reg_size(&reg, 2, 2)), "0.7/8 GB");
        assert_eq!(reg_size(&[], 2, 2), 0);
    }

    #[test]
    fn the_kernel_build_date_is_read_off_proc_version() {
        let v = "Linux version 7.3.0-rc2-gd83595fdc473 (buildbot@host) (gcc 14.2.0) \
                 #1 SMP PREEMPT Mon Sep 28 11:35:23 BST 2026";
        assert_eq!(kernel_built(v), "Sep 28 2026");
        assert_eq!(kernel_built("garbage"), "");
    }

    #[test]
    fn dates_and_durations() {
        assert_eq!(date(1_790_768_081), "2026-09-30");
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(uptime(13_621), "3h 47m");
        assert_eq!(uptime(330), "5m");
        assert_eq!(uptime(2 * 86_400 + 4 * 3600 + 59), "2d 4h");
    }

    /// The classes sit where the SD spec puts them in the SD Status register, here
    /// a card rated C10 U3 V30 A2.
    #[test]
    fn an_sd_cards_ratings_come_from_its_status_register() {
        let mut ssr = vec![0u8; 64];
        ssr[8] = 4;
        ssr[14] = 0x30;
        ssr[15] = 30;
        ssr[21] = 0x02;
        let hex: String = ssr.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(sd_classes(&hex), "C10 U3 V30 A2");
        assert_eq!(sd_classes(&"0".repeat(128)), "-");
        assert_eq!(sd_classes(""), "-");
    }

    #[test]
    fn an_sd_cards_identity_reads_as_words() {
        assert_eq!(sd_maker(0x03), "SanDisk");
        assert_eq!(sd_maker(0x99), "0x99");
        assert_eq!(sd_oem(0x5344), "SD");
        assert_eq!(sd_oem(0x0000), "0x0000");
        assert_eq!(sd_date("05/2023"), "2023-05");
        assert_eq!(sd_date("5/2023"), "2023-05");
        assert_eq!(sd_kind(64_000_000_000), "SDXC");
        assert_eq!(sd_kind(16_000_000_000), "SDHC");
        assert_eq!(sd_kind(1_000_000_000), "SD");
    }

    #[test]
    fn the_ufs_descriptors_read_as_words() {
        assert_eq!(ufs_version(0x0220), "UFS 2.2");
        assert_eq!(ufs_version(0x0310), "UFS 3.1");
        assert_eq!(ufs_link("HS_GEAR3", "2"), "HS-G3 x2");
        assert_eq!(ufs_life(1), "0-10%");
        assert_eq!(ufs_life(10), "90-100%");
        assert_eq!(ufs_life(11), "past rated life");
        assert_eq!(ufs_eol(1), "normal");
    }
}
