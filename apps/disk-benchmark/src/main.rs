//! Disk Benchmark: CrystalDiskMark's default profile on any disk the Flipper has.
//!
//! Two pages. The disk page, where Left and Right choose the disk and the Edit key
//! the size of the test file, out of the sizes its free space has room for. The
//! results page, CrystalDiskMark's table of reads and writes, filling in as the run
//! goes, with the test under way inverted and the run's progress underneath.
//!
//! The run is a thread of its own (see `job`), and the page reads what it has found
//! on a quarter-second clock. Stop, Escape and closing the app all end it the same
//! way, and the file is deleted and anything mounted unmounted on every one.

mod bench;
mod disks;
mod place;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use flipctl_app::paint::Surface;
use flipctl_app::theme::{color, metric, PANEL_H, PANEL_W};
use flipctl_app::{choice, font, pair, text, App, Key, Page, Row, Ui};

use bench::{Config, Phase, Report, Shared, TESTS};
use disks::{size, Disk, Sys};

/// Test file sizes the Edit key steps through, those that fit; CrystalDiskMark's
/// default is 1 GiB.
const SIZES: [u64; 5] = [256 << 20, 512 << 20, 1 << 30, 2 << 30, 4 << 30];
/// How often the disk list is read again, for a disk plugged in or taken out.
const RESCAN: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Debug)]
enum View {
    Disks,
    Results,
}

