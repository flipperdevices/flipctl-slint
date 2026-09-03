//! The player, and the socket it answers questions on.
//!
//! mpv rather than mpg123, which the prototype used: mpg123 decodes an MP3 stream
//! in a tenth of the size, but it cannot be asked anything while it does it. mpv
//! takes a JSON command socket, and that one socket is where three of this app's
//! rows get their answers: the volume it applies live, the output device it can
//! move a running stream to, and the station's own now-playing title.
//!
//! The socket carries JSON lines, so a reply is one line of text with one field
//! worth reading. That is parsed here by hand rather than with serde: the app has
//! no other use for a JSON library, and the alternative is a dependency and a
//! derive for every shape of answer mpv gives.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// How long to wait for a reply. mpv answers a property read in well under a
/// millisecond; the timeout is there so a wedged player cannot stall the frame.
const REPLY_TIMEOUT: Duration = Duration::from_millis(300);

/// Where mpv listens: the app's own runtime directory, which flipctl makes for it.
/// Nothing there survives a reboot and no other app can see it.
fn socket_path() -> PathBuf {
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("radio-mpv.sock")
}

/// One playing stream.
pub struct Player {
    child: Child,
    socket: PathBuf,
}

impl Player {
    /// Start playing `url`, at `volume` percent, on `device` or on the sink flipctl
    /// pinned for the app.
    pub fn start(url: &str, volume: i32, device: Option<&str>) -> std::io::Result<Self> {
        let socket = socket_path();
        // A socket left by a player that was killed rather than stopped would
        // otherwise make mpv refuse to listen, and every question after that would
        // be answered by a file nobody is reading.
        let _ = std::fs::remove_file(&socket);

        let mut command = Command::new("mpv");
        command
            .arg("--no-video")
            .arg("--no-terminal")
            // A stream has no seekable start, and mpv's cache is what turns a
            // stutter in the network into silence rather than a gap.
            .arg("--cache=yes")
            .arg(format!("--volume={volume}"))
            .arg(format!("--input-ipc-server={}", socket.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(device) = device {
            command.arg(format!("--audio-device={device}"));
        }
        command.arg(url);
        // The player dies with the app. Without this a stream survives the window
        // that started it, and nothing on the panel can stop it any more.
        unsafe {
            command.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        let child = command.spawn()?;
        Ok(Self { child, socket })
    }

    /// Whether the player is still running.
    ///
    /// This is the whole of the failure check: a dead URL, a missing codec or a
    /// name that does not resolve all end the same way, with mpv exiting a second
    /// or two after it started.
    pub fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    /// Ask mpv for the value of a property, as a string.
    pub fn get(&self, property: &str) -> Option<String> {
        self.ask(&format!("\"get_property_string\",\"{property}\""))
    }

    /// Set a property. The answer is not read: there is nothing to do about a
    /// refusal, and the next frame shows what the player actually did.
    pub fn set(&self, property: &str, value: &str) {
        self.ask(&format!("\"set_property\",\"{property}\",{value}"));
    }

    /// One request and the `data` of its reply.
    ///
    /// mpv volunteers events on the same socket, so the reply is found by its
    /// request id rather than by being the next line to arrive.
    fn ask(&self, command: &str) -> Option<String> {
        let mut stream = UnixStream::connect(&self.socket).ok()?;
        stream.set_read_timeout(Some(REPLY_TIMEOUT)).ok()?;
        writeln!(stream, "{{\"command\":[{command}],\"request_id\":1}}").ok()?;
        let reader = BufReader::new(stream);
        // Bounded, so a player that is talking about something else cannot hold
        // the loop here: the events it sends while starting a stream are a handful.
        for line in reader.lines().take(64).map_while(Result::ok) {
            if line.contains("\"request_id\":1") {
                return field(&line, "data");
            }
        }
        None
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// The outputs mpv can play to, as `(what to pass it, what to show)`.
///
/// Only its PipeWire ones. flipctl gives an app the sound server and pins the
/// panel's speaker, so those are the outputs that exist for it; the ALSA and
/// PulseAudio views mpv also lists are the same hardware reached another way, and
/// offering all three would be a picker with every device in it three times.
pub fn devices() -> Vec<(String, String)> {
    let out = Command::new("mpv").arg("--audio-device=help").output();
    let text = out
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    text.lines().filter_map(entry).collect()
}

/// One line of `--audio-device=help`: `  'pipewire/<node>' (Description)`.
fn entry(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    let rest = line.strip_prefix('\'')?;
    let (id, rest) = rest.split_once('\'')?;
    // A bare "pipewire" is the server's own default, which is the sink flipctl
    // pinned: that is what the app plays to when it passes no device at all.
    let node = id.strip_prefix("pipewire/")?;
    if node.is_empty() {
        return None;
    }
    // One bracket off each end, and no more: a description of its own can have
    // brackets in it, and "Digital Stereo (HDMI)" is the usual one.
    let label = rest.trim();
    let label = label.strip_prefix('(').unwrap_or(label);
    let label = label.strip_suffix(')').unwrap_or(label);
    Some((id.to_string(), label.to_string()))
}

/// The value of `"<key>":` in a JSON line.
///
/// A quoted string comes back unescaped; a number, a boolean or null comes back as
/// the bare token. Enough for mpv's replies, which put one value in `data`.
///
/// Whatever the station sent, in the alphabet it sent it in: making a title
/// drawable is `font::ascii`'s job where it goes on screen, not this one's.
fn field(line: &str, key: &str) -> Option<String> {
    let at = line.find(&format!("\"{key}\":"))? + key.len() + 3;
    let rest = line[at..].trim_start();
    let Some(quoted) = rest.strip_prefix('"') else {
        let end = rest.find([',', '}']).unwrap_or(rest.len());
        let token = rest[..end].trim();
        return (token != "null").then(|| token.to_string());
    };
    let mut out = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' | 't' | 'r' => out.push(' '),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let code = u32::from_str_radix(&hex, 16).ok()?;
                    out.push(char::from_u32(code).unwrap_or('?'));
                }
                escaped => out.push(escaped),
            },
            c => out.push(c),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_gives_up_its_data() {
        let line = r#"{"data":"Radio 4 - The Archers","request_id":1,"error":"success"}"#;
        assert_eq!(field(line, "data").as_deref(), Some("Radio 4 - The Archers"));
        assert_eq!(field(line, "error").as_deref(), Some("success"));
        assert_eq!(field(line, "nothing"), None);
    }

    #[test]
    fn a_property_that_is_not_set_reads_as_nothing() {
        let line = r#"{"data":null,"request_id":1,"error":"property unavailable"}"#;
        assert_eq!(field(line, "data"), None);
    }

    #[test]
    fn a_bare_value_needs_no_quotes() {
        assert_eq!(field(r#"{"data":60,"error":"success"}"#, "data").as_deref(), Some("60"));
        assert_eq!(field(r#"{"data":true}"#, "data").as_deref(), Some("true"));
    }

    /// An escape is undone, and the title comes back in the alphabet the station
    /// sent it in.
    #[test]
    fn escapes_are_undone_and_the_alphabet_is_left_alone() {
        let line = r#"{"data":"Rock \"n\" Roll \\ Hour","request_id":1}"#;
        assert_eq!(field(line, "data").as_deref(), Some("Rock \"n\" Roll \\ Hour"));
        let cyrillic = r#"{"data":"Наше Радио","request_id":1}"#;
        assert_eq!(field(cyrillic, "data").as_deref(), Some("Наше Радио"));
        // The same title as JSON \u escapes, which is how some servers send it.
        let escaped = r#"{"data":"Наше","request_id":1}"#;
        assert_eq!(field(escaped, "data").as_deref(), Some("Наше"));
    }

    #[test]
    fn only_the_sound_servers_own_devices_are_offered() {
        assert_eq!(
            entry("  'pipewire/alsa_output.platform-sound.stereo-fallback' (Panel Speaker)"),
            Some((
                "pipewire/alsa_output.platform-sound.stereo-fallback".into(),
                "Panel Speaker".into()
            ))
        );
        // The same hardware through the other two APIs, and the server's own
        // default, which is what passing nothing already means.
        assert_eq!(entry("  'pulse/alsa_output.platform-sound' (Panel Speaker)"), None);
        assert_eq!(entry("  'alsa/sysdefault' (Default Audio Device)"), None);
        assert_eq!(entry("  'pipewire' (Default (pipewire))"), None);
        assert_eq!(entry("  'auto' (Autoselect device)"), None);
        assert_eq!(entry("List of detected audio devices:"), None);
    }

    /// A description with brackets of its own keeps them: only the pair the list
    /// itself puts around the label comes off.
    #[test]
    fn a_bracketed_description_survives() {
        let line = "  'pipewire/alsa_output.pci.hdmi-stereo' (AD107 Digital Stereo (HDMI))";
        assert_eq!(entry(line).unwrap().1, "AD107 Digital Stereo (HDMI)");
    }
}

/// Against a real player and a real station, so `#[ignore]`: it needs mpv, the
/// network and a few seconds. Run it with
///
///     cargo test --release -- --ignored --nocapture
///
/// It is the only test that covers the argument list and the reply matching, which
/// is where a change here would break quietly: a bad flag makes mpv exit at once
/// and every question then goes unanswered rather than wrong.
#[cfg(test)]
mod live {
    use super::*;

    #[test]
    #[ignore = "needs mpv, the network and about ten seconds"]
    fn a_station_plays_and_says_what_it_is_playing() {
        // Volume zero, so running the suite on somebody's desk is silent. It also
        // proves --volume is accepted, since mpv reports back what it took.
        let mut player = Player::start("http://media-ice.musicradio.com/HeartLondonMP3", 0, None)
            .expect("start mpv");

        // Wait for sound: `time-pos` appears when audio starts flowing, which is
        // exactly what the app waits for before it claims to be playing.
        let mut waited = 0;
        while player.get("time-pos").is_none() && waited < 20 {
            std::thread::sleep(Duration::from_millis(500));
            waited += 1;
            assert!(player.alive(), "mpv exited instead of playing");
        }
        let position = player.get("time-pos").expect("no position after ten seconds");
        assert!(position.parse::<f64>().expect("a number") > 0.0, "{position}");

        assert_eq!(player.get("volume").as_deref(), Some("0.000000"));
        player.set("volume", "35");
        assert_eq!(player.get("volume").as_deref(), Some("35.000000"));

        // The station's own title. Not asserted to be anything in particular: it
        // is whatever is on the radio, in whatever alphabet. What is asserted is
        // that it survives `font::ascii` as something drawable, since that is the
        // form the page shows.
        let title = player.get("metadata/by-key/icy-title").unwrap_or_default();
        let drawable = flipctl_app::font::ascii(&title);
        println!("icy-title: {title:?} -> {drawable:?}");
        assert!(drawable.chars().all(|c| (' '..='~').contains(&c)), "{drawable:?}");

        // Dropping it ends the player rather than leaving a stream playing with
        // nothing on screen to stop it.
        let socket = player.socket.clone();
        drop(player);
        assert!(!socket.exists(), "the socket outlived the player");
    }
}
