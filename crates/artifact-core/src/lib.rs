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
}

impl ContentPart {
    pub fn artifact(&self) -> Option<&ArtifactRef> {
        match self {
            Self::Image(image) => Some(&image.artifact),
            Self::Audio(artifact) | Self::Video(artifact) => Some(artifact),
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

#[async_trait::async_trait]
pub trait ArtifactStore: Send + Sync {
    async fn put(&self, mime_type: &str, bytes: Vec<u8>) -> Result<ArtifactRef, ArtifactError>;

    async fn get(&self, id: &ArtifactId) -> Result<Vec<u8>, ArtifactError>;

    async fn delete(&self, id: &ArtifactId) -> Result<(), ArtifactError>;
}

struct StoredArtifact {
    reference: ArtifactRef,
    bytes: Vec<u8>,
    expires_at: Instant,
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
}

#[async_trait::async_trait]
impl ArtifactStore for InMemoryArtifactStore {
    async fn put(&self, mime_type: &str, bytes: Vec<u8>) -> Result<ArtifactRef, ArtifactError> {
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
        state.bytes = state.bytes.saturating_add(bytes.len());
        state.artifacts.insert(
            id,
            StoredArtifact {
                reference: reference.clone(),
                bytes,
                expires_at: Instant::now() + self.ttl,
            },
        );
        Ok(reference)
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
}
