//! ModemManager, over D-Bus, for the 5G Modem pages and the status bar's badge.
//!
//! Told rather than asked, like `net`: one thread holds a connection to the system
//! bus, reads every modem once, and then reads again only when ModemManager says
//! something changed -- a property, a modem coming or going, ModemManager itself
//! restarting. Reading is one `GetManagedObjects` call, every modem with every
//! property in about 7 ms, so a change is answered by reading the lot rather than
//! by patching the one property that moved.
//!
//! Two things ModemManager only does when asked, and they are asked here:
//!
//! - **Detailed signal** (RSRP, SNR) is read by ModemManager only after
//!   `Signal.Setup(rate)`. Every modem gets a 10 s rate when it appears, so the
//!   status bar and the menu always have an RSRP to work from, and 2 s while a
//!   screen holds a `Focus`.
//! - **Cell info** (the serving cell's channel, so its band) is a method call, made
//!   every 5 s while a screen holds a `Focus` and not otherwise. ModemManager's bus
//!   policy refuses it to anyone but root unless systemd/flipctl-modemmanager.conf
//!   is installed; the refusal is logged once and the band shows `--`.
//!
//! Started on first use, from whichever of the status bar or the menu asks first, so
//! a build that shows neither never connects. Without the `modem` feature the same
//! functions answer "no modems" and nothing is started.

pub use crate::modem::Modem;

#[cfg(feature = "modem")]
pub use watch::{antenna_focus, focus, generation, modems, reset, AntennaFocus, Focus};

/// Restart the modem in `physdev` through ModemManager, which is what makes it read
/// its SIM again.
#[cfg(not(feature = "modem"))]
pub fn reset(_physdev: &str) {}

/// Every modem ModemManager has, newest reading.
#[cfg(not(feature = "modem"))]
pub fn modems() -> Vec<Modem> {
    Vec::new()
}

/// Changes each time `modems()` would answer differently.
#[cfg(not(feature = "modem"))]
pub fn generation() -> u64 {
    0
}

#[cfg(not(feature = "modem"))]
pub struct Focus;

#[cfg(not(feature = "modem"))]
pub fn focus() -> Focus {
    Focus
}

#[cfg(not(feature = "modem"))]
pub struct AntennaFocus;

#[cfg(not(feature = "modem"))]
pub fn antenna_focus() -> AntennaFocus {
    AntennaFocus
}

/// The rfkill switch on the M.2 slot's W_DISABLE1# line, by its name.
const M2_SWITCH: &str = "M.2 WWAN";

/// Whether the M.2 slot's radio switch is blocked, by software or by hand.
///
/// sysfs rather than `/dev/rfkill`: a read of two files per switch, cheap enough to
/// ask whenever a row is built, and the change that matters arrives anyway as
/// ModemManager disabling the modem.
pub fn m2_blocked() -> bool {
    let Ok(dir) = std::fs::read_dir("/sys/class/rfkill") else {
        return false;
    };
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    dir.flatten().map(|e| e.path()).any(|p| {
        read(p.join("name")).trim() == M2_SWITCH
            && (read(p.join("soft")).trim() == "1" || read(p.join("hard")).trim() == "1")
    })
}

/// Block or unblock the M.2 slot's radio switch.
///
/// One `struct rfkill_event` written to `/dev/rfkill`: the switch's index, found by
/// name in sysfs since the numbers depend on probe order, and `RFKILL_OP_CHANGE`
/// with the soft state wanted. Needs write access to the node, which
/// 70-flipctl-devices.rules gives the `rfkill` group. Blocking it turns the module's
/// radio off in hardware, and NetworkManager then has ModemManager disable the
/// modem; unblocking brings it back in a few seconds.
pub fn set_m2_blocked(blocked: bool) -> std::io::Result<()> {
    use std::io::Write;
    const RFKILL_OP_CHANGE: u8 = 2;
    let read = |p: std::path::PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let idx = std::fs::read_dir("/sys/class/rfkill")?
        .flatten()
        .map(|e| e.path())
        .find(|p| read(p.join("name")).trim() == M2_SWITCH)
        .and_then(|p| read(p.join("index")).trim().parse::<u32>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, M2_SWITCH))?;
    // idx, type (ignored for a change), op, soft, hard.
    let mut event = [0u8; 8];
    event[..4].copy_from_slice(&idx.to_ne_bytes());
    event[5] = RFKILL_OP_CHANGE;
    event[6] = u8::from(blocked);
    std::fs::OpenOptions::new().write(true).open("/dev/rfkill")?.write_all(&event)
}

