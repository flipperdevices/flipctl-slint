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
    let file = root.join("radio-aarch64.AppImage");
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
    assert!(a.work_dir().ends_with("flipctl/apps/radio-aarch64"));
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
    assert_eq!(bundle::read(&root.join("Stock.AppImage"), &[]), Err(bundle::Skip::Stock));
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
