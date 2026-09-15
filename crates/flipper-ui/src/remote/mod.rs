//! Browser remote view: watch the panel and press its buttons from a PC.
//!
//! std only. No HTTP crate, no WebSocket, so no SHA-1 and no base64 handshake:
//! frames go out as an HTTP/1.1 chunked response that the browser reads with
//! `fetch` and a `ReadableStream`, and key presses come back as small POSTs. The
//! whole server is one accept thread plus one thread per connection.
//!
//! Presence-gated, the way fake-flipctl2's mirror was: with nobody watching,
//! `commit` copies nothing and the render loop is unaffected. That is what lets
//! this be compiled into flipctl without being a tax when unused.
//!
//! Behind the `remote` feature, off by default, so flipperos-installer and
//! flipper-boot-menu never carry it.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crate::pixel::Gray8;
use crate::pixel::Rect;
use crate::platform::{Frame, FrameSink, InputSource};
use crate::{FlipperKey, KeyEvent};

/// Frames per second the stream is allowed to push.
///
/// 30 rather than 10, because the cap quantises how long a state is visible: a
/// 30ms press flash sent at 10fps is held for the next 100ms, which reads as the
/// UI being sluggish even though the panel itself drew it for 30ms. At 30fps the
/// granularity is 33ms, close enough that a flash looks like a flash.
///
/// A whole frame is 36864 bytes, so this is 1.1MB/s while something is moving and
/// nothing at all on a still screen, since frames are only produced when the
/// screen changes.
const MAX_FPS: u64 = 30;

/// 8-byte little-endian header, then `w * h` bytes of 8-bit greyscale.
/// Cap on a POST /input body. The only bodies are small JSON key events, so this
/// bounds what an unauthenticated caller can make us allocate. Named because a
/// bare 256 next to a panel-sized buffer reads like a width.
const MAX_INPUT_BODY: usize = 256; // not-a-panel-dimension

/// Cap on a POST body naming one app to install or remove. A path under the apps
/// folder, so larger than a key event and still nothing worth allocating for.
const MAX_ACTION_BODY: usize = 1024; // not-a-panel-dimension

const HEADER: usize = 8;

/// How many frames may wait to be sent. Three is enough to carry a 30ms flash
/// through a 33ms send interval without letting a stalled viewer accumulate
/// megabytes.
const QUEUE_DEPTH: usize = 3;

/// One viewer's pending frames, oldest first.
///
/// Per viewer, not shared. A single queue read with `pop_front` hands each frame
/// to whichever pump wakes first, so two viewers each see about half the frames
/// and a reconnecting one steals from the other. That is exactly the hazard the
/// note below describes for a damage accumulator, and a shared queue has it too:
/// a browser tab that reloads leaves its old connection alive until it errors, so
/// in normal use there are often two.
struct ViewerQueue {
    frames: Mutex<std::collections::VecDeque<StoredFrame>>,
    ready: Condvar,
}

