//! Lending hardware to an app, and taking it back however the app ends.
//!
//! An app asks on flipctl's socket:
//!
//! ```text
//! POST /claim/<resource> HTTP/1.1
//! ```
//!
//! and gets `200` with `{"claimed":"<resource>"}` once the thing is the app's user's,
//! or `404`, `409` or `500` with `{"error":"..."}`. The `200` has no length: the
//! connection stays open, and the resource is the app's for as long as it does. Close,
//! a kill and a crash all close it, so all of them give it back, which is the point of
//! asking flipctl rather than holding it from the app itself.
//!
//! | Resource | Lent | Given back |
//! |---|---|---|
//! | `mt7921` | the onboard Wi-Fi chip, driverless, for an SDR app | both drivers, rfkill as found |
//! | `udc` | the device side of the main USB-C, as /dev/raw-gadget | the boot gadget, started again |
//! | `uac2` | the same port as a USB sound card, 48 kHz stereo both ways, its ALSA card `UAC2Gadget` | the boot gadget, started again |
//! | `usb/<port>` | the usbfs node of a device in a socket, e.g. `usb/1-1.2` | the device, re-enumerated onto its own driver |
//! | `usbip-share/<port>` | a device in a socket, served by USB/IP on port 3240 | the device to its own driver; usbipd stopped if we started it |
//! | `usbip-attach/<ipv4>/<busid>` | a device another machine serves, attached here; where it landed is in `/run/flipctl-usbip-attach-<ipv4>+<busid>` | detached |
//!
//! A hold that outlives flipctl is given back when flipctl starts again. The work is
//! root scripts run through `sudo -n`, the passwordless sudo flipctl already relies
//! on. The state they share is in /run, so it is volatile like the hold.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::sync::Mutex;

/// The Wi-Fi chip.
pub const MT7921: &str = "mt7921";

/// Headers end within this much, or the request is refused.
const MAX_HEAD: usize = 8192;

/// The resources lent out, by name. One owner each.
static OUT: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

/// What can be asked for.
#[derive(Debug, PartialEq)]
enum Resource {
    Mt7921,
    Udc,
    /// The device port as a USB sound card.
    Uac2,
    /// A device in a socket, by its place on the USB tree.
    Usb(String),
    /// A device in a socket, served to the network by USB/IP.
    UsbipShare(String),
    /// A device another machine serves by USB/IP, by its address and busid there.
    UsbipAttach(String, String),
}

impl Resource {
    fn parse(name: &str) -> Option<Resource> {
        if let Some(rest) = name.strip_prefix("usbip-attach/") {
            let (host, busid) = rest.split_once('/')?;
            let ok = host.parse::<std::net::Ipv4Addr>().is_ok() && is_port(busid);
            return ok.then(|| Resource::UsbipAttach(host.into(), busid.into()));
        }
        let port =
            |prefix: &str| name.strip_prefix(prefix).filter(|p| is_port(p)).map(String::from);
        match name {
            MT7921 => Some(Resource::Mt7921),
            "udc" => Some(Resource::Udc),
            "uac2" => Some(Resource::Uac2),
            _ => port("usb/")
                .map(Resource::Usb)
                .or_else(|| port("usbip-share/").map(Resource::UsbipShare)),
        }
    }

    fn name(&self) -> String {
        match self {
            Resource::Mt7921 => MT7921.into(),
            Resource::Udc => "udc".into(),
            Resource::Uac2 => "uac2".into(),
            Resource::Usb(port) => format!("usb/{port}"),
            Resource::UsbipShare(port) => format!("usbip-share/{port}"),
            Resource::UsbipAttach(host, busid) => format!("usbip-attach/{host}/{busid}"),
        }
    }

    /// What the claim takes, so two claims of one thing cannot both be out.
    fn lock(&self) -> String {
        match self {
            Resource::Uac2 => Resource::Udc.name(),
            _ => self.name(),
        }
    }

