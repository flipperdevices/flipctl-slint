//! What the Wi-Fi page reads and what its rows do, all through nmcli.
//!
//! `net` already carries the radio state the status bar and the Network menu need:
//! whether the radio is on, whether it is associated and to what. This is the rest
//! of it, and the reason it is a separate module is that none of it is ambient.
//! Nothing here is read unless the page is open: a scan list, the saved profiles
//! and one profile's settings are three sets of subprocesses, and the prototype
//! starts them in `enter()` and stops them in `exit()`.
//!
//! The same division as `net`: reads happen on a `Watch` thread at the cadence
//! server.js polls at, and writes are one-shot commands a caller runs on a thread
//! of its own because `nmcli device wifi connect` can take twenty-five seconds.

use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::font::TITLE;
use crate::sysinfo::output;
use crate::theme::{count, metric};
use crate::watch::Watch;

/// How often the scan list is re-read, from server.js's `wifi-scan` loop.
const SCAN_EVERY: Duration = Duration::from_secs(5);
/// How often a fresh scan is asked for, from its `wifi-rescan` loop. Longer than
/// the read because the driver refuses one that follows too closely, and because
/// it blocks for the two or three seconds the radio spends sweeping.
const RESCAN_EVERY: Duration = Duration::from_secs(20);
/// How often the saved profiles are re-read. wifi.js polls at 10s so a network
/// saved by the connect flow shows up in the list without reopening it.
const SAVED_EVERY: Duration = Duration::from_secs(10);
/// How long an empty list is taken for "still warming up" rather than "nothing in
/// range", when no rescan has managed to finish. server.js's own grace window.
const SCAN_GRACE: Duration = Duration::from_secs(30);

/// One network the radio can see, deduplicated by SSID.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Network {
    pub ssid: String,
    /// 0-100, as nmcli reports it.
    pub signal: i32,
    /// Raw SECURITY field, e.g. "WPA2", "WPA1 WPA2", "WPA2 802.1X", or empty for
    /// an open network. Interpreted where it is drawn, not here.
    pub security: String,
}

/// The scan list, and whether it can be believed yet.
///
/// `ready` is the whole reason this is a struct: an empty list means "nothing in
/// range" only once a scan has actually completed, and until then the modal has to
/// keep showing its spinner rather than claiming the air is empty.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub ready: bool,
    pub networks: Vec<Network>,
}

/// A profile NetworkManager has saved, i.e. a network this device has joined.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    /// The connection's name, which is what every nmcli call takes.
    pub name: String,
    /// The on-air name. Usually the same as `name`, but they can differ.
    pub ssid: String,
    /// Raw `802-11-wireless-security.key-mgmt`, e.g. "wpa-psk", "sae", or empty.
    pub security: String,
    pub autoconnect: bool,
    /// `connection.timestamp`, or 0 for a profile that has never connected.
    pub last_connected: i64,
}

/// One address family's settings, as configured and as assigned.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Family {
    /// "auto", "manual", "disabled", as nmcli names them.
    pub method: String,
    pub gateway: String,
    /// Comma-separated, straight from nmcli.
    pub dns: String,
    /// Assigned addresses. Empty for a profile that is not active: those keys only
    /// exist while there is a link.
    pub addresses: Vec<String>,
}

/// Everything the verbose page shows about one profile.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Details {
    /// The connection name this was read with.
    pub name: String,
    pub autoconnect: bool,
    /// The PSK, when it was asked for and the profile has one. Empty for an open
    /// network, and for a read that did not ask for secrets.
    pub password: String,
    pub ipv4: Family,
    pub ipv6: Family,
    /// False when the read failed, which is a profile that has gone or an nmcli
    /// that is not there. Distinct from a profile that is merely not active.
    pub known: bool,
}

// ── Parsing nmcli's terse output ───────────────────────────────────────────

/// Split a terse line on its unescaped colons.
///
/// nmcli escapes a colon inside a value as `\:` and a backslash as `\\`, so a
/// naive split shreds an SSID that contains either. Values come back unescaped.
fn terse_fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut escaped = false;
    for c in line.chars() {
        if escaped {
            out.last_mut().unwrap().push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == ':' {
            out.push(String::new());
        } else {
            out.last_mut().unwrap().push(c);
        }
    }
    out
}

/// Split a terse line at its last unescaped colon.
///
/// For `NAME:TYPE`, where NAME is the field that can contain a colon and TYPE
/// cannot: walking back from the end is the only way to tell them apart.
fn terse_split_last(line: &str) -> Option<(String, String)> {
    let fields = terse_fields(line);
    if fields.len() < 2 {
        return None;
    }
    let tail = fields.last().unwrap().clone();
    Some((fields[..fields.len() - 1].join(":"), tail))
}

/// `key:value` per line, as `nmcli -t connection show <name>` prints it.
///
/// A Vec rather than a map because order matters: the assigned addresses are
/// `IP4.ADDRESS[1]`, `IP4.ADDRESS[2]` and so on, and they are collected in the
/// order nmcli listed them.
fn field_lines(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            Some((key.to_string(), value.replace("\\:", ":")))
        })
        .collect()
}

fn field(fields: &[(String, String)], key: &str) -> String {
    fields
        .iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
        .unwrap_or_default()
}

fn is_yes(value: &str) -> bool {
    value.eq_ignore_ascii_case("yes")
}

/// Every value whose key starts with `prefix`, in output order.
fn addresses(fields: &[(String, String)], prefix: &str) -> Vec<String> {
    fields
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(_, v)| v.clone())
        .collect()
}

// ── Reads ──────────────────────────────────────────────────────────────────

/// Parse one `nmcli -t -f SSID,SIGNAL,SECURITY device wifi list` dump.
///
/// Hidden networks are dropped, an SSID seen on several BSSIDs is kept once at its
/// strongest, and the list is ordered by signal as the prototype's is.
fn parse_scan(text: &str) -> Vec<Network> {
    let mut nets: Vec<Network> = Vec::new();
    for line in text.lines().filter(|l| !l.is_empty()) {
        let fields = terse_fields(line);
        let ssid = fields.first().cloned().unwrap_or_default();
        if ssid.is_empty() {
            continue;
        }
        let signal = fields
            .get(1)
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let security = fields.get(2).cloned().unwrap_or_default();
        match nets.iter_mut().find(|n| n.ssid == ssid) {
            // Same network on a stronger BSSID: promote it, and take that BSSID's
            // security with it, exactly as refreshWifiScan does.
            Some(seen) => {
                if signal > seen.signal {
                    seen.signal = signal;
                    seen.security = security;
                }
            }
            None => nets.push(Network { ssid, signal, security }),
        }
    }
    nets.sort_by_key(|net| std::cmp::Reverse(net.signal));
    nets
}

