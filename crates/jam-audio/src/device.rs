//! cpal device discovery and stream-config negotiation.
//!
//! Policy (MVP): the session runs at 48 kHz mono f32 end to end. We pick the
//! requested (or default) device, ask for a 48 kHz f32 config with the
//! requested buffer size, and fail with an actionable error if the device
//! cannot do that (see README — almost every interface can).

use crate::SAMPLE_RATE;
use cpal::traits::{DeviceTrait, HostTrait};
use cpal::{BufferSize, Device, SampleFormat, StreamConfig, SupportedBufferSize};

#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error("no {kind} device found{name}", name = match .name { Some(n) => format!(" matching \"{n}\""), None => String::new() })]
    NotFound {
        kind: &'static str,
        name: Option<String>,
    },
    #[error("device \"{0}\" does not support 48 kHz f32 {1}: {2}")]
    Unsupported(String, &'static str, String),
    #[error("cpal: {0}")]
    Cpal(String),
}

pub struct DeviceInfo {
    pub name: String,
    pub is_input: bool,
    pub is_output: bool,
    pub default_input: bool,
    pub default_output: bool,
}

/// Enumerate devices for `jam devices`.
pub fn list_devices() -> Result<Vec<DeviceInfo>, DeviceError> {
    let host = cpal::default_host();
    let default_in = host.default_input_device().and_then(|d| d.name().ok());
    let default_out = host.default_output_device().and_then(|d| d.name().ok());
    let mut infos: Vec<DeviceInfo> = Vec::new();
    let devices = host
        .devices()
        .map_err(|e| DeviceError::Cpal(e.to_string()))?;
    for device in devices {
        let name = device.name().unwrap_or_else(|_| "<unknown>".into());
        let is_input = device
            .supported_input_configs()
            .map(|mut c| c.next().is_some())
            .unwrap_or(false);
        let is_output = device
            .supported_output_configs()
            .map(|mut c| c.next().is_some())
            .unwrap_or(false);
        infos.push(DeviceInfo {
            default_input: default_in.as_deref() == Some(&name),
            default_output: default_out.as_deref() == Some(&name),
            name,
            is_input,
            is_output,
        });
    }
    Ok(infos)
}

fn find_device(name: Option<&str>, input: bool) -> Result<Device, DeviceError> {
    let host = cpal::default_host();
    let kind = if input { "input" } else { "output" };
    match name {
        None => {
            let dev = if input {
                host.default_input_device()
            } else {
                host.default_output_device()
            };
            dev.ok_or(DeviceError::NotFound { kind, name: None })
        }
        Some(pattern) => {
            let devices = host
                .devices()
                .map_err(|e| DeviceError::Cpal(e.to_string()))?;
            for device in devices {
                let dev_name = device.name().unwrap_or_default();
                if dev_name.to_lowercase().contains(&pattern.to_lowercase()) {
                    let usable = if input {
                        device
                            .supported_input_configs()
                            .map(|mut c| c.next().is_some())
                    } else {
                        device
                            .supported_output_configs()
                            .map(|mut c| c.next().is_some())
                    };
                    if usable.unwrap_or(false) {
                        return Ok(device);
                    }
                }
            }
            Err(DeviceError::NotFound {
                kind,
                name: Some(pattern.to_string()),
            })
        }
    }
}

/// A negotiated device + config pair ready for stream building.
pub struct Negotiated {
    pub device: Device,
    pub config: StreamConfig,
    /// Channel count of the raw device stream; we take/duplicate channel 0.
    pub channels: u16,
}

fn negotiate(device: Device, input: bool, buffer_frames: u32) -> Result<Negotiated, DeviceError> {
    let dev_name = device.name().unwrap_or_else(|_| "<unknown>".into());
    let kind = if input { "input" } else { "output" };
    let mut best: Option<(u16, SupportedBufferSize)> = None;
    let configs: Vec<_> = if input {
        device
            .supported_input_configs()
            .map_err(|e| DeviceError::Unsupported(dev_name.clone(), kind, e.to_string()))?
            .collect()
    } else {
        device
            .supported_output_configs()
            .map_err(|e| DeviceError::Unsupported(dev_name.clone(), kind, e.to_string()))?
            .collect()
    };
    for cfg in configs {
        if cfg.sample_format() != SampleFormat::F32 {
            continue;
        }
        if cfg.min_sample_rate().0 > SAMPLE_RATE || cfg.max_sample_rate().0 < SAMPLE_RATE {
            continue;
        }
        // Prefer the fewest channels (mono capture ideal, stereo fine).
        let candidate = (cfg.channels(), *cfg.buffer_size());
        match &best {
            Some((ch, _)) if *ch <= cfg.channels() => {}
            _ => best = Some(candidate),
        }
    }
    let Some((channels, supported_buffer)) = best else {
        return Err(DeviceError::Unsupported(
            dev_name,
            kind,
            "no f32 config covering 48000 Hz".into(),
        ));
    };
    let buffer_size = match supported_buffer {
        SupportedBufferSize::Range { min, max } => BufferSize::Fixed(buffer_frames.clamp(min, max)),
        SupportedBufferSize::Unknown => BufferSize::Default,
    };
    Ok(Negotiated {
        device,
        config: StreamConfig {
            channels,
            sample_rate: cpal::SampleRate(SAMPLE_RATE),
            buffer_size,
        },
        channels,
    })
}

pub fn open_input(name: Option<&str>, buffer_frames: u32) -> Result<Negotiated, DeviceError> {
    negotiate(find_device(name, true)?, true, buffer_frames)
}

pub fn open_output(name: Option<&str>, buffer_frames: u32) -> Result<Negotiated, DeviceError> {
    negotiate(find_device(name, false)?, false, buffer_frames)
}
