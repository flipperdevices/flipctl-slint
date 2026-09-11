//! The panel sink: DRM/KMS straight to the Flipper One display.
//!
//! Talks to the DRM device through the `drm` crate and nothing else. No
//! libinput, no libudev, no libxkbcommon: the panel has one fixed mode and the
//! buttons are a handful of keys, so none of that tree earns its place in an
//! initramfs.
//!
//! `flipper-one-display.c` takes `DRM_FORMAT_Y8`, the panel's own 8-bit luminance,
//! so a commit is a row copy of our greyscale frame into the dumb buffer and the
//! kernel's part is a memcpy. On a kernel without Y8 the sink falls back to
//! `DRM_FORMAT_XRGB8888` and expands each pixel for the driver to reduce again.
//! Measured on the same kernel at 33MHz, 300 frames: Y8 commits in 10.0ms, XRGB8888
//! in 12.8ms, against 9.2ms on the wire. Damage is tracked and reported, but the
//! driver clips to the whole framebuffer and re-transmits all 37152 SPI bytes on
//! every atomic update, so it currently only lets us skip a commit entirely when
//! nothing moved.

use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::dumbbuffer::DumbBuffer;
use drm::control::{connector, crtc, framebuffer, Device as ControlDevice, Mode};
use drm::Device as DrmDevice;

use crate::pixel::Rect;
use crate::platform::{Frame, FrameSink};

/// The driver name `flipper-one-display.c` registers.
const DRIVER: &str = "flipper_one_display";

/// How long a start-up waits to be DRM master before giving up.
const MASTER_WAIT: Duration = Duration::from_secs(15);

struct Card(File);

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl DrmDevice for Card {}
impl ControlDevice for Card {}

/// `DRM_FORMAT_Y8`: fourcc `GREY`, 8-bit luminance, one plane. Spelt out because the
/// drm-fourcc crate flipctl builds against has no name for it yet.
const Y8: u32 = u32::from_le_bytes(*b"GREY");

pub struct KmsSink {
    /// Whether the panel is somebody else's right now.
    detached: bool,
    card: Card,
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode: Mode,
    buffer: DumbBuffer,
    /// True when the framebuffer is Y8, so a row is copied rather than expanded.
    greyscale: bool,
    fb: framebuffer::Handle,
    modeset_done: bool,
    flush: Flush,
}

/// How to make the driver transmit a frame we have already written into the
/// mapped dumb buffer.
///
/// `DIRTYFB` is the right answer and what every other mipi-dbi tiny driver
/// implements via `drm_atomic_helper_dirtyfb`. `flipper-one-display.c` sets
/// `.fb_create = drm_gem_fb_create` and no `dirty_fb`, so the ioctl returns
/// ENOSYS on this panel. We probe once and remember.
///
/// The fallback re-runs `set_crtc` with the same mode and framebuffer. Mode and
/// fb are unchanged so the atomic helpers set no `mode_changed` and there is no
/// disable/enable cycle; `fo_crtc_check` calls `drm_atomic_add_affected_planes`,
/// which pulls the plane into the commit, so `fo_plane_atomic_update` runs and
/// writes the SPI buffer. Same effect, one extra ioctl.
#[derive(Copy, Clone, PartialEq, Eq)]
enum Flush {
    Probe,
    DirtyFb,
    SetCrtc,
}