struct Job {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Job {
    fn end(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Bench {
    disks: Vec<Disk>,
    chosen: usize,
    size: usize,
    view: View,
    home: PathBuf,
    /// The disk the results are for, by name and as the table heads it, and what
    /// the run has found.
    tested: (String, String),
    report: Option<Shared>,
    job: Option<Job>,
    scanned: Instant,
    /// Free and total bytes of partitions nobody mounted, by `probe_key`, from a
    /// moment's read-only mount; None while that is under way.
    probes: Arc<Mutex<HashMap<String, Option<Result<(u64, u64), String>>>>>,
}

/// A partition as the probe cache knows it: a card swapped for another in the same
/// slot is a different one.
fn probe_key(p: &disks::Part) -> String {
    format!("{} {} {} {}", p.name, p.size, p.fs, p.label)
}

impl Bench {
    fn new(disks: Vec<Disk>) -> Self {
        Self {
            disks,
            chosen: 0,
            size: 2,
            view: View::Disks,
            home: std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/home/user".into()),
            tested: (String::new(), String::new()),
            report: None,
            job: None,
            scanned: Instant::now(),
            probes: Arc::default(),
        }
    }

    fn disk(&self) -> Option<&Disk> {
        self.disks.get(self.chosen)
    }

    /// Free and total bytes where the test file would go: asked of the mount, or of
    /// the probe for a partition nobody mounted. None while the probe runs.
    fn space(&self) -> Option<Result<(u64, u64), String>> {
        let part = self.disk()?.target(&self.home)?;
        match part.mounts.first() {
            Some(m) => Some(place::space(m).map_err(|e| e.to_string())),
            None => self.probes.lock().unwrap().get(&probe_key(part)).cloned().flatten(),
        }
    }

    /// Probe the chosen disk's partition if nobody has it mounted and it has not been
    /// asked yet.
    fn probe(&self) {
        let Some(part) = self.disk().and_then(|d| d.target(&self.home)) else { return };
        if !part.mounts.is_empty() || self.running() {
            return;
        }
        let key = probe_key(part);
        let mut probes = self.probes.lock().unwrap();
        if probes.contains_key(&key) {
            return;
        }
        probes.insert(key.clone(), None);
        let (part, probes) = (part.clone(), self.probes.clone());
        std::thread::spawn(move || {
            let got = place::probe(&part);
            probes.lock().unwrap().insert(key, Some(got));
            flipctl_app::redraw();
        });
    }

    /// The sizes the free space has room for, none until it is known.
    fn fits(&self) -> Vec<usize> {
        let Some(Ok((room, _))) = self.space() else { return vec![] };
        (0..SIZES.len()).filter(|&i| SIZES[i] + place::MARGIN <= room).collect()
    }

    /// The size chosen, or the largest that fits when it does not.
    fn bytes(&self) -> Option<u64> {
        let fits = self.fits();
        let i = if fits.contains(&self.size) { self.size } else { *fits.last()? };
        Some(SIZES[i])
    }

    fn next_size(&mut self) {
        let fits = self.fits();
        let now = fits.iter().position(|&i| Some(SIZES[i]) == self.bytes());
        if let Some(now) = now {
            self.size = fits[(now + 1) % fits.len()];
        }
    }

    fn running(&self) -> bool {
        self.report.as_ref().is_some_and(|r| r.lock().unwrap().phase.running())
    }

    fn start(&mut self) {
        let Some(disk) = self.disk() else { return };
        let Some(part) = disk.target(&self.home).cloned() else { return };
        let Some(bytes) = self.bytes() else { return };
        let config = Config::crystal(bytes);
        let report: Shared = Arc::new(Mutex::new(Report::new(&config)));
        let stop = Arc::new(AtomicBool::new(false));
        let (r, s, home) = (report.clone(), stop.clone(), self.home.clone());
        let thread = std::thread::spawn(move || job(&part, &home, config, &r, &s));
        self.tested = (disk.name.clone(), disk.title());
        self.report = Some(report);
        self.job = Some(Job { stop, thread: Some(thread) });
        self.view = View::Results;
    }

    fn stop(&self) {
        if let Some(job) = &self.job {
            job.stop.store(true, Ordering::Relaxed);
        }
    }

    fn rescan(&mut self) {
        let name = self.disk().map(|d| d.name.clone());
        self.disks = disks::scan(&Sys::default());
        self.chosen = name.and_then(|n| self.disks.iter().position(|d| d.name == n)).unwrap_or(0);
        self.scanned = Instant::now();
        // A partition that went away is asked again when it comes back.
        let here: Vec<String> = self.disks.iter().flat_map(|d| d.parts.iter().map(probe_key)).collect();
        self.probes.lock().unwrap().retain(|k, v| v.is_none() || here.contains(k));
    }

    fn disk_rows(&self) -> Vec<Row> {
        let Some(disk) = self.disk() else {
            return vec![text("No disks found")];
        };
        let last = self.disks.len().saturating_sub(1);
        let mut rows = vec![choice("Disk", disk.title(), self.chosen == 0, self.chosen == last)];
        if !disk.model.is_empty() {
            rows.push(pair("Model", disk.model.clone()));
        }
        rows.push(pair("Link", disk.link.clone()));
        match disk.target(&self.home) {
            None => rows.push(text("No filesystem to test on").dim()),
            Some(p) => {
                let fs = if p.label.is_empty() { p.fs.clone() } else { format!("{} {}", p.fs, p.label) };
                rows.push(pair(p.name.clone(), fs));
                let space = self.space();
                rows.push(match &space {
                    Some(Ok((free, total))) => pair("Free", format!("{} of {}", size(*free), size(*total))),
                    Some(Err(_)) => pair("Free", "cannot mount"),
                    None => pair("Free", "checking"),
                });
                let file = match (self.bytes(), space) {
                    (Some(b), _) => gib(b),
                    (None, Some(Ok(_))) => "no room".into(),
                    _ => "-".into(),
                };
                rows.push(pair("Test file", file));
            }
        }
        rows
    }

    fn can_run(&self) -> bool {
        self.bytes().is_some()
    }
}

/// A size as the disk page says it: "256 MiB", "1 GiB".
fn gib(bytes: u64) -> String {
    if bytes >= 1 << 30 {
        format!("{} GiB", bytes >> 30)
    } else {
        format!("{} MiB", bytes >> 20)
    }
}

/// The run on its thread: the place, the file, the tests, and the place cleaned up
/// whatever happened.
fn job(part: &disks::Part, home: &std::path::Path, config: Config, report: &Shared, stop: &AtomicBool) {
    let set = |phase: Phase| report.lock().unwrap().phase = phase;
    let started = Instant::now();
    let mut place = match place::Place::make(part, home, config.size) {
        Ok(p) => p,
        Err(e) => return set(Phase::Failed(e)),
    };
    let outcome = (|| -> std::io::Result<bool> {
        let file = bench::open(&place.file)?;
        let filled = bench::fill(&file, config.size, stop, |f| set(Phase::Filling(f)))?;
        if !filled {
            return Ok(false);
        }
        report.lock().unwrap().step += 1;
        bench::tests(&file, config, report, stop)
    })();
    set(Phase::Cleaning);
    place.clean();
    set(match outcome {
        Ok(true) => Phase::Done(started.elapsed()),
        Ok(false) => Phase::Stopped,
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => Phase::Failed(format!("No direct I/O on {}", part.fs)),
        Err(e) => Phase::Failed(e.to_string()),
    });
}

impl App for Bench {
    fn page(&self) -> Page {
        match self.view {
            View::Disks => Page::detail(self.disk_rows()),
            View::Results => {
                let report = self.report.as_ref().map(|r| r.lock().unwrap().clone());
                let report = report.unwrap_or_else(|| Report::new(&Config::crystal(0)));
                Page::canvas(flipctl_app::picture(&results(&report, &self.tested.1)), vec![])
            }
        }
    }

    fn buttons(&self) -> [&str; 5] {
        match self.view {
            View::Disks => {
                let can = self.can_run();
                let results = if self.report.is_some() { "Results" } else { "" };
                let sizes = if self.fits().len() > 1 { "Size" } else { "" };
                ["Close", results, "", sizes, if can { "Run" } else { "" }]
            }
            View::Results if self.running() => ["Stop", "", "", "", ""],
            View::Results => ["Close", "Disks", "", "", "Again"],
        }
    }

    fn key(&mut self, key: Key, _: &mut Ui) -> bool {
        match (self.view, key) {
            (View::Results, _) if self.running() => {
                if key == Key::Escape {
                    self.stop();
                }
                return true;
            }
            (View::Disks, Key::Left) => self.chosen = self.chosen.saturating_sub(1),
            (View::Disks, Key::Right) => self.chosen = (self.chosen + 1).min(self.disks.len().saturating_sub(1)),
            (View::Disks, Key::Edit) => self.next_size(),
            (View::Disks, Key::View) if self.report.is_some() => self.view = View::Results,
            (View::Disks, Key::Run) if self.can_run() => self.start(),
            (View::Results, Key::View) => {
                self.view = View::Disks;
                self.rescan();
            }
            (View::Results, Key::Run) => {
                self.rescan();
                self.chosen = self.disks.iter().position(|d| d.name == self.tested.0).unwrap_or(self.chosen);
                if self.can_run() {
                    self.start();
                } else {
                    self.view = View::Disks;
                }
            }
            _ => return false,
        }
        true
    }

    fn every(&self) -> Option<Duration> {
        Some(Duration::from_millis(250))
    }

    fn tick(&mut self, _: &mut Ui) {
        if self.view == View::Disks && self.scanned.elapsed() >= RESCAN {
            self.rescan();
        }
        if self.view == View::Disks {
            self.probe();
        }
        if !self.running() {
            if let Some(mut job) = self.job.take() {
                job.end();
            }
        }
    }

    fn close(&mut self) {
        if let Some(mut job) = self.job.take() {
            job.end();
        }
    }
}

const LABEL_X: i32 = 3;
const READ_X: i32 = 168;
const WRITE_X: i32 = 252;
const TOP: i32 = metric::STATUS_BAR_H + 2;
const PITCH: i32 = 16;

/// The results table, CrystalDiskMark's: a row per test, reads and writes in MB/s,
/// the test under way inverted, and how far the run is underneath.
fn results(r: &Report, disk: &str) -> Surface {
    let mut s = Surface::panel();
    let f = &font::TITLE;
    let right = |s: &mut Surface, t: &str, x: i32, y: i32, c| s.text(f, t, x - font::tw(t), y, c);

    s.text(f, disk, LABEL_X, TOP, color::STATUS_DIM);
    right(&mut s, "Read", READ_X, TOP, color::BLACK);
    right(&mut s, "Write", WRITE_X, TOP, color::BLACK);
    s.hline(0, TOP + PITCH - 2, i32::from(PANEL_W), color::BLACK);

    let active = match r.phase {
        Phase::Run { test, write, .. } => Some((test, write)),
        _ => None,
    };
    for (i, test) in TESTS.iter().enumerate() {
        let y = TOP + PITCH * (i as i32 + 1);
        s.text(f, test.name, LABEL_X, y, color::BLACK);
        for (write, x, score) in [(false, READ_X, r.read[i]), (true, WRITE_X, r.write[i])] {
            let value = score.map_or_else(|| "-".to_string(), mbps);
            if active == Some((i, write)) {
                s.rect(x - 78, y - 1, 80, PITCH - 1, color::BLACK);
                let live = match &r.phase {
                    Phase::Run { mbps: m, .. } if *m > 0.0 => mbps(*m),
                    _ => value,
                };
                right(&mut s, &live, x, y, color::WHITE);
            } else {
                right(&mut s, &value, x, y, if score.is_some() { color::BLACK } else { color::STATUS_DIM });
            }
        }
        s.hline(0, y + PITCH - 2, i32::from(PANEL_W), color::DIVIDER);
    }

    let y = TOP + PITCH * 5 + 2;
    let (left, aside) = status(r);
    s.text(f, &font::fit(&left, i32::from(PANEL_W) - 8 - font::tw(&aside)), LABEL_X, y, color::BLACK);
    right(&mut s, &aside, WRITE_X, y, color::BLACK);

    let bar_y = i32::from(PANEL_H) - metric::BUTTON_H - 9;
    let bar_w = i32::from(PANEL_W) - 2 * LABEL_X;
    s.frame(LABEL_X, bar_y, bar_w, 6, color::BLACK);
    let fill = match r.phase {
        Phase::Done(_) => bar_w,
        Phase::Filling(f) => ((r.step as f64 + f) / r.steps as f64 * bar_w as f64) as i32,
        _ => (r.step as f64 / r.steps.max(1) as f64 * bar_w as f64) as i32,
    };
    s.rect(LABEL_X, bar_y, fill, 6, color::BLACK);
    s
}

/// The line under the table: what is happening, and a number for it on the right.
fn status(r: &Report) -> (String, String) {
    match &r.phase {
        Phase::Setup => ("Checking free space".into(), String::new()),
        Phase::Filling(f) => ("Writing test file".into(), format!("{:.0}%", f * 100.0)),
        Phase::Pause(left) => ("Pause".into(), format!("{left} s")),
        Phase::Run { test, write, pass, .. } => {
            let what = if *write { "Write" } else { "Read" };
            let n = if *pass == 0 { "warm-up".into() } else { format!("{pass}/{}", r.passes) };
            (format!("{what} {}", TESTS[*test].name), n)
        }
        Phase::Cleaning => ("Deleting test file".into(), String::new()),
        Phase::Done(t) => ("Done".into(), format!("{}:{:02}", t.as_secs() / 60, t.as_secs() % 60)),
        Phase::Stopped => ("Stopped".into(), String::new()),
        Phase::Failed(e) => (e.clone(), String::new()),
    }
}

/// MB/s as CrystalDiskMark shows it, two decimals.
fn mbps(v: f64) -> String {
    format!("{v:.2}")
}

flipctl_app::main!(Bench::new(disks::scan(&Sys::default())));

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::{theme, Ethernet, Status};

    fn shot(name: &str, app: Bench) -> Vec<u8> {
        let status = Status { battery: 76, wifi_connected: true, wifi_quality: 80, ethernet: Ethernet::Real, ..Status::default() };
        let frame = flipctl_pages::shot(app, status);
        if std::env::var_os("DISK_BENCH_RENDER").is_some() {
            let path = std::path::Path::new("target/render").join(format!("{name}.png"));
            flipctl_app::render::save(&path, &frame);
        }
        frame
    }

    fn sd() -> Disk {
        Disk {
            name: "mmcblk0".into(),
            kind: "SD card",
            model: "SN32G".into(),
            link: "SD card".into(),
            size: 31_914_983_424,
            parts: vec![disks::Part {
                name: "mmcblk0p1".into(),
                size: 31_880_904_704,
                fs: "exfat".into(),
                label: "SDCARD".into(),
                mounts: vec![],
            }],
        }
    }

    fn ufs() -> Disk {
        Disk {
            name: "sda".into(),
            kind: "UFS",
            model: "SKhynix BWU2A0516B064G".into(),
            link: "UFS G3 x2".into(),
            size: 63_447_793_664,
            parts: vec![disks::Part {
                name: "sda3".into(),
                size: 63_384_322_048,
                fs: "btrfs".into(),
                label: "flipperos".into(),
                mounts: vec!["/".into()],
            }],
        }
    }

    fn with(view: View, phase: Phase, scores: usize) -> Bench {
        let mut app = Bench::new(vec![ufs(), sd()]);
        app.view = view;
        app.tested = ("sda".into(), "UFS 59.1G".into());
        let mut report = Report::new(&Config::crystal(1 << 30));
        let fake = [1953.21, 1102.48, 312.77, 41.06];
        for i in 0..scores.min(4) {
            report.read[i] = Some(fake[i]);
        }
        for i in 4..scores.min(8) {
            report.write[i - 4] = Some(fake[i - 4] * 0.6);
        }
        report.step = (scores as u32) * 5;
        report.phase = phase;
        app.report = Some(Arc::new(Mutex::new(report)));
        app
    }

    fn tokens_only(frame: &[u8]) {
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        let stride = usize::from(theme::PANEL_W);
        // The status bar's icons are sprites with greys of their own.
        let offenders: Vec<String> = frame
            .iter()
            .enumerate()
            .skip(stride * metric::STATUS_BAR_H as usize)
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
            .collect();
        assert!(offenders.is_empty(), "off-palette pixels: {offenders:?}");
    }

    #[test]
    fn every_page_is_drawn_in_tokens() {
        let run = Phase::Run { test: 2, write: true, pass: 2, mbps: 187.4 };
        for (name, app) in [
            ("disks", Bench::new(vec![ufs(), sd()])),
            ("disks-sd", {
                let mut a = probed(Bench::new(vec![ufs(), sd()]), Ok((12 << 30, 32 << 30)));
                a.chosen = 1;
                a
            }),
            ("running", with(View::Results, run, 6)),
            ("done", with(View::Results, Phase::Done(Duration::from_secs(211)), 8)),
            ("failed", with(View::Results, Phase::Failed("Needs 1.1G free, has 512M".into()), 0)),
        ] {
            tokens_only(&shot(name, app));
        }
    }

    #[test]
    fn keys_choose_disk_and_size() {
        let mut app = probed(Bench::new(vec![ufs(), sd()]), Ok((30 << 30, 32 << 30)));
        let mut ui = Ui::default();
        app.key(Key::Right, &mut ui);
        assert_eq!(app.chosen, 1);
        app.key(Key::Right, &mut ui);
        assert_eq!(app.chosen, 1, "stops at the last disk");
        assert_eq!(app.bytes(), Some(1 << 30), "CrystalDiskMark's default");
        for want in [2 << 30, 4 << 30, 256 << 20, 512 << 20, 1 << 30] {
            app.key(Key::Edit, &mut ui);
            assert_eq!(app.bytes(), Some(want), "up to the largest, then round to the smallest");
        }
    }

    /// What a probe of the SD card found, as though it had run.
    fn probed(app: Bench, got: Result<(u64, u64), String>) -> Bench {
        app.probes.lock().unwrap().insert(probe_key(&sd().parts[0]), Some(got));
        app
    }

    /// Only sizes the free space has room for, the margin left over.
    #[test]
    fn only_sizes_that_fit_are_offered() {
        let small = |bytes: u64| probed(Bench::new(vec![sd()]), Ok((bytes, 32 << 30)));
        let app = small(600 << 20);
        assert_eq!(app.fits(), [0, 1]);
        assert_eq!(app.bytes(), Some(512 << 20), "the largest that fits stands in for 1 GiB");
        let app = small(500 << 20);
        assert_eq!(app.fits(), [0]);
        assert_eq!(app.buttons()[3], "", "no Size with one size");
        let app = small(300 << 20);
        assert_eq!((app.bytes(), app.can_run()), (None, false));
        assert_eq!(app.buttons()[4], "", "no Run without room");
        let rows: Vec<String> = app.disk_rows().into_iter().map(|r| r.value).collect();
        assert!(rows.contains(&"no room".into()), "{rows:?}");
    }

    #[test]
    fn nothing_to_run_until_the_probe_answers() {
        let app = Bench::new(vec![sd()]);
        app.probes.lock().unwrap().insert(probe_key(&sd().parts[0]), None);
        let values = |app: &Bench| app.disk_rows().into_iter().map(|r| r.value).collect::<Vec<_>>();
        assert!(values(&app).contains(&"checking".into()));
        assert_eq!(app.buttons()[4], "");
        let app = probed(Bench::new(vec![sd()]), Err("mount: wrong fs type".into()));
        assert!(values(&app).contains(&"cannot mount".into()));
        assert!(!app.can_run());
        let app = probed(Bench::new(vec![sd()]), Ok((12 << 30, 32 << 30)));
        assert!(values(&app).contains(&"12.0G of 32.0G".into()));
        assert!(app.can_run());
    }
}
