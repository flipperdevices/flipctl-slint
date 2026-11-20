//! What goes through, as usbmon's binary records (LINKTYPE_USB_LINUX_MMAPPED), to a
//! pcap file or to Wireshark over TCP.
//!
//! Wireshark reads a pcap stream from a socket as the interface `TCP@host:port`.
//! Each viewer gets the file header and then records as they happen; one that cannot
//! keep up loses records rather than slowing the proxy, and is told by the count.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

/// The port Remote Wireshark listens on.
pub const WIRESHARK_PORT: u16 = 19000;

/// usbmon's transfer types.
pub const KIND_INT: u8 = 1;
pub const KIND_CTRL: u8 = 2;
pub const KIND_BULK: u8 = 3;

/// Records a viewer may fall behind by before it loses some.
const BACKLOG: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Off,
    File,
    Wireshark,
}

impl Mode {
    pub fn word(self) -> &'static str {
        match self {
            Mode::Off => "off",
            Mode::File => "file",
            Mode::Wireshark => "wireshark",
        }
    }

    pub fn from_word(word: &str) -> Option<Mode> {
        [Mode::Off, Mode::File, Mode::Wireshark].into_iter().find(|m| m.word() == word)
    }
}

pub fn file_header() -> [u8; 24] {
    let mut h = [0u8; 24];
    h[0..4].copy_from_slice(&0xA1B2C3D4u32.to_le_bytes());
    h[4..6].copy_from_slice(&2u16.to_le_bytes());
    h[6..8].copy_from_slice(&4u16.to_le_bytes());
    h[16..20].copy_from_slice(&262144u32.to_le_bytes());
    h[20..24].copy_from_slice(&220u32.to_le_bytes());
    h
}

/// One record: the pcap packet header, usbmon's 64 bytes, then the data.
#[allow(clippy::too_many_arguments)]
pub fn record(
    ident: u64,
    event: u8,
    kind: u8,
    ep: u8,
    dev: u8,
    bus: u16,
    status: i32,
    urb_len: u32,
    data: &[u8],
    setup: Option<&[u8; 8]>,
    now: SystemTime,
) -> Vec<u8> {
    let since = now.duration_since(UNIX_EPOCH).unwrap_or_default();
    let (sec, usec) = (since.as_secs(), since.subsec_micros());
    let len = (64 + data.len()) as u32;
    let mut r = Vec::with_capacity(16 + len as usize);
    r.extend((sec as u32).to_le_bytes());
    r.extend(usec.to_le_bytes());
    r.extend(len.to_le_bytes());
    r.extend(len.to_le_bytes());
    r.extend(ident.to_le_bytes());
    r.extend([event, kind, ep, dev]);
    r.extend(bus.to_le_bytes());
    r.push(if setup.is_some() { 0 } else { b'-' });
    r.push(if data.is_empty() { b'<' } else { 0 });
    r.extend((sec as i64).to_le_bytes());
    r.extend((usec as i32).to_le_bytes());
    r.extend(status.to_le_bytes());
    r.extend(urb_len.to_le_bytes());
    r.extend((data.len() as u32).to_le_bytes());
    r.extend(setup.copied().unwrap_or([0; 8]));
    r.extend([0u8; 16]);
    r.extend(data);
    r
}

struct Viewer {
    tx: SyncSender<Arc<Vec<u8>>>,
}

/// The capture: off, to a file made on its first record, or to Wireshark viewers.
pub struct Capture {
    path: PathBuf,
    bus: u16,
    dev: u8,
    ids: AtomicU64,
    inner: Mutex<Inner>,
    /// Records a viewer could not take.
    pub lost: Arc<AtomicU64>,
}

struct Inner {
    mode: Mode,
    file: Option<BufWriter<File>>,
    viewers: Arc<Mutex<Vec<Viewer>>>,
    listening: bool,
}

impl Capture {
    pub fn new(path: PathBuf, bus: u16, dev: u8, mode: Mode) -> Capture {
        let capture = Capture {
            path,
            bus,
            dev,
            ids: AtomicU64::new(0),
            inner: Mutex::new(Inner {
                mode: Mode::Off,
                file: None,
                viewers: Arc::new(Mutex::new(Vec::new())),
                listening: false,
            }),
            lost: Arc::new(AtomicU64::new(0)),
        };
        capture.set_mode(mode);
        capture
    }

