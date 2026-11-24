//! Asking flipctl for something, on the socket it hands every hosted app in
//! `FLIPCTL_SOCKET`.
//!
//! [`claim`] borrows hardware: the resource is the app's for as long as the stream it
//! returns is open, and closing it, or dying, gives it back. [`type_text`] puts
//! flipctl's own keyboard over the app and hands back what was typed. [`leave`] and
//! [`home`] ask for the background, which is what Back does once the app has no step
//! left and what an Esc the app does not use does.
//!
//! ```ignore
//! // Asked from a key, then looked for each time the page is drawn
//! self.typing = Some(flipctl_app::type_text("Add IP address", &self.address));
//!
//! if let Some(answer) = self.typing.as_ref().and_then(Typing::take) {
//!     self.typing = None;
//!     match answer {
//!         Ok(Some(text)) => ..,  // Done
//!         Ok(None) => {}         // Cancel
//!         Err(why) => ..,        // flipctl refused, or is too old to ask
//!     }
//! }
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc::{channel, Receiver};
use std::time::{Duration, Instant};

/// How long a resource in use is asked for again: one just given back is busy for the
/// few seconds flipctl takes to put the system's own use of it back.
const BUSY_WAIT: Duration = Duration::from_secs(8);

/// Borrow `resource` from flipctl, as its `claim` module lists them: `udc`,
/// `usb/1-1.2` and the rest. Blocks while flipctl prepares it, which can take a
/// second or two, and while it is still being given back by its last holder, so ask
/// off the drawing thread.
pub fn claim(resource: &str) -> Result<UnixStream, String> {
    let asked = Instant::now();
    loop {
        match post(&format!("/claim/{resource}")) {
            Ok((stream, _)) => return Ok(stream),
            Err((Some(409), _)) if asked.elapsed() < BUSY_WAIT => {
                std::thread::sleep(Duration::from_millis(500));
            }
            Err((_, why)) => return Err(why),
        }
    }
}

/// The keyboard on its way: what flipctl answers, once the user is done.
pub struct Typing {
    answer: Receiver<Result<Option<String>, String>>,
}

impl Typing {
    /// The answer, once there is one: the text on Done, `None` on Cancel, or why the
    /// keyboard could not be put up. Taken once.
    pub fn take(&self) -> Option<Result<Option<String>, String>> {
        self.answer.try_recv().ok()
    }
}

/// Ask flipctl to put its keyboard over this app, titled `title` and starting with
/// `text`. Returns at once; the page is drawn again when the answer arrives, so
/// [`Typing::take`] is looked at where the page is built. Only the app in front can
/// ask, and the keys are flipctl's until the keyboard is put away.
pub fn type_text(title: &str, text: &str) -> Typing {
    let path = format!("/keyboard?title={}&text={}", encode(title), encode(text));
    let (tx, answer) = channel();
    std::thread::spawn(move || {
        let said =
            post(&path).map_err(|(_, why)| why).and_then(|(_, body)| match field(&body, "text") {
                Some(text) => Ok(text),
                None => Err(format!("flipctl said {body}")),
            });
        let _ = tx.send(said);
        crate::redraw();
    });
    Typing { answer }
}

/// Go to the background, as Back does on an app's first step: flipctl puts back the
/// screen the app was opened from, and the app keeps running. Run outside flipctl,
/// where there is nothing to go back to, the app closes instead.
pub fn leave() {
    away("/leave");
}

/// Go to the background as [`leave`] does, but with flipctl showing its home screen:
/// what Esc does in an app that has not made it anything else.
pub fn home() {
    away("/home");
}

fn away(path: &'static str) {
    if std::env::var_os("FLIPCTL_SOCKET").is_none() {
        let _ = slint::quit_event_loop();
        return;
    }
    std::thread::spawn(move || {
        if let Err((_, why)) = post(path) {
            eprintln!("{path}: {why}");
        }
    });
}

/// One POST and its answer's first line of body, or the error flipctl gave with its
/// status, when it got as far as giving one. The stream is returned open, since a
/// claim lasts as long as it does.
fn post(path: &str) -> Result<(UnixStream, String), (Option<u16>, String)> {
    let socket = std::env::var_os("FLIPCTL_SOCKET")
        .ok_or((None, "flipctl is too old to ask; update it".to_string()))?;
    let flipctl = |e: std::io::Error| (None, format!("flipctl: {e}"));
    let mut s = UnixStream::connect(socket).map_err(flipctl)?;
    write!(s, "POST {path} HTTP/1.1\r\nHost: flipctl\r\nContent-Length: 0\r\n\r\n")
        .map_err(flipctl)?;
    let mut reader = BufReader::new(s.try_clone().map_err(flipctl)?);
    let mut status = String::new();
    reader.read_line(&mut status).map_err(flipctl)?;
    let mut line = String::new();
    while reader.read_line(&mut line).map_err(flipctl)? > 0 && line.trim_end() != "" {
        line.clear();
    }
    let mut body = String::new();
    let _ = reader.read_line(&mut body);
    let body = body.trim_end().to_string();
    let code = status.split(' ').nth(1).and_then(|c| c.parse().ok());
    if code != Some(200) {
        return Err((
            code,
            match field(&body, "error") {
                Some(Some(why)) => why,
                _ => status.trim().to_string(),
            },
        ));
    }
    Ok((s, body))
}

/// A string field of the one-level JSON object flipctl answers with: `Some(None)` for
/// `null`, `None` when it is not there. flipctl escapes only `\` and `"`.
fn field(body: &str, key: &str) -> Option<Option<String>> {
    let at = body.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = body[at..].trim_start();
    if rest.starts_with("null") {
        return Some(None);
    }
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?),
            '"' => return Some(Some(out)),
            c => out.push(c),
        }
    }
    None
}

/// Percent-encoded for a query string: everything but the unreserved characters.
fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (b as char).to_string()
            }
            b => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_is_read_from_its_field() {
        assert_eq!(field(r#"{"text":"10.0.0.2"}"#, "text"), Some(Some("10.0.0.2".into())));
        assert_eq!(field(r#"{"text":"a\"b\\c"}"#, "text"), Some(Some("a\"b\\c".into())));
        assert_eq!(field(r#"{"text":null}"#, "text"), Some(None));
        assert_eq!(field(r#"{"error":"busy"}"#, "text"), None);
        assert_eq!(field(r#"{"text":"cut"#, "text"), None);
    }

    #[test]
    fn a_query_value_is_percent_encoded() {
        assert_eq!(encode("Add IP address"), "Add%20IP%20address");
        assert_eq!(encode("10.0.0.2"), "10.0.0.2");
        assert_eq!(encode("a&b=c"), "a%26b%3Dc");
    }
}
