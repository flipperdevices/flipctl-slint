//! One endpoint, forwarded on a thread of its own: device to PC for IN, PC to device
//! for OUT.
//!
//! A request ends on a short packet or when full, and a transfer that ends on a
//! packet boundary has no short packet. A request larger than one packet can then sit
//! on data, or wait for data, that the other side will not send until it hears back.
//! One packet per request always ends, so that is what every endpoint gets except
//! mass storage, which states each transfer's length: its IN data is always followed
//! by a short status, and its OUT reads are sized to the command block's length. Its
//! requests are large and several are in flight, so the device and the PC work at
//! once rather than taking turns.

use std::os::unix::thread::JoinHandleExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::gadget::Gadget;
use crate::host::{Host, Transfer};
use crate::ioctl::{URB_BULK, URB_INTERRUPT};
use crate::pcap::{Capture, KIND_BULK, KIND_INT};
use crate::proxy::{hexes, trace, Counters};

/// Mass storage: requests this large, this many in flight.
pub const CHUNK: usize = 65536;
pub const DEPTH: usize = 4;

/// What the endpoint's thread and the control thread share.
pub struct Shared {
    pub host: Arc<Host>,
    pub gadget: Arc<Gadget>,
    pub capture: Arc<Capture>,
    pub counters: Arc<Counters>,
}

struct Flags {
    stopping: AtomicBool,
    halted: Mutex<bool>,
    unhalted: Condvar,
    /// The URBs' addresses, which the control thread discards to stop the pipe.
    keys: Mutex<Vec<usize>>,
}

pub struct Pipe {
    pub addr: u8,
    pub handle: u16,
    flags: Arc<Flags>,
    thread: Option<JoinHandle<()>>,
    gadget: Arc<Gadget>,
    host: Arc<Host>,
}

impl Pipe {
    /// Enable the endpoint on the gadget and start forwarding it.
    pub fn start(shared: &Arc<Shared>, desc: &[u8; 7], bulk_only: bool) -> std::io::Result<Pipe> {
        let addr = desc[2];
        let handle = shared.gadget.enable(desc)?;
        let maxp = (u16::from_le_bytes([desc[4], desc[5]]) & 0x7FF) as usize;
        let bulk = desc[3] & 3 == 2;
        let bulk_only = bulk && bulk_only;
        let (size, depth) = if bulk_only { (CHUNK, DEPTH) } else { (maxp.max(1), 1) };
        let kind = if bulk { URB_BULK } else { URB_INTERRUPT };
        let transfers: Vec<Transfer> =
            (0..depth).map(|_| Transfer::new(addr, kind, size)).collect();
        let flags = Arc::new(Flags {
            stopping: AtomicBool::new(false),
            halted: Mutex::new(false),
            unhalted: Condvar::new(),
            keys: Mutex::new(transfers.iter().map(Transfer::key).collect()),
        });
        trace(&format!("enabled ep{addr:02x} as handle {handle}, maxp {maxp}, {depth} x {size}"));
        let mut worker = Worker {
            shared: shared.clone(),
            flags: flags.clone(),
            addr,
            handle,
            maxp,
            size,
            bulk_only,
            pcap_kind: if bulk { KIND_BULK } else { KIND_INT },
            transfers,
            remaining: 0,
            buf: Vec::with_capacity(8 + size),
        };
        let thread = std::thread::Builder::new()
            .name(format!("ep{addr:02x}"))
            .spawn(move || worker.run())?;
        Ok(Pipe {
            addr,
            handle,
            flags,
            thread: Some(thread),
            gadget: shared.gadget.clone(),
            host: shared.host.clone(),
        })
    }

    /// The PC cleared a halt: let the thread go on.
    pub fn unhalt(&self) {
        *self.flags.halted.lock().unwrap() = false;
        self.flags.unhalted.notify_all();
    }

