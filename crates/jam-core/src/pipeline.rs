//! The per-frame audio pipelines, host and client. These run on the audio
//! callback thread: allocation-free after construction, no locks, no
//! syscalls. Datagrams leave through an `emit` closure so the pipelines can
//! be driven headlessly in tests.

use crate::playout::StreamPlayout;
use crate::{ClientShared, EngineParams, HostShared, MonitorMode};
use jam_audio::codec::{AudioEncoder, CodecError};
use jam_audio::meter::Meter;
use jam_audio::mixer::{mix_scaled, soft_clip_buf, unmix_scaled, SmoothedGain};
use jam_audio::{MAX_FRAME_BYTES, MAX_FRAME_SAMPLES};
use jam_protocol::packet::{AudioPacket, Header, PacketType};
use jam_protocol::seq::SeqGen;
use jam_protocol::{HOST_SENDER_ID, MAX_CLIENTS, MAX_DATAGRAM};
use std::sync::atomic::Ordering;

/// Master trim applied to the sum: -6 dB of headroom for four players before
/// the soft clipper starts working.
const MASTER_TRIM: f32 = 0.5;

/// Destination tag for [`crate::net::TxItem`]: client slots 0..MAX_CLIENTS,
/// or the peer/host for the client role.
pub const DEST_PEER: u8 = u8::MAX;

#[allow(clippy::too_many_arguments)]
fn write_audio_datagram(
    dgram: &mut [u8; MAX_DATAGRAM],
    session_id: u16,
    sender_id: u16,
    seq: u16,
    timestamp: u32,
    params: &EngineParams,
    payload: &[u8],
    redundant: Option<&[u8]>,
) -> usize {
    let header = Header {
        ptype: PacketType::Audio,
        session_id,
        sender_id,
        seq,
    };
    let pkt = AudioPacket {
        timestamp,
        codec: params.codec,
        payload,
        redundant,
    };
    pkt.write(&header, dgram)
}

// ---------------------------------------------------------------------------
// Host pipeline
// ---------------------------------------------------------------------------

pub struct HostPipeline {
    frame: usize,
    params: EngineParams,
    playouts: [StreamPlayout; MAX_CLIENTS],
    encoders: [AudioEncoder; MAX_CLIENTS],
    seqs: [SeqGen; MAX_CLIENTS],
    prev_enc: [PrevFrame; MAX_CLIENTS],
    /// Gain smoothers: index 0 = host self, 1..=4 = clients.
    smoothers: [SmoothedGain; MAX_CLIENTS + 1],
    monitor_smoother: SmoothedGain,
    meters: [Meter; MAX_CLIENTS],
    local_meter: Meter,
    decoded: [[f32; MAX_FRAME_SAMPLES]; MAX_CLIENTS],
    scaled: [[f32; MAX_FRAME_SAMPLES]; MAX_CLIENTS],
    scaled_local: [f32; MAX_FRAME_SAMPLES],
    mix: [f32; MAX_FRAME_SAMPLES],
    client_out: [f32; MAX_FRAME_SAMPLES],
    enc_buf: [u8; MAX_FRAME_BYTES],
    dgram: [u8; MAX_DATAGRAM],
    last_active: [bool; MAX_CLIENTS],
    sample_clock: u32,
}

struct PrevFrame {
    buf: [u8; MAX_FRAME_BYTES],
    len: usize,
}

impl PrevFrame {
    fn new() -> Self {
        Self {
            buf: [0u8; MAX_FRAME_BYTES],
            len: 0,
        }
    }
    fn store(&mut self, data: &[u8]) {
        self.buf[..data.len()].copy_from_slice(data);
        self.len = data.len();
    }
    fn get(&self) -> Option<&[u8]> {
        (self.len > 0).then(|| &self.buf[..self.len])
    }
}