struct Shared {
    /// The registered viewers, each with its own queue.
    ///
    /// Deliberately not a damage accumulator. Per-frame damage sounds like the
    /// thrifty choice, but a single shared accumulator is consumed by whichever
    /// viewer reads first, so a second viewer, or one that reconnects, gets a
    /// partial screen or nothing at all. A whole 256x144 greyscale frame is
    /// 36864 bytes, and at the 30fps cap that is 1.1 MB/s, which is free on any
    /// link this device has. Correctness is worth more than the bandwidth.
    ///
    /// A short queue per viewer rather than a single slot, because the UI has
    /// states that live for less than one send interval: a soft button's press
    /// flash is 30ms against a 33ms cadence, so storing only the newest frame
    /// overwrites the pressed frame before any viewer sees it. That looked like the
    /// press state failing intermittently, and looked different per row because
    /// animated icons commit every 200ms and shift the sender's phase. Keeping a
    /// couple of frames means a state that appeared and vanished is still
    /// transmitted, in order.
    viewer_queues: Mutex<Vec<Arc<ViewerQueue>>>,
    /// The last frame committed, kept so a viewer that connects to a still screen
    /// is handed the screen as it stands.
    ///
    /// Counting arrivals in the render loop covers the same case, but only when
    /// the count changes between two passes: a tab that reloads, or one that
    /// replaces another, leaves the count where it was and the newcomer waits for
    /// the screen to move. Priming at registration does not depend on the loop
    /// noticing anything.
    last: Mutex<Option<StoredFrame>>,
    generation: AtomicU64,
    viewers: AtomicUsize,
    /// What the panel currently has running, refreshed by the render loop.
    ///
    /// The page can remove an app, and removing one that is running is refused
    /// here for the same reason it is refused on the panel: the process holds its
    /// files open and its window on the glass. Only the loop knows what is
    /// running, so it says so rather than this side guessing.
    running: Mutex<Vec<String>>,
    /// An app the page has asked to start, by key, waiting for the loop to take it.
    ///
    /// Not started here: launching means checking the runtime, offering to install
    /// what is missing, and hosting the window, none of which this thread can do.
    /// The loop runs it through the same path a key press does. One slot, because
    /// the panel shows one app at a time and a queue of them would be a queue of
    /// screens nobody asked for.
    launch: Mutex<Option<String>>,
    /// Set when the page has installed or removed something.
    ///
    /// The loop keeps its own list of apps and would otherwise go on offering one
    /// that has just been deleted from underneath it. It clears this when it has
    /// looked again.
    apps_changed: AtomicBool,
}

#[derive(Clone)]
struct StoredFrame {
    pixels: Vec<Gray8>,
    w: u16,
    h: u16,
}

pub struct RemoteView {
    shared: Arc<Shared>,
    events: Receiver<KeyEvent>,
    addr: std::net::SocketAddr,
    last_viewers: usize,
}

impl RemoteView {
    /// Bind and start serving. `assets` is the directory holding `device.png`.
    pub fn bind(addr: &str, assets: PathBuf) -> std::io::Result<Self> {
        Self::bind_with_peer(addr, assets, None)
    }

    /// As `bind`, plus a `host:port` running the fake-flipctl2 prototype.
    ///
    /// With a peer, `/` serves the side-by-side comparison and `/peer.png`
    /// proxies the prototype's `/api/screen`. Proxying server-side keeps the page
    /// same-origin, which matters because the prototype's server sends no CORS
    /// headers. The photo view stays available at `/device`.
    pub fn bind_with_peer(
        addr: &str,
        assets: PathBuf,
        peer: Option<String>,
    ) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        let local = listener.local_addr()?;

        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            viewers: AtomicUsize::new(0),
            viewer_queues: Mutex::new(Vec::new()),
            last: Mutex::new(None),
            running: Mutex::new(Vec::new()),
            launch: Mutex::new(None),
            apps_changed: AtomicBool::new(false),
        });
        let (tx, events) = mpsc::channel();

        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("remote-accept".into())
                .spawn(move || accept_loop(listener, shared, tx, assets, peer))?;
        }

        Ok(Self { shared, events, addr: local, last_viewers: 0 })
    }

    /// Tell the page what is running, so it refuses to remove one of them.
    ///
    /// Called from the render loop, which is the only place that knows. Cheap
    /// enough to call every pass: it replaces a short vector of names.
    pub fn set_running(&self, names: Vec<String>) {
        if let Ok(mut running) = self.shared.running.lock() {
            *running = names;
        }
    }

    /// The app the page asked to start, if any. Taking it clears it.
    pub fn take_launch(&self) -> Option<String> {
        self.shared.launch.lock().ok().and_then(|mut slot| slot.take())
    }

    /// Whether the page has installed or removed something since this was asked.
    ///
    /// Taking it clears it, so the loop rediscovers once per change rather than
    /// once per pass forever after.
    pub fn take_apps_changed(&self) -> bool {
        self.shared.apps_changed.swap(false, Ordering::Relaxed)
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.addr
    }

    pub fn viewers(&self) -> usize {
        self.shared.viewers.load(Ordering::Relaxed)
    }

    /// True once per viewer arrival.
    ///
    /// A still screen produces no commits, so a browser opened mid-idle would
    /// otherwise see nothing at all until something moved. The render loop
    /// answers this by re-committing the frame it already has.
    pub fn take_new_viewer(&mut self) -> bool {
        let now = self.viewers();
        let arrived = now > self.last_viewers;
        self.last_viewers = now;
        arrived
    }
}

