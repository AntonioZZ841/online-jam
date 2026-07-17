//! Session join/leave state machines for both sides of the handshake.
//!
//! Pure logic: no sockets, no timers. The embedding code feeds in parsed
//! control packets and clock ticks (`now_ms` from any monotonic source) and
//! executes the returned actions (send a packet, start audio, drop a slot).
//!
//! Reliability model:
//! - `HELLO` is retransmitted by the client until it gets `WELCOME`/`DENY`
//!   (those replies are its implicit ack).
//! - `WELCOME` is retransmitted by the host until the client acks it — either
//!   with an `ACK` packet or implicitly by audio arriving from that sender.
//! - `ROSTER`/`BYE` acks are handled generically by the control loop in
//!   jam-core (every reliable control gets an `ACK` reply); the machines here
//!   only decide *what* to send and *when* to give up.

use crate::packet::{Codec, Control, DenyReason, PlayerInfo};
use crate::{
    CONTROL_MAX_TRIES, CONTROL_RETRY_MS, MAX_CLIENTS, PEER_TIMEOUT_MS, PROTOCOL_VERSION,
    ROOM_CODE_LEN,
};
use std::net::SocketAddr;

/// Audio parameters fixed by the host for the whole session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionParams {
    pub sample_rate: u32,
    pub frame_samples: u16,
    pub codec: Codec,
}

impl Default for SessionParams {
    fn default() -> Self {
        Self {
            sample_rate: 48_000,
            frame_samples: 120,
            codec: Codec::Opus,
        }
    }
}

