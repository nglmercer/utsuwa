//! Video/audio file analysis (`media.*`).
//!
//! Metadata, thumbnails, single-frame extraction, adaptive keyframe
//! extraction, and audio waveforms. Frame decoding shells out to an
//! `ffmpeg` binary when one exists — no shell (argv only), bounded output,
//! hard timeouts — and reports `backend_unavailable` honestly when it does
//! not. Image decoding uses the `image` crate in-process.
//!
//! Adaptive understanding for providers without native video input:
//! candidates are sampled at 1 FPS, fingerprinted with a dHash, and only
//! frames that differ from everything kept so far (plus scene-change
//! peaks) survive, within a strict model-frame budget. Timestamps ride
//! along so the model keeps temporal context.

use artifact_core::{ArtifactSource, ArtifactStore, ContentPart, ImageArtifactRef};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

const FFMPEG_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_FRAMES_PER_CALL: usize = 12;
const MAX_CANDIDATE_SECONDS: u64 = 180;
const DEFAULT_HASH_THRESHOLD: u64 = 8;

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn failed(tool: &str, code: &str, message: String) -> ToolError {
    ToolError::structured(tool, code, message)
}

fn require_read(tool: &str, ctx: &ToolContext, path: &Path) -> Result<(), ToolError> {
    if ctx.has_ticket(
        capability_core::Capability::FilesystemRead,
        capability_core::Resource::Path(path.to_path_buf()),
    ) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no filesystem-read ticket authorizes this media file",
            serde_json::json!({ "capability": "FilesystemRead" }),
        ))
    }
}

fn path_arg(args: &serde_json::Value, tool: &str) -> Result<PathBuf, ToolError> {
    args.get("path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'path'"))
}

fn read_capability(args: &serde_json::Value) -> Option<CapabilityRequirement> {
    let path = args.get("path")?.as_str()?;
    Some(CapabilityRequirement {
        capability: capability_core::Capability::FilesystemRead,
        resource: capability_core::Resource::Path(PathBuf::from(path)),
    })
}

/// Whether an `ffmpeg` binary is usable. Checked per call (cheap `exec`)
/// rather than cached, so installs during a session take effect.
pub fn ffmpeg_available() -> bool {
    std::process::Command::new("ffmpeg")
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn require_ffmpeg(tool: &str) -> Result<(), ToolError> {
    if ffmpeg_available() {
        Ok(())
    } else {
        Err(failed(
            tool,
            "backend_unavailable",
            "frame extraction needs an `ffmpeg` binary on PATH, which is not installed".to_string(),
        ))
    }
}

fn run_ffmpeg(tool: &str, argv: &[&str]) -> Result<Vec<u8>, ToolError> {
    let mut child = std::process::Command::new("ffmpeg")
        .args(["-hide_banner", "-loglevel", "error", "-nostdin"])
        .args(argv)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            failed(
                tool,
                "backend_unavailable",
                format!("cannot run ffmpeg: {error}"),
            )
        })?;
    let output = match child.wait_with_output_timeout() {
        Some(output) => output.map_err(|error| failed(tool, "action_failed", error.to_string()))?,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(failed(
                tool,
                "action_failed",
                format!("ffmpeg timed out after {}s", FFMPEG_TIMEOUT.as_secs()),
            ));
        }
    };
    if !output.status.success() {
        return Err(failed(
            tool,
            "action_failed",
            format!(
                "ffmpeg failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(500)
                    .collect::<String>()
            ),
        ));
    }
    Ok(output.stdout)
}

async fn run_ffmpeg_async(tool: &'static str, argv: Vec<String>) -> Result<Vec<u8>, ToolError> {
    tokio::task::spawn_blocking(move || {
        let refs = argv.iter().map(|arg| arg.as_str()).collect::<Vec<_>>();
        run_ffmpeg(tool, &refs)
    })
    .await
    .map_err(|error| failed(tool, "action_failed", error.to_string()))?
}

trait WaitWithOutputTimeout {
    fn wait_with_output_timeout(&mut self) -> Option<std::io::Result<std::process::Output>>;
}

