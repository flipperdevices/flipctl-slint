#!/usr/bin/env python3
# /// flipctl
# name = "USB Analyzer"
# status = true
# ///
"""Everything the kernel knows about what is in each USB port.

The sockets are found the way the kernel describes them rather than from a table of
this board: a hub port marked `hotplug` (or `unknown`, which is what a board with no
firmware description says) is a socket, a `hardwired` one leads to something inside
the case, and a `not used` one is skipped. A socket has a port on each bus, the USB 2
and the USB 3 one, joined by `peer`, so the two count as one socket and whichever has
the device shows it. Anything plugged in behind a hub in a socket is listed after it
as that socket's `.port`. A socket is named for the connector the device tree wires
it to, so it reads as the case does: USB-A, and a USB-C numbered after the Type-C
port the PD controller drives, which is USB-C1 and comes from the typec class. Any
bus that port's controller grows in host mode is counted as its own.

Most of it is sysfs, readable by anyone. Two things are not there:

  capabilities  what a device supports rather than what it negotiated lives in its
                BOS descriptor, which the kernel reads and does not keep. It is asked
                for once per device through usbfs, a standard GET_DESCRIPTOR, which
                is what `lsusb -v` does; the node is the plugdev group's to open.
  errors        enumeration failures (`device descriptor read/64, error -71`, a port
                that cannot be enabled) are only in the kernel log. /dev/kmsg is read
                from boot and followed, and each message is given to the entry whose
                device, interface or port it names, so a socket that failed to start
                a device still says why when there is no device to show.

A device that supports SuperSpeed and is running at 480Mbps in a socket that offers
5Gbps is called out: that is a USB 2 cable or plug, and it looks like a working
device otherwise.

Hotplug is heard from the kernel's uevent socket, so a device shows the moment it is
plugged in and is selected; the rest is read again once a second for the counters.

Left and Right choose the port, Up and Down scroll, the right soft key jumps to the
next section, Back leaves.
"""

import asyncio
import ctypes
import fcntl
import glob
import os
import pathlib
import re
import socket
import struct
import threading
import uuid
from collections import deque

import flipctl
import slint

DEVICES = pathlib.Path("/sys/bus/usb/devices")
TYPEC = pathlib.Path("/sys/class/typec")
POWER_SUPPLY = pathlib.Path("/sys/class/power_supply")
UDC = pathlib.Path("/sys/class/udc")
GADGETS = pathlib.Path("/sys/kernel/config/usb_gadget")
REGULATORS = pathlib.Path("/sys/class/regulator")
DEVICE_TREE = pathlib.Path("/sys/firmware/devicetree/base")
USB_IDS = ("/usr/share/misc/usb.ids", "/usr/share/hwdata/usb.ids", "/var/lib/usbutils/usb.ids")

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

# Advance widths of Busy9px, the face detail rows are drawn in, ASCII 32..126. From
# crates/flipper-ui/src/font/row.rs. `flipctl.text_width` measures the title face,
# which is narrower, so a value fitted with it would run into its label.
ROW_ADVANCES = (
    6, 3, 4, 9, 8, 10, 9, 2, 4, 4, 6, 6, 3, 4, 2, 6, 7, 5, 7, 7, 7, 7, 7, 7, 7, 7, 2, 3,
    5, 6, 5, 6, 10, 8, 7, 7, 8, 7, 7, 8, 8, 4, 7, 7, 7, 10, 8, 8, 7, 8, 7, 7, 8, 8, 8, 10,
    8, 8, 7, 3, 6, 3, 6, 6, 4, 6, 6, 6, 6, 6, 5, 6, 6, 4, 4, 6, 4, 8, 6, 6, 6, 6, 5, 6, 5,
    6, 6, 10, 6, 6, 6, 4, 2, 4, 8,
)
WIDTH = int(flipctl.theme("panel_w", 256) - 2 * flipctl.theme("margin_h", 8))
GAP = 8
VISIBLE = int(flipctl.theme("detail_visible_rows_bare") or flipctl.theme("detail_visible_rows", 8))

# The short name of a class, which the panel has room for; usb.ids adds the
# subclass or protocol after it.
CLASSES = {
    0x01: "Audio", 0x02: "Comms", 0x03: "HID", 0x05: "Physical", 0x06: "Image",
    0x07: "Printer", 0x08: "Storage", 0x09: "Hub", 0x0A: "CDC data", 0x0B: "Smart card",
    0x0D: "Security", 0x0E: "Video", 0x0F: "Health", 0x10: "AV", 0x11: "Billboard",
    0x12: "Type-C bridge", 0xDC: "Diagnostic", 0xE0: "Wireless", 0xEF: "Misc",
    0xFE: "App specific", 0xFF: "Vendor",
}
SPEEDS = {1.5: "Low", 12: "Full", 480: "High", 5000: "Super", 10000: "Super+", 20000: "Super+ x2"}
SVIDS = {"ff01": "DisplayPort", "8087": "Thunderbolt"}
OPERATION = {"default": "default USB", "1.5A": "Type-C 1.5A", "3.0A": "Type-C 3A",
             "usb_power_delivery": "USB PD"}

USBDEVFS_CONTROL = 0xC0185500
BOS_TYPE = 0x0F
BOS_CAPS = {
    2: "USB2 ext", 3: "SS", 4: "Container ID", 6: "PD", 7: "Battery", 8: "PD sink",
    9: "PD source", 10: "SS+", 11: "PTM", 13: "Billboard", 14: "Auth", 15: "Billboard+",
    16: "Config summary",
}
PLATFORMS = {
    "3408b638-09a9-47a0-8bfd-a0768815b665": "WebUSB",
    "d8dd60df-4589-4cc7-9cd2-659d9e648a9f": "MS OS 2.0",
}

NETLINK_KOBJECT_UEVENT = 15
UEVENT_SUBSYSTEMS = (b"SUBSYSTEM=usb", b"SUBSYSTEM=typec", b"SUBSYSTEM=udc",
                     b"SUBSYSTEM=power_supply", b"SUBSYSTEM=usb_power_delivery")

