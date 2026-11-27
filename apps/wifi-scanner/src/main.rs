//! Wi-Fi Scanner: the networks around and how each protects itself, and for any of
//! them how busy its channel is and who is using it.
//!
//! The list comes from the scans NetworkManager keeps, read from the kernel, and says
//! everything a beacon tells: security, WPS, protocol, load, vendor. Analyze borrows the
//! onboard Wi-Fi from flipctl in monitor mode (see `capture`), tuned to that network's
//! channel at its full width; Wi-Fi is off for the system while a network is analysed,
//! and comes back on Back, Close, or however the app ends. It only listens: nothing is
//! sent, and payloads, which are encrypted, are never looked at.
//!
//! The list, the network card and the dolphin's advice are the Figma frames, painted
//! (see `ui`); the details and the analyser are pages of the framework.

mod beep;
mod capture;
mod classify;
mod dot11;
mod graph;
mod icons;
mod ident;
mod lan;
mod locate;
mod model;
mod nl;
mod oui;
mod radiotap;
mod scan;
mod sprites;
mod taxonomy;
mod ui;
mod watch;

use std::time::{Duration, Instant};

use flipctl_app::font::fit;
use flipctl_app::paint::Surface;
use flipctl_app::pixel::Gray8;
use flipctl_app::theme;
use flipctl_app::{gauge, heading, item, pair, rule, text, App, Key, Page, Row, Ui};

use dot11::Mac;
use model::{Client, EventKind, Model, WINDOW};
use scan::{Network, Security};
use watch::{Scanner, State, Watch};