    /// Stop the thread and disable the endpoint. Its URBs are discarded and its
    /// thread signalled, since a raw-gadget transfer only gives way to a signal.
    pub fn stop(mut self) {
        trace(&format!("stopping ep{:02x}", self.addr));
        self.flags.stopping.store(true, Ordering::SeqCst);
        self.unhalt();
        for key in self.flags.keys.lock().unwrap().iter() {
            self.host.discard(*key);
        }
        if let Some(thread) = self.thread.take() {
            let mut since = Instant::now();
            while !thread.is_finished() {
                unsafe { libc::pthread_kill(thread.as_pthread_t(), libc::SIGUSR1) };
                std::thread::sleep(Duration::from_millis(20));
                if since.elapsed() > Duration::from_secs(1) {
                    trace(&format!("ep{:02x} still not stopped", self.addr));
                    since = Instant::now();
                }
            }
            let _ = thread.join();
        }
        if let Err(e) = self.gadget.disable(self.handle) {
            trace(&format!("disable ep{:02x}: {e}", self.addr));
        }
        trace(&format!("stopped ep{:02x}", self.addr));
    }
}

/// The bytes of OUT data a mass storage command block announces, or 0 for anything
/// that is not one, or for one whose data comes IN.
pub fn data_after(packet: &[u8]) -> usize {
    if packet.len() == 31 && &packet[..4] == b"USBC" && packet[12] & 0x80 == 0 {
        u32::from_le_bytes(packet[8..12].try_into().unwrap()) as usize
    } else {
        0
    }
}

struct Worker {
    shared: Arc<Shared>,
    flags: Arc<Flags>,
    addr: u8,
    handle: u16,
    maxp: usize,
    size: usize,
    bulk_only: bool,
    pcap_kind: u8,
    transfers: Vec<Transfer>,
    /// Bytes of a mass storage OUT transfer still to come.
    remaining: usize,
    /// The gadget transfer's header and data, reused.
    buf: Vec<u8>,
}

impl Worker {
    fn stopping(&self) -> bool {
        self.flags.stopping.load(Ordering::SeqCst)
    }

    fn run(&mut self) {
        let result = if self.addr & 0x80 != 0 { self.run_in() } else { self.run_out() };
        if let Err(e) = result {
            trace(&format!("ep{:02x} ended: {e}", self.addr));
        }
        self.cancel_busy();
    }

    fn submit(&mut self, k: usize, data: &[u8]) -> std::io::Result<()> {
        let s = &self.shared;
        let ident = s.capture.next_id();
        let t = &mut self.transfers[k];
        t.ident = ident;
        let asked = if self.addr & 0x80 != 0 { t.capacity() } else { data.len() } as u32;
        s.capture.write(
            ident,
            b'S',
            self.pcap_kind,
            self.addr,
            -libc::EINPROGRESS,
            asked,
            data,
            None,
        );
        s.host.submit(t, data)
    }

    /// Wait for one URB: its status, or None once the device is gone or we stop.
    fn finish(&mut self, k: usize) -> Option<i32> {
        let t = &mut self.transfers[k];
        t.done.wait();
        t.busy = false;
        if self.flags.stopping.load(Ordering::SeqCst)
            || self.shared.host.gone.load(Ordering::SeqCst)
        {
            return None;
        }
        let status = t.status();
        let into = self.addr & 0x80 != 0;
        let data = if into && status == 0 { t.data() } else { &[] };
        self.shared.capture.write(
            t.ident,
            b'C',
            self.pcap_kind,
            self.addr,
            status,
            t.actual() as u32,
            data,
            None,
        );
        if status < 0 && status != -libc::EPIPE {
            trace(&format!("ep{:02x} error {}", self.addr, -status));
            let addr = self.addr;
            self.shared.counters.line(|| format!("{addr:02x} error {}", -status));
        }
        Some(status)
    }

    /// Take back every URB still queued and wait for each. After a stall the endpoint
    /// is halted, so none of them can hold data. With the device gone nothing more
    /// completes, and the URBs are left to the kernel rather than freed under it.
    fn cancel_busy(&mut self) {
        for t in self.transfers.iter_mut().filter(|t| t.busy) {
            self.shared.host.discard(t.key());
        }
        if self.shared.host.gone.load(Ordering::SeqCst) {
            for t in self.transfers.drain(..).filter(|t| t.busy) {
                std::mem::forget(t);
            }
            return;
        }
        for t in self.transfers.iter_mut().filter(|t| t.busy) {
            t.done.wait();
            t.busy = false;
        }
    }

