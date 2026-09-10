use crate::error::AudioError;
use crate::types::{CaptureInfo, DeviceInfo};
use cpal::traits::{DeviceTrait, HostTrait};

/// The selected CPAL device and the stream settings that will be used for it.
pub struct SelectedInputDevice {
    pub device: cpal::Device,
    pub info: CaptureInfo,
    pub sample_format: cpal::SampleFormat,
    pub config: cpal::StreamConfig,
}

pub fn select_default_input_device(
    requested_sample_rate: Option<u32>,
) -> Result<SelectedInputDevice, AudioError> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or(AudioError::NoInputDevice)?;
    let name = device
        .name()
        .map_err(|error| AudioError::DeviceName(error.to_string()))?;
    let supported = device
        .default_input_config()
        .map_err(|error| AudioError::DefaultInputConfig(error.to_string()))?;
    let mut config = supported.config();

    if let Some(sample_rate) = requested_sample_rate {
        if sample_rate == 0 {
            return Err(AudioError::InvalidConfig(
                "sample_rate must be greater than zero".to_string(),
            ));
        }
        let supported_rate = device
            .supported_input_configs()
            .map_err(|error| AudioError::DefaultInputConfig(error.to_string()))?
            .any(|range| {
                range.channels() == config.channels
                    && range.min_sample_rate().0 <= sample_rate
                    && range.max_sample_rate().0 >= sample_rate
            });
        if !supported_rate {
            return Err(AudioError::UnsupportedSampleRate(sample_rate));
        }
        config.sample_rate = cpal::SampleRate(sample_rate);
    }

    let info = CaptureInfo {
        device: name,
        sample_rate: config.sample_rate.0,
        channels: config.channels,
    };
    let public_info = DeviceInfo {
        name: info.device.clone(),
        sample_rate: info.sample_rate,
        channels: info.channels,
    };
    tracing_info(&public_info);

    Ok(SelectedInputDevice {
        device,
        info,
        sample_format: supported.sample_format(),
        config,
    })
}

// Keep device selection independent of a logging facade. The app host emits
// the structured diagnostics; this lightweight line also helps standalone
// crate users when they run a capture binary directly.
fn tracing_info(info: &DeviceInfo) {
    eprintln!(
        "[audio] device={} sample_rate={} channels={}",
        info.name, info.sample_rate, info.channels
    );
}
