//! Screen Recorder: the panel to an MP4 in ~/Videos, with the speaker's sound, the
//! microphone or none.
//!
//! Start asks flipctl for the panel's frames (session.rs), and flipctl answers by
//! sending the app to the background, so what is recorded is whatever is used next.
//! Opening the app again holds the recording before the app is drawn, and that time is
//! left out of the file: Stop ends it there, Back carries on. Everything that touches a frame or a sample is in this
//! binary: video.rs encodes with OpenH264, mp3.rs with LAME, mp4.rs writes the file
//! and sound.rs reads the sound server.

mod mp3;
mod mp4;
mod pulse;
mod session;
mod sound;
mod video;

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use flipctl_app::{choice, pair, text, App, Key, Page, Ui};

use session::Session;
use sound::{Sound, SOUNDS};

enum State {
    Idle,
    Starting(Receiver<Result<Session, String>>),
    Recording { session: Session, stopping: bool },
    Saved(PathBuf),
    Failed(String),
}

struct Recorder {
    /// Which of [`SOUNDS`] goes with the picture.
    sound: usize,
    state: State,
}

impl Recorder {
    fn busy(&self) -> bool {
        matches!(self.state, State::Starting(_) | State::Recording { .. })
    }

    fn start(&mut self) {
        let sound = SOUNDS[self.sound];
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let started = videos()
                .and_then(|dir| std::fs::create_dir_all(&dir).map(|()| dir))
                .map_err(|e| format!("No Videos folder: {e}"))
                .and_then(|dir| Session::start(sound, dir.join(file_name())));
            let _ = tx.send(started);
            flipctl_app::redraw();
        });
        self.state = State::Starting(rx);
    }
}

impl App for Recorder {
    fn page(&self) -> Page {
        let last = SOUNDS.len() - 1;
        let label = SOUNDS[self.sound].label();
        let mut rows = vec![if self.busy() {
            pair("Sound", label)
        } else {
            choice("Sound", label, self.sound == 0, self.sound == last)
        }];
        match &self.state {
            State::Idle | State::Starting(_) => {}
            State::Recording { stopping: true, .. } => rows.push(text("Saving..")),
            State::Recording { session, .. } => {
                rows.push(pair("Recording", clock(session.recorded())));
                for line in lines(PAUSED, NOTE_W) {
                    rows.push(text(line).dim());
                }
            }
            State::Saved(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
                rows.push(text(format!("Saved: {name}")));
            }
            State::Failed(why) => rows.push(text(why.clone())),
        }
        Page::detail(rows)
    }

    fn buttons(&self) -> [&str; 5] {
        let last = match self.state {
            State::Starting(_) | State::Recording { stopping: true, .. } => "",
            State::Recording { .. } => "Stop",
            _ => "Start",
        };
        ["Close", "", "", "", last]
    }

    fn key(&mut self, key: Key, _: &mut Ui) -> bool {
        if let State::Recording { session, stopping } = &mut self.state {
            if key != Key::Run || *stopping {
                return false;
            }
            session.stop();
            *stopping = true;
            return true;
        }
        if self.busy() {
            return false;
        }
        match key {
            Key::Left => self.sound = self.sound.saturating_sub(1),
            Key::Right => self.sound = (self.sound + 1).min(SOUNDS.len() - 1),
            Key::Run => self.start(),
            _ => return false,
        }
        true
    }

    /// Always, since the framework asks once, at the start: the tick is what moves
    /// Start on to Recording, and Recording on to Saved.
    fn every(&self) -> Option<Duration> {
        Some(Duration::from_millis(500))
    }

    fn tick(&mut self, _: &mut Ui) {
        let next = match &self.state {
            State::Starting(rx) => match rx.try_recv() {
                Ok(Ok(session)) => Some(State::Recording { session, stopping: false }),
                Ok(Err(why)) => Some(State::Failed(why)),
                Err(_) => None,
            },
            State::Recording { session, .. } => session.done().map(|done| match done {
                Ok(path) => State::Saved(path),
                Err(why) => State::Failed(why),
            }),
            _ => None,
        };
        if let Some(next) = next {
            self.state = next;
        }
    }

    /// A recording still running when the app closes is finished, not abandoned.
    fn close(&mut self) {
        let session = match std::mem::replace(&mut self.state, State::Idle) {
            State::Recording { session, .. } => Some(session),
            State::Starting(rx) => {
                rx.recv_timeout(Duration::from_secs(5)).ok().and_then(Result::ok)
            }
            _ => None,
        };
        if let Some(session) = session {
            session.finish();
        }
    }
}