#[cfg(feature = "modem")]
mod watch {
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
    use std::sync::{Mutex, OnceLock};
    use std::thread;
    use std::time::{Duration, Instant};

    use zbus::blocking::{Connection, MessageIterator, Proxy};
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

    use crate::modem::{self, mm, Antennas, Bearer, Cell, Lte, Modem, Nr, Rat, Sim, Umts};

    const BUS: &str = "org.freedesktop.ModemManager1";
    const ROOT: &str = "/org/freedesktop/ModemManager1";
    const MODEM: &str = "org.freedesktop.ModemManager1.Modem";
    const GPP: &str = "org.freedesktop.ModemManager1.Modem.Modem3gpp";
    const SIGNAL: &str = "org.freedesktop.ModemManager1.Modem.Signal";

    /// `Signal.Setup` while nobody is looking, and while a screen is.
    const RATE_IDLE: u32 = 10;
    const RATE_FOCUSED: u32 = 2;
    /// How often a focused screen's cells are asked for.
    const CELLS_EVERY: Duration = Duration::from_secs(5);
    /// How often the per-antenna readings are asked for while the Signal page is open.
    const ANTENNAS_EVERY: Duration = Duration::from_secs(2);
    /// How long to wait for a burst of signals to finish before reading. A modem
    /// registering changes a dozen properties within a few milliseconds.
    const SETTLE: Duration = Duration::from_millis(200);
    /// Between attempts when there is no system bus at all.
    const RETRY: Duration = Duration::from_secs(10);

    enum Wake {
        /// ModemManager said something.
        Told,
        /// A screen took or dropped its `Focus`.
        Focus,
        /// The bus connection's signal stream ended.
        Ended,
    }

    struct Shared {
        modems: Mutex<Vec<Modem>>,
        generation: AtomicU64,
        focused: AtomicUsize,
        antennas: AtomicUsize,
        poke: Mutex<Sender<Wake>>,
    }

