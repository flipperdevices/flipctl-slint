//! The benchmark, run the way CrystalDiskMark 8 runs its default profile.
//!
//! CrystalDiskMark hands each test to DiskSpd (`-b<size> -o<queue> -t1 -W0 -S`, `-r`
//! for random, `-w0` or `-w100`) on a test file it fills first with one megabyte of
//! random data written over and over. `-S` is the OS cache bypassed and nothing
//! more, which is what O_DIRECT is here; writes are not forced through the drive's
//! own cache. Each test is run once unscored and then `passes` times, and the best
//! pass is the score. All four reads go first, then all four writes, with a pause
//! between every two tests so a drive that heats or caches settles. MB/s is
//! 1,000,000 bytes a second.
//!
//! Queue depth is real: io_uring keeps that many requests in flight from one thread,
//! which is what DiskSpd's `-o` does with overlapped I/O.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use io_uring::{opcode, types, IoUring};

pub struct Test {
    pub name: &'static str,
    pub block: usize,
    pub queue: u32,
    pub random: bool,
}

pub const TESTS: [Test; 4] = [
    Test { name: "SEQ1M Q8T1", block: 1 << 20, queue: 8, random: false },
    Test { name: "SEQ1M Q1T1", block: 1 << 20, queue: 1, random: false },
    Test { name: "RND4K Q32T1", block: 4 << 10, queue: 32, random: true },
    Test { name: "RND4K Q1T1", block: 4 << 10, queue: 1, random: true },
];

#[derive(Clone, Copy)]
pub struct Config {
    pub size: u64,
    pub passes: u32,
    pub measure: Duration,
    pub interval: Duration,
}

impl Config {
    pub fn crystal(size: u64) -> Self {
        Self { size, passes: 3, measure: Duration::from_secs(5), interval: Duration::from_secs(5) }
    }

