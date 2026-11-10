// The device and the decoder's threads.
//
//   BirriaSDR's reader thread: streams at the rate the mode wants, restarting the stream when
//     that changes (the rate cannot change during one). It copies each block into a pooled
//     buffer and hands it over without waiting; with no room the block is dropped and counted,
//     never stalling USB.
//   front (one fast core): scanning at 80 Msps, then checking at 20 Msps which of the channels
//     that stand out carry video; or a channel's video, and auto-tune.
//   colour (a slow core): the colour carrier from the front's video.
//   back (another fast core): line sync and fields, woven into frames for the window.
//   post (the slow cores): the optional clean-up of the latest frame.
//
// The window reads Shared and sends Commands.

use crate::birria::{Device, Gain, ANTENNA_AUTO};
use crate::channels::{self, Channel};
use crate::dsp::video::{Standard, HEIGHT, WIDTH};
use crate::front::{candidates, view_offset, Back, Colour, Front, ScanStep, Scanner, VideoBlock, HLOCKED, SCAN_RATE, VIEW_RATE};
use crate::pin;
use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender, TrySendError};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering::Relaxed};
use crate::post::Post;
use crate::weave::Weave;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// USB blocks waiting for the front, about 26 ms at 20 Msps. Short on purpose: when the
/// front stalls, as it does retuning, the oldest are dropped, so it resumes on fresh
/// samples instead of working through a backlog that the picture would trail by.
const IQ_BLOCKS: usize = 32;
const VIDEO_BLOCKS: usize = 64;
pub const NOT_VIEWING: usize = usize::MAX;

pub enum Command {
    View(usize),
    Scan,
    Gain(Gain),
    /// Restart the stream on the antenna in Shared::antenna.
    Antenna,
}

pub struct Frame {
    /// RGB, WIDTH x HEIGHT.
    pub pixels: Vec<u8>,
    pub serial: u64,
    /// The stamp of the block that completed it.
    pub t: u64,
    /// Its lumas for the panel, made for the shape in Shared::fill.
    pub lumas: Vec<u8>,
    pub fill: bool,
}

pub struct Shared {
    /// The latest field, and the latest cleaned-up frame.
    pub frame: Mutex<Frame>,
    frame_ready: Condvar,
    pub post_frame: Mutex<Frame>,
    pub post: AtomicBool,
    /// Whether the picture is stretched across, which sets the size its lumas are made at.
    pub fill: AtomicBool,
    /// Per channel, dB above the median of the last sweep.
    pub powers: Mutex<Vec<Option<f32>>>,
    /// Per channel, whether the last scan found video on it; None where it did not look.
    pub video: Mutex<Vec<Option<bool>>>,
    /// How far the scan is, 0 to 1, f32 bits: the sweep's first SWEEP_SHARE, then the checks.
    pub scan_progress: AtomicU32,
    pub viewing: AtomicUsize,
    pub scanning: AtomicBool,
    /// The rate the stream runs at, or is restarting to.
    pub rate: AtomicU32,
    /// Samples delivered by BirriaSDR, and those dropped because a thread fell behind.
    pub input: AtomicU64,
    pub lost_usb: AtomicU64,
    pub lost_video: AtomicU64,
    pub gaps: AtomicU64,
    pub hlock: AtomicI32,
    pub vlock: AtomicI32,
    pub field_lines: AtomicI32,
    /// Video::good_fields of the decoder.
    good_fields: AtomicU32,
    pub ntsc: AtomicBool,
    /// Decode colour; and whether the last field had it.
    pub colour: AtomicBool,
    pub has_colour: AtomicBool,
    pub signal_hz: AtomicI64,
    /// Settings the window changes: Standard as 0 Auto, 1 PAL, 2 NTSC.
    pub standard: AtomicU8,
    pub invert: AtomicBool,
    pub afc: AtomicBool,
    pub to_strongest: AtomicBool,
    pub auto_gain: AtomicBool,
    /// The gain in use, packed: LNA, LPF and digital in the low three bytes.
    pub gain: AtomicU32,
    /// Viewing rate: 20 or 40 Msps.
    pub view_rate: AtomicU32,
    /// The antenna asked for, chain 0 or 1 or ANTENNA_AUTO, and the chain captured.
    pub antenna: AtomicU8,
    pub antenna_used: AtomicU8,
    /// Auto-tune's correction for the video level, f32 bits, taken by the back thread.
    nudge: AtomicU32,
    quit: AtomicBool,
    pub error: Mutex<Option<String>>,
}

