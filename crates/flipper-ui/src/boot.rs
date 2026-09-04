//! Bootable profiles and the entries that boot them.
//!
//! A row is a profile: a btrfs subvolume the device can boot into, as `list-profiles`
//! reports it. Each carries its boot entries, the files under /boot/loader/entries
//! that name a kernel, an initrd, a command line and the device tree overlays -- one
//! per kernel installed in it, in boot order, so a profile's first entry is the kernel
//! it boots.
//!
//! **What boots is the first entry.** Nothing records a choice anywhere else: no
//! marker, no pin. Entries are sorted, and the first one wins, so marking a profile or
//! choosing a kernel means writing digits into an entry's `sort-key` -- which is
//! `set-boot-order`'s job, never this module's. See `order` for the rule, which
//! `libs/flipper-blsname.sh` implements in shell against the same fields.
//!
//! None of this has a kernel interface to read instead: the subvolume layout and the
//! boot order are conventions of this image, not facts the kernel exposes.
//!
//! Nothing here writes. Renaming, cloning, deleting and factory-resetting a
//! profile are the destructive half of the boot menu and are deliberately absent
//! until they have somewhere safe to be tested.

use std::io::Write;
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
    /// Its boot entries, in boot order: `entries[0]` is the kernel this profile boots.
    /// Empty for a profile with no kernel the menu will show, which has no row.
    pub entries: Vec<Entry>,
    /// Whether this profile is the one that boots when nobody presses anything: its
    /// entries carry the autoboot digit, and one of them is first on the machine.
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

/// One boot entry: one kernel of one profile, as its file states it.
///
/// `id` is the file name without its boot counter or `.conf`, which is what
/// `boot-profile` and `set-boot-order` take and what stays the same across attempts.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// The kernel release it names, e.g. `7.2.0-00249-g26619ffca0bd`.
    pub version: String,
    /// That release as the entry's own `title` carries it, which kernel-install has
    /// already trimmed to something a menu can show.
    pub short: String,
    /// The `sort-key`, which is where the order lives. Read, never written here.
    pub key: String,
    /// Tries left from the file name's boot counter; `None` for an entry that a good
    /// boot has blessed, `Some(0)` for one that has spent every attempt.
    pub tries: Option<u32>,
    /// Overlays the entry applies: shipped with the image, and added by hand.
    pub system: Vec<String>,
    pub user: Vec<String>,
    /// Where its device trees and overlays live, and the file it is written in: what
    /// changing one of its settings has to edit.
    pub dtdir: String,
    pub file: String,
    /// When its file was written, in seconds since the epoch. Which kernel is newest,
    /// since git-describe releases do not compare; see `order`.
    pub at: u64,
}

impl Entry {
    /// Whether every attempt is spent. The spec calls it 'bad': it sorts after
    /// everything else and boots only if somebody asks for it by name.
    pub fn bad(&self) -> bool {
        self.tries == Some(0)
    }

    /// What a screen says about this kernel beside its version, or nothing.
    ///
    /// Nothing where the entry carries no counter, because the file says nothing: the
    /// spec makes an entry good by REMOVING its counter, so one that a boot blessed and
    /// one that was never counted are identical on disk. Naming that state would be
    /// claiming to know which, and `set-boot-order --list` prints the same dash for the
    /// same reason.
    pub fn state(&self) -> Option<&'static str> {
        match self.tries {
            None => None,
            Some(0) => Some("failed"),
            Some(_) => Some("untried"),
        }
    }
}

/// Which kernels the menu offers.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum Kernels {
    /// `MIN_KERNEL` and newer, which is every kernel this board is run on.
    #[default]
    Modern,
    /// Every entry there is, whatever kernel it names: `--all-kernels`.
    All,
}

/// The oldest kernel the menu offers, as major and minor.
///
/// This device runs mainline; the 6.1 BSP entries an older image left on disk boot
/// nothing anybody wants, and a menu is a list of things worth choosing. They are
/// hidden rather than removed, because the files are not ours to delete, and
/// `--all-kernels` brings them back for a bisect.
pub const MIN_KERNEL: (u32, u32) = (7, 0);

/// A boot entry as its own file states it, before the profile it names is looked up.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Conf {
    /// The file's name without its boot counter or `.conf`.
    pub id: String,
    /// The subvolume its command line mounts, from `rootflags=subvol=`.
    pub subvol: String,
    pub version: String,
    pub short: String,
    /// The `sort-key` line, verbatim.
    pub key: String,
    /// Tries left, from the counter in the file name; None when it carries none.
    pub tries: Option<u32>,
    pub system: Vec<String>,
    pub user: Vec<String>,
    /// The `devicetreedir` line, verbatim: where this entry's trees and overlays are,
    /// written as the loader sees them.
    pub dtdir: String,
    /// When the file was written, in seconds since the epoch, or 0 when unknown, and
    /// the file's own name. Both are tiebreaks in the order; see `order`.
    pub at: u64,
    pub file: String,
}

impl Conf {
    /// This conf as an entry.
    fn entry(&self) -> Entry {
        Entry {
            id: self.id.clone(),
            version: self.version.clone(),
            short: self.short.clone(),
            key: self.key.clone(),
            tries: self.tries,
            system: self.system.clone(),
            user: self.user.clone(),
            dtdir: self.dtdir.clone(),
            file: self.file.clone(),
            at: self.at,
        }
    }
}

/// Sort entries into boot order: the first one is what boots.
///
/// Takes references so a caller can order a selection out of one drive's confs without
/// copying them, which is what `listing` does per profile.
///
/// The rule, which `libs/flipper-blsname.sh` implements in shell against the same
/// fields, and which the BLS spec is the source of except where noted:
///
/// 1. an entry with no tries left sorts after everything else
/// 2. then by `sort-key`, ascending
/// 3. then by kernel version, descending, as far as a version compares: the numbers
///    before the first dash. What follows them is a git-describe suffix that does not
///    compare, `7.2.0-00249-g26619ffca0bd` against `7.2.0-ga0d2d145deeb` having no
///    answer in either direction
/// 4. then newest file first, which is what separates two builds of one version, and
///    the fact that answers "the one just put on"
/// 5. then by file name, descending, which is the spec's own last resort
pub fn sort_confs(confs: &mut [&Conf]) {
    confs.sort_by_key(|c| order(c));
}