impl HostPipeline {
    pub fn new(params: &EngineParams) -> Result<Self, CodecError> {
        let mk_enc = || AudioEncoder::new(params.codec, params.sample_rate, params.bitrate_bps);
        let mk_gain = || SmoothedGain::new(1.0, params.frame_samples, params.sample_rate);
        Ok(Self {
            frame: params.frame_samples,
            params: params.clone(),
            playouts: [
                StreamPlayout::new(params)?,
                StreamPlayout::new(params)?,
                StreamPlayout::new(params)?,
                StreamPlayout::new(params)?,
            ],
            encoders: [mk_enc()?, mk_enc()?, mk_enc()?, mk_enc()?],
            seqs: Default::default(),
            prev_enc: [
                PrevFrame::new(),
                PrevFrame::new(),
                PrevFrame::new(),
                PrevFrame::new(),
            ],
            smoothers: [mk_gain(), mk_gain(), mk_gain(), mk_gain(), mk_gain()],
            monitor_smoother: mk_gain(),
            meters: Default::default(),
            local_meter: Meter::new(),
            decoded: [[0.0; MAX_FRAME_SAMPLES]; MAX_CLIENTS],
            scaled: [[0.0; MAX_FRAME_SAMPLES]; MAX_CLIENTS],
            scaled_local: [0.0; MAX_FRAME_SAMPLES],
            mix: [0.0; MAX_FRAME_SAMPLES],
            client_out: [0.0; MAX_FRAME_SAMPLES],
            enc_buf: [0u8; MAX_FRAME_BYTES],
            dgram: [0u8; MAX_DATAGRAM],
            last_active: [false; MAX_CLIENTS],
            sample_clock: 0,
        })
    }

    /// Runs one frame tick: consumes `local_in` (host's own captured frame),
    /// fills `host_out` with what the host hears, and emits one datagram per
    /// active client through `emit(slot, bytes)`.
    pub fn process_frame(
        &mut self,
        shared: &HostShared,
        local_in: &[f32],
        host_out: &mut [f32],
        mut emit: impl FnMut(u8, &[u8]),
    ) {
        let n = self.frame;
        debug_assert_eq!(local_in.len(), n);
        debug_assert_eq!(host_out.len(), n);

        // 1. Pull one frame per active client (decode / conceal).
        for c in 0..MAX_CLIENTS {
            let slot = &shared.clients[c];
            let active = slot.active.load(Ordering::Acquire);
            if active && !self.last_active[c] {
                // Slot came alive (new client): forget stale playout state.
                self.playouts[c].reset_stream();
            }
            self.last_active[c] = active;
            let buf = &mut self.decoded[c][..n];
            if active {
                self.playouts[c].tick(&slot.jb, buf);
                self.meters[c].update(buf);
                self.meters[c].publish(&slot.level);
            } else {
                buf.fill(0.0);
            }
        }

        // 2. Mix with smoothed per-player gains and master headroom trim.
        // Mute/solo fold into each player's gain target (multiply by 0 when
        // excluded) so transitions ramp through the existing smoother — no
        // clicks — and the mix-minus-self subtraction below stays consistent
        // because it reuses the same scaled buffers.
        let any_solo = shared.any_solo();
        let incl_local = if shared.in_mix(0, any_solo) { 1.0 } else { 0.0 };
        let g_local = self.smoothers[0].next(shared.gains[0].get() * incl_local) * MASTER_TRIM;
        let mix = &mut self.mix[..n];
        mix.fill(0.0);
        let scaled_local = &mut self.scaled_local[..n];
        for (d, s) in scaled_local.iter_mut().zip(local_in) {
            *d = s * g_local;
        }
        for (d, s) in mix.iter_mut().zip(scaled_local.iter()) {
            *d += *s;
        }
        for c in 0..MAX_CLIENTS {
            let incl = if shared.in_mix(c + 1, any_solo) {
                1.0
            } else {
                0.0
            };
            let g = self.smoothers[c + 1].next(shared.gains[c + 1].get() * incl) * MASTER_TRIM;
            let src = &self.decoded[c][..n];
            let dst = &mut self.scaled[c][..n];
            for (d, s) in dst.iter_mut().zip(src) {
                *d = s * g;
            }
            if self.last_active[c] {
                for (m, s) in mix.iter_mut().zip(dst.iter()) {
                    *m += *s;
                }
            }
        }

        // 3. Per-client sends: mix-minus-self (or echo), clip, encode, emit.
        for c in 0..MAX_CLIENTS {
            if !self.last_active[c] {
                continue;
            }
            let out = &mut self.client_out[..n];
            if self.params.echo {
                out.copy_from_slice(&self.decoded[c][..n]);
            } else {
                out.copy_from_slice(&self.mix[..n]);
                let scaled_self = &self.scaled[c][..n];
                unmix_scaled(out, scaled_self, 1.0);
            }
            soft_clip_buf(out);
            let Ok(len) = self.encoders[c].encode(out, &mut self.enc_buf) else {
                continue;
            };
            let redundant = if self.params.redundancy {
                self.prev_enc[c].get()
            } else {
                None
            };
            let dlen = write_audio_datagram(
                &mut self.dgram,
                shared.session_id,
                HOST_SENDER_ID,
                self.seqs[c].next(),
                self.sample_clock,
                &self.params,
                &self.enc_buf[..len],
                redundant,
            );
            emit(c as u8, &self.dgram[..dlen]);
            self.prev_enc[c].store(&self.enc_buf[..len]);
        }

        // 4. What the host hears.
        let out = &mut host_out[..n];
        out.copy_from_slice(&self.mix[..n]);
        match self.params.monitor {
            MonitorMode::ThroughMix => {}
            MonitorMode::Off => {
                unmix_scaled(out, &self.scaled_local[..n], 1.0);
            }
            MonitorMode::Direct => {
                unmix_scaled(out, &self.scaled_local[..n], 1.0);
                let g_mon = self.monitor_smoother.next(shared.monitor_gain.get());
                mix_scaled(out, local_in, g_mon);
            }
        }
        soft_clip_buf(out);

        self.local_meter.update(local_in);
        self.local_meter.publish(&shared.local_level);
        self.sample_clock = self.sample_clock.wrapping_add(n as u32);
    }
}

