//! Peak/RMS level metering with atomic publication for the UI thread.

use std::sync::atomic::{AtomicU32, Ordering};

/// Callback-side meter state. Peak holds with exponential decay so the UI's
/// 500 ms refresh still shows transients.
#[derive(Debug)]
pub struct Meter {
    peak: f32,
    rms_sq_avg: f32,
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

impl Meter {
    pub fn new() -> Self {
        Self {
            peak: 0.0,
            rms_sq_avg: 0.0,
        }
    }

    /// Feed one frame; call once per frame tick.
    pub fn update(&mut self, frame: &[f32]) {
        let mut peak = 0.0f32;
        let mut sum = 0.0f32;
        for &x in frame {
            let a = x.abs();
            if a > peak {
                peak = a;
            }
            sum += x * x;
        }
        // ~0.5 s peak decay at 400 frames/s.
        self.peak = (self.peak * 0.995).max(peak);
        let rms_sq = sum / frame.len().max(1) as f32;
        // ~50 ms RMS integration.
        self.rms_sq_avg += (rms_sq - self.rms_sq_avg) * 0.12;
    }

    pub fn publish(&self, shared: &SharedLevel) {
        shared
            .peak_bits
            .store(self.peak.to_bits(), Ordering::Relaxed);
        shared
            .rms_bits
            .store(self.rms_sq_avg.sqrt().to_bits(), Ordering::Relaxed);
    }
}

/// UI-visible level, written by the callback, read anywhere.
#[derive(Debug, Default)]
pub struct SharedLevel {
    peak_bits: AtomicU32,
    rms_bits: AtomicU32,
}

impl SharedLevel {
    pub fn peak(&self) -> f32 {
        f32::from_bits(self.peak_bits.load(Ordering::Relaxed))
    }
    pub fn rms(&self) -> f32 {
        f32::from_bits(self.rms_bits.load(Ordering::Relaxed))
    }
    pub fn rms_db(&self) -> f32 {
        linear_to_db(self.rms())
    }
    pub fn peak_db(&self) -> f32 {
        linear_to_db(self.peak())
    }
}

pub fn linear_to_db(x: f32) -> f32 {
    if x <= 1e-6 {
        -120.0
    } else {
        20.0 * x.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meter_tracks_tone_level() {
        let mut m = Meter::new();
        let tone: Vec<f32> = (0..120).map(|i| (i as f32 * 0.4).sin() * 0.7).collect();
        for _ in 0..100 {
            m.update(&tone);
        }
        let shared = SharedLevel::default();
        m.publish(&shared);
        assert!(shared.peak() > 0.65 && shared.peak() <= 0.71);
        // Sine RMS = amplitude / sqrt(2) ~= 0.49
        assert!((shared.rms() - 0.49).abs() < 0.05);
    }

    #[test]
    fn peak_decays_after_transient() {
        let mut m = Meter::new();
        let hit = [0.9f32; 120];
        let silence = [0.0f32; 120];
        m.update(&hit);
        for _ in 0..800 {
            m.update(&silence);
        }
        let shared = SharedLevel::default();
        m.publish(&shared);
        assert!(shared.peak() < 0.05);
    }

    #[test]
    fn db_conversion() {
        assert_eq!(linear_to_db(0.0), -120.0);
        assert!((linear_to_db(1.0)).abs() < 1e-4);
        assert!((linear_to_db(0.5) + 6.02).abs() < 0.1);
    }
}