    fn shared() -> &'static Shared {
        static SHARED: OnceLock<Shared> = OnceLock::new();
        SHARED.get_or_init(|| {
            let (poke, wakes) = mpsc::channel();
            let told = poke.clone();
            thread::Builder::new()
                .name("mm-watch".into())
                .spawn(move || run(told, wakes))
                .expect("spawn mm watcher");
            Shared {
                modems: Mutex::new(Vec::new()),
                generation: AtomicU64::new(0),
                focused: AtomicUsize::new(0),
                antennas: AtomicUsize::new(0),
                poke: Mutex::new(poke),
            }
        })
    }

    /// Every modem ModemManager has, newest reading. A lock and a clone: cheap
    /// enough for the status bar, which reads it on the render loop.
    pub fn modems() -> Vec<Modem> {
        shared().modems.lock().map(|m| m.clone()).unwrap_or_default()
    }

    /// Changes each time `modems()` would answer differently, so each reader can
    /// keep its own last-seen value rather than sharing one dirty flag.
    pub fn generation() -> u64 {
        shared().generation.load(Ordering::Relaxed)
    }

    /// Held by a screen that shows the modem in detail: faster signal, and cells.
    /// Dropping it is what turns both back down, so it goes with the screen.
    pub struct Focus(());

    pub fn focus() -> Focus {
        let s = shared();
        s.focused.fetch_add(1, Ordering::Relaxed);
        let _ = s.poke.lock().map(|p| p.send(Wake::Focus));
        Focus(())
    }

    impl Drop for Focus {
        fn drop(&mut self) {
            let s = shared();
            s.focused.fetch_sub(1, Ordering::Relaxed);
            let _ = s.poke.lock().map(|p| p.send(Wake::Focus));
        }
    }

    /// Held by the Signal page: per-antenna readings every 2 s, through MM's
    /// `Command()`, on a Quectel modem. Three AT commands a time, and only these, so
    /// they stop the moment the page closes.
    pub struct AntennaFocus(());

    pub fn antenna_focus() -> AntennaFocus {
        let s = shared();
        s.antennas.fetch_add(1, Ordering::Relaxed);
        let _ = s.poke.lock().map(|p| p.send(Wake::Focus));
        AntennaFocus(())
    }

    impl Drop for AntennaFocus {
        fn drop(&mut self) {
            let s = shared();
            s.antennas.fetch_sub(1, Ordering::Relaxed);
            let _ = s.poke.lock().map(|p| p.send(Wake::Focus));
        }
    }

    fn antennas_wanted() -> bool {
        shared().antennas.load(Ordering::Relaxed) > 0
    }

    /// Restart the modem in `physdev` through ModemManager's `Reset()`, which is
    /// what makes it read its SIM again after the board's selector has moved.
    ///
    /// On a thread of its own: the call returns once the modem has been told, but
    /// that takes a moment, and the modem then leaves the bus for about a minute.
    /// The watcher sees it go and come back as a new object in the same port.
    pub fn reset(physdev: &str) {
        let physdev = physdev.to_string();
        let started = thread::Builder::new().name("mm-reset".into()).spawn(move || {
            let asked = Connection::system().and_then(|conn| {
                let managed: Managed =
                    Proxy::new(&conn, BUS, ROOT, "org.freedesktop.DBus.ObjectManager")?
                        .call("GetManagedObjects", &())?;
                let path = managed
                    .iter()
                    .find(|(_, ifaces)| {
                        ifaces.get(MODEM).and_then(|p| str_of(p, "Physdev")).as_deref()
                            == Some(&physdev)
                    })
                    .map(|(path, _)| path.to_string())
                    .ok_or_else(|| zbus::Error::Failure("the modem has gone".into()))?;
                let modem = Proxy::new(&conn, BUS, path.as_str(), MODEM)?;
                let done = modem.call::<_, _, ()>("Reset", &());
                done
            });
            match asked {
                Ok(()) => crate::logline!("modem          reset"),
                Err(e) => crate::logline!("modem          reset refused: {e}"),
            }
        });
        if let Err(e) = started {
            crate::logline!("modem          cannot start a reset: {e}");
        }
    }

    fn focused() -> bool {
        shared().focused.load(Ordering::Relaxed) > 0
    }

    fn publish(fresh: Vec<Modem>) {
        let s = shared();
        if let Ok(mut cur) = s.modems.lock() {
            if *cur != fresh {
                *cur = fresh;
                s.generation.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// What the watcher remembers between readings.
    #[derive(Default)]
    struct Memory {
        /// The last cells read for each modem, by object path.
        cells: HashMap<String, Vec<Cell>>,
        cells_at: Option<Instant>,
        /// Set once the bus policy has refused `GetCellInfo`: asking again every five
        /// seconds would only fill the log. Cleared when a screen takes focus.
        cells_refused: bool,
        /// Modems whose `Signal.Setup` was refused at this rate, for the same reason.
        setup_refused: HashSet<(String, u32)>,
        /// The last per-antenna readings for each modem, or why there are none.
        antennas: HashMap<String, Result<Vec<Antennas>, String>>,
        antennas_at: Option<Instant>,
    }

    fn run(told: Sender<Wake>, wakes: Receiver<Wake>) {
        let mut memory = Memory::default();
        loop {
            let conn = match Connection::system() {
                Ok(conn) => conn,
                Err(e) => {
                    crate::logline!("modem          no system bus: {e}");
                    publish(Vec::new());
                    thread::sleep(RETRY);
                    continue;
                }
            };
            if let Err(e) = listen(&conn, told.clone()) {
                crate::logline!("modem          cannot follow ModemManager: {e}");
            }
            publish(read(&conn, &mut memory));
            loop {
                // Only a focused screen has a reason to wake on a timer: the cells, and
                // on the Signal page the antennas.
                let wake = if focused() {
                    let left = |at: Option<Instant>, every: Duration| {
                        at.map_or(Duration::ZERO, |at| every.saturating_sub(at.elapsed()))
                    };
                    let mut due = left(memory.cells_at, CELLS_EVERY);
                    if antennas_wanted() {
                        due = due.min(left(memory.antennas_at, ANTENNAS_EVERY));
                    }
                    match wakes.recv_timeout(due) {
                        Ok(wake) => Some(wake),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => return,
                    }
                } else {
                    match wakes.recv() {
                        Ok(wake) => Some(wake),
                        Err(_) => return,
                    }
                };
                let mut ended = false;
                match wake {
                    Some(Wake::Told) => {
                        while let Ok(next) = wakes.recv_timeout(SETTLE) {
                            ended |= matches!(next, Wake::Ended);
                        }
                    }
                    Some(Wake::Focus) => {
                        // Ask straight away, and again even if it was refused before:
                        // the policy file may have been installed since.
                        memory.cells_at = None;
                        memory.antennas_at = None;
                        memory.cells_refused = false;
                        memory.setup_refused.clear();
                    }
                    Some(Wake::Ended) => ended = true,
                    None => {}
                }
                if !focused() {
                    memory.cells.clear();
                    memory.cells_at = None;
                }
                if !antennas_wanted() {
                    memory.antennas.clear();
                }
                publish(read(&conn, &mut memory));
                if ended {
                    break;
                }
            }
            thread::sleep(RETRY);
        }
    }

    /// Forward every signal ModemManager sends, and its name changing hands, to the
    /// watcher. Matched on ModemManager's object paths rather than its bus name: a
    /// signal carries the sender's unique name, which is new after every restart.
    fn listen(conn: &Connection, told: Sender<Wake>) -> zbus::Result<()> {
        let ours = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .path_namespace(ROOT)?
            .build();
        let owner = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")?
            .member("NameOwnerChanged")?
            .add_arg(BUS)?
            .build();
        for rule in [ours, owner] {
            let messages = MessageIterator::for_match_rule(rule, conn, Some(64))?;
            let told = told.clone();
            thread::Builder::new()
                .name("mm-signals".into())
                .spawn(move || {
                    for message in messages {
                        if message.is_err() || told.send(Wake::Told).is_err() {
                            break;
                        }
                    }
                    let _ = told.send(Wake::Ended);
                })
                .map_err(|e| zbus::Error::Failure(e.to_string()))?;
        }
        Ok(())
    }

    type Props = HashMap<String, OwnedValue>;
    type Managed = HashMap<OwnedObjectPath, HashMap<String, Props>>;

    /// Every modem, from one call. ModemManager not running is no modems.
    fn read(conn: &Connection, memory: &mut Memory) -> Vec<Modem> {
        let managed: Managed =
            match Proxy::new(conn, BUS, ROOT, "org.freedesktop.DBus.ObjectManager")
                .and_then(|om| om.call("GetManagedObjects", &()))
            {
                Ok(managed) => managed,
                Err(_) => return Vec::new(),
            };
        let rate = if focused() { RATE_FOCUSED } else { RATE_IDLE };
        let cells_due = focused()
            && !memory.cells_refused
            && memory.cells_at.is_none_or(|at| at.elapsed() >= CELLS_EVERY);
        if cells_due {
            memory.cells_at = Some(Instant::now());
        }
        let antennas_due =
            antennas_wanted() && memory.antennas_at.is_none_or(|at| at.elapsed() >= ANTENNAS_EVERY);
        if antennas_due {
            memory.antennas_at = Some(Instant::now());
        }
        let mut out = Vec::new();
        for (path, ifaces) in &managed {
            let Some(props) = ifaces.get(MODEM) else { continue };
            let path = path.as_str();
            let signal = ifaces.get(SIGNAL);
            if signal.is_some() && signal.and_then(|s| u32_of(s, "Rate")) != Some(rate) {
                set_rate(conn, path, rate, memory);
            }
            if cells_due {
                match cell_info(conn, path) {
                    Ok(cells) => {
                        memory.cells.insert(path.to_string(), cells);
                    }
                    Err(e) => {
                        memory.cells_refused = true;
                        crate::logline!("modem          no cell info: {e}");
                    }
                }
            }
            if antennas_due && str_of(props, "Plugin").as_deref() == Some("quectel") {
                memory.antennas.insert(path.to_string(), antennas_of(conn, path));
            }
            let mut m = modem(props, ifaces.get(GPP), signal, memory.cells.get(path));
            match memory.antennas.get(path) {
                Some(Ok(sets)) => m.antennas = sets.clone(),
                Some(Err(why)) => m.antennas_note = why.clone(),
                None if antennas_wanted() && m.plugin != "quectel" => {
                    m.antennas_note = "Not on this modem".into()
                }
                None => {}
            }
            // The SIM and the bearer are objects of their own, outside the managed
            // set, so each is one more call: made only while a screen shows them.
            if focused() {
                m.sim = object_of(props, "Sim").and_then(|sim| sim_of(conn, &sim));
                m.bearer = bearer_of(conn, props);
            }
            out.push(m);
        }
        // Forget the cells of modems that have gone: a modem that comes back is a new
        // object, and its old path will not be used again.
        memory.cells.retain(|path, _| managed.keys().any(|p| p.as_str() == path));
        out
    }

    /// The per-antenna readings, from the three Quectel commands through MM's own
    /// AT port queue. MM refuses `Command()` unless it runs with `--debug`.
    fn antennas_of(conn: &Connection, path: &str) -> Result<Vec<Antennas>, String> {
        let modem = Proxy::new(conn, BUS, path, MODEM).map_err(|e| e.to_string())?;
        let ask = |cmd: &str| -> Result<String, String> {
            modem.call::<_, _, String>("Command", &(cmd, 3u32)).map_err(|e| {
                let said = e.to_string();
                if said.contains("debug mode") {
                    "Needs ModemManager debug mode".to_string()
                } else {
                    said
                }
            })
        };
        let sets = modem::antennas(&ask("AT+QRSRP")?, &ask("AT+QRSRQ")?, &ask("AT+QSINR")?);
        if sets.is_empty() {
            return Err("No reading: LTE and 5G only".into());
        }
        Ok(sets)
    }

    fn set_rate(conn: &Connection, path: &str, rate: u32, memory: &mut Memory) {
        if memory.setup_refused.contains(&(path.to_string(), rate)) {
            return;
        }
        let asked =
            Proxy::new(conn, BUS, path, SIGNAL).and_then(|p| p.call::<_, _, ()>("Setup", &(rate,)));
        if let Err(e) = asked {
            crate::logline!("modem          signal setup {rate}s refused: {e}");
            memory.setup_refused.insert((path.to_string(), rate));
        }
    }

    fn cell_info(conn: &Connection, path: &str) -> zbus::Result<Vec<Cell>> {
        let cells: Vec<Props> = Proxy::new(conn, BUS, path, MODEM)?.call("GetCellInfo", &())?;
        Ok(cells
            .iter()
            .map(|c| {
                let rat = match u32_of(c, "cell-type") {
                    Some(mm::CELL_UMTS) => Rat::Umts,
                    Some(mm::CELL_LTE) => Rat::Lte,
                    Some(mm::CELL_5GNR) => Rat::Nr,
                    _ => Rat::Other,
                };
                let channel = match rat {
                    Rat::Umts => u32_of(c, "uarfcn"),
                    Rat::Lte => u32_of(c, "earfcn"),
                    Rat::Nr => u32_of(c, "nrarfcn"),
                    Rat::Other => None,
                };
                Cell {
                    rat,
                    serving: bool_of(c, "serving").unwrap_or(false),
                    channel,
                    // Hex, as MM gives it: "14C" is PCI 332.
                    pci: str_of(c, "physical-ci").and_then(|h| u32::from_str_radix(&h, 16).ok()),
                    ci: str_of(c, "ci").unwrap_or_default(),
                    tac: str_of(c, "tac").unwrap_or_default(),
                    rsrp: f64_of(c, "rsrp"),
                    rsrq: f64_of(c, "rsrq"),
                    sinr: f64_of(c, "sinr"),
                }
            })
            .collect())
    }

    fn modem(
        props: &Props,
        gpp: Option<&Props>,
        signal: Option<&Props>,
        cells: Option<&Vec<Cell>>,
    ) -> Modem {
        let gpp_str = |k: &str| gpp.and_then(|g| str_of(g, k)).unwrap_or_default();
        let sig = |iface: &str| signal.and_then(|s| dict_of(s, iface)).unwrap_or_default();
        let (lte, nr, umts) = (sig("Lte"), sig("Nr5g"), sig("Umts"));
        Modem {
            physdev: str_of(props, "Physdev").unwrap_or_default(),
            model: str_of(props, "Model").unwrap_or_default(),
            imei: str_of(props, "EquipmentIdentifier").unwrap_or_default(),
            state: props.get("State").and_then(|v| i32::try_from(&**v).ok()).unwrap_or(0),
            failed_reason: u32_of(props, "StateFailedReason").unwrap_or(0),
            access_tech: u32_of(props, "AccessTechnologies").unwrap_or(0),
            quality: props
                .get("SignalQuality")
                .and_then(|v| <(u32, bool)>::try_from(v.try_clone().ok()?).ok())
                .map(|(q, _recent)| q),
            operator_name: gpp_str("OperatorName"),
            operator_code: gpp_str("OperatorCode"),
            registration: gpp.and_then(|g| u32_of(g, "RegistrationState")).unwrap_or(0),
            lte: Lte {
                rssi: f64_of(&lte, "rssi"),
                rsrp: f64_of(&lte, "rsrp"),
                rsrq: f64_of(&lte, "rsrq"),
                snr: f64_of(&lte, "snr"),
            },
            nr: Nr {
                rsrp: f64_of(&nr, "rsrp"),
                rsrq: f64_of(&nr, "rsrq"),
                snr: f64_of(&nr, "snr"),
            },
            umts: Umts {
                rssi: f64_of(&umts, "rssi"),
                rscp: f64_of(&umts, "rscp"),
                ecio: f64_of(&umts, "ecio"),
            },
            cells: cells.cloned().unwrap_or_default(),
            manufacturer: str_of(props, "Manufacturer").unwrap_or_default(),
            revision: str_of(props, "Revision").unwrap_or_default(),
            hardware_revision: str_of(props, "HardwareRevision").unwrap_or_default(),
            carrier_config: str_of(props, "CarrierConfiguration").unwrap_or_default(),
            carrier_revision: str_of(props, "CarrierConfigurationRevision").unwrap_or_default(),
            plugin: str_of(props, "Plugin").unwrap_or_default(),
            primary_port: str_of(props, "PrimaryPort").unwrap_or_default(),
            ports: props
                .get("Ports")
                .and_then(|v| Vec::<(String, u32)>::try_from(v.try_clone().ok()?).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|(name, kind)| format!("{name} ({})", mm::port_type(kind)))
                .collect(),
            drivers: strs_of(props, "Drivers"),
            usb_speed: str_of(props, "Physdev").and_then(|dev| {
                std::fs::read_to_string(format!("{dev}/speed")).ok()?.trim().parse().ok()
            }),
            own_numbers: strs_of(props, "OwnNumbers"),
            unlock_required: u32_of(props, "UnlockRequired").unwrap_or(0),
            unlock_retries: props
                .get("UnlockRetries")
                .and_then(|v| HashMap::<u32, u32>::try_from(v.try_clone().ok()?).ok())
                .map(|m| {
                    let mut v: Vec<(u32, u32)> = m.into_iter().collect();
                    v.sort();
                    v
                })
                .unwrap_or_default(),
            sim: None,
            bearer: None,
            antennas: Vec::new(),
            antennas_note: String::new(),
        }
    }

    fn all(conn: &Connection, path: &str, iface: &str) -> Option<Props> {
        Proxy::new(conn, BUS, path, "org.freedesktop.DBus.Properties")
            .and_then(|p| p.call("GetAll", &(iface,)))
            .ok()
    }

    fn sim_of(conn: &Connection, path: &str) -> Option<Sim> {
        let p = all(conn, path, "org.freedesktop.ModemManager1.Sim")?;
        let s = |k: &str| str_of(&p, k).unwrap_or_default();
        Some(Sim {
            iccid: s("SimIdentifier"),
            imsi: s("Imsi"),
            operator_id: s("OperatorIdentifier"),
            operator_name: s("OperatorName"),
            eid: s("Eid"),
        })
    }

    /// The modem's connected bearer, or its first when none is connected.
    fn bearer_of(conn: &Connection, props: &Props) -> Option<Bearer> {
        let paths = props
            .get("Bearers")
            .and_then(|v| Vec::<OwnedObjectPath>::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default();
        let bearers: Vec<Props> = paths
            .iter()
            .filter_map(|p| all(conn, p.as_str(), "org.freedesktop.ModemManager1.Bearer"))
            .collect();
        let b =
            bearers.iter().find(|b| bool_of(b, "Connected") == Some(true)).or(bearers.first())?;
        let ip4 = dict_of(b, "Ip4Config").unwrap_or_default();
        let ip6 = dict_of(b, "Ip6Config").unwrap_or_default();
        let with_prefix = |ip: &Props| match (str_of(ip, "address"), u32_of(ip, "prefix")) {
            (Some(a), Some(p)) if !a.is_empty() => format!("{a}/{p}"),
            (Some(a), _) => a,
            _ => String::new(),
        };
        let stats = dict_of(b, "Stats").unwrap_or_default();
        // The interface's own counters: MM's stay at zero unless something has turned
        // its statistics polling on.
        let iface = str_of(b, "Interface").unwrap_or_default();
        let u64_of = |k: &str| {
            let sysfs = if k == "rx-bytes" { "rx_bytes" } else { "tx_bytes" };
            let counted =
                std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{sysfs}"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            stats.get(k).and_then(|v| u64::try_from(&**v).ok()).unwrap_or(0).max(counted)
        };
        Some(Bearer {
            connected: bool_of(b, "Connected").unwrap_or(false),
            iface: str_of(b, "Interface").unwrap_or_default(),
            apn: dict_of(b, "Properties").and_then(|p| str_of(&p, "apn")).unwrap_or_default(),
            ipv4: with_prefix(&ip4),
            ipv4_gateway: str_of(&ip4, "gateway").unwrap_or_default(),
            ipv6: with_prefix(&ip6),
            dns: ["dns1", "dns2", "dns3"]
                .iter()
                .flat_map(|k| [str_of(&ip4, k), str_of(&ip6, k)])
                .flatten()
                .filter(|d| !d.is_empty())
                .collect(),
            mtu: u32_of(&ip4, "mtu"),
            rx_bytes: u64_of("rx-bytes"),
            tx_bytes: u64_of("tx-bytes"),
            duration: stats.get("duration").and_then(|v| u32::try_from(&**v).ok()).unwrap_or(0),
        })
    }

    fn object_of(p: &Props, k: &str) -> Option<String> {
        match p.get(k).map(|v| &**v) {
            Some(Value::ObjectPath(o)) if o.as_str() != "/" => Some(o.to_string()),
            _ => None,
        }
    }

    fn strs_of(p: &Props, k: &str) -> Vec<String> {
        p.get(k).and_then(|v| Vec::<String>::try_from(v.try_clone().ok()?).ok()).unwrap_or_default()
    }

    fn str_of(p: &Props, k: &str) -> Option<String> {
        match p.get(k).map(|v| &**v) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        }
    }

    fn u32_of(p: &Props, k: &str) -> Option<u32> {
        p.get(k).and_then(|v| u32::try_from(&**v).ok())
    }

    fn bool_of(p: &Props, k: &str) -> Option<bool> {
        p.get(k).and_then(|v| bool::try_from(&**v).ok())
    }

    /// A value ModemManager has, or `None`: it reports a missing one as -3276.8,
    /// unfiltered, so anything that low is treated as not there.
    fn f64_of(p: &Props, k: &str) -> Option<f64> {
        p.get(k).and_then(|v| f64::try_from(&**v).ok()).filter(|v| v.is_finite() && *v > -1000.0)
    }

    fn dict_of(p: &Props, k: &str) -> Option<Props> {
        Props::try_from(p.get(k)?.try_clone().ok()?).ok()
    }
}
