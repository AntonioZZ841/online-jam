//! Wraparound-safe 16-bit sequence number arithmetic (RFC 1982 style).

/// True if `a` is strictly newer than `b`, treating the u16 space as a ring.
/// Correct as long as the two values are within 32767 steps of each other.
#[inline]
pub fn newer_than(a: u16, b: u16) -> bool {
    a.wrapping_sub(b) as i16 > 0
}

/// Signed distance from `b` to `a` on the ring: positive if `a` is newer.
#[inline]
pub fn diff(a: u16, b: u16) -> i16 {
    a.wrapping_sub(b) as i16
}

/// Monotonic sequence generator for one sender.
#[derive(Debug, Default)]
pub struct SeqGen(u16);

impl SeqGen {
    pub fn new() -> Self {
        Self(0)
    }

    /// Returns the next sequence number to stamp on an outgoing packet.
    #[inline]
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u16 {
        let s = self.0;
        self.0 = self.0.wrapping_add(1);
        s
    }

    /// The sequence number the next call to `next()` will return.
    #[inline]
    pub fn peek(&self) -> u16 {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_basic() {
        assert!(newer_than(1, 0));
        assert!(!newer_than(0, 1));
        assert!(!newer_than(5, 5));
    }

    #[test]
    fn ordering_across_wraparound() {
        assert!(newer_than(0, u16::MAX));
        assert!(newer_than(5, u16::MAX - 5));
        assert!(!newer_than(u16::MAX, 0));
    }

    #[test]
    fn diff_signs() {
        assert_eq!(diff(10, 7), 3);
        assert_eq!(diff(7, 10), -3);
        assert_eq!(diff(2, u16::MAX), 3);
        assert_eq!(diff(u16::MAX, 2), -3);
    }

    #[test]
    fn generator_wraps() {
        let mut g = SeqGen(u16::MAX);
        assert_eq!(g.next(), u16::MAX);
        assert_eq!(g.next(), 0);
        assert_eq!(g.peek(), 1);
    }
}
