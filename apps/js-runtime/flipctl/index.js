// What a JavaScript app on the Flipper One draws with.
//
// An app is made of views: a page of rows, a menu or a log. It hands each the
// functions that say what it shows, and the window, the keys, scrolling and the soft
// bar's press are done here, by the same widgets flipctl's own screens and every Rust
// app are built from. Nothing is reimplemented and no token is retyped.
//
//     const flipctl = require("flipctl");
//
//     let count = 0;
//     const app = flipctl.detail({
//         rows: () => [flipctl.pair("Pressed", String(count))],
//         buttons: ["Close", "", "", "", "Add"],
//     });
//     app.onKey((key) => {
//         if (key === flipctl.Key.Run) {
//             count += 1;
//             return true;
//         }
//         return false;
//     });
//     app.run();
//
// A handler gets a key as it goes down and returns true when it used it; anything it
// leaves is the view's: Escape closes, Up and Down scroll or move a menu's cursor. Back
// is never seen here: it belongs to flipctl, which sends the app to the background.
// After every key the view is drawn again from its functions, and `every` draws it on
// a clock too.
//
// Node's own timers and promises keep running beside the window, since the binding
// drives the loop through libuv: an app with something to wait on does it the usual
// way and calls `app.redraw()` when it has something new. Another view replaces the
// one showing with `other.show()`, in the same window.
//
// The renderer is chosen by the runtime and the font size is 16, because the panel is
// two colours and the pixel fonts are exact at 16px and nowhere else.

"use strict";

const fs = require("node:fs");
const path = require("node:path");
const slint = require("slint-ui");

// Where the widget library, the theme and the fonts are. The runtime sets FLIPCTL_UI
// to its own copy; the fallback is where an image that ships the sources would put
// them, so a script run by hand still finds them.
function uiDir() {
    return process.env.FLIPCTL_UI || "/usr/share/flipctl/ui";
}

// A design token, by the name it has in @theme.
//
// Read out of the generated theme.slint the runtime carries, so a script gets the
// same number flipctl and every Rust app were built with instead of retyping it.
// Lengths come back in pixels and counts as themselves.
let tokens = null;
function theme(name, fallback = 0) {
    if (tokens === null) {
        tokens = {};
        try {
            const text = fs.readFileSync(path.join(uiDir(), "theme.slint"), "utf8");
            const pattern = /out property <(?:length|int|duration)> (\w+):\s*([\d.]+)/g;
            for (const [, key, value] of text.matchAll(pattern)) {
                tokens[key] = Number(value);
            }
        } catch {
            // No theme to read: every caller has a fallback for exactly this.
        }
    }
    return name in tokens ? tokens[name] : fallback;
}

// Advance widths of the panel's 16px title face, ASCII 32..126, one digit each.
// Generated from crates/flipper-ui/src/font/title.rs, which is itself generated from
// the font, and checked against it by tools/appimage/test_bundle.py. This is the face
// Shell sets as the window default, so it is what an app's text is drawn in.
const ADVANCES = "22466872446636256366666666234646966666666466686666666668666454664555553552352855554445585554247";

// Width in pixels, the way flipper_ui::font measures it: the sum of the advances less
// one, because each advance carries a trailing spacing column the last glyph does not
// need. A codepoint outside the font is drawn as `?` and measured as one.
function textWidth(text) {
    let total = 0;
    for (const character of text) {
        let index = character.codePointAt(0) - 32;
        if (index < 0 || index >= ADVANCES.length) {
            index = "?".codePointAt(0) - 32;
        }
        total += Number(ADVANCES[index]);
    }
    return Math.max(0, total - 1);
}

