//! What the frames say about one network: its airtime and traffic each second, its
//! clients, how each is linked and what it is doing, and who joined and left.
//!
//! A frame is the network's when an address is its BSSID, and the client is the
//! other address. A frame that failed its checksum still has its PHY and usually its
//! addresses, so it counts for airtime on an exact BSSID match, and for a client only
//! one a good frame has shown already.

use std::collections::{HashMap, HashSet, VecDeque};

use crate::classify::{self, Activity};
use crate::dot11::{self, Caps, Header, Mac};
use crate::ident::{self, Ident};
use crate::radiotap::{self, Phy, Standard};

/// Seconds rates are averaged over.
pub const WINDOW: usize = 5;
/// Seconds of history the graphs show.
pub const HISTORY: usize = 120;
/// A client not heard for this long is dropped from the list.
const FORGET_US: u64 = 600_000_000;
/// Events kept for the log.
const EVENTS: usize = 200;
/// Stations whose probes are remembered, for names and fingerprints.
const PROBERS: usize = 512;
/// Peers kept per client.
const PEERS: usize = 64;
/// A client unheard for this long has gone quiet.
const QUIET_US: u64 = 60_000_000;

/// Seconds of signal samples kept for the station being located.
pub const LOCATE_US: u64 = 60_000_000;

/// Every group address counts as one peer.
pub const GROUP: Mac = [0xff; 6];

/// One second of the network.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct NetSecond {
    /// Microseconds of airtime heard on the channel, and of it the network's.
    pub air: f32,
    pub bss_air: f32,
    /// Of those, frames that failed their checksum.
    pub bad_air: f32,
    pub bad_bss_air: f32,
    pub down: u64,
    pub up: u64,
    pub frames_down: u32,
    pub frames_up: u32,
    pub beacons: u32,
    pub frames: u32,
    /// The channel's busy share from the radio's survey, 0..100.
    pub busy: Option<f32>,
}

/// One second of a client.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ClientSecond {
    pub frames_up: u32,
    pub frames_down: u32,
    pub bytes_up: u64,
    pub bytes_down: u64,
    pub data_up: u32,
    pub data_down: u32,
    pub retries_up: u32,
    pub retries_down: u32,
    pub air: f32,
    /// Microseconds it spent in power save.
    pub asleep: u64,
    /// Gaps between its data frames, for how regular they are.
    pub gaps: u32,
    pub gap_sum: f64,
    pub gap_sq: f64,
    /// Bytes in the voice and video classes.
    pub realtime: u64,
}

/// How a frame was sent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Link {
    pub standard: Standard,
    pub mcs: u8,
    pub nss: u8,
    pub width: u16,
    pub gi: u16,
    pub rate: f32,
}

