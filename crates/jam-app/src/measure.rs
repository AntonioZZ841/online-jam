//! Latency measurement harnesses.
//!
//! Two complementary measurements (documented in the README):
//!
//! - **Network path** (`--host`, works headless): joins a host running
//!   `--echo`, injects clicks straight into the send pipeline, and times
//!   their return in the decoded stream. Captures network + both jitter
//!   buffers + codec + host processing — everything except local device I/O.
//! - **Device path** (`--loopback`, needs a physical output→input cable):
//!   plays clicks out of the interface and times their arrival back at the
//!   input. Captures driver/hardware buffers, DA/AD conversion.
//!
//! True mouth-to-ear latency ≈ network measurement + device measurement.

use anyhow::{bail, Context};
use jam_core::engine::{start_client_headless, ClientConfig};
use jam_core::EngineParams;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const CLICK_INTERVAL_FRAMES: usize = 200; // one click every 0.5 s at 2.5 ms
const CLICKS: usize = 8;

pub struct MeasureResult {
    pub samples_ms: Vec<f64>,
}

impl MeasureResult {
    pub fn print(&self, label: &str) {
        if self.samples_ms.is_empty() {
            println!("{label}: no clicks detected");
            return;
        }
        let mut sorted = self.samples_ms.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let min = sorted[0];
        let median = sorted[sorted.len() / 2];
        let max = sorted[sorted.len() - 1];
        println!(
            "{label}: min {min:.1} ms · median {median:.1} ms · max {max:.1} ms ({} clicks)",
            sorted.len()
        );
    }
}

/// Measures the round trip through a host running `--echo`, headlessly.
pub fn net_measure(
    host_addr: std::net::SocketAddr,
    code: [u8; 6],
    params: EngineParams,
) -> anyhow::Result<MeasureResult> {
    let frame = params.frame_samples;
    let (handle, mut pipeline, mut tx, shared) = start_client_headless(ClientConfig {
        host_addr,
        room_code: code,
        name: "measure".into(),
        params,
        audio: Default::default(),
    })
    .context("starting measurement session")?;

    // Wait for the handshake.
    let deadline = Instant::now() + Duration::from_secs(6);
    while !shared.joined.load(Ordering::Acquire) {
        if Instant::now() > deadline {
            handle.shutdown();
            bail!("could not join {host_addr} (is the host running with --echo?)");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("joined {host_addr}; measuring the echo path...");

    let frame_ms = frame as f64 * 1_000.0 / 48_000.0;
    let mut input = vec![0.0f32; frame];
    let mut output = vec![0.0f32; frame];
    let mut results = vec![];
    let mut last_click_frame: Option<usize> = None;

    let total_frames = CLICK_INTERVAL_FRAMES * (CLICKS + 1);
    let start = Instant::now();
    for i in 0..total_frames {
        input.fill(0.0);
        if i % CLICK_INTERVAL_FRAMES == 10 && i / CLICK_INTERVAL_FRAMES < CLICKS {
            // A one-frame burst, loud enough to survive the codec.
            for (k, x) in input.iter_mut().enumerate() {
                *x = if k % 2 == 0 { 0.9 } else { -0.9 };
            }
            last_click_frame = Some(i);
        }
        pipeline.process_frame(&shared, &input, &mut output, |dest, bytes| {
            let _ = tx.push(jam_core::net::TxItem::new(dest, bytes));
        });
        if let Some(click_at) = last_click_frame {
            let peak = output.iter().fold(0.0f32, |a, &x| a.max(x.abs()));
            if peak > 0.25 {
                results.push((i - click_at) as f64 * frame_ms);
                last_click_frame = None;
            }
        }
        // Pace at real time.
        let target = start + Duration::from_micros((i as u64 + 1) * (frame_ms * 1_000.0) as u64);
        if let Some(sleep) = target.checked_duration_since(Instant::now()) {
            std::thread::sleep(sleep);
        }
    }
    handle.shutdown();
    Ok(MeasureResult {
        samples_ms: results,
    })
}

/// Measures through a physical loopback cable on the local interface.
pub fn loopback_measure(
    input_name: Option<&str>,
    output_name: Option<&str>,
    buffer: u32,
) -> anyhow::Result<MeasureResult> {
    use cpal::traits::{DeviceTrait, StreamTrait};
    use jam_audio::device::{open_input, open_output};
    use std::sync::atomic::{AtomicBool, AtomicU64};
    use std::sync::{Arc, Mutex};

    let input = open_input(input_name, buffer).context("opening input device")?;
    let output = open_output(output_name, buffer).context("opening output device")?;

    let epoch = Instant::now();
    let click_time_us = Arc::new(AtomicU64::new(0));
    let armed = Arc::new(AtomicBool::new(false));
    let results: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(vec![]));

    let out_channels = output.channels as usize;
    let ct = click_time_us.clone();
    let arm = armed.clone();
    let mut sample_count: u64 = 0;
    let out_stream = output
        .device
        .build_output_stream(
            &output.config,
            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                data.fill(0.0);
                for (i, frame) in data.chunks_exact_mut(out_channels).enumerate() {
                    // A click every 0.5 s: 32 samples of alternating polarity.
                    let pos = (sample_count + i as u64) % 24_000;
                    if pos < 32 {
                        let v = if pos.is_multiple_of(2) { 0.9 } else { -0.9 };
                        for ch in frame.iter_mut() {
                            *ch = v;
                        }
                        if pos == 0 {
                            ct.store(epoch.elapsed().as_micros() as u64, Ordering::Release);
                            arm.store(true, Ordering::Release);
                        }
                    }
                }
                sample_count += data.len() as u64 / out_channels as u64;
            },
            |_| {},
            None,
        )
        .map_err(|e| anyhow::anyhow!("output stream: {e}"))?;

    let in_channels = input.channels as usize;
    let ct = click_time_us.clone();
    let arm = armed.clone();
    let res = results.clone();
    let in_stream = input
        .device
        .build_input_stream(
            &input.config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                if !arm.load(Ordering::Acquire) {
                    return;
                }
                for frame in data.chunks_exact(in_channels) {
                    if frame[0].abs() > 0.3 {
                        let now = epoch.elapsed().as_micros() as u64;
                        let clicked = ct.load(Ordering::Acquire);
                        let delta_ms = (now.saturating_sub(clicked)) as f64 / 1_000.0;
                        if delta_ms < 400.0 {
                            res.lock().unwrap().push(delta_ms);
                        }
                        arm.store(false, Ordering::Release);
                        break;
                    }
                }
            },
            |_| {},
            None,
        )
        .map_err(|e| anyhow::anyhow!("input stream: {e}"))?;

    println!("playing clicks; make sure the loopback cable connects output → input...");
    in_stream.play().map_err(|e| anyhow::anyhow!("{e}"))?;
    out_stream.play().map_err(|e| anyhow::anyhow!("{e}"))?;
    std::thread::sleep(Duration::from_millis(500 * CLICKS as u64 + 500));
    drop(out_stream);
    drop(in_stream);

    let samples_ms = results.lock().unwrap().clone();
    Ok(MeasureResult { samples_ms })
}
