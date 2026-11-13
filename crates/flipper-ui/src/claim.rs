//! Lending the Wi-Fi chip to an app as a radio.
//!
//! The onboard MT7921 is Wi-Fi and Bluetooth in one chip, and an SDR app needs it with
//! neither driver on it. An app asks for it on flipctl's socket:
//!
//! ```text
//! POST /claim/mt7921 HTTP/1.1
//! ```
//!
//! and gets `200` with `{"claimed":"mt7921"}` once the chip is driverless and its USB
//! node is the app's user's, or `409` or `500` with `{"error":"..."}`. The `200` has no
//! length: the connection stays open, and the chip is the app's for as long as it does.
//! Close, a kill and a crash all close it, so all of them give the chip back, which is
//! the point of asking flipctl rather than holding the chip from the app itself.
//!
//! Giving back powers the chip off, loads both drivers and powers it on again, unless
//! it was off when claimed: airplane mode blocks the same rfkill switch, and is left as
//! it was found. A hold that outlives flipctl is given back when flipctl starts again.
//!
//! The work is two root scripts run through `sudo -n`, the passwordless sudo flipctl
//! already relies on. The state they share is in /run, so it is volatile like the hold.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// The one resource there is.
pub const MT7921: &str = "mt7921";

/// Headers end within this much, or the request is refused.
const MAX_HEAD: usize = 8192;

/// Whether a claim is out. One owner at a time.
static OUT: Mutex<bool> = Mutex::new(false);

const COMMON: &str = r#"
RFK=/sys/class/rfkill/rfkill0
DEV=/sys/bus/usb/devices/2-1.1
RULE=/run/udev/rules.d/79-flipctl-mt7921.rules
STATE=/run/flipctl-mt7921.rfkill
wait_dev() {
    i=0
    while [ $i -lt 150 ]; do
        if [ "$1" = present ]; then [ -e $DEV ] && return 0; else [ -e $DEV ] || return 0; fi
        sleep 0.1
        i=$((i + 1))
    done
    return 1
}
"#;

/// Take the chip for user `$1`: udev loads no driver for it and gives its node to the
/// user, the drivers go, and a power cycle clears what they left in the chip. The rfkill
/// state it was in is kept for the release.
///
/// The drivers are unloaded with the chip still powered. Unloading waits out a probe in
/// progress, where a block mid-probe hung mt7921u for 13 s right after a release, and
/// the driver shuts down talking to the chip instead of timing out on its absence.
const HOLD: &str = r#"
[ "$(cat $RFK/name 2>/dev/null)" = "Onboard WiFi+BT" ] || { echo "no onboard Wi-Fi chip"; exit 1; }
[ -e /run/udev/rules.d/79-mt7921-sdr.rules ] && { echo "held by mt7921-sdr-hold"; exit 1; }
[ -e $RULE ] || cat $RFK/soft > $STATE
mkdir -p /run/udev/rules.d
echo "SUBSYSTEM==\"usb\", KERNELS==\"2-1.1\", ATTRS{idVendor}==\"0e8d\", ATTRS{idProduct}==\"7961\", ENV{MODALIAS}=\"\", OWNER=\"$1\", MODE=\"0600\"" > $RULE
udevadm control --reload
modprobe -r mt7921u btusb
echo 1 > $RFK/soft
wait_dev absent || { echo "the chip did not power off"; exit 1; }
echo 0 > $RFK/soft
wait_dev present || { echo "the chip did not come back"; exit 1; }
udevadm settle -t 5
for i in "$DEV"/2-1.1:*; do
    [ -e "$i/driver" ] && { echo "a driver is still bound to $(basename "$i")"; exit 1; }
done
exit 0
"#;

/// Give the chip back, and its rfkill state with it. Nothing to do without a hold.
/// Blocked before the drivers load: a block while mt7921u probes hangs it for 30 s.
const RELEASE: &str = r#"
[ -e $RULE ] || exit 0
echo 1 > $RFK/soft
wait_dev absent || echo "the chip did not power off"
rm -f $RULE
udevadm control --reload
modprobe mt7921u
modprobe btusb
was=$(cat $STATE 2>/dev/null || echo 0)
rm -f $STATE
[ "$was" = 1 ] || echo 0 > $RFK/soft
exit 0
"#;

/// Run one of the scripts as root, and say what went wrong in its own words.
fn root(script: &str) -> Result<(), String> {
    let user = user_name().ok_or("no name for this user")?;
    let out = Command::new("sudo")
        .args(["-n", "sh", "-c", &format!("{COMMON}{script}"), "sh", &user])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sudo: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let said = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    Err(said.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim().to_string())
}