impl Shared {
    fn add_nudge(&self, v: f32) {
        let _ = self.nudge.fetch_update(Relaxed, Relaxed, |b| Some((f32::from_bits(b) + v).to_bits()));
    }

    fn take_nudge(&self) -> f32 {
        f32::from_bits(self.nudge.swap(0f32.to_bits(), Relaxed))
    }

    pub fn gain(&self) -> Gain {
        let g = self.gain.load(Relaxed);
        Gain { lna: g as u8, lpf: (g >> 8) as u8, digital: (g >> 16) as u8 }
    }

    fn set_gain(&self, g: Gain) {
        self.gain.store(g.lna as u32 | (g.lpf as u32) << 8 | (g.digital as u32) << 16, Relaxed);
    }

    fn fail(&self, e: String) {
        *self.error.lock().unwrap() = Some(e);
    }
}

/// A block of interleaved I,Q and the rate it was captured at.
struct Block {
    rate: u32,
    /// When the library handed it over, in lag::now()'s terms.
    t: u64,
    iq: Vec<i16>,
}

pub struct Radio {
    pub shared: Arc<Shared>,
    device: Arc<Device>,
    cmd: Option<Sender<Command>>,
    threads: Vec<JoinHandle<()>>,
}

/// Cores for the front, back and colour threads from FPV_CPUS, else the fast cores 4 and 5 and
/// the slow core 3: BirriaSDR keeps its USB threads to 6 and 7, and at 80 Msps they need most of
/// both; colour on 7 starved one, the chip overran about 45 times a second and the picture
/// never held lock. Colour costs the same at every rate, about 70% of a slow core.
fn decoder_cpus() -> [Option<usize>; 3] {
    match std::env::var("FPV_CPUS").ok().as_deref().map(pin::parse) {
        Some(Some(c)) => [0, 1, 2].map(|i| c.get(i).or(c.last()).copied()),
        Some(None) => [None; 3],
        None => [Some(4), Some(5), Some(3)],
    }
}

struct Pool<T> {
    tx: Sender<T>,
    rx: Receiver<T>,
}

impl<T> Pool<T> {
    fn new(n: usize, make: impl Fn() -> T) -> Pool<T> {
        let (tx, rx) = bounded(n);
        for _ in 0..n {
            let _ = tx.send(make());
        }
        Pool { tx, rx }
    }

    fn clone(&self) -> Pool<T> {
        Pool { tx: self.tx.clone(), rx: self.rx.clone() }
    }

    fn put(&self, v: T) {
        let _ = self.tx.try_send(v);
    }
}

