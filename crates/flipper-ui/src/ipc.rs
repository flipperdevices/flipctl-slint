//! One socket a desktop hands an app to.
//!
//! A bundle double-clicked on the HDMI desktop belongs on the panel, and flipctl is
//! the only thing that can put it there. So the bundle's AppRun asks: `flipctl open
//! <file>` connects here, one line each way, and the running flipctl lists, starts
//! or fronts the app and answers. The desktop and flipctl run as the same user, and
//! the socket sits in that user's 0700 runtime directory, which is the whole of the
//! access control.
//!
//! Not the browser view's HTTP port: that is an optional feature bound on every
//! interface with no authentication, and a request that runs a file is not something
//! to hang off it.

use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
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
        if BufReader::new(reading.take(MAX_LINE as u64 + 1)).read_line(&mut line).is_err() {
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