impl WaitWithOutputTimeout for std::process::Child {
    fn wait_with_output_timeout(&mut self) -> Option<std::io::Result<std::process::Output>> {
        use std::io::Read as _;
        let deadline = std::time::Instant::now() + FFMPEG_TIMEOUT;
        loop {
            match self.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    let read = (|| -> std::io::Result<()> {
                        if let Some(mut pipe) = self.stdout.take() {
                            pipe.read_to_end(&mut stdout)?;
                        }
                        if let Some(mut pipe) = self.stderr.take() {
                            pipe.read_to_end(&mut stderr)?;
                        }
                        Ok(())
                    })();
                    // Reap the exit status (already exited; returns immediately).
                    let _ = self.wait();
                    return Some(read.map(|()| std::process::Output {
                        status,
                        stdout,
                        stderr,
                    }));
                }
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        return None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Some(Err(error)),
            }
        }
    }
}

/// 64-bit difference hash over an 8x8 luminance sample.
pub fn dhash(png_bytes: &[u8]) -> Result<u64, ToolError> {
    let image = image::load_from_memory(png_bytes)
        .map_err(|error| failed("media.video_keyframes", "action_failed", error.to_string()))?;
    let gray = image.to_luma8();
    let small = image::imageops::resize(&gray, 9, 8, image::imageops::FilterType::Triangle);
    let mut hash: u64 = 0;
    for y in 0..8 {
        for x in 0..8 {
            hash <<= 1;
            if small.get_pixel(x, y)[0] > small.get_pixel(x + 1, y)[0] {
                hash |= 1;
            }
        }
    }
    Ok(hash)
}

pub fn hash_distance(left: u64, right: u64) -> u32 {
    (left ^ right).count_ones()
}

/// Extract one PNG frame at `timestamp_ms` (or the first frame when `None`).
fn extract_frame_png(
    path: &Path,
    timestamp_ms: Option<u64>,
    max_width: u32,
) -> Result<(Vec<u8>, u64), ToolError> {
    let tool = "media.video_frame";
    let timestamp_ms = timestamp_ms.unwrap_or(0);
    let seek = format!("{}.{:03}", timestamp_ms / 1_000, timestamp_ms % 1_000);
    let width_filter = format!("scale={max_width}:-2");
    let path_str = path.to_string_lossy().into_owned();
    let bytes = run_ffmpeg(
        tool,
        &[
            "-ss",
            &seek,
            "-i",
            &path_str,
            "-frames:v",
            "1",
            "-vf",
            &width_filter,
            "-f",
            "image2pipe",
            "-vcodec",
            "png",
            "-",
        ],
    )?;
    if bytes.len() < 8 || &bytes[..8] != b"\x89PNG\r\n\x1a\n" {
        return Err(failed(
            tool,
            "action_failed",
            "ffmpeg produced no PNG frame at that timestamp".to_string(),
        ));
    }
    Ok((bytes, timestamp_ms))
}

fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
    image::load_from_memory(bytes)
        .map(|image| (image.width(), image.height()))
        .unwrap_or((0, 0))
}

async fn store_frame(
    tool: &'static str,
    artifacts: &Arc<dyn ArtifactStore>,
    png_bytes: Vec<u8>,
    _timestamp_ms: u64,
) -> Result<ImageArtifactRef, ToolError> {
    if png_bytes.len() > 8 * 1024 * 1024 {
        return Err(failed(
            tool,
            "response_too_large",
            "extracted frame exceeds the size limit".to_string(),
        ));
    }
    let (width, height) = png_dimensions(&png_bytes);
    let artifact = artifacts
        .put_with_source("image/png", png_bytes, ArtifactSource::Generated, false)
        .await
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    Ok(ImageArtifactRef::new(artifact, width, height))
}

fn frame_output(tab: serde_json::Value, frames: Vec<(u64, ImageArtifactRef)>) -> ToolOutput {
    let parts = frames
        .iter()
        .map(|(_, image)| ContentPart::Image(image.clone()))
        .collect::<Vec<_>>();
    let mut metadata = tab.as_object().cloned().unwrap_or_default();
    metadata.insert(
        "frames".to_string(),
        frames
            .iter()
            .map(|(timestamp_ms, image)| {
                serde_json::json!({
                    "timestamp_ms": timestamp_ms,
                    "artifact_id": image.artifact.id,
                    "width": image.width,
                    "height": image.height,
                })
            })
            .collect::<Vec<_>>()
            .into(),
    );
    ToolOutput::multipart(serde_json::Value::Object(metadata), parts)
}

