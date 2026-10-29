"""What a Python app on the Flipper One draws with.

An app is made of views: a page of rows, a menu or a log. It hands each the functions
that say what it shows, and the window, the keys, scrolling and the soft bar's press
are done here, by the same widgets flipctl's own screens and every Rust app are built
from. Nothing is reimplemented and no token is retyped.

    import flipctl

    count = 0

    def rows():
        return [flipctl.pair("Pressed", str(count))]

    app = flipctl.Detail(rows=rows, buttons=["Close", "", "", "", "Add"])

    @app.on_key
    def _(key):
        global count
        if key is flipctl.Key.RUN:
            count += 1
            return True

    app.run()

A handler gets a key as it goes down and returns True when it used it; anything it
leaves is the view's: Escape closes, Up and Down scroll or move a menu's cursor. Back
is never seen here: it belongs to flipctl, which sends the app to the background.
After every key the view is drawn again from its functions, and `every` draws it on a
clock too.

An app that also has something to wait on hands `run` a coroutine, which the same
loop drives: see `ping.fap.py`, which reads its replies off a socket and calls
`app.redraw()` when one lands. Another view replaces the one showing with
`other.show()`, in the same window.

The renderer is the software one and the font size is 16, both settled by the
launcher, because the panel is two colours and the pixel fonts are exact at 16px and
nowhere else.
"""

import asyncio
import datetime
import enum
import inspect
import os
import pathlib
import re

import slint

__all__ = ["Detail", "Key", "Log", "Menu", "choice", "gauge", "heading", "item", "pair", "rule",
           "text", "text_width", "theme", "wrap"]


def _ui_dir() -> pathlib.Path:
    """Where the widget library, the theme and the fonts are.

    The launcher sets `FLIPCTL_UI` to its own copy; the fallback is where an image
    that ships the sources would put them, so a script run by hand still finds them.
    """
    return pathlib.Path(os.environ.get("FLIPCTL_UI", "/usr/share/flipctl/ui"))


_TOKENS: dict[str, float] = {}


def theme(name: str, default: float = 0) -> float:
    """A design token, by the name it has in `@theme`.

    Read out of the generated `theme.slint` the launcher carries, so a script gets
    the same number flipctl and every Rust app were built with instead of retyping
    it. Lengths come back in pixels and counts as themselves.
    """
    if not _TOKENS:
        try:
            text = (_ui_dir() / "theme.slint").read_text()
        except OSError:
            text = ""
        for key, value in re.findall(
            r"out property <(?:length|int|duration)> (\w+):\s*([\d.]+)", text
        ):
            _TOKENS[key] = float(value)
    return _TOKENS.get(name, default)


# Advance widths of the panel's 16px title face, ASCII 32..126, one digit each.
# Generated from crates/flipper-ui/src/font/title.rs, which is itself generated from
# the font; `tools/appimage/test_bundle.py` fails if the two drift apart. This is the
# face `Shell` sets as the window default, so it is what an app's text is drawn in.
_ADVANCES = "22466872446636256366666666234646966666666466686666666668666454664555553552352855554445585554247"


def text_width(text: str) -> int:
    """Width in pixels, the way `flipper_ui::font` measures it.

    The sum of the advances less one, because each advance carries a trailing spacing
    column that the last glyph does not need. A codepoint outside the font renders as
    `?` and is measured as one.
    """
    total = 0
    for char in text:
        index = ord(char) - 32
        if not 0 <= index < len(_ADVANCES):
            index = ord("?") - 32
        total += int(_ADVANCES[index])
    return max(0, total - 1)


def wrap(text: str, width: float = 0) -> list[str]:
    """One line of output broken into lines that fit the panel.

    Breaks at spaces, and splits a single token too long to fit on its own, because a
    path or an exception message is often wider than the screen and dropping its tail
    loses exactly the part worth reading. Same rule as flipctl's own log.
    """
    if not width:
        width = theme("panel_w", 256) - 2 * theme("margin_h", 8)
    out: list[str] = []
    line = ""
    for word in text.split():
        candidate = word if not line else line + " " + word
        if text_width(candidate) <= width:
            line = candidate
            continue
        if line:
            out.append(line)
            line = ""
        while text_width(word) > width:
            cut = len(word)
            while cut > 1 and text_width(word[:cut]) > width:
                cut -= 1
            out.append(word[:cut])
            word = word[cut:]
        line = word
    if line:
        out.append(line)
    return out or [""]


