use anyhow::{anyhow, Result};
use audiopus::{
    coder::{Decoder as OpusDecoder, Encoder as OpusEncoder},
    Application, Bitrate, Channels, MutSignals, SampleRate,
    packet::Packet as OpusPacket,
};
use std::convert::TryFrom;

/// Samples per frame at 48kHz for 2.5ms: 48000 * 0.0025 = 120.
pub const FRAME_SAMPLES: usize = 120;
/// Frame duration in seconds.
pub const FRAME_DURATION_SECS: f64 = 0.0025;
/// Frame duration in microseconds.
pub const FRAME_DURATION_US: u64 = 2500;
/// Sample rate.
pub const SAMPLE_RATE: u32 = 48000;
/// Max encoded packet size (generous upper bound).
pub const MAX_ENCODED_SIZE: usize = 256;

pub struct Encoder {
    inner: OpusEncoder,
}

impl Encoder {
    pub fn new() -> Result<Self> {
        let mut encoder = OpusEncoder::new(
            SampleRate::Hz48000,
            Channels::Mono,
            Application::LowDelay,
        )
        .map_err(|e| anyhow!("opus encoder init: {e}"))?;

        encoder
            .set_bitrate(Bitrate::BitsPerSecond(64000))
            .map_err(|e| anyhow!("set bitrate: {e}"))?;

        encoder
            .set_inband_fec(true)
            .map_err(|e| anyhow!("set fec: {e}"))?;

        encoder
            .set_packet_loss_perc(5)
            .map_err(|e| anyhow!("set loss perc: {e}"))?;

        Ok(Self { inner: encoder })
    }

    /// Encode a frame of `FRAME_SAMPLES` i16 samples into Opus.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Vec<u8>> {
        if pcm.len() != FRAME_SAMPLES {
            return Err(anyhow!(
                "expected {} samples, got {}",
                FRAME_SAMPLES,
                pcm.len()
            ));
        }
        let mut output = vec![0u8; MAX_ENCODED_SIZE];
        let len = self
            .inner
            .encode(pcm, &mut output)
            .map_err(|e| anyhow!("opus encode: {e}"))?;
        output.truncate(len.into());
        Ok(output)
    }
}

pub struct Decoder {
    inner: OpusDecoder,
}

impl Decoder {
    pub fn new() -> Result<Self> {
        let decoder = OpusDecoder::new(SampleRate::Hz48000, Channels::Mono)
            .map_err(|e| anyhow!("opus decoder init: {e}"))?;
        Ok(Self { inner: decoder })
    }

    /// Decode an Opus packet into `FRAME_SAMPLES` i16 samples.
    pub fn decode(&mut self, opus_data: &[u8]) -> Result<Vec<i16>> {
        let mut output = vec![0i16; FRAME_SAMPLES];
        let packet = OpusPacket::try_from(opus_data)
            .map_err(|e| anyhow!("invalid opus packet: {e}"))?;
        let mut_signals = MutSignals::try_from(&mut output)
            .map_err(|e| anyhow!("mut signals: {e}"))?;
        let decoded = self
            .inner
            .decode(Some(packet), mut_signals, false)
            .map_err(|e| anyhow!("opus decode: {e}"))?;
        output.truncate(decoded);
        Ok(output)
    }

    /// Packet loss concealment: generate comfort audio when a packet is missing.
    pub fn decode_plc(&mut self) -> Result<Vec<i16>> {
        let mut output = vec![0i16; FRAME_SAMPLES];
        let mut_signals = MutSignals::try_from(&mut output)
            .map_err(|e| anyhow!("mut signals: {e}"))?;
        let decoded = self
            .inner
            .decode(None, mut_signals, false)
            .map_err(|e| anyhow!("opus PLC: {e}"))?;
        output.truncate(decoded);
        Ok(output)
    }
}

/// Convert f32 PCM [-1.0, 1.0] to i16 samples.
pub fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| {
            let clamped = s.clamp(-1.0, 1.0);
            (clamped * i16::MAX as f32) as i16
        })
        .collect()
}

/// Convert i16 samples to f32 PCM [-1.0, 1.0].
pub fn i16_to_f32(samples: &[i16]) -> Vec<f32> {
    samples
        .iter()
        .map(|&s| s as f32 / i16::MAX as f32)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut encoder = Encoder::new().unwrap();
        let mut decoder = Decoder::new().unwrap();

        // Sine wave test signal
        let pcm: Vec<i16> = (0..FRAME_SAMPLES)
            .map(|i| {
                let t = i as f64 / SAMPLE_RATE as f64;
                (f64::sin(2.0 * std::f64::consts::PI * 440.0 * t) * 10000.0) as i16
            })
            .collect();

        let encoded = encoder.encode(&pcm).unwrap();
        assert!(!encoded.is_empty());
        assert!(encoded.len() < MAX_ENCODED_SIZE);

        let decoded = decoder.decode(&encoded).unwrap();
        assert_eq!(decoded.len(), FRAME_SAMPLES);
    }

    #[test]
    fn test_plc() {
        let mut decoder = Decoder::new().unwrap();
        let plc = decoder.decode_plc().unwrap();
        assert_eq!(plc.len(), FRAME_SAMPLES);
    }

    #[test]
    fn test_f32_i16_conversion() {
        let f32_samples = vec![0.0f32, 1.0, -1.0, 0.5, -0.5];
        let i16_samples = f32_to_i16(&f32_samples);
        let back = i16_to_f32(&i16_samples);
        for (orig, conv) in f32_samples.iter().zip(back.iter()) {
            assert!((orig - conv).abs() < 0.001, "{orig} vs {conv}");
        }
    }
}
