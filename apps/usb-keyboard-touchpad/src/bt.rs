//! The host over Bluetooth: the Flipper as a Bluetooth LE keyboard, mouse and media
//! keys, by HID over GATT, named "Flipper One HID" in a scan and "Flipper One USB
//! Keyboard Touchpad" once connected.
//!
//! BlueZ does the radio and the pairing; this app hands it, over D-Bus, the HID
//! service (the same report map the USB device has, one Report characteristic per
//! report with its Report Reference), Device Information and Battery, an
//! advertisement that says it is a HID device, and an agent that accepts a pairing
//! without a code. flipctl's `bt-hid` claim powers the radio and makes it pairable
//! for as long as this runs, and puts both back afterwards.
//!
//! The D-Bus side runs on a thread of its own, which drives zbus's async API with
//! `zbus::block_on`. Reports reach it over a channel and go out as notifications on
//! their characteristic, once the host has subscribed.

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};
use zbus::{interface, Connection, Proxy};

/// Where this app's objects live on the bus.
const ROOT: &str = "/flipctl/hid";
/// What a scanning host sees the Flipper as: the advertisement holds 18 characters
/// of name beside the flags, the service and the appearance.
const NAME: &str = "Flipper One HID";
/// What a host reads once it connects, and keeps for the paired device: the
/// adapter's name while this runs, which the `bt-hid` claim puts back on release.
const FULL_NAME: &str = "Flipper One USB Keyboard Touchpad";
const ADAPTER: &str = "/org/bluez/hci0";

/// The report map: the `hid` claim's descriptor, report for report.
const REPORT_MAP: &[u8] = &[
    0x05, 0x01, 0x09, 0x06, 0xa1, 0x01, 0x85, 0x01, 0x05, 0x07, 0x19, 0xe0, 0x29, 0xe7, 0x15, 0x00,
    0x25, 0x01, 0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x25, 0x65, 0x05, 0x07, 0x19, 0x00, 0x29, 0x65, 0x81, 0x00, 0xc0, 0x05,
    0x01, 0x09, 0x02, 0xa1, 0x01, 0x85, 0x02, 0x09, 0x01, 0xa1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29,
    0x03, 0x15, 0x00, 0x25, 0x01, 0x95, 0x03, 0x75, 0x01, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81,
    0x01, 0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7f, 0x75, 0x08, 0x95,
    0x03, 0x81, 0x06, 0xc0, 0xc0, 0x05, 0x0c, 0x09, 0x01, 0xa1, 0x01, 0x85, 0x03, 0x15, 0x00, 0x26,
    0xff, 0x03, 0x19, 0x00, 0x2a, 0xff, 0x03, 0x75, 0x10, 0x95, 0x01, 0x81, 0x00, 0xc0,
];

/// A 16-bit Bluetooth SIG UUID in the long form BlueZ takes.
fn uuid(short: u16) -> String {
    format!("0000{short:04x}-0000-1000-8000-00805f9b34fb")
}

fn path(rest: &str) -> OwnedObjectPath {
    OwnedObjectPath::try_from(format!("{ROOT}/{rest}")).expect("a valid object path")
}

struct Service {
    uuid: u16,
}

#[interface(name = "org.bluez.GattService1")]
impl Service {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        uuid(self.uuid)
    }

    #[zbus(property)]
    fn primary(&self) -> bool {
        true
    }
}

/// One characteristic. A Report's value is what was last sent; the others never
/// change.
struct Characteristic {
    uuid: u16,
    service: OwnedObjectPath,
    flags: Vec<&'static str>,
    value: Vec<u8>,
    notifying: bool,
    /// Whether any Report is subscribed to, which is the host being there.
    subscribed: Arc<Mutex<bool>>,
}

#[interface(name = "org.bluez.GattCharacteristic1")]
impl Characteristic {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        uuid(self.uuid)
    }

    #[zbus(property)]
    fn service(&self) -> OwnedObjectPath {
        self.service.clone()
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        self.flags.iter().map(|f| f.to_string()).collect()
    }

    #[zbus(property)]
    fn value(&self) -> Vec<u8> {
        self.value.clone()
    }

    #[zbus(property)]
    fn notifying(&self) -> bool {
        self.notifying
    }

    fn read_value(&self, _options: HashMap<String, OwnedValue>) -> Vec<u8> {
        self.value.clone()
    }

    /// Protocol Mode and the Control Point: taken and ignored. Report mode is the
    /// only one there is, and suspend means nothing to a remote control.
    fn write_value(&mut self, _value: Vec<u8>, _options: HashMap<String, OwnedValue>) {}

    fn start_notify(&mut self) {
        self.notifying = true;
        *self.subscribed.lock().unwrap() = true;
    }

    fn stop_notify(&mut self) {
        self.notifying = false;
    }
}

/// A Report's Report Reference: its report ID and that it is an input report.
struct Descriptor {
    characteristic: OwnedObjectPath,
    value: Vec<u8>,
}

