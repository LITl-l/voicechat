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
/// Used as a hint — falls back to device default if unsupported.
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

fn build_stream_config(buffer_size: BufferSize) -> StreamConfig {
    StreamConfig {
        channels: 1,
        sample_rate: SampleRate(SAMPLE_RATE),
        buffer_size,
    }
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

    // Wrap producer in Arc<Mutex> so we can retry with a different config if needed.
    let producer = Arc::new(std::sync::Mutex::new(producer));

    let make_data_cb = |prod: Arc<std::sync::Mutex<CaptureProducer>>,
                        run: Arc<AtomicBool>|
     -> Box<dyn FnMut(&[f32], &cpal::InputCallbackInfo) + Send> {
        Box::new(move |data, _info| {
            if !run.load(Ordering::Relaxed) {
                return;
            }
            let mut p = prod.lock().unwrap();
            let written = p.push_slice(data);
            if written < data.len() {
                log::warn!(
                    "capture ring buffer overflow: dropped {} samples",
                    data.len() - written
                );
            }
        })
    };

    let make_err_cb = || -> Box<dyn FnMut(cpal::StreamError) + Send> {
        Box::new(|err| log::error!("capture stream error: {err}"))
    };

    // Try low-latency fixed buffer first
    let config = build_stream_config(BufferSize::Fixed(DESIRED_BUFFER_SAMPLES));
    log::info!("Capture device: {dev_name}, trying config: {config:?}");

    let stream = match device.build_input_stream(
        &config,
        make_data_cb(producer.clone(), running.clone()),
        make_err_cb(),
        None,
    ) {
        Ok(s) => {
            log::info!("Capture using fixed buffer size: {DESIRED_BUFFER_SAMPLES}");
            s
        }
        Err(first_err) => {
            let fallback = build_stream_config(BufferSize::Default);
            log::warn!(
                "Fixed buffer size rejected ({first_err}), falling back to: {fallback:?}"
            );
            device.build_input_stream(
                &fallback,
                make_data_cb(producer, running),
                make_err_cb(),
                None,
            )?
        }
    };

    stream.play()?;
    Ok(stream)
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
    let consumer = Arc::new(std::sync::Mutex::new(consumer));

    let make_data_cb = |cons: Arc<std::sync::Mutex<PlayoutConsumer>>,
                        run: Arc<AtomicBool>|
     -> Box<dyn FnMut(&mut [f32], &cpal::OutputCallbackInfo) + Send> {
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
    };

    let make_err_cb = || -> Box<dyn FnMut(cpal::StreamError) + Send> {
        Box::new(|err| log::error!("playout stream error: {err}"))
    };

    let config = build_stream_config(BufferSize::Fixed(DESIRED_BUFFER_SAMPLES));
    log::info!("Playout device: {dev_name}, trying config: {config:?}");

    let stream = match device.build_output_stream(
        &config,
        make_data_cb(consumer.clone(), running.clone()),
        make_err_cb(),
        None,
    ) {
        Ok(s) => {
            log::info!("Playout using fixed buffer size: {DESIRED_BUFFER_SAMPLES}");
            s
        }
        Err(first_err) => {
            let fallback = build_stream_config(BufferSize::Default);
            log::warn!(
                "Fixed buffer size rejected ({first_err}), falling back to: {fallback:?}"
            );
            device.build_output_stream(
                &fallback,
                make_data_cb(consumer, running),
                make_err_cb(),
                None,
            )?
        }
    };

    stream.play()?;
    Ok(stream)
}