impl FrameSink for RemoteView {
    fn commit(&mut self, frame: Frame<'_>, _damage: Rect) -> std::io::Result<()> {
        // Nobody watching: do no work at all. This is what lets the view be
        // compiled into flipctl without costing anything when unused.
        if self.shared.viewers.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }

        let stored = StoredFrame { pixels: frame.pixels.to_vec(), w: frame.w, h: frame.h };
        {
            let viewers = self.shared.viewer_queues.lock().unwrap_or_else(|e| e.into_inner());
            for viewer in viewers.iter() {
                let mut queue = viewer.frames.lock().unwrap_or_else(|e| e.into_inner());
                // Bounded: a viewer that stalls must not grow this without limit.
                // When it is full the oldest goes, because the newest screen
                // matters more than a complete history.
                if queue.len() >= QUEUE_DEPTH {
                    queue.pop_front();
                }
                queue.push_back(stored.clone());
                viewer.ready.notify_all();
            }
        }

        *self.shared.last.lock().unwrap_or_else(|e| e.into_inner()) = Some(stored);

        self.shared.generation.fetch_add(1, Ordering::Release);
        Ok(())
    }
}

impl InputSource for RemoteView {
    fn poll(&mut self) -> Option<KeyEvent> {
        self.events.try_recv().ok()
    }
}

fn accept_loop(
    listener: TcpListener,
    shared: Arc<Shared>,
    tx: Sender<KeyEvent>,
    assets: PathBuf,
    peer: Option<String>,
) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let shared = shared.clone();
        let tx = tx.clone();
        let assets = assets.clone();
        let peer = peer.clone();
        // One thread per connection. A viewer holds its /stream open, so this is
        // one long-lived thread per browser tab plus short-lived ones for the
        // page, the photo and each key press.
        let _ = std::thread::Builder::new().name("remote-conn".into()).spawn(move || {
            let _ = serve(stream, &shared, &tx, &assets, peer.as_deref());
        });
    }
}

