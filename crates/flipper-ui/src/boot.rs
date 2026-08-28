//! Bootable profiles, for the boot menu.
//!
//! A profile is a btrfs subvolume the device can boot into. The list comes from
//! `list-profiles`, and which one is marked to boot next from the metadata
//! partition via `flipmeta`. Both are the tools the prototype's server shells out
//! to, and there is no kernel interface to read instead: the subvolume layout and
//! the boot marker are conventions of this image, not facts the kernel exposes.
//!
//! Nothing here writes. Renaming, cloning, deleting and factory-resetting a
//! profile are the destructive half of the boot menu and are deliberately absent
//! until they have somewhere safe to be tested.

use std::process::{Command, Stdio};

use crate::sysinfo::output;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    /// Subvolume name, e.g. `@Minimal` or `@Desktop__My-Games__`.
    pub name: String,
    /// The profile currently running.
    pub booted: bool,
    /// Subvolume id, which is also what the boot marker records.
    pub id: String,
    pub created: String,
    /// `now` for the booted one, `never` if it has not been, else a timestamp.
    pub last_used: String,
    pub parent: String,
    /// `origin_stock_name`, from which the base name and the icon are derived.
    pub origin: String,
    /// Marked to boot next, per the metadata partition.
    pub auto_boot: bool,
    /// What its filesystem is sitting on. Anything but `Internal` can be listed and
    /// booted, but never marked: see `stores`.
    pub medium: Medium,
    /// The partition it lives on, e.g. `/dev/mmcblk0p3`. Empty for the filesystem
    /// this system booted from, which every tool takes as its default.
    pub dev: String,
    /// The drive that partition is on, e.g. `/dev/sda`, and what to call it.
    pub disk: String,
    pub kind: &'static str,
}

/// The PATH to run a tool with: root's, as this system declares it.
///
/// A tool resolves programs of its own -- boot-profile calls kexec, the size walk
/// calls btrfs -- and those live in the sbin directories, which a login session's
/// PATH leaves out. A child inherits ours, so a boot started from flipctl died with
/// "kexec not installed (kexec-tools)" on a machine carrying /usr/sbin/kexec.
///
/// Read rather than written down: `ENV_SUPATH` in /etc/login.defs is where a system
/// states what root's PATH is, so it stays right when the system changes and a list
/// of directories here would not. None where nothing states one, which is the boot
/// menu image: the launcher sets the PATH there and inheriting it is correct.
///
/// Read once. A system does not move its programs mid-session, and this is asked
/// again for every tool that runs.
fn tool_path() -> Option<&'static str> {
    static PATH: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    PATH.get_or_init(|| env_supath(&std::fs::read_to_string("/etc/login.defs").ok()?))
        .as_deref()
}

/// `ENV_SUPATH` out of an /etc/login.defs, which states it as `ENV_SUPATH PATH=...`.
fn env_supath(defs: &str) -> Option<String> {
    defs.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix("ENV_SUPATH"))
        .filter_map(|value| value.trim().strip_prefix("PATH="))
        .map(str::to_string)
        .next()
}

/// One of the profile tools, ready to run.
///
/// Through sudo, unless we are already root. That is not an optimisation: the boot
/// menu image is an initramfs whose only user is root and which carries no sudo at
/// all, so going through it there fails every call with ENOENT. The failure is
/// silent by design here -- a tool that cannot answer means "unknown" -- so the
/// symptom was an empty profile list on a machine full of profiles.
fn tool(args: &[&str]) -> Command {
    // By absolute path where we can find one, so it does not matter what PATH the
    // program was started with.
    let program = which(args[0]).unwrap_or_else(|| args[0].into());
    let mut cmd = if unsafe { libc::geteuid() } == 0 {
        let mut cmd = Command::new(program);
        cmd.args(&args[1..]);
        cmd
    } else {
        let mut cmd = Command::new("sudo");
        cmd.arg(program);
        cmd.args(&args[1..]);
        cmd
    };
    if let Some(path) = tool_path() {
        cmd.env("PATH", path);
    }
    cmd
}

fn sudo(args: &[&str]) -> Option<String> {
    let out = tool(args)
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Whether this machine has a boot menu at all.
///
/// `list-profiles` is the Flipper's own helper; a machine without it has no btrfs
/// profiles to boot into, so the menu entry and the idle screen's profile line are
/// hidden rather than shown empty.
///
/// A lookup, not a run: answering by executing the helper would cost a process and a
/// sudo prompt on every screen that asks. Answered once and remembered, because a
/// helper does not appear halfway through a session and the caller asks per frame.
///
/// Our own PATH plus the administration directories, because the listing goes through
/// sudo and sudo resolves it against secure_path. A service's PATH routinely leaves
/// the sbin directories out, and the helper is installed in one of them, so looking
/// only where we would find it ourselves reports a machine with no profiles at all
/// while `sudo list-profiles` would have answered.
pub fn available() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| which("list-profiles").is_some())
}

/// Where a tool of ours is, or None.
///
/// PATH plus the administration directories, and the answer is a path rather than a
/// yes: whatever finds a tool has to be what runs it, or the two disagree. They did.
/// `available()` looked in the sbin directories while the invocation was a bare name
/// resolved through the child's PATH, which on Debian includes /usr/local/sbin and
/// under BusyBox init does not -- so the boot menu image found the tools, ran none of
/// them, and drew an empty list with nothing to say about it.
fn which(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    let admin = ["/usr/local/sbin", "/usr/sbin", "/sbin"].map(std::path::PathBuf::from);
    std::env::split_paths(&path)
        .chain(admin)
        .map(|dir| dir.join(name))
        .find(|p| p.is_file())
}

/// The partition type every filesystem of ours carries: the Discoverable Partitions
/// Spec's Linux root for arm64.
const ROOT_TYPE: &str = "b921b045-1df0-41c3-af44-4c6f280d3fae";

