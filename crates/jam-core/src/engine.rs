//! Session assembly: sockets + threads + cpal streams.
//!
//! `start_host` / `start_client` wire everything together for the app. The
//! audio-callback closures own their pipeline and ring endpoints; shared
//! state crosses thread boundaries only through the lock-free structures in
//! [`crate`].

use crate::control::{
    spawn_client_control, spawn_host_control, ClientControlConfig, HostControlConfig,
};
use crate::net::{spawn_rx, spawn_tx, AddrUpdate, RxRole, TxItem};
use crate::pipeline::{ClientPipeline, HostPipeline, DEST_PEER};
use crate::stats::{new_shared, Role, SharedSnapshot};
use crate::{ClientShared, Clock, EngineParams, HostShared};
use cpal::traits::{DeviceTrait, StreamTrait};
use jam_audio::accum::SampleFifo;
use jam_audio::codec::CodecError;
use jam_audio::device::{open_input, open_output, DeviceError, Negotiated};
use jam_protocol::{DEFAULT_PORT, ROOM_CODE_LEN};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Device(#[from] DeviceError),
    #[error(transparent)]
    Codec(#[from] CodecError),
    #[error("audio stream: {0}")]
    Stream(String),
    #[error("socket: {0}")]
    Socket(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct AudioConfig {
    pub input_name: Option<String>,
    pub output_name: Option<String>,
    pub hw_buffer: u32,
}

/// Keeps streams and threads alive; dropping it tears the session down.
pub struct SessionHandle {
    pub snapshot: SharedSnapshot,
    pub clock: Clock,
    /// The local UDP address this session is bound to.
    pub local_addr: SocketAddr,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
    // Streams must stay alive for audio to flow; kept last so they drop
    // (stopping callbacks) before threads are joined.
    _streams: Vec<cpal::Stream>,
    pub xruns: Arc<AtomicU64>,
}

impl SessionHandle {
    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::Release);
        self._streams.clear();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

pub struct HostConfig {
    pub port: u16,
    pub room_code: [u8; ROOM_CODE_LEN],
    pub name: String,
    pub params: EngineParams,
    pub audio: AudioConfig,
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_PORT,
            room_code: *b"JAMJAM",
            name: "host".into(),
            params: EngineParams::default(),
            audio: AudioConfig {
                hw_buffer: jam_audio::DEFAULT_HW_BUFFER,
                ..Default::default()
            },
        }
    }
}

fn session_id_from_entropy() -> u16 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let mixed = nanos ^ pid.rotate_left(16) ^ 0x5AA5;
    // 0 is reserved as "unknown" on clients before WELCOME.
    ((mixed & 0xFFFF) as u16).max(1)
}

pub fn start_host(cfg: HostConfig) -> Result<(SessionHandle, Arc<HostShared>), EngineError> {
    let socket = UdpSocket::bind(("0.0.0.0", cfg.port))?;
    let local_addr = socket.local_addr()?;
    let shared = Arc::new(HostShared::new(session_id_from_entropy()));
    let clock = Clock::new();
    let snapshot = new_shared(Role::Host, "starting...");
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (tx_prod, tx_cons) = rtrb::RingBuffer::<TxItem>::new(256);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (addr_rx_tx, addr_rx_rx) = crossbeam_channel::unbounded();
    let (addr_tx_tx, addr_tx_rx) = crossbeam_channel::unbounded();

    #[allow(clippy::vec_init_then_push)]
    let threads = {
        let mut threads = vec![];
        threads.push(spawn_rx(
            socket.try_clone()?,
            RxRole::Host {
                shared: shared.clone(),
            },
            ctrl_tx,
            addr_rx_rx,
            clock.clone(),
            cfg.params.frame_samples,
            cfg.params.sample_rate,
            shutdown.clone(),
        ));
        threads.push(spawn_tx(
            socket.try_clone()?,
            tx_cons,
            addr_tx_rx,
            shutdown.clone(),
            cfg.params.simulate_loss,
        ));
        threads.push(spawn_host_control(
            socket.try_clone()?,
            shared.clone(),
            HostControlConfig {
                room_code: cfg.room_code,
                host_name: cfg.name.clone(),
                params: cfg.params.clone(),
                hw_buffer: cfg.audio.hw_buffer,
            },
            ctrl_rx,
            addr_rx_tx,
            addr_tx_tx,
            clock.clone(),
            snapshot.clone(),
        ));
        threads
    };

    let (streams, xruns) = start_host_audio(&cfg.params, shared.clone(), tx_prod, &cfg.audio)?;

    Ok((
        SessionHandle {
            snapshot,
            clock,
            local_addr,
            shutdown,
            threads,
            _streams: streams,
            xruns,
        },
        shared,
    ))
}

