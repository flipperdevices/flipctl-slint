#!/usr/bin/env python3
# /// flipctl
# name = "Ethernet Tester"
# status = true
# ///
"""A cable tester for both ports, and a loopback check between them.

Each port's RTL8211F carries Realtek's cable tester, RTCT, and nothing documents it:
the datasheet marks every register it uses reserved, and the kernel only drives it on
the RTL8224. The registers are the RTL8224's, found by trying them on this board with
real faults on a 3m cable. What differs is where the results sit and one fault code:

  start       page 0xa42 reg 17 = 0x00f1 (enable, pairs A-D), bit 15 when done
  results     page 0xa43 reg 27/28 into SRAM, status at 0x802a + 4 * pair and
              distance at 0x802c + 4 * pair (the RTL8224 starts at 0x8026)
  status      bit 6 done, 5 OK, 4 short within the pair, 3 open, 1 short to
              another pair (the RTL8224 uses bit 7 for that), 0 busy: the far
              end was transmitting, which a partner renegotiating does. Bits 2,
              7 and the high byte have never been seen set and read as unknown
  distance    about 61 at the port and 74 more per metre, from the 3m cable

A pair that is not wired at all reads open at 0m, shown as missing, and one to a
live partner reads OK with no distance. The test takes 0.1s on a dead line and
about 1.2s with a partner, and drops the link while it runs; the PHY renegotiates
on its own afterwards.

Both runs the test on each port in turn, then waits for the links and sends raw
frames of the local experimental EtherType from each port to the other, so a cable
from eth0 to eth1 is checked end to end: every pair from both sides, then frames
counted in both directions, with no address or route involved. While one port is
tested the other is powered down: left up, it renegotiates the moment its link
drops, and its link pulses read as a busy line or a false open. A single port gets
the same when a probe frame shows the other port is what it is cabled to, and
otherwise the other port is left alone, since it may well be the uplink.

The registers are reached through the MII ioctls, which is the one way in from
userspace and the one thing here that is not safe: the kernel polls the PHY once a
second under a lock this cannot take, and a poll landing between a page select and
the access reads the wrong page. The page is held for a few register accesses at a
time, so the window is small, and the proper fix is the test living in the driver.

Left and Right choose the port, Up and Down scroll, Run tests, Back leaves.
"""

import asyncio
import json
import pathlib
import queue
import socket
import struct
import subprocess
import threading
import time

import flipctl
import slint

# The kernel names the ports end0 and end1; the panel calls them what the LEDs do.
PORTS = ("end0", "end1")
NAMES = {"end0": "eth0", "end1": "eth1"}
CHOICES = (*NAMES.values(), "Both")
PAIRS = "ABCD"

# The distance fit: raw counts at the port itself, and counts per metre of cable.
ZERO = 61
PER_METRE = 74
# An open this close is at the port itself: the pair is not wired at all. A missing
# pair reads 58 to 63, a few centimetres either side of ZERO, and no patch cable is
# this short.
AT_PORT = 0.1

# The status bits the tester has been seen to set, in the order the panel names them.
STATUS_BITS = ((0x01, "busy"), (0x10, "short"), (0x02, "cross"), (0x08, "open"), (0x20, "OK"))
FAULT_BITS = 0x1A
KNOWN_BITS = 0x7B

PAGE = """
import { Shell } from "@app/shell.slint";
import { DetailBody, DetailRow } from "@flipctl/detail.slint";

export component App inherits Shell {
    in property <[DetailRow]> rows;
    in property <int> offset;
    in property <[string]> buttons;
    in property <int> arrow_pressed;
    in property <bool> at_start;
    in property <bool> at_end;
    callback keyed(string, bool);
    key(text, down) => { root.keyed(text, down); }

    DetailBody {
        rows: root.rows;
        offset: root.offset;
        buttons: root.buttons;
        arrow_pressed: root.arrow_pressed;
        at_start: root.at_start;
        at_end: root.at_end;
    }
}
"""

