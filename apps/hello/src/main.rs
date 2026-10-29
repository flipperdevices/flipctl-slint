use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use flipctl_app::paint::Surface;
use flipctl_app::theme::{color, metric, PANEL_H, PANEL_W};
use flipctl_app::{Key, StatusSource};
use slint::{ModelRc, SharedString, VecModel};

slint::include_modules!();

const SLOTS: u32 = 10;

fn row(kind: i32, label: &str, value: &str, percent: i32) -> DetailRow {
    DetailRow {
        kind,
        label: label.into(),
        value: value.into(),
        percent,
        dim: false,
    }
}

fn rows(presses: u32) -> Vec<DetailRow> {
    vec![
        row(3, "Hello, Flipper One!", "", 0),
        row(1, "", "", 0),
        row(0, "Presses", &presses.to_string(), 0),
        row(2, "Progress", "", (presses * 100 / SLOTS).min(100) as i32),
    ]
}

/// The canvas page: a frame between the status bar and the soft keys, and a slot
/// per press inside it, filled as the presses come in.
fn picture(presses: u32) -> (slint::Image, Vec<CanvasText>) {
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

    let texts = vec![CanvasText {
        x: f32::from(PANEL_W / 2),
        y: (top + 10) as f32,
        text: format!("Presses: {presses}").into(),
        font: 0,
        align: 0,
        white: false,
    }];
    (flipctl_app::picture(&surface), texts)
}

fn draw(ui: &AppWindow, canvas: bool, presses: u32) {
    let buttons: Vec<SharedString> =
        ["Close", if canvas { "Rows" } else { "Draw" }, "", "", "Add"].map(Into::into).into();
    ui.set_buttons(ModelRc::new(VecModel::from(buttons)));
    ui.set_canvas(canvas);
    if canvas {
        let (image, texts) = picture(presses);
        ui.set_picture(image);
        ui.set_texts(ModelRc::new(VecModel::from(texts)));
    } else {
        ui.set_rows(ModelRc::new(VecModel::from(rows(presses))));
    }
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    draw(&ui, false, 0);

    let mut status = StatusSource::new(Duration::from_secs(2));
    flipctl_app::apply_status!(&ui, PanelStatus, status.current());
    let ticking = ui.as_weak();
    let tick = slint::Timer::default();
    tick.start(slint::TimerMode::Repeated, Duration::from_secs(1), move || {
        if let (Some(ui), Some(now)) = (ticking.upgrade(), status.poll()) {
            flipctl_app::apply_status!(&ui, PanelStatus, now);
        }
    });

    let (presses, canvas) = (Rc::new(Cell::new(0)), Rc::new(Cell::new(false)));
    let keys = ui.as_weak();
    ui.on_keyed(move |text, down| {
        let (Some(ui), Some(key)) = (keys.upgrade(), Key::from_slint(&text)) else {
            return;
        };
        ui.set_pressed_slot(match (down, key.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        if !down {
            return;
        }
        match key {
            Key::Back | Key::Escape => {
                let _ = slint::quit_event_loop();
                return;
            }
            Key::Ok | Key::Run => presses.set(presses.get() + 1),
            Key::View => canvas.set(!canvas.get()),
            _ => return,
        }
        draw(&ui, canvas.get(), presses.get());
    });

    ui.run()
}
