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
//! Usage: flipper-boot-menu [--kms-device /dev/dri/cardN]

use std::time::{Duration, Instant};

use flipper_ui::boot_menu::{BootMenu, Outcome};
use flipper_ui::evdev::EvdevSource;
use flipper_ui::kms::KmsSink;
use flipper_ui::slint_render::{render_into, FlipperSlintPlatform};
use flipper_ui::theme::count::BOOT_VISIBLE_ROWS;
use flipper_ui::{keyboard, Frame, FrameSink, InputSource, PANEL_H, PANEL_W};
use slint::ComponentHandle;

slint::include_modules!();

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("Usage: flipper-boot-menu [--kms-device /dev/dri/cardN]");
        return std::process::ExitCode::SUCCESS;
    }
    let card = args
        .windows(2)
        .find(|w| w[0] == "--kms-device")
        .map(|w| w[1].clone());

    match run(card.as_deref()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("boot menu      {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(card: Option<&str>) -> std::io::Result<()> {
    let mut sink = KmsSink::open(card.map(std::path::Path::new))?;
    let (w, h) = sink.size();
    if (w, h) != (PANEL_W, PANEL_H) {
        return Err(std::io::Error::other(format!(
            "panel reports {w}x{h}, this build is compiled for {PANEL_W}x{PANEL_H}"
        )));
    }
    eprintln!("panel          {w}x{h}, {}", sink.format());
    // The buttons are on i2c and their probe can fail, which it has: the menu then exited
    // for want of them and init respawned it about once a second, so the panel showed
    // nothing and the countdown never ran. Draw regardless and keep looking, because a
    // device whose buttons are dead still has to boot the profile that is marked.
    let mut input = match EvdevSource::open() {
        Ok(source) => Some(source),
        Err(e) => {
            eprintln!("boot menu      no buttons yet: {e}");
            None
        }
    };
    let mut looked_for_input = Instant::now();

    let window = FlipperSlintPlatform::install();
    let ui = Menu::new().map_err(|e| std::io::Error::other(e.to_string()))?;
    ui.show().map_err(|e| std::io::Error::other(e.to_string()))?;

    let mut menu = BootMenu::open(BOOT_VISIBLE_ROWS as i32);
    // The keyboard a rename asks for, and the profile it is renaming.
    let mut kb: Option<keyboard::TextInput> = None;
    let mut kb_for = String::new();
    let mut warning = String::new();

    let mut frame: Vec<flipper_ui::Gray8> = Vec::new();
    let mut dirty = true;
    // Whether the takeover has had its one frame; see the loop for why it gets only one.
    let mut takeover_committed = false;
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
                eprintln!("boot menu      buttons appeared");
                input = Some(source);
            }
        }

        // Drained, not sampled: a press and its release arrive as two events and
        // both have to be seen on the turn they land, or a held key reads as stuck.
        while let Some(event) = input.as_mut().and_then(InputSource::poll) {
            dirty = true;
            // What makes the name invalid, which also gates saving it.
            warning = match kb.as_ref() {
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
                // The keyboard owns every key while it is up, and a release is not a
                // press: acting on both typed every character twice. The only thing that
                // wants the release is the OK hold that latches caps lock, which never
                // fired here because release() was never the call being made.
                Some(field) if !event.down => {
                    field.release(event.key);
                }
                Some(field) => match field.key(event.key, warning.is_empty()) {
                    Some(keyboard::Exit::Save(text)) => {
                        menu.renamed(&kb_for, Some(&text));
                        kb = None;
                    }
                    Some(keyboard::Exit::Cancel) => {
                        menu.renamed(&kb_for, None);
                        kb = None;
                    }
                    None => {}
                },
                None => match menu.key(event) {
                    Outcome::Stay => {}
                    // Nothing behind this screen: the menu is the program, so Back
                    // has nowhere to go and the list stays.
                    Outcome::Leave => {}
                    Outcome::Rename { name, label } => {
                        kb = Some(keyboard::TextInput::new("Profile name", &label));
                        kb_for = name;
                    }
                },
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
        let booting = !menu.view().booting.is_empty();
        if dirty {
            dirty = false;
            apply(&ui, &menu, kb.as_ref(), &warning);
            window.request_redraw();
            if !booting || !takeover_committed {
                if let Some(damage) = render_into(&window, &mut frame) {
                    sink.commit(Frame::new(&frame, PANEL_W, PANEL_H), damage)?;
                }
                if booting {
                    takeover_committed = true;
                    eprintln!("boot menu      takeover drawn; the panel is left alone from here");
                }
            }
        }

        std::thread::sleep(pace);
    }
}

/// Push the menu's view onto the window, and the keyboard's if it is up.
fn apply(ui: &Menu, menu: &BootMenu, kb: Option<&keyboard::TextInput>, warning: &str) {
    let view = menu.view();

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
    let lines: Vec<BootPopupRow> = view
        .popup_lines
        .iter()
        .map(|l| BootPopupRow {
            kind: l.kind,
            text: l.text.as_str().into(),
            selected: l.selected,
            heart: l.heart,
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
