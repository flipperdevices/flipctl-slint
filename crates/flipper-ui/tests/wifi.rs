//! The Wi-Fi page and its three modals, pinned as pixels.
//!
//! The rows and the geometry are unit-tested next to themselves in `src/wifi.rs`;
//! what these check is the half that only exists once it is drawn. Every one of
//! them is built by the same functions the binary calls, so a golden that moves is
//! either a real change to the screen or a change to what the screen is made of,
//! and never the test having its own idea of the layout.

#![cfg(feature = "screens")]

mod support;

use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
use flipper_ui::theme;
use flipper_ui::ui::{Root, Screen, WifiDetailRow, WifiNetRow, WifiRow};
use flipper_ui::wifi::{self, Details, Family, Network, Saved};
use flipper_ui::Surface;
use slint::ComponentHandle;

/// The same mapping `apply_wifi` does, and the reason it is repeated here rather
/// than shared: it is the binary's boundary, not the library's, and a test that
/// imported it would be testing its own copy of the model instead of the rows.
fn rows(model: &[wifi::Row]) -> slint::ModelRc<WifiRow> {
    slint::ModelRc::new(slint::VecModel::from(
        model
            .iter()
            .map(|r| WifiRow {
                kind: r.kind,
                y: r.y as f32,
                text: r.text.as_str().into(),
                value: r.value.as_str().into(),
                text_x: r.text_x as f32,
                chevron: r.chevron,
            })
            .collect::<Vec<_>>(),
    ))
}

fn net_rows(model: &[wifi::NetRow]) -> slint::ModelRc<WifiNetRow> {
    slint::ModelRc::new(slint::VecModel::from(
        model
            .iter()
            .map(|r| WifiNetRow {
                text: r.text.as_str().into(),
                security: r.security.as_str().into(),
                quality: r.quality,
                divider: r.divider,
            })
            .collect::<Vec<_>>(),
    ))
}

fn detail_rows(model: &[wifi::DetailRow]) -> slint::ModelRc<WifiDetailRow> {
    slint::ModelRc::new(slint::VecModel::from(
        model
            .iter()
            .map(|r| WifiDetailRow {
                kind: r.kind,
                y: r.y as f32,
                h: r.h as f32,
                label: r.label.as_str().into(),
                value: r.value.as_str().into(),
                selectable: r.selectable,
                in_card: r.in_card,
                toggle: r.toggle,
                dim: r.dim,
                nudge: r.nudge,
            })
            .collect::<Vec<_>>(),
    ))
}

