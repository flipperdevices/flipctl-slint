//! The rename field.
//!
//! The panel opens its on-screen keyboard for this, because a d-pad is what it has.
//! A terminal has a keyboard already, so it gets a field to type in and none of the
//! grid: the same answer reaches `BootMenu::renamed` either way.
//!
//! And the same gate. The panel refuses to save while `boot::rename_warning` has
//! something to say, so this does too, and says the same thing under the field: a
//! name typed here that the panel would have rejected must not get through because it
//! arrived by the other door.

use cursive::event::Key;
use cursive::reexports::crossbeam_channel::Sender;
use cursive::view::{Nameable, Resizable};
use cursive::views::{Dialog, EditView, LinearLayout, OnEventView, TextView};
use cursive::Cursive;

use crate::boot::{rename_warning, Profile};

use super::TerminalEvent;

/// The field's name, which is also how the handle asks whether the dialog is up.
pub const FIELD: &str = "rename-field";
const WARNING: &str = "rename-warning";

/// What a name has to get past, snapshotted when the dialog opens.
///
/// A copy rather than a borrow: the profiles belong to the loop that owns the menu,
/// and this runs on the cursive thread.
pub struct Rules {
    /// The profile being renamed, which is not a clash with itself.
    pub being: Profile,
    pub profiles: Vec<Profile>,
}

impl Rules {
    fn check(&self, text: &str) -> String {
        rename_warning(text, &self.being, &self.profiles)
    }
}

/// A dialog seeded with the profile's current label.
///
/// Enter or Save answers with the text, Esc or Cancel answers with nothing, and
/// either way the layer goes. The catch-all is what keeps the soft keys out: `z x c v
/// b` are letters somebody is trying to type, and a function key pressed by habit
/// must not reach the menu underneath while a name is half-written.
pub fn rename_dialog(
    title: &str,
    seed: &str,
    rules: Rules,
    tx: Sender<TerminalEvent>,
) -> impl cursive::View {
    let rules = std::sync::Arc::new(rules);

    // Refuses rather than closes while the name is one the panel would not take.
    // The warning is already on screen, so there is nothing more to say here.
    let submit = {
        let (rules, tx) = (rules.clone(), tx.clone());
        move |siv: &mut Cursive, text: &str| {
            if !rules.check(text).is_empty() {
                return;
            }
            siv.pop_layer();
            let _ = tx.send(TerminalEvent::Renamed(Some(text.to_string())));
        }
    };
    let on_submit = submit.clone();
    let on_save = move |siv: &mut Cursive| {
        let text = siv
            .call_on_name(FIELD, |field: &mut EditView| field.get_content())
            .map(|c| (*c).clone())
            .unwrap_or_default();
        submit(siv, &text);
    };
    let on_cancel = move |siv: &mut Cursive| {
        siv.pop_layer();
        let _ = tx.send(TerminalEvent::Renamed(None));
    };
    let on_escape = on_cancel.clone();

    let on_edit = {
        let rules = rules.clone();
        move |siv: &mut Cursive, text: &str, _: usize| {
            let warning = rules.check(text);
            siv.call_on_name(WARNING, |view: &mut TextView| view.set_content(warning));
        }
    };

    let body = LinearLayout::vertical()
        .child(
            EditView::new()
                .content(seed)
                .on_edit(on_edit)
                .on_submit(on_submit)
                .with_name(FIELD)
                .fixed_width(32),
        )
        .child(TextView::new(rules.check(seed)).with_name(WARNING));

    OnEventView::new(
        Dialog::around(body)
            .title(title)
            .button("Save", on_save)
            .button("Cancel", on_cancel),
    )
    .on_event(Key::Esc, on_escape)
    .on_event(cursive::event::EventTrigger::any(), |_| {})
}
