// The JavaScript runtime's own screen.
//
// Opened from the Apps list with no argument, this says what the runtime is and what
// the last script that failed had to say. Given a script it runs it instead, which is
// what AppRun does with the argument flipctl passes.
//
// It is written with the library it provides, so the launcher is the first app to
// prove the thing it is for.

"use strict";

const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const flipctl = require("flipctl");

const PAGE = `
import { Shell } from "@app/shell.slint";
import { DetailBody, DetailRow } from "@flipctl/detail.slint";
import { LogBody } from "@flipctl/log.slint";

export component App inherits Shell {
    in property <[DetailRow]> rows;
    in property <[string]> lines;
    in property <int> offset;
    in property <int> total;
    in property <bool> log;
    in property <[string]> buttons;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    if !root.log: DetailBody {
        rows: root.rows;
        buttons: root.buttons;
    }

    if root.log: LogBody {
        title: "Last error";
        lines: root.lines;
        total: root.total;
        offset: root.offset;
        buttons: root.buttons;
    }
}
`;

// Where flipctl gives every app a writable directory of its own, and where AppRun
// leaves what a script said as it died.
const APPS = path.join(
    process.env.XDG_DATA_HOME || path.join(os.homedir(), ".local/share"),
    "flipctl/apps",
);
const ERROR = "last-error.log";

function pair(label, value) {
    return { kind: 0, label, value, percent: 0, dim: false };
}

function rule() {
    return { kind: 1, label: "", value: "", percent: 0, dim: false };
}

// The scripts in the Apps folder that this runtime would be asked to run.
//
// The block is what makes one an app, exactly as flipctl decides it: a .js without
// one is somebody else's module sitting in the folder, and neither of us lists it.
function scripts() {
    const root = path.join(os.homedir(), "Apps");
    const found = [];
    const walk = (dir) => {
        let entries = [];
        try {
            entries = fs.readdirSync(dir, { withFileTypes: true });
        } catch {
            return;
        }
        for (const entry of entries) {
            const full = path.join(dir, entry.name);
            if (entry.isDirectory()) {
                walk(full);
            } else if (entry.name.endsWith(".js")) {
                let head = "";
                try {
                    head = fs.readFileSync(full, "utf8").slice(0, 8192);
                } catch {
                    continue;
                }
                if (head.split("\n").some((line) => line.trimEnd() === "// /// flipctl")) {
                    found.push(full);
                }
            }
        }
    };
    walk(root);
    return found;
}

// The newest traceback any script left behind, and whose it was.
function newestLog() {
    let newest = null;
    let stamp = 0;
    let dirs = [];
    try {
        dirs = fs.readdirSync(APPS);
    } catch {
        return null;
    }
    for (const dir of dirs) {
        const file = path.join(APPS, dir, ERROR);
        try {
            const info = fs.statSync(file);
            if (info.size > 0 && info.mtimeMs > stamp) {
                stamp = info.mtimeMs;
                newest = { name: dir, file };
            }
        } catch {
            // No log for that app, which is the ordinary case.
        }
    }
    return newest;
}

function facts() {
    const newest = newestLog();
    return [
        pair("Node", process.versions.node),
        pair("Slint", require("slint-ui/package.json").version),
        rule(),
        pair("Scripts", String(scripts().length)),
        pair("Last error", newest ? newest.name : "none"),
    ];
}

// The last traceback, wrapped to the panel. A stack is one long line per frame and
// the log body clips rather than wraps, so the line naming the error is exactly the
// one that would be lost. flipctl.wrap measures with the same font metrics flipctl
// measures its own log with.
function logLines() {
    const newest = newestLog();
    if (!newest) {
        return ["no script has failed"];
    }
    const text = fs.readFileSync(newest.file, "utf8");
    return text
        .split("\n")
        .filter((line) => line.length > 0)
        .flatMap((line) => flipctl.wrap(line));
}

// Empty the log on screen, so the next failure is the one being read.
//
// Emptied rather than deleted: AppRun truncates this file at the start of every run
// and writes the script's stderr into it, so removing it buys nothing. If an older
// log is still waiting behind this one it becomes the newest, and a second press
// clears that.
function clearError() {
    const newest = newestLog();
    if (newest) {
        fs.writeFileSync(newest.file, "");
    }
}

function main() {
    const ui = flipctl.load(PAGE, "launcher.slint");
    const visible = Math.trunc(flipctl.theme("log_visible_lines", 8));
    const state = { log: false, offset: 0 };

    const show = () => {
        ui.log = state.log;
        if (state.log) {
            const lines = logLines();
            state.offset = Math.max(0, Math.min(state.offset, lines.length - visible));
            ui.lines = flipctl.rows(lines.slice(state.offset, state.offset + visible));
            ui.total = lines.length;
            ui.offset = state.offset;
            // Nothing to clear, no button offering to: the slot empties instead.
            ui.buttons = flipctl.rows(["Close", "", "", newestLog() ? "Clear" : "", "Runtime"]);
        } else {
            ui.rows = flipctl.rows(facts());
            ui.buttons = flipctl.rows(["Close", "", "", "", "Last error"]);
        }
    };

    flipctl.onKey(ui, (key, down) => {
        if (!down) {
            return;
        }
        if (key === flipctl.Key.Back || key === flipctl.Key.Escape) {
            flipctl.quit();
        } else if (key === flipctl.Key.Run) {
            state.log = !state.log;
            state.offset = 0;
            show();
        } else if (state.log && key === flipctl.Key.Edit) {
            clearError();
            state.offset = 0;
            show();
        } else if (state.log && key === flipctl.Key.Down) {
            state.offset += 1;
            show();
        } else if (state.log && key === flipctl.Key.Up) {
            state.offset = Math.max(0, state.offset - 1);
            show();
        }
    });

    show();
    flipctl.run(ui);
}

main();
