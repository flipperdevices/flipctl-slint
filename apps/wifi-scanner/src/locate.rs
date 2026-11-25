//! Locate: one station's signal, read fast enough to walk towards it. A large dBm
//! figure over the last second, the best seen, a level bar, and the strongest reading
//! of each fifth of a second across the graph.

use flipctl_app::font::{ascii, TITLE};
use flipctl_app::paint::Surface;
use flipctl_app::pixel::Gray8;
use flipctl_app::theme::{color, metric, PANEL_H, PANEL_W};
use flipctl_app::Page;

use crate::model::Model;
use crate::ui::{fit, text, width};

/// The span the large figure and the beep take the median of: short enough to follow
/// a walk. It reaches further back for at least `MIN_SAMPLES`, up to `REACH_US`,
/// since a level read from two or three frames jumps by itself.
const NOW_US: u64 = 500_000;
const MIN_SAMPLES: usize = 5;
const REACH_US: u64 = 1_000_000;
/// The beep moves in steps of this many dB, and only once the signal is this far past
/// the middle of its step, so it holds still while the Flipper does.
const STEP_DB: f32 = 4.0;
const HOLD_DB: f32 = 4.5;
/// Nothing for this long and the figure goes dim and the beep stops. A client asleep in
/// power save says nothing for a second or more at a time, and stopping at once made
/// the beep stutter; the "ago" line still says when it was last heard.
const HEARD_US: u64 = 3_000_000;
/// One graph column.
const BIN_US: u64 = 200_000;
/// The level bar and the graph run from this weak to this strong.
const FLOOR: f32 = -100.0;
const CEILING: f32 = -20.0;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reading {
    /// The mean of the last second, dBm.
    pub now: Option<f32>,
    /// The newest sample: how long ago, microseconds, and dBm.
    pub last: Option<(u64, i8)>,
    pub best: Option<i8>,
    /// The strongest sample of each column, newest first.
    pub bins: Vec<Option<i8>>,
}

pub fn reading(m: &Model, columns: usize) -> Reading {
    let now = m.now;
    let mut recent: Vec<i8> = m
        .samples
        .iter()
        .rev()
        .enumerate()
        .take_while(|(i, (at, _))| now - at <= NOW_US || (*i < MIN_SAMPLES && now - at <= REACH_US))
        .map(|(_, (_, d))| *d)
        .collect();
    // Quiet for longer, the newest frame holds the figure until `HEARD_US`
    if let Some((_, d)) =
        m.samples.back().filter(|(at, _)| recent.is_empty() && now - at <= HEARD_US)
    {
        recent.push(*d);
    }
    recent.sort_unstable();
    let mut bins = vec![None; columns];
    for (at, dbm) in &m.samples {
        let i = ((now - at) / BIN_US) as usize;
        if let Some(b) = bins.get_mut(i) {
            *b = Some(b.map_or(*dbm, |old: i8| old.max(*dbm)));
        }
    }
    Reading {
        now: recent.get(recent.len() / 2).map(|d| f32::from(*d)),
        last: m.samples.back().map(|(at, d)| (now - at, *d)),
        best: m.samples.iter().map(|(_, d)| *d).max(),
        bins,
    }
}

impl Reading {
    /// The figure to show and beep by, while anything has been heard lately.
    pub fn current(&self) -> Option<f32> {
        self.now
    }
}

/// The beep's level, held to steps.
#[derive(Default)]
pub struct Steady {
    step: Option<f32>,
}

impl Steady {
    /// The stepped level for `dbm`, which moves only when `dbm` leaves its step.
    pub fn update(&mut self, dbm: Option<f32>) -> Option<f32> {
        let Some(d) = dbm else {
            self.step = None;
            return None;
        };
        let held = self.step.filter(|s| (d - s).abs() <= HOLD_DB);
        let step = held.unwrap_or_else(|| (d / STEP_DB).round() * STEP_DB);
        self.step = Some(step);
        Some(step)
    }
}

/// Where `dbm` sits between the floor and the ceiling, 0 to 1.
pub fn level(dbm: f32) -> f32 {
    ((dbm - FLOOR) / (CEILING - FLOOR)).clamp(0.0, 1.0)
}

/// `t` in the title face at three times its size.
fn large(s: &mut Surface, t: &str, x: i32, y: i32, c: Gray8) {
    let mut dots = Vec::new();
    TITLE.for_each_pixel(&ascii(t), 0, 0, |px, py| dots.push((px, py)));
    for (px, py) in dots {
        s.rect(x + 3 * px, y + 3 * py, 3, 3, c);
    }
}

