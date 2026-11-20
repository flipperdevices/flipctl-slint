//! The device, through usbfs: control transfers made at once, everything else as
//! asynchronous URBs a reaper thread hands back.

use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::descriptors::{configs, downgrade_config};
use crate::ioctl::*;
use crate::sysfs::{device, read};

/// Completion of one URB, waited on by the thread that sent it.
#[derive(Default)]
pub struct Done {
    finished: Mutex<bool>,
    cond: Condvar,
}

impl Done {
    fn set(&self) {
        *self.finished.lock().unwrap() = true;
        self.cond.notify_all();
    }

    fn clear(&self) {
        *self.finished.lock().unwrap() = false;
    }

    pub fn wait(&self) {
        let mut done = self.finished.lock().unwrap();
        while !*done {
            done = self.cond.wait(done).unwrap();
        }
    }
}

/// One URB and its buffer. The URB is boxed and never moves, since usbfs keeps its
/// address and writes the status back there when it is reaped.
pub struct Transfer {
    urb: Box<Urb>,
    buf: Vec<u8>,
    pub done: Arc<Done>,
    /// Submitted and not yet waited for.
    pub busy: bool,
    /// Its capture records' id.
    pub ident: u64,
}

// The URB's raw pointers point into this transfer's own buffer
unsafe impl Send for Transfer {}

impl Transfer {
    pub fn new(ep: u8, kind: u8, size: usize) -> Transfer {
        let mut buf = vec![0u8; size.max(1)];
        let urb = Box::new(Urb {
            kind,
            endpoint: ep,
            status: 0,
            flags: 0,
            buffer: buf.as_mut_ptr().cast(),
            buffer_length: size as i32,
            actual_length: 0,
            start_frame: 0,
            stream_id: 0,
            error_count: 0,
            signr: 0,
            usercontext: std::ptr::null_mut(),
        });
        Transfer { urb, buf, done: Arc::new(Done::default()), busy: false, ident: 0 }
    }

    pub fn key(&self) -> usize {
        &*self.urb as *const Urb as usize
    }

    /// Status and length once waited for. Read through a volatile load: the kernel
    /// wrote them, not this program.
    pub fn status(&self) -> i32 {
        unsafe { std::ptr::read_volatile(&self.urb.status) }
    }

    pub fn actual(&self) -> usize {
        unsafe { std::ptr::read_volatile(&self.urb.actual_length) }.max(0) as usize
    }

    pub fn data(&self) -> &[u8] {
        &self.buf[..self.actual().min(self.buf.len())]
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }
}

pub struct Host {
    fd: OwnedFd,
    pub bus: u16,
    pub dev: u8,
    pub speed: String,
    descriptors: Vec<u8>,
    pub active: u8,
    pending: Mutex<HashMap<usize, Arc<Done>>>,
    claimed: Mutex<HashSet<u32>>,
    pub gone: AtomicBool,
}

impl Host {
    pub fn open(port: &str) -> io::Result<Arc<Host>> {
        let sysfs = device(port);
        let num = |name: &str| read(&sysfs.join(name)).parse::<u32>().unwrap_or(0);
        let (bus, dev) = (num("busnum") as u16, num("devnum") as u8);
        let path = PathBuf::from(format!("/dev/bus/usb/{bus:03}/{dev:03}"));
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let host = Arc::new(Host {
            fd: file.into(),
            bus,
            dev,
            speed: read(&sysfs.join("speed")),
            descriptors: std::fs::read(sysfs.join("descriptors")).unwrap_or_default(),
            active: num("bConfigurationValue") as u8,
            pending: Mutex::new(HashMap::new()),
            claimed: Mutex::new(HashSet::new()),
            gone: AtomicBool::new(false),
        });
        let reaper = host.clone();
        std::thread::Builder::new().name("reaper".into()).spawn(move || reaper.reap())?;
        Ok(host)
    }

