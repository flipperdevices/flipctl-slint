//! The two graphs: the network's airtime over the last two minutes, and one client's
//! traffic each way.

use flipctl_app::paint::Surface;
use flipctl_app::theme::{color, metric, PANEL_H, PANEL_W};
use flipctl_app::{Page, Text};

use crate::model::{Client, Model};

const LEFT: i32 = 2;

fn area() -> (i32, i32, i32, i32) {
    let top = metric::STATUS_BAR_H + 15;
    let bottom = i32::from(PANEL_H) - metric::BUTTON_H - 3;
    let w = i32::from(PANEL_W) - 2 * LEFT;
    (LEFT, top, w, bottom - top)
}

fn label(x: f32, text: String, align: i32) -> Text {
    Text { x, y: (metric::STATUS_BAR_H + 1) as f32, text, font: 1, align, white: false }
}

/// Bars right to left, newest at the right edge, two pixels a second.
fn columns(w: i32) -> usize {
    ((w - 2) / 2) as usize
}

/// The network's share of the air each second in black, over all that was heard in grey.
pub fn airtime(m: &Model) -> Page {
    let (x, y, w, h) = area();
    let mut s = Surface::panel();
    s.frame(x, y, w, h, color::DIVIDER);
    let inner = h - 2;
    s.dotted_vline(x + w / 2, y + 1, inner, color::DIVIDER);
    let n = columns(w);
    let secs: Vec<_> = m.secs.iter().rev().take(n).collect();
    for (i, sec) in secs.iter().enumerate() {
        let bx = x + w - 3 - 2 * i as i32;
        let all = ((sec.air / 1e6).min(1.0) * inner as f32).round() as i32;
        let ours = ((sec.bss_air / 1e6).min(1.0) * inner as f32).round() as i32;
        s.vline(bx, y + 1 + inner - all, all, color::STATUS_DIM);
        s.vline(bx, y + 1 + inner - ours, ours, color::BLACK);
    }
    let ours = m.avg(|s| f64::from(s.bss_air)) / 1e4;
    let heard = m.avg(|s| f64::from(s.air)) / 1e4;
    let texts = vec![
        label(LEFT as f32 + 2.0, format!("Network {ours:.0}%"), -1),
        label(f32::from(PANEL_W) - 4.0, format!("All {heard:.0}%  2 min"), 1),
    ];
    Page::canvas(flipctl_app::picture(&s), texts)
}

/// Down above the line in black, up below it in grey, on one scale.
pub fn traffic(c: &Client) -> Page {
    let (x, y, w, h) = area();
    let mut s = Surface::panel();
    s.frame(x, y, w, h, color::DIVIDER);
    let mid = y + h / 2;
    s.hline(x + 1, mid, w - 2, color::DIVIDER);
    let half = h / 2 - 2;
    let n = columns(w);
    let secs: Vec<_> = c.secs.iter().rev().take(n).collect();
    let peak = secs.iter().map(|s| s.bytes_down.max(s.bytes_up)).max().unwrap_or(0).max(1);
    let scale = |b: u64| ((b as f64 / peak as f64) * f64::from(half)).round() as i32;
    for (i, sec) in secs.iter().enumerate() {
        let bx = x + w - 3 - 2 * i as i32;
        let down = scale(sec.bytes_down);
        let up = scale(sec.bytes_up);
        s.vline(bx, mid - down, down, color::BLACK);
        s.vline(bx, mid + 1, up, color::STATUS_DIM);
    }
    let texts = vec![
        // Short figures with Up centred: "Mbit/s" in full ran into the peak
        label(
            LEFT as f32 + 2.0,
            format!("Down {}", crate::short_rate(c.avg(|s| s.bytes_down as f64))),
            -1,
        ),
        label(
            f32::from(PANEL_W) / 2.0,
            format!("Up {}", crate::short_rate(c.avg(|s| s.bytes_up as f64))),
            0,
        ),
        label(f32::from(PANEL_W) - 4.0, crate::short_rate(peak as f64), 1),
    ];
    Page::canvas(flipctl_app::picture(&s), texts)
}