// One line of output broken into lines that fit the panel. Breaks at spaces, and
// splits a single token too long to fit on its own, because a path or an exception
// message is often wider than the screen and dropping its tail loses exactly the part
// worth reading. Same rule as flipctl's own log.
function wrap(text, width = 0) {
    if (!width) {
        width = theme("panel_w", 256) - 2 * theme("margin_h", 8);
    }
    const out = [];
    let line = "";
    for (let word of text.split(/\s+/).filter((piece) => piece.length > 0)) {
        const candidate = line ? `${line} ${word}` : word;
        if (textWidth(candidate) <= width) {
            line = candidate;
            continue;
        }
        if (line) {
            out.push(line);
            line = "";
        }
        while (textWidth(word) > width) {
            let cut = word.length;
            while (cut > 1 && textWidth(word.slice(0, cut)) > width) {
                cut -= 1;
            }
            out.push(word.slice(0, cut));
            word = word.slice(cut);
        }
        line = word;
    }
    if (line) {
        out.push(line);
    }
    return out.length > 0 ? out : [""];
}

// ── Rows ─────────────────────────────────────────────────────────────────────
//
// Made whole, every field of the struct filled, so a row means the same here as it
// does to the Python library, which has no choice about it.

function row(kind, label = "", value = "", percent = 0, dim = false) {
    return { kind, label, value, percent, dim };
}

// A label on the left and its value on the right. Dim for a secondary line.
function pair(label, value = "", dim = false) {
    return row(0, label, value, 0, dim);
}

// A divider between groups of rows.
function rule() {
    return row(1);
}

// A label, a bar filled to `percent` (0 to 100), and the value beside it.
function gauge(label, value, percent) {
    return row(2, label, value, Math.max(0, Math.min(100, Math.trunc(percent))));
}

// A line of text across the whole row.
function text(line, dim = false) {
    return row(3, line, "", 0, dim);
}

// A value Left and Right change, between chevrons. `first` and `last` say the value
// is at an end of its range, which drops the chevron pointing past it.
function choice(label, value, first = false, last = false) {
    return { ...row(4, label, value), ends: [first, last] };
}

// A label and value with the label in the bold face.
function heading(label, value = "") {
    return row(5, label, value);
}

// A menu row: its label and, at the right edge, a `status`.
//
// With `chevrons` the right edge is instead `value` between chevrons that say Left and
// Right change it: 1 for an on/off toggle, 2 for a value with a range. `first` and
// `last` drop the chevron pointing past an end of that range. `dim` keeps the status
// in the dim tone when the row is selected, for a remark about the row rather than its
// value; `tag` is a short word drawn right after the label.
function item(label, options = {}) {
    const { status = "", value = "", chevrons = 0, first = false, last = false } = options;
    return {
        label,
        tag: options.tag || "",
        status,
        icon: 0,
        frames: 1,
        chevrons,
        dim: Boolean(options.dim),
        value,
        at_start: first,
        at_end: last,
    };
}

// ── Keys ─────────────────────────────────────────────────────────────────────

// The panel's buttons, by name.
const Key = Object.freeze({
    Up: "up",
    Down: "down",
    Left: "left",
    Right: "right",
    Ok: "ok",
    Back: "back",
    Escape: "escape",
    View: "view",
    Power: "power",
    Edit: "edit",
    Run: "run",
    Ptt: "ptt",
});

// Which soft key a key is, counting from the left.
const SOFT = [Key.Escape, Key.View, Key.Power, Key.Edit, Key.Run];

// What Slint puts in a key event's text. The five soft buttons are letters rather
// than function keys, because that is what the input driver sends for them: KEY_Z,
// KEY_X, KEY_C, KEY_V, KEY_B, and KEY_A for push to talk. Guessing F1 to F5 is why an
// app looks deaf while its clock keeps ticking.
const NAMED = {
    "": Key.Up,
    "": Key.Down,
    "": Key.Left,
    "": Key.Right,
    "\n": Key.Ok,
    "\b": Key.Back,
    z: Key.Escape,
    x: Key.View,
    c: Key.Power,
    v: Key.Edit,
    b: Key.Run,
    a: Key.Ptt,
};

function keyOf(text) {
    return NAMED[text.length === 1 ? text.toLowerCase() : text];
}

// ── The window ───────────────────────────────────────────────────────────────

