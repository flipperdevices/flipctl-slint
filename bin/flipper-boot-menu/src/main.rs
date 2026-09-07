//! The Flipper One boot menu.
//!
//! The first thing the device runs: it lists the bootable profiles it can find, on
//! the internal storage and on anything plugged in, and kexecs into the one chosen,
//! with that profile's own kernel, initrd and device tree. It supplies no kernel of
//! its own and boots nothing by itself.
//!
//! Everything it knows lives in flipper-ui: the screen is flipctl's own boot menu
//! body, the decisions are `boot_menu::BootMenu`, and the profiles come from the
//! btrfs tools. What is here is the loop, the panel, the keys, and the keyboard a
//! rename needs.
//!
//! When stdio is a terminal, the same menu is drawn on it as well, so a boot can be
//! watched and driven from a debug probe. One `BootMenu` answers both: the terminal
//! is a second pair of eyes on the state the panel is showing, never a second copy.
//!
//! Usage: flipper-boot-menu [--kms-device /dev/dri/cardN] [--all-kernels] [--no-tui]

use std::os::unix::process::CommandExt;
use std::time::{Duration, Instant};

use flipper_ui::boot::Kernels;
use flipper_ui::boot_menu::{AutoStart, BootMenu, Outcome, View as BootView};
use flipper_ui::evdev::EvdevSource;
use flipper_ui::kms::KmsSink;
use flipper_ui::slint_render::{render_into, FlipperSlintPlatform};
use flipper_ui::theme::count::BOOT_VISIBLE_ROWS;
use flipper_ui::tui::{text::Rules, Terminal, TerminalEvent};
use flipper_ui::{keyboard, Frame, FrameSink, InputSource, PANEL_H, PANEL_W};
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        flipper_ui::logline!(
            "Usage: flipper-boot-menu [--kms-device /dev/dri/cardN] [--all-kernels] [--no-tui]"
        );
        return std::process::ExitCode::SUCCESS;
    }
    // The terminal is taken when there is one, which is the useful default: run from
    // init with stdio on /dev/console there is none, and nothing happens. --no-tui is
    // for a console somebody else wants, and for proving the menu is no slower without.
    let want_tui = !args.iter().any(|a| a == "--no-tui");
    let card = args
        .windows(2)
        .find(|w| w[0] == "--kms-device")
        .map(|w| w[1].clone());
    // Old kernels are hidden unless asked for: a 6.1 BSP entry left on disk boots
    // nothing anybody wants, and the menu is a list of things worth choosing.
    let kernels = if args.iter().any(|a| a == "--all-kernels") {
        Kernels::All
    } else {
        Kernels::Modern
    };

    match run(card.as_deref(), kernels, want_tui) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            flipper_ui::logline!("boot menu      {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(card: Option<&str>, kernels: Kernels, want_tui: bool) -> std::io::Result<()> {
    let mut sink = KmsSink::open(card.map(std::path::Path::new))?;
    let (w, h) = sink.size();
    if (w, h) != (PANEL_W, PANEL_H) {
        return Err(std::io::Error::other(format!(
            "panel reports {w}x{h}, this build is compiled for {PANEL_W}x{PANEL_H}"
        )));
    }
    flipper_ui::logline!("panel          {w}x{h}, {}", sink.format());
    // The buttons are on i2c and their probe can fail, which it has: the menu then exited
    // for want of them and init respawned it about once a second, so the panel showed
    // nothing and the countdown never ran. Draw regardless and keep looking, because a
    // device whose buttons are dead still has to boot the profile that is marked.
    let mut input = match EvdevSource::open() {
        Ok(source) => Some(source),
        Err(e) => {
            flipper_ui::logline!("boot menu      no buttons yet: {e}");
            None
        }
    };
    let mut looked_for_input = Instant::now();

    let window = FlipperSlintPlatform::install();
    let ui = Menu::new().map_err(|e| std::io::Error::other(e.to_string()))?;
    ui.show().map_err(|e| std::io::Error::other(e.to_string()))?;

    let mut menu = BootMenu::open(BOOT_VISIBLE_ROWS as i32, AutoStart::Countdown, kernels);
    // The keyboard a rename asks for, and the profile it is renaming.
    let mut kb: Option<keyboard::TextInput> = None;
    let mut kb_for = String::new();
    let mut warning = String::new();

    let mut frame: Vec<flipper_ui::Gray8> = Vec::new();
    let mut dirty = true;
    // Whether the takeover has had its one frame; see the loop for why it gets only one.
    let mut takeover_committed = false;
    // Whether anything has reached the panel yet, for the one line that says so, and
    // for the terminal: that is opened after the first frame and never before.
    // Somebody holding the device is waiting on the panel, and setting a terminal up
    // (termios, the alternate screen, its first draw) is work that belongs behind it.
    let mut drawn = false;
    let mut tui: Option<Terminal> = None;
    let mut tui_tried = !want_tui;
    // The same 8ms flipctl paces its loop at: the panel takes a frame every 16 to 19
    // milliseconds, so this is twice the rate anything can be shown at and the keys
    // never wait for a frame.
    let pace = Duration::from_millis(8);

    loop {
        // Buttons that were not there at startup may arrive later, so ask again now and
        // then. Not every frame: opening them walks /dev/input.
        if input.is_none() && looked_for_input.elapsed() >= Duration::from_secs(1) {
            looked_for_input = Instant::now();
            if let Ok(source) = EvdevSource::open() {
                flipper_ui::logline!("boot menu      buttons appeared");
                input = Some(source);
            }
        }

        // Drained, not sampled: a press and its release arrive as two events and
        // both have to be seen on the turn they land, or a held key reads as stuck.
        while let Some(event) = input.as_mut().and_then(InputSource::poll) {
            dirty = true;
            press(event, &mut menu, &mut kb, &mut kb_for, &mut warning, tui.as_ref());
        }

        // The terminal, drained the same way and into the same place. A key typed on
        // a debug probe moves the panel's cursor and a button moves the terminal's,
        // because there is one menu and these are two views of it.
        while let Some(event) = tui.as_mut().and_then(Terminal::poll_event) {
            dirty = true;
            match event {
                TerminalEvent::Key(event) => {
                    press(event, &mut menu, &mut kb, &mut kb_for, &mut warning, tui.as_ref())
                }
                // Typed rather than picked out of the on-screen keyboard, but the
                // same answer to the same question: the panel's field goes with it.
                TerminalEvent::Renamed(text) => {
                    menu.renamed(&kb_for, text.as_deref());
                    kb = None;
                }
                // A shell, for somebody diagnosing a boot from the probe. Disarm
                // first: an image left loaded boots on the next reset, which is not
                // what asking for a shell asked for. init respawns the menu when the
                // shell exits, because exec keeps this PID.
                TerminalEvent::Quit => {
                    if menu.holds_an_arm() {
                        flipper_ui::boot::disarm();
                    }
                    if let Some(terminal) = tui.take() {
                        terminal.stop();
                    }
                    let e = std::process::Command::new("/bin/sh").exec();
                    flipper_ui::logline!("boot menu      no shell: {e}");
                }
                TerminalEvent::Closed => tui = None,
            }
        }

        if menu.tick() {
            dirty = true;
        }
        if let Some(field) = kb.as_mut() {
            // animating() is what expires a press flash, so it has to be asked every
            // frame: without the call the highlight never cleared and the key stayed
            // black as though it were held down. The cursor blinks anyway, so the field
            // is never still for long.
            field.animating();
            dirty = true;
        }

        // The takeover is committed once and never again: nothing may transfer to the
        // panel while a kexec loads.
        //
        // Measured on the device. A commit is an SPI write over pl330, which keeps that
        // controller clocked with an event armed; kexec does not reset it, so the next
        // kernel takes the interrupt during its own probe, walks channels[] with the -1
        // that means "no channel assigned", and dies on a NULL dereference twenty
        // milliseconds before its console exists. With frames still going: a panic on
        // nearly every attempt, dmac0 reading INTEN=0x1. With this one frame only: dmac0
        // gated and idle, and the boot succeeds. The proper fix is a shutdown hook in the
        // pl330 driver, after which frames here would be harmless again.
        if dirty {
            dirty = false;
            // Built once and shown to both screens. It used to be built twice a frame,
            // once here for `booting` and once inside apply().
            let view = menu.view();
            let booting = !view.booting.is_empty();
            apply(&ui, &view, kb.as_ref(), &warning);
            window.request_redraw();
            if !booting || !takeover_committed {
                if let Some(damage) = render_into(&window, &mut frame) {
                    sink.commit(Frame::new(&frame, PANEL_W, PANEL_H), damage)?;
                    if !drawn {
                        drawn = true;
                        flipper_ui::logline!("boot menu      first frame on the panel");
                    }
                }
                if booting {
                    takeover_committed = true;
                    flipper_ui::logline!("boot menu      takeover drawn; the panel is left alone from here");
                }
            }
            match tui.take() {
                // The console is given back at the handover rather than held to the
                // end. Whatever the alternate screen is showing is thrown away when it
                // closes, so the line that says what is booting has to be printed after
                // it, in cooked mode, where it stays above the next kernel's log.
                Some(terminal) if booting => {
                    terminal.stop();
                    println!("Booting {}", view.booting);
                }
                Some(terminal) => {
                    terminal.show(view);
                    tui = Some(terminal);
                }
                None => {}
            }
        }

        // Behind the first frame, so nothing the panel is waiting for queues behind a
        // termios call. Tried once: no terminal is the ordinary case, not an error.
        if drawn && !tui_tried {
            tui_tried = true;
            match Terminal::open() {
                // No line of our own on success: Terminal::open logs what it found
                // while the console can still be seen, which is the only window there
                // is for it.
                Ok(terminal) => {
                    tui = Some(terminal);
                    dirty = true;
                }
                Err(e) => flipper_ui::logline!("boot menu      no terminal: {e}"),
            }
        }

        std::thread::sleep(pace);
    }
}

