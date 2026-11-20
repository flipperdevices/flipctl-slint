//! One socket a desktop hands an app to.
//!
//! A bundle double-clicked on the HDMI desktop belongs on the panel, and flipctl is
//! the only thing that can put it there. So the bundle's AppRun asks: `flipctl open
//! <file>` connects here, one line each way, and the running flipctl lists, starts
//! or fronts the app and answers. The desktop and flipctl run as the same user, and
//! the socket sits in that user's 0700 runtime directory, which is the whole of the
//! access control.
//!
//! An HTTP request on the same socket is an app asking for something. A claim asks for
//! hardware; see `claim`. A hosted app asks for text with
//!
//! ```text
//! POST /keyboard?title=<title>&text=<text> HTTP/1.1
//! ```
//!
//! both percent-encoded, `text` what the field starts with and optional. flipctl puts
//! its own keyboard over the app and answers once the user is done: `200` with
//! `{"text":"..."}`, or `{"text":null}` when they cancel. Only the app in front may
//! ask, one at a time; any other request gets `409` with `{"error":"..."}`.
//!
//! Not the browser view's HTTP port: that is an optional feature bound on every
//! interface with no authentication, and a request that runs a file is not something
//! to hang off it.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

/// The socket's name in the runtime directory.
pub const SOCKET: &str = "flipctl.sock";

/// A request has one line, and this is as long as one may be.
const MAX_LINE: usize = 4096;

/// The user's runtime directory, or where systemd puts it when the environment does
/// not say.
pub(crate) fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

pub fn socket_path() -> PathBuf {
    runtime_dir().join(SOCKET)
}

/// What can be asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Is a flipctl listening.
    Ping,
    /// Put this bundle on the panel.
    Open(PathBuf),
    /// Type some text for the app `pid` belongs to. HTTP only, so answered with
    /// [`Pending::typed`] and [`Pending::refused`].
    Keyboard { title: String, text: String, pid: u32 },
}

/// One line into a request: `ping`, or `open` and an absolute path.
pub fn parse(line: &str) -> Result<Request, String> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.len() > MAX_LINE {
        return Err("bad request".into());
    }
    match line.split_once(' ') {
        None if line == "ping" => Ok(Request::Ping),
        Some(("open", path)) if Path::new(path.trim()).is_absolute() => {
            Ok(Request::Open(PathBuf::from(path.trim())))
        }
        _ => Err("bad request".into()),
    }
}

/// A request waiting for its answer. Answered once, on the connection it came in on.
pub struct Pending {
    pub request: Request,
    stream: UnixStream,
}

impl Pending {
    pub fn ok(self, what: &str) {
        self.reply("ok", what);
    }

    pub fn err(self, why: &str) {
        self.reply("err", why);
    }

    fn reply(mut self, status: &str, text: &str) {
        let _ = writeln!(self.stream, "{status} {text}");
        let _ = self.stream.flush();
    }

    /// What the keyboard was left with: the text, or `None` when it was cancelled.
    pub fn typed(mut self, text: Option<&str>) {
        let body = match text {
            Some(t) => format!("{{\"text\":{}}}", crate::claim::json_string(t)),
            None => "{\"text\":null}".into(),
        };
        let _ = crate::claim::reply(&mut self.stream, 200, "OK", &body);
    }

    /// The keyboard cannot be put up for this request.
    pub fn refused(mut self, why: &str) {
        let _ = crate::claim::reply(&mut self.stream, 409, "Conflict", &crate::claim::error(why));
    }
}

/// The listening side, polled from the render loop.
pub struct Listener {
    pending: Receiver<Pending>,
    path: PathBuf,
}

impl Listener {
    /// Bind in the runtime directory, replacing whatever a previous run left there.
    pub fn bind() -> io::Result<Self> {
        Self::bind_at(socket_path())
    }

    pub fn bind_at(path: PathBuf) -> io::Result<Self> {
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        let (tx, pending) = mpsc::channel();
        std::thread::Builder::new()
            .name("ipc-accept".into())
            .spawn(move || accept_loop(listener, tx))?;
        Ok(Self { pending, path })
    }