/// The saved wireless profiles, newest first.
///
/// Two passes, as server.js does: one listing to find the wireless connections,
/// then one `connection show` each for the fields the list needs. Serial rather
/// than parallel, because there are a handful of these and each is a process.
pub fn saved() -> Vec<Saved> {
    let Some(list) = output(&["nmcli", "-t", "-f", "NAME,TYPE", "connection", "show"]) else {
        return Vec::new();
    };
    let mut out: Vec<Saved> = list
        .lines()
        .filter(|l| !l.is_empty())
        .filter_map(terse_split_last)
        .filter(|(_, kind)| kind == "802-11-wireless")
        .map(|(name, _)| name)
        // The profiles this device makes for its own networking, not networks a
        // person joined: the router mode's own APs.
        .filter(|name| !is_internal(name))
        .map(|name| {
            let fields = output(&["nmcli", "-t", "connection", "show", &name])
                .map(|text| field_lines(&text))
                .unwrap_or_default();
            let ssid = field(&fields, "802-11-wireless.ssid");
            Saved {
                ssid: if ssid.is_empty() { name.clone() } else { ssid },
                security: field(&fields, "802-11-wireless-security.key-mgmt"),
                autoconnect: is_yes(&field(&fields, "connection.autoconnect")),
                last_connected: field(&fields, "connection.timestamp")
                    .parse()
                    .unwrap_or(0),
                name,
            }
        })
        .collect();
    // Most recently used at the top; never-connected profiles sink to the bottom
    // in name order.
    out.sort_by(|a, b| match (a.last_connected, b.last_connected) {
        (0, 0) => a.name.cmp(&b.name),
        (0, _) => std::cmp::Ordering::Greater,
        (_, 0) => std::cmp::Ordering::Less,
        (x, y) => y.cmp(&x),
    });
    out
}

/// Whether a connection is one the firmware made rather than one a person saved.
fn is_internal(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.starts_with("wifi-router") || lower.starts_with("flipper-")
}

/// One profile's settings, with its passphrase.
///
/// `--show-secrets` is what puts the PSK in the dump; it works because this runs
/// as root. A profile that is not active still answers with its configured method,
/// DNS and gateway, which is what makes the page worth opening on a saved network.
pub fn details(name: &str) -> Details {
    let Some(text) = output(&["nmcli", "--show-secrets", "-t", "connection", "show", name]) else {
        return Details { name: name.to_string(), ..Details::default() };
    };
    let fields = field_lines(&text);
    Details {
        name: name.to_string(),
        autoconnect: is_yes(&field(&fields, "connection.autoconnect")),
        password: field(&fields, "802-11-wireless-security.psk"),
        ipv4: Family {
            method: field(&fields, "ipv4.method"),
            gateway: field(&fields, "ipv4.gateway"),
            dns: field(&fields, "ipv4.dns"),
            addresses: addresses(&fields, "IP4.ADDRESS"),
        },
        ipv6: Family {
            method: field(&fields, "ipv6.method"),
            gateway: field(&fields, "ipv6.gateway"),
            dns: field(&fields, "ipv6.dns"),
            addresses: addresses(&fields, "IP6.ADDRESS"),
        },
        known: true,
    }
}

/// The scan poller, running for as long as it is held.
///
/// Wraps a `Watch` rather than being one because the rescan is on its own clock:
/// the list is read every five seconds and a fresh sweep is asked for every twenty,
/// so the fetch has to remember when it last asked.
pub struct ScanSource {
    watch: Watch<Scan>,
}

impl ScanSource {
    pub fn spawn() -> Self {
        let started = Instant::now();
        // Milliseconds since `started`, or 0 for "never". An atomic rather than a
        // mutex because the fetch closure has to be `Fn`.
        let last_rescan = Arc::new(AtomicU64::new(0));
        let rescanned = Arc::new(AtomicU64::new(0));
        // The last list that arrived. A read that fails is not news that the air
        // went quiet, so the previous list is what the page keeps showing.
        let last = Arc::new(std::sync::Mutex::new(Scan::default()));
        let watch = Watch::spawn("watch-wifi-scan", SCAN_EVERY, Scan::default(), move || {
            let since = started.elapsed().as_millis() as u64;
            let asked = last_rescan.load(Ordering::Relaxed);
            if asked == 0 || since.saturating_sub(asked) >= RESCAN_EVERY.as_millis() as u64 {
                last_rescan.store(since.max(1), Ordering::Relaxed);
                // Blocking, so the first read of the list is of a fresh sweep
                // rather than of whatever was cached from before the page opened.
                // A driver that refuses the request fails here and the cached list
                // is read anyway.
                if output(&["nmcli", "device", "wifi", "rescan"]).is_some() {
                    rescanned.store(1, Ordering::Relaxed);
                }
            }
            let Some(text) = output(&[
                "nmcli", "-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list",
            ]) else {
                return last.lock().unwrap().clone();
            };
            let networks = parse_scan(&text);
            // An empty list is the genuine state once a sweep has completed, or
            // once the grace window has passed on a machine where none ever will.
            let ready = !networks.is_empty()
                || rescanned.load(Ordering::Relaxed) == 1
                || started.elapsed() >= SCAN_GRACE;
            let fresh = Scan { ready, networks };
            *last.lock().unwrap() = fresh.clone();
            fresh
        });
        Self { watch }
    }

    pub fn get(&self) -> Scan {
        self.watch.get()
    }

    pub fn take_dirty(&self) -> bool {
        self.watch.take_dirty()
    }
}

/// The saved-profile poller, running for as long as it is held.
///
/// `None` until the first read lands, because an empty list has two meanings: the
/// device has never joined a network, or nobody has looked yet. The modal shows a
/// placeholder for one and a different one for the other.
pub fn saved_watch() -> Watch<Option<Vec<Saved>>> {
    Watch::spawn("watch-wifi-saved", SAVED_EVERY, None, || Some(saved()))
}

// ── Writes ─────────────────────────────────────────────────────────────────

/// Run a command and report what it said if it failed.
///
/// nmcli writes the reason a join failed to stderr ("Secrets were required, but
/// not provided", "No network with SSID 'x' found"), and that sentence is what the
/// screen shows, so it is passed through rather than replaced with our own words.
fn run(args: &[&str]) -> Result<(), String> {
    let out = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if stderr.is_empty() {
        format!("nmcli exited {}", out.status)
    } else {
        stderr
    })
}

/// Join a network, creating a profile for it if there is none.
///
/// An empty password joins an open network, and also reuses a saved profile's own
/// passphrase: `nmcli device wifi connect` takes the stored secret when it is not
/// given one, which is what lets the visible-networks list connect to a network it
/// already knows without asking again.
pub fn connect(ssid: &str, password: &str) -> Result<(), String> {
    if password.is_empty() {
        run(&["nmcli", "device", "wifi", "connect", ssid])
    } else {
        run(&["nmcli", "device", "wifi", "connect", ssid, "password", password])
    }
}

/// Drop the link but keep the profile.
pub fn disconnect(name: &str) -> Result<(), String> {
    run(&["nmcli", "connection", "down", name])
}

/// Delete the profile.
pub fn forget(name: &str) -> Result<(), String> {
    run(&["nmcli", "connection", "delete", name])
}

