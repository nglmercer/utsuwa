//! End-to-end WASM plugin tests with inline WAT guests (no toolchain).
//!
//! Guests are compiled from WAT text with the `wat` crate, written as
//! `plugin.wasm` next to a `plugin.toml`, then driven through the real
//! path: discover → load → enable → `register_enabled` → ticketed invoke.

use capability_core::{Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope};
use plugin_wasm::{bridge_tool_id, PluginRuntime};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tool_core::{ToolContext, ToolError, ToolRegistry};

const ALLOC_FN: &str = r#"
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (global.get $heap) (local.set $ptr)
    (global.get $heap) (local.get $len) (i32.add) (global.set $heap)
    (local.get $ptr))"#;

const ECHO_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 4096))
  (func (export "alloc") (param $len i32) (result i32)
    (local $ptr i32)
    (global.get $heap) (local.set $ptr)
    (global.get $heap) (local.get $len) (i32.add) (global.set $heap)
    (local.get $ptr))
  (func (export "invoke") (param $ptr i32) (param $len i32) (result i64)
    (local $out i32)
    (local.get $len) (call 0) (local.set $out)
    (memory.copy (local.get $out) (local.get $ptr) (local.get $len))
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $out)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))"#;

const LOOP_WAT: &str = r#"(module
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "invoke") (param i32 i32) (result i64)
    (loop $spin (br $spin))
    (i64.const 0)))"#;

