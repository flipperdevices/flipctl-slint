//! Radio state for the Network menu.
//!
//! Unlike `status`, this cannot come from sysfs. Airplane mode is not a kernel
//! concept: the prototype defines it as "NetworkManager has both the wifi and the
//! wwan radios disabled", and the only supported way to read or change that is
//! `nmcli`. So this module does shell out, on a background thread.
//!
//! Told rather than asked. The prototype polls every three seconds, which is two
//! `nmcli` processes every three seconds forever, and NetworkManager and dbus pay for
//! each one: it was the last of our periodic wakeups. `nmcli monitor` is a process
//! that says nothing until something changes, so the state is read once at startup
//! and then only when NetworkManager reports a change, which also means a radio
//! toggle shows up at once instead of up to three seconds later.
//!
//! Writes are optimistic exactly as they are in the prototype: the local value
//! flips immediately so the row redraws on the same frame as the key press, and
//! the next read reconciles if the radio refused.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

use crate::sysinfo::output;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// How long to wait before starting the monitor again after it ends.
///
/// It ends when NetworkManager restarts, and it never starts at all on a machine
/// without it: either way this is the interval between attempts, and a read goes with
/// each one so the state is still refreshed on a machine where the monitor cannot run.
const RETRY: Duration = Duration::from_secs(10);

/// How long to leave a change of ours before checking that it took.
const CONFIRM: Duration = Duration::from_millis(1500);

/// How long to wait for a burst of changes to finish before reading.
///
/// One connection coming up prints several lines, and each read costs two processes,
/// so they are coalesced into one.
const SETTLE: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Net {
    /// Both radios off. `nmcli radio all off` is what sets it.
    pub airplane: bool,
    pub wifi_enabled: bool,
    pub wifi_connected: bool,
    /// Empty when not connected, or when the active connection has no name.
    pub ssid: String,
    /// NetworkManager's overall answer, which is the best any interface gets.
    pub connectivity: Connectivity,
    /// Some interface is behind a captive portal. Not the same as the overall answer
    /// being one: with a cable online beside a portal's Wi-Fi, the whole is full and
    /// only the Wi-Fi says portal.
    pub portal: bool,
}

/// How far NetworkManager's own check gets: it fetches a known page and compares
/// what comes back, so a network that answers with something else is behind a
/// captive portal.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Connectivity {
    #[default]
    Unknown,
    None,
    Portal,
    Limited,
    Full,
}

impl Connectivity {
    fn parse(word: &str) -> Self {
        match word.trim() {
            "none" => Self::None,
            "portal" => Self::Portal,
            "limited" => Self::Limited,
            "full" => Self::Full,
            _ => Self::Unknown,
        }
    }
}

/// `nmcli -t -f CONNECTIVITY,WIFI,WWAN general` prints one line,
/// `<connectivity>:<wifi>:<wwan>`. The terse values are not translated whatever the
/// locale, and the connectivity comes with the radios for no extra process.
fn read_general() -> Option<(Connectivity, bool, bool)> {
    parse_general(&output(&["nmcli", "-t", "-f", "CONNECTIVITY,WIFI,WWAN", "general"])?)
}

fn parse_general(s: &str) -> Option<(Connectivity, bool, bool)> {
    let mut fields = s.lines().next()?.split(':');
    let connectivity = Connectivity::parse(fields.next()?);
    let wifi = fields.next()?.trim() == "enabled";
    let wwan = fields.next()?.trim() == "enabled";
    Some((connectivity, wifi, wwan))
}

/// `nmcli -t -f DEVICE,TYPE,STATE,CONNECTION,IP4-CONNECTIVITY,IP6-CONNECTIVITY device`
/// prints one line per interface. Read for two things in one process: the connected
/// Wi-Fi's connection, whose name for a normal `nmcli dev wifi connect` is the SSID,
/// and whether any interface is behind a portal.
fn read_devices() -> Option<(Option<String>, bool)> {
    Some(parse_devices(&output(&[
        "nmcli",
        "-t",
        "-f",
        "DEVICE,TYPE,STATE,CONNECTION,IP4-CONNECTIVITY,IP6-CONNECTIVITY",
        "device",
    ])?))
}

