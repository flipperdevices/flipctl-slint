//! The folder the takes go in, what is in it, and what the next one is called.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::length;

/// What every take's name starts with, the prototype's own prefix.
const PREFIX: &str = "F1-REC-";

/// The extensions this app writes, which are the files it lists.
pub const EXTENSIONS: [&str; 3] = ["wav", "mp3", "ogg"];

/// `~/Recordings`. `/home` is shared by every profile, so the takes survive a
/// factory reset, and it is where a person looks for them over the network.
pub fn dir() -> PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/home/user".into());
    PathBuf::from(home).join("Recordings")
}

/// What the folder row says: the folder's own name.
pub fn label(dir: &Path) -> String {
    dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// One take in the folder.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    pub name: String,
    pub length: Option<Duration>,
}

/// The takes in `dir`, by name.
pub fn list(dir: &Path) -> Vec<Take> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut takes: Vec<Take> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let ext = Path::new(&name).extension()?.to_str()?.to_ascii_lowercase();
            EXTENSIONS.contains(&ext.as_str()).then(|| Take { length: length::of(&e.path()), name })
        })
        .collect();
    takes.sort_by(|a, b| a.name.cmp(&b.name));
    takes
}

/// The name for the next take, without its extension: one past the highest number
/// any take of any format already has, so a deleted take's number is not handed
/// out again and the newest take is always the last by name.
pub fn next_name(dir: &Path) -> String {
    let highest = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter_map(|e| number(&e.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0);
    format!("{PREFIX}{:03}", highest + 1)
}

/// The counter in a take's file name, `F1-REC-012.wav` is 12.
fn number(file: &str) -> Option<u32> {
    let stem = Path::new(file).file_stem()?.to_str()?;
    stem.strip_prefix(PREFIX)?.parse().ok()
}

/// How the Files list is ordered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Sort {
    NameUp,
    NameDown,
    Longest,
    Shortest,
}

pub const SORTS: [Sort; 4] = [Sort::NameUp, Sort::NameDown, Sort::Longest, Sort::Shortest];

impl Sort {
    pub fn label(self) -> &'static str {
        match self {
            Sort::NameUp => "A-Z",
            Sort::NameDown => "Z-A",
            Sort::Longest => "Longest",
            Sort::Shortest => "Shortest",
        }
    }

    pub fn apply(self, takes: &mut [Take]) {
        let len = |t: &Take| t.length.unwrap_or_default();
        match self {
            Sort::NameUp => takes.sort_by(|a, b| a.name.cmp(&b.name)),
            Sort::NameDown => takes.sort_by(|a, b| b.name.cmp(&a.name)),
            Sort::Longest => takes.sort_by(|a, b| len(b).cmp(&len(a)).then(a.name.cmp(&b.name))),
            Sort::Shortest => takes.sort_by(|a, b| len(a).cmp(&len(b)).then(a.name.cmp(&b.name))),
        }
    }
}

/// Bytes free to this user on the filesystem holding `dir`.
pub fn free(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return None;
    }
    Some(st.f_bavail as u64 * st.f_frsize as u64)
}

/// A length as the Files list shows it, `M:SS` or `H:MM:SS`, `-` when unknown.
pub fn clock(length: Option<Duration>) -> String {
    let Some(length) = length.filter(|l| !l.is_zero()) else {
        return "-".into();
    };
    let s = (length.as_millis() as u64 + 500) / 1000;
    match (s / 3600, s % 3600 / 60, s % 60) {
        (0, m, s) => format!("{m}:{s:02}"),
        (h, m, s) => format!("{h}:{m:02}:{s:02}"),
    }
}

/// Free space as the recording panel shows it: GB with a decimal, whole MB or KB.
pub fn bytes(b: u64) -> String {
    const MB: u64 = 1024 * 1024;
    match b {
        b if b >= 1024 * MB => format!("{:.1} GB", b as f64 / (1024 * MB) as f64),
        b if b >= MB => format!("{} MB", (b as f64 / MB as f64).round()),
        b => format!("{} KB", (b as f64 / 1024.0).round()),
    }
}

/// Time left at a rate, as the recording panel shows it: `2h 5m`, `3m 9s`, `40s`.
pub fn left(seconds: u64) -> String {
    match seconds {
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_counter_goes_past_the_highest_take_of_any_format() {
        let dir = std::env::temp_dir().join(format!("recorder-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(next_name(&dir), "F1-REC-001");
        for f in ["F1-REC-001.wav", "F1-REC-007.ogg", "notes.wav", "F1-REC-x.mp3"] {
            std::fs::write(dir.join(f), b"").unwrap();
        }
        assert_eq!(next_name(&dir), "F1-REC-008");
        let names: Vec<_> = list(&dir).into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["F1-REC-001.wav", "F1-REC-007.ogg", "F1-REC-x.mp3", "notes.wav"]);
    }

    #[test]
    fn a_missing_folder_starts_at_one() {
        assert_eq!(next_name(Path::new("/nonexistent/recorder")), "F1-REC-001");
        assert!(list(Path::new("/nonexistent/recorder")).is_empty());
    }

    #[test]
    fn sorting_by_length_puts_unknown_last_or_first() {
        let take = |n: &str, s: Option<u64>| Take { name: n.into(), length: s.map(Duration::from_secs) };
        let mut takes = vec![take("b", Some(5)), take("a", None), take("c", Some(60))];
        Sort::Longest.apply(&mut takes);
        assert_eq!(takes.iter().map(|t| t.name.as_str()).collect::<String>(), "cba");
        Sort::NameDown.apply(&mut takes);
        assert_eq!(takes.iter().map(|t| t.name.as_str()).collect::<String>(), "cba");
        Sort::Shortest.apply(&mut takes);
        assert_eq!(takes.iter().map(|t| t.name.as_str()).collect::<String>(), "abc");
    }

    #[test]
    fn the_words_for_sizes_and_times() {
        assert_eq!(clock(None), "-");
        assert_eq!(clock(Some(Duration::from_millis(400))), "0:00");
        assert_eq!(clock(Some(Duration::from_secs(65))), "1:05");
        assert_eq!(clock(Some(Duration::from_millis(65_600))), "1:06");
        assert_eq!(clock(Some(Duration::from_secs(3725))), "1:02:05");
        assert_eq!(bytes(3 * 1024 * 1024 * 1024 / 2), "1.5 GB");
        assert_eq!(bytes(5 * 1024 * 1024), "5 MB");
        assert_eq!(bytes(2048), "2 KB");
        assert_eq!(left(7500), "2h 5m");
        assert_eq!(left(189), "3m 9s");
        assert_eq!(left(40), "40s");
    }
}