pub struct ClientConfig {
    pub host_addr: SocketAddr,
    pub room_code: [u8; ROOM_CODE_LEN],
    pub name: String,
    pub params: EngineParams,
    pub audio: AudioConfig,
}

pub fn start_client(cfg: ClientConfig) -> Result<(SessionHandle, Arc<ClientShared>), EngineError> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    let local_addr = socket.local_addr()?;
    let shared = Arc::new(ClientShared::default());
    let clock = Clock::new();
    let snapshot = new_shared(Role::Client, format!("joining {} ...", cfg.host_addr));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (tx_prod, tx_cons) = rtrb::RingBuffer::<TxItem>::new(256);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (_addr_rx_tx, addr_rx_rx) = crossbeam_channel::unbounded::<AddrUpdate>();
    let (addr_tx_tx, addr_tx_rx) = crossbeam_channel::unbounded();
    // The TX thread needs the (fixed) host address.
    let _ = addr_tx_tx.send(AddrUpdate::Peer(cfg.host_addr));

    #[allow(clippy::vec_init_then_push)]
    let threads = {
        let mut threads = vec![];
        threads.push(spawn_rx(
            socket.try_clone()?,
            RxRole::Client {
                shared: shared.clone(),
                host_addr: cfg.host_addr,
            },
            ctrl_tx,
            addr_rx_rx,
            clock.clone(),
            cfg.params.frame_samples,
            cfg.params.sample_rate,
            shutdown.clone(),
        ));
        threads.push(spawn_tx(
            socket.try_clone()?,
            tx_cons,
            addr_tx_rx,
            shutdown.clone(),
            cfg.params.simulate_loss,
        ));
        threads.push(spawn_client_control(
            socket.try_clone()?,
            shared.clone(),
            ClientControlConfig {
                room_code: cfg.room_code,
                name: cfg.name.clone(),
                params: cfg.params.clone(),
                hw_buffer: cfg.audio.hw_buffer,
                host_addr: cfg.host_addr,
            },
            ctrl_rx,
            clock.clone(),
            snapshot.clone(),
        ));
        threads
    };

    let (streams, xruns) = start_client_audio(&cfg.params, shared.clone(), tx_prod, &cfg.audio)?;

    Ok((
        SessionHandle {
            snapshot,
            clock,
            local_addr,
            shutdown,
            threads,
            _streams: streams,
            xruns,
        },
        shared,
    ))
}

// ---------------------------------------------------------------------------
// cpal stream wiring
// ---------------------------------------------------------------------------

/// Instantiates a callback for the device's raw sample format, converting
/// to/from our internal f32 at the edge.
macro_rules! per_format {
    ($format:expr, $build:ident, $($arg:expr),*) => {
        match $format {
            cpal::SampleFormat::F32 => $build::<f32>($($arg),*),
            cpal::SampleFormat::I32 => $build::<i32>($($arg),*),
            cpal::SampleFormat::I16 => $build::<i16>($($arg),*),
            cpal::SampleFormat::U16 => $build::<u16>($($arg),*),
            cpal::SampleFormat::F64 => $build::<f64>($($arg),*),
            cpal::SampleFormat::U32 => $build::<u32>($($arg),*),
            cpal::SampleFormat::I8 => $build::<i8>($($arg),*),
            cpal::SampleFormat::U8 => $build::<u8>($($arg),*),
            cpal::SampleFormat::I64 => $build::<i64>($($arg),*),
            cpal::SampleFormat::U64 => $build::<u64>($($arg),*),
            other => Err(EngineError::Stream(format!(
                "unsupported device sample format {other:?}"
            ))),
        }
    };
}

