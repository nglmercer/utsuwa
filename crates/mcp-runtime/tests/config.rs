//! Config-shape tests: the canonical `mcp.servers` JSON, plus
//! backward-compatible parsing of the legacy Rust shape and the flat
//! TypeScript settings shape. Serialization is always canonical.
use mcp_runtime::{McpServerConfig, McpTransport, TrustLevel};
use std::collections::HashMap;

fn canonical_stdio() -> serde_json::Value {
    serde_json::json!({
        "id": "files",
        "name": "Filesystem",
        "transport": {
            "type": "stdio",
            "command": "npx",
            "args": ["-y", "server-filesystem", "/tmp"],
            "env_allowlist": ["PATH"],
            "extra_env": {"DEBUG": "1"},
            "cwd": "/tmp"
        },
        "enabled": true,
        "trust": "Limited"
    })
}

fn canonical_http() -> serde_json::Value {
    serde_json::json!({
        "id": "remote",
        "transport": {"type": "http", "url": "https://example.com/mcp"},
        "enabled": true,
        "trust": "Untrusted"
    })
}

#[test]
fn canonical_stdio_round_trip() {
    let config: McpServerConfig = serde_json::from_value(canonical_stdio()).unwrap();
    assert_eq!(config.id.0, "files");
    assert_eq!(config.name.as_deref(), Some("Filesystem"));
    assert!(config.enabled);
    assert_eq!(config.trust, TrustLevel::Limited);
    match &config.transport {
        McpTransport::Stdio {
            command,
            args,
            env_allowlist,
            extra_env,
            cwd,
        } => {
            assert_eq!(command, "npx");
            assert_eq!(args.len(), 3);
            assert_eq!(env_allowlist, &vec!["PATH".to_string()]);
            assert_eq!(extra_env.get("DEBUG").map(String::as_str), Some("1"));
            assert_eq!(cwd.as_deref(), Some("/tmp"));
        }
        other => panic!("expected stdio, got {other:?}"),
    }
    config.validate().unwrap();
    // Serialization stays canonical (tagged transport, optional name kept).
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(value["transport"]["type"], "stdio");
    assert_eq!(value["name"], "Filesystem");
    let again: McpServerConfig = serde_json::from_value(value).unwrap();
    assert_eq!(again, config);
}

#[test]
fn canonical_http_round_trip() {
    let config: McpServerConfig = serde_json::from_value(canonical_http()).unwrap();
    assert_eq!(config.transport.kind(), "http");
    config.validate().unwrap();
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(value["transport"]["type"], "http");
    assert_eq!(value["transport"]["url"], "https://example.com/mcp");
    // No credential field exists on the wire shape.
    assert!(value["transport"].get("bearer").is_none());
    assert!(value["transport"].get("token").is_none());
    assert!(value["transport"].get("auth").is_none());
}

#[test]
fn legacy_externally_tagged_stdio_still_parses() {
    let legacy = serde_json::json!({
        "id": "files",
        "transport": {"Stdio": {
            "command": "npx",
            "args": [],
            "env_allowlist": [],
            "extra_env": {}
        }},
        "enabled": false,
        "trust": "Trusted"
    });
    let config: McpServerConfig = serde_json::from_value(legacy).unwrap();
    assert!(!config.enabled);
    assert_eq!(config.name, None);
    match &config.transport {
        McpTransport::Stdio { cwd, .. } => assert_eq!(cwd, &None),
        other => panic!("expected stdio, got {other:?}"),
    }
    config.validate().unwrap();
    // ...but it serializes forward to the canonical shape.
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(value["transport"]["type"], "stdio");
}

