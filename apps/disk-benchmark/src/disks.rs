//! The disks there are to test, from sysfs, udev's database and the mount table.
//!
//! Everything here is a file anyone can read: `/sys/block` for the disks and their
//! partitions, `/run/udev/data` for the filesystem udev found on each (blkid itself
//! needs root to read a device), and `/proc/self/mountinfo` for where each is mounted.
//! A btrfs mount reports an anonymous device number there, so a mount is matched by
//! its source path as well as by number.
//!
//! A disk is listed when it has a filesystem the test file can go in, or when it is
//! big enough to be worth saying it has none. The UFS's boot LUNs (16 and 128 MiB,
//! no filesystem) are what the second rule keeps out.

use std::fs;
use std::path::{Path, PathBuf};

/// Smaller than this and with no filesystem, a disk is a firmware area, not a disk.
const SMALL: u64 = 1 << 30;

/// Filesystems a file cannot go in, or that cannot be written.
const UNUSABLE: &[&str] =
    &["swap", "crypto_LUKS", "LVM2_member", "linux_raid_member", "iso9660", "squashfs", "erofs", "udf"];

/// Where to read from: the real system, or a tree a test made.
pub struct Sys {
    pub block: PathBuf,
    pub udev: PathBuf,
    pub mountinfo: PathBuf,
}