pub fn page(m: &Model, name: &str) -> Page {
    let (left, right) = (2, i32::from(PANEL_W) - 3);
    let top = metric::STATUS_BAR_H;
    let bottom = i32::from(PANEL_H) - metric::BUTTON_H - 2;
    let graph_top = top + 64;
    let columns = (right - left - 1) as usize;
    let r = reading(m, columns);
    let mut s = Surface::panel();

    let best = r.best.map_or(String::new(), |b| format!("Best {b}"));
    let bw = width(&TITLE, &best);
    text(&mut s, &TITLE, &best, right - bw, top + 3, color::BLACK);
    text(&mut s, &TITLE, &fit(&TITLE, name, right - left - bw - 8), left, top + 3, color::BLACK);

    // The figure goes dim once nothing has been heard for a second
    let (figure, tone) = match (r.current(), r.last) {
        (Some(now), _) => (format!("{now:.0}"), color::BLACK),
        (None, Some((_, d))) => (d.to_string(), color::STATUS_DIM),
        (None, None) => ("-".into(), color::STATUS_DIM),
    };
    large(&mut s, &figure, left + 2, top + 12, tone);
    let fw = 3 * width(&TITLE, &figure);
    text(&mut s, &TITLE, "dBm", left + 2 + fw + 6, top + 33, tone);
    let heard = match r.last {
        Some((age, _)) if age < 1_000_000 => "Now".to_string(),
        Some((age, _)) => format!("{}s ago", age / 1_000_000),
        None => "Not heard yet".into(),
    };
    let hw = width(&TITLE, &heard);
    text(&mut s, &TITLE, &heard, right - hw, top + 33, color::BLACK);

    // The level bar
    let bar_y = top + 50;
    s.frame(left, bar_y, right - left + 1, 9, color::BLACK);
    if let Some(now) = r.current().or(r.last.map(|(_, d)| f32::from(d))) {
        let w = ((right - left - 3) as f32 * level(now)).round() as i32;
        s.rect(left + 2, bar_y + 2, w, 5, tone);
    }

    // Strongest of each fifth of a second, newest at the right
    let h = bottom - graph_top;
    s.frame(left, graph_top, right - left + 1, h, color::DIVIDER);
    let inner = h - 2;
    for (i, bin) in r.bins.iter().enumerate() {
        let Some(dbm) = bin else { continue };
        let bh = ((inner as f32) * level(f32::from(*dbm))).round().max(1.0) as i32;
        s.vline(right - 1 - i as i32, graph_top + 1 + inner - bh, bh, color::BLACK);
    }
    Page::canvas(flipctl_app::picture(&s), vec![])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::tests::{data, AP, STA};

    #[test]
    fn the_last_second_the_best_and_one_column_a_fifth() {
        let mut m = Model::new(AP);
        m.locate(Some(STA));
        // Uplink frames at -60 for two seconds, then one at -40 now
        for at in (0..2_000_000).step_by(100_000) {
            let f = data(true, 0, 10, 0);
            m.frame(at, f.len(), &f);
        }
        let r = reading(&m, 30);
        assert_eq!(r.best, r.last.map(|(_, d)| d));
        assert!(r.now.is_some());
        assert_eq!(r.bins.iter().filter(|b| b.is_some()).count(), 10);
        // A frame from someone else is not the target's
        let before = m.samples.len();
        let f = data(false, 0, 10, 0);
        m.frame(2_100_000, f.len(), &f);
        assert_eq!(m.samples.len(), before);
    }

    #[test]
    fn the_beep_holds_its_step_through_jitter_and_moves_on_a_change() {
        let mut s = Steady::default();
        let first = s.update(Some(-61.0));
        for d in [-59.0, -63.0, -64.0, -58.5, -62.0] {
            assert_eq!(s.update(Some(d)), first, "{d}");
        }
        assert_ne!(s.update(Some(-52.0)), first);
        assert_eq!(s.update(None), None);
    }

    #[test]
    fn the_median_of_at_least_five_frames_within_a_second() {
        let mut m = Model::new(AP);
        m.now = 2_000_000;
        // Three in the last half second, two more before: the median of all five
        for (at, d) in [
            (1_100_000, -70),
            (1_200_000, -60),
            (1_700_000, -50),
            (1_800_000, -90),
            (1_900_000, -55),
        ] {
            m.samples.push_back((at, d));
        }
        assert_eq!(reading(&m, 0).now, Some(-60.0));
        // Older than a second counts no more, and two seconds of silence hold the newest
        m.samples.push_front((600_000, -30));
        assert_eq!(reading(&m, 0).now, Some(-60.0));
        m.now = 3_800_000;
        assert_eq!(reading(&m, 0).now, Some(-55.0));
        m.now = 5_000_000;
        assert_eq!(reading(&m, 0).now, None);
    }
}
