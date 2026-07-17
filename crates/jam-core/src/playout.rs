//! Per-stream receive chain: jitter buffer -> decoder/PLC -> adaptive depth
//! corrections. Owned by the audio callback; everything here is
//! allocation-free after construction.

use crate::EngineParams;
use jam_audio::codec::{AudioDecoder, CodecError};
use jam_audio::jitter::{Adjust, JitterBuffer, PlayoutController, PopOutcome};
use jam_audio::mixer::{frame_rms, LOW_ENERGY_RMS};
use jam_audio::MAX_FRAME_BYTES;

pub struct StreamPlayout {
    decoder: AudioDecoder,
    controller: PlayoutController,
    pkt_buf: [u8; MAX_FRAME_BYTES],
    /// When set, the next tick plays a concealed frame *without* consuming
    /// from the buffer, growing effective depth by one frame.
    insert_pending: bool,
}

impl StreamPlayout {
    pub fn new(params: &EngineParams) -> Result<Self, CodecError> {
        let mut controller = PlayoutController::new(params.frame_samples, params.sample_rate);
        if let Some(n) = params.jitter_fixed {
            controller.set_fixed(n);
        }
        Ok(Self {
            decoder: AudioDecoder::new(params.codec, params.sample_rate)?,
            controller,
            pkt_buf: [0u8; MAX_FRAME_BYTES],
            insert_pending: false,
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

        let hit = if self.insert_pending {
            self.insert_pending = false;
            // Concealed insert: play PLC, leave the buffer untouched.
            let _ = self.decoder.decode(None, out);
            false
        } else {
            match jb.pop_next(&mut self.pkt_buf) {
                PopOutcome::Frame { len, .. } => {
                    if self
                        .decoder
                        .decode(Some(&self.pkt_buf[..len]), out)
                        .is_err()
                    {
                        // Corrupt payload: conceal rather than glitch.
                        let _ = self.decoder.decode(None, out);
                        false
                    } else {
                        true
                    }
                }
                PopOutcome::Missing => {
                    let _ = self.decoder.decode(None, out);
                    false
                }
            }
        };

        if self.controller.post_pop(hit, jb.stats.jitter_us()) {
            // Prolonged outage: restart the stream position cleanly.
            jb.reset();
            out.fill(0.0);
            return false;
        }
        self.controller.observe_depth(jb.depth());

        // Apply at most one depth correction per frame, only when quiet
        // enough to be masked.
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
        true
    }

    pub fn target_depth(&self) -> i32 {
        self.controller.target()
    }

    /// Forget priming/pending state; used when a slot is reused by a new
    /// client or the client rejoins. (Decoder state intentionally carries
    /// over: recreating it would allocate, and stale CELT state only colors
    /// the first ~2.5 ms of a new stream.)
    pub fn reset_stream(&mut self) {
        self.controller.reset();
        self.insert_pending = false;
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
}
