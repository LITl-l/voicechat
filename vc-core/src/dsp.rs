use nnnoiseless::DenoiseState;

/// RNNoise frame size: 480 samples = 10ms at 48kHz.
const DENOISE_FRAME_SIZE: usize = 480;

/// Noise suppressor wrapping RNNoise (via nnnoiseless).
///
/// RNNoise operates on 480-sample (10ms) frames at 48kHz.
/// Since our Opus encoder uses 120-sample (2.5ms) frames, this module
/// accumulates 4 encoder frames before processing, adding ~7.5ms latency.
pub struct NoiseSuppressor {
    state: Box<DenoiseState<'static>>,
    /// Accumulator for incoming samples.
    accum: Vec<f32>,
    /// Processed output waiting to be consumed.
    output_buf: Vec<f32>,
    /// Last VAD probability from RNNoise (0.0 = silence, 1.0 = voice).
    last_vad_prob: f32,
    /// Whether noise suppression is enabled.
    enabled: bool,
}

impl Default for NoiseSuppressor {
    fn default() -> Self {
        Self::new()
    }
}

impl NoiseSuppressor {
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            accum: Vec::with_capacity(DENOISE_FRAME_SIZE),
            output_buf: Vec::new(),
            last_vad_prob: 0.0,
            enabled: true,
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Feed audio samples into the suppressor.
    /// Returns denoised samples when a full 480-sample block has been processed,
    /// or immediately if suppression is disabled.
    pub fn process(&mut self, frame: &[f32]) -> Option<Vec<f32>> {
        if !self.enabled {
            return Some(frame.to_vec());
        }

        self.accum.extend_from_slice(frame);

        if self.accum.len() >= DENOISE_FRAME_SIZE {
            let mut denoise_frame = [0.0f32; DENOISE_FRAME_SIZE];
            denoise_frame.copy_from_slice(&self.accum[..DENOISE_FRAME_SIZE]);
            self.accum.drain(..DENOISE_FRAME_SIZE);

            // RNNoise expects samples in [-32768, 32768] range (i16 scale as f32)
            for s in denoise_frame.iter_mut() {
                *s *= 32768.0;
            }

            let mut output_frame = [0.0f32; DENOISE_FRAME_SIZE];
            self.last_vad_prob = self.state.process_frame(&mut output_frame, &denoise_frame);
            denoise_frame.copy_from_slice(&output_frame);

            // Scale back to [-1.0, 1.0]
            for s in denoise_frame.iter_mut() {
                *s /= 32768.0;
            }

            self.output_buf.extend_from_slice(&denoise_frame);
        }

        if !self.output_buf.is_empty() {
            Some(std::mem::take(&mut self.output_buf))
        } else {
            None
        }
    }

    /// Last VAD probability from RNNoise (0.0 = noise, 1.0 = voice).
    pub fn vad_probability(&self) -> f32 {
        self.last_vad_prob
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FRAME_SAMPLES;

    #[test]
    fn test_noise_suppressor_accumulates() {
        let mut ns = NoiseSuppressor::new();
        let frame = vec![0.0f32; FRAME_SAMPLES]; // 120 samples

        // First 3 frames: accumulating, no output
        assert!(ns.process(&frame).is_none());
        assert!(ns.process(&frame).is_none());
        assert!(ns.process(&frame).is_none());

        // 4th frame: 480 samples accumulated, should produce output
        let output = ns.process(&frame);
        assert!(output.is_some());
        assert_eq!(output.unwrap().len(), DENOISE_FRAME_SIZE);
    }

    #[test]
    fn test_noise_suppressor_disabled_passthrough() {
        let mut ns = NoiseSuppressor::new();
        ns.set_enabled(false);

        let frame = vec![0.5f32; FRAME_SAMPLES];
        let output = ns.process(&frame).unwrap();
        assert_eq!(output.len(), FRAME_SAMPLES);
        assert_eq!(output[0], 0.5);
    }
}
