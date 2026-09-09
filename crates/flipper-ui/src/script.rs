//! Apps as scripts in the user's `Apps` folder.
//!
//! One file is the whole app: its head carries its manifest in a comment block, in
//! the shape PEP 723 defines, beside PEP 723's own block naming what it needs
//! installed.
//!
//! ```python
//! # /// script
//! # dependencies = ["httpx"]
//! # ///
//! # /// flipctl
//! # name = "Stations"
//! # audio = true
//! # ///
//! ```
//!
//! **The block is what makes a file an app, not its name.** flipctl knows no
//! languages: it looks for the block behind a handful of comment markers, and what
//! runs the file is the `runtime` the block names, or the file's own extension when
//! it names none. So `stations.py` asks for `py` and `clock.js` asks for `js`, a
//! launcher bundle answers by declaring the same word in `provides`, and a language
//! nobody has thought of yet costs a launcher in the folder rather than a new
//! flipctl.
//!
//! The block is read as text, never by running the file. That is the same rule
//! `bundle.rs` follows for a squashfs and it matters more here, not less: a script is
//! the one kind of app whose whole content is an instruction to an interpreter, and
//! the scan happens in a unit that can reach the GPIO and the USB bus.

use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;

use crate::app::{self, AppEntry};
use crate::bundle::Skip;

/// How a comment starts in the languages a script is likely to be written in.
///
/// Punctuation, not languages: these three cover Python, shell, JavaScript,
/// TypeScript, Ruby, Perl, the C family, Lua, SQL and Haskell between them. A
/// launcher whose language marks comments some other way says so in `comment`.
const MARKERS: [&str; 3] = ["#", "//", "--"];

/// Every comment marker worth trying: the three above, plus whatever the launchers
/// present have declared.
pub fn markers(apps: &[AppEntry]) -> Vec<String> {
    let mut out: Vec<String> = MARKERS.iter().map(|m| (*m).to_string()).collect();
    for app in apps.iter().filter(|a| !a.provides.is_empty()) {
        if !app.comment.is_empty() && !out.contains(&app.comment) {
            out.push(app.comment.clone());
        }
    }
    out
}

/// The block's own name, as PEP 723 spells a block type.
const BLOCK: &str = "flipctl";

/// How much of the file the manifest may be in. A block that has not started by here
/// is not a manifest; it is a comment somewhere in a program.
const HEAD: usize = 8192;

/// One script, read from its own head.
///
/// `dir` is the folder the file sits in, so an icon named by the block is found
/// beside it, and `key` is what the app is filed under, which is where its writable
/// directory goes.
pub fn read(
    path: &Path,
    key: &str,
    group: &[String],
    markers: &[String],
) -> Result<AppEntry, Skip> {
    let text = head_of(path).map_err(|e| Skip::Unreadable(e.to_string()))?;
    let Some(block) = markers.iter().find_map(|m| block_of(&text, m)) else {
        said_once(path, key);
        return Err(Skip::NotOurs);
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let fallback = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_default();
    // The defaults go after the block, and the scanner takes the first match at column
    // zero, so anything the block says wins. `wayland` is what makes a manifest an app
    // and a script has no command of its own to name: what runs it is the launcher.
    // The runtime falls back to the extension, which is the word a person would use
    // for the language anyway, and the empty string for a file that has none.
    let extension =
        path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let src = format!("{block}\nwayland = \"{name}\"\nruntime = \"{extension}\"\n");
    let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
    let app = app::parse_manifest(&src, fallback, group, dir, path.to_path_buf(), key.to_string())
        .ok_or(Skip::NotOurs)?;
    crate::logline!("scripts        {key}: {}", app.name);
    Ok(app)
}

/// The first `HEAD` bytes, as text.
fn head_of(path: &Path) -> std::io::Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; HEAD];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// The manifest inside a `# /// flipctl` block, with the comment markers taken off.
///
/// PEP 723's own shape and its own rules: the block opens on a line that is exactly
/// the marker, every line inside starts with `#`, and it closes on a line that is
/// exactly `# ///`. A block that never closes is not a block.
fn block_of(text: &str, comment: &str) -> Option<String> {
    let opens = format!("{comment} /// {BLOCK}");
    let closes = format!("{comment} ///");
    let mut lines = text.lines();
    lines.position(|l| l.trim_end() == opens)?;
    let mut out = String::new();
    for line in lines {
        let line = line.trim_end();
        if line == closes {
            return Some(out);
        }
        let Some(rest) = line.strip_prefix(comment) else {
            return None;
        };
        out.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        out.push('\n');
    }
    None
}