# A kernel message the way dev_printk writes it: driver, device, text.
MESSAGE = re.compile(r"^([\w-]+) ([\w.:-]+): (.*)$")
DEVICE_NAME = re.compile(r"^(\d+-[\d.]+|usb\d+)$")
PORT_NAME = re.compile(r"^(\d+-[\d.]+|usb\d+)-port(\d+)$")
INTERFACE_NAME = re.compile(r"^(\d+-[\d.]+):\d+\.\d+$")
SCSI_NAME = re.compile(r"^\d+:\d+:\d+:\d+$")
ERROR = re.compile(r"error|fail|unable|cannot|can't|over-?current|not accepting|disabled by"
                   r"|reset|timed? ?out|stall|babble|insufficient|exceed", re.I)
START_FAILED = re.compile(r"descriptor read|not accepting address|unable to enumerate"
                          r"|cannot enable|can't set config|insufficient", re.I)
PLUGGED = re.compile(r"new .*USB device number")
MESSAGES_SHOWN = 6
START_FAILED_S = 120

# Node kinds a driver can give an interface, by the directory sysfs files them under.
NODES = {"block": "Disk", "net": "Network", "tty": "Serial", "sound": "Sound",
         "hidraw": "HID raw", "video4linux": "Video", "usbmisc": "Misc", "bluetooth": "BT"}

# Sections the right soft key jumps between: the heading, and the word on the key.
SECTIONS = {"Capabilities": "Caps", "Power": "Power", "Gadget": "Gadget",
            "Alt modes": "Modes", "Interfaces": "Ifaces", "Ports": "Ports",
            "Errors": "Errors"}


def read(path) -> str:
    try:
        with open(path, errors="replace") as f:
            return f.read().strip()
    except OSError:
        return ""


def clean(text: str) -> str:
    """Printable ASCII, since anything else draws as `?` and a device's own strings
    are whatever its firmware wrote."""
    return "".join(c if 32 <= ord(c) < 127 else "?" for c in text).strip()


def width(text: str) -> int:
    return max(0, sum(ROW_ADVANCES[ord(c) - 32] if 32 <= ord(c) < 127 else 6 for c in text) - 1)


def fit(text: str, room: int) -> str:
    if width(text) <= room:
        return text
    while text and width(text.rstrip() + "..") > room:
        text = text[:-1]
    return text.rstrip() + ".."


def wrap(text: str) -> list[str]:
    """Lines that fit a full-width row, a word too long for one split across two."""
    lines, line = [], ""
    for word in text.split():
        while width(word) > WIDTH:
            cut = len(word)
            while cut > 1 and width(word[:cut]) > WIDTH:
                cut -= 1
            if line:
                lines.append(line)
                line = ""
            lines.append(word[:cut])
            word = word[cut:]
        candidate = f"{line} {word}" if line else word
        if width(candidate) <= WIDTH:
            line = candidate
        else:
            lines.append(line)
            line = word
    if line:
        lines.append(line)
    return lines or [""]


def number(text: str, default: float = 0) -> float:
    found = re.match(r"\s*(-?[\d.]+)", text)
    return float(found.group(1)) if found else default


def chosen(text: str) -> str:
    """The bracketed word of a sysfs choice list, `host [device]`, or the text."""
    found = re.search(r"\[([^]]+)]", text)
    return found.group(1) if found else text


def rate(mbps: float) -> str:
    if mbps >= 1000:
        return f"{mbps / 1000:g}Gbps"
    return f"{mbps:g}Mbps"


def span(seconds: float) -> str:
    seconds = int(seconds)
    if seconds < 60:
        return f"{seconds}s"
    if seconds < 3600:
        return f"{seconds // 60}m {seconds % 60}s"
    return f"{seconds // 3600}h {seconds // 60 % 60}m"


def uptime() -> float:
    return number(read("/proc/uptime"))


def row(label, value="", kind=0, dim=False) -> dict:
    """One detail row, the value cut to the room its label leaves."""
    label = clean(str(label))
    value = clean(str(value))
    if kind == 0:
        value = fit(value, WIDTH - width(label) - GAP)
    elif kind == 3:
        label = fit(label, WIDTH)
    return {"kind": kind, "label": label, "value": value, "percent": 0, "dim": dim}


def divider() -> dict:
    return row("", kind=1)


class Ids:
    """Names from usb.ids, looked up in the file's text rather than parsed whole."""

    def __init__(self) -> None:
        self.text = None

    def load(self) -> str:
        if self.text is None:
            self.text = next((read(p) for p in USB_IDS if os.path.exists(p)), "")
            self.text = "\n" + self.text
        return self.text

    def _line(self, start: int, prefix: str, stop: str):
        """The name on the first line from `start` that begins with `prefix`, before
        the first line that begins with neither the prefix's indent nor more."""
        text = self.load()
        at = start
        while True:
            at = text.find("\n", at)
            if at < 0:
                return None, -1
            at += 1
            if text.startswith(prefix, at):
                end = text.find("\n", at)
                return text[at + len(prefix):end].strip(), at
            if not text.startswith(stop, at) and not text.startswith("#", at):
                return None, -1

    def vendor(self, vid: str):
        text = self.load()
        at = text.find(f"\n{vid}  ")
        if at < 0:
            return None, -1
        end = text.find("\n", at + 1)
        return text[at + len(vid) + 3:end].strip(), at

    def product(self, vid: str, pid: str):
        _, at = self.vendor(vid)
        return self._line(at + 1, f"\t{pid}  ", "\t")[0] if at >= 0 else None

    def klass(self, code: int, sub: int, proto: int) -> str:
        short = CLASSES.get(code, f"class {code:02x}")
        text = self.load()
        at = text.find(f"\nC {code:02x}  ")
        if at < 0 or code in (0x00, 0xFF):
            return short
        sub_name, sub_at = self._line(at + 1, f"\t{sub:02x}  ", "\t")
        proto_name = None
        if sub_at >= 0 and proto:
            proto_name, _ = self._line(sub_at, f"\t\t{proto:02x}  ", "\t\t")
        detail = proto_name or sub_name
        if not detail or detail.lower() in ("none", "unused", "no subclass"):
            return short
        return f"{short} {detail}"


