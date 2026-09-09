//! Running user applications.
//!
//! An app is an AppImage in the user's `Apps` folder with an `app.toml` at its root
//! (`bundle` reads them). It draws its own screen: flipctl runs a compositor, gives
//! the app an output of its own and reads its frames, and nothing describes a screen
//! to flipctl or asks it to lay one out. What an app takes from us is the look,
//! through the widget library in crates/flipctl-app, and that is a dependency of the
//! app rather than a protocol between us.
//!
//! The manifest names what the app needs installed and how it wants to be shown. It
//! is read by scanning the file, never by running anything: a bundle is whatever a
//! person dropped into the folder, and the list has to open before its packages
//! exist. Scanning also means the list is available without starting anything.
//!
//! What used to be here was a protocol: an app wrote scenes as JSON lines and
//! flipctl laid them out as rows, cards, logs or a blitted canvas. It is gone, with
//! the screens that drew it. An app that wants those widgets links them.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The manifest, at the root of a bundle.
pub const MANIFEST: &str = "app.toml";

/// Which edge of the panel is the app's own top.
#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum Rotate {
    /// Landscape, like the panel and like flipctl.
    #[default]
    None,
    /// Portrait, read with the device turned so the panel's left edge is up.
    Left,
    /// Portrait the other way, the panel's right edge up.
    Right,
}

/// An app found on disk, before it runs.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct AppEntry {
    /// The declared name, or the bundle's file name when it says nothing.
    pub name: String,
    /// Where the manifest and the icon are: the bundle's cache directory, which is
    /// also what names its working directory.
    pub dir: PathBuf,
    /// The file this was read from: an `.AppImage`, or a script.
    pub bundle: PathBuf,
    /// What the app is filed under: its path below the Apps folder with the
    /// separators turned into dashes and the extension dropped. Two apps of one name
    /// in two folders keep their own cache and their own working directory.
    pub key: String,
    /// The folders this app sits in under `Apps`, outermost first, empty at the
    /// top level. Apps are grouped by where they live rather than by a category in
    /// the manifest: the folder is already the answer, and two apps cannot
    /// disagree about which folder they are in.
    pub group: Vec<String>,
    /// A file in `dir`. Empty when absent.
    pub icon: String,
    /// Debian packages the app needs from the device.
    pub apt: Vec<String>,
    /// The command line inside the bundle. AppRun is what runs it; here it only has
    /// to be non-empty, which is what says the manifest describes an app.
    pub wayland: String,
    /// The size the program insists on drawing, when it is not the panel's.
    ///
    /// Doom is the case this exists for: it renders 320x200 and will not accept a
    /// smaller window, so tiling it into a 256x144 output shows a corner of the
    /// frame rather than the frame. Declaring the size lets flipctl scale the
    /// output while that app is in front, so the compositor fits the whole picture
    /// onto the panel instead.
    pub size: Option<(u32, u32)>,
    /// Whether the app is allowed to reach the sound server.
    ///
    /// Off by default, because a hosted app gets a runtime directory of its own and
    /// the sockets live in the real one: silence is what absence produces. Declaring
    /// it links PipeWire's sockets in and pins the app to the panel's own speaker, so
    /// it plays there rather than following whatever sink the desktop chose, which on
    /// this device is usually HDMI.
    ///
    /// It covers capture as well as playback: a client that can reach the socket can
    /// record, and separating the two would mean PipeWire access rules rather than
    /// anything of ours.
    pub audio: bool,
    /// Whether flipctl paints its own status bar over the app's picture.
    ///
    /// For an app that cannot draw the bar itself: it has no idea what the battery or
    /// the radios are doing, so it asks flipctl for the panel's usual top row rather
    /// than drawing a picture of one, and simply loses its top 13 pixels. An app on the
    /// framework fills the real bar in itself and leaves this off.
    pub status: bool,
    /// Which way the app's picture is turned relative to the panel.
    ///
    /// The panel is landscape and some apps are drawn portrait, to be read with the
    /// device turned. flipctl needs to know because it paints over their frames: a
    /// status bar belongs on the edge that is the app's top, not on the panel's.
    pub rotate: Rotate,
    /// Environment for the command, each entry `NAME=value`.
    pub env: Vec<String>,
    /// The runtime this app is run through, e.g. `python`. Empty for a program of
    /// its own. A launcher bundle declaring the same word in `provides` is what runs
    /// it; the launchers themselves are not written yet, so today this only names
    /// what is missing.
    pub runtime: String,
    /// The runtime this bundle provides. Only a launcher says anything here.
    pub provides: String,
    /// How a comment starts in the language this launcher runs, for a language whose
    /// scripts flipctl could not otherwise find the manifest in. Only needed for a
    /// marker outside the handful tried by default.
    pub comment: String,
}

