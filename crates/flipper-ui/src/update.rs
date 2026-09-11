//! System updates: a channel, an image fetched to `/tmp`, and a kexec into it.
//!
//! What is downloaded is a FIT (`.itb`) carrying a kernel, a ramdisk and a device
//! tree for every board the build covers. `boot-profile --image` picks the
//! configuration for this one by `compatible`, reads the three out of it, grafts
//! this machine's memory node onto the tree and hands over with `systemctl kexec`.
//! None of that is here, because none of it should be twice: [`crate::boot`] already
//! wraps that tool for the boot menu.
//!
//! It lands in `/tmp`, which is a tmpfs, and stays there under one name per channel.
//! Nothing deletes it: a reboot is the deletion, and a second fetch of the same
//! channel writes over the first. That is the whole storage policy, and it is the
//! reason for choosing `/tmp` over a cache directory that would need a policy.
//!
//! Finding one is two GETs. The build server publishes a `manifest.json` beside every
//! directory it serves, which is what its own listing pages are generated from:
//!
//!   * `<server>/u-boot/manifest.json` lists the builds as `{name, mtime}`, newest
//!     last once sorted. The name is the whole hash-joined directory, ending in `/`.
//!   * `<server>/u-boot/<name>manifest.json` is that build: its number and timestamp,
//!     a `sourcestamps` entry per codebase carrying the branch and revision it was
//!     built from, and a `files` list of `{path, size, sha256}`.
//!
//! One file out of that list is wanted, `flipper-one/installer-falcon.itb`, and its
//! sha256 comes with it, so what was downloaded is checked rather than hoped about.
//!
//! `FLIPCTL_UPDATE_URL` still overrides the lot, which is how this was exercised
//! before the server was understood and is still the way to point the screen at a
//! build by hand.

use std::io::Read;
use std::path::{Path, PathBuf};

/// Where the artefacts are published.
pub const SERVER: &str = "https://dl-linux-images.flipp.dev";
/// The builder whose output carries the installer, and so the directory to look in.
pub const BUILDER: &str = "u-boot";
/// The one file out of a build that this boots. A build publishes thirteen for this
/// board alone -- loaders, idbloader, the boot menu's own FIT -- and none of the rest
/// is an update.
pub const IMAGE: &str = "flipper-one/installer-falcon.itb";
/// The codebase whose branch a channel is. It is the one that decides what an image
/// contains, so it is the one worth filtering on; every other codebase in a build is
/// pinned by it.
const CHANNEL_CODEBASE: &str = "buildscripts";
/// How far back to look for a build on the chosen channel before giving up. The
/// builds are every push, so a channel with nothing recent is a channel with nothing.
const LOOK_BACK: usize = 25;

/// Which build of the system to offer.
///
/// A channel is a branch: a build records the branch of every codebase it was made
/// from, so this is a filter over what the server already says rather than a naming
/// convention invented here. Adding one is a line here and a line in [`Channel::ALL`].
/// They wrap, because the row's chevrons are always drawn and a chevron that does
/// nothing at one end of two choices is worse than a cycle.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Channel {
    /// Every push to the branch the work happens on. Today this is the only one the
    /// server has ever built: every published build is on `dev`.
    #[default]
    Nightly,
    /// A branch cut for release. Nothing is built on it yet, and the screen says so
    /// rather than quietly offering a nightly instead.
    Release,
}

impl Channel {
    pub const ALL: [Channel; 2] = [Channel::Nightly, Channel::Release];

    /// What the row shows.
    pub fn name(self) -> &'static str {
        match self {
            Channel::Release => "Release",
            Channel::Nightly => "Nightly",
        }
    }

    /// The branch a build must have been made from to belong to this channel.
    pub fn branch(self) -> &'static str {
        match self {
            Channel::Nightly => "dev",
            Channel::Release => "release",
        }
    }

    /// The next channel round, in the direction the key pressed.
    pub fn step(self, forward: bool) -> Channel {
        let at = Self::ALL.iter().position(|c| *c == self).unwrap_or(0);
        let len = Self::ALL.len();
        Self::ALL[if forward { (at + 1) % len } else { (at + len - 1) % len }]
    }

    /// The file this channel's image is fetched to, one per channel so switching
    /// channels does not mean re-downloading the one already there.
    pub fn image_path(self) -> PathBuf {
        PathBuf::from(format!("/tmp/flipctl-update-{}.itb", self.name().to_lowercase()))
    }
}