#[test]
fn flat_typescript_shapes_parse() {
    let ts_stdio = serde_json::json!({
        "id": "local",
        "transport": "stdio",
        "name": "Local",
        "command": "uvx",
        "args": ["mcp-server-time"],
        "env": {"TZ": "UTC"},
        "enabled": true
    });
    let config: McpServerConfig = serde_json::from_value(ts_stdio).unwrap();
    assert_eq!(config.name.as_deref(), Some("Local"));
    match &config.transport {
        McpTransport::Stdio {
            command,
            args,
            env_allowlist,
            extra_env,
            cwd,
        } => {
            assert_eq!(command, "uvx");
            assert_eq!(args, &vec!["mcp-server-time".to_string()]);
            assert!(env_allowlist.is_empty());
            assert_eq!(extra_env.get("TZ").map(String::as_str), Some("UTC"));
            assert_eq!(cwd, &None);
        }
        other => panic!("expected stdio, got {other:?}"),
    }
    config.validate().unwrap();

    // A persisted bearerToken is accepted-and-ignored: tokens must travel
    // through mcp.set_server_token, never through settings JSON.
    let ts_http = serde_json::json!({
        "id": "ha",
        "transport": "http",
        "url": "http://homeassistant.local:8123/mcp",
        "bearerToken": "supersecret",
        "enabled": true
    });
    let config: McpServerConfig = serde_json::from_value(ts_http).unwrap();
    match &config.transport {
        McpTransport::Http { url } => assert_eq!(url, "http://homeassistant.local:8123/mcp"),
        other => panic!("expected http, got {other:?}"),
    }
    config.validate().unwrap();
    let serialized = serde_json::to_string(&config).unwrap();
    assert!(
        !serialized.contains("supersecret"),
        "token must not survive into canonical JSON: {serialized}"
    );
}

#[test]
fn http_url_validation_rejects_bad_endpoints() {
    let bad = [
        "",
        "not a url",
        "ftp://example.com/mcp",
        "file:///tmp/x",
        "https://user:pass@example.com/mcp",
        "https://",
    ];
    for url in bad {
        let config = McpServerConfig {
            id: capability_core::ServerId::new("h"),
            name: None,
            transport: McpTransport::Http {
                url: url.to_string(),
            },
            enabled: true,
            trust: TrustLevel::Untrusted,
        };
        assert!(config.validate().is_err(), "url should fail: {url}");
    }
    for url in [
        "http://localhost:8000/mcp",
        "https://example.com/mcp/",
        "http://127.0.0.1:9000/",
    ] {
        let config = McpServerConfig {
            id: capability_core::ServerId::new("h"),
            name: None,
            transport: McpTransport::Http {
                url: url.to_string(),
            },
            enabled: true,
            trust: TrustLevel::Untrusted,
        };
        assert!(config.validate().is_ok(), "url should pass: {url}");
    }
}

#[test]
fn stdio_validation_covers_new_fields() {
    let mut config = McpServerConfig {
        id: capability_core::ServerId::new("s"),
        name: Some("x".repeat(200)),
        transport: McpTransport::Stdio {
            command: "true".to_string(),
            args: Vec::new(),
            env_allowlist: Vec::new(),
            extra_env: HashMap::new(),
            cwd: None,
        },
        enabled: true,
        trust: TrustLevel::Untrusted,
    };
    assert!(config.validate().is_err(), "overlong name should fail");
    config.name = None;
    config.validate().unwrap();

    match &mut config.transport {
        McpTransport::Stdio { cwd, .. } => *cwd = Some("has\0nul".to_string()),
        _ => unreachable!(),
    }
    assert!(config.validate().is_err(), "NUL cwd should fail");
}

#[test]
fn unknown_transport_shapes_fail_to_parse() {
    let bad = serde_json::json!({
        "id": "x",
        "transport": {"type": "websocket", "url": "ws://x"},
        "enabled": true
    });
    assert!(serde_json::from_value::<McpServerConfig>(bad).is_err());

    let missing = serde_json::json!({"id": "x", "enabled": true});
    assert!(serde_json::from_value::<McpServerConfig>(missing).is_err());

    let flat_unknown = serde_json::json!({"id": "x", "transport": "carrier-pigeon"});
    assert!(serde_json::from_value::<McpServerConfig>(flat_unknown).is_err());
}

#[test]
fn bearer_secret_accounts_are_stable() {
    assert_eq!(mcp_runtime::bearer_secret_account("ha"), "mcp.bearer.ha");
}
