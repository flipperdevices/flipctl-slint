//! The four RGB LEDs along the top, drawn by flipctl rather than a kernel trigger.
//!
//! eth0, eth1 and Wi-Fi each say what their port is doing: dark with no link,
//! breathing between a floor and full with a link and no address, solid with one.
//! Every change is a fade, drawn in perceived lightness, as the ETH LED test app
//! worked out. Link is held dark until it has something to say.
//!
//! One thread for the life of the process. It parks on rtnetlink while every light
//! is still and ticks only while one fades or breathes. The timings are settings,
//! which debug mode puts under LED Brightness.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::netlink::{self, Socket};

const LEDS: &str = "/sys/class/leds";
const FRAME: Duration = Duration::from_millis(20);
const RTMGRP_LINK: u32 = 0x1;
const RTMGRP_IPV4_IFADDR: u32 = 0x10;

/// Where the breath peaks and how wide each side is, as fractions of the cycle.
const MU: f32 = 0.30;
const SIGMA_IN: f32 = 0.10;
const SIGMA_OUT: f32 = 0.17;

/// A fade out drops to dark from here: the LED's last steps are too coarse to fade
/// through.
const DROP: f32 = 0.30;

/// One adjustable timing of the animation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Param {
    FadeIn,
    Breathe,
    Floor,
    Solid,
    FadeOut,
}

impl Param {
    pub const ALL: [Param; 5] =
        [Param::FadeIn, Param::Breathe, Param::Floor, Param::Solid, Param::FadeOut];

    pub fn label(self) -> &'static str {
        match self {
            Param::FadeOut => "Fade Out",
            Param::FadeIn => "Fade In",
            Param::Breathe => "Breathe",
            Param::Solid => "Solid",
            Param::Floor => "Floor",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Param::FadeOut => "led_fade_out_ms",
            Param::FadeIn => "led_fade_in_ms",
            Param::Breathe => "led_breathe_ms",
            Param::Solid => "led_solid_ms",
            Param::Floor => "led_floor_percent",
        }
    }

    /// The last step.
    fn last(self) -> u8 {
        match self {
            Param::FadeOut | Param::FadeIn | Param::Solid => 25,
            Param::Breathe => 14,
            Param::Floor => 12,
        }
    }

    /// The step it starts on: a 1.0s fade in, a 2.5s breath and a 0.6s solid, the
    /// ETH LED app's, a 50% floor and a 0.4s fade out.
    fn default(self) -> u8 {
        match self {
            Param::FadeIn => 5,
            Param::FadeOut => 2,
            Param::Solid => 3,
            Param::Breathe => 3,
            Param::Floor => 10,
        }
    }

    /// The value at step `at`: milliseconds for a time, percent for the floor.
    fn at(self, at: u8) -> u32 {
        let at = u32::from(at);
        match self {
            Param::FadeOut | Param::FadeIn | Param::Solid => 200 * at,
            Param::Breathe => 1000 + 500 * at,
            Param::Floor => 5 * at,
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

fn steps() -> &'static [AtomicU8; 5] {
    static AT: OnceLock<[AtomicU8; 5]> = OnceLock::new();
    AT.get_or_init(|| Param::ALL.map(|p| AtomicU8::new(saved(p))))
}

/// The saved step, or the default for a missing or off-step value.
fn saved(p: Param) -> u8 {
    crate::settings::get(p.key())
        .and_then(|v| v.parse::<u32>().ok())
        .and_then(|v| (0..=p.last()).find(|&i| p.at(i) == v))
        .unwrap_or(p.default())
}

fn store(p: Param, at: u8) {
    steps()[p.index()].store(at, Ordering::Relaxed);
    crate::settings::set(p.key(), &p.at(at).to_string());
}

/// The current value: milliseconds for a time, percent for the floor.
pub fn value(p: Param) -> u32 {
    p.at(steps()[p.index()].load(Ordering::Relaxed))
}

/// How the value reads on screen.
pub fn label(p: Param) -> String {
    match p {
        Param::Floor => format!("{}%", value(p)),
        _ => format!("{:.1}s", value(p) as f32 / 1000.0),
    }
}

/// Whether the value is at the start or the end of its range.
pub fn ends(p: Param) -> (bool, bool) {
    let at = steps()[p.index()].load(Ordering::Relaxed);
    (at == 0, at == p.last())
}

/// Step the value up or down and save it. False at the end of the range.
pub fn step(p: Param, up: bool) -> bool {
    let at = steps()[p.index()].load(Ordering::Relaxed);
    let next = if up { at.saturating_add(1).min(p.last()) } else { at.saturating_sub(1) };
    if next == at {
        return false;
    }
    store(p, next);
    true
}

/// Every value back to its default, saved.
pub fn reset() {
    for p in Param::ALL {
        store(p, p.default());
    }
}

/// The timings in the units the lamp steps in.
#[derive(Copy, Clone, Debug)]
pub struct Timings {
    pub fade_out: f32,
    pub fade_in: f32,
    pub breathe: f32,
    pub solid: f32,
    pub floor: f32,
}

impl Timings {
    pub fn now() -> Self {
        let secs = |p| value(p) as f32 / 1000.0;
        Self {
            fade_out: secs(Param::FadeOut),
            fade_in: secs(Param::FadeIn),
            breathe: secs(Param::Breathe),
            solid: secs(Param::Solid),
            floor: value(Param::Floor) as f32 / 100.0,
        }
    }
}

/// What a port is doing, in the order a better one wins.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum State {
    Dark,
    Link,
    Ip,
}

