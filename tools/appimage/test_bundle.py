"""The bundler's own checks, run by ci/test.sh without docker or network.

What is tested is the part flipctl depends on: that the manifest the bundle carries is
one its scanner reads, that the AppDir has the shape appimagetool and flipctl each
expect, and that AppRun renders into a script a shell accepts.
"""

import ast
import os
import re
import shutil
import stat
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

import bundle

REPO = Path(__file__).resolve().parent.parent.parent


class Manifests(unittest.TestCase):
    def test_every_manifest_is_toml_with_a_command(self):
        for manifest in sorted(REPO.glob("apps/*/app.toml")):
            with self.subTest(manifest=manifest.relative_to(REPO)):
                data = bundle.load_manifest(manifest.parent)
                self.assertTrue(data.get("wayland"), "an app names a command")
                # A crate is built; a directory with an AppRun of its own is staged as
                # it stands, which is what a runtime bundle is.
                self.assertIn(bundle.kind_of(manifest.parent, data), ("rust", "staged"))

    def test_a_runtime_bundle_is_staged_not_built(self):
        python = REPO / "apps/python-runtime"
        data = bundle.load_manifest(python)
        self.assertEqual(bundle.kind_of(python, data), "staged")
        self.assertTrue(data.get("provides"), "a launcher says what it runs")
        self.assertTrue((python / "AppRun").is_file())
        self.assertTrue((python / "flipctl/__init__.py").is_file())

    def test_every_client_font_table_matches_the_rust_one(self):
        """A client measures text to wrap it, and it must measure what is drawn.

        Each runtime carries its own copy of the generated Rust font's advances, so
        each can drift. A wrong table wraps a line one word early or one word late,
        which nobody notices until a traceback is unreadable on the panel.
        """
        rust = (REPO / "crates/flipper-ui/src/font/title.rs").read_text()
        advances = "".join(re.findall(r"advance:\s*(\d+)", rust))
        self.assertEqual(len(advances), 95, "ASCII 32..126, one digit each")

        clients = {
            "apps/python-runtime/flipctl/__init__.py": r'_ADVANCES = "(\d+)"',
            "apps/js-runtime/flipctl/index.js": r'const ADVANCES = "(\d+)"',
        }
        for where, pattern in clients.items():
            with self.subTest(client=where):
                found = re.search(pattern, (REPO / where).read_text())
                self.assertIsNotNone(found, "the client carries an advance table")
                self.assertEqual(found.group(1), advances)

    def test_a_script_app_is_an_app_that_parses(self):
        """A script app has to be both, and neither is checked anywhere else.

        Every `.py` under `apps/` that is not part of a bundle's own source is
        deployed into ~/Apps as it stands, at the same path, so a folder here is a
        folder in the menu. A missing block makes it invisible to the scanner and a
        syntax error makes it a traceback on the panel, both of which are cheaper to
        catch here than on the device.
        """
        scripts = sorted(
            path
            for path in (REPO / "apps").rglob("*.py")
            if not any(parent.joinpath("app.toml").exists() for parent in path.parents)
        )
        self.assertTrue(scripts, "the tree has a script app")
        for path in scripts:
            with self.subTest(script=path.name):
                text = path.read_text()
                ast.parse(text)
                head = text[:8192].splitlines()
                self.assertIn("# /// flipctl", head, "the block is what makes it an app")
                self.assertTrue(
                    any(line.startswith("# name = ") for line in head),
                    "the block names the app",
                )

    def test_the_derived_manifest_is_what_the_scanner_reads(self):
        derived = bundle.derive_manifest(
            {
                "name": 'Say "hi"',
                "wayland": "./target/release/thing",
                "icon": "ignored.png",
                "audio": True,
                "apt": ["mpv", "foo"],
                "size": "320x200",
                "pip": ["requests"],
            },
            icon="thing.png",
            source="apps/thing/app.toml",
            version="v1",
        )
        # Column zero, double quotes, one key a line: the shape the Rust scanner
        # reads, and valid TOML besides.
        back = tomllib.loads(derived)
        self.assertEqual(back["wayland"], "./AppRun")
        self.assertEqual(back["icon"], "thing.png")
        self.assertEqual(back["name"], 'Say "hi"')
        self.assertEqual(back["apt"], ["mpv", "foo"])
        self.assertEqual(back["size"], "320x200")
        self.assertTrue(back["audio"])
        self.assertNotIn("pip", back)
        for line in derived.splitlines()[1:]:
            self.assertFalse(line.startswith((" ", "\t")), line)
            self.assertNotIn("'", line.split("=", 1)[1][:2], line)
        self.assertTrue(derived.startswith("# Generated by tools/appimage from apps/thing/app.toml at v1."))


