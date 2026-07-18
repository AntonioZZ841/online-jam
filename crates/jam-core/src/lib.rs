//! Session glue: everything between the wire and the sound card.
//!
//! Thread model (one process per participant; the host also runs the mixer):
//!
//! - **Audio callback** (cpal): pulls capture samples, runs the frame
//!   pipeline ([`pipeline`]) — decode/PLC, mix, encode — and pushes finished
//!   datagrams onto a lock-free ring. No locks, no allocation, no syscalls.
//! - **Net RX thread** ([`net`]): blocking `recv_from`, parses, routes audio
//!   into per-sender jitter buffers and control packets to the control loop.
//! - **Net TX thread** ([`net`]): drains the datagram ring to the socket.
//! - **Control thread** ([`control`]): handshake state machines, keepalives,
//!   ping/RTT, timeouts, stats aggregation.
//!
//! The pipelines are pure with respect to I/O (they emit datagrams through a
//! closure), so the whole audio path is testable without sound hardware —
//! see `tests/localhost.rs`.

pub mod control;
pub mod engine;
pub mod net;
pub mod pipeline;
pub mod playout;
pub mod stats;

use jam_audio::jitter::JitterBuffer;
use jam_audio::meter::SharedLevel;
use jam_protocol::packet::Codec;
use jam_protocol::MAX_CLIENTS;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, Ordering};
use std::time::Instant;

/// How the player hears themselves (see README "Monitoring").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MonitorMode {
    /// Local zero-latency monitor + mix-minus-self from the host.
    #[default]
    Direct,
    /// Hear yourself through the host mix (full round trip).
    ThroughMix,
    /// No self-monitor (hardware direct monitoring on the interface).
    Off,
}

/// Runtime configuration shared by host and client roles.
#[derive(Debug, Clone)]
pub struct EngineParams {
    pub sample_rate: u32,
    pub frame_samples: usize,
    pub codec: Codec,
    pub bitrate_bps: i32,
    /// Attach the previous frame to every packet (RFC 2198-style repair).
    pub redundancy: bool,
    pub monitor: MonitorMode,
    /// Fix the jitter buffer at N frames instead of adapting.
    pub jitter_fixed: Option<i32>,
    /// Continuous clock-drift correction via fractional resampling (M8).
    /// When false, falls back to coarse drop/insert-at-quiet-moments only.
    pub drift_correction: bool,
    /// Drop this fraction of outgoing packets (dev/testing).
    pub simulate_loss: f32,
    /// Host only: loop each client's own audio back instead of the mix
    /// (latency measurement against `jam measure`).
    pub echo: bool,
}

impl Default for EngineParams {
    fn default() -> Self {
        Self {
            sample_rate: jam_audio::SAMPLE_RATE,
            frame_samples: jam_audio::DEFAULT_FRAME_SAMPLES,
            codec: Codec::Opus,
            bitrate_bps: 96_000,
            redundancy: false,
            monitor: MonitorMode::Direct,
            jitter_fixed: None,
            drift_correction: true,
            simulate_loss: 0.0,
            echo: false,
        }
    }
}

/// Monotonic session clock. All `now_ms`/`now_us` values in jam-core are
/// measured from this epoch.
#[derive(Debug, Clone)]
pub struct Clock {
    start: Instant,
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }
    pub fn now_ms(&self) -> u64 {
        self.start.elapsed().as_millis() as u64
    }
    pub fn now_us(&self) -> u64 {
        self.start.elapsed().as_micros() as u64
    }
}

/// An f32 published through an atomic (UI writes, callback reads, or vice
/// versa).
#[derive(Debug)]
pub struct AtomicF32(AtomicU32);

impl AtomicF32 {
    pub fn new(v: f32) -> Self {
        Self(AtomicU32::new(v.to_bits()))
    }
    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
    pub fn set(&self, v: f32) {
        self.0.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// Shared state for one remote audio stream (host: one per client slot;
/// client: a single instance for the mix coming back from the host).
#[derive(Debug)]
pub struct StreamShared {
    /// Set by the control thread when the peer joins/leaves; the audio
    /// callback skips inactive slots.
    pub active: AtomicBool,
    pub jb: JitterBuffer,
    pub level: SharedLevel,
    /// Last time (session clock, µs) anything arrived from this peer.
    pub last_rx_us: AtomicU64,
    /// Smoothed RTT to this peer, µs (measured by the control thread).
    pub rtt_us: AtomicU32,
}

impl Default for StreamShared {
    fn default() -> Self {
        Self {
            active: AtomicBool::new(false),
            jb: JitterBuffer::new(),
            level: SharedLevel::default(),
            last_rx_us: AtomicU64::new(0),
            rtt_us: AtomicU32::new(0),
        }
    }
}

/// Host-role shared state: slots for every possible client plus the host's
/// own input meter and per-player gains (index 0 = host, 1..=4 = clients).
#[derive(Debug)]
pub struct HostShared {
    pub session_id: u16,
    pub clients: [StreamShared; MAX_CLIENTS],
    pub gains: [AtomicF32; MAX_CLIENTS + 1],
    pub monitor_gain: AtomicF32,
    pub local_level: SharedLevel,
    /// Output frames where capture had no data (input/output clock slip).
    pub capture_starved: AtomicU64,
    pub shutdown: AtomicBool,
}

impl HostShared {
    pub fn new(session_id: u16) -> Self {
        Self {
            session_id,
            clients: Default::default(),
            gains: std::array::from_fn(|_| AtomicF32::new(1.0)),
            monitor_gain: AtomicF32::new(1.0),
            local_level: SharedLevel::default(),
            capture_starved: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        }
    }
}

/// Client-role shared state.
#[derive(Debug)]
pub struct ClientShared {
    /// True once WELCOME has been processed; gates the audio path.
    pub joined: AtomicBool,
    pub sender_id: AtomicU16,
    pub session_id: AtomicU16,
    /// The mix stream coming back from the host.
    pub from_host: StreamShared,
    pub monitor_gain: AtomicF32,
    pub master_gain: AtomicF32,
    pub local_level: SharedLevel,
    pub capture_starved: AtomicU64,
    pub shutdown: AtomicBool,
}

impl Default for ClientShared {
    fn default() -> Self {
        Self {
            joined: AtomicBool::new(false),
            sender_id: AtomicU16::new(jam_protocol::UNJOINED_SENDER_ID),
            session_id: AtomicU16::new(0),
            from_host: StreamShared::default(),
            monitor_gain: AtomicF32::new(1.0),
            master_gain: AtomicF32::new(1.0),
            local_level: SharedLevel::default(),
            capture_starved: AtomicU64::new(0),
            shutdown: AtomicBool::new(false),
        }
    }
}
