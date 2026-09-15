#!/usr/bin/env python3
# /// flipctl
# name = "Ping"
# status = true
# ///
"""Ping, on the panel.

One file is the whole app. The block above is its manifest, which flipctl reads
without running this, and what draws is the panel's own widget library through the
`flipctl` package the Python runtime carries, so no token and no geometry is retyped
here. `status = true` in the block is the other half of that: the panel paints its own
status bar into the app's frame, so a script gets the real battery and radios without
reading a single sysfs file.

The echoes are this app's own. Linux hands an unprivileged process ICMP datagram
sockets, which is how `ping` itself works without being setuid, so there is no tool to
call and nothing to install. And because Slint's loop is an asyncio loop, the socket
is awaited rather than polled: the reply wakes the app when it lands. A timer looking
every 100ms instead would report every round trip as 100ms, which on a link that
answers in half of one is not a measurement.

Left, Right or Host change what is being pinged, Run starts and stops it, Back leaves.
"""

import asyncio
import socket
import struct
import time

import flipctl
import slint

PAGE = """
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
"""

ECHO_REQUEST = 8
ECHO_REPLY = 0
# One probe a second, given up on after a second.
PERIOD = 1.0


def gateway():
    """The default route's gateway, which is the first host worth asking about.

    A device with no route out still has one to its own gateway, and an app that opens
    on a timeout looks broken rather than informative.
    """
    try:
        with open("/proc/net/route", encoding="ascii") as routes:
            for line in routes.read().splitlines()[1:]:
                fields = line.split()
                if len(fields) > 2 and fields[1] == "00000000" and fields[2] != "00000000":
                    return socket.inet_ntoa(struct.pack("<L", int(fields[2], 16)))
    except (OSError, ValueError):
        pass
    return None


def checksum(packet: bytes) -> int:
    """The ones-complement sum RFC 792 asks for. The kernel fills this in on a
    datagram socket, but a packet carrying its own is right either way."""
    if len(packet) % 2:
        packet += b"\0"
    total = sum(struct.unpack(f"!{len(packet) // 2}H", packet))
    total = (total & 0xFFFF) + (total >> 16)
    return ~((total & 0xFFFF) + (total >> 16)) & 0xFFFF


class Pinger:
    """One host at a time, and what its replies have looked like so far."""

    def __init__(self):
        self.hosts = [h for h in (gateway(), "1.1.1.1", "8.8.8.8") if h]
        self.at = 0
        self.running = True
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM, socket.IPPROTO_ICMP)
        self.sock.setblocking(False)
        self.seq = 0
        # Set by a key to cut short whatever the loop is waiting for, so Start and a
        # change of host take effect now rather than at the end of the second.
        self.woken = asyncio.Event()
        self.reset()

    @property
    def host(self) -> str:
        return self.hosts[self.at] if self.hosts else "no route"

    def reset(self) -> None:
        self.sent = 0
        self.lost = 0
        self.last = None
        self.best = None
        self.worst = None
        self.total = 0.0

    def choose(self, step: int) -> None:
        if self.hosts:
            self.at = (self.at + step) % len(self.hosts)
            self.reset()
        self.woken.set()

    def toggle(self) -> None:
        self.running = not self.running
        self.woken.set()

    async def probe(self) -> None:
        """One echo, and the wait for the answer to it."""
        self.seq = (self.seq + 1) & 0xFFFF
        # The identifier is the kernel's to choose on this kind of socket: it rewrites
        # that field and matches replies back to us, so the sequence is what this app
        # matches on.
        head = struct.pack("!BBHHH", ECHO_REQUEST, 0, 0, 0, self.seq)
        body = b"flipctl-ping"
        packet = struct.pack("!BBHHH", ECHO_REQUEST, 0, checksum(head + body), 0, self.seq)
        began = time.monotonic()
        self.sent += 1
        try:
            self.sock.sendto(packet + body, (self.host, 0))
        except OSError:
            self.lost += 1
            return
        reply = await self.answer(self.seq, began)
        if reply is None:
            self.lost += 1
        else:
            self.keep(reply)

    async def answer(self, seq: int, began: float):
        """The round trip for one sequence in milliseconds, or None if it never came.

        Anything else on the socket is somebody else's late reply and is dropped: the
        deadline is the one the probe started with, not one per packet read.
        """
        loop = asyncio.get_running_loop()
        while True:
            left = PERIOD - (time.monotonic() - began)
            if left <= 0:
                return None
            try:
                packet = await asyncio.wait_for(loop.sock_recv(self.sock, 1024), left)
            except (asyncio.TimeoutError, OSError):
                return None
            if len(packet) < 8:
                continue
            kind, _, _, _, answered = struct.unpack("!BBHHH", packet[:8])
            if kind == ECHO_REPLY and answered == seq:
                return (time.monotonic() - began) * 1000

    def keep(self, ms: float) -> None:
        self.last = ms
        self.total += ms
        self.best = ms if self.best is None else min(self.best, ms)
        self.worst = ms if self.worst is None else max(self.worst, ms)

    async def pause(self, seconds: float) -> None:
        """Wait, unless a key says not to."""
        try:
            await asyncio.wait_for(self.woken.wait(), seconds)
        except asyncio.TimeoutError:
            pass
        self.woken.clear()

    async def run(self, draw) -> None:
        while True:
            if not self.running or not self.hosts:
                await self.pause(3600)
                draw()
                continue
            began = time.monotonic()
            await self.probe()
            draw()
            await self.pause(max(0.0, PERIOD - (time.monotonic() - began)))

    def rows(self):
        got = self.sent - self.lost
        loss = (100 * self.lost // self.sent) if self.sent else 0
        average = (self.total / got) if got else None

        def pair(label, value, dim=False):
            return {"kind": 0, "label": label, "value": value, "percent": 0, "dim": dim}

        def ms(value):
            return "-" if value is None else f"{value:.1f} ms"

        return [
            pair("Host", self.host),
            pair("Last", ms(self.last)),
            pair("Average", ms(average)),
            {"kind": 1, "label": "", "value": "", "percent": 0, "dim": False},
            pair("Best", ms(self.best), dim=True),
            pair("Worst", ms(self.worst), dim=True),
            pair("Sent", str(self.sent), dim=True),
            # Loss is what says whether a link is usable when the times alone look
            # healthy, so it is the one drawn as a bar.
            {"kind": 2, "label": "Loss", "value": f"{loss}%", "percent": loss, "dim": False},
        ]


def main() -> None:
    ui = flipctl.load(PAGE, "ping.slint")
    pinger = Pinger()

    def draw() -> None:
        ui.rows = pinger.rows()
        ui.buttons = ["Close", "", "", "Host", "Stop" if pinger.running else "Start"]

    @flipctl.on_key(ui)
    def _(key, down):
        if not down:
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            slint.quit_event_loop()
        elif key is flipctl.Key.RUN:
            pinger.toggle()
        elif key in (flipctl.Key.EDIT, flipctl.Key.RIGHT):
            pinger.choose(1)
        elif key is flipctl.Key.LEFT:
            pinger.choose(-1)
        draw()

    draw()
    flipctl.run(ui, pinger.run(draw))


if __name__ == "__main__":
    main()