    fn busy(&self) -> &'static str {
        match self {
            Resource::Mt7921 => "the Wi-Fi chip is in use",
            Resource::Udc | Resource::Uac2 => "the USB-C device port is in use",
            Resource::Usb(_) => "that USB device is in use",
            Resource::UsbipShare(_) => "that USB device is shared already",
            Resource::UsbipAttach(..) => "that remote device is attached already",
        }
    }

    fn hold(&self) -> Result<(), String> {
        match self {
            Resource::Mt7921 => root(&format!("{MT7921_COMMON}{HOLD}"), &[]),
            Resource::Udc => root(&format!("{UDC}{UDC_CHECK}{GADGET_STOP}{UDC_HOLD}"), &[]),
            Resource::Uac2 => root(&format!("{UAC2}{UAC2_CHECK}{GADGET_STOP}{SERIAL}{UAC2_HOLD}"), &[]),
            Resource::Usb(port) => root(&format!("{SOCKET}{USB_HOLD}"), &[port]),
            Resource::UsbipShare(port) => root(&format!("{SOCKET}{SHARE_HOLD}"), &[port]),
            Resource::UsbipAttach(host, busid) => root(ATTACH_HOLD, &[host, busid]),
        }
    }

    fn release(&self) -> Result<(), String> {
        match self {
            Resource::Mt7921 => root(&format!("{MT7921_COMMON}{RELEASE}"), &[]),
            Resource::Udc => root(&format!("{UDC}{UDC_RELEASE}{GADGET_START}"), &[]),
            Resource::Uac2 => root(&format!("{UAC2}{UAC2_RELEASE}{GADGET_START}"), &[]),
            Resource::Usb(port) => root(USB_RELEASE, &[port]),
            Resource::UsbipShare(port) => root(SHARE_RELEASE, &[port]),
            Resource::UsbipAttach(host, busid) => root(ATTACH_RELEASE, &[host, busid]),
        }
    }
}

/// A place on the USB tree as sysfs names it, `<bus>-<port>[.<port>]...`. Nothing else
/// reaches a script or a file name.
fn is_port(p: &str) -> bool {
    let Some((bus, path)) = p.split_once('-') else { return false };
    let digits = |s: &str| !s.is_empty() && s.len() <= 3 && s.bytes().all(|b| b.is_ascii_digit());
    digits(bus) && path.split('.').count() <= 7 && path.split('.').all(digits)
}

const MT7921_COMMON: &str = r#"
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