    /// The next request, if one has arrived.
    pub fn poll(&mut self) -> Option<Pending> {
        self.pending.try_recv().ok()
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Take one line per connection and hand what it asks to the loop. A line that is not
/// a request is answered here, since there is nothing for the loop to do with it.
fn accept_loop(listener: UnixListener, tx: Sender<Pending>) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let Ok(reading) = stream.try_clone() else { continue };
        let mut line = String::new();
        let mut reader = BufReader::new(reading.take(MAX_LINE as u64 + 1));
        if reader.read_line(&mut line).is_err() {
            continue;
        }
        if crate::claim::is_http(&line) {
            // The reader may hold the headers already, so they go along with the line
            let rest = reader.buffer().to_vec();
            let path = line.split(' ').nth(1).unwrap_or("");
            if path == "/keyboard" || path.starts_with("/keyboard?") {
                match keyboard(&line, &rest, &stream) {
                    Ok(request) => {
                        if tx.send(Pending { request, stream }).is_err() {
                            return;
                        }
                    }
                    Err((code, reason, why)) => {
                        let _ = crate::claim::reply(
                            &mut stream,
                            code,
                            reason,
                            &crate::claim::error(why),
                        );
                    }
                }
                continue;
            }
            crate::claim::serve(line, rest, stream);
            continue;
        }
        match parse(&line) {
            Ok(request) => {
                if tx.send(Pending { request, stream }).is_err() {
                    return;
                }
            }
            Err(why) => {
                let _ = writeln!(stream, "err {why}");
            }
        }
    }
}

/// A `/keyboard` request into what it asks, its headers read so the answer is not
/// sent over a request still arriving.
fn keyboard(
    first: &str,
    rest: &[u8],
    stream: &UnixStream,
) -> Result<Request, (u16, &'static str, &'static str)> {
    let bad = (400, "Bad Request", "bad request");
    let mut reader =
        BufReader::new(rest.chain(stream.try_clone().map_err(|_| bad)?)).take(MAX_LINE as u64 * 2);
    let mut header = String::new();
    while reader.read_line(&mut header).map_err(|_| bad)? > 0 && header.trim_end() != "" {
        header.clear();
    }
    let mut parts = first.split(' ');
    if parts.next() != Some("POST") {
        return Err((405, "Method Not Allowed", "the keyboard is a POST"));
    }
    let query = parts.next().and_then(|p| p.split_once('?')).map_or("", |(_, q)| q);
    let (mut title, mut text) = (String::new(), String::new());
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = printable(&percent_decode(value).ok_or(bad)?);
        match key {
            "title" => title = value,
            "text" => text = value,
            _ => return Err(bad),
        }
    }
    let pid = peer_pid(stream).ok_or((500, "Internal Server Error", "who is asking"))?;
    Ok(Request::Keyboard { title, text, pid })
}

/// `%XX` and `+` back into what they stand for.
pub(crate) fn percent_decode(s: &str) -> Option<String> {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        out.push(match b {
            b'%' => {
                let hex = [bytes.next()?, bytes.next()?];
                u8::from_str_radix(std::str::from_utf8(&hex).ok()?, 16).ok()?
            }
            b'+' => b' ',
            b => b,
        });
    }
    String::from_utf8(out).ok()
}

/// What the panel's fonts can draw: printable ASCII, the rest dropped.
fn printable(s: &str) -> String {
    s.chars().filter(|c| (' '..='~').contains(c)).collect()
}

/// The process on the other end of the socket, as the kernel says.
fn peer_pid(stream: &UnixStream) -> Option<u32> {
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    (rc == 0 && cred.pid > 0).then_some(cred.pid as u32)
}

/// What the other side said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Ok(String),
    Refused(String),
    /// Nothing is listening.
    NotRunning,
}

/// Ask the running flipctl.
pub fn send(request: &Request) -> Reply {
    send_to(&socket_path(), request)
}

