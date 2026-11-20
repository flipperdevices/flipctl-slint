//! The PipeWire graph as `pw-dump` gives it: the outputs and inputs to offer, the
//! sound card the PC sees, and the streams the Flipper's own apps play.

use std::process::{Command, Stdio};

use serde_json::Value;

/// The ALSA card the uac2 gadget makes, as PipeWire names it.
pub const GADGET_CARD: &str = "UAC2_Gadget";

const SPEAKER: &str = "Speaker";
const MIC: &str = "Mic";

/// The sink flipctl pinned the app to, which is the panel's speaker, or the name it
/// falls back to, so the app run by hand finds it too.
pub fn speaker() -> String {
    std::env::var("PULSE_SINK")
        .unwrap_or_else(|_| "alsa_output.platform-sound.stereo-fallback".into())
}

/// Nodes by name and label: `first` called `called` at the head, then HDMI, then
/// DisplayPort, then the rest under their descriptions, as Internet radio has them.
fn named(nodes: Vec<&Node>, first: &str, called: &str) -> Vec<(String, String)> {
    let mut ranked: Vec<(usize, String, String)> = nodes
        .into_iter()
        .map(|n| {
            let lower = n.name.to_ascii_lowercase();
            let (rank, label) = if n.name == first {
                (0, called.to_string())
            } else if lower.contains("hdmi") {
                (1, "HDMI".to_string())
            } else if lower.contains("-dp") || lower.contains("displayport") {
                (2, "DisplayPort".to_string())
            } else {
                (3, n.description.clone())
            };
            (rank, n.name.clone(), label)
        })
        .collect();
    ranked.sort_by_key(|(rank, ..)| *rank);
    ranked.into_iter().map(|(_, name, label)| (name, label)).collect()
}

/// Every node this app makes is named under this, so its own are told apart.
pub const OURS: &str = "usb-audio-bridge";

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    pub id: u64,
    pub name: String,
    pub description: String,
    pub class: String,
    pub card: String,
}