/// How often the list asks for a fresh scan while it is shown.
const RESCAN: Duration = Duration::from_secs(10);
/// The actions menu, top to bottom.
const ACTIONS: [&str; 4] = ["Analyze Traffic", "Details", "Security Advice", "Scan Again"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum View {
    List,
    Card,
    Advice,
    Details,
    Actions,
    Traffic,
    Airtime,
    Events,
    Clients,
    Client,
    ClientGraph,
    Peers,
    /// One station's signal, to walk towards it.
    Locate,
}

struct Wifi {
    view: View,
    /// Where Details and Back from it return to.
    back: View,
    scanner: Scanner,
    /// The network under the cursor, by BSSID so a rescan does not move it.
    picked_net: Option<Mac>,
    first: usize,
    advice_page: usize,
    scanned: Instant,
    /// When the app started, for the dots of the first scan.
    started: Instant,
    watch: Option<Watch>,
    lan: lan::Lan,
    picked: Option<Mac>,
    /// The client list as last shown, to keep the cursor on its client.
    listed: Vec<Mac>,
    /// Where Back from Locate goes: the card, which ends the capture, or the client.
    locate_back: View,
    /// Locate's beep while it sounds, and whether it was muted.
    beep: Option<beep::Beep>,
    muted: bool,
    steady: locate::Steady,
}

impl Wifi {
    fn start() -> Self {
        let mut args = std::env::args().skip(1);
        if args.next().as_deref() == Some("--capture") {
            capture::run(&args.next().unwrap_or_default());
        }
        oui::preload();
        let scanner = Scanner::default();
        scanner.scan();
        Self {
            view: View::List,
            back: View::Card,
            scanner,
            picked_net: None,
            first: 0,
            advice_page: 0,
            scanned: Instant::now(),
            started: Instant::now(),
            watch: None,
            lan: lan::Lan::start(),
            picked: None,
            listed: Vec::new(),
            locate_back: View::Card,
            beep: None,
            muted: false,
            steady: locate::Steady::default(),
        }
    }

    fn stop(&mut self) {
        self.watch = None;
        self.picked = None;
    }

    fn nets(&self) -> Vec<Network> {
        self.scanner.nets.lock().unwrap().clone()
    }

    /// The cursor's row, and its network.
    fn selected(&self, nets: &[Network]) -> (usize, Option<Network>) {
        let at = self.picked_net.and_then(|b| nets.iter().position(|n| n.bssid == b)).unwrap_or(0);
        (at, nets.get(at).cloned())
    }

    fn net(&self) -> Option<Network> {
        let nets = self.nets();
        self.selected(&nets).1
    }

    /// A client by its LAN name, else the name it gives itself, else nothing.
    fn own_name(&self, m: Option<&Model>, mac: &Mac) -> Option<String> {
        self.lan.get(mac).name.or_else(|| {
            m.and_then(|m| m.idents.get(mac)).map(|i| i.name.clone()).filter(|n| !n.is_empty())
        })
    }

    /// A client by its name when it has one, else its MAC.
    fn name(&self, m: Option<&Model>, mac: &Mac) -> String {
        self.own_name(m, mac).unwrap_or_else(|| dot11::fmt(mac))
    }

    fn scanning(&self) -> bool {
        self.scanner.scanning.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn status(&self, count: usize) -> String {
        if self.scanning() {
            "Scanning".into()
        } else if watch::onboard_iface().is_none() {
            "No Wi-Fi".into()
        } else {
            format!("{count} networks")
        }
    }

    fn list_surface(&self) -> flipctl_app::paint::Surface {
        let nets = self.nets();
        let (at, _) = self.selected(&nets);
        let rows: Vec<ui::Row> = nets.iter().map(row_of).collect();
        let first = ui::scroll_for(at, self.first, rows.len());
        let empty = if self.scanning() {
            let dots = (self.started.elapsed().as_millis() / 400 % 4) as usize;
            format!("Scanning{}", ".".repeat(dots))
        } else if watch::onboard_iface().is_none() {
            "The Wi-Fi is not available".into()
        } else {
            "No networks found".into()
        };
        ui::list(&rows, at, first, &self.status(nets.len()), &empty)
    }

    /// A modal painted over the list, the status bar dimmed with it and whatever the
    /// modal draws up there put over the bar. What it draws is found by painting it
    /// again over a blank of one grey and keeping what changed.
    fn modal(&self, paint: impl Fn(Surface) -> Surface, dim: u8) -> Page {
        let shown = paint(self.list_surface());
        let mut blank = Surface::panel();
        blank.clear(Gray8(1));
        let probe = paint(blank);
        let ground = probe.get(0, 0);
        let bar = theme::metric::STATUS_BAR_H as u16;
        let over = flipctl_app::overlay(&shown, |x, y| y < bar && probe.get(x, y) != ground);
        Page::canvas(flipctl_app::picture(&shown), vec![]).dim_status(dim).over_status(over)
    }

    /// Locate `who` on the watched network, coming back to `back`.
    fn locate(&mut self, who: Mac, back: View) -> View {
        if let Some(w) = &self.watch {
            w.model.lock().unwrap().locate(Some(who));
        }
        self.locate_back = back;
        if !self.muted {
            self.beep = Some(beep::Beep::start());
        }
        View::Locate
    }

    fn analyze(&mut self) -> View {
        let Some(net) = self.net() else { return self.view };
        self.watch = Some(Watch::start(net));
        View::Traffic
    }

    fn details(&self, n: &Network) -> Page {
        let ssid =
            if n.hidden || n.ssid.is_empty() { "(hidden)".to_string() } else { n.ssid.clone() };
        let mut rows: Vec<Row> = vec![
            heading(fit(&ascii(&ssid), 200), ""),
            pair("BSSID", dot11::fmt(&n.bssid)),
            pair("Vendor", fit(&ascii(&vendor(&n.bssid)), 150)),
        ];
        if let Some(name) = &n.ap_name {
            rows.push(pair("AP Name", fit(&ascii(name), 150)));
        }
        rows.extend([
            pair("Signal", format!("{} dBm", n.signal)),
            pair("Band", band(n.freq)),
            pair("Channel", format!("{}, {} MHz", n.channel(), n.width)),
            pair(
                "Frequency",
                if n.width > 20 {
                    format!("{}, centre {}", n.freq, n.center)
                } else {
                    n.freq.to_string()
                },
            ),
            pair("Protocol", n.protocol),
            pair("Streams", n.streams.to_string()),
            pair("Max Rate", format!("{} Mbit/s", n.max_rate)),
            rule(),
            pair("Security", security_name(n)),
        ]);
        if !n.akm.is_empty() {
            rows.push(pair("Key Management", n.akm.join(" ")));
        }
        if !n.ciphers.is_empty() {
            rows.push(pair("Cipher", n.ciphers.join(" ")));
        }
        if let Some(g) = n.group_cipher {
            rows.push(pair("Group Cipher", g));
        }
        if n.security >= Security::Wpa2 || n.security == Security::Owe {
            let pmf = match n.pmf {
                (_, true) => "Required",
                (true, false) => "Optional",
                _ => "No",
            };
            rows.push(pair("PMF", pmf));
        }
        match &n.wps {
            None => rows.push(pair("WPS", "No")),
            Some(w) => {
                let state = match (w.locked, w.configured) {
                    (true, _) => "Yes, locked",
                    (_, true) => "Yes",
                    _ => "Yes, not set up",
                };
                rows.push(pair("WPS", state));
                if w.push_button {
                    rows.push(pair("WPS Button", "Pressed"));
                }
            }
        }
        let d = &n.device;
        for (label, value) in
            [("Device Name", &d.name), ("Manufacturer", &d.manufacturer), ("Model", &d.model)]
        {
            if !value.is_empty() {
                rows.push(pair(label, fit(&ascii(value), 150)));
            }
        }
        if let Some(kind) = d.kind {
            rows.push(pair("Device Type", kind));
        }
        rows.push(rule());
        rows.push(pair("Clients", n.stations.map_or("-".into(), |s| s.to_string())));
        if let Some(load) = n.load {
            let pct = u32::from(load) * 100 / 255;
            rows.push(gauge("Channel Load", format!("{pct}%"), pct as i32));
        }
        if let Some(cc) = &n.country {
            rows.push(pair("Country", cc.clone()));
        }
        let mut roaming: Vec<&str> = Vec::new();
        for (on, name) in [(n.rrm, "11k"), (n.btm, "11v"), (n.ft, "11r"), (n.mbo, "MBO")] {
            if on {
                roaming.push(name);
            }
        }
        if !roaming.is_empty() {
            rows.push(pair("Roaming", roaming.join(" ")));
        }
        let siblings = siblings(n, &self.nets());
        if !siblings.is_empty() {
            rows.push(pair("Same AP", fit(&ascii(&siblings.join(", ")), 150)));
        }
        if !n.colocated.is_empty() {
            let radios: Vec<String> = n
                .colocated
                .iter()
                .map(|(class, chan)| format!("{} ch {chan}", class_band(*class)))
                .collect();
            rows.push(pair("Other Radios", fit(&radios.join(", "), 150)));
        }
        if n.neighbours > 0 {
            rows.push(pair("Neighbours", n.neighbours.to_string()).dim());
        }
        if let Some(a) = n.access {
            rows.push(pair("Access", access_name(a)));
        }
        if let Some(v) = n.venue.and_then(venue_name) {
            rows.push(pair("Venue", v));
        }
        if n.passpoint {
            rows.push(pair("Hotspot 2.0", "Yes"));
        }
        if let Some(bits) = n.multi_ap {
            rows.push(pair("Mesh", mesh_role(bits)));
        }
        if n.beacon_ms > 0 {
            let dtim = n.dtim.map_or(String::new(), |d| format!(", DTIM {d}"));
            rows.push(pair("Beacon", format!("{} ms{dtim}", n.beacon_ms)).dim());
        }
        if n.uptime > 0 {
            rows.push(pair("AP Uptime", uptime(n.uptime)).dim());
        }
        let chips: Vec<&str> = n.vendors.iter().filter_map(|o| oui::vendor(*o)).collect();
        if !chips.is_empty() {
            rows.push(pair("Chip", fit(&ascii(&short_vendor(&chips.join(", "))), 150)).dim());
        }
        if n.joined {
            rows.push(pair("Flipper", "Connected").dim());
        }
        rows.push(pair("Seen", format!("{}s ago", n.age_ms / 1000)).dim());
        Page::detail(rows).fit_gauges().id("details")
    }

    fn events(&self, m: &Model) -> Page {
        let lines = m
            .events
            .iter()
            .map(|e| {
                let what = match e.kind {
                    EventKind::Heard { again: false } => "heard".to_string(),
                    EventKind::Heard { again: true } => "back".to_string(),
                    EventKind::Quiet => "quiet".to_string(),
                    EventKind::Joined => "joined".to_string(),
                    EventKind::Left { by_ap: true, .. } => "removed by AP".to_string(),
                    EventKind::Left { .. } => "left".to_string(),
                    EventKind::RoamedIn { from } => format!("from {}", dot11::fmt(&from)),
                    EventKind::RoamedOut { to } => format!("to {}", dot11::fmt(&to)),
                };
                let at = m.now.saturating_sub(e.at) / 1_000_000;
                ascii(&format!("{at}s ago {} {what}", self.name(Some(m), &e.who)))
            })
            .collect::<Vec<_>>();
        if lines.is_empty() {
            return Page::log(
                "Events",
                vec![
                    "Clients appearing, joining, leaving".into(),
                    "and going quiet show up here.".into(),
                ],
            );
        }
        Page::log("Events", lines)
    }

    fn traffic(&self, w: &Watch) -> Page {
        let m = w.model.lock().unwrap();
        let n = &w.net;
        let ssid = if n.ssid.is_empty() { "(hidden)" } else { &n.ssid };
        let mut rows = vec![
            heading(fit(&ascii(ssid), 200), ""),
            pair("Channel", format!("{}, {} MHz", n.channel(), n.width)),
        ];
        match w.state() {
            State::Watching => {}
            State::Starting => rows.push(pair("State", "starting")),
            State::Stopped => rows.push(pair("State", "stopped")),
            State::Failed(why) => rows.push(pair("State", fit(&ascii(&why), 150))),
        }
        let share = m.avg(|s| f64::from(s.bss_air)) / 1e4;
        rows.push(gauge("Airtime", format!("{share:.0}%"), share.round() as i32));
        match m.busy() {
            Some(busy) => {
                rows.push(gauge("Channel Busy", format!("{busy:.0}%"), busy.round() as i32))
            }
            None => {
                let heard = m.avg(|s| f64::from(s.air)) / 1e4;
                rows.push(gauge("Channel Used", format!("{heard:.0}%"), heard.round() as i32));
            }
        }
        rows.push(pair("Down", rate(m.avg(|s| s.down as f64))));
        rows.push(pair("Up", rate(m.avg(|s| s.up as f64))));
        rows.push(pair("Clients", format!("{} active, {} seen", m.active(), m.clients.len())));
        rows.push(
            pair(
                "Frames/s",
                format!(
                    "{:.0} down, {:.0} up",
                    m.avg(|s| f64::from(s.frames_down)),
                    m.avg(|s| f64::from(s.frames_up))
                ),
            )
            .dim(),
        );
        rows.push(
            pair("AP Signal", m.ap_signal.map_or("-".into(), |s| format!("{s:.0} dBm"))).dim(),
        );
        rows.push(pair("Beacons/s", format!("{:.0}", m.avg(|s| f64::from(s.beacons)))).dim());
        let ours = m.avg(|s| f64::from(s.bss_air));
        if ours > 0.0 {
            let bad = 100.0 * m.avg(|s| f64::from(s.bad_bss_air)) / ours;
            rows.push(pair("Undecoded", format!("{bad:.0}% of airtime")).dim());
        }
        if !m.mesh.is_empty() {
            rows.push(pair("Mesh Links", m.mesh.len().to_string()).dim());
        }
        if m.dropped > 0 {
            rows.push(pair("Missed by App", m.dropped.to_string()).dim());
        }
        Page::detail(rows).fit_gauges().id("traffic")
    }

    fn clients(&self, m: &Model) -> Page {
        let ranked = m.listed();
        self.lan.want(ranked.iter().copied());
        if ranked.is_empty() {
            return Page::menu(vec![item("No clients yet")]).id("clients");
        }
        let items = ranked
            .iter()
            .map(|mac| {
                let c = &m.clients[mac];
                let quiet = m.now.saturating_sub(c.seen) > WINDOW as u64 * 1_000_000;
                let mut status = if quiet {
                    format!("{}s ago", m.now.saturating_sub(c.seen) / 1_000_000)
                } else {
                    short_rate(c.avg(|s| (s.bytes_down + s.bytes_up) as f64))
                };
                if let Some(sig) = c.signal {
                    status.push_str(&format!(" {sig:.0}"));
                }
                let who = identify(m, mac);
                let label = match (self.own_name(Some(m), mac), who.model) {
                    (Some(name), _) => name,
                    (None, Some(model)) => format!("{model} {}", dot11::fmt(mac)),
                    (None, None) if dot11::is_local(mac) => dot11::fmt(mac),
                    (None, None) => format!("{} {}", vendor(mac), dot11::fmt(mac)),
                };
                let mut it =
                    item(fit(&ascii(&label), 132)).status(status).icon(icons::for_kind(who.kind));
                if dot11::is_local(mac) {
                    it = it.tag("R");
                }
                if quiet {
                    it = it.dim();
                }
                it
            })
            .collect();
        Page::menu(items).id("clients")
    }

    fn client(&self, m: &Model, mac: &Mac) -> Page {
        let Some(c) = m.clients.get(mac) else {
            return Page::detail(vec![text("Gone from the list")]).id("client");
        };
        let known = self.lan.get(mac);
        let said = m.idents.get(mac).cloned().unwrap_or_default();
        let mut rows = vec![heading(fit(&ascii(&self.name(Some(m), mac)), 200), "")];
        if self.own_name(Some(m), mac).is_some() {
            rows.push(pair("MAC", dot11::fmt(mac)));
        }
        let maker =
            (!dot11::is_local(mac)).then(|| oui::vendor([mac[0], mac[1], mac[2]])).flatten();
        let vendor = if dot11::is_local(mac) { "random MAC" } else { maker.unwrap_or("-") };
        rows.push(pair("Vendor", fit(&ascii(vendor), 150)));
        let who = identify(m, mac);
        if let Some(kind) = who.kind {
            let kind = if who.guessed { format!("{kind}, guessed") } else { kind.to_string() };
            rows.push(pair("Type", fit(&ascii(&kind), 150)));
        }
        let model = [said.manufacturer.as_str(), said.model.as_str()].join(" ");
        if !model.trim().is_empty() {
            rows.push(pair("Model", fit(&ascii(model.trim()), 150)));
        } else if let Some(model) = who.model {
            rows.push(pair("Model", fit(&ascii(model), 150)));
        }
        if let Some(p) = said.platform {
            rows.push(pair("Platform", p));
        }
        rows.push(pair("Signal", c.signal.map_or("-".into(), |s| format!("{s:.0} dBm"))));
        let share = c.avg(|s| f64::from(s.air)) / 1e4;
        rows.push(gauge("Airtime", format!("{share:.0}%"), share.round() as i32));
        rows.push(pair("Down", rate(c.avg(|s| s.bytes_down as f64))));
        rows.push(pair("Up", rate(c.avg(|s| s.bytes_up as f64))));
        rows.push(pair("Activity", c.activity.shown.name()));
        rows.push(pair("Device", device(c)));
        if let Some(ip) = known.ip {
            rows.push(pair("IP", ip.to_string()));
        }
        rows.push(pair("Down Link", c.down.map_or("-".into(), link)));
        rows.push(pair("Up Link", c.up.map_or("-".into(), link)));
        rows.push(pair("Retries", retries(c)));
        rows.push(pair("Power Save", power_save(c)));
        if let Some(class) = main_class(c) {
            rows.push(pair("Traffic Class", class));
        }
        if let Some(net) = internet(c, self.lan.gateway()) {
            rows.push(pair("Internet", net));
        }
        rows.push(
            pair(
                "Frames/s",
                format!(
                    "{:.0} down, {:.0} up",
                    c.avg(|s| f64::from(s.frames_down)),
                    c.avg(|s| f64::from(s.frames_up))
                ),
            )
            .dim(),
        );
        let phy = c.down.or(c.up).map(|l| l.rate).unwrap_or(0.0);
        rows.push(
            pair("PHY Rate", if phy > 0.0 { format!("{phy:.0} Mbit/s") } else { "-".into() }).dim(),
        );
        rows.push(
            pair(
                "Total",
                format!(
                    "{:.1} MB down, {:.1} MB up",
                    c.total_down as f64 / 1e6,
                    c.total_up as f64 / 1e6
                ),
            )
            .dim(),
        );
        if let Some(caps) = &c.caps {
            let mut f: Vec<&str> = Vec::new();
            for (on, name) in [
                (caps.rrm, "11k"),
                (caps.btm, "11v"),
                (caps.ft, "11r"),
                (caps.pmf, "PMF"),
                (caps.twt, "TWT"),
            ] {
                if on {
                    f.push(name);
                }
            }
            if !f.is_empty() {
                rows.push(pair("Features", f.join(" ")).dim());
            }
            let chips: Vec<&str> = caps.vendors.iter().filter_map(|o| oui::vendor(*o)).collect();
            if !chips.is_empty() {
                rows.push(pair("Chip", fit(&ascii(&chips.join(", ")), 150)).dim());
            }
            if caps.listen > 0 {
                rows.push(pair("Listen Interval", format!("{} beacons", caps.listen)).dim());
            }
        }
        if let Some(p) = c.print.or_else(|| m.probe_prints.get(mac).copied()) {
            rows.push(pair("Fingerprint", format!("{p:016X}")).dim());
        }
        if let Some(e) = m.events.iter().rev().find(|e| e.who == *mac) {
            let what = match e.kind {
                EventKind::Heard { again: false } => "heard".to_string(),
                EventKind::Heard { again: true } => "back".to_string(),
                EventKind::Quiet => "quiet".to_string(),
                EventKind::Joined => "joined".to_string(),
                EventKind::Left { by_ap: true, .. } => "removed by AP".to_string(),
                EventKind::Left { .. } => "left".to_string(),
                EventKind::RoamedIn { from } => format!("from {}", dot11::fmt(&from)),
                EventKind::RoamedOut { to } => format!("to {}", dot11::fmt(&to)),
            };
            let at = m.now.saturating_sub(e.at) / 1_000_000;
            rows.push(pair("Last Event", fit(&format!("{what}, {at}s ago"), 150)).dim());
        }
        rows.push(
            pair("Last Seen", format!("{}s ago", m.now.saturating_sub(c.seen) / 1_000_000)).dim(),
        );
        Page::detail(rows).fit_gauges().id("client")
    }

    fn peers(&self, m: &Model, mac: &Mac) -> Page {
        let Some(c) = m.clients.get(mac) else {
            return Page::menu(vec![item("Gone from the list")]).id("peers");
        };
        let gateway = self.lan.gateway();
        let mut peers: Vec<(&Mac, &[u64; 2])> = c.peers.iter().collect();
        // A fixed order, so the list holds still: internet, AP, broadcast, then by MAC
        let rank = |p: &Mac| match p {
            p if Some(*p) == gateway => 0,
            p if *p == m.bssid => 1,
            &model::GROUP => 2,
            _ => 3,
        };
        peers.sort_by_key(|(p, _)| (rank(p), **p));
        if peers.is_empty() {
            return Page::menu(vec![item("No peers yet")]).id("peers");
        }
        let items = peers
            .into_iter()
            .map(|(p, b)| {
                let label = if *p == model::GROUP {
                    "Broadcast".to_string()
                } else if Some(*p) == gateway {
                    "Internet".to_string()
                } else if *p == m.bssid {
                    "AP".to_string()
                } else {
                    self.name(Some(m), p)
                };
                // [0] the client sent to it, [1] it sent to the client. One that only
                // ever sent is usually multicast the AP turned into unicast
                let it = item(fit(&ascii(&label), 120));
                if b[0] == 0 {
                    it.status(format!("{} in", bytes(b[1]))).dim()
                } else {
                    it.status(format!("{} in {} out", bytes(b[1]), bytes(b[0])))
                }
            })
            .collect();
        Page::menu(items).id("peers")
    }
}

impl App for Wifi {
    fn page(&self) -> Page {
        let empty = || Page::canvas(flipctl_app::picture(&self.list_surface()), vec![]);
        match self.view {
            View::List => return empty(),
            View::Card | View::Advice | View::Details | View::Actions => {
                let Some(n) = self.net() else { return empty() };
                return match self.view {
                    View::Card => {
                        let c = card_of(&n);
                        self.modal(|under| ui::card(under, &c), ui::CARD_SCRIM)
                    }
                    View::Advice => {
                        let all = advice(&n);
                        let a = &all[self.advice_page % all.len()];
                        let more = all.len() > 1;
                        self.modal(|under| ui::dolphin(under, a, more), ui::ADVICE_SCRIM)
                    }
                    View::Details => self.details(&n),
                    _ => Page::menu(ACTIONS.iter().map(|a| item(*a)).collect()).id("actions"),
                };
            }
            _ => {}
        }
        let Some(w) = self.watch.as_ref() else { return empty() };
        if self.view == View::Traffic {
            return self.traffic(w);
        }
        let m = w.model.lock().unwrap();
        let picked = self.picked.unwrap_or_default();
        match self.view {
            View::Airtime => graph::airtime(&m),
            View::Events => self.events(&m),
            View::Clients => self.clients(&m),
            View::Client => self.client(&m, &picked),
            View::ClientGraph => match m.clients.get(&picked) {
                Some(c) => graph::traffic(c),
                None => self.client(&m, &picked),
            },
            View::Peers => self.peers(&m, &picked),
            View::Locate => {
                let name = if self.locate_back == View::Card {
                    if w.net.ssid.is_empty() {
                        dot11::fmt(&w.net.bssid)
                    } else {
                        w.net.ssid.clone()
                    }
                } else {
                    self.name(Some(&m), &picked)
                };
                locate::page(&m, &name)
            }
            _ => unreachable!(),
        }
    }

    fn buttons(&self) -> [&str; 5] {
        match self.view {
            View::List => ["", "", "", "Actions", ""],
            View::Card => ["", "Help", "", "More", "Locate"],
            View::Advice => ["", "", "", "More", "Analyze"],
            View::Details => ["", "", "", "", "Analyze"],
            View::Actions => ["", "", "", "", ""],
            View::Traffic => ["", "Clients", "", "Events", "Graph"],
            View::Airtime => ["", "Clients", "", "Events", "Rows"],
            View::Events => ["", "Clients", "", "Rows", "Graph"],
            View::Clients => ["", "Traffic", "", "", ""],
            View::Client => ["", "Peers", "", "Locate", "Graph"],
            View::ClientGraph => ["", "Peers", "", "", "Rows"],
            View::Peers => ["", "Client", "", "", ""],
            View::Locate => ["", "", "", if self.muted { "Unmute" } else { "Mute" }, ""],
        }
    }

    fn key(&mut self, key: Key, ui: &mut Ui) -> bool {
        use View::*;
        let next = match (self.view, key) {
            (List, Key::Up | Key::Down) => {
                let nets = self.nets();
                if nets.is_empty() {
                    return true;
                }
                let (at, _) = self.selected(&nets);
                let at = match key {
                    Key::Up => (at + nets.len() - 1) % nets.len(),
                    _ => (at + 1) % nets.len(),
                };
                self.first = ui::scroll_for(at, self.first, nets.len());
                self.picked_net = Some(nets[at].bssid);
                return true;
            }
            (List, Key::Ok) if self.net().is_some() => Card,
            (List, Key::Edit) if self.net().is_some() => {
                ui.select(0);
                Actions
            }
            (Card, Key::View) => {
                self.advice_page = 0;
                Advice
            }
            (Card, Key::Ok) | (Card | Advice, Key::Edit) => {
                self.back = self.view;
                ui.scroll_to(0);
                Details
            }
            (Card | Actions, Key::Back) => List,
            (Card, Key::Run) => {
                let Some(n) = self.net() else { return true };
                self.analyze();
                self.locate(n.bssid, Card)
            }
            (Locate, Key::Edit) => {
                self.muted = !self.muted;
                self.beep = (!self.muted).then(beep::Beep::start);
                return true;
            }
            (Locate, Key::Back) if self.locate_back == Card => {
                self.beep = None;
                self.stop();
                self.scanner.scan();
                self.scanned = Instant::now();
                Card
            }
            (Locate, Key::Back) => {
                self.beep = None;
                if let Some(w) = &self.watch {
                    w.model.lock().unwrap().locate(None);
                }
                self.locate_back
            }
            (Client, Key::Edit) => {
                let Some(who) = self.picked else { return true };
                self.locate(who, Client)
            }
            (Advice, Key::Ok | Key::Right) => {
                self.advice_page += 1;
                return true;
            }
            (Advice, Key::Left) => {
                self.advice_page = self.advice_page.saturating_sub(1);
                return true;
            }
            (Advice, Key::Back) => Card,
            (Advice | Details, Key::Run) | (Details, Key::Ok) => self.analyze(),
            (Details, Key::Back) => self.back,
            (Actions, Key::Ok) => match ui.selected() {
                0 => self.analyze(),
                1 => {
                    self.back = List;
                    ui.scroll_to(0);
                    Details
                }
                2 => {
                    self.advice_page = 0;
                    Advice
                }
                _ => {
                    self.scanner.scan();
                    self.scanned = Instant::now();
                    List
                }
            },
            (Traffic | Airtime | Events, Key::Back) => {
                self.stop();
                self.scanner.scan();
                self.scanned = Instant::now();
                List
            }
            (Traffic | Airtime | Events, Key::View) => Clients,
            (Traffic | Events, Key::Run) => Airtime,
            (Airtime, Key::Run) | (Events, Key::Edit) => Traffic,
            (Traffic | Airtime, Key::Edit) => Events,
            (Clients, Key::Back | Key::View) => Traffic,
            (Clients, Key::Ok) => {
                let ranked = self
                    .watch
                    .as_ref()
                    .map(|w| w.model.lock().unwrap().listed())
                    .unwrap_or_default();
                let Some(mac) = ranked.get(ui.selected().min(ranked.len().saturating_sub(1)))
                else {
                    return true;
                };
                self.picked = Some(*mac);
                ui.scroll_to(0);
                Client
            }
            (Client, Key::Back) => Clients,
            (Client | ClientGraph, Key::View) => Peers,
            (Client, Key::Run) => ClientGraph,
            (ClientGraph, Key::Back | Key::Run) => Client,
            (Peers, Key::Back | Key::View) => Client,
            _ => return false,
        };
        self.view = next;
        true
    }

    fn every(&self) -> Option<Duration> {
        // Quick enough for the dots while the first scan runs
        if self.view == View::Locate {
            return Some(Duration::from_millis(50));
        }
        let waiting = self.view == View::List
            && self.scanning()
            && self.scanner.nets.lock().unwrap().is_empty();
        Some(Duration::from_millis(if waiting { 400 } else { 1000 }))
    }

    fn tick(&mut self, ui: &mut Ui) {
        if let (View::Locate, Some(b), Some(w)) = (self.view, &self.beep, &self.watch) {
            let dbm = locate::reading(&w.model.lock().unwrap(), 0).current();
            b.set(self.steady.update(dbm).map(locate::level));
        }
        if self.view == View::Clients {
            let order =
                self.watch.as_ref().map(|w| w.model.lock().unwrap().listed()).unwrap_or_default();
            let under = self.listed.get(ui.selected()).copied();
            if let Some(at) = under.and_then(|mac| order.iter().position(|o| *o == mac)) {
                if at != ui.selected() {
                    ui.select(at);
                }
            }
            self.listed = order;
        }
        if self.view == View::List && self.watch.is_none() && self.scanned.elapsed() >= RESCAN {
            self.scanned = Instant::now();
            self.scanner.scan();
        }
    }

    fn close(&mut self) {
        self.beep = None;
        self.stop();
    }
}

/// A network as its list row shows it.
fn row_of(n: &Network) -> ui::Row {
    let note = if n.wps.as_ref().is_some_and(|w| !w.locked) {
        "WPS".to_string()
    } else {
        let akm = n.akm.first().copied().unwrap_or("");
        let cipher = n.ciphers.first().copied().unwrap_or("");
        format!("{akm} {cipher}").trim().to_string()
    };
    ui::Row {
        ssid: if n.hidden || n.ssid.is_empty() { "(hidden)".into() } else { n.ssid.clone() },
        sub: format!("{}  Channel {}  {}MHz", band(n.freq), n.channel(), n.width),
        level: match n.signal {
            s if s >= -60 => 3,
            s if s >= -70 => 2,
            s if s >= -80 => 1,
            _ => 0,
        },
        lock: n.security != Security::Open,
        pill: n.security.name().into(),
        warn: weak(n),
        note: if n.security == Security::Open { String::new() } else { note },
    }
}

/// WEP, or WPA with nothing better than TKIP.
fn weak(n: &Network) -> bool {
    n.security == Security::Wep || (n.security == Security::Wpa && !n.ciphers.contains(&"AES"))
}

fn security_name(n: &Network) -> String {
    match n.security {
        Security::Open | Security::Wep | Security::Owe => n.security.name().to_string(),
        s => {
            let akm = n.akm.first().copied().unwrap_or("");
            format!("{} {akm}", s.name()).trim().to_string()
        }
    }
}

fn card_of(n: &Network) -> ui::Card {
    let r = |l: &str, v: String, w| (l.to_string(), v, w);
    let wps = match &n.wps {
        None => "No".to_string(),
        Some(w) if w.locked => "Locked".to_string(),
        Some(_) => "Yes".to_string(),
    };
    ui::Card {
        ssid: if n.hidden || n.ssid.is_empty() { "(hidden)".into() } else { n.ssid.clone() },
        lock: n.security != Security::Open,
        signal: n.signal,
        bars: match n.signal {
            s if s >= -55 => 4,
            s if s >= -65 => 3,
            s if s >= -75 => 2,
            s if s >= -85 => 1,
            _ => 0,
        },
        line: format!("{}  Channel {}  {}MHz", band(n.freq), n.channel(), n.width),
        rows: vec![
            r("Security:", security_name(n), weak(n)),
            r("WPS:", wps, false),
            r("Protocol:", n.protocol.to_string(), false),
            r("BSSID:", dot11::fmt(&n.bssid), false),
            r("Vendor:", vendor(&n.bssid), false),
            r("Clients:", n.stations.map_or("-".into(), |s| s.to_string()), false),
        ],
    }
}

/// What the dolphin has to say about a network, worst first. Never empty.
fn advice(n: &Network) -> Vec<ui::Advice> {
    let ssid = if n.hidden || n.ssid.is_empty() { "(hidden)".to_string() } else { n.ssid.clone() };
    let say = |heading: &str, body: &str| ui::Advice {
        ssid: ssid.clone(),
        heading: heading.into(),
        body: body.into(),
    };
    let mut out = Vec::new();
    match n.security {
        Security::Wep => out.push(say(
            "Weak security!",
            "WEP encryption is flipping ancient! Anyone can hack this network.",
        )),
        Security::Wpa if weak(n) => out.push(say(
            "Weak security!",
            "WPA with TKIP is long broken. The router should use WPA2 or WPA3.",
        )),
        Security::Open => out.push(say(
            "No password!",
            "Anyone nearby can join, and read whatever apps send unencrypted.",
        )),
        _ => {}
    }
    if n.wps.as_ref().is_some_and(|w| !w.locked) {
        out.push(say(
            "WPS is on!",
            "A WPS PIN can often be guessed in hours. Better turn WPS off.",
        ));
    }
    if n.security == Security::Wpa2 && !n.pmf.0 {
        out.push(say(
            "Could be better",
            "WPA3 or PMF would stop tricks that kick devices off this network.",
        ));
    }
    if out.is_empty() {
        out.push(say(
            "Looks good!",
            "Modern encryption. Keep the password long and nobody else's.",
        ));
    }
    out
}

fn band(freq: u32) -> &'static str {
    match freq {
        f if f < 3000 => "2.4GHz",
        f if f >= 5950 => "6GHz",
        _ => "5GHz",
    }
}

/// What a client is, as far as can be told.
struct Who {
    kind: Option<&'static str>,
    /// The kind is a guess from its capabilities.
    guessed: bool,
    /// A model from the signature table.
    model: Option<&'static str>,
}

/// A client's kind from what it says, else its signatures, else a guess.
fn identify(m: &Model, mac: &Mac) -> Who {
    let c = m.clients.get(mac);
    let table = ident::known(c.and_then(|c| c.print), m.probe_prints.get(mac).copied());
    let said = m.idents.get(mac).and_then(|i| i.kind);
    let maker = (!dot11::is_local(mac)).then(|| oui::vendor([mac[0], mac[1], mac[2]])).flatten();
    let guess = ident::guess(c.and_then(|c| c.caps.as_ref()), maker, dot11::is_local(mac));
    let kind = said.or(table.map(|(_, k)| k));
    Who {
        kind: kind.or(guess),
        guessed: kind.is_none() && guess.is_some(),
        model: table.map(|(n, _)| n),
    }
}

/// The other networks of the same AP, by name and band. An AP numbers its BSSIDs from
/// one base: the same but for the last byte, or the first byte's locally administered
/// bits.
fn siblings(n: &Network, all: &[Network]) -> Vec<String> {
    let near = |a: &Mac, b: &Mac| {
        let mid = a[1..5] == b[1..5];
        let first = a[0] | 0x0e == b[0] | 0x0e;
        mid && first && a[5].abs_diff(b[5]) < 16
    };
    all.iter()
        .filter(|o| o.bssid != n.bssid && near(&o.bssid, &n.bssid))
        .map(|o| {
            let ssid = if o.ssid.is_empty() { "(hidden)" } else { &o.ssid };
            format!("{ssid} {}", band(o.freq))
        })
        .collect()
}

/// The band an operating class is in.
fn class_band(class: u8) -> &'static str {
    match class {
        81..=84 => "2.4GHz",
        131..=137 => "6GHz",
        _ => "5GHz",
    }
}

