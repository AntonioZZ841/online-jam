//! Codec wrappers with a real-time-safe call surface: construction allocates
//! (libopus state), but encode/decode afterwards do not.
//!
//! Opus runs in `LowDelay` (RESTRICTED_LOWDELAY) mode — CELT-only, 2.5 ms
//! lookahead. In-band FEC is a SILK feature and does not exist in this mode;
//! loss concealment is PLC (decode with an empty packet) plus optional
//! application-level redundancy handled by the session layer.

use crate::MAX_FRAME_SAMPLES;
use jam_protocol::packet::Codec;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("opus: {0}")]
    Opus(#[from] opus::Error),
    #[error("output buffer too small")]
    OutputTooSmall,
    #[error("invalid payload length for PCM codec")]
    BadPcmLength,
}

pub struct AudioEncoder {
    codec: Codec,
    opus: Option<opus::Encoder>,
}

impl AudioEncoder {
    pub fn new(codec: Codec, sample_rate: u32, bitrate_bps: i32) -> Result<Self, CodecError> {
        let opus = match codec {
            Codec::Opus => {
                let mut enc = opus::Encoder::new(
                    sample_rate,
                    opus::Channels::Mono,
                    opus::Application::LowDelay,
                )?;
                enc.set_bitrate(opus::Bitrate::Bits(bitrate_bps))?;
                Some(enc)
            }
            Codec::PcmS16 | Codec::PcmF32 => None,
        };
        Ok(Self { codec, opus })
    }

    /// Encodes one frame of mono f32 PCM. Returns bytes written into `out`.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, CodecError> {
        debug_assert!(pcm.len() <= MAX_FRAME_SAMPLES);
        match self.codec {
            Codec::Opus => {
                let enc = self.opus.as_mut().expect("opus encoder present");
                Ok(enc.encode_float(pcm, out)?)
            }
            Codec::PcmS16 => {
                let need = pcm.len() * 2;
                if out.len() < need {
                    return Err(CodecError::OutputTooSmall);
                }
                for (chunk, &x) in out.chunks_exact_mut(2).zip(pcm) {
                    let v = (x.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                    chunk.copy_from_slice(&v.to_le_bytes());
                }
                Ok(need)
            }
            Codec::PcmF32 => {
                let need = pcm.len() * 4;
                if out.len() < need {
                    return Err(CodecError::OutputTooSmall);
                }
                for (chunk, &x) in out.chunks_exact_mut(4).zip(pcm) {
                    chunk.copy_from_slice(&x.to_le_bytes());
                }
                Ok(need)
            }
        }
    }
}

pub struct AudioDecoder {
    codec: Codec,
    opus: Option<opus::Decoder>,
}

impl AudioDecoder {
    pub fn new(codec: Codec, sample_rate: u32) -> Result<Self, CodecError> {
        let opus = match codec {
            Codec::Opus => Some(opus::Decoder::new(sample_rate, opus::Channels::Mono)?),
            Codec::PcmS16 | Codec::PcmF32 => None,
        };
        Ok(Self { codec, opus })
    }

