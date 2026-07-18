//! Per-stream receive chain: jitter buffer -> decoder/PLC -> drift-corrected
//! playout. Owned by the audio callback; everything here is allocation-free
//! after construction.
//!
//! Clock-drift handling (M8): by default a cubic Hermite fractional
//! resampler continuously rate-matches the sender's clock, driven by a PI
//! controller on jitter-buffer depth error. Decoded frames pass through the
//! resampler into a small staging FIFO; each tick pops exactly one hardware
//! frame from it. The coarse drop/insert corrections remain as a fallback
//! for step changes (network path change), gated behind a widened deadband
//! so they never fight the resampler. `--no-drift` falls back to pure
//! drop/insert.

use crate::EngineParams;
use jam_audio::accum::SampleFifo;
use jam_audio::codec::{AudioDecoder, CodecError};
use jam_audio::jitter::{Adjust, JitterBuffer, PlayoutController, PopOutcome};
use jam_audio::mixer::{frame_rms, LOW_ENERGY_RMS};
use jam_audio::resample::{DriftController, HermiteResampler};
use jam_audio::{MAX_FRAME_BYTES, MAX_FRAME_SAMPLES};

/// Headroom over the nominal frame for resampler output (ratio <= 1.02).
const RESAMPLE_SLACK: usize = 8;

pub struct StreamPlayout {
    decoder: AudioDecoder,
    controller: PlayoutController,
    pkt_buf: [u8; MAX_FRAME_BYTES],
    /// When set, the next pull plays a concealed frame *without* consuming
    /// from the buffer, growing effective depth by one frame.
    insert_pending: bool,
    frame: usize,
    // Continuous rate matching (enabled unless --no-drift).
    drift_enabled: bool,
    resampler: HermiteResampler,
    drift: DriftController,
    staging: SampleFifo,
    decode_buf: [f32; MAX_FRAME_SAMPLES],
    resample_buf: [f32; MAX_FRAME_SAMPLES + RESAMPLE_SLACK],
}

impl StreamPlayout {
    pub fn new(params: &EngineParams) -> Result<Self, CodecError> {
        let mut controller = PlayoutController::new(params.frame_samples, params.sample_rate);
        if let Some(n) = params.jitter_fixed {
            controller.set_fixed(n);
        }
        if params.drift_correction {
            // The resampler absorbs fine drift; step corrections only for
            // genuine jumps.
            controller.set_deadband(3);
        }
        Ok(Self {
            decoder: AudioDecoder::new(params.codec, params.sample_rate)?,
            controller,
            pkt_buf: [0u8; MAX_FRAME_BYTES],
            insert_pending: false,
            frame: params.frame_samples,
            drift_enabled: params.drift_correction,
            resampler: HermiteResampler::new(),
            drift: DriftController::new(),
            staging: SampleFifo::new(params.frame_samples * 4 + 2 * RESAMPLE_SLACK),
            decode_buf: [0.0; MAX_FRAME_SAMPLES],
            resample_buf: [0.0; MAX_FRAME_SAMPLES + RESAMPLE_SLACK],
        })
    }

    /// Produces one frame of PCM for this stream into `out`.
    /// Returns true if the stream is live (primed and producing audio or
    /// concealment), false when `out` was filled with silence.
    pub fn tick(&mut self, jb: &JitterBuffer, out: &mut [f32]) -> bool {
        let depth = jb.depth();
        if !self.controller.pre_pop(depth, jb.is_started()) {
            out.fill(0.0);
            return false;
        }

        if self.drift_enabled {
            self.tick_resampled(jb, out)
        } else {
            self.tick_stepped(jb, out)
        }
    }

    /// Original path: one packet per tick, drop/insert for drift.
    fn tick_stepped(&mut self, jb: &JitterBuffer, out: &mut [f32]) -> bool {
        let hit = self.pull_decoded_into(jb, 0);
        out.copy_from_slice(&self.decode_buf[..self.frame]);
        let Some(hit) = hit else {
            // Prolonged outage: pull_decoded_into already reset the stream.
            out.fill(0.0);
            return false;
        };
        let _ = hit;
        self.apply_step_adjustment(jb, out);
        true
    }

    /// M8 path: decoded frames flow through the fractional resampler into
    /// the staging FIFO; exactly one frame leaves per tick.
    fn tick_resampled(&mut self, jb: &JitterBuffer, out: &mut [f32]) -> bool {
        // One controller update per tick: positive depth error means the
        // sender runs fast relative to us, so consume faster (ratio < 1).
        let err = (jb.depth() - self.controller.target()) as f64;
        let ratio = self.drift.update(err);
        self.resampler.set_ratio(ratio);

        while self.staging.len() < self.frame {
            if self.pull_decoded_into(jb, 0).is_none() {
                // Outage reset: flush local state and go silent.
                self.reset_stream();
                out.fill(0.0);
                return false;
            }
            let n = self
                .resampler
                .process(&self.decode_buf[..self.frame], &mut self.resample_buf);
            self.staging.push(&self.resample_buf[..n]);
        }
        let popped = self.staging.pop(out);
        debug_assert!(popped);
        self.apply_step_adjustment(jb, out);
        true
    }

