//! The vibration motor, over force feedback.
//!
//! The driver holds a library of effects and plays one when asked for it by
//! number. So this uploads no waveform: an `FF_PERIODIC` effect whose waveform is
//! `FF_CUSTOM` and whose `custom_data` is a single `s16` is the kernel's way of
//! saying "the effect the driver already calls 3". `custom_len` is 1 because that
//! index is the entire payload.
//!
//! Only that path works here. The device advertises `FF_RUMBLE`, `FF_PERIODIC`
//! and `FF_CUSTOM`, but a rumble upload is refused, so the library is all there
//! is. Measured on the board, not read off a datasheet.
//!
//! Three things about the ioctls that cost an afternoon, all of them easy to get
//! wrong and silent when wrong:
//!
//! - `FF_CUSTOM` is 0x5d. 0x5c is `FF_SAW_DOWN`, which this driver does not have,
//!   and asking for it fails the upload rather than the play.
//! - `EVIOCRMFF` takes the effect id **as the argument itself**, not a pointer to
//!   it. Passing a pointer erases whatever integer the pointer happens to be,
//!   leaks the slot, and after sixteen of those every upload starts failing, so
//!   the symptom appears nowhere near the cause.
//! - a play is a whole `input_event`, 24 bytes on this target. The kernel rejects
//!   a short write, which looks like a rejected effect.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::path::PathBuf;

const EV_FF: u16 = 0x15;
const FF_PERIODIC: u16 = 0x51;
const FF_CUSTOM: u16 = 0x5d;

/// The device by name, as the button source finds its own.
const NAME: &str = "Flipper One Haptic";

/// `struct ff_effect` with its union already resolved to the periodic arm.
///
/// Flattened rather than modelled as a union, because only one arm is ever used.
/// The padding is what the union's own alignment would have inserted: it holds a
/// pointer, so the arm starts at 16 rather than 14. `size_of` is asserted below,
/// which is what keeps this honest.
#[repr(C)]
struct FfEffect {
    kind: u16,
    id: i16,
    direction: u16,
    trigger: [u16; 2],
    replay: [u16; 2],
    _pad: u16,
    waveform: u16,
    period: u16,
    magnitude: i16,
    offset: i16,
    phase: u16,
    envelope: [u16; 4],
    custom_len: u32,
    custom_data: *const i16,
}

const EFFECT_SIZE: usize = std::mem::size_of::<FfEffect>();

/// `_IOW('E', nr, size)`.
const fn iow(nr: u32, size: u32) -> libc::c_ulong {
    ((1 << 30) | (size << 16) | ((b'E' as u32) << 8) | nr) as libc::c_ulong
}

pub struct Haptic {
    device: File,
    /// The slot in use. One at a time: the driver has sixteen and the UI never
    /// needs two at once, so each play erases the last.
    loaded: Option<i16>,
}

impl Haptic {
    pub fn open() -> std::io::Result<Self> {
        let path = Self::find()?;
        let device = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { device, loaded: None })
    }

    fn find() -> std::io::Result<PathBuf> {
        for entry in std::fs::read_dir("/sys/class/input")? {
            let entry = entry?;
            let node = entry.file_name();
            let Some(node) = node.to_str() else { continue };
            if !node.starts_with("event") {
                continue;
            }
            let Ok(name) = std::fs::read_to_string(entry.path().join("device/name")) else {
                continue;
            };
            if name.trim() == NAME {
                return Ok(PathBuf::from("/dev/input").join(node));
            }
        }
        Err(std::io::Error::other(format!("no input device named {NAME}")))
    }

    /// Play one of the driver's effects. `ms` of 0 runs the library's own length.
    ///
    /// Errors are dropped: a tick of feedback that did not happen is not worth
    /// failing anything over, and this is called from a redraw.
    pub fn play(&mut self, effect: i16, ms: u16) {
        let _ = self.try_play(effect, ms);
    }

    fn try_play(&mut self, effect: i16, ms: u16) -> std::io::Result<()> {
        if let Some(id) = self.loaded.take() {
            // By value. See the note at the top of this file.
            unsafe { libc::ioctl(self.device.as_raw_fd(), iow(0x81, 4), id as libc::c_int) };
        }
        // Held in a binding, not a temporary: the kernel reads through this
        // pointer during the upload, so it has to outlive the call.
        let index: i16 = effect;
        let mut request = FfEffect {
            kind: FF_PERIODIC,
            id: -1,
            direction: 0,
            trigger: [0, 0],
            replay: [ms, 0],
            _pad: 0,
            waveform: FF_CUSTOM,
            period: 0,
            magnitude: 0,
            offset: 0,
            phase: 0,
            envelope: [0; 4],
            custom_len: 1,
            custom_data: &index,
        };
        let sent = unsafe {
            libc::ioctl(
                self.device.as_raw_fd(),
                iow(0x80, EFFECT_SIZE as u32),
                std::ptr::addr_of_mut!(request),
            )
        };
        if sent < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // The kernel wrote the slot it chose back into the struct.
        let id = request.id;
        self.loaded = Some(id);

        let mut event = [0u8; 24];
        event[16..18].copy_from_slice(&EV_FF.to_ne_bytes());
        event[18..20].copy_from_slice(&(id as u16).to_ne_bytes());
        event[20..24].copy_from_slice(&1i32.to_ne_bytes());
        self.device.write_all(&event)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout the kernel expects, checked here so a field added above cannot
    /// silently shift the union arm and turn every upload into EINVAL.
    #[test]
    fn the_effect_struct_is_the_size_the_kernel_reads() {
        assert_eq!(EFFECT_SIZE, 48);
        let e = FfEffect {
            kind: 0,
            id: 0,
            direction: 0,
            trigger: [0; 2],
            replay: [0; 2],
            _pad: 0,
            waveform: 0,
            period: 0,
            magnitude: 0,
            offset: 0,
            phase: 0,
            envelope: [0; 4],
            custom_len: 0,
            custom_data: std::ptr::null(),
        };
        let base = std::ptr::addr_of!(e) as usize;
        // The union starts at 16, and its two tail fields are what the alignment
        // rules move: 20 and 24 within it.
        assert_eq!(std::ptr::addr_of!(e.waveform) as usize - base, 16);
        assert_eq!(std::ptr::addr_of!(e.custom_len) as usize - base, 16 + 20);
        assert_eq!(std::ptr::addr_of!(e.custom_data) as usize - base, 16 + 24);
    }

    #[test]
    fn the_ioctl_numbers_are_the_kernels() {
        // Printed by a C program against linux/input.h on the device.
        assert_eq!(iow(0x80, 48), 0x40304580);
        assert_eq!(iow(0x81, 4), 0x40044581);
    }
}