    /// Decodes one frame into `pcm` (exactly `pcm.len()` samples expected).
    /// `payload = None` performs packet-loss concealment.
    pub fn decode(&mut self, payload: Option<&[u8]>, pcm: &mut [f32]) -> Result<(), CodecError> {
        debug_assert!(pcm.len() <= MAX_FRAME_SAMPLES);
        match self.codec {
            Codec::Opus => {
                let dec = self.opus.as_mut().expect("opus decoder present");
                // The opus crate treats an empty input slice as packet loss.
                let n = dec.decode_float(payload.unwrap_or(&[]), pcm, false)?;
                // On PLC opus may return fewer samples than requested for
                // unusual frame sizes; zero-fill the tail defensively.
                for x in pcm.iter_mut().skip(n) {
                    *x = 0.0;
                }
                Ok(())
            }
            Codec::PcmS16 => {
                let Some(payload) = payload else {
                    pcm.fill(0.0);
                    return Ok(());
                };
                if payload.len() != pcm.len() * 2 {
                    return Err(CodecError::BadPcmLength);
                }
                for (x, chunk) in pcm.iter_mut().zip(payload.chunks_exact(2)) {
                    *x = i16::from_le_bytes([chunk[0], chunk[1]]) as f32 / i16::MAX as f32;
                }
                Ok(())
            }
            Codec::PcmF32 => {
                let Some(payload) = payload else {
                    pcm.fill(0.0);
                    return Ok(());
                };
                if payload.len() != pcm.len() * 4 {
                    return Err(CodecError::BadPcmLength);
                }
                for (x, chunk) in pcm.iter_mut().zip(payload.chunks_exact(4)) {
                    *x = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DEFAULT_FRAME_SAMPLES, SAMPLE_RATE};

    fn tone(n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| {
                (2.0 * std::f32::consts::PI * 440.0 * i as f32 / SAMPLE_RATE as f32).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn opus_roundtrip_2_5ms_frames() {
        let mut enc = AudioEncoder::new(Codec::Opus, SAMPLE_RATE, 96_000).unwrap();
        let mut dec = AudioDecoder::new(Codec::Opus, SAMPLE_RATE).unwrap();
        let mut out = [0u8; 1500];
        let mut pcm_out = [0.0f32; DEFAULT_FRAME_SAMPLES];
        let signal = tone(DEFAULT_FRAME_SAMPLES * 40);
        let mut total_bytes = 0;
        for frame in signal.chunks_exact(DEFAULT_FRAME_SAMPLES) {
            let n = enc.encode(frame, &mut out).unwrap();
            assert!(n > 0 && n < 200, "unexpected opus frame size {n}");
            total_bytes += n;
            dec.decode(Some(&out[..n]), &mut pcm_out).unwrap();
        }
        // ~96 kbps at 400 frames/s is ~30 bytes/frame; allow generous slack.
        let avg = total_bytes as f32 / 40.0;
        assert!(avg > 10.0 && avg < 80.0, "avg frame bytes {avg}");
    }

    #[test]
    fn opus_plc_produces_output_without_panic() {
        let mut enc = AudioEncoder::new(Codec::Opus, SAMPLE_RATE, 96_000).unwrap();
        let mut dec = AudioDecoder::new(Codec::Opus, SAMPLE_RATE).unwrap();
        let mut out = [0u8; 1500];
        let mut pcm_out = [0.0f32; DEFAULT_FRAME_SAMPLES];
        let signal = tone(DEFAULT_FRAME_SAMPLES);
        let n = enc.encode(&signal, &mut out).unwrap();
        dec.decode(Some(&out[..n]), &mut pcm_out).unwrap();
        // Conceal two lost frames.
        dec.decode(None, &mut pcm_out).unwrap();
        dec.decode(None, &mut pcm_out).unwrap();
    }

    #[test]
    fn pcm_s16_roundtrip() {
        let mut enc = AudioEncoder::new(Codec::PcmS16, SAMPLE_RATE, 0).unwrap();
        let mut dec = AudioDecoder::new(Codec::PcmS16, SAMPLE_RATE).unwrap();
        let signal = tone(DEFAULT_FRAME_SAMPLES);
        let mut out = [0u8; 1024];
        let mut back = [0.0f32; DEFAULT_FRAME_SAMPLES];
        let n = enc.encode(&signal, &mut out).unwrap();
        assert_eq!(n, DEFAULT_FRAME_SAMPLES * 2);
        dec.decode(Some(&out[..n]), &mut back).unwrap();
        for (a, b) in signal.iter().zip(back.iter()) {
            assert!((a - b).abs() < 1e-3);
        }
    }

    #[test]
    fn pcm_f32_roundtrip_is_bit_exact() {
        let mut enc = AudioEncoder::new(Codec::PcmF32, SAMPLE_RATE, 0).unwrap();
        let mut dec = AudioDecoder::new(Codec::PcmF32, SAMPLE_RATE).unwrap();
        let signal = tone(DEFAULT_FRAME_SAMPLES);
        let mut out = [0u8; 1024];
        let mut back = [0.0f32; DEFAULT_FRAME_SAMPLES];
        let n = enc.encode(&signal, &mut out).unwrap();
        dec.decode(Some(&out[..n]), &mut back).unwrap();
        assert_eq!(signal, back.to_vec());
    }

    #[test]
    fn pcm_rejects_bad_length() {
        let mut dec = AudioDecoder::new(Codec::PcmS16, SAMPLE_RATE).unwrap();
        let mut back = [0.0f32; DEFAULT_FRAME_SAMPLES];
        assert!(dec.decode(Some(&[0u8; 7]), &mut back).is_err());
    }
}
