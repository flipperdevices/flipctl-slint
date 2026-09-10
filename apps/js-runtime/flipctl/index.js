// What a JavaScript app on the Flipper One draws with.
//
// The panel's widgets are Slint components and Slint's Node binding compiles them at
// run time, so an app here composes the same list.slint, detail.slint and the rest
// that flipctl's own screens and every Rust app are built from. Nothing is
// reimplemented and no token is retyped: load() points the compiler at the library
// the runtime carries, so @flipctl, @app and @theme mean in a script exactly what
// they mean in a crate.
//
//     const flipctl = require("flipctl");
//
//     const ui = flipctl.load(`
//         import { Shell } from "@app/shell.slint";
//         import { MenuBody } from "@flipctl/list.slint";
//         export component App inherits Shell {
//             in property <int> selected;
//             callback keyed(string, bool);
//             key(text, down) => { root.keyed(text, down); }
//             MenuBody { selected: root.selected; total: 1; real_frame: true;
//                        items: [{ label: "One" }];
//                        buttons: ["Close", "", "", "", "Ok"]; }
//         }
//     `);
//
//     flipctl.onKey(ui, (key, down) => {
//         if (down && key === flipctl.Key.Back) flipctl.quit();
//     });
//
//     flipctl.run(ui);
//
// The renderer is the software one and the font size is 16, both settled by the
// runtime, because the panel is two colours and the pixel fonts are exact at 16px and
// nowhere else.

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

function libraries() {
    const ui = uiDir();
    return {
        flipctl: path.join(ui, "crates/flipper-ui/ui"),
        app: path.join(ui, "crates/flipctl-app/ui"),
        theme: path.join(ui, "theme.slint"),
    };
}

// The component a source exports: the one called App, or its only one.
//
// The exports are not enumerable, so Object.keys comes back empty and finding them
// means asking for the name or walking the own property names.
function instantiate(module_, source) {
    let Component = module_.App;
    if (typeof Component !== "function") {
        Component = Object.getOwnPropertyNames(module_)
            .map((name) => module_[name])
            .find((value) => typeof value === "function");
    }
    if (typeof Component !== "function") {
        throw new Error(`${source}: nothing exported to draw`);
    }
    return new Component();
}

// A component from .slint source, so one file can be a whole app.
function load(source, name = "app.slint") {
    const where = path.join(process.cwd(), name);
    return instantiate(slint.loadSource(source, where, { libraryPaths: libraries() }), name);
}

// A component from a .slint file beside the script.
function loadFile(file) {
    return instantiate(slint.loadFile(file, { libraryPaths: libraries() }), String(file));
}

// The panel's buttons, by name.
const Key = {
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
};

// What Slint puts in a key event's text. The five soft buttons are letters rather
// than function keys, because that is what the input driver sends for them: KEY_Z,
// KEY_X, KEY_C, KEY_V, KEY_B, and KEY_A for push to talk. Guessing F1 to F5 is why an
// app looks deaf while its clock keeps ticking.
const NAMED = {
    "\uf700": Key.Up,
    "\uf701": Key.Down,
    "\uf702": Key.Left,
    "\uf703": Key.Right,
    "\n": Key.Ok,
    "\b": Key.Back,
    z: Key.Escape,
    x: Key.View,
    c: Key.Power,
    v: Key.Edit,
    b: Key.Run,
    a: Key.Ptt,
};

// The key behind a Slint key event, or undefined for one the panel has no button for.
function keyOf(text) {
    return NAMED[text.length === 1 ? text.toLowerCase() : text];
}

// Bind a handler to the shell's key callback, by name rather than by character.
//
// Shell declares `callback key(string, bool)`, and a component inheriting it cannot
// re-export a callback it did not declare, so an app forwards it to one of its own
// called `keyed`. Either is bound here, whichever the component has.
function onKey(ui, handler) {
    const forward = (text, down) => {
        const key = keyOf(text);
        if (key !== undefined) {
            handler(key, down);
        }
    };
    for (const name of ["keyed", "key"]) {
        if (name in ui) {
            ui[name] = forward;
            return;
        }
    }
    throw new Error(
        "the component declares no key callback: inherit Shell and forward its " +
            "`key` to a `keyed` callback of your own",
    );
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

// An array property wants a model, and a plain array is not one.
function rows(items) {
    return new slint.ArrayModel(items);
}

// Show the component and run its loop until it quits. Node's timers keep working
// while it runs, since the binding drives the loop through libuv.
function run(ui) {
    return ui.run();
}

function quit() {
    slint.quitEventLoop();
}

module.exports = {
    Key,
    keyOf,
    load,
    loadFile,
    onKey,
    quit,
    rows,
    run,
    textWidth,
    theme,
    uiDir,
    wrap,
};
