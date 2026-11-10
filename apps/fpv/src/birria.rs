// BirriaSDR's C API (include/birriasdr.h), only what the scanner uses, and a handle that can
// be shared with the reader thread: the library allows retuning, gain changes and cancelling
// while read_async runs on another thread.
//
// The one file that differs from the receiver's own: the library is opened with dlopen
// rather than linked. A framework app may need libc, libm and libgcc_s and nothing else,
// and the bundler fails a build that links more. The bundle carries libbirriasdr from the
// BirriaSDR release (release.lock), on the LD_LIBRARY_PATH AppRun sets.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::OnceLock;

#[repr(C)]
struct RawDevice {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct Gain {
    pub lna: u8,
    pub lpf: u8,
    pub digital: u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct StreamStats {
    samples: u64,
    packets: u64,
    usb_bytes: u64,
    firmware_state: u32,
    dma_errors: u32,
    stage: c_int,
    reset_error: c_int,
}

#[repr(C)]
struct RecoveryEvent {
    next_sample: u64,
    attempt: u32,
    error: c_int,
    previous: StreamStats,
}

type ReadCallback = extern "C" fn(iq: *const i16, count: u32, first_sample: u64, context: *mut c_void);
type RecoveryCallback = extern "C" fn(event: *const RecoveryEvent, context: *mut c_void);

/// The library's entry points, as dlsym found them.
struct Lib {
    strerror: unsafe extern "C" fn(error: c_int) -> *const c_char,
    get_device_count: unsafe extern "C" fn() -> c_int,
    open: unsafe extern "C" fn(dev: *mut *mut RawDevice, index: u32) -> c_int,
    close: unsafe extern "C" fn(dev: *mut RawDevice) -> c_int,
    set_center_freq: unsafe extern "C" fn(dev: *mut RawDevice, frequency_hz: u64) -> c_int,
    set_sample_rate: unsafe extern "C" fn(dev: *mut RawDevice, samples_per_second: u32) -> c_int,
    set_tuner_gain_mode: unsafe extern "C" fn(dev: *mut RawDevice, manual: c_int) -> c_int,
    set_gain_codes: unsafe extern "C" fn(dev: *mut RawDevice, gain: *const Gain) -> c_int,
    get_gain_codes: unsafe extern "C" fn(dev: *mut RawDevice, gain: *mut Gain, manual: *mut c_int) -> c_int,
    set_antenna: unsafe extern "C" fn(dev: *mut RawDevice, antenna: c_uint) -> c_int,
    get_antenna: unsafe extern "C" fn(dev: *mut RawDevice) -> c_uint,
    set_recovery: unsafe extern "C" fn(dev: *mut RawDevice, max_restarts: u32, callback: Option<RecoveryCallback>, context: *mut c_void) -> c_int,
    read_async: unsafe extern "C" fn(dev: *mut RawDevice, callback: ReadCallback, context: *mut c_void, transfer_count: u32, buffer_samples: u32) -> c_int,
    cancel_async: unsafe extern "C" fn(dev: *mut RawDevice) -> c_int,
}

/// The bundle's own, by its versioned name, which only LD_LIBRARY_PATH resolves. Never a
/// system copy: the bundle pins the release the app was built against.
const NAME: &CStr = c"libbirriasdr.so.0.1.0";

static LIB: OnceLock<Result<Lib, String>> = OnceLock::new();

fn open_lib() -> Result<Lib, String> {
    let handle = unsafe { libc::dlopen(NAME.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(unsafe { CStr::from_ptr(libc::dlerror()) }.to_string_lossy().into_owned());
    }
    macro_rules! sym {
        ($name:literal) => {{
            let p = unsafe { libc::dlsym(handle, concat!($name, "\0").as_ptr().cast()) };
            if p.is_null() {
                return Err(format!("libbirriasdr has no {}: too old a release?", $name));
            }
            unsafe { std::mem::transmute::<*mut c_void, _>(p) }
        }};
    }
    Ok(Lib {
        strerror: sym!("birriasdr_strerror"),
        get_device_count: sym!("birriasdr_get_device_count"),
        open: sym!("birriasdr_open"),
        close: sym!("birriasdr_close"),
        set_center_freq: sym!("birriasdr_set_center_freq"),
        set_sample_rate: sym!("birriasdr_set_sample_rate"),
        set_tuner_gain_mode: sym!("birriasdr_set_tuner_gain_mode"),
        set_gain_codes: sym!("birriasdr_set_gain_codes"),
        get_gain_codes: sym!("birriasdr_get_gain_codes"),
        set_antenna: sym!("birriasdr_set_antenna"),
        get_antenna: sym!("birriasdr_get_antenna"),
        set_recovery: sym!("birriasdr_set_recovery"),
        read_async: sym!("birriasdr_read_async"),
        cancel_async: sym!("birriasdr_cancel_async"),
    })
}

/// Open the library, or say why it cannot be. Everything else here assumes this succeeded.
pub fn load() -> Result<(), String> {
    LIB.get_or_init(open_lib).as_ref().map(|_| ()).map_err(Clone::clone)
}

fn lib() -> &'static Lib {
    LIB.get_or_init(open_lib).as_ref().expect("birria::load() first")
}

/// The device is starting, stopping or restarting a read; try again.
pub const BUSY: i32 = -4;

/// set_antenna: both chains receive and the stronger one is captured.
pub const ANTENNA_AUTO: u8 = 0xff;

/// A library error code with its text.
#[derive(Debug, Clone)]
pub struct Error {
    pub code: i32,
    pub what: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = unsafe { CStr::from_ptr((lib().strerror)(self.code)) };
        write!(f, "{}: {} ({})", self.what, text.to_string_lossy(), self.code)
    }
}

impl std::error::Error for Error {}

fn check(code: c_int, what: &'static str) -> Result<(), Error> {
    if code < 0 {
        Err(Error { code, what })
    } else {
        Ok(())
    }
}

/// An open device. Setters other than the live ones must not run during a read.
pub struct Device {
    raw: *mut RawDevice,
}

// The library serializes its live setters internally and documents them as safe during a
// read from another thread; the rest are only called while no read runs.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Devices present, or -1 without the library.
    pub fn count() -> i32 {
        match LIB.get_or_init(open_lib) {
            Ok(lib) => unsafe { (lib.get_device_count)() },
            Err(_) => -1,
        }
    }

