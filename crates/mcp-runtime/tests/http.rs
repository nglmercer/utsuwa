//! Streamable HTTP transport tests against a dependency-free mock MCP
//! server (raw HTTP/1.1 over a tokio TcpListener, single-JSON answers).
//! No public internet involved; everything binds to 127.0.0.1 ephemeral.
use capability_core::{
    AgentId, Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
};
use mcp_runtime::{McpManager, McpServerConfig, McpTimeouts, McpTransport, TrustLevel};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tool_core::{ToolContext, ToolRegistry};

const PROTOCOL_VERSION: &str = "2025-11-25";
const BEARER: &str = "test-token-123";

struct MockMode {
    require_auth: bool,
    garbage: bool,
    hang: bool,
    tools: Vec<serde_json::Value>,
}

impl Default for MockMode {
    fn default() -> Self {
        Self {
            require_auth: false,
            garbage: false,
            hang: false,
            tools: vec![
                serde_json::json!({
                    "name": "echo",
                    "description": "Echo a message",
                    "inputSchema": {"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}
                }),
                serde_json::json!({
                    "name": "failer",
                    "description": "Always fails at the tool level",
                    "inputSchema": {"type": "object"}
                }),
            ],
        }
    }
}

struct MockServer {
    url: String,
    initialize_calls: Arc<AtomicUsize>,
}

async fn read_request(
    stream: &mut tokio::net::TcpStream,
) -> Option<(String, HashMap<String, String>, Vec<u8>)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    // Read until end of headers.
    let header_end = loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_subslice(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default().to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_lowercase(), v.trim().to_string());
        }
    }
    let content_len: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
        .min(1024 * 1024);
    let mut body = buf[header_end..].to_vec();
    while body.len() < content_len {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(content_len);
    Some((request_line, headers, body))
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

async fn write_response(
    stream: &mut tokio::net::TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).await;
    let _ = stream.write_all(body).await;
}

fn json_body(id: &serde_json::Value, result: serde_json::Value) -> Vec<u8> {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})
        .to_string()
        .into_bytes()
}

fn json_error(id: &serde_json::Value, code: i64, message: &str) -> Vec<u8> {
    serde_json::json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
        .to_string()
        .into_bytes()
}

async fn spawn_mock(mode: MockMode) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let mode = Arc::new(mode);
    let initialize_calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&initialize_calls);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                break;
            };
            let mode = Arc::clone(&mode);
            let counter = Arc::clone(&counter);
            tokio::spawn(async move {
                let Some((_line, headers, body)) = read_request(&mut stream).await else {
                    return;
                };
                if mode.hang {
                    // Never respond; the client's timeout must fire.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    return;
                }
                if mode.garbage {
                    write_response(&mut stream, "200 OK", "text/html", b"<html>not json</html>")
                        .await;
                    return;
                }
                if mode.require_auth
                    && headers.get("authorization").map(String::as_str)
                        != Some("Bearer test-token-123")
                {
                    write_response(&mut stream, "401 Unauthorized", "text/plain", b"nope").await;
                    return;
                }
                let Ok(msg) = serde_json::from_slice::<serde_json::Value>(&body) else {
                    write_response(&mut stream, "400 Bad Request", "text/plain", b"bad json").await;
                    return;
                };
                let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
                // Notifications (no id) get an empty 202.
                let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
                if id.is_null() {
                    write_response(&mut stream, "202 Accepted", "text/plain", b"").await;
                    return;
                }
                match method {
                    "initialize" => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        let body = json_body(
                            &id,
                            serde_json::json!({
                                "protocolVersion": PROTOCOL_VERSION,
                                "capabilities": {"tools": {}},
                                "serverInfo": {"name": "mock", "version": "1"}
                            }),
                        );
                        write_response(&mut stream, "200 OK", "application/json", &body).await;
                    }
                    "tools/list" => {
                        let body = json_body(&id, serde_json::json!({"tools": mode.tools}));
                        write_response(&mut stream, "200 OK", "application/json", &body).await;
                    }
                    "tools/call" => {
                        let name = msg
                            .pointer("/params/name")
                            .and_then(|n| n.as_str())
                            .unwrap_or("");
                        let args = msg
                            .pointer("/params/arguments")
                            .cloned()
                            .unwrap_or_default();
                        if name == "echo" {
                            let text = args.get("message").and_then(|m| m.as_str()).unwrap_or("");
                            let body = json_body(
                                &id,
                                serde_json::json!({"content": [{"type": "text", "text": format!("echo:{text}")}]}),
                            );
                            write_response(&mut stream, "200 OK", "application/json", &body).await;
                        } else if name == "failer" {
                            let body = json_body(
                                &id,
                                serde_json::json!({"content": [{"type": "text", "text": "boom"}], "isError": true}),
                            );
                            write_response(&mut stream, "200 OK", "application/json", &body).await;
                        } else {
                            let body = json_error(&id, -32602, "unknown tool");
                            write_response(&mut stream, "200 OK", "application/json", &body).await;
                        }
                    }
                    _ => {
                        let body = json_error(&id, -32601, "unknown method");
                        write_response(&mut stream, "200 OK", "application/json", &body).await;
                    }
                }
            });
        }
    });
    MockServer {
        url: format!("http://127.0.0.1:{port}/mcp"),
        initialize_calls,
    }
}

