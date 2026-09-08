//! MCP runtime tests against a fake stdio server (Python, written to a
//! temp dir at test time). Speaks just enough JSON-RPC + MCP to satisfy
//! the real SDK client: initialize, tools/list, tools/call.

use capability_core::{
    AgentId, Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
};
use mcp_runtime::{
    bridge_tool_id, McpManager, McpServerConfig, McpTransport, TrustLevel,
};
use std::collections::HashMap;
use std::path::PathBuf;
use tool_core::ToolContext;

const FAKE_SERVER: &str = include_str!("fake_server.py");

fn server_script(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("utsuwa-mcp-test-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fake_server.py");
    std::fs::write(&path, FAKE_SERVER).unwrap();
    path
}

fn config(script: &PathBuf, extra_env: HashMap<String, String>) -> McpServerConfig {
    McpServerConfig {
        id: capability_core::ServerId::new("fake"),
        transport: McpTransport::Stdio {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().to_string()],
            env_allowlist: vec!["PATH".to_string()],
            extra_env,
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
        invocation.clone(),
        std::time::Duration::from_secs(120),
    );
    let mut ctx = ToolContext::new(principal).with_ticket(ticket);
    ctx.invocation_id = invocation;
    ctx
}

#[test]
fn config_validation_and_tool_id_mapping() {
    let script = server_script("validate");
    let mut cfg = config(&script, HashMap::new());
    cfg.validate().unwrap();

    cfg.id = capability_core::ServerId::new("bad id!");
    assert!(cfg.validate().is_err());

    cfg.id = capability_core::ServerId::new("ok");
    match &mut cfg.transport {
        McpTransport::Stdio { command, .. } => *command = String::new(),
    }
    assert!(cfg.validate().is_err());

    assert_eq!(bridge_tool_id("srv", "my-tool_2"), "mcp.srv.my-tool_2");
    assert_eq!(bridge_tool_id("srv", "weird name/x"), "mcp.srv.weird_name_x");
}

#[tokio::test]
async fn discover_register_call_through_policy() {
    let script = server_script("happy");
    let manager = McpManager::new();
    manager.configure(config(&script, HashMap::new())).await.unwrap();

    let mut registry = tool_core::ToolRegistry::new();
    let added = manager.register_into("fake", &mut registry).await.unwrap();
    assert_eq!(added.len(), 3);
    assert!(added.contains(&"mcp.fake.echo".to_string()));

    let status = manager.status().await;
    assert_eq!(status.len(), 1);
    assert!(status[0].connected);
    assert_eq!(status[0].tools, 3);

    // The bridge always demands a capability decision — never pure.
    let echo = registry.resolve("mcp.fake.echo").unwrap();
    assert!(echo
        .required_capability(&serde_json::json!({}))
        .is_some());
    assert!(echo
        .metadata()
        .description
        .contains("trust: untrusted"));

    // Without a ticket the broker refuses.
    let plain = ToolContext::new(Principal::Agent(AgentId::new("nobody")));
    let err = echo
        .invoke(plain, serde_json::json!({"message": "hi"}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no capability ticket"), "{err}");

    // With a covering ticket the call reaches the server.
    let out = echo
        .invoke(
            mcp_ticket("fake", "echo"),
            serde_json::json!({"message": "hello-mcp"}),
        )
        .await
        .unwrap();
    assert_eq!(out.content["is_error"], false);
    assert!(out.content["text"].as_str().unwrap().contains("echo:hello-mcp"));

    // A ticket for another tool does not authorize this one.
    let err = echo
        .invoke(
            mcp_ticket("fake", "envcheck"),
            serde_json::json!({"message": "hi"}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not authorize"), "{err}");
}

#[tokio::test]
async fn tool_level_errors_stay_data_and_env_is_default_deny() {
    std::env::set_var("UTSUWA_MCP_PROBE_SECRET", "parent-secret");
    let script = server_script("env");
    let manager = McpManager::new();
    let mut extra = HashMap::new();
    extra.insert(
        "UTSUWA_MCP_PROBE_EXTRA".to_string(),
        "explicit-value".to_string(),
    );
    manager.configure(config(&script, extra)).await.unwrap();
    let mut registry = tool_core::ToolRegistry::new();
    manager.register_into("fake", &mut registry).await.unwrap();

    // Tool-level failure is model-visible data, not a broker error.
    let failer = registry.resolve("mcp.fake.failer").unwrap();
    let out = failer
        .invoke(mcp_ticket("fake", "failer"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out.content["is_error"], true);
    assert!(out.content["text"].as_str().unwrap().contains("boom"));

    // The child saw the explicit extra but not the ambient secret.
    let envcheck = registry.resolve("mcp.fake.envcheck").unwrap();
    let out = envcheck
        .invoke(mcp_ticket("fake", "envcheck"), serde_json::json!({}))
        .await
        .unwrap();
    let text = out.content["text"].as_str().unwrap();
    assert!(text.contains("secret=absent"), "{text}");
    assert!(text.contains("extra=explicit-value"), "{text}");
    std::env::remove_var("UTSUWA_MCP_PROBE_SECRET");
}

#[tokio::test]
async fn disable_unregisters_and_unknown_servers_fail() {
    let script = server_script("disable");
    let manager = McpManager::new();
    manager.configure(config(&script, HashMap::new())).await.unwrap();
    let mut registry = tool_core::ToolRegistry::new();
    manager.register_into("fake", &mut registry).await.unwrap();
    assert!(registry.resolve("mcp.fake.echo").is_ok());

    manager.set_enabled("fake", false, &mut registry).await.unwrap();
    assert!(registry.resolve("mcp.fake.echo").is_err());
    let status = manager.status().await;
    assert!(!status[0].connected);
    assert_eq!(status[0].tools, 0);

    assert!(manager
        .register_into("nope", &mut registry)
        .await
        .is_err());
    assert!(manager
        .remove_from("nope", &mut registry)
        .await
        .is_err());
}