fn access_name(kind: u8) -> &'static str {
    match kind {
        0 => "Private",
        1 => "Private, Guest",
        2 => "Paid Public",
        3 => "Free Public",
        4 => "Personal Device",
        5 => "Emergency",
        _ => "-",
    }
}

fn venue_name(group: u8) -> Option<&'static str> {
    Some(match group {
        1 => "Assembly",
        2 => "Business",
        3 => "Education",
        4 => "Industry",
        5 => "Institution",
        6 => "Shop",
        7 => "Residence",
        8 => "Storage",
        9 => "Utility",
        10 => "Vehicle",
        11 => "Outdoor",
        _ => return None,
    })
}

/// EasyMesh roles: backhaul to a parent, backhaul for children, clients.
fn mesh_role(bits: u8) -> String {
    let mut roles = Vec::new();
    for (bit, name) in [(0x20, "Fronthaul"), (0x40, "Backhaul"), (0x80, "Backhaul Client")] {
        if bits & bit != 0 {
            roles.push(name);
        }
    }
    if roles.is_empty() {
        "EasyMesh".into()
    } else {
        roles.join(", ")
    }
}

/// The registered vendor of a MAC, shortened to its first word.
fn vendor(mac: &Mac) -> String {
    if dot11::is_local(mac) {
        return "Random MAC".into();
    }
    oui::vendor([mac[0], mac[1], mac[2]]).map_or("-".into(), short_vendor)
}

