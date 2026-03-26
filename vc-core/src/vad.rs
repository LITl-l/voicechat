/// Voice Activity Detection configuration.
#[derive(Clone, Debug)]
pub struct VadConfig {
    /// Energy threshold in dB (relative to full scale). Default: -40 dB.
    pub energy_threshold_db: f32,
    /// Hold time in frames after voice stops (prevents clipping word tails).
    /// Default: 20 frames = 50ms at 2.5ms/frame.
    pub hold_frames: u32,
    /// Minimum RNNoise VAD probability to count as voice (0.0-1.0).
    pub rnnoise_threshold: f32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            energy_threshold_db: -40.0,
            hold_frames: 20,
            rnnoise_threshold: 0.5,
        }
    }
}

/// Voice Activity Detector combining energy-based and RNNoise-based detection.
pub struct VoiceActivityDetector {
    config: VadConfig,
    hold_counter: u32,
    is_active: bool,
}

impl VoiceActivityDetector {
    pub fn new(config: VadConfig) -> Self {
        Self {
            config,
            hold_counter: 0,
            is_active: false,
        }
    }

    /// Check if a frame contains voice.
    ///
    /// `pcm` - f32 audio samples [-1.0, 1.0]
    /// `rnnoise_vad` - optional RNNoise VAD probability (when noise suppression is active)
    pub fn detect(&mut self, pcm: &[f32], rnnoise_vad: Option<f32>) -> bool {
        let energy_db = compute_energy_db(pcm);

        let voice_detected = if let Some(vad_prob) = rnnoise_vad {
            energy_db > self.config.energy_threshold_db && vad_prob > self.config.rnnoise_threshold
        } else {
            energy_db > self.config.energy_threshold_db
        };

        if voice_detected {
            self.hold_counter = self.config.hold_frames;
            self.is_active = true;
        } else if self.hold_counter > 0 {
            self.hold_counter -= 1;
        } else {
            self.is_active = false;
        }

        self.is_active
    }

    pub fn is_active(&self) -> bool {
        self.is_active
    }

    pub fn set_config(&mut self, config: VadConfig) {
        self.config = config;
    }
}

/// Compute RMS energy in dB relative to full scale.
fn compute_energy_db(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return f32::NEG_INFINITY;
    }
    let rms = (pcm.iter().map(|&s| s * s).sum::<f32>() / pcm.len() as f32).sqrt();
    if rms < 1e-10 {
        f32::NEG_INFINITY
    } else {
        20.0 * rms.log10()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::FRAME_SAMPLES;

    #[test]
    fn test_silence_not_detected() {
        let mut vad = VoiceActivityDetector::new(VadConfig::default());
        let silence = vec![0.0f32; FRAME_SAMPLES];
        assert!(!vad.detect(&silence, None));
    }

    #[test]
    fn test_loud_signal_detected() {
        let mut vad = VoiceActivityDetector::new(VadConfig::default());
        let loud: Vec<f32> = (0..FRAME_SAMPLES)
            .map(|i| (i as f32 * 0.1).sin() * 0.5)
            .collect();
        assert!(vad.detect(&loud, None));
    }

    #[test]
    fn test_hold_time() {
        let mut vad = VoiceActivityDetector::new(VadConfig {
            hold_frames: 2,
            ..Default::default()
        });

        let loud = vec![0.5f32; FRAME_SAMPLES];
        let silence = vec![0.0f32; FRAME_SAMPLES];

        assert!(vad.detect(&loud, None));
        assert!(vad.detect(&silence, None)); // hold 1
        assert!(vad.detect(&silence, None)); // hold 2
        assert!(!vad.detect(&silence, None)); // expired
    }

    #[test]
    fn test_rnnoise_vad_gate() {
        let mut vad = VoiceActivityDetector::new(VadConfig::default());
        let loud = vec![0.5f32; FRAME_SAMPLES];

        // High energy but low RNNoise confidence: no detection
        assert!(!vad.detect(&loud, Some(0.1)));
        // High energy and high RNNoise confidence: detected
        assert!(vad.detect(&loud, Some(0.9)));
    }

    #[test]
    fn test_energy_db_values() {
        assert!(compute_energy_db(&[1.0, -1.0, 1.0, -1.0]) > -3.1);
        assert_eq!(compute_energy_db(&[0.0; 120]), f32::NEG_INFINITY);
        assert!(compute_energy_db(&[]) == f32::NEG_INFINITY);
    }
}