/// What a filesystem is sitting on.
///
/// Kept apart because they are not interchangeable to a person holding the device: a
/// profile on the machine's own storage is the machine, one on a card is something
/// they put in and can take out again. Each kind gets its own icon; until the design
/// has them, everything that is not internal borrows the card's.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Medium {
    /// The machine's own storage: UFS here, or soldered eMMC elsewhere.
    #[default]
    Internal,
    /// An SD card.
    Sd,
    /// A USB stick or disk.
    Usb,
    /// A SATA or NVMe drive.
    Ssd,
}

impl Medium {
    /// Which of the four, as the Slint row wants it: 0 internal, 1 card, 2 USB, 3 drive.
    pub fn as_i32(self) -> i32 {
        match self {
            Medium::Internal => 0,
            Medium::Sd => 1,
            Medium::Usb => 2,
            Medium::Ssd => 3,
        }
    }
}

/// A filesystem that can hold profiles.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Store {
    /// The partition, e.g. `/dev/sda3` or `/dev/mmcblk0p3`.
    pub dev: String,
    /// The whole drive it is a partition of, e.g. `/dev/sda`.
    pub disk: String,
    /// What it is sitting on.
    pub medium: Medium,
    /// What to call that on screen: UFS, eMMC, SD, USB, NVMe, SATA.
    pub kind: &'static str,
    /// The filesystem this system booted from.
    pub booted: bool,
}

/// What a block device is, from what sysfs says about it.
///
/// `lsblk` reports the chain of subsystems a device hangs off and its transport, both
/// read out of sysfs, and that is what tells these apart. Measured on the device: UFS
/// reports **no** transport at all and only its controller (`...ufshc...`) in the path,
/// while an SD card reports `mmc`. So neither the transport alone nor the name alone is
/// enough, and the removable-media flag is worse than useless: an SD card sets `RM=0`.
///
/// mmc covers both a card and soldered eMMC, which are opposite answers to "is this the
/// machine or something I put in", so the card's own `type` file decides: `SD` for a
/// card, `MMC` for eMMC.
/// What it is, and what to call it on screen.
///
/// No lsblk column ever says UFS: the internal storage looks like plain SCSI on a
/// platform bus, model `BWU2A0516B064G`, with an empty transport. Its controller is
/// the only thing that names it, so the answer comes from the device's sysfs path.
fn classify(subsystems: &str, tran: &str, disk: &str, hotplug: bool) -> (Medium, &'static str) {
    let has = |what: &str| subsystems.split(':').any(|s| s == what);
    if has("usb") {
        return (Medium::Usb, "USB");
    }
    if tran == "nvme" || has("nvme") {
        return (Medium::Ssd, "NVMe");
    }
    if tran == "sata" || has("ata") {
        return (Medium::Ssd, "SATA");
    }
    if tran == "mmc" || has("mmc") {
        // A card and soldered eMMC are opposite answers to "is this the machine or
        // something I put in", and mmc covers both, so the card's own type decides.
        let kind = std::fs::read_to_string(format!("/sys/block/{disk}/device/type"))
            .unwrap_or_default()
            .trim()
            .to_string();
        return if kind == "MMC" {
            (Medium::Internal, "eMMC")
        } else {
            (Medium::Sd, "SD")
        };
    }
    // The controller, for the buses lsblk does not name.
    let path = std::fs::canonicalize(format!("/sys/block/{disk}"))
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    if path.contains("ufshc") || path.contains("ufs") {
        return (Medium::Internal, "UFS");
    }
    // Nothing recognisable. Hotplug is the last clue: something that can be unplugged
    // is not the machine's own storage, and a USB disk is the likeliest of those.
    if hotplug {
        (Medium::Usb, "USB")
    } else {
        (Medium::Internal, "disk")
    }
}

/// Every filesystem of ours the machine can see, the booted one first.
///
/// Found by partition type rather than by filesystem label, because the label
/// cannot tell them apart: a card written with our own image carries the same
/// `flipperos` label as the internal storage, on a partition of the same type. What
/// separates them is hotplug. Note that the removable-media flag does not: an SD
/// card reports `RM=0` and `HOTPLUG=1`, which is the opposite way round to what the
/// name suggests.
pub fn stores() -> Vec<Store> {
    let Some(listing) = output(&[
        "lsblk", "-P", "-o", "PATH,PARTTYPE,FSTYPE,HOTPLUG,SUBSYSTEMS,TRAN,PKNAME",
    ]) else {
        return Vec::new();
    };
    // The booted device, without the subvolume that follows it in brackets.
    let booted = output(&["findmnt", "-no", "SOURCE", "/"])
        .map(|s| s.trim().split('[').next().unwrap_or("").to_string())
        .unwrap_or_default();

    let mut out: Vec<Store> = Vec::new();
    for line in listing.lines() {
        let val = |key: &str| -> String {
            line.split(' ')
                .find_map(|pair| pair.strip_prefix(&format!("{key}=")))
                .map(|v| v.trim_matches('"').to_string())
                .unwrap_or_default()
        };
        if !val("PARTTYPE").eq_ignore_ascii_case(ROOT_TYPE) || val("FSTYPE") != "btrfs" {
            continue;
        }
        let dev = val("PATH");
        if dev.is_empty() {
            continue;
        }
        let parent = val("PKNAME");
        let (medium, kind) = classify(
            &val("SUBSYSTEMS"),
            &val("TRAN"),
            &parent,
            val("HOTPLUG") == "1",
        );
        out.push(Store {
            booted: dev == booted,
            disk: if parent.is_empty() { dev.clone() } else { format!("/dev/{parent}") },
            medium,
            kind,
            dev,
        });
    }
    // The booted filesystem first, then the machine's own, then everything plugged in:
    // the list on screen reads outward from where you are.
    out.sort_by_key(|s| (!s.booted, s.medium.as_i32(), s.dev.clone()));
    out
}