fn short_vendor(name: &str) -> String {
    name.split([' ', ',']).next().unwrap_or(name).to_string()
}

fn uptime(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{} min", s / 60),
        s if s < 86400 => format!("{} h {} min", s / 3600, s % 3600 / 60),
        s => format!("{} days", s / 86400),
    }
}

/// Printable ASCII only: the panel's fonts have nothing else.
fn ascii(s: &str) -> String {
    s.chars().map(|c| if (' '..='~').contains(&c) { c } else { '?' }).collect()
}

/// Bytes a second as a rate, in kbit/s below a megabit.
pub fn rate(bytes: f64) -> String {
    let bits = bytes * 8.0;
    if bits < 1e6 {
        format!("{:.0} kbit/s", bits / 1e3)
    } else if bits < 1e8 {
        format!("{:.1} Mbit/s", bits / 1e6)
    } else {
        format!("{:.0} Mbit/s", bits / 1e6)
    }
}

/// The same, as short as a list row needs.
/// A byte count in three characters or so: 512, 12K, 3.4M.
fn bytes(n: u64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{}K", n / 1000),
        1_000_000..=9_999_999 => format!("{:.1}M", n as f64 / 1e6),
        _ => format!("{}M", n / 1_000_000),
    }
}

pub fn short_rate(bytes: f64) -> String {
    let bits = bytes * 8.0;
    if bits < 1e6 {
        format!("{:.0}k", bits / 1e3)
    } else {
        format!("{:.1}M", bits / 1e6)
    }
}

