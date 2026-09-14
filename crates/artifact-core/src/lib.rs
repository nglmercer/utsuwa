//! Provider-neutral binary artifacts.
//!
//! Tool and model transcripts should carry small, typed references to media,
//! not the media bytes themselves.  The store is deliberately an interface:
//! the initial implementation is an expiring in-memory store, while a host
//! may later replace it with a temporary-filesystem or encrypted store without
//! changing the tool/model boundary.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(pub String);

impl ArtifactId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn fresh() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Image,
    Audio,
    Video,
    Binary,
}

impl ArtifactKind {
    pub fn from_mime_type(mime_type: &str) -> Self {
        let mime_type = mime_type.to_ascii_lowercase();
        if mime_type.starts_with("image/") {
            Self::Image
        } else if mime_type.starts_with("audio/") {
            Self::Audio
        } else if mime_type.starts_with("video/") {
            Self::Video
        } else {
            Self::Binary
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactRef {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub mime_type: String,
    pub size_bytes: u64,
}

impl ArtifactRef {
    pub fn new(id: ArtifactId, mime_type: impl Into<String>, size_bytes: u64) -> Self {
        let mime_type = mime_type.into();
        Self {
            id,
            kind: ArtifactKind::from_mime_type(&mime_type),
            mime_type,
            size_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageArtifactRef {
    pub artifact: ArtifactRef,
    pub width: u32,
    pub height: u32,
}

impl ImageArtifactRef {
    pub fn new(artifact: ArtifactRef, width: u32, height: u32) -> Self {
        Self {
            artifact,
            width,
            height,
        }
    }
}

/// A provider-neutral content part returned by a tool or supplied to a
/// provider adapter. JSON is retained for ordinary tools; media is resolved
/// only at the final adapter boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ContentPart {
    Text(String),
    Json(serde_json::Value),
    Image(ImageArtifactRef),
    Audio(ArtifactRef),
    Video(ArtifactRef),
    /// Opaque bytes (downloads, archives) carried by artifact reference.
    /// The model receives id/mime/size metadata; bytes stay in the store
    /// until an authorized filesystem write materializes them.
    Binary(ArtifactRef),
}

impl ContentPart {
    pub fn artifact(&self) -> Option<&ArtifactRef> {
        match self {
            Self::Image(image) => Some(&image.artifact),
            Self::Audio(artifact) | Self::Video(artifact) | Self::Binary(artifact) => {
                Some(artifact)
            }
            Self::Text(_) | Self::Json(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArtifactError {
    #[error("artifact not found: {0}")]
    NotFound(String),
    #[error("artifact mime type is empty")]
    InvalidMimeType,
    #[error("artifact is too large ({size_bytes} bytes; limit {limit_bytes})")]
    TooLarge {
        size_bytes: usize,
        limit_bytes: usize,
    },
    #[error("artifact store capacity exceeded")]
    CapacityExceeded,
    #[error("artifact store lock failed")]
    Lock,
}

/// Where an artifact came from. Drives lifecycle: captures and microphone
/// recordings expire quickly and are deleted when their session stops;
/// explicit files and downloads live longer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactSource {
    ScreenCapture,
    Camera,
    AudioCapture,
    File,
    Download,
    Generated,
    Tool,
}

/// Lifecycle metadata for one stored artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactMetadata {
    pub id: ArtifactId,
    pub kind: ArtifactKind,
    pub mime_type: String,
    pub size_bytes: u64,
    /// Unix millis when the artifact was stored.
    pub created_at_ms: u64,
    /// Unix millis when the artifact expires, if bounded.
    pub expires_at_ms: Option<u64>,
    pub source: ArtifactSource,
    /// Sensitive artifacts (screen, camera, microphone, secrets) expire
    /// sooner and are never written to logs or audit records.
    pub sensitive: bool,
}

#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, mime_type: &str, bytes: Vec<u8>) -> Result<ArtifactRef, ArtifactError>;

    /// Store with lifecycle metadata. The default implementation ignores
    /// the source/sensitivity (custom stores keep their own policy).
    async fn put_with_source(
        &self,
        mime_type: &str,
        bytes: Vec<u8>,
        _source: ArtifactSource,
        _sensitive: bool,
    ) -> Result<ArtifactRef, ArtifactError> {
        self.put(mime_type, bytes).await
    }

    async fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactError>;

    async fn delete(&self, id: &ArtifactId) -> Result<(), ArtifactError>;

    /// Lifecycle metadata, when the store tracks it.
    async fn metadata(&self, _id: &ArtifactId) -> Option<ArtifactMetadata> {
        None
    }

    /// Delete every artifact from one source (session teardown). Returns
    /// the number of artifacts removed.
    async fn delete_source(&self, _source: ArtifactSource) -> usize {
        0
    }
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// TTL per source. Captures, camera frames, and microphone audio are
/// sensitive by default and expire quickly; explicit files and downloads
/// persist for the host session.
fn ttl_for_source(source: ArtifactSource, sensitive: bool) -> Duration {
    if sensitive {
        return Duration::from_secs(5 * 60);
    }
    match source {
        ArtifactSource::ScreenCapture | ArtifactSource::Camera | ArtifactSource::AudioCapture => {
            Duration::from_secs(10 * 60)
        }
        ArtifactSource::Download | ArtifactSource::File => Duration::from_secs(60 * 60),
        ArtifactSource::Generated | ArtifactSource::Tool => Duration::from_secs(30 * 60),
    }
}

struct StoredArtifact {
    reference: ArtifactRef,
    bytes: Vec<u8>,
    expires_at: Instant,
    created_at_ms: u64,
    source: ArtifactSource,
    sensitive: bool,
}

struct StoreState {
    artifacts: HashMap<ArtifactId, StoredArtifact>,
    bytes: usize,
}

/// Expiring in-memory artifact store. Expired entries are removed lazily on
/// every operation, which keeps the store runtime-independent and means a
/// host does not need to spawn a cleanup task just to avoid retaining a
/// screenshot after its normal lifetime.
#[derive(Clone)]
pub struct InMemoryArtifactStore {
    state: Arc<Mutex<StoreState>>,
    max_artifacts: usize,
    max_bytes: usize,
    max_artifact_bytes: usize,
    ttl: Duration,
}

impl fmt::Debug for InMemoryArtifactStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InMemoryArtifactStore")
            .field("max_artifacts", &self.max_artifacts)
            .field("max_bytes", &self.max_bytes)
            .field("max_artifact_bytes", &self.max_artifact_bytes)
            .field("ttl", &self.ttl)
            .finish()
    }
}

impl Default for InMemoryArtifactStore {
    fn default() -> Self {
        Self::with_limits(
            128,
            128 * 1024 * 1024,
            8 * 1024 * 1024,
            Duration::from_secs(10 * 60),
        )
    }
}

impl InMemoryArtifactStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(
        max_artifacts: usize,
        max_bytes: usize,
        max_artifact_bytes: usize,
        ttl: Duration,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(StoreState {
                artifacts: HashMap::new(),
                bytes: 0,
            })),
            max_artifacts: max_artifacts.max(1),
            max_bytes: max_bytes.max(1),
            max_artifact_bytes: max_artifact_bytes.max(1),
            ttl,
        }
    }

    pub fn len(&self) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        self.sweep_locked(&mut state);
        state.artifacts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn bytes(&self) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        self.sweep_locked(&mut state);
        state.bytes
    }

    pub fn contains(&self, id: &ArtifactId) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        self.sweep_locked(&mut state);
        state.artifacts.contains_key(id)
    }