/// Say once that a file in the folder is not one of ours.
///
/// The list is read again on every visit to the Apps screen, and a line per visit per
/// stray file is noise in a log that has one boot's worth of room. A bundle answers
/// this with the stamp beside what it cached; a script has nothing cached, so the
/// answer is remembered for as long as the program runs.
fn said_once(path: &Path, key: &str) {
    static SAID: Mutex<Option<HashSet<std::path::PathBuf>>> = Mutex::new(None);
    let Ok(mut said) = SAID.lock() else { return };
    if said.get_or_insert_with(HashSet::new).insert(path.to_path_buf()) {
        crate::logline!("scripts        {key}: not a flipctl app, skipped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_read_without_its_comment_markers() {
        let text = "#!/usr/bin/env python3\n\
                    # /// script\n\
                    # dependencies = [\"httpx\"]\n\
                    # ///\n\
                    # /// flipctl\n\
                    # name = \"Stations\"\n\
                    # audio = true\n\
                    # ///\n\
                    import flipctl\n";
        assert_eq!(block_of(text, "#").as_deref(), Some("name = \"Stations\"\naudio = true\n"));
    }

    #[test]
    fn a_bare_hash_is_a_blank_line_inside_the_block() {
        let text = "# /// flipctl\n# name = \"X\"\n#\n# audio = true\n# ///\n";
        assert_eq!(block_of(text, "#").as_deref(), Some("name = \"X\"\n\naudio = true\n"));
    }

    #[test]
    fn a_block_that_does_not_close_is_not_a_block() {
        assert_eq!(block_of("# /// flipctl\n# name = \"X\"\n", "#"), None);
        // A line that stops being a comment ends it, as PEP 723 says.
        assert_eq!(block_of("# /// flipctl\n# name = \"X\"\nprint()\n# ///\n", "#"), None);
    }

    #[test]
    fn a_script_with_no_block_of_ours_has_none() {
        assert_eq!(block_of("print('hello')\n", "#"), None);
        // Somebody else's block is not ours.
        assert_eq!(block_of("# /// script\n# dependencies = []\n# ///\n", "#"), None);
    }

    #[test]
    fn the_marker_is_the_whole_line() {
        assert_eq!(block_of("x = 1  # /// flipctl\n# name = \"X\"\n# ///\n", "#"), None);
    }

    /// Another language's block is the same shape behind its own marker, and nothing
    /// about that language is known here.
    #[test]
    fn a_marker_of_its_own_reads_the_same_way() {
        let js = "#!/usr/bin/env node\n\
                  // /// flipctl\n\
                  // name = \"Clock\"\n\
                  // ///\n\
                  console.log(1)\n";
        assert_eq!(block_of(js, "//").as_deref(), Some("name = \"Clock\"\n"));
        assert_eq!(block_of(js, "#"), None, "the wrong marker finds nothing");
        let lua = "-- /// flipctl\n-- name = \"Thing\"\n-- ///\n";
        assert_eq!(block_of(lua, "--").as_deref(), Some("name = \"Thing\"\n"));
    }

    /// The markers tried are punctuation the languages share, plus whatever a
    /// launcher has declared for one that shares none of it.
    #[test]
    fn a_launcher_can_add_a_marker() {
        assert_eq!(markers(&[]), ["#", "//", "--"]);
        let odd = AppEntry { provides: "bas".into(), comment: "REM".into(), ..Default::default() };
        assert!(markers(&[odd.clone()]).contains(&"REM".to_string()));
        // Something that provides no runtime is not a launcher and teaches nothing.
        let liar = AppEntry { comment: "REM".into(), ..Default::default() };
        assert!(!markers(&[liar]).contains(&"REM".to_string()));
    }
}