fn serve(
    mut stream: TcpStream,
    shared: &Arc<Shared>,
    tx: &Sender<KeyEvent>,
    assets: &PathBuf,
    peer: Option<&str>,
) -> std::io::Result<()> {
    stream.set_nodelay(true)?;
    let mut reader = BufReader::new(stream.try_clone()?);

    let mut request = String::new();
    if reader.read_line(&mut request)? == 0 {
        return Ok(());
    }
    let mut parts = request.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or_default().to_string();

    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) =
            line.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        {
            content_length = v.1.trim().parse().unwrap_or(0);
        }
    }

    let path = target.split('?').next().unwrap_or("/");
    // The shared script's URL carries a version, and the pages are rewritten to
    // request that exact one.
    //
    // Changing the cache header alone does not help a browser that already cached
    // the old copy: it keeps using it until the original lifetime expires. When
    // that stale copy predates a function the page calls, the page's script throws
    // on load and nothing renders at all, which looks like the stream being broken
    // rather than a caching problem. A versioned URL cannot be answered from a
    // cache entry for a different version.
    let shared_js = include_str!("remote.js");
    let device_page = include_str!("page.html");
    let compare_page = include_str!("compare.html");
    let apps_page = include_str!("apps.html");

    match (method.as_str(), path) {
        // The panel in its device photo is the default view; the side-by-side
        // comparison lives at /diff. /device and /compare are kept as aliases so
        // older links do not break.
        ("GET", "/") | ("GET", "/index.html") | ("GET", "/device") => write_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            device_page.as_bytes(),
        ),
        // Shared browser code, so a fix cannot land in one page and not the other.
        //
        // Revalidated, never held: a day-long cache meant a browser kept running
        // the copy it had long after the device had a new one, and the page then
        // called a function that was not there yet, which reads as a view that
        // never connects. The file is 7KB, so a conditional request costs nothing
        // worth saving. A query string is accepted so an old page still resolves.
        ("GET", path) if path == "/remote.js" || path.starts_with("/remote.js?") => write_cached(
            &mut stream,
            "application/javascript; charset=utf-8",
            shared_js.as_bytes(),
            0,
        ),
        // The app manager, as a page: what the device has installed beside what the
        // catalogue offers it. A reader, not a second set of controls; installing is
        // still the panel's business.
        ("GET", "/apps") | ("GET", "/apps.html") => {
            write_response(&mut stream, "200 OK", "text/html; charset=utf-8", apps_page.as_bytes())
        }
        // Both lists, answered from the same code the panel reads them with.
        //
        // The catalogue is a network round trip, taken on this connection's own
        // thread: a viewer waits for their page, and nothing the panel is doing
        // waits for them.
        ("GET", "/apps.json") => write_response(
            &mut stream,
            "200 OK",
            "application/json; charset=utf-8",
            apps_json().as_bytes(),
        ),
        ("GET", "/diff") | ("GET", "/compare") => write_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            compare_page.as_bytes(),
        ),
        ("GET", "/peer.png") => match peer {
            Some(peer) => match fetch_peer_screen(peer) {
                Ok(Some(bytes)) => write_response(&mut stream, "200 OK", "image/png", &bytes),
                // 204: the prototype has no frame yet. The page keeps whatever it
                // last drew rather than flashing.
                Ok(None) => write_response(&mut stream, "204 No Content", "image/png", b""),
                Err(e) => write_response(
                    &mut stream,
                    "502 Bad Gateway",
                    "text/plain",
                    e.to_string().as_bytes(),
                ),
            },
            None => {
                write_response(&mut stream, "404 Not Found", "text/plain", b"no --peer configured")
            }
        },
        ("GET", "/device.png") => match std::fs::read(assets.join("device.png")) {
            // 2 MB, and identical for the life of the build. Served from disk so
            // it never enters a binary, and cacheable so a page reload does not
            // re-fetch it.
            // The photo never changes; a day is fine.
            Ok(bytes) => write_cached(&mut stream, "image/png", &bytes, 86_400),
            // A missing photo should not take the view down: the panel still
            // renders, just without its surround.
            Err(_) => write_response(
                &mut stream,
                "404 Not Found",
                "text/plain",
                b"device.png not installed",
            ),
        },
        ("GET", "/stream") => stream_frames(stream, shared),
        // Install and remove, each naming one app and answering when it is done.
        //
        // Synchronous, on this connection's thread: an install is a download the
        // page is waiting for, and a status it can show beats a job id it would
        // have to poll for. Nothing the panel is doing waits on either.
        ("POST", "/apps/install") | ("POST", "/apps/remove") | ("POST", "/apps/run") => {
            let mut body = vec![0; content_length.min(MAX_ACTION_BODY)];
            reader.read_exact(&mut body)?;
            let said = String::from_utf8_lossy(&body);
            let done = match (path, field(&said, "path"), field(&said, "key")) {
                ("/apps/install", Some(want), _) => install_one(shared, &want),
                ("/apps/remove", _, Some(want)) => remove_one(shared, &want),
                ("/apps/run", _, Some(want)) => run_one(shared, &want),
                _ => Err("no app was named".into()),
            };
            let answer = match &done {
                Ok(said) => format!("{{\"ok\":true,\"said\":\"{}\"}}", escape(said)),
                Err(why) => format!("{{\"ok\":false,\"said\":\"{}\"}}", escape(why)),
            };
            write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                answer.as_bytes(),
            )
        }
        ("POST", "/input") => {
            let mut body = vec![0; content_length.min(MAX_INPUT_BODY)];
            reader.read_exact(&mut body)?;
            if let Some(event) = parse_input(&body) {
                let _ = tx.send(event);
            }
            write_response(&mut stream, "200 OK", "text/plain", b"ok")
        }
        _ => write_response(&mut stream, "404 Not Found", "text/plain", b"not found"),
    }
}