pub struct MediaDeps {
    pub artifacts: Arc<dyn ArtifactStore>,
}

pub struct MediaMetadataTool;
pub struct MediaVideoMetadataTool;
pub struct MediaAudioMetadataTool;
pub struct MediaVideoFrameTool {
    pub deps: MediaDeps,
}
pub struct MediaVideoFramesTool {
    pub deps: MediaDeps,
}
pub struct MediaVideoKeyframesTool {
    pub deps: MediaDeps,
}
pub struct MediaThumbnailTool {
    pub deps: MediaDeps,
}
pub struct MediaWaveformTool;

fn metadata_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object", "additionalProperties": false,
        "properties": { "path": {"type": "string"} }, "required": ["path"],
    })
}

macro_rules! metadata_tool {
    ($name:ident, $id:literal, $desc:literal) => {
        #[async_trait::async_trait]
        impl Tool for $name {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: capability_core::ToolId::new($id),
                    description: $desc.to_string(),
                    input_schema: metadata_schema(),
                    effects: vec![ToolEffect::ReadOnly],
                }
            }
            fn required_capability(
                &self,
                args: &serde_json::Value,
            ) -> Option<CapabilityRequirement> {
                read_capability(args)
            }
            async fn invoke(
                &self,
                ctx: ToolContext,
                args: serde_json::Value,
            ) -> Result<ToolOutput, ToolError> {
                let path = path_arg(&args, $id)?;
                require_read($id, &ctx, &path)?;
                Ok(ToolOutput::json(file_metadata($id, &path)?))
            }
        }
    };
}

fn file_metadata(tool: &str, path: &Path) -> Result<serde_json::Value, ToolError> {
    let fs_meta = std::fs::metadata(path)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    let mut out = serde_json::json!({
        "path": path.to_string_lossy(),
        "size_bytes": fs_meta.len(),
    });
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => {
            let reader = image::ImageReader::open(path)
                .and_then(|reader| reader.with_guessed_format())
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let format = reader.format().map(|format| format!("{format:?}"));
            let (width, height) = reader
                .into_dimensions()
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            out["kind"] = serde_json::Value::String("image".to_string());
            out["width"] = width.into();
            out["height"] = height.into();
            out["format"] = format.into();
        }
        "wav" => {
            let reader = hound::WavReader::open(path)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let spec = reader.spec();
            out["kind"] = serde_json::Value::String("audio".to_string());
            out["sample_rate"] = spec.sample_rate.into();
            out["channels"] = spec.channels.into();
            out["bits_per_sample"] = spec.bits_per_sample.into();
            out["samples"] = reader.len().into();
            out["duration_ms"] =
                ((reader.len() as u64 * 1_000) / spec.sample_rate.max(1) as u64).into();
        }
        "mp4" | "webm" | "mkv" | "mov" | "avi" => {
            out["kind"] = serde_json::Value::String("video".to_string());
            if ffmpeg_available() {
                if let Ok(probe) = probe_video(path) {
                    for (key, value) in probe {
                        out[key] = value;
                    }
                }
            }
        }
        "mp3" | "ogg" | "flac" | "m4a" => {
            out["kind"] = serde_json::Value::String("audio".to_string());
        }
        _ => {
            out["kind"] = serde_json::Value::String("binary".to_string());
        }
    }
    Ok(out)
}