/// An image a channel is offering: what to say about it, and where to get it.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Image {
    /// What the row shows. The build number and the day it was built, which is what
    /// distinguishes two builds to somebody looking at a panel; the hashes that name
    /// the directory distinguish them to a machine and would fill the screen.
    pub version: String,
    pub url: String,
    /// Bytes, or 0 when the manifest did not say.
    pub bytes: u64,
    /// The manifest's own sha256 of the file, so what arrives can be checked. Empty
    /// when it was not published, which is the only case where a download is taken
    /// on trust.
    pub sha256: String,
    /// The source revision the build was made from, for the codebase the channel is
    /// a branch of. Full length; [`Image::short`] is what the screen shows.
    pub revision: String,
    /// The branch that codebase was on, which is the channel this build belongs to.
    pub branch: String,
}

impl Image {
    /// The revision as git abbreviates one, which is what a person compares against
    /// a commit they are looking at. Seven characters, and the row has room for them.
    pub fn short(&self) -> &str {
        self.revision.get(..7).unwrap_or(&self.revision)
    }
}

/// How many builds to read. The server holds two hundred, each needing a GET of its
/// own to learn its branch, and nobody installs the hundredth most recent thing.
/// Twenty covers a few days of pushes on every channel at once.
pub const DEPTH: usize = 20;

/// Every recent build, newest first, each already knowing its channel.
///
/// One pass over the server rather than a question per keypress. Reading a build's
/// branch means fetching that build's own manifest, so a screen that asked as the
/// chevrons moved would spend a network round trip on every press -- which is exactly
/// what the first version of this did, and it froze the panel for seconds at a time.
/// This is slow once, in a thread, and then every channel and every version on the
/// screen is already known.
///
/// Builds that published no image for this board are left out rather than listed and
/// then refused: a build that failed after making its directory is not a version.
pub fn catalogue(depth: usize) -> Result<Vec<Image>, String> {
    let listing = get(&format!("{SERVER}/{BUILDER}/manifest.json"))?;
    let mut builds: Vec<(String, String)> = json::records(&listing, "directories")
        .iter()
        .filter_map(|r| Some((json::string(r, "mtime")?, json::string(r, "name")?)))
        .collect();
    if builds.is_empty() {
        return Err("the build server listed nothing".into());
    }
    // The times are ISO 8601 to the microsecond and all in UTC, so they sort as text.
    builds.sort();
    builds.reverse();

    let mut out = Vec::new();
    for (_, name) in builds.iter().take(depth) {
        let Ok(build) = get(&format!("{SERVER}/{BUILDER}/{name}manifest.json")) else {
            continue;
        };
        let Some(image) = read_build(&build, name) else { continue };
        out.push(image);
    }
    if out.is_empty() {
        return Err("no build published an installer".into());
    }
    Ok(out)
}

/// One build's manifest, as an image, or nothing when it has no installer for this
/// board.
fn read_build(build: &str, name: &str) -> Option<Image> {
    let stamps = json::records(build, "sourcestamps");
    let stamp =
        stamps.iter().find(|r| json::string(r, "codebase").as_deref() == Some(CHANNEL_CODEBASE))?;
    let file = json::records(build, "files")
        .into_iter()
        .find(|r| json::string(r, "path").as_deref() == Some(IMAGE))?;
    let number = json::number(build, "number").unwrap_or(0);
    let day = json::string(build, "timestamp").unwrap_or_default();
    Some(Image {
        version: format!("{number} {}", day.get(..10).unwrap_or("")).trim().to_string(),
        url: format!("{SERVER}/{BUILDER}/{name}{IMAGE}"),
        bytes: json::number(file, "size").unwrap_or(0),
        sha256: json::string(file, "sha256").unwrap_or_default(),
        revision: json::string(stamp, "revision").unwrap_or_default(),
        branch: json::string(stamp, "branch").unwrap_or_default(),
    })
}

