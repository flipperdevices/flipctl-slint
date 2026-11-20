//! Carrying the sound, with PipeWire's own tools: a `pw-loopback` per direction that
//! goes device to device, `pw-link` for the Flipper's apps, and `pw-record` for the
//! levels. Each is a child that dies with the app.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::graph::{speaker, Graph, OURS};

/// How long a loopback buffers, in ms: enough to ride out a busy moment, little enough
/// that the PC's picture and the Flipper's sound stay together.
const LATENCY_MS: &str = "30";

/// Start `cmd` so that it ends when the app does, however the app ends.
pub fn spawn(cmd: &mut Command, stdout: Stdio) -> std::io::Result<Child> {
    unsafe {
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
            Ok(())
        });
    }
    cmd.stdin(Stdio::null()).stdout(stdout).stderr(Stdio::null()).spawn()
}

pub fn end(child: &mut Child) {
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
}

/// One device's sound carried to another, from `source` to `sink` by node name.
pub struct Loopback {
    pub source: String,
    pub sink: String,
    child: Child,
}

impl Loopback {
    pub fn start(name: &str, source: &str, sink: &str) -> std::io::Result<Loopback> {
        let side = |end: &str, target: &str| {
            format!("node.name={OURS}.{name}.{end} target.object={target} node.dont-reconnect=true")
        };
        let child = spawn(
            Command::new("pw-loopback")
                .args(["-n", &format!("{OURS}.{name}"), "-l", LATENCY_MS])
                .args(["--capture-props", &side("in", source)])
                .args(["--playback-props", &side("out", sink)]),
            Stdio::null(),
        )?;
        Ok(Loopback { source: source.into(), sink: sink.into(), child })
    }

    /// What it carries, from and to, while it runs. One that has ended carries nothing,
    /// so it is started again.
    pub fn running(&mut self) -> Option<(String, String)> {
        matches!(self.child.try_wait(), Ok(None)).then(|| (self.source.clone(), self.sink.clone()))
    }
}

impl Drop for Loopback {
    fn drop(&mut self) {
        end(&mut self.child);
    }
}

/// Set a node's own volume, 0 to 100 on the scale wpctl and the volume keys use.
pub fn set_volume(id: u64, percent: i32) -> bool {
    Command::new("wpctl")
        .args(["set-volume", &id.to_string(), &format!("{:.2}", f64::from(percent) / 100.0)])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// How long a test tone lasts, its pitch, and its level: 15% of full scale,
/// about -16.5 dBFS, which is plainly audible and nowhere near loud.
const TONE_MS: u32 = 2000;
const TONE_HZ: f32 = 440.0;
const TONE_PEAK: f32 = 4915.0;

/// A test tone played into `sink`, ending by itself.
pub struct Tone {
    child: Child,
}

impl Tone {
    pub fn start(sink: &str) -> std::io::Result<Tone> {
        let mut cmd = Command::new("pw-play");
        cmd.args(["--target", sink, "-P", &format!("node.name={OURS}.test"), "--raw"]).args([
            "--rate",
            "48000",
            "--channels",
            "2",
            "--format",
            "s16",
            "-",
        ]);
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let mut child =
            cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        let mut input = child.stdin.take();
        std::thread::spawn(move || {
            let Some(input) = input.as_mut() else { return };
            let frames = 48 * TONE_MS;
            let pcm: Vec<u8> = (0..frames)
                .flat_map(|i| {
                    let t = i as f32 / 48000.0;
                    let v = (TONE_PEAK * (std::f32::consts::TAU * TONE_HZ * t).sin()) as i16;
                    let b = v.to_le_bytes();
                    [b[0], b[1], b[0], b[1]]
                })
                .collect();
            // Closing the pipe is what ends pw-play once it has played the lot
            let _ = input.write_all(&pcm);
        });
        Ok(Tone { child })
    }
}

impl Drop for Tone {
    fn drop(&mut self) {
        end(&mut self.child);
    }
}

/// The loudest sample a node has carried since the last look, from `pw-record`.
pub struct Meter {
    child: Child,
    peak: Arc<AtomicU32>,
}

impl Meter {
    /// `sink` reads what goes into a sink, rather than what a source gives.
    pub fn start(name: &str, target: &str, sink: bool) -> std::io::Result<Meter> {
        let props = format!("node.name={OURS}.meter.{name} stream.capture.sink={sink}");
        let mut child = spawn(
            Command::new("pw-record").args(["--target", target, "-P", &props, "--raw"]).args([
                "--rate",
                "48000",
                "--channels",
                "2",
                "--format",
                "s16",
                "-",
            ]),
            Stdio::piped(),
        )?;
        let peak = Arc::new(AtomicU32::new(0));
        let out = child.stdout.take();
        let seen = peak.clone();
        std::thread::spawn(move || {
            let Some(mut out) = out else { return };
            // 10 ms of stereo s16, a whole number of samples per read
            let mut buf = [0u8; 1920];
            while out.read_exact(&mut buf).is_ok() {
                let loudest = buf
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|s| i16::from_le_bytes(*s).unsigned_abs() as u32)
                    .max()
                    .unwrap_or(0);
                seen.fetch_max(loudest, Ordering::Relaxed);
            }
        });
        Ok(Meter { child, peak })
    }

    pub fn take(&self) -> u32 {
        self.peak.swap(0, Ordering::Relaxed)
    }
}