IDS = Ids()


def bos(bus: int, dev: int) -> dict | None:
    """The device's BOS descriptor, parsed, or None when it has none to give."""
    try:
        fd = os.open(f"/dev/bus/usb/{bus:03d}/{dev:03d}", os.O_RDWR)
    except OSError:
        return None
    try:
        def get(length: int) -> bytes:
            data = ctypes.create_string_buffer(length)
            request = bytearray(struct.pack("=BBHHHI4xQ", 0x80, 6, BOS_TYPE << 8, 0, length,
                                            1000, ctypes.addressof(data)))
            got = fcntl.ioctl(fd, USBDEVFS_CONTROL, request)
            return data.raw[:got]

        head = get(5)
        if len(head) < 5 or head[1] != BOS_TYPE:
            return None
        data = get(struct.unpack_from("<H", head, 2)[0])
    except OSError:
        return None
    finally:
        os.close(fd)

    found = {"caps": [], "max": None}
    at = data[0] if data else 5
    while at + 3 <= len(data):
        length, kind = data[at], data[at + 2]
        if length < 3:
            break
        body = data[at:at + length]
        if kind == 5 and len(body) >= 20:
            found["caps"].append(PLATFORMS.get(str(uuid.UUID(bytes_le=bytes(body[4:20]))),
                                               "Platform"))
        else:
            found["caps"].append(BOS_CAPS.get(kind, f"cap {kind:02x}"))
        if kind == 2 and len(body) >= 7:
            attrs = struct.unpack_from("<I", body, 3)[0]
            found["lpm"] = bool(attrs & 0x02)
            found["besl"] = bool(attrs & 0x04)
        elif kind == 3 and len(body) >= 10:
            speeds = struct.unpack_from("<H", body, 4)[0]
            found["ltm"] = bool(body[3] & 0x02)
            found["u1"] = body[7]
            found["u2"] = struct.unpack_from("<H", body, 8)[0]
            best = next((s for bit, s in ((8, 5000), (4, 480), (2, 12), (1, 1.5)) if speeds & bit), None)
            found["max"] = max(found["max"] or 0, best or 0) or None
        elif kind == 10 and len(body) >= 12:
            count = (struct.unpack_from("<I", body, 4)[0] & 0x1F) + 1
            for i in range(count):
                if 12 + 4 * i + 4 > len(body):
                    break
                attr = struct.unpack_from("<I", body, 12 + 4 * i)[0]
                scale = (1e-6, 1e-3, 1, 1000)[(attr >> 4) & 3]
                found["max"] = max(found["max"] or 0, (attr >> 16) * scale)
        at += length
    return found


class Device:
    """One device's sysfs attributes, read once per scan."""

    def __init__(self, name: str) -> None:
        self.name = name
        self.path = DEVICES / name
        self.attrs = {}
        for key in ("idVendor", "idProduct", "manufacturer", "product", "serial", "speed",
                    "version", "bcdDevice", "bDeviceClass", "bDeviceSubClass",
                    "bDeviceProtocol", "bMaxPower", "bmAttributes", "bNumConfigurations",
                    "bConfigurationValue", "configuration", "bMaxPacketSize0", "busnum",
                    "devnum", "maxchild", "quirks", "authorized", "ltm_capable",
                    "rx_lanes", "tx_lanes"):
            self.attrs[key] = read(self.path / key)
        for key in ("runtime_status", "control", "autosuspend_delay_ms", "wakeup",
                    "active_duration", "connected_duration"):
            self.attrs[key] = read(self.path / "power" / key)

    def __getitem__(self, key: str) -> str:
        return self.attrs.get(key, "")

    def hex(self, key: str) -> int:
        try:
            return int(self[key], 16)
        except ValueError:
            return 0

    @property
    def speed(self) -> float:
        return number(self["speed"])

    @property
    def version(self) -> float:
        return number(self["version"])

    @property
    def is_hub(self) -> bool:
        return self["bDeviceClass"] == "09"

    def interfaces(self) -> list[pathlib.Path]:
        def order(path):
            return [int(n) for n in re.findall(r"\d+", path.name.split(":")[1])]
        return sorted(self.path.glob(f"{self.name}:*"), key=order)


class Entry:
    """One choice on the panel: a socket, the Type-C port, an internal device, or a
    device behind a hub in a socket."""

    def __init__(self, name: str, kind: str) -> None:
        self.name = name
        self.kind = kind
        self.ports: list[pathlib.Path] = []
        self.device: Device | None = None
        self.children: list[Entry] = []
        self.typec: pathlib.Path | None = None
        # The port on the parent's hub, for a device behind a hub in a socket.
        self.number = 0
        # The device tree's connector node, for a socket it describes.
        self.connector: pathlib.Path | None = None

    def take(self, device: Device) -> None:
        """A USB 3 hub is two devices on one port, its USB 2 and its USB 3 half: the
        faster one is the one shown."""
        if self.device is None or device.speed > self.device.speed:
            self.device = device


def port_number(port: pathlib.Path) -> int:
    return int(port.name.rsplit("port", 1)[1])


def ports_of(hub: str) -> list[pathlib.Path]:
    found = glob.glob(f"{DEVICES}/{hub}/{hub}:*/{hub}-port*") or glob.glob(
        f"{DEVICES}/{hub}/*:*/{hub}-port*")
    return sorted((pathlib.Path(p) for p in found), key=port_number)


def child_of(port: pathlib.Path) -> str | None:
    link = port / "device"
    return os.path.basename(os.path.realpath(link)) if link.exists() else None


def upstream(name: str) -> str:
    """The port a device name hangs off: `2-1.3` is port 3 of `2-1`, `2-1` port 1 of
    the root hub."""
    if "." in name:
        hub, n = name.rsplit(".", 1)
    else:
        bus, n = name.split("-", 1)
        hub = f"usb{bus}"
    return f"{hub}-port{n}"


PHANDLES: dict[int, pathlib.Path] = {}