    /// Steps the progress bar counts: the fill, every run and every pause, each
    /// about as long as a run.
    pub fn steps(&self) -> u32 {
        let tests = 2 * TESTS.len() as u32;
        1 + tests * (self.passes + 1) + (tests - 1)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Mounting and checking space.
    Setup,
    /// Writing the test file, as a fraction done.
    Filling(f64),
    /// Seconds left of a pause.
    Pause(u32),
    /// Running a test; pass 0 is the unscored one. `mbps` is this pass so far.
    Run { test: usize, write: bool, pass: u32, mbps: f64 },
    Cleaning,
    Done(Duration),
    Stopped,
    Failed(String),
}

impl Phase {
    pub fn running(&self) -> bool {
        !matches!(self, Phase::Done(_) | Phase::Stopped | Phase::Failed(_))
    }
}

/// What the run has found so far, shared with the page.
#[derive(Clone, Debug)]
pub struct Report {
    pub phase: Phase,
    pub read: [Option<f64>; 4],
    pub write: [Option<f64>; 4],
    pub step: u32,
    pub steps: u32,
    pub passes: u32,
}

impl Report {
    pub fn new(config: &Config) -> Self {
        let (steps, passes) = (config.steps(), config.passes);
        Self { phase: Phase::Setup, read: [None; 4], write: [None; 4], step: 0, steps, passes }
    }
}

pub type Shared = Arc<Mutex<Report>>;

/// Memory aligned for O_DIRECT, which wants the buffer on a block boundary too.
struct Buffer {
    ptr: *mut u8,
    layout: std::alloc::Layout,
}

impl Buffer {
    fn random(len: usize, seed: &mut u64) -> Self {
        let layout = std::alloc::Layout::from_size_align(len, 4096).unwrap();
        let ptr = unsafe { std::alloc::alloc(layout) };
        assert!(!ptr.is_null(), "out of memory");
        let mut buf = Self { ptr, layout };
        for chunk in buf.slice().chunks_mut(8) {
            let r = next(seed).to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
        buf
    }

    fn slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    fn at(&self, offset: usize) -> *mut u8 {
        unsafe { self.ptr.add(offset) }
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        unsafe { std::alloc::dealloc(self.ptr, self.layout) }
    }
}

fn next(state: &mut u64) -> u64 {
    // xorshift64*, plenty for picking offsets and making incompressible data.
    *state ^= *state >> 12;
    *state ^= *state << 25;
    *state ^= *state >> 27;
    state.wrapping_mul(0x2545_f491_4f6c_dd1d)
}

pub fn open(path: &std::path::Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .custom_flags(libc::O_DIRECT)
        .open(path)
}

/// Write the whole file from one megabyte of random data, as CrystalDiskMark does.
pub fn fill(file: &File, size: u64, stop: &AtomicBool, mut done: impl FnMut(f64)) -> io::Result<bool> {
    const CHUNK: usize = 1 << 20;
    let mut seed = 0x9e37_79b9_7f4a_7c15;
    let mut buf = Buffer::random(CHUNK, &mut seed);
    let mut at = 0;
    while at < size {
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        file.write_all_at(buf.slice(), at)?;
        at += CHUNK as u64;
        done(at as f64 / size as f64);
    }
    file.sync_all()?;
    Ok(true)
}

/// One pass of a test: requests kept `queue` deep for `time`, in MB/s. `live` is told
/// the rate so far a few times a second. None when stopped.
pub fn pass(
    file: &File,
    size: u64,
    test: &Test,
    write: bool,
    time: Duration,
    stop: &AtomicBool,
    mut live: impl FnMut(f64),
) -> io::Result<Option<f64>> {
    let queue = test.queue as usize;
    let mut ring = IoUring::new(test.queue.next_power_of_two())?;
    let mut seed = 0x2545_f491_4f6c_dd1d ^ (test.block as u64) ^ u64::from(test.queue);
    let buf = Buffer::random(test.block * queue, &mut seed);
    let blocks = size / test.block as u64;
    let fd = types::Fd(file.as_raw_fd());
    let mut sequential = 0u64;
    let mut offset = || {
        let block = if test.random {
            next(&mut seed) % blocks
        } else {
            let b = sequential;
            sequential = (sequential + 1) % blocks;
            b
        };
        block * test.block as u64
    };
    let entry = |slot: usize, at: u64| {
        let ptr = buf.at(slot * test.block);
        let len = test.block as u32;
        let op = if write {
            opcode::Write::new(fd, ptr, len).offset(at).build()
        } else {
            opcode::Read::new(fd, ptr, len).offset(at).build()
        };
        op.user_data(slot as u64)
    };

    let start = Instant::now();
    let deadline = start + time;
    for slot in 0..queue {
        let e = entry(slot, offset());
        unsafe { ring.submission().push(&e).expect("queue sized for every slot") };
    }
    let (mut flight, mut bytes, mut last, mut told) = (queue, 0u64, start, start);
    let mut failed = None;
    while flight > 0 {
        ring.submit_and_wait(1)?;
        let now = Instant::now();
        let done: Vec<(u64, i32)> = ring.completion().map(|c| (c.user_data(), c.result())).collect();
        for (slot, res) in done {
            flight -= 1;
            if res < 0 {
                failed.get_or_insert(io::Error::from_raw_os_error(-res));
            } else if res as usize != test.block {
                failed.get_or_insert(io::Error::new(io::ErrorKind::UnexpectedEof, "short transfer"));
            } else if now <= deadline {
                bytes += res as u64;
                last = now;
            }
            if now < deadline && failed.is_none() && !stop.load(Ordering::Relaxed) {
                let e = entry(slot as usize, offset());
                unsafe { ring.submission().push(&e).expect("a slot just came free") };
                flight += 1;
            }
        }
        if now.duration_since(told) >= Duration::from_millis(250) {
            told = now;
            live(rate(bytes, last - start));
        }
    }
    if let Some(e) = failed {
        return Err(e);
    }
    if stop.load(Ordering::Relaxed) {
        return Ok(None);
    }
    Ok(Some(rate(bytes, last - start)))
}

fn rate(bytes: u64, time: Duration) -> f64 {
    if time.is_zero() {
        return 0.0;
    }
    bytes as f64 / 1e6 / time.as_secs_f64()
}

/// The whole run on a file that exists and is filled: every test, reads first.
pub fn tests(file: &File, config: Config, report: &Shared, stop: &AtomicBool) -> io::Result<bool> {
    let set = |f: &mut dyn FnMut(&mut Report)| f(&mut report.lock().unwrap());
    let order = (0..TESTS.len()).map(|t| (t, false)).chain((0..TESTS.len()).map(|t| (t, true)));
    for (n, (t, write)) in order.enumerate() {
        if n > 0 {
            let secs = config.interval.as_secs() as u32;
            for left in (1..=secs).rev() {
                set(&mut |r| r.phase = Phase::Pause(left));
                if wait(Duration::from_secs(1), stop) {
                    return Ok(false);
                }
            }
            set(&mut |r| r.step += 1);
        }
        for p in 0..=config.passes {
            set(&mut |r| r.phase = Phase::Run { test: t, write, pass: p, mbps: 0.0 });
            let live = |mbps| set(&mut |r| r.phase = Phase::Run { test: t, write, pass: p, mbps });
            let Some(mbps) = pass(file, config.size, &TESTS[t], write, config.measure, stop, live)? else {
                return Ok(false);
            };
            set(&mut |r| {
                r.step += 1;
                if p > 0 {
                    let best = if write { &mut r.write[t] } else { &mut r.read[t] };
                    *best = Some(best.map_or(mbps, |b| b.max(mbps)));
                }
            });
        }
    }
    Ok(true)
}

/// Sleep, waking early when stopped; true if it was.
fn wait(time: Duration, stop: &AtomicBool) -> bool {
    let end = Instant::now() + time;
    while Instant::now() < end {
        if stop.load(Ordering::Relaxed) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    stop.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine end to end on a small file in the temp directory: a fast config,
    /// every score filled, the steps counted to the end.
    #[test]
    fn a_short_run_scores_every_test() {
        let path = std::env::temp_dir().join(format!("disk-bench-{}.tmp", std::process::id()));
        let config = Config { size: 16 << 20, passes: 2, measure: Duration::from_millis(100), interval: Duration::ZERO };
        let file = match open(&path) {
            Ok(f) => f,
            Err(e) if e.raw_os_error() == Some(libc::EINVAL) => {
                eprintln!("skipped: no O_DIRECT in {}", path.display());
                return;
            }
            Err(e) => panic!("{e}"),
        };
        let stop = AtomicBool::new(false);
        assert!(fill(&file, config.size, &stop, |_| {}).unwrap());
        let report: Shared = Arc::new(Mutex::new(Report::new(&config)));
        assert!(tests(&file, config, &report, &stop).unwrap());
        std::fs::remove_file(&path).unwrap();
        let r = report.lock().unwrap();
        assert!(r.read.iter().chain(&r.write).all(|s| s.is_some_and(|v| v > 0.0)), "{r:?}");
        assert_eq!(r.step + 1, r.steps, "every step but the fill, which the job counts");
    }

    #[test]
    fn stopping_ends_a_pass_with_nothing() {
        let path = std::env::temp_dir().join(format!("disk-bench-stop-{}.tmp", std::process::id()));
        let Ok(file) = open(&path) else { return };
        let stop = AtomicBool::new(false);
        fill(&file, 4 << 20, &stop, |_| {}).unwrap();
        stop.store(true, Ordering::Relaxed);
        let r = pass(&file, 4 << 20, &TESTS[2], false, Duration::from_secs(10), &stop, |_| {}).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(r, None);
    }

    #[test]
    fn the_crystal_steps() {
        // 8 tests of 1 + 3 runs, 7 pauses between them, and the fill.
        assert_eq!(Config::crystal(1 << 30).steps(), 1 + 32 + 7);
    }
}