/// Perceived lightness, 0 to 1, at `phase` through one breath: an asymmetric
/// gaussian, normalised per side so both ends reach exactly zero.
fn breath(phase: f32) -> f32 {
    let (sigma, side) = if phase < MU { (SIGMA_IN, MU) } else { (SIGMA_OUT, 1.0 - MU) };
    let edge = side / sigma;
    let floor = (-edge * edge / 2.0).exp();
    let x = (phase - MU) / sigma;
    ((-x * x / 2.0).exp() - floor) / (1.0 - floor)
}

/// The duty a perceived lightness needs, by CIE 1931.
pub fn duty(lightness: f32) -> f32 {
    let star = 100.0 * lightness;
    if star <= 8.0 {
        star / 903.3
    } else {
        ((star + 16.0) / 116.0).powi(3)
    }
}

/// Steps per channel of the RGB565 word the driver sends the MCU.
const STEPS: [u16; 3] = [31, 63, 31];

/// How far the MCU shifts each RGB565 channel up to eight bits.
const SHIFT: [u32; 3] = [3, 2, 3];

/// The byte the MCU gives a WS2812 channel for `step` (flipperone-mcu-firmware,
/// `LED_COLOR_RGB565` in led.h and `led_set_color` in led_backend.c): the step
/// shifted up to eight bits, times the group setting over 255, times 0.5, truncated.
/// Any byte above zero lights.
fn byte(step: u16, channel: usize, group: u8) -> u16 {
    ((u32::from(step) << SHIFT[channel]) * u32::from(group) / 510) as u16
}

/// How far a shown colour's other channels may stray from their share of the
/// brightest one, as a fraction of that share. Further than this it reads as
/// another colour.
const HUE_TOLERANCE: f32 = 0.12;

/// One brightness a colour can be shown at: its RGB565 steps, and the byte its
/// brightest channel reaches on the LED.
type Shade = ([u16; 3], u16);