/// Whether this profile comes up on its own.
///
/// Detached and unwaited, like the radio toggles in `net`: the row it belongs to
/// flips optimistically and must not wait on a process. A refusal here is a
/// profile that has gone, which the next read of it reports anyway.
pub fn set_autoconnect(name: &str, on: bool) {
    crate::net::spawn_detached(&[
        "nmcli",
        "connection",
        "modify",
        name,
        "connection.autoconnect",
        if on { "yes" } else { "no" },
    ]);
}


// ── What the page and its modals measure for themselves ────────────────────
//
// Pure geometry, and the string measuring the geometry needs. It lives here
// rather than beside the rows it positions because it can then be tested without
// a window, which is the same division `boot_menu` draws: the screen's own
// arithmetic in the library, the Slint structs in whatever binary draws them.

/// Width of `text` in the panel's title face, in the prototype's own units.
pub fn tw(text: &str) -> i32 {
    i32::from(TITLE.text_width(text))
}

/// The tallest a modal's frame can be: the cap on its bottom edge, less the
/// margin its top would have at the very least, less the tab above it.
const MAX_FRAME_H: i32 = metric::WIFI_MODAL_MAX_BOT - metric::WIFI_MODAL_PAD - metric::WIFI_TAB_H;

/// Cut `text` to `budget`, ending in ".." when it had to.
///
/// wifi.js appends U+2026, which the panel's fonts do not have: every one of
/// them is printable ASCII and the prototype's own table substitutes "?" for
/// anything else, so a truncated SSID reads "MyNetwo?" on the device. ".." is
/// what this port already truncates with, in `detail::elide`.
pub fn fit(text: &str, budget: i32) -> String {
    if budget <= 0 {
        return String::new();
    }
    if tw(text) <= budget {
        return text.to_string();
    }
    let tail = tw("..");
    let mut out = String::new();
    for c in text.chars() {
        let mut probe = out.clone();
        probe.push(c);
        if tw(&probe) + tail > budget {
            break;
        }
        out = probe;
    }
    out.push_str("..");
    out
}

/// Cut `text` to `budget` from the left.
///
/// For the passphrase, whose tail is the part someone glances at to check they
/// have the right one. No marker: a key with its head cut off is not a word
/// that could be mistaken for the whole of it.
pub fn fit_tail(text: &str, budget: i32) -> String {
    let mut out = text.to_string();
    while !out.is_empty() && tw(&out) > budget {
        out.remove(0);
    }
    out
}

/// The security tag for a scan result.
///
/// nmcli's SECURITY field lists every mode an AP advertises, so the strongest
/// WPA generation present is the one shown, with "-E" appended when the
/// network also wants 802.1X on top of it.
pub fn security_label(sec: &str) -> String {
    if sec.is_empty() {
        return String::new();
    }
    let s = sec.to_ascii_uppercase();
    let enterprise = s.contains("802.1X");
    let version = ["WPA3", "WPA2", "WPA1", "WPA"]
        .into_iter()
        .find(|v| s.contains(v));
    if let Some(v) = version {
        return if enterprise { format!("{v}-E") } else { v.to_string() };
    }
    if s.contains("WEP") {
        return "WEP".into();
    }
    if enterprise {
        return "802.1X".into();
    }
    // Secured by something this does not recognise. Marked rather than shown
    // as open, which is the one reading that would be actively misleading.
    "*".into()
}


/// The tab a modal hangs from. Its own width is what stops the saved-networks
/// modal shrinking below its title.
pub const VISIBLE_TAB: &str = "Visible networks";
pub const SAVED_TAB: &str = "Saved networks";

/// Where a modal's frame lands and how much of its content fits in it.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub frame_x: i32,
    pub frame_w: i32,
    pub frame_y: i32,
    pub frame_h: i32,
    /// The first row's top, and the height the rows have to fit in.
    pub inner_top: i32,
    pub inner_h: i32,
    /// Rows the viewport holds at the list pitch. The row-indexed modal
    /// scrolls by this; the two that scroll in pixels go by `inner_h`.
    pub visible: i32,
}

impl Layout {
    /// The content width, which is the frame less its padding and less the
    /// gutter when the scrollbar is showing.
    pub fn row_w(&self, needs_scroll: bool) -> i32 {
        self.frame_w
            - 2 * metric::WIFI_INNER_PAD
            - if needs_scroll { metric::WIFI_MODAL_GUTTER } else { 0 }
    }
}

/// Place a frame of this height and width: centred on the panel, or pinned so
/// its bottom lands on the cap when centring would push it past.
///
/// `pad_b` is the modal's own bottom padding, which is not the same for all three:
/// the saved list pulls its 5px tighter, and taking the standard one here put a
/// scrollbar on a list that fitted and clipped its last row.
fn place(frame_w: i32, frame_h: i32, frame_x: i32, pad_b: i32) -> Layout {
    let modal_h = metric::WIFI_TAB_H + frame_h;
    let top = (i32::from(crate::PANEL_H) - modal_h)
        .div_euclid(2)
        .min(metric::WIFI_MODAL_MAX_BOT - modal_h)
        .max(metric::WIFI_MODAL_PAD);
    let frame_y = top + metric::WIFI_TAB_H;
    let inner_top = frame_y + metric::WIFI_MODAL_PAD_T;
    let inner_h = frame_y + frame_h - pad_b - inner_top;
    Layout {
        frame_x,
        frame_w,
        frame_y,
        frame_h,
        inner_top,
        inner_h,
        // A row and the rule under it are the pitch; the last row on screen
        // has no rule, so one more fits than the height alone would say.
        visible: 1.max((inner_h + 1) / metric::WIFI_ROW_PITCH),
    }
}

/// The visible-networks modal, which grows a row at a time up to the cap.
///
/// While the scan is out, and once it has come back with nothing, the frame
/// still reserves a couple of rows: the spinner and the placeholder both need
/// somewhere to sit.
pub fn visible_layout(ready: bool, count: i32) -> Layout {
    let max_inner = MAX_FRAME_H - metric::WIFI_MODAL_PAD_T - metric::WIFI_MODAL_PAD_B;
    let cap = 1.max((max_inner + 1) / metric::WIFI_ROW_PITCH);
    let rows = if !ready || count == 0 {
        count::WIFI_MIN_ROWS
    } else {
        count.min(cap)
    };
    let content = rows * metric::WIFI_ROW_H + (rows - 1).max(0);
    let frame_h =
        (content + metric::WIFI_MODAL_PAD_T + metric::WIFI_MODAL_PAD_B).min(MAX_FRAME_H);
    place(
        metric::WIFI_MODAL_W,
        frame_h,
        metric::WIFI_MODAL_PAD,
        metric::WIFI_MODAL_PAD_B,
    )
}

