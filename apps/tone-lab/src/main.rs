//! A tone curve chosen by looking, on the panel the choice is for.
//!
//! The panel is black ink on an orange ground, and nothing on a desk resembles
//! it. So every judgement about Doom's tone had to be made by baking a
//! palette into a wad, building a bundle, installing it and launching the engine:
//! one guess per round trip, with the previous guess already forgotten by the time
//! the next arrived.
//!
//! This holds the picture still and puts the curve under the D-pad instead. The
//! frames are real engine output, captured at panel size with the engine's own
//! video dump, so what is corrected here is exactly what the engine sends. When a
//! setting looks right the numbers are on screen to be read off and baked into a
//! palette wad once.
//!
//! There are two halves to it. The channel weights decide which colour becomes which
//! grey, before any curve exists: the panel is greyscale, so every colour in the game
//! is already being flattened to one number, and the weights are what that flattening
//! is. Doom's red and its brown sit at nearly the same Rec.601 luma, and no tone curve
//! can pull them apart afterwards, because by then they are the same grey. The curve
//! is the second half, and works on the number the weights produced.
//!
//! The curve is a 256-entry lookup, which is also the shape a Doom palette can
//! express: one output grey per input grey, no spatial processing. Anything that
//! looks good here is therefore reproducible in a wad, which a fancier correction
//! would not be.
//!
//! So Save writes the wad rather than writing the numbers down: the engine reads a
//! palette and this already has the exact table, where a settings file would leave
//! the curve to be implemented a second time on the device, in a language with no
//! reason to agree with this one about rounding.

slint::include_modules!();
use flipctl_app::Key;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

const W: usize = 256;
const H: usize = 144;
const PIXELS: usize = W * H;

/// Rec.601, which is what the panel's own conversion uses and what the weights below
/// are a percentage of. At 100 each, this app produces exactly what the device does.
const REC601: [f32; 3] = [0.299, 0.587, 0.114];

/// Engine output, captured headless at panel size: four level entrances, chosen for
/// their range rather than their scenery. Map 1 is the dark one and map 5 the
/// blown-out one, which is the pair a curve has to satisfy at once.
///
/// Colour, and captured without an encoder in the way: the engine's video dump pipes
/// raw rgb24 into a command, so the command was `cat`. An H.264 intermediate would
/// have been fine for judging tone and useless for judging channel weights, since its
/// chroma is stored at half resolution and colours bleed into their neighbours.
const CAPTURES: [&[u8]; 4] = [
    include_bytes!("../frames/1.rgb"),
    include_bytes!("../frames/2.rgb"),
    include_bytes!("../frames/3.rgb"),
    include_bytes!("../frames/4.rgb"),
];

/// The readout's band, kept clear of the picture and drawn after the correction so it
/// stays legible whatever the curve does to everything else. Two lines: seven numbers
/// do not fit across 256 pixels in one.
const BAND_H: usize = 27;
const LINE_H: usize = 13;

/// The engine's own palette, all fourteen pages: the base colours and the damage
/// and pickup tints. A page left uncorrected would flash at a different brightness
/// from the game under it, so Save maps every one of them.
const PLAYPAL: &[u8] = include_bytes!("../playpal.bin");
const PAGES: usize = 14;
const PAGE_BYTES: usize = 256 * 3;

/// Where Doom looks before falling back to the palette in its own bundle. Not
/// inside either bundle: an AppImage is read-only, and the two apps are separate
/// bundles with separate working directories, so a written palette has to live
/// where both can name it.
fn saved_palette() -> std::path::PathBuf {
    let home = std::env::var_os("HOME").unwrap_or_else(|| "/home/user".into());
    std::path::Path::new(&home).join(".local/share/flipctl/doom")
}

/// One adjustable number.
struct Knob {
    name: &'static str,
    value: f32,
    default: f32,
    lo: f32,
    hi: f32,
    step: f32,
    /// Shown with two decimals rather than as a whole number.
    fine: bool,
}

impl Knob {
    fn nudge(&mut self, up: bool) {
        let step = if up { self.step } else { -self.step };
        // Rounded to the step, because repeated addition of 0.05 drifts and the
        // reading is meant to be copied into a palette generator by hand.
        let stepped = (self.value + step) / self.step;
        self.value = (stepped.round() * self.step).clamp(self.lo, self.hi);
    }