impl Default for Sys {
    fn default() -> Self {
        Self {
            block: "/sys/block".into(),
            udev: "/run/udev/data".into(),
            mountinfo: "/proc/self/mountinfo".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Disk {
    pub name: String,
    /// What it is: UFS, SD card, eMMC, USB, NVMe, SATA.
    pub kind: &'static str,
    pub model: String,
    /// How it is connected, as fast as the bus says.
    pub link: String,
    pub size: u64,
    pub parts: Vec<Part>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub name: String,
    pub size: u64,
    pub fs: String,
    pub label: String,
    /// Where it is mounted writable.
    pub mounts: Vec<PathBuf>,
}

impl Disk {
    /// What the disk's choice row says.
    pub fn title(&self) -> String {
        format!("{} {}", self.kind, size(self.size))
    }

    /// The partition the test file goes on: one already mounted where it can, the one
    /// holding `home` first, then the largest with a filesystem.
    pub fn target(&self, home: &Path) -> Option<&Part> {
        let usable = || self.parts.iter().filter(|p| !p.fs.is_empty() && !UNUSABLE.contains(&p.fs.as_str()));
        usable()
            .filter(|p| p.mounts.iter().any(|m| home.starts_with(m)))
            .chain(usable().filter(|p| !p.mounts.is_empty()).max_by_key(|p| p.size))
            .chain(usable().max_by_key(|p| p.size))
            .next()
    }
}

/// Every disk worth listing, sorted by name.
pub fn scan(sys: &Sys) -> Vec<Disk> {
    let mounts = fs::read_to_string(&sys.mountinfo).map(|t| mountinfo(&t)).unwrap_or_default();
    let mut disks: Vec<Disk> = fs::read_dir(&sys.block)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| disk(sys, &e.file_name().to_string_lossy(), &mounts))
        .collect();
    disks.sort_by(|a, b| a.name.cmp(&b.name));
    disks
}

fn virtual_disk(name: &str) -> bool {
    ["loop", "ram", "zram", "dm-", "md", "sr", "nbd", "mtdblock", "fd"].iter().any(|p| name.starts_with(p))
        || (name.starts_with("mmcblk") && (name.contains("boot") || name.contains("rpmb")))
}

fn read(path: impl AsRef<Path>) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

fn disk(sys: &Sys, name: &str, mounts: &[Mount]) -> Option<Disk> {
    if virtual_disk(name) {
        return None;
    }
    let dir = sys.block.join(name);
    let size = read(dir.join("size")).parse::<u64>().ok()? * 512;
    if size == 0 || read(dir.join("ro")) == "1" {
        return None;
    }
    let real = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
    let path = real.to_string_lossy();
    let kind = if path.contains("/usb") {
        "USB"
    } else if name.starts_with("nvme") {
        "NVMe"
    } else if name.starts_with("mmcblk") {
        if read(dir.join("device/type")) == "MMC" { "eMMC" } else { "SD card" }
    } else if path.contains("ufs") {
        "UFS"
    } else if path.contains("/ata") {
        "SATA"
    } else {
        "Disk"
    };

    let mut parts: Vec<Part> = fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().join("partition").exists())
        .filter_map(|e| part(sys, &e.path(), mounts))
        .collect();
    parts.sort_by(|a, b| a.name.cmp(&b.name));
    // A filesystem on the whole disk, as a stick formatted without a table has.
    if parts.is_empty() {
        parts.extend(part(sys, &dir, mounts).filter(|p| !p.fs.is_empty()));
    }
    if parts.iter().all(|p| p.fs.is_empty()) && size < SMALL {
        return None;
    }

    let model = [read(dir.join("device/vendor")), read(dir.join("device/model")), read(dir.join("device/name"))]
        .into_iter()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Some(Disk { name: name.into(), kind, model, link: link(kind, &real), size, parts })
}

fn part(sys: &Sys, dir: &Path, mounts: &[Mount]) -> Option<Part> {
    let name = dir.file_name()?.to_string_lossy().to_string();
    let size = read(dir.join("size")).parse::<u64>().ok()? * 512;
    let dev = read(dir.join("dev"));
    let udev = read(sys.udev.join(format!("b{dev}")));
    let env = |key: &str| {
        udev.lines()
            .find_map(|l| l.strip_prefix("E:")?.strip_prefix(key)?.strip_prefix('='))
            .unwrap_or_default()
            .to_string()
    };
    let source = format!("/dev/{name}");
    let mounts = mounts
        .iter()
        .filter(|m| m.writable && (m.source == source || (m.dev == dev && !dev.is_empty())))
        .map(|m| m.point.clone())
        .collect();
    Some(Part { name, size, fs: env("ID_FS_TYPE"), label: env("ID_FS_LABEL"), mounts })
}

/// How fast the bus under a disk is, from whichever of its ancestors says.
fn link(kind: &str, real: &Path) -> String {
    let up = || real.ancestors().skip(1);
    match kind {
        "USB" => up()
            .find_map(|a| {
                a.join("idVendor").exists().then(|| read(a.join("speed")))?.parse::<u32>().ok()
            })
            .map(|mbps| match mbps {
                m if m >= 1000 => format!("USB {} Gbps", m / 1000),
                m => format!("USB {m} Mbps"),
            })
            .unwrap_or_else(|| "USB".into()),
        "UFS" => up()
            .find(|a| a.join("power_info/gear").exists())
            .map(|a| {
                let p = |f: &str| read(a.join("power_info").join(f));
                let gear = p("gear").trim_start_matches("HS_GEAR_").to_string();
                format!("UFS G{} x{}", gear, p("lane").trim_start_matches("LANE_"))
            })
            .unwrap_or_else(|| "UFS".into()),
        "NVMe" => up()
            .find(|a| a.join("current_link_speed").exists())
            .map(|a| {
                let speed = read(a.join("current_link_speed"));
                let speed = speed.split(" PCIe").next().unwrap_or_default().to_string();
                format!("PCIe {speed} x{}", read(a.join("current_link_width")))
            })
            .unwrap_or_else(|| "PCIe".into()),
        k => k.into(),
    }
}

struct Mount {
    dev: String,
    point: PathBuf,
    source: String,
    writable: bool,
}

fn mountinfo(text: &str) -> Vec<Mount> {
    text.lines()
        .filter_map(|line| {
            let (head, tail) = line.split_once(" - ")?;
            let f: Vec<&str> = head.split(' ').collect();
            let t: Vec<&str> = tail.split(' ').collect();
            let options = f.get(5)?;
            Some(Mount {
                dev: f.get(2)?.to_string(),
                point: unescape(f.get(4)?).into(),
                source: t.get(1)?.to_string(),
                writable: options.split(',').any(|o| o == "rw"),
            })
        })
        .collect()
}

/// A mount point as the kernel writes it: space, tab, newline and backslash in octal.
fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(at) = rest.find('\\') {
        out.push_str(&rest[..at]);
        let code = rest.get(at + 1..at + 4).and_then(|o| u8::from_str_radix(o, 8).ok());
        match code {
            Some(c) => {
                out.push(c as char);
                rest = &rest[at + 4..];
            }
            None => {
                out.push('\\');
                rest = &rest[at + 1..];
            }
        }
    }
    out + rest
}