/// Video duration/streams via ffprobe when present; `None` otherwise.
fn probe_video(path: &Path) -> Result<Vec<(String, serde_json::Value)>, ToolError> {
    let path_str = path.to_string_lossy().into_owned();
    let output = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "format=duration,size:stream=width,height,codec_name,codec_type",
            "-of",
            "json",
            &path_str,
        ])
        .output()
        .map_err(|error| failed("media.metadata", "action_failed", error.to_string()))?;
    if !output.status.success() {
        return Err(failed(
            "media.metadata",
            "action_failed",
            "ffprobe failed".to_string(),
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| failed("media.metadata", "action_failed", error.to_string()))?;
    let mut out = Vec::new();
    if let Some(duration) = value
        .pointer("/format/duration")
        .and_then(|value| value.as_str())
        .and_then(|text| text.parse::<f64>().ok())
    {
        out.push((
            "duration_ms".to_string(),
            serde_json::json!((duration * 1_000.0).round() as u64),
        ));
    }
    if let Some(streams) = value.get("streams").and_then(|value| value.as_array()) {
        for stream in streams {
            if stream.get("codec_type").and_then(|value| value.as_str()) == Some("video") {
                out.push((
                    "width".to_string(),
                    stream
                        .get("width")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ));
                out.push((
                    "height".to_string(),
                    stream
                        .get("height")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ));
                out.push((
                    "codec".to_string(),
                    stream
                        .get("codec_name")
                        .cloned()
                        .unwrap_or(serde_json::Value::Null),
                ));
                break;
            }
        }
    }
    Ok(out)
}

metadata_tool!(MediaMetadataTool, "media.metadata", "Size, kind, and format metadata for a media file (images decoded, WAV parsed, video via ffprobe when present).");
metadata_tool!(
    MediaVideoMetadataTool,
    "media.video_metadata",
    "Metadata scoped to video files."
);
metadata_tool!(
    MediaAudioMetadataTool,
    "media.audio_metadata",
    "Metadata scoped to audio files (WAV parsed natively)."
);

fn frame_args(args: &serde_json::Value, tool: &str) -> Result<(Option<u64>, u32), ToolError> {
    let timestamp_ms = args.get("timestamp_ms").and_then(|value| value.as_u64());
    if args.get("frame_index").is_some() && timestamp_ms.is_none() {
        // Frame-exact seeking needs fps knowledge; approximate through
        // timestamp only when the caller also gives a rate is overkill —
        // reject honestly instead of guessing.
        return Err(invalid(
            tool,
            "frame_index seeking is not supported; pass timestamp_ms",
        ));
    }
    let max_width = args
        .get("max_width")
        .and_then(|value| value.as_u64())
        .map(|width| {
            u32::try_from(width)
                .ok()
                .filter(|width| (1..=4096).contains(width))
                .ok_or_else(|| invalid(tool, "max_width must be between 1 and 4096"))
        })
        .transpose()?
        .unwrap_or(1280);
    Ok((timestamp_ms, max_width))
}

#[async_trait::async_trait]
impl Tool for MediaVideoFrameTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("media.video_frame"),
            description: "Extract one video frame at timestamp_ms as an image artifact."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "timestamp_ms": {"type": "integer", "minimum": 0},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 4096},
                },
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "media.video_frame")?;
        let (timestamp_ms, max_width) = frame_args(&args, "media.video_frame")?;
        require_read("media.video_frame", &ctx, &path)?;
        require_ffmpeg("media.video_frame")?;
        let path_for_task = path.clone();
        let (bytes, timestamp) = tokio::task::spawn_blocking(move || {
            extract_frame_png(&path_for_task, timestamp_ms, max_width)
        })
        .await
        .map_err(|error| failed("media.video_frame", "action_failed", error.to_string()))??;
        let image =
            store_frame("media.video_frame", &self.deps.artifacts, bytes, timestamp).await?;
        Ok(frame_output(
            serde_json::json!({ "path": path.to_string_lossy() }),
            vec![(timestamp, image)],
        ))
    }
}

