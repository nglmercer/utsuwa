use crate::device::select_default_input_device;
use crate::error::AudioError;
use crate::pcm::PcmBuffer;
use crate::vad::{VadEvent, VoiceActivityDetector};
use crate::wav::{encode_wav, OUTPUT_CHANNELS, OUTPUT_SAMPLE_RATE};
use crate::{
    calculate_rms, AudioCaptureConfig, CaptureEvent, CaptureInfo, CaptureStats, RecordedAudio,
    StopReason,
};
use cpal::traits::{DeviceTrait, StreamTrait};
use crossbeam_channel::{bounded, select, Receiver, Sender, TryRecvError, TrySendError};
use std::cmp::min;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type EventCallback = Arc<dyn Fn(CaptureEvent) + Send + Sync + 'static>;

enum WorkerCommand {
    Stop(StopReason),
    Cancel,
    CallbackError(String),
}

struct WorkerOutput {
    audio: RecordedAudio,
    stats: CaptureStats,
}

struct ActiveCapture {
    command_tx: Sender<WorkerCommand>,
    result_rx: Receiver<Result<WorkerOutput, AudioError>>,
    worker: JoinHandle<()>,
}

/// Native microphone capture. The CPAL callback only converts samples into
/// preallocated PCM chunks and performs a non-blocking channel send. VAD,
/// buffering, and WAV encoding all run on the dedicated audio thread.
pub struct AudioCapture {
    active: Option<ActiveCapture>,
    last_stats: Option<CaptureStats>,
}

impl Default for AudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioCapture {
    pub fn new() -> Self {
        Self {
            active: None,
            last_stats: None,
        }
    }

    pub fn start<F>(
        &mut self,
        config: AudioCaptureConfig,
        callback: F,
    ) -> Result<CaptureInfo, AudioError>
    where
        F: Fn(CaptureEvent) + Send + Sync + 'static,
    {
        if self.active.is_some() {
            return Err(AudioError::AlreadyRunning);
        }
        if config.max_duration_ms == 0 {
            return Err(AudioError::InvalidConfig(
                "max_duration_ms must be greater than zero".to_string(),
            ));
        }

        let (ready_tx, ready_rx) = bounded::<Result<CaptureInfo, AudioError>>(1);
        let (command_tx, command_rx) = bounded::<WorkerCommand>(8);
        let (result_tx, result_rx) = bounded::<Result<WorkerOutput, AudioError>>(1);
        let callback: EventCallback = Arc::new(callback);
        let worker_command_tx = command_tx.clone();
        let worker = thread::Builder::new()
            .name("audio-capture-worker".to_string())
            .spawn(move || {
                run_capture_thread(
                    config,
                    callback,
                    command_rx,
                    worker_command_tx,
                    ready_tx,
                    result_tx,
                );
            })
            .map_err(|error| AudioError::Worker(error.to_string()))?;

        let info = match ready_rx.recv() {
            Ok(Ok(info)) => info,
            Ok(Err(error)) => {
                let _ = worker.join();
                return Err(error);
            }
            Err(_) => {
                let _ = worker.join();
                return Err(AudioError::Worker(
                    "audio worker exited before opening the input device".to_string(),
                ));
            }
        };

        self.last_stats = None;
        self.active = Some(ActiveCapture {
            command_tx,
            result_rx,
            worker,
        });
        Ok(info)
    }

    pub fn stop(&mut self) -> Result<RecordedAudio, AudioError> {
        let active = self.active.take().ok_or(AudioError::NotRunning)?;
        let _ = active
            .command_tx
            .send(WorkerCommand::Stop(StopReason::Manual));
        let output = receive_output(&active.result_rx);
        active
            .worker
            .join()
            .map_err(|_| AudioError::Worker("audio worker panicked".to_string()))?;
        let output = output?;
        self.last_stats = Some(output.stats);
        Ok(output.audio)
    }