impl Node {
    fn gadget(&self) -> bool {
        self.card == GADGET_CARD
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Port {
    pub id: u64,
    pub node: u64,
    pub output: bool,
    pub channel: String,
    pub monitor: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Graph {
    pub nodes: Vec<Node>,
    pub ports: Vec<Port>,
    /// Output port to input port.
    pub links: Vec<(u64, u64)>,
    pub default_sink: String,
}

fn text(props: &Value, key: &str) -> String {
    props.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

impl Graph {
    /// The graph now, or why there is none.
    pub fn read() -> Result<Graph, String> {
        let out = Command::new("pw-dump")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|_| "no pw-dump".to_string())?;
        if !out.status.success() {
            return Err("PipeWire is not running".into());
        }
        Graph::parse(&String::from_utf8_lossy(&out.stdout)).ok_or("pw-dump said nonsense".into())
    }

    pub fn parse(json: &str) -> Option<Graph> {
        let all: Vec<Value> = serde_json::from_str(json).ok()?;
        let mut g = Graph::default();
        for o in &all {
            let id = o.get("id").and_then(Value::as_u64).unwrap_or(0);
            let kind = o.get("type").and_then(Value::as_str).unwrap_or_default();
            let info = o.get("info").unwrap_or(&Value::Null);
            let props = info.get("props").unwrap_or(&Value::Null);
            match kind.rsplit(':').next().unwrap_or_default() {
                "Node" => {
                    let name = text(props, "node.name");
                    let mut description = text(props, "node.description");
                    if description.is_empty() {
                        description = name.clone();
                    }
                    g.nodes.push(Node {
                        id,
                        name,
                        description,
                        class: text(props, "media.class"),
                        card: text(props, "alsa.card_name"),
                    });
                }
                "Port" => g.ports.push(Port {
                    id,
                    node: props.get("node.id").and_then(Value::as_u64).unwrap_or(0),
                    output: text(props, "port.direction") == "out",
                    channel: text(props, "audio.channel"),
                    monitor: props.get("port.monitor").and_then(Value::as_bool).unwrap_or(false),
                }),
                "Link" => {
                    let port = |k: &str| info.get(k).and_then(Value::as_u64);
                    if let (Some(o), Some(i)) = (port("output-port-id"), port("input-port-id")) {
                        g.links.push((o, i));
                    }
                }
                "Metadata" => {
                    let entries = o.get("metadata").and_then(Value::as_array);
                    for e in entries.into_iter().flatten() {
                        if e.get("key").and_then(Value::as_str) == Some("default.audio.sink") {
                            g.default_sink = text(e.get("value").unwrap_or(&Value::Null), "name");
                        }
                    }
                }
                _ => {}
            }
        }
        Some(g)
    }

    /// The outputs the PC's sound may play on.
    pub fn sinks(&self) -> Vec<&Node> {
        self.devices("Audio/Sink")
    }

    /// The inputs the PC may hear.
    pub fn sources(&self) -> Vec<&Node> {
        self.devices("Audio/Source")
    }

    /// The outputs as Internet radio names and orders them: the sink flipctl pinned
    /// the app to first as Speaker, then HDMI, then DisplayPort, then the rest under
    /// their own descriptions.
    pub fn named_sinks(&self) -> Vec<(String, String)> {
        let speaker = speaker();
        named(self.sinks(), &speaker, SPEAKER)
    }

    /// The inputs the same way, the speaker's own card's input first as Mic.
    pub fn named_sources(&self) -> Vec<(String, String)> {
        let mic = speaker().replacen("alsa_output.", "alsa_input.", 1);
        named(self.sources(), &mic, MIC)
    }

    fn devices(&self, class: &str) -> Vec<&Node> {
        self.nodes.iter().filter(|n| n.class == class && !n.gadget()).collect()
    }

    /// The sound card's side that brings the PC's sound in, or takes sound to it.
    pub fn gadget(&self, class: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.class == class && n.gadget())
    }

    pub fn node_named(&self, name: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.name == name)
    }

    /// What the Flipper's own apps play: every playback stream but this app's.
    fn streams(&self) -> impl Iterator<Item = &Node> {
        self.nodes.iter().filter(|n| n.class == "Stream/Output/Audio" && !n.name.starts_with(OURS))
    }

    /// The links that carry every stream to `into`'s inputs and do not exist yet:
    /// each channel to its namesake, a mono stream to all of them.
    pub fn missing_links(&self, into: u64) -> Vec<(u64, u64)> {
        let inputs: Vec<&Port> =
            self.ports.iter().filter(|p| p.node == into && !p.output).collect();
        let mut want = Vec::new();
        for stream in self.streams() {
            let outs = self.ports.iter().filter(|p| p.node == stream.id && p.output && !p.monitor);
            for out in outs {
                for input in &inputs {
                    let fits = out.channel == input.channel || out.channel == "MONO";
                    if fits && !self.links.contains(&(out.id, input.id)) {
                        want.push((out.id, input.id));
                    }
                }
            }
        }
        want
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUMP: &str = r#"[
      {"id": 44, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "alsa_output.platform-sound.stereo-fallback",
        "node.description": "On-board NAU8822 Analog Output",
        "media.class": "Audio/Sink", "alsa.card_name": "On-board Analog NAU8822"}}},
      {"id": 45, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "alsa_input.platform-sound.stereo-fallback",
        "node.description": "On-board NAU8822 Analog Input", "media.class": "Audio/Source"}}},
      {"id": 70, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "alsa_output.platform-gadget.0.stereo-fallback",
        "node.description": "UAC2_Gadget Analog Stereo",
        "media.class": "Audio/Sink", "alsa.card_name": "UAC2_Gadget"}}},
      {"id": 71, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "alsa_input.platform-gadget.0.stereo-fallback",
        "media.class": "Audio/Source", "alsa.card_name": "UAC2_Gadget"}}},
      {"id": 80, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "Firefox", "media.class": "Stream/Output/Audio"}}},
      {"id": 81, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "usb-audio-bridge.from-pc.out", "media.class": "Stream/Output/Audio"}}},
      {"id": 82, "type": "PipeWire:Interface:Node", "info": {"props": {
        "node.name": "beep", "media.class": "Stream/Output/Audio"}}},
      {"id": 100, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 70, "port.direction": "in", "audio.channel": "FL"}}},
      {"id": 101, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 70, "port.direction": "in", "audio.channel": "FR"}}},
      {"id": 102, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 70, "port.direction": "out", "audio.channel": "FL", "port.monitor": true}}},
      {"id": 110, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 80, "port.direction": "out", "audio.channel": "FL"}}},
      {"id": 111, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 80, "port.direction": "out", "audio.channel": "FR"}}},
      {"id": 112, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 81, "port.direction": "out", "audio.channel": "FL"}}},
      {"id": 113, "type": "PipeWire:Interface:Port", "info": {"props": {
        "node.id": 82, "port.direction": "out", "audio.channel": "MONO"}}},
      {"id": 120, "type": "PipeWire:Interface:Link", "info": {
        "output-port-id": 110, "input-port-id": 100}},
      {"id": 40, "type": "PipeWire:Interface:Metadata", "metadata": [
        {"subject": 0, "key": "default.audio.sink",
         "value": {"name": "alsa_output.platform-sound.stereo-fallback"}}]}
    ]"#;

    #[test]
    fn the_sound_card_is_kept_apart_from_the_devices() {
        let g = Graph::parse(DUMP).unwrap();
        let sinks: Vec<u64> = g.sinks().iter().map(|n| n.id).collect();
        assert_eq!(sinks, [44]);
        assert_eq!(g.sources().len(), 1);
        assert_eq!(g.gadget("Audio/Sink").map(|n| n.id), Some(70));
        assert_eq!(g.gadget("Audio/Source").map(|n| n.id), Some(71));
        assert_eq!(g.default_sink, "alsa_output.platform-sound.stereo-fallback");
    }

    /// Every app's stream goes to the PC, a mono one on both sides, but not the PC's
    /// own sound coming back, and not a link that is there already.
    #[test]
    fn the_flippers_streams_are_linked_but_not_ours() {
        let g = Graph::parse(DUMP).unwrap();
        let mut want = g.missing_links(70);
        want.sort();
        assert_eq!(want, [(111, 101), (113, 100), (113, 101)]);
    }

    /// Named and ordered as Internet radio has them. Run without flipctl, so the
    /// speaker is the name flipctl falls back to.
    #[test]
    fn devices_are_named_as_the_radio_names_them() {
        let node = |name: &str, description: &str| Node {
            id: 0,
            name: name.into(),
            description: description.into(),
            class: "Audio/Sink".into(),
            card: String::new(),
        };
        let nodes = [
            node("bluez_output.AA_BB.1", "Headphones"),
            node("alsa_output.platform-dp0-sound.stereo-fallback", "DisplayPort Digital Output"),
            node("alsa_output.platform-hdmi-sound.stereo-fallback", "HDMI Digital Output"),
            node("alsa_output.platform-sound.stereo-fallback", "On-board NAU8822 Analog Output"),
        ];
        let named =
            named(nodes.iter().collect(), "alsa_output.platform-sound.stereo-fallback", SPEAKER);
        let labels: Vec<&str> = named.iter().map(|(_, l)| l.as_str()).collect();
        assert_eq!(labels, [SPEAKER, "HDMI", "DisplayPort", "Headphones"]);
        let g = Graph::parse(DUMP).unwrap();
        let sources = g.named_sources();
        assert_eq!(sources[0], ("alsa_input.platform-sound.stereo-fallback".into(), MIC.into()));
    }
}
