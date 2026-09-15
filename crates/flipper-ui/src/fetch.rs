//! Getting a file off a server, and knowing it arrived intact.
//!
//! Two screens need this now: the update screen fetching a system image, and the app
//! manager fetching an app out of the catalogue. They want the same three things, a
//! GET whose answer is text, a download that reports its progress, and a checksum
//! that decides whether what arrived is what was published, so those live here rather
//! than once per screen.
//!
//! curl and sha256sum rather than crates, for the same reason the rest of this binary
//! shells out: the alternative is TLS, certificates and redirects as dependencies,
//! for one GET.

use std::io::Read;
use std::path::{Path, PathBuf};

/// One GET, as text. Seconds rather than minutes: a screen is waiting on this, and a
/// server that has gone away should say so while somebody is still looking.
pub fn get(url: &str) -> Result<String, String> {
    let out = std::process::Command::new("curl")
        .arg("-fsSL")
        .arg("--max-time")
        .arg("20")
        .arg(url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("the server did not answer".into());
    }
    String::from_utf8(out.stdout).map_err(|_| "the answer is not text".into())
}

/// How far a download has got, as the thread doing it reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Progress {
    /// Bytes so far and, when the server said, bytes expected.
    Fetching(u64, u64),
    /// On disk at this path, and verified.
    Ready(PathBuf),
    Failed(String),
}

/// Fetch `url` to `to`, reporting as it goes.
///
/// Written to a `.part` beside the target and renamed at the end, so an interrupted
/// download cannot be mistaken for the real thing: the name exists only once the
/// bytes are all there and the checksum has agreed. `bytes` is what the manifest said
/// to expect, and only gives the progress a denominator.
pub fn download(url: &str, bytes: u64, sha256: &str, to: &Path, mut say: impl FnMut(Progress)) {
    let part = to.with_extension("part");
    let _ = std::fs::remove_file(&part);
    if let Some(parent) = to.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return say(Progress::Failed(format!("cannot make {}: {e}", parent.display())));
        }
    }
    let mut child = match std::process::Command::new("curl")
        .arg("-fsSL")
        .arg("--output")
        .arg(&part)
        .arg("--write-out")
        .arg("%{size_download}")
        .arg(url)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return say(Progress::Failed(format!("curl: {e}"))),
    };

    // Watched from here rather than read from curl: curl's own progress goes to a
    // terminal, and the file's size is the same number without parsing anything.
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_string(&mut out);
                }
                if !status.success() {
                    let _ = std::fs::remove_file(&part);
                    return say(Progress::Failed(format!("download failed ({status})")));
                }
                if let Err(e) = verify(&part, sha256) {
                    let _ = std::fs::remove_file(&part);
                    return say(Progress::Failed(e));
                }
                if let Err(e) = std::fs::rename(&part, to) {
                    return say(Progress::Failed(format!("cannot place the file: {e}")));
                }
                return say(Progress::Ready(to.to_path_buf()));
            }
            Ok(None) => {
                let so_far = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                say(Progress::Fetching(so_far, bytes));
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => return say(Progress::Failed(format!("curl: {e}"))),
        }
    }
}

/// Check what arrived against the hash the manifest published.
///
/// Checked before the file is given its real name, so a corrupt download is never a
/// thing that could be booted or run. Nothing is verified when the manifest published
/// no hash, which is the hand-typed URL case: a URL somebody entered comes with no
/// claim about what is at the other end.
pub fn verify(file: &Path, want: &str) -> Result<(), String> {
    if want.is_empty() {
        return Ok(());
    }
    let out = std::process::Command::new("sha256sum")
        .arg(file)
        .output()
        .map_err(|e| format!("sha256sum: {e}"))?;
    let said = String::from_utf8_lossy(&out.stdout);
    let got = said.split_whitespace().next().unwrap_or("");
    if got.eq_ignore_ascii_case(want) {
        Ok(())
    } else {
        Err("the download does not match its checksum".into())
    }
}

/// A size for the screen, in the few characters a row's right edge allows.
pub fn megabytes(bytes: u64) -> String {
    format!("{:.0}MB", bytes as f64 / 1_048_576.0)
}