// The one window every view draws into, made on first use. One component holds a
// body per kind of view, so moving between a list and a log is a change of mode
// rather than a second window, which a hosted app does not get.
const window = {
    page: null,
    view: null,
    models: {},

    ensure() {
        if (this.page === null) {
            const ui = uiDir();
            const built = slint.loadFile(path.join(ui, "crates/flipctl-app/ui/pages.slint"), {
                libraryPaths: {
                    flipctl: path.join(ui, "crates/flipper-ui/ui"),
                    app: path.join(ui, "crates/flipctl-app/ui"),
                    theme: path.join(ui, "theme.slint"),
                },
            });
            this.page = new built.Page();
            this.page.AppKeys.key = (text, down) => this.key(text, down);
        }
        return this.page;
    },

    // Set a property, an array as the model kept behind it and refilled in place, so
    // a redraw notifies only the rows that changed.
    set(name, value) {
        const page = this.ensure();
        if (!Array.isArray(value)) {
            page[name] = value;
            return;
        }
        let model = this.models[name];
        if (!model) {
            model = this.models[name] = new slint.ArrayModel(value);
            page[name] = model;
            return;
        }
        value.forEach((item, i) => {
            if (i >= model.rowCount()) {
                model.push(item);
            } else if (JSON.stringify(model.rowData(i)) !== JSON.stringify(item)) {
                model.setRowData(i, item);
            }
        });
        if (model.rowCount() > value.length) {
            model.remove(value.length, model.rowCount() - value.length);
        }
    },

    // A key from the panel: shown pressed, then handed to the view that showed it.
    // Left and Right light their chevron while held. A soft key is drawn pressed for
    // press_flash_ms before its view acts on it, as flipctl's own screens do, so the
    // press is seen even when what it does replaces the screen.
    key(text, down) {
        const key = keyOf(text);
        if (key === undefined || this.view === null) {
            return;
        }
        const page = this.ensure();
        if (key === Key.Left || key === Key.Right) {
            page.AppKeys.arrow_pressed = down ? (key === Key.Left ? 1 : 2) : 0;
        }
        if (!down) {
            return;
        }
        const view = this.view;
        const slot = SOFT.indexOf(key);
        // OK in a menu inverts the row under the cursor for the same moment.
        const row = key === Key.Ok && view instanceof Menu;
        if (slot < 0 && !row) {
            view.press(key);
            return;
        }
        if (row) {
            this.set("pressed", true);
        } else {
            page.AppKeys.pressed_slot = slot;
        }
        setTimeout(() => {
            if (row) {
                this.set("pressed", false);
            } else {
                page.AppKeys.pressed_slot = -1;
            }
            view.press(key);
        }, theme("press_flash_ms", 30));
    },
};

// Five soft-key labels, from an array or a function returning one.
function buttons(given) {
    const labels = Array.from((typeof given === "function" ? given() : given) || []);
    return labels.concat(["", "", "", "", ""]).slice(0, 5);
}

// What every view does: keys, closing, drawing and running.
class View {
    constructor(options) {
        this.buttons = options.buttons;
        this.closing = options.onClose;
        this.handler = null;
        this.offset = 0;
    }

    // Use `handler(key)` for this view's keys. It returns true for a key it used.
    onKey(handler) {
        this.handler = handler;
        return this;
    }

    // Make this the view on screen.
    show() {
        window.view = this;
        this.redraw();
    }

    // Draw this view again from its functions, if it is the one showing.
    redraw() {
        if (window.view === this) {
            this.draw(window);
        }
    }

    // Leave the app, after onClose, which may return a promise to wait for.
    close() {
        Promise.resolve(this.closing ? this.closing() : undefined).finally(() =>
            slint.quitEventLoop(),
        );
    }

    // Show this view and run until the app closes. `every` draws the view showing at
    // that many seconds, for an app that only reads.
    run({ every } = {}) {
        const page = window.ensure();
        this.show();
        const timer = every
            ? setInterval(() => window.view && window.view.redraw(), every * 1000)
            : null;
        return page.run().finally(() => timer && clearInterval(timer));
    }