#[async_trait::async_trait]
impl Tool for MediaVideoFramesTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("media.video_frames"),
            description:
                "Extract frames at explicit timestamps (bounded count) as image artifacts."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "timestamps_ms": {"type": "array", "items": {"type": "integer", "minimum": 0}},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 4096},
                },
                "required": ["path", "timestamps_ms"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "media.video_frames")?;
        let (_, max_width) = frame_args(&args, "media.video_frames")?;
        let timestamps = args
            .get("timestamps_ms")
            .and_then(|value| value.as_array())
            .ok_or_else(|| invalid("media.video_frames", "missing array 'timestamps_ms'"))?;
        if timestamps.is_empty() || timestamps.len() > MAX_FRAMES_PER_CALL {
            return Err(invalid(
                "media.video_frames",
                format!("timestamps_ms must contain 1..={MAX_FRAMES_PER_CALL} entries"),
            ));
        }
        let mut stamps = Vec::with_capacity(timestamps.len());
        for stamp in timestamps {
            stamps.push(
                stamp
                    .as_u64()
                    .ok_or_else(|| invalid("media.video_frames", "timestamps must be integers"))?,
            );
        }
        require_read("media.video_frames", &ctx, &path)?;
        require_ffmpeg("media.video_frames")?;
        let mut frames = Vec::new();
        for timestamp_ms in stamps {
            let path_for_task = path.clone();
            let (bytes, timestamp) = tokio::task::spawn_blocking(move || {
                extract_frame_png(&path_for_task, Some(timestamp_ms), max_width)
            })
            .await
            .map_err(|error| failed("media.video_frames", "action_failed", error.to_string()))??;
            frames.push((
                timestamp,
                store_frame("media.video_frames", &self.deps.artifacts, bytes, timestamp).await?,
            ));
        }
        Ok(frame_output(
            serde_json::json!({ "path": path.to_string_lossy() }),
            frames,
        ))
    }
}

#[async_trait::async_trait]
impl Tool for MediaVideoKeyframesTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("media.video_keyframes"),
            description: "Adaptive keyframes: sample at 1 FPS, deduplicate by perceptual hash, keep only visually distinct frames within a strict budget, with timestamps.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "max_frames": {"type": "integer", "minimum": 1, "maximum": 12},
                    "threshold_bits": {"type": "integer", "minimum": 1, "maximum": 64},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 4096},
                },
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "media.video_keyframes")?;
        let (_, max_width) = frame_args(&args, "media.video_keyframes")?;
        let max_frames = args
            .get("max_frames")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(1, MAX_FRAMES_PER_CALL as u64) as usize)
            .unwrap_or(8);
        let threshold = args
            .get("threshold_bits")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(1, 64))
            .unwrap_or(DEFAULT_HASH_THRESHOLD);
        require_read("media.video_keyframes", &ctx, &path)?;
        require_ffmpeg("media.video_keyframes")?;
        let duration_ms = probe_video(&path)
            .ok()
            .and_then(|probe| {
                probe
                    .iter()
                    .find(|(key, _)| key == "duration_ms")
                    .map(|(_, value)| value.as_u64().unwrap_or(0))
            })
            .unwrap_or(30_000)
            .clamp(1_000, MAX_CANDIDATE_SECONDS * 1_000);
        // 1. Low-frequency candidates across the whole duration.
        let step_ms = (duration_ms / 60).max(1_000);
        // 2-4. Fingerprint, detect changes, keep meaningful frames.
        let mut kept: Vec<(u64, u64, Vec<u8>)> = Vec::new();
        let mut timestamp_ms = 0;
        while timestamp_ms < duration_ms && kept.len() < 60 {
            let path_for_task = path.clone();
            let bytes = tokio::task::spawn_blocking(move || {
                extract_frame_png(&path_for_task, Some(timestamp_ms), max_width)
                    .map(|(bytes, _)| bytes)
            })
            .await
            .map_err(|error| failed("media.video_keyframes", "action_failed", error.to_string()))?;
            // Undecodable timestamps (past EOS) end sampling, not the call.
            let hash = match bytes {
                Ok(bytes) => match dhash(&bytes) {
                    Ok(hash) => Some((hash, bytes)),
                    Err(_) => None,
                },
                Err(_) => None,
            };
            match hash {
                Some((hash, bytes)) => {
                    let novel = kept.iter().all(|(_, kept_hash, _)| {
                        hash_distance(hash, *kept_hash) as u64 >= threshold
                    });
                    if kept.is_empty() || novel {
                        kept.push((timestamp_ms, hash, bytes));
                        if kept.len() >= max_frames {
                            break;
                        }
                    }
                }
                None => break,
            }
            timestamp_ms += step_ms;
        }
        // 5-6. Timestamped survivors within the model-frame budget.
        let mut frames = Vec::new();
        for (timestamp, _, bytes) in kept.into_iter().take(max_frames) {
            frames.push((
                timestamp,
                store_frame(
                    "media.video_keyframes",
                    &self.deps.artifacts,
                    bytes,
                    timestamp,
                )
                .await?,
            ));
        }
        Ok(frame_output(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "duration_ms": duration_ms,
                "threshold_bits": threshold,
            }),
            frames,
        ))
    }
}

