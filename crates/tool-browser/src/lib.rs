//! Semantic browser tools (`browser.*`).
//!
//! Interaction goes through browser state (tabs, accessibility/DOM nodes),
//! not screenshots: [`BrowserBackend`] exposes tabs plus a compact
//! [`BrowserNode`] snapshot, and tools act on node ids. Screenshots exist
//! only as an explicit fallback.
//!
//! The bundled [`CdpBackend`] speaks the Chrome DevTools Protocol over the
//! DevTools HTTP API plus one WebSocket per command. It connects to a
//! browser the user already runs with remote debugging enabled (for
//! example `chrome --remote-debugging-port=9222`); it never launches or
//! downloads a browser. The trait is backend-agnostic so a WebDriver BiDi
//! backend can be added later without touching the tools.
//!
//! Authorization is domain-based: navigation requires a
//! [`capability_core::Capability::NetworkConnect`] ticket for the target
//! URL, inspection requires observe tickets scoped to the tab, and
//! interactions require control tickets scoped to the tab. Cookie writes
//! are high-risk and marked destructive.

use artifact_core::{ArtifactStore, ContentPart, ImageArtifactRef};
use capability_core::{Capability, Resource};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// One browser tab (a CDP target of type `page`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowserTab {
    pub id: String,
    pub title: String,
    pub url: String,
}