/// The saved-networks modal, which is only as wide as its longest name.
///
/// Its bottom padding is 5px tighter than the other two: these rows carry a
/// chevron bar and the slack under the last one read as a gap.
pub fn saved_layout(names: &[String]) -> Layout {
    let n = names.len() as i32;
    let content = saved_content_h(n);
    // Never taller than the cap, and never so short that the placeholder has
    // nowhere to sit.
    let frame_h = (content + metric::WIFI_MODAL_PAD_T + metric::WIFI_SAVED_PAD_B)
        .clamp(metric::WIFI_SAVED_MIN_H, MAX_FRAME_H);
    let inner_h = frame_h - metric::WIFI_MODAL_PAD_T - metric::WIFI_SAVED_PAD_B;
    let will_scroll = content > inner_h;
    // Reverse the row's own arithmetic: a name is drawn one text padding in
    // and ellipsised against the same padding on the right, inside a frame
    // whose sides cost the inner padding twice.
    let widest = names.iter().map(|s| tw(s)).max().unwrap_or(0);
    let driven = widest
        + 2 * metric::WIFI_INNER_PAD
        + 2 * metric::WIFI_TEXT_PAD_L
        + if will_scroll { metric::WIFI_MODAL_GUTTER } else { 0 };
    let min_w = tw(SAVED_TAB) + 2 * metric::WIFI_TAB_PAD + metric::WIFI_SAVED_MIN_GAP;
    let frame_w = driven.max(min_w).min(metric::WIFI_MODAL_W);
    place(
        frame_w,
        frame_h,
        (i32::from(crate::PANEL_W) - frame_w).div_euclid(2),
        metric::WIFI_SAVED_PAD_B,
    )
}

/// One network's settings, which is as tall as its rows up to the cap and
/// scrolls past that.
pub fn detail_layout(content_h: i32) -> Layout {
    let frame_h =
        (content_h + metric::WIFI_MODAL_PAD_T + metric::WIFI_MODAL_PAD_B).min(MAX_FRAME_H);
    place(
        metric::WIFI_MODAL_W,
        frame_h,
        metric::WIFI_MODAL_PAD,
        metric::WIFI_MODAL_PAD_B,
    )
}


/// Scroll a row-indexed list so the selected row is inside the viewport.
pub fn keep_row_visible(selected: i32, scroll: i32, visible: i32, total: i32) -> i32 {
    let mut at = scroll;
    if selected < at {
        at = selected;
    } else if selected >= at + visible {
        at = selected - visible + 1;
    }
    at.clamp(0, (total - visible).max(0))
}

/// How tall the saved list's rows come to.
///
/// The last row's own trailing pixel is dropped: the rule belongs between two
/// rows, so the space for one below the last is space for nothing.
pub fn saved_content_h(count: i32) -> i32 {
    if count > 0 {
        count * metric::WIFI_ROW_PITCH - 1
    } else {
        0
    }
}

/// One line of an nmcli refusal, cut to `budget`.
///
/// nmcli says why in a sentence, sometimes over several lines. The first line
/// is the reason; what follows it is context that will not fit anywhere on
/// this panel.
pub fn one_line(text: &str, budget: i32) -> String {
    fit(text.lines().next().unwrap_or_default().trim(), budget)
}


/// Scroll so a row at `y` of height `h` is inside the viewport.
pub fn ensure_visible(y: i32, h: i32, inner_h: i32, content_h: i32, scroll: i32) -> i32 {
    let mut at = scroll;
    if y < at {
        at = y;
    } else if y + h > at + inner_h {
        at = y + h - inner_h;
    }
    at.clamp(0, (content_h - inner_h).max(0))
}

// ── The rows themselves ────────────────────────────────────────────────────
//
// Plain rows, which the binary drawing them maps onto its Slint structs. Same
// division as `boot_menu`: the screen's own model here, where a test can build it
// and check it, and the Slint types on the far side of that mapping.

/// What one page row does, so the key handler never matches on a label.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub enum Act {
    /// The radio, which left, right and ok all flip.
    Radio,
    /// The network in use, which opens its settings.
    Connected,
    Visible,
    Saved,
    /// Present so the entry point is visible, and does nothing when pressed.
    /// wifi.js has no flow behind it either.
    Hidden,
    /// A divider, which the selector skips.
    #[default]
    Divider,
}

/// One row of the Wi-Fi page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Row {
    /// 0 a plain row, 1 a group divider, 2 the radio toggle, 3 the connected
    /// network. What the Slint side draws it as.
    pub kind: i32,
    /// Where it sits. Given rather than multiplied out because the connected row
    /// is a pixel taller than the rest and a divider is 3px, so the pitch is not
    /// constant.
    pub y: i32,
    pub text: String,
    /// The toggle's value, "ON" or "OFF", or the grey half of the connected row.
    pub value: String,
    /// Where `text` starts, for the row whose two halves are different colours.
    /// Measured here because the grey half ends in a space and Slint drops a
    /// trailing space when it measures a string.
    pub text_x: i32,
    /// Whether it drills in, which is what puts the chevron bar on the selector
    /// when it lands here.
    pub chevron: bool,
    pub act: Act,
}

/// One row of the visible-networks or saved-networks modal.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NetRow {
    /// Already cut to the width it was measured against.
    pub text: String,
    /// The security tag, empty for an open network and for every saved row.
    pub security: String,
    /// 0-100, for the same five sprites the status bar picks from.
    pub quality: i32,
    /// Whether a rule is drawn under it.
    pub divider: bool,
}

/// One row of one network's settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DetailRow {
    /// 0 a rule, 1 an action, 2 the passphrase, 3 a label and its value, 4 a
    /// section label, 5 a value under one, 6 a line saying there is nothing to
    /// show, 7 a card: a frame around the lines that follow it.
    pub kind: i32,
    pub y: i32,
    pub h: i32,
    pub label: String,
    pub value: String,
    pub selectable: bool,
    /// A line inside a card, which changes where its rule is inset to and how
    /// light it is drawn.
    pub in_card: bool,
    /// An action whose value is an ON/OFF toggle, which is Auto join.
    pub toggle: bool,
    /// A value with nothing behind it yet: "Loading..", "open", "-".
    pub dim: bool,
    /// Push the value down 2px. The asterisks of a mask sit high in HaxrCorp,
    /// near the ascender, so a mask on the row's own baseline reads as floating.
    pub nudge: bool,
    pub act: DetailAct,
}

fn page_row(kind: i32, y: i32, text: &str, value: &str, chevron: bool, act: Act) -> Row {
    Row {
        kind,
        y,
        text: text.into(),
        value: value.into(),
        text_x: 0,
        chevron,
        act,
    }
}

/// What the connected row narrates before the name itself.
const CONNECTED_TO: &str = "Connected to: ";