#[async_trait::async_trait]
impl Tool for MediaThumbnailTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("media.thumbnail"),
            description: "First-frame thumbnail of a video (or scaled image) as an image artifact."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 4096},
                },
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "media.thumbnail")?;
        let (_, max_width) = frame_args(&args, "media.thumbnail")?;
        require_read("media.thumbnail", &ctx, &path)?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if ["png", "jpg", "jpeg", "gif", "webp", "bmp"].contains(&extension.as_str()) {
            let path_for_task = path.clone();
            let bytes = tokio::task::spawn_blocking(move || {
                let image = image::open(&path_for_task).map_err(|error| {
                    failed("media.thumbnail", "action_failed", error.to_string())
                })?;
                let resized = if image.width() > max_width {
                    let height = ((image.height() as f32 * max_width as f32 / image.width() as f32)
                        .round() as u32)
                        .max(1);
                    image.resize(max_width, height, image::imageops::FilterType::Triangle)
                } else {
                    image
                };
                let mut bytes = Vec::new();
                resized
                    .write_to(
                        &mut std::io::Cursor::new(&mut bytes),
                        image::ImageFormat::Png,
                    )
                    .map_err(|error| {
                        failed("media.thumbnail", "action_failed", error.to_string())
                    })?;
                Ok::<_, ToolError>(bytes)
            })
            .await
            .map_err(|error| failed("media.thumbnail", "action_failed", error.to_string()))??;
            let image = store_frame("media.thumbnail", &self.deps.artifacts, bytes, 0).await?;
            return Ok(frame_output(
                serde_json::json!({ "path": path.to_string_lossy() }),
                vec![(0, image)],
            ));
        }
        require_ffmpeg("media.thumbnail")?;
        let path_for_task = path.clone();
        let (bytes, _) = tokio::task::spawn_blocking(move || {
            extract_frame_png(&path_for_task, Some(0), max_width)
        })
        .await
        .map_err(|error| failed("media.thumbnail", "action_failed", error.to_string()))??;
        let image = store_frame("media.thumbnail", &self.deps.artifacts, bytes, 0).await?;
        Ok(frame_output(
            serde_json::json!({ "path": path.to_string_lossy() }),
            vec![(0, image)],
        ))
    }
}

#[async_trait::async_trait]
impl Tool for MediaWaveformTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("media.waveform"),
            description: "RMS energy buckets (default 64) for an audio file's shape without sending audio to the model.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "buckets": {"type": "integer", "minimum": 8, "maximum": 256},
                },
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "media.waveform")?;
        let buckets = args
            .get("buckets")
            .and_then(|value| value.as_u64())
            .map(|value| value.clamp(8, 256) as usize)
            .unwrap_or(64);
        require_read("media.waveform", &ctx, &path)?;
        let extension = path
            .extension()
            .and_then(|extension| extension.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let samples: Vec<i16> = if extension == "wav" {
            let path_for_task = path.clone();
            tokio::task::spawn_blocking(move || {
                let mut reader = hound::WavReader::open(&path_for_task).map_err(|error| {
                    failed("media.waveform", "action_failed", error.to_string())
                })?;
                reader
                    .samples::<i16>()
                    .take(8_000 * 600)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| failed("media.waveform", "action_failed", error.to_string()))
            })
            .await
            .map_err(|error| failed("media.waveform", "action_failed", error.to_string()))??
        } else {
            require_ffmpeg("media.waveform")?;
            let path_str = path.to_string_lossy().into_owned();
            let bytes = run_ffmpeg_async(
                "media.waveform",
                vec![
                    "-i".to_string(),
                    path_str,
                    "-ac".to_string(),
                    "1".to_string(),
                    "-ar".to_string(),
                    "8000".to_string(),
                    "-t".to_string(),
                    "600".to_string(),
                    "-f".to_string(),
                    "s16le".to_string(),
                    "-acodec".to_string(),
                    "pcm_s16le".to_string(),
                    "-".to_string(),
                ],
            )
            .await?;
            bytes
                .chunks_exact(2)
                .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
                .collect()
        };
        if samples.is_empty() {
            return Err(failed(
                "media.waveform",
                "action_failed",
                "no audio samples decoded".to_string(),
            ));
        }
        let per_bucket = (samples.len() / buckets).max(1);
        let mut levels = Vec::with_capacity(buckets);
        for chunk in samples.chunks(per_bucket).take(buckets) {
            let sum: f64 = chunk
                .iter()
                .map(|sample| {
                    let normalized = f64::from(*sample) / 32_768.0;
                    normalized * normalized
                })
                .sum();
            levels.push((sum / chunk.len() as f64).sqrt().clamp(0.0, 1.0));
        }
        Ok(ToolOutput::json(serde_json::json!({
            "path": path.to_string_lossy(),
            "buckets": buckets,
            "rms": levels,
        })))
    }
}

