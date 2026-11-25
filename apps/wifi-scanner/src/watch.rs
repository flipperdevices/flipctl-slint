//! The app's side: the onboard Wi-Fi's interface, the network list, and the capture
//! process watching one network, whose messages it feeds into the model.

use std::io::{BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::capture;
use crate::model::Model;
use crate::nl::{self, Nl80211};
use crate::scan::{self, Network};

/// Where the onboard chip sits on the USB tree, as flipctl's claim has it.
const ONBOARD: &str = "/sys/bus/usb/devices/2-1.1";

/// How long a scan of every band takes at most, read once a second meanwhile.
const SCAN_SECS: u32 = 8;

/// The onboard Wi-Fi's interface, while the system has it.
pub fn onboard_iface() -> Option<String> {
    let onboard = std::fs::canonicalize(ONBOARD).ok()?;
    std::fs::read_dir("/sys/class/net").ok()?.flatten().find_map(|e| {
        let dev = std::fs::canonicalize(e.path().join("device")).ok()?;
        dev.starts_with(&onboard).then(|| e.file_name().to_string_lossy().into_owned())
    })
}

/// The networks the last scan saw, and a rescan through NetworkManager.
#[derive(Clone, Default)]
pub struct Scanner {
    pub nets: Arc<Mutex<Vec<Network>>>,
    pub scanning: Arc<AtomicBool>,
}

impl Scanner {
    /// Read what the kernel has now, then ask NetworkManager for a fresh scan.
    pub fn scan(&self) {
        if self.scanning.swap(true, Ordering::SeqCst) {
            return;
        }
        let me = self.clone();
        std::thread::spawn(move || {
            let read = |me: &Scanner| {
                let Some(iface) = onboard_iface() else { return };
                let found = nl::ifindex(&iface)
                    .and_then(|i| Nl80211::open().and_then(|w| w.scan_dump(i)).ok())
                    .unwrap_or_default();
                if !found.is_empty() {
                    *me.nets.lock().unwrap() = scan::networks(&found);
                }
                flipctl_app::redraw();
            };
            // An interface NetworkManager has only just taken up again refuses a rescan
            // for a moment
            for _ in 0..20 {
                if onboard_iface().is_some() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            read(&me);
            if let Some(iface) = onboard_iface() {
                for _ in 0..4 {
                    let ok = Command::new("nmcli")
                        .args(["dev", "wifi", "rescan", "ifname", &iface])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .is_ok_and(|s| s.success());
                    if ok {
                        break;
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                // The scan does 2.4 GHz first, then the many 5 GHz channels, some only by
                // listening for a beacon, so a network joins the list as soon as it is
                // heard rather than when the whole scan is done
                for _ in 0..SCAN_SECS {
                    std::thread::sleep(Duration::from_secs(1));
                    read(&me);
                }
            }
            me.scanning.store(false, Ordering::SeqCst);
            flipctl_app::redraw();
        });
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Starting,
    Watching,
    Failed(String),
    Stopped,
}

/// One network being watched by a capture process of its own.
pub struct Watch {
    pub net: Network,
    pub model: Arc<Mutex<Model>>,
    pub state: Arc<Mutex<State>>,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
}

impl Watch {
    pub fn start(net: Network) -> Self {
        let model = Arc::new(Mutex::new(Model::new(net.bssid)));
        let state = Arc::new(Mutex::new(State::Starting));
        let mut watch = Self { net, model, state, child: None, stdin: None };
        let Some(iface) = onboard_iface() else {
            *watch.state.lock().unwrap() = State::Failed("the Wi-Fi is busy or missing".into());
            return watch;
        };
        let exe = std::env::current_exe().unwrap_or_else(|_| "/proc/self/exe".into());
        let spawned = Command::new(exe)
            .args(["--capture", &iface])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                *watch.state.lock().unwrap() = State::Failed(format!("capture: {e}"));
                return watch;
            }
        };
        let mut stdin = child.stdin.take();
        let n = &watch.net;
        if let Some(s) = stdin.as_mut() {
            let _ = writeln!(s, "tune {} {} {}", n.freq, n.width, n.center);
        }
        if let Some(out) = child.stdout.take() {
            let (model, state) = (watch.model.clone(), watch.state.clone());
            std::thread::spawn(move || read(out, &model, &state));
        }
        watch.child = Some(child);
        watch.stdin = stdin;
        watch
    }

    pub fn state(&self) -> State {
        self.state.lock().unwrap().clone()
    }
}

impl Drop for Watch {
    /// The end of its stdin ends the capture process; it is waited for off this thread.
    fn drop(&mut self) {
        drop(self.stdin.take());
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                for _ in 0..30 {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

fn read(out: impl Read, model: &Mutex<Model>, state: &Mutex<State>) {
    let mut out = BufReader::with_capacity(1 << 16, out);
    let mut head = [0u8; 5];
    let mut payload = Vec::new();
    while out.read_exact(&mut head).is_ok() {
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        payload.resize(len, 0);
        if out.read_exact(&mut payload).is_err() {
            break;
        }
        match head[0] {
            capture::FRAME if len >= 12 => {
                let at = u64::from_le_bytes(payload[..8].try_into().expect("8"));
                let on_air = u32::from_le_bytes(payload[8..12].try_into().expect("4")) as usize;
                model.lock().unwrap().frame(at, on_air, &payload[12..]);
            }
            capture::SURVEY if len >= 25 => {
                let active = u64::from_le_bytes(payload[4..12].try_into().expect("8"));
                let busy = u64::from_le_bytes(payload[12..20].try_into().expect("8"));
                let noise = Some(payload[20] as i8).filter(|n| *n != 0);
                let dropped = u32::from_le_bytes(payload[21..25].try_into().expect("4"));
                model.lock().unwrap().survey(active, busy, noise, dropped);
            }
            capture::TUNED => {
                *state.lock().unwrap() = State::Watching;
                flipctl_app::redraw();
            }
            capture::ERROR => {
                *state.lock().unwrap() =
                    State::Failed(String::from_utf8_lossy(&payload).into_owned());
                flipctl_app::redraw();
            }
            _ => {}
        }
    }
    let mut s = state.lock().unwrap();
    if !matches!(*s, State::Failed(_)) {
        *s = State::Stopped;
    }
    flipctl_app::redraw();
}