impl Link {
    fn of(phy: &Phy) -> Self {
        Self {
            standard: phy.standard,
            mcs: phy.mcs,
            nss: phy.nss,
            width: phy.width,
            gi: phy.gi,
            rate: phy.rate,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Client {
    /// When it was first heard, which keeps its place in the list.
    pub first: u64,
    pub seen: u64,
    pub now: ClientSecond,
    pub secs: VecDeque<ClientSecond>,
    pub total_up: u64,
    pub total_down: u64,
    /// The last data frame each way.
    pub up: Option<Link>,
    pub down: Option<Link>,
    pub signal: Option<f32>,
    /// In power save since this time.
    pub asleep_since: Option<u64>,
    /// Bytes by access category: background, best effort, video, voice.
    pub classes: [u64; 4],
    /// Bytes up and down by the other end of its traffic, as the AP relays it.
    pub peers: HashMap<Mac, [u64; 2]>,
    pub caps: Option<Caps>,
    /// The signature hash of its association request.
    pub print: Option<u64>,
    pub joined: Option<u64>,
    pub activity: Activity,
    /// Logged as gone quiet, and not heard since.
    pub quiet: bool,
    last_data: Option<u64>,
}

impl Client {
    fn new(at: u64) -> Self {
        Self { first: at, seen: at, ..Self::default() }
    }

    /// The last `n` whole seconds, newest last.
    pub fn recent(&self, n: usize) -> impl Iterator<Item = &ClientSecond> {
        self.secs.iter().skip(self.secs.len().saturating_sub(n))
    }

    /// A field averaged over the window.
    pub fn avg(&self, f: impl Fn(&ClientSecond) -> f64) -> f64 {
        let n = self.secs.len().min(WINDOW);
        if n == 0 {
            return 0.0;
        }
        self.recent(WINDOW).map(f).sum::<f64>() / n as f64
    }

    pub fn asleep(&self) -> bool {
        self.asleep_since.is_some()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    /// First heard while watching, or heard again after going quiet.
    Heard {
        again: bool,
    },
    /// Unheard for a minute.
    Quiet,
    Joined,
    /// Deauthenticated or disassociated; by the AP or not, and the reason when readable.
    Left {
        by_ap: bool,
        reason: Option<u16>,
    },
    RoamedIn {
        from: Mac,
    },
    RoamedOut {
        to: Mac,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub at: u64,
    pub who: Mac,
    pub kind: EventKind,
}

/// The network being watched.
#[derive(Clone, Debug, Default)]
pub struct Model {
    pub bssid: Mac,
    /// The capture clock, microseconds.
    pub now: u64,
    pub cur: NetSecond,
    pub secs: VecDeque<NetSecond>,
    pub clients: HashMap<Mac, Client>,
    pub events: VecDeque<Event>,
    pub ap_signal: Option<f32>,
    pub noise: Option<i8>,
    pub dropped: u64,
    /// What stations said about themselves in probes and association requests.
    pub idents: HashMap<Mac, Ident>,
    /// Fingerprints of the probes of stations not yet seen associating.
    pub probe_prints: HashMap<Mac, u64>,
    /// The far ends of four-address frames through the AP: mesh and WDS links.
    pub mesh: HashSet<Mac>,
    /// The station being located, and the signal of each frame it sent: when, and dBm.
    pub target: Option<Mac>,
    pub samples: VecDeque<(u64, i8)>,
    known: HashSet<Mac>,
    last_ampdu: Option<u32>,
    survey: Option<(u64, u64)>,
    reassoc_from: HashMap<Mac, Mac>,
}

fn ema(old: Option<f32>, new: f32) -> Option<f32> {
    Some(old.map_or(new, |o| o * 0.8 + new * 0.2))
}

impl Model {
    pub fn new(bssid: Mac) -> Self {
        Self { bssid, ..Self::default() }
    }

    fn event(&mut self, who: Mac, kind: EventKind) {
        if self.events.len() == EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(Event { at: self.now, who, kind });
    }

    fn client(&mut self, who: Mac) -> &mut Client {
        let now = self.now;
        self.clients.entry(who).or_insert_with(|| Client::new(now))
    }

    /// One captured frame: the capture clock, its length on air, and what was kept of it.
    pub fn frame(&mut self, at: u64, len: usize, data: &[u8]) {
        self.now = self.now.max(at);
        let Some(phy) = radiotap::parse(data) else { return };
        let mut size = len.saturating_sub(phy.len);
        let mut body = &data[phy.len.min(data.len())..];
        if phy.flags & radiotap::FLAG_FCS != 0 {
            size = size.saturating_sub(4);
            if body.len() >= len.saturating_sub(phy.len) {
                body = &body[..body.len().saturating_sub(4)];
            }
        }
        if size < 10 || body.len() < 10 {
            return;
        }
        let preamble =
            if phy.ampdu.is_some() && phy.ampdu == self.last_ampdu { 0.0 } else { phy.preamble };
        self.last_ampdu = phy.ampdu;
        let air = preamble + if phy.rate > 0.0 { size as f32 * 8.0 / phy.rate } else { 0.0 };
        self.cur.air += air;
        self.cur.frames += 1;
        let Some(h) = dot11::header(body) else { return };
        self.sample(&h, &phy);
        if phy.flags & radiotap::FLAG_BAD_FCS != 0 {
            self.bad(&h, air);
            return;
        }
        self.good(&h, &phy, body, size, air);
    }

    fn bad(&mut self, h: &Header, air: f32) {
        self.cur.bad_air += air;
        let b = self.bssid;
        let up = h.a1 == b;
        if h.a1 != b && h.a2 != Some(b) {
            return;
        }
        self.cur.bss_air += air;
        self.cur.bad_bss_air += air;
        let who = if up { h.a2 } else { Some(h.a1) };
        if let Some(who) = who.filter(|w| self.known.contains(w)) {
            let c = self.client(who);
            if up {
                c.now.frames_up += 1;
            } else {
                c.now.frames_down += 1;
            }
            c.now.air += air;
        }
    }

    /// The signal of a frame the station being located sent. One that failed its
    /// checksum counts too: its signal was measured all the same, and a corrupted sender
    /// address matching the target's exactly is all but impossible. At a weak signal
    /// those are many of the frames, and leaving them out starved the reading.
    fn sample(&mut self, h: &Header, phy: &Phy) {
        let (Some(t), Some(dbm)) = (self.target, phy.signal) else { return };
        // An AP by its beacons only: sent at one power and rate, where its data frames
        // vary by several dB with the rate they are sent at
        let beacon = h.kind == dot11::MGMT && h.sub == dot11::BEACON;
        if h.a2 == Some(t) && (t != self.bssid || beacon) {
            self.samples.push_back((self.now, dbm));
            while self.samples.front().is_some_and(|(at, _)| self.now - at > LOCATE_US) {
                self.samples.pop_front();
            }
        }
    }

    fn good(&mut self, h: &Header, phy: &Phy, body: &[u8], size: usize, air: f32) {
        let b = self.bssid;
        let (a1, a2, a3) = (h.a1, h.a2, h.a3);
        if h.kind == dot11::MGMT {
            self.management(h, body);
        }
        let (mut ours, mut up, mut who) = (false, false, None);
        match h.kind {
            dot11::DATA => match h.ds {
                1 if a1 == b => (ours, up, who) = (true, true, a2),
                2 if a2 == Some(b) => (ours, who) = (true, Some(a1)),
                3 if a1 == b || a2 == Some(b) => {
                    ours = true;
                    let other = if a1 == b { a2 } else { Some(a1) };
                    if let Some(o) =
                        other.filter(|o| self.mesh.len() < PEERS || self.mesh.contains(o))
                    {
                        self.mesh.insert(o);
                    }
                }
                0 if a3 == Some(b) => ours = true,
                _ => {}
            },
            dot11::MGMT => {
                if a2 == Some(b) {
                    (ours, who) = (true, Some(a1));
                    if h.sub == dot11::BEACON {
                        self.cur.beacons += 1;
                    }
                } else if a3 == Some(b) || a1 == b {
                    (ours, up, who) = (true, true, a2);
                }
            }
            _ => match a2 {
                Some(t) if t == b => (ours, who) = (true, Some(a1)),
                Some(_) if a1 == b => (ours, up, who) = (true, true, a2),
                None if a1 == b => (ours, up) = (true, true),
                None if self.clients.contains_key(&a1) => (ours, who) = (true, Some(a1)),
                _ => {}
            },
        }
        if !ours {
            return;
        }
        self.cur.bss_air += air;
        if !up && a2 == Some(b) {
            if let Some(s) = phy.signal {
                self.ap_signal = ema(self.ap_signal, f32::from(s));
            }
        }
        let data = h.kind == dot11::DATA;
        if data {
            if up {
                self.cur.up += size as u64;
                self.cur.frames_up += 1;
            } else {
                self.cur.down += size as u64;
                self.cur.frames_down += 1;
            }
        }
        let Some(who) = who.filter(|w| !dot11::is_group(w) && *w != b) else { return };
        // A station only joins the list by data or by associating: every AP answers a
        // scanning phone's probes, and that does not make it anyone's client
        let joins = data || (h.kind == dot11::MGMT && h.sub <= dot11::REASSOC_RESP);
        if !joins && !self.clients.contains_key(&who) {
            return;
        }
        let first = self.known.insert(who);
        let back = self.clients.get(&who).is_some_and(|c| c.quiet);
        if first || back {
            self.event(who, EventKind::Heard { again: back });
        }
        let now = self.now;
        let c = self.client(who);
        c.seen = now;
        c.quiet = false;
        c.now.air += air;
        if up {
            c.now.frames_up += 1;
            if let Some(s) = phy.signal {
                c.signal = ema(c.signal, f32::from(s));
            }
            // Power management is meaningful on what a station sends, control aside
            if h.kind != dot11::CTRL {
                match (h.pm, c.asleep_since) {
                    (true, None) => c.asleep_since = Some(now),
                    (false, Some(_)) => c.asleep_since = None,
                    _ => {}
                }
            }
        } else {
            c.now.frames_down += 1;
        }
        if !data {
            return;
        }
        let link = Link::of(phy);
        if up {
            c.now.bytes_up += size as u64;
            c.now.data_up += 1;
            c.now.retries_up += u32::from(h.retry);
            c.total_up += size as u64;
            c.up = Some(link);
        } else {
            c.now.bytes_down += size as u64;
            c.now.data_down += 1;
            c.now.retries_down += u32::from(h.retry);
            c.total_down += size as u64;
            c.down = Some(link);
        }
        if let Some(last) = c.last_data {
            let gap = now.saturating_sub(last) as f64;
            c.now.gaps += 1;
            c.now.gap_sum += gap;
            c.now.gap_sq += gap * gap;
        }
        c.last_data = Some(now);
        if let Some(tid) = h.tid {
            let ac = dot11::access_category(tid);
            c.classes[ac] += size as u64;
            if ac >= 2 {
                c.now.realtime += size as u64;
            }
        }
        // The other end: the destination going up, the source coming down
        let peer = match (h.ds, a3) {
            (1 | 2, Some(p)) if dot11::is_group(&p) => Some(GROUP),
            (1 | 2, Some(p)) => Some(p),
            _ => None,
        };
        if let Some(peer) = peer {
            if c.peers.len() < PEERS || c.peers.contains_key(&peer) {
                c.peers.entry(peer).or_default()[usize::from(!up)] += size as u64;
            }
        }
    }

    /// Joins, leaves and roams, and what a joining station says it can do.
    fn management(&mut self, h: &Header, frame: &[u8]) {
        let b = self.bssid;
        let body = frame.get(h.body..).unwrap_or(&[]);
        let (to_ap, from_ap) = (h.a1 == b, h.a2 == Some(b));
        let station = if to_ap {
            h.a2
        } else if from_ap {
            Some(h.a1)
        } else {
            None
        };
        match h.sub {
            dot11::PROBE_REQ => {
                let Some(sta) = h.a2.filter(|s| !dot11::is_group(s)) else { return };
                if self.idents.len() >= PROBERS && !self.idents.contains_key(&sta) {
                    return;
                }
                let said = ident::from_ies(body);
                if !said.is_empty() {
                    self.idents.entry(sta).or_default().merge(said);
                }
                if self.probe_prints.len() < PROBERS {
                    self.probe_prints
                        .insert(sta, ident::print_hash(&ident::signature("probe", body)));
                }
            }
            dot11::ASSOC_REQ | dot11::REASSOC_REQ if to_ap => {
                let Some(sta) = station else { return };
                let reassoc = h.sub == dot11::REASSOC_REQ;
                let caps = dot11::caps(body, reassoc);
                let ies = body.get(if reassoc { 10 } else { 4 }..).unwrap_or(&[]);
                let said = ident::from_ies(ies);
                if !said.is_empty() {
                    self.idents.entry(sta).or_default().merge(said);
                }
                self.client(sta).print = Some(ident::print_hash(&ident::signature("assoc", ies)));
                self.client(sta).caps = Some(caps);
                if reassoc {
                    if let Some(from) = dot11::reassoc_from(body).filter(|f| *f != b) {
                        self.reassoc_from.insert(sta, from);
                    }
                }
            }
            dot11::REASSOC_REQ => {
                // A client of ours asking another AP
                if let Some(sta) = h.a2.filter(|s| self.clients.contains_key(s)) {
                    if h.a3 != Some(b) {
                        self.event(sta, EventKind::RoamedOut { to: h.a1 });
                    }
                }
            }
            dot11::ASSOC_RESP | dot11::REASSOC_RESP if from_ap => {
                let Some(sta) = station else { return };
                if dot11::assoc_status(body) != Some(0) {
                    return;
                }
                let now = self.now;
                self.client(sta).joined = Some(now);
                let kind = match self.reassoc_from.remove(&sta) {
                    Some(from) => EventKind::RoamedIn { from },
                    None => EventKind::Joined,
                };
                self.event(sta, kind);
            }
            dot11::DEAUTH | dot11::DISASSOC if to_ap || from_ap => {
                let Some(sta) = station.filter(|s| !dot11::is_group(s)) else { return };
                let reason = if h.protected { None } else { dot11::reason(body) };
                let left = EventKind::Left { by_ap: from_ap, reason };
                // A deauth is often sent twice, and both ways
                let repeat = self.events.iter().rev().take(4).any(|e| {
                    e.who == sta
                        && matches!(e.kind, EventKind::Left { .. })
                        && self.now - e.at < 1_000_000
                });
                if !repeat {
                    self.event(sta, left);
                }
                if let Some(c) = self.clients.get_mut(&sta) {
                    c.joined = None;
                }
            }
            _ => {}
        }
    }

    /// The radio's survey, once a second, which closes the second.
    pub fn survey(&mut self, active_ms: u64, busy_ms: u64, noise: Option<i8>, dropped: u32) {
        if let Some((a, bz)) = self.survey {
            if active_ms > a && busy_ms >= bz {
                self.cur.busy = Some(100.0 * (busy_ms - bz) as f32 / (active_ms - a) as f32);
            }
        }
        if active_ms > 0 {
            self.survey = Some((active_ms, busy_ms));
        }
        if noise.is_some() {
            self.noise = noise;
        }
        self.dropped += u64::from(dropped);
        self.roll();
    }

    fn roll(&mut self) {
        if self.secs.len() == HISTORY {
            self.secs.pop_front();
        }
        self.secs.push_back(std::mem::take(&mut self.cur));
        let now = self.now;
        for c in self.clients.values_mut() {
            let mut s = std::mem::take(&mut c.now);
            if let Some(since) = c.asleep_since {
                s.asleep = now.saturating_sub(since).min(1_000_000);
            }
            if c.secs.len() == HISTORY {
                c.secs.pop_front();
            }
            c.secs.push_back(s);
            let label = classify::activity(c);
            c.activity.update(label);
        }
        let gone: Vec<Mac> = self
            .clients
            .iter()
            .filter(|(_, c)| !c.quiet && now.saturating_sub(c.seen) >= QUIET_US)
            .map(|(m, _)| *m)
            .collect();
        for who in gone {
            self.clients.get_mut(&who).expect("listed").quiet = true;
            self.event(who, EventKind::Quiet);
        }
        self.clients.retain(|_, c| now.saturating_sub(c.seen) < FORGET_US);
    }

    /// A network field averaged over the window.
    pub fn avg(&self, f: impl Fn(&NetSecond) -> f64) -> f64 {
        let n = self.secs.len().min(WINDOW);
        if n == 0 {
            return 0.0;
        }
        self.secs.iter().skip(self.secs.len() - n).map(f).sum::<f64>() / n as f64
    }

    /// The last busy share the survey gave.
    pub fn busy(&self) -> Option<f32> {
        self.secs.iter().rev().find_map(|s| s.busy)
    }

    /// Start locating `who`, from no samples.
    pub fn locate(&mut self, who: Option<Mac>) {
        self.target = who;
        self.samples.clear();
    }

    /// Clients in the order they were first heard, so the list holds still to be read.
    pub fn listed(&self) -> Vec<Mac> {
        let mut all: Vec<(&Mac, &Client)> = self.clients.iter().collect();
        all.sort_by_key(|(m, c)| (c.first, **m));
        all.into_iter().map(|(m, _)| *m).collect()
    }

    pub fn active(&self) -> usize {
        let now = self.now;
        self.clients
            .values()
            .filter(|c| now.saturating_sub(c.seen) <= WINDOW as u64 * 1_000_000)
            .count()
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub const AP: Mac = [0xcc, 0x2d, 0xe0, 0xc2, 0x9e, 0xa0];
    pub const STA: Mac = [0xde, 0xad, 0x4b, 0x52, 0x55, 0x9a];
    pub const PEER: Mac = [0x88, 0xd8, 0x2e, 0x56, 0x91, 0x13];

    /// Radiotap with flags, signal and a VHT MCS 7 / 1 stream / 80 MHz field.
    pub fn radiotap(bad: bool) -> Vec<u8> {
        let present: u32 = 1 << 1 | 1 << 5 | 1 << 21;
        let mut b = vec![0u8, 0, 0, 0];
        b.extend_from_slice(&present.to_le_bytes());
        b.push(if bad { 0x40 } else { 0 });
        b.push(-50i8 as u8);
        b.extend_from_slice(&[0, 0, 0, 4, 7 << 4 | 1, 0, 0, 0, 0, 0, 0, 0]);
        let len = b.len() as u16;
        b[2..4].copy_from_slice(&len.to_le_bytes());
        b
    }

    /// A QoS data frame, up (station to AP) or down, with `payload` bytes after the header.
    pub fn data(up: bool, tid: u8, payload: usize, flags: u8) -> Vec<u8> {
        let mut f = radiotap(false);
        let ds = if up { 1 } else { 2 };
        f.extend_from_slice(&[0x88, ds | flags, 0, 0]);
        let (a1, a2) = if up { (AP, STA) } else { (STA, AP) };
        f.extend_from_slice(&a1);
        f.extend_from_slice(&a2);
        f.extend_from_slice(&PEER);
        f.extend_from_slice(&[0, 0, tid, 0]);
        f.extend(std::iter::repeat(0u8).take(payload));
        f
    }

    fn mgmt(sub: u8, a1: Mac, a2: Mac, a3: Mac, body: &[u8]) -> Vec<u8> {
        let mut f = radiotap(false);
        f.extend_from_slice(&[sub << 4, 0, 0, 0]);
        f.extend_from_slice(&a1);
        f.extend_from_slice(&a2);
        f.extend_from_slice(&a3);
        f.extend_from_slice(&[0, 0]);
        f.extend_from_slice(body);
        f
    }

    fn feed(m: &mut Model, at: u64, f: &[u8]) {
        m.frame(at, f.len(), f);
    }

    #[test]
    fn data_is_counted_each_way_with_its_peer_and_class() {
        let mut m = Model::new(AP);
        feed(&mut m, 1_000, &data(true, 6, 100, 0));
        feed(&mut m, 2_000, &data(false, 0, 1000, 0x08));
        feed(&mut m, 3_000, &data(true, 6, 100, 0x10));
        m.survey(1000, 100, Some(-95), 0);
        let c = &m.clients[&STA];
        let s = c.secs.back().unwrap();
        assert_eq!((s.data_up, s.data_down, s.retries_down), (2, 1, 1));
        assert_eq!(s.bytes_up, 2 * 126);
        assert_eq!(c.classes[3], 2 * 126);
        assert_eq!(c.classes[1], 1026);
        assert_eq!(c.peers[&PEER], [252, 1026]);
        assert!(c.asleep(), "the last frame up had PM set");
        assert_eq!(c.up.unwrap().standard, Standard::Vht);
        assert_eq!(c.signal, Some(-50.0));
        assert_eq!(m.secs.back().unwrap().frames_up, 2);
    }

    #[test]
    fn a_bad_frame_counts_for_a_known_client_only() {
        let mut m = Model::new(AP);
        let mut bad = data(true, 0, 50, 0);
        bad[8] = 0x40;
        feed(&mut m, 10, &bad);
        assert!(m.clients.is_empty());
        assert!(m.cur.bad_bss_air > 0.0);
        feed(&mut m, 20, &data(true, 0, 50, 0));
        feed(&mut m, 30, &bad);
        assert_eq!(m.clients[&STA].now.frames_up, 2);
    }

    #[test]
    fn joins_roams_and_leaves_are_logged() {
        let mut m = Model::new(AP);
        let other: Mac = [2, 0, 0, 0, 0, 1];
        // Reassociation from another AP, accepted
        let mut req = vec![0x31, 0x04, 0x0a, 0x00];
        req.extend_from_slice(&other);
        feed(&mut m, 100, &mgmt(dot11::REASSOC_REQ, AP, STA, AP, &req));
        feed(&mut m, 200, &mgmt(dot11::REASSOC_RESP, STA, AP, AP, &[0x31, 0x04, 0, 0, 1, 0xc0]));
        // Deauthenticated by the AP, sent twice
        feed(&mut m, 300, &mgmt(dot11::DEAUTH, STA, AP, AP, &[3, 0]));
        feed(&mut m, 400, &mgmt(dot11::DEAUTH, STA, AP, AP, &[3, 0]));
        let kinds: Vec<EventKind> = m.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Heard { again: false },
                EventKind::RoamedIn { from: other },
                EventKind::Left { by_ap: true, reason: Some(3) }
            ]
        );
        assert!(m.clients[&STA].caps.is_some());
        assert_eq!(m.clients[&STA].joined, None);
    }

    #[test]
    fn the_survey_gives_the_busy_share() {
        let mut m = Model::new(AP);
        m.survey(1000, 100, None, 0);
        m.survey(2000, 350, None, 3);
        assert_eq!(m.busy(), Some(25.0));
        assert_eq!(m.dropped, 3);
    }

    #[test]
    fn a_client_going_quiet_and_coming_back_is_logged() {
        let mut m = Model::new(AP);
        feed(&mut m, 1, &data(true, 0, 10, 0));
        m.now = QUIET_US + 2;
        m.survey(0, 0, None, 0);
        m.survey(0, 0, None, 0);
        feed(&mut m, QUIET_US + 3, &data(true, 0, 10, 0));
        let kinds: Vec<EventKind> = m.events.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::Heard { again: false },
                EventKind::Quiet,
                EventKind::Heard { again: true }
            ]
        );
    }

    #[test]
    fn a_client_unheard_for_ten_minutes_is_forgotten() {
        let mut m = Model::new(AP);
        feed(&mut m, 1, &data(true, 0, 10, 0));
        m.now = FORGET_US + 2;
        m.survey(0, 0, None, 0);
        assert!(m.clients.is_empty());
    }

    #[test]
    fn a_scanning_station_is_nobodys_client() {
        let mut m = Model::new(AP);
        // Its probe and the AP's answer
        feed(&mut m, 1, &mgmt(dot11::PROBE_REQ, GROUP, STA, GROUP, &[0, 0]));
        feed(&mut m, 2, &mgmt(5, STA, AP, AP, &[0; 12]));
        assert!(m.clients.is_empty());
        feed(&mut m, 3, &data(true, 0, 10, 0));
        assert!(m.clients.contains_key(&STA));
    }

    #[test]
    fn the_list_keeps_the_order_clients_were_heard_in() {
        let mut m = Model::new(AP);
        feed(&mut m, 1, &data(true, 0, 10, 0));
        feed(&mut m, 2, &mgmt(dot11::ASSOC_REQ, AP, PEER, AP, &[0, 0, 10, 0]));
        for at in 3..50 {
            feed(&mut m, at, &mgmt(dot11::ASSOC_REQ, AP, PEER, AP, &[0, 0, 10, 0]));
        }
        assert_eq!(m.listed(), vec![STA, PEER]);
        assert_eq!(m.clients[&PEER].caps.as_ref().map(|c| c.listen), Some(10));
    }
}