    fn raw(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    pub fn superspeed(&self) -> bool {
        self.speed.parse::<f64>().unwrap_or(0.0) >= 5000.0
    }

    /// The config with this value, as the PC is shown it.
    pub fn config(&self, value: u8) -> Option<Vec<u8>> {
        let c = configs(&self.descriptors).into_iter().find(|c| c.len() >= 6 && c[5] == value)?;
        Some(if self.superspeed() { downgrade_config(c) } else { c.to_vec() })
    }

    /// The config at this index, as the PC is shown it, for a GET_DESCRIPTOR.
    pub fn config_at(&self, index: usize) -> Option<Vec<u8>> {
        let c = *configs(&self.descriptors).get(index)?;
        Some(if self.superspeed() { downgrade_config(c) } else { c.to_vec() })
    }

    pub fn control(&self, setup: &[u8; 8], out: &[u8]) -> io::Result<Vec<u8>> {
        let length = u16::from_le_bytes([setup[6], setup[7]]);
        let mut buf = vec![0u8; usize::from(length).max(1)];
        let into = setup[0] & 0x80 != 0;
        if !into {
            let n = out.len().min(buf.len());
            buf[..n].copy_from_slice(&out[..n]);
        }
        let mut ct = CtrlTransfer {
            request_type: setup[0],
            request: setup[1],
            value: u16::from_le_bytes([setup[2], setup[3]]),
            index: u16::from_le_bytes([setup[4], setup[5]]),
            length,
            timeout: 5000,
            data: buf.as_mut_ptr().cast(),
        };
        let n = ioctl(self.raw(), USBDEVFS_CONTROL, (&mut ct as *mut CtrlTransfer).cast())?;
        buf.truncate(if into { n as usize } else { 0 });
        Ok(buf)
    }

    /// Take an interface from whatever driver has it.
    pub fn claim(&self, number: u32) -> io::Result<()> {
        let mut dc = DisconnectClaim { interface: number, flags: 0, driver: [0; 256] };
        ioctl(self.raw(), USBDEVFS_DISCONNECT_CLAIM, (&mut dc as *mut DisconnectClaim).cast())?;
        self.claimed.lock().unwrap().insert(number);
        Ok(())
    }

    pub fn claimed(&self, number: u32) -> bool {
        self.claimed.lock().unwrap().contains(&number)
    }

    pub fn release_all(&self) {
        for mut n in self.claimed.lock().unwrap().drain() {
            let _ = ioctl(self.raw(), USBDEVFS_RELEASEINTERFACE, (&mut n as *mut u32).cast());
        }
    }

    pub fn set_configuration(&self, value: u8) -> io::Result<()> {
        let mut v = i32::from(value);
        ioctl(self.raw(), USBDEVFS_SETCONFIGURATION, (&mut v as *mut i32).cast())?;
        Ok(())
    }

    pub fn set_interface(&self, number: u32, alt: u32) -> io::Result<()> {
        let mut arg = [number, alt];
        ioctl(self.raw(), USBDEVFS_SETINTERFACE, arg.as_mut_ptr().cast())?;
        Ok(())
    }

    pub fn clear_halt(&self, ep: u8) -> io::Result<()> {
        let mut v = u32::from(ep);
        ioctl(self.raw(), USBDEVFS_CLEAR_HALT, (&mut v as *mut u32).cast())?;
        Ok(())
    }

    /// Send a URB: an IN one asks for its whole buffer, an OUT one sends `data`.
    pub fn submit(&self, t: &mut Transfer, data: &[u8]) -> io::Result<()> {
        let into = t.urb.endpoint & 0x80 != 0;
        if !into {
            t.buf[..data.len()].copy_from_slice(data);
        }
        t.urb.buffer_length = if into { t.buf.len() as i32 } else { data.len() as i32 };
        t.urb.status = 0;
        t.urb.actual_length = 0;
        t.done.clear();
        let key = t.key();
        self.pending.lock().unwrap().insert(key, t.done.clone());
        if let Err(e) = ioctl(self.raw(), USBDEVFS_SUBMITURB, key as *mut libc::c_void) {
            self.pending.lock().unwrap().remove(&key);
            return Err(e);
        }
        t.busy = true;
        Ok(())
    }

    /// Ask for a URB back early. It is reaped like any other, with its status saying so.
    pub fn discard(&self, key: usize) {
        let _ = ioctl(self.raw(), USBDEVFS_DISCARDURB, key as *mut libc::c_void);
    }

    fn reap(&self) {
        loop {
            let mut got: *mut libc::c_void = std::ptr::null_mut();
            match ioctl(self.raw(), USBDEVFS_REAPURB, (&mut got as *mut *mut libc::c_void).cast()) {
                Ok(_) => {
                    if let Some(done) = self.pending.lock().unwrap().remove(&(got as usize)) {
                        done.set();
                    }
                }
                Err(e) if e.raw_os_error() == Some(libc::EINTR) => continue,
                Err(_) => {
                    // The device is gone: nothing more completes, so wake everyone
                    self.gone.store(true, Ordering::SeqCst);
                    for (_, done) in self.pending.lock().unwrap().drain() {
                        done.set();
                    }
                    return;
                }
            }
        }
    }
}