impl Drop for Meter {
    fn drop(&mut self) {
        end(&mut self.child);
    }
}

/// What the watcher last saw.
#[derive(Clone)]
pub struct Seen {
    pub graph: Result<Graph, String>,
    /// The PC has the sound card configured.
    pub connected: bool,
    /// The speaker's node, its volume in percent and whether it is muted.
    pub speaker: Option<(u64, (i32, bool))>,
}

impl Default for Seen {
    fn default() -> Seen {
        Seen { graph: Err(String::new()), connected: false, speaker: None }
    }
}

/// A thread that reads the graph, the sound card and the speaker's volume twice a
/// second, and while asked to, links every app's stream to the card's input towards
/// the PC. It stops when dropped, and its links go with the card or are taken down
/// when it is told to stop.
pub struct Watch {
    pub seen: Arc<Mutex<Seen>>,
    link: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Watch {
    pub fn start() -> Watch {
        let seen = Arc::new(Mutex::new(Seen { graph: Graph::read(), ..Seen::default() }));
        let (link, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (s, l, t) = (seen.clone(), link.clone(), stop.clone());
        std::thread::spawn(move || {
            let mut made: Vec<(u64, u64)> = Vec::new();
            while !t.load(Ordering::Relaxed) {
                let graph = Graph::read();
                if let Ok(g) = &graph {
                    made.retain(|m| g.links.contains(m));
                    match g.gadget("Audio/Sink") {
                        Some(card) if l.load(Ordering::Relaxed) => {
                            for (out, input) in g.missing_links(card.id) {
                                if pw_link(&[&out.to_string(), &input.to_string()]) {
                                    made.push((out, input));
                                }
                            }
                        }
                        _ => {
                            for (out, input) in made.drain(..) {
                                pw_link(&["-d", &out.to_string(), &input.to_string()]);
                            }
                        }
                    }
                }
                let speaker = graph
                    .as_ref()
                    .ok()
                    .and_then(|g| g.node_named(&speaker()).map(|n| n.id))
                    .and_then(|id| get_volume(id).map(|v| (id, v)));
                let now = Seen { connected: udc_state() == "configured", speaker, graph };
                *s.lock().unwrap() = now;
                flipctl_app::redraw();
                std::thread::sleep(Duration::from_millis(500));
            }
            for (out, input) in made {
                pw_link(&["-d", &out.to_string(), &input.to_string()]);
            }
        });
        Watch { seen, link, stop }
    }

    /// Whether the Flipper's apps are heard by the PC.
    pub fn link_streams(&self, on: bool) {
        self.link.store(on, Ordering::Relaxed);
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// A node's volume in percent and whether it is muted, as wpctl reads it.
pub fn get_volume(id: u64) -> Option<(i32, bool)> {
    let out = Command::new("wpctl")
        .args(["get-volume", &id.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_volume(&String::from_utf8_lossy(&out.stdout))
}

fn parse_volume(said: &str) -> Option<(i32, bool)> {
    let level: f64 = said.strip_prefix("Volume: ")?.split_whitespace().next()?.parse().ok()?;
    Some(((level * 100.0).round() as i32, said.contains("[MUTED]")))
}

/// Mute a node or not.
pub fn set_mute(id: u64, muted: bool) -> bool {
    Command::new("wpctl")
        .args(["set-mute", &id.to_string(), if muted { "1" } else { "0" }])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn pw_link(args: &[&str]) -> bool {
    Command::new("pw-link")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The device port's state, as the kernel names it: "configured" once a PC has
/// enumerated the sound card.
fn udc_state() -> String {
    let ports = std::fs::read_dir("/sys/class/udc").into_iter().flatten().flatten();
    for port in ports.filter(|p| !p.file_name().to_string_lossy().contains("vudc")) {
        if let Ok(state) = std::fs::read_to_string(port.path().join("state")) {
            return state.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_volume_is_read_from_wpctl() {
        assert_eq!(parse_volume("Volume: 0.25\n"), Some((25, false)));
        assert_eq!(parse_volume("Volume: 1.00 [MUTED]\n"), Some((100, true)));
        assert_eq!(parse_volume(""), None);
    }
}