/// What the page says while a recording is held for it.
const PAUSED: &str = "Recording is paused while this app is shown";

/// How wide a note may run: the panel less the rows' margins.
const NOTE_W: u16 = flipctl_app::theme::PANEL_W - 16;

/// `text` broken at spaces into lines no wider than `budget` in the rows' font.
/// The framework's `wrap` measures in the title font, which is narrower.
fn lines(text: &str, budget: u16) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match out.last_mut() {
            Some(line)
                if flipctl_app::font::ROW.text_width(&format!("{line} {word}")) <= budget =>
            {
                line.push(' ');
                line.push_str(word);
            }
            _ => out.push(word.to_string()),
        }
    }
    out
}

/// A length as the page shows it: `0:42`, `12:05`.
fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// Where recordings go.
fn videos() -> std::io::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(|home| PathBuf::from(home).join("Videos"))
        .ok_or_else(|| std::io::Error::other("no HOME"))
}

/// `Screen_2026-11-25_15-59-10.mp4`, in local time.
fn file_name() -> String {
    // SAFETY: time() with a null pointer only returns; localtime_r writes into `tm`.
    let tm = unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&now, &mut tm);
        tm
    };
    name_for(&tm)
}

fn name_for(tm: &libc::tm) -> String {
    format!(
        "Screen_{:04}-{:02}-{:02}_{:02}-{:02}-{:02}.mp4",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

/// Which of [`SOUNDS`] a new recording starts with: the microphone, for a voice over
/// what is shown.
const DEFAULT_SOUND: Sound = Sound::Mic;

flipctl_app::main!(Recorder {
    sound: SOUNDS.iter().position(|s| *s == DEFAULT_SOUND).unwrap_or(0),
    state: State::Idle,
});

#[cfg(test)]
mod tests {
    use super::*;
    use flipctl_app::{theme, Status};

    #[test]
    fn the_note_fits_the_panel() {
        let lines = lines(PAUSED, NOTE_W);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines.join(" "), PAUSED);
    }

    #[test]
    fn a_length_is_minutes_and_seconds() {
        assert_eq!(clock(Duration::from_millis(42_900)), "0:42");
        assert_eq!(clock(Duration::from_secs(725)), "12:05");
    }

    #[test]
    fn a_file_is_named_for_when_it_was_made() {
        // SAFETY: an all-zero tm is a valid value to fill in.
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        (tm.tm_year, tm.tm_mon, tm.tm_mday) = (126, 10, 25);
        (tm.tm_hour, tm.tm_min, tm.tm_sec) = (15, 59, 10);
        assert_eq!(name_for(&tm), "Screen_2026-11-25_15-59-10.mp4");
    }

    #[test]
    fn left_and_right_choose_the_sound_and_stop_at_the_ends() {
        let mut app = Recorder { sound: 0, state: State::Idle };
        let mut ui = Ui::default();
        app.key(Key::Left, &mut ui);
        assert_eq!(app.sound, 0);
        for _ in 0..5 {
            app.key(Key::Right, &mut ui);
        }
        assert_eq!(SOUNDS[app.sound].label(), "System + Mic");
    }

    #[test]
    fn the_pages_are_drawn_only_in_tokens() {
        let saved = State::Saved(PathBuf::from("/home/user/Videos/Screen_2026-11-25_15-59-10.mp4"));
        let recording =
            State::Recording { session: Session::idle(Duration::from_secs(42)), stopping: false };
        for (name, state) in [("saved", saved), ("recording", recording)] {
            shows_only_tokens(name, Recorder { sound: 3, state });
        }
    }

    /// The page, saved to target/render when `SCREEN_RENDER=1`, with every pixel a
    /// design-token value.
    fn shows_only_tokens(name: &str, app: Recorder) {
        let frame = flipctl_pages::shot(app, Status::default());
        if std::env::var_os("SCREEN_RENDER").is_some() {
            let path = format!("target/render/{name}.png");
            flipctl_app::render::save(std::path::Path::new(&path), &frame);
        }
        let allowed: Vec<u8> = theme::ALL_COLORS.iter().map(|(_, _, r, ..)| *r).collect();
        let stride = usize::from(theme::PANEL_W);
        let offenders: Vec<String> = frame
            .iter()
            .enumerate()
            .filter(|(_, px)| !allowed.contains(px))
            .take(8)
            .map(|(i, px)| format!("({}, {}) = {px}", i % stride, i / stride))
            .collect();
        assert!(offenders.is_empty(), "off-palette pixels: {offenders:?}");
    }
}
