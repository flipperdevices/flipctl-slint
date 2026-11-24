//! The two kernel interfaces the proxy talks to: usbfs for the device, raw-gadget for
//! the PC. Their structs as the kernel lays them out, and their request numbers.

use std::io;
use std::os::fd::RawFd;

use libc::{c_ulong, c_void};

const NONE: u32 = 0;
const WRITE: u32 = 1;
const READ: u32 = 2;

const fn ioc(dir: u32, kind: u8, nr: u8, size: usize) -> c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((kind as u32) << 8) | nr as u32) as c_ulong
}

/// One ioctl, its argument a pointer or a plain value.
pub fn ioctl(fd: RawFd, request: c_ulong, arg: *mut c_void) -> io::Result<i32> {
    let r = unsafe { libc::ioctl(fd, request as _, arg) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(r)
    }
}

/// An ioctl whose argument is a number rather than a pointer.
pub fn ioctl_value(fd: RawFd, request: c_ulong, value: usize) -> io::Result<i32> {
    ioctl(fd, request, value as *mut c_void)
}

// usbfs

#[repr(C)]
pub struct CtrlTransfer {
    pub request_type: u8,
    pub request: u8,
    pub value: u16,
    pub index: u16,
    pub length: u16,
    pub timeout: u32,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct Urb {
    pub kind: u8,
    pub endpoint: u8,
    pub status: i32,
    pub flags: u32,
    pub buffer: *mut c_void,
    pub buffer_length: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub stream_id: u32,
    pub error_count: i32,
    pub signr: u32,
    pub usercontext: *mut c_void,
}

#[repr(C)]
pub struct DisconnectClaim {
    pub interface: u32,
    pub flags: u32,
    pub driver: [u8; 256],
}

const _: () = assert!(std::mem::size_of::<CtrlTransfer>() == 24);
const _: () = assert!(std::mem::size_of::<Urb>() == 56);
const _: () = assert!(std::mem::size_of::<DisconnectClaim>() == 264);

pub const USBDEVFS_CONTROL: c_ulong = ioc(READ | WRITE, b'U', 0, 24);
pub const USBDEVFS_SETINTERFACE: c_ulong = ioc(READ, b'U', 4, 8);
pub const USBDEVFS_SETCONFIGURATION: c_ulong = ioc(READ, b'U', 5, 4);
pub const USBDEVFS_SUBMITURB: c_ulong = ioc(READ, b'U', 10, 56);
pub const USBDEVFS_DISCARDURB: c_ulong = ioc(NONE, b'U', 11, 0);
pub const USBDEVFS_REAPURB: c_ulong = ioc(WRITE, b'U', 12, 8);
pub const USBDEVFS_RELEASEINTERFACE: c_ulong = ioc(READ, b'U', 16, 4);
pub const USBDEVFS_CLEAR_HALT: c_ulong = ioc(READ, b'U', 21, 4);
pub const USBDEVFS_DISCONNECT_CLAIM: c_ulong = ioc(READ, b'U', 27, 264);

pub const URB_INTERRUPT: u8 = 1;
pub const URB_BULK: u8 = 3;

// raw-gadget

#[repr(C)]
pub struct RawInit {
    pub driver_name: [u8; 128],
    pub device_name: [u8; 128],
    pub speed: u8,
}

const _: () = assert!(std::mem::size_of::<RawInit>() == 257);

pub const RAW_INIT: c_ulong = ioc(WRITE, b'U', 0, 257);
pub const RAW_RUN: c_ulong = ioc(NONE, b'U', 1, 0);
pub const RAW_EVENT_FETCH: c_ulong = ioc(READ, b'U', 2, 8);
pub const RAW_EP0_WRITE: c_ulong = ioc(WRITE, b'U', 3, 8);
pub const RAW_EP0_READ: c_ulong = ioc(READ | WRITE, b'U', 4, 8);
pub const RAW_EP_ENABLE: c_ulong = ioc(WRITE, b'U', 5, 9);
pub const RAW_EP_DISABLE: c_ulong = ioc(WRITE, b'U', 6, 4);
pub const RAW_EP_WRITE: c_ulong = ioc(WRITE, b'U', 7, 8);
pub const RAW_EP_READ: c_ulong = ioc(READ | WRITE, b'U', 8, 8);
pub const RAW_CONFIGURE: c_ulong = ioc(NONE, b'U', 9, 0);
pub const RAW_VBUS_DRAW: c_ulong = ioc(WRITE, b'U', 10, 4);
pub const RAW_EP0_STALL: c_ulong = ioc(NONE, b'U', 12, 0);
pub const RAW_EP_SET_HALT: c_ulong = ioc(WRITE, b'U', 13, 4);
pub const RAW_EP_CLEAR_HALT: c_ulong = ioc(WRITE, b'U', 14, 4);
pub const RAW_IO_ZERO: u16 = 1;

pub const EV_CONTROL: u32 = 2;
pub const EV_RESET: u32 = 5;
pub const EV_DISCONNECT: u32 = 6;

pub const SPEED_FULL: u8 = 2;
pub const SPEED_HIGH: u8 = 3;

#[cfg(test)]
mod tests {
    use super::*;

    /// The numbers the Python proxy used against the same kernel, worked out there
    /// from the C headers' sizes.
    #[test]
    fn request_numbers_match_the_kernel() {
        assert_eq!(USBDEVFS_SUBMITURB, 0x8038550a);
        assert_eq!(USBDEVFS_REAPURB, 0x4008550c);
        assert_eq!(USBDEVFS_CONTROL, 0xc0185500);
        assert_eq!(RAW_EP_READ, 0xc0085508);
    }
}
