use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, StreamConfig};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::codec::SAMPLE_RATE;

/// Desired buffer size in samples for minimum latency (~2ms at 48kHz = 96 samples).
/// Used as a hint — falls back to device default if unsupported.
const DESIRED_BUFFER_SAMPLES: u32 = 96;

/// Ring buffer capacity in f32 samples (enough for ~200ms of audio).
/// Sized generously to handle Windows timer granularity (~15.6ms) and scheduling jitter.
const RING_BUFFER_CAPACITY: usize = 9600;

pub type CaptureProducer = ringbuf::HeapProd<f32>;
pub type CaptureConsumer = ringbuf::HeapCons<f32>;
pub type PlayoutProducer = ringbuf::HeapProd<f32>;
pub type PlayoutConsumer = ringbuf::HeapCons<f32>;

/// Create a lock-free SPSC ring buffer pair for audio samples.
pub fn create_audio_ring_buffer() -> (CaptureProducer, CaptureConsumer) {
    let rb = HeapRb::<f32>::new(RING_BUFFER_CAPACITY);
    rb.split()
}

/// Query available audio devices.
pub fn list_input_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices: Vec<String> = host
        .input_devices()
        .map_err(|e| anyhow!("failed to list input devices: {e}"))?
        .filter_map(|d| d.name().ok())
        .collect();
    Ok(devices)
}

pub fn list_output_devices() -> Result<Vec<String>> {
    let host = cpal::default_host();
    let devices: Vec<String> = host
        .output_devices()
        .map_err(|e| anyhow!("failed to list output devices: {e}"))?
        .filter_map(|d| d.name().ok())
        .collect();
    Ok(devices)
}

fn build_stream_config(channels: u16, buffer_size: BufferSize) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size,
    }
}

/// Build a list of configs to try, from most preferred to least.
/// Returns (config, is_device_default) pairs.
fn candidate_configs(device_default: Option<&StreamConfig>) -> Vec<StreamConfig> {
    let mut configs = vec![
        // 1. Mono, fixed low-latency buffer
        build_stream_config(1, BufferSize::Fixed(DESIRED_BUFFER_SAMPLES)),
        // 2. Mono, default buffer
        build_stream_config(1, BufferSize::Default),
    ];
    // 3. Device's native config (may be stereo) with default buffer
    if let Some(dev) = device_default {
        if dev.channels > 1 {
            configs.push(build_stream_config(
                dev.channels,
                BufferSize::Fixed(DESIRED_BUFFER_SAMPLES),
            ));
            configs.push(build_stream_config(dev.channels, BufferSize::Default));
        }
    }
    configs
}

