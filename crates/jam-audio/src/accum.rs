//! Fixed-capacity sample FIFO bridging hardware buffer sizes (64/128/256
//! samples) to codec frames (120/240 samples). Single-threaded: lives inside
//! the audio callback. No allocation after construction.

pub struct SampleFifo {
    buf: Vec<f32>,
    head: usize,
    len: usize,
}

impl SampleFifo {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0.0; capacity],
            head: 0,
            len: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Appends samples, dropping the *oldest* ones on overflow so latency
    /// stays bounded. Returns how many were dropped.
    pub fn push(&mut self, samples: &[f32]) -> usize {
        let cap = self.buf.len();
        let mut dropped = 0;
        if samples.len() >= cap {
            // Pathological: incoming block alone fills the FIFO.
            dropped = self.len + samples.len() - cap;
            self.head = 0;
            self.len = cap;
            let start = samples.len() - cap;
            self.buf.copy_from_slice(&samples[start..]);
            return dropped;
        }
        let overflow = (self.len + samples.len()).saturating_sub(cap);
        if overflow > 0 {
            self.head = (self.head + overflow) % cap;
            self.len -= overflow;
            dropped = overflow;
        }
        let tail = (self.head + self.len) % cap;
        let first = (cap - tail).min(samples.len());
        self.buf[tail..tail + first].copy_from_slice(&samples[..first]);
        let rest = samples.len() - first;
        if rest > 0 {
            self.buf[..rest].copy_from_slice(&samples[first..]);
        }
        self.len += samples.len();
        dropped
    }

    /// Pops exactly `out.len()` samples if available; returns false (and
    /// leaves the FIFO untouched) otherwise.
    pub fn pop(&mut self, out: &mut [f32]) -> bool {
        if self.len < out.len() {
            return false;
        }
        let cap = self.buf.len();
        let first = (cap - self.head).min(out.len());
        out[..first].copy_from_slice(&self.buf[self.head..self.head + first]);
        let rest = out.len() - first;
        if rest > 0 {
            out[first..].copy_from_slice(&self.buf[..rest]);
        }
        self.head = (self.head + out.len()) % cap;
        self.len -= out.len();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_pop_across_wrap() {
        let mut f = SampleFifo::new(16);
        let mut out = [0.0f32; 6];
        for round in 0..10 {
            let base = round as f32 * 6.0;
            let block: Vec<f32> = (0..6).map(|i| base + i as f32).collect();
            assert_eq!(f.push(&block), 0);
            assert!(f.pop(&mut out));
            assert_eq!(out.to_vec(), block);
        }
    }

    #[test]
    fn pop_fails_when_underfilled() {
        let mut f = SampleFifo::new(8);
        f.push(&[1.0, 2.0]);
        let mut out = [0.0f32; 4];
        assert!(!f.pop(&mut out));
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn overflow_drops_oldest() {
        let mut f = SampleFifo::new(4);
        f.push(&[1.0, 2.0, 3.0, 4.0]);
        assert_eq!(f.push(&[5.0, 6.0]), 2);
        let mut out = [0.0f32; 4];
        assert!(f.pop(&mut out));
        assert_eq!(out, [3.0, 4.0, 5.0, 6.0]);
    }

    #[test]
    fn giant_push_keeps_newest() {
        let mut f = SampleFifo::new(4);
        f.push(&[0.0; 2]);
        let big: Vec<f32> = (0..10).map(|i| i as f32).collect();
        assert_eq!(f.push(&big), 8);
        let mut out = [0.0f32; 4];
        assert!(f.pop(&mut out));
        assert_eq!(out, [6.0, 7.0, 8.0, 9.0]);
    }
}
