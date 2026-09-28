//! Chromium, headless, driven over the DevTools protocol.
//!
//! `--remote-debugging-pipe` puts the protocol on the child's fds 3 and 4, one JSON
//! message per NUL, so there is no port to listen on and no websocket to speak. The
//! engine never opens a window: its pictures of the page arrive as screencast frames,
//! which it sends only when the page changes and never ahead of the last one being
//! acknowledged.
//!
//! One thread reads everything the engine says. Frames are decoded there, off the
//! thread that draws, and acknowledged only once decoded, which is what keeps a busy
//! page from queueing frames faster than the device can take them.
//!
//! A tab is a page of the engine's own, a target, with a session attached to it.
//! One of them is current: commands that name no tab go to it, and only it is
//! screencast, so a tab behind the one on screen costs no frames.

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use serde_json::{json, Value};

use crate::page::Page;

/// The size the page is laid out at: seven panels wide, so x7 is one CSS pixel to
/// one panel pixel, and the shape of the panel, so x1 fills it.
pub const VIEWPORT: (u32, u32) = (1792, 1008);

/// How long a command may take to be answered before the engine is taken for gone.
const ANSWER: Duration = Duration::from_secs(20);

/// What the engine tells the app.
pub enum Event {
    /// A picture of the current tab.
    Frame(Page),
    /// A tab's top-level page moved to a new address. The tab is its session.
    Url { session: String, url: String },
    /// A tab's page finished loading, and this is its title.
    Title { session: String, title: String },
    /// The engine went away, or never came up. The reason, for the dialog.
    Gone(String),
}

/// A tab: the engine's page, and the session the app speaks to it on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub target: String,
    pub session: String,
}

/// Where a command goes.
#[derive(Copy, Clone)]
enum To<'a> {
    Browser,
    Current,
    Tab(&'a str),
}

type Pending = Mutex<HashMap<u64, mpsc::Sender<Result<Value, String>>>>;

struct Link {
    out: Mutex<File>,
    next: AtomicU64,
    pending: Pending,
    /// The current tab's session, once there is one.
    session: Mutex<Option<String>>,
    /// Titles asked for and not yet answered: the command, and whose title it is.
    titles: Mutex<HashMap<u64, String>>,
}

impl Link {
    fn write(&self, id: u64, method: &str, params: Value, to: To<'_>) -> io::Result<()> {
        let mut msg = json!({ "id": id, "method": method, "params": params });
        let session = match to {
            To::Browser => None,
            To::Current => self.session.lock().unwrap().clone(),
            To::Tab(session) => Some(session.to_string()),
        };
        if let Some(session) = session {
            msg["sessionId"] = Value::String(session);
        }
        let mut bytes = serde_json::to_vec(&msg)?;
        bytes.push(0);
        self.out.lock().unwrap().write_all(&bytes)
    }

    fn send(&self, method: &str, params: Value, to: To<'_>) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let _ = self.write(id, method, params, to);
    }

    /// Ask a tab for its title without waiting: the reader hands the answer on as
    /// an event.
    fn ask_title(&self, session: &str) {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.titles.lock().unwrap().insert(id, session.to_string());
        let ask = json!({ "expression": "document.title", "returnByValue": true });
        if self.write(id, "Runtime.evaluate", ask, To::Tab(session)).is_err() {
            self.titles.lock().unwrap().remove(&id);
        }
    }

    fn call(&self, method: &str, params: Value, to: To<'_>) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.pending.lock().unwrap().insert(id, tx);
        if let Err(e) = self.write(id, method, params, to) {
            self.pending.lock().unwrap().remove(&id);
            return Err(format!("{method}: {e}"));
        }
        match rx.recv_timeout(ANSWER) {
            Ok(answer) => answer,
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                Err(format!("{method}: no answer"))
            }
        }
    }
}

pub struct Browser {
    link: Arc<Link>,
    child: Child,
}