impl AppEntry {
    /// The program to run and the arguments to pass it.
    ///
    /// Through a shell, because what runs is a command line: the bundle, quoted, or
    /// the launcher with the bundle as its argument.
    pub fn command(&self) -> (PathBuf, Vec<PathBuf>) {
        (PathBuf::from("/bin/sh"), vec![PathBuf::from("-c"), PathBuf::from(self.launch_line(None))])
    }

    /// The command line the shell runs: the bundle, or `via` with the bundle as its
    /// argument, each single-quoted so a space in a file name stays in the name.
    pub fn launch_line(&self, via: Option<&AppEntry>) -> String {
        match via {
            Some(launcher) => format!("{} {}", quoted(&launcher.bundle), quoted(&self.bundle)),
            None => quoted(&self.bundle),
        }
    }

    /// A writable directory of the app's own, which is where it runs.
    ///
    /// A bundle is a read-only image and a script's folder is the user's, so a
    /// program that writes beside itself needs somewhere else, and this is the same
    /// place every launch.
    pub fn work_dir(&self) -> PathBuf {
        xdg_home("XDG_DATA_HOME", ".local/share").join("flipctl/apps").join(&self.key)
    }

    /// The short word the list shows beside the name of an app that is run through
    /// something, and nothing for a bundle, which is the panel's own format.
    ///
    /// It is the runtime itself, which for a script is its extension: `py` beside a
    /// Python one, `js` beside a JavaScript one, and the same word the message names
    /// when nothing provides it.
    pub fn tag(&self) -> &str {
        &self.runtime
    }

    pub fn icon_path(&self) -> Option<PathBuf> {
        (!self.icon.is_empty()).then(|| self.dir.join(&self.icon))
    }
}

/// A path as one shell word.
fn quoted(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

/// The user's home, or the device's user when the environment does not say.
pub(crate) fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/home/user"))
}

/// An XDG base directory: the variable, else its documented default under home.
pub(crate) fn xdg_home(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(fallback))
}

/// The launcher `entry` runs through, if it asks for one.
///
/// `Ok(None)` for a program of its own, the first bundle providing the runtime
/// otherwise, and an error naming the runtime when nothing does: said up front in
/// the install dialog, the way a missing package is.
pub fn launcher_for<'a>(
    apps: &'a [AppEntry],
    entry: &AppEntry,
) -> Result<Option<&'a AppEntry>, String> {
    if entry.runtime.is_empty() {
        return Ok(None);
    }
    apps.iter()
        .find(|a| a.provides == entry.runtime)
        .map(Some)
        .ok_or_else(|| format!("needs the {} runtime, which is not installed", entry.runtime))
}

/// A module-level string assignment, e.g. `name = "Ping"`.
///
/// Only column zero counts, so an assignment inside a table is not mistaken for the
/// manifest. Both quote styles are accepted, and a trailing comment is ignored.
fn py_string(src: &str, key: &str) -> Option<String> {
    src.lines().filter(|l| !l.starts_with(char::is_whitespace)).find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            return None;
        }
        let v = v.trim();
        let quote = v.chars().next().filter(|c| *c == '"' || *c == '\'')?;
        let rest = &v[1..];
        let end = rest.find(quote)?;
        Some(rest[..end].to_string())
    })
}

/// A boolean field, e.g. `audio = true`. None when the key is absent, so a
/// caller can tell "said no" from "said nothing".
fn py_bool(src: &str, key: &str) -> Option<bool> {
    src.lines().filter(|l| !l.starts_with(char::is_whitespace)).find_map(|line| {
        let (k, v) = line.split_once('=')?;
        if k.trim() != key {
            return None;
        }
        match v.trim().trim_end_matches(|c: char| c == ',').to_ascii_lowercase().as_str() {
            "true" | "yes" | "1" => Some(true),
            "false" | "no" | "0" => Some(false),
            _ => None,
        }
    })
}