    /// Pulls one packet (or concealment) from the buffer and decodes it into
    /// `decode_buf[offset..offset+frame]`. Returns `Some(hit)` normally, or
    /// `None` when the controller declared a prolonged outage (the jitter
    /// buffer has been reset; the caller must go silent and re-prime).
    fn pull_decoded_into(&mut self, jb: &JitterBuffer, offset: usize) -> Option<bool> {
        let dst = &mut self.decode_buf[offset..offset + self.frame];
        let hit = if self.insert_pending {
            self.insert_pending = false;
            let _ = self.decoder.decode(None, dst);
            false
        } else {
            match jb.pop_next(&mut self.pkt_buf) {
                PopOutcome::Frame { len, .. } => {
                    if self
                        .decoder
                        .decode(Some(&self.pkt_buf[..len]), dst)
                        .is_err()
                    {
                        let _ = self.decoder.decode(None, dst);
                        false
                    } else {
                        true
                    }
                }
                PopOutcome::Missing => {
                    let _ = self.decoder.decode(None, dst);
                    false
                }
            }
        };
        if self.controller.post_pop(hit, jb.stats.jitter_us()) {
            jb.reset();
            return None;
        }
        self.controller.observe_depth(jb.depth());
        Some(hit)
    }

    /// Applies at most one coarse depth correction, only at quiet moments.
    fn apply_step_adjustment(&mut self, jb: &JitterBuffer, out: &[f32]) {
        if frame_rms(out) < LOW_ENERGY_RMS {
            match self.controller.take_adjustment() {
                Adjust::None => {}
                Adjust::DropOne => {
                    jb.skip_next();
                }
                Adjust::InsertOne => {
                    self.insert_pending = true;
                }
            }
        }
    }

    pub fn target_depth(&self) -> i32 {
        self.controller.target()
    }

    /// The resampler's current correction in parts per million (0 when drift
    /// correction is disabled). Negative = consuming faster than nominal.
    pub fn drift_ppm(&self) -> f64 {
        if self.drift_enabled {
            self.drift.current_ppm()
        } else {
            0.0
        }
    }

