//! Clock-drift compensation: a 4-point cubic Hermite (Catmull-Rom)
//! fractional resampler driven by a PI controller on jitter-buffer
//! occupancy error. Corrections stay within a few hundred ppm, where
//! Hermite interpolation is audibly transparent and adds only ~2 samples
//! of delay (unlike sinc kernels, which add group delay).

/// Variable-ratio fractional resampler. `ratio` is output-rate / input-rate:
/// ratio > 1 consumes input slower (stretches), ratio < 1 consumes faster.
#[derive(Debug)]
pub struct HermiteResampler {
    /// Last four input samples: h[3] is the newest.
    h: [f32; 4],
    /// Fractional read position within the current interval [h[1], h[2]].
    phase: f64,
    ratio: f64,
    primed: usize,
}

impl HermiteResampler {
    pub fn new() -> Self {
        Self {
            h: [0.0; 4],
            phase: 0.0,
            ratio: 1.0,
            primed: 0,
        }
    }

    pub fn set_ratio(&mut self, ratio: f64) {
        self.ratio = ratio.clamp(0.98, 1.02);
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    #[inline]
    fn interpolate(&self, t: f32) -> f32 {
        let [y0, y1, y2, y3] = self.h;
        let c0 = y1;
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + c0
    }

    /// Feeds `input`, writing resampled output into `out`. Returns the number
    /// of output samples produced (bounded by `out.len()`; size `out` with
    /// ~`input.len() * ratio + 2` headroom to avoid truncation).
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) -> usize {
        let mut produced = 0;
        let step = 1.0 / self.ratio; // input samples consumed per output sample
        for &x in input {
            self.h = [self.h[1], self.h[2], self.h[3], x];
            if self.primed < 3 {
                self.primed += 1;
                continue;
            }
            // A new interval [h[1], h[2]] became available; phase measures
            // position inside it.
            while self.phase < 1.0 {
                if produced < out.len() {
                    out[produced] = self.interpolate(self.phase as f32);
                    produced += 1;
                }
                self.phase += step;
            }
            self.phase -= 1.0;
        }
        produced
    }
}

impl Default for HermiteResampler {
    fn default() -> Self {
        Self::new()
    }
}

/// PI controller mapping jitter-buffer depth error (frames) to a resample
/// ratio near 1.0. Slew-limited so corrections glide instead of stepping.
#[derive(Debug)]
pub struct DriftController {
    kp: f64,
    ki: f64,
    integral: f64,
    current_ppm: f64,
    /// Max ppm change per update, keeping ratio changes inaudible.
    slew_ppm: f64,
    max_ppm: f64,
}

impl DriftController {
    pub fn new() -> Self {
        Self {
            // Critically damped for per-frame (2.5 ms) updates: the loop
            // dynamics give omega = sqrt(ki * 1e-6) and zeta =
            // kp * 1e-6 / (2 * omega); kp=450, ki=0.05 puts zeta at ~1.0
            // with an ~11 s time constant.
            kp: 450.0,
            ki: 0.05,
            integral: 0.0,
            current_ppm: 0.0,
            slew_ppm: 0.5,
            max_ppm: 500.0,
        }
    }

    /// `depth_error` = measured depth - target depth, in frames. Positive
    /// error means the buffer is filling: the sender's clock runs fast
    /// relative to ours, so we must consume faster (ratio < 1).
    pub fn update(&mut self, depth_error: f64) -> f64 {
        self.integral = (self.integral + depth_error * self.ki).clamp(-self.max_ppm, self.max_ppm);
        let want_ppm = -(depth_error * self.kp + self.integral);
        let want_ppm = want_ppm.clamp(-self.max_ppm, self.max_ppm);
        let delta = (want_ppm - self.current_ppm).clamp(-self.slew_ppm, self.slew_ppm);
        self.current_ppm += delta;
        1.0 + self.current_ppm * 1e-6
    }

    pub fn current_ppm(&self) -> f64 {
        self.current_ppm
    }
}

impl Default for DriftController {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_ratio_passes_signal_through() {
        let mut rs = HermiteResampler::new();
        let input: Vec<f32> = (0..4800).map(|i| (i as f32 * 0.05).sin()).collect();
        let mut out = vec![0.0f32; 5000];
        let n = rs.process(&input, &mut out);
        // Within a couple samples of input length.
        assert!((n as i64 - input.len() as i64).abs() <= 3, "produced {n}");
        // At unity ratio and phase 0 the interpolator outputs h[1] exactly,
        // which maps to out[i] = input[i + 1]: a pure 1-sample shift in this
        // indexing (the physical latency is 2 samples behind the newest
        // input). The signal must match closely.
        let mut max_err = 0.0f32;
        for i in 10..n - 10 {
            let err = (out[i] - input[i + 1]).abs();
            max_err = max_err.max(err);
        }
        assert!(max_err < 1e-3, "max_err={max_err}");
    }

    #[test]
    fn ratio_changes_output_count() {
        let mut rs = HermiteResampler::new();
        rs.set_ratio(1.01);
        let input = vec![0.0f32; 10_000];
        let mut out = vec![0.0f32; 11_000];
        let n = rs.process(&input, &mut out);
        assert!((n as f64 - 10_000.0 * 1.01).abs() < 20.0, "n={n}");

        let mut rs = HermiteResampler::new();
        rs.set_ratio(0.99);
        let n = rs.process(&input, &mut out);
        assert!((n as f64 - 10_000.0 * 0.99).abs() < 20.0, "n={n}");
    }

    #[test]
    fn resampled_tone_stays_clean() {
        // 200 ppm offset must not distort a sine beyond tiny error.
        let mut rs = HermiteResampler::new();
        rs.set_ratio(1.0002);
        let input: Vec<f32> = (0..48_000)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / 48_000.0).sin())
            .collect();
        let mut out = vec![0.0f32; 48_200];
        let n = rs.process(&input, &mut out);
        assert!(n > 47_900);
        // The output is still a sine of the same amplitude: check peak level
        // of the middle chunk.
        let mid = &out[1000..n - 1000];
        let peak = mid.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
        assert!((peak - 1.0).abs() < 0.01);
    }

    #[test]
    fn drift_controller_converges_on_simulated_offset() {
        // Simulate a sender 200 ppm fast: depth grows by 200e-6 frames per
        // frame unless the ratio compensates.
        let mut ctl = DriftController::new();
        let mut depth = 0.0f64; // depth error in frames
        let sender_ppm = 200.0;
        let mut worst_late = 0.0f64;
        for i in 0..400_000 {
            // 400k frames = ~16 minutes
            let ratio = ctl.update(depth);
            let consume_ppm = (1.0 - ratio) * 1e6; // positive consumes faster
            depth += (sender_ppm - consume_ppm) * 1e-6;
            if i > 200_000 {
                worst_late = worst_late.max(depth.abs());
            }
        }
        // After convergence the controller holds depth error near zero and
        // its correction near the true offset.
        assert!(
            (ctl.current_ppm() + sender_ppm).abs() < 20.0,
            "ppm={}",
            ctl.current_ppm()
        );
        assert!(worst_late < 1.0, "depth error {worst_late}");
    }

    #[test]
    fn drift_controller_is_slew_limited() {
        let mut ctl = DriftController::new();
        let r1 = ctl.update(1000.0);
        // One update can only move by slew_ppm.
        assert!((r1 - 1.0).abs() <= 0.6e-6);
    }
}
