// The analog FPV channel plan: bands A, B, E, F, R (Raceband) and L, eight channels each, MHz.

pub struct Channel {
    pub band: char,
    pub number: u8,
    pub mhz: u32,
}

const BANDS: [(char, [u32; 8]); 6] = [
    ('A', [5865, 5845, 5825, 5805, 5785, 5765, 5745, 5725]),
    ('B', [5733, 5752, 5771, 5790, 5809, 5828, 5847, 5866]),
    ('E', [5705, 5685, 5665, 5645, 5885, 5905, 5925, 5945]),
    ('F', [5740, 5760, 5780, 5800, 5820, 5840, 5860, 5880]),
    ('R', [5658, 5695, 5732, 5769, 5806, 5843, 5880, 5917]),
    ('L', [5362, 5399, 5436, 5473, 5510, 5547, 5584, 5621]),
];

/// Every channel, in band order.
pub fn all() -> Vec<Channel> {
    BANDS
        .iter()
        .flat_map(|(band, mhz)| mhz.iter().enumerate().map(move |(i, &m)| Channel { band: *band, number: i as u8 + 1, mhz: m }))
        .collect()
}

impl Channel {
    pub fn name(&self) -> String {
        format!("{}{}", self.band, self.number)
    }

    pub fn hz(&self) -> f64 {
        self.mhz as f64 * 1e6
    }
}