class AppDir(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="flipctl-bundle-test-"))
        # A repository stand-in: the licenses the bundle has to carry.
        repo = self.tmp / "repo"
        (repo / "LICENSES").mkdir(parents=True)
        (repo / "LICENSE").write_text("MIT\n")
        (repo / "LICENSES" / "MIT.txt").write_text("MIT text\n")
        (repo / "third_party" / "flipctl-fonts" / "Font-FlipCTL").mkdir(parents=True)
        (repo / "third_party" / "flipctl-fonts" / "Font-FlipCTL" / "LICENSE").write_text("font terms\n")
        app = repo / "apps" / "thing"
        app.mkdir(parents=True)
        (app / "app.toml").write_text('name = "Thing"\nwayland = "./target/release/thing-app"\naudio = true\n')
        (app / "Cargo.toml").write_text('[workspace]\n\n[package]\nname = "thing-app"\n')
        (app / "THIRD-PARTY-LICENSES.md").write_text("# Third-party licenses\n")
        self.binary = self.tmp / "thing-app"
        self.binary.write_bytes(b"\x7fELF fake")
        self.repo, self.app = repo, app

    def tearDown(self):
        shutil.rmtree(self.tmp)

    def test_the_appdir_has_the_shape_both_sides_expect(self):
        appdir = self.tmp / "AppDir"
        bundle.stage(self.app, appdir, self.binary, "v1", self.repo)

        apprun = appdir / "AppRun"
        self.assertTrue(apprun.stat().st_mode & stat.S_IXUSR)
        self.assertIn('exec "$APPDIR/usr/bin/thing-app" "$@"', apprun.read_text())
        self.assertNotIn("@COMMAND@", apprun.read_text())
        manifest = tomllib.loads((appdir / "app.toml").read_text())
        self.assertEqual(manifest["wayland"], "./AppRun")
        self.assertEqual(manifest["icon"], "thing.png")
        self.assertTrue(manifest["audio"])
        self.assertTrue((appdir / "thing.png").is_file(), "the default icon stands in")
        desktop = (appdir / "thing.desktop").read_text()
        self.assertIn("Exec=AppRun", desktop)
        self.assertIn("Icon=thing", desktop)
        self.assertIn("X-FlipCTL-App=thing", desktop)
        self.assertEqual((appdir / "usr" / "bin" / "thing-app").read_bytes(), b"\x7fELF fake")
        self.assertTrue((appdir / "usr" / "bin" / "thing-app").stat().st_mode & stat.S_IXUSR)
        doc = appdir / "usr" / "share" / "doc" / "thing-app"
        for name in ("THIRD-PARTY-LICENSES.md", "LICENSE", "LICENSES/MIT.txt", "fonts/Font-FlipCTL/LICENSE"):
            self.assertTrue((doc / name).is_file(), name)
        self.assertFalse((appdir / "Cargo.toml").exists(), "no crate travels")
        for path in appdir.rglob("*"):
            if path.is_symlink():
                self.assertFalse(os.readlink(path).startswith("/"), path)

    def test_a_kind_that_is_not_bundled_yet_is_refused(self):
        (self.app / "Cargo.toml").unlink()
        with self.assertRaises(SystemExit):
            bundle.stage(self.app, self.tmp / "AppDir", self.binary, "v1", self.repo)

    def test_the_rendered_apprun_passes_shellcheck(self):
        if shutil.which("shellcheck") is None:
            self.skipTest("no shellcheck")
        rendered = self.tmp / "AppRun"
        rendered.write_text(bundle.render_apprun('"$APPDIR/usr/bin/thing-app" "$@"'))
        subprocess.run(["shellcheck", str(rendered)], check=True)


if __name__ == "__main__":
    unittest.main()