/// Start capturing audio from the default input device.
/// Samples are pushed into the ring buffer producer.
pub fn start_capture(
    producer: CaptureProducer,
    running: Arc<AtomicBool>,
    device_name: Option<&str>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .input_devices()
            .map_err(|e| anyhow!("input devices: {e}"))?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .ok_or_else(|| anyhow!("input device '{name}' not found"))?,
        None => host
            .default_input_device()
            .ok_or_else(|| anyhow!("no default input device"))?,
    };

    let dev_name = device.name().unwrap_or_default();

    let device_default_config = device.default_input_config().ok().map(StreamConfig::from);

    let configs = candidate_configs(device_default_config.as_ref());

    let producer = Arc::new(std::sync::Mutex::new(producer));

    let make_err_cb = || -> Box<dyn FnMut(cpal::StreamError) + Send> {
        Box::new(|err| log::error!("capture stream error: {err}"))
    };

    type InputCb = Box<dyn FnMut(&[f32], &cpal::InputCallbackInfo) + Send>;
    let mut last_err = None;
    for config in &configs {
        log::info!("Capture device: {dev_name}, trying config: {config:?}");

        let channels = config.channels as usize;
        let prod = producer.clone();
        let run = running.clone();

        let data_cb: InputCb = if channels == 1 {
            let mut overflow_samples = 0usize;
            let mut overflow_log_time: Option<Instant> = None;
            Box::new(move |data, _info| {
                if !run.load(Ordering::Relaxed) {
                    return;
                }
                let mut p = prod.lock().unwrap();
                let written = p.push_slice(data);
                if written < data.len() {
                    overflow_samples += data.len() - written;
                    let should_log = overflow_log_time
                        .map(|t| t.elapsed() >= Duration::from_secs(2))
                        .unwrap_or(true);
                    if should_log {
                        log::warn!(
                            "capture ring buffer overflow: dropped {overflow_samples} samples total"
                        );
                        overflow_samples = 0;
                        overflow_log_time = Some(Instant::now());
                    }
                }
            })
        } else {
            // Downmix multi-channel to mono by averaging
            let mut overflow_samples = 0usize;
            let mut overflow_log_time: Option<Instant> = None;
            Box::new(move |data, _info| {
                if !run.load(Ordering::Relaxed) {
                    return;
                }
                let mono: Vec<f32> = data
                    .chunks_exact(channels)
                    .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                    .collect();
                let mut p = prod.lock().unwrap();
                let written = p.push_slice(&mono);
                if written < mono.len() {
                    overflow_samples += mono.len() - written;
                    let should_log = overflow_log_time
                        .map(|t| t.elapsed() >= Duration::from_secs(2))
                        .unwrap_or(true);
                    if should_log {
                        log::warn!(
                            "capture ring buffer overflow: dropped {overflow_samples} samples total"
                        );
                        overflow_samples = 0;
                        overflow_log_time = Some(Instant::now());
                    }
                }
            })
        };

        match device.build_input_stream(config, data_cb, make_err_cb(), None) {
            Ok(stream) => {
                if channels > 1 {
                    log::info!("Capture using {channels}-channel config, downmixing to mono");
                } else {
                    log::info!("Capture using mono config");
                }
                stream.play()?;
                return Ok(stream);
            }
            Err(e) => {
                log::warn!("Config rejected ({e}), trying next...");
                last_err = Some(e);
            }
        }
    }

    Err(anyhow!(
        "no supported capture config for device '{dev_name}': {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}

/// Start playout to the default output device.
/// Samples are pulled from the ring buffer consumer.
pub fn start_playout(
    consumer: PlayoutConsumer,
    running: Arc<AtomicBool>,
    device_name: Option<&str>,
) -> Result<cpal::Stream> {
    let host = cpal::default_host();
    let device = match device_name {
        Some(name) => host
            .output_devices()
            .map_err(|e| anyhow!("output devices: {e}"))?
            .find(|d| d.name().is_ok_and(|n| n == name))
            .ok_or_else(|| anyhow!("output device '{name}' not found"))?,
        None => host
            .default_output_device()
            .ok_or_else(|| anyhow!("no default output device"))?,
    };

    let dev_name = device.name().unwrap_or_default();

    let device_default_config = device.default_output_config().ok().map(StreamConfig::from);

    let configs = candidate_configs(device_default_config.as_ref());

    let consumer = Arc::new(std::sync::Mutex::new(consumer));

    let make_err_cb = || -> Box<dyn FnMut(cpal::StreamError) + Send> {
        Box::new(|err| log::error!("playout stream error: {err}"))
    };

    type OutputCb = Box<dyn FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send>;
    let mut last_err = None;
    for config in &configs {
        log::info!("Playout device: {dev_name}, trying config: {config:?}");

        let channels = config.channels as usize;
        let cons = consumer.clone();
        let run = running.clone();

        let data_cb: OutputCb = if channels == 1 {
            Box::new(move |data, _info| {
                if !run.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }
                let mut c = cons.lock().unwrap();
                let read = c.pop_slice(data);
                if read < data.len() {
                    data[read..].fill(0.0);
                }
            })
        } else {
            // Upmix mono to multi-channel by duplicating
            Box::new(move |data, _info| {
                if !run.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }
                let mono_frames = data.len() / channels;
                let mut mono_buf = vec![0.0f32; mono_frames];
                let mut c = cons.lock().unwrap();
                let read = c.pop_slice(&mut mono_buf);
                drop(c);
                for i in 0..mono_frames {
                    let sample = if i < read { mono_buf[i] } else { 0.0 };
                    for ch in 0..channels {
                        data[i * channels + ch] = sample;
                    }
                }
            })
        };

        match device.build_output_stream(config, data_cb, make_err_cb(), None) {
            Ok(stream) => {
                if channels > 1 {
                    log::info!("Playout using {channels}-channel config, upmixing from mono");
                } else {
                    log::info!("Playout using mono config");
                }
                stream.play()?;
                return Ok(stream);
            }
            Err(e) => {
                log::warn!("Config rejected ({e}), trying next...");
                last_err = Some(e);
            }
        }
    }

    Err(anyhow!(
        "no supported playout config for device '{dev_name}': {}",
        last_err.map(|e| e.to_string()).unwrap_or_default()
    ))
}
