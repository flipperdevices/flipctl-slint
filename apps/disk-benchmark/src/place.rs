//! Where the test file goes, and putting everything back afterwards.
//!
//! A partition nobody mounted is mounted under `/run/disk-benchmark` for the run and
//! unmounted after it. The file goes in a folder of its own at the top of the
//! filesystem, or in the home folder when that is on it; where the top belongs to
//! root, the folder is made through sudo and handed to us, so the test itself runs
//! as the user. Only `mount`, `umount`, `install`, `rmdir` and `mkdir` are ever run
//! through sudo, and nothing is ever deleted there but that one folder, emptied first.

use std::ffi::CString;
use std::fs;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::disks::{size, Part};

const FOLDER: &str = ".disk-benchmark";
const MOUNTS: &str = "/run/disk-benchmark";
/// Left free beyond the file, so the filesystem's own metadata has room.
pub const MARGIN: u64 = 64 << 20;
/// Filesystems that have no owners and take theirs from the mount.
const OWNERLESS: &[&str] = &["vfat", "exfat", "ntfs", "ntfs3", "msdos"];

pub struct Place {
    pub file: PathBuf,
    dir: PathBuf,
    /// The dir was made through sudo, in a folder only root may change.
    dir_by_root: bool,
    mounted: Option<PathBuf>,
}

fn sudo(args: &[&str]) -> Result<(), String> {
    let out = Command::new("sudo")
        .arg("-n")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sudo: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let line = err.lines().find(|l| !l.trim().is_empty()).unwrap_or("failed").trim();
    Err(format!("{}: {line}", args[0]))
}

/// Bytes free to us, and bytes in all, in the filesystem holding `path`.
pub fn space(path: &Path) -> io::Result<(u64, u64)> {
    let c = CString::new(path.as_os_str().as_bytes())?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let block = st.f_frsize as u64;
    Ok((st.f_bavail as u64 * block, st.f_blocks as u64 * block))
}

/// Where in the partition's mounts the folder goes: the home folder when the
/// partition holds it, else the first mount.
fn base(part: &Part, home: &Path) -> Option<PathBuf> {
    if part.mounts.iter().any(|m| home.starts_with(m)) {
        return Some(home.to_path_buf());
    }
    part.mounts.first().cloned()
}

/// Free and total bytes on a partition nobody has mounted: mounted read-only for
/// the moment it takes to ask, at the place a run would mount it.
pub fn probe(part: &Part) -> Result<(u64, u64), String> {
    let point = Path::new(MOUNTS).join(&part.name);
    let point_s = point.to_string_lossy().to_string();
    sudo(&["mkdir", "-p", &point_s])?;
    let dev = format!("/dev/{}", part.name);
    let mounted = sudo(&["mount", "-o", "ro,noatime", &dev, &point_s]);
    let space = mounted.clone().and_then(|()| space(&point).map_err(|e| e.to_string()));
    if mounted.is_ok() {
        let _ = sudo(&["umount", &point_s]);
    }
    let _ = sudo(&["rmdir", &point_s]);
    space
}

impl Place {
    /// Mount if need be, make the folder, and check `bytes` fit.
    pub fn make(part: &Part, home: &Path, bytes: u64) -> Result<Place, String> {
        let mut place = Place { file: PathBuf::new(), dir: PathBuf::new(), dir_by_root: false, mounted: None };
        let base = match base(part, home) {
            // Ours, left by a run that was killed: it is unmounted this time.
            Some(base) if base.starts_with(MOUNTS) => {
                place.mounted = Some(base.clone());
                base
            }
            Some(base) => base,
            None => {
                let point = Path::new(MOUNTS).join(&part.name);
                let point_s = point.to_string_lossy().to_string();
                sudo(&["mkdir", "-p", &point_s])?;
                let mut options = "noatime".to_string();
                if OWNERLESS.contains(&part.fs.as_str()) {
                    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                    options += &format!(",uid={uid},gid={gid}");
                }
                let dev = format!("/dev/{}", part.name);
                if let Err(e) = sudo(&["mount", "-o", &options, &dev, &point_s]) {
                    let _ = sudo(&["rmdir", &point_s]);
                    return Err(e);
                }
                place.mounted = Some(point.clone());
                point
            }
        };
        if let Err(e) = place.folder(&base, bytes) {
            place.clean();
            return Err(e);
        }
        Ok(place)
    }

    fn folder(&mut self, base: &Path, bytes: u64) -> Result<(), String> {
        let dir = base.join(FOLDER);
        match fs::create_dir(&dir) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
                let dir_s = dir.to_string_lossy().to_string();
                sudo(&["install", "-d", "-o", &uid.to_string(), "-g", &gid.to_string(), &dir_s])?;
                self.dir_by_root = true;
            }
            Err(e) => return Err(format!("{}: {e}", dir.display())),
        }
        self.dir = dir.clone();
        self.file = dir.join("test.tmp");
        // A file left by a run that never cleaned up is ours to replace, and its space
        // is counted as free.
        let _ = fs::remove_file(&self.file);
        let (have, _) = space(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        if have < bytes + MARGIN {
            return Err(format!("Needs {} free, has {}", size(bytes + MARGIN), size(have)));
        }
        Ok(())
    }

    /// Delete the file and the folder, and unmount what we mounted.
    pub fn clean(&mut self) {
        if !self.file.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.file);
        }
        if !self.dir.as_os_str().is_empty() && fs::remove_dir(&self.dir).is_err() && self.dir_by_root {
            let _ = sudo(&["rmdir", &self.dir.to_string_lossy()]);
        }
        if let Some(point) = self.mounted.take() {
            let point = point.to_string_lossy().to_string();
            if sudo(&["umount", &point]).is_ok() {
                let _ = sudo(&["rmdir", &point]);
            }
        }
        self.file = PathBuf::new();
        self.dir = PathBuf::new();
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.clean();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(mounts: &[&str]) -> Part {
        Part {
            name: "sda3".into(),
            size: 1 << 40,
            fs: "btrfs".into(),
            label: String::new(),
            mounts: mounts.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn home_first_then_the_first_mount() {
        let home = Path::new("/home/user");
        assert_eq!(base(&part(&["/", "/home"]), home), Some(home.into()));
        assert_eq!(base(&part(&["/media/x", "/mnt"]), home), Some("/media/x".into()));
        assert_eq!(base(&part(&[]), home), None);
    }

    #[test]
    fn a_folder_in_a_mounted_place_is_made_and_removed() {
        let root = std::env::temp_dir().join(format!("disk-bench-place-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let p = part(&[root.to_str().unwrap()]);
        let mut place = Place::make(&p, Path::new("/nowhere"), 1 << 20).unwrap();
        assert_eq!(place.file, root.join(FOLDER).join("test.tmp"));
        fs::write(&place.file, b"x").unwrap();
        place.clean();
        assert!(!root.join(FOLDER).exists());
        let err = Place::make(&p, Path::new("/nowhere"), 1 << 60).err().unwrap();
        assert!(err.starts_with("Needs "), "{err}");
        assert!(!root.join(FOLDER).exists(), "a refused run leaves nothing");
        fs::remove_dir(&root).unwrap();
    }
}
