use serde::{Deserialize, Serialize};

/// Configuration for one native microphone session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioCaptureConfig {
    pub auto_stop: bool,
    pub silence_duration_ms: u64,
    pub max_duration_ms: u64,
    pub sample_rate: Option<u32>,
    /// Keep PCM samples for WAV output. Disable this for level-only monitoring
    /// so a long-lived monitor does not grow an in-memory recording.
    pub retain_audio: bool,
}

impl Default for AudioCaptureConfig {
    fn default() -> Self {
        Self {
            auto_stop: true,
            silence_duration_ms: 1_000,
            max_duration_ms: 45_000,
            sample_rate: None,
            retain_audio: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CaptureEvent {
    Started,
    AudioLevel { rms: f32, peak_rms: f32 },
    SpeechStarted,
    SpeechEnded,
    Stopped { reason: StopReason },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Manual,
    SilenceDetected,
    MaximumDuration,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub name: String,
    pub sample_rate: u32,
    pub channels: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureInfo {
    pub device: String,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Final VAD and PCM counters kept beside the encoded result for diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureStats {
    pub current_rms: f32,
    pub peak_rms: f32,
    pub noise_floor: f32,
    pub speech_threshold: f32,
    pub speech_candidate_active: bool,
    pub speech_detected: bool,
    pub silence_duration_ms: u64,
    pub duration_ms: u64,
    pub chunk_count: usize,
    pub dropped_chunks: u64,
}

#[derive(Debug)]
pub struct RecordedAudio {
    pub wav_data: Vec<u8>,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration_ms: u64,
    pub bytes: usize,
}
