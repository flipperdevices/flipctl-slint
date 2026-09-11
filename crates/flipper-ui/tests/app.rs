//! App discovery.
//!
//! An app is an AppImage with an `app.toml` at the root of its squashfs, and what is
//! read from that manifest decides whether the app can be listed and started at all.
//! Every test here reads a bundle without running it, which is the property that lets
//! the list open on a folder a person can drop anything into.
//!
//! The fixtures are made here, without appimagetool: a squashfs written by the same
//! crate that reads it, behind a 64-byte ELF header carrying the type-2 magic, which
//! is all the reader looks at before the image.

#![cfg(feature = "bundle")]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use backhand::compression::Compressor;
use backhand::{FilesystemCompressor, FilesystemWriter, NodeHeader};
use flipper_ui::{app, bundle};

/// A scratch tree of this process's own, and the cache and data homes pointed into
/// it, so nothing here touches the real ~/.cache.
fn sandbox() -> PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("flipper-ui-bundles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("mkdir");
        std::env::set_var("XDG_CACHE_HOME", root.join("cache"));
        std::env::set_var("XDG_DATA_HOME", root.join("data"));
        root
    })
    .clone()
}

/// A fresh Apps folder for one test.
fn apps_folder(name: &str) -> PathBuf {
    let dir = sandbox().join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("mkdir");
    dir
}

/// Write a bundle at `path` holding `files` at its root and, optionally, a `.DirIcon`
/// link to one of them.
fn bundle_with(path: &Path, files: &[(&str, &[u8])], dir_icon: Option<&str>) {
    let mut fs = FilesystemWriter::default();
    fs.set_compressor(FilesystemCompressor::new(Compressor::Zstd, None).unwrap());
    let header = NodeHeader::new(0o644, 0, 0, 0);
    for (name, bytes) in files {
        fs.push_file(Cursor::new(bytes.to_vec()), name, header).expect("push");
    }
    if let Some(target) = dir_icon {
        fs.push_symlink(target, ".DirIcon", header).expect("symlink");
    }
    let mut image = Cursor::new(Vec::new());
    fs.write(&mut image).expect("squashfs");

    // ELF64, little-endian, the AppImage type-2 magic, and a section header table
    // that ends at byte 64 with nothing in it: the image starts right after.
    let mut elf = vec![0u8; 64];
    elf[..4].copy_from_slice(b"\x7fELF");
    elf[4] = 2;
    elf[5] = 1;
    elf[8..11].copy_from_slice(b"AI\x02");
    elf[0x28..0x30].copy_from_slice(&64u64.to_le_bytes());
    elf[0x3a..0x3c].copy_from_slice(&64u16.to_le_bytes());
    elf.extend_from_slice(&image.into_inner());
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(path, elf).expect("write");
}

const RADIO: &str = r#"# The radio's manifest, as the bundler writes it.
name = "Internet radio"
wayland = "./AppRun"
icon = "radio.png"
apt = ["mpv"]
audio = true
size = "320x200"
status = true
rotate = "left"
env = ["FOO=bar", "X=1"]
runtime = "python"
"#;

/// A bundle is listed from its manifest, with the manifest and the icon kept where
/// `dir` says and the file itself remembered.
#[test]
fn a_bundle_is_read_without_being_run() {
    let root = apps_folder("read");
    let file = root.join("radio-flipctl-aarch64.AppImage");
    bundle_with(&file, &[("app.toml", RADIO.as_bytes()), ("radio.png", b"png")], None);

    let apps = bundle::discover(&root);
    assert_eq!(apps.len(), 1, "{apps:?}");
    let a = &apps[0];
    assert_eq!(a.name, "Internet radio");
    assert_eq!(a.bundle, file);
    assert_eq!(a.wayland, "./AppRun");
    assert_eq!(a.apt, ["mpv"]);
    assert!(a.audio && a.status);
    assert_eq!(a.size, Some((320, 200)));
    assert_eq!(a.rotate, app::Rotate::Left);
    assert_eq!(a.env, ["FOO=bar", "X=1"]);
    assert_eq!(a.runtime, "python");
    assert!(a.group.is_empty());
    assert!(a.dir.is_absolute());
    assert!(a.dir.join(app::MANIFEST).is_file(), "the manifest is kept at {}", a.dir.display());
    assert_eq!(std::fs::read(a.icon_path().expect("an icon")).unwrap(), b"png");
    assert!(a.work_dir().ends_with("flipctl/apps/radio-flipctl-aarch64"));
    let (program, args) = a.command();
    assert_eq!(program, Path::new("/bin/sh"));
    assert_eq!(args[1].to_str().unwrap(), format!("'{}'", file.display()));
}

