//! WASM plugin execution (plan Phase 22 host side, Phase 23 host API).
//!
//! Guests are wasmtime core modules (Component Model + WIT is the planned
//! packaging upgrade; the host-function boundary below maps onto it).
//!
//! ## Guest contract
//!
//! Every plugin tool is one guest export with a JSON-in/JSON-out calling
//! convention over linear memory:
//!
//! ```text
//! memory: exported linear memory
//! alloc(len: i32) -> i32: bump-allocate `len` bytes, return the pointer
//! invoke(arg_ptr: i32, arg_len: i32) -> i64: run the tool;
//!     return value packs `(ret_ptr << 32) | ret_len`
//! ```
//!
//! The host writes the JSON arguments with `alloc`, calls `invoke`, and
//! reads back `ret_len` bytes. Returned bytes should be JSON (used as the
//! tool result content) or plain text (wrapped as `{"text": …}`).
//!
//! ## Host API (import module `"utsuwa"`)
//!
//! No WASI is linked — a module importing it fails to instantiate, so
//! guests start with zero authority. The narrow imports are:
//!
//! ```text
//! host.log(ptr, len): append a UTF-8 log line (capped, per invocation)
//! host.fs.read(path_ptr, path_len) -> i64: read a file through the
//!     invocation ticket; returns packed JSON {"ok","content"/"error"}
//! host.fs.write(path_ptr, path_len, data_ptr, data_len) -> i64: replace
//!     a file through the invocation ticket; same envelope
//! ```
//!
//! Policy denials come back as envelope JSON (the model sees them), never
//! as traps. Traps are reserved for ABI violations. Arguments are never
//! logged.

use capability_core::{Capability, CapabilityRequest, InvocationId, PluginId, Principal, Resource};
use plugin_core::{PluginManifest, PluginRegistry, TrustLevel};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use tool_core::{CapabilityRequirement, Tool, ToolContext, ToolError, ToolMetadata, ToolOutput};
use wasmtime::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

/// Fuel per tool invocation: bounds guest CPU even for infinite loops.
pub const FUEL_PER_CALL: u64 = 10_000_000;
/// Guest linear memory cap (16 MiB).
pub const MAX_MEMORY_BYTES: usize = 16 << 20;
/// Largest single guest/host transfer (args, results, file reads).
pub const MAX_IO_BYTES: usize = 64 * 1024;
/// Cap on captured `host.log` lines per invocation.
pub const MAX_LOG_LINES: usize = 64;

#[derive(Debug, thiserror::Error)]
pub enum WasmError {
    #[error("plugin error: {0}")]
    Plugin(#[from] plugin_core::PluginError),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("guest ABI violation: {0}")]
    Abi(String),
    #[error("guest trapped: {0}")]
    Trap(String),
    #[error("unknown plugin '{0}'")]
    Unknown(String),
}

pub struct PluginRuntime {
    engine: Engine,
    registry: Mutex<PluginRegistry>,
    instances: Mutex<HashMap<String, Mutex<LoadedPlugin>>>,
    /// Plugin directory per id, remembered at discovery for `update`.
    dirs: Mutex<HashMap<String, std::path::PathBuf>>,
}

/// Per-invocation guest authority. The two tickets are *derived* by
/// [`PluginRuntime::begin_call`] from the agent's `PluginInvoke` ticket:
/// same invocation id, 60 s TTL, scope capped by the manifest's declared
/// filesystem permissions. The manifest is the trust contract; the outer
/// `PluginInvoke` approval gates every invocation.
struct ActiveCall {
    read_ticket: Option<capability_core::CapabilityTicket>,
    write_ticket: Option<capability_core::CapabilityTicket>,
    invocation: InvocationId,
    logs: Vec<String>,
}

struct StoreData {
    plugin: PluginId,
    call: Option<ActiveCall>,
    limits: wasmtime::StoreLimits,
}

struct LoadedPlugin {
    store: Store<StoreData>,
    memory: Memory,
    alloc: TypedFunc<i32, i32>,
    invoke: TypedFunc<(i32, i32), i64>,
}

impl PluginRuntime {
    pub fn new() -> Result<Self, WasmError> {
        let mut config = wasmtime::Config::new();
        config.consume_fuel(true);
        let engine = Engine::new(&config).map_err(|e| WasmError::Engine(e.to_string()))?;
        Ok(Self {
            engine,
            registry: Mutex::new(PluginRegistry::new()),
            instances: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
        })
    }

