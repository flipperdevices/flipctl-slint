"""The Python runtime's own screen.

Opened from the Apps list with no argument, this says what the runtime is and what
the last script that failed had to say. Given a script it runs it instead, which is
what AppRun does with the argument flipctl passes.

It is written with the library it provides, so the launcher is the first app to prove
the thing it is for.
"""

import os
import pathlib
import subprocess
import sys

import flipctl
import slint

# Where flipctl gives every app a writable directory of its own, and where AppRun
# leaves what a script said as it died.
APPS = pathlib.Path(
    os.environ.get("XDG_DATA_HOME", pathlib.Path.home() / ".local/share")
) / "flipctl/apps"
ERROR = "last-error.log"


def first_line(*command: str) -> str:
    """What a program says about itself, or a dash when it will not say."""
    try:
        out = subprocess.run(command, capture_output=True, text=True, timeout=10)
    except (OSError, subprocess.SubprocessError):
        return "-"
    said = (out.stdout or out.stderr).strip().splitlines()
    return said[0] if said else "-"


def scripts() -> list[pathlib.Path]:
    """The scripts in the Apps folder that are apps.

    The block is what makes one an app, exactly as flipctl decides it: a `.py`
    without one is somebody else's module sitting in the folder, and neither of us
    lists it. Counting every `.py` instead is how this came to claim two scripts
    where there was one.
    """
    found = []
    for path in sorted(pathlib.Path.home().glob("Apps/*.py")):
        try:
            head = path.read_text(errors="replace")[:8192]
        except OSError:
            continue
        if any(line.rstrip() == "# /// flipctl" for line in head.splitlines()):
            found.append(path)
    return found


def newest_log() -> pathlib.Path | None:
    """The log of whichever script failed most recently, or None if none did."""
    logs = [p for p in APPS.glob(f"*/{ERROR}") if p.is_file() and p.stat().st_size]
    return max(logs, key=lambda p: p.stat().st_mtime) if logs else None


def last_error() -> tuple[str, list[str]]:
    """The newest traceback any script left behind, and whose it was."""
    newest = newest_log()
    if newest is None:
        return "", []
    return newest.parent.name, newest.read_text(errors="replace").splitlines()


def clear_error() -> None:
    """Empty the log on screen, so the next failure is the one being read.

    Emptied rather than deleted: AppRun truncates this file at the start of every run
    and writes the script's stderr into it, so removing it buys nothing and losing the
    directory would lose the app's working files with it. If an older log is still
    waiting behind this one it becomes the newest, and a second press clears that.
    """
    newest = newest_log()
    if newest is not None:
        newest.write_text("")


def facts(here: pathlib.Path) -> list[dict]:
    """What this runtime is, in the order a person would ask."""
    version = f"{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}"
    uv = here / "usr/bin/uv"
    cache = pathlib.Path(os.environ.get("UV_CACHE_DIR", pathlib.Path.home() / ".cache/uv"))
    who, _ = last_error()
    return [
        flipctl.pair("Python", version),
        flipctl.pair("uv", first_line(str(uv), "--version").replace("uv ", "") if uv.exists() else "-"),
        flipctl.pair("Slint", getattr(slint, "__version__", "1.17")),
        flipctl.rule(),
        flipctl.pair("Scripts", str(len(scripts()))),
        flipctl.pair("Packages", str(len(list(cache.glob("archive-v*/*")))) if cache.is_dir() else "0"),
        flipctl.pair("Last error", who or "none"),
    ]


def log_lines() -> list[str]:
    """The last traceback, wrapped to the panel.

    A traceback is one long line per frame and the log body clips rather than wraps,
    so the part that names the exception is exactly the part that would be lost.
    `flipctl.wrap` measures with the same font metrics flipctl\'s own log uses.
    """
    who, lines = last_error()
    if not lines:
        return ["nothing from " + who if who else "no script has failed"]
    return [part for line in lines for part in flipctl.wrap(line)]


def main() -> None:
    here = pathlib.Path(os.environ.get("APPDIR", pathlib.Path(__file__).parent.parent.parent))
    about = flipctl.Detail(rows=lambda: facts(here), buttons=["Close", "", "", "", "Last error"])
    # Nothing to clear, no button offering to: the slot empties instead.
    log = flipctl.Log(
        lines=log_lines,
        title="Last error",
        buttons=lambda: ["Close", "", "", "Clear" if newest_log() else "", "Runtime"],
    )

    @about.on_key
    def _(key):
        if key is flipctl.Key.RUN:
            log.offset = 0
            log.show()
            return True
        return False

    @log.on_key
    def _(key):
        if key is flipctl.Key.RUN:
            about.show()
            return True
        if key is flipctl.Key.EDIT and newest_log():
            clear_error()
            log.offset = 0
            return True
        return False

    about.run()


if __name__ == "__main__":
    main()