/// Every brightness `colour` can be shown at under group setting `group` without
/// its hue straying past [`HUE_TOLERANCE`], dimmest first, one per byte.
///
/// The MCU leaves each channel only a few dozen bytes at a dim group setting, and
/// truncates, so most combinations near the bottom are some other colour: lime with
/// red rounded away is green. Only these are ever shown, so a light can be a byte
/// off in brightness but never off in colour. A colour with none at this setting
/// stays dark.
fn shades(colour: [u8; 3], group: u8) -> Vec<Shade> {
    let anchor = (0..3).max_by_key(|&i| colour[i]).unwrap_or(0);
    if colour[anchor] == 0 {
        return Vec::new();
    }
    let share = |i: usize| f32::from(colour[i]) / f32::from(colour[anchor]);
    // The lowest step of `channel` whose byte is nearest `want`.
    let nearest = |channel: usize, want: f32| -> u16 {
        (0..=STEPS[channel])
            .min_by(|&a, &b| {
                let off = |s| (f32::from(byte(s, channel, group)) - want).abs();
                off(a).total_cmp(&off(b))
            })
            .unwrap_or(0)
    };
    let mut out: Vec<Shade> = Vec::new();
    for s in 1..=STEPS[anchor] {
        let lead = byte(s, anchor, group);
        if lead == 0 || out.last().is_some_and(|&(_, b)| b == lead) {
            continue;
        }
        let steps: [u16; 3] = std::array::from_fn(|i| {
            if i == anchor {
                s
            } else {
                nearest(i, f32::from(lead) * share(i))
            }
        });
        let true_to_colour = (0..3).filter(|&i| i != anchor && colour[i] > 0).all(|i| {
            let got = f32::from(byte(steps[i], i, group)) / f32::from(lead);
            (got / share(i) - 1.0).abs() <= HUE_TOLERANCE
        });
        if true_to_colour {
            out.push((steps, lead));
        }
    }
    out
}

/// The shade nearest `duty` of the brightest one, or dark below half the dimmest.
fn pick(shades: &[Shade], duty: f32) -> [u16; 3] {
    let (Some(&(_, dimmest)), Some(&(_, top))) = (shades.first(), shades.last()) else {
        return [0; 3];
    };
    let level = f32::from(top) * duty;
    if level < f32::from(dimmest) / 2.0 {
        return [0; 3];
    }
    let off = |s: &&Shade| (f32::from(s.1) - level).abs();
    shades.iter().min_by(|a, b| off(a).total_cmp(&off(b))).map_or([0; 3], |s| s.0)
}

/// The `multi_intensity` that the driver's truncation turns back into `steps`.
fn intensities(steps: [u16; 3]) -> [u8; 3] {
    std::array::from_fn(|i| (u32::from(steps[i]) * 255).div_ceil(u32::from(STEPS[i])) as u8)
}

/// One light: a level in perceived lightness and the state driving it. A fade is
/// held as where it started and how far through it is, so it can begin anywhere.
#[derive(Clone, Debug)]
///
/// An address that comes before the light has breathed once waits for that breath:
/// a reconnect reuses its lease and has one in a fraction of a second, and every
/// connect should look the same.
pub struct Lamp {
    state: State,
    level: f32,
    phase: f32,
    came_from: f32,
    through: f32,
    /// How many breaths have been drawn since the fade in ended.
    breathed: f32,
    /// Solid, waiting for the first breath to finish.
    waiting: bool,
}

impl Default for Lamp {
    fn default() -> Self {
        Self {
            state: State::Dark,
            level: 0.0,
            phase: MU,
            came_from: 0.0,
            through: 1.0,
            breathed: 0.0,
            waiting: false,
        }
    }
}

impl Lamp {
    pub fn level(&self) -> f32 {
        self.level
    }

    /// Play the effect `p` adjusts, from where it starts: dark into a fade in, the
    /// top of a breath, the floor into solid, or full into a fade out.
    pub fn preview(&mut self, p: Param, floor: f32) {
        let (from, to, level) = match p {
            Param::FadeIn => (State::Dark, State::Link, 0.0),
            Param::Breathe | Param::Floor => (State::Link, State::Link, 1.0),
            Param::Solid => (State::Link, State::Ip, floor),
            Param::FadeOut => (State::Ip, State::Dark, 1.0),
        };
        *self = Lamp { state: from, level, came_from: level, ..Lamp::default() };
        // Straight there, without the first breath: each effect is seen on its own.
        self.go(to);
    }

    /// Move to `state`. Solid waits for the first breath after a fade in.
    pub fn enter(&mut self, state: State) {
        self.waiting = state == State::Ip && self.state == State::Link && self.breathed < 1.0;
        if !self.waiting {
            self.go(state);
        }
    }

