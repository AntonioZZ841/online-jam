//! Lock-free jitter buffer: one network-receive writer, one audio-callback
//! reader, no locks, no allocation after construction.
//!
//! Layout: a fixed ring of [`SLOTS`] slots, slot index = `seq % SLOTS`. Each
//! slot stores the *full* 16-bit sequence number so packets aliased across
//! the ring window (64 x 2.5 ms = 160 ms) are rejected. Payload bytes are
//! `AtomicU8` with relaxed ordering; cross-thread ordering is carried
//! entirely by the slot-state acquire/release pair, and the reader re-checks
//! the slot generation after copying to discard the (practically impossible,
//! but theoretically racy) torn read when a stalled reader races a writer
//! overwriting a 160 ms-stale slot.
//!
//! Adaptive playout policy ([`PlayoutController`]) is kept separate from the
//! buffer so it can be unit-tested against scripted arrival patterns.

use crate::MAX_FRAME_BYTES;
use jam_protocol::seq;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, Ordering};

pub const SLOTS: usize = 64;

const STATE_EMPTY: u8 = 0;
const STATE_WRITING: u8 = 1;
const STATE_WRITTEN: u8 = 2;

struct Slot {
    state: AtomicU8,
    seq: AtomicU16,
    timestamp: AtomicU32,
    len: AtomicU16,
    payload: [AtomicU8; MAX_FRAME_BYTES],
}

impl Slot {
    fn new() -> Self {
        Self {
            state: AtomicU8::new(STATE_EMPTY),
            seq: AtomicU16::new(0),
            timestamp: AtomicU32::new(0),
            len: AtomicU16::new(0),
            payload: [const { AtomicU8::new(0) }; MAX_FRAME_BYTES],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushOutcome {
    Stored,
    /// Arrived after its playout slot had already been concealed.
    Late,
    /// Same seq already present.
    Duplicate,
    /// Payload larger than `MAX_FRAME_BYTES`.
    TooBig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PopOutcome {
    /// A frame was copied into `out` (`len` bytes, sender timestamp).
    Frame { len: usize, timestamp: u32 },
    /// Nothing usable for this sequence number: conceal (PLC).
    Missing,
}

/// Counters the UI reads; updated with relaxed ordering.
#[derive(Debug, Default)]
pub struct JitterStats {
    pub received: AtomicU64,
    pub late: AtomicU64,
    pub duplicates: AtomicU64,
    pub concealed: AtomicU64,
    pub played: AtomicU64,
    pub resets: AtomicU64,
    /// RFC 3550-style inter-arrival jitter estimate, microseconds (f32 bits).
    pub jitter_us_bits: AtomicU32,
}

impl JitterStats {
    pub fn jitter_us(&self) -> f32 {
        f32::from_bits(self.jitter_us_bits.load(Ordering::Relaxed))
    }
    pub fn loss_fraction(&self) -> f32 {
        let played = self.played.load(Ordering::Relaxed) as f32;
        let concealed = self.concealed.load(Ordering::Relaxed) as f32;
        if played + concealed == 0.0 {
            0.0
        } else {
            concealed / (played + concealed)
        }
    }
}

impl std::fmt::Debug for JitterBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitterBuffer")
            .field("started", &self.started.load(Ordering::Relaxed))
            .field("depth", &self.depth())
            .finish_non_exhaustive()
    }
}

pub struct JitterBuffer {
    slots: Box<[Slot; SLOTS]>,
    /// Next sequence number the reader will play. Owned by the reader;
    /// writer reads it to classify late packets.
    next_seq: AtomicU16,
    /// Newest sequence number stored. Owned by the writer.
    newest_seq: AtomicU16,
    /// Becomes true on the first push; `next_seq`/`newest_seq` are garbage
    /// before that.
    started: AtomicBool,
    pub stats: JitterStats,
}

impl Default for JitterBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl JitterBuffer {
    pub fn new() -> Self {
        // Box the slot array via a Vec to avoid a large stack temporary.
        let slots: Vec<Slot> = (0..SLOTS).map(|_| Slot::new()).collect();
        let slots: Box<[Slot; SLOTS]> = slots.into_boxed_slice().try_into().ok().unwrap();
        Self {
            slots,
            next_seq: AtomicU16::new(0),
            newest_seq: AtomicU16::new(0),
            started: AtomicBool::new(false),
            stats: JitterStats::default(),
        }
    }

    /// Writer side (network receive thread).
    pub fn push(&self, incoming_seq: u16, timestamp: u32, payload: &[u8]) -> PushOutcome {
        if payload.len() > MAX_FRAME_BYTES {
            return PushOutcome::TooBig;
        }
        if !self.started.load(Ordering::Acquire) {
            // First packet establishes the stream position for both sides.
            self.next_seq.store(incoming_seq, Ordering::Relaxed);
            self.newest_seq.store(incoming_seq, Ordering::Relaxed);
            self.started.store(true, Ordering::Release);
        } else {
            let next = self.next_seq.load(Ordering::Acquire);
            if seq::newer_than(next, incoming_seq) {
                self.stats.late.fetch_add(1, Ordering::Relaxed);
                return PushOutcome::Late;
            }
            let newest = self.newest_seq.load(Ordering::Relaxed);
            if seq::newer_than(incoming_seq, newest) {
                self.newest_seq.store(incoming_seq, Ordering::Relaxed);
            }
        }

        let slot = &self.slots[incoming_seq as usize % SLOTS];
        let state = slot.state.load(Ordering::Acquire);
        if state == STATE_WRITTEN && slot.seq.load(Ordering::Relaxed) == incoming_seq {
            self.stats.duplicates.fetch_add(1, Ordering::Relaxed);
            return PushOutcome::Duplicate;
        }
        // Claim the slot. Whatever was here is either consumed (EMPTY) or at
        // least a full ring older than the incoming packet — overwrite it.
        slot.state.store(STATE_WRITING, Ordering::Release);
        slot.seq.store(incoming_seq, Ordering::Relaxed);
        slot.timestamp.store(timestamp, Ordering::Relaxed);
        slot.len.store(payload.len() as u16, Ordering::Relaxed);
        for (dst, src) in slot.payload.iter().zip(payload) {
            dst.store(*src, Ordering::Relaxed);
        }
        slot.state.store(STATE_WRITTEN, Ordering::Release);
        self.stats.received.fetch_add(1, Ordering::Relaxed);
        PushOutcome::Stored
    }

    /// Writer side: record the arrival-jitter estimate computed by the
    /// receive thread (it owns the arrival clock).
    pub fn publish_jitter_us(&self, jitter_us: f32) {
        self.stats
            .jitter_us_bits
            .store(jitter_us.to_bits(), Ordering::Relaxed);
    }

    /// Reader side: how many frames are buffered ahead of the read position
    /// (including the frame at the read position, if present). Can be
    /// negative right after a gap.
    pub fn depth(&self) -> i32 {
        if !self.started.load(Ordering::Acquire) {
            return 0;
        }
        let newest = self.newest_seq.load(Ordering::Relaxed);
        let next = self.next_seq.load(Ordering::Relaxed);
        seq::diff(newest, next) as i32 + 1
    }

    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::Acquire)
    }

