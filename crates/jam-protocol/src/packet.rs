//! Datagram wire format: hand-rolled, bounds-checked serialization.
//!
//! Layout of every datagram (big-endian):
//!
//! ```text
//! offset 0  u8   magic (0xA5)
//! offset 1  u8   version (high nibble) | packet type (low nibble)
//! offset 2  u16  session id
//! offset 4  u16  sender id (0 = host, 1..=4 = clients, 0xFFFF = unjoined)
//! offset 6  u16  sequence number (per sender, wraps)
//! offset 8  ...  type-specific body
//! ```
//!
//! Audio bodies borrow from the receive buffer (zero-copy, no allocation on
//! the hot path). Control bodies deserialize into owned values; they are
//! parsed on the control thread where allocation is fine.

use crate::seq;
use crate::{MAGIC, MAX_NAME_LEN, PROTOCOL_VERSION, ROOM_CODE_LEN};

pub const HEADER_LEN: usize = 8;
pub const AUDIO_HEADER_LEN: usize = 8; // timestamp(4) codec(1) flags(1) len(2)

pub const AUDIO_FLAG_REDUNDANCY: u8 = 0b0000_0001;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
    #[error("datagram too short")]
    Truncated,
    #[error("bad magic byte")]
    BadMagic,
    #[error("unsupported protocol version {0}")]
    BadVersion(u8),
    #[error("unknown packet type {0}")]
    BadType(u8),
    #[error("unknown codec {0}")]
    BadCodec(u8),
    #[error("unknown deny reason {0}")]
    BadDenyReason(u8),
    #[error("invalid string field")]
    BadString,
    #[error("length field exceeds datagram")]
    BadLength,
    #[error("trailing bytes after body")]
    TrailingBytes,
    #[error("value out of range")]
    OutOfRange,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PacketType {
    Audio = 0,
    Hello = 1,
    Welcome = 2,
    Deny = 3,
    Ping = 4,
    Pong = 5,
    Keepalive = 6,
    Roster = 7,
    Bye = 8,
    Ack = 9,
}