// ---------------------------------------------------------------------------
// Client pipeline
// ---------------------------------------------------------------------------

pub struct ClientPipeline {
    frame: usize,
    params: EngineParams,
    encoder: AudioEncoder,
    seq: SeqGen,
    prev_enc: PrevFrame,
    playout: StreamPlayout,
    mix_meter: Meter,
    local_meter: Meter,
    master_smoother: SmoothedGain,
    monitor_smoother: SmoothedGain,
    decoded: [f32; MAX_FRAME_SAMPLES],
    enc_buf: [u8; MAX_FRAME_BYTES],
    dgram: [u8; MAX_DATAGRAM],
    was_joined: bool,
    sample_clock: u32,
}

impl ClientPipeline {
    pub fn new(params: &EngineParams) -> Result<Self, CodecError> {
        Ok(Self {
            frame: params.frame_samples,
            params: params.clone(),
            encoder: AudioEncoder::new(params.codec, params.sample_rate, params.bitrate_bps)?,
            seq: SeqGen::new(),
            prev_enc: PrevFrame::new(),
            playout: StreamPlayout::new(params)?,
            mix_meter: Meter::new(),
            local_meter: Meter::new(),
            master_smoother: SmoothedGain::new(1.0, params.frame_samples, params.sample_rate),
            monitor_smoother: SmoothedGain::new(1.0, params.frame_samples, params.sample_rate),
            decoded: [0.0; MAX_FRAME_SAMPLES],
            enc_buf: [0u8; MAX_FRAME_BYTES],
            dgram: [0u8; MAX_DATAGRAM],
            was_joined: false,
            sample_clock: 0,
        })
    }

