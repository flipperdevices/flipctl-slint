//! Vendor names from the IEEE OUI registry, embedded: `oui.tsv`, made from the
//! registry by `tools/oui.py`, so the app needs nothing installed.

use std::collections::HashMap;
use std::sync::OnceLock;

const TABLE_TSV: &str = include_str!("../oui.tsv");

static TABLE: OnceLock<HashMap<[u8; 3], &'static str>> = OnceLock::new();

/// Index the table on a thread of its own, so the first lookup does not wait on it.
pub fn preload() {
    std::thread::spawn(|| {
        TABLE.get_or_init(|| parse(TABLE_TSV));
    });
}

/// Lines of the prefix in hex, a tab and the name.
fn parse(text: &'static str) -> HashMap<[u8; 3], &'static str> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((hex, name)) = line.split_once('\t') else { continue };
        let Ok(v) = u32::from_str_radix(hex, 16) else { continue };
        if hex.len() == 6 {
            out.insert([(v >> 16) as u8, (v >> 8) as u8, v as u8], name);
        }
    }
    out
}

/// The registered vendor of `oui`, once the registry is read.
pub fn vendor(oui: [u8; 3]) -> Option<&'static str> {
    TABLE.get()?.get(&oui).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_table() {
        let t = parse("B06B11\tExample Corp\nnot a line\n");
        assert_eq!(t.get(&[0xb0, 0x6b, 0x11]).copied(), Some("Example Corp"));
        assert_eq!(t.len(), 1);
        let all = parse(TABLE_TSV);
        assert!(all.len() > 30_000, "{}", all.len());
        assert_eq!(all.get(&[0, 0, 0]).copied(), Some("XEROX CORPORATION"));
    }
}