/// Without an `icon` key the AppImage convention's `.DirIcon` link names the file.
#[test]
fn the_dir_icon_stands_in_for_a_missing_icon_key() {
    let root = apps_folder("diricon");
    let file = root.join("clock.AppImage");
    let manifest = "name = \"Clock\"\nwayland = \"./AppRun\"\n";
    bundle_with(
        &file,
        &[("app.toml", manifest.as_bytes()), ("clock.png", b"tick")],
        Some("clock.png"),
    );

    let apps = bundle::discover(&root);
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].icon, "clock.png");
    assert_eq!(std::fs::read(apps[0].icon_path().unwrap()).unwrap(), b"tick");
}

/// A stock AppImage has no manifest and is not listed; nor is a file that only has
/// the extension. Both are remembered so the folder is not reopened every visit.
#[test]
fn stock_and_bogus_files_are_skipped_once() {
    let root = apps_folder("stock");
    bundle_with(&root.join("Stock.AppImage"), &[("AppRun", b"#!/bin/sh\n")], None);
    std::fs::write(root.join("not-really.AppImage"), b"hello").unwrap();
    bundle_with(
        &root.join("Good.AppImage"),
        &[("app.toml", b"name = \"Good\"\nwayland = \"./AppRun\"\n")],
        None,
    );

    let apps = bundle::discover(&root);
    assert_eq!(apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Good"]);
    assert_eq!(bundle::read(&root.join("Stock.AppImage"), &[]), Err(bundle::Skip::NotOurs));
    assert_eq!(
        bundle::read(&root.join("not-really.AppImage"), &[]),
        Err(bundle::Skip::NotAppImage)
    );
    assert!(matches!(
        bundle::read(&root.join("missing.AppImage"), &[]),
        Err(bundle::Skip::Unreadable(_))
    ));
}

/// An unchanged file is answered from what was read the first time; a changed one is
/// read again.
#[test]
fn an_unchanged_bundle_is_not_reopened() {
    let root = apps_folder("cache");
    let file = root.join("thing.AppImage");
    bundle_with(&file, &[("app.toml", b"name = \"First\"\nwayland = \"./AppRun\"\n")], None);
    let first = bundle::discover(&root);
    assert_eq!(first[0].name, "First");

    // Doctor the kept manifest: if the cache is trusted, its name is what shows.
    std::fs::write(first[0].dir.join(app::MANIFEST), "name = \"Cached\"\nwayland = \"./AppRun\"\n")
        .unwrap();
    let again = bundle::discover(&root);
    assert_eq!(again[0].name, "Cached", "the same file is not opened twice");

    // A different file under the same name is opened afresh.
    let longer = b"name = \"Second\"\nwayland = \"./AppRun\"\n# and a comment to change the size\n";
    bundle_with(&file, &[("app.toml", longer)], None);
    let third = bundle::discover(&root);
    assert_eq!(third[0].name, "Second");
}

const STATIONS: &str = r#"#!/usr/bin/env python3
# /// script
# dependencies = ["httpx"]
# ///
# /// flipctl
# name = "Stations"
# icon = "stations.png"
# audio = true
# ///
import flipctl
"#;

/// A script is an app too: one file in the same folder, listed beside the bundles
/// with the runtime it needs filled in for it.
#[test]
fn a_script_is_listed_beside_a_bundle() {
    let root = apps_folder("scripts");
    std::fs::write(root.join("stations.py"), STATIONS).unwrap();
    std::fs::write(root.join("stations.png"), b"png").unwrap();
    std::fs::write(root.join("notes.py"), "x = 1\n").unwrap();
    bundle_with(
        &root.join("radio.AppImage"),
        &[("app.toml", b"name = \"Internet radio\"\nwayland = \"./AppRun\"\n")],
        None,
    );

    let apps = bundle::discover(&root);
    assert_eq!(
        apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["Internet radio", "Stations"],
        "the script without a block of ours is not listed"
    );
    let script = &apps[1];
    assert_eq!(script.bundle, root.join("stations.py"));
    assert_eq!(script.key, "stations");
    assert_eq!(script.runtime, "py", "defaulted from the extension");
    assert!(script.audio);
    assert_eq!(script.wayland, "stations.py", "non-empty, so it is an app");
    assert_eq!(script.icon_path(), Some(root.join("stations.png")));
    assert!(script.work_dir().ends_with("flipctl/apps/stations"));
    // What runs it is the launcher, with the script as its argument.
    let launcher = app::AppEntry {
        bundle: root.join("python-runtime-flipctl-aarch64.AppImage"),
        provides: "python".into(),
        ..Default::default()
    };
    assert_eq!(
        script.launch_line(Some(&launcher)),
        format!("'{}' '{}'", launcher.bundle.display(), script.bundle.display())
    );
}

/// A block of its own beats the defaults, and a script may name a launcher that is
/// not Python.
#[test]
fn a_script_block_overrides_the_defaults() {
    let root = apps_folder("script-override");
    std::fs::write(
        root.join("thing.py"),
        "# /// flipctl\n# name = \"Thing\"\n# runtime = \"pypy\"\n# rotate = \"left\"\n# ///\n",
    )
    .unwrap();
    let apps = bundle::discover(&root);
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].runtime, "pypy", "the block beats the extension");
    assert_eq!(apps[0].rotate, app::Rotate::Left);
    assert_eq!(apps[0].name, "Thing");
}

