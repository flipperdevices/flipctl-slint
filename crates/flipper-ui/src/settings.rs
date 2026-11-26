//! What a person sets in flipctl and expects back after a reboot.
//!
//! One file, `~/.config/flipctl/settings.conf`, a `key = value` line per setting. Read
//! once and kept in memory: menus ask on every redraw.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

const DEBUG_MODE: &str = "debug_mode";

fn path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("flipctl").join("settings.conf"))
}

fn debug_flag() -> &'static AtomicBool {
    static FLAG: OnceLock<AtomicBool> = OnceLock::new();
    FLAG.get_or_init(|| {
        let text = path().and_then(|p| std::fs::read_to_string(p).ok()).unwrap_or_default();
        AtomicBool::new(parse_bool(&text, DEBUG_MODE).unwrap_or(false))
    })
}

/// Whether debug mode is on: the main menu shows Testing only then.
pub fn debug_mode() -> bool {
    debug_flag().load(Ordering::Relaxed)
}

/// Turn debug mode on or off and save it. A failed save keeps the new value for
/// this session and says so on stderr.
pub fn set_debug_mode(on: bool) {
    debug_flag().store(on, Ordering::Relaxed);
    set(DEBUG_MODE, if on { "true" } else { "false" });
}

/// The saved value for `key`, if any.
pub fn get(key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path()?).ok()?;
    value(&text, key).map(str::to_string)
}

/// Save `val` for `key`. A failed save says so on stderr.
pub fn set(key: &str, val: &str) {
    let Some(path) = path() else {
        eprintln!("settings       no HOME, {key} not saved");
        return;
    };
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    if let Err(e) = write(&path, &with_value(&old, key, val)) {
        eprintln!("settings       {}: {e}", path.display());
    }
}

/// Written beside the target and renamed over it, so a power cut leaves the old
/// file or the new one and never half of either.
fn write(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("conf.new");
    std::fs::write(&tmp, text)?;
    std::fs::File::open(&tmp)?.sync_all()?;
    std::fs::rename(&tmp, path)
}

fn value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    text.lines().rev().find_map(|line| {
        let (k, v) = line.split_once('=')?;
        (k.trim() == key).then(|| v.trim())
    })
}

fn parse_bool(text: &str, key: &str) -> Option<bool> {
    match value(text, key)? {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// `text` with `key` set to `val`, other lines kept as they were.
fn with_value(text: &str, key: &str, val: &str) -> String {
    let mut out = String::new();
    let mut found = false;
    for line in text.lines() {
        let ours = line.split_once('=').is_some_and(|(k, _)| k.trim() == key);
        if !ours {
            out.push_str(line);
            out.push('\n');
        } else if !found {
            out.push_str(&format!("{key} = {val}\n"));
            found = true;
        }
    }
    if !found {
        out.push_str(&format!("{key} = {val}\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_or_odd_value_is_none() {
        assert_eq!(parse_bool("", DEBUG_MODE), None);
        assert_eq!(parse_bool("debug_mode = yes\n", DEBUG_MODE), None);
        assert_eq!(parse_bool("debug_mode_x = true\n", DEBUG_MODE), None);
    }

    #[test]
    fn the_last_line_for_a_key_wins() {
        assert_eq!(parse_bool("debug_mode = true\ndebug_mode=false\n", DEBUG_MODE), Some(false));
        assert_eq!(parse_bool("  debug_mode =  true  ", DEBUG_MODE), Some(true));
    }

    #[test]
    fn setting_a_value_keeps_the_other_lines() {
        assert_eq!(with_value("", DEBUG_MODE, "true"), "debug_mode = true\n");
        assert_eq!(
            with_value("a = 1\ndebug_mode = false\nb = 2\ndebug_mode = x\n", DEBUG_MODE, "true"),
            "a = 1\ndebug_mode = true\nb = 2\n"
        );
    }

    #[test]
    fn a_saved_value_reads_back() {
        let dir = std::env::temp_dir().join(format!("flipctl-settings-{}", std::process::id()));
        let path = dir.join("flipctl").join("settings.conf");
        write(&path, &with_value("", DEBUG_MODE, "true")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(parse_bool(&text, DEBUG_MODE), Some(true));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