/// Bytes as lsblk says them: binary units, one decimal under 100.
pub fn size(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < units.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 || v >= 100.0 {
        format!("{:.0}{}", v, units[u])
    } else {
        format!("{:.1}{}", v, units[u])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, text: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    /// The test Flipper as it was on 2026-11-05: the UFS with its root btrfs and three
    /// boot LUNs, an SD card nobody mounted, and a USB stick on a 5 Gbps port.
    fn flipper(root: &Path) -> Sys {
        let devices = root.join("devices");
        let block = root.join("block");
        let udev = root.join("udev");
        fs::create_dir_all(&block).unwrap();
        let add = |name: &str, under: &str, sectors: u64, dev: &str, parts: &[(&str, u64, &str, &str)]| {
            let real = devices.join(under).join("block").join(name);
            write(&real.join("size"), &sectors.to_string());
            write(&real.join("ro"), "0");
            write(&real.join("dev"), dev);
            for (i, (p, sectors, fs, label)) in parts.iter().enumerate() {
                let d = real.join(p);
                write(&d.join("partition"), &(i + 1).to_string());
                write(&d.join("size"), &sectors.to_string());
                let pdev = format!("{}:{}", dev.split(':').next().unwrap(), i + 1 + dev.split(':').nth(1).unwrap().parse::<usize>().unwrap());
                write(&d.join("dev"), &pdev);
                if !fs.is_empty() {
                    write(&udev.join(format!("b{pdev}")), &format!("E:ID_FS_TYPE={fs}\nE:ID_FS_LABEL={label}\n"));
                }
            }
            std::os::unix::fs::symlink(&real, block.join(name)).unwrap();
            real
        };
        let ufs = "platform/2a2d0000.ufshc/host0/target0:0:0";
        let sda = add("sda", &format!("{ufs}/0:0:0:0"), 123_928_576, "8:0", &[
            ("sda1", 122_880, "vfat", "boot"),
            ("sda2", 8192, "", ""),
            ("sda3", 123_797_504, "btrfs", "flipperos"),
        ]);
        write(&sda.join("device/vendor"), "SKhynix");
        write(&sda.join("device/model"), "BWU2A0516B064G");
        write(&devices.join("platform/2a2d0000.ufshc/power_info/gear"), "HS_GEAR_3");
        write(&devices.join("platform/2a2d0000.ufshc/power_info/lane"), "2");
        add("sdb", &format!("{ufs}/0:0:0:1"), 32_768, "8:16", &[]);
        add("sdd", &format!("{ufs}/0:0:0:3"), 262_144, "8:48", &[]);
        let sd = add("mmcblk0", "platform/2a310000.mmc/mmc_host/mmc0/mmc0:aaaa", 62_333_952, "179:0", &[
            ("mmcblk0p1", 62_267_392, "exfat", "SDCARD"),
            ("mmcblk0p2", 65_536, "", ""),
        ]);
        write(&sd.join("device/type"), "SD");
        write(&sd.join("device/name"), "SN32G");
        add("mmcblk0boot0", "platform/2a310000.mmc/mmc_host/mmc0/mmc0:aaaa", 8192, "179:8", &[]);
        let usb = "platform/usb/xhci/usb2/2-1";
        write(&devices.join(usb).join("idVendor"), "0781");
        write(&devices.join(usb).join("speed"), "5000");
        add("sde", &format!("{usb}/2-1:1.0/host1/target1:0:0/1:0:0:0"), 60_062_500, "8:64", &[("sde1", 60_060_000, "ext4", "")]);
        add("loop0", "virtual", 1000, "7:0", &[]);

        let mountinfo = root.join("mountinfo");
        write(&mountinfo, "\
24 1 0:22 /@ / rw,noatime shared:1 - btrfs /dev/sda3 rw,compress=zstd:1
25 24 0:22 /@home /home rw,noatime shared:2 - btrfs /dev/sda3 rw
26 24 8:1 / /boot/efi ro,noatime shared:3 - vfat /dev/sda1 ro
27 24 8:65 / /media/user/My\\040Stick rw,nosuid shared:4 - ext4 /dev/sde1 rw
");
        Sys { block, udev, mountinfo }
    }

    #[test]
    fn the_flipper_as_it_was() {
        let dir = std::env::temp_dir().join(format!("disk-bench-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let disks = scan(&flipper(&dir));
        let names: Vec<_> = disks.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["mmcblk0", "sda", "sde"], "no boot LUNs, no mmc boot areas, no loop");

        let (sd, ufs, usb) = (&disks[0], &disks[1], &disks[2]);
        assert_eq!((sd.kind, sd.model.as_str(), sd.title().as_str()), ("SD card", "SN32G", "SD card 29.7G"));
        assert_eq!(ufs.link, "UFS G3 x2");
        assert_eq!(ufs.model, "SKhynix BWU2A0516B064G");
        assert_eq!(usb.link, "USB 5 Gbps");

        let home = Path::new("/home/user");
        let t = ufs.target(home).unwrap();
        assert_eq!(t.name, "sda3", "the btrfs holding home, though / and /home are subvolumes");
        assert_eq!(t.mounts, [PathBuf::from("/"), PathBuf::from("/home")]);
        let t = sd.target(home).unwrap();
        assert_eq!((t.name.as_str(), t.fs.as_str(), t.mounts.len()), ("mmcblk0p1", "exfat", 0));
        assert_eq!(usb.target(home).unwrap().mounts, [PathBuf::from("/media/user/My Stick")]);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_blank_disk_has_no_target() {
        let disk = Disk {
            name: "sdf".into(),
            kind: "USB",
            model: String::new(),
            link: String::new(),
            size: 8 << 30,
            parts: vec![Part { name: "sdf1".into(), size: 8 << 30, fs: "swap".into(), label: String::new(), mounts: vec![] }],
        };
        assert_eq!(disk.target(Path::new("/home/user")), None);
    }

    #[test]
    fn sizes() {
        assert_eq!(size(512), "512B");
        assert_eq!(size(16 << 20), "16.0M");
        assert_eq!(size(63_447_793_664), "59.1G");
        assert_eq!(size(256 << 30), "256G");
    }
}