fn http_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        id: capability_core::ServerId::new("http"),
        name: Some("HTTP mock".to_string()),
        transport: McpTransport::Http {
            url: url.to_string(),
        },
        enabled: true,
        trust: TrustLevel::Untrusted,
    }
}

fn mcp_ticket(server: &str, tool: &str) -> ToolContext {
    let principal = Principal::Agent(AgentId::new("test-agent"));
    let invocation = InvocationId::fresh();
    let ticket = CapabilityTicket::mint(
        principal.clone(),
        Capability::McpInvoke,
        ResourceScope::new(vec![Resource::McpTool {
            server: server.to_string(),
            tool: tool.to_string(),
        }]),
        invocation,
        std::time::Duration::from_secs(120),
    );
    let mut ctx = ToolContext::new(principal).with_ticket(ticket);
    ctx.invocation_id = invocation;
    ctx
}

#[tokio::test]
async fn http_discover_register_call_through_policy() {
    let mock = spawn_mock(MockMode::default()).await;
    let manager = McpManager::new();
    manager.configure(http_config(&mock.url)).await.unwrap();

    let mut registry = ToolRegistry::new();
    let added = manager.register_into("http", &mut registry).await.unwrap();
    assert!(added.contains(&"mcp.http.echo".to_string()));
    assert!(added.contains(&"mcp.http.failer".to_string()));
    assert_eq!(mock.initialize_calls.load(Ordering::SeqCst), 1);

    // Tool runs through the same bridge/policy path as stdio.
    let echo = registry.resolve("mcp.http.echo").unwrap();
    let out = echo
        .invoke(
            mcp_ticket("http", "echo"),
            serde_json::json!({"message": "hi"}),
        )
        .await
        .unwrap();
    assert_eq!(out.content["is_error"], false);
    assert!(out.content["text"].as_str().unwrap().contains("echo:hi"));

    // Tool-level failure stays data, not a transport error.
    let failer = registry.resolve("mcp.http.failer").unwrap();
    let failed = failer
        .invoke(mcp_ticket("http", "failer"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(failed.content["is_error"], true);

    // Policy still denies without a ticket.
    let plain = ToolContext::new(Principal::Agent(AgentId::new("nobody")));
    let denied = echo
        .invoke(plain, serde_json::json!({"message": "hi"}))
        .await;
    assert!(denied.is_err());

    let status = manager.status().await;
    assert_eq!(status.len(), 1);
    assert_eq!(status[0].transport, "http");
    assert!(status[0].connected);
    assert_eq!(status[0].tools, 2);
    assert_eq!(status[0].last_error, None);
}

#[tokio::test]
async fn http_bearer_token_authenticates_and_never_leaks() {
    let mock = spawn_mock(MockMode {
        require_auth: true,
        ..Default::default()
    })
    .await;

    // With the token resolved from the (injected) secret store: connects.
    let secrets: HashMap<String, String> = HashMap::from([(
        mcp_runtime::bearer_secret_account("http"),
        BEARER.to_string(),
    )]);
    let manager = McpManager::with_options(
        Some(Arc::new(move |account: &str| secrets.get(account).cloned())),
        McpTimeouts::default(),
    );
    manager.configure(http_config(&mock.url)).await.unwrap();
    let mut registry = ToolRegistry::new();
    manager.register_into("http", &mut registry).await.unwrap();

    // Without a resolver: the 401 fails discovery, and the error text plus
    // status surface must not contain the token.
    let bare = McpManager::new();
    bare.configure(http_config(&mock.url)).await.unwrap();
    let mut registry2 = ToolRegistry::new();
    let error = bare
        .register_into("http", &mut registry2)
        .await
        .unwrap_err();
    assert!(!error.to_string().contains(BEARER), "{error}");
    let status = bare.status().await;
    let last = status[0].last_error.as_deref().unwrap();
    assert!(!last.contains(BEARER), "{last}");
    assert!(!status[0].connected);
}

#[tokio::test]
async fn http_connection_refused_is_a_clean_protocol_error() {
    // Bind-then-drop to get a port nothing listens on.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let manager = McpManager::new();
    manager
        .configure(http_config(&format!("http://127.0.0.1:{port}/mcp")))
        .await
        .unwrap();
    let mut registry = ToolRegistry::new();
    let error = manager
        .register_into("http", &mut registry)
        .await
        .unwrap_err();
    let message = error.to_string();
    assert!(
        message.contains("initialize failed"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn http_garbage_response_is_a_clean_protocol_error() {
    let mock = spawn_mock(MockMode {
        garbage: true,
        ..Default::default()
    })
    .await;
    let manager = McpManager::new();
    manager.configure(http_config(&mock.url)).await.unwrap();
    let mut registry = ToolRegistry::new();
    let error = manager
        .register_into("http", &mut registry)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("initialize failed"),
        "unexpected error: {error}"
    );
    assert!(mock.initialize_calls.load(Ordering::SeqCst) <= 1);
}

#[tokio::test]
async fn http_hang_hits_the_connect_timeout() {
    let mock = spawn_mock(MockMode {
        hang: true,
        ..Default::default()
    })
    .await;
    let manager = McpManager::with_options(
        None,
        McpTimeouts {
            connect: Duration::from_millis(300),
            rpc: Duration::from_millis(300),
        },
    );
    manager.configure(http_config(&mock.url)).await.unwrap();
    let mut registry = ToolRegistry::new();
    let started = std::time::Instant::now();
    let error = manager
        .register_into("http", &mut registry)
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "connect was not bounded"
    );
    assert!(
        matches!(error, mcp_runtime::McpError::Timeout { .. }),
        "unexpected error: {error}"
    );
}

#[tokio::test]
async fn sync_lifecycle_changed_restarts_and_unchanged_retains() {
    let mock_a = spawn_mock(MockMode::default()).await;
    let mock_b = spawn_mock(MockMode {
        tools: vec![serde_json::json!({
            "name": "other",
            "description": "Other tool",
            "inputSchema": {"type": "object"}
        })],
        ..Default::default()
    })
    .await;

    let manager = McpManager::new();
    manager.configure(http_config(&mock_a.url)).await.unwrap();
    let mut registry = ToolRegistry::new();
    manager.register_into("http", &mut registry).await.unwrap();
    assert!(registry.resolve("mcp.http.echo").is_ok());

    // Unchanged sync retains the live client (no second handshake).
    manager
        .sync_configs(vec![http_config(&mock_a.url)])
        .await
        .unwrap();
    let mut registry2 = ToolRegistry::new();
    manager.register_into("http", &mut registry2).await.unwrap();
    assert_eq!(mock_a.initialize_calls.load(Ordering::SeqCst), 1);

    // Changed config drops the client; the next registration reconnects
    // to the new endpoint and discovers its tools instead.
    manager
        .sync_configs(vec![http_config(&mock_b.url)])
        .await
        .unwrap();
    assert!(
        !manager.status().await[0].connected,
        "changed config must drop the client"
    );
    let mut registry3 = ToolRegistry::new();
    let added = manager.register_into("http", &mut registry3).await.unwrap();
    assert_eq!(added, vec!["mcp.http.other".to_string()]);
    assert_eq!(mock_b.initialize_calls.load(Ordering::SeqCst), 1);

    // Removal drops the server entirely.
    manager.sync_configs(vec![]).await.unwrap();
    assert!(manager.server_ids().await.is_empty());
    assert!(manager.status().await.is_empty());
}

#[tokio::test]
async fn drop_client_forces_reconnect_and_last_error_clears() {
    let mock = spawn_mock(MockMode::default()).await;
    let manager = McpManager::new();
    manager.configure(http_config(&mock.url)).await.unwrap();
    let mut registry = ToolRegistry::new();
    manager.register_into("http", &mut registry).await.unwrap();
    assert_eq!(mock.initialize_calls.load(Ordering::SeqCst), 1);

    manager.drop_client("http").await;
    assert!(!manager.status().await[0].connected);
    manager.register_into("http", &mut registry).await.unwrap();
    assert_eq!(mock.initialize_calls.load(Ordering::SeqCst), 2);

    // A failed registration records a bounded error; success clears it.
    manager
        .configure(McpServerConfig {
            id: capability_core::ServerId::new("http"),
            name: None,
            transport: McpTransport::Http {
                url: "http://127.0.0.1:1/mcp".to_string(),
            },
            enabled: true,
            trust: TrustLevel::Untrusted,
        })
        .await
        .unwrap();
    let mut registry2 = ToolRegistry::new();
    manager
        .register_into("http", &mut registry2)
        .await
        .unwrap_err();
    let status = manager.status().await;
    let last = status[0].last_error.as_deref().unwrap();
    assert!(last.len() <= 300, "error not bounded: {last}");

    manager.configure(http_config(&mock.url)).await.unwrap();
    manager.register_into("http", &mut registry2).await.unwrap();
    assert_eq!(manager.status().await[0].last_error, None);
}

#[test]
fn status_serializes_without_credentials() {
    // Shape check only; the IPC layer relies on these exact keys.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mock = spawn_mock(MockMode::default()).await;
        let manager = McpManager::new();
        manager.configure(http_config(&mock.url)).await.unwrap();
        let mut registry = ToolRegistry::new();
        manager.register_into("http", &mut registry).await.unwrap();
        let status = manager.status().await;
        let json = serde_json::to_value(&status).unwrap();
        assert_eq!(json[0]["id"], "http");
        assert_eq!(json[0]["name"], "HTTP mock");
        assert_eq!(json[0]["transport"], "http");
        assert_eq!(json[0]["connected"], true);
        assert_eq!(json[0]["tools"], 2);
        assert!(json[0].get("last_error").is_none());
        let text = serde_json::to_string(&json).unwrap();
        assert!(!text.to_lowercase().contains("token"));
        assert!(!text.to_lowercase().contains("bearer"));
    });
}
