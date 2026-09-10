"""What a Python app on the Flipper One draws with.

The panel's widgets are Slint components, and Slint's Python binding compiles them at
run time, so an app here composes the same `list.slint`, `detail.slint` and the rest
that flipctl's own screens and every Rust app are built from. Nothing is
reimplemented and no token is retyped: `load` points the compiler at the library the
launcher carries, so `@flipctl`, `@app` and `@theme` mean in a script exactly what
they mean in a crate.

    import flipctl

    ui = flipctl.load('''
        import { Shell } from "@app/shell.slint";
        import { MenuBody } from "@flipctl/list.slint";
        export component App inherits Shell {
            in property <int> selected;
            callback keyed(string, bool);
            key(text, down) => { root.keyed(text, down); }
            MenuBody {
                items: [{ label: "One" }, { label: "Two" }];
                selected: root.selected;
                total: 2;
                real_frame: true;
                buttons: ["Close", "", "", "", "Ok"];
            }
        }
    ''')

    @flipctl.on_key(ui)
    def _(key, down):
        if down and key is flipctl.Key.DOWN:
            ui.selected = 1

    flipctl.run(ui)

An app that also has something to wait on hands `run` a coroutine, which the same
loop drives: see `apps/ping.py`, which reads its replies off a socket.

The renderer is the software one and the font size is 16, both settled by the
launcher, because the panel is two colours and the pixel fonts are exact at 16px and
nowhere else.
"""

import enum
import os
import pathlib
import re
import sys

import slint

__all__ = ["Key", "key_of", "load", "load_file", "on_key", "run", "text_width",
           "theme", "ui_dir", "wrap"]


def ui_dir() -> pathlib.Path:
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
            text = (ui_dir() / "theme.slint").read_text()
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


def _libraries() -> dict[str, pathlib.Path]:
    ui = ui_dir()
    return {
        "flipctl": ui / "crates/flipper-ui/ui",
        "app": ui / "crates/flipctl-app/ui",
        "theme": ui / "theme.slint",
    }


def _compiler() -> "slint.native.Compiler":
    compiler = slint.native.Compiler()
    compiler.library_paths = _libraries()
    return compiler


class App:
    """A component, with its properties as attributes and its callbacks bindable.

    What the compiler hands back is a `ComponentInstance`, which speaks
    `set_property` and `set_callback` and has no attributes of its own. That is a
    workable interface and a tiresome one to write an app against, so this wraps it:
    `ui.rows = [...]` where the property exists, and a plain AttributeError naming it
    where it does not, rather than a silent assignment that draws nothing.
    """

    def __init__(self, instance, properties, callbacks):
        object.__setattr__(self, "_instance", instance)
        object.__setattr__(self, "_properties", set(properties))
        object.__setattr__(self, "_callbacks", set(callbacks))
        object.__setattr__(self, "_models", {})

    def __getattr__(self, name):
        if name in object.__getattribute__(self, "_properties"):
            return object.__getattribute__(self, "_instance").get_property(name)
        raise AttributeError(f"no property {name!r}; the component has {self.properties()}")

    def __setattr__(self, name, value):
        if name not in object.__getattribute__(self, "_properties"):
            raise AttributeError(f"no property {name!r}; the component has {self.properties()}")
        # An array in Slint is a model, and a plain list is not one: handing it over
        # raw is refused with \"Object is not a dict or NamedTuple\", which says
        # nothing about the list being the problem. Rows are what an app sets most,
        # so they are wrapped here rather than in every app.
        if isinstance(value, (list, tuple)):
            value = self._model(name, list(value))
        object.__getattribute__(self, "_instance").set_property(name, value)

    def _model(self, name: str, rows: list):
        """The model behind one array property, kept and refilled rather than remade.

        Slint holds the model weakly, so a fresh `ListModel` per redraw is collected
        the moment the app drops its own reference and the panel fills with \"Model
        implementation is lacking self object (in row_count)\" while the screen stops
        updating. Keeping it here fixes that, and refilling in place means a redraw
        notifies only the rows that actually changed.
        """
        models = object.__getattribute__(self, "_models")
        model = models.get(name)
        if model is None:
            model = slint.ListModel(rows)
            models[name] = model
            return model
        for i, row in enumerate(rows):
            if i >= model.row_count():
                model.append(row)
            elif model.row_data(i) != row:
                model.set_row_data(i, row)
        while model.row_count() > len(rows):
            del model[model.row_count() - 1]
        return model

    def properties(self) -> list[str]:
        return sorted(object.__getattribute__(self, "_properties"))

    def callbacks(self) -> list[str]:
        return sorted(object.__getattribute__(self, "_callbacks"))

    def on(self, name: str, handler) -> None:
        """Bind one of the component's callbacks."""
        if name not in object.__getattribute__(self, "_callbacks"):
            raise AttributeError(f"no callback {name!r}; the component has {self.callbacks()}")
        object.__getattribute__(self, "_instance").set_callback(name, handler)

    def show(self) -> None:
        object.__getattribute__(self, "_instance").show()

    def hide(self) -> None:
        object.__getattribute__(self, "_instance").hide()


def _one(result, source: str) -> App:
    """The component a file exports: the one called `App`, or its only one."""
    for diagnostic in result.diagnostics:
        print(f"flipctl: {source}: {diagnostic}", file=sys.stderr)
    names = result.component_names
    if not names:
        raise RuntimeError(f"{source}: nothing exported to draw")
    definition = result.component("App" if "App" in names else names[0])
    return App(definition.create(), definition.properties, definition.callbacks)


def load(source: str, name: str = "app.slint") -> App:
    """A component from `.slint` source, so one file can be a whole app."""
    return _one(_compiler().build_from_source(source, pathlib.Path(name)), name)


def load_file(path) -> App:
    """A component from a `.slint` file beside the script."""
    path = pathlib.Path(path)
    return _one(_compiler().build_from_path(path), str(path))


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
    "\uf700": Key.UP,
    "\uf701": Key.DOWN,
    "\uf702": Key.LEFT,
    "\uf703": Key.RIGHT,
    "\n": Key.OK,
    "\b": Key.BACK,
    "z": Key.ESCAPE,
    "x": Key.VIEW,
    "c": Key.POWER,
    "v": Key.EDIT,
    "b": Key.RUN,
    "a": Key.PTT,
}


def key_of(text: str):
    """The key behind a Slint key event, or None for one the panel has no button for."""
    return _NAMED.get(text.lower() if len(text) == 1 else text)


def on_key(component):
    """Bind a handler to the shell's key callback, by name rather than by character.

    `Shell` declares `callback key(string, bool)`, and a component inheriting it
    cannot re-export a callback it did not declare, so an app forwards it to one of
    its own called `keyed`. Either is bound here, whichever the component has.
    """

    def bind(handler):
        def forward(text, down):
            key = key_of(text)
            if key is not None:
                handler(key, down)

        for name in ("keyed", "key"):
            if name in component.callbacks():
                component.on(name, forward)
                return handler
        raise RuntimeError(
            "the component declares no key callback: inherit Shell and forward its "
            "`key` to a `keyed` callback of your own"
        )

    return bind


def run(component: App, main=None) -> None:
    """Show the component and run its loop until it quits.

    Slint's loop is an asyncio one, so `main` may be a coroutine and it runs beside
    the drawing on the same thread: a socket awaited there wakes the app the moment
    it is readable, which is both simpler and more accurate than a timer that looks.
    An app with nothing to wait on passes nothing and binds keys alone.
    """
    component.show()
    slint.run_event_loop(main)