/// The bootable profiles, in the order `list-profiles` reports them.
///
/// Columns are positional and separated by runs of two or more spaces:
///
/// ```text
/// NAME [<- booted] KIND ID CREATED LAST_USED RO PARENT ORIGIN
/// ```
///
/// The booted marker sits in its own column and shifts everything after it,
/// which is why the offset is computed rather than fixed. Anything that is not a
/// `profile` is skipped: `_old` backups are leftovers, not somewhere to boot.
pub fn profiles() -> Vec<Profile> {
    // No check that the tools are there: they ship in the boot menu image, and an
    // image without them has nothing to do. flipctl still checks, to hide its Boot
    // row on a machine with no profile tools at all.
    //
    // Every filesystem of ours, not just the booted one, so a card's profiles are
    // offered alongside the machine's own.
    //
    // The marker is only applied to the machine's own storage. Ids are per
    // filesystem and they collide: the card in this device holds ids 263 to 271 and
    // the internal storage 264 to 272, so a bare id says nothing about which
    // filesystem it means. A profile anywhere else therefore never wears the heart,
    // and never can, which is also why Auto Start is not offered for one.
    let stores = stores();
    if stores.is_empty() {
        let marked = marker("");
        // No lsblk, or nothing recognisable: ask about the booted filesystem alone,
        // which is what this did before there was anything else to ask about.
        let Some(listing) = sudo(&["list-profiles"]) else {
            eprintln!("boot           list-profiles answered nothing; no profiles to show");
            return Vec::new();
        };
        return parse_listing(&listing, &marked, Medium::Internal, "", "", "disk");
    }

    // One thread per drive, and the marker read alongside them.
    //
    // A listing mounts that filesystem's top level and walks every subvolume on it,
    // which is seconds of I/O per drive, and the drives are independent: nothing is
    // shared and the read-only tools take no lock, only the mutating ones do. Done in
    // turn, the menu waits for the sum; done at once, for the slowest. The order of
    // the results is the order of `stores`, so the list on screen does not depend on
    // which drive answered first.
    let (marked, listings) = std::thread::scope(|scope| {
        let mark = scope.spawn(|| marker(&internal_dev(&stores)));
        let reads: Vec<_> = stores
            .iter()
            .map(|store| {
                scope.spawn(move || {
                    if store.booted {
                        sudo(&["list-profiles"])
                    } else {
                        sudo(&["list-profiles", "-d", &store.dev])
                    }
                })
            })
            .collect();
        let listings: Vec<Option<String>> =
            reads.into_iter().map(|r| r.join().unwrap_or(None)).collect();
        (mark.join().unwrap_or_default(), listings)
    });

    let mut out = Vec::new();
    for (store, listing) in stores.iter().zip(listings) {
        let Some(listing) = listing else {
            // A drive that answers nothing is not the same as a drive with no
            // profiles, and only the log can tell the two apart afterwards.
            eprintln!(
                "boot           list-profiles answered nothing for {}",
                if store.booted { "the booted filesystem" } else { store.dev.as_str() }
            );
            continue;
        };
        let internal = store.medium == Medium::Internal;
        let marker = if internal { marked.as_str() } else { "" };
        // The booted filesystem is every tool's default, so it needs no device: this
        // way a profile carries one only when saying which is the point.
        let dev = if store.booted { "" } else { store.dev.as_str() };
        out.extend(parse_listing(&listing, marker, store.medium, dev, &store.disk, store.kind));
    }
    out
}

/// The listing, parsed. Split out so it can be tested against real output.
pub fn parse_listing(
    listing: &str,
    marked: &str,
    medium: Medium,
    dev: &str,
    disk: &str,
    kind: &'static str,
) -> Vec<Profile> {
    let mut out = Vec::new();
    let mut started = false;
    for line in listing.lines() {
        let line = line.trim();
        if !started {
            started = line.starts_with("NAME");
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split("  ").filter(|c| !c.trim().is_empty()).map(str::trim).collect();
        if cols.len() < 4 {
            continue;
        }
        let booted = cols.get(1).is_some_and(|c| *c == "<- booted");
        let base = if booted { 2 } else { 1 };
        if cols.get(base).is_none_or(|k| *k != "profile") {
            continue;
        }
        let id = cols.get(base + 1).unwrap_or(&"").to_string();
        // PARENT and ORIGIN carry the stock's id as "name (id)".
        let strip_id = |s: &str| s.split(" (").next().unwrap_or(s).trim().to_string();
        let dash_to_empty = |s: String| if s == "-" { String::new() } else { s };
        out.push(Profile {
            auto_boot: !marked.is_empty() && id == marked,
            medium,
            dev: dev.to_string(),
            disk: disk.to_string(),
            kind,
            name: cols[0].to_string(),
            booted,
            created: cols.get(base + 2).unwrap_or(&"").to_string(),
            last_used: cols.get(base + 3).unwrap_or(&"").to_string(),
            parent: dash_to_empty(strip_id(cols.get(base + 5).unwrap_or(&""))),
            origin: dash_to_empty(strip_id(cols.get(base + 6).unwrap_or(&""))),
            id,
        });
    }
    out
}

/// The base name inside an origin, e.g. `@Desktop_968_stock` gives `Desktop`.
pub fn origin_base(origin: &str) -> String {
    let trimmed = origin.trim_start_matches('@');
    let Some(rest) = trimmed.strip_suffix("_stock") else {
        return String::new();
    };
    // What remains is `<base>_<build>`; the build is all digits.
    match rest.rsplit_once('_') {
        Some((base, build)) if !build.is_empty() && build.chars().all(|c| c.is_ascii_digit()) => {
            base.to_string()
        }
        _ => String::new(),
    }
}

/// What the row shows.
///
/// One a user derived is named `@Base__label__` and shows its label in brackets,
/// so a clone is visibly a clone. Anything else shows its plain name.
///
/// A dash reads as a space either way. boot_menu.js only does that inside the
/// brackets, which leaves an image called `@No-Graphics` reading as a subvolume
/// name rather than as a name; every profile is spelled the same way here.
pub fn display_name(name: &str) -> String {
    let raw = name.trim_start_matches('@');
    match raw.split_once("__") {
        Some((_, rest)) => match rest.strip_suffix("__") {
            Some(label) => format!("[{}]", label.replace('-', " ")),
            None => raw.replace('-', " "),
        },
        None => raw.replace('-', " "),
    }
}

/// Which icon a profile takes, keyed off its origin's base name and falling back
/// to its own.
pub fn icon_key(name: &str, origin: &str) -> &'static str {
    let base = origin_base(origin);
    for candidate in [base.as_str(), name] {
        let n = candidate.to_lowercase();
        if n.contains("graphics") {
            return "graphics";
        }
        if n.contains("minimal") {
            return "minimal";
        }
        if n.contains("desktop") {
            return "desktop";
        }
        if n.contains("router") {
            return "router";
        }
        if n.contains("media") || n.contains("tv") {
            return "media";
        }
    }
    ""
}

