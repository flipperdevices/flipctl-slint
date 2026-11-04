//! The pages drawn headlessly and read back. `RECORDER_RENDER=1 cargo test` leaves
//! the frames in target/render to be looked at.

use super::*;
use flipctl_app::dropdown;
use flipctl_app::theme::{self, metric};
use flipper_ui::slint_render::{render_frame, FlipperSlintPlatform};
use slint::platform::software_renderer::MinimalSoftwareWindow;

fn panel() -> Rc<MinimalSoftwareWindow> {
    thread_local! {
        static WINDOW: Rc<MinimalSoftwareWindow> = FlipperSlintPlatform::install();
    }
    WINDOW.with(Rc::clone)
}

fn shot(name: &str, state: &Recorder) -> Vec<u8> {
    let window = panel();
    let ui = AppWindow::new().expect("create AppWindow");
    apply(&ui, state);
    ui.show().expect("show");
    slint::platform::update_timers_and_animations();
    let frame: Vec<u8> = render_frame(&window).expect("frame").iter().map(|px| px.0).collect();
    drop(ui);
    if std::env::var_os("RECORDER_RENDER").is_some() {
        save(name, &frame);
    }
    frame
}

fn save(name: &str, frame: &[u8]) {
    let dir = std::path::Path::new("target/render");
    std::fs::create_dir_all(dir).expect("create target/render");
    let file = std::fs::File::create(dir.join(format!("{name}.png"))).expect("create png");
    let mut encoder =
        png::Encoder::new(std::io::BufWriter::new(file), u32::from(theme::PANEL_W), u32::from(theme::PANEL_H));
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().expect("png header").write_image_data(frame).expect("png data");
}

fn at(frame: &[u8], x: i32, y: i32) -> u8 {
    frame[(y * i32::from(theme::PANEL_W) + x) as usize]
}

/// A folder of its own per test, with the takes named in it.
fn folder(test: &str, takes: &[&str]) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("recorder-ui-{}-{test}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (i, name) in takes.iter().enumerate() {
        let mut bytes = length::wav_header(96_000 * (i as u32 + 1) * 7, capture::RATE, 1).to_vec();
        bytes.resize(bytes.len() + 96_000 * (i + 1) * 7, 0);
        std::fs::write(dir.join(name), bytes).unwrap();
    }
    dir
}

/// A state that owes nothing to the machine: the codec answered, the gain known.
fn recorder(test: &str, takes: &[&str]) -> Recorder {
    let dir = folder(test, takes);
    Recorder {
        asking: None,
        codec: Some(true),
        gain: Some(40),
        next: files::next_name(&dir),
        dir,
        level: 0.5,
        outputs: vec![
            ("alsa_output.platform-sound.stereo-fallback".into(), "Speaker".into()),
            ("alsa_output.platform-hdmi-sound.HiFi__HDMI__sink".into(), "HDMI".into()),
            ("alsa_output.platform-dp0-sound.stereo-fallback".into(), "DisplayPort".into()),
        ],
        started: Instant::now() - Duration::from_millis(1234),
        ..Recorder::new()
    }
}

fn recording(state: &mut Recorder) {
    state.take = Some(Take {
        name: "F1-REC-004".into(),
        path: state.dir.join("F1-REC-004.wav"),
        format: Format::Wav,
    });
    state.elapsed = Duration::from_millis(83_450);
    state.free = Some(3 * 1024 * 1024 * 1024);
}

/// Invariant 1, with the two exemptions the panel's own tests make: a sprite's box,
/// whose edges are greys on purpose, and a scrim, which is a token at 75% white.
fn only_tokens(name: &str, frame: &[u8], state: &Recorder) {
    let tokens: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
    let scrimmed: Vec<u8> = tokens.iter().map(|c| ((u16::from(*c) + 3 * 255 + 2) / 4) as u8).collect();
    let mut boxes: Vec<(i32, i32, i32)> =
        state.screen(Duration::ZERO).sprites.iter().map(|s| (s.x, s.y, 10)).collect();
    if state.page == Page::Main {
        let icon = metric::APP_BAR_ICON_W;
        boxes.push((metric::APP_BAR_ICON_X, metric::STATUS_BAR_H + metric::APP_BAR_ICON_DY, icon));
    }
    let stride = i32::from(theme::PANEL_W);
    let offenders: Vec<String> = frame
        .iter()
        .enumerate()
        .map(|(i, px)| (i as i32 % stride, i as i32 / stride, *px))
        .filter(|(x, y, _)| !boxes.iter().any(|(bx, by, n)| (*bx..bx + n).contains(x) && (*by..by + n).contains(y)))
        .filter(|(_, _, px)| !tokens.contains(px) && !scrimmed.iter().any(|s| s.abs_diff(*px) <= 1))
        .take(8)
        .map(|(x, y, px)| format!("({x}, {y}) = {px}"))
        .collect();
    assert!(offenders.is_empty(), "{name}: off-palette pixels: {offenders:?}");
}