    fn go(&mut self, state: State) {
        if state == self.state {
            return;
        }
        self.came_from = self.level;
        self.through = 0.0;
        self.state = state;
        // The fade in ends at the top of the breath, which carries on from there.
        if state == State::Link {
            self.phase = MU;
            self.breathed = 0.0;
        }
    }

    fn fading(&self) -> bool {
        self.through < 1.0
    }

    /// Whether the next step would change nothing.
    pub fn still(&self) -> bool {
        !self.fading() && self.state != State::Link
    }

    pub fn step(&mut self, dt: f32, t: &Timings) {
        let target = if self.state == State::Dark { 0.0 } else { 1.0 };
        if self.fading() {
            let fade = match self.state {
                State::Dark => t.fade_out,
                State::Link => t.fade_in,
                State::Ip => t.solid,
            };
            self.through = if fade <= 0.0 { 1.0 } else { (self.through + dt / fade).min(1.0) };
            self.level = self.came_from + (target - self.came_from) * self.through;
            if self.state == State::Dark && self.level < DROP {
                self.level = 0.0;
                self.through = 1.0;
            }
        } else if self.state == State::Link {
            self.breathed += dt / t.breathe;
            // A breath starts and ends at its top, so solid takes over from full.
            if self.waiting && self.breathed >= 1.0 {
                self.waiting = false;
                self.phase = MU;
                self.level = 1.0;
                self.go(State::Ip);
                self.through = 1.0;
                return;
            }
            self.phase = (self.phase + dt / t.breathe) % 1.0;
            // The floor lifts the whole breath rather than clipping its bottom.
            self.level = t.floor + (1.0 - t.floor) * breath(self.phase);
        } else {
            self.level = target;
        }
    }
}

/// What an LED follows.
#[derive(Copy, Clone)]
enum Source {
    Port(&'static str),
    Wifi,
    Off,
}

/// Name on the device, what it follows, and its colour.
const TABLE: [(&str, Source, [u8; 3]); 4] = [
    ("eth0", Source::Port("end0"), [120, 255, 0]),
    ("eth1", Source::Port("end1"), [120, 255, 0]),
    ("wifi", Source::Wifi, [0, 255, 255]),
    ("link", Source::Off, [0, 0, 0]),
];

struct Light {
    source: Source,
    colour: [u8; 3],
    lamp: Lamp,
    file: File,
    shown: String,
    /// The shades of `colour`, and the group setting they were worked out under.
    shades: Option<(u8, Vec<Shade>)>,
}

impl Light {
    /// Take the LED off whatever trigger it has and open its colour for writing.
    fn open(name: &str, source: Source, colour: [u8; 3]) -> Option<Self> {
        let dir = PathBuf::from(LEDS).join(format!("flipper-one:rgb:{name}"));
        if !dir.exists() {
            return None;
        }
        let took = (|| {
            std::fs::write(dir.join("trigger"), "none")?;
            std::fs::write(dir.join("multi_intensity"), "0 0 0")?;
            std::fs::write(dir.join("brightness"), "255")?;
            std::fs::OpenOptions::new().write(true).open(dir.join("multi_intensity"))
        })();
        match took {
            Ok(file) => Some(Self {
                source,
                colour,
                lamp: Lamp::default(),
                file,
                shown: "0 0 0".into(),
                shades: None,
            }),
            Err(e) => {
                eprintln!("net leds       {name}: {e}");
                None
            }
        }
    }