    pub fn open(index: u32) -> Result<Device, Error> {
        let mut raw = std::ptr::null_mut();
        check(unsafe { (lib().open)(&mut raw, index) }, "open")?;
        Ok(Device { raw })
    }

    pub fn set_sample_rate(&self, rate: u32) -> Result<(), Error> {
        check(unsafe { (lib().set_sample_rate)(self.raw, rate) }, "set_sample_rate")
    }

    /// Live: allowed while reading.
    pub fn set_center_freq(&self, hz: u64) -> Result<(), Error> {
        check(unsafe { (lib().set_center_freq)(self.raw, hz) }, "set_center_freq")
    }

    /// Live: allowed while reading.
    pub fn set_gain(&self, gain: Gain) -> Result<(), Error> {
        check(unsafe { (lib().set_tuner_gain_mode)(self.raw, 1) }, "set_tuner_gain_mode")?;
        check(unsafe { (lib().set_gain_codes)(self.raw, &gain) }, "set_gain_codes")
    }

    /// Live: hand the gain to the library's AGC, which keeps the level in a window and moves
    /// the codes along its measured table, LNA first.
    pub fn set_auto_gain(&self) -> Result<(), Error> {
        check(unsafe { (lib().set_tuner_gain_mode)(self.raw, 0) }, "set_tuner_gain_mode")
    }

    /// Live: the codes in force, which in automatic mode are the AGC's current choice.
    pub fn gain_codes(&self) -> Result<Gain, Error> {
        let mut gain = Gain::default();
        let mut manual = 0;
        check(unsafe { (lib().get_gain_codes)(self.raw, &mut gain, &mut manual) }, "get_gain_codes")?;
        Ok(gain)
    }

    /// Only while no read runs: chain 0 or 1, or ANTENNA_AUTO. Applies at the next read.
    pub fn set_antenna(&self, antenna: u8) -> Result<(), Error> {
        check(unsafe { (lib().set_antenna)(self.raw, antenna as c_uint) }, "set_antenna")
    }

    /// Live: the chain captured, which in automatic mode is the library's choice so far.
    pub fn antenna(&self) -> u8 {
        unsafe { (lib().get_antenna)(self.raw) as u8 }
    }

    /// Live: allowed while reading.
    pub fn cancel(&self) {
        unsafe { (lib().cancel_async)(self.raw) };
    }

    /// Stream until cancel: `on_samples` gets interleaved I,Q blocks and `on_gap` each
    /// discontinuity, both on the calling thread, which this blocks. Recovery is on, so
    /// overruns and unreadable packets become gaps instead of ending the read.
    pub fn read<S: FnMut(&[i16]), G: FnMut(i32)>(&self, on_samples: S, on_gap: G) -> Result<(), Error> {
        struct Ctx<S, G> {
            samples: S,
            gap: G,
        }
        extern "C" fn samples<S: FnMut(&[i16]), G: FnMut(i32)>(iq: *const i16, count: u32, _first: u64, context: *mut c_void) {
            let ctx = unsafe { &mut *(context as *mut Ctx<S, G>) };
            let block = unsafe { std::slice::from_raw_parts(iq, 2 * count as usize) };
            (ctx.samples)(block);
        }
        extern "C" fn gap<S: FnMut(&[i16]), G: FnMut(i32)>(event: *const RecoveryEvent, context: *mut c_void) {
            let ctx = unsafe { &mut *(context as *mut Ctx<S, G>) };
            (ctx.gap)(unsafe { (*event).error });
        }
        let lib = lib();
        let mut ctx = Ctx { samples: on_samples, gap: on_gap };
        let pctx = &mut ctx as *mut Ctx<S, G> as *mut c_void;
        check(unsafe { (lib.set_recovery)(self.raw, 8, Some(gap::<S, G>), pctx) }, "set_recovery")?;
        let result = unsafe { (lib.read_async)(self.raw, samples::<S, G>, pctx, 0, 0) };
        unsafe { (lib.set_recovery)(self.raw, 0, None, std::ptr::null_mut()) };
        check(result, "read_async")
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        unsafe { (lib().close)(self.raw) };
    }
}