impl Drop for Browser {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The User-Agent the same version of desktop Chromium sends on Linux: the major
/// version and nothing finer, as Chrome's own reduced string has it, and the
/// platform it reports whatever the machine is. `None` when the engine will not say
/// which version it is, and then it keeps its own.
fn user_agent(program: &str) -> Option<String> {
    let out = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    let major = major_version(&String::from_utf8_lossy(&out.stdout))?;
    Some(format!(
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
         Chrome/{major}.0.0.0 Safari/537.36"
    ))
}

/// The major version out of `chromium --version`: "Chromium 152.0.7977.82 built on
/// Debian GNU/Linux 13 (trixie)" is 152.
fn major_version(text: &str) -> Option<u32> {
    text.split_whitespace()
        .filter(|word| word.contains('.'))
        .find_map(|word| word.split('.').next()?.parse().ok())
}

/// A pipe whose two ends are both above the fds the child is given, so moving
/// them onto 3 and 4 can never have one overwrite the other.
fn pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut high = [0; 2];
    for (i, fd) in fds.iter().enumerate() {
        high[i] = unsafe { libc::fcntl(*fd, libc::F_DUPFD_CLOEXEC, 10) };
        unsafe { libc::close(*fd) };
        if high[i] < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((high[0], high[1]))
}

impl Browser {
    /// Start the engine, with no page yet: `opener` gives it one.
    ///
    /// Call it from a thread that lives as long as the app. The engine is told to
    /// die with its parent, and to the kernel the parent is the thread that forked
    /// it: started from a thread that then returned, it was killed a second later.
    /// `tell` is called from the reader thread, once per event.
    pub fn launch(
        program: &str,
        profile: &std::path::Path,
        tell: impl Fn(Event) + Send + 'static,
    ) -> Result<Self, String> {
        let (to_child, ours_out) = pipe().map_err(|e| e.to_string())?;
        let (ours_in, from_child) = pipe().map_err(|e| e.to_string())?;

        let mut cmd = Command::new(program);
        cmd.arg("--headless")
            .arg("--remote-debugging-pipe")
            .arg(format!("--user-data-dir={}", profile.display()))
            .arg(format!("--window-size={},{}", VIEWPORT.0, VIEWPORT.1))
            .args([
                "--no-first-run",
                "--no-default-browser-check",
                "--hide-scrollbars",
                "--mute-audio",
                // navigator.webdriver, which is the first thing a bot check reads.
                "--disable-blink-features=AutomationControlled",
            ]);
        // Headless says so in its User-Agent, and DuckDuckGo answered the first
        // search with a captcha. Set for the whole engine rather than per tab: a tab
        // is created on its address and its first request is already on the wire
        // before a tab of it could be told anything, and creating it blank and
        // sending it on afterwards leaves it with no pictures (see open).
        if let Some(agent) = user_agent(program) {
            cmd.arg(format!("--user-agent={agent}"));
        }
        cmd.arg("about:blank").stdin(Stdio::null()).stdout(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                // Dies with the app, so an engine cannot outlive the window it drew in.
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                // flipctl runs with an ambient CAP_WAKE_ALARM, which every descendant
                // inherits, and a sandbox refuses to start beside capabilities it
                // cannot account for. None of the engine wants it.
                libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
                if libc::dup2(to_child, 3) < 0 || libc::dup2(from_child, 4) < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let spawned = cmd.spawn();
        unsafe {
            libc::close(to_child);
            libc::close(from_child);
        }
        let child = match spawned {
            Ok(child) => child,
            Err(e) => {
                unsafe {
                    libc::close(ours_out);
                    libc::close(ours_in);
                }
                return Err(format!("{program}: {e}"));
            }
        };

        let link = Arc::new(Link {
            out: Mutex::new(unsafe { File::from_raw_fd(ours_out) }),
            next: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            session: Mutex::new(None),
            titles: Mutex::new(HashMap::new()),
        });
        let reader = unsafe { File::from_raw_fd(ours_in) };
        let listening = Arc::clone(&link);
        std::thread::Builder::new()
            .name("cdp".into())
            .spawn(move || read(reader, &listening, &tell))
            .map_err(|e| e.to_string())?;

        Ok(Self { link, child })
    }

    /// Put `to` on screen: its pictures start and the one before's stop, and every
    /// command that names no tab goes to it from now on.
    pub fn switch(&self, from: Option<&Tab>, to: &Tab) {
        if let Some(from) = from.filter(|from| *from != to) {
            self.link.send("Page.stopScreencast", json!({}), To::Tab(&from.session));
        }
        *self.link.session.lock().unwrap() = Some(to.session.clone());
        // To the front first: a tab behind the one opened last is hidden, and a
        // hidden page is not painted, so it would be screencast and send nothing.
        self.link.send("Page.bringToFront", json!({}), To::Tab(&to.session));
        self.link.send("Page.startScreencast", screencast(), To::Tab(&to.session));
    }

    pub fn close(&self, tab: &Tab) {
        self.link.send("Target.closeTarget", json!({ "targetId": tab.target }), To::Browser);
    }

    pub fn reload(&self) {
        self.link.send("Page.reload", json!({}), To::Current);
    }

    /// The engine's link for another thread: for the calls that wait on an answer
    /// from the page, which can take as long as the page likes.
    pub fn remote(&self) -> Remote {
        Remote(Arc::clone(&self.link))
    }

    pub fn navigate(&self, url: &str) {
        self.link.send("Page.navigate", json!({ "url": url }), To::Current);
    }

    /// One page back. False when there is nothing to go back to.
    pub fn back(&self) -> bool {
        self.step(-1)
    }

    pub fn forward(&self) -> bool {
        self.step(1)
    }

    /// `by` pages through the current tab's history. False when that is off its end.
    fn step(&self, by: isize) -> bool {
        let Ok(history) = self.link.call("Page.getNavigationHistory", json!({}), To::Current)
        else {
            return false;
        };
        let at = history["currentIndex"].as_u64().unwrap_or(0) as isize;
        let entries = history["entries"].as_array().cloned().unwrap_or_default();
        let Some(next) = usize::try_from(at + by).ok().and_then(|i| entries.get(i)) else {
            return false;
        };
        // The blank page a target is born on is not somewhere to go back to.
        if next["url"].as_str().is_some_and(|u| u == "about:blank") {
            return false;
        }
        self.link.send(
            "Page.navigateToHistoryEntry",
            json!({ "entryId": next["id"] }),
            To::Current,
        );
        true
    }

    fn mouse(&self, kind: &str, at: (f32, f32), extra: Value) {
        let mut params = json!({ "type": kind, "x": at.0, "y": at.1 });
        if let (Value::Object(p), Value::Object(e)) = (&mut params, extra) {
            p.extend(e);
        }
        self.link.send("Input.dispatchMouseEvent", params, To::Current);
    }

    /// The pointer is here, in CSS pixels: what makes a page's hover states show.
    pub fn hover(&self, at: (f32, f32)) {
        self.mouse("mouseMoved", at, json!({}));
    }

    pub fn wheel(&self, at: (f32, f32), by: (f32, f32)) {
        self.mouse("mouseWheel", at, json!({ "deltaX": by.0, "deltaY": by.1 }));
    }
}

/// A text field on the page, as the keyboard needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub value: String,
    /// What the page calls it: its label, placeholder or name, whichever it has.
    pub label: String,
}

/// The element that has the focus, followed into shadow roots and into frames the
/// page can reach. Defined once and put in front of each script that needs it.
const FOCUSED: &str = "const focused = () => {
    let e = document.activeElement;
    for (;;) {
        if (e && e.shadowRoot && e.shadowRoot.activeElement) { e = e.shadowRoot.activeElement; continue; }
        if (e && e.tagName === 'IFRAME') {
            let d = null;
            try { d = e.contentDocument; } catch (_) {}
            if (d && d.activeElement) { e = d.activeElement; continue; }
        }
        return e;
    }
};
const typed = ['', 'text', 'search', 'email', 'url', 'tel', 'password', 'number'];
const field = (e) => e && !e.disabled && !e.readOnly && (
    (e.tagName === 'INPUT' && typed.includes((e.getAttribute('type') || '').toLowerCase()))
    || e.tagName === 'TEXTAREA' || e.isContentEditable);
