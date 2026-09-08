//! The boot menu's terminal screen, off the device.
//!
//! Two modes. `--dump` lays a synthetic view out and prints it as text, which is how
//! the geometry is read without a terminal at all. With no arguments it runs the real
//! cursive front end on the current terminal, so the keys and the redraw can be
//! tried: arrows move, F2 and F4 would open the popups on a real menu, Ctrl-C quits.
//!
//!     cargo run -p flipper-ui --features tui --example tui_probe -- --dump
//!     cargo run -p flipper-ui --features tui --example tui_probe

use flipper_ui::boot_menu::{PopupLine, Row, View};
use flipper_ui::tui::boot;

fn row(label: &str, status: &str, auto: bool, medium: i32) -> Row {
    Row {
        label: label.into(),
        status: status.into(),
        icon: 1,
        icon_w: 10.0,
        icon_h: 8.0,
        auto,
        medium,
    }
}

/// A list the size the device actually shows, with one of each badge on it.
fn sample(popup: bool) -> View {
    View {
        rows: vec![
            row("Minimal", "Used 3 hours ago", false, 0),
            row("Desktop", "Running", true, 0),
            row("TV-Media-Box", "Used 2 days ago", false, 0),
            row("Router", "", false, 0),
            row("[a very long derived profile label]", "Used 9 days ago", false, 1),
            row("Recovery", "Never used", false, 2),
        ],
        selected: 1,
        scroll: 0,
        countdown: 40,
        remaining: 3,
        loading: false,
        spin_frame: 0,
        booting: String::new(),
        going_down: false,
        popup_open: popup,
        popup_title: "Desktop".into(),
        popup_icon: 2,
        popup_lines: vec![
            PopupLine {
                kind: 0,
                y: 0.0,
                text: "Drive: /dev/sda (UFS)".into(),
                value: String::new(),
                selected: false,
                heart: false,
            },
            PopupLine {
                kind: 0,
                y: 0.0,
                text: "Kernel: 7.0.1 (running)".into(),
                value: String::new(),
                selected: false,
                heart: false,
            },
            PopupLine {
                kind: 2,
                y: 0.0,
                text: "Video Out".into(),
                value: "HDMI".into(),
                selected: true,
                heart: false,
            },
            PopupLine {
                kind: 1,
                y: 0.0,
                text: "Config".into(),
                value: String::new(),
                selected: false,
                heart: false,
            },
        ],
        popup_message: Vec::new(),
        popup_button: String::new(),
        size_num: "378.0".into(),
        size_unit: "MiB".into(),
        size_loading: false,
        popup_w: 0.0,
        popup_body_h: 0.0,
        size_slot_w: 0.0,
        buttons: ["", "View", "", "Edit", ""],
        // Measured, as it is once View has been opened on this profile.
        selected_size: Some(("378.0".into(), "MiB".into())),
    }
}

fn dump(title: &str, view: &View, cols: usize, rows: usize) {
    println!("{title}  ({cols}x{rows})");
    println!("+{}+", "-".repeat(cols));
    for line in boot::screen(view, cols, rows) {
        println!("|{line}|");
    }
    println!("+{}+\n", "-".repeat(cols));
}

fn main() {
    if std::env::args().any(|a| a == "--dump") {
        dump("the list, counting down", &sample(false), 80, 24);
        dump("the View popup over it", &sample(true), 80, 24);
        let mut narrow = sample(false);
        narrow.countdown = -1;
        narrow.remaining = 0;
        dump("a small terminal, countdown cancelled", &narrow, 40, 12);
        let mut booting = sample(false);
        booting.booting = "Desktop".into();
        booting.buttons = ["", "", "", "", ""];
        dump("the handover", &booting, 80, 24);
        let mut empty = sample(false);
        empty.rows.clear();
        empty.loading = true;
        empty.countdown = -1;
        dump("still reading the drives", &empty, 80, 24);
        return;
    }

    let mut terminal = match flipper_ui::tui::Terminal::open() {
        Ok(terminal) => terminal,
        Err(e) => {
            eprintln!("no terminal: {e}");
            std::process::exit(1);
        }
    };
    // No BootMenu here: this is the drawing side on its own, so the countdown is
    // wound by hand and the keys only prove they arrive.
    let mut view = sample(false);
    let mut pressed = String::new();
    let start = std::time::Instant::now();
    loop {
        while let Some(event) = terminal.poll_event() {
            match event {
                flipper_ui::tui::TerminalEvent::Key(event) if event.down => {
                    pressed = format!("{:?}", event.key);
                    match event.key {
                        flipper_ui::FlipperKey::Down => {
                            view.selected = (view.selected + 1) % view.rows.len() as i32
                        }
                        flipper_ui::FlipperKey::Up => {
                            view.selected =
                                (view.selected - 1).rem_euclid(view.rows.len() as i32)
                        }
                        flipper_ui::FlipperKey::View => view.popup_open = !view.popup_open,
                        _ => {}
                    }
                }
                flipper_ui::tui::TerminalEvent::Quit => {
                    terminal.stop();
                    println!("quit");
                    return;
                }
                flipper_ui::tui::TerminalEvent::Closed => return,
                _ => {}
            }
        }
        let elapsed = start.elapsed().as_secs_f32();
        view.countdown = ((elapsed / 10.0) * 100.0) as i32;
        view.remaining = (10.0 - elapsed).max(0.0).ceil() as i32;
        if view.countdown > 100 {
            view.countdown = -1;
        }
        view.popup_title = if pressed.is_empty() {
            "Desktop".into()
        } else {
            format!("Desktop  last key {pressed}")
        };
        terminal.show(clone_of(&view));
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}

/// `View` is plain data but carries no `Clone`, and the probe redraws from one it
/// keeps. The menu itself never needs this: it builds a fresh view every frame.
fn clone_of(view: &View) -> View {
    View {
        rows: view
            .rows
            .iter()
            .map(|r| row(&r.label, &r.status, r.auto, r.medium))
            .collect(),
        selected: view.selected,
        scroll: view.scroll,
        countdown: view.countdown,
        remaining: view.remaining,
        loading: view.loading,
        spin_frame: view.spin_frame,
        booting: view.booting.clone(),
        going_down: view.going_down,
        popup_open: view.popup_open,
        popup_title: view.popup_title.clone(),
        popup_icon: view.popup_icon,
        popup_lines: view
            .popup_lines
            .iter()
            .map(|l| PopupLine {
                kind: l.kind,
                y: l.y,
                text: l.text.clone(),
                value: l.value.clone(),
                selected: l.selected,
                heart: l.heart,
            })
            .collect(),
        popup_message: view.popup_message.clone(),
        popup_button: view.popup_button.clone(),
        size_num: view.size_num.clone(),
        size_unit: view.size_unit.clone(),
        size_loading: view.size_loading,
        popup_w: view.popup_w,
        popup_body_h: view.popup_body_h,
        size_slot_w: view.size_slot_w,
        buttons: view.buttons,
        selected_size: view.selected_size.clone(),
    }
}
