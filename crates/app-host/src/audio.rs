use audio_capture::{
    AudioCapture, AudioCaptureConfig, AudioError, CaptureEvent, CaptureInfo, CaptureStats,
};
use serde_json::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;
use wry::http::{Request, Response, StatusCode};

const MEDIA_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_MEDIA_ENTRIES: usize = 8;
pub const MEDIA_PATH_PREFIX: &str = "/__media/";

struct MediaEntry {
    inserted_at: Instant,
    wav_data: Vec<u8>,
}

/// Host-owned short-lived media handles. Audio never crosses the IPC bridge;
/// the WebView fetches the bytes from `companion://app/__media/{id}` instead.
pub struct MediaRegistry {
    entries: Mutex<HashMap<String, MediaEntry>>,
}

impl MediaRegistry {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }

    pub fn insert(&self, id: String, wav_data: Vec<u8>) -> Result<(), AudioError> {
        let mut entries = self
            .entries
            .lock()
            .map_err(|_| AudioError::Worker("media registry lock poisoned".to_string()))?;
        purge_expired(&mut entries);
        while entries.len() >= MAX_MEDIA_ENTRIES {
            let oldest = entries
                .iter()
                .min_by_key(|(_, entry)| entry.inserted_at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                entries.remove(&oldest);
            } else {
                break;
            }
        }
        entries.insert(
            id,
            MediaEntry {
                inserted_at: Instant::now(),
                wav_data,
            },
        );
        Ok(())
    }

    pub fn handle(&self, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
        let uri = request.uri();
        let host = uri.host().unwrap_or("");
        let path = uri.path();
        let capture_id = media_capture_id(path);
        let valid_route = uri.scheme_str() == Some(crate::protocol::APP_SCHEME)
            && uri.host() == Some(crate::protocol::APP_HOST)
            && capture_id.is_some();

        let (status, body, wav_bytes): (StatusCode, Cow<'static, [u8]>, usize) = if !valid_route {
            (StatusCode::FORBIDDEN, Cow::Borrowed(b"forbidden"), 0)
        } else {
            let id = capture_id.expect("validated media capture id");
            let body = self.entries.lock().ok().and_then(|mut entries| {
                purge_expired(&mut entries);
                entries.get(id).map(|entry| entry.wav_data.clone())
            });
            match body {
                Some(bytes) => {
                    let wav_bytes = bytes.len();
                    (StatusCode::OK, Cow::Owned(bytes), wav_bytes)
                }
                None => (StatusCode::NOT_FOUND, Cow::Borrowed(b"not found"), 0),
            }
        };

        tracing::info!(
            host,
            path,
            capture_id = capture_id.unwrap_or(""),
            response_status = status.as_u16(),
            wav_bytes,
            "audio media request"
        );
        response(
            status,
            body,
            if status == StatusCode::OK {
                "audio/wav"
            } else {
                "text/plain"
            },
        )
    }
}

pub fn is_media_path(path: &str) -> bool {
    path == "/__media" || path.starts_with(MEDIA_PATH_PREFIX)
}

fn media_capture_id(path: &str) -> Option<&str> {
    let id = path.strip_prefix(MEDIA_PATH_PREFIX)?;
    if id.is_empty()
        || id.contains('/')
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return None;
    }
    Some(id)
}

impl Default for MediaRegistry {
    fn default() -> Self {
        Self::new()
    }
}

fn purge_expired(entries: &mut HashMap<String, MediaEntry>) {
    let now = Instant::now();
    entries.retain(|_, entry| now.duration_since(entry.inserted_at) < MEDIA_TTL);
}

fn response(
    status: StatusCode,
    body: Cow<'static, [u8]>,
    content_type: &'static str,
) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(status)
        .header("Content-Type", content_type)
        .header("Cache-Control", "no-store")
        // The dev WebView page is http://localhost while media is still served
        // from the same companion://app origin in bundled builds; a wildcard
        // keeps the dev page able to fetch the handle and is safe here because
        // the registry is local-only, short-lived, and contains no credentials.
        .header("Access-Control-Allow-Origin", "*")
        .body(body)
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[])))
}

/// Coordinates the crate-level capture handle with the typed host bridge.
pub struct AudioCaptureManager {
    capture: Mutex<AudioCapture>,
    active_id: Mutex<Option<String>>,
    media: Arc<MediaRegistry>,
    emit: crate::runtime::EmitFn,
}

impl AudioCaptureManager {
    pub fn new(media: Arc<MediaRegistry>, emit: crate::runtime::EmitFn) -> Self {
        Self {
            capture: Mutex::new(AudioCapture::new()),
            active_id: Mutex::new(None),
            media,
            emit,
        }
    }

    pub fn media_registry(&self) -> Arc<MediaRegistry> {
        Arc::clone(&self.media)
    }