impl TryFrom<u8> for PacketType {
    type Error = ParseError;
    fn try_from(v: u8) -> Result<Self, ParseError> {
        Ok(match v {
            0 => Self::Audio,
            1 => Self::Hello,
            2 => Self::Welcome,
            3 => Self::Deny,
            4 => Self::Ping,
            5 => Self::Pong,
            6 => Self::Keepalive,
            7 => Self::Roster,
            8 => Self::Bye,
            9 => Self::Ack,
            other => return Err(ParseError::BadType(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Codec {
    #[default]
    Opus = 0,
    PcmS16 = 1,
    PcmF32 = 2,
}

impl TryFrom<u8> for Codec {
    type Error = ParseError;
    fn try_from(v: u8) -> Result<Self, ParseError> {
        Ok(match v {
            0 => Self::Opus,
            1 => Self::PcmS16,
            2 => Self::PcmF32,
            other => return Err(ParseError::BadCodec(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum DenyReason {
    BadRoomCode = 0,
    RoomFull = 1,
    VersionMismatch = 2,
}

impl TryFrom<u8> for DenyReason {
    type Error = ParseError;
    fn try_from(v: u8) -> Result<Self, ParseError> {
        Ok(match v {
            0 => Self::BadRoomCode,
            1 => Self::RoomFull,
            2 => Self::VersionMismatch,
            other => return Err(ParseError::BadDenyReason(other)),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub ptype: PacketType,
    pub session_id: u16,
    pub sender_id: u16,
    pub seq: u16,
}

impl Header {
    pub fn write(&self, out: &mut [u8]) -> usize {
        out[0] = MAGIC;
        out[1] = (PROTOCOL_VERSION << 4) | (self.ptype as u8);
        out[2..4].copy_from_slice(&self.session_id.to_be_bytes());
        out[4..6].copy_from_slice(&self.sender_id.to_be_bytes());
        out[6..8].copy_from_slice(&self.seq.to_be_bytes());
        HEADER_LEN
    }

    pub fn parse(buf: &[u8]) -> Result<(Header, &[u8]), ParseError> {
        if buf.len() < HEADER_LEN {
            return Err(ParseError::Truncated);
        }
        if buf[0] != MAGIC {
            return Err(ParseError::BadMagic);
        }
        let version = buf[1] >> 4;
        if version != PROTOCOL_VERSION {
            return Err(ParseError::BadVersion(version));
        }
        let ptype = PacketType::try_from(buf[1] & 0x0F)?;
        let header = Header {
            ptype,
            session_id: u16::from_be_bytes([buf[2], buf[3]]),
            sender_id: u16::from_be_bytes([buf[4], buf[5]]),
            seq: u16::from_be_bytes([buf[6], buf[7]]),
        };
        Ok((header, &buf[HEADER_LEN..]))
    }
}

// ---------------------------------------------------------------------------
// Audio packets (zero-copy)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacket<'a> {
    /// Sample clock of the first sample in this frame, in the sender's clock.
    pub timestamp: u32,
    pub codec: Codec,
    pub payload: &'a [u8],
    /// Re-encoded copy of the *previous* frame, if the sender enabled
    /// redundancy (RFC 2198-style). Lets the receiver repair a single loss
    /// with zero added latency.
    pub redundant: Option<&'a [u8]>,
}

impl<'a> AudioPacket<'a> {
    /// Serializes the audio body after `header` into `out`.
    /// Returns the total datagram length. Panics if `out` is too small —
    /// callers size `out` as `MAX_DATAGRAM`.
    pub fn write(&self, header: &Header, out: &mut [u8]) -> usize {
        let mut n = header.write(out);
        out[n..n + 4].copy_from_slice(&self.timestamp.to_be_bytes());
        out[n + 4] = self.codec as u8;
        out[n + 5] = if self.redundant.is_some() {
            AUDIO_FLAG_REDUNDANCY
        } else {
            0
        };
        let plen = self.payload.len() as u16;
        out[n + 6..n + 8].copy_from_slice(&plen.to_be_bytes());
        n += AUDIO_HEADER_LEN;
        out[n..n + self.payload.len()].copy_from_slice(self.payload);
        n += self.payload.len();
        if let Some(red) = self.redundant {
            let rlen = red.len() as u16;
            out[n..n + 2].copy_from_slice(&rlen.to_be_bytes());
            n += 2;
            out[n..n + red.len()].copy_from_slice(red);
            n += red.len();
        }
        n
    }

    /// Parses the body that follows a `PacketType::Audio` header.
    pub fn parse(body: &'a [u8]) -> Result<AudioPacket<'a>, ParseError> {
        if body.len() < AUDIO_HEADER_LEN {
            return Err(ParseError::Truncated);
        }
        let timestamp = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
        let codec = Codec::try_from(body[4])?;
        let flags = body[5];
        let plen = u16::from_be_bytes([body[6], body[7]]) as usize;
        let rest = &body[AUDIO_HEADER_LEN..];
        if rest.len() < plen {
            return Err(ParseError::BadLength);
        }
        let payload = &rest[..plen];
        let mut tail = &rest[plen..];
        let redundant = if flags & AUDIO_FLAG_REDUNDANCY != 0 {
            if tail.len() < 2 {
                return Err(ParseError::Truncated);
            }
            let rlen = u16::from_be_bytes([tail[0], tail[1]]) as usize;
            tail = &tail[2..];
            if tail.len() < rlen {
                return Err(ParseError::BadLength);
            }
            let red = &tail[..rlen];
            tail = &tail[rlen..];
            Some(red)
        } else {
            None
        };
        if !tail.is_empty() {
            return Err(ParseError::TrailingBytes);
        }
        Ok(AudioPacket {
            timestamp,
            codec,
            payload,
            redundant,
        })
    }
}

// ---------------------------------------------------------------------------
// Control packets (owned)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInfo {
    pub id: u16,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Control {
    Hello {
        proto_version: u8,
        room_code: [u8; ROOM_CODE_LEN],
        name: String,
        /// Bitmask of `Codec` values the client can use (bit N = codec N).
        codec_mask: u8,
        /// Bitmask of supported frame sizes (bit 0 = 2.5 ms, bit 1 = 5 ms).
        frame_mask: u8,
    },
    Welcome {
        assigned_id: u16,
        session_id: u16,
        sample_rate: u32,
        frame_samples: u16,
        codec: Codec,
        roster: Vec<PlayerInfo>,
    },
    Deny {
        reason: DenyReason,
    },
    Ping {
        /// Sender's transmit time, microseconds on its own clock.
        t1_us: u64,
    },
    Pong {
        t1_us: u64,
        /// Receive and re-transmit times on the responder's clock.
        t2_us: u64,
        t3_us: u64,
    },
    Keepalive,
    Roster {
        players: Vec<PlayerInfo>,
    },
    Bye,
    Ack {
        /// Sequence number and type of the control packet being acknowledged.
        acked_seq: u16,
        acked_type: u8,
    },
}

impl Control {
    pub fn packet_type(&self) -> PacketType {
        match self {
            Control::Hello { .. } => PacketType::Hello,
            Control::Welcome { .. } => PacketType::Welcome,
            Control::Deny { .. } => PacketType::Deny,
            Control::Ping { .. } => PacketType::Ping,
            Control::Pong { .. } => PacketType::Pong,
            Control::Keepalive => PacketType::Keepalive,
            Control::Roster { .. } => PacketType::Roster,
            Control::Bye => PacketType::Bye,
            Control::Ack { .. } => PacketType::Ack,
        }
    }

    /// Serializes header + body into `out`, returning the datagram length.
    pub fn write(&self, header: &Header, out: &mut [u8]) -> usize {
        debug_assert_eq!(header.ptype, self.packet_type());
        let mut w = Writer::new(out);
        w.pos = header.write(w.buf);
        match self {
            Control::Hello {
                proto_version,
                room_code,
                name,
                codec_mask,
                frame_mask,
            } => {
                w.u8(*proto_version);
                w.bytes(room_code);
                w.string(name);
                w.u8(*codec_mask);
                w.u8(*frame_mask);
            }
            Control::Welcome {
                assigned_id,
                session_id,
                sample_rate,
                frame_samples,
                codec,
                roster,
            } => {
                w.u16(*assigned_id);
                w.u16(*session_id);
                w.u32(*sample_rate);
                w.u16(*frame_samples);
                w.u8(*codec as u8);
                w.players(roster);
            }
            Control::Deny { reason } => w.u8(*reason as u8),
            Control::Ping { t1_us } => w.u64(*t1_us),
            Control::Pong {
                t1_us,
                t2_us,
                t3_us,
            } => {
                w.u64(*t1_us);
                w.u64(*t2_us);
                w.u64(*t3_us);
            }
            Control::Keepalive | Control::Bye => {}
            Control::Roster { players } => w.players(players),
            Control::Ack {
                acked_seq,
                acked_type,
            } => {
                w.u16(*acked_seq);
                w.u8(*acked_type);
            }
        }
        w.pos
    }

    /// Parses the body that follows a control-type header.
    pub fn parse(ptype: PacketType, body: &[u8]) -> Result<Control, ParseError> {
        let mut r = Reader::new(body);
        let control = match ptype {
            PacketType::Audio => return Err(ParseError::BadType(ptype as u8)),
            PacketType::Hello => {
                let proto_version = r.u8()?;
                let mut room_code = [0u8; ROOM_CODE_LEN];
                room_code.copy_from_slice(r.bytes(ROOM_CODE_LEN)?);
                let name = r.string()?;
                let codec_mask = r.u8()?;
                let frame_mask = r.u8()?;
                Control::Hello {
                    proto_version,
                    room_code,
                    name,
                    codec_mask,
                    frame_mask,
                }
            }
            PacketType::Welcome => Control::Welcome {
                assigned_id: r.u16()?,
                session_id: r.u16()?,
                sample_rate: r.u32()?,
                frame_samples: r.u16()?,
                codec: Codec::try_from(r.u8()?)?,
                roster: r.players()?,
            },
            PacketType::Deny => Control::Deny {
                reason: DenyReason::try_from(r.u8()?)?,
            },
            PacketType::Ping => Control::Ping { t1_us: r.u64()? },
            PacketType::Pong => Control::Pong {
                t1_us: r.u64()?,
                t2_us: r.u64()?,
                t3_us: r.u64()?,
            },
            PacketType::Keepalive => Control::Keepalive,
            PacketType::Roster => Control::Roster {
                players: r.players()?,
            },
            PacketType::Bye => Control::Bye,
            PacketType::Ack => Control::Ack {
                acked_seq: r.u16()?,
                acked_type: r.u8()?,
            },
        };
        r.finish()?;
        Ok(control)
    }
}

/// A fully parsed datagram.
#[derive(Debug, Clone, PartialEq)]
pub enum Payload<'a> {
    Audio(AudioPacket<'a>),
    Control(Control),
}

/// Parses a whole datagram. This is the single entry point the receive
/// thread uses; it is fuzzed and must never panic on arbitrary input.
pub fn parse_datagram(datagram: &[u8]) -> Result<(Header, Payload<'_>), ParseError> {
    let (header, body) = Header::parse(datagram)?;
    let payload = match header.ptype {
        PacketType::Audio => Payload::Audio(AudioPacket::parse(body)?),
        ptype => Payload::Control(Control::parse(ptype, body)?),
    };
    Ok((header, payload))
}

/// Key identifying a reliable control packet awaiting acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AckKey {
    pub seq: u16,
    pub ptype: u8,
}

/// Whether this control type is sent reliably. HELLO retransmits until its
/// implicit ack (WELCOME/DENY) arrives; WELCOME retransmits until ACKed.
/// Everything else is best-effort: a lost ROSTER only staleness the name
/// list until the next join/leave, and BYE is repeated blindly on shutdown.
pub fn is_reliable(ptype: PacketType) -> bool {
    matches!(ptype, PacketType::Hello | PacketType::Welcome)
}

/// Reassembly of stale-sequence filtering used by receivers: accept a control
/// packet only if its seq is newer than the last one seen from that sender
/// (per type), tolerating the first packet.
pub fn accept_control_seq(last: &mut Option<u16>, incoming: u16) -> bool {
    match *last {
        None => {
            *last = Some(incoming);
            true
        }
        Some(prev) => {
            if seq::newer_than(incoming, prev) {
                *last = Some(incoming);
                true
            } else {
                false
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Cursor helpers
// ---------------------------------------------------------------------------

struct Writer<'a> {
    buf: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    fn new(buf: &'a mut [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn u8(&mut self, v: u8) {
        self.buf[self.pos] = v;
        self.pos += 1;
    }
    fn u16(&mut self, v: u16) {
        self.buf[self.pos..self.pos + 2].copy_from_slice(&v.to_be_bytes());
        self.pos += 2;
    }
    fn u32(&mut self, v: u32) {
        self.buf[self.pos..self.pos + 4].copy_from_slice(&v.to_be_bytes());
        self.pos += 4;
    }
    fn u64(&mut self, v: u64) {
        self.buf[self.pos..self.pos + 8].copy_from_slice(&v.to_be_bytes());
        self.pos += 8;
    }
    fn bytes(&mut self, v: &[u8]) {
        self.buf[self.pos..self.pos + v.len()].copy_from_slice(v);
        self.pos += v.len();
    }
    fn string(&mut self, s: &str) {
        let bytes = s.as_bytes();
        let n = bytes.len().min(MAX_NAME_LEN);
        // Truncate on a char boundary so the reader always gets valid UTF-8.
        let mut n = n;
        while n > 0 && !s.is_char_boundary(n) {
            n -= 1;
        }
        self.u8(n as u8);
        self.bytes(&bytes[..n]);
    }
    fn players(&mut self, players: &[PlayerInfo]) {
        self.u8(players.len() as u8);
        for p in players {
            self.u16(p.id);
            self.string(&p.name);
        }
    }
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], ParseError> {
        if self.buf.len() - self.pos < n {
            return Err(ParseError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, ParseError> {
        Ok(self.bytes(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ParseError> {
        let b = self.bytes(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, ParseError> {
        let b = self.bytes(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn u64(&mut self) -> Result<u64, ParseError> {
        let b = self.bytes(8)?;
        Ok(u64::from_be_bytes(b.try_into().unwrap()))
    }
    fn string(&mut self) -> Result<String, ParseError> {
        let n = self.u8()? as usize;
        if n > MAX_NAME_LEN {
            return Err(ParseError::OutOfRange);
        }
        let bytes = self.bytes(n)?;
        std::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ParseError::BadString)
    }
    fn players(&mut self) -> Result<Vec<PlayerInfo>, ParseError> {
        let n = self.u8()? as usize;
        if n > crate::MAX_CLIENTS + 1 {
            return Err(ParseError::OutOfRange);
        }
        let mut players = Vec::with_capacity(n);
        for _ in 0..n {
            let id = self.u16()?;
            let name = self.string()?;
            players.push(PlayerInfo { id, name });
        }
        Ok(players)
    }
    fn finish(self) -> Result<(), ParseError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(ParseError::TrailingBytes)
        }
    }
}