#[test]
fn every_page_is_drawn_only_in_tokens() {
    let mut state = recorder("tokens", &["F1-REC-001.wav", "F1-REC-002.wav", "F1-REC-003.wav"]);
    let check = |name: &str, state: &Recorder| only_tokens(name, &shot(name, state), state);
    check("idle", &state);
    state.selected = 4;
    state.picking = Some(1);
    check("format-picker", &state);
    state.picking = None;
    recording(&mut state);
    check("recording", &state);
    state.paused = true;
    check("paused", &state);
    state.take = None;
    state.paused = false;
    state.open_files();
    check("files", &state);
    state.overlay = Overlay::Edit { selected: 1 };
    check("edit", &state);
    state.overlay = Overlay::Delete { name: "F1-REC-002.wav".into(), selected: 0 };
    check("delete", &state);
    state.overlay = Overlay::Player(Playback {
        name: "F1-REC-002.wav".into(),
        player: None,
        failed: false,
        position: Duration::from_secs(5),
        length: Some(Duration::from_secs(14)),
        playing: true,
        ended: false,
        flash: Some((1, 10)),
        flashed: None,
        volume: 60,
    });
    check("player", &state);
    let mut empty = recorder("empty", &[]);
    empty.open_files();
    check("files-empty", &empty);
    empty.page = Page::Main;
    empty.error = Some(vec!["Recording error".into(), "Microphone not found".into()]);
    check("error", &empty);
}

/// The meter is the chip's width, outlined, and black as far as the level reaches.
#[test]
fn the_meter_fills_to_the_level() {
    let state = recorder("meter", &[]);
    let frame = shot("meter", &state);
    let y = 33 + 3;
    let half = dropdown::CHIP_X + metric::DROP_CHIP_W / 2;
    assert_eq!(at(&frame, half - 3, y), theme::color::BLACK.0);
    assert_eq!(at(&frame, half + 3, y), theme::color::WHITE.0);
    assert_eq!(at(&frame, dropdown::CHIP_X + metric::DROP_CHIP_W - 1, y), theme::color::BLACK.0, "outline");
}

/// While a take runs only the gain row is left, and the panel is black under it.
#[test]
fn a_take_replaces_the_rows_with_the_panel() {
    let mut state = recorder("panel", &[]);
    state.selected = 4;
    recording(&mut state);
    state.selected = 0;
    let frame = shot("panel", &state);
    assert_eq!(state.shown_rows(), [Row::Gain]);
    assert_eq!(at(&frame, 128, 120), theme::color::BLACK.0, "the panel");
    assert_eq!(at(&frame, 128, 83), theme::color::WHITE.0, "the rule under the name");
    assert_eq!(state.screen(Duration::ZERO).buttons, ["Close", "Files", "", "Pause", "Stop"]);
}

#[test]
fn up_and_down_skip_the_rows_that_are_not_choices() {
    let mut state = recorder("rows", &[]);
    state.key(Key::Down);
    assert_eq!(ROWS[state.selected], Row::Input);
    state.key(Key::Down);
    assert_eq!(ROWS[state.selected], Row::Output);
    state.key(Key::Down);
    assert_eq!(ROWS[state.selected], Row::Format, "File name is skipped");
    state.key(Key::Down);
    assert_eq!(ROWS[state.selected], Row::Gain);
    state.key(Key::Up);
    assert_eq!(ROWS[state.selected], Row::Format);
    state.key(Key::Right);
    assert_eq!(FORMATS[state.format], Format::Mp3);
    state.key(Key::Ok);
    assert_eq!(state.picking, Some(1));
    state.key(Key::Down);
    state.key(Key::Ok);
    assert_eq!(FORMATS[state.format], Format::Ogg);
}

#[test]
fn deleting_asks_first_and_cancel_is_the_default() {
    let mut state = recorder("delete", &["F1-REC-001.wav", "F1-REC-002.wav"]);
    state.open_files();
    state.key(Key::Down);
    state.key(Key::Edit);
    state.key(Key::Ok);
    assert!(matches!(&state.overlay, Overlay::Delete { name, selected: 0 } if name == "F1-REC-002.wav"));
    state.key(Key::Ok);
    assert_eq!(state.files.len(), 2, "Cancel deleted nothing");
    state.key(Key::Edit);
    state.key(Key::Ok);
    state.key(Key::Down);
    state.key(Key::Ok);
    assert_eq!(state.files.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["F1-REC-001.wav"]);
    assert_eq!(state.next, "F1-REC-002", "the highest number is free again");
}

#[test]
fn sort_orders_the_list_and_keeps_the_selection() {
    let mut state = recorder("sort", &["F1-REC-001.wav", "F1-REC-002.wav", "F1-REC-003.wav"]);
    state.open_files();
    state.key(Key::Down);
    state.key(Key::Edit);
    state.key(Key::Down);
    state.key(Key::Right);
    assert_eq!(state.files[0].name, "F1-REC-003.wav", "Z-A");
    assert_eq!(state.files[state.file_sel].name, "F1-REC-002.wav");
    state.key(Key::Right);
    assert_eq!(state.files[0].name, "F1-REC-003.wav", "longest: 21s");
    state.key(Key::Escape);
    assert!(matches!(state.overlay, Overlay::None));
}

/// Output device is the row under Input device: Left and Right cycle it, Ok opens
/// its picker, and a take then plays on that sink.
#[test]
fn the_output_row_chooses_the_sink() {
    let mut state = recorder("output", &[]);
    state.key(Key::Down);
    state.key(Key::Down);
    assert_eq!(ROWS[state.selected], Row::Output, "under Input device");
    state.key(Key::Right);
    assert_eq!(state.output_label(), "HDMI");
    state.key(Key::Ok);
    assert_eq!(state.picking, Some(1));
    shot("output-picker", &state);
    state.key(Key::Down);
    state.key(Key::Ok);
    assert_eq!(state.sink(), "alsa_output.platform-dp0-sound.stereo-fallback");
    shot("output", &state);
    state.key(Key::Right);
    assert_eq!(state.output_label(), "Speaker", "round to the first");
}
