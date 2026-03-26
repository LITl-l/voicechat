use anyhow::{anyhow, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, SampleRate, StreamConfig};
use ringbuf::{
    traits::{Consumer, Producer, Split},
    HeapRb,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crate::codec::SAMPLE_RATE;

/// Desired buffer size in samples for minimum latency (~2ms at 48kHz = 96 samples).
const DESIRED_BUFFER_SAMPLES: u32 = 96;

/// Ring buffer capacity in f32 samples (enough for ~50ms of audio).
const RING_BUFFER_CAPACITY: usize = 2400;

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

fn build_stream_config() -> StreamConfig {
    StreamConfig {
        channels: 1,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size: BufferSize::Fixed(DESIRED_BUFFER_SAMPLES),
    }
}

/// Start capturing audio from the default input device.
/// Samples are pushed into the ring buffer producer.
pub fn start_capture(
    mut producer: CaptureProducer,
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

    let config = build_stream_config();
    log::info!(
        "Capture device: {}, config: {:?}",
        device.name().unwrap_or_default(),
        config
    );

    let running_clone = running.clone();
    let stream = device.build_input_stream(
        &config,
        move |data: &[f32], _info: &cpal::InputCallbackInfo| {
            if !running_clone.load(Ordering::Relaxed) {
                return;
            }
            let written = producer.push_slice(data);
            if written < data.len() {
                log::warn!(
                    "capture ring buffer overflow: dropped {} samples",
                    data.len() - written
                );
            }
        },
        move |err| {
            log::error!("capture stream error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok(stream)
}

/// Start playout to the default output device.
/// Samples are pulled from the ring buffer consumer.
pub fn start_playout(
    mut consumer: PlayoutConsumer,
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

    let config = build_stream_config();
    log::info!(
        "Playout device: {}, config: {:?}",
        device.name().unwrap_or_default(),
        config
    );

    let running_clone = running.clone();
    let stream = device.build_output_stream(
        &config,
        move |data: &mut [f32], _info: &cpal::OutputCallbackInfo| {
            if !running_clone.load(Ordering::Relaxed) {
                data.fill(0.0);
                return;
            }
            let read = consumer.pop_slice(data);
            // Fill remaining with silence if ring buffer underflows
            if read < data.len() {
                data[read..].fill(0.0);
            }
        },
        move |err| {
            log::error!("playout stream error: {err}");
        },
        None,
    )?;

    stream.play()?;
    Ok(stream)
}