/// Extracts channel 0 from an interleaved input callback into the capture
/// ring; drops samples if the ring is full (the pipeline handles gaps).
fn input_stream(
    negotiated: &Negotiated,
    prod: rtrb::Producer<f32>,
    xruns: Arc<AtomicU64>,
) -> Result<cpal::Stream, EngineError> {
    per_format!(
        negotiated.sample_format,
        build_input,
        negotiated,
        prod,
        xruns
    )
}

fn build_input<T>(
    negotiated: &Negotiated,
    mut prod: rtrb::Producer<f32>,
    xruns: Arc<AtomicU64>,
) -> Result<cpal::Stream, EngineError>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    use cpal::Sample;
    let channels = negotiated.channels as usize;
    let xr = xruns.clone();
    negotiated
        .device
        .build_input_stream(
            &negotiated.config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                for frame in data.chunks_exact(channels) {
                    let _ = prod.push(f32::from_sample(frame[0]));
                }
            },
            move |_err| {
                xr.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| EngineError::Stream(e.to_string()))
}

/// Generic output wiring: `process` produces one mono frame per call; the
/// closure state fans it out to all output channels at the device block size.
fn output_stream(
    negotiated: &Negotiated,
    frame_samples: usize,
    process: impl FnMut(&mut [f32]) + Send + 'static,
    xruns: Arc<AtomicU64>,
) -> Result<cpal::Stream, EngineError> {
    per_format!(
        negotiated.sample_format,
        build_output,
        negotiated,
        frame_samples,
        process,
        xruns
    )
}

fn build_output<T>(
    negotiated: &Negotiated,
    frame_samples: usize,
    mut process: impl FnMut(&mut [f32]) + Send + 'static,
    xruns: Arc<AtomicU64>,
) -> Result<cpal::Stream, EngineError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = negotiated.channels as usize;
    // Capacity generously exceeds any realistic device callback (~1.4 s), so
    // `pending.len()` can always reach `needed` and the fill loop below never
    // spins forever even when the driver hands us a very large buffer
    // (possible when the device reports an unknown buffer size and cpal falls
    // back to BufferSize::Default).
    let mut pending = SampleFifo::new(frame_samples * 16 + 65_536);
    let mut frame_buf = vec![0.0f32; frame_samples];
    let xr = xruns.clone();
    negotiated
        .device
        .build_output_stream(
            &negotiated.config,
            move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
                let needed = data.len() / channels;
                while pending.len() < needed {
                    process(&mut frame_buf);
                    pending.push(&frame_buf);
                }
                // Drain in frame-sized chunks and fan each mono sample across
                // the device's channels — no fixed intermediate cap, so large
                // callbacks are served in full instead of leaving a stale tail.
                let mut written = 0;
                while written < needed {
                    let take = (needed - written).min(frame_buf.len());
                    let ok = pending.pop(&mut frame_buf[..take]);
                    debug_assert!(ok);
                    for (k, &s) in frame_buf[..take].iter().enumerate() {
                        let base = (written + k) * channels;
                        let v = T::from_sample(s);
                        for ch in 0..channels {
                            data[base + ch] = v;
                        }
                    }
                    written += take;
                }
            },
            move |_err| {
                xr.fetch_add(1, Ordering::Relaxed);
            },
            None,
        )
        .map_err(|e| EngineError::Stream(e.to_string()))
}

