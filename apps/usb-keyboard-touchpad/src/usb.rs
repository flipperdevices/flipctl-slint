//! The host over USB-C: flipctl's `hid` claim makes the device port a keyboard, a
//! mouse and media keys, and lends the app the `/dev/hidgN` they are written to.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver};

/// Where the claim says the device node is.
const DEV: &str = "/run/flipctl-hid.dev";

/// How long a report waits for the one before it to reach the host.
const WRITE_WAIT_MS: i32 = 100;

pub struct Usb {
    /// The claim, while it is being taken.
    taking: Option<Receiver<Result<UnixStream, String>>>,
    held: Option<(UnixStream, File)>,
    error: String,
}

impl Usb {
    /// Ask for the port. The claim takes a second or two, so it is asked off the
    /// drawing thread and looked for on each tick.
    pub fn start() -> Usb {
        let (tx, rx) = channel();
        std::thread::spawn(move || {
            let _ = tx.send(flipctl_app::claim("hid"));
            flipctl_app::redraw();
        });
        Usb { taking: Some(rx), held: None, error: String::new() }
    }

    /// Pick up the claim once it has been answered.
    pub fn poll(&mut self) {
        let Some(answer) = self.taking.as_ref().and_then(|rx| rx.try_recv().ok()) else {
            return;
        };
        self.taking = None;
        match answer.and_then(|claim| {
            let path = std::fs::read_to_string(DEV).map_err(|e| format!("{DEV}: {e}"))?;
            let dev = OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(path.trim())
                .map_err(|e| format!("{}: {e}", path.trim()))?;
            Ok((claim, dev))
        }) {
            Ok(held) => self.held = Some(held),
            // The host's screen has room for a word: the reason goes to the log
            Err(e) => {
                eprintln!("usb-keyboard-touchpad: USB: {e}");
                self.error = "failed".into();
            }
        }
    }

    /// One report. Dropped rather than queued when the host is not reading: a key
    /// that arrives seconds late is worse than one that does not arrive.
    pub fn send(&mut self, report: &[u8]) {
        let Some((_, dev)) = self.held.as_mut() else {
            return;
        };
        // The gadget takes one report at a time, so a release written right behind
        // its press finds the port busy: wait for it briefly rather than lose the
        // release and leave the key held on the host
        for _ in 0..2 {
            match dev.write(report) {
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: dev.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
                    if unsafe { libc::poll(&mut fd, 1, WRITE_WAIT_MS) } <= 0 {
                        return;
                    }
                }
                _ => return,
            }
        }
    }

    /// What the monitor says: the claim being taken, its error, or the host.
    pub fn state(&self) -> String {
        if !self.error.is_empty() {
            return self.error.clone();
        }
        if self.held.is_none() {
            return "starting".into();
        }
        if port_state() == "configured" {
            "ready".into()
        } else {
            "waiting".into()
        }
    }
}

/// The device port's state as the kernel names it: "configured" once a host has
/// set the device up.
fn port_state() -> String {
    let ports = std::fs::read_dir("/sys/class/udc").into_iter().flatten().flatten();
    for port in ports.filter(|p| !p.file_name().to_string_lossy().contains("vudc")) {
        if let Ok(state) = std::fs::read_to_string(port.path().join("state")) {
            return state.trim().to_string();
        }
    }
    String::new()
}