#[interface(name = "org.bluez.GattDescriptor1")]
impl Descriptor {
    #[zbus(property, name = "UUID")]
    fn uuid(&self) -> String {
        uuid(0x2908)
    }

    #[zbus(property)]
    fn characteristic(&self) -> OwnedObjectPath {
        self.characteristic.clone()
    }

    #[zbus(property)]
    fn flags(&self) -> Vec<String> {
        vec!["read".into()]
    }

    fn read_value(&self, _options: HashMap<String, OwnedValue>) -> Vec<u8> {
        self.value.clone()
    }
}

struct Advertisement;

#[interface(name = "org.bluez.LEAdvertisement1")]
impl Advertisement {
    #[zbus(property, name = "Type")]
    fn kind(&self) -> String {
        "peripheral".into()
    }

    #[zbus(property, name = "ServiceUUIDs")]
    fn service_uuids(&self) -> Vec<String> {
        vec![uuid(0x1812)]
    }

    /// Given here rather than included from the adapter, which has [`FULL_NAME`]: the
    /// kernel cuts an included name past 10 characters down to "Flipper On".
    #[zbus(property)]
    fn local_name(&self) -> String {
        NAME.into()
    }

    /// A generic HID device: it is a keyboard and a mouse at once.
    #[zbus(property)]
    fn appearance(&self) -> u16 {
        0x03c0
    }

    #[zbus(property)]
    fn discoverable(&self) -> bool {
        true
    }

    fn release(&self) {}
}

/// Pairing without a code: the Flipper has no way to show or take one that the
/// host's prompt would ask for.
struct Agent;

#[interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    fn request_pin_code(&self, _device: OwnedObjectPath) -> String {
        "0000".into()
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, _pincode: String) {}

    fn request_passkey(&self, _device: OwnedObjectPath) -> u32 {
        0
    }

    fn display_passkey(&self, _device: OwnedObjectPath, _passkey: u32, _entered: u16) {}

    fn request_confirmation(&self, _device: OwnedObjectPath, _passkey: u32) {}

    fn request_authorization(&self, _device: OwnedObjectPath) {}

    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) {}

    fn cancel(&self) {}
}

/// The reports' characteristics, by report ID.
const REPORTS: [(u8, &str); 3] = [(1, "hid/keyboard"), (2, "hid/mouse"), (3, "hid/media")];

/// Put the objects on the bus. Returns the connection that serves them.
async fn serve(subscribed: &Arc<Mutex<bool>>) -> zbus::Result<Connection> {
    let hid = path("hid");
    let ch = |uuid: u16, service: &OwnedObjectPath, flags: &[&'static str], value: Vec<u8>| {
        Characteristic {
            uuid,
            service: service.clone(),
            flags: flags.to_vec(),
            value,
            notifying: false,
            subscribed: Arc::clone(subscribed),
        }
    };
    let info = path("info");
    let battery = path("battery");
    let mut b = zbus::connection::Builder::system()?
        .serve_at(ROOT, zbus::fdo::ObjectManager)?
        .serve_at(hid.clone(), Service { uuid: 0x1812 })?
        .serve_at(path("hid/mode"), ch(0x2a4e, &hid, &["read", "write-without-response"], vec![1]))?
        .serve_at(
            path("hid/map"),
            ch(0x2a4b, &hid, &["read", "encrypt-read"], REPORT_MAP.to_vec()),
        )?
        .serve_at(path("hid/info"), ch(0x2a4a, &hid, &["read"], vec![0x11, 0x01, 0x00, 0x02]))?
        .serve_at(path("hid/control"), ch(0x2a4c, &hid, &["write-without-response"], vec![0]))?
        .serve_at(info.clone(), Service { uuid: 0x180a })?
        // PnP ID: a USB vendor ID, the USB gadget's own IDs, version 1.0
        .serve_at(
            path("info/pnp"),
            ch(0x2a50, &info, &["read"], vec![0x02, 0x6b, 0x1d, 0x04, 0x01, 0x00, 0x01]),
        )?
        .serve_at(battery.clone(), Service { uuid: 0x180f })?
        .serve_at(path("battery/level"), ch(0x2a19, &battery, &["read", "notify"], vec![100]))?
        .serve_at(path("adv"), Advertisement)?
        .serve_at(path("agent"), Agent)?;
    for (id, at) in REPORTS {
        let report = ch(0x2a4d, &hid, &["read", "encrypt-read", "notify"], vec![0; size(id)]);
        b = b.serve_at(path(at), report)?.serve_at(
            path(&format!("{at}/ref")),
            Descriptor { characteristic: path(at), value: vec![id, 1] },
        )?;
    }
    b.build().await
}

/// A report's length without its ID: keyboard 8, mouse 4, media 2.
fn size(id: u8) -> usize {
    match id {
        1 => 8,
        2 => 4,
        _ => 2,
    }
}

