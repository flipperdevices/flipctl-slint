//! The apps a device can install, and getting one of them onto it.
//!
//! Nothing ships with the image any more: a fresh device has an empty `~/Apps`, and
//! this is where everything in it comes from. The catalogue is one file on a server,
//! listing what is on offer and where each file actually lives, and installing is a
//! download into the folder the entry asks for. The index and the files it names need
//! not be in the same place, which is the point of an explicit `url` per entry: the
//! index can sit in a git repository while the bytes are release assets, and the
//! device neither knows nor cares.
//!
//! ```toml
//! [[app]]
//! name = "Breathing LED"
//! path = "Test Tools/breathing-led.py"
//! url = "https://example/breathing-led.py"
//! runtime = "python"
//! size = 9021
//! sha256 = "..."
//! ```
//!
//! `path` is the only field with any authority over the filesystem, and it arrives
//! over the network, so it is checked rather than trusted: see [`Listing::dest`]. The
//! rest is read with the same readers that parse an app's own manifest, because it is
//! the same shape of file and `runtime` and `provides` already mean there exactly
//! what they mean here.
//!
//! A script is useless without the runtime that starts it, and the runtime is itself
//! an entry in the catalogue, so [`needs`] answers what else has to come along before
//! an app will run. That question is asked before the download rather than after,
//! since the alternative is a person installing a script and finding out at launch.

use crate::app::{py_string, py_u64, AppEntry};
use std::path::{Path, PathBuf};

/// Where the catalogue is published.
///
/// `FLIPCTL_APPS_URL` overrides it, for pointing a device at a catalogue being worked
/// on rather than the published one.
pub const INDEX: &str =
    "https://raw.githubusercontent.com/flipperdevices/flipctl-apps/dev/apps.toml";

/// One app on offer.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    /// What to call it on the screen.
    pub name: String,
    /// Where it goes, relative to the apps folder: `Test Tools/breathing-led.py`.
    pub path: String,
    /// Where to get it. Not derived from `path`: the bytes need not be laid out the
    /// way the device lays them out, and release assets are a flat namespace.
    pub url: String,
    /// What to expect, which is what gives the progress bar a denominator and the
    /// confirmation a number to show before anything is downloaded.
    pub size: u64,
    /// What it must hash to.
    pub sha256: String,
    /// The runtime it needs, for a script.
    pub runtime: String,
    /// The runtime it is, for a launcher bundle.
    pub provides: String,
}

impl Listing {
    /// The folder it is filed under, empty at the top level.
    pub fn folder(&self) -> &str {
        self.path.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("")
    }

    /// The file it is installed as.
    pub fn file_name(&self) -> &str {
        self.path.rsplit_once('/').map(|(_, file)| file).unwrap_or(&self.path)
    }

    /// Where it lands under `root`, or nothing when the path is one no catalogue is
    /// allowed to ask for.
    ///
    /// This is the whole of the trust boundary. `path` is a string from a server that
    /// is turned into a file to write, so anything that could escape the apps folder
    /// or name something other than a plain file below it is refused outright rather
    /// than sanitised: a catalogue with a path like that is wrong, and quietly
    /// rewriting it would install something under a name nobody published.
    pub fn dest(&self, root: &Path) -> Option<PathBuf> {
        if !safe(&self.path) {
            return None;
        }
        Some(root.join(&self.path))
    }

    /// Whether it is already on the device, in any of `roots`.
    pub fn present(&self, roots: &[PathBuf]) -> bool {
        roots.iter().any(|r| self.dest(r).is_some_and(|p| p.exists()))
    }

    /// Whether it is a bundle rather than a script, which decides whether the file
    /// has to come out executable.
    fn is_bundle(&self) -> bool {
        std::path::Path::new(self.file_name())
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(crate::bundle::EXT))
    }
}