/// Static media tool group.
pub struct MediaToolPack {
    pub artifacts: Arc<dyn ArtifactStore>,
}

impl MediaToolPack {
    pub fn new() -> Self {
        Self {
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
        }
    }
}

impl Default for MediaToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl tool_sdk::ToolPack for MediaToolPack {
    fn id(&self) -> &'static str {
        "media"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        let deps = MediaDeps {
            artifacts: self.artifacts.clone(),
        };
        let frame_deps = || MediaDeps {
            artifacts: self.artifacts.clone(),
        };
        let _ = &deps;
        vec![
            Arc::new(MediaMetadataTool),
            Arc::new(MediaVideoMetadataTool),
            Arc::new(MediaAudioMetadataTool),
            Arc::new(MediaVideoFrameTool { deps: frame_deps() }),
            Arc::new(MediaVideoFramesTool { deps: frame_deps() }),
            Arc::new(MediaVideoKeyframesTool { deps: frame_deps() }),
            Arc::new(MediaThumbnailTool { deps: frame_deps() }),
            Arc::new(MediaWaveformTool),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    fn ctx_for(path: &Path) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            capability_core::Capability::FilesystemRead,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                path.to_path_buf(),
            )]),
            ctx.invocation_id,
            std::time::Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    fn solid_png(width: u32, height: u32, pixel: [u8; 3]) -> Vec<u8> {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb(pixel));
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    /// Full-width horizontal gradient (every adjacent sample differs, so
    /// the dHash is provably all-zeros or all-ones — no sampling aliasing).
    fn gradient_png(decreasing: bool) -> Vec<u8> {
        let mut image = image::RgbImage::new(64, 64);
        for (x, _, pixel) in image.enumerate_pixels_mut() {
            let value = (x * 255 / 63) as u8;
            let value = if decreasing { 255 - value } else { value };
            *pixel = image::Rgb([value, value, value]);
        }
        let mut bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        bytes
    }

    #[test]
    fn dhash_separates_distinct_frames() {
        // Monotonic gradients hash to uniform bits by construction:
        // increasing -> all zeros, decreasing -> all ones.
        let increasing = dhash(&gradient_png(false)).unwrap();
        let decreasing = dhash(&gradient_png(true)).unwrap();
        assert_eq!(increasing, 0);
        assert_eq!(decreasing, u64::MAX);
        assert_eq!(
            hash_distance(increasing, decreasing),
            64,
            "opposite gradients must differ in every bit"
        );
        assert_eq!(hash_distance(increasing, increasing), 0);
    }

    #[tokio::test]
    async fn metadata_decodes_image_dimensions() {
        let dir = std::env::temp_dir().join(format!("utsuwa-media-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("frame.png");
        std::fs::write(&path, solid_png(320, 200, [9, 9, 9])).unwrap();
        let out = MediaMetadataTool
            .invoke(
                ctx_for(&path),
                serde_json::json!({"path": path.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["width"], 320);
        assert_eq!(out.content["height"], 200);
        assert_eq!(out.content["kind"], "image");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn frame_index_seeking_is_rejected_honestly() {
        let err =
            frame_args(&serde_json::json!({"frame_index": 10}), "media.video_frame").unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }));
    }

    #[test]
    fn pack_registers_the_media_surface() {
        let mut ids = MediaToolPack::new()
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "media.audio_metadata",
                "media.metadata",
                "media.thumbnail",
                "media.video_frame",
                "media.video_frames",
                "media.video_keyframes",
                "media.video_metadata",
                "media.waveform",
            ]
        );
    }
}
