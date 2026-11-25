//! The capture process: this binary run again with `--capture <interface>`.
//!
//! It makes a user namespace and a network namespace of its own, which needs no
//! privilege, and claims `wifi-monitor` from flipctl, which puts the onboard Wi-Fi in
//! monitor mode and moves it in. Root in its own user namespace, it then owns the radio
//! there: it tunes it and reads every frame off a packet socket. A separate process
//! because only a process with one thread may unshare a user namespace, and the app's
//! window has several.
//!
//! It talks to the app over its stdin and stdout, and goes when either closes: the
//! claim's connection closes with it, its namespace goes, and the kernel hands the
//! radio back to flipctl's.

use std::io::{self, BufRead, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::nl::{self, Nl80211};

/// Bytes of each frame passed on: radiotap, the header and a management body's
/// elements. The length of the whole frame goes with it.
pub const SNAP: usize = 768;

/// Messages to the app, each `kind`, a little-endian u32 length, and the payload.
pub const READY: u8 = b'R';
pub const ERROR: u8 = b'E';
pub const TUNED: u8 = b'T';
/// u64 microseconds, u32 length on air, then the first `SNAP` bytes.
pub const FRAME: u8 = b'F';
/// u32 frequency, u64 active ms, u64 busy ms, i8 noise, u32 frames dropped.
pub const SURVEY: u8 = b'S';

fn say(out: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    out.write_all(&[kind])?;
    out.write_all(&(payload.len() as u32).to_le_bytes())?;
    out.write_all(payload)
}

fn fail(why: impl std::fmt::Display) -> ! {
    let mut out = io::stdout().lock();
    let _ = say(&mut out, ERROR, why.to_string().as_bytes());
    let _ = out.flush();
    std::process::exit(1);
}

/// This process alone in a new user namespace, mapped to its own user, and a new
/// network namespace that namespace owns.
fn unshare() -> io::Result<()> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
        return Err(io::Error::last_os_error());
    }
    std::fs::write("/proc/self/setgroups", "deny")?;
    std::fs::write("/proc/self/uid_map", format!("0 {uid} 1"))?;
    std::fs::write("/proc/self/gid_map", format!("0 {gid} 1"))?;
    Ok(())
}