    fn show(&self) -> String {
        if self.fine {
            format!("{}{:.2}", self.name, self.value)
        } else {
            format!("{}{}", self.name, self.value.round() as i32)
        }
    }
}

struct Lab {
    knobs: [Knob; 7],
    chosen: usize,
    frame: usize,
    invert: bool,
    /// Show the picture as the engine sent it, for comparing against the correction
    /// without having to remember what it looked like.
    bypass: bool,
    chrome: bool,
    /// What Save last said, shown until the next key so a press is acknowledged
    /// without a timer to expire it.
    note: Option<String>,
}

impl Lab {
    fn new() -> Self {
        Self {
            knobs: [
                // The defaults are the curve chosen on the panel, so the app opens on
                // what Doom is already shipping and a session starts by comparing
                // against it rather than by rebuilding it.
                Knob { name: "Ga", value: 0.5, default: 0.5, lo: 0.2, hi: 3.0, step: 0.05, fine: true },
                Knob { name: "Fl", value: 0.0, default: 0.0, lo: 0.0, hi: 255.0, step: 4.0, fine: false },
                Knob { name: "Ce", value: 255.0, default: 255.0, lo: 0.0, hi: 255.0, step: 4.0, fine: false },
                Knob { name: "Sc", value: 1.0, default: 1.0, lo: -1.0, hi: 1.0, step: 0.05, fine: true },
                // Per cent of the panel's own weight for that channel, so all three at
                // 100 is Rec.601 exactly and the defaults reproduce the device rather
                // than approximating it.
                Knob { name: "R", value: 100.0, default: 100.0, lo: 0.0, hi: 300.0, step: 10.0, fine: false },
                Knob { name: "G", value: 100.0, default: 100.0, lo: 0.0, hi: 300.0, step: 10.0, fine: false },
                Knob { name: "B", value: 100.0, default: 100.0, lo: 0.0, hi: 300.0, step: 10.0, fine: false },
            ],
            chosen: 0,
            frame: 0,
            invert: false,
            bypass: false,
            chrome: true,
            note: None,
        }
    }

    fn reset(&mut self) {
        for knob in &mut self.knobs {
            knob.value = knob.default;
        }
        self.invert = false;
    }

    /// The whole correction, as the 256 greys it maps.
    fn lut(&self) -> [u8; 256] {
        let (gamma, floor, ceiling, shape) =
            (self.knobs[0].value, self.knobs[1].value, self.knobs[2].value, self.knobs[3].value);
        let mut lut = [0u8; 256];
        for (i, out) in lut.iter_mut().enumerate() {
            let mut v = i as f32 / 255.0;
            v = v.powf(gamma);
            v = curve(v, shape);
            let mut grey = floor + v * (ceiling - floor);
            if self.invert {
                grey = 255.0 - grey;
            }
            *out = grey.clamp(0.0, 255.0).round() as u8;
        }
        lut
    }

    /// The grey a colour becomes, under the current channel weights.
    ///
    /// Normalised by their sum, so turning one channel down darkens what that colour
    /// contributes rather than darkening the whole picture, and the knobs can be
    /// judged against each other instead of against the brightness they leave behind.
    fn luma(&self, r: u8, g: u8, b: u8) -> u8 {
        let w = [
            REC601[0] * self.knobs[4].value / 100.0,
            REC601[1] * self.knobs[5].value / 100.0,
            REC601[2] * self.knobs[6].value / 100.0,
        ];
        let total = w[0] + w[1] + w[2];
        if total <= 0.0 {
            return 0;
        }
        let y = (w[0] * f32::from(r) + w[1] * f32::from(g) + w[2] * f32::from(b)) / total;
        y.clamp(0.0, 255.0).round() as u8
    }

    fn shown(&self, i: usize) -> String {
        let text = self.knobs[i].show();
        if i == self.chosen {
            format!("[{text}]")
        } else {
            format!(" {text} ")
        }
    }