/// The status text: how long ago the profile was last booted.
///
/// Singular units, matching the mockup. Two sentinels: nothing at all for a
/// profile never booted, and `Running` for the one running now, because "used 0
/// minutes ago" is a strange way to say that.
pub fn used_ago(last_used: &str, now: std::time::SystemTime) -> String {
    match last_used {
        "" | "never" => return String::new(),
        "now" => return "Running".into(),
        _ => {}
    }
    let Some(then) = parse_stamp(last_used) else {
        // Not a timestamp: show it verbatim rather than inventing one.
        return last_used.to_string();
    };
    let now_secs = now
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64);
    let secs = (now_secs - then).max(0);
    let plural = |n: i64, unit: &str| {
        format!("Used {n} {unit}{} ago", if n == 1 { "" } else { "s" })
    };
    if secs < 60 {
        return "Used just now".into();
    }
    let mins = secs / 60;
    if mins < 60 {
        return plural(mins, "min");
    }
    let hours = mins / 60;
    if hours < 24 {
        return plural(hours, "hour");
    }
    let days = hours / 24;
    if days < 30 {
        return plural(days, "day");
    }
    let months = days / 30;
    if months < 12 {
        return plural(months, "month");
    }
    plural(days / 365, "year")
}

/// Exposed for tests, which need a fixed instant to measure against.
pub fn parse_stamp_for_test(s: &str) -> Option<i64> {
    parse_stamp(s)
}

/// `YYYY-MM-DD HH:MM:SS` as a Unix timestamp, treated as UTC.
///
/// Written out rather than pulled from a crate: the only timestamps here come
/// from one tool in one format, and the comparison is against wall-clock seconds.
fn parse_stamp(s: &str) -> Option<i64> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.split('-');
    let (y, m, day) = (
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
        d.next()?.parse::<i64>().ok()?,
    );
    let mut t = time.split(':');
    let (hh, mm, ss) = (
        t.next()?.parse::<i64>().ok()?,
        t.next()?.parse::<i64>().ok()?,
        t.next().and_then(|v| v.parse::<i64>().ok()).unwrap_or(0),
    );
    Some(days_from_civil(y, m, day) * 86400 + hh * 3600 + mm * 60 + ss)
}

/// Howard Hinnant's civil-to-days conversion, which is exact and short.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

// ── Sizes and details, for the Info popup ──────────────────────────────────

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Space {
    /// Freed by deleting this subvolume alone, uncompressed.
    pub unique: String,
    /// Real on-disk size, compressed. Counts shared extents, so not additive.
    pub referenced: String,
    /// Apparent size, uncompressed.
    pub total: String,
}

/// Disk space for one subvolume.
///
/// `btrfs-show-space -q <@subvol>` prints just that subvolume, as
/// `TOTAL=.. UNIQUE=..`, and takes a few seconds. Without the name it measures
/// every subvolume on the filesystem, which is 19 of them here and over a minute:
/// that is the whole-filesystem report, not this.
///
/// `-q` skips the compsize walk that produces REFERENCED, which is the expensive
/// half. The popup only labels one number "Size", so the extra walk buys nothing.
///
/// Sizes are the tool's own strings ("3.5GB") and are never reformatted, so every
/// view of the same number agrees.
///
/// `dev` names the filesystem the profile is on, as `boot_now` does and for the same
/// reason: the tool finds its own device from the mounted root, and there is no
/// mounted root of ours in an initramfs, nor the right one for a profile on a card.
pub fn space(dev: &str, name: &str) -> Option<Space> {
    if !valid_name(name) {
        return None;
    }
    let mut args: Vec<&str> = vec!["btrfs-show-space", "-q"];
    if !dev.is_empty() {
        args.push("-d");
        args.push(dev);
    }
    args.push(name);
    parse_space(&sudo(&args)?)
}

/// The `KEY=value` pairs `btrfs-show-space <@subvol>` prints.
pub fn parse_space(out: &str) -> Option<Space> {
    let field = |key: &str| {
        out.split_whitespace()
            .find_map(|tok| tok.strip_prefix(key))
            .map(str::to_string)
    };
    let total = field("TOTAL=")?;
    Some(Space {
        unique: field("UNIQUE=").unwrap_or_default(),
        // Absent under -q, which is the mode this uses.
        referenced: field("REFERENCED=").unwrap_or_default(),
        total,
    })
}

/// Device tree overlays a profile applies at boot.
///
/// Read from the profile's BLS entry, not from a directory. The overlays a profile
/// actually gets are whatever its boot entry's `devicetree-overlay` line names, and
/// that is the only place the answer exists: the files live inside the profile's own
/// subvolume, which is not mounted unless it is the one running.
///
/// The entry is found by its `options` line carrying `rootflags=subvol=<name>`.
/// Entries are sorted and the last match wins, which is the newest kernel, matching
/// the order the bootloader itself would pick.
///
/// System and user overlays are split by path: a drop-in under /etc/kernel/dtbo is
/// something someone added, anything else ships with the image.
pub fn dtbo(dev: &str, subvol: &str) -> (Vec<String>, Vec<String>) {
    if dev.is_empty() {
        return dtbo_in(std::path::Path::new(BOOTED_ENTRIES), subvol);
    }
    match TopLevel::ro(dev) {
        Some(top) => dtbo_in(&top.path.join("boot/loader/entries"), subvol),
        None => (Vec::new(), Vec::new()),
    }
}

