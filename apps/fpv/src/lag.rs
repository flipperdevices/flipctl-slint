//! How old the picture is: each USB block is stamped as the library hands it over, the
//! stamp rides along to the field it completes, and the age is taken when the field is
//! published and again when a page is built from it. Beyond that, the window, flipctl
//! and the panel add a fixed delay this cannot see.
//!
//! Each second sums up into one line, shown on the Info page, and appended to
//! /tmp/fpv-lag.log when that file exists, which is how a run is measured from outside.

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

const LOG: &str = "/tmp/fpv-lag.log";

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

/// Now, in microseconds since the first stamp.
pub fn now() -> u64 {
    epoch().elapsed().as_micros() as u64
}

/// A second's worth of one age, in microseconds.
#[derive(Default)]
struct Ages {
    count: AtomicU64,
    sum: AtomicU64,
    max: AtomicU64,
}

impl Ages {
    fn add(&self, us: u64) {
        self.count.fetch_add(1, Relaxed);
        self.sum.fetch_add(us, Relaxed);
        self.max.fetch_max(us, Relaxed);
    }

    /// Count, mean ms and max ms, and start again.
    fn take(&self) -> (u64, f64, f64) {
        let (n, sum, max) = (self.count.swap(0, Relaxed), self.sum.swap(0, Relaxed), self.max.swap(0, Relaxed));
        (n, if n > 0 { sum as f64 / n as f64 / 1000.0 } else { 0.0 }, max as f64 / 1000.0)
    }
}

static PUBLISHED: OnceLock<Ages> = OnceLock::new();
static DRAWN: OnceLock<Ages> = OnceLock::new();
/// What a paint costs: the shrink, and all of it.
static COST: [OnceLock<Ages>; 2] = [OnceLock::new(), OnceLock::new()];
/// The deepest each queue got: USB to front, front to colour, colour to back.
static DEPTH: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static LAST: Mutex<(u64, String)> = Mutex::new((0, String::new()));

/// A field went out, made from data stamped `t`.
pub fn published(t: u64) {
    PUBLISHED.get_or_init(Ages::default).add(now().saturating_sub(t));
}

/// A page was built from a field stamped `t`.
pub fn drawn(t: u64) {
    if t > 0 {
        DRAWN.get_or_init(Ages::default).add(now().saturating_sub(t));
    }
}

/// Paint step `which` has taken since `started`.
pub fn cost(which: usize, started: u64) {
    COST[which].get_or_init(Ages::default).add(now().saturating_sub(started));
}

/// Queue `which` held `n` when looked at.
pub fn depth(which: usize, n: usize) {
    DEPTH[which].fetch_max(n as u64, Relaxed);
    tick();
}

/// A timed line for the log alone: what the radio did, to see where time goes.
pub fn event(what: std::fmt::Arguments) {
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(LOG) {
        let _ = writeln!(f, "{:.3} {what}", now() as f64 / 1e6);
    }
}

/// The last second's line.
pub fn summary() -> String {
    LAST.lock().unwrap().1.clone()
}

fn tick() {
    let t = now();
    let mut last = LAST.lock().unwrap();
    if t < last.0 + 1_000_000 {
        return;
    }
    last.0 = t;
    let (np, mp, xp) = PUBLISHED.get_or_init(Ages::default).take();
    let (nd, md, xd) = DRAWN.get_or_init(Ages::default).take();
    let d = DEPTH.each_ref().map(|a| a.swap(0, Relaxed));
    let (_, ms, xs) = COST[0].get_or_init(Ages::default).take();
    let (_, mt, xt) = COST[1].get_or_init(Ages::default).take();
    last.1 = format!("{md:.0}/{xd:.0} ms q{}/{}/{}", d[0], d[1], d[2]);
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(LOG) {
        let _ = writeln!(
            f,
            "{:.1} fields {np} published {mp:.1}/{xp:.1} ms drawn {nd} {md:.1}/{xd:.1} ms queues {}/{}/{} shrink {ms:.1}/{xs:.1} paint {mt:.1}/{xt:.1} ms",
            t as f64 / 1e6,
            d[0],
            d[1],
            d[2]
        );
    }
}
