//! Control-plane threads: handshakes, keepalives, ping/RTT, liveness,
//! roster, and stats aggregation. Nothing here is real-time; allocation and
//! locking are fine.

use crate::net::{AddrUpdate, CtrlEvent};
use crate::stats::{estimate_latency_ms, PlayerRow, Role, SharedSnapshot};
use crate::{ClientShared, Clock, EngineParams, HostShared};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use jam_protocol::handshake::{
    ClientAction, ClientHandshake, HostAction, HostHandshake, JoinFailure, SessionParams,
};
use jam_protocol::packet::{Control, Header, PlayerInfo};
use jam_protocol::seq::SeqGen;
use jam_protocol::{
    HOST_SENDER_ID, KEEPALIVE_MS, MAX_CLIENTS, MAX_DATAGRAM, PEER_TIMEOUT_MS, REJOIN_WINDOW_MS,
    ROOM_CODE_LEN,
};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

const TICK_MS: u64 = 50;
const PING_INTERVAL_MS: u64 = 1_000;
const SNAPSHOT_INTERVAL_MS: u64 = 500;
/// EWMA weight for new RTT samples.
const RTT_ALPHA: f32 = 0.2;

struct ControlIo {
    socket: UdpSocket,
    seq: SeqGen,
    buf: [u8; MAX_DATAGRAM],
}

impl ControlIo {
    fn new(socket: UdpSocket) -> Self {
        Self {
            socket,
            seq: SeqGen::new(),
            buf: [0u8; MAX_DATAGRAM],
        }
    }

    fn send(&mut self, control: &Control, session_id: u16, sender_id: u16, to: SocketAddr) -> u16 {
        let seq = self.seq.next();
        let header = Header {
            ptype: control.packet_type(),
            session_id,
            sender_id,
            seq,
        };
        let n = control.write(&header, &mut self.buf);
        let _ = self.socket.send_to(&self.buf[..n], to);
        seq
    }
}

fn update_rtt(atomic: &std::sync::atomic::AtomicU32, sample_us: u64) {
    let old = atomic.load(Ordering::Relaxed);
    let new = if old == 0 {
        sample_us as f32
    } else {
        old as f32 * (1.0 - RTT_ALPHA) + sample_us as f32 * RTT_ALPHA
    };
    atomic.store(new as u32, Ordering::Relaxed);
}

fn rtt_from_pong(rx_us: u64, t1_us: u64, t2_us: u64, t3_us: u64) -> u64 {
    (rx_us.saturating_sub(t1_us)).saturating_sub(t3_us.saturating_sub(t2_us))
}

// ---------------------------------------------------------------------------
// Host control loop
// ---------------------------------------------------------------------------

