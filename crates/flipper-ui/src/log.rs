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

use std::fs::OpenOptions;
use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, OnceLock};

/// Where a line goes: the node itself where we may write it, else one long-lived `sudo` that
/// may, else nowhere.
///
/// The node is root-only, and flipctl runs as `user`, so opening it directly is exactly the
/// case that used to end here in silence. sudo is how everything else privileged in this
/// crate gets done -- `boot::tool` puts every profile tool behind it -- so the log does the
/// same rather than asking for the node's permissions to be changed for it.
///
/// One child for the life of the program, not one per line: `cat` writes each read straight
/// through, and our writes are a line each, so a line still arrives as one record, which is
/// the rule `/dev/kmsg` imposes. A process per line would also turn a log call into
/// something slow enough to matter in a boot path.
enum Sink {
    Node(std::fs::File),
    Sudo(Child),
}

fn sink() -> Option<&'static Mutex<Sink>> {
    static SINK: OnceLock<Option<Mutex<Sink>>> = OnceLock::new();
    SINK.get_or_init(|| {
        if let Ok(f) = OpenOptions::new().write(true).open("/dev/kmsg") {
            return Some(Mutex::new(Sink::Node(f)));
        }
        // No -n: sudo here is the same passwordless sudo the profile tools already rely on,
        // and a prompt would hang a program that has no terminal. Failure is silence, as
        // everywhere else in this module.
        Command::new("sudo")
            .args(["sh", "-c", "cat > /dev/kmsg"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .ok()
            .map(|child| Mutex::new(Sink::Sudo(child)))
    })
    .as_ref()
}

/// Write one line, as one write.
pub fn write_line(text: &str) {
    let Some(sink) = sink() else { return };
    let Ok(mut sink) = sink.lock() else { return };
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    let bytes = line.as_bytes();
    let _ = match &mut *sink {
        Sink::Node(f) => f.write_all(bytes),
        Sink::Sudo(child) => match child.stdin.as_mut() {
            Some(pipe) => pipe.write_all(bytes).and_then(|()| pipe.flush()),
            None => Ok(()),
        },
    };
}

/// `eprintln!`, but as a single write, to the kernel log, and unable to panic.
///
/// Same formatting, so converting a call is only the name.
#[macro_export]
macro_rules! logline {
    ($($arg:tt)*) => { $crate::log::write_line(&format!($($arg)*)) };
}
