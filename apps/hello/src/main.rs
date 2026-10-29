use flipctl_app::paint::Surface;
use flipctl_app::theme::{color, metric, PANEL_H, PANEL_W};
use flipctl_app::{gauge, pair, rule, text, App, Key, Page, Text, Ui};

const SLOTS: u32 = 10;

struct Hello {
    presses: u32,
    canvas: bool,
}

impl App for Hello {
    fn page(&self) -> Page {
        if self.canvas {
            return picture(self.presses);
        }
        Page::detail(vec![
            text("Hello, Flipper One!"),
            rule(),
            pair("Presses", self.presses.to_string()),
            gauge("Progress", "", (self.presses * 100 / SLOTS) as i32),
        ])
        .fit_gauges()
    }

    fn buttons(&self) -> [&str; 5] {
        ["Close", if self.canvas { "Rows" } else { "Draw" }, "", "", "Add"]
    }

    fn key(&mut self, key: Key, _: &mut Ui) -> bool {
        match key {
            Key::Ok | Key::Run => self.presses += 1,
            Key::View => self.canvas = !self.canvas,
            _ => return false,
        }
        true
    }
}

/// The canvas page: a frame between the status bar and the soft keys, and a slot
/// per press inside it, filled as the presses come in.
fn picture(presses: u32) -> Page {
    let top = metric::STATUS_BAR_H + 2;
    let bottom = i32::from(PANEL_H) - metric::BUTTON_H - 2;
    let (x, w, h) = (2, i32::from(PANEL_W) - 4, bottom - top);

    let mut surface = Surface::panel();
    surface.round_frame(x, top, w, h, 2, color::BLACK);

    let (size, gap) = (18, 6);
    let left = (i32::from(PANEL_W) - (SLOTS as i32 * (size + gap) - gap)) / 2;
    let y = top + h - size - 12;
    for i in 0..SLOTS {
        let sx = left + i as i32 * (size + gap);
        if i < presses {
            surface.rect(sx, y, size, size, color::BLACK);
        } else {
            surface.frame(sx, y, size, size, color::STATUS_DIM);
        }
    }

    let texts = vec![Text {
        x: f32::from(PANEL_W / 2),
        y: (top + 10) as f32,
        text: format!("Presses: {presses}"),
        ..Text::default()
    }];
    Page::canvas(flipctl_app::picture(&surface), texts)
}

flipctl_app::main!(Hello { presses: 0, canvas: false });

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::{theme, Ethernet, Status};

    /// Both pages, with what a running device would put on the bar. The frames go to
    /// target/render when `HELLO_RENDER=1` asks for them, to look at.
    fn shot(name: &str, canvas: bool, status: Status) -> Vec<u8> {
        let frame = flipctl_pages::shot(Hello { presses: 4, canvas }, status);
        if std::env::var_os("HELLO_RENDER").is_some() {
            let path = std::path::Path::new("target/render").join(format!("{name}.png"));
            flipctl_app::render::save(&path, &frame);
        }
        frame
    }

    fn live() -> Status {
        Status {
            battery: 76,
            wifi_connected: true,
            wifi_quality: 80,
            ethernet: Ethernet::Real,
            ..Status::default()
        }
    }

    /// Every pixel holds a design-token value, with nothing on the bar: its icons are
    /// sprites whose edges are greys of their own.
    #[test]
    fn both_pages_are_drawn_only_in_tokens() {
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        for canvas in [false, true] {
            let frame = shot(if canvas { "canvas" } else { "rows" }, canvas, Status::default());
            let stride = usize::from(theme::PANEL_W);
            let offenders: Vec<String> = frame
                .iter()
                .enumerate()
                .filter(|(_, px)| !allowed.contains(px))
                .take(8)
                .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
                .collect();
            assert!(offenders.is_empty(), "canvas {canvas}: off-palette pixels: {offenders:?}");
        }
    }

    /// The pages as the guide shows them, with a live bar.
    #[test]
    fn the_guide_shots() {
        let rows = shot("hello", false, live());
        let canvas = shot("hello-canvas", true, live());
        assert_ne!(rows, canvas);
    }
}
