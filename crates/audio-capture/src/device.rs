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
    select_input_device(None, requested_sample_rate)
}

/// Open exactly the requested input device. `None` (or `"default"`)
/// means the OS default input; any other name must match an available
/// input device exactly, otherwise [`AudioError::UnknownDevice`] is
/// returned and NO other device is opened as a fallback.
pub fn select_input_device(
    requested: Option<&str>,
    requested_sample_rate: Option<u32>,
) -> Result<SelectedInputDevice, AudioError> {
    let host = cpal::default_host();
    let device = match requested.map(str::trim) {
        None | Some("") | Some("default") => host
            .default_input_device()
            .ok_or(AudioError::NoInputDevice)?,
        Some(name) => {
            let devices = host
                .input_devices()
                .map_err(|error| AudioError::DeviceName(error.to_string()))?;
            let mut exact = None;
            for candidate in devices {
                let candidate_name = candidate
                    .name()
                    .map_err(|error| AudioError::DeviceName(error.to_string()))?;
                if match_device_name(&candidate_name, name) {
                    exact = Some((candidate, candidate_name));
                    break;
                }
            }
            exact
                .map(|(device, _)| device)
                .ok_or_else(|| AudioError::UnknownDevice(name.to_string()))?
        }
    };
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

/// Exact device-name matching. Device identity is security-relevant (a
/// microphone grant covers one device), so matching is byte-exact —
/// no case folding, no substring or prefix matching that could open a
/// different device than the one the user authorized.
pub fn match_device_name(candidate: &str, requested: &str) -> bool {
    candidate == requested
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