    /// Two lines: the tone knobs, then the channel weights and what is being looked
    /// at. Seven numbers do not fit across 256 pixels in one.
    fn readout(&self) -> (String, String) {
        let tone: String = (0..4).map(|i| self.shown(i)).collect();
        let source = if self.frame < CAPTURES.len() {
            format!("{}/{}", self.frame + 1, CAPTURES.len())
        } else {
            "ramp".into()
        };
        let weights: String = (4..7).map(|i| self.shown(i)).collect();
        let mut second = format!("{weights} {source}");
        if self.invert {
            second.push_str(" inv");
        }
        if self.bypass {
            second.push_str(" RAW");
        }
        if let Some(note) = &self.note {
            second.push(' ');
            second.push_str(note);
        }
        (tone, second)
    }

    /// Write the current curve where Doom will find it, as the palette itself.
    fn save(&self) -> std::io::Result<std::path::PathBuf> {
        let lut = self.lut();
        let mut data = Vec::with_capacity(PAGES * PAGE_BYTES);
        for entry in PLAYPAL.chunks_exact(3) {
            // The same two steps as the preview, in the same order: weights decide
            // which grey the colour is, the curve decides what that grey becomes. A
            // palette can carry both, because both end in one grey per entry.
            let grey = lut[usize::from(self.luma(entry[0], entry[1], entry[2]))];
            data.extend_from_slice(&[grey; 3]);
        }

        let dir = saved_palette();
        std::fs::create_dir_all(&dir)?;
        let wad = dir.join("palette.wad");
        // A PWAD of one lump: the header, the palette, then the directory naming it.
        let mut out = Vec::with_capacity(28 + data.len());
        out.extend_from_slice(b"PWAD");
        out.extend_from_slice(&1i32.to_le_bytes());
        out.extend_from_slice(&((12 + data.len()) as i32).to_le_bytes());
        out.extend_from_slice(&data);
        out.extend_from_slice(&12i32.to_le_bytes());
        out.extend_from_slice(&(data.len() as i32).to_le_bytes());
        out.extend_from_slice(b"PLAYPAL\0");
        // Written beside the target and renamed, so a Doom launched mid-save reads
        // one palette or the other rather than half of each.
        let staging = dir.join("palette.wad.new");
        std::fs::write(&staging, &out)?;
        std::fs::rename(&staging, &wad)?;

        // The numbers as well, because the wad is not readable by eye and the next
        // session starts from what this one decided.
        let (tone, weights) = self.readout();
        std::fs::write(dir.join("curve.txt"), format!("{tone}\n{weights}\n"))?;
        Ok(wad)
    }
}

/// An S-curve either way: positive steepens the midtones, negative flattens them.
///
/// Smoothstep is the steepening half and its exact inverse is the flattening half,
/// so the two directions undo each other and a knob walked up and back lands where
/// it started.
fn curve(v: f32, amount: f32) -> f32 {
    if amount == 0.0 {
        return v;
    }
    let target = if amount > 0.0 {
        v * v * (3.0 - 2.0 * v)
    } else {
        0.5 - (((1.0 - 2.0 * v).clamp(-1.0, 1.0)).asin() / 3.0).sin()
    };
    v + (target - v) * amount.abs()
}

/// A calibration card rather than a scene: the top half a smooth sweep, which shows
/// banding and where the curve clips, and the bottom sixteen flat patches, which
/// show whether neighbouring greys are still separable once the panel has them.
fn ramp() -> Vec<u8> {
    let mut out = vec![0u8; PIXELS * 3];
    for y in 0..H {
        for x in 0..W {
            let v = if y < H / 2 { x as u8 } else { (x * 16 / W * 17) as u8 };
            // Neutral, so the card shows what the curve does and never what the
            // channel weights do: a grey is the same grey under any weighting.
            out[(y * W + x) * 3..(y * W + x) * 3 + 3].fill(v);
        }
    }
    out
}