# ── Rows ─────────────────────────────────────────────────────────────────────
#
# A row handed to Slint has to carry every field of its struct: one left out reaches
# the body as nothing rather than a default, and the first read of it takes the
# interpreter down. So rows are made here, whole.


def _row(kind: int, label: str = "", value: str = "", percent: int = 0, dim: bool = False):
    return {"kind": kind, "label": label, "value": value, "percent": percent, "dim": dim}


def pair(label: str, value: str = "", dim: bool = False) -> dict:
    """A label on the left and its value on the right. Dim for a secondary line."""
    return _row(0, label, value, dim=dim)


def rule() -> dict:
    """A divider between groups of rows."""
    return _row(1)


def gauge(label: str, value: str, percent: int) -> dict:
    """A label, a bar filled to `percent` (0 to 100), and the value beside it."""
    return _row(2, label, value, max(0, min(100, int(percent))))


def text(line: str, dim: bool = False) -> dict:
    """A line of text across the whole row."""
    return _row(3, line, dim=dim)


def choice(label: str, value: str, first: bool = False, last: bool = False) -> dict:
    """A value Left and Right change, between chevrons. `first` and `last` say the value
    is at an end of its range, which drops the chevron pointing past it."""
    return {**_row(4, label, value), "_ends": (first, last)}


def heading(label: str, value: str = "") -> dict:
    """A label and value with the label in the bold face."""
    return _row(5, label, value)


def item(label: str, status: str = "", value: str = "", chevrons: int = 0,
         first: bool = False, last: bool = False, dim: bool = False, tag: str = "") -> dict:
    """A menu row: its label and, at the right edge, a `status`.

    With `chevrons` the right edge is instead `value` between chevrons that say Left
    and Right change it: 1 for an on/off toggle, 2 for a value with a range. `first`
    and `last` drop the chevron pointing past an end of that range. `dim` keeps the
    status in the dim tone when the row is selected, for a remark about the row rather
    than its value; `tag` is a short word drawn right after the label.
    """
    return {"label": label, "tag": tag, "status": status, "icon": 0,
            "picture": slint.Image(), "frames": 1, "chevrons": chevrons, "dim": dim,
            "value": value, "at_start": first, "at_end": last}


# ── Keys ─────────────────────────────────────────────────────────────────────


class Key(enum.Enum):
    """The panel's buttons, by name."""

    UP = "up"
    DOWN = "down"
    LEFT = "left"
    RIGHT = "right"
    OK = "ok"
    BACK = "back"
    ESCAPE = "escape"
    VIEW = "view"
    POWER = "power"
    EDIT = "edit"
    RUN = "run"
    PTT = "ptt"

    @property
    def soft_slot(self):
        """Which soft key this is, counting from the left, or None."""
        bar = (Key.ESCAPE, Key.VIEW, Key.POWER, Key.EDIT, Key.RUN)
        return bar.index(self) if self in bar else None


# What Slint puts in a key event's text. The five soft buttons are letters rather
# than function keys, because that is what the input driver sends for them: KEY_Z,
# KEY_X, KEY_C, KEY_V, KEY_B, and KEY_A for push to talk. Guessing F1 to F5 is why an
# app looks deaf while its clock keeps ticking.
_NAMED = {
    "": Key.UP,
    "": Key.DOWN,
    "": Key.LEFT,
    "": Key.RIGHT,
    "\n": Key.OK,
    "\b": Key.BACK,
    "z": Key.ESCAPE,
    "x": Key.VIEW,
    "c": Key.POWER,
    "v": Key.EDIT,
    "b": Key.RUN,
    "a": Key.PTT,
}


def _key_of(text: str):
    """The key behind a Slint key event, or None for one the panel has no button for."""
    return _NAMED.get(text.lower() if len(text) == 1 else text)


# ── The window ───────────────────────────────────────────────────────────────