    /// Reader side: pop the frame at the current read position, advancing it
    /// by one regardless of hit or miss (a miss means the caller conceals).
    pub fn pop_next(&self, out: &mut [u8]) -> PopOutcome {
        debug_assert!(out.len() >= MAX_FRAME_BYTES);
        if !self.started.load(Ordering::Acquire) {
            return PopOutcome::Missing;
        }
        let expect = self.next_seq.load(Ordering::Relaxed);
        let outcome = self.take(expect, out);
        self.next_seq
            .store(expect.wrapping_add(1), Ordering::Release);
        match outcome {
            PopOutcome::Frame { .. } => self.stats.played.fetch_add(1, Ordering::Relaxed),
            PopOutcome::Missing => self.stats.concealed.fetch_add(1, Ordering::Relaxed),
        };
        outcome
    }

    /// Reader side: drop the frame at the read position without decoding it
    /// (used to shrink the buffer). Returns true if a stored frame was
    /// discarded.
    pub fn skip_next(&self) -> bool {
        if !self.started.load(Ordering::Acquire) {
            return false;
        }
        let expect = self.next_seq.load(Ordering::Relaxed);
        let slot = &self.slots[expect as usize % SLOTS];
        let had = slot.state.load(Ordering::Acquire) == STATE_WRITTEN
            && slot.seq.load(Ordering::Relaxed) == expect;
        if had {
            slot.state.store(STATE_EMPTY, Ordering::Release);
        }
        self.next_seq
            .store(expect.wrapping_add(1), Ordering::Release);
        had
    }

