//! The volume, which is the codec's own gain rather than a number in the player.
//!
//! mpv has a `--volume`, and using it was the obvious thing and the wrong one. It is
//! software attenuation with unity at 100, so turning it up as far as it goes does
//! not amplify anything; the loudest this app could ever be was whatever the stack
//! below it happened to be set to. On this device that is the NAU8822's `Speaker`
//! control, and the image leaves it at 40 of 63, which the driver reports as
//! -17.00dB against a ceiling of +6.00dB. Twenty-three decibels of the amplifier
//! were simply never asked for, which is why the radio was quieter than the
//! prototype, whose own volume slider was that control all along.
//!
//! So the volume moves the sink instead. WirePlumber puts the ALSA mixer behind the
//! sink's volume rather than mixing in software, so writing 1.0 here walks `Speaker`
//! to 63 and +6.00dB, measured:
//!
//!     sink 1.00  ->  Speaker 63  100%   +6.00dB
//!     sink 0.40  ->  Speaker 40   63%  -17.00dB
//!     sink 0.20  ->  Speaker 22   35%  -35.00dB
//!
//! and mpv is left at 100, where it attenuates nothing.
//!
//! The sound server is reached over the socket flipctl links into the app's runtime
//! directory, so none of this needs the codec: the unit is `DevicePolicy=closed` and
//! an `amixer` inside it cannot even see the card, which is the other reason this is
//! not the prototype's approach transcribed.
//!
//! Two consequences worth knowing. The volume is the machine's, not this app's, so
//! it stays where it was left, and WirePlumber remembers it across boots. And the
//! row shows what the machine is actually set to when the app opens, rather than a
//! number of its own that would be a guess.

use std::process::Command;

/// The sink flipctl pinned this app to, which is the panel's speaker.
///
/// Set as `PIPEWIRE_NODE`; the fallback is the same name flipctl falls back to, so
/// the app run by hand outside flipctl still finds it.
fn sink_name() -> String {
    std::env::var("PIPEWIRE_NODE")
        .unwrap_or_else(|_| "alsa_output.platform-sound.stereo-fallback".into())
}

/// The sink's node id, which is what wpctl takes: it refuses a name.
///
/// Ids are handed out as things appear and are not the same twice, so this is asked
/// rather than remembered. `pw-cli ls Node` prints an `id N,` line and then the
/// node's properties, so the id wanted is the last one seen before the name matches.
fn node_id() -> Option<u32> {
    let out = Command::new("pw-cli").arg("ls").arg("Node").output().ok()?;
    id_of(&String::from_utf8_lossy(&out.stdout), &sink_name())
}

/// The id of the node called `name` in a `pw-cli ls Node` listing.
///
/// Every node is an `id N,` line and then its properties, one per line, so the id
/// wanted is the last one seen when the name matches. Split out from the command so
/// the shape of that listing is something a test can hold.
fn id_of(listing: &str, name: &str) -> Option<u32> {
    let want = format!("node.name = \"{name}\"");
    let mut id = None;
    for line in listing.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("id ") {
            id = rest.split(',').next().and_then(|n| n.parse().ok());
        } else if line == want {
            return id;
        }
    }
    None
}

/// What the speaker is set to, 0 to 100, or nothing if the sound server is not there.
///
/// `wpctl get-volume` answers `Volume: 0.40`, and a muted sink adds ` [MUTED]`, which
/// is not this row's business: the volume it would return to is the number shown.
pub fn volume() -> Option<i32> {
    let id = node_id()?;
    let out = Command::new("wpctl").arg("get-volume").arg(id.to_string()).output().ok()?;
    let said = String::from_utf8_lossy(&out.stdout);
    let value: f32 = said.split_whitespace().nth(1)?.parse().ok()?;
    Some((value * 100.0).round() as i32)
}

/// Set the speaker to `percent`, 0 to 100.
///
/// Failure is silent on purpose. There is nothing for the panel to say about a sound
/// server that is not answering, and the row has already moved: the alternative is a
/// dialog in front of somebody who pressed a volume key.
pub fn set_volume(percent: i32) {
    let Some(id) = node_id() else { return };
    let level = format!("{:.2}", percent.clamp(0, 100) as f32 / 100.0);
    let _ = Command::new("wpctl")
        .arg("set-volume")
        .arg(id.to_string())
        .arg(level)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim from `pw-cli ls Node` on the device, tabs and all. The listing is a
    /// debug dump rather than an interface, so the fixture is the real thing: a
    /// change in its shape is a thing to find here rather than in a silent volume
    /// row that stops moving.
    const LISTING: &str = "\tid 35, type PipeWire:Interface:Node/3\n \t\tobject.serial = \"35\"\n \t\tnode.description = \"On-board NAU8822 Analog Output\"\n \t\tnode.name = \"alsa_output.platform-sound.stereo-fallback\"\n \t\tmedia.class = \"Audio/Sink\"\n\tid 46, type PipeWire:Interface:Node/3\n \t\tnode.name = \"alsa_input.platform-sound.stereo-fallback\"\n \t\tmedia.class = \"Audio/Source\"\n\tid 50, type PipeWire:Interface:Node/3\n \t\tnode.name = \"alsa_output.platform-hdmi-sound.HiFi__HDMI__sink\"\n \t\tmedia.class = \"Audio/Sink\"\n";

    #[test]
    fn the_sink_is_found_by_name() {
        assert_eq!(id_of(LISTING, "alsa_output.platform-sound.stereo-fallback"), Some(35));
    }

    /// The output and the input differ by three letters and sit next to each other,
    /// so a match on the wrong one would send the volume to the microphone.
    #[test]
    fn the_input_of_the_same_card_is_not_it() {
        assert_eq!(id_of(LISTING, "alsa_input.platform-sound.stereo-fallback"), Some(46));
        assert_eq!(id_of(LISTING, "alsa_output.platform-hdmi-sound.HiFi__HDMI__sink"), Some(50));
    }

    /// A sink that is not there is not the first one that is.
    #[test]
    fn an_absent_name_finds_nothing() {
        assert_eq!(id_of(LISTING, "alsa_output.nothing.like.it"), None);
        assert_eq!(id_of("", "alsa_output.platform-sound.stereo-fallback"), None);
    }
}