class _Window:
    """The one window every view draws into, made on first use.

    One component holds a body per kind of view, so moving between a list and a log
    is a change of mode rather than a second window, which a hosted app does not get.
    """

    def __init__(self):
        self.page = None
        self.view = None
        self.models: dict[str, slint.ListModel] = {}
        self.timers: list[slint.Timer] = []

    def ensure(self):
        if self.page is None:
            ui = _ui_dir()
            compiler = slint.native.Compiler()
            compiler.library_paths = {
                "flipctl": ui / "crates/flipper-ui/ui",
                "app": ui / "crates/flipctl-app/ui",
                "theme": ui / "theme.slint",
            }
            built = compiler.build_from_path(ui / "crates/flipctl-app/ui/pages.slint")
            for diagnostic in built.diagnostics:
                print(f"flipctl: pages.slint: {diagnostic}")
            self.page = built.component("Page").create()
            self.page.set_global_callback("AppKeys", "key", self.key)
        return self.page

    def set(self, name: str, value) -> None:
        """Set a property, a list as the model kept behind it.

        Slint holds a model weakly, so a fresh one per redraw is collected as soon as
        the app lets go of it and the screen stops updating with "Model implementation
        is lacking self object". The model is kept here and refilled in place, which
        also means a redraw notifies only the rows that changed.
        """
        page = self.ensure()
        if not isinstance(value, list):
            page.set_property(name, value)
            return
        model = self.models.get(name)
        if model is None:
            model = self.models[name] = slint.ListModel(value)
            page.set_property(name, model)
            return
        for i, row in enumerate(value):
            if i >= model.row_count():
                model.append(row)
            elif model.row_data(i) != row:
                model.set_row_data(i, row)
        while model.row_count() > len(value):
            del model[model.row_count() - 1]

    def key(self, text: str, down: bool) -> None:
        """A key from the panel: shown pressed, then handed to the view that showed it.

        Left and Right light their chevron while held. A soft key is drawn pressed for
        `press_flash_ms` before its view acts on it, as flipctl's own screens do, so the
        press is seen even when what it does replaces the screen.
        """
        key = _key_of(text)
        if key is None or self.view is None:
            return
        page = self.ensure()
        if key in (Key.LEFT, Key.RIGHT):
            arrow = (1 if key is Key.LEFT else 2) if down else 0
            page.set_global_property("AppKeys", "arrow_pressed", arrow)
        if not down:
            return
        view = self.view
        slot = key.soft_slot
        # OK in a menu inverts the row under the cursor for the same moment.
        row = key is Key.OK and isinstance(view, Menu)
        if slot is None and not row:
            view._press(key)
            return
        if row:
            self.set("pressed", True)
        else:
            page.set_global_property("AppKeys", "pressed_slot", slot)

        def release():
            if row:
                self.set("pressed", False)
            else:
                page.set_global_property("AppKeys", "pressed_slot", -1)
            view._press(key)

        flash = datetime.timedelta(milliseconds=theme("press_flash_ms", 30))
        slint.Timer.single_shot(flash, release)


_window = _Window()


def _buttons(given) -> list[str]:
    """Five soft-key labels, from a list or a function returning one."""
    labels = list(given() if callable(given) else given or [])
    return (labels + [""] * 5)[:5]


class _View:
    """What every view does: keys, closing, drawing and running."""

    def __init__(self, buttons, on_close):
        self._buttons_given = buttons
        self._on_close = on_close
        self._handler = None
        self.offset = 0

    def on_key(self, handler):
        """Use `handler(key)` for this view's keys. It returns True for a key it used."""
        self._handler = handler
        return handler

    def show(self) -> None:
        """Make this the view on screen."""
        _window.view = self
        self.redraw()

    def redraw(self) -> None:
        """Draw this view again from its functions, if it is the one showing."""
        if _window.view is self:
            self._draw(_window)

    def close(self) -> None:
        """Leave the app, after `on_close`, which may be a coroutine to wait for."""
        done = self._on_close() if self._on_close else None
        if inspect.isawaitable(done):

            async def then():
                await done
                slint.quit_event_loop()

            asyncio.get_running_loop().create_task(then())
        else:
            slint.quit_event_loop()

    def run(self, task=None, every: float | None = None) -> None:
        """Show this view and run until the app closes.

        `task` is a coroutine the same loop drives, for an app with something to wait
        on. `every` draws the view showing at that many seconds, for one that only
        reads.
        """
        page = _window.ensure()
        self.show()
        if every:
            timer = slint.Timer()
            timer.start(
                slint.TimerMode.Repeated,
                datetime.timedelta(seconds=every),
                lambda: _window.view and _window.view.redraw(),
            )
            _window.timers.append(timer)
        page.show()
        slint.run_event_loop(task)
        # Gone before the interpreter is: a Wayland window left to its teardown races
        # Slint's clipboard thread, and an app closed by a key crashed one exit in three.
        page.hide()

    def _press(self, key) -> None:
        used = self._handler(key) if self._handler else False
        if not used:
            if key is Key.ESCAPE:
                self.close()
                return
            if key is Key.DOWN:
                self._move(1)
            elif key is Key.UP:
                self._move(-1)
        # Whatever the handler did, including showing another view, is drawn now.
        if _window.view is not None:
            _window.view.redraw()

    def _move(self, step: int) -> None:
        """Up and Down: scroll, which drawing clamps to what there is."""
        self.offset += step

    def _draw(self, window: _Window) -> None:
        raise NotImplementedError