fn parse_devices(s: &str) -> (Option<String>, bool) {
    let mut ssid = None;
    let mut portal = false;
    for line in s.lines() {
        let fields = terse_fields(line);
        let [_, kind, state, connection, ip4, ip6] = fields.as_slice() else { continue };
        if kind == "wifi" && state == "connected" && ssid.is_none() {
            ssid = Some(connection.clone());
        }
        portal |= [ip4, ip6].iter().any(|c| Connectivity::parse(c) == Connectivity::Portal);
    }
    (ssid, portal)
}

/// One line of nmcli's terse output, split on the colons that are not escaped: a
/// connection's name can hold a colon, which comes out as `\:`, and a backslash as
/// `\\`.
fn terse_fields(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => fields.last_mut().unwrap().extend(chars.next()),
            ':' => fields.push(String::new()),
            _ => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

fn read_net() -> Net {
    let (connectivity, wifi_enabled, wwan_enabled) =
        read_general().unwrap_or((Connectivity::Unknown, false, false));
    let (ssid, behind) = read_devices().unwrap_or((None, false));
    Net {
        airplane: !wifi_enabled && !wwan_enabled,
        wifi_enabled,
        wifi_connected: ssid.is_some(),
        ssid: ssid.unwrap_or_default(),
        connectivity,
        portal: behind || connectivity == Connectivity::Portal,
    }
}

/// The page a browser opens on to meet a captive portal: NetworkManager's own check
/// address, which the portal is already intercepting, so asking for it is what gets
/// the portal's sign-in page back. Read from NetworkManager's merged configuration
/// rather than written down twice, by its full path since /usr/sbin is on no user's
/// PATH; Debian's default when it cannot be read.
pub fn portal_page() -> String {
    const DEBIAN: &str = "http://network-test.debian.org/nm";
    output(&["/usr/sbin/NetworkManager", "--print-config"])
        .and_then(|config| connectivity_uri(&config))
        .unwrap_or_else(|| DEBIAN.to_string())
}

/// The `uri` of the `[connectivity]` section of `NetworkManager --print-config`.
fn connectivity_uri(config: &str) -> Option<String> {
    let mut inside = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            inside = line == "[connectivity]";
        } else if inside {
            if let Some(uri) = line.strip_prefix("uri=") {
                return Some(uri.trim().to_string()).filter(|u| !u.is_empty());
            }
        }
    }
    None
}

/// Polls nmcli on its own thread so the render loop never waits on a subprocess.
/// Why the watcher woke: NetworkManager said something, or we changed something and
/// want to see whether it took.
enum Wake {
    Told,
    Ours,
    /// The monitor's output ended, which is what NetworkManager restarting looks like.
    Ended,
}

pub struct NetSource {
    state: Arc<Mutex<Net>>,
    /// How a write asks the watcher to look again. A radio the hardware refuses
    /// produces no event at all, so the optimistic flip has to be checked by us.
    poke: std::sync::mpsc::Sender<Wake>,
    /// Set whenever the poller or a write changes the state, so the loop knows to
    /// repaint without diffing.
    dirty: Arc<AtomicBool>,
}