/// A language flipctl has never heard of needs no flipctl change at all: the block in
/// the file is what makes it an app, its extension is what it asks for, and a launcher
/// answers by providing that word. Nothing about JavaScript is compiled in.
#[test]
fn a_launcher_teaches_flipctl_a_new_language() {
    let root = apps_folder("js");
    let clock =
        "#!/usr/bin/env node\n// /// flipctl\n// name = \"Clock\"\n// ///\nconsole.log(1)\n";
    std::fs::write(root.join("clock.js"), clock).unwrap();

    // The block is what makes it an app, so it is listed before any launcher exists,
    // tagged with what it asks for and refused by name until something provides it.
    let alone = bundle::discover(&root);
    assert_eq!(alone.len(), 1);
    assert_eq!(alone[0].runtime, "js", "the extension, since the block names none");
    assert_eq!(alone[0].tag(), "js");
    assert_eq!(
        app::launcher_for(&alone, &alone[0]),
        Err("needs the js runtime, which is not installed".into())
    );

    bundle_with(
        &root.join("js-flipctl-aarch64.AppImage"),
        &[("app.toml", b"name = \"JavaScript\"\nwayland = \"./AppRun\"\nprovides = \"js\"\n")],
        None,
    );

    let apps = bundle::discover(&root);
    assert_eq!(apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["Clock", "JavaScript"]);
    let script = &apps[0];
    assert_eq!(script.runtime, "js");
    assert_eq!(script.bundle, root.join("clock.js"));
    // And it is runnable: the launcher that taught the extension also provides it.
    assert_eq!(
        app::launcher_for(&apps, script).unwrap().map(|l| l.name.as_str()),
        Some("JavaScript")
    );
}

