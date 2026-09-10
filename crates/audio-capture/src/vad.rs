/// Events emitted by the native voice activity detector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VadEvent {
    SpeechStarted,
    SpeechEnded,
    InitialSilence,
    MaximumDuration,
}

/// Small adaptive VAD for one spoken utterance.
pub struct VoiceActivityDetector {
    pub(crate) noise_floor: f32,
    pub(crate) threshold: f32,
    pub(crate) speech_detected: bool,
    pub(crate) silence_ms: u64,
    speech_candidate_ms: u64,
    elapsed_ms: u64,
    last_rms: f32,
    peak_rms: f32,
    initial_silence_ms: u64,
    silence_duration_limit_ms: u64,
    max_duration_ms: u64,
}

impl VoiceActivityDetector {
    pub fn new(silence_duration_ms: u64, max_duration_ms: u64) -> Self {
        Self {
            noise_floor: 0.005,
            threshold: 0.015,
            speech_detected: false,
            silence_ms: 0,
            speech_candidate_ms: 0,
            elapsed_ms: 0,
            last_rms: 0.0,
            peak_rms: 0.0,
            initial_silence_ms: 5_000,
            silence_duration_limit_ms: silence_duration_ms.max(1),
            max_duration_ms: max_duration_ms.max(1),
        }
    }

    pub fn update(&mut self, rms: f32, duration_ms: u64) -> Option<VadEvent> {
        let duration_ms = duration_ms.max(1);
        self.elapsed_ms = self.elapsed_ms.saturating_add(duration_ms);
        let level = if rms.is_finite() { rms.max(0.0) } else { 0.0 };
        self.last_rms = level;
        self.peak_rms = self.peak_rms.max(level);
        self.threshold = (self.noise_floor * 2.2).max(0.015);

        if self.elapsed_ms >= self.max_duration_ms {
            return Some(VadEvent::MaximumDuration);
        }

        if level >= self.threshold {
            self.speech_candidate_ms = self.speech_candidate_ms.saturating_add(duration_ms);
            self.silence_ms = 0;
            if !self.speech_detected && self.speech_candidate_ms >= 100 {
                self.speech_detected = true;
                return Some(VadEvent::SpeechStarted);
            }
        } else if !self.speech_detected {
            self.speech_candidate_ms = 0;
            self.noise_floor = self.noise_floor * 0.95 + level * 0.05;
            self.threshold = (self.noise_floor * 2.2).max(0.015);
            if self.elapsed_ms >= self.initial_silence_ms {
                return Some(VadEvent::InitialSilence);
            }
        } else {
            self.silence_ms = self.silence_ms.saturating_add(duration_ms);
            if self.silence_ms >= self.silence_duration_limit_ms {
                return Some(VadEvent::SpeechEnded);
            }
        }
        None
    }

    pub fn current_rms(&self) -> f32 {
        self.last_rms
    }

    pub fn peak_rms(&self) -> f32 {
        self.peak_rms
    }

    pub fn speech_candidate_active(&self) -> bool {
        self.speech_candidate_ms > 0 && !self.speech_detected
    }

    pub fn is_speech_detected(&self) -> bool {
        self.speech_detected
    }

    pub fn noise_floor(&self) -> f32 {
        self.noise_floor
    }

    pub fn threshold(&self) -> f32 {
        self.threshold
    }

    pub fn silence_duration_ms(&self) -> u64 {
        self.silence_ms
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_requires_a_short_confirmation_window() {
        let mut vad = VoiceActivityDetector::new(1_000, 45_000);
        assert_eq!(vad.update(0.08, 40), None);
        assert_eq!(vad.update(0.08, 70), Some(VadEvent::SpeechStarted));
        assert!(vad.is_speech_detected());
    }

    #[test]
    fn confirmed_speech_ends_after_configured_silence() {
        let mut vad = VoiceActivityDetector::new(100, 45_000);
        assert_eq!(vad.update(0.08, 100), Some(VadEvent::SpeechStarted));
        assert_eq!(vad.update(0.0, 100), Some(VadEvent::SpeechEnded));
    }
}