    pub fn next_id(&self) -> u64 {
        self.ids.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn set_mode(&self, mode: Mode) {
        let mut inner = self.inner.lock().unwrap();
        inner.mode = mode;
        if mode != Mode::Wireshark {
            // Dropping a viewer's sender ends its thread and closes its socket
            inner.viewers.lock().unwrap().clear();
        }
        if mode == Mode::Wireshark && !inner.listening {
            inner.listening = true;
            let viewers = inner.viewers.clone();
            std::thread::Builder::new()
                .name("wireshark".into())
                .spawn(move || listen(viewers))
                .ok();
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn write(
        &self,
        ident: u64,
        event: u8,
        kind: u8,
        ep: u8,
        status: i32,
        urb_len: u32,
        data: &[u8],
        setup: Option<&[u8; 8]>,
    ) {
        let mut inner = self.inner.lock().unwrap();
        if inner.mode == Mode::Off {
            return;
        }
        let r = record(
            ident,
            event,
            kind,
            ep,
            self.dev,
            self.bus,
            status,
            urb_len,
            data,
            setup,
            SystemTime::now(),
        );
        match inner.mode {
            Mode::File => {
                if inner.file.is_none() {
                    if let Ok(f) = File::create(&self.path) {
                        let mut w = BufWriter::with_capacity(1 << 20, f);
                        let _ = w.write_all(&file_header());
                        inner.file = Some(w);
                    }
                }
                if let Some(f) = inner.file.as_mut() {
                    let _ = f.write_all(&r);
                }
            }
            Mode::Wireshark => {
                let r = Arc::new(r);
                inner.viewers.lock().unwrap().retain(|v| match v.tx.try_send(r.clone()) {
                    Ok(()) => true,
                    Err(TrySendError::Full(_)) => {
                        self.lost.fetch_add(1, Ordering::Relaxed);
                        true
                    }
                    Err(TrySendError::Disconnected(_)) => false,
                });
            }
            Mode::Off => {}
        }
    }

    pub fn flush(&self) {
        if let Some(f) = self.inner.lock().unwrap().file.as_mut() {
            let _ = f.flush();
        }
    }

    pub fn viewers(&self) -> usize {
        self.inner.lock().unwrap().viewers.lock().unwrap().len()
    }
}

/// Take viewers for as long as the process lives; while the mode is not Wireshark the
/// list is cleared, so a viewer connecting then sees the header and nothing more
/// until it is.
fn listen(viewers: Arc<Mutex<Vec<Viewer>>>) {
    let Ok(listener) = TcpListener::bind(("0.0.0.0", WIRESHARK_PORT)) else { return };
    for stream in listener.incoming().flatten() {
        let (tx, rx) = sync_channel::<Arc<Vec<u8>>>(BACKLOG);
        viewers.lock().unwrap().push(Viewer { tx });
        std::thread::Builder::new().name("viewer".into()).spawn(move || serve(stream, rx)).ok();
    }
}

fn serve(stream: TcpStream, rx: std::sync::mpsc::Receiver<Arc<Vec<u8>>>) {
    let _ = stream.set_nodelay(true);
    let mut w = BufWriter::new(stream);
    if w.write_all(&file_header()).and_then(|_| w.flush()).is_err() {
        return;
    }
    while let Ok(r) = rx.recv() {
        if w.write_all(&r).is_err() {
            return;
        }
        // Batch what is already queued, then send
        while let Ok(more) = rx.try_recv() {
            if w.write_all(&more).is_err() {
                return;
            }
        }
        if w.flush().is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_record_is_usbmons_64_bytes_after_the_packet_header() {
        let at = UNIX_EPOCH + Duration::new(1_000, 5_000);
        let setup = [0x80, 6, 0, 1, 0, 0, 18, 0];
        let r = record(7, b'S', KIND_CTRL, 0x80, 3, 1, -115, 18, &[], Some(&setup), at);
        assert_eq!(r.len(), 16 + 64);
        assert_eq!(&r[8..12], &64u32.to_le_bytes(), "captured length");
        assert_eq!(&r[16..24], &7u64.to_le_bytes(), "the URB id");
        assert_eq!(&r[24..28], &[b'S', KIND_CTRL, 0x80, 3]);
        assert_eq!(r[30], 0, "setup present");
        assert_eq!(r[31], b'<', "no data");
        assert_eq!(&r[40..44], &5i32.to_le_bytes(), "microseconds");
        assert_eq!(&r[44..48], &(-115i32).to_le_bytes());
        assert_eq!(&r[56..64], &setup);
        let r = record(7, b'C', KIND_BULK, 0x81, 3, 1, 0, 4, &[1, 2, 3, 4], None, at);
        assert_eq!(r.len(), 16 + 64 + 4);
        assert_eq!((r[30], r[31]), (b'-', 0));
        assert_eq!(&r[80..], &[1, 2, 3, 4]);
    }

    #[test]
    fn the_file_header_is_usbmon_mmapped() {
        let h = file_header();
        assert_eq!(&h[..4], &[0xd4, 0xc3, 0xb2, 0xa1]);
        assert_eq!(&h[20..], &220u32.to_le_bytes());
    }

    #[test]
    fn modes_have_words_for_the_pipe() {
        for m in [Mode::Off, Mode::File, Mode::Wireshark] {
            assert_eq!(Mode::from_word(m.word()), Some(m));
        }
        assert_eq!(Mode::from_word("on"), None);
    }
}