fn start_host_audio(
    params: &EngineParams,
    shared: Arc<HostShared>,
    mut tx: rtrb::Producer<TxItem>,
    audio: &AudioConfig,
) -> Result<(Vec<cpal::Stream>, Arc<AtomicU64>), EngineError> {
    let xruns = Arc::new(AtomicU64::new(0));
    let input = open_input(audio.input_name.as_deref(), audio.hw_buffer)?;
    let output = open_output(audio.output_name.as_deref(), audio.hw_buffer)?;

    let (cap_prod, mut cap_cons) = rtrb::RingBuffer::<f32>::new(params.sample_rate as usize / 4);
    let in_stream = input_stream(&input, cap_prod, xruns.clone())?;

    let mut pipeline = HostPipeline::new(params)?;
    let frame = params.frame_samples;
    // Bound extra capture latency against input/output clock drift, but never
    // below one device callback's worth of samples plus headroom — otherwise a
    // large output buffer (e.g. --buffer 512 > 4 frames) would consume more
    // per callback than the cap allows and starve every cycle with no drift.
    let drift_cap = (frame * 4).max(audio.hw_buffer as usize * 2);
    let mut local_in = vec![0.0f32; frame];
    let sh = shared.clone();
    let out_stream = output_stream(
        &output,
        frame,
        move |host_out: &mut [f32]| {
            // With independent devices the input clock may run faster, so
            // without this the ring would fill and add a fixed quarter-second
            // of latency. Keep occupancy bounded, dropping the oldest samples.
            while cap_cons.slots() > drift_cap {
                let _ = cap_cons.pop();
            }
            if cap_cons.slots() >= frame {
                for x in local_in.iter_mut() {
                    *x = cap_cons.pop().unwrap_or(0.0);
                }
            } else {
                local_in.fill(0.0);
                sh.capture_starved.fetch_add(1, Ordering::Relaxed);
            }
            pipeline.process_frame(&sh, &local_in, host_out, |dest, bytes| {
                let _ = tx.push(TxItem::new(dest, bytes));
            });
        },
        xruns.clone(),
    )?;

    in_stream
        .play()
        .map_err(|e| EngineError::Stream(e.to_string()))?;
    out_stream
        .play()
        .map_err(|e| EngineError::Stream(e.to_string()))?;
    Ok((vec![in_stream, out_stream], xruns))
}

fn start_client_audio(
    params: &EngineParams,
    shared: Arc<ClientShared>,
    mut tx: rtrb::Producer<TxItem>,
    audio: &AudioConfig,
) -> Result<(Vec<cpal::Stream>, Arc<AtomicU64>), EngineError> {
    let xruns = Arc::new(AtomicU64::new(0));
    let input = open_input(audio.input_name.as_deref(), audio.hw_buffer)?;
    let output = open_output(audio.output_name.as_deref(), audio.hw_buffer)?;

    let (cap_prod, mut cap_cons) = rtrb::RingBuffer::<f32>::new(params.sample_rate as usize / 4);
    let in_stream = input_stream(&input, cap_prod, xruns.clone())?;

    let mut pipeline = ClientPipeline::new(params)?;
    let frame = params.frame_samples;
    // See the host path for the rationale; the cap must cover one device
    // callback so a large --buffer doesn't starve every cycle.
    let drift_cap = (frame * 4).max(audio.hw_buffer as usize * 2);
    let mut local_in = vec![0.0f32; frame];
    let sh = shared.clone();
    let out_stream = output_stream(
        &output,
        frame,
        move |out: &mut [f32]| {
            // Bound capture latency against input/output clock drift (see the
            // host path for the rationale).
            while cap_cons.slots() > drift_cap {
                let _ = cap_cons.pop();
            }
            if cap_cons.slots() >= frame {
                for x in local_in.iter_mut() {
                    *x = cap_cons.pop().unwrap_or(0.0);
                }
            } else {
                local_in.fill(0.0);
                sh.capture_starved.fetch_add(1, Ordering::Relaxed);
            }
            pipeline.process_frame(&sh, &local_in, out, |dest, bytes| {
                debug_assert_eq!(dest, DEST_PEER);
                let _ = tx.push(TxItem::new(dest, bytes));
            });
        },
        xruns.clone(),
    )?;

    in_stream
        .play()
        .map_err(|e| EngineError::Stream(e.to_string()))?;
    out_stream
        .play()
        .map_err(|e| EngineError::Stream(e.to_string()))?;
    Ok((vec![in_stream, out_stream], xruns))
}

// ---------------------------------------------------------------------------
// Headless variants (no sound card) — used by tests and `--headless` hosts.
// ---------------------------------------------------------------------------

/// Starts a host session without audio hardware: network + control threads
/// run normally and the caller drives the pipeline by hand. Returns the
/// handle plus the parts the audio callback would normally own.
pub fn start_host_headless(
    cfg: HostConfig,
) -> Result<
    (
        SessionHandle,
        HostPipeline,
        rtrb::Producer<TxItem>,
        Arc<HostShared>,
    ),
    EngineError,