/// Whether `path` is one a catalogue may name: a plain relative path, below the apps
/// folder, naming a file.
///
/// Split out from [`Listing::dest`] so the rules are one list that a test can walk
/// rather than a chain of conditions inside a method.
fn safe(path: &str) -> bool {
    if path.is_empty() || path.ends_with('/') {
        return false;
    }
    // A Windows separator, a NUL and a leading tilde are not path traversal here, but
    // none of them is a thing a generated catalogue produces either.
    if path.contains('\\') || path.contains('\0') || path.starts_with('~') {
        return false;
    }
    let p = Path::new(path);
    if p.is_absolute() {
        return false;
    }
    p.components().all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// The catalogue, as the server has it.
pub fn fetch_index() -> Result<Vec<Listing>, String> {
    let url = std::env::var("FLIPCTL_APPS_URL").unwrap_or_else(|_| INDEX.to_string());
    let src =
        crate::fetch::get(&url).map_err(|_| "the app catalogue did not answer".to_string())?;
    let apps = parse(&src);
    if apps.is_empty() {
        return Err("the app catalogue is empty".into());
    }
    Ok(apps)
}

/// Every app in the text of a catalogue.
///
/// An entry with no name, no url or an unusable path is dropped rather than shown:
/// there is nothing a person could do with a row that cannot be installed, and a
/// catalogue one of whose entries is malformed is still a catalogue.
pub fn parse(src: &str) -> Vec<Listing> {
    records(src)
        .into_iter()
        .map(|record| Listing {
            name: py_string(record, "name").unwrap_or_default(),
            path: py_string(record, "path").unwrap_or_default(),
            url: py_string(record, "url").unwrap_or_default(),
            size: py_u64(record, "size").unwrap_or(0),
            sha256: py_string(record, "sha256").unwrap_or_default(),
            runtime: py_string(record, "runtime").unwrap_or_default(),
            provides: py_string(record, "provides").unwrap_or_default(),
        })
        .filter(|a| !a.name.is_empty() && !a.url.is_empty() && safe(&a.path))
        .collect()
}

/// The body of each `[[app]]` table.
///
/// A record runs from its header to the next table of any kind, so a key under some
/// other table is never read as if it belonged to the app above it. The readers want
/// column-zero assignments, which is what the lines between two headers are.
fn records(src: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut body: Option<usize> = None;
    let mut at = 0usize;
    for line in src.split_inclusive('\n') {
        let next = at + line.len();
        if line.starts_with('[') {
            if let Some(start) = body.take() {
                out.push(&src[start..at]);
            }
            if line.trim() == "[[app]]" {
                body = Some(next);
            }
        }
        at = next;
    }
    if let Some(start) = body {
        out.push(&src[start..]);
    }
    out
}

/// What else has to be installed before `app` will run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Needs<'a> {
    /// Nothing: it is a bundle, or its runtime is already here.
    Nothing,
    /// This runtime, which the catalogue can supply.
    Runtime(&'a Listing),
    /// A runtime by this name, which nothing on offer provides.
    Unavailable(String),
}

/// Whether `app` can run once it is installed, and what to fetch with it if not.
///
/// The rule matching a script to its launcher is `provides == runtime`, which is the
/// rule [`crate::app::launcher_for`] already applies to what is installed; this asks
/// the same question of what is installed and then of what is on offer.
pub fn needs<'a>(app: &Listing, installed: &[AppEntry], offered: &'a [Listing]) -> Needs<'a> {
    if app.runtime.is_empty() || installed.iter().any(|a| a.provides == app.runtime) {
        return Needs::Nothing;
    }
    match offered.iter().find(|l| l.provides == app.runtime) {
        Some(runtime) => Needs::Runtime(runtime),
        None => Needs::Unavailable(app.runtime.clone()),
    }
}

/// Fetch `app` into `root`, reporting as it goes.
///
/// A bundle is made executable and a script is not, which is the same distinction
/// staging makes: a script is started by its runtime and never by the kernel, so an
/// execute bit on it would only be a way to run it with the wrong interpreter.
pub fn install(app: &Listing, root: &Path, mut say: impl FnMut(crate::fetch::Progress)) {
    let Some(to) = app.dest(root) else {
        return say(crate::fetch::Progress::Failed(format!("{} is not a path", app.path)));
    };
    let bundle = app.is_bundle();
    crate::fetch::download(&app.url, app.size, &app.sha256, &to, |p| {
        if let crate::fetch::Progress::Ready(path) = &p {
            if bundle {
                set_executable(path);
            }
        }
        say(p)
    })
}

