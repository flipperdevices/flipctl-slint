//! The on-screen keyboard: its ragged geometry, its navigation and the field.
//!
//! Geometry is checked against numbers read out of the prototype rather than out
//! of this port, because the whole point of the port is to land on the same
//! pixels: `drawKeyboard`'s 15px keys, 35px wide keys and left padding put `q`
//! over `a` over `z`, and the number row's lift range works out at 13px.

use flipper_ui::key::FlipperKey::{Back, Down, Edit, Left, Ok, Right, Run, Up, View};
use flipper_ui::keyboard::{self, Exit, Focus, Grid, Layout, Shift, TextInput};

/// Press a key with the text always considered valid.
fn press(input: &mut TextInput, key: flipper_ui::key::FlipperKey) -> Option<Exit> {
    input.key(key, true)
}

/// The ABC layout is as wide as its widest row: 35 + 11 * 15 + 35.
#[test]
fn the_chrome_is_as_wide_as_its_widest_row() {
    let grid = Grid::new(Layout::Abc);
    assert_eq!(grid.width(), 235);
    // Three rows reach that width; the number row is one wide key short.
    assert_eq!(grid.row_width(0), 200);
    assert_eq!(grid.row_width(1), 235);
    assert_eq!(grid.row_width(2), 235);
    assert_eq!(grid.row_width(3), 235);
}

/// Rows with no leading wide key are padded by one, so the letter columns line
/// up: `q`, `a` and `z` all start at the same x.
#[test]
fn letter_columns_line_up_across_rows() {
    let grid = Grid::new(Layout::Abc);
    let q = grid.cell_x(1, 0);
    let a = grid.cell_x(2, 1);
    let z = grid.cell_x(3, 1);
    assert_eq!((q, a, z), (35, 35, 35));
    // And the number row sits over the letters, 1 above q.
    assert_eq!(grid.cell_x(0, 0), 35);
}

/// The number row lives above the chrome and takes none of its height.
#[test]
fn the_number_row_is_outside_the_chrome() {
    let grid = Grid::new(Layout::Abc);
    assert!(grid.has_peek());
    assert_eq!(grid.chrome_rows(), 3, "four rows, one of them above");

    // A symbol layout has no number row: its digits are an ordinary row.
    let sym = Grid::new(Layout::Sym);
    assert!(!sym.has_peek());
    assert_eq!(sym.chrome_rows(), 3);
}

/// At rest the number row shows a 2px tab; selected, it clears the chrome. The
/// difference is the 13px the prototype's comment names.
#[test]
fn the_number_row_rests_low_and_rises_when_selected() {
    let mut input = TextInput::new("t", "");
    let folded = input.placed()[0].y;
    // Up from QWERTY lands on the number row, which starts the lift.
    press(&mut input, Up);
    // The wave eases, so take it at the end.
    std::thread::sleep(std::time::Duration::from_millis(220));
    let _ = input.animating();
    let popped = input.placed()[0].y;
    assert_eq!(folded - popped, 13, "lift range");
    // A card sunk behind the QWERTY row paints only its top edge.
    let mut fresh = TextInput::new("t", "");
    assert_eq!(fresh.placed()[0].clip_h, 2);
    assert_eq!(fresh.placed()[12].clip_h, 16, "an ordinary key never clips");
    let _ = fresh.animating();
}

/// The keyboard opens on QWERTY, not on the number row: the numbers are the extra
/// reached with Up.
#[test]
fn it_opens_on_the_letters() {
    let input = TextInput::new("Profile name", "abc");
    assert_eq!(input.row, 1);
    assert_eq!(input.col, 0);
    assert_eq!(input.cursor, 3, "the caret starts after the seeded text");
}

/// Typing inserts at the caret; one-shot shift capitalises exactly one letter.
#[test]
fn shift_capitalises_one_letter_and_releases() {
    let mut input = TextInput::new("t", "");
    // Down snaps to the column above, so `q` leads to `a` and then `z`; the shift
    // key is the wide cell to their left.
    press(&mut input, Down);
    press(&mut input, Down);
    press(&mut input, Left);
    assert!(input.on_shift(), "expected the shift key, got {:?}", input.col);
    press(&mut input, Ok);
    assert_eq!(input.shift, Shift::Once);

    press(&mut input, Right);
    press(&mut input, Ok);
    assert_eq!(input.text, "Z");
    assert_eq!(input.shift, Shift::Off, "one-shot shift releases");

    press(&mut input, Ok);
    assert_eq!(input.text, "Zz");
}