/// A module-level list of strings, e.g. `apt = ["a", "b"]`.
///
/// Written across as many lines as the author likes, with or without a trailing
/// comma, because that is how a list of packages tends to grow.
fn py_list(src: &str, key: &str) -> Vec<String> {
    let Some(at) =
        src.find(&format!("\n{key}")).map(|i| i + 1).or_else(|| src.starts_with(key).then_some(0))
    else {
        return Vec::new();
    };
    let rest = &src[at..];
    let Some(open) = rest.find('[') else {
        return Vec::new();
    };
    // A list of string literals cannot contain a bracket, so the first one closes
    // it.
    let Some(close) = rest[open..].find(']') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let body = &rest[open + 1..open + close];
    let mut chars = body.char_indices();
    while let Some((i, c)) = chars.next() {
        if c != '"' && c != '\'' {
            continue;
        }
        if let Some(end) = body[i + 1..].find(c) {
            let item = &body[i + 1..i + 1 + end];
            if !item.is_empty() {
                out.push(item.to_string());
            }
            // Skip past the closing quote.
            for _ in 0..=end {
                chars.next();
            }
        }
    }
    out
}

/// A `size = "320x200"` field, as a pair.
fn py_size(src: &str, key: &str) -> Option<(u32, u32)> {
    let raw = py_string(src, key)?;
    let (w, h) = raw.split_once(['x', 'X'])?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// One app from the text of its manifest, or `None` when the text does not
/// describe one: a manifest with no command is not an app.
///
/// `dir` is where the manifest and the icon can be found again, `bundle` the file
/// they were read from, and `fallback` the name to show when the manifest gives
/// none.
pub fn parse_manifest(
    src: &str,
    fallback: String,
    group: &[String],
    dir: PathBuf,
    bundle: PathBuf,
    key: String,
) -> Option<AppEntry> {
    let wayland = py_string(src, "wayland").unwrap_or_default();
    if wayland.is_empty() {
        return None;
    }
    Some(AppEntry {
        name: py_string(src, "name").unwrap_or(fallback),
        icon: py_string(src, "icon").unwrap_or_default(),
        apt: py_list(src, "apt"),
        wayland,
        size: py_size(src, "size"),
        audio: py_bool(src, "audio").unwrap_or(false),
        status: py_bool(src, "status").unwrap_or(false),
        rotate: match py_string(src, "rotate").unwrap_or_default().as_str() {
            "left" => Rotate::Left,
            "right" => Rotate::Right,
            _ => Rotate::None,
        },
        env: py_list(src, "env"),
        runtime: py_string(src, "runtime").unwrap_or_default(),
        provides: py_string(src, "provides").unwrap_or_default(),
        comment: py_string(src, "comment").unwrap_or_default(),
        group: group.to_vec(),
        dir,
        bundle,
        key,
    })
}

/// What an app still needs before it can run.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Missing {
    /// Debian packages that are not installed.
    pub apt: Vec<String>,
}

impl Missing {
    pub fn is_empty(&self) -> bool {
        self.apt.is_empty()
    }

    /// One line for a log, e.g. "2 packages: curl, mpv".
    pub fn summary(&self) -> String {
        let n = self.apt.len();
        if n == 0 {
            return String::new();
        }
        format!("{n} package{}: {}", if n == 1 { "" } else { "s" }, self.apt.join(", "))
    }
}

