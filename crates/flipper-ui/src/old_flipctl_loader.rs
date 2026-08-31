//! Loading the old flipctl, the flipctl2 prototype, in place of this one.
//!
//! It is the thing this port is measured against, and it draws on the same
//! glass: `fake-flipctl-node-server` serves its page and `cog-seat1` puts that
//! page on the panel. The panel has one owner at a time, so switching to it is
//! not "start the other one", it is a handover: this build has to let go of DRM
//! master before cog can take it.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

/// The browser that draws the prototype on the panel.
const COG: &str = "cog-seat1.service";
/// The prototype's own server, which serves the page cog loads.
const SERVER: &str = "fake-flipctl-node-server.service";

/// Whether this machine has the prototype installed at all.
///
/// Both units, because either one missing makes the row a way to lose the panel
/// and get nothing back. Asked of systemd rather than by looking for a file: a
/// unit can come from any of four directories and `LoadState` is the one answer
/// that covers all of them.
///
/// A filter, not a guarantee, and `start` does not lean on it: `LoadState` is the
/// manager's in-memory view, and a unit whose file has been deleted still reads
/// `loaded` until something reloads the manager. That is not hypothetical, it is
/// how a machine offered this row for a prototype it could no longer start.
///
/// Answered once and remembered, the way `boot::available` is: the row asking is
/// rebuilt on every navigation, and a unit does not appear halfway through a
/// session.
pub fn available() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        let Ok(out) = Command::new("systemctl")
            .args(["show", "-p", "LoadState", "--value", COG, SERVER])
            .stderr(Stdio::null())
            .output()
        else {
            return false;
        };
        let states = String::from_utf8_lossy(&out.stdout);
        states.lines().filter(|state| *state == "loaded").count() == 2
    })
}

/// Stop flipctl and bring the prototype up on the panel.
///
/// One detached transient unit, because the middle of this is stopping whoever
/// asked for it: run as our own child, the stop would kill the script halfway and
/// leave the panel with no owner at all. Same reason `net::reboot` does it.
///
/// The stop is explicit rather than left to `Conflicts=` in flipctl.service:
/// nothing autostarts the prototype now that this build is what boots. It is also
/// the wait, which is the part that matters: it returns once the unit is inactive,
/// and that is when the panel is free for cog to take.
///
/// The pkill is for a flipctl started by hand rather than through the unit, as the
/// README describes: there is nothing to stop, and the process would still be
/// holding the panel when cog reached for it.
///
/// The prototype's server starts after we are gone, because we are in its way
/// twice over: 8899 is the port flipctl serves its own browser view on, and it is
/// the URL cog loads. Starting the server first would have it fail to bind a port
/// flipctl still holds.
///
/// Which is why the server starting is the condition for the rest: if it will not
/// come up, flipctl does, and the panel keeps an owner. Handing the glass to a cog
/// that can only show a connection error, with nothing left to hand it back, is
/// the one outcome this must not have, and no pre-flight check can rule it out on
/// its own.
pub fn start() {
    let script = format!(
        "systemctl stop flipctl.service; \
         pkill -x flipctl; \
         if systemctl start {SERVER}; then \
             systemctl start {COG}; \
         else \
             systemctl start flipctl.service; \
         fi"
    );
    let mut args = vec!["systemd-run", "--collect", "--no-block", "sh", "-c", &script];
    // The transient unit then runs as root, so nothing inside the script needs
    // sudo of its own.
    if unsafe { libc::geteuid() } != 0 {
        args.insert(0, "sudo");
    }
    crate::net::spawn_detached(&args);
}