# Runs as root, once per test, and prints one JSON line per result. It is fed to
# python on stdin rather than as an argument, which sudo would log whole on every
# run. A TERM or a closed pipe does not stop it mid-access: a test left with the PHY
# on another page would leave the kernel reading the wrong registers, so it finishes
# the step it is on and then stops.
HELPER = r"""
import errno, fcntl, glob, json, mmap, os, signal, socket, struct, sys, threading, time

SIOCGMIIPHY, SIOCGMIIREG, SIOCSMIIREG = 0x8947, 0x8948, 0x8949
ETHERTYPE = 0x88B5
PACKET_OUTGOING = 4
PACKET_STATISTICS = 6
PACKET_VERSION, PACKET_TX_RING, TPACKET_V2 = 10, 13, 1
TP_STATUS_AVAILABLE, TP_STATUS_SEND_REQUEST, TP_STATUS_WRONG_FORMAT = 0, 1, 4
# The ring: one 2K slot per frame, 32 to a 64K block, and the frame after the V2
# header, which is 32 bytes once aligned.
SLOT, BLOCK, SLOT_DATA = 2048, 1 << 16, 32
SOL_PACKET = 263
SO_RCVBUFFORCE = 33
SO_TIMESTAMPNS = 35
FRAMES = 5000
BUSY_TRIES = 3
QUIET_S = 0.5
MAGIC = b"FLIPETH"
WARM_MAGIC = b"FLIPWRM"
WARM_FRAMES = 1000
PEERS = ("end0", "end1")
PATTERN = bytes(range(256)) * 6

stopping = False


def stop(*_):
    global stopping
    stopping = True


for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
    signal.signal(sig, stop)
signal.signal(signal.SIGPIPE, signal.SIG_IGN)


def say(**fields):
    try:
        print(json.dumps(fields), flush=True)
    except OSError:
        pass


class Phy:
    def __init__(self, port):
        self.name = port.encode()
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.addr = 0
        self.addr = self.mii(SIOCGMIIPHY, 0)[1]

    def mii(self, request, reg, value=0):
        ifr = struct.pack("16sHHHH8x", self.name, self.addr, reg, value, 0)
        return struct.unpack("16sHHHH8x", fcntl.ioctl(self.sock, request, ifr))

    def read(self, reg):
        return self.mii(SIOCGMIIREG, reg)[4]

    def write(self, reg, value):
        self.mii(SIOCSMIIREG, reg, value)

    def paged(self, page, access):
        held = self.read(31)
        self.write(31, page)
        try:
            return access()
        finally:
            self.write(31, held)

    # Power the PHY down, so a port under test on the far end hears nothing.
    def quiet(self):
        self.bmcr = self.paged(0, lambda: self.read(0))
        self.paged(0, lambda: self.write(0, self.bmcr | 0x0800))
        time.sleep(QUIET_S)

    # Power it back up as it was, renegotiating if it negotiates at all.
    def wake(self):
        restart = 0x0200 if self.bmcr & 0x1000 else 0
        self.paged(0, lambda: self.write(0, (self.bmcr & ~0x0800) | restart))

    def sram(self, at):
        return self.paged(0xA43, lambda: (self.write(27, at), self.read(28))[1])

    def cable_test(self):
        if (self.read(2), self.read(3)) != (0x001C, 0xC916):
            raise OSError("not an RTL8211F")
        for _ in range(BUSY_TRIES):
            self.paged(0xA42, lambda: self.write(17, 0x00F1))
            began = time.monotonic()
            while not self.paged(0xA42, lambda: self.read(17)) & 0x8000:
                if time.monotonic() - began > 5:
                    raise OSError("test never finished")
                time.sleep(0.05)
            took = time.monotonic() - began
            pairs = [[self.sram(0x802A + 4 * p), self.sram(0x802C + 4 * p)] for p in range(4)]
            if not any(status & 0x01 for status, _ in pairs) or stopping:
                break
            time.sleep(0.2)
        return pairs, took


def sysfs(port, name):
    with open(f"/sys/class/net/{port}/{name}") as f:
        return f.read().strip()


def carrier(port):
    try:
        return sysfs(port, "carrier") == "1"
    except OSError:
        return False


# The other port, when this one is cabled straight to it: a few small frames sent
# here and listened for there. None when either has no link or nothing arrives.
def peer(port):
    other = next((p for p in PEERS if p != port), None)
    if other is None or not (carrier(port) and carrier(other)):
        return None
    src = bytes.fromhex(sysfs(port, "address").replace(":", ""))
    dst = bytes.fromhex(sysfs(other, "address").replace(":", ""))
    inbox = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETHERTYPE))
    inbox.bind((other, ETHERTYPE))
    inbox.settimeout(0.1)
    outbox = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETHERTYPE))
    outbox.bind((port, ETHERTYPE))
    frame = (dst + src + struct.pack("!H", ETHERTYPE) + MAGIC).ljust(60, b"\0")
    try:
        for _ in range(3):
            outbox.send(frame)
        began = time.monotonic()
        while time.monotonic() - began < 0.3:
            try:
                got, where = inbox.recvfrom(128)
            except socket.timeout:
                continue
            if where[2] != PACKET_OUTGOING and got[6:12] == src:
                return other
    finally:
        inbox.close()
        outbox.close()
    return None


# The fastest cluster's cores, by cpufreq. Empty when cpufreq says nothing. The
# sender and the listener are pinned there, which touches this process and nothing
# else: on a little core the Flipper rather than the cable would set the rate.
def big_cores():
    best, cores = 0, []
    for policy in glob.glob("/sys/devices/system/cpu/cpufreq/policy*"):
        try:
            with open(policy + "/cpuinfo_max_freq") as f:
                top = int(f.read())
            with open(policy + "/affected_cpus") as f:
                cpus = [int(c) for c in f.read().split()]
        except (OSError, ValueError):
            continue
        if top > best:
            best, cores = top, cpus
    return cores


# Every frame is written into the ring first and one send hands the lot to the
# kernel, which is what lets this reach line rate: a send per frame from Python tops
# out near 180Mbps. The frames go through the interface's queue rather than straight
# to the driver, so while the driver's own ring is full the queue keeps the wire fed;
# a full queue gives frames back still marked for sending, and the send is repeated
# until none are. The kernel walks the ring from where its last send stopped and
# stops at the first slot not marked, so frames are written from `start` on, wrapping,
# never from slot 0. Returns how many went and where the next batch starts.
def send_ring(outbox, ring, frames, start, total):
    used = [(start + i) % total for i in range(len(frames))]
    for slot, frame in zip(used, frames):
        at = slot * SLOT
        ring[at + SLOT_DATA : at + SLOT_DATA + len(frame)] = frame
        struct.pack_into("II", ring, at + 4, len(frame), len(frame))
        struct.pack_into("I", ring, at, TP_STATUS_SEND_REQUEST)
    began = time.monotonic()
    while True:
        try:
            outbox.send(b"")
        except OSError as e:
            if e.errno not in (errno.ENOBUFS, errno.EAGAIN):
                raise
            time.sleep(0.0005)
        states = [struct.unpack_from("I", ring, slot * SLOT)[0] for slot in used]
        if TP_STATUS_WRONG_FORMAT in states:
            raise OSError("the kernel refused a frame")
        if all(state == TP_STATUS_AVAILABLE for state in states):
            break
        if time.monotonic() - began > 5:
            break
    return states.count(TP_STATUS_AVAILABLE), (start + len(frames)) % total


def traffic(tx, rx):
    cores = big_cores()
    send_core, listen_core = cores[:2] if len(cores) >= 2 else (None, None)
    src = bytes.fromhex(sysfs(tx, "address").replace(":", ""))
    dst = bytes.fromhex(sysfs(rx, "address").replace(":", ""))
    crc = int(sysfs(rx, "statistics/rx_crc_errors"))
    head = dst + src + struct.pack("!H", ETHERTYPE) + MAGIC
    body = PATTERN[: 1500 - len(MAGIC) - 4]
    warm = dst + src + struct.pack("!H", ETHERTYPE) + WARM_MAGIC + bytes(4) + body

    inbox = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETHERTYPE))
    inbox.bind((rx, ETHERTYPE))
    inbox.setsockopt(socket.SOL_SOCKET, SO_RCVBUFFORCE, 32 << 20)
    inbox.setsockopt(socket.SOL_SOCKET, SO_TIMESTAMPNS, 1)
    inbox.settimeout(0.2)
    outbox = socket.socket(socket.AF_PACKET, socket.SOCK_RAW, socket.htons(ETHERTYPE))
    outbox.setsockopt(SOL_PACKET, PACKET_VERSION, TPACKET_V2)
    blocks = -(-FRAMES * SLOT // BLOCK)
    total = blocks * BLOCK // SLOT
    outbox.setsockopt(SOL_PACKET, PACKET_TX_RING,
                      struct.pack("IIII", BLOCK, blocks, SLOT, total))
    outbox.bind((tx, ETHERTYPE))
    ring = mmap.mmap(outbox.fileno(), blocks * BLOCK)

    seen, bad, arrived = set(), 0, []
    done = threading.Event()

    def listen():
        nonlocal bad
        if listen_core is not None:
            os.sched_setaffinity(0, {listen_core})
        while not done.is_set():
            try:
                frame, notes, _, where = inbox.recvmsg(2048, 64)
            except socket.timeout:
                continue
            if where[2] == PACKET_OUTGOING or frame[6:12] != src:
                continue
            if not frame[14:].startswith(MAGIC):
                continue
            at = len(head)
            seq = struct.unpack("!I", frame[at : at + 4])[0]
            if frame[at + 4 :] != body or seq >= FRAMES:
                bad += 1
            else:
                seen.add(seq)
                for level, kind, data in notes:
                    if (level, kind) == (socket.SOL_SOCKET, SO_TIMESTAMPNS):
                        sec, nsec = struct.unpack("qq", data[:16])
                        arrived.append(sec + nsec / 1e9)

    listener = threading.Thread(target=listen)
    listener.start()
    held = os.sched_getaffinity(0)
    try:
        if send_core is not None:
            os.sched_setaffinity(0, {send_core})
        # Uncounted and untimed, since the listener skips anything without MAGIC:
        # it is there so the clocks have ramped before the frames that are measured.
        _, start = send_ring(outbox, ring, [warm] * WARM_FRAMES, 0, total)
        time.sleep(0.05)
        inbox.getsockopt(SOL_PACKET, PACKET_STATISTICS, 8)
        frames = [head + struct.pack("!I", seq) + body for seq in range(FRAMES)]
        sent, _ = send_ring(outbox, ring, frames, start, total)
    finally:
        os.sched_setaffinity(0, held)
    time.sleep(0.5)
    done.set()
    listener.join()
    missed = struct.unpack("II", inbox.getsockopt(SOL_PACKET, PACKET_STATISTICS, 8))[1]
    crc = int(sysfs(rx, "statistics/rx_crc_errors")) - crc
    # The rate is taken where the frames land, from the kernel's own arrival stamps,
    # since the sender only knows when the driver took the frames, not when they left.
    span = max(arrived) - min(arrived) if len(arrived) > 1 else 0
    mbit = round((len(arrived) - 1) * (len(head) + 4 + len(body)) * 8 / span / 1e6) if span else 0
    say(traffic=tx, to=rx, sent=sent, received=len(seen), corrupt=bad, crc=crc,
        missed=missed, mbit=mbit)

def main():
    ports = [a for a in sys.argv[1:] if not a.startswith("-")]
    for port in ports:
        if stopping:
            return
        say(step="cable", port=port)
        near = [p for p in ports if p != port] or [peer(port)]
        others = [Phy(p) for p in near if p]
        try:
            for other in others:
                other.quiet()
            pairs, took = Phy(port).cable_test()
        except OSError as e:
            say(port=port, error=str(e))
            continue
        finally:
            for other in others:
                if hasattr(other, "bmcr"):
                    other.wake()
        say(port=port, pairs=pairs, took=round(took, 2))
    if "--traffic" not in sys.argv or stopping:
        return
    say(step="link")
    began = time.monotonic()
    while not all(carrier(p) for p in ports):
        if stopping or time.monotonic() - began > 10:
            say(error="no link")
            return
        time.sleep(0.1)
    for tx, rx in (ports, ports[::-1]):
        if stopping:
            return
        say(step="traffic", port=tx)
        try:
            traffic(tx, rx)
        except OSError as e:
            say(port=tx, error=str(e))
            return


try:
    main()
except Exception as e:
    say(error=f"{type(e).__name__}: {e}")
"""