/// Caps lock holds until it is turned off.
#[test]
fn caps_lock_holds() {
    let mut input = TextInput::new("t", "");
    input.latch_caps();
    press(&mut input, Ok);
    press(&mut input, Right);
    press(&mut input, Ok);
    assert_eq!(input.text, "QW");
    assert_eq!(input.shift, Shift::Caps);
}

/// The caret travels through the text rather than always sitting at its end.
#[test]
fn the_caret_moves_inside_the_field() {
    let mut input = TextInput::new("t", "abc");
    press(&mut input, Up);
    press(&mut input, Up);
    assert_eq!(input.focus, Focus::Field);
    press(&mut input, Left);
    press(&mut input, Left);
    assert_eq!(input.cursor, 1);
    // Down off the field lands on the keyboard's top row, which in ABC is the
    // number row, and what it types goes in at the caret.
    press(&mut input, Down);
    press(&mut input, Ok);
    assert_eq!(input.text, "a1bc");
}

/// The V key deletes and the X key swaps the layout, which is where the two tabs
/// under the keyboard sit.
#[test]
fn the_tabs_delete_and_swap_the_layout() {
    let mut input = TextInput::new("t", "ab");
    press(&mut input, Edit);
    assert_eq!(input.text, "a");
    assert_eq!(input.tab_pressed(), 2);

    press(&mut input, View);
    assert_eq!(input.layout, Layout::Sym);
    assert_eq!(input.tab_pressed(), 1);
    press(&mut input, View);
    assert_eq!(input.layout, Layout::Abc);
}

/// The wide cell at the head of a symbol row swaps the two symbol layouts, and
/// emits nothing.
#[test]
fn the_symbol_layouts_swap_between_themselves() {
    let mut input = TextInput::new("t", "");
    press(&mut input, View);
    assert_eq!(input.layout, Layout::Sym);
    // The swap keeps the selection, which in this layout is that wide cell.
    assert_eq!((input.row, input.col), (1, 0));
    press(&mut input, Ok);
    assert_eq!(input.layout, Layout::Sym2);
    assert_eq!(input.text, "", "a layout switch types nothing");
}

/// Done commits, and is refused while the caller says the text is unusable.
#[test]
fn done_is_refused_while_the_text_is_invalid() {
    let mut input = TextInput::new("t", "abc");
    assert_eq!(input.key(Run, false), None);
    assert_eq!(input.key(Run, true), Some(Exit::Save("abc".into())));
}

/// Leaving an unchanged field just leaves; leaving a changed one asks first.
#[test]
fn leaving_asks_only_when_there_is_something_to_lose() {
    let mut unchanged = TextInput::new("t", "abc");
    assert_eq!(press(&mut unchanged, Back), Some(Exit::Cancel));

    let mut changed = TextInput::new("t", "abc");
    press(&mut changed, Ok);
    assert_eq!(press(&mut changed, Back), None);
    assert_eq!(changed.discard, Some(0), "Keep is highlighted first");
    // Keep dismisses and the text survives.
    assert_eq!(press(&mut changed, Ok), None);
    assert_eq!(changed.discard, None);
    assert!(changed.text.ends_with('q'));
    // Discard leaves.
    press(&mut changed, Back);
    press(&mut changed, Down);
    assert_eq!(changed.discard, Some(1));
    assert_eq!(press(&mut changed, Ok), Some(Exit::Cancel));
}

/// Text that fits is shown whole, and the caret sits one pixel past it.
#[test]
fn short_text_is_not_truncated() {
    let fitted = keyboard::fit_input("abc", 3, 138);
    assert_eq!(fitted.visible, "abc");
    assert_eq!(fitted.cursor_dx, i32::from(flipper_ui::font::TITLE.text_width("abc")) + 1);
}

/// Text too long for the field is windowed around the caret, and each truncated
/// side is marked.
#[test]
fn long_text_is_windowed_around_the_caret() {
    let long = "abcdefghijklmnopqrstuvwxyz0123456789";
    let inner = 60;

    let at_start = keyboard::fit_input(long, 0, inner);
    assert!(at_start.visible.starts_with('a'), "{}", at_start.visible);
    assert!(at_start.visible.ends_with("...>"), "{}", at_start.visible);

    let at_end = keyboard::fit_input(long, long.len(), inner);
    assert!(at_end.visible.starts_with("<..."), "{}", at_end.visible);
    assert!(at_end.visible.ends_with('9'), "{}", at_end.visible);

    let middle = keyboard::fit_input(long, 18, inner);
    assert!(middle.visible.starts_with("<..."), "{}", middle.visible);
    assert!(middle.visible.ends_with("...>"), "{}", middle.visible);
    for f in [&at_start, &at_end, &middle] {
        assert!(
            i32::from(flipper_ui::font::TITLE.text_width(&f.visible)) <= inner,
            "{} overruns the field",
            f.visible
        );
    }
}

