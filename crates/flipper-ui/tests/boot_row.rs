//! What a boot menu row can hold.
//!
//! The panel is 256 pixels wide, and a row draws a profile's name, a heart or a medium
//! badge after it, and the status right-aligned against the other edge. Whether the
//! last of those still fits is arithmetic on the real font tables rather than a guess,
//! and the failure is silent: the two texts are drawn from opposite edges and simply
//! overlap.
//!
//! Everything here is measured in HaxrCorp 4090, which is what a boot row is drawn
//! in: this screen keeps one font in every state.

use flipper_ui::boot::Medium;
use flipper_ui::boot_menu::status_fits;

/// A factory profile, and a derived one showing its label in brackets: the long names
/// are the ones a user makes.
const NAME: &str = "Desktop";
const DERIVED: &str = "[Desktop Before upgrade]";

#[test]
fn an_ordinary_row_has_room_for_its_status() {
    assert!(status_fits(NAME, "Used 11 months ago", false, Medium::Internal));
    assert!(status_fits(NAME, "Running", true, Medium::Internal));

    // A card's row carries the wider badge and is still fine at this width.
    assert!(status_fits(NAME, "Running", false, Medium::Sd));
    assert!(status_fits(DERIVED, "Running", false, Medium::Internal));
}

/// A long name is what runs out of width, and then the status goes rather than being
/// drawn over.
///
/// Written as the boundary rather than as a chosen string: the widest row that fits
/// must stop fitting when anything more is drawn on it, whatever the font tables
/// happen to measure.
#[test]
fn the_status_goes_when_the_name_does_not_leave_room() {
    let status = "Used 3 hours ago";
    let mut name = String::from("[Desktop");
    while status_fits(&name, status, false, Medium::Internal) {
        name.push('o');
    }
    // One character back is the widest name that fits beside this status.
    name.pop();
    assert!(status_fits(&name, status, false, Medium::Internal));

    // The heart and the medium badge sit between the name and the status, so each of
    // them is width as much as a character is.
    assert!(
        !status_fits(&name, status, true, Medium::Internal),
        "the heart has to fit too"
    );
    assert!(
        !status_fits(&name, status, false, Medium::Sd),
        "and so does the wider medium badge"
    );
}

/// A profile never booted has no status at all, which always fits.
#[test]
fn a_row_with_nothing_to_say_always_fits() {
    assert!(status_fits(DERIVED, "", true, Medium::Sd));
}
