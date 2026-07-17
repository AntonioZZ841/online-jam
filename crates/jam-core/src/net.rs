//! Network receive/transmit threads. One UDP socket per process; the RX
//! thread owns `recv_from`, the TX thread drains a lock-free ring filled by
//! the audio callback, and the control thread sends its own (tiny) traffic
//! through a cloned socket handle.

use crate::pipeline::DEST_PEER;
use crate::{ClientShared, Clock, HostShared};
use crossbeam_channel::{Receiver, Sender};
use jam_audio::jitter::ArrivalJitter;
use jam_protocol::packet::{parse_datagram, Control, Header, PacketType, Payload};
use jam_protocol::{HOST_SENDER_ID, MAX_CLIENTS, MAX_DATAGRAM};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// One outgoing datagram, produced by the audio callback.
pub struct TxItem {
    pub dest: u8,
    pub len: u16,
    pub buf: [u8; MAX_DATAGRAM],
}

impl TxItem {
    pub fn new(dest: u8, bytes: &[u8]) -> Self {
        let mut buf = [0u8; MAX_DATAGRAM];
        buf[..bytes.len()].copy_from_slice(bytes);
        Self {
            dest,
            len: bytes.len() as u16,
            buf,
        }
    }
}

/// Address-table updates from the control thread to the RX/TX threads.
#[derive(Debug, Clone, Copy)]
pub enum AddrUpdate {
    Client { slot: u8, addr: Option<SocketAddr> },
    Peer(SocketAddr),
}

/// A control packet routed to the control thread.
#[derive(Debug)]
pub struct CtrlEvent {
    pub from: SocketAddr,
    pub header: Header,
    pub control: Control,
    pub rx_us: u64,
}

/// Spawns the TX thread: drains the ring, resolves destination slots to
/// addresses, sends. Also implements the dev-only outgoing loss simulation.
pub fn spawn_tx(
    socket: UdpSocket,
    mut ring: rtrb::Consumer<TxItem>,
    addr_rx: Receiver<AddrUpdate>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    simulate_loss: f32,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("jam-net-tx".into())
        .spawn(move || {
            let mut clients: [Option<SocketAddr>; MAX_CLIENTS] = [None; MAX_CLIENTS];
            let mut peer: Option<SocketAddr> = None;
            // Cheap deterministic LCG for loss simulation.
            let mut rng: u64 = 0x9E3779B97F4A7C15;
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                while let Ok(update) = addr_rx.try_recv() {
                    match update {
                        AddrUpdate::Client { slot, addr } => {
                            if (slot as usize) < MAX_CLIENTS {
                                clients[slot as usize] = addr;
                            }
                        }
                        AddrUpdate::Peer(addr) => peer = Some(addr),
                    }
                }
                let mut idle = true;
                while let Ok(item) = ring.pop() {
                    idle = false;
                    if simulate_loss > 0.0 {
                        rng = rng
                            .wrapping_mul(6364136223846793005)
                            .wrapping_add(1442695040888963407);
                        let roll = (rng >> 40) as f32 / (1u32 << 24) as f32;
                        if roll < simulate_loss {
                            continue;
                        }
                    }
                    let dest = if item.dest == DEST_PEER {
                        peer
                    } else {
                        clients.get(item.dest as usize).copied().flatten()
                    };
                    if let Some(addr) = dest {
                        let _ = socket.send_to(&item.buf[..item.len as usize], addr);
                    }
                }
                if idle {
                    // ~250 µs poll keeps added latency negligible against the
                    // 2.5 ms frame cadence without burning a core.
                    std::thread::sleep(Duration::from_micros(250));
                }
            }
        })
        .expect("spawn tx thread")
}

/// Which role the RX thread routes for.
pub enum RxRole {
    Host {
        shared: Arc<HostShared>,
    },
    Client {
        shared: Arc<ClientShared>,
        host_addr: SocketAddr,
    },
}