impl KmsSink {
    /// Open the panel.
    ///
    /// `explicit` is honoured when given. Otherwise every `/dev/dri/card*` is
    /// probed and the one whose driver name is `flipper_one_display` wins.
    /// Auto-detection matters because card numbers are renumbered by kernel
    /// rebuilds, which has already broken flipctl once; a by-path symlink is
    /// stable but only if the SPI address never changes.
    pub fn open(explicit: Option<&Path>) -> std::io::Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => Self::find_panel()?,
        };

        let card = Self::open_as_master(&path, MASTER_WAIT)?;

        let name = card.get_driver()?.name;
        if explicit.is_none() && name != DRIVER {
            return Err(std::io::Error::other(format!(
                "{} is driven by {name:?}, expected {DRIVER:?}",
                path.display()
            )));
        }

        let resources = card.resource_handles()?;

        // The panel is a fixed-mode SPI connector, so take the first connected
        // one and its first mode rather than scoring a list.
        let (connector, mode) = resources
            .connectors()
            .iter()
            .filter_map(|handle| card.get_connector(*handle, false).ok())
            .find(|info| info.state() == connector::State::Connected)
            .and_then(|info| info.modes().first().copied().map(|m| (info.handle(), m)))
            .ok_or_else(|| std::io::Error::other("no connected connector with a mode"))?;

        let crtc = *resources.crtcs().first().ok_or_else(|| std::io::Error::other("no CRTC"))?;

        let (w, h) = mode.size();

        // Y8 when the kernel takes it, XRGB8888 otherwise.
        //
        // The panel is 8-bit greyscale and so is everything this crate renders, so
        // an XRGB8888 buffer means expanding every pixel to 32bpp (147KB of writes
        // per frame) for the driver to reduce it again with a per-pixel conversion,
        // while a one-byte format makes the commit a row copy and the kernel's part
        // a memcpy. DRM_FORMAT_Y8 is that format by its right name: luminance, which
        // is what the panel takes, rather than R8's red channel. The driver advertises
        // it since `drm/tiny: flipper-one-display: add support for Y8 format`.
        //
        // A kernel without it fails the ADDFB2 below and the sink falls back to
        // XRGB8888, so one binary runs on either and is merely slower on the old one.
        // `FLIPPER_FB_FORMAT=xrgb` forces the fallback, for measuring against it.
        //
        // The framebuffer is added through the ffi crate rather than the safe one:
        // the drm crate names formats with an enum that predates Y8, and there is no
        // way past it. The dumb buffer itself is format-agnostic, one byte per pixel
        // is one byte per pixel, so it is created with the enum's R8 and the kernel is
        // told the truth when the framebuffer is made from it.
        let want_y8 =
            !std::env::var("FLIPPER_FB_FORMAT").is_ok_and(|v| v.eq_ignore_ascii_case("xrgb"));

        let (mut buffer, fb, greyscale) = match card
            .create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::R8, 8)
            .and_then(|b| {
                if want_y8 {
                    Ok(b)
                } else {
                    Err(std::io::Error::other("XRGB8888 asked for"))
                }
            })
            .and_then(|b| {
                let handle = u32::from(Buffer::handle(&b));
                let made = drm_ffi::mode::add_fb2(
                    card.as_fd(),
                    u32::from(w),
                    u32::from(h),
                    Y8,
                    &[handle, 0, 0, 0],
                    &[Buffer::pitch(&b), 0, 0, 0],
                    &[0; 4],
                    &[0; 4],
                    0,
                )?;
                drm::control::from_u32::<framebuffer::Handle>(made.fb_id)
                    .ok_or_else(|| std::io::Error::other("kernel returned framebuffer 0"))
                    .map(|fb| (b, fb))
            }) {
            Ok((b, fb)) => (b, fb, true),
            Err(_) => {
                let b =
                    card.create_dumb_buffer((u32::from(w), u32::from(h)), DrmFourcc::Xrgb8888, 32)?;
                let fb = card.add_framebuffer(&b, 24, 32)?;
                (b, fb, false)
            }
        };

        // Start from a known frame: an uninitialised dumb buffer is whatever the
        // allocator left behind, and on a device with no vblank that garbage
        // would be visible until the first real commit.
        {
            let mut map = card.map_dumb_buffer(&mut buffer)?;
            map.as_mut().fill(0xff);
        }

        Ok(Self {
            detached: false,
            card,
            crtc,
            connector,
            mode,
            buffer,
            fb,
            greyscale,
            modeset_done: false,
            flush: Flush::Probe,
        })
    }

    fn find_panel() -> std::io::Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("card"))
            })
            .collect();
        candidates.sort();

        for path in &candidates {
            let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
                continue;
            };
            if Card(file).get_driver().is_ok_and(|d| d.name == DRIVER) {
                return Ok(path.clone());
            }
        }

        Err(std::io::Error::other(format!(
            "no /dev/dri/card* is driven by {DRIVER:?} (tried {})",
            candidates.len()
        )))
    }

    pub fn size(&self) -> (u16, u16) {
        self.mode.size()
    }
}

