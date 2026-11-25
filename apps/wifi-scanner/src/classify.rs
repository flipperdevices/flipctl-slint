//! What a client seems to be doing, from the shape of its traffic alone: how much each
//! way, how big its frames are, how regular, and in which traffic class. The payload is
//! encrypted and never looked at.
//!
//! The thresholds are ours and rough. A label changes only after the new one has held
//! for a few seconds, so one burst does not flip it.

use crate::model::{Client, ClientSecond};

/// Seconds the rules look at.
const SPAN: usize = 5;
/// Seconds a new label must hold before it is shown.
const HOLD: u8 = 3;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Label {
    #[default]
    Unknown,
    Idle,
    Call,
    Streaming,
    Download,
    Upload,
    Browsing,
}

impl Label {
    pub fn name(self) -> &'static str {
        match self {
            Label::Unknown => "-",
            Label::Idle => "Idle",
            Label::Call => "Call",
            Label::Streaming => "Streaming",
            Label::Download => "Download",
            Label::Upload => "Upload",
            Label::Browsing => "Browsing",
        }
    }
}

/// The label shown, and the one waiting to replace it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Activity {
    pub shown: Label,
    next: Label,
    held: u8,
}

impl Activity {
    pub fn update(&mut self, label: Label) {
        if label == self.shown {
            self.held = 0;
            return;
        }
        if label == self.next {
            self.held += 1;
        } else {
            (self.next, self.held) = (label, 1);
        }
        if self.held >= HOLD || self.shown == Label::Unknown {
            (self.shown, self.held) = (label, 0);
        }
    }
}

/// The label the last seconds suggest.
pub fn activity(c: &Client) -> Label {
    let secs: Vec<&ClientSecond> = c.recent(SPAN).collect();
    label(&secs)
}

pub fn label(secs: &[&ClientSecond]) -> Label {
    if secs.len() < 2 {
        return Label::Unknown;
    }
    let n = secs.len() as f64;
    let sum = |f: fn(&ClientSecond) -> f64| secs.iter().map(|s| f(s)).sum::<f64>();
    let down = sum(|s| s.bytes_down as f64) / n;
    let up = sum(|s| s.bytes_up as f64) / n;
    let fdown = sum(|s| f64::from(s.data_down)) / n;
    let fup = sum(|s| f64::from(s.data_up)) / n;
    let realtime = sum(|s| s.realtime as f64) / n;
    let total = down + up;
    if total < 2_000.0 && fdown + fup < 10.0 {
        return Label::Idle;
    }
    // Seconds with real traffic, and how steady the downlink is from second to second
    let busy = secs.iter().filter(|s| s.bytes_down + s.bytes_up > 20_000).count();
    let mean_down = down.max(1.0);
    let spread = (secs.iter().map(|s| (s.bytes_down as f64 - down).powi(2)).sum::<f64>() / n)
        .sqrt()
        / mean_down;
    let small = |bytes: f64, frames: f64| frames > 0.0 && bytes / frames < 400.0;
    // Gaps between data frames: a call sends every 10..30 ms, steadily
    let (gaps, gap_sum, gap_sq) = secs
        .iter()
        .fold((0u32, 0.0, 0.0), |(g, s, q), x| (g + x.gaps, s + x.gap_sum, q + x.gap_sq));
    let regular = gaps > 20 && {
        let mean = gap_sum / f64::from(gaps);
        let var = (gap_sq / f64::from(gaps) - mean * mean).max(0.0);
        mean < 40_000.0 && var.sqrt() / mean < 1.0
    };
    if fup >= 20.0
        && fdown >= 20.0
        && small(up, fup)
        && small(down, fdown)
        && total < 400_000.0
        && (regular || realtime > total * 0.5)
    {
        return Label::Call;
    }
    if up > 150_000.0 && up > 3.0 * down {
        return Label::Upload;
    }
    if down > 1_500_000.0 && busy == secs.len() && spread < 0.5 {
        return Label::Download;
    }
    if down > 150_000.0 && up < down * 0.2 && busy * 2 >= secs.len() {
        return Label::Streaming;
    }
    Label::Browsing
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(down: u64, up: u64, fdown: u32, fup: u32) -> ClientSecond {
        ClientSecond {
            bytes_down: down,
            bytes_up: up,
            data_down: fdown,
            data_up: fup,
            ..Default::default()
        }
    }

    fn of(s: &[ClientSecond]) -> Label {
        label(&s.iter().collect::<Vec<_>>())
    }

    #[test]
    fn keepalives_are_idle() {
        assert_eq!(of(&[sec(300, 200, 2, 2); 5]), Label::Idle);
    }

    #[test]
    fn small_regular_frames_both_ways_are_a_call() {
        let mut s = sec(8_000, 8_000, 50, 50);
        s.gaps = 100;
        s.gap_sum = 100.0 * 10_000.0;
        s.gap_sq = 100.0 * 10_000.0 * 10_000.0;
        assert_eq!(of(&[s; 5]), Label::Call);
    }

    #[test]
    fn steady_heavy_downlink_is_a_download_and_lighter_is_streaming() {
        assert_eq!(of(&[sec(3_000_000, 60_000, 2000, 1000); 5]), Label::Download);
        let video = [
            sec(900_000, 20_000, 700, 300),
            sec(50_000, 2_000, 40, 20),
            sec(900_000, 20_000, 700, 300),
        ];
        assert_eq!(of(&video), Label::Streaming);
    }

    #[test]
    fn heavy_uplink_is_an_upload() {
        assert_eq!(of(&[sec(40_000, 2_000_000, 400, 1500); 5]), Label::Upload);
    }

    #[test]
    fn a_label_must_hold_before_it_shows() {
        let mut a = Activity::default();
        a.update(Label::Idle);
        assert_eq!(a.shown, Label::Idle);
        a.update(Label::Browsing);
        a.update(Label::Browsing);
        assert_eq!(a.shown, Label::Idle);
        a.update(Label::Browsing);
        assert_eq!(a.shown, Label::Browsing);
    }
}
