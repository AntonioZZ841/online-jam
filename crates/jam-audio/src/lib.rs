//! Real-time audio building blocks.
//!
//! Everything intended to run on the audio callback thread is allocation-free
//! and lock-free after construction. Modules that talk to hardware (cpal)
//! live in [`device`]; everything else is pure DSP/data-structure code that
//! is fully testable off-line.

pub mod accum;
pub mod codec;
pub mod device;
pub mod jitter;
pub mod meter;
pub mod mixer;
pub mod resample;

/// Fixed session sample rate for the MVP. Devices that cannot open a 48 kHz
/// stream are rejected with a clear error (see README for rationale).
pub const SAMPLE_RATE: u32 = 48_000;
/// Default network/codec frame: 120 samples = 2.5 ms at 48 kHz.
pub const DEFAULT_FRAME_SAMPLES: usize = 120;
/// The largest frame we support on the wire: 5 ms at 48 kHz.
pub const MAX_FRAME_SAMPLES: usize = 240;
/// Upper bound for one encoded frame's payload on the wire (PCM f32 at 5 ms
/// is 960 bytes; Opus frames are far smaller).
pub const MAX_FRAME_BYTES: usize = 1024;
/// Default hardware buffer request, in samples.
pub const DEFAULT_HW_BUFFER: u32 = 128;