impl KmsSink {
    /// The framebuffer format in use, for the startup log. Y8 means the driver
    /// took the panel's own format and the commit is a row copy.
    pub fn format(&self) -> &'static str {
        if self.greyscale {
            "Y8"
        } else {
            "XRGB8888"
        }
    }
}

impl FrameSink for KmsSink {
    fn commit(&mut self, frame: Frame<'_>, damage: Rect) -> std::io::Result<()> {
        if damage.is_empty() && self.modeset_done {
            return Ok(());
        }

        let (w, h) = self.mode.size();
        if (frame.w, frame.h) != (w, h) {
            return Err(std::io::Error::other(format!(
                "frame is {}x{}, panel is {w}x{h}",
                frame.w, frame.h
            )));
        }

        let pitch = self.buffer.pitch() as usize;
        {
            let mut map = self.card.map_dumb_buffer(&mut self.buffer)?;
            let dst = map.as_mut();

            for (row, pixels) in (damage.y..damage.y + damage.h).zip(frame.rows(damage)) {
                if self.greyscale {
                    // One byte per pixel, so the row *is* the row: `Gray8` is
                    // `repr(transparent)` over `u8`, so this is a memcpy rather than
                    // the 36864 single-byte writes it used to be.
                    let start = usize::from(row) * pitch + usize::from(damage.x);
                    let bytes = crate::pixel::as_bytes(pixels);
                    dst[start..start + bytes.len()].copy_from_slice(bytes);
                } else {
                    let start = usize::from(row) * pitch + usize::from(damage.x) * 4;
                    let words = &mut dst[start..start + pixels.len() * 4];
                    for (word, px) in words.chunks_exact_mut(4).zip(pixels) {
                        word.copy_from_slice(&px.to_xrgb8888().to_le_bytes());
                    }
                }
            }
        }

        if !self.modeset_done {
            self.set_crtc()?;
            self.modeset_done = true;
            return Ok(());
        }

        if self.flush != Flush::SetCrtc {
            // The driver ignores the clip list and retransmits the whole frame,
            // but reporting real damage costs nothing and is correct the day it
            // stops.
            let clip = drm::control::ClipRect::new(
                damage.x,
                damage.y,
                damage.x + damage.w,
                damage.y + damage.h,
            );
            match self.card.dirty_framebuffer(self.fb, &[clip]) {
                Ok(()) => {
                    self.flush = Flush::DirtyFb;
                    return Ok(());
                }
                Err(e) if self.flush == Flush::Probe && is_unsupported(&e) => {
                    self.flush = Flush::SetCrtc;
                }
                Err(e) => return Err(e),
            }
        }

        self.set_crtc()
    }
}

impl KmsSink {
    /// Let go of the panel, so something else can drive it.
    ///
    /// One client owns a card at a time. Releasing the lock is what lets the
    /// kernel's framebuffer console, or a program that speaks KMS itself, paint
    /// the panel while flipctl waits.
    pub fn detach(&mut self) -> std::io::Result<()> {
        self.detached = true;
        self.card.release_master_lock()
    }

    /// Whether the panel has been let go of and not taken back.
    ///
    /// Asked every iteration by the caller, because the panel having an owner is an
    /// invariant rather than something to remember at five call sites: forgetting it
    /// once leaves flipctl rendering frames into a framebuffer nothing is showing,
    /// with no error anywhere, which is indistinguishable from a frozen device.
    pub fn is_detached(&self) -> bool {
        self.detached
    }

