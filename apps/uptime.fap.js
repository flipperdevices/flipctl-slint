#!/usr/bin/env node
// /// flipctl
// name = "Uptime"
// status = true
// ///
//
// How long this machine has been up, and how hard it is working.
//
// One file is the whole app. The block above is its manifest, which flipctl reads
// without running this, and the runtime it asks for is the one its extension names:
// `js`, which the JavaScript runtime bundle answers to. What draws is the panel's own
// widget library through the `flipctl` module that runtime carries, so no token and
// no geometry is repeated here.
//
// Everything on screen comes from two files in procfs and nothing else, read once a
// second: /proc/uptime for how long and how much of it was idle, and /proc/loadavg
// for the load and the process counts.

"use strict";

const fs = require("node:fs");
const os = require("node:os");
const flipctl = require("flipctl");

const PAGE = `
import { Shell } from "@app/shell.slint";
import { DetailBody, DetailRow } from "@flipctl/detail.slint";

export component App inherits Shell {
    in property <[DetailRow]> rows;
    in property <[string]> buttons;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    DetailBody {
        rows: root.rows;
        buttons: root.buttons;
        // The gauge is sized around its own label, which is an app's to choose.
        fit_gauges: true;
    }
}
`;

const CORES = os.cpus().length;

function pair(label, value, dim = false) {
    return { kind: 0, label, value, percent: 0, dim };
}

function rule() {
    return { kind: 1, label: "", value: "", percent: 0, dim: false };
}

function gauge(label, value, percent) {
    return { kind: 2, label, value, percent, dim: false };
}

// Days, hours, minutes, seconds, dropping the units that are zero from the left.
// Seconds are shown only while there are no days, since a machine up for a week does
// not need them and the row has a right edge.
function spell(seconds) {
    const day = Math.floor(seconds / 86400);
    const hour = Math.floor((seconds % 86400) / 3600);
    const minute = Math.floor((seconds % 3600) / 60);
    const second = Math.floor(seconds % 60);
    if (day > 0) {
        return `${day}d ${hour}h ${minute}m`;
    }
    if (hour > 0) {
        return `${hour}h ${minute}m ${second}s`;
    }
    return minute > 0 ? `${minute}m ${second}s` : `${second}s`;
}

function reading(path) {
    try {
        return fs.readFileSync(path, "utf8").trim();
    } catch {
        return "";
    }
}

// Two numbers: seconds since boot, and seconds all the cores have spent idle. The
// second one is a sum across cores, so the share of a machine that has been idle is
// it divided by the uptime and the core count.
function uptime() {
    const [up, idle] = reading("/proc/uptime").split(/\s+/).map(Number);
    return { up: up || 0, idle: idle || 0 };
}

// The three load averages, then how many processes are runnable out of how many there
// are, then the last pid, which nothing here needs.
function loadavg() {
    const fields = reading("/proc/loadavg").split(/\s+/);
    const [running, total] = (fields[3] || "0/0").split("/");
    return {
        one: Number(fields[0]) || 0,
        five: Number(fields[1]) || 0,
        fifteen: Number(fields[2]) || 0,
        running: running || "0",
        total: total || "0",
    };
}

function rows() {
    const { up, idle } = uptime();
    const load = loadavg();
    // Load is a count of runnable processes, so a machine is fully committed when it
    // matches the core count. Shown as a share of that, clamped, because a load above
    // the core count says "oversubscribed" and a bar cannot say more than full.
    const busy = Math.min(100, Math.round((load.one / CORES) * 100));
    const wasIdle = up > 0 ? Math.round((idle / (up * CORES)) * 100) : 0;
    const booted = new Date(Date.now() - up * 1000);
    const clock = booted.toTimeString().slice(0, 5);
    const day = booted.toDateString().slice(4, 10);

    return [
        pair("Uptime", spell(up)),
        pair("Booted", `${day} ${clock}`),
        rule(),
        pair("Load", load.one.toFixed(2)),
        pair("5m / 15m", `${load.five.toFixed(2)} ${load.fifteen.toFixed(2)}`, true),
        pair("Processes", `${load.running} of ${load.total}`, true),
        pair("Idle", `${wasIdle}%`, true),
        gauge(`Busy of ${CORES}`, `${busy}%`, busy),
    ];
}

function main() {
    const ui = flipctl.load(PAGE, "uptime.slint");

    const draw = () => {
        ui.rows = flipctl.rows(rows());
        ui.buttons = flipctl.rows(["Close", "", "", "", ""]);
    };

    flipctl.onKey(ui, (key, down) => {
        if (down && (key === flipctl.Key.Back || key === flipctl.Key.Escape)) {
            flipctl.quit();
        }
    });

    draw();
    // Node's timers keep running while the Slint loop does, because the binding
    // watches libuv's backend rather than replacing the loop.
    const beat = setInterval(draw, 1000);
    flipctl.run(ui).then(() => clearInterval(beat));
}

main();