/// A packet socket on `ifindex`, every protocol, with a receive timeout.
fn packet_socket(ifindex: u32) -> io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let proto = (libc::ETH_P_ALL as u16).to_be();
    let fd = unsafe {
        libc::socket(libc::AF_PACKET, libc::SOCK_RAW | libc::SOCK_CLOEXEC, i32::from(proto))
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let mut addr: libc::sockaddr_ll = unsafe { std::mem::zeroed() };
    addr.sll_family = libc::AF_PACKET as u16;
    addr.sll_protocol = proto;
    addr.sll_ifindex = ifindex as i32;
    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
    let size = std::mem::size_of::<libc::sockaddr_ll>() as libc::socklen_t;
    if unsafe { libc::bind(raw, std::ptr::addr_of!(addr).cast(), size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let buf: libc::c_int = 8 << 20;
    let tv = libc::timeval { tv_sec: 0, tv_usec: 100_000 };
    unsafe {
        libc::setsockopt(raw, libc::SOL_SOCKET, libc::SO_RCVBUF, std::ptr::addr_of!(buf).cast(), 4);
        libc::setsockopt(
            raw,
            libc::SOL_SOCKET,
            libc::SO_RCVTIMEO,
            std::ptr::addr_of!(tv).cast(),
            std::mem::size_of::<libc::timeval>() as libc::socklen_t,
        );
    }
    Ok(fd)
}

/// Frames the socket dropped since last asked.
fn dropped(fd: &std::os::fd::OwnedFd) -> u32 {
    let mut stats = [0u32; 2];
    let mut len = 8 as libc::socklen_t;
    let raw = std::os::fd::AsRawFd::as_raw_fd(fd);
    let got = unsafe {
        libc::getsockopt(
            raw,
            libc::SOL_PACKET,
            libc::PACKET_STATISTICS,
            stats.as_mut_ptr().cast(),
            &mut len,
        )
    };
    if got == 0 {
        stats[1]
    } else {
        0
    }
}

/// Run as the capture process, and never return.
pub fn run(iface: &str) -> ! {
    if let Err(e) = unshare() {
        fail(format!("no namespace of our own: {e}"));
    }
    let claim = match flipctl_app::claim("wifi-monitor") {
        Ok(stream) => stream,
        Err(why) => fail(why),
    };
    let since = Instant::now();
    let ifindex = loop {
        if let Some(i) = nl::ifindex(iface) {
            break i;
        }
        if since.elapsed() > Duration::from_secs(5) {
            fail("the Wi-Fi did not arrive");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let wifi = Nl80211::open().unwrap_or_else(|e| fail(format!("nl80211: {e}")));
    if let Err(e) = wifi.set_monitor(ifindex) {
        fail(format!("monitor mode: {e}"));
    }
    if let Err(e) = nl::link_up(ifindex) {
        fail(format!("Wi-Fi up: {e}"));
    }
    let socket = packet_socket(ifindex).unwrap_or_else(|e| fail(format!("capture: {e}")));
    let raw = std::os::fd::AsRawFd::as_raw_fd(&socket);

    // Commands, one a line: "tune <freq> <width> <center>". The end of stdin is the end.
    let (tx, commands) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
        std::process::exit(0);
    });

    let mut out = io::BufWriter::with_capacity(1 << 16, io::stdout().lock());
    let gone = |e: io::Error| -> ! {
        drop(e);
        std::process::exit(0)
    };
    say(&mut out, READY, iface.as_bytes()).and_then(|_| out.flush()).unwrap_or_else(|e| gone(e));

    let mut buf = vec![0u8; 16384];
    let mut flushed = Instant::now();
    let mut surveyed = Instant::now();
    let mut frame = Vec::with_capacity(SNAP + 12);
    loop {
        while let Ok(line) = commands.try_recv() {
            let words: Vec<u32> =
                line.split_whitespace().skip(1).filter_map(|w| w.parse().ok()).collect();
            if let [freq, width, center] = words[..] {
                match wifi.tune(ifindex, freq, width as u16, center) {
                    Ok(()) => say(&mut out, TUNED, &freq.to_le_bytes()),
                    Err(e) => say(&mut out, ERROR, format!("tune {freq}: {e}").as_bytes()),
                }
                .unwrap_or_else(|e| gone(e));
                let _ = dropped(&socket);
            }
        }
        let got = unsafe { libc::recv(raw, buf.as_mut_ptr().cast(), buf.len(), libc::MSG_TRUNC) };
        if got > 0 {
            let len = got as usize;
            let kept = len.min(SNAP).min(buf.len());
            let at = since.elapsed().as_micros() as u64;
            frame.clear();
            frame.extend_from_slice(&at.to_le_bytes());
            frame.extend_from_slice(&(len as u32).to_le_bytes());
            frame.extend_from_slice(&buf[..kept]);
            say(&mut out, FRAME, &frame).unwrap_or_else(|e| gone(e));
        }
        if surveyed.elapsed() >= Duration::from_secs(1) {
            surveyed = Instant::now();
            let mut s = Vec::with_capacity(25);
            if let Ok(Some(v)) = wifi.survey(ifindex) {
                s.extend_from_slice(&v.freq.to_le_bytes());
                s.extend_from_slice(&v.active_ms.to_le_bytes());
                s.extend_from_slice(&v.busy_ms.to_le_bytes());
                s.push(v.noise.unwrap_or(0) as u8);
            } else {
                s.extend_from_slice(&[0u8; 21]);
            }
            s.extend_from_slice(&dropped(&socket).to_le_bytes());
            say(&mut out, SURVEY, &s).unwrap_or_else(|e| gone(e));
        }
        // Often enough that Locate follows a walk; a pipe write is cheap
        if got <= 0 || flushed.elapsed() >= Duration::from_millis(25) {
            out.flush().unwrap_or_else(|e| gone(e));
            flushed = Instant::now();
        }
        // The claim is held for as long as this process lives
        let _ = &claim;
    }
}