class Detail(_View):
    """A page of rows: `rows()` returns them, made with `pair`, `gauge` and the rest.

    `buttons` is the soft bar, five labels or a function returning them. Up and Down
    scroll a page longer than the panel. `fit_gauges` sizes a gauge around its own
    label rather than the column flipctl's screens use.
    """

    def __init__(self, rows, buttons=None, fit_gauges: bool = False, on_close=None):
        super().__init__(buttons, on_close)
        self._rows = rows
        self._fit_gauges = fit_gauges

    def _draw(self, window: _Window) -> None:
        rows = list(self._rows())
        first, last = next((r["_ends"] for r in rows if "_ends" in r), (False, False))
        rows = [{k: v for k, v in r.items() if k != "_ends"} for r in rows]
        # Clamped to what is there now, so a row that goes away does not leave the
        # page scrolled past its end.
        window.set("mode", 0)
        visible = window.page.get_property("visible_rows")
        self.offset = max(0, min(self.offset, len(rows) - visible))
        window.set("rows", rows)
        window.set("offset", self.offset)
        window.set("fit_gauges", self._fit_gauges)
        window.set("at_start", first)
        window.set("at_end", last)
        window.set("buttons", _buttons(self._buttons_given))


class Log(_View):
    """A log: `lines()` returns its lines, oldest first, under `title`.

    Lines longer than the panel are worth passing through `wrap` first. Up and Down
    scroll.
    """

    def __init__(self, lines, title: str = "", buttons=None, on_close=None):
        super().__init__(buttons, on_close)
        self._lines = lines
        self._title = title

    def _draw(self, window: _Window) -> None:
        lines = list(self._lines())
        visible = int(theme("log_visible_lines", 8))
        self.offset = max(0, min(self.offset, len(lines) - visible))
        window.set("mode", 1)
        window.set("log_title", self._title)
        window.set("lines", lines[self.offset : self.offset + visible])
        window.set("total", len(lines))
        window.set("log_offset", self.offset)
        window.set("buttons", _buttons(self._buttons_given))


class Menu(_View):
    """A menu: `items()` returns its rows, made with `item`.

    Up and Down move the cursor, round from the last row to the first, and the rows
    shown follow it; `selected` is the row under it. OK flashes that row before the
    handler sees it, as flipctl's own menus do.
    """

    def __init__(self, items, buttons=None, on_close=None):
        super().__init__(buttons, on_close)
        self._items = items
        self._count = 0
        self._scroll = 0
        self.selected = 0

    def _move(self, step: int) -> None:
        if self._count:
            self.selected = (self.selected + step) % self._count

    def _draw(self, window: _Window) -> None:
        items = list(self._items())
        self._count = len(items)
        self.selected = max(0, min(self.selected, self._count - 1))
        # The window moves only as far as keeps the cursor in it.
        visible = int(theme("list_visible_rows", 5))
        top = min(max(self._scroll, self.selected - visible + 1), self.selected)
        self._scroll = max(0, min(top, self._count - visible))
        window.set("mode", 2)
        window.set("items", items)
        window.set("selected", self.selected)
        window.set("scroll", self._scroll)
        window.set("buttons", _buttons(self._buttons_given))