/// Stop the boot gadget so the port is free, keeping the unit that ran it in `$STATE`
/// for the release. The claim's own check goes before this, so a kernel without what
/// it needs leaves the boot gadget up.
///
/// The getty on the gadget's serial port goes first. While it holds /dev/ttyGS0,
/// removing the ACM function waits for it in D state, and the gadget's stop with it.
/// udev starts it again with the port.
const GADGET_STOP: &str = r#"
unit=$(systemctl list-units --state=active --no-legend --plain 'usb-*gadget.service' | awk '{print $1; exit}')
[ -e $STATE ] || echo "$unit" > $STATE
systemctl stop 'serial-getty@flipserial*.service'
[ -z "$unit" ] || systemctl stop "$unit"
for g in /sys/kernel/config/usb_gadget/*/UDC; do
    [ -n "$(cat "$g" 2>/dev/null)" ] && { echo "the port is held by gadget $(basename "$(dirname "$g")")"; exit 1; }
done
"#;

/// Start the gadget `$STATE` says ran before the hold.
const GADGET_START: &str = r#"
unit=$(cat $STATE)
rm -f $STATE
[ -z "$unit" ] || systemctl start --no-block "$unit"
exit 0
"#;

const UDC: &str = r#"
STATE=/run/flipctl-udc.unit
"#;

/// Before the boot gadget stops: raw-gadget exists.
const UDC_CHECK: &str = r#"
[ -e /dev/raw-gadget ] || modprobe raw_gadget 2>/dev/null
[ -e /dev/raw-gadget ] || { echo "this kernel has no raw-gadget"; exit 1; }
"#;

/// Take the device port for user `$1`, as /dev/raw-gadget.
const UDC_HOLD: &str = r#"
chown "$1" /dev/raw-gadget
exit 0
"#;

/// Nothing to do without a hold.
const UDC_RELEASE: &str = r#"
[ -e $STATE ] || exit 0
chown root /dev/raw-gadget 2>/dev/null
"#;

const UAC2: &str = r#"
STATE=/run/flipctl-uac2.unit
G=/sys/kernel/config/usb_gadget/flipctl-uac2
"#;

/// Before the boot gadget stops: the kernel has the function.
const UAC2_CHECK: &str = r#"
modprobe usb_f_uac2 2>/dev/null || { echo "this kernel has no USB audio gadget"; exit 1; }
"#;

/// The board's serial number, worked out as usb-gadget-setup.sh does for the system's
/// own gadgets, so a PC sees one Flipper in every mode: the device tree's if the
/// bootloader put it there, else the OTP's the way U-Boot computes serial#, else the
/// machine-id. Uppercase hex, which mass storage needs and the rest do not mind.
const SERIAL: &str = r#"
if [ -r /sys/firmware/devicetree/base/serial-number ]; then
    serial=$(tr -d '\0' < /sys/firmware/devicetree/base/serial-number)
else
    serial=$(/usr/local/bin/rk3576_cpu_serial.sh 2>/dev/null | awk -F'\t' '/^serial:/{print $2}')
fi
serial=${serial:-$(cat /etc/machine-id)}
serial=$(printf '%s' "$serial" | tr -cd '0-9A-Fa-f' | tr 'a-f' 'A-F')
: "${serial:=0123456789AB}"
"#;

/// The port as a USB Audio Class 2 sound card: stereo at 48 kHz, PipeWire's own rate,
/// both ways. It has no volume or mute of its own, so the PC's controls scale the
/// sound on the PC: the gadget's would only be reported to ALSA here and applied by
/// nothing. Flipper's own vendor and product IDs, and the board's serial number.
const UAC2_HOLD: &str = r#"
F=$G/functions/uac2.usb0
mkdir -p $G/strings/0x409 $F $G/configs/c.1 || { echo "configfs refused the gadget"; exit 1; }
echo 0x37C1 > $G/idVendor
echo 0xF125 > $G/idProduct
echo 0x0100 > $G/bcdDevice
echo "$serial" > $G/strings/0x409/serialnumber
echo "Flipper FZCO" > $G/strings/0x409/manufacturer
echo "Flipper One USB Sound Card" > $G/strings/0x409/product
mkdir -p $G/configs/c.1/strings/0x409
echo "Flipper One USB Sound Card" > $G/configs/c.1/strings/0x409/configuration
for d in p c; do
    echo 48000 > $F/${d}_srate
    echo 3 > $F/${d}_chmask
    echo 2 > $F/${d}_ssize
    echo 0 > $F/${d}_mute_present
    echo 0 > $F/${d}_volume_present
done
echo "Flipper One" > $F/function_name
echo 250 > $G/configs/c.1/MaxPower
[ -e $G/configs/c.1/uac2.usb0 ] || ln -s $F $G/configs/c.1/
for u in /sys/class/udc/*; do
    case $u in *vudc*) ;; *) udc=${u##*/}; break ;; esac
done
echo "$udc" > $G/UDC || { echo "the port did not take the sound card"; exit 1; }
exit 0
"#;

/// Take the sound card down. Nothing to do without a hold or the gadget left behind.
const UAC2_RELEASE: &str = r#"
[ -e $STATE ] || [ -d $G ] || exit 0
if [ -d $G ]; then
    echo "" > $G/UDC 2>/dev/null
    rm -f $G/configs/c.1/uac2.usb0
    rmdir $G/configs/c.1/strings/0x409 $G/configs/c.1 $G/functions/uac2.usb0 $G/strings/0x409 $G