/// Which of `packages` dpkg does not have installed.
///
/// One query for the lot: dpkg-query takes several names and reports a line each,
/// and an unknown package makes it exit non-zero while still printing the ones it
/// does know, so the output is parsed either way.
fn missing_apt(packages: &[String]) -> Vec<String> {
    if packages.is_empty() {
        return Vec::new();
    }
    let out = Command::new("dpkg-query")
        .arg("-W")
        .arg("-f=${binary:Package} ${db:Status-Status}\n")
        .args(packages)
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else {
        // No dpkg at all: nothing can be checked, so claim nothing is missing
        // rather than blocking every app behind an install that cannot work.
        return Vec::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let installed: Vec<&str> = text
        .lines()
        .filter_map(|l| {
            let (name, status) = l.split_once(' ')?;
            // Without the architecture: dpkg prints `qt6-wayland:arm64` for anything
            // marked Multi-Arch: same, and a manifest names the package, not the
            // build of it. Comparing the two verbatim reported every multi-arch
            // dependency as missing however many times it was installed.
            let name = name.split(':').next()?;
            (status.trim() == "installed").then_some(name)
        })
        .collect();
    packages.iter().filter(|p| !installed.contains(&p.as_str())).cloned().collect()
}

/// Stop a program and everything it started.
///
/// The direct child of a launch is `/bin/sh`, and the program is its child, so
/// killing the child leaves the program running: with a window still on a workspace
/// flipctl then believes is free, which is how a game ended up tiled beside a file
/// manager. Launches put the app in its own process group so the whole group can be
/// signalled here.
///
/// TERM first, because a program that has a chance to exit tidily takes its
/// temporary files with it, then KILL for whatever ignored it.
pub fn stop_group(pid: u32) {
    let group = -(pid as i32);
    unsafe {
        libc::kill(group, libc::SIGTERM);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    unsafe {
        libc::kill(group, libc::SIGKILL);
    }
}

/// The parents of `pid`, nearest first, up to init.
///
/// Read from `/proc/<pid>/stat`, field four. A bundle's window belongs to a
/// grandchild of the process flipctl started: the AppImage runtime forks AppRun,
/// which execs the program. Walking up from the window's owner is how the two are
/// matched, since the kernel here was built without `/proc/<pid>/task/*/children`.
pub fn ancestors(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut at = pid;
    while at > 1 && out.len() < 64 {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{at}/stat")) else {
            break;
        };
        // The command name is in parentheses and may hold spaces, so the fields are
        // counted from the closing one.
        let Some(after) = stat.rfind(')') else { break };
        let Some(parent) =
            stat[after + 1..].split_whitespace().nth(1).and_then(|p| p.parse::<u32>().ok())
        else {
            break;
        };
        out.push(parent);
        at = parent;
    }
    out
}

/// What `entry` still needs.
///
/// Blocking: it runs dpkg-query, so callers put it on a thread rather than in a
/// render loop.
pub fn missing(entry: &AppEntry) -> Missing {
    Missing { apt: missing_apt(&entry.apt) }
}

/// Install what an app needs, reporting progress as lines.
///
/// Blocking and slow: apt reaches the network. `log` is called per output line so
/// a caller can show it while it runs, which matters because this can take a
/// minute and a frozen screen looks broken.
pub fn install(missing: &Missing, mut log: impl FnMut(String)) -> Result<(), String> {
    if !missing.apt.is_empty() {
        // Always update first. A device flashed from a stock image has no package
        // lists at all, and one that has sat for a while has stale ones, so the
        // install fails with "unable to locate package" for a package that is in
        // the archive. The cost is one round trip against a minute of installing.
        log("apt-get update".into());
        run_logged(
            Command::new("sudo")
                .args(["apt-get", "update"])
                .env("DEBIAN_FRONTEND", "noninteractive"),
            &mut log,
        )?;
        log(format!("apt: {}", missing.apt.join(" ")));
        run_logged(
            Command::new("sudo")
                .args(["apt-get", "install", "-y", "--no-install-recommends"])
                .args(&missing.apt)
                // Non-interactive: a package that stops to ask a question would
                // hang here with nobody able to answer it.
                .env("DEBIAN_FRONTEND", "noninteractive"),
            &mut log,
        )?;
    }
    log("done".into());
    Ok(())
}

/// Read one of a child's pipes on a thread, a line at a time, onto a shared channel.
///
/// Generic because stdout and stderr are different types and this is the same job
/// twice.
fn pump<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    is_err: bool,
    tx: &std::sync::mpsc::Sender<(bool, String)>,
) {
    let Some(pipe) = pipe else { return };
    let tx = tx.clone();
    std::thread::spawn(move || {
        for line in BufReader::new(pipe).lines().map_while(Result::ok) {
            if tx.send((is_err, line)).is_err() {
                return;
            }
        }
    });
}