/// Both app lists as JSON: what is installed, and what the catalogue offers.
///
/// Written by hand rather than derived, which is how the manifests behind both of
/// these are read too: the document is two fixed shapes, and deriving it would put
/// the crate's only serialisation dependency here.
fn apps_json() -> String {
    use crate::app::human_size;

    let roots = crate::bundle::roots();
    let installed = crate::bundle::discover_all(&roots);
    let mut out = String::from("{\"installed\":[");
    for (i, app) in installed.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let at = if app.shipped() {
            String::from("Part of the image")
        } else {
            let mut at = String::from("Apps");
            for folder in &app.group {
                at.push('/');
                at.push_str(folder);
            }
            at
        };
        let tag = app.tag();
        let size = human_size(app.footprint().total());
        let size = if tag.is_empty() { size } else { format!("{size}  {tag}") };
        out.push_str(&format!(
            "{{\"name\":\"{}\",\"key\":\"{}\",\"location\":\"{}\",\"size\":\"{}\",\"shipped\":{}}}",
            escape(&app.name),
            escape(&app.key),
            escape(&at),
            escape(&size),
            app.shipped()
        ));
    }
    out.push_str("],\"offered\":[");
    let offered = crate::catalogue::fetch_index();
    if let Ok(offers) = &offered {
        for (i, offer) in offers.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&format!(
                "{{\"name\":\"{}\",\"path\":\"{}\",\"folder\":\"{}\",\"size\":\"{}\",\"installed\":{}}}",
                escape(&offer.name),
                escape(&offer.path),
                escape(offer.folder()),
                escape(&human_size(offer.size)),
                offer.present(&roots)
            ));
        }
    }
    out.push(']');
    if let Err(why) = &offered {
        out.push_str(&format!(",\"error\":\"{}\"", escape(why)));
    }
    out.push('}');
    out
}

/// Install the catalogue entry with this path, and whatever it needs with it.
///
/// The same queue the panel builds: the runtime first, so a script is never on the
/// device ahead of what starts it. Progress goes nowhere, because the page is
/// waiting on the answer rather than watching a bar.
fn install_one(shared: &Arc<Shared>, path: &str) -> Result<String, String> {
    let offers = crate::catalogue::fetch_index()?;
    let offer = offers
        .iter()
        .find(|o| o.path == path)
        .ok_or_else(|| "the catalogue does not offer that".to_string())?;
    let roots = crate::bundle::roots();
    let installed = crate::bundle::discover_all(&roots);

    let mut queue = Vec::new();
    match crate::catalogue::needs(offer, &installed, &offers) {
        crate::catalogue::Needs::Runtime(runtime) => queue.push(runtime.clone()),
        crate::catalogue::Needs::Unavailable(runtime) => {
            return Err(format!("needs the {runtime} runtime, which is not on offer"));
        }
        crate::catalogue::Needs::Nothing => {}
    }
    queue.push(offer.clone());

    let root = crate::bundle::root();
    for item in &queue {
        let mut failed = None;
        crate::catalogue::install(item, &root, |p| {
            if let crate::fetch::Progress::Failed(why) = p {
                failed = Some(why);
            }
        });
        // Set either way: a queue that fails on its second file has its first one
        // installed, and the panel has to hear about that too.
        shared.apps_changed.store(true, Ordering::Relaxed);
        if let Some(why) = failed {
            return Err(why);
        }
    }
    Ok(match queue.len() {
        1 => format!("{} installed", offer.name),
        _ => format!("{} installed, with {}", offer.name, queue[0].name),
    })
}

/// Remove the installed app with this key.
///
/// Refused for an app the image shipped, which `uninstall` decides, and for one
/// that is running, which only the render loop knows.
fn remove_one(shared: &Arc<Shared>, key: &str) -> Result<String, String> {
    let roots = crate::bundle::roots();
    let apps = crate::bundle::discover_all(&roots);
    let app = apps.iter().find(|a| a.key == key).ok_or_else(|| "no such app".to_string())?;
    let running = shared.running.lock().map(|r| r.contains(&app.name)).unwrap_or(false);
    if running {
        return Err(format!("{} is running: close it first", app.name));
    }
    app.uninstall().map_err(|e| e.to_string())?;
    shared.apps_changed.store(true, Ordering::Relaxed);
    Ok(format!("{} uninstalled", app.name))
}