    /// Forget priming/pending state; used when a slot is reused by a new
    /// client or the client rejoins. (Decoder state intentionally carries
    /// over: recreating it would allocate, and stale CELT state only colors
    /// the first ~2.5 ms of a new stream.)
    pub fn reset_stream(&mut self) {
        self.controller.reset();
        self.insert_pending = false;
        // Drain staged samples and restart the resampler cleanly; the drift
        // estimate (integral) is kept — the clock offset it learned is a
        // physical property that survives dropouts.
        let mut sink = [0.0f32; MAX_FRAME_SAMPLES];
        while !self.staging.is_empty() {
            let take = self.staging.len().min(MAX_FRAME_SAMPLES);
            let _ = self.staging.pop(&mut sink[..take]);
        }
        self.resampler = HermiteResampler::new();
        let ppm = self.drift.current_ppm();
        self.resampler.set_ratio(1.0 + ppm * 1e-6);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jam_audio::codec::AudioEncoder;
    use jam_protocol::packet::Codec;

    fn params() -> EngineParams {
        EngineParams {
            codec: Codec::PcmF32,
            drift_correction: false,
            ..Default::default()
        }
    }

    fn drift_params() -> EngineParams {
        EngineParams {
            codec: Codec::PcmF32,
            drift_correction: true,
            ..Default::default()
        }
    }

    fn frame_bytes(value: f32, n: usize) -> Vec<u8> {
        let pcm = vec![value; n];
        let mut enc = AudioEncoder::new(Codec::PcmF32, 48_000, 0).unwrap();
        let mut out = vec![0u8; n * 4];
        let len = enc.encode(&pcm, &mut out).unwrap();
        out.truncate(len);
        out
    }

    #[test]
    fn silent_until_primed_then_plays() {
        let p = params();
        let jb = JitterBuffer::new();
        let mut sp = StreamPlayout::new(&p).unwrap();
        let mut out = vec![0.0f32; p.frame_samples];

        assert!(!sp.tick(&jb, &mut out)); // nothing arrived yet

        jb.push(0, 0, &frame_bytes(0.25, p.frame_samples));
        assert!(!sp.tick(&jb, &mut out)); // depth 1 < target 2, still priming
        jb.push(1, 120, &frame_bytes(0.25, p.frame_samples));
        assert!(sp.tick(&jb, &mut out));
        assert!(out.iter().all(|&x| (x - 0.25).abs() < 1e-6));
    }

    #[test]
    fn loss_is_concealed_and_stream_continues() {
        let p = params();
        let jb = JitterBuffer::new();
        let mut sp = StreamPlayout::new(&p).unwrap();
        let mut out = vec![0.0f32; p.frame_samples];
        jb.push(0, 0, &frame_bytes(0.5, p.frame_samples));
        jb.push(1, 120, &frame_bytes(0.5, p.frame_samples));
        jb.push(3, 360, &frame_bytes(0.5, p.frame_samples)); // 2 lost
        assert!(sp.tick(&jb, &mut out)); // 0
        assert!(sp.tick(&jb, &mut out)); // 1
        sp.tick(&jb, &mut out); // 2 concealed (PCM PLC = silence)
        assert!(out.iter().all(|&x| x == 0.0));
        assert!(sp.tick(&jb, &mut out)); // 3 plays
        assert!(out.iter().all(|&x| (x - 0.5).abs() < 1e-6));
    }

    #[test]
    fn prolonged_outage_resets_and_reprimes() {
        let p = params();
        let jb = JitterBuffer::new();
        let mut sp = StreamPlayout::new(&p).unwrap();
        let mut out = vec![0.0f32; p.frame_samples];
        jb.push(0, 0, &frame_bytes(0.5, p.frame_samples));
        jb.push(1, 120, &frame_bytes(0.5, p.frame_samples));
        assert!(sp.tick(&jb, &mut out));
        // Feed nothing for > 0.5 s worth of frames: stream resets.
        let mut reset_seen = false;
        for _ in 0..400 {
            if !sp.tick(&jb, &mut out) && !jb.is_started() {
                reset_seen = true;
                break;
            }
        }
        assert!(reset_seen);
        // New stream far away in seq space is accepted after reset.
        jb.push(30_000, 0, &frame_bytes(0.7, p.frame_samples));
        jb.push(30_001, 120, &frame_bytes(0.7, p.frame_samples));
        assert!(sp.tick(&jb, &mut out));
        assert!(out.iter().all(|&x| (x - 0.7).abs() < 1e-6));
    }

    /// Drives a StreamPlayout against a sender whose clock runs at
    /// `(1 + ppm*1e-6)` times the receiver's, for `ticks` receiver frames.
    /// Returns (concealed in the second half, resets, final drift ppm,
    /// max depth seen in the second half).
    fn soak(sender_ppm: f64, ticks: u64) -> (u64, u64, f64, i32) {
        let p = drift_params();
        let n = p.frame_samples;
        let jb = JitterBuffer::new();
        let mut sp = StreamPlayout::new(&p).unwrap();
        let mut out = vec![0.0f32; n];
        let payload = frame_bytes(0.3, n);

        // Sender emits one packet every `n / (1+ppm)` receiver-samples.
        let sender_period = n as f64 / (1.0 + sender_ppm * 1e-6);
        let mut next_send = 0.0f64;
        let mut seq: u16 = 0;
        let mut concealed_half = 0u64;
        let mut max_depth_half = 0i32;

        for i in 0..ticks {
            let now = i as f64 * n as f64;
            while next_send <= now {
                jb.push(seq, (seq as u32).wrapping_mul(n as u32), &payload);
                seq = seq.wrapping_add(1);
                next_send += sender_period;
            }
            let before = jb
                .stats
                .concealed
                .load(std::sync::atomic::Ordering::Relaxed);
            sp.tick(&jb, &mut out);
            let after = jb
                .stats
                .concealed
                .load(std::sync::atomic::Ordering::Relaxed);
            if i >= ticks / 2 {
                concealed_half += after - before;
                max_depth_half = max_depth_half.max(jb.depth());
            }
        }
        let resets = jb.stats.resets.load(std::sync::atomic::Ordering::Relaxed);
        (concealed_half, resets, sp.drift_ppm(), max_depth_half)
    }

    /// M8 acceptance: a sender 200 ppm fast must converge with no buffer
    /// resets and no ongoing concealment once the controller locks.
    #[test]
    fn soak_fast_sender_converges_without_resets() {
        // 120k frames = 5 simulated minutes.
        let (concealed, resets, ppm, max_depth) = soak(200.0, 120_000);
        assert_eq!(resets, 0, "no jitter-buffer resets");
        assert_eq!(concealed, 0, "no concealment in steady state");
        assert!(
            (ppm + 200.0).abs() < 40.0,
            "controller near -200 ppm (got {ppm:.1})"
        );
        assert!(max_depth <= 8, "depth stays bounded (got {max_depth})");
    }

    /// Slow sender: the buffer drains instead of filling; the resampler must
    /// stretch (ratio > 1) and hold the stream gap-free.
    #[test]
    fn soak_slow_sender_converges_without_underruns() {
        let (concealed, resets, ppm, max_depth) = soak(-200.0, 120_000);
        assert_eq!(resets, 0, "no jitter-buffer resets");
        assert_eq!(concealed, 0, "no concealment in steady state");
        assert!(
            (ppm - 200.0).abs() < 40.0,
            "controller near +200 ppm (got {ppm:.1})"
        );
        assert!(max_depth <= 8, "depth stays bounded (got {max_depth})");
    }

    /// Zero offset: the drift path must be audibly transparent.
    #[test]
    fn drift_path_passes_signal_through_at_zero_offset() {
        let (concealed, resets, ppm, _) = soak(0.0, 20_000);
        assert_eq!(resets, 0);
        assert_eq!(concealed, 0);
        assert!(ppm.abs() < 20.0, "near-zero correction (got {ppm:.1})");
    }
}