";

/// What the page answers to a script, or `None` for a script that threw or
/// answered nothing.
fn value(answer: Value) -> Option<Value> {
    if answer.get("exceptionDetails").is_some() {
        return None;
    }
    let v = answer["result"]["value"].clone();
    (!v.is_null()).then_some(v)
}

/// The engine's link, for a thread of its own.
pub struct Remote(Arc<Link>);

impl Remote {
    fn evaluate(&self, script: &str) -> Result<Option<Value>, String> {
        let answer = self.0.call(
            "Runtime.evaluate",
            json!({ "expression": format!("(() => {{ {FOCUSED} {script} }})()"), "returnByValue": true }),
            To::Current,
        )?;
        Ok(value(answer))
    }

    /// A click, waited on: the engine answers each half once the page has had it,
    /// so whatever is asked next sees the focus where the click put it.
    pub fn click(&self, at: (f32, f32)) -> Result<(), String> {
        let at = json!({ "x": at.0, "y": at.1, "button": "left", "buttons": 1, "clickCount": 1 });
        for kind in ["mouseMoved", "mousePressed", "mouseReleased"] {
            let mut params = at.clone();
            params["type"] = Value::String(kind.into());
            self.0.call("Input.dispatchMouseEvent", params, To::Current)?;
        }
        Ok(())
    }