fn link(l: model::Link) -> String {
    let mcs = if l.standard == radiotap::Standard::Legacy {
        format!("{:.0}M", l.rate)
    } else {
        format!("MCS{} {}x {}MHz", l.mcs, l.nss, l.width)
    };
    format!("{} {mcs}", l.standard.name())
}

fn retries(c: &Client) -> String {
    let share =
        |r: f64, d: f64| if d > 0.0 { format!("{:.0}%", 100.0 * r / d) } else { "-".into() };
    let (rd, dd) = (c.avg(|s| f64::from(s.retries_down)), c.avg(|s| f64::from(s.data_down)));
    let (ru, du) = (c.avg(|s| f64::from(s.retries_up)), c.avg(|s| f64::from(s.data_up)));
    format!("{} down, {} up", share(rd, dd), share(ru, du))
}

fn power_save(c: &Client) -> String {
    let n = c.secs.len().min(WINDOW * 6);
    let asleep: u64 = c.recent(n).map(|s| s.asleep).sum();
    let share = if n > 0 { 100 * asleep / (n as u64 * 1_000_000) } else { 0 };
    let now = if c.asleep() { "Asleep" } else { "Awake" };
    format!("{now}, {share}% asleep")
}

fn main_class(c: &Client) -> Option<String> {
    let total: u64 = c.classes.iter().sum();
    let (i, top) = c.classes.iter().enumerate().max_by_key(|(_, b)| **b)?;
    if total == 0 {
        return None;
    }
    let name = ["Background", "Best Effort", "Video", "Voice"][i];
    Some(format!("{name} {}%", 100 * top / total))
}