const WASI_WAT: &str = r#"(module
  (import "wasi_snapshot_preview1" "fd_write"
    (func $fd_write (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (func (export "alloc") (param i32) (result i32) (i32.const 1024))
  (func (export "invoke") (param i32 i32) (result i64)
    (drop (call $fd_write (i32.const 0) (i32.const 0) (i32.const 0) (i32.const 0)))
    (i64.const 0)))"#;

fn test_dir(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("utsuwa-wasm-{name}-{}", std::process::id()))
}

fn write_plugin(root: &Path, id: &str, fs_toml: &str, wat_src: &str) {
    let dir = root.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let manifest = format!(
        "[plugin]\nid = \"{id}\"\nname = \"{id} test\"\nversion = \"0.1.0\"\napi = 1\n\n\
         [runtime]\ntype = \"wasm\"\n\n\
         [permissions.filesystem]\n{fs_toml}\n\n\
         [[tools]]\nname = \"run\"\ndescription = \"test tool\"\n"
    );
    std::fs::write(dir.join("plugin.toml"), manifest).unwrap();
    let bytes = wat::parse_str(wat_src).expect("test WAT must compile");
    assert!(bytes.starts_with(&[0x00, 0x61, 0x73, 0x6D]));
    std::fs::write(dir.join("plugin.wasm"), bytes).unwrap();
}

fn setup(root: &Path, id: &str, fs_toml: &str, wat_src: &str) -> Arc<PluginRuntime> {
    write_plugin(root, id, fs_toml, wat_src);
    let rt = Arc::new(PluginRuntime::new().unwrap());
    let ids = rt.discover_dir(root).unwrap();
    assert!(ids.contains(&id.to_string()), "discovered: {ids:?}");
    rt.load(id).unwrap();
    rt.enable(id).unwrap();
    rt
}

/// Agent ticket authorizing one plugin tool call for one invocation.
fn invoke_ticket(
    plugin: &str,
    tool: &str,
    principal: &Principal,
    invocation: &InvocationId,
) -> CapabilityTicket {
    CapabilityTicket::mint(
        principal.clone(),
        Capability::PluginInvoke,
        ResourceScope::new(vec![Resource::PluginTool {
            plugin: plugin.to_string(),
            tool: tool.to_string(),
        }]),
        invocation.clone(),
        Duration::from_secs(120),
    )
}

fn invoke_ctx(plugin: &str, tool: &str) -> ToolContext {
    let principal = Principal::User;
    let ctx = ToolContext::new(principal.clone());
    let ticket = invoke_ticket(plugin, tool, &principal, &ctx.invocation_id);
    ctx.with_ticket(ticket)
}

fn registered(rt: &Arc<PluginRuntime>, tool_id: &str) -> ToolRegistry {
    let mut reg = ToolRegistry::new();
    let added = rt.register_enabled(&mut reg).unwrap();
    assert!(added.contains(&tool_id.to_string()), "registered: {added:?}");
    reg
}

#[tokio::test]
async fn echo_roundtrip_through_bridge() {
    let root = test_dir("echo");
    let rt = setup(&root, "echo", "", ECHO_WAT);
    let tool_id = bridge_tool_id("echo", "run");
    let reg = registered(&rt, &tool_id);
    assert_eq!(rt.enabled_manifests().len(), 1);

    let args = serde_json::json!({"hello": "guest", "n": 42});
    let out = reg
        .invoke(&tool_id, invoke_ctx("echo", "run"), args.clone())
        .await
        .unwrap();
    assert_eq!(out.content, args);
    assert!(out.content.get("guest_log").is_none());

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn denied_without_ticket() {
    let root = test_dir("noticket");
    let rt = setup(&root, "noticket", "", ECHO_WAT);
    let tool_id = bridge_tool_id("noticket", "run");
    let reg = registered(&rt, &tool_id);

    let err = reg
        .invoke(&tool_id, ToolContext::new(Principal::User), serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied { .. }), "unexpected: {err}");

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn cross_plugin_ticket_rejected() {
    let root = test_dir("cross");
    let rt = setup(&root, "cross", "", ECHO_WAT);
    let tool_id = bridge_tool_id("cross", "run");
    let reg = registered(&rt, &tool_id);

    // Ticket minted for a *different* plugin tool must not authorize this one.
    let err = reg
        .invoke(&tool_id, invoke_ctx("other", "run"), serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::Denied { .. }), "unexpected: {err}");

    std::fs::remove_dir_all(&root).ok();
}

fn fs_writer_wat(path: &str, data: &str) -> String {
    format!(
        r#"(module
  (import "utsuwa" "host.fs.write" (func $write (param i32 i32 i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{path}")
  (data (i32.const 512) "{data}")
  {ALLOC_FN}
  (func (export "invoke") (param i32 i32) (result i64)
    (call $write (i32.const 0) (i32.const {plen}) (i32.const 512) (i32.const {dlen}))))"#,
        plen = path.len(),
        dlen = data.len()
    )
}

fn fs_reader_wat(path: &str) -> String {
    format!(
        r#"(module
  (import "utsuwa" "host.fs.read" (func $read (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{path}")
  {ALLOC_FN}
  (func (export "invoke") (param i32 i32) (result i64)
    (call $read (i32.const 0) (i32.const {len}))))"#,
        len = path.len()
    )
}

#[tokio::test]
async fn guest_file_read_inside_manifest_scope() {
    let root = test_dir("fsread");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("secret.txt");
    std::fs::write(&file, "guest-visible").unwrap();
    let scope = root.to_string_lossy().to_string();

    let rt = setup(
        &root,
        "fsread",
        &format!("read = [\"{scope}\"]\nwrite = []"),
        &fs_reader_wat(&file.to_string_lossy()),
    );
    let tool_id = bridge_tool_id("fsread", "run");
    let reg = registered(&rt, &tool_id);

    let out = reg
        .invoke(&tool_id, invoke_ctx("fsread", "run"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out.content["ok"], true, "envelope: {}", out.content);
    assert_eq!(out.content["content"], "guest-visible");

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn guest_file_read_outside_scope_denied_as_envelope() {
    let root = test_dir("fsdeny");
    std::fs::create_dir_all(&root).unwrap();
    let outside = root.join("outside.txt");
    std::fs::write(&outside, "must-stay-hidden").unwrap();

    // Manifest grants nothing: the derived plugin ticket cannot exist, so
    // the guest sees a denial envelope — never a trap, never the bytes.
    let rt = setup(&root, "fsdeny", "", &fs_reader_wat(&outside.to_string_lossy()));
    let tool_id = bridge_tool_id("fsdeny", "run");
    let reg = registered(&rt, &tool_id);

    let out = reg
        .invoke(&tool_id, invoke_ctx("fsdeny", "run"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out.content["ok"], false, "envelope: {}", out.content);
    let err = out.content["error"].as_str().unwrap_or_default();
    assert!(!err.is_empty(), "denial must explain itself");

    std::fs::remove_dir_all(&root).ok();
}

fn logger_wat(lines: usize) -> String {
    let calls = (0..lines).map(|_| "    (call $log (i32.const 0) (i32.const 11))").collect::<Vec<_>>().join("\n");
    format!(
        r#"(module
  (import "utsuwa" "host.log" (func $log (param i32 i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "hello-guest")
  (data (i32.const 64) "{{}}")
  {ALLOC_FN}
  (func (export "invoke") (param i32 i32) (result i64)
{calls}
    (i64.or (i64.shl (i64.extend_i32_u (i32.const 64)) (i64.const 32)) (i64.const 2))))"#
    )
}

#[tokio::test]
async fn guest_file_write_leaves_mutation_evidence() {
    use sha2::{Digest, Sha256};
    let hash = |s: &[u8]| format!("{:x}", Sha256::digest(s));

    let root = test_dir("fswrite");
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("out.txt");
    std::fs::write(&file, "before-guest").unwrap();
    let scope = root.to_string_lossy().to_string();

    let rt = setup(
        &root,
        "fswrite",
        &format!("read = []\nwrite = [\"{scope}\"]"),
        &fs_writer_wat(&file.to_string_lossy(), "after-guest"),
    );
    let tool_id = bridge_tool_id("fswrite", "run");
    let reg = registered(&rt, &tool_id);

    let out = reg
        .invoke(&tool_id, invoke_ctx("fswrite", "run"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out.content["ok"], true, "envelope: {}", out.content);
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "after-guest");
    // The audit witness: path + before/after hashes, never contents.
    let evidence = out.mutation.as_ref().expect("write attaches evidence");
    assert_eq!(evidence.path, file.to_string_lossy());
    assert_eq!(
        evidence.before_sha256.as_deref(),
        Some(hash(b"before-guest").as_str())
    );
    assert_eq!(
        evidence.after_sha256.as_deref(),
        Some(hash(b"after-guest").as_str())
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn guest_logs_captured_and_capped() {
    let root = test_dir("logs");
    let rt = setup(&root, "logs", "", &logger_wat(3));
    let tool_id = bridge_tool_id("logs", "run");
    let reg = registered(&rt, &tool_id);

    let out = reg
        .invoke(&tool_id, invoke_ctx("logs", "run"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(
        out.content["guest_log"],
        serde_json::json!(["hello-guest", "hello-guest", "hello-guest"])
    );

    // 200 lines still arrive as 64: the cap holds per invocation.
    let root2 = test_dir("logs2");
    let rt2 = setup(&root2, "logs2", "", &logger_wat(200));
    let tool_id2 = bridge_tool_id("logs2", "run");
    let reg2 = registered(&rt2, &tool_id2);
    let out2 = reg2
        .invoke(&tool_id2, invoke_ctx("logs2", "run"), serde_json::json!({}))
        .await
        .unwrap();
    assert_eq!(out2.content["guest_log"].as_array().unwrap().len(), 64);

    std::fs::remove_dir_all(&root).ok();
    std::fs::remove_dir_all(&root2).ok();
}

#[tokio::test]
async fn infinite_loop_trapped_by_fuel() {
    let root = test_dir("loop");
    let rt = setup(&root, "spin", "", LOOP_WAT);
    let tool_id = bridge_tool_id("spin", "run");
    let reg = registered(&rt, &tool_id);

    let err = reg
        .invoke(&tool_id, invoke_ctx("spin", "run"), serde_json::json!({}))
        .await
        .unwrap_err();
    match err {
        ToolError::Failed { message, .. } => assert!(
            message.contains("fuel"),
            "fuel exhaustion must be named: {message}"
        ),
        other => panic!("expected fuel trap, got: {other}"),
    }

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn wasi_import_rejected_at_load() {
    let root = test_dir("wasi");
    write_plugin(&root, "wasi", "", WASI_WAT);
    let rt = Arc::new(PluginRuntime::new().unwrap());
    rt.discover_dir(&root).unwrap();
    let err = rt.load("wasi").unwrap_err().to_string();
    assert!(
        err.contains("wasi_snapshot_preview1") || err.contains("unknown import"),
        "zero ambient authority must be explicit: {err}"
    );

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn update_refreshes_serving_plugin_and_survives_bad_disk() {
    let root = test_dir("update");
    let rt = setup(&root, "upd", "", ECHO_WAT);
    let tool_id = bridge_tool_id("upd", "run");
    let reg = registered(&rt, &tool_id);

    // Fresh bytes on disk: update keeps the plugin serving them.
    let dir = root.join("upd");
    let bytes = wat::parse_str(ECHO_WAT).unwrap();
    std::fs::write(dir.join("plugin.wasm"), bytes).unwrap();
    rt.update("upd").unwrap();
    let reg2 = registered(&rt, &tool_id);
    let out = reg2
        .invoke(&tool_id, invoke_ctx("upd", "run"), serde_json::json!({"v": 2}))
        .await
        .unwrap();
    assert_eq!(out.content, serde_json::json!({"v": 2}));
    let _ = reg;

    // Broken manifest on disk: update fails, the serving plugin survives.
    std::fs::write(dir.join("plugin.toml"), "not = [valid").unwrap();
    assert!(rt.update("upd").is_err());
    let reg3 = registered(&rt, &tool_id);
    let out = reg3
        .invoke(&tool_id, invoke_ctx("upd", "run"), serde_json::json!({"still": "here"}))
        .await
        .unwrap();
    assert_eq!(out.content, serde_json::json!({"still": "here"}));

    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn lifecycle_disable_and_reload() {
    let root = test_dir("life");
    let rt = setup(&root, "life", "", ECHO_WAT);
    let tool_id = bridge_tool_id("life", "run");
    let reg = registered(&rt, &tool_id);
    assert!(reg.resolve(&tool_id).is_ok());

    // Disable drops the instance: a fresh registry gets no tools.
    rt.disable("life").unwrap();
    let mut reg2 = ToolRegistry::new();
    let added = rt.register_enabled(&mut reg2).unwrap();
    assert!(added.is_empty(), "disabled plugin must not register: {added:?}");

    // Reload keeps the enabled state (re-enable, then reload).
    rt.enable("life").unwrap();
    rt.reload("life").unwrap();
    let reg3 = registered(&rt, &tool_id);
    let out = reg3
        .invoke(&tool_id, invoke_ctx("life", "run"), serde_json::json!({"again": true}))
        .await
        .unwrap();
    assert_eq!(out.content, serde_json::json!({"again": true}));

    std::fs::remove_dir_all(&root).ok();
}