/// The entries of the filesystem this system booted from, shared by every profile on
/// it. `Profile::dev` is empty for exactly those, which is what selects this path.
const BOOTED_ENTRIES: &str = "/boot/loader/entries";

/// A name for one mount, which no other mount of this process will use.
///
/// The drives are read one thread each, and every one of them mounts a filesystem to
/// read its entries. Named per process, as this was, they all mount over the same
/// directory: the second mount stacks on the first, so a thread reads whatever
/// filesystem is on top rather than the one it asked for, and which thread that is
/// depends on the order two mounts land in.
///
/// What that looked like: two drives, and each pass one of them reported its entries
/// while the other reported none, swapping between passes. A profile marked to boot
/// by itself on the drive that came back empty was then not marked at all -- no
/// heart on its row, and nothing for the countdown to boot -- until the list was
/// read again and the race fell the other way.
///
/// The device's own name is in here because it makes the mount legible in
/// /proc/mounts while it exists, and the counter because the same device can be
/// mounted twice at once: the marker read goes alongside the listings, and on the
/// internal drive that is the same filesystem twice.
fn mount_point(dev: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let leaf = dev.rsplit('/').find(|part| !part.is_empty()).unwrap_or("dev");
    format!(
        "flipctl-entries.{}.{leaf}.{}",
        std::process::id(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// A read-only top-level mount of another filesystem, unmounted when dropped.
///
/// A profile's boot entry lives on the profile's own drive: a card's entry names
/// kernels only that card has, and an initramfs has no /boot of ours at all. Read-only
/// because reading is all this is for, and subvolid=5 because that is where the
/// entries are, as the tools mount it.
///
/// Drop does the unmounting so a read that returns early, or panics, cannot leave the
/// filesystem mounted: this runs while a popup is open, and the drive it holds is one
/// someone may pull out.
struct TopLevel {
    path: std::path::PathBuf,
}

impl TopLevel {
    fn ro(dev: &str) -> Option<Self> {
        // /run first, as the tools do: it is tmpfs, root-only, and nobody sweeps it by
        // glob. Whether it can be written is not a question the mode bits answer -- an
        // unprivileged flipctl cannot write root's 0755 /run -- so try and see.
        let name = mount_point(dev);
        let path = ["/run", "/tmp"]
            .into_iter()
            .map(|base| std::path::Path::new(base).join(&name))
            .find(|path| std::fs::create_dir_all(path).is_ok())?;
        let mount = TopLevel { path };
        let target = mount.path.to_str()?;
        match run(&["mount", "-t", "btrfs", "-o", "ro,subvolid=5", dev, target]) {
            Ok(()) => Some(mount),
            Err(_) => None,
        }
    }
}

impl Drop for TopLevel {
    fn drop(&mut self) {
        if let Some(target) = self.path.to_str() {
            let _ = run(&["umount", target]);
        }
        let _ = std::fs::remove_dir(&self.path);
    }
}

/// `dtbo`, against a given entries directory. Split out so it can be tested.
pub fn dtbo_in(dir: &std::path::Path, subvol: &str) -> (Vec<String>, Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut confs: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "conf"))
        .collect();
    confs.sort();

    let want = format!("rootflags=subvol={subvol}");
    let mut chosen = None;
    for path in confs {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let matches = text.lines().filter(|l| l.starts_with("options")).any(|l| {
            // The option must end at a word boundary, or @Minimal would match
            // @Minimal__clone__.
            l.split_whitespace().any(|opt| opt == want)
        });
        if matches {
            chosen = Some(text);
        }
    }
    let Some(text) = chosen else {
        return (Vec::new(), Vec::new());
    };

    let mut system = Vec::new();
    let mut user = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("devicetree-overlay") else {
            continue;
        };
        for path in rest.split_whitespace() {
            let base = path.rsplit('/').next().unwrap_or(path).to_string();
            if path.contains("/etc/kernel/dtbo/") {
                user.push(base);
            } else {
                system.push(base);
            }
        }
    }
    (system, user)
}

// ── Actions ────────────────────────────────────────────────────────────────