    pub fn cancel(&mut self) {
        let Some(active) = self.active.take() else {
            return;
        };
        let _ = active.command_tx.send(WorkerCommand::Cancel);
        let _ = active.worker.join();
        self.last_stats = None;
    }

    pub fn stats(&self) -> Option<&CaptureStats> {
        self.last_stats.as_ref()
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn receive_output(
    result_rx: &Receiver<Result<WorkerOutput, AudioError>>,
) -> Result<WorkerOutput, AudioError> {
    result_rx
        .recv()
        .map_err(|_| AudioError::Worker("audio worker stopped without a result".to_string()))?
}

/// This thread owns the CPAL stream because CPAL deliberately makes its
/// stream handle non-Send on some backends. It is still separate from the
/// realtime callback: the callback only sends pooled PCM chunks here.
fn run_capture_thread(
    config: AudioCaptureConfig,
    callback: EventCallback,
    command_rx: Receiver<WorkerCommand>,
    command_tx: Sender<WorkerCommand>,
    ready_tx: Sender<Result<CaptureInfo, AudioError>>,
    result_tx: Sender<Result<WorkerOutput, AudioError>>,
) {
    let selected = match select_default_input_device(config.sample_rate) {
        Ok(selected) => selected,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    let info = selected.info.clone();
    let (audio_tx, audio_rx) = bounded::<Vec<i16>>(64);
    let (free_tx, free_rx) = bounded::<Vec<i16>>(16);
    let dropped_chunks = Arc::new(AtomicU64::new(0));
    let chunk_capacity = (info.sample_rate as usize)
        .saturating_mul(info.channels as usize)
        .saturating_div(4)
        .max(16_384);
    for _ in 0..16 {
        // These allocations happen before the realtime callback starts.
        if free_tx.send(Vec::with_capacity(chunk_capacity)).is_err() {
            let _ = ready_tx.send(Err(AudioError::Worker(
                "PCM buffer pool closed".to_string(),
            )));
            return;
        }
    }

    let stream = match build_input_stream(
        &selected.device,
        &selected.config,
        selected.sample_format,
        audio_tx,
        free_tx.clone(),
        free_rx,
        Arc::clone(&dropped_chunks),
        command_tx,
    ) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready_tx.send(Err(error));
            return;
        }
    };
    if let Err(error) = stream.play() {
        let _ = ready_tx.send(Err(AudioError::StreamPlay(error.to_string())));
        return;
    }

    if ready_tx.send(Ok(info.clone())).is_err() {
        return;
    }
    eprintln!("[audio] capture started");
    callback(CaptureEvent::Started);
    let output = worker_loop(
        config,
        info,
        audio_rx,
        free_tx,
        command_rx,
        stream,
        callback,
        dropped_chunks,
    );
    let _ = result_tx.send(output);
}

fn worker_loop(
    config: AudioCaptureConfig,
    info: CaptureInfo,
    audio_rx: Receiver<Vec<i16>>,
    free_tx: Sender<Vec<i16>>,
    command_rx: Receiver<WorkerCommand>,
    stream: cpal::Stream,
    callback: EventCallback,
    dropped_chunks: Arc<AtomicU64>,
) -> Result<WorkerOutput, AudioError> {
    let mut pcm = PcmBuffer::new(info.sample_rate, info.channels);
    let mut vad = VoiceActivityDetector::new(config.silence_duration_ms, config.max_duration_ms);
    let started_at = Instant::now();
    let mut last_level_at = started_at;
    let mut chunk_count = 0usize;
    let mut current_rms = 0.0f32;
    let mut peak_rms = 0.0f32;
    let mut captured_frames = 0u64;

    let stop_reason = 'capture: loop {
        if started_at.elapsed().as_millis() as u64 >= config.max_duration_ms {
            break 'capture StopReason::MaximumDuration;
        }

        select! {
            recv(command_rx) -> command => {
                match command {
                    Ok(WorkerCommand::Stop(reason)) => {
                        break 'capture reason;
                    }
                    Ok(WorkerCommand::Cancel) | Err(_) => return Err(AudioError::Worker("capture cancelled".to_string())),
                    Ok(WorkerCommand::CallbackError(error)) => {
                        eprintln!("[audio] input callback error: {error}");
                        break 'capture StopReason::Error;
                    }
                }
            }
            recv(audio_rx) -> chunk => {
                let Ok(chunk) = chunk else {
                    break 'capture StopReason::Error;
                };
                let frames = chunk.len() / info.channels.max(1) as usize;
                if frames == 0 {
                    let _ = free_tx.try_send(chunk);
                    continue;
                }
                let rms = calculate_rms(&chunk);
                current_rms = rms;
                peak_rms = peak_rms.max(rms);
                let chunk_duration_ms = (frames as u64 * 1_000 / info.sample_rate.max(1) as u64).max(1);
                captured_frames = captured_frames.saturating_add(frames as u64);
                if config.retain_audio {
                    pcm.append(&chunk);
                }
                chunk_count = chunk_count.saturating_add(1);
                let _ = free_tx.try_send(chunk);

                if config.auto_stop {
                    if let Some(event) = vad.update(rms, chunk_duration_ms) {
                        match event {
                            VadEvent::SpeechStarted => {
                                eprintln!("[audio] speech detected");
                                callback(CaptureEvent::SpeechStarted);
                            }
                            VadEvent::SpeechEnded => {
                                callback(CaptureEvent::SpeechEnded);
                                break 'capture StopReason::SilenceDetected;
                            }
                            VadEvent::InitialSilence => {
                                break 'capture StopReason::SilenceDetected;
                            }
                            VadEvent::MaximumDuration => {
                                break 'capture StopReason::MaximumDuration;
                            }
                        }
                    }
                }

                if last_level_at.elapsed() >= Duration::from_millis(50) {
                    callback(CaptureEvent::AudioLevel {
                        rms,
                        peak_rms: peak_rms.max(vad.peak_rms()),
                    });
                    last_level_at = Instant::now();
                }
            }
            default(Duration::from_millis(20)) => {}
        }
    };