/// One press, wherever it came from.
///
/// A button and a key typed on the serial console are the same event by the time
/// they reach here, which is what keeps the two screens showing the same thing. The
/// only asymmetry is the rename: the panel has a d-pad and needs its on-screen
/// keyboard, the terminal has a real one and gets a field, so both go up together and
/// whichever is answered first takes the other down.
fn press(
    event: flipper_ui::KeyEvent,
    menu: &mut BootMenu,
    kb: &mut Option<keyboard::TextInput>,
    kb_for: &mut String,
    warning: &mut String,
    tui: Option<&Terminal>,
) {
    // What makes the name invalid, which also gates saving it.
    *warning = match kb.as_ref() {
        Some(field) => {
            let being = flipper_ui::boot::Profile {
                name: kb_for.clone(),
                ..Default::default()
            };
            flipper_ui::boot::rename_warning(&field.text, &being, menu.profiles())
        }
        None => String::new(),
    };
    match kb.as_mut() {
        // The keyboard owns every key while it is up, and a release is not a press:
        // acting on both typed every character twice. The only thing that wants the
        // release is the OK hold that latches caps lock, which never fired here
        // because release() was never the call being made.
        Some(field) if !event.down => {
            field.release(event.key);
        }
        Some(field) => match field.key(event.key, warning.is_empty()) {
            Some(keyboard::Exit::Save(text)) => {
                menu.renamed(kb_for, Some(&text));
                *kb = None;
                if let Some(terminal) = tui {
                    terminal.dismiss_prompt();
                }
            }
            Some(keyboard::Exit::Cancel) => {
                menu.renamed(kb_for, None);
                *kb = None;
                if let Some(terminal) = tui {
                    terminal.dismiss_prompt();
                }
            }
            None => {}
        },
        None => match menu.key(event) {
            Outcome::Stay => {}
            // Nothing behind this screen: the menu is the program, so Back has
            // nowhere to go. It reads the drives again instead, which is what
            // somebody who just pushed a card in is asking for.
            Outcome::Leave => menu.reread(),
            Outcome::Rename { name, label } => {
                if let Some(terminal) = tui {
                    let being = flipper_ui::boot::Profile {
                        name: name.clone(),
                        ..Default::default()
                    };
                    let rules = Rules {
                        being,
                        profiles: menu.profiles().to_vec(),
                    };
                    terminal.prompt_rename("Profile name", &label, rules);
                }
                *kb = Some(keyboard::TextInput::new("Profile name", &label));
                *kb_for = name;
            }
        },
    }
}

