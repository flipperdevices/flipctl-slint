// The decoder: IQ at 20, 40 or 80 Msps -> shift the channel to 0 Hz -> complex half-bands to
// 20 Msps -> FM discriminator -> brightness and colour filters to 10 Msps -> line sync ->
// fields.
//
// Every per-sample loop works on contiguous f32 arrays so the compiler vectorizes it: the
// same decoder in SDR++ ran at a third of the speed while its atan2 and filters did not. Where
// it would not (de-interleaving the raw I,Q), split.rs says so to NEON directly.

pub mod chroma;
pub mod fm;
pub mod halfband;
pub mod linesync;
pub mod shift;
pub mod split;
pub mod video;

/// Samples per second into the discriminator.
pub const IQ_RATE: f64 = 20e6;
