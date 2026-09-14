//! Capability-scoped HTTP tools (`http.*`).
//!
//! Every request requires a [`capability_core::Capability::NetworkConnect`]
//! ticket for the destination host/port, and every destination passes an
//! SSRF guard before any socket opens: loopback, private LAN ranges,
//! link-local (including cloud instance metadata), multicast, and
//! unspecified addresses are rejected unless the host explicitly allows
//! private targets. Responses and downloads are size-bounded; downloads
//! land in [`artifact_core`] first and reach disk only through a separate
//! authorized filesystem operation.

use artifact_core::{ArtifactRef, ArtifactSource, ArtifactStore, ContentPart};
use capability_core::{Capability, Resource};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// Bounds for every HTTP tool call.
#[derive(Debug, Clone)]
pub struct HttpLimits {
    pub timeout: Duration,
    pub max_response_bytes: usize,
    pub max_download_bytes: usize,
    pub max_redirects: usize,
    /// When true, private/loopback/link-local destinations are permitted
    /// (explicit host opt-in, e.g. local development servers).
    pub allow_private_targets: bool,
}

impl Default for HttpLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            max_response_bytes: 2 * 1024 * 1024,
            max_download_bytes: 20 * 1024 * 1024,
            max_redirects: 5,
            allow_private_targets: false,
        }
    }
}

/// A validated HTTP(S) destination about to be authorized and fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Destination {
    pub scheme: String,
    pub host: String,
    pub port: u16,
}

fn parse_destination(url: &str, tool: &str) -> Result<(url::Url, Destination), ToolError> {
    let parsed = url::Url::parse(url).map_err(|error| {
        ToolError::structured(tool, "invalid_target", format!("invalid URL: {error}"))
    })?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::structured(
            tool,
            "invalid_target",
            format!("only http(s) URLs are supported, got '{}'", parsed.scheme()),
        ));
    }
    let Some(host) = parsed.host_str().map(str::to_string) else {
        return Err(ToolError::structured(
            tool,
            "invalid_target",
            "URL has no host",
        ));
    };
    let port = parsed.port_or_known_default().unwrap_or(443);
    let scheme = parsed.scheme().to_string();
    Ok((parsed, Destination { scheme, host, port }))
}

/// SSRF guard: reject non-public destinations unless explicitly allowed.
/// DNS is resolved here, before the request, so a hostname that resolves
/// to a private address is still blocked (TOCTOU between check and connect
/// is documented; the capability ticket remains the second barrier).
pub async fn check_destination(
    tool: &str,
    destination: &Destination,
    allow_private: bool,
) -> Result<(), ToolError> {
    if allow_private {
        return Ok(());
    }
    // Literal IPs are classified directly.
    if let Ok(ip) = destination.host.parse::<IpAddr>() {
        if is_public_ip(&ip) {
            return Ok(());
        }
        return Err(ssrf_denied(tool, &destination.host));
    }
    // Hostnames: every resolved address must be public.
    let addrs = tokio::net::lookup_host((destination.host.as_str(), destination.port))
        .await
        .map_err(|_| ssrf_denied(tool, &destination.host))?;
    let mut saw_addr = false;
    for addr in addrs {
        saw_addr = true;
        if !is_public_ip(&addr.ip()) {
            return Err(ssrf_denied(tool, &destination.host));
        }
    }
    if !saw_addr {
        return Err(ssrf_denied(tool, &destination.host));
    }
    Ok(())
}

fn ssrf_denied(tool: &str, host: &str) -> ToolError {
    ToolError::structured_with_details(
        tool,
        "network_denied",
        format!("destination '{host}' is not a public network target"),
        serde_json::json!({
            "host": host,
            "reason": "SSRF protection: loopback, private, link-local, and metadata addresses need explicit host opt-in",
        }),
    )
}

fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation())
        }
        IpAddr::V6(v6) => {
            !(v6.is_loopback() || v6.is_multicast() || v6.is_unspecified()
                // Unique-local (fc00::/7) and link-local (fe80::/10).
                || (v6.segments()[0] & 0xfe00) == 0xfc00
                || (v6.segments()[0] & 0xffc0) == 0xfe80)
        }
    }
}