fi
[ -e $STATE ] || exit 0
"#;

/// Give user `$1` the usbfs node of the device at `$2`. Only a device in a socket: one
/// wired inside the case has a claim of its own or none.
const USB_HOLD: &str = r#"
node=$(printf /dev/bus/usb/%03d/%03d "$(cat "$d/busnum")" "$(cat "$d/devnum")")
echo "$node" > "/run/flipctl-usb-$2.node"
chown "$1" "$node"
exit 0
"#;

/// Give the device back: re-enumerated, its own driver binds again. One unplugged
/// meanwhile is gone already, and whatever took its place re-enumerates harmlessly.
const USB_RELEASE: &str = r#"
f=/run/flipctl-usb-$2.node
[ -e "$f" ] || exit 0
node=$(cat "$f")
rm -f "$f"
[ -e "$node" ] && chown root "$node"
d=/sys/bus/usb/devices/$2
[ -e "$d/authorized" ] && { echo 0 > "$d/authorized"; echo 1 > "$d/authorized"; }
exit 0
"#;

/// What a device at `$2` must be to be lent at all: plugged in, not a hub, and in a
/// socket rather than wired inside the case, which has a claim of its own or none.
/// Leaves `$d` naming it.
const SOCKET: &str = r#"
d=/sys/bus/usb/devices/$2
[ -e "$d/devnum" ] || { echo "nothing is plugged in at $2"; exit 1; }
[ "$(cat "$d/bDeviceClass")" = 09 ] && { echo "$2 is a hub"; exit 1; }
case $2 in
*.*) hub=${2%.*}; ports=/sys/bus/usb/devices/$hub/$hub:1.0/$hub-port ;;
*) bus=${2%-*}; ports=/sys/bus/usb/devices/usb$bus/$bus-0:1.0/usb$bus-port ;;
esac
[ "$(cat "$ports${2##*[.-]}/connect_type" 2>/dev/null)" = hardwired ] \
    && { echo "$2 is wired inside the case"; exit 1; }
"#;

/// Serve the device at `$2` by USB/IP: usbip-host takes it from its driver and
/// usbipd answers on port 3240. usbipd is started only if nothing runs one, and then
/// remembered, so the last share to end stops it.
const SHARE_HOLD: &str = r#"
command -v usbip >/dev/null || { echo "usbip is not installed"; exit 1; }
modprobe usbip-host || { echo "this kernel has no usbip-host"; exit 1; }
if ! pgrep -x usbipd >/dev/null; then
    usbipd -D || { echo "usbipd did not start"; exit 1; }
    touch /run/flipctl-usbipd
fi
out=$(usbip bind -b "$2" 2>&1) || { echo "${out##*: }"; exit 1; }
touch "/run/flipctl-usbip-share-$2"
exit 0
"#;

/// Stop serving `$2`, give it back to its own driver, and stop a usbipd we started
/// once nothing is shared.
const SHARE_RELEASE: &str = r#"
f=/run/flipctl-usbip-share-$2
[ -e "$f" ] || exit 0
rm -f "$f"
usbip unbind -b "$2" >/dev/null 2>&1
d=/sys/bus/usb/devices/$2
[ -e "$d/authorized" ] && { echo 0 > "$d/authorized"; echo 1 > "$d/authorized"; }
if [ -e /run/flipctl-usbipd ] && ! ls /run/flipctl-usbip-share-* >/dev/null 2>&1; then
    pkill -x usbipd
    rm -f /run/flipctl-usbipd
fi
exit 0
"#;

/// Attach device `$3` that `$2` serves by USB/IP, and wait for it to arrive on the
/// virtual host controller. The state file says which vhci port it took and where it
/// is on this machine's USB tree, readable by the app, which goes on to claim it.
const ATTACH_HOLD: &str = r#"
command -v usbip >/dev/null || { echo "usbip is not installed"; exit 1; }
modprobe vhci-hcd || { echo "this kernel has no vhci-hcd"; exit 1; }
out=$(usbip attach -r "$2" -b "$3" 2>&1) || { echo "${out##*: }"; exit 1; }
want="usbip://$2:3240/$3"
i=0
while [ $i -lt 50 ]; do
    found=$(usbip port 2>/dev/null | awk -v want="$want" \
        '/^Port [0-9]+:/ { p = $2 + 0 } index($0, want) { print p, $1; exit }')
    case $found in *-*) break ;; esac
    sleep 0.1
    i=$((i + 1))
done
case $found in *-*) ;; *) echo "the device did not arrive"; exit 1 ;; esac
echo "$found" > "/run/flipctl-usbip-attach-$2+$3"
exit 0
"#;