impl Radio {
    /// Without `colour` there is no colour thread at all: the front hands its blocks
    /// straight to the back, a queue and a thread fewer between a field and the panel.
    pub fn start(gain: Gain, colour: bool, repaint: impl Fn() + Send + Sync + 'static) -> Result<Radio, String> {
        // BirriaSDR reads these on open: the endpoint burst birria_burst patched in, and
        // the fast cores its USB threads keep to, apart from the decoder's.
        for (k, v) in [("BIRRIA_BURST", "3"), ("BIRRIA_CPUS", "6-7")] {
            if std::env::var_os(k).is_none() {
                std::env::set_var(k, v);
            }
        }
        debug_log(format_args!("radio start"));
        if Device::count() <= 0 {
            return Err("The Wi-Fi chip was not found.".into());
        }
        let device = Arc::new(Device::open(0).map_err(|e| format!("Could not open the Wi-Fi chip: {e}. Is another SDR app running?"))?);
        debug_log(format_args!("opened"));
        device.set_gain(gain).map_err(|e| e.to_string())?;
        let all = channels::all();
        device.set_center_freq((all[0].hz() - view_offset(VIEW_RATE)) as u64).map_err(|e| e.to_string())?;

        let shared = Arc::new(Shared {
            frame: Mutex::new(Frame { pixels: vec![0; WIDTH * HEIGHT * 3], serial: 0, t: 0, lumas: Vec::new(), fill: false }),
            frame_ready: Condvar::new(),
            post_frame: Mutex::new(Frame { pixels: vec![0; WIDTH * HEIGHT * 3], serial: 0, t: 0, lumas: Vec::new(), fill: false }),
            fill: AtomicBool::new(false),
            post: AtomicBool::new(false),
            powers: Mutex::new(vec![None; all.len()]),
            video: Mutex::new(vec![None; all.len()]),
            scan_progress: AtomicU32::new(0f32.to_bits()),
            viewing: AtomicUsize::new(NOT_VIEWING),
            scanning: AtomicBool::new(false),
            rate: AtomicU32::new(VIEW_RATE as u32),
            input: AtomicU64::new(0),
            lost_usb: AtomicU64::new(0),
            lost_video: AtomicU64::new(0),
            gaps: AtomicU64::new(0),
            hlock: AtomicI32::new(0),
            vlock: AtomicI32::new(0),
            field_lines: AtomicI32::new(0),
            good_fields: AtomicU32::new(0),
            ntsc: AtomicBool::new(false),
            colour: AtomicBool::new(colour),
            has_colour: AtomicBool::new(false),
            signal_hz: AtomicI64::new(0),
            standard: AtomicU8::new(0),
            invert: AtomicBool::new(false),
            afc: AtomicBool::new(true),
            to_strongest: AtomicBool::new(true),
            auto_gain: AtomicBool::new(true),
            gain: AtomicU32::new(0),
            view_rate: AtomicU32::new(VIEW_RATE as u32),
            antenna: AtomicU8::new(ANTENNA_AUTO),
            antenna_used: AtomicU8::new(0),
            nudge: AtomicU32::new(0f32.to_bits()),
            quit: AtomicBool::new(false),
            error: Mutex::new(None),
        });

        shared.set_gain(gain);
        let (iq_tx, iq_rx) = bounded::<Block>(IQ_BLOCKS);
        let iq_old = iq_rx.clone();
        // The queue full, and one more in each of the reader's and the front's hands
        let iq_pool = Pool::new(IQ_BLOCKS + 2, || Block { rate: 0, t: 0, iq: Vec::with_capacity(2 * 16384) });
        let (video_tx, colour_rx) = bounded::<VideoBlock>(VIDEO_BLOCKS);
        let (colour_tx, video_rx) = if colour { bounded::<VideoBlock>(VIDEO_BLOCKS) } else { (video_tx.clone(), colour_rx.clone()) };
        let video_pool = Pool::new(VIDEO_BLOCKS, VideoBlock::default);
        let (cmd_tx, cmd_rx) = bounded::<Command>(64);
        let [front_cpu, back_cpu, colour_cpu] = decoder_cpus();
        let repaint = Arc::new(repaint);
        let mut threads = Vec::new();

        {
            let (device, shared, pool) = (device.clone(), shared.clone(), iq_pool.clone());
            threads.push(std::thread::Builder::new().name("fpv-usb".into()).spawn(move || reader(&device, &shared, &pool, &iq_tx, &iq_old)).unwrap());
        }
        {
            let (device, shared, repaint, video_pool) = (device.clone(), shared.clone(), repaint.clone(), video_pool.clone());
            threads.push(std::thread::Builder::new().name("fpv-front".into()).spawn(move || {
                if let Some(cpu) = front_cpu {
                    pin::pin(cpu);
                }
                front_loop(&device, &shared, &all, iq_rx, iq_pool, cmd_rx, video_tx, video_pool, &*repaint);
            }).unwrap());
        }
        if colour {
            let (shared, video_pool) = (shared.clone(), video_pool.clone());
            threads.push(std::thread::Builder::new().name("fpv-colour".into()).spawn(move || {
                if let Some(cpu) = colour_cpu {
                    pin::pin(cpu);
                }
                colour_loop(&shared, colour_rx, colour_tx, video_pool);
            }).unwrap());
        }
        {
            let (shared, repaint) = (shared.clone(), repaint.clone());
            threads.push(std::thread::Builder::new().name("fpv-post".into()).spawn(move || {
                pin::pin_to(&[0, 1, 2, 3]);
                post_loop(&shared, &*repaint);
            }).unwrap());
        }
        {
            let shared = shared.clone();
            threads.push(std::thread::Builder::new().name("fpv-back".into()).spawn(move || {
                if let Some(cpu) = back_cpu {
                    pin::pin(cpu);
                }
                back_loop(&shared, video_rx, video_pool, &*repaint);
            }).unwrap());
        }
        Ok(Radio { shared, device, cmd: Some(cmd_tx), threads })
    }

