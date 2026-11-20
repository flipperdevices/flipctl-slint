//! The PC's side, through raw-gadget: ep0's requests as events, and an ioctl per
//! transfer on every other endpoint. A transfer blocks until the PC takes or gives
//! its data, and only a signal makes it give way.

use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};

use crate::ioctl::*;

pub struct Gadget {
    fd: OwnedFd,
}

/// The gadget's own name, which raw-gadget matches, by the driver of the controller.
fn gadget_name(driver: &str) -> &str {
    match driver {
        "dwc3" | "dwc3-rockchip" => "dwc3-gadget",
        "dummy_hcd" => "dummy_udc",
        other => other,
    }
}

impl Gadget {
    pub fn open(udc: &str, speed: u8) -> io::Result<Gadget> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/raw-gadget")?;
        let gadget = Gadget { fd: file.into() };
        let link = std::fs::read_link(format!("/sys/class/udc/{udc}/device/driver"))?;
        let driver = link.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut init = RawInit { driver_name: [0; 128], device_name: [0; 128], speed };
        let name = gadget_name(&driver).as_bytes();
        init.driver_name[..name.len().min(127)].copy_from_slice(&name[..name.len().min(127)]);
        init.device_name[..udc.len().min(127)]
            .copy_from_slice(&udc.as_bytes()[..udc.len().min(127)]);
        ioctl(gadget.raw(), RAW_INIT, (&mut init as *mut RawInit).cast())?;
        ioctl(gadget.raw(), RAW_RUN, std::ptr::null_mut())?;
        Ok(gadget)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// The next event: its type and its data, a setup packet for a control request.
    pub fn event(&self) -> io::Result<(u32, Vec<u8>)> {
        let mut buf = [0u8; 8 + 64];
        buf[4..8].copy_from_slice(&64u32.to_le_bytes());
        ioctl(self.raw(), RAW_EVENT_FETCH, buf.as_mut_ptr().cast())?;
        let kind = u32::from_le_bytes(buf[0..4].try_into().unwrap());
        let n = (u32::from_le_bytes(buf[4..8].try_into().unwrap()) as usize).min(64);
        Ok((kind, buf[8..8 + n].to_vec()))
    }

    /// One transfer: `buf` holds the 8-byte header and then the data.
    fn io(
        &self,
        request: libc::c_ulong,
        ep: u16,
        flags: u16,
        buf: &mut Vec<u8>,
        length: usize,
    ) -> io::Result<usize> {
        if buf.len() < 8 + length {
            buf.resize(8 + length, 0);
        }
        buf[0..2].copy_from_slice(&ep.to_le_bytes());
        buf[2..4].copy_from_slice(&flags.to_le_bytes());
        buf[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        Ok(ioctl(self.raw(), request, buf.as_mut_ptr().cast())? as usize)
    }

    pub fn ep0_write(&self, data: &[u8], zero: bool) -> io::Result<()> {
        let mut buf = Vec::with_capacity(8 + data.len());
        buf.extend([0u8; 8]);
        buf.extend(data);
        self.io(RAW_EP0_WRITE, 0, if zero { RAW_IO_ZERO } else { 0 }, &mut buf, data.len())?;
        Ok(())
    }

    pub fn ep0_read(&self, length: usize) -> io::Result<Vec<u8>> {
        let mut buf = Vec::new();
        let n = self.io(RAW_EP0_READ, 0, 0, &mut buf, length)?;
        Ok(buf[8..8 + n.min(length)].to_vec())
    }

    pub fn ep0_stall(&self) -> io::Result<()> {
        ioctl(self.raw(), RAW_EP0_STALL, std::ptr::null_mut())?;
        Ok(())
    }

    /// Enable an endpoint by its descriptor; the handle names it from then on.
    pub fn enable(&self, desc: &[u8; 7]) -> io::Result<u16> {
        let mut full = [0u8; 9];
        full[..7].copy_from_slice(desc);
        Ok(ioctl(self.raw(), RAW_EP_ENABLE, full.as_mut_ptr().cast())? as u16)
    }

    pub fn disable(&self, handle: u16) -> io::Result<()> {
        ioctl_value(self.raw(), RAW_EP_DISABLE, handle.into())?;
        Ok(())
    }

    /// To the PC. `buf` is the caller's to reuse; the data goes after its header.
    pub fn write(&self, handle: u16, buf: &mut Vec<u8>, data: &[u8], zero: bool) -> io::Result<()> {
        buf.resize(8 + data.len(), 0);
        buf[8..].copy_from_slice(data);
        self.io(RAW_EP_WRITE, handle, if zero { RAW_IO_ZERO } else { 0 }, buf, data.len())?;
        Ok(())
    }

    /// From the PC, up to `length` bytes, into the caller's buffer after its header.
    pub fn read(&self, handle: u16, buf: &mut Vec<u8>, length: usize) -> io::Result<usize> {
        let n = self.io(RAW_EP_READ, handle, 0, buf, length)?;
        Ok(n.min(length))
    }

    pub fn configure(&self) -> io::Result<()> {
        ioctl(self.raw(), RAW_CONFIGURE, std::ptr::null_mut())?;
        Ok(())
    }

    pub fn vbus_draw(&self, units: u8) -> io::Result<()> {
        ioctl_value(self.raw(), RAW_VBUS_DRAW, units.into())?;
        Ok(())
    }

    pub fn set_halt(&self, handle: u16) -> io::Result<()> {
        ioctl_value(self.raw(), RAW_EP_SET_HALT, handle.into())?;
        Ok(())
    }

    pub fn clear_halt(&self, handle: u16) -> io::Result<()> {
        ioctl_value(self.raw(), RAW_EP_CLEAR_HALT, handle.into())?;
        Ok(())
    }
}