def phandle(value: int) -> pathlib.Path | None:
    """The device tree node a phandle names. The tree is fixed while the system
    runs, so it is walked once."""
    if not PHANDLES:
        for here, _, files in os.walk(DEVICE_TREE):
            if "phandle" in files:
                with open(os.path.join(here, "phandle"), "rb") as f:
                    PHANDLES[int.from_bytes(f.read()[:4], "big")] = pathlib.Path(here)
        PHANDLES.setdefault(-1, DEVICE_TREE)
    return PHANDLES.get(value)


def connector(port: pathlib.Path) -> pathlib.Path | None:
    """The connector node a hub port is wired to, from the
    device tree's graph: the hub's `port@N` endpoint points at the connector's.
    A port that reaches it through a mux, as a USB 3 lane does, finds nothing, and
    its USB 2 peer is what answers for the socket."""
    hub = port.name.rsplit("-port", 1)[0]
    node = DEVICES / hub / "of_node"
    for base in (node / "ports", node):
        remote = base / f"port@{port_number(port)}" / "endpoint" / "remote-endpoint"
        try:
            target = phandle(int.from_bytes(remote.read_bytes()[:4], "big"))
        except OSError:
            continue
        while target is not None and target != DEVICE_TREE:
            if re.search(r"usb-[abc]-connector", read(target / "compatible")):
                return target
            target = target.parent
    return None


def scan() -> list[Entry]:
    sockets: dict[str, Entry] = {}
    internal: list[Entry] = []
    typec_ports = sorted(p for p in TYPEC.glob("port*") if re.fullmatch(r"port\d+", p.name))
    typecs = []
    controllers = {}
    for port in typec_ports:
        entry = Entry("", "typec")
        entry.typec = port
        typecs.append(entry)
        for link in port.glob("supplier:platform:*"):
            controllers[os.path.realpath(link)] = entry

    def typec_of(hub: str):
        where = os.path.realpath(DEVICES / hub)
        return next((e for c, e in controllers.items() if where.startswith(c + "/")), None)

    def walk(hub: str, owner: Entry | None) -> None:
        for port in ports_of(hub):
            child = child_of(port)
            device = Device(child) if child else None
            if owner is not None:
                if device is None:
                    continue
                entry = next((e for e in owner.children if e.number == port_number(port)), None)
                if entry is None:
                    entry = Entry("", "child")
                    entry.number = port_number(port)
                    owner.children.append(entry)
                entry.ports.append(port)
                entry.take(device)
                if device.is_hub:
                    walk(child, entry)
                continue
            ctype = read(port / "connect_type")
            entry = typec_of(hub)
            if entry is None:
                if ctype == "not used":
                    continue
                if ctype == "hardwired":
                    if device and device.is_hub:
                        walk(child, None)
                    elif device:
                        entry = Entry(f"Internal {len(internal) + 1}", "internal")
                        entry.ports.append(port)
                        entry.take(device)
                        internal.append(entry)
                    continue
                peer = port / "peer"
                key = min(os.path.realpath(port), os.path.realpath(peer)) if peer.exists() \
                    else os.path.realpath(port)
                entry = sockets.get(key)
                if entry is None:
                    entry = sockets[key] = Entry("", "socket")
            entry.ports.append(port)
            if device:
                entry.take(device)
                if device.is_hub:
                    walk(child, entry)

    roots = sorted((p.name for p in DEVICES.glob("usb*")), key=lambda n: int(n[3:]))
    for root in roots:
        walk(root, None)

    # Named for the connector, numbered only where a board has more than one of a
    # kind, and the Type-C port the PD controller drives is the first C.
    lettered = {"C": list(typecs), "A": [], "B": []}
    unknown = []
    for entry in sockets.values():
        entry.connector = next((c for c in map(connector, entry.ports) if c), None)
        found = entry.connector and re.search(r"usb-([abc])-connector",
                                              read(entry.connector / "compatible"))
        (lettered[found.group(1).upper()] if found else unknown).append(entry)
    for letter, group in lettered.items():
        for i, entry in enumerate(group):
            entry.name = f"USB-{letter}" + (f"{i + 1}" if len(group) > 1 else "")
    for i, entry in enumerate(unknown):
        entry.name = f"USB {i + 1}"

    def flat(entry: Entry) -> list[Entry]:
        out = [entry]
        for child in sorted(entry.children, key=lambda e: e.number):
            child.name = f"{entry.name}.{child.number}"
            out += flat(child)
        return out

    entries = []
    for entry in [*lettered["C"], *lettered["A"], *lettered["B"], *unknown, *internal]:
        entries += flat(entry)
    return entries


def pdos(directory: pathlib.Path) -> list[str]:
    """A PD capabilities directory, one `5V 3A 15W` per object."""
    out = []
    for obj in sorted(directory.glob("*:*"), key=lambda p: int(p.name.split(":")[0])):
        kind = obj.name.split(":", 1)[1]
        volts = number(read(obj / "voltage")) / 1000
        lo = number(read(obj / "minimum_voltage")) / 1000
        hi = number(read(obj / "maximum_voltage")) / 1000
        amps = number(read(obj / "maximum_current") or read(obj / "operational_current")) / 1000
        watts = number(read(obj / "maximum_power") or read(obj / "operational_power")) / 1000
        if kind == "fixed_supply":
            out.append(f"{volts:g}V {amps:g}A {volts * amps:g}W")
        elif kind == "programmable_supply":
            out.append(f"{lo:g}-{hi:g}V {amps:g}A PPS")
        elif kind == "battery":
            out.append(f"{lo:g}-{hi:g}V {watts:g}W battery")
        else:
            out.append(f"{lo:g}-{hi:g}V {amps:g}A variable")
    return out


CONNECTORS: dict[pathlib.Path, list[pathlib.Path]] = {}


def connector_vbus(node: pathlib.Path) -> list[str]:
    """The regulator a connector's `vbus-supply` names, as `5V enabled`. Matched on
    the regulator's own node rather than a device link, which a connector with no
    driver bound never gets. Fixed by the device tree, so looked up once."""
    if node not in CONNECTORS:
        try:
            supply = phandle(int.from_bytes((node / "vbus-supply").read_bytes()[:4], "big"))
        except OSError:
            supply = None
        CONNECTORS[node] = [
            reg for reg in REGULATORS.iterdir()
            if supply and pathlib.Path(os.path.realpath(reg / "of_node")) == supply
        ]
    return [f"{number(read(reg / 'microvolts')) / 1e6:g}V {read(reg / 'state') or '-'}"
            for reg in CONNECTORS[node]]