/// The page's rows, for the radio state as it is now.
///
/// Rebuilt every time anything moves, as `render` does in the prototype: the
/// connected row appears and disappears with the connection, and every row
/// below the toggle is hidden while the radio is off. There is nothing useful
/// to do with them then, and offering them would be a lie about what the
/// device can do at that moment.
pub fn page_rows(net: &crate::net::Net) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut y = metric::WIFI_CONTAINER_Y;
    rows.push(page_row(
        2,
        y,
        "Wi-Fi",
        if net.wifi_enabled { "ON" } else { "OFF" },
        false,
        Act::Radio,
    ));
    y += metric::WIFI_ROW_H;
    if !net.wifi_enabled {
        return rows;
    }
    rows.push(page_row(1, y, "", "", false, Act::Divider));
    y += metric::WIFI_DIVIDER_H;
    if net.wifi_connected && !net.ssid.is_empty() {
        let mut row = page_row(3, y, &net.ssid, CONNECTED_TO, true, Act::Connected);
        row.text_x = metric::WIFI_TEXT_PAD_L + tw(CONNECTED_TO);
        rows.push(row);
        // A pixel of air under the connected row, so it reads as its own thing
        // rather than as the first of a pair with the row below.
        y += metric::WIFI_ROW_H + 1;
    }
    rows.push(page_row(0, y, "See visible networks", "", true, Act::Visible));
    y += metric::WIFI_ROW_H;
    rows.push(page_row(1, y, "", "", false, Act::Divider));
    y += metric::WIFI_DIVIDER_H;
    rows.push(page_row(0, y, "Saved networks", "", true, Act::Saved));
    y += metric::WIFI_ROW_H;
    rows.push(page_row(
        0,
        y,
        "Connect to Hidden Network",
        "",
        true,
        Act::Hidden,
    ));
    rows
}

/// The visible-networks rows, each cut to what is left after its own signal
/// sprite and security tag.
///
/// `last_visible` is the bottom row of the viewport: the rule under a row
/// separates it from the next one, so the last row on screen does not get one.
pub fn visible_rows(nets: &[Network], row_w: i32, last_visible: i32) -> Vec<NetRow> {
    nets.iter()
        .enumerate()
        .map(|(i, net)| {
            let security = security_label(&net.security);
            // Laid out right to left, as the prototype's measure pass runs:
            // the sprite, then the tag, then whatever is left for the name.
            let mut end = row_w - 7 - metric::WIFI_ROW_PAD_R - metric::WIFI_SIGNAL_GAP;
            if !security.is_empty() {
                end -= tw(&security) + metric::WIFI_SIGNAL_GAP;
            }
            NetRow {
                text: fit(&net.ssid, end - metric::WIFI_TEXT_PAD_L),
                security,
                quality: net.signal,
                divider: (i as i32) < last_visible,
            }
        })
        .collect()
}

/// The saved-networks rows: the name and nothing else.
///
/// No security column and no signal: a saved profile is not in range by
/// definition, and what it is protected by is on the page behind it, where it
/// is something the user can act on.
pub fn saved_rows(names: &[String], row_w: i32) -> Vec<NetRow> {
    names
        .iter()
        .map(|name| NetRow {
            text: fit(name, row_w - 2 * metric::WIFI_TEXT_PAD_L),
            security: Default::default(),
            quality: 0,
            divider: false,
        })
        .collect()
}

/// The name to show for a saved profile, which is its SSID where they differ.
pub fn saved_names(saved: &[Saved]) -> Vec<String> {
    saved
        .iter()
        .map(|s| {
            if s.ssid.is_empty() {
                s.name.clone()
            } else {
                s.ssid.clone()
            }
        })
        .collect()
}

fn drow(kind: i32, h: i32) -> DetailRow {
    DetailRow {
        kind,
        h,
        ..DetailRow::default()
    }
}

/// nmcli's method names, as the prototype relabels them.
fn method(m: &str) -> String {
    match m {
        "auto" => "DHCP".into(),
        "manual" => "Manual".into(),
        "disabled" => "Disabled".into(),
        "" => "-".into(),
        other => other.into(),
    }
}

/// A label and its value on one line, inside a card.
fn card_kv(label: &str, value: &str) -> DetailRow {
    let mut row = drow(3, metric::WIFI_KV_H);
    row.label = label.into();
    row.value = value.into();
    row.in_card = true;
    row
}

/// A list under `label`, inside a card.
///
/// One item is a label and its value on one line; several become a section
/// label with the values indented under it, because two addresses do not fit
/// on one line and wrapping them would lose which is which.
fn card_list(out: &mut Vec<DetailRow>, label: &str, items: &[String]) {
    match items {
        [] => {}
        [one] => out.push(card_kv(label, one)),
        many => {
            let mut head = drow(4, metric::WIFI_KV_H);
            head.label = label.into();
            head.in_card = true;
            out.push(head);
            for item in many {
                let mut row = drow(5, metric::WIFI_KV_H);
                row.value = item.as_str().into();
                row.in_card = true;
                out.push(row);
            }
        }
    }
}

/// One family's lines: how it is configured, what it was given, and where it
/// sends traffic.
///
/// A profile that is not active answers with its configured method, DNS and
/// gateway and no addresses, which is what makes the card worth drawing for a
/// saved network as well as for the live one.
fn family_lines(label: &str, f: &Family) -> Vec<DetailRow> {
    let mut out = vec![card_kv("Method:", &method(&f.method))];
    card_list(&mut out, &format!("{label}:"), &f.addresses);
    if !f.gateway.is_empty() {
        out.push(card_kv("Gateway:", &f.gateway));
    }
    let dns: Vec<String> = f
        .dns
        .split(',')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    card_list(&mut out, "DNS:", &dns);
    out
}

/// What a settings row does when it is pressed.
#[derive(Copy, Clone, Default, PartialEq, Eq, Debug)]
pub enum DetailAct {
    /// A rule, or a card's own line: the selector skips it.
    #[default]
    None,
    /// Drop the link, keeping the profile. Only on the live connection.
    Disconnect,
    /// Delete the profile.
    Forget,
    AutoJoin,
    /// Reached for reading, not for pressing: the passphrase is revealed by
    /// holding OK on it, so there is nothing for a press to do and it does not
    /// flash.
    Password,
    /// A card. Selectable, and pressing it flashes and does nothing: the
    /// prototype leaves the address editor for later and this is its handle.
    Card,
}

/// The settings page for one profile: its rows, what each does, and the height
/// they come to.
pub struct DetailView {
    pub rows: Vec<DetailRow>,
    pub content_h: i32,
}