    fn lock_registry(&self) -> Result<std::sync::MutexGuard<'_, PluginRegistry>, WasmError> {
        self.registry
            .lock()
            .map_err(|_| WasmError::Engine("plugin registry lock failed".to_string()))
    }

    // -- lifecycle ------------------------------------------------------

    /// Discover plugins under `dir` (manifest + bytes, magic-checked).
    /// Remembers each plugin directory for [`PluginRuntime::update`].
    pub fn discover_dir(&self, dir: &Path) -> Result<Vec<String>, WasmError> {
        let ids = {
            let mut registry = self.lock_registry()?;
            registry.discover_dir(dir);
            registry.ids()
        };
        let mut dirs = self
            .dirs
            .lock()
            .map_err(|_| WasmError::Engine("plugin dir lock failed".to_string()))?;
        for id in &ids {
            dirs.insert(id.clone(), dir.join(id));
        }
        Ok(ids)
    }

    /// Validate + instantiate a discovered plugin. A record already on
    /// `Validated` (fresh from [`PluginRegistry::update`]) skips the
    /// re-validation step and instantiates directly.
    pub fn load(&self, id: &str) -> Result<(), WasmError> {
        let bytes = {
            let mut registry = self.lock_registry()?;
            let state = registry
                .get(id)
                .ok_or_else(|| WasmError::Unknown(id.to_string()))?
                .state
                .clone();
            if !matches!(state, plugin_core::PluginState::Validated) {
                registry.validate(id)?;
            }
            registry.get(id).ok_or_else(|| WasmError::Unknown(id.to_string()))?.module_bytes.clone()
        };
        let manifest = self
            .lock_registry()?
            .get(id)
            .ok_or_else(|| WasmError::Unknown(id.to_string()))?
            .manifest
            .clone();
        let loaded = self.instantiate(&manifest, &bytes)?;
        self.instances.lock().map_err(|_| WasmError::Engine("instance lock failed".to_string()))?.insert(id.to_string(), Mutex::new(loaded));
        self.lock_registry()?.mark_loaded(id)?;
        Ok(())
    }

    /// Enable: the plugin must be loaded; tools join registries through
    /// [`PluginRuntime::register_enabled`]. Enabling never grants anything.
    pub fn enable(&self, id: &str) -> Result<(), WasmError> {
        Ok(self.lock_registry()?.enable(id)?)
    }

    /// Disable: drop the instance (freeing guest memory) and mark disabled.
    pub fn disable(&self, id: &str) -> Result<(), WasmError> {
        self.instances
            .lock()
            .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?
            .remove(id);
        Ok(self.lock_registry()?.disable(id)?)
    }