    /// Open the card as its DRM master, waiting for at most `limit`.
    ///
    /// A fd becomes master at open when nobody holds master. Otherwise SET_MASTER is
    /// EACCES for that fd for good: without CAP_SYS_ADMIN the kernel grants it only to a
    /// fd that was master before. So EACCES means somebody held the card when it was
    /// opened, and the only way forward is to close and open again once they are gone.
    /// EBUSY is a fd that was master and lost it, which takes it back when the holder
    /// drops it. Both are waited out here, before any buffer is created on the fd, since a
    /// buffer belongs to the fd that made it.
    ///
    /// The wait exists because logind activates this session a moment after the unit
    /// starts, and it was seen to last 10s once in 19 boots for a reason the kernel log
    /// did not show. Giving up costs a restart that re-pays the whole start-up, so the
    /// error says what was seen: the card, how long, and which refusal.
    fn open_as_master(path: &Path, limit: Duration) -> std::io::Result<Card> {
        let began = Instant::now();
        let mut card = Card(OpenOptions::new().read(true).write(true).open(path)?);
        let mut waited = false;
        loop {
            let err = match card.acquire_master_lock() {
                Ok(()) => {
                    if waited {
                        crate::logline!(
                            "panel          DRM master granted after {}ms",
                            began.elapsed().as_millis()
                        );
                    }
                    return Ok(card);
                }
                Err(e) => e,
            };
            let never_ours = err.kind() == std::io::ErrorKind::PermissionDenied;
            if began.elapsed() >= limit {
                return Err(std::io::Error::new(
                    err.kind(),
                    format!(
                        "{}: no DRM master after {}ms: {err}{}",
                        path.display(),
                        began.elapsed().as_millis(),
                        if never_ours { " (another client held master at every open)" } else { "" }
                    ),
                ));
            }
            if !waited {
                crate::logline!(
                    "panel          waiting for DRM master on {} ({err})",
                    path.display()
                );
                waited = true;
            }
            std::thread::sleep(Duration::from_millis(100));
            if never_ours {
                card = Card(OpenOptions::new().read(true).write(true).open(path)?);
            }
        }
    }

    /// Take it back, and put our own frame up again.
    ///
    /// Whoever had the panel in the meantime left something else on it, and the
    /// panel has no scanout: it shows the last frame that was written until a
    /// commit writes another. So the modeset is not optional here.
    pub fn attach(&mut self) -> std::io::Result<()> {
        let began = std::time::Instant::now();
        self.card.acquire_master_lock()?;
        let acquired = began.elapsed();
        self.detached = false;
        let result = self.set_crtc();
        // Timed because taking the panel back is on the critical path of every
        // switch, and a modeset on this panel is not obviously cheap: the driver
        // re-runs whatever the controller needs on enable.
        eprintln!(
            "panel          reclaimed in {}ms (master {}, modeset {})",
            began.elapsed().as_millis(),
            acquired.as_millis(),
            (began.elapsed() - acquired).as_millis()
        );
        result
    }

    fn set_crtc(&self) -> std::io::Result<()> {
        self.card.set_crtc(self.crtc, Some(self.fb), (0, 0), &[self.connector], Some(self.mode))
    }

    /// Which flush path the panel ended up on. Reported by the demo so a driver
    /// gaining `dirty_fb` is visible rather than silently unused.
    pub fn flush_path(&self) -> &'static str {
        match self.flush {
            Flush::Probe => "unprobed",
            Flush::DirtyFb => "DIRTYFB",
            Flush::SetCrtc => "set_crtc (driver has no dirty_fb)",
        }
    }
}

/// ENOSYS or EOPNOTSUPP: the driver does not implement the ioctl at all, as
/// opposed to rejecting these particular arguments.
fn is_unsupported(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(38) | Some(95))
}
