#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("no default input device is available")]
    NoInputDevice,
    #[error("could not read input device name: {0}")]
    DeviceName(String),
    #[error("could not read default input configuration: {0}")]
    DefaultInputConfig(String),
    #[error("requested sample rate {0} Hz is not supported by the input device")]
    UnsupportedSampleRate(u32),
    #[error("unsupported input sample format: {0}")]
    UnsupportedSampleFormat(String),
    #[error("could not build input stream: {0}")]
    StreamBuild(String),
    #[error("could not start input stream: {0}")]
    StreamPlay(String),
    #[error("an audio capture session is already running")]
    AlreadyRunning,
    #[error("no audio capture session is running")]
    NotRunning,
    #[error("audio capture worker failed: {0}")]
    Worker(String),
    #[error("WAV encoding failed: {0}")]
    Wav(String),
    #[error("invalid audio capture configuration: {0}")]
    InvalidConfig(String),
}