    // A manual stop can race with the last callback. The stream remains on
    // this thread, so dropping it here stops the native device safely before
    // the accepted chunks are drained.
    drop(stream);
    while let Ok(chunk) = audio_rx.try_recv() {
        let frames = chunk.len() / info.channels.max(1) as usize;
        if frames > 0 {
            captured_frames = captured_frames.saturating_add(frames as u64);
            if config.retain_audio {
                pcm.append(&chunk);
            }
            chunk_count = chunk_count.saturating_add(1);
        }
        let _ = free_tx.try_send(chunk);
    }

    let wav_data = if config.retain_audio {
        encode_wav(&pcm)?
    } else {
        Vec::new()
    };
    eprintln!("[audio] wav generated bytes={}", wav_data.len());
    let duration_ms = if config.retain_audio {
        pcm.duration_ms()
    } else {
        captured_frames.saturating_mul(1_000) / info.sample_rate.max(1) as u64
    };
    let stats = CaptureStats {
        current_rms: current_rms.max(vad.current_rms()),
        peak_rms: peak_rms.max(vad.peak_rms()),
        noise_floor: vad.noise_floor(),
        speech_threshold: vad.threshold(),
        speech_candidate_active: vad.speech_candidate_active(),
        speech_detected: vad.is_speech_detected(),
        silence_duration_ms: vad.silence_duration_ms(),
        duration_ms,
        chunk_count,
        dropped_chunks: dropped_chunks.load(Ordering::Relaxed),
    };
    let audio = RecordedAudio {
        bytes: wav_data.len(),
        wav_data,
        sample_rate: OUTPUT_SAMPLE_RATE,
        channels: OUTPUT_CHANNELS,
        duration_ms: stats.duration_ms,
    };
    callback(CaptureEvent::Stopped {
        reason: stop_reason,
    });
    Ok(WorkerOutput { audio, stats })
}