impl NetSource {
    pub fn spawn() -> Self {
        let state = Arc::new(Mutex::new(Net::default()));
        let dirty = Arc::new(AtomicBool::new(false));
        // One channel for the life of the watcher: the monitor's lines and our own
        // writes both arrive on it, and it is what the watcher blocks on.
        let (poke, wakes) = std::sync::mpsc::channel::<Wake>();
        let told = poke.clone();
        let (s, d) = (Arc::clone(&state), Arc::clone(&dirty));
        thread::Builder::new()
            .name("net-watch".into())
            .spawn(move || {
                let publish = |fresh: Net| {
                    let mut cur = s.lock().unwrap();
                    if *cur != fresh {
                        *cur = fresh;
                        d.store(true, Ordering::Relaxed);
                    }
                };
                loop {
                    publish(read_net());
                    let mut monitor = match {
                        let mut command = Command::new("nmcli");
                        command
                            .arg("monitor")
                            .stdin(Stdio::null())
                            .stdout(Stdio::piped())
                            .stderr(Stdio::null());
                        // Die with us. The monitor is useless without the process
                        // that reads it, and nothing else would ever stop it: the
                        // unit sets PAMName=login, which puts us in a logind
                        // session scope rather than the service's cgroup, so
                        // systemd restarting the service kills us and leaves this
                        // child parented to init. Four of them had accumulated on
                        // the test device, one per restart, 58MB between them.
                        //
                        // SAFETY: pre_exec runs in the forked child before exec,
                        // where only async-signal-safe calls are allowed. prctl is
                        // one, and this call touches nothing else.
                        unsafe {
                            command.pre_exec(|| {
                                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) < 0 {
                                    return Err(std::io::Error::last_os_error());
                                }
                                Ok(())
                            });
                        }
                        command.spawn()
                    } {
                        Ok(child) => child,
                        // No NetworkManager here, or no nmcli: nothing to watch and
                        // nothing to read, so wait rather than spin.
                        Err(_) => {
                            thread::sleep(RETRY);
                            continue;
                        }
                    };
                    if let Some(out) = monitor.stdout.take() {
                        // The lines are read on a thread of their own and this one
                        // waits: a burst then costs one read rather than one read per
                        // line, and the read happens after the burst rather than in the
                        // middle of it.
                        let lines = told.clone();
                        thread::Builder::new()
                            .name("net-monitor".into())
                            .spawn(move || {
                                use std::io::{BufRead, BufReader};
                                for line in BufReader::new(out).lines().map_while(Result::ok) {
                                    let _ = line;
                                    if lines.send(Wake::Told).is_err() {
                                        return;
                                    }
                                }
                                // The pipe closed. Said explicitly, because the channel
                                // itself never closes: a write holds a sender for the
                                // life of the source, so waiting for the receive to
                                // fail would wait forever and nothing would ever be
                                // read again.
                                let _ = lines.send(Wake::Ended);
                            })
                            .ok();
                        let mut ended = false;
                        while let Ok(wake) = wakes.recv() {
                            match wake {
                                // NetworkManager talks in bursts: wait for quiet. The
                                // burst is drained, but not the news that came with it:
                                // NetworkManager stopping is a burst of events followed
                                // by the pipe closing, and swallowing that last one left
                                // the watcher blocked on a monitor that had gone.
                                Wake::Told => {
                                    while let Ok(next) = wakes.recv_timeout(SETTLE) {
                                        ended |= matches!(next, Wake::Ended);
                                    }
                                }
                                // Our own change: give the radio a moment to refuse it.
                                Wake::Ours => thread::sleep(CONFIRM),
                                Wake::Ended => ended = true,
                            }
                            // Read before leaving: the radios going with NetworkManager
                            // is itself the state, and the screen should say so.
                            publish(read_net());
                            if ended {
                                break;
                            }
                        }
                    }
                    // The monitor ended, which NetworkManager restarting does.
                    let _ = monitor.wait();
                    thread::sleep(RETRY);
                }
            })
            .expect("spawn net watcher");
        Self { state, dirty, poke }
    }

    pub fn get(&self) -> Net {
        self.state.lock().unwrap().clone()
    }

    /// True once per change, so the caller can repaint only when something moved.
    pub fn take_dirty(&self) -> bool {
        self.dirty.swap(false, Ordering::Relaxed)
    }

    /// Turn the Wi-Fi radio on or off.
    ///
    /// Not the same control as airplane mode, which is both radios at once: this is
    /// `nmcli radio wifi`, which the Wi-Fi page's own toggle drives. Optimistic and
    /// confirmed the same way, for the same reason: the row has to redraw on the
    /// frame the key was pressed on.
    ///
    /// Turning the radio on cannot leave airplane mode set, since that is the state
    /// of both radios being off. Turning it off says nothing about the other one, so
    /// that is left for the confirming read to settle.
    pub fn set_wifi_enabled(&self, on: bool) {
        {
            let mut cur = self.state.lock().unwrap();
            cur.wifi_enabled = on;
            if on {
                cur.airplane = false;
            } else {
                cur.wifi_connected = false;
                cur.ssid.clear();
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        crate::system::spawn_detached(&["nmcli", "radio", "wifi", if on { "on" } else { "off" }]);
        let _ = self.poke.send(Wake::Ours);
    }

    /// Turn airplane mode on or off.
    ///
    /// Airplane on means both radios off, so the nmcli argument is inverted. The
    /// local value flips first and the command runs detached: `nmcli radio all`
    /// takes a few hundred milliseconds and the row must not wait for it.
    pub fn set_airplane(&self, on: bool) {
        {
            let mut cur = self.state.lock().unwrap();
            cur.airplane = on;
            cur.wifi_enabled = !on;
            if on {
                cur.wifi_connected = false;
                cur.ssid.clear();
            }
        }
        self.dirty.store(true, Ordering::Relaxed);
        crate::system::spawn_detached(&["nmcli", "radio", "all", if on { "off" } else { "on" }]);
        // The row is showing what we asked for, not what happened: have the watcher
        // look again once the radio has had time to refuse.
        let _ = self.poke.send(Wake::Ours);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What nmcli 1.52 prints, read on the device, and a portal as the fake one
    /// made it say.
    #[test]
    fn the_general_line_carries_connectivity_and_both_radios() {
        assert_eq!(parse_general("full:enabled:enabled\n"), Some((Connectivity::Full, true, true)));
        assert_eq!(
            parse_general("portal:enabled:disabled"),
            Some((Connectivity::Portal, true, false))
        );
        assert_eq!(
            parse_general("limited:disabled:disabled").map(|g| g.0),
            Some(Connectivity::Limited)
        );
        assert_eq!(
            parse_general("something:enabled:enabled").map(|g| g.0),
            Some(Connectivity::Unknown)
        );
        assert_eq!(parse_general("full:enabled"), None, "a short line is not a reading");
    }

    /// What the device printed on the portal tester's Wi-Fi with its cable online, and
    /// a moment in which the Wi-Fi said portal: the whole was full throughout.
    #[test]
    fn a_portal_on_one_interface_is_seen_beside_a_full_one() {
        let limited = "end0:ethernet:connected:Router WAN:full:limited\n\
                       wlxb06b11673c1a:wifi:connected:TEST-PORTAL:limited:limited\n\
                       lo:loopback:connected (externally):lo:unknown:unknown\n\
                       wlxb26b11673c1a:wifi:disconnected::none:none\n";
        assert_eq!(parse_devices(limited), (Some("TEST-PORTAL".into()), false));
        let portal = limited.replace("TEST-PORTAL:limited", "TEST-PORTAL:portal");
        assert_eq!(parse_devices(&portal), (Some("TEST-PORTAL".into()), true));
        // A name with a colon in it, escaped as nmcli escapes it.
        let colon = "wlan0:wifi:connected:Cafe\\: Guest:portal:none\n";
        assert_eq!(parse_devices(colon), (Some("Cafe: Guest".into()), true));
        assert_eq!(parse_devices(""), (None, false));
    }

    /// The check address out of `NetworkManager --print-config`, as the device prints
    /// it, and nothing out of a section that is not the connectivity one.
    #[test]
    fn the_check_address_comes_out_of_the_connectivity_section() {
        let printed = "[main]\nplugins=ifupdown,keyfile\n\n[ifupdown]\nmanaged=false\n\n\
                       [connectivity]\nuri=http://network-test.debian.org/nm\ninterval=300\n\
                       response=NetworkManager is online\n";
        assert_eq!(connectivity_uri(printed).as_deref(), Some("http://network-test.debian.org/nm"));
        assert_eq!(connectivity_uri("[main]\nuri=http://nope/\n"), None);
        assert_eq!(connectivity_uri("[connectivity]\nuri=\n"), None);
    }
}
