//! The choice popup, pinned as pixels.
//!
//! Its geometry is unit-tested in `src/popup.rs`; what this checks is the half that
//! only exists once it is drawn. Built by the same functions the binary calls, with
//! the options the USB-C power question asks on a Flipper One.

#![cfg(feature = "screens")]

mod support;

use flipper_ui::popup::{self, Choice};
use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
use flipper_ui::theme;
use flipper_ui::ui::{ChoiceLine, ChoiceRow, ChoiceView, ListItem, Root, Screen};
use flipper_ui::Surface;
use slint::ComponentHandle;

/// The same mapping the binary does at its boundary.
fn apply(screen: &Root, choice: &Choice) {
    let v = popup::view(choice);
    let lines = |lines: &[popup::Line]| {
        slint::ModelRc::new(slint::VecModel::from(
            lines
                .iter()
                .map(|l| ChoiceLine { x: l.x as f32, y: l.y as f32, text: l.text.as_str().into() })
                .collect::<Vec<_>>(),
        ))
    };
    screen.set_choice_view(ChoiceView {
        x: v.x as f32,
        y: v.y as f32,
        w: v.w as f32,
        h: v.h as f32,
        title: v.title.text.as_str().into(),
        title_x: v.title.x as f32,
        title_y: v.title.y as f32,
        row_x: v.row_x as f32,
        row_w: v.row_w as f32,
        sel_y: v.sel_y as f32,
    });
    screen.set_choice_hints(lines(&v.hints));
    screen.set_choice_rows(slint::ModelRc::new(slint::VecModel::from(
        v.rows
            .iter()
            .map(|r| ChoiceRow {
                y: r.y as f32,
                text: r.text.as_str().into(),
                note: r.note.as_str().into(),
                note_x: r.note_x as f32,
                rule: r.rule,
            })
            .collect::<Vec<_>>(),
    )));
    screen.set_choice_open(true);
}

fn usb_power(selected: usize) -> Choice {
    Choice {
        title: "Select USB-C1 output power".into(),
        options: vec!["5V 3A (15W)".into(), "5-20V (18W)".into(), "5-20V (27W)".into()],
        notes: vec![
            "Compatibility mode".into(),
            "Normal mode".into(),
            "Burst-only high power mode".into(),
        ],
        hints: vec![vec![], vec![], vec!["CAUTION: the device will overheat quickly".into()]],
        selected,
    }
}

/// The settings menu, without icons, so everything under the wash is a token.
fn settings(screen: &Root) {
    let items: Vec<ListItem> = ["System info", "Battery info", "USB-C1 power out", "Disk info"]
        .iter()
        .map(|label| ListItem { label: (*label).into(), frames: 1, ..Default::default() })
        .collect();
    screen.set_total(items.len() as i32);
    screen.set_items(slint::ModelRc::new(slint::VecModel::from(items)));
    screen.set_selected(2);
    screen.set_real_frame(true);
    screen.set_battery(87);
    screen.set_breadcrumb("> Settings".into());
    screen.set_screen(Screen::Menu);
}

fn grab(window: &slint::platform::software_renderer::MinimalSoftwareWindow) -> Surface {
    let frame = render_frame(window).expect("frame");
    let mut surface = Surface::panel();
    for (i, px) in frame.iter().enumerate() {
        let x = (i % usize::from(theme::PANEL_W)) as i32;
        let y = (i / usize::from(theme::PANEL_W)) as i32;
        surface.pixel(x, y, *px);
    }
    surface
}

/// Tokens only, or the scrim's exact composite of one, outside the status bar's
/// sprites; and the goldens, with the selector on each of the three options and the
/// right button pressed.
#[test]
fn the_choice_popup_holds_its_geometry() {
    let tokens: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
    let dimmed = |over: u8| {
        (f32::from(theme::color::OVERLAY.0) * theme::alpha::OVERLAY
            + f32::from(over) * (1.0 - theme::alpha::OVERLAY))
            .round() as u8
    };
    let allowed: Vec<u8> =
        tokens.iter().copied().chain(tokens.iter().copied().map(dimmed)).collect();
    let sprite = |x: i32, y: i32| {
        y < theme::metric::STATUS_BAR_H && (x < 48 || x >= i32::from(theme::PANEL_W) - 20)
    };

    let window = FlipperSlintPlatform::install();
    let screen = Root::new().expect("create Root");
    screen.show().expect("show");
    settings(&screen);

    for (name, selected, pressed) in [
        ("choice-usb-power", 0, -1),
        ("choice-usb-power-18w", 1, -1),
        ("choice-usb-power-27w-apply", 2, 4),
    ] {
        apply(&screen, &usb_power(selected));
        screen.set_choice_pressed_slot(pressed);
        slint::platform::update_timers_and_animations();
        let surface = grab(&window);

        let mut offenders = Vec::new();
        for y in 0..i32::from(theme::PANEL_H) {
            for x in 0..i32::from(theme::PANEL_W) {
                let v = surface.get(x as u16, y as u16).0;
                if !sprite(x, y) && !allowed.contains(&v) {
                    offenders.push(format!("({x}, {y}) = {v}"));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "{name}: off-palette pixels {:?}",
            &offenders[..offenders.len().min(8)]
        );
        support::assert_golden(name, &surface);
    }
}