/// One GET, as text. Seconds rather than minutes: a screen is waiting on this, and a
/// build server that has gone away should say so while somebody is still looking.
fn get(url: &str) -> Result<String, String> {
    let out = std::process::Command::new("curl")
        .arg("-fsSL")
        .arg("--max-time")
        .arg("20")
        .arg(url)
        .output()
        .map_err(|e| format!("curl: {e}"))?;
    if !out.status.success() {
        return Err("the build server did not answer".into());
    }
    String::from_utf8(out.stdout).map_err(|_| "the manifest is not text".into())
}

/// What `channel` is currently offering, from the build server.
///
/// The outer manifest lists every build; they are walked newest first and the first
/// one on this channel's branch that actually published the image wins. Walked rather
/// than taking the newest outright because a channel is a filter: the newest build
/// overall is not necessarily on the branch asked for, and a build can fail after
/// producing a directory.
///
/// `FLIPCTL_UPDATE_URL` overrides the lot, for pointing the screen at one build.
pub fn latest(channel: Channel) -> Result<Image, String> {
    if let Ok(url) = std::env::var("FLIPCTL_UPDATE_URL") {
        if !url.is_empty() {
            return Ok(Image {
                version: std::env::var("FLIPCTL_UPDATE_VERSION")
                    .unwrap_or_else(|_| "by hand".into()),
                url,
                bytes: 0,
                sha256: String::new(),
                revision: String::new(),
                branch: String::new(),
            });
        }
    }

    let listing = get(&format!("{SERVER}/{BUILDER}/manifest.json"))?;
    let mut builds: Vec<(String, String)> = json::records(&listing, "directories")
        .iter()
        .filter_map(|r| Some((json::string(r, "mtime")?, json::string(r, "name")?)))
        .collect();
    if builds.is_empty() {
        return Err("the build server listed nothing".into());
    }
    // The times are ISO 8601 to the microsecond and all in UTC, so they sort as text.
    builds.sort();
    builds.reverse();

    for (_, name) in builds.iter().take(LOOK_BACK) {
        let url = format!("{SERVER}/{BUILDER}/{name}manifest.json");
        let Ok(build) = get(&url) else { continue };
        let stamps = json::records(&build, "sourcestamps");
        let Some(stamp) = stamps
            .iter()
            .find(|r| json::string(r, "codebase").as_deref() == Some(CHANNEL_CODEBASE))
            .filter(|r| json::string(r, "branch").as_deref() == Some(channel.branch()))
        else {
            continue;
        };
        let revision = json::string(stamp, "revision").unwrap_or_default();
        let Some(file) = json::records(&build, "files")
            .into_iter()
            .find(|r| json::string(r, "path").as_deref() == Some(IMAGE))
        else {
            continue;
        };
        let number = json::number(&build, "number").unwrap_or(0);
        let day = json::string(&build, "timestamp").unwrap_or_default();
        return Ok(Image {
            version: format!("{number} {}", day.get(..10).unwrap_or("")).trim().to_string(),
            url: format!("{SERVER}/{BUILDER}/{name}{IMAGE}"),
            bytes: json::number(file, "size").unwrap_or(0),
            sha256: json::string(file, "sha256").unwrap_or_default(),
            revision,
            branch: channel.branch().into(),
        });
    }
    Err(format!("no {} build to install", channel.name()))
}