fn place(screen: &Root, layout: &wifi::Layout) {
    screen.set_wifi_modal_x(layout.frame_x as f32);
    screen.set_wifi_modal_w(layout.frame_w as f32);
    screen.set_wifi_modal_y(layout.frame_y as f32);
    screen.set_wifi_modal_h(layout.frame_h as f32);
    screen.set_wifi_modal_inner_top(layout.inner_top as f32);
    screen.set_wifi_modal_inner_h(layout.inner_h as f32);
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

/// A live connection, as nmcli reports one.
fn details() -> Details {
    Details {
        name: "Flipper Lab".into(),
        autoconnect: true,
        password: "correct-horse".into(),
        ipv4: Family {
            method: "auto".into(),
            gateway: "192.168.1.1".into(),
            dns: "192.168.1.1,1.1.1.1".into(),
            addresses: vec!["192.168.1.110/24".into()],
        },
        ipv6: Family {
            method: "auto".into(),
            gateway: String::new(),
            dns: String::new(),
            addresses: vec!["fe80::a00:27ff:fe4e:66a1/64".into()],
        },
        known: true,
    }
}

fn networks() -> Vec<Network> {
    [
        ("Flipper Lab", 88, "WPA2"),
        ("A network with a very long name", 71, "WPA2 WPA3"),
        ("Guest", 54, ""),
        ("Cafe 5G", 33, "WPA2 802.1X"),
        ("Far away", 11, "WEP"),
    ]
    .into_iter()
    .map(|(ssid, signal, security)| Network {
        ssid: ssid.into(),
        signal,
        security: security.into(),
    })
    .collect()
}

/// Everything the screen can be, in one process: Slint's platform is global and
/// can only be installed once.
#[test]
fn the_wifi_screens_hold_their_geometry() {
    let window = FlipperSlintPlatform::install();
    let screen = Root::new().expect("create Root");
    screen.show().expect("show");
    // Fixed readings, so a golden does not move with the host's own battery.
    screen.set_battery(87);
    screen.set_wifi_connected(true);
    screen.set_wifi_quality(88);
    screen.set_breadcrumb("> Network > Wi-Fi".into());
    screen.set_screen(Screen::Wifi);

    // ── The page, joined to a network ──────────────────────────────────────
    let joined = flipper_ui::net::Net {
        airplane: false,
        wifi_enabled: true,
        wifi_connected: true,
        ssid: "Flipper Lab".into(),
    };
    let page = wifi::page_rows(&joined);
    screen.set_wifi_rows(rows(&page));
    // The connected row, which is the one that drills in and so the one that
    // shows the chevron bar.
    screen.set_wifi_selected(2);
    support::assert_golden("wifi-page", &grab(&window));

    // The radio off, which hides every row that would be a lie while it is.
    let off = flipper_ui::net::Net::default();
    screen.set_wifi_rows(rows(&wifi::page_rows(&off)));
    screen.set_wifi_selected(0);
    screen.set_wifi_connected(false);
    support::assert_golden("wifi-page-off", &grab(&window));

    // ── The visible networks ───────────────────────────────────────────────
    screen.set_wifi_rows(rows(&page));
    screen.set_wifi_selected(3);
    screen.set_wifi_connected(true);
    let nets = networks();
    let layout = wifi::visible_layout(true, nets.len() as i32);
    let needs_scroll = nets.len() as i32 > layout.visible;
    screen.set_wifi_net_rows(net_rows(&wifi::visible_rows(
        &nets,
        layout.row_w(needs_scroll),
        layout.visible - 1,
    )));
    screen.set_wifi_modal_tab(wifi::VISIBLE_TAB.into());
    screen.set_wifi_modal_signal(true);
    screen.set_wifi_modal_selected(1);
    screen.set_wifi_modal_bar_total(nets.len() as i32);
    screen.set_wifi_modal_bar_visible(layout.visible);
    place(&screen, &layout);
    screen.set_wifi_overlay(1);
    support::assert_golden("wifi-visible", &grab(&window));

    // The row being pressed, which fills and inverts and drops the selector.
    screen.set_wifi_modal_pressed(1);
    support::assert_golden("wifi-visible-pressed", &grab(&window));
    screen.set_wifi_modal_pressed(-1);

    // Waiting on the first sweep, and the pill it puts up when there is nothing
    // to do with the row that was pressed.
    let waiting = wifi::visible_layout(false, 0);
    screen.set_wifi_net_rows(net_rows(&[]));
    screen.set_wifi_modal_loading(true);
    screen.set_wifi_modal_bar_total(0);
    place(&screen, &waiting);
    support::assert_golden("wifi-scanning", &grab(&window));
    screen.set_wifi_modal_loading(false);

    // ── The saved profiles ─────────────────────────────────────────────────
    let saved: Vec<Saved> = ["Flipper Lab", "Office WiFi", "Home"]
        .into_iter()
        .map(|name| Saved {
            name: name.into(),
            ssid: name.into(),
            ..Saved::default()
        })
        .collect();
    let names = wifi::saved_names(&saved);
    let layout = wifi::saved_layout(&names);
    let needs_scroll = wifi::saved_content_h(names.len() as i32) > layout.inner_h;
    screen.set_wifi_net_rows(net_rows(&wifi::saved_rows(
        &names,
        layout.row_w(needs_scroll),
    )));
    screen.set_wifi_modal_tab(wifi::SAVED_TAB.into());
    screen.set_wifi_modal_signal(false);
    screen.set_wifi_modal_chevron(true);
    screen.set_wifi_modal_selected(1);
    screen.set_wifi_modal_bar_total(wifi::saved_content_h(names.len() as i32));
    screen.set_wifi_modal_bar_visible(layout.inner_h);
    place(&screen, &layout);
    support::assert_golden("wifi-saved", &grab(&window));

    // ── One network's settings ─────────────────────────────────────────────
    let live = details();
    let mut view = wifi::detail_rows(Some(&live), false, true, false);
    let layout = wifi::detail_layout(view.content_h);
    let needs_scroll = view.content_h > layout.inner_h;
    wifi::fit_password(&mut view.rows, layout.row_w(needs_scroll));
    screen.set_wifi_detail_rows(detail_rows(&view.rows));
    screen.set_wifi_modal_tab("Flipper Lab".into());
    screen.set_wifi_modal_selected(0);
    screen.set_wifi_modal_bar_total(view.content_h);
    screen.set_wifi_modal_bar_visible(layout.inner_h);
    screen.set_wifi_modal_offset(0.0);
    place(&screen, &layout);
    screen.set_wifi_overlay(2);
    support::assert_golden("wifi-details", &grab(&window));

    // The passphrase row, whose selector is given more air than the rest, and the
    // IPv4 card, which takes the chevron bar because the whole card is the target.
    let pw = view.rows.iter().position(|r| r.kind == 2).expect("a passphrase row");
    screen.set_wifi_modal_selected(pw as i32);
    support::assert_golden("wifi-details-password", &grab(&window));

    let card = view.rows.iter().position(|r| r.kind == 7).expect("a card");
    let row = &view.rows[card];
    // Scrolled to it, since the settings are taller than the frame.
    let offset = wifi::ensure_visible(row.y, row.h, layout.inner_h, view.content_h, 0);
    screen.set_wifi_modal_selected(card as i32);
    screen.set_wifi_modal_offset(offset as f32);
    support::assert_golden("wifi-details-card", &grab(&window));

    // Waiting on the read, with the spinner over the body.
    let mut waiting = wifi::detail_rows(None, true, false, false);
    let layout = wifi::detail_layout(waiting.content_h);
    wifi::fit_password(&mut waiting.rows, layout.row_w(false));
    screen.set_wifi_detail_rows(detail_rows(&waiting.rows));
    screen.set_wifi_modal_selected(0);
    screen.set_wifi_modal_offset(0.0);
    screen.set_wifi_modal_bar_total(waiting.content_h);
    screen.set_wifi_modal_bar_visible(layout.inner_h);
    place(&screen, &layout);
    support::assert_golden("wifi-details-loading", &grab(&window));
}

/// Every pixel of every Wi-Fi screen must hold a design-token value, or the
/// scrim's own composite of one.
///
/// The same invariant `render.rs` pins for the menu, applied to the screens this
/// port adds, with two exemptions and both are the ones it already makes:
///
///   * The signal sprites, for the reason the menu's icons are exempt: they are
///     6-bit greyscale so their edges survive, and snapping them to tokens was
///     tried and reverted.
///   * A modal's scrim, which is 75% white over whatever was behind it. The
///     prototype dims the whole canvas including the status bar, so the dimmed
///     tones are unavoidable -- but they are not a free pass: only the exact
///     composite of a token is allowed, which still catches an antialiased corner
///     or a font that fell back.
#[test]
fn wifi_frames_use_only_design_tokens() {
    let tokens: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
    // What the scrim makes of each of them.
    let dimmed = |over: u8| {
        (f32::from(theme::color::OVERLAY.0) * theme::alpha::OVERLAY
            + f32::from(over) * (1.0 - theme::alpha::OVERLAY))
            .round() as u8
    };
    let allowed: Vec<u8> = tokens
        .iter()
        .copied()
        .chain(tokens.iter().copied().map(dimmed))
        .collect();

    let window = FlipperSlintPlatform::install();
    let screen = Root::new().expect("create Root");
    screen.show().expect("show");
    screen.set_battery(87);
    screen.set_breadcrumb("> Network > Wi-Fi".into());
    screen.set_screen(Screen::Wifi);

    let joined = flipper_ui::net::Net {
        airplane: false,
        wifi_enabled: true,
        wifi_connected: true,
        ssid: "Flipper Lab".into(),
    };
    screen.set_wifi_rows(rows(&wifi::page_rows(&joined)));
    screen.set_wifi_selected(2);

    let nets = networks();
    let list = wifi::visible_layout(true, nets.len() as i32);
    let needs_scroll = nets.len() as i32 > list.visible;
    let live = details();
    let mut settings = wifi::detail_rows(Some(&live), false, true, false);
    let page = wifi::detail_layout(settings.content_h);
    wifi::fit_password(&mut settings.rows, page.row_w(true));

    for overlay in [0, 1, 2] {
        match overlay {
            1 => {
                screen.set_wifi_net_rows(net_rows(&wifi::visible_rows(
                    &nets,
                    list.row_w(needs_scroll),
                    list.visible - 1,
                )));
                screen.set_wifi_modal_tab(wifi::VISIBLE_TAB.into());
                screen.set_wifi_modal_signal(true);
                screen.set_wifi_modal_selected(1);
                screen.set_wifi_modal_bar_total(nets.len() as i32);
                screen.set_wifi_modal_bar_visible(list.visible);
                place(&screen, &list);
            }
            2 => {
                screen.set_wifi_detail_rows(detail_rows(&settings.rows));
                screen.set_wifi_modal_tab("Flipper Lab".into());
                screen.set_wifi_modal_selected(0);
                screen.set_wifi_modal_bar_total(settings.content_h);
                screen.set_wifi_modal_bar_visible(page.inner_h);
                place(&screen, &page);
            }
            _ => {}
        }
        screen.set_wifi_overlay(overlay);
        slint::platform::update_timers_and_animations();
        let frame = render_frame(&window).expect("a state change always needs painting");

        let stride = usize::from(theme::PANEL_W);
        // The status bar's own clusters, and the signal sprite on each list row.
        let mut sprite_boxes: Vec<(i32, i32, i32, i32)> = vec![
            (0, 0, 48, theme::metric::STATUS_BAR_H),
            (
                i32::from(theme::PANEL_W) - 20,
                0,
                20,
                theme::metric::STATUS_BAR_H,
            ),
        ];
        if overlay == 1 {
            let row_w = list.row_w(needs_scroll);
            for i in 0..list.visible {
                sprite_boxes.push((
                    list.frame_x + theme::metric::WIFI_INNER_PAD + row_w - 7
                        - theme::metric::WIFI_ROW_PAD_R,
                    list.inner_top + i * theme::metric::WIFI_ROW_PITCH,
                    7,
                    theme::metric::WIFI_ROW_H,
                ));
            }
        }
        let in_sprite_box = |x: i32, y: i32| {
            sprite_boxes
                .iter()
                .any(|(bx, by, bw, bh)| x >= *bx && x < bx + bw && y >= *by && y < by + bh)
        };

        let offenders: Vec<(usize, u8)> = frame
            .iter()
            .enumerate()
            .filter(|(i, px)| {
                let (x, y) = ((i % stride) as i32, (i / stride) as i32);
                !in_sprite_box(x, y) && !allowed.contains(&px.0)
            })
            .map(|(i, px)| (i, px.0))
            .collect();

        assert!(
            offenders.is_empty(),
            "overlay {overlay}: {} pixels are not a design token; first at {:?}",
            offenders.len(),
            offenders
                .iter()
                .take(8)
                .map(|(i, v)| format!("({}, {}) = {v}", i % stride, i / stride))
                .collect::<Vec<_>>(),
        );
    }
}