/// Push the menu's view onto the window, and the keyboard's if it is up.
fn apply(ui: &Menu, view: &BootView, kb: Option<&keyboard::TextInput>, warning: &str) {
    let rows: Vec<BootRow> = view
        .rows
        .iter()
        .map(|r| BootRow {
            label: r.label.as_str().into(),
            status: r.status.as_str().into(),
            icon: r.icon,
            icon_w: r.icon_w,
            icon_h: r.icon_h,
            auto: r.auto,
            medium: r.medium,
        })
        .collect();
    ui.set_rows(slint::ModelRc::new(slint::VecModel::from(rows)));
    ui.set_selected(view.selected);
    ui.set_scroll(view.scroll);
    ui.set_countdown(view.countdown);
    ui.set_remaining(view.remaining);
    ui.set_loading(view.loading);
    ui.set_spin_frame(view.spin_frame);
    ui.set_booting(view.booting.as_str().into());

    let buttons: Vec<slint::SharedString> =
        view.buttons.iter().map(|s| (*s).into()).collect();
    ui.set_buttons(slint::ModelRc::new(slint::VecModel::from(buttons)));

    ui.set_popup_open(view.popup_open);
    ui.set_popup_title(view.popup_title.as_str().into());
    ui.set_popup_icon(view.popup_icon);
    // The popup header uses the larger 14x14 icons, not the row's.
    ui.set_popup_icon_w(14.0);
    ui.set_popup_icon_h(14.0);
    ui.set_popup_size_number(view.size_num.as_str().into());
    ui.set_popup_size_unit(view.size_unit.as_str().into());
    // While loading, the slot keeps the widest spinner frame's width; once the value
    // lands it is the value's own.
    ui.set_popup_size_slot_w(view.size_slot_w);
    // The frame's own measured width, so it fits what is in it.
    ui.set_popup_w(view.popup_w);
    ui.set_popup_body_h(view.popup_body_h);
    let lines: Vec<BootPopupRow> = view
        .popup_lines
        .iter()
        .map(|l| BootPopupRow {
            kind: l.kind,
            y: l.y,
            text: l.text.as_str().into(),
            selected: l.selected,
            heart: l.heart,
            value: l.value.as_str().into(),
        })
        .collect();
    ui.set_popup_rows(slint::ModelRc::new(slint::VecModel::from(lines)));
    let message: Vec<slint::SharedString> =
        view.popup_message.iter().map(|m| m.as_str().into()).collect();
    ui.set_popup_message(slint::ModelRc::new(slint::VecModel::from(message)));
    ui.set_popup_hint(view.popup_button.as_str().into());

    ui.set_keyboard(kb.is_some());
    if let Some(field) = kb {
        let v = field.view(warning, field.cursor_visible());
        ui.set_kb_title(v.title.as_str().into());
        ui.set_kb_text(v.text.as_str().into());
        ui.set_kb_field_w(v.field_w);
        ui.set_kb_cursor_dx(v.cursor_dx);
        ui.set_kb_cursor_on(v.cursor_on);
        ui.set_kb_field_focused(v.field_focused);
        ui.set_kb_warning(v.warning.as_str().into());
        let cells: Vec<KbCell> = v
            .cells
            .iter()
            .map(|c| KbCell {
                x: c.x as f32,
                y: c.y as f32,
                w: c.w as f32,
                text: c.text.as_str().into(),
                icon: c.icon,
                icon_w: c.icon_w as f32,
                icon_h: c.icon_h as f32,
                selected: c.selected,
                pressed: c.pressed,
                clip_h: c.clip_h as f32,
            })
            .collect();
        ui.set_kb_cells(slint::ModelRc::new(slint::VecModel::from(cells)));
        ui.set_kb_chrome_x(v.chrome.0);
        ui.set_kb_chrome_y(v.chrome.1);
        ui.set_kb_chrome_w(v.chrome.2);
        ui.set_kb_chrome_h(v.chrome.3);
        ui.set_kb_lang_label(v.lang_label.into());
        ui.set_kb_tab_label(v.tab_label.into());
        ui.set_kb_tab_focus(v.tab_focus);
        ui.set_kb_tab_pressed(v.tab_pressed);
        ui.set_kb_discard(v.discard);
        // The keyboard's own two labelled keys, as flipctl labels them.
        let buttons: Vec<slint::SharedString> = ["Cancel", "", "", "", "Done"]
            .iter()
            .map(|s| (*s).into())
            .collect();
        ui.set_kb_buttons(slint::ModelRc::new(slint::VecModel::from(buttons)));
    }
}