    /// Reload bytes from the registry record and re-instantiate, keeping
    /// the enabled state. Re-registration happens on the next
    /// `register_enabled` pass (registries are rebuilt per agent turn).
    pub fn reload(&self, id: &str) -> Result<(), WasmError> {
        let was_enabled = {
            let registry = self.lock_registry()?;
            let record = registry.get(id).ok_or_else(|| WasmError::Unknown(id.to_string()))?;
            record.state == plugin_core::PluginState::Enabled
        };
        if was_enabled {
            self.disable(id)?;
        }
        self.load(id)?;
        if was_enabled {
            self.enable(id)?;
        }
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<(), WasmError> {
        self.instances
            .lock()
            .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?
            .remove(id);
        self.dirs
            .lock()
            .map_err(|_| WasmError::Engine("plugin dir lock failed".to_string()))?
            .remove(id);
        Ok(self.lock_registry()?.remove(id)?)
    }

    /// Phase 24 `update`: refresh manifest + bytes from the discovered
    /// directory and re-instantiate, keeping the enabled state. A broken
    /// on-disk state fails with the previous record intact; a serving
    /// plugin is restored from those intact bytes so an update failure
    /// never silently unserves it.
    pub fn update(&self, id: &str) -> Result<(), WasmError> {
        let dir = self
            .dirs
            .lock()
            .map_err(|_| WasmError::Engine("plugin dir lock failed".to_string()))?
            .get(id)
            .cloned()
            .ok_or_else(|| WasmError::Unknown(id.to_string()))?;
        let was_enabled = {
            let registry = self.lock_registry()?;
            let record = registry.get(id).ok_or_else(|| WasmError::Unknown(id.to_string()))?;
            record.state == plugin_core::PluginState::Enabled
        };
        self.instances
            .lock()
            .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?
            .remove(id);
        if was_enabled {
            self.lock_registry()?.disable(id)?;
        }
        // Bind the refresh outcome first: the registry guard must drop
        // before the restore path below re-locks it (a guard held across
        // the `if let` body would deadlock `load`).
        let refreshed = self.lock_registry()?.update(id, &dir);
        if let Err(e) = refreshed {
            if was_enabled {
                // Record (and bytes) survived: restore service.
                self.load(id)?;
                self.enable(id)?;
            }
            return Err(e.into());
        }
        self.load(id)?;
        if was_enabled {
            self.enable(id)?;
        }
        Ok(())
    }

    /// Register every enabled plugin's tools into `registry`. Called per
    /// agent turn; instances load lazily on first use.
    pub fn register_enabled(
        self: &Arc<Self>,
        registry: &mut tool_core::ToolRegistry,
    ) -> Result<Vec<String>, WasmError> {
        let runtime_arc = Arc::clone(self);
        let ids: Vec<String> = {
            let registry = self.lock_registry()?;
            registry.enabled().iter().map(|r| r.manifest.id.0.clone()).collect()
        };
        let mut added = Vec::new();
        for id in ids {
            if !self
                .instances
                .lock()
                .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?
                .contains_key(&id)
            {
                if let Err(e) = self.load(&id) {
                    tracing::warn!(plugin = %id, error = %e, "plugin failed to load; skipping this turn");
                    continue;
                }
            }
            let manifests = self.lock_registry()?;
            let record = manifests.get(&id).ok_or_else(|| WasmError::Unknown(id.clone()))?;
            for tool in &record.manifest.tools {
                let bridge = PluginToolBridge {
                    plugin: record.manifest.id.clone(),
                    tool: tool.name.clone(),
                    description: tool_description(&record.manifest, tool),
                    runtime: Arc::clone(&runtime_arc),
                    id: id.clone(),
                };
                match registry.register(Arc::new(bridge)) {
                    Ok(()) => added.push(bridge_tool_id(&id, &tool.name)),
                    Err(tool_core::ToolError::DuplicateId(_)) => continue,
                    Err(e) => {
                        return Err(WasmError::Engine(format!(
                            "cannot register plugin tool '{}': {e}",
                            tool.name
                        )))
                    }
                }
            }
        }
        Ok(added)
    }

    /// Manifests of enabled plugins (for UIs and diagnostics).
    pub fn enabled_manifests(&self) -> Vec<PluginManifest> {
        self.lock_registry()
            .map(|registry| registry.enabled().iter().map(|r| r.manifest.clone()).collect())
            .unwrap_or_default()
    }

    fn instantiate(&self, manifest: &PluginManifest, bytes: &[u8]) -> Result<LoadedPlugin, WasmError> {
        // `Module::new` accepts binary or text (wat feature): text is a
        // development convenience, real plugins ship bytes.
        let module =
            Module::new(&self.engine, bytes).map_err(|e| WasmError::Engine(e.to_string()))?;
        let mut linker: Linker<StoreData> = Linker::new(&self.engine);
        linker
            .func_wrap("utsuwa", "host.log", host_log)
            .map_err(|e| WasmError::Engine(e.to_string()))?;
        linker
            .func_wrap("utsuwa", "host.fs.read", host_fs_read)
            .map_err(|e| WasmError::Engine(e.to_string()))?;
        linker
            .func_wrap("utsuwa", "host.fs.write", host_fs_write)
            .map_err(|e| WasmError::Engine(e.to_string()))?;
        // No WASI, no environment, no network: unknown imports (including
        // wasi_snapshot_preview1) fail here with zero guest authority.
        let mut store = Store::new(
            &self.engine,
            StoreData {
                plugin: manifest.id.clone(),
                call: None,
                limits: wasmtime::StoreLimitsBuilder::new()
                    .memory_size(MAX_MEMORY_BYTES)
                    .build(),
            },
        );
        store.limiter(|data: &mut StoreData| &mut data.limits);
        let instance = linker
            .instantiate(&mut store, &module)
            .map_err(|e| WasmError::Engine(format!("instantiate: {e}")))?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| WasmError::Abi("guest must export `memory`".to_string()))?;
        let alloc: TypedFunc<i32, i32> = instance
            .get_typed_func(&mut store, "alloc")
            .map_err(|_| WasmError::Abi("guest must export `alloc(i32) -> i32`".to_string()))?;
        let invoke: TypedFunc<(i32, i32), i64> = instance
            .get_typed_func(&mut store, "invoke")
            .map_err(|_| WasmError::Abi("guest must export `invoke(i32, i32) -> i64`".to_string()))?;
        Ok(LoadedPlugin {
            store,
            memory,
            alloc,
            invoke,
        })
    }