def nodes(interface: pathlib.Path) -> tuple[list[tuple[str, str]], list[pathlib.Path]]:
    """What the drivers made of an interface: (kind, name) pairs, and the SCSI
    devices under it, whose error counters belong to the disk."""
    found, scsi = [], []
    for here, dirs, _ in os.walk(interface):
        depth = here[len(str(interface)):].count(os.sep)
        if depth > 7:
            dirs[:] = []
            continue
        parent = os.path.basename(here)
        for d in sorted(dirs):
            if parent in NODES and not (parent == "sound" and not d.startswith("card")):
                if parent == "block":
                    size = number(read(os.path.join(here, d, "size"))) * 512 / 1e9
                    found.append((NODES[parent], f"{d} {size:.1f} GB"))
                else:
                    found.append((NODES[parent], d))
            elif re.fullmatch(r"event\d+", d) and parent.startswith("input"):
                found.append(("Input", d))
            elif SCSI_NAME.match(d):
                scsi.append(pathlib.Path(here) / d)
        dirs[:] = [d for d in dirs if d != "subsystem" and d != "power"]
    return found, scsi


class Log:
    """Kernel messages that name a USB device, a port, an interface or a disk, from
    boot onwards. Read on a thread: /dev/kmsg blocks, one record per read."""

    def __init__(self) -> None:
        self.records: deque = deque(maxlen=4000)
        self.version = 0

    def start(self) -> None:
        threading.Thread(target=self.follow, daemon=True).start()

    def follow(self) -> None:
        try:
            fd = os.open("/dev/kmsg", os.O_RDONLY)
        except OSError:
            return
        while True:
            try:
                record = os.read(fd, 8192).decode(errors="replace")
            except BrokenPipeError:
                continue
            except OSError:
                return
            head, _, text = record.partition(";")
            fields = head.split(",")
            found = MESSAGE.match(text.split("\n", 1)[0])
            if len(fields) < 3 or not found:
                continue
            level = int(fields[0]) & 7
            self.records.append((int(fields[2]) / 1e6, level, *found.groups()))
            token = found.group(2)
            if any(p.match(token) for p in (DEVICE_NAME, PORT_NAME, INTERFACE_NAME, SCSI_NAME)):
                self.version += 1