/// One entry's place in the boot order, comparable against another's.
type Order = (
    bool,
    String,
    std::cmp::Reverse<(u32, u32, u32)>,
    std::cmp::Reverse<u64>,
    std::cmp::Reverse<String>,
);

/// One entry's place in that order, as a sort key.
fn order(c: &Conf) -> Order {
    (
        c.tries == Some(0),
        c.key.clone(),
        std::cmp::Reverse(version_rank(&c.version)),
        std::cmp::Reverse(c.at),
        std::cmp::Reverse(c.file.clone()),
    )
}

/// A kernel release as major, minor and patch, for comparing two of them.
///
/// Only the numbers before the first dash: what follows is a git-describe suffix, and
/// `libs/flipper-blsname.sh` reads the same three numbers out of it in shell. A release
/// with no numbers ranks below every real one and is still bootable by name.
pub fn version_rank(version: &str) -> (u32, u32, u32) {
    let numbers = version.split('-').next().unwrap_or_default();
    let mut parts = numbers.split('.').map(|p| p.parse::<u32>().unwrap_or(0));
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
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

/// Everything the boot menu reads: the profiles, and which of their entries boots.
pub struct Listing {
    /// Profiles in the order `list-profiles` reports them, each carrying its own
    /// entries in boot order. A profile whose every kernel is hidden keeps its place
    /// with no entries: it has no row, and a name check still has to see it.
    pub profiles: Vec<Profile>,
    /// Which profile boots when nobody presses anything: the one holding the first
    /// entry on the machine's own storage. None when nothing is bootable there.
    pub first: Option<usize>,
    /// How many entries this listing left out for naming a kernel below
    /// `MIN_KERNEL`. Reported once by whoever asked, not per profile.
    pub hidden: usize,
}

/// The profiles on every filesystem of ours, the booted drive's first.
///
/// For what reasons about profiles rather than about what boots: a name check, a
/// delete, a factory reset. The kernels a profile has are on it either way.
pub fn profiles() -> Vec<Profile> {
    listing(Kernels::All).profiles
}

/// Every profile and every entry, in one read, ordered as the menu shows them.
///
/// One read because it is one walk: each drive is mounted once, its `list-profiles`
/// and its `loader/entries` read on the same thread, and asking twice would mount
/// every filesystem again for an answer already in hand.
///
/// **Which profile boots** is the profile holding the first entry in `order`, counting
/// only the machine's own storage. A card carries entries of its own, with keys and
/// names that repeat, so a card's order says what that card would boot on its own
/// hardware and nothing about this machine: including it would let inserting a card
/// change what the device does when left alone.
///
/// An entry that mounts a subvolume which is no profile here is dropped, with a log
/// line: nothing can be booted from an entry whose root does not exist, and one left
/// behind by a deleted profile looks from the outside like a missing row.
pub fn listing(kernels: Kernels) -> Listing {
    let drives = read_drives();
    let mut profiles: Vec<Profile> = Vec::new();
    // Entries left out for naming a kernel below the floor, counted rather than
    // announced one profile at a time: it is the same sentence about every profile that
    // still has a BSP entry, on every boot.
    let mut hidden = 0usize;
    // The best (profile index, order key) seen on internal storage, which is what boots.
    let mut best: Option<(usize, Order)> = None;

    for drive in &drives {
        for conf in &drive.confs {
            if !drive.profiles.iter().any(|p| p.name == conf.subvol) {
                crate::logline!(
                    "boot           entry {} mounts {}, which is no profile here",
                    conf.id,
                    conf.subvol
                );
            }
        }
        for profile in &drive.profiles {
            let mut mine: Vec<&Conf> = drive
                .confs
                .iter()
                .filter(|c| c.subvol == profile.name)
                .collect();
            let all = mine.len();
            if kernels == Kernels::Modern {
                mine.retain(|c| version_at_least(&c.version, MIN_KERNEL));
            }
            hidden += all - mine.len();
            sort_confs(&mut mine);

            let at = profiles.len();
            if drive.store.medium == Medium::Internal {
                if let Some(front) = mine.first() {
                    let key = order(front);
                    if best.as_ref().is_none_or(|(_, b)| key < *b) {
                        best = Some((at, key));
                    }
                }
            }
            let mut profile = profile.clone();
            profile.entries = mine.iter().map(|c| c.entry()).collect();
            profiles.push(profile);
        }
    }

    let first = best.map(|(at, _)| at);
    if let Some(at) = first {
        profiles[at].auto_boot = true;
        crate::logline!(
            "boot           {} boots by itself, kernel {}",
            profiles[at].name,
            profiles[at].entries.first().map_or("none", |e| e.version.as_str())
        );
    } else {
        crate::logline!("boot           nothing on the machine's own storage is bootable");
    }
    Listing { profiles, first, hidden }
}

/// Whether a kernel release is `min` or newer, on major and minor alone.
///
/// Ours are git-describe strings -- `7.2.0-00249-g26619ffca0bd` -- so only the numbers
/// before the first dash compare at all. That is enough for the question being asked,
/// which is whether an entry names a 6.1 BSP kernel or a mainline one.
///
/// A release that will not parse counts as new enough: a hidden row cannot be booted,
/// so the safer of the two mistakes is to show one kernel too many.
pub fn version_at_least(version: &str, min: (u32, u32)) -> bool {
    // A release that will not parse ranks 0 and would fail every comparison, so it is
    // answered before the numbers are looked at.
    if version
        .split('-')
        .next()
        .and_then(|n| n.split('.').next())
        .is_none_or(|major| major.parse::<u32>().is_err())
    {
        return true;
    }
    let (major, minor, _) = version_rank(version);
    (major, minor) >= min
}

/// One drive's answer: what it is, the profiles on it, and the entries that boot them.
struct Drive {
    store: Store,
    profiles: Vec<Profile>,
    confs: Vec<Conf>,
}

/// Every filesystem of ours, read at once: its profiles and the entries that boot them.
///
/// A listing mounts that filesystem's top level and walks every subvolume on it, which
/// is seconds of I/O per drive, and the drives are independent: nothing is shared and
/// the read-only tools take no lock, only the mutating ones do. Done in turn, the menu
/// waits for the sum; done at once, for the slowest. The order of the results is the
/// order of `stores`, so the list on screen does not depend on which drive answered
/// first.
///
/// Each drive's entries are read on that drive's own thread, because reading them is
/// reading through a mount of it: read afterwards instead, every drive would be mounted
/// twice for a few hundred bytes a file.
///
/// No check that the tools are there: they ship in the boot menu image, and an image
/// without them has nothing to do. flipctl still checks, to hide its Boot row on a
/// machine with no profile tools at all.
fn read_drives() -> Vec<Drive> {
    let found = stores();
    // All of them on one line, and before the reads: which drives were seen is what a
    // read that never returns leaves behind, and /dev/kmsg gives a process ten lines
    // every five seconds, so a line per drive is one the pivot cannot spare.
    if !found.is_empty() {
        let seen: Vec<String> = found
            .iter()
            .map(|s| {
                format!(
                    "{} on {} ({}{})",
                    s.dev,
                    s.disk,
                    s.kind,
                    if s.booted { ", booted" } else { "" }
                )
            })
            .collect();
        crate::logline!("boot           drives: {}", seen.join("; "));
    }
    // No lsblk, or nothing recognisable: ask about the booted filesystem alone, which
    // is what every tool takes as its default and where our own /boot is. Named as a
    // store rather than handled apart, so there is one read path and not two.
    let stores = if found.is_empty() {
        vec![Store {
            dev: String::new(),
            disk: String::new(),
            medium: Medium::Internal,
            kind: "disk",
            booted: true,
        }]
    } else {
        found
    };

    // One thread per drive.
    let read = std::thread::scope(|scope| {
        let reads: Vec<_> = stores
            .iter()
            .map(|store| {
                scope.spawn(move || {
                    let at = std::time::Instant::now();
                    let listing = if store.booted {
                        sudo(&["list-profiles"])
                    } else {
                        sudo(&["list-profiles", "-d", &store.dev])
                    };
                    let confs = drive_confs(store);
                    crate::logline!(
                        "boot           read {} in {:.3}s, {} rows and {} entries",
                        dev_label(store),
                        at.elapsed().as_secs_f64(),
                        listing.as_deref().map_or(0, |l| l.lines().count()),
                        confs.len()
                    );
                    (listing, confs)
                })
            })
            .collect();
        reads
            .into_iter()
            .map(|r| r.join().unwrap_or((None, Vec::new())))
            .collect::<Vec<(Option<String>, Vec<Conf>)>>()
    });

    let mut out = Vec::new();
    for (store, (listing, confs)) in stores.into_iter().zip(read) {
        let Some(listing) = listing else {
            // A drive that answers nothing is not the same as a drive with no profiles,
            // and only the log can tell the two apart afterwards.
            crate::logline!(
                "boot           list-profiles answered nothing for {}",
                dev_label(&store)
            );
            continue;
        };
        // The booted filesystem is every tool's default, so it needs no device: this
        // way a profile carries one only when saying which is the point.
        let dev = if store.booted { "" } else { store.dev.as_str() };
        let profiles = parse_listing(&listing, store.medium, dev, &store.disk, store.kind);
        out.push(Drive { store, profiles, confs });
    }
    out
}

/// What to call a drive in the log. The booted filesystem has no device of its own
/// here, being every tool's default.
fn dev_label(store: &Store) -> &str {
    if store.dev.is_empty() {
        "the booted filesystem"
    } else {
        store.dev.as_str()
    }
}

/// The boot entries one drive carries.
///
/// A profile's entries live on the profile's own drive: a card's entry names kernels
/// only that card has, and profile names repeat across drives, so the booted /boot
/// would answer with the wrong entry rather than with none. For the booted filesystem
/// that is /boot itself; any other drive is read through a read-only top-level mount,
/// as the tools do.
fn drive_confs(store: &Store) -> Vec<Conf> {
    if store.booted {
        return read_confs(std::path::Path::new(BOOTED_ENTRIES));
    }
    match TopLevel::ro(&store.dev) {
        Some(top) => read_confs(&top.path.join("boot/loader/entries")),
        None => Vec::new(),
    }
}

/// Every entry in one directory, parsed.
///
/// A file that cannot be read or does not name a subvolume is skipped: the loader
/// directory holds whatever anyone has put there, and only an entry that mounts
/// something is a row.
fn read_confs(dir: &std::path::Path) -> Vec<Conf> {
    let Ok(listing) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for file in listing.flatten() {
        let path = file.path();
        if path.extension().is_none_or(|x| x != "conf") {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(mut conf) = parse_conf(name, &text) else {
            continue;
        };
        conf.at = file
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        out.push(conf);
    }
    out
}

/// One entry file, parsed from its name and its text. None when it mounts no
/// subvolume.
///
/// `name` is the file name, counter and suffix included: the counter is boot-counting
/// state and the id is the name without it, so both come out of the same string. See
/// `libs/flipper-blsname.sh`, which spells the same two names out in shell.
///
/// Fields are `key value` and a key has to end at whitespace, or `devicetree` would
/// answer with `devicetreedir`'s value. The first line that says something wins, as a
/// loader would take it.
///
/// Overlays are gathered from every `devicetree-overlay` line rather than the first,
/// since a profile's own drop-ins may be listed apart from the image's, and split by
/// path: something under /etc/kernel/dtbo is what someone added, anything else ships
/// with the image.
pub fn parse_conf(name: &str, text: &str) -> Option<Conf> {
    let field = |key: &str| -> &str {
        text.lines()
            .filter_map(|l| l.strip_prefix(key))
            .filter(|rest| rest.starts_with(char::is_whitespace))
            .map(str::trim)
            .find(|rest| !rest.is_empty())
            .unwrap_or_default()
    };

    // rootflags carries the subvolume, and may carry more after it.
    let subvol = field("options")
        .split_whitespace()
        .find_map(|opt| opt.strip_prefix("rootflags=subvol="))?
        .split(',')
        .next()
        .unwrap_or_default()
        .to_string();
    if subvol.is_empty() {
        return None;
    }

    // Older entries carry no version line; the kernel path holds it either way.
    let version = match field("version") {
        "" => version_in_path(field("linux")),
        stated => stated.to_string(),
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

    let (id, tries) = split_counter(name);
    Some(Conf {
        id,
        tries,
        subvol,
        key: field("sort-key").to_string(),
        short: short_version(field("title"), &version),
        version,
        system,
        user,
        dtdir: field("devicetreedir").to_string(),
        at: 0,
        file: name.to_string(),
    })
}

// ── Video out ──────────────────────────────────────────────────────────────

/// What the Video Out line offers, and the overlay each choice applies.
///
/// A table rather than a search of the overlay directory: a `.dtbo` says nothing
/// about what it does, and that directory holds every board's, so "video" in a file
/// name is as likely to be another machine's demo panel as this one's HDMI.
///
/// The first applies nothing, because the base tree already wires HDMI to the 4K pipe
/// and DP to the 2.5K one. The second swaps them. The third disables the display
/// pipeline, both PHYs and the HDMI and DP audio cards, and leaves the GPU up for the
/// panel that is soldered on. Both overlays are SoC nodes from rk3576.dtsi rather than
/// board ones, which is why they are offered whatever board the profile is for.
///
/// User overlays are not part of this. What somebody dropped into /etc/kernel/dtbo is
/// theirs, `add-dtbo` is what manages it, and this touches only the overlays an entry
/// carries because the image put them there.
pub const VIDEO_OUT: &[(&str, &str)] = &[
    ("", "HDMI 4k"),
    ("rk3576-dp-4k-hdmi-2.5k", "DisplayPort 4k"),
    ("rk3576-no-graphics", "Headless"),
];

/// Which of them a profile is on: what its newest entry that this setting is written
/// to says, so the row reads back what the write put there.
pub fn video_out_for(p: &Profile) -> usize {
    p.entries
        .iter()
        .find(|e| version_at_least(&e.version, MIN_KERNEL))
        .map_or(0, video_out_of)
}

/// Which of them an entry is on, as an index into `VIDEO_OUT`.
pub fn video_out_of(entry: &Entry) -> usize {
    VIDEO_OUT
        .iter()
        .position(|(name, _)| {
            !name.is_empty() && entry.system.iter().any(|o| o == &format!("{name}.dtbo"))
        })
        .unwrap_or(0)
}

/// The entry file's text with `choice` applied.
///
/// Pure, so what the file should say can be tested without a device or a loader. The
/// rule: every overlay this table knows about comes off, the chosen one goes on, and
/// every other overlay the entry carried stays exactly where it was, user drop-ins
/// included. A line that ends up with nothing on it is removed rather than left empty,
/// and an entry that had no such line gets one after its `devicetreedir`, which is
/// where kernel-install writes it and where a reader looks for it.
pub fn with_video_out(text: &str, choice: usize) -> String {
    let dtdir = text
        .lines()
        .filter_map(|l| l.strip_prefix("devicetreedir"))
        .map(str::trim)
        .find(|rest| !rest.is_empty())
        .unwrap_or_default();
    let ours = |path: &str| {
        let base = path.rsplit('/').next().unwrap_or(path);
        VIDEO_OUT
            .iter()
            .any(|(name, _)| !name.is_empty() && base == format!("{name}.dtbo"))
    };

    let mut keep: Vec<String> = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("devicetree-overlay") {
            keep.extend(rest.split_whitespace().filter(|p| !ours(p)).map(str::to_string));
        }
    }
    if let Some((name, _)) = VIDEO_OUT.get(choice).filter(|(name, _)| !name.is_empty()) {
        keep.push(format!("{dtdir}/rockchip/{name}.dtbo"));
    }

    let mut out = String::with_capacity(text.len() + 96);
    let mut written = false;
    for line in text.lines() {
        if line.starts_with("devicetree-overlay") {
            // The first of them carries the whole list, and the rest go: one line is
            // what kernel-install writes and what a reader of this file expects.
            if !written && !keep.is_empty() {
                out.push_str(&format!("devicetree-overlay {}\n", keep.join(" ")));
                written = true;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
        if line.starts_with("devicetreedir") && !written && !keep.is_empty() {
            out.push_str(&format!("devicetree-overlay {}\n", keep.join(" ")));
            written = true;
        }
    }
    out
}

/// Put `choice` on every entry of a profile.
///
/// Every entry, not the one that boots: the row says what comes out of this profile's
/// video connectors, and a profile that answered differently depending on which of its
/// kernels happened to boot would be a setting nobody could rely on. Each entry names
/// its own kernel's tree directory, so each gets the overlay from under its own, which
/// is why the path is read out of every file rather than built once.
///
/// The booted filesystem is written in place; any other drive is mounted at its top
/// level for as long as the rewrite takes, because that is where its entries are. Both
/// are the same file edit: nothing here knows or cares which drive it is on, and the
/// kernel row beside this one has always reached any profile the same way.
///
/// Entries below `MIN_KERNEL` are skipped, and `video_out_of` reads the first entry
/// at or above it, so a profile that still carries a 6.1 entry answers for the
/// kernels it actually boots.
pub fn set_video_out(p: &Profile, choice: usize) -> Result<(), String> {
    // Held for the whole loop, so one mount covers every entry, and dropped with it.
    let hold = match p.dev.is_empty() {
        true => None,
        false => Some(
            TopLevel::rw(&p.dev)
                .ok_or_else(|| format!("could not mount {} to write to", p.dev))?,
        ),
    };
    let dir = match &hold {
        Some(top) => top.path.join("boot/loader/entries"),
        None => std::path::PathBuf::from(BOOTED_ENTRIES),
    };
    for entry in &p.entries {
        // The BSP kernels are not written to. These overlays are nodes of our own
        // tree and a 6.1 entry's directory does not hold them, so an overlay line
        // there would name a file that is not there and the loader would fail on an
        // entry that boots today. Those entries are hidden from the menu anyway;
        // --all-kernels brings them back for a bisect and this still leaves them be.
        if !version_at_least(&entry.version, MIN_KERNEL) {
            continue;
        }
        if entry.file.is_empty() || entry.file.contains('/') {
            return Err("an entry with no file".into());
        }
        set_one_video_out(&dir.join(&entry.file), choice)?;
    }
    // The lines are written, but on btrfs metadata reaches the disk with the next
    // transaction commit, up to `commit=` seconds later, and what this row changes is
    // what the next boot does. Somebody who picks an output and pulls the power should
    // not come back to the old one. Once for the whole change, since a profile can have
    // four entries, and `sync` needs no privilege, unlike the writes above.
    unsafe { libc::sync() };
    Ok(())
}

/// One entry file, rewritten.
///
/// The file belongs to root and flipctl does not, so the bytes go through `tee`:
/// deciding what the file should say is `with_video_out`'s job, and the privileged
/// step is as small as writing what it decided.
fn set_one_video_out(path: &std::path::Path, choice: usize) -> Result<(), String> {
    let shown = path.display();
    let text = std::fs::read_to_string(path).map_err(|e| format!("{shown}: {e}"))?;
    let next = with_video_out(&text, choice);
    if next == text {
        return Ok(());
    }
    let Some(target) = path.to_str() else {
        return Err(format!("{shown}: not a path a tool can take"));
    };
    let mut child = tool(&["tee", target])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("tee: {e}"))?;
    child
        .stdin
        .take()
        .ok_or_else(|| "tee: no stdin".to_string())?
        .write_all(next.as_bytes())
        .map_err(|e| format!("tee: {e}"))?;
    match child.wait() {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("tee {shown}: {status}")),
        Err(e) => Err(format!("tee {shown}: {e}")),
    }
}

/// An entry file name split into its id and the tries left in its boot counter.
///
/// `900-flipperos-Desktop-7.2.0-x+2-1.conf` is that entry on its second attempt of
/// three; without a `+` it has been blessed by a good boot and is not counted at all.
/// A counter that will not parse is treated as no counter: a name nobody here wrote is
/// not a reason to call an entry bad and refuse to boot it.
pub fn split_counter(name: &str) -> (String, Option<u32>) {
    let stem = name.strip_suffix(".conf").unwrap_or(name);
    match stem.split_once('+') {
        Some((id, counter)) => (
            id.to_string(),
            counter
                .split('-')
                .next()
                .and_then(|left| left.parse::<u32>().ok()),
        ),
        None => (stem.to_string(), None),
    }
}

/// The kernel release a `linux` path holds, for an entry that states no version.
///
/// Ours put the kernel in its modules directory, which is where the release is named;
/// an image that keeps kernels in /boot names it in the file instead.
fn version_in_path(path: &str) -> String {
    let mut parts = path.split('/');
    while let Some(part) = parts.next() {
        if part == "modules" {
            return parts.next().unwrap_or_default().to_string();
        }
    }
    path.rsplit('/')
        .next()
        .and_then(|file| file.strip_prefix("vmlinuz-"))
        .unwrap_or_default()
        .to_string()
}

/// The version as a title carries it: `title Desktop 7.2.0-00249-g26619` gives
/// `7.2.0-00249-g26619`.
///
/// Taken from the title because kernel-install has already trimmed it there to fit a
/// menu, and this menu is 256 pixels wide. A title that ends in anything but a version
/// -- a profile whose name is the whole title -- falls back to the release itself.
fn short_version(title: &str, version: &str) -> String {
    match title.rsplit_once(char::is_whitespace) {
        Some((_, last)) if last.starts_with(|c: char| c.is_ascii_digit()) => last.to_string(),
        _ => version.to_string(),
    }
}

/// The listing, parsed. Split out so it can be tested against real output.
///
/// Columns are positional and separated by runs of two or more spaces:
///
/// ```text
/// NAME [<- booted] KIND ID CREATED LAST_USED RO PARENT ORIGIN
/// ```
///
/// The booted marker sits in its own column and shifts everything after it, which is
/// why the offset is computed rather than fixed. Anything that is not a `profile` is
/// skipped: `_old` backups are leftovers, not somewhere to boot.
pub fn parse_listing(
    listing: &str,
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
        let cols: Vec<&str> = line
            .split("  ")
            .filter(|c| !c.trim().is_empty())
            .map(str::trim)
            .collect();
        if cols.len() < 4 {
            continue;
        }
        let booted = cols.get(1).is_some_and(|c| *c == "<- booted");
        let base = if booted { 2 } else { 1 };
        if cols.get(base).is_none_or(|k| *k != "profile") {
            continue;
        }
        // PARENT and ORIGIN carry the stock's id as "name (id)".
        let strip_id = |s: &str| s.split(" (").next().unwrap_or(s).trim().to_string();
        let dash_to_empty = |s: String| if s == "-" { String::new() } else { s };
        out.push(Profile {
            // Filled in by `listing`, which is what reads the entries.
            entries: Vec::new(),
            auto_boot: false,
            medium,
            dev: dev.to_string(),
            disk: disk.to_string(),
            kind,
            name: cols[0].to_string(),
            booted,
            id: cols.get(base + 1).unwrap_or(&"").to_string(),
            created: cols.get(base + 2).unwrap_or(&"").to_string(),
            last_used: cols.get(base + 3).unwrap_or(&"").to_string(),
            parent: dash_to_empty(strip_id(cols.get(base + 5).unwrap_or(&""))),
            origin: dash_to_empty(strip_id(cols.get(base + 6).unwrap_or(&""))),
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

/// The kernel this system is running, which is half of what "running" means.
///
/// From procfs rather than from `uname`: it is the same string the tools compare against
/// (boot-profile reads `uname -r`, which reads this file) and it costs no process.
///
/// Read once and remembered: a system does not change kernels without rebooting, and the
/// row status asks per frame.
pub fn running_kernel() -> &'static str {
    static RELEASE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    RELEASE.get_or_init(|| {
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
            .to_string()
    })
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

/// A top-level mount of another filesystem, unmounted when dropped.
///
/// A profile's boot entry lives on the profile's own drive: a card's entry names
/// kernels only that card has, and an initramfs has no /boot of ours at all.
/// subvolid=5 because that is where the entries are, as the tools mount it.
///
/// Read-only for a listing, which is all a listing needs, and writable for a setting
/// that lands in an entry file. Writable is the shorter of the two: a file is
/// rewritten and the mount goes, rather than being held for as long as a popup is open.
///
/// Drop does the unmounting so a read that returns early, or panics, cannot leave the
/// filesystem mounted: this runs while a popup is open, and the drive it holds is one
/// someone may pull out.
struct TopLevel {
    path: std::path::PathBuf,
}

impl TopLevel {
    fn ro(dev: &str) -> Option<Self> {
        Self::mount(dev, "ro,subvolid=5")
    }

    /// Writable, for changing a setting that lives in a boot entry.
    fn rw(dev: &str) -> Option<Self> {
        Self::mount(dev, "rw,subvolid=5")
    }

    fn mount(dev: &str, opts: &str) -> Option<Self> {
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
        match run(&["mount", "-t", "btrfs", "-o", opts, dev, target]) {
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

/// An entry id safe to hand to the tools: an entry file's name without `.conf`.
///
/// Not a quoting question -- these go straight to execve with no shell -- but a "this
/// cannot be an entry" one. kernel-install's names carry a band, a profile and a
/// version and nothing else, and nothing here builds a path out of one, so anything
/// with a separator in it is refused.
pub fn valid_entry_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 255
        && !id.starts_with('.')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '+'))
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
                crate::logline!("tool           {}", line.trim());
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
        (Some(code), _) => crate::logline!("tool           {} exited {code}, saying nothing", args[0]),
        (_, Some(sig)) => crate::logline!("tool           {} killed by signal {sig}", args[0]),
        _ => crate::logline!("tool           {} died without a status", args[0]),
    }
    Err("the tool gave no answer".into())
}

/// Boot a profile now, and do not come back.
///
/// `boot-profile` picks one of two ways over, and says in the kernel log which one it
/// took. A profile whose kernel and device tree are the ones already running is
/// pivoted into: its root is put in place and PID 1 handed over, which costs a mount
/// rather than a second kernel boot. From the boot menu's initramfs that is
/// switch_root; from a booted profile it is systemd's soft-reboot, which replaces
/// userspace without touching the kernel. Everything else is kexec'd, which is the
/// only way a profile boots with a kernel or a tree of its own: the tool reads that
/// entry for the kernel, the initrd and the command line, assembles its device tree
/// from the board base plus the entry's overlays through fdtoverlay, loads all of it
/// and hands over. Which way is the tool's call alone: it compares the tree it
/// assembled with the one the kernel is running, so a change to an entry since this
/// kernel booted is seen there, whoever made it.
///
/// The entry is named, not the profile: the row on screen showed one kernel, and that
/// is the one that has to boot. Handing over a profile name would let the tool choose
/// again, and a second opinion about which kernel a profile boots is exactly what this
/// design removes.
///
/// Returns `Ok(false)` only when it failed to leave, which on a real boot cannot
/// happen: the machine is already on its way out. `Ok(true)` is the dry run, which
/// loaded the image, unloaded it and stayed.
///
/// `FLIPCTL_BOOT_DRY_RUN=1` is that dry run: it proves the entry resolves, the kernel
/// and initrd are there and the device tree assembles, without losing the session that
/// asked. The caller has to say so on screen, because nothing else will.
///
/// A profile on another filesystem is booted with `-d`, which is what makes it that
/// card's entry rather than the one of the same name on the internal storage: entry
/// ids repeat across filesystems written from the same image.
pub fn boot_now(p: &Profile) -> Result<bool, String> {
    let Some(entry) = p.entries.first() else {
        return Err(format!("{} has no kernel to boot", display_name(&p.name)));
    };
    if !valid_entry_id(&entry.id) {
        return Err("invalid entry".into());
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
    args.push(&entry.id);
    run(&args)?;
    Ok(dry)
}

/// Load the image a profile would kexec into, while nobody is pressing anything.
///
/// The loading is the slow half of a kexec boot and none of it is I/O: one syscall
/// placing a 29MB kernel at its destination and cloning the kernel's page tables,
/// against 0.05s to read the files and 0.04s to assemble the device tree. Measured on
/// this board, both kernels: 2.48s in that syscall until `arm64: trans_pgd: clone only
/// the linear map that exists at runtime`, which stopped the walk cloning the whole
/// kernel table 16 times on a VA-52 kernel running at VA 48, and 0.43s after it. The
/// clone was 88% of it; what is left is copying and hashing the image, which no patch
/// makes free.
///
/// Either figure is time a boot then does not spend, and a menu waiting for a key has
/// it to spare.
///
/// Nothing is loaded for a profile that would be pivoted into: a pivot keeps this
/// kernel, so there is no image. The kernel holds one image at a time, so each of
/// these replaces the last, and which profile is worth holding is the caller's
/// decision rather than this function's.
///
/// Returns whether the kernel now holds an image, read from it rather than assumed:
/// the difference between a load and a decision not to load is invisible in the exit
/// status, and a caller timing the syscall would otherwise time the decision.
///
/// Arming does not count as an attempt -- the tool leaves the counter alone until it
/// actually hands over -- so a menu that arms and then sits there costs a profile
/// nothing.
pub fn arm(p: &Profile) -> Result<bool, String> {
    let Some(entry) = p.entries.first() else {
        return Err("nothing to arm".into());
    };
    if !valid_entry_id(&entry.id) {
        return Err("invalid entry".into());
    }
    let mut args: Vec<&str> = vec!["boot-profile", "--arm"];
    if !p.dev.is_empty() {
        args.push("-d");
        args.push(&p.dev);
    }
    args.push(&entry.id);
    run(&args)?;
    Ok(kexec_loaded())
}

/// Whether the kernel is holding a kexec image.
///
/// The kernel's own answer, and the only one there is: what a profile's entry asks for
/// decides whether the tool loads anything, and nothing in the reply distinguishes
/// "loaded" from "nothing to load".
pub fn kexec_loaded() -> bool {
    std::fs::read_to_string("/sys/kernel/kexec_loaded")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

/// Discard an image left loaded by `arm`.
///
/// Takes no profile: the kernel holds one image at a time, so there is only ever one
/// image to discard, whichever profile it was loaded for.
///
/// Detached and unwaited, because this runs when a screen closes and the panel must
/// not stop for it. The tool retries the kexec syscall for up to four seconds to
/// wait out a load that is still going, which is the case that matters here: an arm
/// still in flight lands and is then discarded, rather than the discard being the
/// thing that is lost.
pub fn disarm() {
    let mut cmd = tool(&["boot-profile", "--disarm"]);
    if let Err(e) = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        crate::logline!("boot menu      cannot disarm: {e}");
    }
}

/// Make this profile the one that boots when nobody presses anything.
///
/// Nothing is recorded anywhere: `set-boot-order` moves the autoboot digit in the
/// entries' sort-keys, and the first entry is what boots. So the answer to "what boots"
/// is always the entries themselves, and there is no marker left to dangle when a
/// profile or a kernel goes away.
pub fn set_auto_start(dev: &str, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err("invalid name".into());
    }
    run(&on_dev("set-boot-order", dev, &["--autoboot", name]))
}

/// Boot this kernel for its profile from now on.
///
/// The same mechanism one level in: the rank digit moves within that profile's entries.
/// The tool also puts the entry back on trial, because a kernel nobody has booted with
/// this profile's device tree is not a kernel this profile is known to boot -- and if
/// it does not, the counter runs out and the profile's other kernel becomes first again.
///
/// Installing a kernel does the same thing for itself: a new entry is written at rank 0
/// with a full counter, so the kernel you just installed is the one that boots. This is
/// for going back, or for picking something other than the newest.
pub fn set_kernel(dev: &str, id: &str) -> Result<(), String> {
    if !valid_entry_id(id) {
        return Err("invalid entry".into());
    }
    run(&on_dev("set-boot-order", dev, &["--kernel", id]))
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

/// Delete a profile.
///
/// Nothing to repair afterwards: `delete-profile` removes the entries that boot this
/// profile along with it, and what boots is whatever entry is first once they are gone
/// -- the next band's chosen kernel. That is the whole benefit of keeping the order in
/// the entries rather than in a marker pointing at one.
pub fn delete(dev: &str, name: &str) -> Result<(), String> {
    if !valid_name(name) {
        return Err("invalid name".into());
    }
    run(&on_dev("delete-profile", dev, &["-y", name]))
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
            Ok(()) => crate::logline!("boot           reaped leftover backup {name}"),
            Err(e) => crate::logline!("boot           could not reap {name}: {e}"),
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
/// The order survives: an entry's name and key are built from the profile's name and
/// the kernel version, and a reset changes neither, so the entries come back as they
/// were and the profile keeps its place. It was the subvolume id the old marker used
/// that a reset invalidated.
///
/// Returns true when the profile reset was the running one, which means the
/// caller has to reboot: the root now mounted is the copy that was moved aside.
pub fn factory_reset(dev: &str, name: &str, origin: &str) -> Result<bool, String> {
    if !valid_name(name) || !valid_name(origin) {
        return Err("invalid name".into());
    }
    let booted = profiles()
        .iter()
        .find(|p| p.name == name)
        .is_some_and(|p| p.booted);

    run(&on_dev("create-profile", dev, &["-y", "--no-keep", origin, name]))?;
    Ok(booted)
}

/// Rename a profile's label, on the drive the profile is on.
///
/// `rename-profile` removes the profile's entries and reissues them under the new name,
/// and it carries the autoboot digit across itself: a profile that booted by itself
/// still does afterwards. Nothing to do here, which is the point of the order living
/// in the entries the tool is already rewriting.
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
    // Only the machine's own storage can be marked: what boots by itself is the first
    // entry there, and a card's own order says what that card would boot on its own
    // hardware. See `listing`.
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

#[cfg(test)]
mod video_tests {
    use super::*;

    const ENTRY: &str = "\
title      Desktop 7.2.0-ge8750f615eb
version    7.2.0-ge8750f615ebf
sort-key   debian-1100-Desktop-0
options    root=UUID=fe225468 rootflags=subvol=@Desktop flipper.entry=900-x
linux      /@Desktop/usr/lib/modules/7.2.0-ge8750f615ebf/vmlinuz
devicetreedir /@Desktop/usr/lib/linux-image-7.2.0-ge8750f615ebf
initrd     /@Desktop/usr/lib/modules/7.2.0-ge8750f615ebf/initrd
";

    fn overlays(text: &str) -> Vec<String> {
        text.lines()
            .filter_map(|l| l.strip_prefix("devicetree-overlay"))
            .flat_map(|rest| rest.split_whitespace().map(str::to_string))
            .collect()
    }

    /// The default applies nothing, so an entry that had no overlay line still has
    /// none: a line saying "no overlays" is a line a reader has to interpret.
    #[test]
    fn the_default_leaves_the_file_alone() {
        assert_eq!(with_video_out(ENTRY, 0), ENTRY);
        assert!(!with_video_out(ENTRY, 0).contains("devicetree-overlay"));
    }

    /// A choice becomes one line, holding the overlay under the device tree
    /// directory this entry already names.
    #[test]
    fn a_choice_is_written_under_the_entrys_own_tree_directory() {
        let headless = with_video_out(ENTRY, 2);
        assert_eq!(
            overlays(&headless),
            vec![
                "/@Desktop/usr/lib/linux-image-7.2.0-ge8750f615ebf/rockchip/\
                 rk3576-no-graphics.dtbo"
                    .replace("                 ", "")
            ]
        );
        // And it sits after the directory it is relative to, which is where
        // kernel-install puts it.
        let lines: Vec<&str> = headless.lines().collect();
        let dir = lines.iter().position(|l| l.starts_with("devicetreedir")).unwrap();
        assert!(lines[dir + 1].starts_with("devicetree-overlay"));
        // Nothing else moved.
        assert_eq!(
            headless.lines().filter(|l| !l.starts_with("devicetree-overlay")).count(),
            ENTRY.lines().count()
        );
    }

    /// One at a time: picking another replaces the first rather than stacking, and
    /// picking the default takes the line away again.
    #[test]
    fn a_choice_replaces_the_last_one() {
        let headless = with_video_out(ENTRY, 2);
        let dp = with_video_out(&headless, 1);
        assert_eq!(overlays(&dp).len(), 1);
        assert!(overlays(&dp)[0].ends_with("rk3576-dp-4k-hdmi-2.5k.dtbo"));
        assert_eq!(with_video_out(&dp, 0), ENTRY);
    }

    /// Every other overlay stays, wherever it came from. A user drop-in belongs to
    /// whoever put it there and add-dtbo is what manages it; an overlay the image
    /// shipped for something other than video is not this line's business either.
    #[test]
    fn overlays_that_are_not_ours_are_untouched() {
        let with_others = ENTRY.replace(
            "initrd",
            "devicetree-overlay /@Desktop/usr/lib/linux-image-7.2.0-ge8750f615ebf/rockchip/\
             rk3576-flipper-one-sata.dtbo /etc/kernel/dtbo/mine.dtbo\ninitrd",
        );
        let headless = with_video_out(&with_others, 2);
        let got = overlays(&headless);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(got.iter().any(|o| o.ends_with("rk3576-flipper-one-sata.dtbo")));
        assert!(got.iter().any(|o| o == "/etc/kernel/dtbo/mine.dtbo"));
        assert!(got.iter().any(|o| o.ends_with("rk3576-no-graphics.dtbo")));
        // Back to the default: ours goes, theirs stays, and the line survives.
        let plain = with_video_out(&headless, 0);
        assert_eq!(overlays(&plain).len(), 2);
        assert!(!plain.contains("no-graphics"));
    }

    /// A real entry, off the device, kept verbatim: the leading comment,
    /// kernel-install's own spacing and a full UUID, none of which the shortened
    /// fixture above has.
    const REAL: &str = r#"# Boot Loader Specification type#1 entry (Flipper One)
title      Desktop 7.2.0-ge8750f615eb
version    7.2.0-ge8750f615ebf
sort-key   debian-1100-Desktop-0
options    root=UUID=fe225468-8170-4391-bd08-934eced9ea38 audit=0 console=tty1 console=ttyS0,1500000n8 console=ttyS4,1500000n8 fbcon=map:1 rootflags=subvol=@Desktop flipper.entry=900-flipperos-Desktop-7.2.0-ge8750f615ebf
linux      /@Desktop/usr/lib/modules/7.2.0-ge8750f615ebf/vmlinuz
devicetreedir /@Desktop/usr/lib/linux-image-7.2.0-ge8750f615ebf
initrd     /@Desktop/usr/lib/modules/7.2.0-ge8750f615ebf/initrd
"#;

    /// The transformation run against that, so what a keypress writes to a file
    /// which then has to boot is read before it ever does.
    #[test]
    fn a_real_entry_survives_the_round_trip() {
        let real = REAL;
        let headless = with_video_out(real, 2);
        // The overlay goes in the loader's namespace, like every other path in the
        // file: U-Boot reads the filesystem's top level, so a path without the
        // subvolume in front of it names nothing.
        assert!(headless.contains(
            "devicetree-overlay /@Desktop/usr/lib/linux-image-7.2.0-ge8750f615ebf/\
             rockchip/rk3576-no-graphics.dtbo"
                .replace("             ", "")
                .as_str()
        ));
        let back = with_video_out(&headless, 0);
        assert_eq!(back, real, "the round trip did not land back where it started");
    }

    /// A profile's other kernels get the same answer, each under its own tree
    /// directory, and a BSP entry is left alone: these overlays are nodes of our own
    /// tree, so a 6.1 entry would name a file that does not exist and fail to boot
    /// something that boots today.
    #[test]
    fn every_modern_entry_is_written_and_the_old_ones_are_not() {
        let conf = |file: &str, version: &str, dir: &str| Conf {
            file: file.into(),
            version: version.into(),
            dtdir: dir.into(),
            ..Default::default()
        };
        let p = Profile {
            entries: vec![
                conf("900-a.conf", "7.2.0-new", "/@Desktop/usr/lib/linux-image-7.2.0-new")
                    .entry(),
                conf("900-b.conf", "7.1.0-old", "/@Desktop/usr/lib/linux-image-7.1.0-old")
                    .entry(),
                conf("900-c.conf", "6.1.172", "/@Desktop/usr/lib/linux-image-6.1.172").entry(),
            ],
            ..Default::default()
        };
        let written: Vec<&str> = p
            .entries
            .iter()
            .filter(|e| version_at_least(&e.version, MIN_KERNEL))
            .map(|e| e.file.as_str())
            .collect();
        assert_eq!(written, vec!["900-a.conf", "900-b.conf"], "the BSP entry was written");

        // Each takes the overlay from under its own kernel, not the first one's.
        for entry in &p.entries {
            let text = format!("devicetreedir {}\n", entry.dtdir);
            let out = with_video_out(&text, 2);
            assert!(
                out.contains(&format!("{}/rockchip/rk3576-no-graphics.dtbo", entry.dtdir)),
                "{out}"
            );
        }
    }

    /// What the row reads back, which has to agree with what it wrote.
    #[test]
    fn an_entry_says_which_one_it_is_on() {
        let entry = |text: &str| parse_conf("900-x.conf", text).expect("conf").entry();
        assert_eq!(video_out_of(&entry(ENTRY)), 0);
        assert_eq!(video_out_of(&entry(&with_video_out(ENTRY, 1))), 1);
        assert_eq!(video_out_of(&entry(&with_video_out(ENTRY, 2))), 2);
        // A user drop-in of the same name is theirs, and is not this row's answer.
        let mine = ENTRY.replace(
            "initrd",
            "devicetree-overlay /etc/kernel/dtbo/rk3576-no-graphics.dtbo\ninitrd",
        );
        assert_eq!(video_out_of(&entry(&mine)), 0);
    }
}