fn requirement(destination: &Destination) -> CapabilityRequirement {
    CapabilityRequirement {
        capability: Capability::NetworkConnect,
        resource: Resource::HostPort {
            host: destination.host.clone(),
            port: destination.port,
        },
    }
}

fn require_network(
    tool: &str,
    ctx: &ToolContext,
    destination: &Destination,
) -> Result<(), ToolError> {
    let requirement = requirement(destination);
    if ctx.has_ticket(requirement.capability.clone(), requirement.resource.clone()) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no NetworkConnect ticket authorizes this destination",
            serde_json::json!({
                "capability": "NetworkConnect",
                "host": destination.host,
                "port": destination.port,
            }),
        ))
    }
}

fn client_for(limits: &HttpLimits) -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .timeout(limits.timeout)
        .redirect(reqwest::redirect::Policy::limited(limits.max_redirects))
        .build()
        .map_err(|error| ToolError::structured("http.request", "network_denied", error.to_string()))
}

/// Read at most `limit` bytes; anything larger is an explicit error, never
/// a truncated silent success.
async fn read_bounded(
    tool: &str,
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, ToolError> {
    if let Some(declared) = response.content_length() {
        if declared > limit as u64 {
            return Err(ToolError::structured_with_details(
                tool,
                "response_too_large",
                format!("declared {declared} bytes exceeds the {limit}-byte limit"),
                serde_json::json!({ "declared_bytes": declared, "limit_bytes": limit }),
            ));
        }
    }
    let mut out = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        ToolError::structured(
            tool,
            "network_denied",
            format!("response read failed: {error}"),
        )
    })? {
        if out.len().saturating_add(chunk.len()) > limit {
            return Err(ToolError::structured_with_details(
                tool,
                "response_too_large",
                format!("response exceeds the {limit}-byte limit"),
                serde_json::json!({ "limit_bytes": limit }),
            ));
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

fn error_status(tool: &str, status: reqwest::StatusCode, body_excerpt: &str) -> ToolError {
    ToolError::structured_with_details(
        tool,
        "provider_unsupported_media",
        format!("HTTP {status}"),
        serde_json::json!({ "status": status.as_u16(), "body_excerpt": body_excerpt }),
    )
}

struct HttpRequestTool {
    limits: HttpLimits,
    method: &'static str,
    tool_id: &'static str,
    artifacts: Option<Arc<dyn ArtifactStore>>,
    download: bool,
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

#[async_trait::async_trait]
impl Tool for HttpRequestTool {
    fn metadata(&self) -> ToolMetadata {
        let (description, properties, required) = if self.download {
            (
                "Download a URL to a content-addressed artifact (binary stays out of model context). Writing it to disk is a separate filesystem operation.",
                serde_json::json!({
                    "url": {"type": "string"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                    "max_bytes": {"type": "integer", "minimum": 1},
                }),
                vec!["url"],
            )
        } else if self.method == "HEAD" {
            (
                "HEAD a URL: status and headers only, no body.",
                serde_json::json!({
                    "url": {"type": "string"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                }),
                vec!["url"],
            )
        } else if self.method == "GET" {
            (
                "GET a URL with bounded response size. Large bodies are rejected, never silently truncated.",
                serde_json::json!({
                    "url": {"type": "string"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                    "max_bytes": {"type": "integer", "minimum": 1},
                }),
                vec!["url"],
            )
        } else {
            (
                "Send an HTTP request with an explicit method, optional headers, and an optional text body.",
                serde_json::json!({
                    "method": {"type": "string", "enum": ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE", "OPTIONS"]},
                    "url": {"type": "string"},
                    "headers": {"type": "object", "additionalProperties": {"type": "string"}},
                    "body": {"type": "string"},
                    "max_bytes": {"type": "integer", "minimum": 1},
                }),
                vec!["url"],
            )
        };
        ToolMetadata {
            id: capability_core::ToolId::new(self.tool_id),
            description: description.to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "additionalProperties": false,
                "properties": properties,
                "required": required,
            }),
            effects: vec![ToolEffect::Network],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let url = args.get("url")?.as_str()?;
        let (_, destination) = parse_destination(url, self.tool_id).ok()?;
        Some(requirement(&destination))
    }

    fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
        self.required_capability(args).into_iter().collect()
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let url = args
            .get("url")
            .and_then(|value| value.as_str())
            .ok_or_else(|| invalid(self.tool_id, "missing string 'url'"))?;
        let (parsed, destination) = parse_destination(url, self.tool_id)?;
        require_network(self.tool_id, &ctx, &destination)?;
        check_destination(
            self.tool_id,
            &destination,
            self.limits.allow_private_targets,
        )
        .await?;

        let max_bytes = args
            .get("max_bytes")
            .and_then(|value| value.as_u64())
            .map(|value| value as usize)
            .unwrap_or(if self.download {
                self.limits.max_download_bytes
            } else {
                self.limits.max_response_bytes
            });
        let cap = if self.download {
            max_bytes.min(self.limits.max_download_bytes)
        } else {
            max_bytes.min(self.limits.max_response_bytes)
        };

        let method = if self.tool_id == "http.request" {
            args.get("method")
                .and_then(|value| value.as_str())
                .unwrap_or("GET")
        } else {
            self.method
        };
        let client = client_for(&self.limits)?;
        let mut request = client.request(
            method
                .parse()
                .map_err(|_| invalid(self.tool_id, "invalid HTTP method"))?,
            parsed,
        );
        if let Some(headers) = args.get("headers").and_then(|value| value.as_object()) {
            for (name, value) in headers {
                let value = value
                    .as_str()
                    .ok_or_else(|| invalid(self.tool_id, "header values must be strings"))?;
                request = request.header(name.as_str(), value);
            }
        }
        if self.tool_id == "http.request" {
            if let Some(body) = args.get("body").and_then(|value| value.as_str()) {
                if body.len() > self.limits.max_response_bytes {
                    return Err(invalid(self.tool_id, "request body exceeds the size limit"));
                }
                request = request.body(body.to_string());
            }
        }
        let response = request.send().await.map_err(|error| {
            ToolError::structured(
                self.tool_id,
                "network_denied",
                format!("request failed: {error}"),
            )
        })?;
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string(),
                    serde_json::Value::String(value.to_str().unwrap_or("<non-utf8>").to_string()),
                )
            })
            .collect::<serde_json::Map<String, serde_json::Value>>();
        if !status.is_success() {
            let body = read_bounded(self.tool_id, response, 8 * 1024)
                .await
                .unwrap_or_default();
            return Err(error_status(
                self.tool_id,
                status,
                &String::from_utf8_lossy(&body),
            ));
        }
        if self.method == "HEAD" && !self.download {
            return Ok(ToolOutput::json(serde_json::json!({
                "url": url,
                "status": status.as_u16(),
                "headers": headers,
            })));
        }
        let bytes = read_bounded(self.tool_id, response, cap).await?;
        if self.download {
            let store = self.artifacts.clone().ok_or_else(|| {
                ToolError::structured(
                    self.tool_id,
                    "backend_unavailable",
                    "no artifact store configured",
                )
            })?;
            let mime = mime_guess(url);
            let artifact: ArtifactRef = store
                .put_with_source(&mime, bytes, ArtifactSource::Download, false)
                .await
                .map_err(|error| {
                    ToolError::structured(self.tool_id, "action_failed", error.to_string())
                })?;
            let metadata = serde_json::json!({
                "artifact_id": artifact.id,
                "filename": filename_guess(url),
                "mime_type": artifact.mime_type,
                "size_bytes": artifact.size_bytes,
                "status": status.as_u16(),
            });
            return Ok(ToolOutput::multipart(
                metadata,
                vec![ContentPart::Binary(artifact)],
            ));
        }
        match String::from_utf8(bytes) {
            Ok(text) => Ok(ToolOutput::json(serde_json::json!({
                "url": url,
                "status": status.as_u16(),
                "headers": headers,
                "body": text,
            }))),
            Err(_) => Err(ToolError::structured_with_details(
                self.tool_id,
                "unsupported_operation",
                "response body is not UTF-8 text; use http.download for binary content",
                serde_json::json!({ "status": status.as_u16() }),
            )),
        }
    }
}

fn mime_guess(url: &str) -> String {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    if path.ends_with(".png") {
        "image/png".to_string()
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg".to_string()
    } else if path.ends_with(".zip") {
        "application/zip".to_string()
    } else if path.ends_with(".json") {
        "application/json".to_string()
    } else if path.ends_with(".pdf") {
        "application/pdf".to_string()
    } else {
        "application/octet-stream".to_string()
    }
}

fn filename_guess(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path
        .rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())?;
    Some(last.to_string())
}

/// Static HTTP tool group. Downloads require an artifact store.
pub struct HttpToolPack {
    pub limits: HttpLimits,
    pub artifacts: Option<Arc<dyn ArtifactStore>>,
}

impl HttpToolPack {
    pub fn new() -> Self {
        Self {
            limits: HttpLimits::default(),
            artifacts: None,
        }
    }

    pub fn with_artifacts(store: Arc<dyn ArtifactStore>) -> Self {
        Self {
            limits: HttpLimits::default(),
            artifacts: Some(store),
        }
    }
}

impl Default for HttpToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl tool_sdk::ToolPack for HttpToolPack {
    fn id(&self) -> &'static str {
        "http"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(HttpRequestTool {
                limits: self.limits.clone(),
                method: "GET",
                tool_id: "http.get",
                artifacts: None,
                download: false,
            }),
            Arc::new(HttpRequestTool {
                limits: self.limits.clone(),
                method: "HEAD",
                tool_id: "http.head",
                artifacts: None,
                download: false,
            }),
            Arc::new(HttpRequestTool {
                limits: self.limits.clone(),
                method: "GET",
                tool_id: "http.request",
                artifacts: None,
                download: false,
            }),
            Arc::new(HttpRequestTool {
                limits: self.limits.clone(),
                method: "GET",
                tool_id: "http.download",
                artifacts: self.artifacts.clone(),
                download: true,
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    #[test]
    fn private_and_special_destinations_are_rejected() {
        for host in [
            "127.0.0.1",
            "10.0.0.5",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "::1",
        ] {
            assert!(!is_public_ip(&host.parse().unwrap()), "{host}");
        }
        assert!(is_public_ip(&"93.184.216.34".parse().unwrap()));
        assert!(is_public_ip(
            &"2606:2800:220:1:248:1893:25c8:1946".parse().unwrap()
        ));
    }

    #[test]
    fn non_http_schemes_are_rejected() {
        assert!(parse_destination("file:///etc/passwd", "http.get").is_err());
        assert!(parse_destination("ftp://example.com/x", "http.get").is_err());
        assert!(parse_destination("not a url", "http.get").is_err());
    }

    #[tokio::test]
    async fn literal_private_destination_fails_before_any_socket() {
        let destination = Destination {
            scheme: "http".to_string(),
            host: "127.0.0.1".to_string(),
            port: 80,
        };
        let err = check_destination("http.get", &destination, false)
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("network_denied"));
        // Explicit host opt-in permits it.
        check_destination("http.get", &destination, true)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn calls_without_network_ticket_are_denied() {
        let pack = HttpToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let get = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "http.get")
            .unwrap();
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let err = get
            .invoke(ctx, serde_json::json!({"url": "https://example.com/"}))
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
    }

    #[test]
    fn pack_registers_get_head_request_download() {
        let pack = HttpToolPack::new();
        let mut ids = pack
            .tools(&tool_sdk::ToolLoadContext::default())
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec!["http.download", "http.get", "http.head", "http.request"]
        );
    }
}
