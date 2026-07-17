//! End-to-end session test over real UDP sockets on localhost, with the
//! audio pipelines driven by hand (no sound card needed): the M3/M4
//! milestones as an automated test.
//!
//! Host + two clients join a room; each participant "plays" a distinct sine
//! tone; after a couple of seconds we assert that each client hears the
//! other two players but NOT itself (mix-minus-self), and the host hears
//! both clients.

use jam_core::engine::{start_client_headless, start_host_headless, ClientConfig, HostConfig};
use jam_core::net::TxItem;
use jam_core::pipeline::{ClientPipeline, HostPipeline};
use jam_core::{ClientShared, EngineParams, HostShared, MonitorMode};
use jam_protocol::packet::Codec;
use std::f32::consts::TAU;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

const FRAME: usize = 120;
const RATE: u32 = 48_000;
const ROOM: [u8; 6] = *b"TEST01";

fn params(loss: f32, redundancy: bool) -> EngineParams {
    EngineParams {
        codec: Codec::PcmF32,
        monitor: MonitorMode::Off,
        simulate_loss: loss,
        redundancy,
        ..Default::default()
    }
}

struct Tone {
    freq: f32,
    phase: f32,
}

impl Tone {
    fn new(freq: f32) -> Self {
        Self { freq, phase: 0.0 }
    }
    fn fill(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            *x = self.phase.sin() * 0.3;
            self.phase += TAU * self.freq / RATE as f32;
            if self.phase > TAU {
                self.phase -= TAU;
            }
        }
    }
}