/// A subvolume name safe to hand to the tools.
///
/// The prototype's server checks the same shape because it builds a shell command
/// out of it. Here the arguments go straight to execve with no shell, so this is
/// not about quoting: it is about not asking a tool to operate on something that
/// cannot be a profile.
fn valid_name(name: &str) -> bool {
    name.len() > 1
        && name.starts_with('@')
        && name[1..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn run(args: &[&str]) -> Result<(), String> {
    let out = tool(args)
        .output()
        .map_err(|e| format!("cannot run {}: {e}", args[0]))?;
    if out.status.success() {
        // A tool that succeeded can still have something to say, and it says it on
        // stderr: boot-profile warns there when it has to boot a profile without
        // this machine's memory node. Swallowing that is how a broken graft sat through
        // several hangs, each looking exactly like the bug it was meant to fix.
        for line in String::from_utf8_lossy(&out.stderr).lines() {
            if !line.trim().is_empty() {
                eprintln!("tool           {}", line.trim());
            }
        }
        return Ok(());
    }
    // The tools put the useful line last, on either stream.
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
    // A tool's own last line is its error and goes on screen as it stands. A tool that
    // fails silently leaves only its exit status, which is a fact for the log and not
    // for a 256x144 panel: the screen says what happened, the log says what to chase.
    // Reporting nothing at all is the one thing that must not happen -- a bare
    // "failed" on both is how a stack overflow in a shell shim went three rounds
    // undiagnosed.
    if let Some(line) = text.lines().rev().find(|l| !l.trim().is_empty()) {
        return Err(line.trim().to_string());
    }
    use std::os::unix::process::ExitStatusExt;
    match (out.status.code(), out.status.signal()) {
        (Some(code), _) => eprintln!("tool           {} exited {code}, saying nothing", args[0]),
        (_, Some(sig)) => eprintln!("tool           {} killed by signal {sig}", args[0]),
        _ => eprintln!("tool           {} died without a status", args[0]),
    }
    Err("the tool gave no answer".into())
}

/// Boot a profile now, by kexec, and do not come back.
///
/// `boot-profile` reads that profile's own BLS entry for the kernel, the initrd and
/// the command line, assembles its device tree (the running profile's is the live
/// one; any other profile's is the board base plus the entry's overlays through
/// fdtoverlay), loads all of it with kexec and hands over. So a profile boots with
/// its own kernel and its own tree, which is what mounting its root and switching
/// into it could not do.
///
/// U-Boot does not read the auto-boot marker and has no timeout of its own, so this
/// is the only thing that acts on a choice: the marker says which profile the menu
/// boots when nobody presses anything, and this is what boots it.
///
/// Returns `Ok(false)` only when it failed to leave, which on a real boot cannot
/// happen: the machine is already on its way out. `Ok(true)` is the dry run, which
/// loaded the image, unloaded it and stayed.
///
/// `FLIPCTL_BOOT_DRY_RUN=1` is that dry run: it proves the entry resolves, the kernel
/// and initrd are there and the device tree assembles, without losing the session
/// that asked. The caller has to say so on screen, because nothing else will.
///
/// A profile on another filesystem is booted with `-d`, which is what makes it that
/// card's profile rather than the one of the same name on the internal storage. Names
/// and subvolume ids both repeat across filesystems, so the device is the only thing
/// that distinguishes them, and boot-profile reads that device's own BLS entries for
/// the kernel, initrd and device tree it names.
pub fn boot_now(p: &Profile) -> Result<bool, String> {
    let name = p.name.as_str();
    if !valid_name(name) {
        return Err("invalid name".into());
    }
    let dry = std::env::var_os("FLIPCTL_BOOT_DRY_RUN").is_some();
    let mut args: Vec<&str> = vec!["boot-profile"];
    if dry {
        args.push("--dry-run");
    }
    if !p.dev.is_empty() {
        args.push("-d");
        args.push(&p.dev);
    }
    args.push(name);
    run(&args)?;
    Ok(dry)
}

/// The drive whose metadata partition holds the marker: the machine's own storage.
///
/// Empty when that is the filesystem this booted from, which every tool takes as its
/// default, or when there is no internal store to name.
fn internal_dev(stores: &[Store]) -> String {
    stores
        .iter()
        .find(|s| s.medium == Medium::Internal && !s.booted)
        .map(|s| s.dev.clone())
        .unwrap_or_default()
}

/// The auto-boot marker: a subvolume id, or empty when nothing is marked.
///
/// `flipmeta` finds the metadata partition by its GPT type, and a card written with
/// our own image carries one too, so with a card in there are two and it refuses to
/// choose. The marker only ever means the machine's own storage, so that is the drive
/// named. Absent or unparseable reads as nothing marked.
fn marker(dev: &str) -> String {
    let mut args: Vec<&str> = vec!["flipmeta"];
    if !dev.is_empty() {
        args.push("-d");
        args.push(dev);
    }
    args.extend(["get", "boot"]);
    sudo(&args)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or_default()
}

/// Point the auto-boot marker at a profile.
pub fn set_auto_start(dev: &str, id: &str) -> Result<(), String> {
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
        return Err("no subvolume id".into());
    }
    run(&on_dev("flipmeta", dev, &["set", "boot", id]))
}

/// A tool command against one filesystem, with the drive named unless it is the booted one.
///
/// `Profile::dev` is empty for the filesystem this system booted from, which every tool
/// takes as its default. Anything else has to be named, or the tool acts on the booted
/// drive instead: profile names repeat across drives, so deleting a card's @Minimal
/// would delete the machine's own. In an initramfs there is no default to fall back on
/// either -- the root is `rootfs`, which is not a block device, and the tool refuses.
fn on_dev<'a>(tool: &'a str, dev: &'a str, rest: &[&'a str]) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec![tool];
    if !dev.is_empty() {
        args.push("-d");
        args.push(dev);
    }
    args.extend(rest.iter().copied());
    args
}

/// Copy a profile under a new name, on the drive the profile is on.
pub fn clone(dev: &str, source: &str, dest: &str) -> Result<(), String> {
    if !valid_name(source) || !valid_name(dest) {
        return Err("invalid name".into());
    }
    run(&on_dev("create-profile", dev, &["-y", source, dest]))
}

/// Delete a profile, clearing the auto-boot marker if it pointed at it.
///
/// Order matters: the id has to be read before the subvolume goes, or there is
/// nothing left to compare the marker against and autoboot would be left pointing
/// at something deleted.
pub fn delete(dev: &str, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err("invalid name".into());
    }
    let marked = profiles()
        .into_iter()
        .find(|p| p.name == name && p.auto_boot && p.dev == dev);
    run(&on_dev("delete-profile", dev, &["-y", name]))?;
    if let Some(p) = marked {
        run(&on_dev("flipmeta", &p.dev, &["del", "boot"]))?;
    }
    Ok(())
}

/// Remove the `_old_<stamp>` copy a factory reset of the booted profile left behind.
///
/// `--no-keep` cannot drop the live root, so `create-profile` moves it aside and the
/// machine reboots into the fresh one. Once that has happened the copy is dead weight,
/// and this is the first moment anything can say so, which is why it runs at startup
/// rather than as part of the reset.
///
/// Only the booted profile's own copies go: a leftover belonging to any other profile
/// may still be someone's way back, and a hand-made backup is not ours to delete. The
/// boot menu never calls this, having no booted profile of ours to reason about.
pub fn reap_old_backups() {
    if !available() {
        return;
    }
    let Some(booted) = sudo(&["findmnt", "-no", "FSROOT", "/"]) else {
        return;
    };
    let booted = booted.trim().trim_start_matches('/');
    if booted.is_empty() || !valid_name(booted) {
        return;
    }
    // Named rather than left to the tools' default. The booted profile is on the
    // booted filesystem by definition, but a leftover is deleted here and a delete
    // aimed at a filesystem by assumption is not one to leave to assumption: profile
    // names repeat across drives, so a default that ever went elsewhere would delete
    // another drive's subvolume of the same name.
    let dev = stores()
        .into_iter()
        .find(|s| s.booted)
        .map(|s| s.dev)
        .unwrap_or_default();
    let on_dev = |rest: &[&str]| -> Vec<String> {
        let mut args: Vec<String> = vec![rest[0].to_string()];
        if !dev.is_empty() {
            args.push("-d".into());
            args.push(dev.clone());
        }
        args.extend(rest[1..].iter().map(|a| a.to_string()));
        args
    };
    let list = on_dev(&["list-profiles"]);
    let Some(listing) = sudo(&list.iter().map(String::as_str).collect::<Vec<_>>()) else {
        return;
    };
    for line in listing.lines() {
        let name = line.split("  ").next().unwrap_or("").trim();
        if !is_old_backup(name, booted) {
            continue;
        }
        let del = on_dev(&["delete-profile", "-y", name]);
        match run(&del.iter().map(String::as_str).collect::<Vec<_>>()) {
            Ok(()) => eprintln!("boot           reaped leftover backup {name}"),
            Err(e) => eprintln!("boot           could not reap {name}: {e}"),
        }
    }
}