    // Up and Down: scroll, which drawing clamps to what there is.
    move(step) {
        this.offset += step;
    }

    press(key) {
        const used = this.handler ? this.handler(key) : false;
        if (!used) {
            if (key === Key.Escape) {
                this.close();
                return;
            }
            if (key === Key.Down) {
                this.move(1);
            } else if (key === Key.Up) {
                this.move(-1);
            }
        }
        // Whatever the handler did, including showing another view, is drawn now.
        if (window.view) {
            window.view.redraw();
        }
    }
}

// A page of rows: `rows()` returns them, made with pair, gauge and the rest.
//
// `buttons` is the soft bar, five labels or a function returning them. Up and Down
// scroll a page longer than the panel. `fitGauges` sizes a gauge around its own label
// rather than the column flipctl's screens use.
class Detail extends View {
    constructor(options) {
        super(options);
        this.rows = options.rows;
        this.fitGauges = Boolean(options.fitGauges);
    }

    draw(win) {
        const rows = Array.from(this.rows());
        const [first, last] = (rows.find((r) => r.ends) || { ends: [false, false] }).ends;
        const clean = rows.map(({ ends, ...r }) => r);
        win.set("mode", 0);
        // Clamped to what is there now, so a row that goes away does not leave the
        // page scrolled past its end.
        const visible = win.page.visible_rows;
        this.offset = Math.max(0, Math.min(this.offset, clean.length - visible));
        win.set("rows", clean);
        win.set("offset", this.offset);
        win.set("fit_gauges", this.fitGauges);
        win.set("at_start", first);
        win.set("at_end", last);
        win.set("buttons", buttons(this.buttons));
    }
}

// A log: `lines()` returns its lines, oldest first, under `title`. Lines longer than
// the panel are worth passing through wrap first. Up and Down scroll.
class Log extends View {
    constructor(options) {
        super(options);
        this.lines = options.lines;
        this.title = options.title || "";
    }

    draw(win) {
        const lines = Array.from(this.lines());
        const visible = Math.trunc(theme("log_visible_lines", 8));
        this.offset = Math.max(0, Math.min(this.offset, lines.length - visible));
        win.set("mode", 1);
        win.set("log_title", this.title);
        win.set("lines", lines.slice(this.offset, this.offset + visible));
        win.set("total", lines.length);
        win.set("log_offset", this.offset);
        win.set("buttons", buttons(this.buttons));
    }
}

// A menu: `items()` returns its rows, made with item.
//
// Up and Down move the cursor, round from the last row to the first, and the rows shown
// follow it; `selected` is the row under it. OK flashes that row before the handler
// sees it, as flipctl's own menus do.
class Menu extends View {
    constructor(options) {
        super(options);
        this.items = options.items;
        this.count = 0;
        this.top = 0;
        this.selected = 0;
    }

    move(step) {
        if (this.count) {
            this.selected = (this.selected + step + this.count) % this.count;
        }
    }

    draw(win) {
        const items = Array.from(this.items());
        this.count = items.length;
        this.selected = Math.max(0, Math.min(this.selected, this.count - 1));
        // The window moves only as far as keeps the cursor in it.
        const visible = Math.trunc(theme("list_visible_rows", 5));
        const top = Math.min(Math.max(this.top, this.selected - visible + 1), this.selected);
        this.top = Math.max(0, Math.min(top, this.count - visible));
        win.set("mode", 2);
        win.set("items", items);
        win.set("selected", this.selected);
        win.set("scroll", this.top);
        win.set("buttons", buttons(this.buttons));
    }
}

module.exports = {
    Key,
    choice,
    detail: (options) => new Detail(options),
    gauge,
    heading,
    item,
    log: (options) => new Log(options),
    menu: (options) => new Menu(options),
    pair,
    rule,
    text,
    textWidth,
    theme,
    wrap,
};
