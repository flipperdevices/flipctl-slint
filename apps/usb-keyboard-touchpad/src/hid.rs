//! HID reports, as the `hid` claim's descriptor lays them out: report 1 a boot
//! keyboard's modifier byte, a reserved byte and six key slots; report 2 three
//! buttons, x, y and a wheel; report 3 one 16-bit consumer usage. The same reports
//! go over Bluetooth, whose report map is the same descriptor.

/// Left shift, in the modifier byte.
const SHIFT: u8 = 0x02;

pub const BACKSPACE: u8 = 0x2a;

/// Consumer usages for the media keys.
pub const VOLUME_UP: u16 = 0xe9;
pub const VOLUME_DOWN: u16 = 0xea;

/// The key a character is typed with on a US layout, and whether it needs shift.
pub fn usage(c: char) -> Option<(u8, u8)> {
    let plain = |k: u8| Some((0, k));
    let shifted = |k: u8| Some((SHIFT, k));
    match c {
        'a'..='z' => plain(0x04 + (c as u8 - b'a')),
        'A'..='Z' => shifted(0x04 + (c as u8 - b'A')),
        '1'..='9' => plain(0x1e + (c as u8 - b'1')),
        '0' => plain(0x27),
        ' ' => plain(0x2c),
        '-' => plain(0x2d),
        '_' => shifted(0x2d),
        '=' => plain(0x2e),
        '+' => shifted(0x2e),
        '[' => plain(0x2f),
        '{' => shifted(0x2f),
        ']' => plain(0x30),
        '}' => shifted(0x30),
        '\\' => plain(0x31),
        '|' => shifted(0x31),
        ';' => plain(0x33),
        ':' => shifted(0x33),
        '\'' => plain(0x34),
        '"' => shifted(0x34),
        '`' => plain(0x35),
        '~' => shifted(0x35),
        ',' => plain(0x36),
        '<' => shifted(0x36),
        '.' => plain(0x37),
        '>' => shifted(0x37),
        '/' => plain(0x38),
        '?' => shifted(0x38),
        '!' => shifted(0x1e),
        '@' => shifted(0x1f),
        '#' => shifted(0x20),
        '$' => shifted(0x21),
        '%' => shifted(0x22),
        '^' => shifted(0x23),
        '&' => shifted(0x24),
        '*' => shifted(0x25),
        '(' => shifted(0x26),
        ')' => shifted(0x27),
        _ => None,
    }
}

/// A key held with its modifiers, or every key let go with `(0, 0)`.
pub fn keyboard(modifiers: u8, key: u8) -> [u8; 9] {
    [1, modifiers, 0, key, 0, 0, 0, 0, 0]
}

/// The buttons held (bit 0 left, 1 right, 2 middle) and the movement since the last.
pub fn mouse(buttons: u8, dx: i8, dy: i8, wheel: i8) -> [u8; 5] {
    [2, buttons, dx as u8, dy as u8, wheel as u8]
}

/// A media key held, or let go with 0.
pub fn consumer(usage: u16) -> [u8; 3] {
    let [lo, hi] = usage.to_le_bytes();
    [3, lo, hi]
}

/// Pad travel turned into pointer movement: scaled, with the remainder carried so a
/// slow drag still moves, and split into steps a report's signed byte can hold.
#[derive(Default)]
pub struct Pointer {
    carry: (f32, f32),
}

/// Pointer pixels per raw pad unit.
const GAIN: f32 = 0.6;

impl Pointer {
    pub fn moved(&mut self, dx: i32, dy: i32) -> Vec<(i8, i8)> {
        let x = self.carry.0 + dx as f32 * GAIN;
        let y = self.carry.1 + dy as f32 * GAIN;
        let (mut wx, mut wy) = (x.trunc() as i32, y.trunc() as i32);
        self.carry = (x.fract(), y.fract());
        let mut steps = Vec::new();
        while wx != 0 || wy != 0 {
            let sx = wx.clamp(-127, 127);
            let sy = wy.clamp(-127, 127);
            steps.push((sx as i8, sy as i8));
            wx -= sx;
            wy -= sy;
        }
        steps
    }

    pub fn reset(&mut self) {
        self.carry = (0.0, 0.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_map_to_us_keys() {
        assert_eq!(usage('a'), Some((0, 0x04)));
        assert_eq!(usage('Z'), Some((SHIFT, 0x1d)));
        assert_eq!(usage('1'), Some((0, 0x1e)));
        assert_eq!(usage('0'), Some((0, 0x27)));
        assert_eq!(usage('?'), Some((SHIFT, 0x38)));
        assert_eq!(usage(' '), Some((0, 0x2c)));
        assert_eq!(usage('é'), None);
        // Every key the on-screen keyboard can type has a key here
        for c in "qwertyuiop[asdfghjkl:]zxcvbnm,./?1234567890-+()={}@#$;<>|\"'!_%^&*`~\\".chars() {
            assert!(usage(c).is_some(), "{c}");
        }
    }

    #[test]
    fn reports_are_laid_out_as_the_descriptor_says() {
        assert_eq!(keyboard(SHIFT, 0x04), [1, 2, 0, 4, 0, 0, 0, 0, 0]);
        assert_eq!(mouse(1, -1, 2, 0), [2, 1, 0xff, 2, 0]);
        assert_eq!(consumer(VOLUME_UP), [3, 0xe9, 0]);
    }

    #[test]
    fn pointer_keeps_the_remainder_and_splits_big_moves() {
        let mut p = Pointer::default();
        assert!(p.moved(1, 0).is_empty(), "0.6 carried");
        assert_eq!(p.moved(1, 0), [(1, 0)], "1.2");
        let big = p.moved(500, -500);
        assert_eq!(big.iter().map(|s| i32::from(s.0)).sum::<i32>(), 300);
        assert_eq!(big.iter().map(|s| i32::from(s.1)).sum::<i32>(), -300);
    }
}