/// Whether `name` is a `<booted>_old_<stamp>` copy, with create-profile's own stamp
/// shape: `YYYY-MM-DD_HH-MM-SS`, plus the `_<n>` it adds when two land in one second.
///
/// Spelled out rather than pattern-matched loosely, because the answer decides what
/// gets deleted: `@Desktop_old_notes` is someone's subvolume, not a leftover.
pub fn is_old_backup(name: &str, booted: &str) -> bool {
    let Some(stamp) = name
        .strip_prefix(booted)
        .and_then(|rest| rest.strip_prefix("_old_"))
    else {
        return false;
    };
    let (stamp, extra) = match stamp.split_once('_') {
        // The date, then the time, then anything more is the collision counter.
        Some((date, rest)) => match rest.split_once('_') {
            Some((time, n)) => (format!("{date}_{time}"), Some(n)),
            None => (format!("{date}_{rest}"), None),
        },
        None => return false,
    };
    if extra.is_some_and(|n| n.is_empty() || !n.chars().all(|c| c.is_ascii_digit())) {
        return false;
    }
    // YYYY-MM-DD_HH-MM-SS: digits where digits belong, separators where they belong.
    let shape = "dddd-dd-dd_dd-dd-dd";
    stamp.len() == shape.len()
        && stamp.chars().zip(shape.chars()).all(|(c, s)| match s {
            'd' => c.is_ascii_digit(),
            sep => c == sep,
        })
}

/// Replace a profile with a fresh copy of the stock it came from.
///
/// `create-profile --no-keep` moves the old copy aside and gives the new one a
/// fresh subvolume id, so a marker that pointed at the old id has to be moved or
/// autoboot would land on the stale `_old` copy.
///
/// Returns true when the profile reset was the running one, which means the
/// caller has to reboot: the root now mounted is the copy that was moved aside.
pub fn factory_reset(dev: &str, name: &str, origin: &str) -> Result<bool, String> {
    if !valid_name(name) || !valid_name(origin) {
        return Err("invalid name".into());
    }
    let before = profiles();
    let was_marked = before
        .iter()
        .find(|p| p.name == name)
        .is_some_and(|p| p.auto_boot);
    let booted = before
        .iter()
        .find(|p| p.name == name)
        .is_some_and(|p| p.booted);

    run(&on_dev("create-profile", dev, &["-y", "--no-keep", origin, name]))?;

    if was_marked {
        if let Some(fresh) = profiles().into_iter().find(|p| p.name == name) {
            if !fresh.id.is_empty() {
                set_auto_start(&fresh.dev, &fresh.id)?;
            }
        }
    }
    Ok(booted)
}

/// Rename a profile's label, on the drive the profile is on.
pub fn rename(dev: &str, name: &str, dest: &str) -> Result<(), String> {
    if !valid_name(name) || !valid_name(dest) {
        return Err("invalid name".into());
    }
    run(&on_dev("rename-profile", dev, &[name, dest]))
}

/// The destination name a clone of `p` should take.
///
/// `@Base__label__`, with the label derived from the source and a number appended
/// if that is taken, so cloning twice does not fail on a name collision.
pub fn clone_dest(p: &Profile, existing: &[Profile]) -> String {
    let base = {
        let b = origin_base(&p.origin);
        if b.is_empty() {
            p.name.trim_start_matches('@').to_string()
        } else {
            b
        }
    };
    let raw = p.name.trim_start_matches('@');
    let src = match raw.split_once("__") {
        Some((_, rest)) => rest.trim_end_matches('_').to_string(),
        None => raw.to_string(),
    };
    let safe: String = src
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' })
        .collect();
    let taken = |n: &str| existing.iter().any(|e| e.name == n);
    let first = format!("@{base}__{safe}-clone__");
    if !taken(&first) {
        return first;
    }
    (2..)
        .map(|n| format!("@{base}__{safe}-clone-{n}__"))
        .find(|c| !taken(c))
        .unwrap_or(first)
}

/// Whether a profile is one a user made, which is the `@Base__label__` shape the
/// list shows in brackets.
///
/// Everything else is an image: a factory profile, whose name is what ties it to
/// its stock, or a subvolume made outside this menu. Neither may be renamed or
/// deleted.
pub fn is_user_profile(name: &str) -> bool {
    label_of(name).is_some()
}

/// The label inside a `@Base__label__` name, if it has one.
fn label_of(name: &str) -> Option<&str> {
    let raw = name.trim_start_matches('@');
    let (_, rest) = raw.split_once("__")?;
    let label = rest.strip_suffix("__")?;
    (!label.is_empty()).then_some(label)
}

/// Why the name being typed cannot be used, or empty when it can.
///
/// Shown under the field and used to gate saving, so both have one answer. A name
/// that resolves to what the profile is already called is allowed here and refused
/// later: retyping the same name is not an error, it just is not a rename.
pub fn rename_warning(text: &str, profile: &Profile, existing: &[Profile]) -> String {
    let dest = rename_dest(profile, text);
    if dest.is_empty() {
        return "Name required".into();
    }
    if dest != profile.name && existing.iter().any(|p| p.name == dest) {
        return "Name already exists".into();
    }
    String::new()
}

