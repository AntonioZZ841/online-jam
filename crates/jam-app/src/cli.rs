//! Command-line interface definitions.

use clap::{Args, Parser, Subcommand};
use jam_core::{EngineParams, MonitorMode};
use jam_protocol::packet::Codec;
use jam_protocol::{DEFAULT_PORT, ROOM_CODE_LEN};

#[derive(Parser)]
#[command(
    name = "jam",
    about = "Low-latency online jam sessions: one player hosts, the others join.",
    version
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// List audio devices.
    Devices,
    /// Host a session on this machine (you play too).
    Host(HostArgs),
    /// Join a session hosted by another player.
    Join(JoinArgs),
    /// Measure network round-trip time to a host (no audio needed).
    Ping(PingArgs),
    /// Measure audio round-trip latency with a loopback cable or `--echo` host.
    Measure(MeasureArgs),
}

#[derive(Args, Clone)]
pub struct AudioArgs {
    /// Input device name substring (default: system default).
    #[arg(long)]
    pub input: Option<String>,
    /// Output device name substring (default: system default).
    #[arg(long)]
    pub output: Option<String>,
    /// Hardware buffer size in samples (64/128/256; smaller = lower latency).
    #[arg(long, default_value_t = 128)]
    pub buffer: u32,
    /// Network frame length in ms (2.5 or 5).
    #[arg(long, default_value = "2.5")]
    pub frame: String,
    /// Opus bitrate in kbit/s.
    #[arg(long, default_value_t = 96)]
    pub bitrate: u32,
    /// Send each frame twice (previous frame piggybacked) to repair single
    /// packet losses at the cost of double audio bandwidth.
    #[arg(long)]
    pub redundancy: bool,
    /// Self-monitoring: direct (local, instant), mix (through the host), off
    /// (use your interface's hardware monitoring).
    #[arg(long, default_value = "direct")]
    pub monitor: String,
    /// Fix the jitter buffer at N frames instead of adapting (testing).
    #[arg(long)]
    pub jitter: Option<i32>,
    /// Disable continuous clock-drift correction (falls back to occasional
    /// frame drop/insert at quiet moments).
    #[arg(long)]
    pub no_drift: bool,
    /// Use raw PCM instead of Opus (LAN only; ~1 Mbit/s per stream).
    #[arg(long)]
    pub pcm: bool,
    /// Drop this percentage of outgoing packets (network-impairment testing).
    #[arg(long, default_value_t = 0.0, hide = true)]
    pub simulate_loss: f32,
}

impl AudioArgs {
    pub fn params(&self, echo: bool) -> anyhow::Result<EngineParams> {
        let frame_samples = match self.frame.as_str() {
            "2.5" => 120,
            "5" => 240,
            other => anyhow::bail!("--frame must be 2.5 or 5 (got {other})"),
        };
        let monitor = match self.monitor.as_str() {
            "direct" => MonitorMode::Direct,
            "mix" => MonitorMode::ThroughMix,
            "off" => MonitorMode::Off,
            other => anyhow::bail!("--monitor must be direct, mix, or off (got {other})"),
        };
        if !matches!(self.buffer, 32 | 64 | 128 | 256 | 512) {
            anyhow::bail!("--buffer must be one of 32/64/128/256/512");
        }
        Ok(EngineParams {
            sample_rate: jam_audio::SAMPLE_RATE,
            frame_samples,
            codec: if self.pcm { Codec::PcmF32 } else { Codec::Opus },
            bitrate_bps: (self.bitrate * 1_000) as i32,
            redundancy: self.redundancy,
            monitor,
            jitter_fixed: self.jitter,
            drift_correction: !self.no_drift,
            simulate_loss: self.simulate_loss / 100.0,
            echo,
        })
    }
}

#[derive(Args)]
pub struct HostArgs {
    /// UDP port to listen on (forward this port on your router).
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,
    /// Room code players must present (default: random).
    #[arg(long)]
    pub code: Option<String>,
    /// Your display name.
    #[arg(long, default_value = "host")]
    pub name: String,
    /// Loop each player's own audio straight back (for `jam measure`).
    #[arg(long, hide = true)]
    pub echo: bool,
    /// Skip the public-IP (STUN) lookup.
    #[arg(long)]
    pub no_stun: bool,
    /// Open a graphical session window instead of the terminal display
    /// (requires a build with `--features gui`).
    #[arg(long)]
    pub gui: bool,
    #[command(flatten)]
    pub audio: AudioArgs,
}

#[derive(Args)]
pub struct JoinArgs {
    /// Host address, e.g. 203.0.113.7:47820 or bandmate.example:47820.
    pub host: String,
    /// Room code from the host.
    #[arg(long)]
    pub code: String,
    /// Your display name.
    #[arg(long, default_value = "player")]
    pub name: String,
    /// Open a graphical session window instead of the terminal display
    /// (requires a build with `--features gui`).
    #[arg(long)]
    pub gui: bool,
    #[command(flatten)]
    pub audio: AudioArgs,
}

#[derive(Args)]
pub struct PingArgs {
    /// Host address, e.g. 203.0.113.7:47820.
    pub host: String,
    /// Number of pings to send.
    #[arg(short = 'n', long, default_value_t = 10)]
    pub count: u32,
}

#[derive(Args)]
pub struct MeasureArgs {
    /// Measure through a physical loopback cable on the local interface.
    #[arg(long, conflicts_with = "host")]
    pub loopback: bool,
    /// Measure end-to-end against a host running with `--echo`.
    #[arg(long)]
    pub host: Option<String>,
    /// Room code (required with --host).
    #[arg(long)]
    pub code: Option<String>,
    #[command(flatten)]
    pub audio: AudioArgs,
}

/// Parse or generate a 6-character room code (A-Z, 0-9).
pub fn room_code(input: Option<&str>) -> anyhow::Result<[u8; ROOM_CODE_LEN]> {
    match input {
        Some(s) => {
            let up = s.to_uppercase();
            let bytes = up.as_bytes();
            if bytes.len() != ROOM_CODE_LEN || !bytes.iter().all(|b| b.is_ascii_alphanumeric()) {
                anyhow::bail!("room code must be exactly 6 letters/digits (e.g. BLUES1)");
            }
            let mut code = [0u8; ROOM_CODE_LEN];
            code.copy_from_slice(bytes);
            Ok(code)
        }
        None => {
            const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
            let mut state = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x1234_5678)
                ^ (std::process::id() as u64) << 32;
            let mut code = [0u8; ROOM_CODE_LEN];
            for c in code.iter_mut() {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *c = ALPHABET[(state >> 56) as usize % ALPHABET.len()];
            }
            Ok(code)
        }
    }
}
