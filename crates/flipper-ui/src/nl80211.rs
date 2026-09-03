//! The associated station's signal, asked of the kernel directly.
//!
//! `status` would rather read this out of sysfs, and on a kernel with
//! `CONFIG_CFG80211_WEXT` it does: `/proc/net/wireless` and
//! `/sys/class/net/<if>/wireless/link` both come from that compat layer. This
//! board's kernel has it off, so the file is absent and the `wireless/` directory
//! is empty -- which is how the status bar came to draw an empty signal icon on a
//! full-strength link. Turning WEXT back on would make the sysfs path work again
//! everywhere, and `status` still prefers it, so this becomes dead weight the day
//! that happens rather than something to unpick.
//!
//! Without it the kernel's only interface for the number is nl80211, which is
//! what `iw` and NetworkManager use. Asking it directly is a socket and two
//! messages: no subprocess, no root, and no waiting on NetworkManager. `libc` is
//! already a dependency here for `getifaddrs`, for the same reason -- a UI process
//! should not be shelling out to `ip`.
//!
//! Everything below is generic netlink: resolve the `nl80211` family by name
//! through the controller, then dump the stations on one interface. A managed
//! client has exactly one station, the AP it is joined to.

use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

use crate::netlink::{self, each_attr, each_message, Socket, NLM_F_DUMP};

/// The controller's own family id, which is the one fixed point in the protocol.
const GENL_ID_CTRL: u16 = 16;
/// `genlmsghdr` is four bytes: command, version, and two reserved.
const GENL_HDR: usize = 4;

const CTRL_CMD_GETFAMILY: u8 = 3;
const CTRL_ATTR_FAMILY_ID: u16 = 1;
const CTRL_ATTR_FAMILY_NAME: u16 = 2;

const NL80211_CMD_GET_STATION: u8 = 17;
const NL80211_ATTR_IFINDEX: u16 = 3;
const NL80211_ATTR_STA_INFO: u16 = 21;
const NL80211_STA_INFO_SIGNAL: u16 = 7;
const NL80211_STA_INFO_SIGNAL_AVG: u16 = 13;

/// How long to wait for the kernel. A local netlink round trip is microseconds;
/// this only has to stop a wedged socket from stalling the render loop, which
/// reads the status bar inline once a second.
const REPLY_TIMEOUT: Duration = Duration::from_millis(100);

/// The `nl80211` family id, once it has been resolved.
///
/// Cached because it takes a round trip of its own and does not change while the
/// module is loaded, and cleared whenever a request against it fails: cfg80211 is
/// a module here, and a reload would give it a new id that a stale cache would
/// keep sending to.
static FAMILY: AtomicU16 = AtomicU16::new(0);

/// One generic-netlink request: the family's four-byte header, then attributes.
fn request(family: u16, cmd: u8, flags: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
    netlink::message(family, flags, &[cmd, 0, 0, 0], attrs)
}

/// Ask the controller for the `nl80211` family's id.
fn resolve_family(socket: &Socket) -> Option<u16> {
    let mut name = b"nl80211".to_vec();
    name.push(0);
    socket
        .send(&request(
            GENL_ID_CTRL,
            CTRL_CMD_GETFAMILY,
            0,
            &[(CTRL_ATTR_FAMILY_NAME, &name)],
        ))
        .ok()?;
    let mut buf = [0u8; 4096];
    let got = socket.recv(&mut buf).ok()?;
    let mut family = None;
    each_message(&buf[..got], |_, body| {
        each_attr(&body[GENL_HDR..], |kind, payload| {
            if kind == CTRL_ATTR_FAMILY_ID && payload.len() >= 2 {
                family = Some(u16::from_ne_bytes([payload[0], payload[1]]));
            }
        });
    });
    family.filter(|id| *id != 0)
}

/// The signal of the station this interface is joined to, in dBm.
///
/// The averaged reading where the driver keeps one, since the instantaneous value
/// swings by several dB between frames and the bar has five buckets to put it in.
fn station_signal(socket: &Socket, family: u16, ifindex: u32) -> Option<i8> {
    socket
        .send(&request(
            family,
            NL80211_CMD_GET_STATION,
            NLM_F_DUMP,
            &[(NL80211_ATTR_IFINDEX, &ifindex.to_ne_bytes())],
        ))
        .ok()?;

    let mut buf = [0u8; 8192];
    let mut signal = None;
    let mut average = None;
    // A dump can arrive in several messages. Read until one of them says DONE, or
    // until the kernel stops answering, which the receive timeout bounds.
    for _ in 0..8 {
        let Ok(got) = socket.recv(&mut buf) else {
            break;
        };
        let ok = each_message(&buf[..got], |_, body| {
            each_attr(&body[GENL_HDR..], |kind, payload| {
                if kind != NL80211_ATTR_STA_INFO {
                    return;
                }
                each_attr(payload, |info, value| match info {
                    NL80211_STA_INFO_SIGNAL if !value.is_empty() => {
                        signal = Some(value[0] as i8);
                    }
                    NL80211_STA_INFO_SIGNAL_AVG if !value.is_empty() => {
                        average = Some(value[0] as i8);
                    }
                    _ => {}
                });
            });
        });
        if !ok {
            return None;
        }
        if average.is_some() || signal.is_some() {
            break;
        }
    }
    average.or(signal)
}

