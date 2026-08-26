//! Shared UI component library for the Flipper One 256x144 SPI panel.
//!
//! Three consumers: flipperos-installer (size-critical, maskrom-loaded),
//! flipper-boot-menu (Falcon initramfs) and flipctl, the system launcher.

pub mod font;
pub mod key;
pub mod layout;
pub mod paint;
pub mod pixel;

/// Design tokens, generated from tokens.toml at build time.
#[allow(dead_code)]
pub mod theme {
    include!(concat!(env!("OUT_DIR"), "/theme.rs"));
}

pub use font::BitmapFont;
pub use key::{FlipperKey, KeyEvent};
pub use paint::Surface;
pub use pixel::{Gray8, Rect};
pub use theme::{PANEL_H, PANEL_W};
