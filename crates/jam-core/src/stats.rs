//! UI-facing session snapshot, rebuilt by the control thread every ~500 ms.
//! Shared through a plain mutex: only the control and UI threads touch it,
//! never the audio callback.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    Host,
    Client,
}

#[derive(Debug, Clone, Default)]
pub struct PlayerRow {
    pub id: u16,
    pub name: String,
    pub active: bool,
    pub rms_db: f32,
    pub peak_db: f32,
    /// Fraction of frames concealed (0..1).
    pub loss: f32,
    pub jitter_ms: f32,
    pub buffer_ms: f32,
    pub rtt_ms: f32,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub role: Role,
    pub connected: bool,
    /// One-line status ("hosting room ABC123", "joining...", "denied: ...").
    pub status: String,
    pub room_code: String,
    pub players: Vec<PlayerRow>,
    pub local_rms_db: f32,
    pub local_peak_db: f32,
    pub capture_starved: u64,
    pub xruns: u64,
    /// Best-effort mouth-to-ear estimate, ms (see README for the model).
    pub est_latency_ms: f32,
}

pub type SharedSnapshot = Arc<Mutex<Snapshot>>;

pub fn new_shared(role: Role, status: impl Into<String>) -> SharedSnapshot {
    Arc::new(Mutex::new(Snapshot {
        role,
        status: status.into(),
        ..Default::default()
    }))
}

/// Mouth-to-ear estimate from measured parts. `hw_buffer` is the per-side
/// hardware buffer in samples; the remote side is assumed symmetric. The
/// remote peer's jitter buffer for *our* stream is not observable, so it is
/// approximated with the 2-frame minimum.
pub fn estimate_latency_ms(
    hw_buffer: u32,
    frame_samples: usize,
    sample_rate: u32,
    own_buffer_ms: f32,
    rtt_ms: f32,
) -> f32 {
    let per_sample_ms = 1_000.0 / sample_rate as f32;
    let hw = hw_buffer as f32 * per_sample_ms;
    let frame = frame_samples as f32 * per_sample_ms;
    // capture + frame accumulation + network (rtt/2 each way is already the
    // round trip halved twice: up + down = rtt) + far-end minimum buffer +
    // own jitter buffer + playback.
    hw + frame + rtt_ms + 2.0 * frame + own_buffer_ms + hw
}