fn draw(ui: &AppWindow, lab: &Lab, ramp: &[u8]) {
    let source: &[u8] = CAPTURES.get(lab.frame).copied().unwrap_or(ramp);
    let lut = lab.lut();

    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(W as u32, H as u32);
    for (px, rgb) in buffer.make_mut_slice().iter_mut().zip(source.chunks_exact(3)) {
        let grey = lab.luma(rgb[0], rgb[1], rgb[2]);
        let grey = if lab.bypass { grey } else { lut[usize::from(grey)] };
        *px = slint::Rgb8Pixel { r: grey, g: grey, b: grey };
    }
    if lab.chrome {
        // The readout sits on a solid band rather than on the picture: over a dark
        // frame black text is invisible, and over a bright one it is worse.
        for px in buffer.make_mut_slice().iter_mut().take(BAND_H * W) {
            *px = slint::Rgb8Pixel { r: 255, g: 255, b: 255 };
        }
    }
    ui.set_picture(slint::Image::from_rgb8(buffer));

    let texts: Vec<CanvasText> = if lab.chrome {
        let (tone, weights) = lab.readout();
        let line = |y: usize, body: String| CanvasText {
            x: 128.0,
            y: y as f32,
            text: body.into(),
            font: 1,
            align: 0,
            white: false,
        };
        vec![line(0, tone), line(LINE_H, weights)]
    } else {
        Vec::new()
    };
    ui.set_texts(ModelRc::new(VecModel::from(texts)));

    let buttons: Vec<SharedString> = if lab.chrome {
        // Slot 2 is the power button. Flip is on Ptt rather than on the bar: the
        // bar has four usable slots and Save has to be one of them.
        ["Reset", "Hide", "", "Frame", "Save"].iter().map(|&s| s.into()).collect()
    } else {
        Vec::new()
    };
    ui.set_buttons(ModelRc::new(VecModel::from(buttons)));
}

fn main() -> Result<(), slint::PlatformError> {
    let ui = AppWindow::new()?;
    let ramp = ramp();
    let lab = std::rc::Rc::new(std::cell::RefCell::new(Lab::new()));
    draw(&ui, &lab.borrow(), &ramp);

    let keys = ui.as_weak();
    let pressed = lab.clone();
    ui.on_keyed(move |text, down| {
        let Some(ui) = keys.upgrade() else {
            return;
        };
        let Some(key) = Key::from_slint(text.as_str()) else {
            return;
        };
        ui.set_pressed_slot(match (down, key.soft_slot()) {
            (true, Some(slot)) => slot as i32,
            _ => -1,
        });
        if !down {
            return;
        }
        let mut lab = pressed.borrow_mut();
        match key {
            // Back, which flipctl keeps for itself when it hosts the app: hosted, this
            // arm never runs and leaving is flipctl's.
            Key::Back => {
                let _ = slint::quit_event_loop();
                return;
            }
            Key::Left => lab.chosen = (lab.chosen + lab.knobs.len() - 1) % lab.knobs.len(),
            Key::Right => lab.chosen = (lab.chosen + 1) % lab.knobs.len(),
            Key::Up => {
                let chosen = lab.chosen;
                lab.knobs[chosen].nudge(true);
            }
            Key::Down => {
                let chosen = lab.chosen;
                lab.knobs[chosen].nudge(false);
            }
            // Held rather than toggled would be better for a comparison, but a key
            // release is the only other event there is and it is already spoken for
            // by the soft bar's flash.
            Key::Ok => lab.bypass = !lab.bypass,
            Key::Escape => lab.reset(),
            Key::View => lab.chrome = !lab.chrome,
            // The ramp is one past the captures.
            Key::Edit => lab.frame = (lab.frame + 1) % (CAPTURES.len() + 1),
            Key::Ptt => lab.invert = !lab.invert,
            Key::Run => {
                let note = match lab.save() {
                    Ok(path) => {
                        eprintln!("tone lab: wrote {}", path.display());
                        "saved".to_string()
                    }
                    Err(e) => {
                        eprintln!("tone lab: cannot save: {e}");
                        "save failed".to_string()
                    }
                };
                lab.note = Some(note);
                draw(&ui, &lab, &ramp);
                return;
            }
            // Slot 2 is the power button and is never given an action, here or
            // anywhere.
            _ => return,
        }
        // The acknowledgement belongs to the press that earned it and to no other.
        lab.note = None;
        // Also to the journal, so the numbers behind a verdict survive the app being
        // closed and can be read off the device later.
        let (tone, weights) = lab.readout();
        eprintln!("tone lab: {tone} {weights}");
        draw(&ui, &lab, &ramp);
    });

    ui.run()
}