    /// Reader side: forget the stream position entirely; the next push
    /// re-establishes it. Used after a prolonged outage.
    pub fn reset(&self) {
        for slot in self.slots.iter() {
            slot.state.store(STATE_EMPTY, Ordering::Release);
        }
        self.started.store(false, Ordering::Release);
        self.stats.resets.fetch_add(1, Ordering::Relaxed);
    }

    fn take(&self, expect: u16, out: &mut [u8]) -> PopOutcome {
        let slot = &self.slots[expect as usize % SLOTS];
        if slot.state.load(Ordering::Acquire) != STATE_WRITTEN
            || slot.seq.load(Ordering::Relaxed) != expect
        {
            return PopOutcome::Missing;
        }
        let len = slot.len.load(Ordering::Relaxed) as usize;
        let timestamp = slot.timestamp.load(Ordering::Relaxed);
        if len > MAX_FRAME_BYTES {
            return PopOutcome::Missing;
        }
        for (dst, src) in out[..len].iter_mut().zip(slot.payload.iter()) {
            *dst = src.load(Ordering::Relaxed);
        }
        // Re-check generation: if a writer overwrote this slot mid-copy the
        // seq or state changed and the copy is torn — discard it.
        if slot.state.load(Ordering::Acquire) != STATE_WRITTEN
            || slot.seq.load(Ordering::Relaxed) != expect
        {
            return PopOutcome::Missing;
        }
        slot.state.store(STATE_EMPTY, Ordering::Release);
        PopOutcome::Frame { len, timestamp }
    }
}

// ---------------------------------------------------------------------------
// Adaptive playout policy
// ---------------------------------------------------------------------------

/// Decision the audio callback executes for buffer-depth adaptation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adjust {
    None,
    /// Drop one frame (shrink the buffer by one frame's worth of latency).
    DropOne,
    /// Play one concealed frame without consuming (grow by one frame).
    InsertOne,
}

/// Target-depth controller: `target = min + ceil(k * jitter / frame)` frames,
/// clamped, re-evaluated periodically; depth corrections are applied only at
/// low-energy moments so they stay inaudible.
#[derive(Debug)]
pub struct PlayoutController {
    frame_us: f32,
    min_frames: i32,
    max_frames: i32,
    k: f32,
    target: i32,
    eval_interval_frames: u32,
    frames_since_eval: u32,
    primed: bool,
    consecutive_misses: u32,
    /// After this many consecutive concealed frames the stream is considered
    /// interrupted and the buffer re-primes from scratch.
    reset_after_misses: u32,
    pending: Adjust,
}

impl PlayoutController {
    pub fn new(frame_samples: usize, sample_rate: u32) -> Self {
        let frame_us = frame_samples as f32 * 1e6 / sample_rate as f32;
        let eval_interval_frames = (2_000_000.0 / frame_us) as u32; // ~2 s
        Self {
            frame_us,
            min_frames: 2,
            max_frames: 20,
            k: 3.0,
            target: 2,
            eval_interval_frames,
            frames_since_eval: 0,
            primed: false,
            consecutive_misses: 0,
            reset_after_misses: (500_000.0 / frame_us) as u32, // ~0.5 s
            pending: Adjust::None,
        }
    }

    pub fn target(&self) -> i32 {
        self.target
    }

    pub fn is_primed(&self) -> bool {
        self.primed
    }

    /// Fixed-depth mode for A/B latency testing (`--jitter fixed:<n>`).
    pub fn set_fixed(&mut self, frames: i32) {
        self.min_frames = frames;
        self.max_frames = frames;
        self.target = frames;
    }

    /// Called once per frame tick *before* popping. Returns false while the
    /// buffer is priming (caller outputs silence and must not pop).
    pub fn pre_pop(&mut self, depth: i32, started: bool) -> bool {
        if !self.primed {
            if started && depth >= self.target {
                self.primed = true;
                self.consecutive_misses = 0;
            }
            return self.primed;
        }
        true
    }

    /// Called once per frame tick after popping, with whether the pop hit and
    /// the fresh jitter estimate. Drives target re-evaluation and reset.
    /// Returns true if the stream should be reset (prolonged outage).
    pub fn post_pop(&mut self, hit: bool, jitter_us: f32) -> bool {
        if hit {
            self.consecutive_misses = 0;
        } else {
            self.consecutive_misses += 1;
            if self.consecutive_misses >= self.reset_after_misses {
                self.primed = false;
                self.consecutive_misses = 0;
                self.pending = Adjust::None;
                return true;
            }
        }
        self.frames_since_eval += 1;
        if self.frames_since_eval >= self.eval_interval_frames {
            self.frames_since_eval = 0;
            let jitter_frames = (self.k * jitter_us / self.frame_us).ceil() as i32;
            self.target = (self.min_frames + jitter_frames).clamp(self.min_frames, self.max_frames);
        }
        false
    }

