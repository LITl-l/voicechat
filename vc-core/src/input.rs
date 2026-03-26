use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Voice input transmission mode.
#[derive(Clone, Debug, PartialEq)]
pub enum InputMode {
    /// Always transmitting.
    AlwaysOn,
    /// Transmit only while PTT key is held.
    PushToTalk,
    /// Transmit when VAD detects voice.
    VoiceActivation,
}

/// Gates audio transmission based on the current input mode.
pub struct InputGate {
    mode: InputMode,
    /// PTT state — set from UI/input thread, read from audio thread.
    ptt_active: Arc<AtomicBool>,
    vad_active: bool,
    is_transmitting: bool,
}

impl InputGate {
    pub fn new(mode: InputMode) -> Self {
        Self {
            mode,
            ptt_active: Arc::new(AtomicBool::new(false)),
            vad_active: false,
            is_transmitting: false,
        }
    }

    /// Clone of the PTT flag for use by input/UI threads.
    pub fn ptt_flag(&self) -> Arc<AtomicBool> {
        self.ptt_active.clone()
    }

    pub fn set_mode(&mut self, mode: InputMode) {
        self.mode = mode;
    }

    pub fn mode(&self) -> &InputMode {
        &self.mode
    }

    pub fn set_vad_active(&mut self, active: bool) {
        self.vad_active = active;
    }

    pub fn set_ptt(&self, pressed: bool) {
        self.ptt_active.store(pressed, Ordering::Relaxed);
    }

    /// Returns true if audio should be transmitted in the current frame.
    pub fn should_transmit(&mut self) -> bool {
        self.is_transmitting = match self.mode {
            InputMode::AlwaysOn => true,
            InputMode::PushToTalk => self.ptt_active.load(Ordering::Relaxed),
            InputMode::VoiceActivation => self.vad_active,
        };
        self.is_transmitting
    }

    pub fn is_transmitting(&self) -> bool {
        self.is_transmitting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_always_on() {
        let mut gate = InputGate::new(InputMode::AlwaysOn);
        assert!(gate.should_transmit());
    }

    #[test]
    fn test_ptt() {
        let mut gate = InputGate::new(InputMode::PushToTalk);
        assert!(!gate.should_transmit());
        gate.set_ptt(true);
        assert!(gate.should_transmit());
        gate.set_ptt(false);
        assert!(!gate.should_transmit());
    }

    #[test]
    fn test_ptt_flag_shared() {
        let gate = InputGate::new(InputMode::PushToTalk);
        let flag = gate.ptt_flag();
        flag.store(true, Ordering::Relaxed);
        assert!(gate.ptt_active.load(Ordering::Relaxed));
    }

    #[test]
    fn test_voice_activation() {
        let mut gate = InputGate::new(InputMode::VoiceActivation);
        assert!(!gate.should_transmit());
        gate.set_vad_active(true);
        assert!(gate.should_transmit());
        gate.set_vad_active(false);
        assert!(!gate.should_transmit());
    }
}