/// The label to seed the Rename field with: dashes read as spaces, and a name
/// with no label offered whole.
pub fn profile_label(name: &str) -> String {
    match label_of(name) {
        Some(label) => label.replace('-', " "),
        None => name.trim_start_matches('@').to_string(),
    }
}

/// Free text to a subvolume label: spaces become dashes and anything else that is
/// not a letter, a digit or a dash is dropped.
pub fn encode_label(text: &str) -> String {
    let collapsed: String = text
        .trim()
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect();
    // A run of spaces became a run of dashes, and leading or trailing dashes are
    // not part of a name.
    let mut out = String::new();
    for c in collapsed.chars() {
        if c == '-' && out.ends_with('-') {
            continue;
        }
        out.push(c);
    }
    out.trim_matches('-').to_string()
}

/// The name a rename to `text` would produce, or empty if the text carries no
/// usable label.
pub fn rename_dest(p: &Profile, text: &str) -> String {
    let label = encode_label(text);
    if label.is_empty() {
        return String::new();
    }
    let base = {
        let b = origin_base(&p.origin);
        if b.is_empty() {
            p.name.trim_start_matches('@').split("__").next().unwrap_or("").to_string()
        } else {
            b
        }
    };
    format!("@{base}__{label}__")
}

/// The actions offered for a profile.
///
/// Rename and Delete are only offered on a profile a user made. The booted one
/// cannot be deleted either, because `delete-profile` refuses it and there is no
/// point offering what will fail.
pub fn edit_actions(p: &Profile) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = if is_user_profile(&p.name) {
        vec!["Rename", "Clone", "Factory Reset", "Delete", "Auto Start"]
    } else {
        vec!["Clone", "Factory Reset", "Auto Start"]
    };
    if p.booted {
        out.retain(|a| *a != "Delete");
    }
    // Only the machine's own storage can be marked: the marker is a subvolume id, and
    // ids are only meaningful inside the filesystem that issued them.
    if p.medium != Medium::Internal {
        out.retain(|a| *a != "Auto Start");
    }
    out
}

/// The Info popup's "Last used" line.
///
/// Longer than the row's version on purpose: the row has to fit beside a name, so
/// it says "Used 3 hours ago", while the popup has the width to give the relative
/// time and the timestamp it came from.
pub fn info_last_used(last_used: &str, now: std::time::SystemTime) -> String {
    match last_used {
        "now" => return "Running".into(),
        "" | "never" => return "never".into(),
        _ => {}
    }
    let rel = used_ago(last_used, now);
    match rel.strip_prefix("Used ") {
        Some(r) => format!("{r} ({last_used})"),
        None => last_used.to_string(),
    }
}

/// A size string split into its number and unit, for the popup's fixed slots.
///
/// The tools already format these ("3.5GB"), and reformatting would disagree with
/// what every other view of the same number says.
pub fn size_parts(s: &str) -> (String, String) {
    if s.is_empty() {
        return ("?".into(), String::new());
    }
    let split = s
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    if num.is_empty() {
        (s.to_string(), String::new())
    } else {
        (num.to_string(), unit.trim().to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::env_supath;

    /// Both samples are real files: Debian trixie on the device, and the Arch host.
    /// The separator is a tab in both, and the value carries the PATH= prefix.
    #[test]
    fn reads_root_path_as_a_system_states_it() {
        let debian = "ENV_SUPATH\tPATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin\n\
                      ENV_PATH\tPATH=/usr/local/bin:/usr/bin:/bin\n";
        assert_eq!(
            env_supath(debian).as_deref(),
            Some("/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin")
        );

        let arch = "# comments and blank lines\n\nENV_SUPATH\tPATH=/usr/local/sbin:/usr/local/bin:/usr/bin\n";
        assert_eq!(
            env_supath(arch).as_deref(),
            Some("/usr/local/sbin:/usr/local/bin:/usr/bin")
        );

        // A commented-out declaration states nothing, and neither does ENV_PATH: the
        // user's own PATH is not the one a tool of ours runs with.
        assert_eq!(env_supath("#ENV_SUPATH\tPATH=/nope\n"), None);
        assert_eq!(env_supath("ENV_PATH\tPATH=/usr/bin\n"), None);
    }
}

#[cfg(test)]
mod mount_tests {
    use super::*;

    /// Two mounts never share a directory, whether they are two drives being read at
    /// once or the same drive read twice, which the marker read does alongside the
    /// listings.
    ///
    /// Sharing one is what made a marked profile lose its heart: the drives are read
    /// a thread each, the second mount stacked on the first, and a thread read the
    /// wrong filesystem and reported no entries at all.
    #[test]
    fn every_mount_gets_a_directory_of_its_own() {
        let names = [
            mount_point("/dev/sda3"),
            mount_point("/dev/mmcblk0p3"),
            mount_point("/dev/sda3"),
        ];
        let unique: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "two mounts share a name: {names:?}");
        // The device is in the name, so the mount is legible in /proc/mounts while
        // it is there.
        assert!(names[0].contains("sda3"), "{}", names[0]);
        assert!(names[1].contains("mmcblk0p3"), "{}", names[1]);
        // And nothing in it can walk out of /run or /tmp.
        for name in &names {
            assert!(!name.contains('/'), "{name}");
            assert!(!name.contains(".."), "{name}");
        }
    }

    /// A device path that is only slashes, or empty, still yields a usable name
    /// rather than one that resolves to the parent directory.
    #[test]
    fn an_odd_device_name_is_still_a_directory_name() {
        for dev in ["", "/", "///", "/dev/"] {
            let name = mount_point(dev);
            assert!(!name.contains('/'), "{dev:?} -> {name}");
            assert!(name.starts_with("flipctl-entries."), "{dev:?} -> {name}");
        }
    }
}