def link(port: str) -> str:
    base = pathlib.Path("/sys/class/net") / port
    try:
        if (base / "carrier").read_text().strip() != "1":
            return "down"
        speed = (base / "speed").read_text().strip()
        duplex = (base / "duplex").read_text().strip()
    except OSError:
        return "down"
    return f"{speed} {duplex}"


def metres(raw: int) -> float:
    return max(raw - ZERO, 0) / PER_METRE


def is_open(status: int) -> bool:
    return status & 0x7A == 0x48


def verdict(status: int, raw: int) -> str:
    """One pair's result, as the panel says it.

    Every bit set is named, and one the table above cannot name adds "unknown", so
    a combination nobody has seen still says that it is one.
    """
    words = [] if status & 0x40 else ["unfinished"]
    words += [name for bit, name in STATUS_BITS if status & bit]
    faults = status & FAULT_BITS
    if faults == 0x08 and not status & 0x01 and metres(raw) < AT_PORT:
        words = ["missing"]
    elif faults:
        words.append(f"{metres(raw):.1f}m")
    if status & ~KNOWN_BITS & 0xFFFF:
        words.append("unknown")
    return " ".join(words) or "none"


def busy(status: int) -> bool:
    return bool(status & 0x01)


def capacity(pairs: list[list[int]]) -> int | None:
    """The fastest link one port's pairs can carry: 1000, 100, 0, or None for no cable.

    Gigabit needs all four pairs and 100 or 10 needs A and B. A live partner makes
    every sound pair read OK, so then anything else is a fault. With nothing on the
    far end a sound pair reads open at the cable's length, so a pair counts when it is
    open within half a metre of the longest one, and a short or an open nearer the
    port does not. Every pair open at the port is no cable at all.
    """
    if any(status & 0x20 for status, _ in pairs):
        good = [bool(status & 0x20) for status, _ in pairs]
    else:
        far = max((metres(raw) for status, raw in pairs if is_open(status)), default=0)
        if far < AT_PORT and all(is_open(status) for status, _ in pairs):
            return None
        good = [is_open(status) and far >= AT_PORT and metres(raw) >= far - 0.5
                for status, raw in pairs]
    if all(good):
        return 1000
    return 100 if good[0] and good[1] else 0