/// Give a freshly downloaded bundle its execute bit.
///
/// Failure is not reported: the download is on disk and correct, and a mode that
/// would not take is something the launch will say far more usefully than a progress
/// bar could.
fn set_executable(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shape the index generator writes, with the three cases that matter in it:
    /// a bundle, a script that needs a runtime, and the runtime that provides it.
    const INDEX: &str = r#"# flipctl app catalogue
[[app]]
name = "Internet radio"
path = "Media/radio-aarch64.fap.AppImage"
url = "https://example/radio-aarch64.fap.AppImage"
size = 13068752
sha256 = "aaaa"

[[app]]
name = "Breathing LED"
path = "Test Tools/breathing-led.py"
url = "https://example/breathing-led.py"
runtime = "python"
size = 9021
sha256 = "bbbb"

[[app]]
name = "Python runtime"
path = "python-runtime-aarch64.fap.AppImage"
url = "https://example/python-runtime-aarch64.fap.AppImage"
provides = "python"
size = 31230472
sha256 = "cccc"
"#;

    fn listing(path: &str) -> Listing {
        Listing { path: path.into(), ..Default::default() }
    }

    #[test]
    fn every_field_of_every_entry() {
        let apps = parse(INDEX);
        assert_eq!(apps.len(), 3);
        assert_eq!(apps[0].name, "Internet radio");
        assert_eq!(apps[0].url, "https://example/radio-aarch64.fap.AppImage");
        assert_eq!(apps[0].size, 13068752);
        assert_eq!(apps[0].sha256, "aaaa");
        assert_eq!(apps[1].runtime, "python");
        assert_eq!(apps[2].provides, "python");
    }

    /// A key belongs to the table it is under. Without that, `provides` on the last
    /// entry would be read as the second entry's too, and a script would look like
    /// its own runtime.
    #[test]
    fn a_key_does_not_leak_into_the_entry_above() {
        let apps = parse(INDEX);
        assert_eq!(apps[0].runtime, "");
        assert_eq!(apps[1].provides, "");
    }

    /// Anything but `[[app]]` ends the record it follows and contributes nothing.
    #[test]
    fn another_table_is_not_an_app() {
        let src = "[[app]]\nname = \"A\"\nurl = \"u\"\npath = \"a\"\n\n[meta]\nname = \"B\"\nurl = \"u\"\n";
        let apps = parse(src);
        assert_eq!(apps.len(), 1);
        assert_eq!(apps[0].name, "A");
    }

    #[test]
    fn an_empty_or_headerless_catalogue_has_no_apps() {
        assert!(parse("").is_empty());
        assert!(parse("name = \"A\"\nurl = \"u\"\npath = \"a\"\n").is_empty());
    }

    /// An entry that could not be installed is not offered.
    #[test]
    fn an_unusable_entry_is_dropped() {
        let src = "[[app]]\nname = \"No url\"\npath = \"a\"\n\n\
                   [[app]]\nname = \"\"\nurl = \"u\"\npath = \"a\"\n\n\
                   [[app]]\nname = \"Escapes\"\nurl = \"u\"\npath = \"../a\"\n";
        assert!(parse(src).is_empty());
    }

    /// The trust boundary. Every one of these is a string a server could send.
    #[test]
    fn a_path_may_not_leave_the_apps_folder() {
        for bad in [
            "",
            "/etc/passwd",
            "../outside",
            "Test Tools/../../outside",
            "a/../../b",
            "./a",
            "~/x",
            "a\\b",
            "Tools/",
            "a\0b",
        ] {
            assert!(listing(bad).dest(Path::new("/home/user/Apps")).is_none(), "allowed {bad:?}");
        }
    }

    #[test]
    fn a_good_path_lands_under_the_root() {
        let at = listing("Test Tools/breathing-led.py").dest(Path::new("/home/user/Apps"));
        assert_eq!(at, Some(PathBuf::from("/home/user/Apps/Test Tools/breathing-led.py")));
        let at = listing("radio.AppImage").dest(Path::new("/home/user/Apps"));
        assert_eq!(at, Some(PathBuf::from("/home/user/Apps/radio.AppImage")));
    }

    #[test]
    fn the_folder_and_the_file_come_out_of_the_path() {
        let app = listing("Test Tools/breathing-led.py");
        assert_eq!(app.folder(), "Test Tools");
        assert_eq!(app.file_name(), "breathing-led.py");
        let top = listing("radio.AppImage");
        assert_eq!(top.folder(), "");
        assert_eq!(top.file_name(), "radio.AppImage");
    }

    #[test]
    fn only_a_bundle_is_made_executable() {
        assert!(listing("radio-aarch64.fap.AppImage").is_bundle());
        assert!(listing("Media/radio.appimage").is_bundle());
        assert!(!listing("Test Tools/breathing-led.py").is_bundle());
        assert!(!listing("uptime.js").is_bundle());
    }

    fn installed(provides: &str) -> AppEntry {
        AppEntry { provides: provides.into(), ..Default::default() }
    }

    #[test]
    fn a_bundle_needs_nothing() {
        let apps = parse(INDEX);
        assert_eq!(needs(&apps[0], &[], &apps), Needs::Nothing);
    }

    #[test]
    fn a_script_asks_for_the_runtime_that_provides_it() {
        let apps = parse(INDEX);
        assert_eq!(needs(&apps[1], &[], &apps), Needs::Runtime(&apps[2]));
    }

    #[test]
    fn a_runtime_already_installed_is_not_asked_for_again() {
        let apps = parse(INDEX);
        assert_eq!(needs(&apps[1], &[installed("python")], &apps), Needs::Nothing);
        assert_eq!(needs(&apps[1], &[installed("js")], &apps), Needs::Runtime(&apps[2]));
    }

    /// A directory of this test's own, named for it so two running at once cannot
    /// tread on each other.
    fn scratch(who: &str) -> PathBuf {
        let at = std::env::temp_dir()
            .join(format!("flipctl-catalogue-{}", std::process::id()))
            .join(who);
        let _ = std::fs::remove_dir_all(&at);
        std::fs::create_dir_all(&at).expect("scratch");
        at
    }

    /// An entry served over `file:`, which curl fetches without a network, so the
    /// whole path a real install takes is what runs here: the download, the checksum,
    /// the rename off `.part` and the mode.
    fn served(at: &Path, name: &str, body: &[u8]) -> Listing {
        let from = at.join(name);
        std::fs::write(&from, body).expect("write");
        let sum = std::process::Command::new("sha256sum").arg(&from).output().expect("sha256sum");
        let said = String::from_utf8_lossy(&sum.stdout);
        Listing {
            name: name.to_string(),
            path: format!("Test Tools/{name}"),
            url: format!("file://{}", from.display()),
            size: body.len() as u64,
            sha256: said.split_whitespace().next().unwrap_or("").to_string(),
            ..Default::default()
        }
    }

    fn outcome(app: &Listing, root: &Path) -> crate::fetch::Progress {
        let mut last = crate::fetch::Progress::Failed("nothing happened".into());
        install(app, root, |p| {
            if !matches!(p, crate::fetch::Progress::Fetching(..)) {
                last = p;
            }
        });
        last
    }

    /// The folder the entry names is made on the way, since a first install into it
    /// is the usual case on a device whose apps folder is empty.
    #[test]
    fn an_install_lands_at_the_path_and_is_not_executable_for_a_script() {
        let at = scratch("script");
        let root = at.join("Apps");
        let app = served(&at, "thing.py", b"print('hi')\n");
        assert!(matches!(outcome(&app, &root), crate::fetch::Progress::Ready(_)));
        let landed = root.join("Test Tools/thing.py");
        assert_eq!(std::fs::read(&landed).unwrap(), b"print('hi')\n");
        assert!(app.present(&[root.clone()]));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&landed).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0, "a script must not come out executable");
        }
    }

    #[test]
    fn a_bundle_comes_out_executable() {
        let at = scratch("bundle");
        let root = at.join("Apps");
        let app = served(&at, "thing-aarch64.fap.AppImage", b"ELF and a squashfs\n");
        assert!(matches!(outcome(&app, &root), crate::fetch::Progress::Ready(_)));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let landed = root.join("Test Tools/thing-aarch64.fap.AppImage");
            let mode = std::fs::metadata(&landed).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "a bundle nothing can execute cannot start");
        }
    }

    /// The checksum is what decides, and a file that fails it is not left behind
    /// under the name a person would then try to run.
    #[test]
    fn a_wrong_checksum_installs_nothing() {
        let at = scratch("wrong");
        let root = at.join("Apps");
        let mut app = served(&at, "thing.py", b"print('hi')\n");
        app.sha256 = "0".repeat(64);
        assert!(matches!(outcome(&app, &root), crate::fetch::Progress::Failed(_)));
        assert!(!root.join("Test Tools/thing.py").exists());
        assert!(!root.join("Test Tools/thing.part").exists());
        assert!(!app.present(&[root]));
    }

    /// A path the catalogue may not name is refused before curl is ever started.
    #[test]
    fn an_escaping_path_installs_nothing() {
        let at = scratch("escape");
        let root = at.join("Apps");
        let mut app = served(&at, "thing.py", b"print('hi')\n");
        app.path = "../escaped.py".into();
        assert!(matches!(outcome(&app, &root), crate::fetch::Progress::Failed(_)));
        assert!(!at.join("escaped.py").exists());
    }

    #[test]
    fn a_runtime_nothing_offers_is_named_rather_than_guessed_at() {
        let apps = parse(INDEX);
        let orphan = Listing { runtime: "lua".into(), ..apps[1].clone() };
        assert_eq!(needs(&orphan, &[], &apps), Needs::Unavailable("lua".into()));
    }
}