pub struct HostControlConfig {
    pub room_code: [u8; ROOM_CODE_LEN],
    pub host_name: String,
    pub params: EngineParams,
    pub hw_buffer: u32,
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_host_control(
    socket: UdpSocket,
    shared: Arc<HostShared>,
    cfg: HostControlConfig,
    ctrl_rx: Receiver<CtrlEvent>,
    addr_to_rx: Sender<AddrUpdate>,
    addr_to_tx: Sender<AddrUpdate>,
    clock: Clock,
    snapshot: SharedSnapshot,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("jam-control".into())
        .spawn(move || {
            let session_params = SessionParams {
                sample_rate: cfg.params.sample_rate,
                frame_samples: cfg.params.frame_samples as u16,
                codec: cfg.params.codec,
            };
            let mut hs = HostHandshake::new(
                cfg.room_code,
                shared.session_id,
                session_params,
                cfg.host_name.clone(),
            );
            let mut io = ControlIo::new(socket);
            let mut names: [Option<String>; MAX_CLIENTS] = Default::default();
            let mut last_ping = 0u64;
            let mut last_keepalive = 0u64;
            let mut last_snapshot = 0u64;

            let run = |hs: &mut HostHandshake,
                       io: &mut ControlIo,
                       names: &mut [Option<String>; MAX_CLIENTS],
                       shared: &HostShared,
                       addr_to_rx: &Sender<AddrUpdate>,
                       addr_to_tx: &Sender<AddrUpdate>,
                       _clock: &Clock,
                       actions: Vec<HostAction>| {
                for action in actions {
                    match action {
                        HostAction::Send { to, control } => {
                            let is_welcome = matches!(control, Control::Welcome { .. });
                            let seq = io.send(&control, shared.session_id, HOST_SENDER_ID, to);
                            if is_welcome {
                                hs.welcome_sent(to, seq);
                            }
                        }
                        HostAction::ClientJoined { sender_id, name } => {
                            let slot = (sender_id - 1) as usize;
                            let addr = hs.client_addr(sender_id);
                            let stream = &shared.clients[slot];
                            stream.jb.reset();
                            // Deliberately do NOT prime last_rx_us here: the
                            // liveness feed below turns a nonzero last_rx_us
                            // into mark_heard, which would implicitly ack our
                            // own WELCOME and kill its retransmission. The
                            // handshake's own last_heard (set in on_hello)
                            // guards the join timeout until real audio/ACK
                            // arrives from the client.
                            stream.last_rx_us.store(0, Ordering::Release);
                            stream.rtt_us.store(0, Ordering::Relaxed);
                            stream.active.store(true, Ordering::Release);
                            names[slot] = Some(name);
                            let update = AddrUpdate::Client {
                                slot: slot as u8,
                                addr,
                            };
                            let _ = addr_to_rx.send(update);
                            let _ = addr_to_tx.send(update);
                        }
                        HostAction::ClientLeft { sender_id, .. } => {
                            let slot = (sender_id - 1) as usize;
                            let stream = &shared.clients[slot];
                            stream.active.store(false, Ordering::Release);
                            stream.jb.reset();
                            names[slot] = None;
                            let update = AddrUpdate::Client {
                                slot: slot as u8,
                                addr: None,
                            };
                            let _ = addr_to_rx.send(update);
                            let _ = addr_to_tx.send(update);
                        }
                    }
                }
            };

            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    // Best-effort goodbye so clients drop out immediately.
                    for _ in 0..2 {
                        for (_, addr) in hs.client_addrs() {
                            io.send(&Control::Bye, shared.session_id, HOST_SENDER_ID, addr);
                        }
                    }
                    return;
                }
                let event = ctrl_rx.recv_timeout(Duration::from_millis(TICK_MS));
                let now_ms = clock.now_ms();
                match event {
                    Ok(ev) => {
                        if let Some(sender) = hs.sender_for_addr(ev.from) {
                            hs.mark_heard(sender, now_ms);
                        }
                        match &ev.control {
                            Control::Hello { .. } => {
                                let actions = hs.on_hello(ev.from, &ev.control, now_ms);
                                run(
                                    &mut hs,
                                    &mut io,
                                    &mut names,
                                    &shared,
                                    &addr_to_rx,
                                    &addr_to_tx,
                                    &clock,
                                    actions,
                                );
                            }
                            Control::Ack { acked_seq, .. } => {
                                let actions = hs.on_ack(ev.from, *acked_seq, now_ms);
                                run(
                                    &mut hs,
                                    &mut io,
                                    &mut names,
                                    &shared,
                                    &addr_to_rx,
                                    &addr_to_tx,
                                    &clock,
                                    actions,
                                );
                            }
                            Control::Pong {
                                t1_us,
                                t2_us,
                                t3_us,
                            } => {
                                if let Some(sender) = hs.sender_for_addr(ev.from) {
                                    if sender >= 1 {
                                        let rtt = rtt_from_pong(ev.rx_us, *t1_us, *t2_us, *t3_us);
                                        update_rtt(
                                            &shared.clients[(sender - 1) as usize].rtt_us,
                                            rtt,
                                        );
                                    }
                                }
                            }
                            // Ignore a BYE that doesn't carry our session id:
                            // a spoofed one must not drop a client.
                            Control::Bye if ev.header.session_id == shared.session_id => {
                                let actions = hs.on_bye(ev.from);
                                run(
                                    &mut hs,
                                    &mut io,
                                    &mut names,
                                    &shared,
                                    &addr_to_rx,
                                    &addr_to_tx,
                                    &clock,
                                    actions,
                                );
                            }
                            _ => {}
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    // The RX thread dropped its ctrl_tx during shutdown. Say
                    // goodbye so clients tear down immediately instead of
                    // waiting out the 5 s liveness timeout.
                    Err(RecvTimeoutError::Disconnected) => {
                        for (_, addr) in hs.client_addrs() {
                            io.send(&Control::Bye, shared.session_id, HOST_SENDER_ID, addr);
                        }
                        return;
                    }
                }

                // Feed audio-path liveness into the handshake machine.
                for slot in 0..MAX_CLIENTS {
                    let us = shared.clients[slot].last_rx_us.load(Ordering::Acquire);
                    if us > 0 {
                        hs.mark_heard((slot + 1) as u16, us / 1_000);
                    }
                }
                let actions = hs.tick(now_ms);
                run(
                    &mut hs,
                    &mut io,
                    &mut names,
                    &shared,
                    &addr_to_rx,
                    &addr_to_tx,
                    &clock,
                    actions,
                );

                if now_ms.saturating_sub(last_ping) >= PING_INTERVAL_MS {
                    last_ping = now_ms;
                    for (_, addr) in hs.client_addrs() {
                        let ping = Control::Ping {
                            t1_us: clock.now_us(),
                        };
                        io.send(&ping, shared.session_id, HOST_SENDER_ID, addr);
                    }
                }
                if now_ms.saturating_sub(last_keepalive) >= KEEPALIVE_MS {
                    last_keepalive = now_ms;
                    for (_, addr) in hs.client_addrs() {
                        io.send(&Control::Keepalive, shared.session_id, HOST_SENDER_ID, addr);
                    }
                }
                if now_ms.saturating_sub(last_snapshot) >= SNAPSHOT_INTERVAL_MS {
                    last_snapshot = now_ms;
                    build_host_snapshot(&shared, &cfg, &names, &snapshot);
                }
            }
        })
        .expect("spawn host control thread")
}

fn build_host_snapshot(
    shared: &HostShared,
    cfg: &HostControlConfig,
    names: &[Option<String>; MAX_CLIENTS],
    snapshot: &SharedSnapshot,
) {
    let frame_ms = cfg.params.frame_samples as f32 * 1_000.0 / cfg.params.sample_rate as f32;
    let mut players = vec![PlayerRow {
        id: HOST_SENDER_ID,
        name: format!("{} (you)", cfg.host_name),
        active: true,
        rms_db: shared.local_level.rms_db(),
        peak_db: shared.local_level.peak_db(),
        ..Default::default()
    }];
    let mut worst_rtt = 0.0f32;
    let mut worst_buffer = 0.0f32;
    for (slot, name) in names.iter().enumerate() {
        let stream = &shared.clients[slot];
        if !stream.active.load(Ordering::Acquire) {
            continue;
        }
        let rtt_ms = stream.rtt_us.load(Ordering::Relaxed) as f32 / 1_000.0;
        let buffer_ms = stream.jb.depth().max(0) as f32 * frame_ms;
        worst_rtt = worst_rtt.max(rtt_ms);
        worst_buffer = worst_buffer.max(buffer_ms);
        players.push(PlayerRow {
            id: (slot + 1) as u16,
            name: name
                .clone()
                .unwrap_or_else(|| format!("player {}", slot + 1)),
            active: true,
            rms_db: stream.level.rms_db(),
            peak_db: stream.level.peak_db(),
            loss: stream.jb.stats.loss_fraction(),
            jitter_ms: stream.jb.stats.jitter_us() / 1_000.0,
            buffer_ms,
            rtt_ms,
        });
    }
    let mut snap = snapshot.lock().unwrap();
    snap.role = Role::Host;
    snap.connected = true;
    snap.room_code = String::from_utf8_lossy(&cfg.room_code).into_owned();
    snap.status = format!(
        "hosting room {} — {} player(s)",
        snap.room_code,
        players.len()
    );
    snap.local_rms_db = shared.local_level.rms_db();
    snap.local_peak_db = shared.local_level.peak_db();
    snap.capture_starved = shared.capture_starved.load(Ordering::Relaxed);
    snap.est_latency_ms = estimate_latency_ms(
        cfg.hw_buffer,
        cfg.params.frame_samples,
        cfg.params.sample_rate,
        worst_buffer,
        worst_rtt,
    );
    snap.players = players;
}

// ---------------------------------------------------------------------------
// Client control loop
// ---------------------------------------------------------------------------

pub struct ClientControlConfig {
    pub room_code: [u8; ROOM_CODE_LEN],
    pub name: String,
    pub params: EngineParams,
    pub hw_buffer: u32,
    pub host_addr: SocketAddr,
}

pub fn spawn_client_control(
    socket: UdpSocket,
    shared: Arc<ClientShared>,
    cfg: ClientControlConfig,
    ctrl_rx: Receiver<CtrlEvent>,
    clock: Clock,
    snapshot: SharedSnapshot,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("jam-control".into())
        .spawn(move || {
            let mut cs = ClientHandshake::new(cfg.room_code, cfg.name.clone());
            let mut io = ControlIo::new(socket);
            let mut roster: Vec<PlayerInfo> = vec![];
            let mut last_roster_seq: Option<u16> = None;
            let mut last_ping = 0u64;
            let mut last_keepalive = 0u64;
            let mut last_snapshot = 0u64;
            // While `Some`, we keep re-attempting the handshake until the
            // deadline passes.
            let mut rejoin_deadline: Option<u64> = Some(clock.now_ms() + REJOIN_WINDOW_MS);
            let mut terminal_status: Option<String> = None;

            let send_ctrl = |io: &mut ControlIo, shared: &ClientShared, c: &Control| {
                let session = shared.session_id.load(Ordering::Acquire);
                let sender = shared.sender_id.load(Ordering::Acquire);
                io.send(c, session, sender, cfg.host_addr)
            };

            let now0 = clock.now_ms();
            for action in cs.start(now0) {
                if let ClientAction::Send(c) = action {
                    send_ctrl(&mut io, &shared, &c);
                }
            }

            loop {
                if shared.shutdown.load(Ordering::Acquire) {
                    if shared.joined.load(Ordering::Acquire) {
                        for _ in 0..2 {
                            send_ctrl(&mut io, &shared, &Control::Bye);
                        }
                    }
                    return;
                }
                let event = ctrl_rx.recv_timeout(Duration::from_millis(TICK_MS));
                let now_ms = clock.now_ms();
                let mut actions: Vec<ClientAction> = vec![];
                match event {
                    Ok(ev) => match &ev.control {
                        Control::Welcome { .. } => {
                            // Always ack — the host may be retransmitting
                            // because our previous ack was lost.
                            let ack = Control::Ack {
                                acked_seq: ev.header.seq,
                                acked_type: ev.header.ptype as u8,
                            };
                            // Session id must match what the WELCOME carries
                            // so the host accepts the ack.
                            if let Control::Welcome { session_id, .. } = ev.control {
                                let sender = shared.sender_id.load(Ordering::Acquire);
                                io.send(&ack, session_id, sender, cfg.host_addr);
                            }
                            actions = cs.on_control(&ev.control);
                        }
                        Control::Roster { players } => {
                            if jam_protocol::packet::accept_control_seq(
                                &mut last_roster_seq,
                                ev.header.seq,
                            ) {
                                roster = players.clone();
                            }
                        }
                        Control::Deny { .. } => {
                            actions = cs.on_control(&ev.control);
                        }
                        Control::Pong {
                            t1_us,
                            t2_us,
                            t3_us,
                        } => {
                            let rtt = rtt_from_pong(ev.rx_us, *t1_us, *t2_us, *t3_us);
                            update_rtt(&shared.from_host.rtt_us, rtt);
                        }
                        // Only honor a BYE stamped with our session id, so a
                        // spoofed datagram can't kick us off.
                        Control::Bye
                            if ev.header.session_id
                                == shared.session_id.load(Ordering::Acquire) =>
                        {
                            shared.joined.store(false, Ordering::Release);
                            shared.from_host.jb.reset();
                            terminal_status = Some("host ended the session".into());
                            rejoin_deadline = None;
                        }
                        _ => {}
                    },
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        if shared.joined.load(Ordering::Acquire) {
                            send_ctrl(&mut io, &shared, &Control::Bye);
                        }
                        return;
                    }
                }

                actions.extend(cs.tick(now_ms));

                for action in actions {
                    match action {
                        ClientAction::Send(c) => {
                            send_ctrl(&mut io, &shared, &c);
                        }
                        ClientAction::StartAudio {
                            sender_id,
                            session_id,
                            params,
                        } => {
                            // The audio pipeline was built from this client's
                            // own CLI flags before the handshake; we can't
                            // reconfigure it here. If the host negotiated a
                            // different frame size or codec, our encoder/
                            // decoder won't line up and audio would be silent,
                            // so warn loudly instead of failing quietly.
                            if params.frame_samples as usize != cfg.params.frame_samples
                                || params.codec != cfg.params.codec
                            {
                                terminal_status = Some(format!(
                                    "parameter mismatch: host uses {:?} at {} samples/frame — \
                                     restart with matching --codec/--frame",
                                    params.codec, params.frame_samples
                                ));
                            }
                            shared.from_host.jb.reset();
                            shared
                                .from_host
                                .last_rx_us
                                .store(clock.now_us(), Ordering::Release);
                            shared.session_id.store(session_id, Ordering::Release);
                            shared.sender_id.store(sender_id, Ordering::Release);
                            shared.joined.store(true, Ordering::Release);
                            // Fresh join window for any later disconnect.
                            rejoin_deadline = Some(now_ms + REJOIN_WINDOW_MS);
                            // A restarted host resets its control SeqGen, so
                            // its new roster seqs may look "older" than what we
                            // saw from the previous instance. Forget the dedup
                            // watermark on every (re)join.
                            last_roster_seq = None;
                        }
                        ClientAction::RosterUpdate(r) => roster = r,
                        ClientAction::Failed(failure) => {
                            match failure {
                                JoinFailure::Denied(reason) => {
                                    terminal_status = Some(format!("denied: {reason:?}"));
                                    rejoin_deadline = None;
                                }
                                JoinFailure::Timeout => {
                                    // Retry whole handshakes until the rejoin
                                    // window closes.
                                    match rejoin_deadline {
                                        Some(deadline) if now_ms < deadline => {
                                            cs = ClientHandshake::new(
                                                cfg.room_code,
                                                cfg.name.clone(),
                                            );
                                            for a in cs.start(now_ms) {
                                                if let ClientAction::Send(c) = a {
                                                    send_ctrl(&mut io, &shared, &c);
                                                }
                                            }
                                        }
                                        _ => {
                                            terminal_status =
                                                Some("could not reach host (timed out)".into());
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Liveness: if the host went quiet, drop to rejoining.
                if shared.joined.load(Ordering::Acquire) {
                    let last = shared.from_host.last_rx_us.load(Ordering::Acquire);
                    if clock.now_us().saturating_sub(last) > PEER_TIMEOUT_MS * 1_000 {
                        shared.joined.store(false, Ordering::Release);
                        shared.from_host.jb.reset();
                        rejoin_deadline = Some(now_ms + REJOIN_WINDOW_MS);
                        cs = ClientHandshake::new(cfg.room_code, cfg.name.clone());
                        for a in cs.start(now_ms) {
                            if let ClientAction::Send(c) = a {
                                send_ctrl(&mut io, &shared, &c);
                            }
                        }
                    }
                }

                let joined = shared.joined.load(Ordering::Acquire);
                if joined && now_ms.saturating_sub(last_ping) >= PING_INTERVAL_MS {
                    last_ping = now_ms;
                    let ping = Control::Ping {
                        t1_us: clock.now_us(),
                    };
                    send_ctrl(&mut io, &shared, &ping);
                }
                if joined && now_ms.saturating_sub(last_keepalive) >= KEEPALIVE_MS {
                    last_keepalive = now_ms;
                    send_ctrl(&mut io, &shared, &Control::Keepalive);
                }
                if now_ms.saturating_sub(last_snapshot) >= SNAPSHOT_INTERVAL_MS {
                    last_snapshot = now_ms;
                    build_client_snapshot(
                        &shared,
                        &cfg,
                        &roster,
                        terminal_status.as_deref(),
                        &snapshot,
                    );
                }
            }
        })
        .expect("spawn client control thread")
}

fn build_client_snapshot(
    shared: &ClientShared,
    cfg: &ClientControlConfig,
    roster: &[PlayerInfo],
    terminal_status: Option<&str>,
    snapshot: &SharedSnapshot,
) {
    let joined = shared.joined.load(Ordering::Acquire);
    let frame_ms = cfg.params.frame_samples as f32 * 1_000.0 / cfg.params.sample_rate as f32;
    let own_id = shared.sender_id.load(Ordering::Acquire);
    let stream = &shared.from_host;
    let rtt_ms = stream.rtt_us.load(Ordering::Relaxed) as f32 / 1_000.0;
    let buffer_ms = stream.jb.depth().max(0) as f32 * frame_ms;

    let mut players: Vec<PlayerRow> = vec![];
    for p in roster {
        let is_me = p.id == own_id;
        let mut row = PlayerRow {
            id: p.id,
            name: if is_me {
                format!("{} (you)", p.name)
            } else {
                p.name.clone()
            },
            active: true,
            ..Default::default()
        };
        if p.id == HOST_SENDER_ID {
            // The host row doubles as "the mix": that's the one stream we
            // actually receive and measure.
            row.rms_db = stream.level.rms_db();
            row.peak_db = stream.level.peak_db();
            row.loss = stream.jb.stats.loss_fraction();
            row.jitter_ms = stream.jb.stats.jitter_us() / 1_000.0;
            row.buffer_ms = buffer_ms;
            row.rtt_ms = rtt_ms;
        }
        if is_me {
            row.rms_db = shared.local_level.rms_db();
            row.peak_db = shared.local_level.peak_db();
        }
        players.push(row);
    }

    let mut snap = snapshot.lock().unwrap();
    snap.role = Role::Client;
    snap.connected = joined;
    snap.room_code = String::from_utf8_lossy(&cfg.room_code).into_owned();
    snap.status = if let Some(t) = terminal_status {
        t.to_string()
    } else if joined {
        format!("connected to {} — room {}", cfg.host_addr, snap.room_code)
    } else {
        format!("joining {} ...", cfg.host_addr)
    };
    snap.local_rms_db = shared.local_level.rms_db();
    snap.local_peak_db = shared.local_level.peak_db();
    snap.capture_starved = shared.capture_starved.load(Ordering::Relaxed);
    snap.est_latency_ms = estimate_latency_ms(
        cfg.hw_buffer,
        cfg.params.frame_samples,
        cfg.params.sample_rate,
        buffer_ms,
        rtt_ms,
    );
    snap.players = players;
}