/// Enough JSON to read these manifests, and no more.
///
/// Both shapes are an array of flat objects: `directories` is `{name, mtime}`,
/// `files` is `{path, size, sha256, mtime}`, `sourcestamps` is `{codebase, branch,
/// revision, ...}`. Nothing nested is wanted out of any of them, so this finds an
/// array by its key, cuts it into the objects inside it, and reads a string or a
/// number out of one by name.
///
/// It is not a JSON parser and must not be asked to be one: no nested objects inside
/// a record, no arrays of arrays, no numbers that are not integers. What it does do
/// is respect strings, because that is where a scan that counts brackets goes wrong
/// -- a `}` inside a value is not the end of anything, and these manifests carry URLs
/// and paths full of punctuation.
mod json {
    /// Walk `text` from `at`, returning the index just past the value that starts
    /// there, with `open`/`close` the brackets it is delimited by.
    fn span_end(text: &str, at: usize, open: u8, close: u8) -> Option<usize> {
        let bytes = text.as_bytes();
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        for (i, b) in bytes.iter().enumerate().skip(at) {
            if in_string {
                if escaped {
                    escaped = false;
                } else if *b == b'\\' {
                    escaped = true;
                } else if *b == b'"' {
                    in_string = false;
                }
                continue;
            }
            match *b {
                b'"' => in_string = true,
                b if b == open => depth += 1,
                b if b == close => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(i + 1);
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// Where `"key"` is used as a key, rather than appearing inside some value.
    ///
    /// A key is a string followed by a colon, which is what separates
    /// `"branch": "dev"` from a revision that happens to contain the word.
    fn key_at(text: &str, key: &str) -> Option<usize> {
        let needle = format!("\"{key}\"");
        let mut from = 0usize;
        while let Some(rel) = text[from..].find(&needle) {
            let at = from + rel;
            let after = at + needle.len();
            if text[after..].trim_start().starts_with(':') {
                return Some(after + text[after..].find(':')? + 1);
            }
            from = at + needle.len();
        }
        None
    }

    /// The objects inside the array called `key`, each as its own `{...}` text.
    pub fn records<'a>(text: &'a str, key: &str) -> Vec<&'a str> {
        let Some(after) = key_at(text, key) else { return Vec::new() };
        let Some(open) = text[after..].find('[').map(|i| after + i) else { return Vec::new() };
        let Some(end) = span_end(text, open, b'[', b']') else { return Vec::new() };
        let inner = &text[open + 1..end - 1];

        let mut out = Vec::new();
        let mut at = 0usize;
        while let Some(rel) = inner[at..].find('{') {
            let start = at + rel;
            let Some(stop) = span_end(inner, start, b'{', b'}') else { break };
            out.push(&inner[start..stop]);
            at = stop;
        }
        out
    }

    /// A string value by name, with the escapes JSON allows in these files undone.
    pub fn string(record: &str, key: &str) -> Option<String> {
        let after = key_at(record, key)?;
        let rest = record[after..].trim_start();
        let rest = rest.strip_prefix('"')?;
        let mut out = String::new();
        let mut escaped = false;
        for c in rest.chars() {
            if escaped {
                out.push(match c {
                    'n' => '\n',
                    't' => '\t',
                    other => other,
                });
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                return Some(out);
            } else {
                out.push(c);
            }
        }
        None
    }

    /// A whole number by name.
    pub fn number(record: &str, key: &str) -> Option<u64> {
        let after = key_at(record, key)?;
        let rest = record[after..].trim_start();
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        digits.parse().ok()
    }
}

/// How far a download has got, as the thread doing it reports it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Progress {
    /// Bytes so far and, when the server said, bytes expected.
    Fetching(u64, u64),
    /// On disk at this path, ready to boot.
    Ready(PathBuf),
    Failed(String),
}

/// Fetch `image` to `to`, reporting as it goes.
///
/// Written to a `.part` beside the target and renamed at the end, so an interrupted
/// download cannot be mistaken for an image: the name exists only once the bytes are
/// all there. curl rather than an HTTP crate, for the same reason the rest of this
/// binary shells out: the alternative is TLS, certificates and redirects as
/// dependencies, for one GET.
pub fn fetch(image: &Image, to: &Path, mut say: impl FnMut(Progress)) {
    let part = to.with_extension("part");
    let _ = std::fs::remove_file(&part);
    let mut child = match std::process::Command::new("curl")
        .arg("-fsSL")
        .arg("--output")
        .arg(&part)
        .arg("--write-out")
        .arg("%{size_download}")
        .arg(&image.url)
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
                if let Err(e) = verify(&part, &image.sha256) {
                    let _ = std::fs::remove_file(&part);
                    return say(Progress::Failed(e));
                }
                if let Err(e) = std::fs::rename(&part, to) {
                    return say(Progress::Failed(format!("cannot place the image: {e}")));
                }
                return say(Progress::Ready(to.to_path_buf()));
            }
            Ok(None) => {
                let so_far = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                say(Progress::Fetching(so_far, image.bytes));
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => return say(Progress::Failed(format!("curl: {e}"))),
        }
    }
}

