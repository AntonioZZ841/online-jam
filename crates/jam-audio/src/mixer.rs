//! Gain smoothing, mixing, and soft clipping. All functions are
//! allocation-free and run on the audio callback.

/// Per-frame smoothed gain: moves toward the UI-set target with a one-pole
/// lowpass evaluated once per frame (~400 Hz update rate), which keeps
/// full-range gain changes inaudible (~10 ms settle) without per-sample cost.
#[derive(Debug, Clone, Copy)]
pub struct SmoothedGain {
    current: f32,
    /// Per-frame smoothing coefficient for a ~10 ms time constant at 2.5 ms
    /// frames: 1 - exp(-frame/tau) with tau = 10 ms.
    alpha: f32,
}

impl SmoothedGain {
    pub fn new(initial: f32, frame_samples: usize, sample_rate: u32) -> Self {
        let frame_s = frame_samples as f32 / sample_rate as f32;
        let tau = 0.010;
        Self {
            current: initial,
            alpha: 1.0 - (-frame_s / tau).exp(),
        }
    }

    /// Advances one frame toward `target` and returns the gain to apply.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self, target: f32) -> f32 {
        self.current += (target - self.current) * self.alpha;
        if (self.current - target).abs() < 1e-4 {
            self.current = target;
        }
        self.current
    }

    pub fn current(&self) -> f32 {
        self.current
    }
}

/// `dst[i] += src[i] * gain`
#[inline]
pub fn mix_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += s * gain;
    }
}

/// `dst[i] -= src[i] * gain` — used for mix-minus-self.
#[inline]
pub fn unmix_scaled(dst: &mut [f32], src: &[f32], gain: f32) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d -= s * gain;
    }
}

/// Cubic soft clipper `y = x - x³/3`: unity gain for quiet signals,
/// saturating smoothly to ±2/3 at |x| = 1, hard-limited beyond (the ~3.5 dB
/// ceiling below full scale is deliberate headroom). No lookahead —
/// lookahead would add latency.
#[inline]
pub fn soft_clip(x: f32) -> f32 {
    if x >= 1.0 {
        2.0 / 3.0
    } else if x <= -1.0 {
        -2.0 / 3.0
    } else {
        x - x * x * x / 3.0
    }
}

pub fn soft_clip_buf(buf: &mut [f32]) {
    for x in buf.iter_mut() {
        *x = soft_clip(*x);
    }
}

/// Root-mean-square of a frame; used for the "low energy" gate that decides
/// when jitter-buffer depth corrections are inaudible.
#[inline]
pub fn frame_rms(buf: &[f32]) -> f32 {
    if buf.is_empty() {
        return 0.0;
    }
    let sum: f32 = buf.iter().map(|x| x * x).sum();
    (sum / buf.len() as f32).sqrt()
}

/// RMS below this is considered a quiet moment (-50 dBFS).
pub const LOW_ENERGY_RMS: f32 = 0.003;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoothed_gain_converges_in_about_10ms() {
        let mut g = SmoothedGain::new(0.0, 120, 48_000);
        let mut frames = 0;
        while (g.next(1.0) - 1.0).abs() > 1e-3 && frames < 100 {
            frames += 1;
        }
        // ~10 ms tau -> residual 1e-3 needs ~7 tau = ~28 frames (70 ms).
        assert!(frames > 2 && frames < 40, "settled in {frames} frames");
    }

    #[test]
    fn mix_and_unmix_cancel() {
        let src = [0.5f32; 8];
        let mut dst = [0.0f32; 8];
        mix_scaled(&mut dst, &src, 0.8);
        unmix_scaled(&mut dst, &src, 0.8);
        assert!(dst.iter().all(|x| x.abs() < 1e-7));
    }

    #[test]
    fn soft_clip_is_transparent_when_quiet_and_bounded_when_loud() {
        assert!((soft_clip(0.1) - 0.0997).abs() < 1e-3); // ~unity below -10 dB
        assert_eq!(soft_clip(2.0), 2.0 / 3.0);
        assert_eq!(soft_clip(-2.0), -2.0 / 3.0);
        assert!((soft_clip(1.0) - 2.0 / 3.0).abs() < 1e-6);
        // Monotonic through the knee.
        let mut prev = -2.0;
        for i in -20..=20 {
            let y = soft_clip(i as f32 / 10.0);
            assert!(y >= prev);
            prev = y;
        }
    }

    #[test]
    fn rms_of_silence_and_tone() {
        assert_eq!(frame_rms(&[0.0; 120]), 0.0);
        let tone: Vec<f32> = (0..120).map(|i| (i as f32 * 0.3).sin() * 0.5).collect();
        let r = frame_rms(&tone);
        assert!(r > 0.2 && r < 0.5);
    }
}