// ---------------------------------------------------------------------------
// Client side
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JoinFailure {
    Denied(DenyReason),
    Timeout,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientPhase {
    Idle,
    Joining {
        last_tx_ms: u64,
        tries: u32,
    },
    Joined {
        sender_id: u16,
        session_id: u16,
        params: SessionParams,
    },
    Failed(JoinFailure),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ClientAction {
    /// Send this control packet to the host.
    Send(Control),
    /// Handshake completed: bring up the audio path.
    StartAudio {
        sender_id: u16,
        session_id: u16,
        params: SessionParams,
    },
    /// Roster changed (initial roster arrives inside WELCOME).
    RosterUpdate(Vec<PlayerInfo>),
    /// Handshake failed permanently; tear down.
    Failed(JoinFailure),
}

#[derive(Debug)]
pub struct ClientHandshake {
    phase: ClientPhase,
    hello: Control,
}

impl ClientHandshake {
    pub fn new(room_code: [u8; ROOM_CODE_LEN], name: String) -> Self {
        Self {
            phase: ClientPhase::Idle,
            hello: Control::Hello {
                proto_version: PROTOCOL_VERSION,
                room_code,
                name,
                codec_mask: 0b0000_0111, // opus + both pcm variants
                frame_mask: 0b0000_0011, // 2.5 ms + 5 ms
            },
        }
    }

    pub fn phase(&self) -> &ClientPhase {
        &self.phase
    }

    /// Kick off the join. Returns the first HELLO to send.
    pub fn start(&mut self, now_ms: u64) -> Vec<ClientAction> {
        self.phase = ClientPhase::Joining {
            last_tx_ms: now_ms,
            tries: 1,
        };
        vec![ClientAction::Send(self.hello.clone())]
    }

    /// Clock tick; drives HELLO retransmission and the give-up timeout.
    pub fn tick(&mut self, now_ms: u64) -> Vec<ClientAction> {
        let ClientPhase::Joining { last_tx_ms, tries } = &mut self.phase else {
            return vec![];
        };
        if now_ms.saturating_sub(*last_tx_ms) < CONTROL_RETRY_MS {
            return vec![];
        }
        if *tries >= CONTROL_MAX_TRIES {
            self.phase = ClientPhase::Failed(JoinFailure::Timeout);
            return vec![ClientAction::Failed(JoinFailure::Timeout)];
        }
        *last_tx_ms = now_ms;
        *tries += 1;
        vec![ClientAction::Send(self.hello.clone())]
    }

    /// A control packet arrived from the host.
    pub fn on_control(&mut self, control: &Control) -> Vec<ClientAction> {
        match (&self.phase, control) {
            (
                ClientPhase::Joining { .. },
                Control::Welcome {
                    assigned_id,
                    session_id,
                    sample_rate,
                    frame_samples,
                    codec,
                    roster,
                },
            ) => {
                let params = SessionParams {
                    sample_rate: *sample_rate,
                    frame_samples: *frame_samples,
                    codec: *codec,
                };
                self.phase = ClientPhase::Joined {
                    sender_id: *assigned_id,
                    session_id: *session_id,
                    params,
                };
                vec![
                    ClientAction::StartAudio {
                        sender_id: *assigned_id,
                        session_id: *session_id,
                        params,
                    },
                    ClientAction::RosterUpdate(roster.clone()),
                ]
            }
            (ClientPhase::Joining { .. }, Control::Deny { reason }) => {
                self.phase = ClientPhase::Failed(JoinFailure::Denied(*reason));
                vec![ClientAction::Failed(JoinFailure::Denied(*reason))]
            }
            (ClientPhase::Joined { .. }, Control::Roster { players }) => {
                vec![ClientAction::RosterUpdate(players.clone())]
            }
            _ => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Host side
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LeaveReason {
    Bye,
    Timeout,
    WelcomeUnacked,
}

#[derive(Debug, Clone, PartialEq)]
enum SlotState {
    /// WELCOME sent, awaiting ack (explicit ACK or first audio).
    Pending {
        welcome_seq: Option<u16>,
        last_tx_ms: u64,
        tries: u32,
    },
    Active,
}

#[derive(Debug, Clone)]
struct Slot {
    addr: SocketAddr,
    name: String,
    state: SlotState,
    last_heard_ms: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HostAction {
    /// Send this control to one address. `needs_seq_capture` is true for
    /// WELCOME: the embedder must report the header seq it stamped back via
    /// `welcome_sent` so acks can be matched.
    Send { to: SocketAddr, control: Control },
    /// A client finished the handshake (audio slots may now activate).
    ClientJoined { sender_id: u16, name: String },
    /// A client left; free its audio slot.
    ClientLeft { sender_id: u16, reason: LeaveReason },
}

#[derive(Debug)]
pub struct HostHandshake {
    room_code: [u8; ROOM_CODE_LEN],
    session_id: u16,
    params: SessionParams,
    host_name: String,
    slots: [Option<Slot>; MAX_CLIENTS],
}

impl HostHandshake {
    pub fn new(
        room_code: [u8; ROOM_CODE_LEN],
        session_id: u16,
        params: SessionParams,
        host_name: String,
    ) -> Self {
        Self {
            room_code,
            session_id,
            params,
            host_name,
            slots: std::array::from_fn(|_| None),
        }
    }

    pub fn session_id(&self) -> u16 {
        self.session_id
    }

    /// sender_id for slot index (slot 0 -> sender 1; the host itself is 0).
    fn sender_id(slot: usize) -> u16 {
        (slot + 1) as u16
    }

    fn slot_of_sender(sender_id: u16) -> Option<usize> {
        (1..=MAX_CLIENTS as u16)
            .contains(&sender_id)
            .then(|| (sender_id - 1) as usize)
    }

    /// Full roster including the host, for WELCOME/ROSTER packets.
    pub fn roster(&self) -> Vec<PlayerInfo> {
        let mut players = vec![PlayerInfo {
            id: crate::HOST_SENDER_ID,
            name: self.host_name.clone(),
        }];
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(s) = slot {
                players.push(PlayerInfo {
                    id: Self::sender_id(i),
                    name: s.name.clone(),
                });
            }
        }
        players
    }

    pub fn client_addr(&self, sender_id: u16) -> Option<SocketAddr> {
        let slot = Self::slot_of_sender(sender_id)?;
        self.slots[slot].as_ref().map(|s| s.addr)
    }

    /// Addresses of all connected (pending or active) clients.
    pub fn client_addrs(&self) -> Vec<(u16, SocketAddr)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, s)| s.as_ref().map(|s| (Self::sender_id(i), s.addr)))
            .collect()
    }

    /// The sender id registered for a source address, if any. The receive
    /// thread uses this mapping (mirrored into its own table) to reject
    /// spoofed sender ids.
    pub fn sender_for_addr(&self, addr: SocketAddr) -> Option<u16> {
        self.slots.iter().enumerate().find_map(|(i, s)| {
            s.as_ref()
                .filter(|s| s.addr == addr)
                .map(|_| Self::sender_id(i))
        })
    }

    fn welcome_for(&self, slot: usize) -> Control {
        Control::Welcome {
            assigned_id: Self::sender_id(slot),
            session_id: self.session_id,
            sample_rate: self.params.sample_rate,
            frame_samples: self.params.frame_samples,
            codec: self.params.codec,
            roster: self.roster(),
        }
    }

    fn broadcast_roster(&self, except: Option<SocketAddr>) -> Vec<HostAction> {
        let roster = self.roster();
        self.slots
            .iter()
            .flatten()
            .filter(|s| Some(s.addr) != except)
            .map(|s| HostAction::Send {
                to: s.addr,
                control: Control::Roster {
                    players: roster.clone(),
                },
            })
            .collect()
    }

    /// HELLO received from `addr`.
    pub fn on_hello(&mut self, addr: SocketAddr, hello: &Control, now_ms: u64) -> Vec<HostAction> {
        let Control::Hello {
            proto_version,
            room_code,
            name,
            ..
        } = hello
        else {
            return vec![];
        };
        if *proto_version != PROTOCOL_VERSION {
            return vec![HostAction::Send {
                to: addr,
                control: Control::Deny {
                    reason: DenyReason::VersionMismatch,
                },
            }];
        }
        if *room_code != self.room_code {
            return vec![HostAction::Send {
                to: addr,
                control: Control::Deny {
                    reason: DenyReason::BadRoomCode,
                },
            }];
        }
        // Same address again: a HELLO retry (our WELCOME may be in flight or
        // lost) or a client restart. Reset to Pending and resend WELCOME —
        // idempotent from the client's point of view.
        if let Some(slot) = self
            .slots
            .iter()
            .position(|s| s.as_ref().map(|s| s.addr == addr).unwrap_or(false))
        {
            let s = self.slots[slot].as_mut().unwrap();
            s.name = name.clone();
            s.last_heard_ms = now_ms;
            s.state = SlotState::Pending {
                welcome_seq: None,
                last_tx_ms: now_ms,
                tries: 1,
            };
            return vec![HostAction::Send {
                to: addr,
                control: self.welcome_for(slot),
            }];
        }
        let Some(slot) = self.slots.iter().position(Option::is_none) else {
            return vec![HostAction::Send {
                to: addr,
                control: Control::Deny {
                    reason: DenyReason::RoomFull,
                },
            }];
        };
        self.slots[slot] = Some(Slot {
            addr,
            name: name.clone(),
            state: SlotState::Pending {
                welcome_seq: None,
                last_tx_ms: now_ms,
                tries: 1,
            },
            last_heard_ms: now_ms,
        });
        let mut actions = vec![
            HostAction::Send {
                to: addr,
                control: self.welcome_for(slot),
            },
            HostAction::ClientJoined {
                sender_id: Self::sender_id(slot),
                name: name.clone(),
            },
        ];
        actions.extend(self.broadcast_roster(Some(addr)));
        actions
    }

    /// The embedder stamped a header seq on an outgoing WELCOME; remember it
    /// so the client's ACK can be matched.
    pub fn welcome_sent(&mut self, to: SocketAddr, seq: u16) {
        for slot in self.slots.iter_mut().flatten() {
            if slot.addr == to {
                if let SlotState::Pending { welcome_seq, .. } = &mut slot.state {
                    *welcome_seq = Some(seq);
                }
            }
        }
    }

    /// ACK received from a client address.
    pub fn on_ack(&mut self, addr: SocketAddr, acked_seq: u16, now_ms: u64) -> Vec<HostAction> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if let Some(s) = slot {
                if s.addr == addr {
                    s.last_heard_ms = now_ms;
                    if let SlotState::Pending {
                        welcome_seq: Some(ws),
                        ..
                    } = &s.state
                    {
                        if *ws == acked_seq {
                            s.state = SlotState::Active;
                            let _ = i;
                        }
                    }
                }
            }
        }
        vec![]
    }

    /// Any packet (audio included) arrived from this sender: refresh liveness
    /// and treat as implicit WELCOME ack.
    pub fn mark_heard(&mut self, sender_id: u16, now_ms: u64) {
        if let Some(slot) = Self::slot_of_sender(sender_id) {
            if let Some(s) = self.slots[slot].as_mut() {
                s.last_heard_ms = now_ms;
                if matches!(s.state, SlotState::Pending { .. }) {
                    s.state = SlotState::Active;
                }
            }
        }
    }

    /// BYE received.
    pub fn on_bye(&mut self, addr: SocketAddr) -> Vec<HostAction> {
        self.remove_where(|s| s.addr == addr, LeaveReason::Bye)
    }

    /// Clock tick: WELCOME retransmits and silence timeouts.
    pub fn tick(&mut self, now_ms: u64) -> Vec<HostAction> {
        let mut actions = vec![];
        let mut resend = vec![];
        let mut dropped = false;
        for (i, entry) in self.slots.iter_mut().enumerate() {
            let Some(s) = entry else { continue };
            if now_ms.saturating_sub(s.last_heard_ms) > PEER_TIMEOUT_MS {
                actions.push(HostAction::ClientLeft {
                    sender_id: Self::sender_id(i),
                    reason: LeaveReason::Timeout,
                });
                *entry = None;
                dropped = true;
                continue;
            }
            if let SlotState::Pending {
                last_tx_ms, tries, ..
            } = &mut s.state
            {
                if now_ms.saturating_sub(*last_tx_ms) >= CONTROL_RETRY_MS {
                    if *tries >= CONTROL_MAX_TRIES {
                        actions.push(HostAction::ClientLeft {
                            sender_id: Self::sender_id(i),
                            reason: LeaveReason::WelcomeUnacked,
                        });
                        *entry = None;
                        dropped = true;
                    } else {
                        *last_tx_ms = now_ms;
                        *tries += 1;
                        resend.push(i);
                    }
                }
            }
        }
        for slot in resend {
            if let Some(s) = &self.slots[slot] {
                actions.push(HostAction::Send {
                    to: s.addr,
                    control: self.welcome_for(slot),
                });
            }
        }
        if dropped {
            actions.extend(self.broadcast_roster(None));
        }
        actions
    }

    fn remove_where(
        &mut self,
        pred: impl Fn(&Slot) -> bool,
        reason: LeaveReason,
    ) -> Vec<HostAction> {
        let mut actions = vec![];
        let mut removed = false;
        for (i, entry) in self.slots.iter_mut().enumerate() {
            if entry.as_ref().map(&pred).unwrap_or(false) {
                actions.push(HostAction::ClientLeft {
                    sender_id: Self::sender_id(i),
                    reason: reason.clone(),
                });
                *entry = None;
                removed = true;
            }
        }
        if removed {
            actions.extend(self.broadcast_roster(None));
        }
        actions
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(n: u8) -> SocketAddr {
        format!("10.0.0.{n}:5000").parse().unwrap()
    }

    fn hello(name: &str) -> Control {
        Control::Hello {
            proto_version: PROTOCOL_VERSION,
            room_code: *b"ABC123",
            name: name.into(),
            codec_mask: 1,
            frame_mask: 1,
        }
    }

    fn host() -> HostHandshake {
        HostHandshake::new(*b"ABC123", 42, SessionParams::default(), "host".into())
    }

    #[test]
    fn client_full_join() {
        let mut c = ClientHandshake::new(*b"ABC123", "al".into());
        let a = c.start(0);
        assert!(matches!(a[0], ClientAction::Send(Control::Hello { .. })));
        // Two retries at the retransmit interval.
        assert_eq!(c.tick(CONTROL_RETRY_MS).len(), 1);
        assert_eq!(c.tick(CONTROL_RETRY_MS + 10).len(), 0); // too soon
        let welcome = Control::Welcome {
            assigned_id: 2,
            session_id: 42,
            sample_rate: 48_000,
            frame_samples: 120,
            codec: Codec::Opus,
            roster: vec![],
        };
        let a = c.on_control(&welcome);
        assert!(matches!(
            a[0],
            ClientAction::StartAudio {
                sender_id: 2,
                session_id: 42,
                ..
            }
        ));
        assert!(matches!(c.phase(), ClientPhase::Joined { .. }));
        // Duplicate WELCOME after joining is ignored.
        assert!(c.on_control(&welcome).is_empty());
    }

    #[test]
    fn client_gives_up_after_max_tries() {
        let mut c = ClientHandshake::new(*b"ABC123", "al".into());
        c.start(0);
        let mut now = 0;
        let mut failed = false;
        for _ in 0..CONTROL_MAX_TRIES + 1 {
            now += CONTROL_RETRY_MS;
            for a in c.tick(now) {
                if matches!(a, ClientAction::Failed(JoinFailure::Timeout)) {
                    failed = true;
                }
            }
        }
        assert!(failed);
    }

    #[test]
    fn client_denied() {
        let mut c = ClientHandshake::new(*b"ABC123", "al".into());
        c.start(0);
        let a = c.on_control(&Control::Deny {
            reason: DenyReason::BadRoomCode,
        });
        assert!(matches!(
            a[0],
            ClientAction::Failed(JoinFailure::Denied(DenyReason::BadRoomCode))
        ));
    }

    #[test]
    fn host_assigns_slots_and_denies_when_full() {
        let mut h = host();
        for n in 1..=MAX_CLIENTS as u8 {
            let a = h.on_hello(addr(n), &hello(&format!("p{n}")), 0);
            assert!(a.iter().any(|a| matches!(
                a,
                HostAction::ClientJoined { sender_id, .. } if *sender_id == n as u16
            )));
        }
        let a = h.on_hello(addr(9), &hello("late"), 0);
        assert!(matches!(
            a[0],
            HostAction::Send {
                control: Control::Deny {
                    reason: DenyReason::RoomFull
                },
                ..
            }
        ));
    }

    #[test]
    fn host_rejects_bad_code_and_version() {
        let mut h = host();
        let a = h.on_hello(
            addr(1),
            &Control::Hello {
                proto_version: PROTOCOL_VERSION,
                room_code: *b"WRONG!",
                name: "x".into(),
                codec_mask: 1,
                frame_mask: 1,
            },
            0,
        );
        assert!(matches!(
            a[0],
            HostAction::Send {
                control: Control::Deny {
                    reason: DenyReason::BadRoomCode
                },
                ..
            }
        ));
        let a = h.on_hello(
            addr(1),
            &Control::Hello {
                proto_version: PROTOCOL_VERSION + 1,
                room_code: *b"ABC123",
                name: "x".into(),
                codec_mask: 1,
                frame_mask: 1,
            },
            0,
        );
        assert!(matches!(
            a[0],
            HostAction::Send {
                control: Control::Deny {
                    reason: DenyReason::VersionMismatch
                },
                ..
            }
        ));
    }

    #[test]
    fn duplicate_hello_is_idempotent() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        let a = h.on_hello(addr(1), &hello("al"), 50);
        // Resends WELCOME to the same slot, no second ClientJoined.
        assert_eq!(
            a.iter()
                .filter(|a| matches!(a, HostAction::ClientJoined { .. }))
                .count(),
            0
        );
        assert!(matches!(
            a[0],
            HostAction::Send {
                control: Control::Welcome { assigned_id: 1, .. },
                ..
            }
        ));
    }

    #[test]
    fn welcome_retransmits_then_gives_up() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        let mut now = 0;
        let mut resends = 0;
        let mut left = false;
        for _ in 0..CONTROL_MAX_TRIES + 2 {
            now += CONTROL_RETRY_MS;
            for a in h.tick(now) {
                match a {
                    HostAction::Send {
                        control: Control::Welcome { .. },
                        ..
                    } => resends += 1,
                    HostAction::ClientLeft {
                        reason: LeaveReason::WelcomeUnacked,
                        ..
                    } => left = true,
                    _ => {}
                }
            }
        }
        assert_eq!(resends, CONTROL_MAX_TRIES - 1);
        assert!(left);
    }

    #[test]
    fn ack_activates_pending_client() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        h.welcome_sent(addr(1), 7);
        h.on_ack(addr(1), 7, 10);
        // No WELCOME retransmit after activation.
        assert!(h.tick(CONTROL_RETRY_MS + 20).iter().all(|a| !matches!(
            a,
            HostAction::Send {
                control: Control::Welcome { .. },
                ..
            }
        )));
    }

    #[test]
    fn audio_implicitly_acks_welcome() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        h.mark_heard(1, 10);
        assert!(h.tick(CONTROL_RETRY_MS + 20).iter().all(|a| !matches!(
            a,
            HostAction::Send {
                control: Control::Welcome { .. },
                ..
            }
        )));
    }

    #[test]
    fn silent_client_times_out_and_roster_updates() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        h.on_hello(addr(2), &hello("bo"), 0);
        h.mark_heard(1, 0);
        h.mark_heard(2, 6_000); // bo stays alive
        let actions = h.tick(PEER_TIMEOUT_MS + 1_000);
        assert!(actions.iter().any(|a| matches!(
            a,
            HostAction::ClientLeft {
                sender_id: 1,
                reason: LeaveReason::Timeout
            }
        )));
        // Survivor gets a fresh roster without the departed player.
        let roster_sent = actions.iter().find_map(|a| match a {
            HostAction::Send {
                to,
                control: Control::Roster { players },
            } if *to == addr(2) => Some(players.clone()),
            _ => None,
        });
        let players = roster_sent.expect("roster broadcast to survivor");
        assert!(players.iter().all(|p| p.id != 1));
        assert!(players.iter().any(|p| p.id == 2));
    }

    #[test]
    fn bye_frees_slot_for_reuse() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        h.on_bye(addr(1));
        let a = h.on_hello(addr(3), &hello("cy"), 100);
        assert!(a
            .iter()
            .any(|a| matches!(a, HostAction::ClientJoined { sender_id: 1, .. })));
    }

    #[test]
    fn roster_includes_host_and_clients() {
        let mut h = host();
        h.on_hello(addr(1), &hello("al"), 0);
        let roster = h.roster();
        assert_eq!(roster[0].id, crate::HOST_SENDER_ID);
        assert_eq!(roster[0].name, "host");
        assert_eq!(roster[1].id, 1);
        assert_eq!(roster[1].name, "al");
    }
}