    /// Write the lamp's level, unless it would change nothing: the LEDs share an
    /// i2c bus with the buttons.
    fn show(&mut self, group: u8) {
        if self.shades.as_ref().is_none_or(|(at, _)| *at != group) {
            self.shades = Some((group, shades(self.colour, group)));
        }
        let shown = self.shades.as_ref().map_or(&[][..], |(_, s)| s);
        let line =
            intensities(pick(shown, duty(self.lamp.level()))).map(|c| c.to_string()).join(" ");
        if line != self.shown {
            if let Err(e) = self.file.write_at(line.as_bytes(), 0) {
                eprintln!("net leds       {e}");
            }
            self.shown = line;
        }
    }
}

/// An interface's state. Carrier is unreadable while it is down, which is dark, and
/// a 169.254 address means nothing answered, which is no address.
fn port(iface: &str) -> State {
    let carrier = std::fs::read_to_string(format!("/sys/class/net/{iface}/carrier"))
        .is_ok_and(|c| c.trim() == "1");
    if !carrier {
        State::Dark
    } else if crate::status::ipv4_all(iface).iter().any(|a| !a.starts_with("169.254.")) {
        State::Ip
    } else {
        State::Link
    }
}

/// NetworkManager is bringing a Wi-Fi up, and the thread has yet to look at it.
static WIFI_CONNECTING: AtomicBool = AtomicBool::new(false);
static WIFI_TOLD: AtomicBool = AtomicBool::new(false);

/// Whether NetworkManager is connecting a Wi-Fi, as its watcher reads it. The light
/// starts from there rather than from the link: scanning, association and the
/// handshake come before the carrier, and a connect is all of that.
pub fn wifi_connecting(on: bool) {
    if WIFI_CONNECTING.swap(on, Ordering::Relaxed) != on {
        WIFI_TOLD.store(true, Ordering::Relaxed);
        wake();
    }
}

/// The best state among the wireless interfaces, with connecting counted as a link.
fn wifi() -> State {
    let connecting =
        if WIFI_CONNECTING.load(Ordering::Relaxed) { State::Link } else { State::Dark };
    let Ok(dir) = std::fs::read_dir("/sys/class/net") else { return connecting };
    dir.flatten()
        .filter(|e| {
            std::fs::read_to_string(e.path().join("uevent"))
                .is_ok_and(|u| u.lines().any(|l| l == "DEVTYPE=wlan"))
        })
        .map(|e| port(&e.file_name().to_string_lossy()))
        .max()
        .map_or(connecting, |best| best.max(connecting))
}

fn look(lights: &mut [Light]) {
    for light in lights {
        let state = match light.source {
            Source::Port(iface) => port(iface),
            Source::Wifi => wifi(),
            Source::Off => State::Dark,
        };
        light.lamp.enter(state);
    }
}

/// How often the link group setting is read while a light moves.
const GROUP_READ: Duration = Duration::from_secs(1);

/// The link group setting, full when it cannot be read.
fn link_group() -> u8 {
    crate::leds::read(crate::leds::Group::Link).unwrap_or(u8::MAX)
}

/// How long a preview holds the lights after the last press, before they show the
/// ports again.
const PREVIEW: Duration = Duration::from_secs(15);

/// A preview asked for from the LED Brightness screen, and the eventfd that wakes
/// the thread for it or for NetworkManager: with every light still, the thread is
/// parked on netlink.
struct Asked {
    param: Mutex<Option<Param>>,
    wake: OwnedFd,
}

fn asked() -> Option<&'static Asked> {
    static ASKED: OnceLock<Option<Asked>> = OnceLock::new();
    ASKED
        .get_or_init(|| {
            // SAFETY: constant arguments, checked before ownership is taken.
            let fd = unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) };
            // SAFETY: a fresh descriptor this call owns.
            (fd >= 0).then(|| Asked {
                param: Mutex::new(None),
                wake: unsafe { OwnedFd::from_raw_fd(fd) },
            })
        })
        .as_ref()
}

/// Play the effect `p` adjusts on the network LEDs, then go back to the ports.
pub fn preview(p: Param) {
    let Some(a) = asked() else { return };
    *a.param.lock().unwrap_or_else(|e| e.into_inner()) = Some(p);
    wake();
}

/// Wake the thread, parked on netlink while every light is still.
fn wake() {
    let Some(a) = asked() else { return };
    let one = 1u64.to_ne_bytes();
    // SAFETY: eight bytes from a local buffer to an eventfd, which is what it reads.
    unsafe { libc::write(a.wake.as_raw_fd(), one.as_ptr().cast(), one.len()) };
}