    /// The text field that has the focus, if the focus is in one.
    pub fn focused(&self) -> Result<Option<Field>, String> {
        let found = self.evaluate(
            "const e = focused();
            if (!field(e)) return null;
            const label = (e.labels && e.labels[0] && e.labels[0].innerText)
                || e.getAttribute('aria-label') || e.placeholder || e.name || '';
            return { value: e.isContentEditable ? e.innerText : e.value, label: label.trim() };",
        )?;
        Ok(found.map(|v| Field {
            value: v["value"].as_str().unwrap_or_default().to_string(),
            label: v["label"].as_str().unwrap_or_default().to_string(),
        }))
    }

    /// Replace the focused field's text with `text`, as typing would: its contents
    /// selected and typed over, so the page hears the same input events a keyboard
    /// makes. False when the focus has left the field meanwhile.
    pub fn fill(&self, text: &str) -> Result<bool, String> {
        let selected = self.evaluate(
            "const e = focused();
            if (!field(e)) return false;
            if (e.isContentEditable) {
                const r = document.createRange();
                r.selectNodeContents(e);
                const s = getSelection();
                s.removeAllRanges();
                s.addRange(r);
            } else {
                e.select();
            }
            return true;",
        )?;
        if selected != Some(Value::Bool(true)) {
            return Ok(false);
        }
        if text.is_empty() {
            self.evaluate("document.execCommand('delete'); return true;")?;
        } else {
            self.0.call("Input.insertText", json!({ "text": text }), To::Current)?;
        }
        Ok(true)
    }