pub fn send_to(path: &Path, request: &Request) -> Reply {
    let Ok(mut stream) = UnixStream::connect(path) else {
        return Reply::NotRunning;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let line = match request {
        Request::Ping => "ping".to_string(),
        Request::Open(path) => format!("open {}", path.display()),
        Request::Keyboard { .. } => return Reply::Refused("not a line request".into()),
    };
    if writeln!(stream, "{line}").and_then(|()| stream.flush()).is_err() {
        return Reply::NotRunning;
    }
    let mut reply = String::new();
    if BufReader::new(stream).read_line(&mut reply).is_err() || reply.is_empty() {
        return Reply::NotRunning;
    }
    let reply = reply.trim_end();
    match reply.split_once(' ').unwrap_or((reply, "")) {
        ("ok", text) => Reply::Ok(text.to_string()),
        ("err", text) => Reply::Refused(text.to_string()),
        _ => Reply::Refused(reply.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_is_a_request_or_refused() {
        assert_eq!(parse("ping\n"), Ok(Request::Ping));
        assert_eq!(
            parse("open /home/user/Apps/radio.AppImage\n"),
            Ok(Request::Open(PathBuf::from("/home/user/Apps/radio.AppImage")))
        );
        assert!(parse("open relative.AppImage").is_err(), "a path has to be absolute");
        assert!(parse("open ").is_err());
        assert!(parse("").is_err());
        assert!(parse("launch /x").is_err());
        assert!(parse(&format!("open /{}", "a".repeat(MAX_LINE))).is_err());
    }

    /// An HTTP request is answered on the socket itself, headers and all arriving in
    /// one write, as a client sends them.
    #[test]
    fn an_http_request_is_answered_on_the_socket() {
        let path =
            std::env::temp_dir().join(format!("flipctl-ipc-http-{}.sock", std::process::id()));
        let _listener = Listener::bind_at(path.clone()).expect("bind");
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream
            .write_all(b"POST /claim/hackrf HTTP/1.1\r\nHost: flipctl\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 404 "), "{answer}");
    }

    #[test]
    fn a_query_value_is_decoded_to_what_the_font_draws() {
        assert_eq!(percent_decode("10.0.0.2").as_deref(), Some("10.0.0.2"));
        assert_eq!(percent_decode("Add+IP%20address%3F").as_deref(), Some("Add IP address?"));
        assert_eq!(percent_decode("%4"), None);
        assert_eq!(percent_decode("%zz"), None);
        assert_eq!(printable("a\u{2014}b\n"), "ab");
    }

    /// The keyboard is asked for over HTTP, waits in the loop, and the answer goes back
    /// on the same connection.
    #[test]
    fn a_keyboard_request_waits_for_its_answer() {
        let path = std::env::temp_dir().join(format!("flipctl-ipc-kb-{}.sock", std::process::id()));
        let mut listener = Listener::bind_at(path.clone()).expect("bind");
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream
            .write_all(b"POST /keyboard?title=Add+IP+address&text=10.0.0.%32 HTTP/1.1\r\nHost: flipctl\r\nContent-Length: 0\r\n\r\n")
            .unwrap();
        let pending = loop {
            if let Some(p) = listener.poll() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(
            pending.request,
            Request::Keyboard {
                title: "Add IP address".into(),
                text: "10.0.0.2".into(),
                pid: std::process::id(),
            }
        );
        pending.typed(Some("10.8.0.\"1"));
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 200 "), "{answer}");
        assert!(answer.ends_with("{\"text\":\"10.8.0.\\\"1\"}\n"), "{answer}");
    }

    #[test]
    fn a_keyboard_request_is_a_post() {
        let path =
            std::env::temp_dir().join(format!("flipctl-ipc-kb-get-{}.sock", std::process::id()));
        let _listener = Listener::bind_at(path.clone()).expect("bind");
        let mut stream = UnixStream::connect(&path).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        stream.write_all(b"GET /keyboard HTTP/1.1\r\n\r\n").unwrap();
        let mut answer = String::new();
        stream.read_to_string(&mut answer).unwrap();
        assert!(answer.starts_with("HTTP/1.1 405 "), "{answer}");
    }

    /// A ping goes in, the loop answers, the client reads it back.
    #[test]
    fn a_ping_round_trips() {
        let path = std::env::temp_dir().join(format!("flipctl-ipc-{}.sock", std::process::id()));
        let mut listener = Listener::bind_at(path.clone()).expect("bind");
        let client = std::thread::spawn({
            let path = path.clone();
            move || send_to(&path, &Request::Ping)
        });
        let pending = loop {
            if let Some(p) = listener.poll() {
                break p;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(pending.request, Request::Ping);
        pending.ok("flipctl");
        assert_eq!(client.join().unwrap(), Reply::Ok("flipctl".into()));
        drop(listener);
        assert!(!path.exists(), "the socket is unlinked with the listener");
        assert_eq!(send_to(&path, &Request::Ping), Reply::NotRunning);
    }
}