class Analyzer:
    """The entries, which one is chosen, and what each device has said about itself."""

    def __init__(self) -> None:
        self.entries: list[Entry] = []
        self.choice = 0
        self.log = Log()
        self.bos: dict[tuple, dict | None] = {}
        self.asking: set = set()
        self.changed = threading.Event()
        self.version = 0
        self.known: set[str] = set()
        self.selected_new = False
        self.names: dict[str, Entry] = {}

    def start(self) -> None:
        self.log.start()
        threading.Thread(target=self.listen, daemon=True).start()
        self.rescan()

    def listen(self) -> None:
        """Wake the next tick when the kernel adds or removes anything USB."""
        try:
            sock = socket.socket(socket.AF_NETLINK, socket.SOCK_DGRAM, NETLINK_KOBJECT_UEVENT)
            sock.bind((0, 1))
        except OSError:
            return
        while True:
            try:
                event = sock.recv(65536)
            except OSError:
                return
            if any(s in event for s in UEVENT_SUBSYSTEMS):
                self.changed.set()

    @property
    def entry(self) -> Entry | None:
        return self.entries[self.choice] if self.entries else None

    def rescan(self) -> None:
        held = self.entry.name if self.entry else None
        self.entries = scan()
        names = [e.name for e in self.entries]
        plugged = {f"{e.name}/{e.device.name}/{e.device['devnum']}" for e in self.entries
                   if e.device and e.kind != "internal"}
        fresh = sorted(plugged - self.known) if self.known else []
        self.known = plugged | {"*"}
        if fresh:
            held = fresh[0].split("/", 1)[0]
            self.selected_new = True
        self.choice = names.index(held) if held in names else min(self.choice, max(0, len(names) - 1))
        self.names = self.owners()

    def choose(self, step: int) -> None:
        self.choice = min(max(self.choice + step, 0), max(0, len(self.entries) - 1))

    def capabilities(self, device: Device) -> dict | None:
        """The BOS, asked for on a thread so a device that sits on a control request
        cannot hold the panel, and only for a device that says it has one."""
        if device.version < 2.01 or not device["busnum"] or not device["devnum"]:
            return None
        key = (device.name, device["busnum"], device["devnum"])
        if key not in self.bos and key not in self.asking:
            self.asking.add(key)

            def ask():
                self.bos[key] = bos(int(device["busnum"]), int(device["devnum"]))
                self.asking.discard(key)
                self.version += 1

            threading.Thread(target=ask, daemon=True).start()
        return self.bos.get(key)

    def owners(self) -> dict[str, Entry]:
        """Every name a kernel message might use for something an entry holds."""
        names: dict[str, Entry] = {}
        for entry in self.entries:
            for port in entry.ports:
                names[port.name] = entry
            if entry.device:
                names[entry.device.name] = entry
                for interface in entry.device.interfaces():
                    names[interface.name] = entry
                    for scsi in nodes(interface)[1]:
                        names[scsi.name] = entry
            if entry.typec:
                names[entry.typec.name] = entry
                names[os.path.basename(os.path.realpath(entry.typec / "device"))] = entry
                for link in entry.typec.glob("supplier:platform:*"):
                    names[os.path.basename(os.path.realpath(link))] = entry
        return names

    def owner(self, token: str, names: dict[str, Entry]) -> Entry | None:
        """Whose a message is. A device that never finished enumerating has a name and
        nothing in sysfs, so it belongs to whoever has its port, and failing that to
        the hub it was plugged into."""
        for _ in range(8):
            if token in names:
                return names[token]
            if INTERFACE_NAME.match(token):
                token = INTERFACE_NAME.match(token).group(1)
            elif PORT_NAME.match(token):
                token = PORT_NAME.match(token).group(1)
            elif DEVICE_NAME.match(token) and not token.startswith("usb"):
                port = upstream(token)
                if port in names:
                    return names[port]
                token = port.rsplit("-port", 1)[0]
            else:
                return None
        return None

    def messages(self, entry: Entry) -> list[tuple]:
        return [r for r in list(self.log.records) if self.owner(r[3], self.names) is entry]

    def rows(self) -> list[dict]:
        entry = self.entry
        rows = [row("Port", entry.name if entry else "none", kind=4)]
        if entry is None:
            return rows + [row("Device", "no USB found")]
        said = self.messages(entry)
        errors = [r for r in said if r[1] <= 3 or ERROR.search(r[4])]
        device = entry.device
        socket_max = self.socket_max(entry)

        if entry.kind == "socket":
            rows.append(row("Socket", f"USB{'3' if socket_max >= 5000 else '2'}, {rate(socket_max)}"))
            rows.append(row("Link", self.port_state(entry)))
        elif entry.kind == "typec":
            rows += self.typec_head(entry.typec)
        elif entry.kind == "internal":
            rows.append(row("Wired", "inside the case", dim=True))

        caps = self.capabilities(device) if device else None
        if device:
            rows += self.identity(device, caps, socket_max)
        elif entry.kind != "typec":
            rows.append(row("Device", "nothing connected"))
        last_plug = max((r[0] for r in said if PLUGGED.search(r[4])), default=None)
        if not device and errors and START_FAILED.search(errors[-1][4]) \
                and uptime() - errors[-1][0] < START_FAILED_S \
                and (last_plug is None or errors[-1][0] >= last_plug):
            rows.append(row("Note", "device failed to start"))
        if errors:
            rows.append(row("Errors", str(len(errors))))

        if device:
            rows += self.heading("Capabilities")
            rows += self.capability_rows(device, caps)
        rows += self.heading("Power")
        rows += self.power_rows(entry, device)
        if entry.typec:
            rows += self.gadget_rows(entry.typec)
            rows += self.altmode_rows(entry.typec)
        if device:
            rows += self.heading("Interfaces")
            rows += self.interface_rows(device)
            if device.is_hub:
                rows += self.heading("Ports")
                rows += self.hub_rows(device)
        rows += self.heading("Errors")
        rows += self.error_rows(entry, device, said, errors)
        return rows

    @staticmethod
    def heading(title: str) -> list[dict]:
        return [divider(), row(title, kind=3)]

    @staticmethod
    def socket_max(entry: Entry) -> float:
        speeds = []
        for port in entry.ports:
            root = os.path.realpath(port).split("/")
            bus = next((p for p in root if re.fullmatch(r"usb\d+", p)), None)
            if bus:
                speeds.append(number(read(DEVICES / bus / "speed")))
        return max(speeds, default=0)

    @staticmethod
    def port_state(entry: Entry) -> str:
        states = [read(p / "state") for p in entry.ports]
        order = ("configured", "suspended", "addressed", "default", "powered",
                 "attached", "not attached")
        best = min((s for s in states if s), key=lambda s: order.index(s) if s in order else 99,
                   default="-")
        return best

    def identity(self, device: Device, caps: dict | None, socket_max: float) -> list[dict]:
        vid, pid = device["idVendor"], device["idProduct"]
        name = device["product"] or IDS.product(vid, pid) or "-"
        maker = device["manufacturer"] or IDS.vendor(vid)[0] or "-"
        code = device.hex("bDeviceClass")
        if code in (0x00, 0xEF):
            seen = []
            for interface in device.interfaces():
                k = CLASSES.get(int(read(interface / "bInterfaceClass") or "0", 16), "?")
                if k not in seen:
                    seen.append(k)
            klass = ", ".join(seen) or "per interface"
        else:
            klass = IDS.klass(code, device.hex("bDeviceSubClass"), device.hex("bDeviceProtocol"))
        lanes = max(number(device["rx_lanes"], 1), number(device["tx_lanes"], 1))
        speed = device.speed
        speed_text = f"{rate(speed)} {SPEEDS.get(speed, '')}".strip()
        if lanes > 1:
            speed_text += f" x{lanes:g}"
        rows = [
            row("Device", name),
            row("Maker", maker),
            row("ID", f"{vid}:{pid}"),
            row("Class", klass),
            row("Speed", speed_text),
        ]
        capable = (caps or {}).get("max") or (5000 if device.version >= 3 else None)
        if capable and capable >= 5000 and speed <= 480 and socket_max >= 5000:
            rows.append(row("Note", "USB2 link, check cable"))
        if device["authorized"] == "0":
            rows.append(row("Note", "not authorized"))
        return rows

    @staticmethod
    def capability_rows(device: Device, caps: dict | None) -> list[dict]:
        rows = [row("USB", device["version"]),
                row("Release", f"{device.hex('bcdDevice') >> 8:x}.{device.hex('bcdDevice') & 0xFF:02x}")]
        if caps:
            if caps.get("max"):
                rows.append(row("Max speed", f"{rate(caps['max'])} {SPEEDS.get(caps['max'], '')}"))
            if "lpm" in caps:
                rows.append(row("LPM", ("yes, BESL" if caps.get("besl") else "yes")
                                if caps["lpm"] else "no"))
            if "u1" in caps:
                rows.append(row("U1/U2 exit", f"{caps['u1']}us / {caps['u2']}us"))
            if "ltm" in caps:
                rows.append(row("LTM", "yes" if caps["ltm"] else "no"))
            rows.append(row("BOS", ", ".join(caps["caps"]) or "empty", dim=True))
        elif device.version >= 2.01:
            rows.append(row("BOS", "not readable", dim=True))
        config = device["configuration"]
        rows.append(row("Config", f"{device['bConfigurationValue'] or '-'} of "
                                  f"{device['bNumConfigurations'] or '-'}"
                                  + (f" {config}" if config else "")))
        rows.append(row("EP0", f"{device['bMaxPacketSize0'] or '-'}B", dim=True))
        if device["serial"]:
            rows.append(row("Serial", device["serial"], dim=True))
        rows.append(row("Address", f"bus {device['busnum']} dev {device['devnum']}", dim=True))
        rows.append(row("Sysfs", device.name, dim=True))
        if device.hex("quirks"):
            rows.append(row("Quirks", device["quirks"]))
        return rows

    def power_rows(self, entry: Entry, device: Device | None) -> list[dict]:
        rows = []
        if entry.connector:
            for vbus in connector_vbus(entry.connector):
                rows.append(row("VBUS", vbus))
        if entry.typec:
            rows += self.typec_power(entry.typec)
        if device is None:
            return rows or [row("Draw", "-", dim=True)]
        attrs = device.hex("bmAttributes")
        rows.append(row("Max draw", device["bMaxPower"] or "-"))
        rows.append(row("Supply", "self powered" if attrs & 0x40 else "bus powered"))
        wake = "yes" if attrs & 0x20 else "no"
        if attrs & 0x20 and device["wakeup"]:
            wake += f", {device['wakeup']}"
        rows.append(row("Remote wake", wake))
        state = device["runtime_status"] or "-"
        if device["control"] == "auto":
            delay = number(device["autosuspend_delay_ms"], -1)
            state += f", auto {delay / 1000:g}s" if delay >= 0 else ", auto"
        rows.append(row("PM", state))
        connected = number(device["connected_duration"]) / 1000
        active = number(device["active_duration"]) / 1000
        rows.append(row("Connected", span(connected)))
        if connected:
            share = min(100, int(100 * active / connected))
            rows.append(row("Active", f"{share}%"))
        return rows

    @staticmethod
    def typec_head(port: pathlib.Path) -> list[dict]:
        partner = port.parent / f"{port.name}-partner"
        rows = [
            row("Data role", chosen(read(port / "data_role")) or "-"),
            row("Power role", chosen(read(port / "power_role")) or "-"),
        ]
        if partner.exists():
            kind = read(partner / "type")
            rows.append(row("Partner", kind if kind and not kind.startswith("not_") else "connected"))
            rows.append(row("Partner PD", read(partner / "supports_usb_power_delivery") or "-"))
        else:
            rows.append(row("Partner", "none"))
        rows.append(row("Orientation", read(port / "orientation") or "-"))
        mode = read(port / "power_operation_mode")
        rows.append(row("Mode", OPERATION.get(mode, mode or "-")))
        rows.append(row("Type-C rev", read(port / "usb_typec_revision") or "-", dim=True))
        rows.append(row("PD rev", read(port / "usb_power_delivery_revision") or "-", dim=True))
        return rows

    @staticmethod
    def typec_power(port: pathlib.Path) -> list[dict]:
        rows = []
        tcpc = os.path.realpath(port / "device")
        for psy in POWER_SUPPLY.iterdir():
            if os.path.realpath(psy / "device") != tcpc:
                continue
            if read(psy / "online") == "1":
                volts = number(read(psy / "voltage_now")) / 1e6
                amps = number(read(psy / "current_now") or read(psy / "current_max")) / 1e6
                rows.append(row("Contract", f"{volts:.1f}V {amps:.2f}A {volts * amps:.1f}W"))
                rows.append(row("Supply", chosen(read(psy / "usb_type")) or "-"))
            else:
                rows.append(row("Contract", "none"))
        partner = port.parent / f"{port.name}-partner"
        theirs = partner / "usb_power_delivery"
        for i, pdo in enumerate(pdos(theirs / "source-capabilities")):
            rows.append(row(f"Device source {i + 1}", pdo, dim=True))
        for i, pdo in enumerate(pdos(theirs / "sink-capabilities")):
            rows.append(row(f"Device sink {i + 1}", pdo, dim=True))
        ours = port / "usb_power_delivery"
        for i, pdo in enumerate(pdos(ours / "sink-capabilities")):
            rows.append(row(f"Flipper sink {i + 1}", pdo, dim=True))
        for i, pdo in enumerate(pdos(ours / "source-capabilities")):
            rows.append(row(f"Flipper source {i + 1}", pdo, dim=True))
        return rows

    def gadget_rows(self, port: pathlib.Path) -> list[dict]:
        """What this side looks like to a host, when the port is a device."""
        controllers = {os.path.basename(os.path.realpath(link))
                       for link in port.glob("supplier:platform:*")}
        udcs = [u for u in UDC.glob("*") if u.name in controllers] or list(UDC.glob("*"))
        if not udcs:
            return []
        udc = udcs[0]
        rows = self.heading("Gadget")
        rows.append(row("State", read(udc / "state") or "-"))
        speed = read(udc / "current_speed")
        rows.append(row("Speed", speed if speed and speed != "UNKNOWN" else "-"))
        rows.append(row("Max speed", read(udc / "maximum_speed") or "-", dim=True))
        for gadget in GADGETS.glob("*"):
            if read(gadget / "UDC") != udc.name:
                continue
            product = read(gadget / "strings" / "0x409" / "product")
            rows.append(row("Name", product or gadget.name))
            rows.append(row("ID", f"{read(gadget / 'idVendor')[2:]}:{read(gadget / 'idProduct')[2:]}",
                            dim=True))
            functions = []
            for link in sorted(gadget.glob("configs/*/*")):
                if link.is_symlink():
                    kind, _, instance = link.resolve().name.partition(".")
                    functions.append(instance if kind == "ffs" else kind)
            rows.append(row("Functions", ", ".join(functions) or "none"))
            break
        else:
            rows.append(row("Name", read(udc / "function") or "none"))
        return rows

    def altmode_rows(self, port: pathlib.Path) -> list[dict]:
        partner = port.parent / f"{port.name}-partner"
        modes = sorted(partner.glob(f"{partner.name}.*")) if partner.exists() else []
        mine = not modes
        modes = modes or sorted(port.glob(f"{port.name}.*"))
        if not modes:
            return []
        rows = self.heading("Alt modes")
        for mode in modes:
            svid = read(mode / "svid").lower()
            name = SVIDS.get(svid, f"SVID {svid or '-'}")
            state = "supported" if mine else ("active" if read(mode / "active") == "yes" else "off")
            rows.append(row(name, state))
        return rows

    @staticmethod
    def interface_rows(device: Device) -> list[dict]:
        rows = []
        for interface in device.interfaces():
            num = int(read(interface / "bInterfaceNumber") or "0", 16)
            alt = int(read(interface / "bAlternateSetting") or "0")
            code = int(read(interface / "bInterfaceClass") or "0", 16)
            klass = IDS.klass(code, int(read(interface / "bInterfaceSubClass") or "0", 16),
                              int(read(interface / "bInterfaceProtocol") or "0", 16))
            driver = interface / "driver"
            driver = os.path.basename(os.path.realpath(driver)) if driver.exists() else "no driver"
            label = f"If {num}" + (f" alt {alt}" if alt else "")
            rows.append(row(label, f"{klass}, {driver}"))
            text = read(interface / "interface")
            if text:
                rows.append(row("Name", text, dim=True))
            for kind, name in nodes(interface)[0]:
                rows.append(row(kind, name, dim=True))
            for ep in sorted(interface.glob("ep_*")):
                address = read(ep / "bEndpointAddress")
                size = int(read(ep / "wMaxPacketSize") or "0", 16)
                mult = ((size >> 11) & 3) + 1
                value = f"{read(ep / 'type')} {size & 0x7FF}B" + (f" x{mult}" if mult > 1 else "")
                interval = read(ep / "interval")
                if interval and read(ep / "type") not in ("Bulk", "Control"):
                    value += f" {interval}"
                rows.append(row(f"EP {address} {read(ep / 'direction')}", value, dim=True))
        return rows or [row("Interfaces", "none")]

    @staticmethod
    def hub_rows(device: Device) -> list[dict]:
        rows = []
        for port in ports_of(device.name):
            child = child_of(port)
            state = read(port / "state") or ("connected" if child else "empty")
            rows.append(row(f"Port {port_number(port)}", state))
            oc = number(read(port / "over_current_count"))
            if oc:
                rows.append(row("Over-current", f"{oc:g}", dim=True))
        return rows or [row("Ports", "none")]

    @staticmethod
    def error_rows(entry: Entry, device: Device | None, said: list, errors: list) -> list[dict]:
        now = uptime()
        plugs = [r for r in said if PLUGGED.search(r[4])]
        rows = [row("Kernel log", f"{len(errors)} error{'s' if len(errors) != 1 else ''}")]
        if plugs:
            rows.append(row("Plugged", f"{len(plugs)}x, last {span(now - plugs[-1][0])} ago"))
        over = sum(number(read(p / "over_current_count")) for p in entry.ports)
        rows.append(row("Over-current", f"{over:g}"))
        if device:
            for interface in device.interfaces():
                found, scsi = nodes(interface)
                for target in scsi:
                    failed = int(read(target / "ioerr_cnt") or "0", 16)
                    rows.append(row("Disk I/O err", str(failed)))
                for kind, name in found:
                    if kind == "Network":
                        stats = pathlib.Path("/sys/class/net") / name / "statistics"
                        rx, tx = read(stats / "rx_errors"), read(stats / "tx_errors")
                        rows.append(row("Net errors", f"rx {rx or '-'} tx {tx or '-'} {name}"))
        for at, _, driver, token, text in errors[-MESSAGES_SHOWN:][::-1]:
            for line in wrap(f"{span(now - at)} ago, {token}: {text}"):
                rows.append(row(line, kind=3, dim=True))
        return rows