/// Goertzel power of `freq` in `signal`, normalized by length.
fn goertzel(signal: &[f32], freq: f32) -> f32 {
    let w = TAU * freq / RATE as f32;
    let coeff = 2.0 * w.cos();
    let (mut s1, mut s2) = (0.0f32, 0.0f32);
    for &x in signal {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    power / (signal.len() as f32 * signal.len() as f32 / 4.0)
}

fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

struct Participant<P> {
    pipeline: P,
    tx: rtrb::Producer<TxItem>,
    tone: Tone,
    input: Vec<f32>,
    output: Vec<f32>,
    heard: Vec<f32>,
}

impl<P> Participant<P> {
    fn new(pipeline: P, tx: rtrb::Producer<TxItem>, freq: f32) -> Self {
        Self {
            pipeline,
            tx,
            tone: Tone::new(freq),
            input: vec![0.0; FRAME],
            output: vec![0.0; FRAME],
            heard: vec![],
        }
    }
}

#[test]
fn two_clients_join_mix_and_hear_each_other() {
    let (host_handle, host_pipe, host_tx, host_shared) = start_host_headless(HostConfig {
        port: 0,
        room_code: ROOM,
        name: "hostess".into(),
        params: params(0.0, false),
        ..Default::default()
    })
    .expect("host starts");
    let host_addr = format!("127.0.0.1:{}", host_handle.local_addr.port())
        .parse()
        .unwrap();

    let mk_client = |name: &str| {
        start_client_headless(ClientConfig {
            host_addr,
            room_code: ROOM,
            name: name.into(),
            params: params(0.0, false),
            audio: Default::default(),
        })
        .expect("client starts")
    };
    let (c1_handle, c1_pipe, c1_tx, c1_shared) = mk_client("alice");
    let (c2_handle, c2_pipe, c2_tx, c2_shared) = mk_client("bob");

    assert!(
        wait_until(Duration::from_secs(5), || {
            c1_shared.joined.load(Ordering::Acquire) && c2_shared.joined.load(Ordering::Acquire)
        }),
        "both clients complete the handshake"
    );

    // Distinct tones: host 440 Hz, alice 660 Hz, bob 880 Hz.
    let mut host = Participant::new(host_pipe, host_tx, 440.0);
    let mut alice = Participant::new(c1_pipe, c1_tx, 660.0);
    let mut bob = Participant::new(c2_pipe, c2_tx, 880.0);

    // Drive all three pipelines from one paced loop (~2.5 ms per frame).
    // 1200 frames = 3 s.
    let start = Instant::now();
    for i in 0..1200usize {
        step_host(&mut host, &host_shared);
        step_client(&mut alice, &c1_shared);
        step_client(&mut bob, &c2_shared);
        // Keep only the tail for analysis (steady state).
        let target = start + Duration::from_micros(2_500 * (i as u64 + 1));
        if let Some(sleep) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
    }

    let tail = |v: &Vec<f32>| v[v.len() - RATE as usize..].to_vec(); // last 1 s
    let host_heard = tail(&host.heard);
    let alice_heard = tail(&alice.heard);
    let bob_heard = tail(&bob.heard);

    // Host (monitor Off) hears both clients, not itself.
    let h440 = goertzel(&host_heard, 440.0);
    let h660 = goertzel(&host_heard, 660.0);
    let h880 = goertzel(&host_heard, 880.0);
    assert!(
        h660 > 1e-4 && h880 > 1e-4,
        "host hears clients: 660={h660}, 880={h880}"
    );
    assert!(
        h440 < h660 * 0.01,
        "host self tone suppressed: 440={h440} vs 660={h660}"
    );

    // Alice hears host + bob, not herself.
    let a440 = goertzel(&alice_heard, 440.0);
    let a660 = goertzel(&alice_heard, 660.0);
    let a880 = goertzel(&alice_heard, 880.0);
    assert!(
        a440 > 1e-4 && a880 > 1e-4,
        "alice hears host+bob: 440={a440}, 880={a880}"
    );
    assert!(
        a660 < a440 * 0.01,
        "alice self tone suppressed: 660={a660} vs 440={a440}"
    );

    // Bob hears host + alice, not himself.
    let b880 = goertzel(&bob_heard, 880.0);
    let b440 = goertzel(&bob_heard, 440.0);
    assert!(b440 > 1e-4, "bob hears host");
    assert!(b880 < b440 * 0.01, "bob self tone suppressed");

    // Roster propagated: host snapshot shows 3 players.
    assert!(
        wait_until(Duration::from_secs(2), || {
            host_handle.snapshot.lock().unwrap().players.len() == 3
        }),
        "host snapshot lists host + 2 clients"
    );
    let names: Vec<String> = host_handle
        .snapshot
        .lock()
        .unwrap()
        .players
        .iter()
        .map(|p| p.name.clone())
        .collect();
    assert!(names.iter().any(|n| n.contains("alice")));
    assert!(names.iter().any(|n| n.contains("bob")));

    c1_handle.shutdown();
    c2_handle.shutdown();
    host_handle.shutdown();
}

/// Under 5% simulated outgoing loss with redundancy enabled, the host's
/// received stream stays nearly gap-free (single losses repaired by the
/// piggybacked previous frame).
#[test]
fn redundancy_repairs_simulated_loss() {
    let (host_handle, host_pipe, host_tx, host_shared) = start_host_headless(HostConfig {
        port: 0,
        room_code: ROOM,
        name: "host".into(),
        params: params(0.0, false),
        ..Default::default()
    })
    .expect("host starts");
    let host_addr = format!("127.0.0.1:{}", host_handle.local_addr.port())
        .parse()
        .unwrap();

    // Client drops 5% of its outgoing packets but attaches redundancy.
    let (c_handle, c_pipe, c_tx, c_shared) = start_client_headless(ClientConfig {
        host_addr,
        room_code: ROOM,
        name: "lossy".into(),
        params: params(0.05, true),
        audio: Default::default(),
    })
    .expect("client starts");

    assert!(
        wait_until(Duration::from_secs(5), || c_shared
            .joined
            .load(Ordering::Acquire)),
        "client joins"
    );

    let mut host = Participant::new(host_pipe, host_tx, 440.0);
    let mut client = Participant::new(c_pipe, c_tx, 660.0);
    let start = Instant::now();
    for i in 0..1600usize {
        step_host(&mut host, &host_shared);
        step_client(&mut client, &c_shared);
        let target = start + Duration::from_micros(2_500 * (i as u64 + 1));
        if let Some(sleep) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
    }

    let stats = &host_shared.clients[0].jb.stats;
    let played = stats.played.load(Ordering::Relaxed) as f32;
    let concealed = stats.concealed.load(Ordering::Relaxed) as f32;
    assert!(played > 800.0, "stream flowed ({played} frames)");
    // Raw 5% loss would conceal ~5%; redundancy repairs single losses, so
    // only back-to-back losses (~0.25%) remain. Allow slack for priming.
    let rate = concealed / (played + concealed);
    assert!(
        rate < 0.02,
        "concealment rate {rate} (concealed {concealed} / played {played})"
    );

    c_handle.shutdown();
    host_handle.shutdown();
}

fn step_host(p: &mut Participant<HostPipeline>, shared: &Arc<HostShared>) {
    let tone = {
        p.tone.fill(&mut p.input);
        &p.input
    };
    let Participant {
        pipeline,
        tx,
        output,
        heard,
        ..
    } = p;
    pipeline.process_frame(shared, tone, output, |dest, bytes| {
        let _ = tx.push(TxItem::new(dest, bytes));
    });
    heard.extend_from_slice(output);
}

fn step_client(p: &mut Participant<ClientPipeline>, shared: &Arc<ClientShared>) {
    let tone = {
        p.tone.fill(&mut p.input);
        &p.input
    };
    let Participant {
        pipeline,
        tx,
        output,
        heard,
        ..
    } = p;
    pipeline.process_frame(shared, tone, output, |dest, bytes| {
        let _ = tx.push(TxItem::new(dest, bytes));
    });
    heard.extend_from_slice(output);
}