    pub fn sweep(&self) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        self.sweep_locked(&mut state)
    }

    fn sweep_locked(&self, state: &mut StoreState) -> usize {
        let before = state.artifacts.len();
        state.artifacts.retain(|_, artifact| {
            if artifact.expires_at > Instant::now() {
                true
            } else {
                state.bytes = state.bytes.saturating_sub(artifact.bytes.len());
                false
            }
        });
        before.saturating_sub(state.artifacts.len())
    }

    fn remove_oldest_until_fit(&self, state: &mut StoreState, incoming: usize) {
        while (state.artifacts.len() >= self.max_artifacts)
            || state.bytes.saturating_add(incoming) > self.max_bytes
        {
            let Some(oldest_id) = state
                .artifacts
                .iter()
                .min_by_key(|(_, artifact)| artifact.expires_at)
                .map(|(id, _)| id.clone())
            else {
                break;
            };
            if let Some(oldest) = state.artifacts.remove(&oldest_id) {
                state.bytes = state.bytes.saturating_sub(oldest.bytes.len());
            }
        }
    }

    async fn put_inner(
        &self,
        mime_type: &str,
        bytes: Vec<u8>,
        source: ArtifactSource,
        sensitive: bool,
    ) -> Result<ArtifactRef, ArtifactError> {
        let mime_type = mime_type.trim();
        if mime_type.is_empty() {
            return Err(ArtifactError::InvalidMimeType);
        }
        if bytes.len() > self.max_artifact_bytes {
            return Err(ArtifactError::TooLarge {
                size_bytes: bytes.len(),
                limit_bytes: self.max_artifact_bytes,
            });
        }
        let mut state = self.state.lock().map_err(|_| ArtifactError::Lock)?;
        self.sweep_locked(&mut state);
        self.remove_oldest_until_fit(&mut state, bytes.len());
        if state.bytes.saturating_add(bytes.len()) > self.max_bytes {
            return Err(ArtifactError::CapacityExceeded);
        }
        let id = ArtifactId::fresh();
        let reference = ArtifactRef::new(id.clone(), mime_type, bytes.len() as u64);
        // Sensitive artifacts always expire quickly, even when the store
        // default TTL is longer.
        let ttl = self.ttl.min(ttl_for_source(source, sensitive));
        state.bytes = state.bytes.saturating_add(bytes.len());
        state.artifacts.insert(
            id,
            StoredArtifact {
                reference: reference.clone(),
                bytes,
                expires_at: Instant::now() + ttl,
                created_at_ms: unix_millis(),
                source,
                sensitive,
            },
        );
        Ok(reference)
    }
}

