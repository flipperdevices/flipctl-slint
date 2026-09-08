//! Asking the machine to do something to itself.
//!
//! `sysinfo` reads what the machine is; this acts on it. Rebooting, restarting a
//! service, handing the panel to another program: all of them outlive the process
//! that asked, so none of them can be an ordinary child.
//!
//! Not in `net`, which is where these started: that module is nmcli and the radios,
//! and a reboot is not a network operation. What they had in common was only the
//! helper that spawns them.

use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;

/// Start a command and do not wait for it.
///
/// The child is deliberately left unreaped: it outlives the call by design and
/// the process count here is bounded by how fast a person can press a key.
pub fn spawn_detached(args: &[&str]) {
    let _ = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Run a script in a transient system unit, as root, without waiting for it.
///
/// The unit is the point: systemd tearing this process down must not kill the
/// command mid-flight, which is exactly what a plain child would suffer when what
/// was asked for is a reboot or a restart of our own service.
///
/// Asked for as root, because `systemd-run` talks to the system manager and from an
/// ordinary user that is "Interactive authentication required" and nothing runs at
/// all. The unit then runs as root, so nothing inside the script needs sudo of its
/// own. flipctl.service runs as `user`, so this branch is the live one on a device.
///
/// A refusal is logged rather than dropped. It went to /dev/null before, which is
/// why a Reboot row that never rebooted looked like a dead key.
///
/// It is also sent to the receiver, for a caller with something on screen that a
/// refusal makes untrue. Only a refusal is sent: a unit that was queued says nothing
/// and closes the channel, so nothing has to guess at a timeout.
pub fn spawn_transient(script: &str) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    let mut args = vec!["systemd-run", "--collect", "--no-block", "sh", "-c", script];
    if unsafe { libc::geteuid() } != 0 {
        args.insert(0, "sudo");
    }
    let child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn();
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            crate::logline!("transient      cannot run {}: {e}", args[0]);
            let _ = tx.send(format!("cannot run {}", args[0]));
            return rx;
        }
    };
    // --no-block means this returns as soon as the unit is queued, so waiting for it
    // costs nothing and is the only way to find out it was refused.
    thread::spawn(move || {
        let mut said = String::new();
        if let Some(mut err) = child.stderr.take() {
            use std::io::Read;
            let _ = err.read_to_string(&mut said);
        }
        let reason = match child.wait() {
            Ok(status) if status.success() => return,
            Ok(status) => {
                let said = said.lines().next().unwrap_or("no reason given").to_string();
                crate::logline!("transient      refused ({status}): {said}");
                said
            }
            Err(e) => {
                crate::logline!("transient      {e}");
                e.to_string()
            }
        };
        let _ = tx.send(reason);
    });
    rx
}

/// Reboot, the way the prototype's `/api/system/reboot` does it.
///
/// The receiver is `spawn_transient`'s: a reason if the machine is staying up after
/// all, and otherwise nothing.
pub fn reboot() -> Receiver<String> {
    spawn_transient("systemctl reboot")
}
