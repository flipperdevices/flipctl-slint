//! What was typed into the address field, as somewhere to go.

/// Where words that are not an address are sent.
const SEARCH: &str = "https://duckduckgo.com/?q=";

/// An address as typed, a bare host, or words to search for. `None` for nothing.
pub fn resolve(typed: &str) -> Option<String> {
    let t = typed.trim();
    if t.is_empty() {
        return None;
    }
    if t.contains("://") || t.starts_with("about:") || t.starts_with("data:") {
        return Some(t.to_string());
    }
    let host = t.split(['/', ':']).next().unwrap_or_default();
    if !t.contains(' ') && (host.contains('.') || host == "localhost") {
        return Some(format!("https://{t}"));
    }
    Some(format!("{SEARCH}{}", query(t)))
}

/// Form encoding: the unreserved characters as they are, a space as `+`, and every
/// other byte as `%XX`.
fn query(words: &str) -> String {
    let mut out = String::new();
    for b in words.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_address_is_left_alone() {
        assert_eq!(
            resolve(" http://example.com/a?b=c ").as_deref(),
            Some("http://example.com/a?b=c")
        );
        assert_eq!(resolve("about:blank").as_deref(), Some("about:blank"));
    }

    #[test]
    fn a_bare_host_is_given_https() {
        assert_eq!(resolve("flipper.net").as_deref(), Some("https://flipper.net"));
        assert_eq!(
            resolve("docs.flipper.net/zero").as_deref(),
            Some("https://docs.flipper.net/zero")
        );
        assert_eq!(resolve("localhost:8080").as_deref(), Some("https://localhost:8080"));
    }

    #[test]
    fn words_are_searched_for() {
        assert_eq!(
            resolve("flipper one").as_deref(),
            Some("https://duckduckgo.com/?q=flipper+one")
        );
        assert_eq!(resolve("c++").as_deref(), Some("https://duckduckgo.com/?q=c%2B%2B"));
        assert_eq!(resolve("   "), None);
    }
}