/// Spawns the RX thread. Audio goes straight into jitter buffers; control
/// packets go to the control thread; PINGs are answered inline for accurate
/// RTT timestamps.
#[allow(clippy::too_many_arguments)]
pub fn spawn_rx(
    socket: UdpSocket,
    role: RxRole,
    ctrl_tx: Sender<CtrlEvent>,
    addr_rx: Receiver<AddrUpdate>,
    clock: Clock,
    frame_samples: usize,
    sample_rate: u32,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("set read timeout");
    std::thread::Builder::new()
        .name("jam-net-rx".into())
        .spawn(move || {
            let mut buf = [0u8; 2048];
            // Host role: authoritative addr->slot map, mirrored from control.
            let mut client_addrs: [Option<SocketAddr>; MAX_CLIENTS] = [None; MAX_CLIENTS];
            let mut jitters: Vec<ArrivalJitter> = (0..MAX_CLIENTS.max(1))
                .map(|_| ArrivalJitter::default())
                .collect();
            let us_per_sample = 1_000_000.0 / sample_rate as f64;
            loop {
                if shutdown.load(Ordering::Acquire) {
                    return;
                }
                while let Ok(update) = addr_rx.try_recv() {
                    if let AddrUpdate::Client { slot, addr } = update {
                        if (slot as usize) < MAX_CLIENTS {
                            client_addrs[slot as usize] = addr;
                            jitters[slot as usize] = ArrivalJitter::default();
                        }
                    }
                }
                let (len, from) = match socket.recv_from(&mut buf) {
                    Ok(x) => x,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue;
                    }
                    Err(_) => continue,
                };
                let now_us = clock.now_us();
                let Ok((header, payload)) = parse_datagram(&buf[..len]) else {
                    continue;
                };

                // Stateless ping echo, answered here so t2/t3 are accurate.
                if let Payload::Control(Control::Ping { t1_us }) = &payload {
                    let pong = Control::Pong {
                        t1_us: *t1_us,
                        t2_us: now_us,
                        t3_us: clock.now_us(),
                    };
                    let mut out = [0u8; 64];
                    let reply_header = Header {
                        ptype: PacketType::Pong,
                        session_id: header.session_id,
                        sender_id: match &role {
                            RxRole::Host { .. } => HOST_SENDER_ID,
                            RxRole::Client { shared, .. } => {
                                shared.sender_id.load(Ordering::Acquire)
                            }
                        },
                        seq: header.seq,
                    };
                    let n = pong.write(&reply_header, &mut out);
                    let _ = socket.send_to(&out[..n], from);
                    continue;
                }

                match (&role, payload) {
                    (RxRole::Host { shared }, Payload::Audio(audio)) => {
                        if header.session_id != shared.session_id {
                            continue;
                        }
                        let sender = header.sender_id;
                        if sender == 0 || sender as usize > MAX_CLIENTS {
                            continue;
                        }
                        let slot = (sender - 1) as usize;
                        // Anti-spoof: source address must match the slot.
                        if client_addrs[slot] != Some(from) {
                            continue;
                        }
                        let stream = &shared.clients[slot];
                        stream.last_rx_us.store(now_us, Ordering::Release);
                        let media_us = (audio.timestamp as f64 * us_per_sample) as u64;
                        let j = jitters[slot].on_arrival(now_us, media_us);
                        stream.jb.publish_jitter_us(j);
                        stream.jb.push(header.seq, audio.timestamp, audio.payload);
                        if let Some(red) = audio.redundant {
                            stream.jb.push(
                                header.seq.wrapping_sub(1),
                                audio.timestamp.wrapping_sub(frame_samples as u32),
                                red,
                            );
                        }
                    }
                    (RxRole::Client { shared, host_addr }, Payload::Audio(audio)) => {
                        if from != *host_addr
                            || header.sender_id != HOST_SENDER_ID
                            || !shared.joined.load(Ordering::Acquire)
                            || header.session_id != shared.session_id.load(Ordering::Acquire)
                        {
                            continue;
                        }
                        let stream = &shared.from_host;
                        stream.last_rx_us.store(now_us, Ordering::Release);
                        let media_us = (audio.timestamp as f64 * us_per_sample) as u64;
                        let j = jitters[0].on_arrival(now_us, media_us);
                        stream.jb.publish_jitter_us(j);
                        stream.jb.push(header.seq, audio.timestamp, audio.payload);
                        if let Some(red) = audio.redundant {
                            stream.jb.push(
                                header.seq.wrapping_sub(1),
                                audio.timestamp.wrapping_sub(frame_samples as u32),
                                red,
                            );
                        }
                    }
                    (role, Payload::Control(control)) => {
                        // Client role: only the host may talk control to us.
                        if let RxRole::Client { host_addr, shared } = role {
                            if from != *host_addr {
                                continue;
                            }
                            shared.from_host.last_rx_us.store(now_us, Ordering::Release);
                        }
                        if let RxRole::Host { shared } = role {
                            let sender = header.sender_id;
                            if (1..=MAX_CLIENTS as u16).contains(&sender) {
                                let slot = (sender - 1) as usize;
                                if client_addrs[slot] == Some(from) {
                                    shared.clients[slot]
                                        .last_rx_us
                                        .store(now_us, Ordering::Release);
                                }
                            }
                        }
                        let _ = ctrl_tx.send(CtrlEvent {
                            from,
                            header,
                            control,
                            rx_us: now_us,
                        });
                    }
                }
            }
        })
        .expect("spawn rx thread")
}
