//! The terminal front end.
//!
//! The second way to put a view model on a screen, beside `slint_render`. Nothing
//! here decides anything: keys leave through a channel to whichever loop owns the
//! state machine, snapshots come back the same way, and the panel is driven from the
//! same loop, so the two screens cannot disagree.
//!
//! Cursive runs its own event loop, and it wants a thread. The caller keeps its own
//! loop and talks to this one through `poll_event` and `show`, both of which are
//! non-blocking: a terminal that is slow, or absent, must not pace the panel.
//!
//! Three things a serial console needs that a desktop terminal does not:
//!
//! - No controlling terminal. Cursive's default backend opens `/dev/tty`, which an
//!   init-spawned process may not have; this hands crossterm a dup of stdout
//!   instead, and crossterm reads keys from stdin when stdin is a tty.
//! - A window size. A serial line reports 0x0, which lays every widget out at
//!   nothing, so an unset size is filled in here.
//! - Quiet from the kernel. The console loglevel is turned down while the screen is
//!   up, because printk writes to the same UART and does not know about raw mode.

pub mod boot;
pub mod text;

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::os::fd::FromRawFd;
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;

use cursive::reexports::crossbeam_channel::{bounded, unbounded, Receiver, Sender};
use cursive::view::Nameable;
use cursive::{CbSink, Cursive};

use crate::boot_menu::View;
use crate::key::KeyEvent;

/// What the terminal has to say to the loop that owns the state.
pub enum TerminalEvent {
    /// A press and, right behind it, its release.
    Key(KeyEvent),
    /// The rename dialog closed: the name typed, or nothing if it was cancelled.
    Renamed(Option<String>),
    /// Somebody asked for a shell.
    Quit,
    /// The event loop ended, so nothing more will arrive.
    Closed,
}

/// What the terminal is set to, so it can be put back.
///
/// Restored on drop, and again from the panic hook: `panic = "abort"` runs hooks but
/// no destructors, and a terminal left in raw mode with the alternate screen up is
/// one nobody can read the panic on.
struct Termios(libc::termios);

/// Everything the reset sequence has to undo: the alternate screen, a hidden cursor,
/// and mouse reporting, which crossterm turns on and a dead process never turns off.
const RESET: &str = "\x1b[?1049l\x1b[?25h\x1b[?1000l\x1b[?1006l";

static SAVED: Mutex<Option<Termios>> = Mutex::new(None);
static HOOK: Once = Once::new();

unsafe impl Send for Termios {}

fn save_termios() {
    let mut raw: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut raw) } == 0 {
        *SAVED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Termios(raw));
    }
}

fn restore_termios() {
    let saved = SAVED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Termios(raw)) = saved.as_ref() {
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, raw) };
    }
}

fn install_panic_hook() {
    HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_termios();
            let mut out = std::io::stdout();
            let _ = out.write_all(RESET.as_bytes());
            let _ = out.flush();
            previous(info);
        }));
    });
}

/// Give the terminal a size when the kernel does not know one.
///
/// A serial line carries no window size, so `TIOCGWINSZ` answers 0x0 and every widget
/// lays out at nothing. The terminal at the far end does know, though, and will say if
/// asked: park the cursor past the bottom right, where it sticks at the last cell, and
/// send a cursor position report. What comes back is the size.
///
/// Only useful while somebody is attached to hand the answer back. Nobody there means
/// the read times out and 24x80 stands, which is what the installer's launcher writes
/// with stty on these same consoles. Attach before the menu starts to be asked.
fn ensure_winsize() -> (u16, u16, &'static str) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let known = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0
        && ws.ws_row != 0
        && ws.ws_col != 0;

    // Asked even when the kernel claims to know, because on a serial line it does
    // not: nothing ever told it, so whatever it reports is a default that happens to
    // be a plausible size. Trusting it drew a 24-row screen into a 19-row terminal.
    // The terminal at the far end is the only thing that actually knows.
    let (rows, cols, how) = match ask_the_terminal() {
        Some((rows, cols)) => (rows, cols, "asked"),
        None if known => (ws.ws_row, ws.ws_col, "from the kernel"),
        None => (24, 80, "nobody answered, assuming"),
    };
    ws.ws_row = rows;
    ws.ws_col = cols;
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSWINSZ, &ws) };
    (rows, cols, how)
}