> {
    let socket = UdpSocket::bind(("0.0.0.0", cfg.port))?;
    let local_addr = socket.local_addr()?;
    let shared = Arc::new(HostShared::new(session_id_from_entropy()));
    let clock = Clock::new();
    let snapshot = new_shared(Role::Host, "starting...");
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (tx_prod, tx_cons) = rtrb::RingBuffer::<TxItem>::new(256);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (addr_rx_tx, addr_rx_rx) = crossbeam_channel::unbounded();
    let (addr_tx_tx, addr_tx_rx) = crossbeam_channel::unbounded();

    let threads = vec![
        spawn_rx(
            socket.try_clone()?,
            RxRole::Host {
                shared: shared.clone(),
            },
            ctrl_tx,
            addr_rx_rx,
            clock.clone(),
            cfg.params.frame_samples,
            cfg.params.sample_rate,
            shutdown.clone(),
        ),
        spawn_tx(
            socket.try_clone()?,
            tx_cons,
            addr_tx_rx,
            shutdown.clone(),
            cfg.params.simulate_loss,
        ),
        spawn_host_control(
            socket.try_clone()?,
            shared.clone(),
            HostControlConfig {
                room_code: cfg.room_code,
                host_name: cfg.name.clone(),
                params: cfg.params.clone(),
                hw_buffer: cfg.audio.hw_buffer,
            },
            ctrl_rx,
            addr_rx_tx,
            addr_tx_tx,
            clock.clone(),
            snapshot.clone(),
        ),
    ];

    let pipeline = HostPipeline::new(&cfg.params)?;
    Ok((
        SessionHandle {
            snapshot,
            clock,
            local_addr,
            shutdown,
            threads,
            _streams: vec![],
            xruns: Arc::new(AtomicU64::new(0)),
        },
        pipeline,
        tx_prod,
        shared,
    ))
}

/// Client-side counterpart of [`start_host_headless`].
pub fn start_client_headless(
    cfg: ClientConfig,
) -> Result<
    (
        SessionHandle,
        ClientPipeline,
        rtrb::Producer<TxItem>,
        Arc<ClientShared>,
    ),
    EngineError,
> {
    let socket = UdpSocket::bind(("0.0.0.0", 0))?;
    let local_addr = socket.local_addr()?;
    let shared = Arc::new(ClientShared::default());
    let clock = Clock::new();
    let snapshot = new_shared(Role::Client, format!("joining {} ...", cfg.host_addr));
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let (tx_prod, tx_cons) = rtrb::RingBuffer::<TxItem>::new(256);
    let (ctrl_tx, ctrl_rx) = crossbeam_channel::unbounded();
    let (_addr_rx_tx, addr_rx_rx) = crossbeam_channel::unbounded::<AddrUpdate>();
    let (addr_tx_tx, addr_tx_rx) = crossbeam_channel::unbounded();
    let _ = addr_tx_tx.send(AddrUpdate::Peer(cfg.host_addr));

    let threads = vec![
        spawn_rx(
            socket.try_clone()?,
            RxRole::Client {
                shared: shared.clone(),
                host_addr: cfg.host_addr,
            },
            ctrl_tx,
            addr_rx_rx,
            clock.clone(),
            cfg.params.frame_samples,
            cfg.params.sample_rate,
            shutdown.clone(),
        ),
        spawn_tx(
            socket.try_clone()?,
            tx_cons,
            addr_tx_rx,
            shutdown.clone(),
            cfg.params.simulate_loss,
        ),
        spawn_client_control(
            socket.try_clone()?,
            shared.clone(),
            ClientControlConfig {
                room_code: cfg.room_code,
                name: cfg.name.clone(),
                params: cfg.params.clone(),
                hw_buffer: cfg.audio.hw_buffer,
                host_addr: cfg.host_addr,
            },
            ctrl_rx,
            clock.clone(),
            snapshot.clone(),
        ),
    ];

    let pipeline = ClientPipeline::new(&cfg.params)?;
    Ok((
        SessionHandle {
            snapshot,
            clock,
            local_addr,
            shutdown,
            threads,
            _streams: vec![],
            xruns: Arc::new(AtomicU64::new(0)),
        },
        pipeline,
        tx_prod,
        shared,
    ))
}
