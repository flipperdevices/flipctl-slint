//! The board's SIM selector: which card the M.2 modem's SIM 1 lines go to.
//!
//! Flipper One puts the physical SIM tray and its own eSIM chip behind an analog
//! switch on the modem's SIM 1 interface, driven by one GPIO: `sim_sel`, GPIO1_C0,
//! set high at boot by rk3576-flipper-one-rev-f0b1c2.dts. Checked on the test unit
//! on 2026-10-24:
//!
//! - high is the physical SIM, low the eSIM;
//! - the line keeps its level after the request that set it is released, so it is
//!   set and let go rather than held for as long as flipctl runs;
//! - the modem does not notice the swap by itself: it reads the card again only
//!   when it restarts, which is ModemManager's `Reset()`.
//!
//! Spoken to through the kernel's GPIO character device, as libgpiod does it, so
//! nothing is run and nothing beyond `libc` is linked. A read requests the line
//! "as is": requesting it as an input would let the switch float and drop the SIM.
//! The chip is found by the controller's address, not by its number, which depends
//! on probe order.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd};

/// The GPIO1 bank's controller, and the line in it: C0 is 2 * 8 + 0.
const CONTROLLER: &str = "/2ae10000.gpio/";
const LINE: u32 = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Card {
    Physical,
    Esim,
}

impl Card {
    pub fn name(self) -> &'static str {
        match self {
            Card::Physical => "Physical",
            Card::Esim => "Flipper eSIM",
        }
    }

    pub fn other(self) -> Card {
        match self {
            Card::Physical => Card::Esim,
            Card::Esim => Card::Physical,
        }
    }
}

// The uAPI's v2 structures, from <linux/gpio.h>.
const GPIO_V2_LINES_MAX: usize = 64;
const GPIO_V2_LINE_NUM_ATTRS_MAX: usize = 10;
const GPIO_V2_LINE_FLAG_OUTPUT: u64 = 1 << 3;
const GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES: u32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct LineAttribute {
    id: u32,
    padding: u32,
    /// flags, values or debounce period, by `id`.
    value: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ConfigAttribute {
    attr: LineAttribute,
    mask: u64,
}

#[repr(C)]
struct LineConfig {
    flags: u64,
    num_attrs: u32,
    padding: [u32; 5],
    attrs: [ConfigAttribute; GPIO_V2_LINE_NUM_ATTRS_MAX],
}

#[repr(C)]
struct LineRequest {
    offsets: [u32; GPIO_V2_LINES_MAX],
    consumer: [u8; 32],
    config: LineConfig,
    num_lines: u32,
    event_buffer_size: u32,
    padding: [u32; 5],
    fd: i32,
}

#[repr(C)]
#[derive(Default)]
struct LineValues {
    bits: u64,
    mask: u64,
}

/// `_IOWR(0xB4, nr, size)`.
const fn iowr(nr: u64, size: usize) -> u64 {
    (3 << 30) | ((size as u64) << 16) | (0xB4 << 8) | nr
}
const GPIO_V2_GET_LINE_IOCTL: u64 = iowr(0x07, std::mem::size_of::<LineRequest>());
const GPIO_V2_LINE_GET_VALUES_IOCTL: u64 = iowr(0x0E, std::mem::size_of::<LineValues>());

/// The chip device the selector is on.
fn chip() -> io::Result<File> {
    for entry in std::fs::read_dir("/sys/bus/gpio/devices")?.flatten() {
        let real = std::fs::canonicalize(entry.path()).unwrap_or_default();
        if real.to_string_lossy().contains(CONTROLLER) {
            let name = entry.file_name();
            return File::options()
                .read(true)
                .write(true)
                .open(format!("/dev/{}", name.to_string_lossy()));
        }
    }
    Err(io::Error::new(io::ErrorKind::NotFound, "no SIM selector on this board"))
}

/// Request the line, as is or driven to `drive`, and return the request's fd.
fn request(drive: Option<bool>) -> io::Result<File> {
    let chip = chip()?;
    let mut consumer = [0u8; 32];
    consumer[..11].copy_from_slice(b"flipctl-sim");
    let mut config =
        LineConfig { flags: 0, num_attrs: 0, padding: [0; 5], attrs: Default::default() };
    if let Some(high) = drive {
        // The value goes in with the request, so the line never passes through the
        // other level on its way.
        config.flags = GPIO_V2_LINE_FLAG_OUTPUT;
        config.num_attrs = 1;
        config.attrs[0] = ConfigAttribute {
            attr: LineAttribute {
                id: GPIO_V2_LINE_ATTR_ID_OUTPUT_VALUES,
                padding: 0,
                value: u64::from(high),
            },
            mask: 1,
        };
    }
    let mut offsets = [0u32; GPIO_V2_LINES_MAX];
    offsets[0] = LINE;
    let mut req = LineRequest {
        offsets,
        consumer,
        config,
        num_lines: 1,
        event_buffer_size: 0,
        padding: [0; 5],
        fd: -1,
    };
    // SAFETY: the request is the kernel's struct, laid out as <linux/gpio.h> has it,
    // and lives for the whole call.
    if unsafe { libc::ioctl(chip.as_raw_fd(), GPIO_V2_GET_LINE_IOCTL as _, &mut req) } < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the kernel handed this descriptor over, and nothing else owns it.
    Ok(unsafe { File::from_raw_fd(req.fd) })
}

/// Which card the modem is on, or `None` on a board without the selector.
pub fn read() -> Option<Card> {
    let line = request(None).ok()?;
    let mut values = LineValues { bits: 0, mask: 1 };
    // SAFETY: as above, the kernel's struct for this call.
    if unsafe { libc::ioctl(line.as_raw_fd(), GPIO_V2_LINE_GET_VALUES_IOCTL as _, &mut values) } < 0
    {
        return None;
    }
    Some(if values.bits & 1 == 1 { Card::Physical } else { Card::Esim })
}

/// Put the modem on `card`. Takes effect for the modem when it next restarts.
pub fn set(card: Card) -> io::Result<()> {
    // Dropping the request releases the line, and it keeps the level it was given.
    request(Some(card == Card::Physical)).map(drop)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the kernel checks the ioctl numbers against.
    #[test]
    fn the_structs_are_the_kernels() {
        assert_eq!(std::mem::size_of::<LineRequest>(), 592);
        assert_eq!(std::mem::size_of::<LineValues>(), 16);
        assert_eq!(GPIO_V2_GET_LINE_IOCTL, 0xC250_B407);
        assert_eq!(GPIO_V2_LINE_GET_VALUES_IOCTL, 0xC010_B40E);
    }
}
