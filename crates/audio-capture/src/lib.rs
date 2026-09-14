mod capture;
mod device;
mod error;
mod pcm;
mod types;
mod vad;
mod wav;

pub use capture::{AudioCapture, FinishedCaptureView};
pub use device::{match_device_name, select_default_input_device, select_input_device};
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

    #[test]
    fn device_matching_is_byte_exact() {
        assert!(match_device_name(
            "Built-in Microphone",
            "Built-in Microphone"
        ));
        assert!(!match_device_name(
            "Built-in Microphone",
            "built-in microphone"
        ));
        assert!(!match_device_name("Built-in Microphone", "Built-in"));
        assert!(!match_device_name("Built-in Microphone", "Microphone"));
        assert!(!match_device_name(
            "Built-in Microphone",
            "Built-in Microphone "
        ));
        assert!(!match_device_name("Mic A", "Mic B"));
    }

    #[test]
    fn missing_device_never_falls_back_to_default() {
        // Hardware-independent: whatever the host enumerates, a name that
        // matches nothing must error — it must never open another device.
        let result = select_input_device(Some("utsuwa-no-such-device-xyz"), None);
        let kind = match &result {
            Ok(_) => "opened-a-device",
            Err(AudioError::UnknownDevice(_)) => "unknown-device",
            Err(other) => panic!("unexpected selection error: {other}"),
        };
        assert_eq!(
            kind, "unknown-device",
            "missing device must not open a fallback"
        );
        if let Err(AudioError::UnknownDevice(name)) = result {
            assert_eq!(name, "utsuwa-no-such-device-xyz");
        }
    }

    #[test]
    fn idle_capture_reports_not_running_and_nothing_to_take() {
        let mut capture = AudioCapture::new();
        assert!(!capture.is_running());
        assert!(capture.poll_finished().is_none());
        assert!(capture.take_finished().is_none());
        assert!(matches!(capture.stop(), Err(AudioError::NotRunning)));
    }

    #[test]
    fn auto_stopped_recording_is_visible_then_handed_over() {
        use crate::{CaptureStats, RecordedAudio, StopReason};
        let mut capture = AudioCapture::new();
        capture.inject_finished_for_tests(
            RecordedAudio {
                wav_data: vec![1, 2, 3],
                sample_rate: 16_000,
                channels: 1,
                duration_ms: 500,
                bytes: 3,
            },
            CaptureStats {
                current_rms: 0.0,
                peak_rms: 0.1,
                noise_floor: 0.0,
                speech_threshold: 0.05,
                speech_candidate_active: false,
                speech_detected: true,
                silence_duration_ms: 1_000,
                duration_ms: 500,
                chunk_count: 10,
                dropped_chunks: 0,
            },
            StopReason::SilenceDetected,
        );
        // Status sees the finished worker without consuming it…
        let view = capture.poll_finished().expect("finished view");
        assert_eq!(view.reason, StopReason::SilenceDetected);
        assert_eq!(view.bytes, 3);
        assert!(!capture.is_running());
        // …and stop hands the recording over instead of erroring.
        let recorded = capture.stop().expect("stashed recording");
        assert_eq!(recorded.wav_data, vec![1, 2, 3]);
        assert!(capture.poll_finished().is_none());
    }
}