    /// The device stalled: the rest of the queue goes, the PC is stalled too, and we
    /// wait for it to clear the halt. False if we stop meanwhile.
    fn stalled(&mut self) -> bool {
        trace(&format!("ep{:02x} stall", self.addr));
        let addr = self.addr;
        self.shared.counters.line(|| format!("{addr:02x} stall"));
        self.remaining = 0;
        self.cancel_busy();
        *self.flags.halted.lock().unwrap() = true;
        if self.shared.gadget.set_halt(self.handle).is_err() {
            return !self.stopping();
        }
        let mut halted = self.flags.halted.lock().unwrap();
        while *halted && !self.stopping() {
            halted = self.flags.unhalted.wait(halted).unwrap();
        }
        !self.stopping()
    }

    fn run_in(&mut self) -> std::io::Result<()> {
        let depth = self.transfers.len();
        for k in 0..depth {
            self.submit(k, &[])?;
        }
        let mut k = 0;
        while !self.stopping() {
            let Some(status) = self.finish(k) else { return Ok(()) };
            if status == -libc::EPIPE {
                if !self.stalled() {
                    return Ok(());
                }
                for j in 0..depth {
                    self.submit((k + j) % depth, &[])?;
                }
                continue;
            }
            if status < 0 {
                std::thread::sleep(Duration::from_millis(50));
            } else {
                let t = &self.transfers[k];
                let n = t.actual();
                self.shared.counters.count(self.addr, n);
                let addr = self.addr;
                self.shared.counters.line(|| format!("{addr:02x} IN  {n:>5} {}", hexes(t.data())));
                let zero = n > 0 && n < self.size && n.is_multiple_of(self.maxp);
                let mut buf = std::mem::take(&mut self.buf);
                let written = self.shared.gadget.write(self.handle, &mut buf, t.data(), zero);
                self.buf = buf;
                written?;
            }
            self.submit(k, &[])?;
            k = (k + 1) % depth;
        }
        Ok(())
    }

    /// PC to device. Within a mass storage data phase the chunks go to the device
    /// without waiting for each other; the queue is emptied at its end, so a stall
    /// is answered before the PC's next command.
    fn run_out(&mut self) -> std::io::Result<()> {
        let depth = self.transfers.len();
        let mut k = 0;
        let mut buf = std::mem::take(&mut self.buf);
        while !self.stopping() {
            let size = if self.remaining > 0 { self.size.min(self.remaining) } else { self.size };
            let n = self.shared.gadget.read(self.handle, &mut buf, size)?;
            let in_data_phase = self.remaining > 0;
            if in_data_phase {
                // A short packet ends the transfer early
                self.remaining = if n < size { 0 } else { self.remaining - n };
            } else if self.bulk_only {
                self.remaining = data_after(&buf[8..8 + n]);
            }
            self.shared.counters.count(self.addr, n);
            let addr = self.addr;
            self.shared
                .counters
                .line(|| format!("{addr:02x} OUT {n:>5} {}", hexes(&buf[8..8 + n])));
            if self.transfers[k].busy {
                match self.finish(k) {
                    None => return Ok(()),
                    Some(s) if s == -libc::EPIPE => {
                        if !self.stalled() {
                            return Ok(());
                        }
                        continue;
                    }
                    Some(_) => {}
                }
            }
            self.submit(k, &buf[8..8 + n])?;
            k = (k + 1) % depth;
            if !(in_data_phase && self.remaining > 0) && !self.drain(k) {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Wait for every OUT URB, oldest first: after a stall the ones behind it never
    /// complete on their own. False if we stop meanwhile.
    fn drain(&mut self, oldest: usize) -> bool {
        let depth = self.transfers.len();
        for j in 0..depth {
            let k = (oldest + j) % depth;
            if self.transfers[k].busy {
                match self.finish(k) {
                    None => return false,
                    Some(s) if s == -libc::EPIPE => return self.stalled(),
                    Some(_) => {}
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::data_after;

    fn cbw(length: u32, flags: u8) -> Vec<u8> {
        let mut c = b"USBC".to_vec();
        c.extend(7u32.to_le_bytes());
        c.extend(length.to_le_bytes());
        c.push(flags);
        c.extend([0u8; 18]);
        c
    }

    #[test]
    fn a_write_command_announces_its_data() {
        assert_eq!(data_after(&cbw(122880, 0)), 122880);
        assert_eq!(data_after(&cbw(122880, 0x80)), 0, "a read's data comes IN");
        assert_eq!(data_after(&cbw(0, 0)), 0);
        let mut bad = cbw(512, 0);
        bad[0] = b'X';
        assert_eq!(data_after(&bad), 0);
        assert_eq!(data_after(&[0u8; 512]), 0, "data, not a command");
    }
}