/// Run a command, passing each output line to `log`, and fail on a non-zero exit.
fn run_logged(cmd: &mut Command, log: &mut impl FnMut(String)) -> Result<(), String> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        // Name the program. Without it a missing curl or an unavailable sudo reads
        // as "cannot start: No such file or directory (os error 2)" on a log screen
        // with no other context, which says nothing about what to install.
        .map_err(|e| format!("cannot start {}: {e}", cmd.get_program().to_string_lossy()))?;

    // Both pipes, on threads of their own, onto one channel. stderr is where the
    // interesting output is: apt reports its problems there, so collecting it to
    // print at the end left the screen still for the length of an install. A reader
    // per pipe also means neither can fill and block the other.
    let (tx, lines) = std::sync::mpsc::channel::<(bool, String)>();
    pump(child.stdout.take(), false, &tx);
    pump(child.stderr.take(), true, &tx);
    // The loop below ends when every sender is gone, so this one cannot be held.
    drop(tx);

    // The last of stderr, for the message a failure ends with.
    const TAIL: usize = 20;
    let mut tail: Vec<String> = Vec::new();
    for (is_err, line) in lines {
        if line.trim().is_empty() {
            continue;
        }
        if is_err {
            if tail.len() == TAIL {
                tail.remove(0);
            }
            tail.push(line.clone());
        }
        log(line);
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(tail
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .cloned()
            .unwrap_or_else(|| format!("exited with {status}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A program that is not there says which one, since the log screen is all
    /// the context there is.
    #[test]
    fn a_missing_program_is_named() {
        let mut log = |_: String| {};
        let err = run_logged(&mut Command::new("definitely-not-a-program"), &mut log)
            .expect_err("it cannot start");
        assert!(err.starts_with("cannot start definitely-not-a-program:"), "{err}");
    }

    #[test]
    fn the_summary_counts_packages() {
        let mut m = Missing::default();
        assert!(m.is_empty());
        assert_eq!(m.summary(), "");
        m.apt = vec!["libfoo".into()];
        assert_eq!(m.summary(), "1 package: libfoo");
        m.apt.push("libbar".into());
        assert_eq!(m.summary(), "2 packages: libfoo, libbar");
    }

    /// The window's owner is a grandchild of what flipctl started, so the chain
    /// upward has to be readable. This process's parent is the first link.
    #[test]
    fn ancestors_walk_up_to_init() {
        let mine = std::process::id();
        let up = ancestors(mine);
        let parent = unsafe { libc::getppid() } as u32;
        assert_eq!(up.first(), Some(&parent), "{up:?}");
        assert!(up.last().is_some_and(|p| *p <= 1 || up.len() == 64), "{up:?}");
        assert!(ancestors(1).is_empty());
    }

    /// The row says what runs a script, and says nothing for a bundle: that is the
    /// panel's own format and needs no label.
    #[test]
    fn only_an_app_run_through_something_is_tagged() {
        let script = AppEntry { runtime: "py".into(), ..Default::default() };
        assert_eq!(script.tag(), "py");
        let native = AppEntry {
            bundle: PathBuf::from("/home/user/Apps/radio-flipctl-aarch64.AppImage"),
            ..Default::default()
        };
        assert_eq!(native.tag(), "");
    }

    /// A launcher is found by what it provides, and its absence is a sentence.
    #[test]
    fn a_runtime_names_its_launcher_or_its_absence() {
        let python =
            AppEntry { name: "Python".into(), provides: "python".into(), ..Default::default() };
        let script =
            AppEntry { name: "Script".into(), runtime: "python".into(), ..Default::default() };
        let plain = AppEntry::default();
        let apps = vec![python.clone(), script.clone()];
        assert_eq!(launcher_for(&apps, &plain), Ok(None));
        assert_eq!(launcher_for(&apps, &script).unwrap().map(|l| &l.name), Some(&python.name));
        assert_eq!(
            launcher_for(&[script.clone()], &script),
            Err("needs the python runtime, which is not installed".into())
        );
    }

    /// A space in a file name stays inside one shell word, with or without a launcher.
    #[test]
    fn the_launch_line_quotes_the_bundle() {
        let app = AppEntry {
            bundle: PathBuf::from("/home/user/Apps/My Radio.AppImage"),
            key: "My Radio".into(),
            ..Default::default()
        };
        assert_eq!(app.launch_line(None), "'/home/user/Apps/My Radio.AppImage'");
        let via = AppEntry {
            bundle: PathBuf::from("/home/user/Apps/python.AppImage"),
            ..Default::default()
        };
        assert_eq!(
            app.launch_line(Some(&via)),
            "'/home/user/Apps/python.AppImage' '/home/user/Apps/My Radio.AppImage'"
        );
        let (program, args) = app.command();
        assert_eq!(program, Path::new("/bin/sh"));
        assert_eq!(args[0], Path::new("-c"));
        assert_eq!(args[1], Path::new("'/home/user/Apps/My Radio.AppImage'"));
        assert!(app.work_dir().ends_with("flipctl/apps/My Radio"));
    }
}