/// The share of its bytes that went to or came from the default gateway.
fn internet(c: &Client, gateway: Option<Mac>) -> Option<String> {
    let gw = c.peers.get(&gateway?)?;
    let total: u64 = c.peers.values().map(|b| b[0] + b[1]).sum();
    (total > 0).then(|| format!("{}% of bytes", 100 * (gw[0] + gw[1]) / total))
}

fn device(c: &Client) -> String {
    match &c.caps {
        Some(caps) => {
            let mut s = format!("{}, {}x{}", caps.generation(), caps.nss, caps.nss);
            if caps.width > 20 {
                s.push_str(&format!(", {} MHz", caps.width));
            }
            s
        }
        None => "-".into(),
    }
}

flipctl_app::main!(Wifi::start());
#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::{Ethernet, Status};

    #[test]
    fn rates_read_in_kbit_below_a_megabit() {
        assert_eq!(rate(1_000.0), "8 kbit/s");
        assert_eq!(rate(250_000.0), "2.0 Mbit/s");
        assert_eq!(short_rate(50_000.0), "400k");
    }

    fn net() -> scan::Network {
        scan::Network {
            bssid: model::tests::AP,
            ssid: "My_Home".into(),
            freq: 2437,
            width: 20,
            center: 2437,
            signal: -45,
            joined: true,
            ..Default::default()
        }
    }

    /// A model fed a few seconds of a call-like client and a deauth.
    fn busy_model() -> Model {
        let mut m = Model::new(model::tests::AP);
        let mut at = 0;
        for sec in 0..8u64 {
            for i in 0..40 {
                at = sec * 1_000_000 + i * 20_000;
                let f = model::tests::data(i % 2 == 0, 6, 160, 0);
                m.frame(at, f.len(), &f);
            }
            m.survey(sec * 1000 + 1000, sec * 300 + 300, Some(-92), 0);
        }
        m.now = at;
        // More clients, one of each way of telling what it is
        let kinds: [(Mac, Option<&str>, Option<&'static str>); 5] = [
            ([0x3c, 0x28, 0x6d, 0x11, 0x22, 0x33], Some("Pixel 8"), Some("Phone")),
            ([0x30, 0x24, 0xa9, 0x81, 0x98, 0xcd], Some("HP M15 LaserJet"), Some("Printer")),
            ([0xb0, 0x60, 0x88, 0x18, 0xd1, 0xfc], None, Some("Laptop")),
            ([0x24, 0x0a, 0xc4, 0x5e, 0x10, 0x07], None, None),
            ([0xde, 0x11, 0x4b, 0x52, 0x55, 0x01], None, None),
        ];
        for (i, (mac, name, kind)) in kinds.into_iter().enumerate() {
            let caps = (i == 3).then(|| dot11::Caps {
                ht: true,
                nss: 1,
                band_2g: true,
                ..Default::default()
            });
            let mut c = Client::default();
            (c.first, c.seen, c.caps) = (1 + i as u64, at, caps);
            m.clients.insert(mac, c);
            let said =
                ident::Ident { name: name.unwrap_or_default().into(), kind, ..Default::default() };
            if !said.is_empty() {
                m.idents.insert(mac, said);
            }
        }
        m
    }

    /// A network like the design's first, as a scan would give it.
    fn design_net() -> Network {
        Network {
            security: Security::Wep,
            wps: Some(scan::Wps { configured: true, ..Default::default() }),
            protocol: "802.11g",
            stations: Some(3),
            streams: 1,
            max_rate: 54,
            beacon_ms: 102,
            uptime: 90_061,
            ..net()
        }
    }

    fn app_on(view: View) -> Wifi {
        let scanner = Scanner::default();
        let more = |ssid: &str, freq, width, signal, security, akm: &'static str, last| Network {
            bssid: [2, 0, 0, 0, 0, last],
            ssid: ssid.into(),
            freq,
            width,
            signal,
            security,
            akm: vec![akm],
            ciphers: vec!["AES"],
            ..Default::default()
        };
        scanner.nets.lock().unwrap().extend([
            design_net(),
            Network {
                akm: vec![],
                ciphers: vec![],
                ..more("Cafe-WiFi", 2412, 20, -62, Security::Open, "", 1)
            },
            more("YourMomGuest", 5600, 80, -64, Security::Wpa3, "SAE", 2),
            more("Office_Guest", 5180, 80, -75, Security::Wpa3, "SAE", 3),
            more("Lab", 5745, 80, -82, Security::Wpa2, "PSK", 4),
            more("Printer", 2437, 20, -85, Security::Wpa2, "PSK", 5),
        ]);
        let mut watch = Watch::start(net());
        *watch.model.lock().unwrap() = busy_model();
        if view == View::Locate {
            // Walking towards it: weak to strong over the last twenty seconds, gaps between
            let mut m = watch.model.lock().unwrap();
            m.locate(Some(model::tests::STA));
            m.now = 30_000_000;
            let now = m.now;
            for i in 0..100u64 {
                if i % 7 != 3 {
                    m.samples.push_back((
                        now - (100 - i) * 200_000,
                        (-82 + (i * 30 / 100) as i64) as i8,
                    ));
                }
            }
        }
        *watch.state.lock().unwrap() = State::Watching;
        watch.net = net();
        Wifi {
            view,
            back: View::Card,
            scanner,
            picked_net: None,
            first: 0,
            advice_page: 0,
            scanned: Instant::now(),
            started: Instant::now(),
            watch: Some(watch),
            lan: lan::Lan::start(),
            picked: Some(model::tests::STA),
            listed: Vec::new(),
            locate_back: View::Client,
            beep: None,
            muted: false,
            steady: locate::Steady::default(),
        }
    }

    fn shot(name: &str, view: View) -> Vec<u8> {
        let status = Status { battery: 76, ethernet: Ethernet::Real, ..Status::default() };
        let frame = flipctl_pages::shot(app_on(view), status);
        if std::env::var_os("WIFI_RENDER").is_some() {
            let path = std::path::Path::new("target/render").join(format!("{name}.png"));
            flipctl_app::render::save(&path, &frame);
        }
        frame
    }

    /// The first scan, before any network is in: the list says so.
    #[test]
    fn the_empty_list_says_it_is_scanning() {
        let app = app_on(View::List);
        app.scanner.nets.lock().unwrap().clear();
        app.scanner.scanning.store(true, std::sync::atomic::Ordering::SeqCst);
        let status = Status { battery: 76, ethernet: Ethernet::Real, ..Status::default() };
        let frame = flipctl_pages::shot(app, status);
        if std::env::var_os("WIFI_RENDER").is_some() {
            flipctl_app::render::save(std::path::Path::new("target/render/list-empty.png"), &frame);
        }
        let inked = frame[70 * 256..82 * 256].iter().filter(|p| **p == 0).count();
        assert!(inked > 20, "no text where the list would be");
    }

    /// Every pixel holds a design-token value. Exempt, as in flipctl's own render tests:
    /// sprite boxes, whose edges are greys of their own; the list's fade under the soft
    /// keys; and what lies behind a modal, which is a composite. The status bar is
    /// skipped for its sprites.
    #[test]
    fn every_page_is_drawn_only_in_tokens() {
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        for (name, view) in [
            ("list", View::List),
            ("card", View::Card),
            ("advice", View::Advice),
            ("details", View::Details),
            ("actions", View::Actions),
            ("traffic", View::Traffic),
            ("airtime", View::Airtime),
            ("events", View::Events),
            ("clients", View::Clients),
            ("client", View::Client),
            ("client-graph", View::ClientGraph),
            ("peers", View::Peers),
            ("locate", View::Locate),
        ] {
            let frame = shot(name, view);
            // The region held to the tokens, and the sprite boxes inside it
            type Boxes = &'static [(i32, i32, i32, i32)];
            let (areas, sprites): (Boxes, Boxes) = match view {
                View::List => (&[(0, 13, 256, 123)], &[(2, 14, 16, 28)]),
                View::Card => (
                    &[(50, 13, 124, 25), (50, 25, 203, 27), (50, 27, 206, 123)],
                    &[(61, 34, 83, 49)],
                ),
                View::Advice => (&[], &[]),
                _ => (&[(0, 13, 256, 144)], &[]),
            };
            let inside = |x: i32, y: i32, (x0, y0, x1, y1): (i32, i32, i32, i32)| {
                x >= x0 && x < x1 && y >= y0 && y < y1
            };
            let stride = usize::from(theme::PANEL_W);
            let off: Vec<String> = frame
                .iter()
                .enumerate()
                .map(|(i, px)| ((i % stride) as i32, (i / stride) as i32, *px))
                .filter(|(x, y, _)| {
                    areas.iter().any(|a| inside(*x, *y, *a))
                        && !sprites.iter().any(|b| inside(*x, *y, *b))
                })
                .filter(|(_, _, px)| !allowed.contains(px))
                .take(6)
                .map(|(x, y, px)| format!("({x}, {y}) = {px}"))
                .collect();
            assert!(off.is_empty(), "{name}: off-palette pixels: {off:?}");
        }
    }

    /// What the parsers make of a real capture: `WIFI_PCAP=x.pcap cargo test pcap -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn pcap_report() {
        let Ok(path) = std::env::var("WIFI_PCAP") else { return };
        let b = std::fs::read(path).unwrap();
        let mut at = 24;
        let mut seen = std::collections::BTreeSet::new();
        while at + 16 <= b.len() {
            let n = u32::from_le_bytes(b[at + 8..at + 12].try_into().unwrap()) as usize;
            let f = &b[at + 16..at + 16 + n];
            at += 16 + n;
            let Some(phy) = radiotap::parse(f) else { continue };
            if phy.flags & radiotap::FLAG_BAD_FCS != 0 {
                continue;
            }
            let mut body = &f[phy.len..];
            if phy.flags & radiotap::FLAG_FCS != 0 {
                body = &body[..body.len().saturating_sub(4)];
            }
            let Some(h) = dot11::header(body) else { continue };
            if h.kind != dot11::MGMT {
                continue;
            }
            let frame = &body[h.body.min(body.len())..];
            let line = match h.sub {
                dot11::BEACON | 5 if frame.len() > 12 => {
                    let bss = nl::Bss {
                        bssid: h.a3.unwrap_or_default(),
                        freq: 2412,
                        ies: frame[12..].to_vec(),
                        ..Default::default()
                    };
                    let n = scan::network(&bss);
                    let extra = (
                        n.device.clone(),
                        n.ap_name.clone(),
                        n.colocated.clone(),
                        n.neighbours,
                        n.access,
                        n.venue,
                        n.passpoint,
                        n.multi_ap,
                    );
                    if n.device.is_empty()
                        && n.ap_name.is_none()
                        && n.colocated.is_empty()
                        && n.neighbours == 0
                        && n.access.is_none()
                        && n.multi_ap.is_none()
                    {
                        continue;
                    }
                    format!("AP  {} {:?} {:?}", dot11::fmt(&n.bssid), n.ssid, extra)
                }
                dot11::PROBE_REQ | dot11::ASSOC_REQ | dot11::REASSOC_REQ => {
                    let ies = match h.sub {
                        dot11::ASSOC_REQ => frame.get(4..),
                        dot11::REASSOC_REQ => frame.get(10..),
                        _ => Some(frame),
                    }
                    .unwrap_or(&[]);
                    let probe = h.sub == dot11::PROBE_REQ;
                    let sig = ident::signature(if probe { "probe" } else { "assoc" }, ies);
                    let hash = ident::print_hash(&sig);
                    let id = ident::from_ies(ies);
                    let caps = (!probe).then(|| dot11::caps(frame, h.sub == dot11::REASSOC_REQ));
                    let table = if probe {
                        ident::known(None, Some(hash))
                    } else {
                        ident::known(Some(hash), None)
                    };
                    format!(
                        "STA {} sub{} {hash:016X} {:?} {:?} {:?} {sig}",
                        dot11::fmt(&h.a2.unwrap_or_default()),
                        h.sub,
                        table,
                        id,
                        caps.map(|c| (c.generation(), c.nss, c.twt, c.listen)),
                    )
                }
                _ => continue,
            };
            if seen.insert(line.clone()) {
                println!("{line}");
            }
        }
    }
}