/// This process's user, whose the chip's node becomes.
fn user_name() -> Option<String> {
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() {
        return None;
    }
    Some(unsafe { std::ffi::CStr::from_ptr((*pw).pw_name) }.to_string_lossy().into_owned())
}

/// Give back a hold a previous flipctl left, on a thread of its own.
pub fn recover() {
    std::thread::spawn(|| {
        if std::path::Path::new("/run/udev/rules.d/79-flipctl-mt7921.rules").exists() {
            crate::logline!("claim          {MT7921} left held, giving it back");
            if let Err(e) = root(RELEASE) {
                crate::logline!("claim          {MT7921} release: {e}");
            }
        }
    });
}

/// Whether a request's first line is for this module rather than the line protocol.
pub fn is_http(line: &str) -> bool {
    line.split(' ').nth(2).is_some_and(|v| v.trim_end().starts_with("HTTP/"))
}

/// Answer one HTTP request whose first line has been read, on a thread of its own: a
/// claim holds its connection for as long as the app does.
pub fn serve(first: String, rest: Vec<u8>, stream: UnixStream) {
    std::thread::spawn(move || {
        if let Err(e) = handle(&first, &rest, stream) {
            crate::logline!("claim          {e}");
        }
    });
}

fn handle(first: &str, rest: &[u8], mut stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(rest.chain(stream.try_clone()?).take(MAX_HEAD as u64));
    // The headers say nothing a claim needs, but they are read so the reply is not sent
    // over a request still arriving.
    let mut line = String::new();
    while reader.read_line(&mut line)? > 0 && line.trim_end() != "" {
        line.clear();
    }
    let mut parts = first.split(' ');
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let Some(resource) = path.strip_prefix("/claim/") else {
        return reply(&mut stream, 404, "Not Found", &error("no such endpoint"));
    };
    if resource != MT7921 {
        return reply(&mut stream, 404, "Not Found", &error("no such resource"));
    }
    if method != "POST" {
        return reply(&mut stream, 405, "Method Not Allowed", &error("a claim is a POST"));
    }
    {
        let mut out = OUT.lock().unwrap();
        if *out {
            return reply(&mut stream, 409, "Conflict", &error("the Wi-Fi chip is in use"));
        }
        *out = true;
    }
    crate::logline!("claim          {MT7921} taking");
    if let Err(e) = root(HOLD) {
        crate::logline!("claim          {MT7921} hold: {e}");
        let _ = root(RELEASE);
        *OUT.lock().unwrap() = false;
        return reply(&mut stream, 500, "Internal Server Error", &error(&e));
    }
    let held = (|| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\r\n{{\"claimed\":\"{MT7921}\"}}\n"
        )?;
        stream.flush()?;
        // Held until the app closes its end, or dies and the kernel closes it
        stream.set_read_timeout(None)?;
        let mut sink = [0u8; 256]; // not-a-panel-dimension
        while stream.read(&mut sink)? > 0 {}
        Ok::<(), std::io::Error>(())
    })();
    crate::logline!("claim          {MT7921} giving back");
    if let Err(e) = root(RELEASE) {
        crate::logline!("claim          {MT7921} release: {e}");
    }
    *OUT.lock().unwrap() = false;
    held
}

fn error(why: &str) -> String {
    let why: String = why.chars().filter(|c| !c.is_control()).collect();
    format!("{{\"error\":\"{}\"}}", why.replace('\\', "\\\\").replace('"', "\\\""))
}

fn reply(stream: &mut UnixStream, code: u16, reason: &str, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {code} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}\n",
        body.len() + 1
    )?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_is_told_from_the_line_protocol() {
        assert!(is_http("POST /claim/mt7921 HTTP/1.1\r\n"));
        assert!(!is_http("ping\n"));
        assert!(!is_http("open /home/user/Apps/a b HTTP.AppImage\n"));
    }

    #[test]
    fn an_error_is_one_json_string() {
        assert_eq!(error("say \"no\"\n"), r#"{"error":"say \"no\""}"#);
    }

    /// Anything but a claim of the one resource is answered without touching the chip.
    #[test]
    fn a_wrong_request_is_refused() {
        for (first, code) in [
            ("GET /status HTTP/1.1", "404"),
            ("POST /claim/hackrf HTTP/1.1", "404"),
            ("GET /claim/mt7921 HTTP/1.1", "405"),
        ] {
            let (mut a, b) = UnixStream::pair().unwrap();
            a.write_all(b"Host: flipctl\r\n\r\n").unwrap();
            handle(first, b"", b).unwrap();
            let mut answer = String::new();
            a.read_to_string(&mut answer).unwrap();
            assert!(answer.starts_with(&format!("HTTP/1.1 {code} ")), "{first}: {answer}");
            assert!(answer.ends_with("\"}\n"), "{answer}");
        }
    }
}