    /// Called once per frame tick with the current depth; latches a pending
    /// one-frame correction when depth drifts out of the deadband.
    pub fn observe_depth(&mut self, depth: i32) {
        if self.pending != Adjust::None || !self.primed {
            return;
        }
        if depth > self.target + 1 {
            self.pending = Adjust::DropOne;
        } else if depth < self.target - 1 {
            self.pending = Adjust::InsertOne;
        }
    }

    /// The callback asks this at low-energy moments; returns and clears the
    /// pending correction.
    pub fn take_adjustment(&mut self) -> Adjust {
        std::mem::replace(&mut self.pending, Adjust::None)
    }

    /// Forget priming and pending state (stream restarted or slot reused).
    pub fn reset(&mut self) {
        self.primed = false;
        self.consecutive_misses = 0;
        self.pending = Adjust::None;
        self.frames_since_eval = 0;
    }
}

/// RFC 3550 §6.4.1 inter-arrival jitter, computed by the receive thread.
#[derive(Debug, Default)]
pub struct ArrivalJitter {
    prev: Option<(u64, u64)>, // (arrival_us, media_us)
    j_us: f32,
}

impl ArrivalJitter {
    /// `arrival_us`: local receive time. `timestamp_samples`: sender media
    /// clock from the packet, converted by the caller into microseconds.
    pub fn on_arrival(&mut self, arrival_us: u64, media_us: u64) -> f32 {
        if let Some((pa, pm)) = self.prev {
            let transit = arrival_us as i64 - media_us as i64;
            let prev_transit = pa as i64 - pm as i64;
            let d = (transit - prev_transit).abs() as f32;
            self.j_us += (d - self.j_us) / 16.0;
        }
        self.prev = Some((arrival_us, media_us));
        self.j_us
    }

    pub fn jitter_us(&self) -> f32 {
        self.j_us
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(tag: u8) -> Vec<u8> {
        vec![tag; 40]
    }

    #[test]
    fn in_order_stream_plays_back() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        for s in 0..10u16 {
            assert_eq!(
                jb.push(s, s as u32 * 120, &payload(s as u8)),
                PushOutcome::Stored
            );
        }
        assert_eq!(jb.depth(), 10);
        for s in 0..10u16 {
            match jb.pop_next(&mut out) {
                PopOutcome::Frame { len, timestamp } => {
                    assert_eq!(len, 40);
                    assert_eq!(timestamp, s as u32 * 120);
                    assert!(out[..len].iter().all(|b| *b == s as u8));
                }
                PopOutcome::Missing => panic!("frame {s} missing"),
            }
        }
        assert_eq!(jb.depth(), 0);
    }