#[async_trait::async_trait]
impl ArtifactStore for InMemoryArtifactStore {
    async fn put(&self, mime_type: &str, bytes: Vec<u8>) -> Result<ArtifactRef, ArtifactError> {
        self.put_inner(mime_type, bytes, ArtifactSource::Tool, false)
            .await
    }

    async fn put_with_source(
        &self,
        mime_type: &str,
        bytes: Vec<u8>,
        source: ArtifactSource,
        sensitive: bool,
    ) -> Result<ArtifactRef, ArtifactError> {
        self.put_inner(mime_type, bytes, source, sensitive).await
    }

    async fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactError> {
        let mut state = self.state.lock().map_err(|_| ArtifactError::Lock)?;
        self.sweep_locked(&mut state);
        state
            .artifacts
            .get(id)
            .map(|artifact| artifact.bytes.clone())
            .ok_or_else(|| ArtifactError::NotFound(id.to_string()))
    }

    async fn delete(&self, id: &ArtifactId) -> Result<(), ArtifactError> {
        let mut state = self.state.lock().map_err(|_| ArtifactError::Lock)?;
        self.sweep_locked(&mut state);
        let artifact = state
            .artifacts
            .remove(id)
            .ok_or_else(|| ArtifactError::NotFound(id.to_string()))?;
        state.bytes = state
            .bytes
            .saturating_sub(artifact.reference.size_bytes as usize);
        Ok(())
    }

    async fn metadata(&self, id: &ArtifactId) -> Option<ArtifactMetadata> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        self.sweep_locked(&mut state);
        state.artifacts.get(id).map(|artifact| ArtifactMetadata {
            id: artifact.reference.id.clone(),
            kind: artifact.reference.kind,
            mime_type: artifact.reference.mime_type.clone(),
            size_bytes: artifact.reference.size_bytes,
            created_at_ms: artifact.created_at_ms,
            expires_at_ms: artifact
                .expires_at
                .checked_duration_since(Instant::now())
                .map(|remaining| unix_millis().saturating_add(remaining.as_millis() as u64)),
            source: artifact.source,
            sensitive: artifact.sensitive,
        })
    }

    async fn delete_source(&self, source: ArtifactSource) -> usize {
        let Ok(mut state) = self.state.lock() else {
            return 0;
        };
        self.sweep_locked(&mut state);
        let ids: Vec<ArtifactId> = state
            .artifacts
            .iter()
            .filter(|(_, artifact)| artifact.source == source)
            .map(|(id, _)| id.clone())
            .collect();
        let removed = ids.len();
        for id in ids {
            if let Some(artifact) = state.artifacts.remove(&id) {
                state.bytes = state.bytes.saturating_sub(artifact.bytes.len());
            }
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_and_deletes_binary_without_serializing_bytes() {
        let store = InMemoryArtifactStore::with_limits(4, 64, 64, Duration::from_secs(60));
        let reference = store.put("image/png", vec![1, 2, 3]).await.unwrap();
        assert_eq!(reference.kind, ArtifactKind::Image);
        assert_eq!(reference.size_bytes, 3);
        assert_eq!(store.get(&reference.id).await.unwrap(), vec![1, 2, 3]);
        store.delete(&reference.id).await.unwrap();
        assert!(matches!(
            store.get(&reference.id).await,
            Err(ArtifactError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn expired_artifacts_are_removed_on_access() {
        let store = InMemoryArtifactStore::with_limits(4, 64, 64, Duration::from_millis(1));
        let reference = store
            .put("application/octet-stream", vec![1])
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert_eq!(store.sweep(), 1);
        assert!(!store.contains(&reference.id));
    }

    #[tokio::test]
    async fn limits_reject_oversized_artifacts() {
        let store = InMemoryArtifactStore::with_limits(4, 64, 2, Duration::from_secs(60));
        assert!(matches!(
            store.put("image/png", vec![1, 2, 3]).await,
            Err(ArtifactError::TooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn sourced_artifacts_carry_lifecycle_metadata() {
        let store = InMemoryArtifactStore::with_limits(8, 256, 256, Duration::from_secs(3600));
        let reference = store
            .put_with_source("image/png", vec![9, 9], ArtifactSource::ScreenCapture, true)
            .await
            .unwrap();
        let metadata = store.metadata(&reference.id).await.unwrap();
        assert_eq!(metadata.source, ArtifactSource::ScreenCapture);
        assert!(metadata.sensitive);
        assert!(metadata.created_at_ms > 0);
        // Sensitive captures expire in minutes even with an hour-long store.
        let ttl_ms = metadata
            .expires_at_ms
            .unwrap()
            .saturating_sub(metadata.created_at_ms);
        assert!(ttl_ms <= 5 * 60 * 1000, "{ttl_ms}");

        let download = store
            .put_with_source("application/zip", vec![1], ArtifactSource::Download, false)
            .await
            .unwrap();
        assert_eq!(store.delete_source(ArtifactSource::ScreenCapture).await, 1);
        assert!(!store.contains(&reference.id));
        assert!(store.contains(&download.id));
    }
}