    /// Call a loaded tool: ticket-checked by the caller (bridge), executed
    /// here under fuel with per-call state. Returns raw guest bytes plus
    /// captured logs.
    fn call_tool(
        &self,
        id: &str,
        args_json: &[u8],
    ) -> Result<(Vec<u8>, Vec<String>), WasmError> {
        let instances = self
            .instances
            .lock()
            .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?;
        let instance = instances.get(id).ok_or_else(|| WasmError::Unknown(id.to_string()))?;
        let mut plugin = instance
            .lock()
            .map_err(|_| WasmError::Engine("plugin lock failed".to_string()))?;
        // Disjoint field borrows: `store` mutably, funcs/memory shared.
        let LoadedPlugin {
            store,
            memory,
            alloc,
            invoke,
        } = &mut *plugin;
        store.set_fuel(FUEL_PER_CALL).map_err(|e| WasmError::Engine(e.to_string()))?;
        let arg_ptr = alloc
            .call(&mut *store, args_json.len() as i32)
            .map_err(|e| WasmError::Trap(format!("guest alloc failed: {e}")))?;
        if args_json.len() > MAX_IO_BYTES {
            return Err(WasmError::Abi("tool arguments exceed 64 KiB".to_string()));
        }
        memory
            .write(&mut *store, arg_ptr as usize, args_json)
            .map_err(|e| WasmError::Abi(format!("cannot write guest args: {e}")))?;
        let packed = invoke
            .call(&mut *store, (arg_ptr, args_json.len() as i32))
            .map_err(|e| {
                // Name fuel exhaustion explicitly: zero remaining fuel with
                // a trap means the guest spun past its CPU bound.
                let out_of_fuel = store.get_fuel().map(|left| left == 0).unwrap_or(false);
                WasmError::Trap(if out_of_fuel {
                    format!("guest ran out of fuel (infinite loop?): {e}")
                } else {
                    e.to_string()
                })
            })?;
        let ret_ptr = (packed >> 32) as u32 as usize;
        let ret_len = (packed & 0xFFFF_FFFF) as u32 as usize;
        if ret_len > MAX_IO_BYTES {
            return Err(WasmError::Abi("tool result exceeds 64 KiB".to_string()));
        }
        let mem_len = memory.data_size(&mut *store);
        if ret_ptr.saturating_add(ret_len) > mem_len {
            return Err(WasmError::Abi("tool result points outside guest memory".to_string()));
        }
        let mut out = vec![0u8; ret_len];
        memory
            .read(&mut *store, ret_ptr, &mut out)
            .map_err(|e| WasmError::Abi(format!("cannot read guest result: {e}")))?;
        let logs = store.data_mut().call.take().map(|c| c.logs).unwrap_or_default();
        Ok((out, logs))
    }