    /// A new tab on `url`, attached to and laid out at the viewport. Not on screen
    /// until it is switched to.
    pub fn open(&self, url: &str) -> Result<Tab, String> {
        let link = &self.0;
        let target = link.call("Target.createTarget", json!({ "url": url }), To::Browser)?;
        let target = target["targetId"].as_str().ok_or("no target")?.to_string();
        let attached = link.call(
            "Target.attachToTarget",
            json!({ "targetId": target, "flatten": true }),
            To::Browser,
        )?;
        let session = attached["sessionId"].as_str().ok_or("no session")?.to_string();
        let tab = To::Tab(&session);
        link.call(
            "Emulation.setDeviceMetricsOverride",
            json!({ "width": VIEWPORT.0, "height": VIEWPORT.1, "deviceScaleFactor": 1, "mobile": false }),
            tab,
        )?;
        link.call("Page.enable", json!({}), tab)?;
        // A page quick enough to have loaded before Page.enable never says it has,
        // so its title is asked for now as well as when it loads.
        link.ask_title(&session);
        Ok(Tab { target, session })
    }
}

fn screencast() -> Value {
    json!({ "format": "jpeg", "quality": 85, "maxWidth": VIEWPORT.0, "maxHeight": VIEWPORT.1 })
}

/// The reader: answers to their callers, events to the app.
fn read(from: File, link: &Link, tell: &impl Fn(Event)) {
    let mut from = BufReader::with_capacity(1 << 20, from);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match from.read_until(0, &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if buf.last() == Some(&0) {
            buf.pop();
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else { continue };

        if let Some(id) = msg["id"].as_u64() {
            // A title the reader asked for itself, which nobody else is waiting on.
            if let Some(session) = link.titles.lock().unwrap().remove(&id) {
                if let Some(title) = msg["result"]["result"]["value"].as_str() {
                    tell(Event::Title { session, title: title.to_string() });
                }
                continue;
            }
            let answer = match msg.get("error") {
                Some(e) => Err(e["message"].as_str().unwrap_or("error").to_string()),
                None => Ok(msg["result"].clone()),
            };
            match link.pending.lock().unwrap().remove(&id) {
                Some(tx) => {
                    let _ = tx.send(answer);
                }
                // Nobody is waiting on a command that was only sent, so a refusal
                // of one would otherwise vanish.
                None => {
                    if let Err(why) = answer {
                        eprintln!("browser: the engine refused command {id}: {why}");
                    }
                }
            }
            continue;
        }

        let params = &msg["params"];
        let from = msg["sessionId"].as_str().unwrap_or_default();
        let current = link.session.lock().unwrap().as_deref() == Some(from);
        match msg["method"].as_str().unwrap_or_default() {
            "Page.screencastFrame" => {
                // Acknowledged whichever tab it came from, since that is what lets the
                // engine send the next one, but shown only for the tab on screen: a
                // frame still in flight from the tab just left is not a picture of this
                // one.
                let frame = current.then(|| decode(params)).flatten();
                link.send(
                    "Page.screencastFrameAck",
                    json!({ "sessionId": params["sessionId"] }),
                    To::Tab(from),
                );
                if let Some(page) = frame {
                    tell(Event::Frame(page));
                }
            }
            "Page.frameNavigated" if params["frame"].get("parentId").is_none() => {
                if let Some(url) = params["frame"]["url"].as_str() {
                    tell(Event::Url { session: from.to_string(), url: url.to_string() });
                }
            }
            // The engine does not say when a title changes: its target events carry
            // the one the page had when it was created, which is its address. So
            // each tab is asked once it has loaded, without waiting on the answer.
            "Page.loadEventFired" => link.ask_title(from),
            "Inspector.targetCrashed" => tell(Event::Gone("The page crashed".into())),
            _ => {}
        }
    }
    // Whoever is still waiting for an answer is not getting one.
    link.pending.lock().unwrap().clear();
    tell(Event::Gone("Chromium stopped".into()));
}

/// A screencast frame as a page: the JPEG's luma is exactly the grey the panel
/// wants, so the colour is never decoded at all.
fn decode(params: &Value) -> Option<Page> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;

    let data = base64::engine::general_purpose::STANDARD.decode(params["data"].as_str()?).ok()?;
    let options = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::Luma);
    let mut decoder = zune_jpeg::JpegDecoder::new_with_options(io::Cursor::new(&data), options);
    let grey = decoder.decode().ok()?;
    let (w, h) = decoder.dimensions()?;
    let meta = &params["metadata"];
    let css_w = meta["deviceWidth"].as_f64().unwrap_or(w as f64) as f32;
    let css_h = meta["deviceHeight"].as_f64().unwrap_or(h as f64) as f32;
    let mut page =
        Page::from_luma(w as u32, h as u32, &grey, (css_w / w as f32, css_h / h as f32))?;
    page.at_top = meta["scrollOffsetY"].as_f64().unwrap_or(0.0) < 1.0;
    Some(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine itself, end to end: a page goes in and a picture of it comes out.
    /// Needs a chromium on the machine, so it runs only when asked for:
    /// `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn chromium_sends_a_picture_of_the_page() {
        let dir = std::env::temp_dir().join(format!("browser-app-test-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let page = "data:text/html,<body style='margin:0;background:black'></body>";
        let browser = Browser::launch("chromium", &dir, move |event| {
            let _ = tx.lock().unwrap().send(event);
        })
        .expect("launch");
        let tab = browser.remote().open(page).expect("open");
        browser.switch(None, &tab);
        let frame = loop {
            match rx.recv_timeout(Duration::from_secs(20)).expect("an event") {
                Event::Frame(page) => break page,
                Event::Gone(why) => panic!("{why}"),
                Event::Url { .. } | Event::Title { .. } => {}
            }
        };
        assert_eq!((frame.w, frame.h), VIEWPORT);
        let middle = frame.render(crate::view::Placement { s: 1.0, dx: -896, dy: -504 }, 1, 1);
        assert!(middle[0] < 16, "a black page came out as {}", middle[0]);
        drop(browser);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_version_comes_out_of_what_chromium_prints() {
        let debian = "Chromium 152.0.7977.82 built on Debian GNU/Linux 13 (trixie)";
        assert_eq!(major_version(debian), Some(152));
        assert_eq!(major_version("Chromium 152.0.7977.82 Arch Linux"), Some(152));
        assert_eq!(major_version("something else entirely"), None);
    }

    /// A page is served to an ordinary desktop Chromium: no Headless in the
    /// User-Agent or its client hints, and navigator.webdriver unset. `cargo test
    /// -- --ignored`.
    #[test]
    #[ignore]
    fn a_page_sees_an_ordinary_browser() {
        use std::io::Read as _;
        let server = std::net::TcpListener::bind("127.0.0.1:0").expect("listen");
        let port = server.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = mpsc::channel();
        std::thread::spawn(move || {
            for stream in server.incoming().flatten() {
                let mut stream = stream;
                let mut buf = [0u8; 8192];
                let n = stream.read(&mut buf).unwrap_or(0);
                // A connection opened ahead of time and never used says nothing.
                if n == 0 {
                    continue;
                }
                let head = String::from_utf8_lossy(&buf[..n]).to_string();
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 13\r\n\r\n<p>hello</p>\n",
                );
                let _ = seen_tx.send(head);
            }
        });

        let dir = std::env::temp_dir().join(format!("browser-app-ua-{}", std::process::id()));
        let browser = Browser::launch("chromium", &dir, |_| {}).expect("launch");
        let remote = browser.remote();
        let tab = remote.open(&format!("http://127.0.0.1:{port}/")).expect("open");
        browser.switch(None, &tab);
        let head = seen_rx.recv_timeout(Duration::from_secs(20)).expect("a request");
        assert!(!head.to_lowercase().contains("headless"), "{head}");
        assert!(head.contains("Chrome/"), "{head}");
        std::thread::sleep(Duration::from_millis(500));
        let seen = remote
            .evaluate("return [navigator.userAgent, String(navigator.webdriver)];")
            .expect("ask");
        let seen = seen.expect("an answer");
        assert!(!seen[0].as_str().unwrap().contains("Headless"), "{seen}");
        assert_ne!(seen[1], "true", "navigator.webdriver is set");
        drop(browser);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Two tabs, a black page and a white one: only the one on screen sends
    /// pictures, switching swaps which, and both titles arrive. `cargo test --
    /// --ignored`.
    #[test]
    #[ignore]
    fn only_the_tab_on_screen_is_seen() {
        let dir = std::env::temp_dir().join(format!("browser-app-tabs-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let tx = Mutex::new(tx);
        let browser = Browser::launch("chromium", &dir, move |event| {
            let _ = tx.lock().unwrap().send(event);
        })
        .expect("launch");
        let remote = browser.remote();
        let black = remote
            .open("data:text/html,<title>Black</title><body style='margin:0;background:black'>")
            .expect("open black");
        let white = remote
            .open("data:text/html,<title>White</title><body style='margin:0;background:white'>")
            .expect("open white");
        let middle = |page: &Page| {
            page.render(crate::view::Placement { s: 1.0, dx: -896, dy: -504 }, 1, 1)[0]
        };
        // Everything for a while: the frames in order, and every title seen.
        let settle = |titles: &mut Vec<String>| {
            let mut frames = Vec::new();
            while let Ok(event) = rx.recv_timeout(Duration::from_secs(2)) {
                match event {
                    Event::Frame(page) => frames.push(middle(&page)),
                    Event::Title { title, .. } => titles.push(title),
                    Event::Gone(why) => panic!("{why}"),
                    Event::Url { .. } => {}
                }
            }
            frames
        };
        let mut titles = Vec::new();

        browser.switch(None, &black);
        let frames = settle(&mut titles);
        assert!(!frames.is_empty() && frames.iter().all(|&g| g < 16), "black tab: {frames:?}");

        browser.switch(Some(&black), &white);
        let frames = settle(&mut titles);
        assert!(!frames.is_empty(), "no picture of the white tab");
        assert!(*frames.last().unwrap() > 240, "white tab ended on {frames:?}");

        assert!(
            titles.iter().any(|t| t == "Black") && titles.iter().any(|t| t == "White"),
            "{titles:?}"
        );
        browser.close(&black);
        drop(browser);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A click in a field finds it, with what the page calls it, and a fill types
    /// over it so that the page hears it as input. A click on anything else finds
    /// nothing. `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn a_click_in_a_field_finds_it_and_a_fill_types_into_it() {
        let dir = std::env::temp_dir().join(format!("browser-app-field-{}", std::process::id()));
        let page = "data:text/html,<body style='margin:0'>\
            <label for=q>Search</label>\
            <input id=q value=old style='position:absolute;left:100px;top:100px;width:300px;height:40px'\
                oninput='document.title=this.value'>\
            <p style='position:absolute;left:100px;top:400px'>words</p></body>";
        let browser = Browser::launch("chromium", &dir, |_| {}).expect("launch");
        let remote = browser.remote();
        let tab = remote.open(page).expect("open");
        browser.switch(None, &tab);
        std::thread::sleep(Duration::from_millis(500));

        remote.click((250.0, 120.0)).expect("click");
        let field = remote.focused().expect("ask").expect("the click focused no field");
        assert_eq!(field, Field { value: "old".into(), label: "Search".into() });

        assert!(remote.fill("new words").expect("fill"));
        let typed = remote.evaluate("return [focused().value, document.title];").expect("read");
        assert_eq!(typed, Some(json!(["new words", "new words"])), "typed over, and heard");

        assert!(remote.fill("").expect("clear"));
        let cleared = remote.evaluate("return focused().value;").expect("read");
        assert_eq!(cleared, Some(json!("")));

        remote.click((120.0, 410.0)).expect("click");
        assert_eq!(remote.focused().expect("ask"), None, "text is not a field");
        drop(browser);
        let _ = std::fs::remove_dir_all(dir);
    }
}
