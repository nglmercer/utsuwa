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

fn client_for(limits: &HttpLimits) -> Result<reqwest::Client, ToolError> {
    // Redirects are NEVER automatic: every redirect target must independently
    // pass scheme validation, SSRF validation, and capability authorization
    // (see `FetchChain::run`). An automatic policy would let a public
    // URL bounce to 127.0.0.1 / metadata endpoints on the initial ticket.
    reqwest::Client::builder()
        .timeout(limits.timeout)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|error| ToolError::structured("http.request", "network_denied", error.to_string()))
}

/// Authorize one hop of a request chain: parse the URL, require a
/// `NetworkConnect` ticket for exactly that host/port, then run the SSRF
/// guard. The initial URL reports a plain `permission_required`; redirect
/// targets report a retriable `redirect_authorization_required` carrying
/// the new host/port so the agent can authorize and retry — authority is
/// never silently widened to a new origin.
async fn authorize_hop(
    tool: &str,
    ctx: &ToolContext,
    url: &url::Url,
    allow_private: bool,
    is_initial: bool,
) -> Result<Destination, ToolError> {
    let (_, destination) = parse_destination(url.as_str(), tool)?;
    let requirement = requirement(&destination);
    if !ctx.has_ticket(requirement.capability.clone(), requirement.resource.clone()) {
        if is_initial {
            return Err(ToolError::structured_with_details(
                tool,
                "permission_required",
                "no NetworkConnect ticket authorizes this destination",
                serde_json::json!({
                    "capability": "NetworkConnect",
                    "host": destination.host,
                    "port": destination.port,
                }),
            ));
        }
        return Err(ToolError::structured_with_details(
            tool,
            "redirect_authorization_required",
            "redirect target needs its own NetworkConnect ticket; the original ticket does not cover a new origin",
            serde_json::json!({
                "capability": "NetworkConnect",
                "redirect_url": url.as_str(),
                "host": destination.host,
                "port": destination.port,
            }),
        ));
    }
    check_destination(tool, &destination, allow_private).await?;
    Ok(destination)
}

/// Resolve a `Location` header against the current URL and validate its
/// shape (absolute or relative). Scheme/host rules are enforced by
/// [`parse_destination`]; SSRF and capability checks happen per hop in
/// [`authorize_hop`], never here, so this stays pure and unit-testable.
fn resolve_redirect_target(
    tool: &str,
    current: &url::Url,
    location: &str,
) -> Result<url::Url, ToolError> {
    if location.trim().is_empty() {
        return Err(ToolError::structured(
            tool,
            "action_failed",
            "redirect response is missing a usable Location header",
        ));
    }
    let target = current.join(location).map_err(|error| {
        ToolError::structured(
            tool,
            "invalid_target",
            format!("bad redirect target: {error}"),
        )
    })?;
    // Enforce http(s) before anything else touches the target.
    parse_destination(target.as_str(), tool)?;
    Ok(target)
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// One request chain: the initial URL plus redirect-following policy.
/// Bundled so the hop loop takes one argument (see `run`).
struct FetchChain<'a> {
    tool: &'a str,
    client: &'a reqwest::Client,
    ctx: &'a ToolContext,
    method: String,
    headers: Vec<(String, String)>,
    body: Option<String>,
    limits: &'a HttpLimits,
}