/// Compact accessibility/DOM node. Full HTML never reaches the model:
/// snapshots carry role/name/text/value/href/bounds plus visibility flags,
/// and incremental updates carry only changed nodes.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BrowserNode {
    pub id: String,
    pub role: Option<String>,
    pub tag: String,
    pub name: Option<String>,
    pub text: Option<String>,
    pub value: Option<String>,
    pub href: Option<String>,
    pub bounds: Option<BrowserRect>,
    pub visible: bool,
    pub enabled: bool,
    pub focused: bool,
    pub sensitive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BrowserRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BrowserSnapshot {
    pub snapshot_id: String,
    pub tab_id: String,
    pub url: String,
    pub title: String,
    pub nodes: Vec<BrowserNode>,
    /// Ids removed since `since_snapshot_id` (empty on a full snapshot).
    #[serde(default)]
    pub removed_node_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BrowserCookie {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("browser backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("unknown tab '{0}'")]
    UnknownTab(String),
    #[error("unknown node '{0}'")]
    UnknownNode(String),
    #[error("action failed: {0}")]
    ActionFailed(String),
    #[error("unsupported operation: {0}")]
    Unsupported(String),
    #[error("wait timed out: {0}")]
    Timeout(String),
}

pub const MAX_SNAPSHOT_NODES: usize = 2_000;
pub const MAX_TEXT_CHARS: usize = 8_192;

/// Browser control surface. Methods with no sensible implementation return
/// [`BrowserError::Unsupported`] — never fake success.
#[async_trait::async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn status(&self) -> Result<serde_json::Value, BrowserError>;
    async fn list_tabs(&self) -> Result<Vec<BrowserTab>, BrowserError>;
    async fn open(&self, url: &str) -> Result<BrowserTab, BrowserError>;
    async fn close_tab(&self, tab_id: &str) -> Result<(), BrowserError>;
    async fn navigate(&self, tab_id: &str, url: &str) -> Result<(), BrowserError>;
    async fn back(&self, tab_id: &str) -> Result<(), BrowserError>;
    async fn forward(&self, tab_id: &str) -> Result<(), BrowserError>;
    async fn reload(&self, tab_id: &str) -> Result<(), BrowserError>;
    async fn snapshot(
        &self,
        tab_id: &str,
        since: Option<&str>,
    ) -> Result<BrowserSnapshot, BrowserError>;
    async fn query(
        &self,
        tab_id: &str,
        selector: &str,
        limit: usize,
    ) -> Result<Vec<BrowserNode>, BrowserError>;
    async fn click(&self, tab_id: &str, node_id: &str) -> Result<(), BrowserError>;
    async fn type_text(&self, tab_id: &str, node_id: &str, text: &str) -> Result<(), BrowserError>;
    async fn set_value(&self, tab_id: &str, node_id: &str, value: &str)
        -> Result<(), BrowserError>;
    async fn select(
        &self,
        tab_id: &str,
        node_id: &str,
        values: &[String],
    ) -> Result<(), BrowserError>;
    async fn scroll(
        &self,
        tab_id: &str,
        node_id: Option<&str>,
        dx: i32,
        dy: i32,
    ) -> Result<(), BrowserError>;
    async fn get_text(&self, tab_id: &str, node_id: Option<&str>) -> Result<String, BrowserError>;
    async fn get_attribute(
        &self,
        tab_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<Option<String>, BrowserError>;
    async fn wait_for(
        &self,
        tab_id: &str,
        selector: Option<&str>,
        text: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(), BrowserError>;
    async fn screenshot(
        &self,
        tab_id: &str,
        artifacts: &Arc<dyn ArtifactStore>,
    ) -> Result<ImageArtifactRef, BrowserError>;
    async fn cookies_list(&self, tab_id: &str) -> Result<Vec<BrowserCookie>, BrowserError>;
    async fn cookies_set(&self, tab_id: &str, cookie: BrowserCookie) -> Result<(), BrowserError>;
    async fn cookies_delete(&self, tab_id: &str, name: &str) -> Result<(), BrowserError>;
}

/// Always-available placeholder: honest unavailability, never fake tabs.
pub struct StubBackend;

#[async_trait::async_trait]
impl BrowserBackend for StubBackend {
    async fn status(&self) -> Result<serde_json::Value, BrowserError> {
        Ok(serde_json::json!({"available": false, "backend": "none"}))
    }
    async fn list_tabs(&self) -> Result<Vec<BrowserTab>, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn open(&self, _url: &str) -> Result<BrowserTab, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn close_tab(&self, _tab_id: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn navigate(&self, _tab_id: &str, _url: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn back(&self, _tab_id: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn forward(&self, _tab_id: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn reload(&self, _tab_id: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn snapshot(
        &self,
        _tab_id: &str,
        _since: Option<&str>,
    ) -> Result<BrowserSnapshot, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn query(
        &self,
        _tab_id: &str,
        _selector: &str,
        _limit: usize,
    ) -> Result<Vec<BrowserNode>, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn click(&self, _tab_id: &str, _node_id: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn type_text(
        &self,
        _tab_id: &str,
        _node_id: &str,
        _text: &str,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn set_value(
        &self,
        _tab_id: &str,
        _node_id: &str,
        _value: &str,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn select(
        &self,
        _tab_id: &str,
        _node_id: &str,
        _values: &[String],
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn scroll(
        &self,
        _tab_id: &str,
        _node_id: Option<&str>,
        _dx: i32,
        _dy: i32,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn get_text(
        &self,
        _tab_id: &str,
        _node_id: Option<&str>,
    ) -> Result<String, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn get_attribute(
        &self,
        _tab_id: &str,
        _node_id: &str,
        _name: &str,
    ) -> Result<Option<String>, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn wait_for(
        &self,
        _tab_id: &str,
        _selector: Option<&str>,
        _text: Option<&str>,
        _timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn screenshot(
        &self,
        _tab_id: &str,
        _artifacts: &Arc<dyn ArtifactStore>,
    ) -> Result<ImageArtifactRef, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn cookies_list(&self, _tab_id: &str) -> Result<Vec<BrowserCookie>, BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn cookies_set(&self, _tab_id: &str, _cookie: BrowserCookie) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
    async fn cookies_delete(&self, _tab_id: &str, _name: &str) -> Result<(), BrowserError> {
        Err(BrowserError::BackendUnavailable(
            "no browser backend is connected".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// CDP backend
// ---------------------------------------------------------------------------

/// Chrome DevTools Protocol backend over a user-provided debugging endpoint
/// (`http://localhost:9222`). Tab management uses the DevTools HTTP API;
/// page interaction opens one WebSocket per command to the tab's
/// `webSocketDebuggerUrl`. No browser is launched or downloaded here.
#[derive(Debug, Clone)]
pub struct CdpBackend {
    http: reqwest::Client,
    endpoint: String,
}

impl CdpBackend {
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .expect("reqwest client builds"),
            endpoint: endpoint.into().trim_end_matches('/').to_string(),
        }
    }

    async fn targets(&self) -> Result<Vec<serde_json::Value>, BrowserError> {
        let response = self
            .http
            .get(format!("{}/json/list", self.endpoint))
            .send()
            .await
            .map_err(|error| {
                BrowserError::BackendUnavailable(format!("CDP endpoint unreachable: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(BrowserError::BackendUnavailable(format!(
                "CDP endpoint returned HTTP {}",
                response.status()
            )));
        }
        response.json().await.map_err(|error| {
            BrowserError::ActionFailed(format!("invalid CDP target list: {error}"))
        })
    }

    fn page_targets(targets: &[serde_json::Value]) -> Vec<BrowserTab> {
        targets
            .iter()
            .filter(|target| target.get("type").and_then(|kind| kind.as_str()) == Some("page"))
            .filter_map(|target| {
                Some(BrowserTab {
                    id: target.get("id")?.as_str()?.to_string(),
                    title: target
                        .get("title")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    url: target
                        .get("url")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                })
            })
            .collect()
    }

    async fn ws_url(&self, tab_id: &str) -> Result<String, BrowserError> {
        let targets = self.targets().await?;
        targets
            .iter()
            .find(|target| {
                target.get("id").and_then(|id| id.as_str()) == Some(tab_id)
                    && target.get("type").and_then(|kind| kind.as_str()) == Some("page")
            })
            .and_then(|target| {
                target
                    .get("webSocketDebuggerUrl")
                    .and_then(|url| url.as_str())
            })
            .map(str::to_string)
            .ok_or_else(|| BrowserError::UnknownTab(tab_id.to_string()))
    }

    /// Send one CDP command and await its result object.
    async fn command(
        &self,
        tab_id: &str,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, BrowserError> {
        use futures_util::{SinkExt, StreamExt};
        let url = self.ws_url(tab_id).await?;
        let (stream, _) = tokio_tungstenite::connect_async(&url)
            .await
            .map_err(|error| {
                BrowserError::BackendUnavailable(format!("CDP websocket failed: {error}"))
            })?;
        let (mut sink, mut source) = stream.split();
        let id = 1;
        sink.send(tokio_tungstenite::tungstenite::Message::Text(
            serde_json::json!({"id": id, "method": method, "params": params})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|error| BrowserError::ActionFailed(format!("CDP send failed: {error}")))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(BrowserError::Timeout(format!("{method} timed out")));
            }
            let message = tokio::time::timeout(remaining, source.next())
                .await
                .map_err(|_| BrowserError::Timeout(format!("{method} timed out")))?
                .ok_or_else(|| BrowserError::ActionFailed("CDP connection closed".to_string()))?
                .map_err(|error| BrowserError::ActionFailed(format!("CDP read failed: {error}")))?;
            let text = match message {
                tokio_tungstenite::tungstenite::Message::Text(text) => text,
                _ => continue,
            };
            let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| {
                BrowserError::ActionFailed(format!("invalid CDP frame: {error}"))
            })?;
            if value.get("id").and_then(|value| value.as_u64()) == Some(id) {
                if let Some(error) = value.get("error") {
                    return Err(BrowserError::ActionFailed(format!(
                        "CDP {method} failed: {error}"
                    )));
                }
                return Ok(value
                    .get("result")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null));
            }
            // Event frames (Page.loadEventFired, …) are ignored on single-shot commands.
        }
    }

    /// Evaluate JavaScript in the tab and return the RemoteObject result.
    async fn evaluate(
        &self,
        tab_id: &str,
        expression: &str,
        await_promise: bool,
    ) -> Result<serde_json::Value, BrowserError> {
        let result = self
            .command(
                tab_id,
                "Runtime.evaluate",
                serde_json::json!({
                    "expression": expression,
                    "awaitPromise": await_promise,
                    "returnByValue": true,
                    "userGesture": true,
                }),
            )
            .await?;
        let remote = result
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        if remote.get("subtype").and_then(|value| value.as_str()) == Some("error") {
            return Err(BrowserError::ActionFailed(format!(
                "page script failed: {}",
                remote
                    .get("description")
                    .and_then(|value| value.as_str())
                    .unwrap_or("unknown")
            )));
        }
        Ok(remote
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null))
    }
}

fn truncate_text(text: &str) -> String {
    let mut out: String = text.chars().take(MAX_TEXT_CHARS).collect();
    if text.chars().count() > MAX_TEXT_CHARS {
        out.push_str("…[truncated]");
    }
    out
}

/// Local sensitivity check for browser nodes (password/token inputs).
/// Mirrors the desktop heuristic so secret values never reach the model.
pub fn browser_node_sensitive(role: Option<&str>, name: Option<&str>, tag: &str) -> bool {
    let haystack = format!(
        "{} {} {}",
        role.unwrap_or_default(),
        name.unwrap_or_default(),
        tag
    )
    .to_lowercase();
    haystack.contains("password")
        || haystack.contains("passcode")
        || haystack.contains("token")
        || haystack.contains("secret")
}

pub const BROWSER_SENSITIVE_MASK: &str = "••••••••";

fn ax_node_to_browser(node: &serde_json::Value) -> Option<BrowserNode> {
    let node_id = node
        .get("nodeId")
        .and_then(|value| value.as_str())?
        .to_string();
    let role = node
        .get("role")
        .and_then(|value| value.get("value"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let name = node
        .get("name")
        .and_then(|value| value.get("value"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let description = node
        .get("description")
        .and_then(|value| value.get("value"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let mut value = node
        .get("value")
        .and_then(|value| value.get("value"))
        .and_then(|value| value.as_str())
        .map(str::to_string);
    let tag = role.clone().unwrap_or_else(|| "unknown".to_string());
    let sensitive = browser_node_sensitive(
        role.as_deref(),
        name.as_deref().or(description.as_deref()),
        &tag,
    );
    if sensitive {
        value = Some(BROWSER_SENSITIVE_MASK.to_string());
    }
    Some(BrowserNode {
        id: format!("ax-{node_id}"),
        role,
        tag,
        name,
        text: description,
        value,
        href: None,
        bounds: None,
        visible: node
            .get("ignored")
            .and_then(|value| value.as_bool())
            .map(|ignored| !ignored)
            .unwrap_or(true),
        enabled: node
            .get("disabled")
            .and_then(|value| value.as_bool())
            .map(|disabled| !disabled)
            .unwrap_or(true),
        focused: node
            .get("focused")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        sensitive,
    })
}

fn dom_match_to_browser(index: usize, value: &serde_json::Value) -> BrowserNode {
    let name = value
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let tag = value
        .get("tag")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    let sensitive = browser_node_sensitive(None, name.as_deref(), &tag);
    BrowserNode {
        id: format!("dom-{index}"),
        role: value
            .get("role")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        tag,
        name,
        text: name_clone(value),
        value: if sensitive {
            Some(BROWSER_SENSITIVE_MASK.to_string())
        } else {
            value
                .get("value")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        },
        href: value
            .get("href")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        bounds: value.get("bounds").and_then(|bounds| {
            Some(BrowserRect {
                x: bounds.get("x")?.as_f64()?,
                y: bounds.get("y")?.as_f64()?,
                width: bounds.get("width")?.as_f64()?,
                height: bounds.get("height")?.as_f64()?,
            })
        }),
        visible: value
            .get("visible")
            .and_then(|v| v.as_bool())
            .unwrap_or(true),
        enabled: true,
        focused: false,
        sensitive,
    }
}

fn name_clone(value: &serde_json::Value) -> Option<String> {
    value
        .get("text")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

#[async_trait::async_trait]
impl BrowserBackend for CdpBackend {
    async fn status(&self) -> Result<serde_json::Value, BrowserError> {
        let response = self
            .http
            .get(format!("{}/json/version", self.endpoint))
            .send()
            .await
            .map_err(|error| {
                BrowserError::BackendUnavailable(format!("CDP endpoint unreachable: {error}"))
            })?;
        if !response.status().is_success() {
            return Err(BrowserError::BackendUnavailable(format!(
                "CDP endpoint returned HTTP {}",
                response.status()
            )));
        }
        let mut info: serde_json::Value = response.json().await.map_err(|error| {
            BrowserError::ActionFailed(format!("invalid CDP version payload: {error}"))
        })?;
        info["available"] = serde_json::Value::Bool(true);
        Ok(info)
    }

    async fn list_tabs(&self) -> Result<Vec<BrowserTab>, BrowserError> {
        Ok(Self::page_targets(&self.targets().await?))
    }

    async fn open(&self, url: &str) -> Result<BrowserTab, BrowserError> {
        let response = self
            .http
            .put(format!(
                "{}/json/new?{}",
                self.endpoint,
                serde_urlencoded(url)
            ))
            .send()
            .await
            .map_err(|error| BrowserError::ActionFailed(format!("CDP open failed: {error}")))?;
        if !response.status().is_success() {
            return Err(BrowserError::ActionFailed(format!(
                "CDP open returned HTTP {}",
                response.status()
            )));
        }
        let target: serde_json::Value = response.json().await.map_err(|error| {
            BrowserError::ActionFailed(format!("invalid CDP open payload: {error}"))
        })?;
        Ok(BrowserTab {
            id: target
                .get("id")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            title: target
                .get("title")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string(),
            url: target
                .get("url")
                .and_then(|value| value.as_str())
                .unwrap_or(url)
                .to_string(),
        })
    }

    async fn close_tab(&self, tab_id: &str) -> Result<(), BrowserError> {
        let response = self
            .http
            .get(format!("{}/json/close/{tab_id}", self.endpoint))
            .send()
            .await
            .map_err(|error| BrowserError::ActionFailed(format!("CDP close failed: {error}")))?;
        if !response.status().is_success() {
            return Err(BrowserError::UnknownTab(tab_id.to_string()));
        }
        Ok(())
    }

    async fn navigate(&self, tab_id: &str, url: &str) -> Result<(), BrowserError> {
        self.command(tab_id, "Page.navigate", serde_json::json!({"url": url}))
            .await?;
        Ok(())
    }

    async fn back(&self, tab_id: &str) -> Result<(), BrowserError> {
        self.evaluate(tab_id, "history.back()", false).await?;
        Ok(())
    }

    async fn forward(&self, tab_id: &str) -> Result<(), BrowserError> {
        self.evaluate(tab_id, "history.forward()", false).await?;
        Ok(())
    }

    async fn reload(&self, tab_id: &str) -> Result<(), BrowserError> {
        self.command(tab_id, "Page.reload", serde_json::json!({}))
            .await?;
        Ok(())
    }

    async fn snapshot(
        &self,
        tab_id: &str,
        _since: Option<&str>,
    ) -> Result<BrowserSnapshot, BrowserError> {
        let result = self
            .command(tab_id, "Accessibility.getFullAXTree", serde_json::json!({}))
            .await?;
        let empty = Vec::new();
        let ax_nodes = result
            .get("nodes")
            .and_then(|value| value.as_array())
            .unwrap_or(&empty);
        let mut nodes = Vec::new();
        for node in ax_nodes.iter().take(MAX_SNAPSHOT_NODES) {
            if let Some(browser) = ax_node_to_browser(node) {
                nodes.push(browser);
            }
        }
        let tabs = self.list_tabs().await.unwrap_or_default();
        let tab = tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .ok_or_else(|| BrowserError::UnknownTab(tab_id.to_string()))?;
        Ok(BrowserSnapshot {
            snapshot_id: uuid::Uuid::new_v4().to_string(),
            tab_id: tab_id.to_string(),
            url: tab.url.clone(),
            title: tab.title.clone(),
            nodes,
            removed_node_ids: Vec::new(),
        })
    }

    async fn query(
        &self,
        tab_id: &str,
        selector: &str,
        limit: usize,
    ) -> Result<Vec<BrowserNode>, BrowserError> {
        let expression = format!(
            r#"(() => {{ const els = Array.from(document.querySelectorAll({sel})); return els.slice(0, {lim}).map(el => {{ const r = el.getBoundingClientRect(); return {{ tag: el.tagName.toLowerCase(), text: (el.innerText || el.value || "").slice(0, 500), value: (el.value || null), href: (el.href || null), role: el.getAttribute("role"), visible: r.width > 0 && r.height > 0, bounds: {{ x: r.x, y: r.y, width: r.width, height: r.height }} }}; }}); }})()"#,
            sel = serde_json::to_string(selector).unwrap_or_default(),
            lim = limit.clamp(1, 100),
        );
        let value = self.evaluate(tab_id, &expression, false).await?;
        let empty = Vec::new();
        let items = value.as_array().unwrap_or(&empty);
        Ok(items
            .iter()
            .enumerate()
            .map(|(index, item)| dom_match_to_browser(index, item))
            .collect())
    }

    async fn click(&self, tab_id: &str, node_id: &str) -> Result<(), BrowserError> {
        let expression = node_script(node_id, "el.click()");
        self.evaluate(tab_id, &expression, false).await?;
        Ok(())
    }

    async fn type_text(&self, tab_id: &str, node_id: &str, text: &str) -> Result<(), BrowserError> {
        let expression = format!(
            r#"(() => {{ const el = {finder}; if (!el) return "missing"; el.focus(); document.execCommand("insertText", false, {text}); el.dispatchEvent(new Event("input", {{ bubbles: true }})); el.dispatchEvent(new Event("change", {{ bubbles: true }})); return "ok"; }})()"#,
            finder = node_finder(node_id),
            text = serde_json::to_string(text).unwrap_or_default(),
        );
        let value = self.evaluate(tab_id, &expression, false).await?;
        if value.as_str() == Some("missing") {
            return Err(BrowserError::UnknownNode(node_id.to_string()));
        }
        Ok(())
    }

    async fn set_value(
        &self,
        tab_id: &str,
        node_id: &str,
        value: &str,
    ) -> Result<(), BrowserError> {
        let expression = format!(
            r#"(() => {{ const el = {finder}; if (!el) return "missing"; el.focus(); const setter = Object.getOwnPropertyDescriptor(Object.getPrototypeOf(el), "value")?.set || Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value")?.set; if (setter && (el.tagName === "INPUT" || el.tagName === "TEXTAREA")) {{ setter.call(el, {val}); }} else {{ el.value = {val}; }} el.dispatchEvent(new Event("input", {{ bubbles: true }})); el.dispatchEvent(new Event("change", {{ bubbles: true }})); return "ok"; }})()"#,
            finder = node_finder(node_id),
            val = serde_json::to_string(value).unwrap_or_default(),
        );
        let result = self.evaluate(tab_id, &expression, false).await?;
        if result.as_str() == Some("missing") {
            return Err(BrowserError::UnknownNode(node_id.to_string()));
        }
        Ok(())
    }

    async fn select(
        &self,
        tab_id: &str,
        node_id: &str,
        values: &[String],
    ) -> Result<(), BrowserError> {
        let expression = format!(
            r#"(() => {{ const el = {finder}; if (!el || el.tagName !== "SELECT") return "missing"; const wanted = new Set({vals}); for (const opt of el.options) opt.selected = wanted.has(opt.value); el.dispatchEvent(new Event("input", {{ bubbles: true }})); el.dispatchEvent(new Event("change", {{ bubbles: true }})); return "ok"; }})()"#,
            finder = node_finder(node_id),
            vals = serde_json::to_string(values).unwrap_or_default(),
        );
        let result = self.evaluate(tab_id, &expression, false).await?;
        if result.as_str() == Some("missing") {
            return Err(BrowserError::UnknownNode(node_id.to_string()));
        }
        Ok(())
    }

    async fn scroll(
        &self,
        tab_id: &str,
        node_id: Option<&str>,
        dx: i32,
        dy: i32,
    ) -> Result<(), BrowserError> {
        let expression = match node_id {
            Some(node) => format!(
                r#"(() => {{ const el = {finder}; if (!el) return "missing"; el.scrollBy({dx}, {dy}); return "ok"; }})()"#,
                finder = node_finder(node),
            ),
            None => format!(r#"(() => {{ window.scrollBy({dx}, {dy}); return "ok"; }})()"#),
        };
        let result = self.evaluate(tab_id, &expression, false).await?;
        if result.as_str() == Some("missing") {
            return Err(BrowserError::UnknownNode(
                node_id.unwrap_or_default().to_string(),
            ));
        }
        Ok(())
    }

    async fn get_text(&self, tab_id: &str, node_id: Option<&str>) -> Result<String, BrowserError> {
        let expression = match node_id {
            Some(node) => format!(
                r#"(() => {{ const el = {finder}; return el ? (el.innerText || "") : "§missing§"; }})()"#,
                finder = node_finder(node),
            ),
            None => r#"(() => document.body ? document.body.innerText : "")()"#.to_string(),
        };
        let value = self.evaluate(tab_id, &expression, false).await?;
        match value.as_str() {
            Some("§missing§") => Err(BrowserError::UnknownNode(
                node_id.unwrap_or_default().to_string(),
            )),
            Some(text) => Ok(truncate_text(text)),
            None => Ok(String::new()),
        }
    }

    async fn get_attribute(
        &self,
        tab_id: &str,
        node_id: &str,
        name: &str,
    ) -> Result<Option<String>, BrowserError> {
        if !is_safe_attribute(name) {
            return Err(BrowserError::ActionFailed(format!(
                "refusing to read attribute '{name}'"
            )));
        }
        let expression = format!(
            r#"(() => {{ const el = {finder}; if (!el) return "§missing§"; const v = el.getAttribute({attr}); return v === null ? null : String(v).slice(0, 2000); }})()"#,
            finder = node_finder(node_id),
            attr = serde_json::to_string(name).unwrap_or_default(),
        );
        let value = self.evaluate(tab_id, &expression, false).await?;
        if value.as_str() == Some("§missing§") {
            return Err(BrowserError::UnknownNode(node_id.to_string()));
        }
        Ok(value.as_str().map(str::to_string))
    }

    async fn wait_for(
        &self,
        tab_id: &str,
        selector: Option<&str>,
        text: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(), BrowserError> {
        let timeout_ms = timeout_ms.clamp(100, 60_000);
        let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
        loop {
            let expression = match (selector, text) {
                (Some(selector), _) => format!(
                    r#"(() => !!document.querySelector({sel}))()"#,
                    sel = serde_json::to_string(selector).unwrap_or_default(),
                ),
                (None, Some(text)) => format!(
                    r#"(() => document.body && document.body.innerText.includes({t}))()"#,
                    t = serde_json::to_string(text).unwrap_or_default(),
                ),
                (None, None) => r#"(() => document.readyState === "complete")()"#.to_string(),
            };
            let ready = self
                .evaluate(tab_id, &expression, false)
                .await?
                .as_bool()
                .unwrap_or(false);
            if ready {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(BrowserError::Timeout(format!(
                    "condition not met within {timeout_ms}ms"
                )));
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn screenshot(
        &self,
        tab_id: &str,
        artifacts: &Arc<dyn ArtifactStore>,
    ) -> Result<ImageArtifactRef, BrowserError> {
        let result = self
            .command(
                tab_id,
                "Page.captureScreenshot",
                serde_json::json!({"format": "png"}),
            )
            .await?;
        let data = result
            .get("data")
            .and_then(|value| value.as_str())
            .ok_or_else(|| {
                BrowserError::ActionFailed("CDP screenshot returned no data".to_string())
            })?;
        let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
            .map_err(|error| {
                BrowserError::ActionFailed(format!("invalid screenshot encoding: {error}"))
            })?;
        let artifact = artifacts
            .put_with_source(
                "image/png",
                bytes,
                artifact_core::ArtifactSource::ScreenCapture,
                true,
            )
            .await
            .map_err(|error| BrowserError::ActionFailed(error.to_string()))?;
        // Dimensions come from the layout viewport; the PNG itself is the
        // source of truth at render time.
        let viewport = self
            .evaluate(
                tab_id,
                "({width: window.innerWidth, height: window.innerHeight})",
                false,
            )
            .await
            .unwrap_or(serde_json::Value::Null);
        Ok(ImageArtifactRef::new(
            artifact,
            viewport
                .get("width")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32,
            viewport
                .get("height")
                .and_then(|value| value.as_u64())
                .unwrap_or(0) as u32,
        ))
    }

    async fn cookies_list(&self, tab_id: &str) -> Result<Vec<BrowserCookie>, BrowserError> {
        let tabs = self.list_tabs().await.unwrap_or_default();
        let url = tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.url.clone())
            .unwrap_or_default();
        let result = self
            .command(
                tab_id,
                "Network.getCookies",
                serde_json::json!({"urls": [url]}),
            )
            .await?;
        let empty = Vec::new();
        Ok(result
            .get("cookies")
            .and_then(|value| value.as_array())
            .unwrap_or(&empty)
            .iter()
            .filter_map(|cookie| {
                Some(BrowserCookie {
                    name: cookie.get("name")?.as_str()?.to_string(),
                    value: cookie
                        .get("value")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    domain: cookie
                        .get("domain")
                        .and_then(|value| value.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    path: cookie
                        .get("path")
                        .and_then(|value| value.as_str())
                        .unwrap_or("/")
                        .to_string(),
                    secure: cookie
                        .get("secure")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                    http_only: cookie
                        .get("httpOnly")
                        .and_then(|value| value.as_bool())
                        .unwrap_or(false),
                })
            })
            .collect())
    }

    async fn cookies_set(&self, tab_id: &str, cookie: BrowserCookie) -> Result<(), BrowserError> {
        let tabs = self.list_tabs().await.unwrap_or_default();
        let url = tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.url.clone())
            .unwrap_or_default();
        let result = self
            .command(
                tab_id,
                "Network.setCookie",
                serde_json::json!({
                    "name": cookie.name, "value": cookie.value,
                    "domain": cookie.domain, "path": cookie.path,
                    "secure": cookie.secure, "httpOnly": cookie.http_only,
                    "url": url,
                }),
            )
            .await?;
        if result.get("success").and_then(|value| value.as_bool()) == Some(false) {
            return Err(BrowserError::ActionFailed(
                "CDP refused to set the cookie".to_string(),
            ));
        }
        Ok(())
    }

    async fn cookies_delete(&self, tab_id: &str, name: &str) -> Result<(), BrowserError> {
        let tabs = self.list_tabs().await.unwrap_or_default();
        let url = tabs
            .iter()
            .find(|tab| tab.id == tab_id)
            .map(|tab| tab.url.clone())
            .unwrap_or_default();
        self.command(
            tab_id,
            "Network.deleteCookies",
            serde_json::json!({"name": name, "url": url}),
        )
        .await?;
        Ok(())
    }
}

fn serde_urlencoded(url: &str) -> String {
    format!(
        "url={}",
        url::form_urlencoded::byte_serialize(url.as_bytes()).collect::<String>()
    )
}

/// DOM node finder: `dom-<index>` ids from `browser.query` address a live
/// query result; `ax-<id>` ids address the accessibility snapshot.
fn node_finder(node_id: &str) -> String {
    if let Some(index) = node_id
        .strip_prefix("dom-")
        .and_then(|rest| rest.parse::<usize>().ok())
    {
        format!(r#"utsuwaLastQuery[{index}]"#,)
    } else {
        // AX ids have no live DOM handle: fall back to document order by
        // accessible name is unreliable, so report missing honestly.
        r#"null"#.to_string()
    }
}

fn node_script(node_id: &str, action: &str) -> String {
    format!(
        r#"(() => {{ const el = {finder}; if (!el) return "missing"; {action}; return "ok"; }})()"#,
        finder = node_finder(node_id),
    )
}

/// Only inert attribute reads are allowed (no `on*` handlers, no
/// `srcdoc`, no executable content).
fn is_safe_attribute(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !(lower.starts_with("on") || lower == "srcdoc" || lower.starts_with("data:text/html"))
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn backend_error(tool: &str, error: BrowserError) -> ToolError {
    match error {
        BrowserError::BackendUnavailable(detail) => ToolError::structured_with_details(
            tool,
            "backend_unavailable",
            detail.clone(),
            serde_json::json!({"detail": detail}),
        ),
        BrowserError::UnknownTab(tab_id) => ToolError::structured_with_details(
            tool,
            "invalid_target",
            format!("unknown tab '{tab_id}'"),
            serde_json::json!({"tab_id": tab_id, "next_tool": "browser.list_tabs"}),
        ),
        BrowserError::UnknownNode(node_id) => ToolError::structured_with_details(
            tool,
            "stale_element",
            format!("unknown node '{node_id}'"),
            serde_json::json!({"node_id": node_id, "next_tool": "browser.snapshot"}),
        ),
        BrowserError::ActionFailed(detail) => ToolError::structured_with_details(
            tool,
            "action_failed",
            detail.clone(),
            serde_json::json!({"detail": detail}),
        ),
        BrowserError::Unsupported(detail) => ToolError::structured_with_details(
            tool,
            "unsupported_operation",
            detail.clone(),
            serde_json::json!({"detail": detail}),
        ),
        BrowserError::Timeout(detail) => ToolError::structured_with_details(
            tool,
            "action_failed",
            detail.clone(),
            serde_json::json!({"detail": detail, "timeout": true}),
        ),
    }
}

fn require_ticket(
    tool: &str,
    ctx: &ToolContext,
    capability: Capability,
    resource: Resource,
) -> Result<(), ToolError> {
    if ctx.has_ticket(capability.clone(), resource.clone()) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no capability ticket authorizes this browser call",
            serde_json::json!({ "capability": format!("{capability:?}") }),
        ))
    }
}

fn url_resource(url: &str) -> Result<Resource, ToolError> {
    let parsed = url::Url::parse(url).map_err(|_| {
        ToolError::structured(
            "browser.open",
            "invalid_target",
            format!("invalid URL: '{url}'"),
        )
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::structured(
            "browser.open",
            "invalid_target",
            format!(
                "only http(s) navigation is supported, got '{}'",
                parsed.scheme()
            ),
        ));
    }
    Ok(Resource::Url {
        scheme: parsed.scheme().to_string(),
        host: parsed.host_str().unwrap_or_default().to_string(),
        port: parsed.port_or_known_default().unwrap_or(443),
    })
}

fn tab_arg(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
    args.get("tab_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'tab_id'"))
}

fn node_arg(args: &serde_json::Value, tool: &str) -> Result<String, ToolError> {
    args.get("node_id")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'node_id'"))
}

pub struct BrowserStatusTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserListTabsTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserOpenTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserNavigateTool {
    pub backend: Arc<dyn BrowserBackend>,
}

#[async_trait::async_trait]
impl Tool for BrowserStatusTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.status"),
            description: "Check whether a browser backend is connected (no tabs are listed)."
                .to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    async fn invoke(
        &self,
        _ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let status = self
            .backend
            .status()
            .await
            .map_err(|error| backend_error("browser.status", error))?;
        Ok(ToolOutput::json(status))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserListTabsTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.list_tabs"),
            description: "List open browser tabs (id, title, url).".to_string(),
            input_schema: serde_json::json!({"type": "object", "additionalProperties": false, "properties": {}}),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Application("browser".to_string()),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        _args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        require_ticket(
            "browser.list_tabs",
            &ctx,
            Capability::DesktopObserve,
            Resource::Application("browser".to_string()),
        )?;
        let tabs = self
            .backend
            .list_tabs()
            .await
            .map_err(|error| backend_error("browser.list_tabs", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "tabs": tabs })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserOpenTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.open"),
            description:
                "Open a URL in a new tab. The destination URL needs its own network authorization."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "url": {"type": "string"} }, "required": ["url"],
            }),
            effects: vec![ToolEffect::Network],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let url = args.get("url")?.as_str()?;
        Some(CapabilityRequirement {
            capability: Capability::NetworkConnect,
            resource: url_resource(url).ok()?,
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let url = args
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("browser.open", "missing string 'url'"))?;
        let resource = url_resource(url)?;
        require_ticket("browser.open", &ctx, Capability::NetworkConnect, resource)?;
        let tab = self
            .backend
            .open(url)
            .await
            .map_err(|error| backend_error("browser.open", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "tab": tab })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserNavigateTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.navigate"),
            description:
                "Navigate a tab to a URL. The destination URL needs its own network authorization."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "url": {"type": "string"} },
                "required": ["tab_id", "url"],
            }),
            effects: vec![ToolEffect::Network],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let url = args.get("url")?.as_str()?;
        Some(CapabilityRequirement {
            capability: Capability::NetworkConnect,
            resource: url_resource(url).ok()?,
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.navigate")?;
        let url = args
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("browser.navigate", "missing string 'url'"))?;
        let resource = url_resource(url)?;
        require_ticket(
            "browser.navigate",
            &ctx,
            Capability::NetworkConnect,
            resource,
        )?;
        self.backend
            .navigate(&tab_id, url)
            .await
            .map_err(|error| backend_error("browser.navigate", error))?;
        Ok(ToolOutput::json(
            serde_json::json!({ "ok": true, "tab_id": tab_id, "url": url }),
        ))
    }
}

macro_rules! simple_tab_action {
    ($name:ident, $id:literal, $desc:literal, $method:ident, $observe:expr) => {
        pub struct $name { pub backend: Arc<dyn BrowserBackend> }
        #[async_trait::async_trait]
        impl Tool for $name {
            fn metadata(&self) -> ToolMetadata {
                ToolMetadata {
                    id: capability_core::ToolId::new($id),
                    description: $desc.to_string(),
                    input_schema: serde_json::json!({
                        "type": "object", "additionalProperties": false,
                        "properties": { "tab_id": {"type": "string"} }, "required": ["tab_id"],
                    }),
                    effects: vec![if $observe { ToolEffect::ReadOnly } else { ToolEffect::DesktopControl }],
                }
            }
            fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
                Some(CapabilityRequirement {
                    capability: if $observe { Capability::DesktopObserve } else { Capability::DesktopControl },
                    resource: Resource::Window(tab_arg(args, $id).ok()?),
                })
            }
            async fn invoke(&self, ctx: ToolContext, args: serde_json::Value) -> Result<ToolOutput, ToolError> {
                let tab_id = tab_arg(&args, $id)?;
                let capability = if $observe { Capability::DesktopObserve } else { Capability::DesktopControl };
                require_ticket($id, &ctx, capability, Resource::Window(tab_id.clone()))?;
                self.backend.$method(&tab_id).await.map_err(|error| backend_error($id, error))?;
                Ok(ToolOutput::json(serde_json::json!({ "ok": true, "tab_id": tab_id })))
            }
        }
    };
}

simple_tab_action!(
    BrowserCloseTabTool,
    "browser.close_tab",
    "Close a tab.",
    close_tab,
    false
);
simple_tab_action!(
    BrowserBackTool,
    "browser.back",
    "Go back in a tab's history.",
    back,
    false
);
simple_tab_action!(
    BrowserForwardTool,
    "browser.forward",
    "Go forward in a tab's history.",
    forward,
    false
);
simple_tab_action!(
    BrowserReloadTool,
    "browser.reload",
    "Reload a tab.",
    reload,
    false
);

pub struct BrowserSnapshotTool {
    pub backend: Arc<dyn BrowserBackend>,
    pub snapshots: SnapshotCache,
}
pub struct BrowserQueryTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserClickTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserTypeTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserSetValueTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserSelectTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserScrollTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserGetTextTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserGetAttributeTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserWaitForTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserScreenshotTool {
    pub backend: Arc<dyn BrowserBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
}
pub struct BrowserCookiesListTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserCookiesSetTool {
    pub backend: Arc<dyn BrowserBackend>,
}
pub struct BrowserCookiesDeleteTool {
    pub backend: Arc<dyn BrowserBackend>,
}

/// Bounded per-tab snapshot memory for incremental `since_snapshot_id`
/// updates. Keeps only the latest node list per tab.
/// Latest node list per tab: `(snapshot_id, nodes)`.
type TabSnapshot = (String, Vec<BrowserNode>);

#[derive(Clone, Default)]
pub struct SnapshotCache {
    inner: Arc<tokio::sync::Mutex<HashMap<String, TabSnapshot>>>,
}

impl SnapshotCache {
    pub fn new() -> Self {
        Self::default()
    }

    async fn diff(
        &self,
        tab_id: &str,
        snapshot_id: &str,
        current: Vec<BrowserNode>,
    ) -> (Vec<BrowserNode>, Vec<String>) {
        let mut inner = self.inner.lock().await;
        let changed = match inner.get(tab_id) {
            Some((previous_id, previous)) if previous_id == snapshot_id => {
                let old: HashMap<&str, &BrowserNode> = previous
                    .iter()
                    .map(|node| (node.id.as_str(), node))
                    .collect();
                let new_ids: std::collections::HashSet<&str> =
                    current.iter().map(|node| node.id.as_str()).collect();
                let changed = current
                    .iter()
                    .filter(|node| old.get(node.id.as_str()).is_none_or(|old| *old != *node))
                    .cloned()
                    .collect();
                let removed = previous
                    .iter()
                    .filter(|node| !new_ids.contains(node.id.as_str()))
                    .map(|node| node.id.clone())
                    .collect();
                inner.insert(
                    tab_id.to_string(),
                    (uuid::Uuid::new_v4().to_string(), current),
                );
                // Return the fresh snapshot id through the changed list path below.
                let _ = snapshot_id;
                (changed, removed)
            }
            _ => {
                inner.insert(
                    tab_id.to_string(),
                    (uuid::Uuid::new_v4().to_string(), current.clone()),
                );
                (current, Vec::new())
            }
        };
        changed
    }

    async fn snapshot_id(&self, tab_id: &str) -> Option<String> {
        self.inner
            .lock()
            .await
            .get(tab_id)
            .map(|(id, _)| id.clone())
    }
}

#[async_trait::async_trait]
impl Tool for BrowserSnapshotTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.snapshot"),
            description: "Compact accessibility snapshot of a tab (nodes with role/name/text/bounds). Pass since_snapshot_id for changed nodes only.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "since_snapshot_id": {"type": "string"} },
                "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.snapshot").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.snapshot")?;
        require_ticket(
            "browser.snapshot",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let since = args
            .get("since_snapshot_id")
            .and_then(|value| value.as_str());
        let mut snapshot = self
            .backend
            .snapshot(&tab_id, since)
            .await
            .map_err(|error| backend_error("browser.snapshot", error))?;
        if snapshot.nodes.len() > MAX_SNAPSHOT_NODES {
            snapshot.nodes.truncate(MAX_SNAPSHOT_NODES);
        }
        let (nodes, removed) = self
            .snapshots
            .diff(&tab_id, since.unwrap_or_default(), snapshot.nodes)
            .await;
        snapshot.nodes = nodes;
        snapshot.removed_node_ids = removed;
        snapshot.snapshot_id = self
            .snapshots
            .snapshot_id(&tab_id)
            .await
            .unwrap_or(snapshot.snapshot_id);
        Ok(ToolOutput::json(serde_json::json!({
            "snapshot_id": snapshot.snapshot_id,
            "tab_id": snapshot.tab_id,
            "url": snapshot.url,
            "title": snapshot.title,
            "nodes": snapshot.nodes,
            "removed_node_ids": snapshot.removed_node_ids,
        })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserQueryTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.query"),
            description: "Query DOM nodes by CSS selector (bounded results with text/bounds)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "tab_id": {"type": "string"},
                    "selector": {"type": "string"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100},
                },
                "required": ["tab_id", "selector"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.query").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.query")?;
        let selector = args
            .get("selector")
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| invalid("browser.query", "missing string 'selector'"))?;
        if selector.len() > 1_024 {
            return Err(invalid("browser.query", "selector is too long"));
        }
        let limit = args
            .get("limit")
            .and_then(|value| value.as_u64())
            .unwrap_or(20)
            .clamp(1, 100) as usize;
        require_ticket(
            "browser.query",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let nodes = self
            .backend
            .query(&tab_id, selector, limit)
            .await
            .map_err(|error| backend_error("browser.query", error))?;
        Ok(ToolOutput::json(
            serde_json::json!({ "tab_id": tab_id, "nodes": nodes }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserClickTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.click"),
            description: "Click a snapshot node by id. Prefer this over coordinates.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"} },
                "required": ["tab_id", "node_id"],
            }),
            effects: vec![ToolEffect::DesktopControl, ToolEffect::ExternalSideEffect],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.click").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.click")?;
        let node_id = node_arg(&args, "browser.click")?;
        require_ticket(
            "browser.click",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .click(&tab_id, &node_id)
            .await
            .map_err(|error| backend_error("browser.click", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserTypeTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.type"),
            description: "Type text into a node (appends). Fires input/change events.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"}, "text": {"type": "string"} },
                "required": ["tab_id", "node_id", "text"],
            }),
            effects: vec![ToolEffect::DesktopControl],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.type").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.type")?;
        let node_id = node_arg(&args, "browser.type")?;
        let text = args
            .get("text")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("browser.type", "missing string 'text'"))?;
        if text.chars().count() > MAX_TEXT_CHARS {
            return Err(invalid("browser.type", "text is too long"));
        }
        require_ticket(
            "browser.type",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .type_text(&tab_id, &node_id, text)
            .await
            .map_err(|error| backend_error("browser.type", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserSetValueTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.set_value"),
            description: "Replace a form control's value (fires input/change events). Submitting the form is a separate side-effecting action.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"}, "value": {"type": "string"} },
                "required": ["tab_id", "node_id", "value"],
            }),
            effects: vec![ToolEffect::DesktopControl],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.set_value").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.set_value")?;
        let node_id = node_arg(&args, "browser.set_value")?;
        let value = args
            .get("value")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid("browser.set_value", "missing string 'value'"))?;
        if value.chars().count() > MAX_TEXT_CHARS {
            return Err(invalid("browser.set_value", "value is too long"));
        }
        require_ticket(
            "browser.set_value",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .set_value(&tab_id, &node_id, value)
            .await
            .map_err(|error| backend_error("browser.set_value", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserSelectTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.select"),
            description: "Select option values in a <select> node.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"}, "values": {"type": "array", "items": {"type": "string"}} },
                "required": ["tab_id", "node_id", "values"],
            }),
            effects: vec![ToolEffect::DesktopControl],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.select").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.select")?;
        let node_id = node_arg(&args, "browser.select")?;
        let values = args
            .get("values")
            .and_then(|value| value.as_array())
            .ok_or_else(|| invalid("browser.select", "missing array 'values'"))?;
        if values.len() > 32 {
            return Err(invalid("browser.select", "too many values"));
        }
        let mut selected = Vec::with_capacity(values.len());
        for value in values {
            selected.push(
                value
                    .as_str()
                    .ok_or_else(|| invalid("browser.select", "'values' must be strings"))?
                    .to_string(),
            );
        }
        require_ticket(
            "browser.select",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .select(&tab_id, &node_id, &selected)
            .await
            .map_err(|error| backend_error("browser.select", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserScrollTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.scroll"),
            description: "Scroll a node (or the page when node_id is omitted) by a pixel delta."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "tab_id": {"type": "string"}, "node_id": {"type": "string"},
                    "delta_x": {"type": "integer"}, "delta_y": {"type": "integer"},
                },
                "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::DesktopControl],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.scroll").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.scroll")?;
        let node_id = args.get("node_id").and_then(|value| value.as_str());
        let dx = args
            .get("delta_x")
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            .clamp(-5_000, 5_000) as i32;
        let dy = args
            .get("delta_y")
            .and_then(|value| value.as_i64())
            .unwrap_or(0)
            .clamp(-5_000, 5_000) as i32;
        require_ticket(
            "browser.scroll",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .scroll(&tab_id, node_id, dx, dy)
            .await
            .map_err(|error| backend_error("browser.scroll", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserGetTextTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.get_text"),
            description: "Read rendered text of a node (or the page when node_id is omitted). Bounded output.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"} },
                "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.get_text").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.get_text")?;
        let node_id = args.get("node_id").and_then(|value| value.as_str());
        require_ticket(
            "browser.get_text",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let text = self
            .backend
            .get_text(&tab_id, node_id)
            .await
            .map_err(|error| backend_error("browser.get_text", error))?;
        Ok(ToolOutput::json(
            serde_json::json!({ "tab_id": tab_id, "text": truncate_text(&text) }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserGetAttributeTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.get_attribute"),
            description: "Read one inert DOM attribute (href, title, alt, …). Event handlers and executable content are refused.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "node_id": {"type": "string"}, "name": {"type": "string"} },
                "required": ["tab_id", "node_id", "name"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.get_attribute").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.get_attribute")?;
        let node_id = node_arg(&args, "browser.get_attribute")?;
        let name = args
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("browser.get_attribute", "missing string 'name'"))?;
        require_ticket(
            "browser.get_attribute",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let value = self
            .backend
            .get_attribute(&tab_id, &node_id, name)
            .await
            .map_err(|error| backend_error("browser.get_attribute", error))?;
        Ok(ToolOutput::json(
            serde_json::json!({ "tab_id": tab_id, "node_id": node_id, "name": name, "value": value }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserWaitForTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.wait_for"),
            description: "Wait for a selector, page text, or load completion (bounded timeout)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "tab_id": {"type": "string"}, "selector": {"type": "string"},
                    "text": {"type": "string"}, "timeout_ms": {"type": "integer", "minimum": 100, "maximum": 60000},
                },
                "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.wait_for").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.wait_for")?;
        let selector = args.get("selector").and_then(|value| value.as_str());
        let text = args.get("text").and_then(|value| value.as_str());
        let timeout_ms = args
            .get("timeout_ms")
            .and_then(|value| value.as_u64())
            .unwrap_or(5_000)
            .clamp(100, 60_000);
        require_ticket(
            "browser.wait_for",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .wait_for(&tab_id, selector, text, timeout_ms)
            .await
            .map_err(|error| backend_error("browser.wait_for", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserScreenshotTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.screenshot"),
            description: "Capture a tab screenshot as an artifact reference (explicit fallback; snapshots come first).".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"} }, "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.screenshot").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.screenshot")?;
        require_ticket(
            "browser.screenshot",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let image = self
            .backend
            .screenshot(&tab_id, &self.artifacts)
            .await
            .map_err(|error| backend_error("browser.screenshot", error))?;
        Ok(ToolOutput::multipart(
            serde_json::json!({
                "tab_id": tab_id, "artifact_id": image.artifact.id,
                "mime_type": image.artifact.mime_type, "width": image.width, "height": image.height,
            }),
            vec![ContentPart::Image(image)],
        ))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserCookiesListTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.cookies.list"),
            description:
                "List a tab's cookies. Values may contain session secrets: never log them."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"} }, "required": ["tab_id"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopObserve,
            resource: Resource::Window(tab_arg(args, "browser.cookies.list").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.cookies.list")?;
        require_ticket(
            "browser.cookies.list",
            &ctx,
            Capability::DesktopObserve,
            Resource::Window(tab_id.clone()),
        )?;
        let cookies = self
            .backend
            .cookies_list(&tab_id)
            .await
            .map_err(|error| backend_error("browser.cookies.list", error))?;
        Ok(ToolOutput::json(
            serde_json::json!({ "tab_id": tab_id, "cookies": cookies }),
        ))
    }
}

fn cookie_arg(args: &serde_json::Value, tool: &str) -> Result<BrowserCookie, ToolError> {
    let name = args
        .get("name")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid(tool, "missing string 'name'"))?;
    let value = args
        .get("value")
        .and_then(|value| value.as_str())
        .ok_or_else(|| invalid(tool, "missing string 'value'"))?;
    if name.len() > 256 || value.len() > 4_096 {
        return Err(invalid(tool, "cookie name/value too long"));
    }
    Ok(BrowserCookie {
        name: name.to_string(),
        value: value.to_string(),
        domain: args
            .get("domain")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        path: args
            .get("path")
            .and_then(|value| value.as_str())
            .unwrap_or("/")
            .to_string(),
        secure: args
            .get("secure")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        http_only: args
            .get("http_only")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    })
}

#[async_trait::async_trait]
impl Tool for BrowserCookiesSetTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.cookies.set"),
            description: "Set a cookie (high-risk: explicit authorization required).".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "tab_id": {"type": "string"}, "name": {"type": "string"}, "value": {"type": "string"},
                    "domain": {"type": "string"}, "path": {"type": "string"},
                    "secure": {"type": "boolean"}, "http_only": {"type": "boolean"},
                },
                "required": ["tab_id", "name", "value"],
            }),
            effects: vec![ToolEffect::Destructive, ToolEffect::ExternalSideEffect],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.cookies.set").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.cookies.set")?;
        let cookie = cookie_arg(&args, "browser.cookies.set")?;
        require_ticket(
            "browser.cookies.set",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .cookies_set(&tab_id, cookie)
            .await
            .map_err(|error| backend_error("browser.cookies.set", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

#[async_trait::async_trait]
impl Tool for BrowserCookiesDeleteTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("browser.cookies.delete"),
            description: "Delete a cookie (high-risk: explicit authorization required)."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "tab_id": {"type": "string"}, "name": {"type": "string"} },
                "required": ["tab_id", "name"],
            }),
            effects: vec![ToolEffect::Destructive, ToolEffect::ExternalSideEffect],
        }
    }
    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::DesktopControl,
            resource: Resource::Window(tab_arg(args, "browser.cookies.delete").ok()?),
        })
    }
    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let tab_id = tab_arg(&args, "browser.cookies.delete")?;
        let name = args
            .get("name")
            .and_then(|value| value.as_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| invalid("browser.cookies.delete", "missing string 'name'"))?;
        require_ticket(
            "browser.cookies.delete",
            &ctx,
            Capability::DesktopControl,
            Resource::Window(tab_id.clone()),
        )?;
        self.backend
            .cookies_delete(&tab_id, name)
            .await
            .map_err(|error| backend_error("browser.cookies.delete", error))?;
        Ok(ToolOutput::json(serde_json::json!({ "ok": true })))
    }
}

/// Static browser tool group.
pub struct BrowserToolPack {
    pub backend: Arc<dyn BrowserBackend>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub snapshots: SnapshotCache,
}

impl BrowserToolPack {
    pub fn stub() -> Self {
        Self {
            backend: Arc::new(StubBackend),
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
            snapshots: SnapshotCache::new(),
        }
    }

    pub fn with_services(
        backend: Arc<dyn BrowserBackend>,
        artifacts: Arc<dyn ArtifactStore>,
    ) -> Self {
        Self {
            backend,
            artifacts,
            snapshots: SnapshotCache::new(),
        }
    }
}

impl tool_sdk::ToolPack for BrowserToolPack {
    fn id(&self) -> &'static str {
        "browser"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(BrowserStatusTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserListTabsTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserOpenTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserCloseTabTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserNavigateTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserBackTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserForwardTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserReloadTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserSnapshotTool {
                backend: self.backend.clone(),
                snapshots: self.snapshots.clone(),
            }),
            Arc::new(BrowserQueryTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserClickTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserTypeTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserSetValueTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserSelectTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserScrollTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserGetTextTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserGetAttributeTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserWaitForTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserScreenshotTool {
                backend: self.backend.clone(),
                artifacts: self.artifacts.clone(),
            }),
            Arc::new(BrowserCookiesListTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserCookiesSetTool {
                backend: self.backend.clone(),
            }),
            Arc::new(BrowserCookiesDeleteTool {
                backend: self.backend.clone(),
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    struct FakeBrowser {
        tabs: Vec<BrowserTab>,
        clicked: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl FakeBrowser {
        fn new() -> Self {
            Self {
                tabs: vec![BrowserTab {
                    id: "tab-1".to_string(),
                    title: "Example".to_string(),
                    url: "https://example.com/".to_string(),
                }],
                clicked: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl BrowserBackend for FakeBrowser {
        async fn status(&self) -> Result<serde_json::Value, BrowserError> {
            Ok(serde_json::json!({"available": true}))
        }
        async fn list_tabs(&self) -> Result<Vec<BrowserTab>, BrowserError> {
            Ok(self.tabs.clone())
        }
        async fn open(&self, url: &str) -> Result<BrowserTab, BrowserError> {
            Ok(BrowserTab {
                id: "tab-2".to_string(),
                title: String::new(),
                url: url.to_string(),
            })
        }
        async fn close_tab(&self, _tab_id: &str) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn navigate(&self, _tab_id: &str, _url: &str) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn back(&self, _tab_id: &str) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn forward(&self, _tab_id: &str) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn reload(&self, _tab_id: &str) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn snapshot(
            &self,
            tab_id: &str,
            _since: Option<&str>,
        ) -> Result<BrowserSnapshot, BrowserError> {
            Ok(BrowserSnapshot {
                snapshot_id: "snap-1".to_string(),
                tab_id: tab_id.to_string(),
                url: "https://example.com/".to_string(),
                title: "Example".to_string(),
                nodes: vec![BrowserNode {
                    id: "ax-1".to_string(),
                    role: Some("textbox".to_string()),
                    tag: "input".to_string(),
                    name: Some("Password".to_string()),
                    text: None,
                    value: Some("secret-value".to_string()),
                    href: None,
                    bounds: None,
                    visible: true,
                    enabled: true,
                    focused: false,
                    sensitive: true,
                }],
                removed_node_ids: Vec::new(),
            })
        }
        async fn query(
            &self,
            _tab_id: &str,
            _selector: &str,
            _limit: usize,
        ) -> Result<Vec<BrowserNode>, BrowserError> {
            Ok(Vec::new())
        }
        async fn click(&self, tab_id: &str, node_id: &str) -> Result<(), BrowserError> {
            self.clicked
                .lock()
                .unwrap()
                .push((tab_id.to_string(), node_id.to_string()));
            Ok(())
        }
        async fn type_text(
            &self,
            _tab_id: &str,
            _node_id: &str,
            _text: &str,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn set_value(
            &self,
            _tab_id: &str,
            _node_id: &str,
            _value: &str,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn select(
            &self,
            _tab_id: &str,
            _node_id: &str,
            _values: &[String],
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn scroll(
            &self,
            _tab_id: &str,
            _node_id: Option<&str>,
            _dx: i32,
            _dy: i32,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn get_text(
            &self,
            _tab_id: &str,
            _node_id: Option<&str>,
        ) -> Result<String, BrowserError> {
            Ok("hello".to_string())
        }
        async fn get_attribute(
            &self,
            _tab_id: &str,
            _node_id: &str,
            _name: &str,
        ) -> Result<Option<String>, BrowserError> {
            Ok(None)
        }
        async fn wait_for(
            &self,
            _tab_id: &str,
            _selector: Option<&str>,
            _text: Option<&str>,
            _timeout_ms: u64,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn screenshot(
            &self,
            _tab_id: &str,
            _artifacts: &Arc<dyn ArtifactStore>,
        ) -> Result<ImageArtifactRef, BrowserError> {
            Err(BrowserError::Unsupported("fake has no pixels".to_string()))
        }
        async fn cookies_list(&self, _tab_id: &str) -> Result<Vec<BrowserCookie>, BrowserError> {
            Ok(Vec::new())
        }
        async fn cookies_set(
            &self,
            _tab_id: &str,
            _cookie: BrowserCookie,
        ) -> Result<(), BrowserError> {
            Ok(())
        }
        async fn cookies_delete(&self, _tab_id: &str, _name: &str) -> Result<(), BrowserError> {
            Ok(())
        }
    }

    fn ctx_for(capability: Capability, resource: Resource) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            capability,
            capability_core::ResourceScope::new(vec![resource]),
            ctx.invocation_id,
            Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    #[test]
    fn pack_registers_the_full_browser_surface() {
        let pack = BrowserToolPack::stub();
        let mut ids = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        for expected in [
            "browser.status",
            "browser.list_tabs",
            "browser.open",
            "browser.close_tab",
            "browser.navigate",
            "browser.back",
            "browser.forward",
            "browser.reload",
            "browser.snapshot",
            "browser.query",
            "browser.click",
            "browser.type",
            "browser.set_value",
            "browser.select",
            "browser.scroll",
            "browser.get_text",
            "browser.get_attribute",
            "browser.wait_for",
            "browser.screenshot",
            "browser.cookies.list",
            "browser.cookies.set",
            "browser.cookies.delete",
        ] {
            assert!(ids.contains(&expected.to_string()), "{ids:?}");
        }
    }

    #[test]
    fn navigation_requires_url_scoped_network_tickets() {
        let backend: Arc<dyn BrowserBackend> = Arc::new(FakeBrowser::new());
        let open = BrowserOpenTool { backend };
        let requirement = open
            .required_capability(&serde_json::json!({"url": "https://example.com/"}))
            .unwrap();
        assert_eq!(requirement.capability, Capability::NetworkConnect);
        assert_eq!(
            requirement.resource,
            Resource::Url {
                scheme: "https".to_string(),
                host: "example.com".to_string(),
                port: 443
            }
        );
        assert!(open
            .required_capability(&serde_json::json!({"url": "file:///etc/passwd"}))
            .is_none());
    }

    #[tokio::test]
    async fn interactions_require_control_tickets_and_record_clicks() {
        let backend: Arc<dyn BrowserBackend> = Arc::new(FakeBrowser::new());
        let click = BrowserClickTool { backend };
        let err = click
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("t"))),
                serde_json::json!({"tab_id": "tab-1", "node_id": "ax-1"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        click
            .invoke(
                ctx_for(
                    Capability::DesktopControl,
                    Resource::Window("tab-1".to_string()),
                ),
                serde_json::json!({"tab_id": "tab-1", "node_id": "ax-1"}),
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_tabs_report_invalid_target() {
        let backend: Arc<dyn BrowserBackend> = Arc::new(StubBackend);
        let list = BrowserListTabsTool { backend };
        let err = list
            .invoke(
                ctx_for(
                    Capability::DesktopObserve,
                    Resource::Application("browser".to_string()),
                ),
                serde_json::json!({}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("backend_unavailable"), "{err:?}");
    }

    #[test]
    fn snapshot_nodes_mask_secret_values() {
        let node = ax_node_to_browser(&serde_json::json!({
            "nodeId": "7",
            "role": {"value": "textbox"},
            "name": {"value": "Password"},
            "value": {"value": "hunter2"},
        }))
        .unwrap();
        assert!(node.sensitive);
        assert_eq!(node.value.as_deref(), Some(BROWSER_SENSITIVE_MASK));
        let plain = ax_node_to_browser(&serde_json::json!({
            "nodeId": "8",
            "role": {"value": "button"},
            "name": {"value": "Save"},
        }))
        .unwrap();
        assert!(!plain.sensitive);
    }

    #[test]
    fn executable_attributes_are_refused() {
        assert!(!is_safe_attribute("onclick"));
        assert!(!is_safe_attribute("srcdoc"));
        assert!(is_safe_attribute("href"));
        assert!(is_safe_attribute("alt"));
    }

    #[test]
    fn dom_node_ids_address_live_query_results() {
        assert_eq!(node_finder("dom-3"), "utsuwaLastQuery[3]");
        // AX ids have no live DOM handle: the script must fail honestly.
        assert_eq!(node_finder("ax-9"), "null");
    }
}