class Tester:
    """What was chosen, what the last run found, and the helper doing the next one."""

    def __init__(self) -> None:
        self.choice = 0
        self.results: dict[str, list[str]] = {}
        self.capacities: dict[str, int | None] = {}
        self.line_busy = False
        self.traffic: dict[str, dict] = {}
        self.state = "-"
        self.since = 0.0
        self.heard = False
        self.failed = False
        self.helper: subprocess.Popen | None = None
        self.lines: queue.Queue = queue.Queue()

    @property
    def ports(self) -> tuple[str, ...]:
        return PORTS if CHOICES[self.choice] == "Both" else (PORTS[self.choice],)

    @property
    def running(self) -> bool:
        return self.helper is not None

    def choose(self, step: int) -> None:
        if self.running:
            return
        self.choice = min(max(self.choice + step, 0), len(CHOICES) - 1)

    def run(self) -> None:
        if self.running:
            return
        self.results = {p: ["-"] * 4 for p in self.ports}
        self.capacities = {}
        self.line_busy = False
        self.traffic = {}
        self.heard = self.failed = False
        self.state = "starting"
        args = ["sudo", "-n", "python3", "-", *self.ports]
        if len(self.ports) == 2:
            args.append("--traffic")
        try:
            self.helper = subprocess.Popen(
                args, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                stderr=subprocess.DEVNULL, text=True,
            )
        except OSError as e:
            self.state = f"failed: {e}"
            return
        try:
            self.helper.stdin.write(HELPER)
            self.helper.stdin.close()
        except OSError:
            pass
        threading.Thread(target=self.read, args=(self.helper,), daemon=True).start()

    def read(self, helper: subprocess.Popen) -> None:
        for line in helper.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def take(self) -> bool:
        """Apply whatever the helper has said since the last look; True if anything."""
        changed = False
        while True:
            try:
                line = self.lines.get_nowait()
            except queue.Empty:
                return changed
            changed = True
            if line is None:
                self.finish()
                continue
            try:
                said = json.loads(line)
            except ValueError:
                continue
            self.hear(said)

    def hear(self, said: dict) -> None:
        self.heard = True
        if "error" in said:
            self.failed = True
            where = NAMES.get(said.get("port"))
            self.state = f"{where}: {said['error']}" if where else said["error"]
        elif "pairs" in said:
            self.results[said["port"]] = [verdict(s, r) for s, r in said["pairs"]]
            self.capacities[said["port"]] = capacity(said["pairs"])
            self.line_busy |= any(busy(status) for status, _ in said["pairs"])
        elif "traffic" in said:
            self.traffic[said["traffic"]] = said
        elif said.get("step") == "cable":
            self.state = f"testing {NAMES[said['port']]}"
        elif said.get("step") == "link":
            self.state = "waiting for link"
            self.since = time.monotonic()
        elif said.get("step") == "traffic":
            self.state = f"frames from {NAMES[said['port']]}"

    def finish(self) -> None:
        helper, self.helper = self.helper, None
        code = helper.wait() if helper else 0
        if self.failed:
            return
        if code:
            self.state = f"helper exit {code}" if self.heard else "sudo refused"
            return
        known = [c for c in self.capacities.values() if c is not None]
        if not self.capacities:
            self.state = "done"
        elif self.line_busy:
            self.state = "done, line busy"
        elif not known:
            self.state = "done, no cable"
        elif min(known):
            self.state = f"done, max {min(known)}Mbps"
        else:
            self.state = "done, cable unusable"

    def stop(self) -> None:
        """Let a running test finish its step rather than leave the PHY half way."""
        if self.helper is not None:
            try:
                self.helper.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.helper.kill()

    def rows(self) -> list[dict]:
        def row(label, value, dim=False, kind=0):
            return {"kind": kind, "label": label, "value": value, "percent": 0, "dim": dim}

        divider = row("", "", kind=1)
        both = len(self.ports) == 2
        rows = [row("Port", CHOICES[self.choice], kind=4)]
        for port in self.ports:
            rows.append(row(f"Link {NAMES[port]}" if both else "Link", link(port)))
        state = self.state
        if self.running and state == "waiting for link":
            state += f" {int(time.monotonic() - self.since)}s"
        rows.append(row("Test", state))
        rows.append(divider)
        for i, pair in enumerate(PAIRS):
            found = [self.results.get(p, ["-"] * 4)[i] for p in self.ports]
            rows.append(row(f"Pair {pair}", " / ".join(found)))
        if both:
            rows.append(divider)
            for tx, rx in (PORTS, PORTS[::-1]):
                got = self.traffic.get(tx)
                if got is None:
                    rows.append(row(f"{NAMES[tx]} > {NAMES[rx]}", "-"))
                    continue
                # Frames the socket dropped because this app read too slowly arrived
                # fine, so they are not the cable's loss.
                missed = got.get("missed", 0)
                lost = max(got["sent"] - got["received"] - got["corrupt"] - missed, 0)
                rows.append(row(f"{NAMES[tx]} > {NAMES[rx]}",
                                f"received {got['received']}/{got['sent']}"))
                rows.append(row("Lost/corrupt", f"{lost}/{got['corrupt']}", dim=True))
                if missed:
                    rows.append(row("Missed by app", str(missed), dim=True))
                rows.append(row("CRC errors", str(got["crc"]), dim=True))
                rows.append(row("Receive rate", f"{got['mbit']}Mbps", dim=True))
        return rows