/// Bundles are found in folders at any depth, a folder is a group, and the list is
/// sorted by folder then name. A symlinked directory is not followed.
#[test]
fn bundles_are_found_in_folders() {
    let root = apps_folder("folders");
    let manifest = |name: &str| format!("name = \"{name}\"\nwayland = \"./AppRun\"\n");
    bundle_with(&root.join("loose.AppImage"), &[("app.toml", manifest("Loose").as_bytes())], None);
    bundle_with(
        &root.join("net/ping.appimage"),
        &[("app.toml", manifest("Ping").as_bytes())],
        None,
    );
    bundle_with(
        &root.join("net/deeper/nmap.AppImage"),
        &[("app.toml", manifest("Nmap").as_bytes())],
        None,
    );
    bundle_with(
        &root.join(".hidden/x.AppImage"),
        &[("app.toml", manifest("Hidden").as_bytes())],
        None,
    );
    std::os::unix::fs::symlink(&root, root.join("net/loop")).expect("symlink");

    let apps = bundle::discover(&root);
    let found: Vec<(&str, Vec<&str>)> = apps
        .iter()
        .map(|a| (a.name.as_str(), a.group.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(
        found,
        vec![("Loose", vec![]), ("Ping", vec!["net"]), ("Nmap", vec!["net", "deeper"]),]
    );
    // The key carries the folder, so two bundles of one name in two folders keep
    // separate caches.
    assert!(apps[1].dir.ends_with("net-ping"), "{}", apps[1].dir.display());
}

/// Two roots are one list: the image's read-only folder and the user's own, merged
/// before anything is read, so a runtime the image ships can name a script the user
/// wrote and a copy of theirs replaces a copy of ours.
#[test]
fn a_user_app_shadows_the_one_the_image_ships() {
    let system = apps_folder("two-system");
    let mine = apps_folder("two-mine");

    // Shipped: a JavaScript runtime and a clock written against it.
    bundle_with(
        &system.join("js-flipctl-aarch64.AppImage"),
        &[("app.toml", b"name = \"JavaScript\"\nwayland = \"./AppRun\"\nprovides = \"js\"\n")],
        None,
    );
    std::fs::write(system.join("clock.js"), "// /// flipctl\n// name = \"Clock\"\n// ///\n")
        .unwrap();

    let roots = [system.clone(), mine.clone()];
    let shipped = bundle::discover_all(&roots);
    assert_eq!(
        shipped.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["Clock", "JavaScript"],
        "an app in the read-only root is listed like any other"
    );
    // The marker that made the script an app came from a bundle in the other root.
    assert_eq!(shipped[0].runtime, "js");
    assert_eq!(
        app::launcher_for(&shipped, &shipped[0]).unwrap().map(|l| l.name.as_str()),
        Some("JavaScript")
    );

    // The user's own clock, at the same place below their folder, is the one listed.
    std::fs::write(mine.join("clock.js"), "// /// flipctl\n// name = \"My clock\"\n// ///\n")
        .unwrap();
    let merged = bundle::discover_all(&roots);
    assert_eq!(
        merged.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        ["JavaScript", "My clock"],
        "one entry, not two"
    );
    let clock = merged.iter().find(|a| a.name == "My clock").expect("the user's copy");
    assert_eq!(clock.bundle, mine.join("clock.js"), "read from the user's root");
    assert_eq!(clock.key, "clock", "and keeping the key, so it inherits the work directory");
}

/// A folder of one name in both roots is one folder, and the apps inside it merge the
/// same way anything else does.
#[test]
fn a_folder_in_both_roots_is_one_folder() {
    let system = apps_folder("folder-system");
    let mine = apps_folder("folder-mine");
    std::fs::create_dir_all(system.join("Test Tools")).unwrap();
    std::fs::create_dir_all(mine.join("Test Tools")).unwrap();
    std::fs::write(system.join("Test Tools/leds.py"), "# /// flipctl\n# name = \"LEDs\"\n# ///\n")
        .unwrap();
    std::fs::write(mine.join("Test Tools/mine.py"), "# /// flipctl\n# name = \"Mine\"\n# ///\n")
        .unwrap();

    let apps = bundle::discover_all(&[system, mine]);
    assert_eq!(apps.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["LEDs", "Mine"]);
    for app in &apps {
        assert_eq!(app.group, ["Test Tools"], "both sit in the same group");
    }
    assert_eq!(apps[0].key, "Test Tools-leds");
}