    /// Install per-invocation guest authority before a bridge call.
    /// Verifies the agent ticket authorizes this exact plugin tool, then
    /// derives plugin-bound read/write tickets capped by the manifest.
    /// Defense in depth with the bridge's `authorize`: forged or
    /// cross-plugin tickets never become guest authority.
    fn begin_call(
        &self,
        id: &str,
        tool: &str,
        agent_ticket: &capability_core::CapabilityTicket,
        agent_principal: &Principal,
        invocation: InvocationId,
    ) -> Result<(), WasmError> {
        let manifest = self
            .lock_registry()?
            .get(id)
            .ok_or_else(|| WasmError::Unknown(id.to_string()))?
            .manifest
            .clone();
        let request = CapabilityRequest {
            principal: agent_principal.clone(),
            capability: Capability::PluginInvoke,
            resource: Resource::PluginTool {
                plugin: manifest.id.0.clone(),
                tool: tool.to_string(),
            },
        };
        agent_ticket
            .check(agent_principal, &request, &invocation)
            .map_err(|e| WasmError::Engine(format!("plugin call not authorized: {e}")))?;
        let principal = Principal::WasmPlugin(manifest.id.clone());
        let ttl = std::time::Duration::from_secs(60);
        let scope_of = |paths: &[String]| {
            let resources: Vec<Resource> =
                paths.iter().map(|p| Resource::Path(p.into())).collect();
            (!resources.is_empty()).then(|| capability_core::ResourceScope::new(resources))
        };
        let mint = |capability: Capability, scope: Option<capability_core::ResourceScope>| {
            scope.map(|scope| {
                capability_core::CapabilityTicket::mint(
                    principal.clone(),
                    capability,
                    scope,
                    invocation.clone(),
                    ttl,
                )
            })
        };
        let call = ActiveCall {
            read_ticket: mint(Capability::FilesystemRead, scope_of(&manifest.filesystem.read)),
            write_ticket: mint(Capability::FilesystemWrite, scope_of(&manifest.filesystem.write)),
            invocation,
            logs: Vec::new(),
        };
        let instances = self
            .instances
            .lock()
            .map_err(|_| WasmError::Engine("instance lock failed".to_string()))?;
        let instance = instances.get(id).ok_or_else(|| WasmError::Unknown(id.to_string()))?;
        let mut plugin = instance
            .lock()
            .map_err(|_| WasmError::Engine("plugin lock failed".to_string()))?;
        plugin.store.data_mut().call = Some(call);
        Ok(())
    }
}

impl Default for PluginRuntime {
    fn default() -> Self {
        Self::new().expect("wasmtime engine builds")
    }
}

// -- guest memory helpers --------------------------------------------------

fn guest_memory(caller: &mut Caller<'_, StoreData>) -> Result<Memory, wasmtime::Error> {
    caller
        .get_export("memory")
        .and_then(|e| e.into_memory())
        .ok_or_else(|| wasmtime::Error::msg("guest must export `memory`"))
}