/// Check what arrived against the hash the manifest published.
///
/// Checked before the file is given its real name, so a corrupt download is never a
/// thing that could be booted. Nothing is verified when the manifest published no
/// hash, which is the `FLIPCTL_UPDATE_URL` case: a URL typed by hand comes with no
/// claim about what is at the other end.
///
/// sha256sum rather than a crate, for the same reason curl is doing the download.
fn verify(file: &Path, want: &str) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from the server, trimmed to three entries. The outer manifest is what
    /// the listing pages are generated from.
    const LISTING: &str = r#"{
 "build": { "builder": "uboot" },
 "directories": [
  { "name": "u=0ff9d9cc__installer=b1863991/", "mtime": "2026-08-24T14:06:11.505000+00:00" },
  { "name": "u=e995d0eb__installer=604e6dbc/", "mtime": "2026-09-11T11:41:27.000000+00:00" },
  { "name": "u=1f5af61b__installer=b1863991/", "mtime": "2026-09-01T09:02:00.000000+00:00" }
 ]
}"#;

    /// One build's own manifest, cut down but otherwise as served: the punctuation in
    /// the URLs and paths is the part that breaks a scan which does not respect
    /// strings.
    const BUILD: &str = r#"{
 "build": {
  "builder": "uboot",
  "number": 627,
  "url": "https://linux-images.flipp.dev/#/builders/6/builds/627",
  "timestamp": "2026-09-11T11:41:04Z"
 },
 "sourcestamps": [
  { "codebase": "buildscripts", "branch": "dev", "revision": "c8b8af87b2455e2d" },
  { "codebase": "linux-mainline", "branch": "flipper-devel", "revision": "cf84c6019d" }
 ],
 "files": [
  { "path": "evb/installer-falcon.itb", "size": 41487872, "sha256": "aaaa" },
  { "path": "flipper-one/installer-falcon.itb", "size": 41716736, "sha256": "96ac92a8706663b0" },
  { "path": "flipper-one/u-boot-rockchip.bin", "size": 9964032, "sha256": "037a82f8" }
 ]
}"#;

    #[test]
    fn the_builds_are_read_out_of_the_listing() {
        let records = json::records(LISTING, "directories");
        assert_eq!(records.len(), 3);
        assert_eq!(json::string(records[1], "name").unwrap(), "u=e995d0eb__installer=604e6dbc/");
        // The times sort as text, which is what picking the newest relies on.
        let mut times: Vec<_> = records.iter().map(|r| json::string(r, "mtime").unwrap()).collect();
        times.sort();
        assert!(times.last().unwrap().starts_with("2026-09-11"));
    }

    #[test]
    fn the_wanted_file_is_picked_out_of_the_build() {
        let files = json::records(BUILD, "files");
        assert_eq!(files.len(), 3, "three files, and the evb one is not ours");
        let ours = files
            .iter()
            .find(|r| json::string(r, "path").as_deref() == Some(IMAGE))
            .expect("the board's installer");
        assert_eq!(json::number(ours, "size"), Some(41_716_736));
        assert_eq!(json::string(ours, "sha256").unwrap(), "96ac92a8706663b0");
    }

    #[test]
    fn a_channel_is_the_branch_of_one_codebase() {
        let stamps = json::records(BUILD, "sourcestamps");
        let branch = stamps
            .iter()
            .find(|r| json::string(r, "codebase").as_deref() == Some(CHANNEL_CODEBASE))
            .and_then(|r| json::string(r, "branch"));
        assert_eq!(branch.as_deref(), Some(Channel::Nightly.branch()));
        assert_ne!(branch.as_deref(), Some(Channel::Release.branch()));
    }

    /// The build's own number and day, which are outside the arrays.
    #[test]
    fn the_build_number_and_day_are_read() {
        assert_eq!(json::number(BUILD, "number"), Some(627));
        let day = json::string(BUILD, "timestamp").unwrap();
        assert_eq!(&day[..10], "2026-09-11");
    }

    /// A `}` or a `"` inside a value ends nothing. The build URL carries a `#` and
    /// slashes, and a scan that counted brackets without knowing about strings would
    /// cut the record short.
    #[test]
    fn punctuation_inside_values_does_not_end_a_record() {
        let awkward =
            r#"{ "files": [ { "path": "a/b}c\"d", "size": 7 }, { "path": "e", "size": 8 } ] }"#;
        let files = json::records(awkward, "files");
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!(json::string(files[0], "path").unwrap(), "a/b}c\"d");
        assert_eq!(json::number(files[1], "size"), Some(8));
    }

    /// A key is a name followed by a colon. A revision that happens to contain the
    /// word "branch" is not one.
    #[test]
    fn a_value_that_looks_like_a_key_is_not_one() {
        let r = r#"{ "revision": "\"branch\": \"release\"", "branch": "dev" }"#;
        assert_eq!(json::string(r, "branch").unwrap(), "dev");
    }

    #[test]
    fn nothing_is_found_in_nothing() {
        assert!(json::records("", "files").is_empty());
        assert!(json::records("{}", "files").is_empty());
        assert_eq!(json::string("{}", "path"), None);
        assert_eq!(json::number("{}", "size"), None);
    }

    #[test]
    fn the_channels_cycle_in_both_directions() {
        assert_eq!(Channel::Release.step(true), Channel::Nightly);
        assert_eq!(Channel::Nightly.step(true), Channel::Release);
        assert_eq!(Channel::Release.step(false), Channel::Nightly);
        // Whichever way and however far, it is always one of the channels.
        let mut at = Channel::default();
        for i in 0..10 {
            at = at.step(i % 3 == 0);
            assert!(Channel::ALL.contains(&at));
        }
    }

    /// One file per channel, in the tmpfs, so switching back does not re-download
    /// and nothing has to be cleaned up.
    #[test]
    fn each_channel_has_its_own_file_in_tmp() {
        let paths: Vec<_> = Channel::ALL.iter().map(|c| c.image_path()).collect();
        for path in &paths {
            assert!(path.starts_with("/tmp"), "{path:?}");
            assert_eq!(path.extension().and_then(|e| e.to_str()), Some("itb"), "{path:?}");
        }
        assert_ne!(paths[0], paths[1]);
    }

    /// Against the real build server, so `#[ignore]`: it needs the network. Run it
    /// with
    ///
    ///     cargo test -p flipper-ui --lib update:: -- --ignored --nocapture
    ///
    /// It is the only check that the two manifests still have the shape the parser
    /// above assumes, which is the thing that will break without warning when the
    /// server changes.
    #[test]
    #[ignore = "needs the build server"]
    fn the_server_offers_a_nightly() {
        std::env::remove_var("FLIPCTL_UPDATE_URL");
        let image = latest(Channel::Nightly).expect("a nightly");
        println!("{image:#?}");
        assert!(image.url.ends_with(IMAGE), "{}", image.url);
        assert!(image.url.starts_with(SERVER), "{}", image.url);
        assert!(image.bytes > 10_000_000, "an installer is tens of megabytes: {}", image.bytes);
        assert_eq!(image.sha256.len(), 64, "a sha256 is 64 hex characters");
        assert!(!image.version.is_empty());
    }

    /// A channel nothing is built on says so, rather than offering a nightly.
    #[test]
    #[ignore = "needs the build server"]
    fn a_channel_with_no_builds_says_so() {
        std::env::remove_var("FLIPCTL_UPDATE_URL");
        let said = latest(Channel::Release).unwrap_err();
        assert!(said.contains("Release"), "{said}");
    }

    #[test]
    fn sizes_read_in_megabytes() {
        assert_eq!(megabytes(41_772_544), "40MB");
        assert_eq!(megabytes(0), "0MB");
    }
}