/// Ask the panel to start the installed app with this key.
///
/// Checked here and started there: the name is resolved now so a stale page gets
/// told, and the launch itself is left to the loop, which is where the runtime
/// check, the dependency question and the hosting live.
fn run_one(shared: &Arc<Shared>, key: &str) -> Result<String, String> {
    let apps = crate::bundle::discover_all(&crate::bundle::roots());
    let app = apps.iter().find(|a| a.key == key).ok_or_else(|| "no such app".to_string())?;
    if shared.running.lock().map(|r| r.contains(&app.name)).unwrap_or(false) {
        return Ok(format!("{} is already running", app.name));
    }
    match shared.launch.lock() {
        Ok(mut slot) => {
            *slot = Some(key.to_string());
            Ok(format!("{} starting", app.name))
        }
        Err(_) => Err("the panel is not listening".to_string()),
    }
}

/// One `"key":"value"` out of a small JSON object.
///
/// The bodies here are two fields written by our own page, so this reads a string
/// by name and understands the escapes `JSON.stringify` produces, rather than
/// being a parser. It is the counterpart of [`escape`].
fn field(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let at = body.find(&needle)? + needle.len();
    let rest = body[at..].trim_start().strip_prefix(':')?.trim_start();
    let mut chars = rest.strip_prefix('"')?.chars();
    let mut out = String::new();
    loop {
        match chars.next()? {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    out.push(char::from_u32(u32::from_str_radix(&hex, 16).ok()?)?);
                }
                c => out.push(c),
            },
            c => out.push(c),
        }
    }
}

/// A string as the body of a JSON string, without its quotes.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Fetch the prototype's current screen.
///
/// A deliberately small HTTP/1.0 client: one request, `Connection: close`, read
/// to EOF, so there is no keep-alive or chunked decoding to get wrong. Returns
/// `None` for the prototype's 204, which is what it answers before cog has
/// uploaded anything.
fn fetch_peer_screen(peer: &str) -> std::io::Result<Option<Vec<u8>>> {
    let mut stream = TcpStream::connect(peer)?;
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    write!(stream, "GET /api/screen HTTP/1.0\r\nHost: {peer}\r\nConnection: close\r\n\r\n")?;
    stream.flush()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw)?;

    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::other("peer sent no header terminator"))?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let status = head.lines().next().unwrap_or_default();

    if status.contains(" 204") {
        return Ok(None);
    }
    if !status.contains(" 200") {
        return Err(std::io::Error::other(format!("peer answered {status:?}")));
    }
    Ok(Some(raw[split + 4..].to_vec()))
}

/// Minimal parse of `{"key":"up","down":true}`. Deliberately not a JSON parser:
/// this endpoint accepts two fields and anything else is ignored.
fn parse_input(body: &[u8]) -> Option<KeyEvent> {
    let text = std::str::from_utf8(body).ok()?;
    let key_start = text.find("\"key\"")? + 5;
    let rest = &text[key_start..];
    let open = rest.find('"')? + 1;
    let close = rest[open..].find('"')? + open;
    let key = FlipperKey::from_name(&rest[open..close])?;
    let down = text.contains("\"down\":true") || text.contains("\"down\": true");
    Some(KeyEvent { key, down })
}

/// A static body with an explicit cache lifetime.
///
/// The device photo never changes and is worth caching for a day. The shared
/// script asks for 0, so the browser revalidates it on every load rather than
/// running whatever it kept from an earlier deploy.
fn write_cached(
    stream: &mut TcpStream,
    content_type: &str,
    body: &[u8],
    max_age: u32,
) -> std::io::Result<()> {
    let cache =
        if max_age == 0 { "no-cache".to_string() } else { format!("public, max-age={max_age}") };
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: {cache}\r\n\
         Connection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)?;
    stream.flush()
}

