//! The sound server, spoken to directly over the PulseAudio socket PipeWire serves.
//!
//! flipctl links that socket into an app's runtime directory when the manifest says
//! `audio`, and every profile has `pipewire-pulse`, so this needs no program on the
//! device: the protocol is the pure-Rust `pulseaudio` crate's, used synchronously,
//! one connection per thread, as the recorder uses it. Playback, the volume, the
//! outputs and word of them changing all go through here.
//!
//! The volume is the sink's, not the stream's. A stream's volume only attenuates, so
//! turning it up as far as it goes amplifies nothing; the codec's own `Speaker`
//! control sits behind the speaker sink's volume, and its last 23dB are only reached
//! from there. It stays where it was left, and WirePlumber remembers it.

use std::ffi::CString;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;

use pulseaudio::protocol::{self, ChannelVolume, Command, CommandReply, Volume};

/// The sink flipctl pinned this app to, which is the panel's speaker. The fallback
/// is the name flipctl falls back to, so the app run by hand still finds it.
pub fn pinned() -> String {
    std::env::var("PULSE_SINK").unwrap_or_else(|_| "alsa_output.platform-sound.stereo-fallback".into())
}

pub fn err(e: protocol::ProtocolError) -> io::Error {
    match e {
        protocol::ProtocolError::Io(e) => e,
        e => io::Error::other(e.to_string()),
    }
}

pub fn cstr(s: &str) -> CString {
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
        props.set(protocol::Prop::ApplicationName, cstr("Internet radio"));
        let _: protocol::SetClientNameReply = conn.ask(&Command::SetClientName(props))?;
        Ok(conn)
    }

    pub fn send(&mut self, command: &Command) -> io::Result<u32> {
        self.seq += 1;
        protocol::write_command_message(self.sock.get_mut(), self.seq, command, self.version).map_err(err)?;
        Ok(self.seq)
    }

    /// A command and its reply, on a connection with no stream on it.
    pub fn ask<R: CommandReply>(&mut self, command: &Command) -> io::Result<R> {
        self.send(command)?;
        protocol::read_reply_message::<R>(&mut self.sock, self.version).map(|(_, r)| r).map_err(err)
    }

    /// A command answered with an empty reply.
    pub fn act(&mut self, command: &Command) -> io::Result<()> {
        self.send(command)?;
        protocol::read_ack_message(&mut self.sock).map(drop).map_err(err)
    }

    /// The sinks, as `(name, description)`, in the server's order.
    pub fn sinks(&mut self) -> io::Result<Vec<(String, String)>> {
        let sinks: protocol::SinkInfoList = self.ask(&Command::GetSinkInfoList)?;
        Ok(sinks
            .iter()
            .map(|s| {
                let name = s.name.to_string_lossy().into_owned();
                let described = s.description.as_ref().map(|d| d.to_string_lossy().into_owned());
                let description = described.unwrap_or_else(|| name.clone());
                (name, description)
            })
            .collect())
    }

    pub fn sink_volume(&mut self, name: &str) -> Option<i32> {
        let info: protocol::SinkInfo =
            self.ask(&Command::GetSinkInfo(protocol::GetSinkInfo { index: None, name: Some(cstr(name)) })).ok()?;
        Some(percent(&info.cvolume))
    }

    pub fn set_sink_volume(&mut self, name: &str, percent: i32) -> io::Result<()> {
        self.act(&Command::SetSinkVolume(protocol::SetDeviceVolumeParams {
            device_index: None,
            device_name: Some(cstr(name)),
            volume: volume(percent),
        }))
    }

    /// Move a playing stream, by its server index, to the sink called `sink`. The
    /// stream carries on where it was, so the sound moves without a gap.
    pub fn move_stream(&mut self, index: u32, sink: &str) -> io::Result<()> {
        self.act(&Command::MoveSinkInput(protocol::MoveStreamParams {
            index: Some(index),
            device_index: None,
            device_name: Some(cstr(sink)),
        }))
    }
}

/// A device's volume as the row shows it: the first channel against the server's
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

    #[test]
    fn a_volume_reads_back_as_its_percentage() {
        for p in [0, 40, 85, 100] {
            assert_eq!(percent(&volume(p)), p);
        }
        assert_eq!(percent(&volume(150)), 100, "clamped");
    }
}