/// The preview asked for since the last call, emptying the eventfd.
fn take_preview() -> Option<Param> {
    let a = asked()?;
    let mut buf = [0u8; 8];
    // SAFETY: reading eight bytes into a local buffer from an eventfd.
    unsafe { libc::read(a.wake.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    a.param.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Wait until one of `fds` has something to read or `timeout` passes, None meaning
/// forever. Which one is not said: the caller drains them all.
fn wait(fds: &[RawFd], timeout: Option<Duration>) {
    let mut polls: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let ms = timeout.map_or(-1, |t| t.as_millis() as libc::c_int);
    // SAFETY: the vector and its length describe the same entries, all owned for the
    // duration.
    unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, ms) };
}

/// Whether the socket has a message waiting, without blocking.
fn pending(socket: &Socket) -> bool {
    let mut fd = libc::pollfd { fd: socket.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one pollfd, owned for the duration.
    unsafe { libc::poll(&mut fd, 1, 0) > 0 }
}

/// Take the LEDs and drive them for the life of the process. Once.
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = thread::Builder::new().name("net-leds".into()).spawn(run);
    });
}

fn run() {
    let mut lights: Vec<Light> = TABLE
        .iter()
        .filter_map(|&(name, source, colour)| Light::open(name, source, colour))
        .collect();
    if lights.is_empty() {
        return;
    }
    let socket = Socket::subscribe(netlink::ROUTE, RTMGRP_LINK | RTMGRP_IPV4_IFADDR)
        .map_err(|e| eprintln!("net leds       no rtnetlink, polling: {e}"))
        .ok();
    let mut fds: Vec<RawFd> = asked().map(|a| a.wake.as_raw_fd()).into_iter().collect();
    fds.extend(socket.as_ref().map(|s| s.as_fd().as_raw_fd()));
    let mut buf = [0u8; 8192];
    look(&mut lights);
    let mut last = Instant::now();
    // The MCU's link group setting, which the colours are chosen under. Its own menu
    // can change it unannounced, so it is read again while a light moves.
    let mut group = link_group();
    let mut group_read = last;
    // Until when a preview holds the lights instead of the ports.
    let mut previewing: Option<Instant> = None;
    loop {
        let busy = lights.iter().any(|l| !l.lamp.still());
        let until_preview_ends = previewing.map(|u| u.saturating_duration_since(Instant::now()));
        let timeout = match (busy, until_preview_ends, &socket) {
            (true, ..) => Some(FRAME),
            (false, Some(left), _) => Some(left),
            // With no subscription the ports are polled instead.
            (false, None, None) => Some(Duration::from_secs(1)),
            (false, None, Some(_)) => None,
        };
        wait(&fds, timeout);
        // Drain what woke us; one message or a dozen mean the same thing.
        let mut told = socket.is_none() | WIFI_TOLD.swap(false, Ordering::Relaxed);
        if let Some(s) = &socket {
            while pending(s) {
                told = true;
                if s.recv(&mut buf).is_err() {
                    break;
                }
            }
        }
        let now = Instant::now();
        if let Some(p) = take_preview() {
            let floor = Timings::now().floor;
            for light in lights.iter_mut().filter(|l| !matches!(l.source, Source::Off)) {
                light.lamp.preview(p, floor);
            }
            previewing = Some(now + PREVIEW);
        } else if previewing.is_some_and(|until| now >= until) {
            previewing = None;
            look(&mut lights);
        } else if told && previewing.is_none() {
            look(&mut lights);
        }
        // A still light has nothing to catch up on, so time spent parked is not
        // time through the fade that starts now.
        let dt = if busy { (now - last).as_secs_f32() } else { 0.0 };
        last = now;
        if busy && now - group_read >= GROUP_READ {
            group = link_group();
            group_read = now;
        }
        let t = Timings::now();
        for light in &mut lights {
            light.lamp.step(dt, &t);
            light.show(group);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn timings() -> Timings {
        Timings { fade_out: 1.0, fade_in: 1.0, breathe: 2.5, solid: 0.6, floor: 0.35 }
    }

    fn run_for(lamp: &mut Lamp, secs: f32, t: &Timings) {
        let frames = (secs / 0.02).round() as usize;
        for _ in 0..frames {
            lamp.step(0.02, t);
        }
    }

    #[test]
    fn the_breath_reaches_zero_at_both_ends_and_one_at_the_peak() {
        assert!(breath(0.0).abs() < 1e-6);
        assert!(breath(0.999_999).abs() < 1e-4);
        assert!((breath(MU) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn link_fades_in_then_breathes_above_the_floor() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.enter(State::Link);
        run_for(&mut lamp, 0.5, &t);
        assert!((lamp.level() - 0.5).abs() < 0.01, "half way at half time");
        run_for(&mut lamp, 0.5, &t);
        assert!(!lamp.still());
        let mut low: f32 = 1.0;
        for _ in 0..200 {
            lamp.step(0.02, &t);
            low = low.min(lamp.level());
        }
        assert!((0.35 - 1e-4..0.36).contains(&low), "bottoms out on the floor: {low}");
    }

    #[test]
    fn ip_settles_solid_and_still() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.enter(State::Ip);
        run_for(&mut lamp, 0.7, &t);
        assert_eq!(lamp.level(), 1.0);
        assert!(lamp.still());
    }

    #[test]
    fn fade_out_drops_to_dark_below_the_last_steps() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.enter(State::Ip);
        run_for(&mut lamp, 1.0, &t);
        lamp.enter(State::Dark);
        run_for(&mut lamp, 0.6, &t);
        assert!(lamp.level() > DROP);
        run_for(&mut lamp, 0.2, &t);
        assert_eq!(lamp.level(), 0.0);
        assert!(lamp.still());
    }

    #[test]
    fn a_preview_starts_where_its_effect_does() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.preview(Param::FadeOut, t.floor);
        assert_eq!(lamp.level(), 1.0);
        run_for(&mut lamp, 1.0, &t);
        assert_eq!(lamp.level(), 0.0);
        lamp.preview(Param::FadeIn, t.floor);
        assert_eq!(lamp.level(), 0.0);
        run_for(&mut lamp, 1.0, &t);
        assert!(!lamp.still(), "breathing after the fade in");
        lamp.preview(Param::Solid, t.floor);
        assert_eq!(lamp.level(), t.floor);
        run_for(&mut lamp, 0.7, &t);
        assert_eq!(lamp.level(), 1.0);
        lamp.preview(Param::Breathe, t.floor);
        assert!(!lamp.still());
    }

    /// A reconnect has its address almost at once, and still breathes once first.
    #[test]
    fn an_early_address_waits_for_one_breath() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.enter(State::Link);
        run_for(&mut lamp, 0.4, &t);
        lamp.enter(State::Ip);
        // The rest of the fade in, then a whole 2.5s breath: still breathing before.
        run_for(&mut lamp, 0.6 + 2.4, &t);
        assert!(!lamp.still(), "still breathing");
        run_for(&mut lamp, 0.2, &t);
        assert_eq!(lamp.level(), 1.0);
        assert!(lamp.still(), "solid after one breath");
    }

    #[test]
    fn losing_the_address_while_waiting_keeps_breathing() {
        let t = timings();
        let mut lamp = Lamp::default();
        lamp.enter(State::Link);
        lamp.enter(State::Ip);
        lamp.enter(State::Link);
        run_for(&mut lamp, 5.0, &t);
        assert!(!lamp.still(), "no address, still breathing");
    }

    #[test]
    fn a_zero_fade_is_instant() {
        let t = Timings { solid: 0.0, ..timings() };
        let mut lamp = Lamp::default();
        lamp.enter(State::Ip);
        lamp.step(0.0, &t);
        assert_eq!(lamp.level(), 1.0);
    }

    #[test]
    fn defaults_are_the_eth_led_apps() {
        let shown: Vec<_> = Param::ALL.map(|p| p.at(p.default())).into();
        assert_eq!(shown, [1000, 2500, 50, 600, 400]);
        for p in Param::ALL {
            assert!(p.default() <= p.last());
        }
        assert_eq!(Param::Floor.at(Param::Floor.last()), 60);
        assert_eq!(Param::FadeIn.at(Param::FadeIn.last()), 5000);
        assert_eq!(Param::Breathe.at(Param::Breathe.last()), 8000);
    }

    /// What the driver sends the MCU for a `multi_intensity` at full brightness.
    fn driver(intensity: [u8; 3]) -> [u16; 3] {
        std::array::from_fn(|i| u16::from(intensity[i]) * STEPS[i] / 255)
    }

    /// The bytes the MCU gives the LED for RGB565 `steps`.
    fn bytes(steps: [u16; 3], group: u8) -> [u16; 3] {
        std::array::from_fn(|i| byte(steps[i], i, group))
    }

    /// Measured on eth0 with the link group at 51: the last dark and first lit step.
    #[test]
    fn the_mcu_lights_where_it_was_seen_to() {
        assert_eq!([byte(1, 0, 51), byte(2, 0, 51)], [0, 1], "red");
        assert_eq!([byte(2, 1, 51), byte(3, 1, 51)], [0, 1], "green");
        assert_eq!([byte(1, 2, 51), byte(2, 2, 51)], [0, 1], "blue");
    }

    /// The colour a light shows at `duty` under `group`, as bytes on the LED.
    fn shown(colour: [u8; 3], duty: f32, group: u8) -> [u16; 3] {
        bytes(pick(&shades(colour, group), duty), group)
    }

    const COLOURS: [[u8; 3]; 3] = [[120, 255, 0], [0, 255, 255], [255, 255, 255]];

    #[test]
    fn the_driver_lands_on_the_steps_chosen() {
        for group in [26, 51, 128, 255] {
            for colour in COLOURS {
                for (steps, _) in shades(colour, group) {
                    assert_eq!(driver(intensities(steps)), steps, "{colour:?} under {group}");
                }
            }
        }
    }

    /// At every group setting and every level: dark, or the colour asked for.
    #[test]
    fn no_level_shows_another_colour() {
        for group in 0..=u8::MAX {
            for colour in COLOURS {
                let anchor = (0..3).max_by_key(|&i| colour[i]).unwrap();
                let shades = shades(colour, group);
                let mut last = 0;
                for i in 0..=500 {
                    let b = bytes(pick(&shades, i as f32 / 500.0), group);
                    // Never darker for more duty, or a breath would flicker.
                    assert!(b[anchor] >= last, "{colour:?} under {group} dimmed at {i}");
                    last = b[anchor];
                    if b[anchor] == 0 {
                        assert_eq!(b, [0; 3], "{colour:?} under {group} at {i}");
                        continue;
                    }
                    for c in 0..3 {
                        let want = f32::from(colour[c]) / f32::from(colour[anchor]);
                        let got = f32::from(b[c]) / f32::from(b[anchor]);
                        let ok = if colour[c] == 0 {
                            b[c] == 0
                        } else {
                            (got / want - 1.0).abs() <= HUE_TOLERANCE
                        };
                        assert!(ok, "{colour:?} under {group} at {i} shows {b:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn lime_at_the_settings_it_was_seen_under() {
        let lime = [120, 255, 0];
        // The breath's default floor, 35% lightness, with the link group at 51.
        assert_eq!(shown(lime, duty(0.35), 51), [1, 2, 0]);
        assert_eq!(pick(&shades(lime, 255), 1.0), [15, 63, 0]);
        assert_eq!(pick(&shades(lime, 255), 0.0), [0; 3]);
        // Too dim a setting to tell red from no red: dark rather than green.
        assert!(shades(lime, 3).is_empty());
    }

    #[test]
    fn duty_is_monotonic_and_ends_at_one() {
        assert_eq!(duty(0.0), 0.0);
        assert!((duty(1.0) - 1.0).abs() < 1e-6);
        let mut prev = 0.0;
        for i in 1..=100 {
            let d = duty(i as f32 / 100.0);
            assert!(d > prev);
            prev = d;
        }
    }
}