/// The field grows with its text between the two caps the prototype sets.
#[test]
fn the_field_grows_with_the_text() {
    assert_eq!(keyboard::field_w(0), 150, "the minimum");
    assert_eq!(keyboard::field_w(200), 212);
    assert_eq!(keyboard::field_w(1000), 238, "the cap");
}

// ── The touchpad ───────────────────────────────────────────────────────────
//
// keyboard_test.js's scheme, and its numbers: the selection moves by how far the
// finger has travelled since it went down, never to where the finger is. The pad
// sits beside the screen, so no point on it means a particular key.

use flipper_ui::platform::Touch;

/// A finger down at the pad's middle, which is where every drag below starts.
const MID: Touch = Touch { x: 512, y: 400, down: true };

fn drag(input: &mut TextInput, dx: i32, dy: i32) -> bool {
    input.touch(Touch { x: MID.x + dx, y: MID.y + dy, down: true })
}

#[test]
fn a_touch_alone_moves_nothing() {
    let mut input = TextInput::new("Name", "");
    let (row, col) = (input.row, input.col);
    assert!(!input.touch(MID), "landing is not a move");
    assert_eq!((input.row, input.col), (row, col));
}

/// One column per 48 pad units, after the sensitivity divider halves the delta:
/// 96 raw units of X is one cell. Slower than the prototype's 32 on purpose.
#[test]
fn a_column_is_ninety_six_raw_units() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    assert_eq!(input.col, 0);
    assert!(!drag(&mut input, 47, 0), "under half a step stays put");
    assert_eq!(input.col, 0);
    assert!(drag(&mut input, 96, 0), "a full step moves one column");
    assert_eq!(input.col, 1);
    drag(&mut input, 288, 0);
    assert_eq!(input.col, 3, "three steps from the anchor, not from the last report");
}

/// A row is 90 units after the same halving, so 180 raw. Quicker than the
/// prototype's 130, which could not cross the keyboard in one stroke.
#[test]
fn a_row_is_one_hundred_and_eighty_raw_units() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    let start = input.row;
    drag(&mut input, 0, 180);
    assert_eq!(input.row, start + 1);
    drag(&mut input, 0, 360);
    assert_eq!(input.row, start + 2);
}

/// Lifting ends the stroke, and the next one measures from where it lands: a
/// drag is never continued across a lift.
#[test]
fn lifting_re_anchors_the_next_drag() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    drag(&mut input, 192, 0);
    let col = input.col;
    assert_eq!(col, 2);
    input.touch(Touch { down: false, ..MID });
    // Same absolute position as the end of the last stroke: a scheme that
    // measured from the old anchor would jump, this one does not move at all.
    input.touch(Touch { x: MID.x + 192, y: MID.y, down: true });
    assert_eq!(input.col, col);
}

/// Lifting never presses. The pad reaches a key; OK is what types it.
#[test]
fn lifting_does_not_type() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    drag(&mut input, 64, 0);
    assert!(input.touch(Touch { down: false, ..MID }).eq(&false));
    assert_eq!(input.text, "", "a stroke that crossed keys typed nothing");
}

/// Dragging above the top row reaches the field, and X there is the caret.
#[test]
fn dragging_up_reaches_the_field_and_moves_the_caret() {
    let mut input = TextInput::new("Name", "abcd");
    assert_eq!(input.focus, Focus::Keys);
    input.touch(MID);
    // Far enough up to clear every keyboard row.
    drag(&mut input, 0, -260 * 6);
    assert_eq!(input.focus, Focus::Field);
    let at = input.cursor;
    // Entering does not drag the caret; only movement after it does.
    assert_eq!(at, 4, "the caret stayed where it was on the way in");
    input.touch(Touch { x: MID.x - 192, y: MID.y - 260 * 6, down: true });
    assert_eq!(input.cursor, 2, "two columns left of the anchor");
}

