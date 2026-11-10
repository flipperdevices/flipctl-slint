//! Taking the Wi-Fi chip for the radio and giving it back.
//!
//! flipctl lends it: `POST /claim/mt7921` on its socket answers once the chip has no
//! driver on it and its USB node is this user's, and the chip stays the app's while the
//! connection is open. Dropping the connection gives it back, and so does the app dying
//! any other way, since the kernel closes the socket then. flipctl puts rfkill back as
//! it found it, so airplane mode survives the radio.
//!
//! The `birria_burst` patch the bundle carries runs here, as the user, once the chip is
//! ours: it makes the chip re-enumerate, and the claim keeps the new node the user's.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The chip's place on the USB tree.
const DEV: &str = "/sys/bus/usb/devices/2-1.1";

/// What a flipctl from before the claim gets: it neither names its socket to an app
/// nor answers HTTP on it, and nothing else can take the chip.
const OLD_FLIPCTL: &str = "FPV Receiver needs a newer flipctl. Update flipctl and try again.";

/// The open claim. Dropping it gives the chip back.
static CLAIM: Mutex<Option<UnixStream>> = Mutex::new(None);

/// Take the chip from Wi-Fi. Blocks for the several seconds the power cycle takes.
pub fn claim() -> Result<(), String> {
    let mut held = CLAIM.lock().unwrap();
    if held.is_some() {
        return Ok(());
    }
    // A hosted app has a runtime dir of its own; flipctl names its socket instead
    let socket = std::env::var_os("FLIPCTL_SOCKET").map(PathBuf::from).ok_or(OLD_FLIPCTL)?;
    let mut stream = UnixStream::connect(&socket).map_err(|e| format!("flipctl: {e}"))?;
    write!(stream, "POST /claim/mt7921 HTTP/1.1\r\nHost: flipctl\r\nContent-Length: 0\r\n\r\n").map_err(|e| format!("flipctl: {e}"))?;
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| format!("flipctl: {e}"))?);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(|e| format!("flipctl: {e}"))?;
    // The headers, then the body's one line: what was claimed, or why not
    let mut line = String::new();
    while reader.read_line(&mut line).map_err(|e| format!("flipctl: {e}"))? > 0 && line.trim_end() != "" {
        line.clear();
    }
    let mut body = String::new();
    let _ = reader.read_line(&mut body);
    if !status.starts_with("HTTP/") {
        return Err(OLD_FLIPCTL.into());
    }
    if status.split(' ').nth(1) != Some("200") {
        return Err(said(&body).unwrap_or_else(|| format!("flipctl: {}", status.trim_end())));
    }
    *held = Some(stream);
    drop(held);
    burst();
    Ok(())
}

/// Give the chip back. The radio must be closed first: powering the chip off under a
/// reader leaves it stuck.
pub fn release() {
    CLAIM.lock().unwrap().take();
}

/// The `error` of a JSON reply.
fn said(body: &str) -> Option<String> {
    let rest = body.split_once("\"error\":\"")?.1;
    Some(rest.rsplit_once("\"}")?.0.replace("\\\"", "\"").replace("\\\\", "\\"))
}

/// Patch the chip for 80 MSps and wait for it to come back as the user's. Without the
/// patch the rates below 80 MSps still work, so a failure is only reported.
fn burst() {
    let Some(bin) = std::env::var_os("APPDIR").map(|d| PathBuf::from(d).join("usr/bin/birria_burst")) else {
        eprintln!("fpv: APPDIR is not set: no burst patch, 80 MSps will overrun");
        return;
    };
    match Command::new(&bin).status() {
        Ok(s) if s.success() => {}
        other => {
            eprintln!("fpv: birria_burst: {other:?}: 80 MSps will overrun");
            return;
        }
    }
    // The port reset drops the chip and it returns with a new address
    std::thread::sleep(Duration::from_secs(3));
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if node_is_ours() {
            return;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    eprintln!("fpv: the chip did not come back after the burst patch");
}

fn node_is_ours() -> bool {
    let read = |f: &str| std::fs::read_to_string(format!("{DEV}/{f}")).ok().and_then(|s| s.trim().parse::<u32>().ok());
    let (Some(bus), Some(dev)) = (read("busnum"), read("devnum")) else { return false };
    std::fs::metadata(format!("/dev/bus/usb/{bus:03}/{dev:03}")).is_ok_and(|m| m.uid() == unsafe { libc::getuid() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_comes_out_of_the_reply() {
        assert_eq!(said("{\"error\":\"the Wi-Fi chip is in use\"}\n").as_deref(), Some("the Wi-Fi chip is in use"));
        assert_eq!(said("{\"error\":\"say \\\"no\\\"\"}").as_deref(), Some("say \"no\""));
        assert_eq!(said("{\"claimed\":\"mt7921\"}"), None);
    }
}
