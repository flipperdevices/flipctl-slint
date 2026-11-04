//! The sound server, spoken to directly over the PulseAudio socket PipeWire serves.
//!
//! flipctl links that socket into an app's runtime directory when the manifest says
//! `audio`, so this needs no program on the device: the protocol is the pure-Rust
//! `pulseaudio` crate's, used synchronously, one connection per thread. Capture,
//! playback and the two volumes all go through here, which is what lets the app run
//! with nothing installed beyond the sound server itself.

use std::ffi::CString;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;

use pulseaudio::protocol::{self, ChannelVolume, Command, CommandReply, Volume};

/// The codec's capture side, by name. Card numbers move between kernels; this name
/// is derived from the platform device.
pub const SOURCE: &str = "alsa_input.platform-sound.stereo-fallback";

/// The sink flipctl pinned this app to, which is the panel's speaker.
pub fn sink() -> String {
    std::env::var("PULSE_SINK").unwrap_or_else(|_| "alsa_output.platform-sound.stereo-fallback".into())
}

pub fn err(e: protocol::ProtocolError) -> io::Error {
    match e {
        protocol::ProtocolError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

fn cstr(s: &str) -> CString {
    CString::new(s).unwrap_or_default()
}

/// One authenticated connection.
pub struct Conn {
    pub sock: BufReader<UnixStream>,
    pub version: u16,
    seq: u32,
}

impl Conn {
    pub fn open() -> io::Result<Self> {
        let path = pulseaudio::socket_path_from_env().ok_or_else(|| io::Error::other("no sound server"))?;
        let mut conn = Self { sock: BufReader::new(UnixStream::connect(path)?), version: protocol::MAX_VERSION, seq: 0 };
        let cookie = pulseaudio::cookie_path_from_env().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        let auth: protocol::AuthReply = conn.ask(&Command::Auth(protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        }))?;
        conn.version = conn.version.min(auth.version);
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::ApplicationName, cstr("Voice recorder"));
        let _: protocol::SetClientNameReply = conn.ask(&Command::SetClientName(props))?;
        Ok(conn)
    }

    pub fn send(&mut self, command: &Command) -> io::Result<u32> {
        self.seq += 1;
        protocol::write_command_message(self.sock.get_mut(), self.seq, command, self.version).map_err(err)?;
        Ok(self.seq)
    }

    /// A command under a sequence number of the caller's, to know its reply by.
    pub fn send_as(&mut self, seq: u32, command: &Command) -> io::Result<()> {
        protocol::write_command_message(self.sock.get_mut(), seq, command, self.version).map_err(err)
    }

    /// A command and its reply, on a connection with no stream on it yet.
    pub fn ask<R: CommandReply>(&mut self, command: &Command) -> io::Result<R> {
        self.send(command)?;
        protocol::read_reply_message::<R>(&mut self.sock, self.version).map(|(_, r)| r).map_err(err)
    }

    /// A command answered with an empty reply.
    fn act(&mut self, command: &Command) -> io::Result<()> {
        self.send(command)?;
        protocol::read_ack_message(&mut self.sock).map(drop).map_err(err)
    }

    /// The source's volume, 0 to 100, or nothing when it is not there.
    pub fn source_volume(&mut self, name: &str) -> Option<i32> {
        let info: protocol::SourceInfo =
            self.ask(&Command::GetSourceInfo(protocol::GetSourceInfo { index: None, name: Some(cstr(name)) })).ok()?;
        Some(percent(&info.cvolume))
    }

    pub fn sink_volume(&mut self, name: &str) -> Option<i32> {
        let info: protocol::SinkInfo =
            self.ask(&Command::GetSinkInfo(protocol::GetSinkInfo { index: None, name: Some(cstr(name)) })).ok()?;
        Some(percent(&info.cvolume))
    }

    /// The outputs a take can be played on, as `(sink name, label)`: the panel's
    /// speaker, HDMI and DisplayPort first, by those names, then anything else plugged in
    /// under its own description. HDMI is there only while a display is.
    pub fn outputs(&mut self) -> Vec<(String, String)> {
        let sinks: protocol::SinkInfoList = self.ask(&Command::GetSinkInfoList).unwrap_or_default();
        let mut out: Vec<(usize, String, String)> = sinks
            .iter()
            .map(|s| {
                let name = s.name.to_string_lossy().into_owned();
                let described = s.description.as_ref().map(|d| d.to_string_lossy().into_owned());
                let (rank, label) = label(&name, described.as_deref());
                (rank, name, label)
            })
            .collect();
        out.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
        out.into_iter().map(|(_, name, label)| (name, label)).collect()
    }

    pub fn set_source_volume(&mut self, name: &str, percent: i32) -> io::Result<()> {
        self.act(&Command::SetSourceVolume(protocol::SetDeviceVolumeParams {
            device_index: None,
            device_name: Some(cstr(name)),
            volume: volume(percent),
        }))
    }

    pub fn set_sink_volume(&mut self, name: &str, percent: i32) -> io::Result<()> {
        self.act(&Command::SetSinkVolume(protocol::SetDeviceVolumeParams {
            device_index: None,
            device_name: Some(cstr(name)),
            volume: volume(percent),
        }))
    }
}

/// What an output is called on the row, and where it sorts.
fn label(name: &str, description: Option<&str>) -> (usize, String) {
    let lower = name.to_ascii_lowercase();
    if lower.contains("hdmi") {
        (1, "HDMI".into())
    } else if lower.contains("-dp") || lower.contains("displayport") {
        (2, "DisplayPort".into())
    } else if lower.contains("platform-sound") {
        (0, "Speaker".into())
    } else {
        (3, flipctl_app::font::ascii(description.unwrap_or(name)))
    }
}

/// A device's volume as the rows show it: the first channel against the server's
/// 100%, the same number wpctl prints.
fn percent(cv: &ChannelVolume) -> i32 {
    let raw = cv.channels().first().map_or(0, Volume::as_u32);
    (raw as f64 * 100.0 / f64::from(Volume::NORM.as_u32())).round() as i32
}

/// Both channels at `percent` of the server's 100%.
fn volume(percent: i32) -> ChannelVolume {
    let raw = (f64::from(Volume::NORM.as_u32()) * f64::from(percent.clamp(0, 100)) / 100.0).round() as u32;
    let mut cv = ChannelVolume::empty();
    cv.push(Volume::from_u32_clamped(raw));
    cv.push(Volume::from_u32_clamped(raw));
    cv
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The device's own sink names, and the radio's HDMI fixture.
    #[test]
    fn outputs_are_named_for_the_connector() {
        assert_eq!(label("alsa_output.platform-sound.stereo-fallback", None), (0, "Speaker".into()));
        assert_eq!(label("alsa_output.platform-dp0-sound.stereo-fallback", None), (2, "DisplayPort".into()));
        assert_eq!(label("alsa_output.platform-hdmi-sound.HiFi__HDMI__sink", None), (1, "HDMI".into()));
        assert_eq!(label("bluez_output.AA_BB.1", Some("Headphones")), (3, "Headphones".into()));
    }

    #[test]
    fn a_volume_reads_back_as_its_percentage() {
        for p in [0, 40, 85, 100] {
            assert_eq!(percent(&volume(p)), p);
        }
        assert_eq!(percent(&volume(150)), 100, "clamped");
    }
}