/// Build it.
///
/// Order and grouping are the prototype's: Disconnect first when this is the
/// live connection, then Forget, then the pair a person actually changes, then
/// a card per address family. Severity reads top to bottom.
///
/// The passphrase is left whole here and cut to the row afterwards, by
/// `fit_password`: how wide the rows are depends on whether the scrollbar is
/// showing, which depends on how tall the content came out, which is what this
/// is working out.
pub fn detail_rows(
    details: Option<&Details>,
    loading: bool,
    active: bool,
    reveal: bool,
) -> DetailView {
    let mut rows: Vec<DetailRow> = Vec::new();
    if active {
        rows.push(action("Disconnect", DetailAct::Disconnect));
        rows.push(drow(0, metric::WIFI_SECTION_H));
    }
    rows.push(action("Forget this network", DetailAct::Forget));
    rows.push(drow(0, metric::WIFI_SECTION_H));

    let mut auto = action("Auto join", DetailAct::AutoJoin);
    auto.toggle = true;
    auto.value = match details {
        Some(d) if d.known => if d.autoconnect { "ON" } else { "OFF" }.into(),
        _ => "-".into(),
    };
    rows.push(auto);

    let mut pw = action("Password", DetailAct::Password);
    pw.kind = 2;
    let (value, dim, nudge) = match details {
        None if loading => ("..".to_string(), true, false),
        None => ("-".to_string(), true, false),
        Some(d) if !d.known => ("-".to_string(), true, false),
        Some(d) if d.password.is_empty() => ("open".to_string(), true, false),
        Some(d) if reveal => (d.password.clone(), false, false),
        // Asterisks rather than bullets: U+2022 is not in the panel's fonts.
        // As many as the key is long, so a short and a long one look different.
        Some(d) => ("*".repeat(d.password.chars().count()), false, true),
    };
    pw.value = value;
    pw.dim = dim;
    pw.nudge = nudge;
    rows.push(pw);

    rows.push(drow(0, metric::WIFI_SECTION_H));

    let cards: Vec<Vec<DetailRow>> = match details.filter(|d| d.known) {
        Some(d) => vec![
            family_lines("IPv4", &d.ipv4),
            family_lines("IPv6", &d.ipv6),
        ],
        // Nothing to show yet, or a profile that has gone. One card either
        // way, so the section is never an empty hole.
        None => {
            let mut line = drow(6, metric::WIFI_KV_H);
            line.value = if loading { "Loading.." } else { "Unavailable" }.into();
            line.in_card = true;
            vec![vec![line]]
        }
    };

    // Place them. A card is one selectable row as tall as its lines plus its
    // own padding, and its lines follow it at their own offsets.
    let mut y = 0;
    for row in rows.iter_mut() {
        row.y = y;
        y += row.h;
        // Auto join and the passphrase get a pixel between them rather than a
        // rule: they belong together, and a whole row of air would separate
        // them. The same pixel goes under the pair, pushing the rule below it
        // down by one.
        if matches!(row.act, DetailAct::AutoJoin | DetailAct::Password) {
            y += 1;
        }
    }
    for (i, lines) in cards.iter().enumerate() {
        if lines.is_empty() {
            continue;
        }
        let inner: i32 = lines.iter().map(|l| l.h).sum();
        let mut card = drow(7, 2 * metric::WIFI_CARD_PAD + inner);
        card.y = y;
        card.selectable = true;
        card.act = DetailAct::Card;
        rows.push(card);
        let mut ly = y + metric::WIFI_CARD_PAD;
        for line in lines {
            let mut line = line.clone();
            line.y = ly;
            ly += line.h;
            rows.push(line);
        }
        y += 2 * metric::WIFI_CARD_PAD + inner;
        // A gap between the two cards, so their outlines do not kiss when the
        // selection moves from one to the other.
        if i + 1 < cards.len() {
            y += 1;
        }
    }
    DetailView { rows, content_h: y }
}

/// Cut the passphrase to the row, once the row's width is known.
///
/// Right-aligned against the content edge and cut from the left, so what
/// survives is the tail.
pub fn fit_password(rows: &mut [DetailRow], row_w: i32) {
    let budget = row_w
        - metric::WIFI_ROW_PAD_R
        - (metric::WIFI_TEXT_PAD_L + tw("Password") + metric::WIFI_PW_GAP);
    for row in rows.iter_mut().filter(|r| r.kind == 2) {
        row.value = fit_tail(&row.value, budget);
    }
}

/// An action row: a label the selector can land on and press.
fn action(text: &str, act: DetailAct) -> DetailRow {
    let mut row = drow(1, metric::WIFI_ROW_H);
    row.label = text.into();
    row.selectable = true;
    row.act = act;
    row
}


/// The profile to join `ssid` with, if this device already has one.
///
/// Looked up by both the on-air name and the profile's own name, as the
/// prototype's map is: for a profile made by joining a network the two are the
/// same, but they can differ, and either should find it. Connecting by the
/// profile's name is what reuses its passphrase, which is what lets a network the
/// device already knows be joined from the list without being asked again.
pub fn saved_match(saved: &[Saved], ssid: &str) -> Option<String> {
    saved
        .iter()
        .find(|s| s.ssid == ssid || s.name == ssid)
        .map(|s| s.name.clone())
}

/// Which rows the selector can land on.
pub fn selectable(rows: &[DetailRow]) -> Vec<bool> {
    rows.iter().map(|r| r.selectable).collect()
}

