//! The panel's frames, handed to the app recording them.
//!
//! flipctl draws or composites every frame that reaches the glass, so it is the one
//! place that can see them; a hosted app sees only its own cage. An app asks with
//!
//! ```text
//! POST /screen-record HTTP/1.1
//! ```
//!
//! on the socket in `ipc`, and the answer is `200` followed by one record per frame
//! for as long as the connection stays open: a 12-byte little-endian header, the
//! `CLOCK_MONOTONIC` time the frame was committed in microseconds (u64) then its
//! width and height (u16 each), and `width * height` bytes of 8-bit grey. A still
//! screen sends nothing. The clock is the one every process on the machine reads, so
//! the app lines its own sound up against these times directly.
//!
//! While the recording app is in front nothing is sent, since it would be filming
//! itself. A record of zero width and height marks the moment it came to the front,
//! so the app can cut that stretch out; the next frame is the moment it left again.
//!
//! The writing is on a thread of its own behind a short queue, and a frame that finds
//! the queue full is dropped, so an app that falls behind costs it frames and never
//! costs the panel a commit.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::Arc;

use crate::pixel::{self, Gray8};

/// Bytes before each frame's pixels.
pub const HEADER: usize = 12;

/// Frames that may wait for the app. Enough to ride out a slow encode, few enough
/// that a stalled app holds a fraction of a megabyte.
const QUEUE_DEPTH: usize = 8;

/// How long the last frames may take to reach an app once the recording is over.
const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// One frame as the app receives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// `CLOCK_MONOTONIC` when the frame was committed, in microseconds.
    pub at_us: u64,
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u8>,
}

/// `CLOCK_MONOTONIC` in microseconds: the clock in each record.
pub fn now_us() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: a valid out-pointer; CLOCK_MONOTONIC always exists on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1_000
}

/// A frame as it goes out: the header and the pixels.
pub fn encode(at_us: u64, width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + pixels.len());
    out.extend_from_slice(&at_us.to_le_bytes());
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(pixels);
    out
}

/// The next record, or `None` at the end of the stream. A record cut short by the
/// stream ending is the end too: flipctl stops mid-frame when it closes.
pub fn read(r: &mut impl Read) -> io::Result<Option<Record>> {
    let mut head = [0u8; HEADER];
    if !fill(r, &mut head)? {
        return Ok(None);
    }
    let at_us = u64::from_le_bytes(head[..8].try_into().unwrap_or_default());
    let width = u16::from_le_bytes([head[8], head[9]]);
    let height = u16::from_le_bytes([head[10], head[11]]);
    let mut pixels = vec![0; usize::from(width) * usize::from(height)];
    if !fill(r, &mut pixels)? {
        return Ok(None);
    }
    Ok(Some(Record { at_us, width, height, pixels }))
}

/// `buf` filled, or false when the stream ended first.
fn fill(r: &mut impl Read, buf: &mut [u8]) -> io::Result<bool> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) => return Ok(false),
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

/// One app's recording, from flipctl's side.
///
/// Dropping it ends the stream, which is how the app learns the recording is over.
pub struct Tap {
    owner: String,
    frames: SyncSender<Vec<u8>>,
    socket: UnixStream,
    gone: Arc<AtomicBool>,
    /// Whether the app is in front. It is when it asks, before anything is recorded,
    /// so that needs no mark.
    held: bool,
}