    #[test]
    fn reordered_packets_play_in_order() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        jb.push(0, 0, &payload(0));
        jb.push(2, 240, &payload(2));
        jb.push(1, 120, &payload(1)); // out of order
        for s in 0..3u16 {
            match jb.pop_next(&mut out) {
                PopOutcome::Frame { .. } => assert_eq!(out[0], s as u8),
                PopOutcome::Missing => panic!("frame {s} missing"),
            }
        }
    }

    #[test]
    fn lost_packet_reports_missing_then_recovers() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        jb.push(0, 0, &payload(0));
        jb.push(2, 240, &payload(2)); // 1 lost
        assert!(matches!(jb.pop_next(&mut out), PopOutcome::Frame { .. }));
        assert_eq!(jb.pop_next(&mut out), PopOutcome::Missing);
        assert!(matches!(jb.pop_next(&mut out), PopOutcome::Frame { .. }));
        assert_eq!(jb.stats.concealed.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn late_packet_counted_and_rejected() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        jb.push(0, 0, &payload(0));
        jb.push(1, 120, &payload(1));
        jb.pop_next(&mut out);
        jb.pop_next(&mut out);
        assert_eq!(jb.push(0, 0, &payload(0)), PushOutcome::Late);
        assert_eq!(jb.stats.late.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn duplicate_detected() {
        let jb = JitterBuffer::new();
        jb.push(5, 600, &payload(5));
        assert_eq!(jb.push(5, 600, &payload(5)), PushOutcome::Duplicate);
    }

    #[test]
    fn seq_wraparound_is_seamless() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        let start = u16::MAX - 3;
        for i in 0..8u16 {
            jb.push(start.wrapping_add(i), i as u32 * 120, &payload(i as u8));
        }
        for i in 0..8u16 {
            match jb.pop_next(&mut out) {
                PopOutcome::Frame { .. } => assert_eq!(out[0], i as u8),
                PopOutcome::Missing => panic!("frame {i} missing across wrap"),
            }
        }
    }

    #[test]
    fn skip_next_drops_stored_frame() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        jb.push(0, 0, &payload(0));
        jb.push(1, 120, &payload(1));
        assert!(jb.skip_next());
        match jb.pop_next(&mut out) {
            PopOutcome::Frame { .. } => assert_eq!(out[0], 1),
            PopOutcome::Missing => panic!(),
        }
    }

    #[test]
    fn reset_reprimes() {
        let jb = JitterBuffer::new();
        let mut out = [0u8; MAX_FRAME_BYTES];
        jb.push(100, 0, &payload(1));
        jb.pop_next(&mut out);
        jb.reset();
        assert!(!jb.is_started());
        // A new stream position far away is accepted cleanly.
        assert_eq!(jb.push(40_000, 0, &payload(9)), PushOutcome::Stored);
        assert!(matches!(jb.pop_next(&mut out), PopOutcome::Frame { .. }));
    }

    #[test]
    fn controller_primes_at_target_depth() {
        let mut pc = PlayoutController::new(120, 48_000);
        assert!(!pc.pre_pop(0, false));
        assert!(!pc.pre_pop(1, true));
        assert!(pc.pre_pop(2, true)); // default target 2
        assert!(pc.is_primed());
    }

    #[test]
    fn controller_requests_shrink_when_deep() {
        let mut pc = PlayoutController::new(120, 48_000);
        pc.pre_pop(2, true);
        pc.observe_depth(pc.target() + 2);
        assert_eq!(pc.take_adjustment(), Adjust::DropOne);
        assert_eq!(pc.take_adjustment(), Adjust::None);
    }

    #[test]
    fn controller_requests_growth_when_shallow() {
        let mut pc = PlayoutController::new(120, 48_000);
        pc.pre_pop(2, true);
        // Push target up so a shallow depth triggers growth.
        for _ in 0..1000 {
            pc.post_pop(true, 20_000.0); // 20 ms jitter -> large target
        }
        assert!(pc.target() > 3);
        pc.observe_depth(1);
        assert_eq!(pc.take_adjustment(), Adjust::InsertOne);
    }

    #[test]
    fn controller_resets_after_prolonged_outage() {
        let mut pc = PlayoutController::new(120, 48_000);
        pc.pre_pop(2, true);
        let mut reset = false;
        for _ in 0..300 {
            if pc.post_pop(false, 0.0) {
                reset = true;
                break;
            }
        }
        assert!(reset);
        assert!(!pc.is_primed());
    }

    #[test]
    fn controller_target_tracks_jitter() {
        let mut pc = PlayoutController::new(120, 48_000);
        pc.pre_pop(2, true);
        for _ in 0..1000 {
            pc.post_pop(true, 0.0);
        }
        assert_eq!(pc.target(), 2);
        for _ in 0..1000 {
            pc.post_pop(true, 5_000.0); // 5 ms jitter
        }
        // 3 * 5ms / 2.5ms = 6 frames above min.
        assert_eq!(pc.target(), 8);
    }

    #[test]
    fn arrival_jitter_converges() {
        let mut aj = ArrivalJitter::default();
        // Perfectly paced arrivals -> zero jitter.
        for i in 0..100u64 {
            aj.on_arrival(i * 2_500, i * 2_500);
        }
        assert!(aj.jitter_us() < 1.0);
        // Alternating +1 ms/on-time arrivals -> |d| is 1 ms every step, so
        // the EWMA approaches 1 ms from below.
        for i in 100..300u64 {
            let wobble = if i % 2 == 0 { 1_000 } else { 0 };
            aj.on_arrival(i * 2_500 + wobble, i * 2_500);
        }
        assert!(aj.jitter_us() > 900.0 && aj.jitter_us() <= 1_000.0);
    }
}