fn build_input_stream(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    sample_format: cpal::SampleFormat,
    audio_tx: Sender<Vec<i16>>,
    free_tx: Sender<Vec<i16>>,
    free_rx: Receiver<Vec<i16>>,
    dropped_chunks: Arc<AtomicU64>,
    command_tx: Sender<WorkerCommand>,
) -> Result<cpal::Stream, AudioError> {
    match sample_format {
        cpal::SampleFormat::I8 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: i8| i16::from(sample) * 256,
        ),
        cpal::SampleFormat::F32 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: f32| float_to_i16(sample),
        ),
        cpal::SampleFormat::F64 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: f64| float_to_i16(sample as f32),
        ),
        cpal::SampleFormat::I16 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: i16| sample,
        ),
        cpal::SampleFormat::I32 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: i32| (sample >> 16) as i16,
        ),
        cpal::SampleFormat::I64 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: i64| (sample >> 48) as i16,
        ),
        cpal::SampleFormat::U8 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: u8| (i16::from(sample) - 128) * 256,
        ),
        cpal::SampleFormat::U16 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: u16| (i32::from(sample) - 32_768) as i16,
        ),
        cpal::SampleFormat::U32 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: u32| ((i64::from(sample) - (1_i64 << 31)) >> 16) as i16,
        ),
        cpal::SampleFormat::U64 => build_typed_stream(
            device,
            config,
            audio_tx,
            free_tx,
            free_rx,
            dropped_chunks,
            command_tx,
            |sample: u64| ((i128::from(sample) - (1_i128 << 63)) >> 48) as i16,
        ),
        other => Err(AudioError::UnsupportedSampleFormat(format!("{other:?}"))),
    }
}

fn build_typed_stream<T, F>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    audio_tx: Sender<Vec<i16>>,
    free_tx: Sender<Vec<i16>>,
    free_rx: Receiver<Vec<i16>>,
    dropped_chunks: Arc<AtomicU64>,
    command_tx: Sender<WorkerCommand>,
    convert: F,
) -> Result<cpal::Stream, AudioError>
where
    T: cpal::SizedSample + Copy + 'static,
    F: Fn(T) -> i16 + Send + Sync + 'static,
{
    let error_tx = command_tx;
    device
        .build_input_stream(
            config,
            move |data: &[T], _| {
                send_samples(
                    data,
                    &free_rx,
                    &audio_tx,
                    &free_tx,
                    &dropped_chunks,
                    &convert,
                );
            },
            move |error| {
                let _ = error_tx.try_send(WorkerCommand::CallbackError(error.to_string()));
            },
            None,
        )
        .map_err(|error| AudioError::StreamBuild(error.to_string()))
}

fn send_samples<T, F>(
    data: &[T],
    free_rx: &Receiver<Vec<i16>>,
    audio_tx: &Sender<Vec<i16>>,
    free_tx: &Sender<Vec<i16>>,
    dropped_chunks: &AtomicU64,
    convert: &F,
) where
    T: Copy,
    F: Fn(T) -> i16,
{
    let mut offset = 0;
    while offset < data.len() {
        let mut buffer = match free_rx.try_recv() {
            Ok(buffer) => buffer,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => {
                dropped_chunks.fetch_add(1, Ordering::Relaxed);
                return;
            }
        };
        buffer.clear();
        let count = min(buffer.capacity(), data.len() - offset);
        for sample in &data[offset..offset + count] {
            buffer.push(convert(*sample));
        }
        offset += count;
        match audio_tx.try_send(buffer) {
            Ok(()) => {}
            Err(TrySendError::Full(buffer) | TrySendError::Disconnected(buffer)) => {
                dropped_chunks.fetch_add(1, Ordering::Relaxed);
                let _ = free_tx.try_send(buffer);
                return;
            }
        }
    }
}

fn float_to_i16(value: f32) -> i16 {
    (value.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16
}
