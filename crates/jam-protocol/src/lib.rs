//! Wire format and session state machines for the jam protocol.
//!
//! This crate performs no I/O: packets are parsed from and serialized into
//! caller-provided byte slices, and the handshake state machines are pure
//! `(state, event) -> actions` functions driven by an external clock. That
//! keeps everything here unit-testable without sockets or timers, and keeps
//! the audio hot path free of allocation (audio packets borrow their payloads
//! from the receive buffer).

pub mod handshake;
pub mod packet;
pub mod seq;

/// First byte of every datagram; anything else is dropped on sight.
pub const MAGIC: u8 = 0xA5;
pub const PROTOCOL_VERSION: u8 = 1;
pub const DEFAULT_PORT: u16 = 47820;

/// Maximum number of connected clients. The host itself is player 0, so a
/// full room is `MAX_CLIENTS + 1` musicians.
pub const MAX_CLIENTS: usize = 4;
pub const HOST_SENDER_ID: u16 = 0;
/// Sender id used by clients before the host has assigned them a slot.
pub const UNJOINED_SENDER_ID: u16 = 0xFFFF;

pub const ROOM_CODE_LEN: usize = 6;
pub const MAX_NAME_LEN: usize = 32;
/// Upper bound on any datagram we send. Opus frames keep datagrams well
/// inside a 1500-byte MTU; the ceiling is sized for the worst legal case —
/// PCM f32 at a 5 ms frame (960 bytes) carried with redundancy (a second
/// 960-byte copy) plus headers ≈ 1938 bytes — so serialization can never
/// overflow the fixed send buffers and panic the audio callback. Such large
/// datagrams only occur in `--pcm` mode, which is LAN-only by design.
pub const MAX_DATAGRAM: usize = 2048;

/// Reliable-control retransmit interval and attempt cap (100 ms x 10).
pub const CONTROL_RETRY_MS: u64 = 100;
pub const CONTROL_MAX_TRIES: u32 = 10;
/// Keepalive cadence while no audio is flowing, and the silence threshold
/// after which a peer is declared dead.
pub const KEEPALIVE_MS: u64 = 500;
pub const PEER_TIMEOUT_MS: u64 = 5_000;
/// How long a disconnected client keeps trying to rejoin.
pub const REJOIN_WINDOW_MS: u64 = 30_000;