/// dBm on the percentage scale NetworkManager uses.
///
/// Clamped to -100..-40 and scaled across that 60dB span, which is
/// `nm_wifi_utils_level_to_quality`'s own curve, so the number means what it means
/// everywhere else on this device.
///
/// The scale is shared; the reading is not. This is the live signal of the station
/// we are joined to, while the visible-networks list shows what nmcli reports for
/// each BSS, which is measured during a scan and only moves when another one runs.
/// Measured on the device: -42dBm from the link against 78 from the last scan of
/// the same AP, which is a bucket apart on a five-bucket icon. Both are honest
/// about their own source, and the badge is the one that should track the link.
fn quality(dbm: i8) -> i32 {
    let dbm = i32::from(dbm).clamp(-100, -40);
    (dbm + 100) * 100 / 60
}

/// The signal for `ifname` as a percentage, or `None` when there is no link, no
/// nl80211, or no answer in time.
pub fn signal_percent(ifname: &str) -> Option<i32> {
    // Read every time, and do not be tempted to cache it beside the family id.
    // Turning the radio off powers the chip down and reloads its firmware on the
    // way back, and mac80211 builds a *new* netdev for it: the kernel log shows
    // `renamed from wlan2`, then wlan3, then wlan4, each renamed back to the same
    // name by udev. Measured on the device after a dozen such cycles, the wifi
    // interface was at ifindex 14 while the ethernet port it booted with was still
    // at 2. A cached index would keep asking about an interface that no longer
    // exists, and the badge would go empty after the first airplane-mode toggle --
    // which looks exactly like the bug this module was written to fix.
    let ifindex: u32 = std::fs::read_to_string(format!("/sys/class/net/{ifname}/ifindex"))
        .ok()?
        .trim()
        .parse()
        .ok()?;
    let socket = Socket::open(netlink::GENERIC).ok()?;
    socket.set_timeout(REPLY_TIMEOUT).ok()?;

    let mut family = FAMILY.load(Ordering::Relaxed);
    if family == 0 {
        family = resolve_family(&socket)?;
        FAMILY.store(family, Ordering::Relaxed);
    }

    match station_signal(&socket, family, ifindex) {
        Some(dbm) => Some(quality(dbm)),
        None => {
            // Either there is no station, or the id we cached is no longer the
            // family's. Forget it so the next read pays for a lookup rather than
            // asking the wrong family forever.
            FAMILY.store(0, Ordering::Relaxed);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dbm_becomes_the_percentage_networkmanager_reports() {
        // The reading behind this port's own bug report: nmcli said 78.
        assert_eq!(quality(-53), 78);
        assert_eq!(quality(-100), 0);
        assert_eq!(quality(-40), 100);
        // Beyond the ends, which drivers do report.
        assert_eq!(quality(-120), 0);
        assert_eq!(quality(-10), 100);
    }

    /// One attribute: a 4-byte header then its payload, padded to four.
    fn attr(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut out = ((crate::netlink::ATTR_HDR + payload.len()) as u16)
            .to_ne_bytes()
            .to_vec();
        out.extend_from_slice(&kind.to_ne_bytes());
        out.extend_from_slice(payload);
        out.resize(crate::netlink::align4(out.len()), 0);
        out
    }

    #[test]
    fn the_signal_is_read_out_of_its_nested_attribute() {
        // What the kernel sends back: STA_INFO nesting SIGNAL and SIGNAL_AVG.
        let mut info = attr(NL80211_STA_INFO_SIGNAL, &[(-47i8) as u8]);
        info.extend(attr(NL80211_STA_INFO_SIGNAL_AVG, &[(-53i8) as u8]));
        let body = attr(NL80211_ATTR_STA_INFO, &info);

        let mut signal = None;
        let mut average = None;
        each_attr(&body, |kind, payload| {
            assert_eq!(kind, NL80211_ATTR_STA_INFO);
            each_attr(payload, |info, value| match info {
                NL80211_STA_INFO_SIGNAL => signal = Some(value[0] as i8),
                NL80211_STA_INFO_SIGNAL_AVG => average = Some(value[0] as i8),
                _ => {}
            });
        });
        assert_eq!(signal, Some(-47));
        // The average is what the reading prefers: the instantaneous value swings
        // several dB between frames.
        assert_eq!(average, Some(-53));
        assert_eq!(quality(average.unwrap()), 78);
    }

}