/// The next row the selector can land on, wrapping. -1 when there is none.
pub fn next_selectable(ok: &[bool], from: i32, dir: i32) -> i32 {
    let n = ok.len() as i32;
    if n == 0 {
        return -1;
    }
    let mut at = from;
    for _ in 0..n {
        at = (at + dir).rem_euclid(n);
        if ok[at as usize] {
            return at;
        }
    }
    -1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::Net;

    /// A profile's settings as nmcli reports them for a live connection.
    fn live_details() -> Details {
        Details {
            name: "Home".into(),
            autoconnect: true,
            password: "hunter2".into(),
            ipv4: Family {
                method: "auto".into(),
                gateway: "10.0.0.1".into(),
                dns: "10.0.0.1,1.1.1.1".into(),
                addresses: vec!["10.0.0.5/24".into()],
            },
            ipv6: Family {
                method: "auto".into(),
                gateway: String::new(),
                dns: String::new(),
                addresses: vec!["fe80::1/64".into()],
            },
            known: true,
        }
    }

    #[test]
    fn a_security_field_becomes_its_strongest_generation() {
        assert_eq!(security_label(""), "");
        assert_eq!(security_label("WPA1 WPA2"), "WPA2");
        assert_eq!(security_label("WPA2 WPA3"), "WPA3");
        assert_eq!(security_label("WPA2 802.1X"), "WPA2-E");
        assert_eq!(security_label("802.1X"), "802.1X");
        assert_eq!(security_label("WEP"), "WEP");
        // Protected by something unrecognised, which must not read as open.
        assert_eq!(security_label("OWE"), "*");
    }

    #[test]
    fn text_is_cut_to_its_budget_from_whichever_end_matters() {
        assert_eq!(fit("Home", 200), "Home");
        let cut = fit("A very long network name indeed", 40);
        assert!(cut.ends_with(".."), "{cut}");
        assert!(tw(&cut) <= 40, "{cut} is {} wide", tw(&cut));
        // The passphrase keeps its tail, and takes no marker with it: a key with
        // its head cut off is not a word that could be read as the whole of it.
        let key = fit_tail("supersecretpassphrase", 40);
        assert!("supersecretpassphrase".ends_with(&key), "{key}");
        assert!(tw(&key) <= 40);
    }

    /// Hand-derived from wifi.js's computeLayout, not from this implementation.
    #[test]
    fn the_visible_list_centres_until_it_reaches_the_cap() {
        // Four networks: 4 * 13 + 3 rules = 55 of content in a 62px frame, and the
        // modal centres because its bottom is clear of y128.
        let four = visible_layout(true, 4);
        assert_eq!(four.frame_h, 62);
        assert_eq!(four.frame_y, 49);
        assert_eq!(four.inner_top, 52);
        assert_eq!(four.inner_h, 55);
        assert_eq!(four.visible, 4);

        // Twelve: only seven fit, so the frame grows to those seven and no
        // further, and its bottom is pinned to the y128 cap.
        let many = visible_layout(true, 12);
        assert_eq!(many.frame_h, 7 * metric::WIFI_ROW_H + 6 + 3 + 4);
        assert_eq!(many.frame_y + many.frame_h, metric::WIFI_MODAL_MAX_BOT);
        assert_eq!(many.visible, 7);
        assert!(many.inner_top > many.frame_y);

        // Still scanning: room for two rows is reserved so the spinner has
        // somewhere to sit.
        let waiting = visible_layout(false, 0);
        assert_eq!(waiting.frame_h, 2 * metric::WIFI_ROW_H + 1 + 3 + 4);
        assert_eq!(waiting.visible, 2);
    }

    #[test]
    fn the_visible_list_never_reserves_the_gutter_it_does_not_need() {
        let four = visible_layout(true, 4);
        assert_eq!(
            four.row_w(false) - four.row_w(true),
            metric::WIFI_MODAL_GUTTER
        );
        assert_eq!(four.row_w(false), metric::WIFI_MODAL_W - 6);
    }

    #[test]
    fn the_saved_list_is_as_wide_as_its_longest_name() {
        let short = saved_layout(&["a".into()]);
        // Never narrower than its own tab with room either side.
        let min = tw(SAVED_TAB) + 2 * metric::WIFI_TAB_PAD + metric::WIFI_SAVED_MIN_GAP;
        assert_eq!(short.frame_w, min);
        // Centred on the panel.
        assert_eq!(short.frame_x, (i32::from(crate::PANEL_W) - short.frame_w) / 2);

        let long = saved_layout(&["A rather long network name".into()]);
        assert!(long.frame_w > short.frame_w);
        assert!(long.frame_w <= metric::WIFI_MODAL_W);

        // An empty list still has room for its placeholder.
        assert_eq!(saved_layout(&[]).frame_h, metric::WIFI_SAVED_MIN_H);

        // Three names fit exactly, and must not be given a scrollbar for it: this
        // modal's bottom padding is its own, and taking the standard one put a bar
        // on a list that fitted and clipped its last row.
        let three = saved_layout(&(0..3).map(|i| format!("net{i}")).collect::<Vec<_>>());
        assert_eq!(three.inner_h, saved_content_h(3));

        // Twenty profiles overflow, so the frame caps and the bar's gutter is
        // taken out of the rows.
        let full = saved_layout(&(0..20).map(|i| format!("net{i}")).collect::<Vec<_>>());
        assert_eq!(full.frame_h, 108);
        assert!(saved_content_h(20) > full.inner_h);
    }

    #[test]
    fn the_settings_modal_caps_at_the_panel_and_scrolls_past_it() {
        let tall = detail_layout(200);
        assert_eq!(tall.frame_h, 108);
        assert_eq!(tall.frame_y + tall.frame_h, metric::WIFI_MODAL_MAX_BOT);
        assert_eq!(tall.inner_h, 101);
        // Short content is centred instead.
        let short = detail_layout(40);
        assert_eq!(short.frame_h, 47);
        assert_eq!(short.inner_h, 40);
        assert!(short.frame_y + short.frame_h < metric::WIFI_MODAL_MAX_BOT);
    }

    #[test]
    fn the_radio_being_off_leaves_only_its_own_row() {
        let off = Net { wifi_enabled: false, ..Net::default() };
        let rows = page_rows(&off);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].act, Act::Radio);
        assert_eq!(rows[0].value, "OFF");
        assert_eq!(rows[0].y, metric::WIFI_CONTAINER_Y);
    }

    #[test]
    fn the_connected_row_comes_and_goes_with_the_connection() {
        let on = Net {
            wifi_enabled: true,
            wifi_connected: false,
            ..Net::default()
        };
        let acts: Vec<Act> = page_rows(&on).iter().map(|r| r.act).collect();
        assert_eq!(
            acts,
            [Act::Radio, Act::Divider, Act::Visible, Act::Divider, Act::Saved, Act::Hidden]
        );

        let joined = Net {
            wifi_enabled: true,
            wifi_connected: true,
            ssid: "Home".into(),
            ..Net::default()
        };
        let rows = page_rows(&joined);
        let connected = &rows[2];
        assert_eq!(connected.act, Act::Connected);
        assert_eq!(connected.text, "Home");
        assert!(connected.chevron);
        // A pixel of air under it, so it is not read as the first of a pair.
        assert_eq!(rows[3].y - connected.y, metric::WIFI_ROW_H + 1);
        // Every row still clears the bottom of the panel.
        let last = rows.last().unwrap();
        assert!(last.y + metric::WIFI_ROW_H < i32::from(crate::PANEL_H));
    }

    #[test]
    fn only_the_live_connection_can_be_disconnected() {
        let details = live_details();
        let saved = detail_rows(Some(&details), false, false, false);
        assert_eq!(saved.rows[0].act, DetailAct::Forget);
        let active = detail_rows(Some(&details), false, true, false);
        assert_eq!(active.rows[0].act, DetailAct::Disconnect);
        assert_eq!(active.rows[2].act, DetailAct::Forget);
    }

    #[test]
    fn the_settings_rows_stack_without_a_gap_they_did_not_ask_for() {
        let details = live_details();
        let view = detail_rows(Some(&details), false, true, false);
        // Every row sits where the one above it ends, but for the pixel under the
        // pair a person changes and the one between the two cards.
        let mut expected = 0;
        for row in &view.rows {
            // A card's lines are placed inside it rather than after it.
            if row.in_card {
                continue;
            }
            let slack = row.y - expected;
            assert!(
                (0..=1).contains(&slack),
                "row {:?} at y{} leaves {slack} of slack",
                row.act,
                row.y
            );
            expected = row.y + row.h;
        }
        assert!(view.content_h >= expected);

        // Auto join and the passphrase are a pixel apart, not a row.
        let auto = view.rows.iter().find(|r| r.act == DetailAct::AutoJoin).unwrap();
        let pw = view.rows.iter().find(|r| r.act == DetailAct::Password).unwrap();
        assert_eq!(pw.y - (auto.y + auto.h), 1);

        // A card is as tall as its lines plus its own padding.
        let card = view.rows.iter().position(|r| r.kind == 7).unwrap();
        let lines: i32 = view.rows[card + 1..]
            .iter()
            .take_while(|r| r.in_card)
            .map(|r| r.h)
            .sum();
        assert_eq!(view.rows[card].h, lines + 2 * metric::WIFI_CARD_PAD);
        assert!(lines > 0, "the card must hold its own lines");
    }

    #[test]
    fn a_family_with_several_addresses_indents_them_under_a_label() {
        let mut details = live_details();
        details.ipv4.addresses = vec!["10.0.0.5/24".into(), "10.0.0.6/24".into()];
        let view = detail_rows(Some(&details), false, false, false);
        let labels: Vec<&str> = view
            .rows
            .iter()
            .filter(|r| r.in_card && r.kind == 4)
            .map(|r| r.label.as_str())
            .collect();
        assert!(labels.contains(&"IPv4:"), "{labels:?}");
        // Two values under it, and the DNS pair under its own label.
        let values = view.rows.iter().filter(|r| r.kind == 5).count();
        assert_eq!(values, 4);
    }

    #[test]
    fn the_passphrase_is_masked_until_it_is_held() {
        let details = live_details();
        let masked = detail_rows(Some(&details), false, false, false);
        let row = masked.rows.iter().find(|r| r.kind == 2).unwrap();
        assert_eq!(row.value, "*".repeat("hunter2".len()));
        // The mask sits high in this face, so it is nudged onto the row's line.
        assert!(row.nudge);

        let shown = detail_rows(Some(&details), false, false, true);
        let row = shown.rows.iter().find(|r| r.kind == 2).unwrap();
        assert_eq!(row.value, "hunter2");
        assert!(!row.nudge);

        // An open network has none, and says so rather than showing an empty slot.
        let mut open = live_details();
        open.password.clear();
        let row = detail_rows(Some(&open), false, false, false)
            .rows
            .into_iter()
            .find(|r| r.kind == 2)
            .unwrap();
        assert_eq!(row.value, "open");
        assert!(row.dim);
    }

    #[test]
    fn a_profile_that_has_not_arrived_yet_still_draws_a_card() {
        let view = detail_rows(None, true, false, false);
        let card = view.rows.iter().find(|r| r.kind == 7).unwrap();
        assert!(card.selectable);
        let line = view.rows.iter().find(|r| r.kind == 6).unwrap();
        assert_eq!(line.value, "Loading..");
        // And one that failed to read says something different.
        let gone = detail_rows(None, false, false, false);
        assert_eq!(gone.rows.iter().find(|r| r.kind == 6).unwrap().value, "Unavailable");
    }

    #[test]
    fn the_selector_skips_what_it_cannot_land_on() {
        let view = detail_rows(Some(&live_details()), false, true, false);
        let reachable = selectable(&view.rows);
        // From the first action, down lands on the next action rather than on the
        // rule between them.
        let next = next_selectable(&reachable, 0, 1);
        assert!(next > 0 && view.rows[next as usize].selectable);
        // And it wraps.
        let last = reachable.iter().rposition(|ok| *ok).unwrap() as i32;
        assert_eq!(next_selectable(&reachable, last, 1), 0);
        // A list with nothing to land on says so rather than parking on a rule.
        assert_eq!(next_selectable(&[false, false], 0, 1), -1);
    }

    #[test]
    fn scrolling_follows_the_selection_and_stops_at_the_ends() {
        // Rows: seven visible out of twelve.
        assert_eq!(keep_row_visible(3, 0, 7, 12), 0);
        assert_eq!(keep_row_visible(7, 0, 7, 12), 1);
        assert_eq!(keep_row_visible(11, 0, 7, 12), 5);
        // Never past the end, and never before the start.
        assert_eq!(keep_row_visible(11, 9, 7, 12), 5);
        assert_eq!(keep_row_visible(0, 5, 7, 12), 0);
        // A list that fits does not scroll at all.
        assert_eq!(keep_row_visible(3, 0, 7, 4), 0);

        // Pixels: a row at y100 of height 13 in a 50px viewport of 200px content.
        assert_eq!(ensure_visible(100, 13, 50, 200, 0), 63);
        assert_eq!(ensure_visible(0, 13, 50, 200, 63), 0);
        assert_eq!(ensure_visible(190, 13, 50, 200, 0), 150);
    }

    #[test]
    fn a_known_network_is_matched_by_either_of_its_names() {
        let saved = vec![
            Saved {
                name: "Home".into(),
                ssid: "Home".into(),
                ..Saved::default()
            },
            Saved {
                name: "office-profile".into(),
                ssid: "Office WiFi".into(),
                ..Saved::default()
            },
        ];
        assert_eq!(saved_match(&saved, "Home").as_deref(), Some("Home"));
        // Matched on the air name, joined by the profile's own: that is what
        // reuses the stored passphrase.
        assert_eq!(
            saved_match(&saved, "Office WiFi").as_deref(),
            Some("office-profile")
        );
        assert_eq!(saved_match(&saved, "Cafe"), None);
    }

    #[test]
    fn a_visible_row_leaves_room_for_its_own_signal_and_tag() {
        let nets = vec![
            Network {
                ssid: "A network with a name that will not fit".into(),
                signal: 70,
                security: "WPA2".into(),
            },
            Network {
                ssid: "Open".into(),
                signal: 20,
                security: String::new(),
            },
        ];
        // A narrow row, so the long name genuinely has to be cut.
        let row_w = 120;
        // Both rows are on screen, so the second one is the last and gets no rule.
        let rows = visible_rows(&nets, row_w, 1);
        assert_eq!(rows.len(), 2);
        assert!(rows[0].text.ends_with(".."));
        assert_eq!(rows[0].security, "WPA2");
        assert_eq!(rows[1].security, "");
        // The rule separates two rows, so the last one on screen has none.
        assert!(rows[0].divider);
        assert!(!rows[1].divider);
        // The name cannot reach the tag.
        assert!(
            tw(&rows[0].text)
                <= row_w
                    - 7
                    - metric::WIFI_ROW_PAD_R
                    - 2 * metric::WIFI_SIGNAL_GAP
                    - tw("WPA2")
                    - metric::WIFI_TEXT_PAD_L
        );
    }

    #[test]
    fn terse_split_keeps_escaped_colons() {
        assert_eq!(terse_fields(r"a\:b:70:WPA2"), ["a:b", "70", "WPA2"]);
        assert_eq!(
            terse_split_last(r"my\:net:802-11-wireless"),
            Some(("my:net".into(), "802-11-wireless".into()))
        );
    }

    #[test]
    fn scan_dedupes_by_ssid_and_sorts_by_signal() {
        let nets = parse_scan("weak:20:WPA2\nhome:40:WPA1 WPA2\nhome:80:WPA2\n:99:\n");
        assert_eq!(nets.len(), 2);
        assert_eq!(nets[0].ssid, "home");
        assert_eq!(nets[0].signal, 80);
        // The stronger BSSID's security wins with it.
        assert_eq!(nets[0].security, "WPA2");
        assert_eq!(nets[1].ssid, "weak");
    }

    #[test]
    fn addresses_come_back_in_order() {
        let fields = field_lines(
            "ipv4.method:auto\nIP4.ADDRESS[1]:10.0.0.2/24\nIP4.ADDRESS[2]:10.0.0.3/24\n",
        );
        assert_eq!(field(&fields, "ipv4.method"), "auto");
        assert_eq!(
            addresses(&fields, "IP4.ADDRESS"),
            ["10.0.0.2/24", "10.0.0.3/24"]
        );
    }

    #[test]
    fn internal_profiles_are_not_saved_networks() {
        assert!(is_internal("wifi-router-5g"));
        assert!(is_internal("flipper-gadget"));
        assert!(!is_internal("Home WiFi"));
    }
}