/// Register the agent, the GATT application and the advertisement with BlueZ.
async fn register(conn: &Connection) -> zbus::Result<()> {
    let none: HashMap<&str, Value> = HashMap::new();
    let adapter = Proxy::new(conn, "org.bluez", ADAPTER, "org.freedesktop.DBus.Properties").await?;
    adapter.call_method("Set", &("org.bluez.Adapter1", "Alias", Value::from(FULL_NAME))).await?;
    let agent = ObjectPath::from(path("agent"));
    let agents = Proxy::new(conn, "org.bluez", "/org/bluez", "org.bluez.AgentManager1").await?;
    agents.call_method("RegisterAgent", &(&agent, "NoInputNoOutput")).await?;
    agents.call_method("RequestDefaultAgent", &(&agent,)).await?;
    let gatt = Proxy::new(conn, "org.bluez", ADAPTER, "org.bluez.GattManager1").await?;
    gatt.call_method("RegisterApplication", &(ObjectPath::try_from(ROOT)?, &none)).await?;
    let adverts = Proxy::new(conn, "org.bluez", ADAPTER, "org.bluez.LEAdvertisingManager1").await?;
    adverts.call_method("RegisterAdvertisement", &(ObjectPath::from(path("adv")), &none)).await?;
    Ok(())
}

/// Take it all back: the advertisement, the application and the agent.
async fn unregister(conn: &Connection) {
    let root = OwnedObjectPath::try_from(ROOT).expect("a valid object path");
    let calls = [
        (ADAPTER, "org.bluez.LEAdvertisingManager1", "UnregisterAdvertisement", path("adv")),
        (ADAPTER, "org.bluez.GattManager1", "UnregisterApplication", root),
        ("/org/bluez", "org.bluez.AgentManager1", "UnregisterAgent", path("agent")),
    ];
    for (at, iface, method, arg) in calls {
        if let Ok(p) = Proxy::new(conn, "org.bluez", at, iface).await {
            let _ = p.call_method(method, &(ObjectPath::from(arg),)).await;
        }
    }
}

/// A report on a characteristic, sent as a notification if the host subscribed.
async fn notify(conn: &Connection, at: &str, value: &[u8]) -> zbus::Result<()> {
    let iface = conn.object_server().interface::<_, Characteristic>(path(at)).await?;
    let notifying = {
        let mut ch = iface.get_mut().await;
        ch.value = value.to_vec();
        ch.notifying
    };
    if notifying {
        iface.get().await.value_changed(iface.signal_emitter()).await?;
    }
    Ok(())
}

pub struct Bluetooth {
    reports: Sender<Vec<u8>>,
    state: Arc<Mutex<String>>,
    subscribed: Arc<Mutex<bool>>,
}

impl Bluetooth {
    /// Take the radio, put the objects on the bus, and send reports until dropped.
    pub fn start() -> Bluetooth {
        let (reports, rx) = channel::<Vec<u8>>();
        let state = Arc::new(Mutex::new("starting".to_string()));
        let subscribed = Arc::new(Mutex::new(false));
        let (s, sub) = (Arc::clone(&state), Arc::clone(&subscribed));
        std::thread::spawn(move || run(rx, s, sub));
        Bluetooth { reports, state, subscribed }
    }

    /// One report, its ID first as the USB device sends it.
    pub fn send(&mut self, report: &[u8]) {
        let _ = self.reports.send(report.to_vec());
    }

    pub fn state(&self) -> String {
        let state = self.state.lock().unwrap().clone();
        if state == "ready" && !*self.subscribed.lock().unwrap() {
            return "waiting".into();
        }
        state
    }
}

fn run(rx: Receiver<Vec<u8>>, state: Arc<Mutex<String>>, subscribed: Arc<Mutex<bool>>) {
    let set = |s: &str| {
        *state.lock().unwrap() = s.to_string();
        flipctl_app::redraw();
    };
    // The host's screen has room for a word: the reason goes to the log
    let fail = |why: String| {
        eprintln!("usb-keyboard-touchpad: Bluetooth: {why}");
        set("failed");
    };
    // Held for as long as the thread runs: closing it gives the radio back
    let _claim = match flipctl_app::claim("bt-hid") {
        Ok(c) => c,
        Err(e) => return fail(e),
    };
    let conn = match zbus::block_on(serve(&subscribed)) {
        Ok(c) => c,
        Err(e) => return fail(format!("D-Bus: {e}")),
    };
    if let Err(e) = zbus::block_on(register(&conn)) {
        zbus::block_on(unregister(&conn));
        return fail(format!("BlueZ: {e}"));
    }
    set("ready");
    loop {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(report) => {
                let Some((&id, body)) = report.split_first() else { continue };
                if let Some((_, at)) = REPORTS.iter().find(|(r, _)| *r == id) {
                    let _ = zbus::block_on(notify(&conn, at, body));
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    zbus::block_on(unregister(&conn));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_map_is_the_usb_descriptor() {
        assert_eq!(REPORT_MAP.len(), 126);
        // Report IDs 1, 2 and 3, in that order
        let ids: Vec<u8> = REPORT_MAP.windows(2).filter(|w| w[0] == 0x85).map(|w| w[1]).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert_eq!(uuid(0x1812), "00001812-0000-1000-8000-00805f9b34fb");
    }
}