def main() -> None:
    ui = flipctl.load(PAGE, "ethernet-tester.slint")
    tester = Tester()
    visible = int(flipctl.theme("detail_visible_rows_bare") or flipctl.theme("detail_visible_rows", 8))
    state = {"offset": 0, "arrow": 0}

    def draw() -> None:
        rows = tester.rows()
        state["offset"] = min(max(state["offset"], 0), max(0, len(rows) - visible))
        ui.rows = rows
        ui.offset = state["offset"]
        ui.arrow_pressed = state["arrow"]
        ui.at_start = tester.choice == 0
        ui.at_end = tester.choice == len(CHOICES) - 1
        run = "" if tester.running else "Run"
        ui.buttons = ["Close", "", "", "", run]

    async def leave() -> None:
        tester.stop()
        slint.quit_event_loop()

    @flipctl.on_key(ui)
    def _(key, down):
        if key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            state["arrow"] = 0 if not down else 1 if key is flipctl.Key.LEFT else 2
        if not down:
            draw()
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            asyncio.get_running_loop().create_task(leave())
        elif key is flipctl.Key.RUN:
            tester.run()
        elif key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            tester.choose(1 if key is flipctl.Key.RIGHT else -1)
        elif key is flipctl.Key.DOWN:
            state["offset"] += 1
        elif key is flipctl.Key.UP:
            state["offset"] -= 1
        draw()

    async def tick() -> None:
        """Results as they arrive, and the links once a second."""
        counted = 0
        while True:
            await asyncio.sleep(0.1)
            counted += 1
            if tester.take() or counted % 10 == 0:
                draw()

    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