    pub fn send(&self, c: Command) {
        if let Some(cmd) = &self.cmd {
            let _ = cmd.send(c);
        }
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.shared.quit.store(true, Relaxed);
        self.shared.frame_ready.notify_all();
        self.device.cancel();
        self.cmd = None;
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Stream at shared.rate until told to quit; a cancel without quit restarts at the new rate
/// and antenna.
fn reader(device: &Device, shared: &Shared, pool: &Pool<Block>, iq_tx: &Sender<Block>, iq_old: &Receiver<Block>) {
    while !shared.quit.load(Relaxed) {
        let rate = shared.rate.load(Relaxed);
        if let Err(e) = device.set_sample_rate(rate) {
            shared.fail(format!("Sample rate {} Msps: {e}", rate / 1_000_000));
            return;
        }
        if let Err(e) = device.set_antenna(shared.antenna.load(Relaxed)) {
            shared.fail(format!("Antenna: {e}"));
        }
        debug_log(format_args!("stream at {} Msps", rate / 1_000_000));
        let first = std::sync::atomic::AtomicBool::new(true);
        let result = device.read(
            |iq| {
                let n = (iq.len() / 2) as u64;
                if first.swap(false, Relaxed) {
                    debug_log(format_args!("first samples"));
                }
                shared.input.fetch_add(n, Relaxed);
                // A scan stops reading while it retunes and skips what it loses
                let lost = || {
                    if !shared.scanning.load(Relaxed) {
                        shared.lost_usb.fetch_add(n, Relaxed);
                    }
                };
                let Ok(mut block) = pool.rx.try_recv() else {
                    lost();
                    return;
                };
                block.rate = rate;
                block.t = crate::lag::now();
                block.iq.clear();
                block.iq.extend_from_slice(iq);
                // Full: the oldest goes, not this one
                if let Err(TrySendError::Full(block)) = iq_tx.try_send(block) {
                    lost();
                    if let Ok(old) = iq_old.try_recv() {
                        pool.put(old);
                    }
                    if let Err(TrySendError::Full(block)) = iq_tx.try_send(block) {
                        pool.put(block);
                    }
                }
            },
            |_| {
                debug_log(format_args!("device gap"));
                shared.gaps.fetch_add(1, Relaxed);
            },
        );
        if let Err(e) = result {
            if shared.quit.load(Relaxed) {
                return;
            }
            shared.fail(format!("The stream stopped: {e}"));
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

enum Mode {
    Idle,
    View,
    Scan,
    /// After a sweep: viewing each candidate in `queue` in turn, from `at`, to see whether it
    /// carries video. `samples` is how much of it the front has had, `base` the decoder's good
    /// fields once it had settled.
    Check { queue: Vec<usize>, at: usize, samples: u64, base: Option<u32> },
}

/// A channel checked for video, in seconds of its signal, not of the clock: after a restart
/// the stream arrives in a burst. Time to let pass after tuning, the good fields that make it
/// video, with horizontal lock, and how long it gets.
const CHECK_SETTLE_S: f64 = 0.1;
const CHECK_FIELDS: u32 = 10;
const CHECK_TIMEOUT_S: f64 = 0.6;
/// The share of the scan's progress the sweep takes, the checks the rest: about the time each
/// takes with a candidate or two, the restart to 20 Msps included.
const SWEEP_SHARE: f32 = 0.4;
/// Checks run at 20 Msps, whatever the viewing rate: the sweep's 80 Msps does not decode
/// reliably, and 20 is the lightest to restart into and run.
const CHECK_RATE: f64 = 20e6;

#[allow(clippy::too_many_arguments)]
/// FPV_DUMP=FILE: 0.5 s of what the front is fed, raw, 2 s after each change of channel, to
/// FILE-0.cs16 to FILE-3.cs16 in turn; and a log of every change on stderr, timed.
struct Dump {
    path: String,
    count: usize,
    file: Option<std::io::BufWriter<std::fs::File>>,
    skip: usize,
    left: usize,
}

static DEBUG_START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// A line of the FPV_DUMP log.
fn debug_log(what: std::fmt::Arguments) {
    crate::lag::event(what);
    if std::env::var_os("FPV_DUMP").is_some() {
        let t = DEBUG_START.get_or_init(Instant::now).elapsed().as_secs_f64();
        eprintln!("{t:8.3} {what}");
    }
}

impl Dump {
    fn from_env() -> Option<Dump> {
        let path = std::env::var("FPV_DUMP").ok()?;
        Some(Dump { path, count: 0, file: None, skip: 0, left: 0 })
    }

    /// Start a capture: the channel changed.
    fn arm(&mut self) {
        let path = format!("{}-{}.cs16", self.path, self.count % 4);
        self.count += 1;
        self.file = std::fs::File::create(&path).map(std::io::BufWriter::new).map_err(|e| eprintln!("FPV_DUMP {path}: {e}")).ok();
        (self.skip, self.left) = (0, 0);
    }

    fn add(&mut self, iq: &[i16], rate: u32, center: f64, channel: f64) {
        use std::io::Write;
        let Some(file) = &mut self.file else { return };
        if self.skip == 0 && self.left == 0 {
            (self.skip, self.left) = (2 * rate as usize, rate as usize / 2);
        }
        let n = iq.len() / 2;
        if self.skip > 0 {
            self.skip = self.skip.saturating_sub(n);
            return;
        }
        let bytes: Vec<u8> = iq.iter().flat_map(|v| v.to_le_bytes()).collect();
        let _ = file.write_all(&bytes);
        self.left = self.left.saturating_sub(n);
        if self.left == 0 {
            let _ = file.flush();
            self.file = None;
            debug_log(format_args!("captured {}-{}.cs16: --rate {rate} --center {center:.0} --channel {channel:.0}", self.path, (self.count - 1) % 4));
        }
    }
}

fn front_loop(
    device: &Device,
    shared: &Shared,
    all: &[Channel],
    iq_rx: Receiver<Block>,
    iq_pool: Pool<Block>,
    cmd_rx: Receiver<Command>,
    video_tx: Sender<VideoBlock>,
    video_pool: Pool<VideoBlock>,
    repaint: &(dyn Fn() + Send + Sync),
) {
    let mut front = Front::new(VIEW_RATE);
    // Where the chip is tuned while viewing: auto-tune moves it
    let center = std::cell::Cell::new(0.0f64);
    let mut scanner = Scanner::new(SCAN_RATE);
    // Auto gain is the library's AGC. A scan compares channel powers, so it holds the gain
    // manual while one runs; the sliders follow the AGC's codes, read every GAIN_EVERY blocks.
    const GAIN_EVERY: u64 = 8;
    let mut auto_applied: Option<bool> = None;
    let mut blocks = 0u64;
    let mut mode = Mode::Idle;
    let mut last_view = 0usize;
    // When the stream was asked to change rate, to ask again if it has not
    let mut rate_asked = Instant::now();
    let mut dump = Dump::from_env();

    // While the stream starts or restarts, the device answers BUSY: try again for up to 3 s
    let tune = |hz: f64| {
        debug_log(format_args!("tune {:.3} MHz", hz / 1e6));
        let started = Instant::now();
        for busy in 0..150 {
            match device.set_center_freq(hz as u64) {
                Err(e) if e.code == crate::birria::BUSY => std::thread::sleep(Duration::from_millis(20)),
                Err(e) => return shared.fail(format!("Tuning to {:.1} MHz: {e}", hz / 1e6)),
                Ok(()) => {
                    debug_log(format_args!("tuned in {:.0} ms, {busy} busy", started.elapsed().as_secs_f64() * 1e3));
                    return;
                }
            }
        }
        shared.fail(format!("Tuning to {:.1} MHz: the device stayed busy", hz / 1e6));
    };
    let set_rate = |rate: f64, asked: &mut Instant| {
        if shared.rate.swap(rate as u32, Relaxed) != rate as u32 {
            debug_log(format_args!("rate {} Msps: restart", rate / 1e6));
            device.cancel();
            *asked = Instant::now();
        }
    };
    // Blocks queued before a retune are the last channel's: a check must not count them
    let drain = || {
        while let Ok(b) = iq_rx.try_recv() {
            iq_pool.put(b);
        }
    };
    let aim = |index: usize, rate: f64, front: &mut Front, asked: &mut Instant| {
        if front.rate() != rate {
            *front = Front::new(rate);
        }
        set_rate(rate, asked);
        center.set(all[index].hz() - view_offset(rate));
        tune(center.get());
        front.reset_afc();
        shared.take_nudge();
    };
    let view = |index: usize, front: &mut Front, mode: &mut Mode, last_view: &mut usize, asked: &mut Instant| {
        let rate = shared.view_rate.load(Relaxed) as f64;
        debug_log(format_args!("view {} at {} Msps", all[index].name(), rate / 1e6));
        aim(index, rate, front, asked);
        shared.viewing.store(index, Relaxed);
        shared.scanning.store(false, Relaxed);
        *last_view = index;
        *mode = Mode::View;
    };

    loop {
        for c in cmd_rx.try_iter() {
            match c {
                Command::View(i) => {
                    view(i, &mut front, &mut mode, &mut last_view, &mut rate_asked);
                    if let Some(d) = &mut dump {
                        d.arm();
                    }
                }
                Command::Scan => {
                    shared.viewing.store(NOT_VIEWING, Relaxed);
                    shared.scanning.store(true, Relaxed);
                    shared.scan_progress.store(0f32.to_bits(), Relaxed);
                    set_rate(SCAN_RATE, &mut rate_asked);
                    tune(scanner.start(all.iter().map(Channel::hz).collect()));
                    mode = Mode::Scan;
                }
                Command::Antenna => {
                    debug_log(format_args!("antenna {}: restart", shared.antenna.load(Relaxed)));
                    device.cancel();
                }
                Command::Gain(g) => {
                    match device.set_gain(g) {
                        Ok(()) => {
                            debug_log(format_args!("gain {}/{}/{}", g.lna, g.lpf, g.digital));
                            shared.set_gain(g)
                        }
                        Err(e) => shared.fail(format!("Gain: {e}")),
                    }
                }
            }
        }
        let block = match iq_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(b) => b,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        crate::lag::depth(0, iq_rx.len());
        // Blocks from before a rate change belong to the previous mode. A cancel that came
        // before the stream had started is lost, so ask again after a while.
        let wanted = shared.rate.load(Relaxed);
        if block.rate != wanted {
            if rate_asked.elapsed() > Duration::from_secs(2) {
                device.cancel();
                rate_asked = Instant::now();
            }
            iq_pool.put(block);
            continue;
        }
        let auto = shared.auto_gain.load(Relaxed) && !matches!(mode, Mode::Scan);
        if auto_applied != Some(auto) {
            let applied = if auto { device.set_auto_gain() } else { device.set_gain(shared.gain()) };
            match applied {
                Ok(()) => auto_applied = Some(auto),
                Err(e) if e.code == crate::birria::BUSY => {}
                Err(e) => shared.fail(format!("Gain: {e}")),
            }
        }
        blocks += 1;
        if blocks % GAIN_EVERY == 0 {
            if auto {
                if let Ok(g) = device.gain_codes() {
                    shared.set_gain(g);
                }
            }
            shared.antenna_used.store(device.antenna(), Relaxed);
        }
        match mode {
            Mode::Idle => {}
            Mode::Scan => match scanner.feed(&block.iq) {
                ScanStep::Continue => {}
                ScanStep::Tune(hz) => {
                    shared.scan_progress.store((SWEEP_SHARE * scanner.progress()).to_bits(), Relaxed);
                    tune(hz)
                }
                ScanStep::Done => {
                    shared.scan_progress.store(SWEEP_SHARE.to_bits(), Relaxed);
                    let mut sorted = scanner.powers.clone();
                    sorted.sort_by(f32::total_cmp);
                    let median = sorted[sorted.len() / 2].max(1e-12);
                    let db: Vec<f32> = scanner.powers.iter().map(|p| 10.0 * (p.max(1e-12) / median).log10()).collect();
                    let hz: Vec<f64> = all.iter().map(Channel::hz).collect();
                    let queue = candidates(&db, &hz);
                    debug_log(format_args!("sweep done: checking {}", queue.iter().map(|&i| all[i].name()).collect::<Vec<_>>().join(" ")));
                    *shared.powers.lock().unwrap() = db.into_iter().map(Some).collect();
                    *shared.video.lock().unwrap() = vec![None; all.len()];
                    if let Some(&first) = queue.first() {
                        aim(first, CHECK_RATE, &mut front, &mut rate_asked);
                        drain();
                        mode = Mode::Check { queue, at: 0, samples: 0, base: None };
                    } else {
                        // Nothing stands out of the noise: stay
                        view(last_view, &mut front, &mut mode, &mut last_view, &mut rate_asked);
                        if let Some(d) = &mut dump {
                            d.arm();
                        }
                    }
                    repaint();
                }
            },
            Mode::View | Mode::Check { .. } => {
                if let (Mode::View, Some(d)) = (&mode, &mut dump) {
                    d.add(&block.iq, block.rate, center.get(), all[last_view].hz());
                }
                // Video runs at 10 Msps
                let n_video = (block.iq.len() / 2) as u64 * 10_000_000 / block.rate as u64;
                match video_pool.rx.try_recv() {
                    Ok(mut video) => {
                        video.clear();
                        video.t = block.t;
                        front.process(&block.iq, &mut video);
                        if let Err(TrySendError::Full(video)) = video_tx.try_send(video) {
                            shared.lost_video.fetch_add(n_video, Relaxed);
                            video_pool.put(video);
                        }
                    }
                    Err(_) => {
                        shared.lost_video.fetch_add(n_video, Relaxed);
                    }
                }
                if shared.afc.load(Relaxed) {
                    let locked = shared.hlock.load(Relaxed) > HLOCKED;
                    if let Some(hz) = front.afc(locked, &mut |n| shared.add_nudge(n)) {
                        // Retune the chip, so the channel stays a quarter of the rate below it
                        center.set(center.get() + hz);
                        tune(center.get());
                        front.reset_afc();
                    }
                }
                shared.signal_hz.store(front.signal_offset() as i64, Relaxed);

                // Checking: a few good fields with horizontal lock is video, nothing in time is
                // not; then the next candidate, or, after the last, the strongest with video
                let mut done = None;
                if let Mode::Check { queue, at, samples, base } = &mut mode {
                    *samples += (block.iq.len() / 2) as u64;
                    let t = *samples as f64 / CHECK_RATE;
                    let good = shared.good_fields.load(Relaxed);
                    let (fields, hlock) = (good.wrapping_sub(base.unwrap_or(good)), shared.hlock.load(Relaxed));
                    if t >= CHECK_SETTLE_S {
                        base.get_or_insert(good);
                    }
                    let video = base.is_some() && fields >= CHECK_FIELDS && hlock > HLOCKED;
                    let checked = *at as f64 + (t / CHECK_TIMEOUT_S).min(1.0);
                    let progress = SWEEP_SHARE + (1.0 - SWEEP_SHARE) * (checked / queue.len() as f64) as f32;
                    shared.scan_progress.store(progress.to_bits(), Relaxed);
                    if video || t >= CHECK_TIMEOUT_S {
                        let ch = queue[*at];
                        debug_log(format_args!("check {}: {} ({fields} fields, hlock {hlock}, {t:.2} s)", all[ch].name(), if video { "video" } else { "no video" }));
                        let mut found = shared.video.lock().unwrap();
                        for (f, c) in found.iter_mut().zip(all) {
                            if c.mhz == all[ch].mhz {
                                *f = Some(video);
                            }
                        }
                        *at += 1;
                        if *at < queue.len() {
                            (*samples, *base) = (0, None);
                            aim(queue[*at], CHECK_RATE, &mut front, &mut rate_asked);
                            drain();
                        } else {
                            let strongest = queue.iter().copied().find(|&i| found[i] == Some(true));
                            done = Some(match strongest {
                                Some(i) if shared.to_strongest.load(Relaxed) => i,
                                _ => last_view,
                            });
                        }
                        repaint();
                    }
                }
                if let Some(next) = done {
                    view(next, &mut front, &mut mode, &mut last_view, &mut rate_asked);
                    if let Some(d) = &mut dump {
                        d.arm();
                    }
                }
            }
        }
        iq_pool.put(block);
    }
}

/// The latest frame through the clean-up, while it is switched on; frames that arrive while
/// one is being processed are skipped.
fn post_loop(shared: &Shared, repaint: &(dyn Fn() + Send + Sync)) {
    let mut post = Post::new();
    let mut pixels = vec![0u8; WIDTH * HEIGHT * 3];
    let mut out = vec![0u8; WIDTH * HEIGHT * 3];
    let mut seen = 0u64;
    let mut t: u64;
    while !shared.quit.load(Relaxed) {
        {
            let mut f = shared.frame.lock().unwrap();
            while f.serial == seen && !shared.quit.load(Relaxed) {
                f = shared.frame_ready.wait_timeout(f, Duration::from_millis(200)).unwrap().0;
            }
            seen = f.serial;
            if !shared.post.load(Relaxed) {
                continue;
            }
            pixels.copy_from_slice(&f.pixels);
            t = f.t;
        }
        post.process(&pixels, &mut out);
        let fill = shared.fill.load(Relaxed);
        let lumas = crate::picture::lumas_for(&out, if fill { crate::picture::Fit::Fill } else { crate::picture::Fit::Box });
        {
            let mut p = shared.post_frame.lock().unwrap();
            p.pixels.copy_from_slice(&out);
            p.serial += 1;
            p.t = t;
            p.lumas = lumas;
            p.fill = fill;
        }
        repaint();
    }
}

/// Every block passes here in order, and gets colour while it is switched on.
fn colour_loop(shared: &Shared, rx: Receiver<VideoBlock>, tx: Sender<VideoBlock>, pool: Pool<VideoBlock>) {
    let mut colour = Colour::new();
    for mut block in rx.iter() {
        crate::lag::depth(1, rx.len());
        if shared.colour.load(Relaxed) {
            colour.process(&mut block, shared.ntsc.load(Relaxed));
        }
        if let Err(TrySendError::Full(block)) = tx.try_send(block) {
            shared.lost_video.fetch_add(block.len() as u64, Relaxed);
            pool.put(block);
        }
    }
}

fn back_loop(shared: &Shared, video_rx: Receiver<VideoBlock>, video_pool: Pool<VideoBlock>, repaint: &(dyn Fn() + Send + Sync)) {
    let mut back = Back::new();
    let mut standard = Standard::Auto;
    // Each field woven into a frame here, once, for the window and the clean-up alike
    let mut weave = Weave::new();
    let mut woven = vec![0u8; WIDTH * HEIGHT * 3];
    for video in video_rx.iter() {
        let want = match shared.standard.load(Relaxed) {
            1 => Standard::Pal,
            2 => Standard::Ntsc,
            _ => Standard::Auto,
        };
        if want != standard {
            standard = want;
            back.set_standard(want);
        }
        back.video.invert = shared.invert.load(Relaxed);
        back.video.colour = shared.colour.load(Relaxed);
        back.video.offset = (back.video.offset + shared.take_nudge()).clamp(-1.0, 1.0);
        crate::lag::depth(2, video_rx.len());
        let t = video.t;
        back.process(&video, &mut |v, locked| {
            weave.process(&v.pixels, v.active_lines(), v.field_lowered, &mut woven);
            let fill = shared.fill.load(Relaxed);
            let lumas = crate::picture::lumas_for(&woven, if fill { crate::picture::Fit::Fill } else { crate::picture::Fit::Box });
            {
                let mut f = shared.frame.lock().unwrap();
                f.pixels.copy_from_slice(&woven);
                f.serial += 1;
                f.t = t;
                f.lumas = lumas;
                f.fill = fill;
            }
            crate::lag::published(t);
            shared.frame_ready.notify_all();
            if t > 0 && shared.good_fields.load(Relaxed) == 0 && v.good_fields > 0 {
                debug_log(format_args!("first good field"));
            }
            shared.hlock.store(locked, Relaxed);
            shared.vlock.store(v.vlock, Relaxed);
            shared.field_lines.store(v.field_lines, Relaxed);
            shared.good_fields.store(v.good_fields, Relaxed);
            shared.ntsc.store(v.effective() == Standard::Ntsc, Relaxed);
            shared.has_colour.store(v.has_colour, Relaxed);
            repaint();
        });
        video_pool.put(video);
    }
}