def main() -> None:
    ui = flipctl.load(PAGE, "usb-analyzer.slint")
    analyzer = Analyzer()
    analyzer.start()
    state = {"offset": 0, "arrow": 0, "rows": [], "jump": None}

    def sections(rows: list[dict]) -> list[tuple[int, str]]:
        return [(i - 1, SECTIONS[r["label"]]) for i, r in enumerate(rows)
                if r["kind"] == 3 and r["label"] in SECTIONS]

    def draw() -> None:
        if analyzer.selected_new:
            analyzer.selected_new = False
            state["offset"] = 0
        rows = analyzer.rows()
        last = max(0, len(rows) - VISIBLE)
        state["offset"] = min(max(state["offset"], 0), last)
        ahead = [(min(i, last), word) for i, word in sections(rows) if min(i, last) > state["offset"]]
        state["jump"] = ahead[0][0] if ahead else 0
        ui.rows = rows
        ui.offset = state["offset"]
        ui.arrow_pressed = state["arrow"]
        ui.at_start = analyzer.choice == 0
        ui.at_end = analyzer.choice >= len(analyzer.entries) - 1
        ui.buttons = ["Close", "", "", "", ahead[0][1] if ahead else "Top"]

    @flipctl.on_key(ui)
    def _(key, down):
        if key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            state["arrow"] = 0 if not down else 1 if key is flipctl.Key.LEFT else 2
        if not down:
            draw()
            return
        if key in (flipctl.Key.BACK, flipctl.Key.ESCAPE):
            slint.quit_event_loop()
        elif key is flipctl.Key.RUN:
            state["offset"] = state["jump"] or 0
        elif key in (flipctl.Key.LEFT, flipctl.Key.RIGHT):
            analyzer.choose(1 if key is flipctl.Key.RIGHT else -1)
            state["offset"] = 0
        elif key is flipctl.Key.DOWN:
            state["offset"] += 1
        elif key is flipctl.Key.UP:
            state["offset"] -= 1
        draw()

    async def tick() -> None:
        """A rescan the moment the kernel says something changed, the counters once a
        second, and a redraw whenever the log or a BOS answer brings news."""
        counted = 0
        seen = (analyzer.log.version, analyzer.version)
        while True:
            await asyncio.sleep(0.1)
            counted += 1
            news = (analyzer.log.version, analyzer.version) != seen
            if analyzer.changed.is_set():
                analyzer.changed.clear()
                analyzer.rescan()
                news = True
            elif counted % 10 == 0:
                analyzer.rescan()
                news = True
            if news:
                seen = (analyzer.log.version, analyzer.version)
                draw()

    draw()
    flipctl.run(ui, tick())


if __name__ == "__main__":
    main()