    pub fn start(&self, config: AudioCaptureConfig) -> Result<Value, AudioError> {
        let capture_id = Uuid::new_v4().simple().to_string();
        let event_id = capture_id.clone();
        let emit = Arc::clone(&self.emit);
        let callback = move |event: CaptureEvent| {
            let data = serde_json::json!({
                "capture_id": event_id,
                "event": event,
            });
            emit(ipc_core::HostEvent {
                event: "audio.capture".to_string(),
                data,
            });
        };

        let mut capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::Worker("audio capture lock poisoned".to_string()))?;
        let info = capture.start(config, callback)?;
        *self
            .active_id
            .lock()
            .map_err(|_| AudioError::Worker("audio capture lock poisoned".to_string()))? =
            Some(capture_id.clone());
        Ok(start_payload(capture_id, info))
    }

    pub fn stop(&self) -> Result<Value, AudioError> {
        let capture_id = self
            .active_id
            .lock()
            .map_err(|_| AudioError::Worker("audio capture lock poisoned".to_string()))?
            .clone()
            .ok_or(AudioError::NotRunning)?;
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::Worker("audio capture lock poisoned".to_string()))?;
        let audio = match capture.stop() {
            Ok(audio) => audio,
            Err(error) => {
                if let Ok(mut active_id) = self.active_id.lock() {
                    *active_id = None;
                }
                return Err(error);
            }
        };
        let stats = capture.stats().cloned().unwrap_or_else(|| CaptureStats {
            current_rms: 0.0,
            peak_rms: 0.0,
            noise_floor: 0.0,
            speech_threshold: 0.0,
            speech_candidate_active: false,
            speech_detected: false,
            silence_duration_ms: 0,
            duration_ms: audio.duration_ms,
            chunk_count: 0,
            dropped_chunks: 0,
        });
        let wav_bytes = audio.bytes;
        let duration_ms = audio.duration_ms;
        let sample_rate = audio.sample_rate;
        let channels = audio.channels;
        self.media.insert(capture_id.clone(), audio.wav_data)?;
        if let Ok(mut active_id) = self.active_id.lock() {
            *active_id = None;
        }
        Ok(serde_json::json!({
            "capture_id": capture_id,
            "media_url": format!(
                "{}://{}/__media/{}",
                crate::protocol::APP_SCHEME,
                crate::protocol::APP_HOST,
                capture_id
            ),
            "mime_type": "audio/wav",
            "sample_rate": sample_rate,
            "channels": channels,
            "duration_ms": duration_ms,
            "wav_bytes": wav_bytes,
            "stats": stats,
        }))
    }

    pub fn cancel(&self) -> Result<(), AudioError> {
        let mut capture = self
            .capture
            .lock()
            .map_err(|_| AudioError::Worker("audio capture lock poisoned".to_string()))?;
        capture.cancel();
        if let Ok(mut active_id) = self.active_id.lock() {
            *active_id = None;
        }
        Ok(())
    }
}

impl Drop for AudioCaptureManager {
    fn drop(&mut self) {
        if let Ok(capture) = self.capture.get_mut() {
            capture.cancel();
        }
    }
}

fn start_payload(capture_id: String, info: CaptureInfo) -> Value {
    serde_json::json!({
        "capture_id": capture_id,
        "backend": "native-cpal",
        "device": info.device,
        "sample_rate": info.sample_rate,
        "channels": info.channels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wry::http::Request;

    #[test]
    fn media_registry_serves_a_capture_by_handle_without_base64() {
        let registry = MediaRegistry::new();
        registry
            .insert("capture-1".to_string(), vec![1, 2, 3, 4])
            .unwrap();

        let request = Request::builder()
            .uri("companion://app/__media/capture-1")
            .body(Vec::new())
            .unwrap();
        let response = registry.handle(request);

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.body().as_ref(), &[1, 2, 3, 4]);
        assert_eq!(response.headers()["Content-Type"], "audio/wav");
    }

    #[test]
    fn media_registry_does_not_expose_unknown_handles() {
        let registry = MediaRegistry::new();
        let request = Request::builder()
            .uri("companion://app/__media/missing")
            .body(Vec::new())
            .unwrap();
        let response = registry.handle(request);

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn media_registry_rejects_traversal_and_non_media_paths() {
        let registry = MediaRegistry::new();
        for uri in [
            "companion://app/__media/../capture-1",
            "companion://app/__media/%2e%2e",
            "companion://app/__media/capture-1/other",
            "companion://app/app.js",
            "companion://media/__media/capture-1",
        ] {
            let request = Request::builder().uri(uri).body(Vec::new()).unwrap();
            assert_eq!(
                registry.handle(request).status(),
                StatusCode::FORBIDDEN,
                "{uri}"
            );
        }
    }

    #[test]
    fn media_path_routing_does_not_claim_app_assets() {
        assert!(is_media_path("/__media/capture-1"));
        assert!(is_media_path("/__media/../capture-1"));
        assert!(!is_media_path("/app.js"));
        assert!(!is_media_path("/app"));
    }
}