/// Detach what `$2` serves as `$3`. Nothing to do without a hold.
const ATTACH_RELEASE: &str = r#"
f=/run/flipctl-usbip-attach-$2+$3
[ -e "$f" ] || exit 0
read -r port _ < "$f"
rm -f "$f"
usbip detach -p "$port" >/dev/null 2>&1
exit 0
"#;

/// Run one of the scripts as root, with this user as `$1` and `args` after it, and
/// say what went wrong in its own words.
fn root(script: &str, args: &[&str]) -> Result<(), String> {
    let user = user_name().ok_or("no name for this user")?;
    let out = Command::new("sudo")
        .args(["-n", "sh", "-c", script, "sh", &user])
        .args(args)
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

/// Give back what a previous flipctl left held, on a thread of its own.
pub fn recover() {
    std::thread::spawn(|| {
        let mut left = Vec::new();
        if std::path::Path::new("/run/udev/rules.d/79-flipctl-mt7921.rules").exists() {
            left.push(Resource::Mt7921);
        }
        if std::path::Path::new("/run/flipctl-udc.unit").exists() {
            left.push(Resource::Udc);
        }
        if std::path::Path::new("/run/flipctl-uac2.unit").exists()
            || std::path::Path::new("/sys/kernel/config/usb_gadget/flipctl-uac2").exists()
        {
            left.push(Resource::Uac2);
        }
        for entry in std::fs::read_dir("/run").into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(port) =
                name.strip_prefix("flipctl-usb-").and_then(|n| n.strip_suffix(".node"))
            {
                left.extend(Resource::parse(&format!("usb/{port}")));
            } else if let Some(port) = name.strip_prefix("flipctl-usbip-share-") {
                left.extend(Resource::parse(&format!("usbip-share/{port}")));
            } else if let Some(pair) = name.strip_prefix("flipctl-usbip-attach-") {
                if let Some((host, busid)) = pair.split_once('+') {
                    left.extend(Resource::parse(&format!("usbip-attach/{host}/{busid}")));
                }
            }
        }
        for resource in left {
            let name = resource.name();
            crate::logline!("claim          {name} left held, giving it back");
            if let Err(e) = resource.release() {
                crate::logline!("claim          {name} release: {e}");
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
    let Some(asked) = path.strip_prefix("/claim/") else {
        return reply(&mut stream, 404, "Not Found", &error("no such endpoint"));
    };
    let Some(resource) = Resource::parse(asked) else {
        return reply(&mut stream, 404, "Not Found", &error("no such resource"));
    };
    if method != "POST" {
        return reply(&mut stream, 405, "Method Not Allowed", &error("a claim is a POST"));
    }
    let name = resource.name();
    let lock = resource.lock();
    if !OUT.lock().unwrap().insert(lock.clone()) {
        return reply(&mut stream, 409, "Conflict", &error(resource.busy()));
    }
    crate::logline!("claim          {name} taking");
    if let Err(e) = resource.hold() {
        crate::logline!("claim          {name} hold: {e}");
        let _ = resource.release();
        OUT.lock().unwrap().remove(&lock);
        return reply(&mut stream, 500, "Internal Server Error", &error(&e));
    }
    let held = (|| {
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nCache-Control: no-store\r\n\r\n{{\"claimed\":\"{name}\"}}\n"
        )?;
        stream.flush()?;
        // Held until the app closes its end, or dies and the kernel closes it
        stream.set_read_timeout(None)?;
        let mut sink = [0u8; 256]; // not-a-panel-dimension
        while stream.read(&mut sink)? > 0 {}
        Ok::<(), std::io::Error>(())
    })();
    crate::logline!("claim          {name} giving back");
    if let Err(e) = resource.release() {
        crate::logline!("claim          {name} release: {e}");
    }
    OUT.lock().unwrap().remove(&lock);
    held
}

pub(crate) fn error(why: &str) -> String {
    format!("{{\"error\":{}}}", json_string(why))
}

/// A JSON string literal: quoted, with control characters dropped.
pub(crate) fn json_string(s: &str) -> String {
    let s: String = s.chars().filter(|c| !c.is_control()).collect();
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

pub(crate) fn reply(
    stream: &mut UnixStream,
    code: u16,
    reason: &str,
    body: &str,
) -> std::io::Result<()> {
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

    /// Only a sysfs port name reaches a script.
    #[test]
    fn a_port_is_a_sysfs_name() {
        for good in ["1-1", "1-1.2", "2-1.4.3", "10-2"] {
            assert_eq!(
                Resource::parse(&format!("usb/{good}")),
                Some(Resource::Usb(good.into())),
                "{good}"
            );
        }
        for bad in [
            "", "1", "1-", "-1", "1-1.", "1-1..2", "a-1", "1-1 ", "1-1;id", "../1-1", "1-1/x",
            "usb1",
        ] {
            assert_eq!(Resource::parse(&format!("usb/{bad}")), None, "{bad:?}");
        }
        assert_eq!(Resource::parse("udc"), Some(Resource::Udc));
        assert_eq!(Resource::parse("uac2"), Some(Resource::Uac2));
        assert_eq!(Resource::Uac2.lock(), Resource::Udc.lock(), "one port");
        assert_eq!(Resource::parse("usb"), None);
        assert_eq!(
            Resource::parse("usbip-share/1-1.2"),
            Some(Resource::UsbipShare("1-1.2".into()))
        );
        assert_eq!(Resource::parse("usbip-share/1-1;id"), None);
    }

    /// An attach names an IPv4 address and a busid, and nothing else gets through.
    #[test]
    fn an_attach_is_an_address_and_a_busid() {
        assert_eq!(
            Resource::parse("usbip-attach/192.168.1.115/1-1.2"),
            Some(Resource::UsbipAttach("192.168.1.115".into(), "1-1.2".into()))
        );
        for bad in [
            "usbip-attach/192.168.1.115",
            "usbip-attach/host.lan/1-1.2",
            "usbip-attach/192.168.1.300/1-1.2",
            "usbip-attach/192.168.1.115/1-1.2/x",
            "usbip-attach/192.168.1.115;id/1-1",
            "usbip-attach//1-1",
        ] {
            assert_eq!(Resource::parse(bad), None, "{bad}");
        }
    }

    /// Anything but a claim of a known resource is answered without touching anything.
    #[test]
    fn a_wrong_request_is_refused() {
        for (first, code) in [
            ("GET /status HTTP/1.1", "404"),
            ("POST /claim/hackrf HTTP/1.1", "404"),
            ("POST /claim/usb/1-1;reboot HTTP/1.1", "404"),
            ("POST /claim/usb/../1-1 HTTP/1.1", "404"),
            ("GET /claim/mt7921 HTTP/1.1", "405"),
            ("GET /claim/udc HTTP/1.1", "405"),
            ("GET /claim/uac2 HTTP/1.1", "405"),
            ("GET /claim/usb/1-1.2 HTTP/1.1", "405"),
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
