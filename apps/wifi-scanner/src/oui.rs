//! Vendor names from the IEEE OUI registry, as Debian's ieee-data ships it.

use std::collections::HashMap;
use std::sync::OnceLock;

const FILE: &str = "/usr/share/ieee-data/oui.txt";

static TABLE: OnceLock<HashMap<[u8; 3], String>> = OnceLock::new();

/// Read the registry on a thread of its own, so the first lookup does not wait on it.
pub fn preload() {
    std::thread::spawn(|| {
        TABLE.get_or_init(load);
    });
}

fn load() -> HashMap<[u8; 3], String> {
    std::fs::read(FILE).map(|b| parse(&String::from_utf8_lossy(&b))).unwrap_or_default()
}

fn parse(text: &str) -> HashMap<[u8; 3], String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let Some((prefix, rest)) = line.split_once("(hex)") else { continue };
        let hex: Vec<u8> =
            prefix.trim().split('-').filter_map(|h| u8::from_str_radix(h, 16).ok()).collect();
        if let [a, b, c] = hex[..] {
            out.insert([a, b, c], rest.trim().to_string());
        }
    }
    out
}

/// The registered vendor of `oui`, once the registry is read.
pub fn vendor(oui: [u8; 3]) -> Option<&'static str> {
    TABLE.get()?.get(&oui).map(String::as_str)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_format() {
        let t = parse(
            "OUI/MA-L\n\nB0-6B-11   (hex)\t\tExample Corp\nB06B11     (base 16)\t\tExample Corp\n",
        );
        assert_eq!(t.get(&[0xb0, 0x6b, 0x11]).map(String::as_str), Some("Example Corp"));
        assert_eq!(t.len(), 1);
    }
}