    /// One frame tick: send the local frame to the host (if joined), pull
    /// the returning mix, and fill `out` with what this player hears.
    pub fn process_frame(
        &mut self,
        shared: &ClientShared,
        local_in: &[f32],
        out: &mut [f32],
        mut emit: impl FnMut(u8, &[u8]),
    ) {
        let n = self.frame;
        debug_assert_eq!(local_in.len(), n);
        debug_assert_eq!(out.len(), n);

        let joined = shared.joined.load(Ordering::Acquire);
        if joined && !self.was_joined {
            self.playout.reset_stream();
        }
        self.was_joined = joined;

        // 1. Upstream: encode and send our own frame.
        if joined {
            if let Ok(len) = self.encoder.encode(local_in, &mut self.enc_buf) {
                let redundant = if self.params.redundancy {
                    self.prev_enc.get()
                } else {
                    None
                };
                let dlen = write_audio_datagram(
                    &mut self.dgram,
                    shared.session_id.load(Ordering::Acquire),
                    shared.sender_id.load(Ordering::Acquire),
                    self.seq.next(),
                    self.sample_clock,
                    &self.params,
                    &self.enc_buf[..len],
                    redundant,
                );
                emit(DEST_PEER, &self.dgram[..dlen]);
                self.prev_enc.store(&self.enc_buf[..len]);
            }
        }

        // 2. Downstream: the mix coming back from the host.
        let decoded = &mut self.decoded[..n];
        if joined {
            self.playout.tick(&shared.from_host.jb, decoded);
        } else {
            decoded.fill(0.0);
        }
        self.mix_meter.update(decoded);
        self.mix_meter.publish(&shared.from_host.level);

        // 3. Local output: mix (master gain) + optional direct monitor.
        let g_master = self.master_smoother.next(shared.master_gain.get());
        for (d, s) in out.iter_mut().zip(decoded.iter()) {
            *d = s * g_master;
        }
        if matches!(self.params.monitor, MonitorMode::Direct) {
            let g_mon = self.monitor_smoother.next(shared.monitor_gain.get());
            mix_scaled(out, local_in, g_mon);
        }
        soft_clip_buf(out);

        self.local_meter.update(local_in);
        self.local_meter.publish(&shared.local_level);
        self.sample_clock = self.sample_clock.wrapping_add(n as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jam_protocol::packet::{parse_datagram, Codec, Payload};

    fn params() -> EngineParams {
        EngineParams {
            codec: Codec::PcmF32,
            monitor: MonitorMode::Off,
            ..Default::default()
        }
    }

    /// Feed a client slot's jitter buffer directly and check the host mixes
    /// it, sends mix-minus-self, and hears the client.
    #[test]
    fn host_mixes_and_subtracts_self() {
        let p = params();
        let n = p.frame_samples;
        let shared = HostShared::new(7);
        let mut hp = HostPipeline::new(&p).unwrap();

        // Client 0 active with a constant 0.4 signal, client 1 active with 0.2.
        shared.clients[0].active.store(true, Ordering::Release);
        shared.clients[1].active.store(true, Ordering::Release);
        let mk = |v: f32| {
            let pcm = vec![v; n];
            let mut enc = AudioEncoder::new(Codec::PcmF32, 48_000, 0).unwrap();
            let mut buf = vec![0u8; n * 4];
            let len = enc.encode(&pcm, &mut buf).unwrap();
            buf.truncate(len);
            buf
        };
        for s in 0..3u16 {
            shared.clients[0].jb.push(s, s as u32 * 120, &mk(0.4));
            shared.clients[1].jb.push(s, s as u32 * 120, &mk(0.2));
        }

        let local_in = vec![0.1f32; n];
        let mut host_out = vec![0.0f32; n];
        let mut sent: Vec<(u8, Vec<u8>)> = vec![];
        // Run several frames so gain smoothing settles and buffers prime.
        for _ in 0..3 {
            hp.process_frame(&shared, &local_in, &mut host_out, |d, b| {
                sent.push((d, b.to_vec()))
            });
        }

        // Host hears (0.4 + 0.2) * trim (self removed in Off mode).
        let expect_host = (0.4 + 0.2) * 0.5;
        let mid = host_out[n / 2];
        assert!(
            (mid - expect_host).abs() < 0.02,
            "host hears {mid}, expected ~{expect_host}"
        );

        // Both clients got packets; decode the last one for client 0:
        // mix minus self = (0.1 + 0.2) * trim.
        let last_c0 = sent.iter().rev().find(|(d, _)| *d == 0).unwrap();
        let (h, payload) = parse_datagram(&last_c0.1).unwrap();
        assert_eq!(h.session_id, 7);
        assert_eq!(h.sender_id, HOST_SENDER_ID);
        let Payload::Audio(a) = payload else { panic!() };
        let mut dec = jam_audio::codec::AudioDecoder::new(Codec::PcmF32, 48_000).unwrap();
        let mut pcm = vec![0.0f32; n];
        dec.decode(Some(a.payload), &mut pcm).unwrap();
        let expect_c0 = (0.1 + 0.2) * 0.5;
        assert!(
            (pcm[n / 2] - expect_c0).abs() < 0.02,
            "client0 hears {}, expected ~{expect_c0}",
            pcm[n / 2]
        );
    }

    /// Encode a constant-value PCM-f32 frame of `n` samples.
    fn enc_const(v: f32, n: usize) -> Vec<u8> {
        let pcm = vec![v; n];
        let mut enc = AudioEncoder::new(Codec::PcmF32, 48_000, 0).unwrap();
        let mut buf = vec![0u8; n * 4];
        let len = enc.encode(&pcm, &mut buf).unwrap();
        buf.truncate(len);
        buf
    }

    /// Run the host over many frames with two active clients emitting constant
    /// levels, keeping both jitter buffers fed, and return the steady host-out
    /// value (host is silent + monitor Off, so it hears exactly the mix).
    fn steady_host_out(configure: impl FnOnce(&HostShared)) -> f32 {
        let p = params();
        let n = p.frame_samples;
        let shared = HostShared::new(7);
        let mut hp = HostPipeline::new(&p).unwrap();
        shared.clients[0].active.store(true, Ordering::Release);
        shared.clients[1].active.store(true, Ordering::Release);
        configure(&shared);

        let local_in = vec![0.0f32; n];
        let mut host_out = vec![0.0f32; n];
        let mut seq = 0u16;
        // Prime a couple of frames ahead of the reader.
        for _ in 0..3 {
            shared.clients[0]
                .jb
                .push(seq, seq as u32 * 120, &enc_const(0.4, n));
            shared.clients[1]
                .jb
                .push(seq, seq as u32 * 120, &enc_const(0.2, n));
            seq = seq.wrapping_add(1);
        }
        // Run well past the ~10 ms gain-smoother settle time, feeding one
        // fresh frame per tick so neither buffer starves.
        for _ in 0..60 {
            shared.clients[0]
                .jb
                .push(seq, seq as u32 * 120, &enc_const(0.4, n));
            shared.clients[1]
                .jb
                .push(seq, seq as u32 * 120, &enc_const(0.2, n));
            seq = seq.wrapping_add(1);
            hp.process_frame(&shared, &local_in, &mut host_out, |_, _| {});
        }
        host_out[n / 2]
    }

    #[test]
    fn muting_a_player_removes_it_from_the_mix() {
        // Both active → host hears (0.4 + 0.2) * trim = 0.30.
        let both = steady_host_out(|_| {});
        assert!((both - 0.30).abs() < 0.02, "both active: {both}");
        // Mute client 1 (gain index 2) → only client 0 remains: 0.4 * 0.5.
        let muted = steady_host_out(|s| s.muted[2].store(true, Ordering::Relaxed));
        assert!(
            (muted - 0.20).abs() < 0.02,
            "client1 muted: {muted}, expected ~0.20"
        );
    }

    #[test]
    fn soloing_isolates_to_soloed_players() {
        // Solo client 0 (gain index 1): only client 0 is heard.
        let solo0 = steady_host_out(|s| s.soloed[1].store(true, Ordering::Relaxed));
        assert!(
            (solo0 - 0.20).abs() < 0.02,
            "solo client0: {solo0}, expected ~0.20 (0.4*trim)"
        );
        // Solo client 1 instead: only client 1 (0.2 * 0.5 = 0.10).
        let solo1 = steady_host_out(|s| s.soloed[2].store(true, Ordering::Relaxed));
        assert!(
            (solo1 - 0.10).abs() < 0.02,
            "solo client1: {solo1}, expected ~0.10 (0.2*trim)"
        );
        // Solo the host itself (index 0, silent here): clients are excluded,
        // and since the host feeds no signal the mix is empty.
        let solo_host = steady_host_out(|s| s.soloed[0].store(true, Ordering::Relaxed));
        assert!(
            solo_host.abs() < 0.02,
            "solo silent host: {solo_host}, expected ~0"
        );
    }

    #[test]
    fn solo_on_a_non_audible_slot_does_not_silence_the_room() {
        // Muting the only soloed player must not count as an active solo:
        // the room falls back to a normal mix (client1 still heard), never
        // to silence.
        let solo_then_mute = steady_host_out(|s| {
            s.soloed[1].store(true, Ordering::Relaxed);
            s.muted[1].store(true, Ordering::Relaxed);
        });
        assert!(
            (solo_then_mute - 0.10).abs() < 0.02,
            "solo+mute the same player should fall back to the rest of the \
             mix (~0.10), not silence: {solo_then_mute}"
        );
        // Soloing an empty/inactive slot (index 3 — clients[2] is not active
        // in this harness) is a no-op, not a room-killer.
        let solo_empty = steady_host_out(|s| s.soloed[3].store(true, Ordering::Relaxed));
        assert!(
            (solo_empty - 0.30).abs() < 0.02,
            "solo on an empty slot must leave the full mix (~0.30): {solo_empty}"
        );
    }

    #[test]
    fn client_sends_when_joined_and_applies_monitor() {
        let p = EngineParams {
            codec: Codec::PcmF32,
            monitor: MonitorMode::Direct,
            ..Default::default()
        };
        let n = p.frame_samples;
        let shared = ClientShared::default();
        let mut cp = ClientPipeline::new(&p).unwrap();
        let local_in = vec![0.3f32; n];
        let mut out = vec![0.0f32; n];

        // Not joined: no datagrams, output only silence+monitor.
        let mut count = 0;
        cp.process_frame(&shared, &local_in, &mut out, |_, _| count += 1);
        assert_eq!(count, 0);

        shared.session_id.store(9, Ordering::Release);
        shared.sender_id.store(2, Ordering::Release);
        shared.joined.store(true, Ordering::Release);
        let mut sent = vec![];
        for _ in 0..3 {
            cp.process_frame(&shared, &local_in, &mut out, |d, b| {
                sent.push((d, b.to_vec()))
            });
        }
        assert_eq!(sent.len(), 3);
        let (h, payload) = parse_datagram(&sent[0].1).unwrap();
        assert_eq!(h.sender_id, 2);
        assert_eq!(h.session_id, 9);
        assert!(matches!(payload, Payload::Audio(_)));
        // Direct monitor: hears own signal (no mix yet).
        assert!((out[n / 2] - 0.3).abs() < 0.05);
    }

    #[test]
    fn redundancy_attaches_previous_frame() {
        let p = EngineParams {
            codec: Codec::PcmF32,
            redundancy: true,
            monitor: MonitorMode::Off,
            ..Default::default()
        };
        let n = p.frame_samples;
        let shared = ClientShared::default();
        shared.joined.store(true, Ordering::Release);
        shared.sender_id.store(1, Ordering::Release);
        let mut cp = ClientPipeline::new(&p).unwrap();
        let mut out = vec![0.0f32; n];
        let mut sent = vec![];
        let a = vec![0.11f32; n];
        let b = vec![0.22f32; n];
        cp.process_frame(&shared, &a, &mut out, |_, bts| sent.push(bts.to_vec()));
        cp.process_frame(&shared, &b, &mut out, |_, bts| sent.push(bts.to_vec()));
        let (_, p1) = parse_datagram(&sent[0]).unwrap();
        let Payload::Audio(a1) = p1 else { panic!() };
        assert!(a1.redundant.is_none(), "first packet has no previous frame");
        let (_, p2) = parse_datagram(&sent[1]).unwrap();
        let Payload::Audio(a2) = p2 else { panic!() };
        let red = a2.redundant.expect("second packet carries redundancy");
        assert_eq!(red, a1.payload, "redundant copy is the previous payload");
    }
}