/// The tab strip is a dead end: dragging past it does not wrap onto the number
/// row, which is the D-pad's way in alone.
#[test]
fn the_tab_strip_does_not_wrap_onto_the_numbers() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    drag(&mut input, 0, 260 * 8);
    assert!(
        matches!(input.focus, Focus::Tab123 | Focus::TabBackspace | Focus::Keys),
        "focus {:?}",
        input.focus
    );
    assert_ne!(input.row, 0, "never landed back on the number row");
}

/// The step boundary, against keyboard_test.js's own arithmetic:
/// `round((raw / TP_SLOW_DIVIDER) / TP_Y_UNITS_PER_STEP)`, all in floating point.
/// A row is 180 raw units, so the tick is at half of that.
#[test]
fn a_row_ticks_at_half_a_step_the_way_the_prototype_rounds_it() {
    for (raw, want) in [(0, 0), (89, 0), (90, 1), (180, 1), (269, 1), (270, 2)] {
        let mut input = TextInput::new("Name", "");
        input.touch(MID);
        let start = input.row as i32;
        drag(&mut input, 0, raw);
        assert_eq!(input.row as i32 - start, want, "{raw} raw units of Y should be {want} rows");
    }
}

/// Dragging up reaches the number row before the field, because the prototype's
/// _setSelection clamps the row to 0 rather than skipping the peek row. Only the
/// step past it goes to the field.
#[test]
fn dragging_up_passes_through_the_number_row() {
    let mut input = TextInput::new("Name", "ab");
    assert_eq!(input.row, 1, "starts on QWERTY, the number row is the opt-in one");
    input.touch(MID);
    drag(&mut input, 0, -180);
    assert_eq!(input.focus, Focus::Keys);
    assert_eq!(input.row, 0, "the peek row is a row the pad can reach");
    drag(&mut input, 0, -360);
    assert_eq!(input.focus, Focus::Field, "one more step leaves for the field");
}

/// Ties round toward positive, because Math.round does and the step arithmetic
/// is ported from it. So down ticks at exactly half a step and up has to pass
/// it: an asymmetry that is the prototype's, not an accident here.
#[test]
fn a_tie_rounds_down_the_screen_not_away_from_zero() {
    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    let start = input.row as i32;
    drag(&mut input, 0, 90);
    assert_eq!(input.row as i32 - start, 1, "+90 is a tie and goes down");

    let mut input = TextInput::new("Name", "");
    input.touch(MID);
    let start = input.row as i32;
    drag(&mut input, 0, -90);
    assert_eq!(input.row as i32 - start, 0, "-90 is the same tie and does not go up");
    drag(&mut input, 0, -91);
    assert_eq!(input.row as i32 - start, -1, "one unit past it does");
}

/// A vertical drag lands under the finger, not on the same index.
///
/// The rows are ragged: the home and shift rows lead with a 35px key where QWERTY
/// leads with a 15px one, so index 4 on one row is a key to the side of index 4 on
/// the next. Carrying the index is what made a straight drag down step sideways;
/// the d-pad has always carried the position instead, through closest_col.
#[test]
fn a_vertical_drag_keeps_its_place_across_ragged_rows() {
    let grid = Grid::new(Layout::Abc);
    for col in [0usize, 4, 8] {
        let mut input = TextInput::new("Name", "");
        // Start on QWERTY at a known cell.
        for _ in 0..col {
            press(&mut input, Right);
        }
        assert_eq!((input.row, input.col), (1, col));
        let want = grid.closest_col(2, cell_center(&grid, 1, col));

        input.touch(MID);
        drag(&mut input, 0, 180);
        assert_eq!(input.row, 2, "one row down");
        assert_eq!(
            input.col, want,
            "row 1 col {col} should land under itself on row 2, which is {want}"
        );
    }
}

/// The same, upward, where the offset runs the other way.
#[test]
fn dragging_up_a_ragged_row_also_lands_under_the_finger() {
    let grid = Grid::new(Layout::Abc);
    let mut input = TextInput::new("Name", "");
    press(&mut input, Down);
    for _ in 0..4 {
        press(&mut input, Right);
    }
    assert_eq!(input.row, 2);
    let from = input.col;
    let want = grid.closest_col(1, cell_center(&grid, 2, from));
    input.touch(MID);
    drag(&mut input, 0, -180);
    assert_eq!(input.row, 1);
    assert_eq!(input.col, want, "row 2 col {from} should land on row 1 col {want}");
}

/// `cell_center_2x` is private, so measure it the way the grid does.
fn cell_center(grid: &Grid, row: usize, col: usize) -> i32 {
    2 * grid.cell_x(row, col) + grid.rows[row][col].width()
}
