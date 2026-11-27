//! The sound server, spoken to directly over the PulseAudio socket PipeWire serves.
//!
//! The same connection as Voice Recorder's: flipctl links the socket into the app's
//! runtime directory when the manifest says `audio`, and the protocol is the pure-Rust
//! `pulseaudio` crate's, used synchronously.

use std::ffi::CString;
use std::io::{self, BufReader};
use std::os::unix::net::UnixStream;

use pulseaudio::protocol::{self, Command, CommandReply};

/// The codec's capture side, by name.
pub const MIC: &str = "alsa_input.platform-sound.stereo-fallback";

/// What the panel's speaker plays, which is where flipctl pins every app's sound.
pub fn speaker_monitor() -> String {
    let sink = std::env::var("PULSE_SINK")
        .unwrap_or_else(|_| "alsa_output.platform-sound.stereo-fallback".into());
    format!("{sink}.monitor")
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
        let path = pulseaudio::socket_path_from_env()
            .ok_or_else(|| io::Error::other("no sound server"))?;
        let mut conn = Self {
            sock: BufReader::new(UnixStream::connect(path)?),
            version: protocol::MAX_VERSION,
            seq: 0,
        };
        let cookie = pulseaudio::cookie_path_from_env()
            .and_then(|p| std::fs::read(p).ok())
            .unwrap_or_default();
        let auth: protocol::AuthReply = conn.ask(&Command::Auth(protocol::AuthParams {
            version: protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie,
        }))?;
        conn.version = conn.version.min(auth.version);
        let mut props = protocol::Props::new();
        props.set(protocol::Prop::ApplicationName, cstr("Screen Recorder"));
        let _: protocol::SetClientNameReply = conn.ask(&Command::SetClientName(props))?;
        Ok(conn)
    }

    /// A command and its reply, on a connection with no stream on it yet.
    pub fn ask<R: CommandReply>(&mut self, command: &Command) -> io::Result<R> {
        self.seq += 1;
        protocol::write_command_message(self.sock.get_mut(), self.seq, command, self.version)
            .map_err(err)?;
        protocol::read_reply_message::<R>(&mut self.sock, self.version).map(|(_, r)| r).map_err(err)
    }
}
