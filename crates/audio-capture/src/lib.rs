mod capture;
mod device;
mod error;
mod pcm;
mod types;
mod vad;
mod wav;

pub use capture::AudioCapture;
pub use error::AudioError;
pub use pcm::PcmBuffer;
pub use types::{
    AudioCaptureConfig, CaptureEvent, CaptureInfo, CaptureStats, DeviceInfo, RecordedAudio,
    StopReason,
};
pub use vad::VoiceActivityDetector;
pub use wav::{OUTPUT_CHANNELS, OUTPUT_SAMPLE_RATE};

/// Return normalized RMS energy for signed PCM samples.
pub fn calculate_rms(samples: &[i16]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_squares: f64 = samples
        .iter()
        .map(|sample| {
            let normalized = f64::from(*sample) / 32_768.0;
            normalized * normalized
        })
        .sum();
    (sum_squares / samples.len() as f64).sqrt().clamp(0.0, 1.0) as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rms_is_normalized() {
        assert_eq!(calculate_rms(&[]), 0.0);
        assert!((calculate_rms(&[i16::MAX, i16::MAX]) - 1.0).abs() < 0.001);
        assert!((calculate_rms(&[16_384, -16_384]) - 0.5).abs() < 0.001);
    }

    #[test]
    fn capture_retains_audio_by_default() {
        assert!(AudioCaptureConfig::default().retain_audio);
    }
}