impl FetchChain<'_> {
    async fn run(mut self, initial: url::Url) -> Result<(reqwest::Response, url::Url), ToolError> {
        let mut current = initial;
        let mut visited: Vec<url::Url> = vec![current.clone()];
        let mut hops = 0usize;
        loop {
            let is_initial = hops == 0;
            authorize_hop(
                self.tool,
                self.ctx,
                &current,
                self.limits.allow_private_targets,
                is_initial,
            )
            .await?;
            let mut request = self.client.request(
                self.method
                    .parse()
                    .map_err(|_| invalid(self.tool, "invalid HTTP method"))?,
                current.clone(),
            );
            for (name, value) in &self.headers {
                request = request.header(name.as_str(), value.as_str());
            }
            if let Some(body) = &self.body {
                request = request.body(body.clone());
            }
            let response = request.send().await.map_err(|error| {
                ToolError::structured(
                    self.tool,
                    "network_denied",
                    format!("request failed: {error}"),
                )
            })?;
            let status = response.status();
            let location = if is_redirect(status) {
                response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string)
            } else {
                None
            };
            let Some(location) = location else {
                return Ok((response, current));
            };
            hops += 1;
            if hops > self.limits.max_redirects {
                return Err(ToolError::structured(
                    self.tool,
                    "action_failed",
                    format!(
                        "too many redirects (limit {}); possible redirect loop",
                        self.limits.max_redirects
                    ),
                ));
            }
            let target = resolve_redirect_target(self.tool, &current, &location)?;
            if visited.contains(&target) {
                return Err(ToolError::structured(
                    self.tool,
                    "action_failed",
                    format!("redirect loop detected at {}", target.as_str()),
                ));
            }
            // 301/302/303 convert non-GET/HEAD hops to GET (matching browser
            // and reqwest semantics); 307/308 preserve method and body.
            if matches!(status.as_u16(), 303)
                || (matches!(status.as_u16(), 301 | 302)
                    && self.method != "GET"
                    && self.method != "HEAD")
            {
                self.method = "GET".to_string();
                self.body = None;
            }
            if target.origin() != current.origin() {
                self.headers.retain(|(name, _)| {
                    !matches!(
                        name.to_ascii_lowercase().as_str(),
                        "authorization" | "proxy-authorization" | "cookie"
                    )
                });
            }
            visited.push(target.clone());
            current = target;
        }
    }
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
        let (parsed, _) = parse_destination(url, self.tool_id)?;

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
        let mut headers: Vec<(String, String)> = Vec::new();
        if let Some(object) = args.get("headers").and_then(|value| value.as_object()) {
            for (name, value) in object {
                let value = value
                    .as_str()
                    .ok_or_else(|| invalid(self.tool_id, "header values must be strings"))?;
                headers.push((name.clone(), value.to_string()));
            }
        }
        let body = if self.tool_id == "http.request" {
            match args.get("body").and_then(|value| value.as_str()) {
                Some(body) => {
                    if body.len() > self.limits.max_response_bytes {
                        return Err(invalid(self.tool_id, "request body exceeds the size limit"));
                    }
                    Some(body)
                }
                None => None,
            }
        } else {
            None
        };
        // Every hop — initial URL and each redirect target — passes
        // capability and SSRF validation inside the chain runner.
        let (response, final_url) = FetchChain {
            tool: self.tool_id,
            client: &client,
            ctx: &ctx,
            method: method.to_string(),
            // Credentials must never leak across origins on redirect.
            headers,
            body: body.map(str::to_string),
            limits: &self.limits,
        }
        .run(parsed)
        .await?;
        let final_url = final_url.as_str().to_string();
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
                "url": final_url,
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
            let mime = mime_guess(&final_url);
            let artifact: ArtifactRef = store
                .put_with_source(&mime, bytes, ArtifactSource::Download, false)
                .await
                .map_err(|error| {
                    ToolError::structured(self.tool_id, "action_failed", error.to_string())
                })?;
            let metadata = serde_json::json!({
                "artifact_id": artifact.id,
                "filename": filename_guess(&final_url),
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
                "url": final_url,
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
    use capability_core::{AgentId, Principal, ResourceScope};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

    // --- Redirect SSRF regression tests ---------------------------------
    //
    // A hand-rolled HTTP server (no extra dev-dependencies): routes by
    // path, parses the request line plus headers, and answers canned
    // redirects. Server B stands in for "another origin" (different port
    // means a different NetworkConnect ticket).

    fn ticket_ctx(host: &str, port: u16) -> ToolContext {
        ctx_with_tickets(&[(host, port)])
    }

    fn ctx_with_tickets(hosts: &[(&str, u16)]) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let mut ctx = ctx;
        for (host, port) in hosts {
            let ticket = capability_core::CapabilityTicket::mint(
                ctx.principal.clone(),
                Capability::NetworkConnect,
                ResourceScope::new(vec![Resource::HostPort {
                    host: host.to_string(),
                    port: *port,
                }]),
                ctx.invocation_id,
                Duration::from_secs(120),
            );
            ctx = ctx.with_ticket(ticket);
        }
        ctx
    }

    fn get_tool_for_tests(max_redirects: usize) -> HttpRequestTool {
        HttpRequestTool {
            limits: HttpLimits {
                allow_private_targets: true,
                max_redirects,
                ..HttpLimits::default()
            },
            method: "GET",
            tool_id: "http.get",
            artifacts: None,
            download: false,
        }
    }

    fn request_tool_for_tests() -> HttpRequestTool {
        HttpRequestTool {
            limits: HttpLimits {
                allow_private_targets: true,
                ..HttpLimits::default()
            },
            method: "GET",
            tool_id: "http.request",
            artifacts: None,
            download: false,
        }
    }

    /// Serve one connection: parse `METHOD path` plus headers, route, reply.
    async fn serve_one(
        mut stream: tokio::net::TcpStream,
        own: std::net::SocketAddr,
        other_port: u16,
    ) {
        let mut raw = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > 64 * 1024 || raw.windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(_) => return,
            }
        }
        let head = String::from_utf8_lossy(&raw).into_owned();
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("GET").to_string();
        let path = parts.next().unwrap_or("/").to_string();
        let mut headers = std::collections::HashMap::new();
        for line in lines {
            if line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
        let (status, location, body) = match path.as_str() {
            "/ok" => (200, None, "hello".to_string()),
            "/same" => (302, Some("/ok".to_string()), String::new()),
            "/rel" => (302, Some("ok".to_string()), String::new()),
            "/other" => (
                302,
                Some(format!("http://127.0.0.1:{other_port}/ok")),
                String::new(),
            ),
            "/private" => (302, Some("http://10.0.0.1/".to_string()), String::new()),
            "/loop" => (302, Some("/loop".to_string()), String::new()),
            "/chain" => (302, Some("/chain2".to_string()), String::new()),
            "/chain2" => (302, Some("/ok".to_string()), String::new()),
            "/echo-method" => (200, None, method),
            "/preserve" => (307, Some("/echo-method".to_string()), String::new()),
            "/convert" => (302, Some("/echo-method".to_string()), String::new()),
            "/leak-same" => (302, Some("/show".to_string()), String::new()),
            "/leak-other" => (
                302,
                Some(format!("http://127.0.0.1:{other_port}/show")),
                String::new(),
            ),
            "/show" => (
                200,
                None,
                headers.get("authorization").cloned().unwrap_or_default(),
            ),
            _ => (404, None, "not found".to_string()),
        };
        let _ = own;
        let mut response = format!(
            "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nConnection: close\r\n",
            if status == 200 { "OK" } else { "Redirect" },
            body.len()
        );
        if let Some(location) = location {
            response.push_str(&format!("Location: {location}\r\n"));
        }
        response.push_str("\r\n");
        response.push_str(&body);
        let _ = stream.write_all(response.as_bytes()).await;
    }

    async fn spawn_server(other_port: u16) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                serve_one(stream, addr, other_port).await;
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn same_host_redirect_is_followed() {
        let (addr, handle) = spawn_server(0).await;
        let tool = get_tool_for_tests(5);
        let out = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/same", addr.port())}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "hello");
        assert!(out.content["url"].as_str().unwrap().ends_with("/ok"));
        handle.abort();
    }

    #[tokio::test]
    async fn relative_location_redirect_is_followed() {
        let (addr, handle) = spawn_server(0).await;
        let tool = get_tool_for_tests(5);
        let out = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/rel", addr.port())}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "hello");
        handle.abort();
    }

    #[tokio::test]
    async fn cross_origin_redirect_without_second_ticket_is_retriable() {
        let (addr_b, handle_b) = spawn_server(0).await;
        let (addr_a, handle_a) = spawn_server(addr_b.port()).await;
        let tool = get_tool_for_tests(5);
        let err = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr_a.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/other", addr_a.port())}),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err.code(),
            Some("redirect_authorization_required"),
            "{err:?}"
        );
        let details = err.model_message();
        assert!(details.contains(&addr_b.port().to_string()), "{details}");
        handle_a.abort();
        handle_b.abort();
    }

    #[tokio::test]
    async fn cross_origin_redirect_with_second_ticket_is_followed() {
        let (addr_b, handle_b) = spawn_server(0).await;
        let (addr_a, handle_a) = spawn_server(addr_b.port()).await;
        let tool = get_tool_for_tests(5);
        let out = tool
            .invoke(
                ctx_with_tickets(&[("127.0.0.1", addr_a.port()), ("127.0.0.1", addr_b.port())]),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/other", addr_a.port())}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "hello");
        handle_a.abort();
        handle_b.abort();
    }

    #[tokio::test]
    async fn redirect_cannot_reach_private_target_without_ssrf_opt_in() {
        // Direct hop-level proof: even WITH a ticket, a redirect to a
        // private/metadata address fails the SSRF guard when the host has
        // not opted in. The invoke loop runs this same check per hop.
        for target in [
            "http://10.0.0.1/",
            "http://192.168.0.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://127.0.0.1/",
        ] {
            let url = url::Url::parse(target).unwrap();
            // Ticket-first ordering still ends at the SSRF guard: with a
            // matching ticket attached, only the guard can stop the hop.
            let host = url.host_str().unwrap().to_string();
            let port = url.port_or_known_default().unwrap_or(80);
            let err = authorize_hop("http.get", &ticket_ctx(&host, port), &url, false, false)
                .await
                .unwrap_err();
            assert_eq!(err.code(), Some("network_denied"), "{target}: {err:?}");
        }
    }

    #[tokio::test]
    async fn live_redirect_to_private_target_is_denied() {
        // End-to-end: the loop really does re-check the target. The other
        // port here stands in for an unreachable private address only in
        // shape — the SSRF decision itself is proven above; this proves
        // the loop routes through it by denying a hop with no ticket.
        let (addr, handle) = spawn_server(0).await;
        let tool = get_tool_for_tests(5);
        let err = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/private", addr.port())}),
            )
            .await
            .unwrap_err();
        // allow_private_targets=true for the loopback harness, so the
        // private target passes SSRF here — but the new origin still has
        // no ticket and must not be reached.
        assert_eq!(
            err.code(),
            Some("redirect_authorization_required"),
            "{err:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn redirect_loop_is_detected() {
        let (addr, handle) = spawn_server(0).await;
        let tool = get_tool_for_tests(5);
        let err = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/loop", addr.port())}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("action_failed"), "{err:?}");
        assert!(err.model_message().contains("loop"), "{err:?}");
        handle.abort();
    }

    #[tokio::test]
    async fn redirect_limit_is_enforced() {
        let (addr, handle) = spawn_server(0).await;
        let tool = get_tool_for_tests(1);
        let err = tool
            .invoke(
                ticket_ctx("127.0.0.1", addr.port()),
                serde_json::json!({"url": format!("http://127.0.0.1:{}/chain", addr.port())}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("action_failed"), "{err:?}");
        assert!(
            err.model_message().contains("too many redirects"),
            "{err:?}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn redirect_preserves_method_on_307_and_converts_on_302() {
        let (addr, handle) = spawn_server(0).await;
        let tool = request_tool_for_tests();
        let ctx = || ticket_ctx("127.0.0.1", addr.port());
        let base = format!("http://127.0.0.1:{}", addr.port());
        let out = tool
            .invoke(
                ctx(),
                serde_json::json!({"method": "POST", "url": format!("{base}/preserve"), "body": "x"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "POST");
        let out = tool
            .invoke(
                ctx(),
                serde_json::json!({"method": "POST", "url": format!("{base}/convert"), "body": "x"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "GET");
        handle.abort();
    }

    #[tokio::test]
    async fn credentials_are_stripped_on_cross_origin_redirect() {
        let (addr_b, handle_b) = spawn_server(0).await;
        let (addr_a, handle_a) = spawn_server(addr_b.port()).await;
        let tool = get_tool_for_tests(5);
        let ctx =
            || ctx_with_tickets(&[("127.0.0.1", addr_a.port()), ("127.0.0.1", addr_b.port())]);
        let base_a = format!("http://127.0.0.1:{}", addr_a.port());
        // Same-origin redirect keeps the credential header.
        let out = tool
            .invoke(
                ctx(),
                serde_json::json!({
                    "url": format!("{base_a}/leak-same"),
                    "headers": {"authorization": "secret"},
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "secret");
        // Cross-origin redirect strips it before the second hop.
        let out = tool
            .invoke(
                ctx(),
                serde_json::json!({
                    "url": format!("{base_a}/leak-other"),
                    "headers": {"authorization": "secret"},
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["body"], "");
        handle_a.abort();
        handle_b.abort();
    }

    #[test]
    fn redirect_target_resolution_rejects_bad_schemes() {
        let current = url::Url::parse("https://public.example/start").unwrap();
        // Relative and same-origin targets resolve.
        assert_eq!(
            resolve_redirect_target("http.get", &current, "/next")
                .unwrap()
                .as_str(),
            "https://public.example/next"
        );
        // HTTPS -> HTTP downgrade resolves here; the per-hop ticket + SSRF
        // check still gates it before any socket opens.
        let downgrade =
            resolve_redirect_target("http.get", &current, "http://public.example/plain").unwrap();
        assert_eq!(downgrade.scheme(), "http");
        // Non-http(s) schemes never resolve.
        for bad in [
            "ftp://public.example/x",
            "file:///etc/passwd",
            "gopher://x/",
        ] {
            let err = resolve_redirect_target("http.get", &current, bad).unwrap_err();
            assert_eq!(err.code(), Some("invalid_target"), "{bad}: {err:?}");
        }
        assert!(
            resolve_redirect_target("http.get", &current, "   ")
                .unwrap_err()
                .code()
                == Some("action_failed")
        );
    }

    #[tokio::test]
    async fn hop_authorization_rechecks_scheme_and_ticket() {
        // HTTPS target with a ticket for exactly that origin passes the
        // hop check (no connection is opened by the check itself).
        let url = url::Url::parse("https://public.example:443/x").unwrap();
        // allow_private=true keeps this DNS-free (literal-IP SSRF cases
        // are covered by `redirect_cannot_reach_private_target_*`).
        let dest = authorize_hop(
            "http.get",
            &ticket_ctx("public.example", 443),
            &url,
            true,
            false,
        )
        .await
        .unwrap();
        assert_eq!(dest.port, 443);
        // HTTP downgrade to port 80 needs its own ticket.
        let plain = url::Url::parse("http://public.example/x").unwrap();
        let err = authorize_hop(
            "http.get",
            &ticket_ctx("public.example", 443),
            &plain,
            true,
            false,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.code(),
            Some("redirect_authorization_required"),
            "{err:?}"
        );
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