impl Tap {
    /// Start sending frames on `stream`, which has had its `200` already, to the app
    /// called `owner`.
    pub fn new(stream: UnixStream, owner: String) -> io::Result<Self> {
        let socket = stream.try_clone()?;
        let (frames, queued) = mpsc::sync_channel::<Vec<u8>>(QUEUE_DEPTH);
        let gone = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&gone);
        std::thread::Builder::new().name("screen-rec".into()).spawn(move || {
            let mut stream = stream;
            for record in queued {
                if stream.write_all(&record).is_err() {
                    break;
                }
            }
            flag.store(true, Ordering::Relaxed);
        })?;
        Ok(Self { owner, frames, socket, gone, held: true })
    }

    /// The app recording.
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// A frame as it is committed, with the app in front as it is drawn. Dropped when
    /// the app is behind, and not sent at all while the app is in front.
    ///
    /// The app in front is taken here as well as once a turn: the turn that answers
    /// the app also sends it to the background and draws the screen underneath, and
    /// that frame is the recording's first. Left to the next turn, a still screen would
    /// send nothing until something on it moved.
    pub fn frame(&mut self, front: Option<&str>, pixels: &[Gray8], width: u16, height: u16) {
        self.front(front);
        if !self.held {
            self.send(encode(now_us(), width, height, pixel::as_bytes(pixels)));
        }
    }

    fn send(&self, record: Vec<u8>) {
        if let Err(TrySendError::Disconnected(_)) = self.frames.try_send(record) {
            self.gone.store(true, Ordering::Relaxed);
        }
    }

    /// Which app is in front, told every turn of the loop: coming to the front holds
    /// the recording, with a mark saying when, and leaving lets it go on.
    pub fn front(&mut self, front: Option<&str>) {
        let own = front == Some(self.owner.as_str());
        if own && !self.held {
            self.send(encode(now_us(), 0, 0, &[]));
        }
        self.held = own;
    }

    /// Whether the app hung up, which is the end of the recording. Asked of the socket
    /// as well as the writer, since nothing is written while the app is in front, and
    /// that is where it is when its Stop is pressed.
    pub fn over(&self) -> bool {
        if self.gone.load(Ordering::Relaxed) {
            return true;
        }
        let mut fd =
            libc::pollfd { fd: self.socket.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
        // SAFETY: one valid pollfd, and a zero timeout.
        let ready = unsafe { libc::poll(&mut fd, 1, 0) };
        ready > 0 && fd.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR) != 0
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        // The writer finishes what is queued and closes the stream, which the app reads
        // as the end. An app that has stopped reading holds it a second at most.
        let _ = self.socket.set_write_timeout(Some(WRITE_TIMEOUT));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_reads_back_as_written() {
        let pixels: Vec<u8> = (0..12).collect();
        let mut wire = encode(1_234_567, 4, 3, &pixels);
        wire.extend(encode(1_250_000, 2, 1, &[7, 9]));
        let mut r = &wire[..];
        assert_eq!(
            read(&mut r).unwrap(),
            Some(Record { at_us: 1_234_567, width: 4, height: 3, pixels })
        );
        assert_eq!(read(&mut r).unwrap().map(|f| f.pixels), Some(vec![7, 9]));
        assert_eq!(read(&mut r).unwrap(), None);
    }

    #[test]
    fn a_record_cut_short_is_the_end() {
        let wire = encode(1, 4, 4, &[0; 16]);
        assert_eq!(read(&mut &wire[..5]).unwrap(), None);
        assert_eq!(read(&mut &wire[..HEADER + 3]).unwrap(), None);
    }

    #[test]
    fn frames_reach_the_app_and_dropping_ends_the_stream() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut tap = Tap::new(ours, "Screen Recorder".into()).unwrap();
        tap.front(None);
        let before = now_us();
        tap.frame(None, pixel::from_bytes(&[1, 2, 3, 4, 5, 6]), 3, 2);
        let got = read(&mut theirs).unwrap().unwrap();
        assert_eq!((got.width, got.height, got.pixels), (3, 2, vec![1, 2, 3, 4, 5, 6]));
        assert!(got.at_us >= before && got.at_us <= now_us());
        drop(tap);
        assert_eq!(read(&mut theirs).unwrap(), None);
    }

    /// Nothing while the app is in front, which it is as it asks; one mark when it
    /// comes back to the front after being away, and frames again once it leaves.
    #[test]
    fn the_app_in_front_holds_the_recording_with_a_mark() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut tap = Tap::new(ours, "Screen Recorder".into()).unwrap();
        tap.frame(Some("Screen Recorder"), pixel::from_bytes(&[9; 4]), 2, 2);
        tap.frame(Some("Doom"), pixel::from_bytes(&[1; 4]), 2, 2);
        tap.frame(Some("Screen Recorder"), pixel::from_bytes(&[9; 4]), 2, 2);
        tap.front(Some("Screen Recorder"));
        tap.frame(None, pixel::from_bytes(&[2; 4]), 2, 2);
        drop(tap);
        let mut got = Vec::new();
        while let Some(r) = read(&mut theirs).unwrap() {
            got.push((r.width, r.pixels.first().copied()));
        }
        assert_eq!(got, vec![(2, Some(1)), (0, None), (2, Some(2))]);
    }

    /// The screen drawn as the app goes to the background is the first frame, with
    /// nothing between asking and that frame to lift the hold.
    #[test]
    fn the_screen_under_the_app_is_recorded_at_once() {
        let (ours, mut theirs) = UnixStream::pair().unwrap();
        let mut tap = Tap::new(ours, "Screen Recorder".into()).unwrap();
        tap.frame(None, pixel::from_bytes(&[5; 4]), 2, 2);
        drop(tap);
        assert_eq!(read(&mut theirs).unwrap().map(|r| r.pixels), Some(vec![5; 4]));
    }

    #[test]
    fn an_app_that_hangs_up_while_in_front_ends_it() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let tap = Tap::new(ours, "Screen Recorder".into()).unwrap();
        assert!(!tap.over());
        theirs.shutdown(std::net::Shutdown::Both).unwrap();
        assert!(tap.over(), "noticed with nothing written");
    }

    #[test]
    fn an_app_that_hangs_up_ends_it() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut tap = Tap::new(ours, "Screen Recorder".into()).unwrap();
        tap.front(None);
        drop(theirs);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !tap.over() {
            assert!(std::time::Instant::now() < deadline, "never noticed");
            tap.frame(None, pixel::from_bytes(&[0; 64]), 8, 8);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
}