fn read_guest_str(
    caller: &mut Caller<'_, StoreData>,
    ptr: i32,
    len: i32,
) -> Result<String, wasmtime::Error> {
    if ptr < 0 || len < 0 || len as usize > MAX_IO_BYTES {
        return Err(wasmtime::Error::msg("string out of bounds"));
    }
    let memory = guest_memory(caller)?;
    let mut buf = vec![0u8; len as usize];
    memory
        .read(caller, ptr as usize, &mut buf)
        .map_err(|_| wasmtime::Error::msg("string outside guest memory"))?;
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn guest_alloc(caller: &mut Caller<'_, StoreData>, len: usize) -> Result<usize, wasmtime::Error> {
    let alloc = caller
        .get_export("alloc")
        .and_then(|e| e.into_func())
        .ok_or_else(|| wasmtime::Error::msg("guest must export `alloc`"))?
        .typed::<i32, i32>(&mut *caller)
        .map_err(|_| wasmtime::Error::msg("bad `alloc` signature"))?;
    let ptr = alloc
        .call(caller, len as i32)
        .map_err(|e| wasmtime::Error::msg(format!("guest alloc failed: {e}")))?;
    if ptr < 0 {
        return Err(wasmtime::Error::msg("guest alloc returned negative"));
    }
    Ok(ptr as usize)
}

fn return_guest_bytes(
    caller: &mut Caller<'_, StoreData>,
    bytes: &[u8],
) -> Result<i64, wasmtime::Error> {
    let ptr = guest_alloc(caller, bytes.len())?;
    let memory = guest_memory(caller)?;
    memory
        .write(caller, ptr, bytes)
        .map_err(|_| wasmtime::Error::msg("cannot write guest memory"))?;
    Ok(((ptr as i64) << 32) | (bytes.len() as i64))
}

// -- host imports ----------------------------------------------------------

/// `host.log(ptr, len)`: capture a guest log line. Capped per invocation;
/// never a secret risk (the guest chose to log it).
fn host_log(mut caller: Caller<'_, StoreData>, ptr: i32, len: i32) -> Result<(), wasmtime::Error> {
    let line = read_guest_str(&mut caller, ptr, len)?;
    if let Some(call) = caller.data_mut().call.as_mut() {
        if call.logs.len() < MAX_LOG_LINES {
            call.logs.push(line);
        }
    }
    Ok(())
}

fn ticket_check(
    caller: &Caller<'_, StoreData>,
    capability: Capability,
    resource: Resource,
) -> Result<(), String> {
    let data = caller.data();
    let call = data.call.as_ref().ok_or_else(|| "no active call".to_string())?;
    let ticket = match capability {
        Capability::FilesystemRead => call.read_ticket.as_ref(),
        Capability::FilesystemWrite => call.write_ticket.as_ref(),
        _ => None,
    }
    .ok_or_else(|| {
        "no capability ticket: route plugin calls through the agent + policy engine".to_string()
    })?;
    let principal = Principal::WasmPlugin(data.plugin.clone());
    let request = CapabilityRequest {
        principal: principal.clone(),
        capability: capability.clone(),
        resource,
    };
    ticket.check(&principal, &request, &call.invocation).map_err(|e| {
        format!(
            "ticket does not authorize this plugin call: {}",
            match e {
                capability_core::TicketError::Expired => "capability ticket expired",
                capability_core::TicketError::PrincipalMismatch =>
                    "ticket bound to a different principal",
                capability_core::TicketError::InvocationMismatch =>
                    "ticket bound to a different invocation",
                capability_core::TicketError::CapabilityMismatch
                | capability_core::TicketError::ScopeMismatch =>
                    "ticket does not cover this resource",
            }
        )
    })
}

/// `host.fs.read(path_ptr, path_len) -> i64`: file read through the
/// invocation ticket. Denials return envelope JSON, never traps.
fn host_fs_read(
    mut caller: Caller<'_, StoreData>,
    path_ptr: i32,
    path_len: i32,
) -> Result<i64, wasmtime::Error> {
    let path = read_guest_str(&mut caller, path_ptr, path_len)?;
    let envelope = match ticket_check(
        &caller,
        Capability::FilesystemRead,
        Resource::Path(path.clone().into()),
    ) {
        Err(reason) => serde_json::json!({"ok": false, "error": reason}),
        Ok(()) => match std::fs::read(&path) {
            Err(e) => serde_json::json!({"ok": false, "error": format!("read failed: {e}")}),
            Ok(bytes) => {
                let mut bytes = bytes;
                let truncated = bytes.len() > MAX_IO_BYTES;
                bytes.truncate(MAX_IO_BYTES);
                serde_json::json!({
                    "ok": true,
                    "content": String::from_utf8_lossy(&bytes).into_owned(),
                    "truncated": truncated,
                })
            }
        },
    };
    return_guest_bytes(&mut caller, envelope.to_string().as_bytes())
}

/// `host.fs.write(path_ptr, path_len, data_ptr, data_len) -> i64`: full
/// file replacement through the invocation ticket. Mutations are audited
/// by the agent layer; the envelope reports the outcome.
fn host_fs_write(
    mut caller: Caller<'_, StoreData>,
    path_ptr: i32,
    path_len: i32,
    data_ptr: i32,
    data_len: i32,
) -> Result<i64, wasmtime::Error> {
    let path = read_guest_str(&mut caller, path_ptr, path_len)?;
    if data_len < 0 || data_len as usize > MAX_IO_BYTES {
        return Err(wasmtime::Error::msg("data out of bounds"));
    }
    let memory = guest_memory(&mut caller)?;
    let mut data = vec![0u8; data_len as usize];
    memory
        .read(&mut caller, data_ptr as usize, &mut data)
        .map_err(|_| wasmtime::Error::msg("data outside guest memory"))?;
    let envelope = match ticket_check(
        &caller,
        Capability::FilesystemWrite,
        Resource::Path(path.clone().into()),
    ) {
        Err(reason) => serde_json::json!({"ok": false, "error": reason}),
        Ok(()) => match std::fs::write(&path, &data) {
            Err(e) => serde_json::json!({"ok": false, "error": format!("write failed: {e}")}),
            Ok(()) => serde_json::json!({"ok": true, "bytes": data.len()}),
        },
    };
    return_guest_bytes(&mut caller, envelope.to_string().as_bytes())
}

// -- tool bridge -----------------------------------------------------------

/// Registry id for a plugin tool.
pub fn bridge_tool_id(plugin: &str, tool: &str) -> String {
    format!("plugin.{plugin}.{tool}")
}

fn tool_description(manifest: &PluginManifest, tool: &plugin_core::PluginToolDecl) -> String {
    let summary = if tool.description.is_empty() {
        "(no description)"
    } else {
        &tool.description
    };
    format!(
        "[WASM plugin '{}' v{} | trust: {}] {summary} Server-unverified guest code: runs sandboxed, no ambient authority.",
        manifest.id.0,
        manifest.version,
        manifest.trust.as_str()
    )
}

/// One guest tool bridged into the host [`ToolRegistry`]. Holds the
/// runtime by `Arc` (`Store` is `Send`, so the mutex-guarded instances
/// map is `Sync` with no unsafe code).
pub struct PluginToolBridge {
    plugin: PluginId,
    tool: String,
    description: String,
    runtime: Arc<PluginRuntime>,
    id: String,
}

impl PluginToolBridge {
    fn authorize(&self, ctx: &ToolContext) -> Result<(), ToolError> {
        let ticket = ctx.ticket.as_ref().ok_or_else(|| ToolError::Denied {
            tool: "plugin".to_string(),
            reason: "no capability ticket: plugin tools authorize through the agent + policy engine"
                .to_string(),
        })?;
        let request = CapabilityRequest {
            principal: ctx.principal.clone(),
            capability: Capability::PluginInvoke,
            resource: Resource::PluginTool {
                plugin: self.plugin.0.clone(),
                tool: self.tool.clone(),
            },
        };
        ticket
            .check(&ctx.principal, &request, &ctx.invocation_id)
            .map_err(|_| ToolError::Denied {
                tool: "plugin".to_string(),
                reason: "ticket does not authorize this plugin tool".to_string(),
            })
    }
}

#[async_trait::async_trait]
impl Tool for PluginToolBridge {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new(bridge_tool_id(&self.plugin.0, &self.tool)),
            description: self.description.clone(),
            input_schema: serde_json::json!({ "type": "object" }),
            effects: vec![tool_core::ToolEffect::ExternalSideEffect],
        }
    }

    /// Always `Some`: guest code never runs without a policy decision.
    fn required_capability(&self, _args: &serde_json::Value) -> Option<CapabilityRequirement> {
        Some(CapabilityRequirement {
            capability: Capability::PluginInvoke,
            resource: Resource::PluginTool {
                plugin: self.plugin.0.clone(),
                tool: self.tool.clone(),
            },
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        self.authorize(&ctx)?;
        // wasmtime is synchronous: run the guest, then return to async.
        let failed = |message: String| ToolError::Failed {
            tool: "plugin".to_string(),
            message,
        };
        let args_json = serde_json::to_vec(&args).map_err(|e| failed(e.to_string()))?;
        let ticket = ctx.ticket.clone().ok_or_else(|| failed("missing capability ticket".to_string()))?;
        self.runtime
            .begin_call(
                &self.id,
                &self.tool,
                &ticket,
                &ctx.principal,
                ctx.invocation_id.clone(),
            )
            .map_err(|e| failed(e.to_string()))?;
        let (bytes, logs) = self
            .runtime
            .call_tool(&self.id, &args_json)
            .map_err(|e| failed(e.to_string()))?;
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let content: serde_json::Value =
            serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({"text": text}));
        let mut content = content;
        if !logs.is_empty() {
            content["guest_log"] = serde_json::json!(logs);
        }
        Ok(ToolOutput::new(content))
    }
}

/// Trust tier display for diagnostics.
pub fn trust_label(trust: TrustLevel) -> &'static str {
    trust.as_str()
}

/// UI/IPC view of one known plugin: identity, trust, lifecycle state,
/// and tool names. State and trust are plain strings so the frontend
/// never parses Rust enums.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub version: String,
    pub trust: String,
    pub state: String,
    pub tools: Vec<String>,
}

impl PluginRuntime {
    /// Snapshot of every known plugin for `plugin.list`.
    pub fn infos(&self) -> Vec<PluginInfo> {
        let registry = match self.lock_registry() {
            Ok(registry) => registry,
            Err(_) => return Vec::new(),
        };
        registry
            .ids()
            .iter()
            .filter_map(|id| registry.get(id))
            .map(|record| PluginInfo {
                id: record.manifest.id.0.clone(),
                name: record.manifest.name.clone(),
                version: record.manifest.version.clone(),
                trust: record.manifest.trust.as_str().to_string(),
                state: record.state.as_str().to_string(),
                tools: record.manifest.tools.iter().map(|t| t.name.clone()).collect(),
            })
            .collect()
    }
}