/// Ask the far end for its size with a cursor position report.
///
/// Raw mode for the duration, because the reply has no newline in it and a cooked
/// terminal would sit on it for ever. `VTIME` is the whole timeout: two tenths is long
/// enough for a reply at any speed this console runs at, and short enough that a boot
/// with nobody watching does not notice the wait.
fn ask_the_terminal() -> Option<(u16, u16)> {
    use std::io::Read;

    let mut saved: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut saved) } != 0 {
        return None;
    }
    let mut raw = saved;
    unsafe { libc::cfmakeraw(&mut raw) };
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 2;
    if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } != 0 {
        return None;
    }

    // Save the cursor, drive it into the far corner, ask where that turned out to be,
    // put it back. A terminal clamps the move to its own last cell, which is the whole
    // trick: the answer is its size.
    let mut out = std::io::stdout();
    let asked = out
        .write_all(b"\x1b7\x1b[999;999H\x1b[6n\x1b8")
        .and_then(|()| out.flush())
        .is_ok();

    let mut reply = Vec::new();
    if asked {
        let mut buf = [0u8; 32];
        // Several short reads: the reply can arrive split across them, and the loop
        // ends on the R that terminates it or on the first read that times out.
        for _ in 0..8 {
            match std::io::stdin().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    reply.extend_from_slice(&buf[..n]);
                    if reply.contains(&b'R') {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    }
    unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &saved) };
    parse_size_report(&reply)
}

/// `ESC [ rows ; cols R`, out of whatever else is in the buffer.
fn parse_size_report(reply: &[u8]) -> Option<(u16, u16)> {
    let end = reply.iter().position(|&b| b == b'R')?;
    let start = reply[..end].iter().rposition(|&b| b == 0x1b)?;
    let body = std::str::from_utf8(&reply[start + 2..end]).ok()?;
    let (rows, cols) = body.split_once(';')?;
    let rows: u16 = rows.trim_start_matches('[').parse().ok()?;
    let cols: u16 = cols.parse().ok()?;
    // A terminal too small to lay anything out in, or a number that is plainly not a
    // size, is worse than the fallback.
    (rows >= 8 && cols >= 20 && rows <= 300 && cols <= 1000).then_some((rows, cols))
}

/// The console loglevel, turned down for as long as the screen is up.
///
/// printk writes straight to the UART driver, so raw mode does not hold it back and
/// a kernel message lands in the middle of whatever is drawn. Only the console level
/// changes: everything still reaches `/dev/kmsg`, which is where this crate logs and
/// where a boot is read back from afterwards.
struct Printk(Option<String>);

const PRINTK: &str = "/proc/sys/kernel/printk";

impl Printk {
    fn quiet() -> Self {
        let was = std::fs::read_to_string(PRINTK).ok();
        if was.is_some() && std::fs::write(PRINTK, "1\n").is_err() {
            crate::logline!("tui            could not quieten the console loglevel");
        }
        Self(was)
    }
}

impl Drop for Printk {
    fn drop(&mut self) {
        if let Some(was) = self.0.take() {
            let _ = std::fs::write(PRINTK, was);
        }
    }
}

/// Ask the terminal its size again, and tell the kernel what it said.
///
/// There is no resize notification on a serial line: no protocol carries one, so
/// nothing ever calls `TIOCSWINSZ` and no `SIGWINCH` is ever raised. A window that
/// changed size can only be noticed by asking, which is why this hangs off the key
/// that already means "put my screen right" rather than happening by itself.
///
/// Asked through crossterm rather than by hand, unlike the query at startup: its own
/// reader is running by now, and it is the one that will see the reply.
pub fn resync_size() -> Option<(u16, u16)> {
    use cursive::backends::crossterm::crossterm::{cursor, execute};

    let mut out = std::io::stdout();
    // Park the cursor past the far corner, where it stops at the last cell, ask where
    // that turned out to be, then put it back.
    execute!(out, cursor::SavePosition, cursor::MoveTo(9999, 9999)).ok()?;
    let reply = cursor::position().ok();
    let _ = execute!(out, cursor::RestorePosition);
    let (col, row) = reply?;
    let (cols, rows) = (col.saturating_add(1), row.saturating_add(1));
    if rows < 8 || cols < 20 {
        return None;
    }
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if ws.ws_row != rows || ws.ws_col != cols {
        ws.ws_row = rows;
        ws.ws_col = cols;
        unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCSWINSZ, &ws) };
        crate::logline!("tui            terminal is now {cols}x{rows}");
    }
    Some((rows, cols))
}

/// A running terminal front end.
///
/// Dropping it ends the event loop and puts the terminal back, so a caller that
/// returns early does not have to remember to.
pub struct Terminal {
    cb: CbSink,
    slot: Arc<Mutex<Option<View>>>,
    rx: Receiver<TerminalEvent>,
    tx: Sender<TerminalEvent>,
    thread: Option<JoinHandle<()>>,
    _printk: Printk,
}

