//! Log lines, written to the kernel's log and nowhere else.
//!
//! `dmesg` is where a boot gets read back: it timestamps every line and holds our
//! messages in order with the driver messages around them. So that is the only place
//! these go. No second sink, because two sinks means two formats and a boot log that
//! reads differently depending on where it was caught.
//!
//! Two rules `/dev/kmsg` imposes, both learned the hard way. It makes **one record per
//! write**, so a line must be formatted before it is written or it arrives in pieces:
//!
//! ```text
//! [3.087747] panel
//! [3.087822] 256
//! [3.088092] x
//! [3.088318] 144
//! ```
//!
//! And a failed write there panics `eprintln!`, which killed the menu mid-line. Hence
//! one write per line, and every error dropped: a log able to take the program down is
//! worse than no log at all.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::OnceLock;

/// The kernel log, opened once. None where it is not ours to write, which is anything
/// not running as root: flipctl on a desktop, or the tests, where these lines are of no
/// use to anyone anyway.
fn kmsg() -> Option<&'static File> {
    static KMSG: OnceLock<Option<File>> = OnceLock::new();
    KMSG.get_or_init(|| OpenOptions::new().write(true).open("/dev/kmsg").ok())
        .as_ref()
}

/// Write one line, as one write.
pub fn write_line(text: &str) {
    let Some(mut sink) = kmsg() else { return };
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    let _ = sink.write_all(line.as_bytes());
}

/// `eprintln!`, but as a single write, to the kernel log, and unable to panic.
///
/// Same formatting, so converting a call is only the name.
#[macro_export]
macro_rules! logline {
    ($($arg:tt)*) => { $crate::log::write_line(&format!($($arg)*)) };
}