/// Hold the connection open and push each new frame as a chunk.
///
/// Chunk boundaries are not visible to `fetch`, so the frame header is in-band
/// and the browser reassembles. Registering as a viewer here is what un-gates
/// `commit`.
fn stream_frames(mut stream: TcpStream, shared: &Arc<Shared>) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\n\
         Transfer-Encoding: chunked\r\nCache-Control: no-store\r\n\r\n"
    )?;
    stream.flush()?;

    let queue = Arc::new(ViewerQueue {
        frames: Mutex::new(std::collections::VecDeque::new()),
        ready: Condvar::new(),
    });
    // Registered and counted before the first frame goes in, so a commit landing
    // in between reaches this queue too, and the worst case is the same frame
    // twice rather than none at all.
    shared.viewer_queues.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::clone(&queue));
    shared.viewers.fetch_add(1, Ordering::Relaxed);
    // The screen as it stands. Without this a viewer joining a still screen has
    // nothing to draw until something moves, which on an idle menu can be a long
    // wait, and reads as a page that never connected.
    if let Some(frame) = shared.last.lock().unwrap_or_else(|e| e.into_inner()).clone() {
        queue.frames.lock().unwrap_or_else(|e| e.into_inner()).push_back(frame);
        queue.ready.notify_all();
    }

    let result = pump(&mut stream, &queue);

    shared
        .viewer_queues
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|v| !Arc::ptr_eq(v, &queue));
    shared.viewers.fetch_sub(1, Ordering::Relaxed);

    // Terminating chunk, best effort: the viewer has usually gone already.
    let _ = stream.write_all(b"0\r\n\r\n");
    result
}

/// An 8-byte header describing a zero-area region. The page ignores it; its only
/// job is to make a vanished viewer show up as a write error.
fn write_heartbeat(stream: &mut TcpStream) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", HEADER)?;
    stream.write_all(&[0u8; HEADER])?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn pump(stream: &mut TcpStream, viewer: &Arc<ViewerQueue>) -> std::io::Result<()> {
    let frame_time = Duration::from_millis(1000 / MAX_FPS);
    let mut body = Vec::new();
    // When the last frame went out, so the rate limit can be a minimum interval
    // between sends rather than a sleep after each one. Started far enough back
    // that the first frame goes immediately.
    let mut last_sent = std::time::Instant::now() - frame_time;

    loop {
        let mut queue = viewer.frames.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if !queue.is_empty() {
                break;
            }
            let (guard, timed_out) = viewer
                .ready
                .wait_timeout(queue, Duration::from_secs(2))
                .unwrap_or_else(|e| e.into_inner());
            queue = guard;
            if !queue.is_empty() {
                break;
            }
            if timed_out.timed_out() {
                // A still screen produces no frames, which is the point, but the
                // connection must not look finished and a viewer that walked away
                // must be noticed. An empty region does both: the page skips it,
                // and a dead socket surfaces here as a write error.
                drop(queue);
                write_heartbeat(stream)?;
                queue = viewer.frames.lock().unwrap_or_else(|e| e.into_inner());
            }
        }
        let stored = queue.pop_front().expect("checked above");
        // Whether more frames are already waiting, decided before the lock goes.
        let burst = !queue.is_empty();
        drop(queue);

        // Rate-limit by when the last frame was sent, not by sleeping after
        // sending one.
        //
        // Sleeping afterwards spent the cap on an idle screen and then charged a
        // keypress for it: a press landing inside that window waited out whatever
        // remained, up to the full frame time. Measuring from the last send means a
        // screen that has been still for longer than the interval sends
        // immediately, which is every interactive press.
        //
        // A burst is exempt, so the two frames of a 30ms press flash still arrive
        // back to back instead of being stretched to the cap.
        if !burst {
            let since = last_sent.elapsed();
            if since < frame_time {
                std::thread::sleep(frame_time - since);
            }
        }

        body.clear();
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&0u16.to_le_bytes());
        body.extend_from_slice(&stored.w.to_le_bytes());
        body.extend_from_slice(&stored.h.to_le_bytes());
        body.extend(stored.pixels.iter().map(|p| p.0));

        debug_assert_eq!(body.len(), HEADER + usize::from(stored.w) * usize::from(stored.h));
        write!(stream, "{:x}\r\n", body.len())?;
        stream.write_all(&body)?;
        stream.write_all(b"\r\n")?;
        stream.flush()?;
        last_sent = std::time::Instant::now();
    }
}