impl Terminal {
    /// Take over stdin and stdout, if they are a terminal at all.
    ///
    /// Fails rather than draws when they are not, which is the ordinary case: run
    /// from a service, from a pipe, or from the boot menu image as it stands today,
    /// there is nobody to draw for and the caller carries on with the panel alone.
    pub fn open() -> std::io::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(std::io::Error::other("stdin and stdout are not a terminal"));
        }
        let (rows, cols, how) = ensure_winsize();
        save_termios();
        install_panic_hook();

        let (tx, rx) = unbounded::<TerminalEvent>();
        let (ready_tx, ready_rx) = bounded::<CbSink>(1);
        let keys = tx.clone();
        let thread = std::thread::Builder::new()
            .name("tui".into())
            .spawn(move || run(keys, ready_tx))?;

        // The loop is what owns the Cursive, so its sink comes back from in there.
        // A send that never arrives means the thread died before it started, which
        // is a bug here rather than a terminal that could not be opened.
        let cb = ready_rx
            .recv()
            .map_err(|_| std::io::Error::other("the terminal thread ended before it started"))?;

        // Said before the console goes quiet, because after that nothing this program
        // logs reaches the screen: the guard below is what stops printk writing over
        // the menu, and it cannot tell our lines from the kernel's. Everything after
        // this point is in dmesg, which the shell on Ctrl-C can read back.
        crate::logline!("tui            terminal {cols}x{rows} ({how}), front end up");
        let printk = Printk::quiet();

        Ok(Self {
            cb,
            slot: Arc::new(Mutex::new(None)),
            rx,
            tx,
            thread: Some(thread),
            _printk: printk,
        })
    }

    /// Non-blocking. `None` when nothing is queued.
    pub fn poll_event(&mut self) -> Option<TerminalEvent> {
        self.rx.try_recv().ok()
    }

    /// Put a snapshot on the screen.
    ///
    /// Coalescing: a callback is only queued when the slot was empty, so a spinner
    /// turning ten times a second leaves one redraw outstanding rather than ten. The
    /// caller can hand over every frame it considers dirty without thinking about it.
    pub fn show(&self, view: View) {
        let first = {
            let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
            let first = slot.is_none();
            *slot = Some(view);
            first
        };
        if !first {
            return;
        }
        let slot = Arc::clone(&self.slot);
        let _ = self.cb.send(Box::new(move |siv: &mut Cursive| {
            let taken = slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(view) = taken {
                siv.call_on_name(boot::NAME, |screen: &mut boot::BootScreen| screen.set(view));
            }
        }));
    }

    /// Ask for a name, seeded with the one the profile has.
    ///
    /// `rules` is what the panel checks the same name against, snapshotted: the
    /// dialog runs on its own thread and cannot reach back into the menu.
    pub fn prompt_rename(&self, title: &str, seed: &str, rules: text::Rules) {
        let (title, seed, tx) = (title.to_string(), seed.to_string(), self.tx.clone());
        let _ = self.cb.send(Box::new(move |siv: &mut Cursive| {
            siv.add_layer(text::rename_dialog(&title, &seed, rules, tx));
        }));
    }

    /// Take the dialog down without an answer, for a rename the panel's keyboard
    /// finished first.
    pub fn dismiss_prompt(&self) {
        let _ = self.cb.send(Box::new(|siv: &mut Cursive| {
            let up = siv
                .call_on_name(text::FIELD, |_: &mut cursive::views::EditView| ())
                .is_some();
            if up {
                siv.pop_layer();
            }
        }));
    }

    /// End the event loop and give the terminal back, in cooked mode with the
    /// alternate screen gone. Anything printed after this stays in the scrollback.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        let Some(thread) = self.thread.take() else {
            return;
        };
        let _ = self.cb.send(Box::new(|siv: &mut Cursive| siv.quit()));
        let _ = thread.join();
        restore_termios();
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The cursive thread: build the screen, hand the sink back, run until told to stop.
fn run(tx: Sender<TerminalEvent>, ready: Sender<CbSink>) {
    let mut siv = cursive::CursiveRunnable::new(|| {
        // A dup of stdout rather than /dev/tty: an init-spawned process need not have
        // a controlling terminal, and opening the one it does not have is how this
        // fails on a board where the launcher did not arrange one.
        let fd = unsafe { libc::dup(libc::STDOUT_FILENO) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        cursive::backends::crossterm::Backend::init_with_stdout_file(unsafe {
            File::from_raw_fd(fd)
        })
    });

    // The terminal's own colours, not cursive's blue: this shares a screen with a
    // kernel log and should look like the console it is on. Selection is Reverse,
    // which every terminal has.
    // Cursive's own default palette, which is what flipperos-installer's serial console
    // draws with: panes on a coloured ground, titles and the cursor picked out. Left as
    // the default rather than set to the terminal's two colours, which was the earlier
    // choice here and read as plain text beside the installer's screen.
    siv.set_theme(cursive::theme::Theme::default());
    // Cursive binds Ctrl-C to quit, as a pre-event, so the screen never sees it. Here
    // it has to: this console is the one the image used to put a root shell on, and
    // Ctrl-C is how that shell is asked for. Quitting the front end and leaving the
    // menu running blind on the panel is not a thing anybody wants from a key that
    // easy to hit. Window resize and Exit keep their defaults.
    siv.clear_global_callbacks(cursive::event::Event::CtrlChar('c'));
    siv.add_fullscreen_layer(boot::BootScreen::new(tx.clone()).with_name(boot::NAME));

    if ready.send(siv.cb_sink().clone()).is_err() {
        return;
    }
    if let Err(e) = siv.try_run() {
        crate::logline!("tui            {e}");
    }
    let _ = tx.send(TerminalEvent::Closed);
}
