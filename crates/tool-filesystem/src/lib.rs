//! Filesystem broker (plan Phase 16, Task 13) with a native plugin
//! registry ([`plugin`]).
//!
//! Tools: `filesystem.list`, `filesystem.stat`, `filesystem.read`,
//! `filesystem.read_range`, `filesystem.search_text`, `filesystem.glob`,
//! `filesystem.patch`, `filesystem.write`. Every call requires a
//! capability ticket minted from a policy `Allow`; the broker
//! re-validates the ticket and enforces scope on **canonical** paths, so
//! `..` and symlinks cannot escape the granted tree. Never
//! `allowed_root.join(user_input)` without this check.

pub mod plugin;

use capability_core::{Capability, CapabilityRequest, CapabilityTicket, Resource, TicketError};
use std::path::{Path, PathBuf};
use tool_core::{CapabilityRequirement, ToolContext, ToolError, ToolOutput};

/// Broker limits (plan Phase 36).
#[derive(Debug, Clone)]
pub struct FilesystemLimits {
    pub max_read_bytes: usize,
    pub max_list_entries: usize,
    /// Largest single `filesystem.write` payload.
    pub max_write_bytes: usize,
}

impl Default for FilesystemLimits {
    fn default() -> Self {
        Self {
            max_read_bytes: 256 * 1024,
            max_list_entries: 500,
            max_write_bytes: 256 * 1024,
        }
    }
}

fn denied(message: impl Into<String>) -> ToolError {
    ToolError::Denied {
        tool: "filesystem".to_string(),
        reason: message.into(),
    }
}

fn failed(message: impl Into<String>) -> ToolError {
    ToolError::Failed {
        tool: "filesystem".to_string(),
        message: message.into(),
    }
}

/// Extract the `path` argument. Only absolute paths are accepted — relative
/// paths would resolve against an ambient cwd, which is implicit authority.
fn arg_path(args: &serde_json::Value) -> Result<PathBuf, ToolError> {
    let path = args
        .get("path")
        .and_then(|p| p.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            tool: "filesystem".to_string(),
            message: "missing string 'path' argument".to_string(),
        })?;
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(ToolError::InvalidArgs {
            tool: "filesystem".to_string(),
            message: "path must be absolute".to_string(),
        });
    }
    Ok(path)
}

/// Validate the ticket and resolve the path to a canonical location inside
/// the ticket's granted scope. Fails closed on every mismatch.
fn authorized_path(
    ctx: &ToolContext,
    capability: Capability,
    path: &Path,
) -> Result<PathBuf, ToolError> {
    let ticket = ctx.ticket.as_ref().ok_or_else(|| {
        denied("no capability ticket: route filesystem access through the agent + policy engine")
    })?;
    let target = path.canonicalize().map_err(|_| {
        failed(format!(
            "cannot access '{}': no such file or unreadable",
            path.display()
        ))
    })?;
    // Resolve symlinks before checking the ticket. Policy, grants, tickets,
    // broker checks, and audit records must all authorize this same target.
    check_ticket(ticket, ctx, &capability, &target)?;
    // Scope roots are canonicalized too, so a symlink inside the granted
    // tree pointing outside can never satisfy the prefix check.
    let mut roots = Vec::new();
    for resource in &ticket.scope.resources {
        if let Resource::Path(root) = resource {
            if let Ok(canonical) = root.canonicalize() {
                roots.push(canonical);
            }
        }
    }
    if roots.iter().any(|root| target.starts_with(root)) {
        Ok(target)
    } else {
        Err(denied(format!(
            "'{}' is outside the granted scope",
            path.display()
        )))
    }
}

/// Validate the ticket and resolve a *write* target inside the granted
/// scope, returning the resolved path plus whether it must be created.
/// Symlinks are refused outright (a link inside the scope, dangling or
/// not, could redirect creation or truncation outside it). Existing
/// files canonicalize exactly like reads; missing files resolve through
/// their nearest existing ancestor, so grants for not-yet-existing files
/// and directories work while `..` can never escape: everything is
/// lexically normalized first, symlinks resolve at the existing prefix,
/// and the remainder is literal single components.
fn authorized_write_path(
    ctx: &ToolContext,
    capability: Capability,
    path: &Path,
    create_parents: bool,
) -> Result<(PathBuf, bool), ToolError> {
    let ticket = ctx.ticket.as_ref().ok_or_else(|| {
        denied("no capability ticket: route filesystem access through the agent + policy engine")
    })?;
    if std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
    {
        return Err(denied(format!(
            "'{}' is a symlink: writes through links are refused",
            path.display()
        )));
    }
    // arg_path guarantees absolute paths; normalize away `.`/`..`/dupes
    // before any resolution so later prefix checks see no surprises.
    let normalized = lexical_normalize(path).ok_or_else(|| ToolError::InvalidArgs {
        tool: "filesystem".to_string(),
        message: "path must be absolute".to_string(),
    })?;
    let mut roots = Vec::new();
    for resource in &ticket.scope.resources {
        if let Resource::Path(root) = resource {
            if let Some(resolved) = resolve_target(root) {
                roots.push(resolved);
            }
        }
    }
    let in_scope = |target: &Path| roots.iter().any(|root| target.starts_with(root));
    let (target, create) = if normalized.exists() {
        let target = normalized
            .canonicalize()
            .map_err(|_| failed(format!("cannot access '{}': unreadable", path.display())))?;
        (target, false)
    } else {
        if normalized.file_name().is_none() {
            return Err(ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "path must name a file".to_string(),
            });
        }
        let target = resolve_target(&normalized).ok_or_else(|| {
            failed(format!(
                "cannot access '{}': no reachable parent directory",
                path.display()
            ))
        })?;
        (target, true)
    };
    check_ticket(ticket, ctx, &capability, &target)?;
    if !in_scope(&target) {
        return Err(denied(format!(
            "'{}' is outside the granted scope",
            path.display()
        )));
    }
    if create && create_parents {
        let parent = target.parent().ok_or_else(|| {
            failed(format!(
                "parent directory does not exist: {}",
                target.display()
            ))
        })?;
        if !parent.is_dir() {
            let resolved_parent = resolve_target(parent).ok_or_else(|| {
                failed(format!(
                    "parent directory does not exist: {}",
                    parent.display()
                ))
            })?;
            if !in_scope(&resolved_parent) {
                return Err(denied(format!(
                    "parent directory '{}' is outside the granted scope",
                    parent.display()
                )));
            }
        }
    }
    if !create && target.is_dir() {
        return Err(failed(format!("'{}' is a directory", path.display())));
    }
    Ok((target, create))
}

/// Lexical absolute-path normalization: resolves `.`, `..`, and
/// duplicate separators without touching the filesystem. `None` for
/// relative paths (grants and write targets are absolute-only).
fn lexical_normalize(raw: &Path) -> Option<PathBuf> {
    use std::path::Component;
    if !raw.is_absolute() {
        return None;
    }
    let mut out = PathBuf::new();
    for component in raw.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

/// Resolve a normalized absolute path against the filesystem: existing
/// paths canonicalize (symlinks resolved); missing paths resolve through
/// their nearest existing ancestor plus the literal remaining
/// components. `None` when nothing up to the root exists.
fn resolve_target(normalized: &Path) -> Option<PathBuf> {
    if let Ok(canonical) = normalized.canonicalize() {
        return Some(canonical);
    }
    let mut below: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = normalized;
    loop {
        if let Ok(canonical) = cursor.canonicalize() {
            let mut full = canonical;
            for component in below.iter().rev() {
                full.push(component);
            }
            return Some(full);
        }
        // Normalized paths only run out of names at the root, and the
        // root always canonicalizes — so `None` here ends the walk.
        let name = cursor.file_name()?;
        below.push(name.to_os_string());
        cursor = cursor.parent()?;
    }
}

fn check_ticket(
    ticket: &CapabilityTicket,
    ctx: &ToolContext,
    capability: &Capability,
    path: &Path,
) -> Result<(), ToolError> {
    let request = CapabilityRequest {
        principal: ctx.principal.clone(),
        capability: capability.clone(),
        resource: Resource::Path(path.to_path_buf()),
    };
    ticket
        .check(&ctx.principal, &request, &ctx.invocation_id)
        .map_err(|err| match err {
            TicketError::Expired => denied("capability ticket expired"),
            TicketError::PrincipalMismatch => denied("ticket bound to a different principal"),
            TicketError::InvocationMismatch => denied("ticket bound to a different invocation"),
            TicketError::CapabilityMismatch | TicketError::ScopeMismatch => {
                denied("ticket does not cover this capability/resource")
            }
        })
}

fn requirement(capability: Capability, path: &Path) -> CapabilityRequirement {
    let resolved = lexical_normalize(path)
        .and_then(|normalized| resolve_target(&normalized))
        .unwrap_or_else(|| path.to_path_buf());
    CapabilityRequirement {
        capability,
        resource: Resource::Path(resolved),
    }
}

/// Scope an explicit recursive-parent write to the nearest existing
/// directory. Creating a missing parent is itself a filesystem mutation, so
/// a ticket for only the eventual file must not silently authorize it.
fn nearest_existing_directory(path: &Path) -> Option<PathBuf> {
    let mut cursor = path.parent()?;
    loop {
        if cursor.is_dir() {
            return cursor.canonicalize().ok();
        }
        cursor = cursor.parent()?;
    }
}

fn write_requirement(args: &serde_json::Value) -> Option<CapabilityRequirement> {
    let path = arg_path(args).ok()?;
    let create_parents = args
        .get("create_parents")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if create_parents {
        if let Some(parent) = path.parent() {
            if !parent.is_dir() {
                if let Some(scope) = nearest_existing_directory(&path) {
                    return Some(requirement(Capability::FilesystemWrite, &scope));
                }
            }
        }
    }
    Some(requirement(Capability::FilesystemWrite, &path))
}

fn entry_kind(file_type: &std::fs::FileType) -> &'static str {
    if file_type.is_dir() {
        "dir"
    } else if file_type.is_file() {
        "file"
    } else if file_type.is_symlink() {
        "symlink"
    } else {
        "other"
    }
}

pub struct ListTool {
    pub limits: FilesystemLimits,
}

#[async_trait::async_trait]
impl tool_core::Tool for ListTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.list"),
            description: "List directory entries inside the granted scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        arg_path(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let dir = authorized_path(&ctx, Capability::FilesystemRead, &arg_path(&args)?)?;
        if !dir.is_dir() {
            return Err(failed(format!("'{}' is not a directory", dir.display())));
        }
        let mut entries = Vec::new();
        for entry in dir.read_dir().map_err(|e| failed(e.to_string()))? {
            let entry = entry.map_err(|e| failed(e.to_string()))?;
            let file_type = entry.file_type().map_err(|e| failed(e.to_string()))?;
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": entry_kind(&file_type),
            }));
            if entries.len() >= self.limits.max_list_entries {
                break;
            }
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(ToolOutput::new(serde_json::json!({
            "path": dir.to_string_lossy(),
            "entries": entries,
        })))
    }
}

pub struct StatTool;

#[async_trait::async_trait]
impl tool_core::Tool for StatTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.stat"),
            description: "Stat a file inside the granted scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        arg_path(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = authorized_path(&ctx, Capability::FilesystemRead, &arg_path(&args)?)?;
        let meta = std::fs::symlink_metadata(&path).map_err(|e| failed(e.to_string()))?;
        Ok(ToolOutput::new(serde_json::json!({
            "path": path.to_string_lossy(),
            "kind": entry_kind(&meta.file_type()),
            "size": meta.len(),
            "readonly": meta.permissions().readonly(),
        })))
    }
}

pub struct ReadTool {
    pub limits: FilesystemLimits,
}

#[async_trait::async_trait]
impl tool_core::Tool for ReadTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.read"),
            description: "Read a file inside the granted scope (capped).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } },
                "required": ["path"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        arg_path(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = authorized_path(&ctx, Capability::FilesystemRead, &arg_path(&args)?)?;
        let bytes = read_capped(&path, 0, self.limits.max_read_bytes as u64)?;
        let text = String::from_utf8_lossy(&bytes.content).into_owned();
        Ok(ToolOutput {
            content: serde_json::json!({ "path": path.to_string_lossy(), "content": text }),
            truncated: bytes.truncated,
            mutation: None,
        })
    }
}

pub struct ReadRangeTool {
    pub limits: FilesystemLimits,
}

#[async_trait::async_trait]
impl tool_core::Tool for ReadRangeTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.read_range"),
            description: "Read a byte range of a file inside the granted scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "offset": { "type": "integer", "minimum": 0 },
                    "length": { "type": "integer", "minimum": 1 },
                },
                "required": ["path", "offset", "length"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        arg_path(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let offset =
            args.get("offset")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: "filesystem".to_string(),
                    message: "missing unsigned 'offset'".to_string(),
                })?;
        let length =
            args.get("length")
                .and_then(|v| v.as_u64())
                .ok_or_else(|| ToolError::InvalidArgs {
                    tool: "filesystem".to_string(),
                    message: "missing unsigned 'length'".to_string(),
                })?;
        let length = length.min(self.limits.max_read_bytes as u64);
        let path = authorized_path(&ctx, Capability::FilesystemRead, &arg_path(&args)?)?;
        let bytes = read_capped(&path, offset, length)?;
        Ok(ToolOutput {
            content: serde_json::json!({
                "path": path.to_string_lossy(),
                "offset": offset,
                "content": String::from_utf8_lossy(&bytes.content),
            }),
            truncated: bytes.truncated,
            mutation: None,
        })
    }
}

/// Text search over the granted tree (plan Phase 18, Task 15).
/// Native substring matching — embeddings come later, if ever.
/// Symlinks are never followed: a symlinked dir inside the scope could
/// point outside it, and a symlinked file could leak ungranted content.
pub struct SearchTextTool {
    pub limits: SearchLimits,
}

#[derive(Debug, Clone)]
pub struct SearchLimits {
    pub max_results: usize,
    pub max_file_bytes: u64,
}

impl Default for SearchLimits {
    fn default() -> Self {
        Self {
            max_results: 50,
            max_file_bytes: 1024 * 1024,
        }
    }
}

#[async_trait::async_trait]
impl tool_core::Tool for SearchTextTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.search_text"),
            description: "Substring search across files inside the granted scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string" },
                    "pattern": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1 },
                },
                "required": ["root", "pattern"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        // Invalid roots fail closed downstream (no ticket → broker denies).
        search_root(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "missing non-empty 'pattern'".to_string(),
            })?;
        let root = search_root(&args)?;
        let root = authorized_path(&ctx, Capability::FilesystemRead, &root)?;
        if !root.is_dir() {
            return Err(failed(format!("'{}' is not a directory", root.display())));
        }
        let max_results = capped_max(&args, &self.limits);
        // Collect one extra to distinguish "exactly capped" from truncated.
        let mut matches = Vec::new();
        let mut stack = vec![root.clone()];
        while let Some(dir) = stack.pop() {
            let entries = match dir.read_dir() {
                Ok(entries) => entries,
                Err(_) => continue, // unreadable dir: skip, don't fail the search
            };
            for entry in entries.flatten() {
                if matches.len() > max_results {
                    break;
                }
                // symlink_metadata: never follow symlinks while walking.
                let meta = match entry.path().symlink_metadata() {
                    Ok(meta) => meta,
                    Err(_) => continue,
                };
                let file_type = meta.file_type();
                if file_type.is_symlink() {
                    continue;
                }
                if file_type.is_dir() {
                    stack.push(entry.path());
                    continue;
                }
                if !file_type.is_file() || meta.len() > self.limits.max_file_bytes {
                    continue;
                }
                search_file(&entry.path(), &root, pattern, &mut matches, max_results + 1);
            }
            if matches.len() > max_results {
                break;
            }
        }
        let truncated = matches.len() > max_results;
        matches.truncate(max_results);
        Ok(ToolOutput {
            content: serde_json::json!({
                "root": root.to_string_lossy(),
                "pattern": pattern,
                "matches": matches,
            }),
            truncated,
            mutation: None,
        })
    }
}

fn search_file(
    path: &Path,
    root: &Path,
    pattern: &str,
    matches: &mut Vec<serde_json::Value>,
    max_results: usize,
) {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    if bytes.contains(&0) {
        return; // binary: skip
    }
    let text = String::from_utf8_lossy(&bytes);
    let relative = path
        .strip_prefix(root)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    for (index, line) in text.lines().enumerate() {
        if line.contains(pattern) {
            matches.push(serde_json::json!({
                "path": relative,
                "line": index + 1,
                "text": line.chars().take(300).collect::<String>(),
            }));
            if matches.len() >= max_results {
                return;
            }
        }
    }
}

/// Glob matching relative to a granted root. Supports `*`, `**`, `?` per
/// path component. Symlinked dirs are not descended; symlinked files are
/// listed (no content is read, so nothing leaks).
pub struct GlobTool {
    pub limits: SearchLimits,
}

#[async_trait::async_trait]
impl tool_core::Tool for GlobTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.glob"),
            description: "Glob files inside the granted scope (*, **, ?).".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "root": { "type": "string" },
                    "pattern": { "type": "string" },
                    "max_results": { "type": "integer", "minimum": 1 },
                },
                "required": ["root", "pattern"],
            }),
            effects: vec![tool_core::ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        // Invalid roots fail closed downstream (no ticket → broker denies).
        search_root(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemRead, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let pattern = args
            .get("pattern")
            .and_then(|p| p.as_str())
            .filter(|p| !p.is_empty())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "missing non-empty 'pattern'".to_string(),
            })?;
        let root = search_root(&args)?;
        let root = authorized_path(&ctx, Capability::FilesystemRead, &root)?;
        if !root.is_dir() {
            return Err(failed(format!("'{}' is not a directory", root.display())));
        }
        let max_results = capped_max(&args, &self.limits);
        let components: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
        let mut paths = Vec::new();
        glob_walk(&root, &root, &components, &mut paths, max_results + 1);
        paths.sort();
        paths.dedup();
        let truncated = paths.len() > max_results;
        paths.truncate(max_results);
        Ok(ToolOutput {
            content: serde_json::json!({
                "root": root.to_string_lossy(),
                "pattern": pattern,
                "paths": paths,
            }),
            truncated,
            mutation: None,
        })
    }
}

fn glob_walk(
    root: &Path,
    dir: &Path,
    components: &[&str],
    out: &mut Vec<String>,
    max_results: usize,
) {
    if out.len() >= max_results || components.is_empty() {
        return;
    }
    if components[0] == "**" {
        // `**` matches zero or more components.
        glob_walk(root, dir, &components[1..], out, max_results);
        let Ok(entries) = dir.read_dir() else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.path().symlink_metadata() else {
                continue;
            };
            if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                glob_walk(root, &entry.path(), components, out, max_results);
            }
        }
        return;
    }
    let Ok(entries) = dir.read_dir() else {
        return;
    };
    let last = components.len() == 1;
    for entry in entries.flatten() {
        if out.len() >= max_results {
            return;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !component_matches(components[0], &name) {
            continue;
        }
        let Ok(meta) = entry.path().symlink_metadata() else {
            continue;
        };
        if last {
            if let Ok(relative) = entry.path().strip_prefix(root) {
                out.push(relative.to_string_lossy().into_owned());
            }
        } else if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
            glob_walk(root, &entry.path(), &components[1..], out, max_results);
        }
    }
}

/// One path component against `*` / `?` wildcards (byte-wise; asset
/// names from the filesystem are the common case and stay exact).
fn component_matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.as_bytes(), name.as_bytes());
    let (mut i, mut j) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while j < name.len() {
        if i < pattern.len() && (pattern[i] == b'?' || pattern[i] == name[j]) {
            i += 1;
            j += 1;
        } else if i < pattern.len() && pattern[i] == b'*' {
            star = Some(i);
            mark = j;
            i += 1;
        } else if let Some(s) = star {
            i = s + 1;
            mark += 1;
            j = mark;
        } else {
            return false;
        }
    }
    while i < pattern.len() && pattern[i] == b'*' {
        i += 1;
    }
    i == pattern.len()
}

/// Apply string replacements to a file (plan Phase 19, Task 16).
///
/// Safety contract, enforced in order:
/// 1. Ticket + canonical scope check (shared [`authorized_path`]).
/// 2. Optional `expected_hash` (sha256 hex of current bytes): mismatch
///    means the file changed under the agent — abort before touching disk.
/// 3. Every `old` block must occur exactly once (count != 1 aborts).
/// 4. Atomic write via temp-file + rename, so a crash never leaves a
///    half-written file. Rollback checkpoints arrive in Task 43.
pub struct PatchTool {
    pub limits: FilesystemLimits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Replacement {
    old: String,
    new: String,
}

fn parse_replacements(args: &serde_json::Value) -> Result<Vec<Replacement>, ToolError> {
    let invalid = |message: &str| ToolError::InvalidArgs {
        tool: "filesystem".to_string(),
        message: message.to_string(),
    };
    let ops = args
        .get("replacements")
        .and_then(|v| v.as_array())
        .ok_or_else(|| invalid("missing 'replacements' array"))?;
    if ops.is_empty() {
        return Err(invalid("'replacements' must not be empty"));
    }
    ops.iter()
        .map(|op| {
            let old = op
                .get("old")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("each replacement needs string 'old'"))?;
            let new = op
                .get("new")
                .and_then(|v| v.as_str())
                .ok_or_else(|| invalid("each replacement needs string 'new'"))?;
            if old.is_empty() {
                return Err(invalid("'old' must not be empty"));
            }
            Ok(Replacement {
                old: old.to_string(),
                new: new.to_string(),
            })
        })
        .collect()
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

#[async_trait::async_trait]
impl tool_core::Tool for PatchTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.patch"),
            description: "Apply exact-match replacements to a file in scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "expected_hash": { "type": "string" },
                    "replacements": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": { "type": "string" },
                                "new": { "type": "string" },
                            },
                            "required": ["old", "new"],
                        },
                    },
                },
                "required": ["path", "replacements"],
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        // Invalid paths fail closed downstream (no ticket → broker denies).
        arg_path(args)
            .ok()
            .map(|path| requirement(Capability::FilesystemWrite, &path))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let replacements = parse_replacements(&args)?;
        let path = authorized_path(&ctx, Capability::FilesystemWrite, &arg_path(&args)?)?;
        if !path.is_file() {
            return Err(failed(format!("'{}' is not a file", path.display())));
        }
        let current = std::fs::read(&path).map_err(|e| failed(e.to_string()))?;
        if current.len() as u64 > self.limits.max_read_bytes as u64 {
            return Err(failed(format!(
                "'{}' exceeds the patchable size limit",
                path.display()
            )));
        }
        if let Some(expected) = args.get("expected_hash").and_then(|v| v.as_str()) {
            let actual = sha256_hex(&current);
            if actual != expected {
                return Err(failed(format!(
                    "hash mismatch for '{}': file changed since it was read (expected {expected}, got {actual})",
                    path.display()
                )));
            }
        }
        let before_hash = sha256_hex(&current);
        let mut text = String::from_utf8(current)
            .map_err(|_| failed(format!("'{}' is not valid UTF-8", path.display())))?;
        for replacement in &replacements {
            let count = text.matches(&replacement.old).count();
            if count != 1 {
                return Err(failed(format!(
                    "replacement block occurs {count} times (must occur exactly once): '{}'",
                    replacement.old.chars().take(80).collect::<String>()
                )));
            }
            text = text.replacen(&replacement.old, &replacement.new, 1);
        }
        // Atomic write: temp file in the same directory + rename.
        let mut temp = path.clone().into_os_string();
        temp.push(format!(
            ".utsuwa-patch-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let temp = PathBuf::from(temp);
        std::fs::write(&temp, text.as_bytes()).map_err(|e| failed(e.to_string()))?;
        if let Err(err) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(failed(err.to_string()));
        }
        let after_hash = sha256_hex(text.as_bytes());
        Ok(ToolOutput::new(serde_json::json!({
            "path": path.to_string_lossy(),
            "replacements": replacements.len(),
            "hash": after_hash,
        }))
        .with_mutation(tool_core::MutationEvidence {
            path: path.to_string_lossy().into_owned(),
            before_sha256: Some(before_hash),
            after_sha256: Some(after_hash),
        }))
    }
}

pub struct WriteTool {
    pub limits: FilesystemLimits,
}

fn create_parents_arg(args: &serde_json::Value) -> Result<bool, ToolError> {
    match args.get("create_parents") {
        None => Ok(false),
        Some(value) => value.as_bool().ok_or_else(|| ToolError::InvalidArgs {
            tool: "filesystem.write".to_string(),
            message: "'create_parents' must be a boolean when provided".to_string(),
        }),
    }
}

#[async_trait::async_trait]
impl tool_core::Tool for WriteTool {
    fn metadata(&self) -> tool_core::ToolMetadata {
        tool_core::ToolMetadata {
            id: capability_core::ToolId::new("filesystem.write"),
            description: "Write a UTF-8 file to an absolute host-native path inside the granted scope (capped). The parent directory must already exist unless create_parents=true. For Desktop, Documents, Downloads, and other special directories, use the exact paths supplied by system.environment or host_environment; never guess or translate those directory names. Symlinks are never followed for the target.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute host-native file path. Use the exact special-directory path reported by system.environment or host_environment."
                    },
                    "content": {
                        "type": "string",
                        "description": "UTF-8 file contents."
                    },
                    "create_parents": {
                        "type": "boolean",
                        "default": false,
                        "description": "Explicitly create missing parent directories recursively. Omit or set false to require an existing parent directory."
                    },
                },
                "required": ["path", "content"],
            }),
            effects: vec![tool_core::ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        write_requirement(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let content = args
            .get("content")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: "missing string 'content' argument".to_string(),
            })?;
        if content.len() > self.limits.max_write_bytes {
            return Err(ToolError::InvalidArgs {
                tool: "filesystem".to_string(),
                message: format!(
                    "content exceeds the {} byte limit",
                    self.limits.max_write_bytes
                ),
            });
        }
        let create_parents = create_parents_arg(&args)?;
        let (path, created) = authorized_write_path(
            &ctx,
            Capability::FilesystemWrite,
            &arg_path(&args)?,
            create_parents,
        )?;
        let before_hash = std::fs::read(&path).ok().map(|bytes| sha256_hex(&bytes));
        let mut parent_created = false;
        if created {
            let parent = path.parent().ok_or_else(|| {
                failed(format!(
                    "parent directory does not exist: {}",
                    path.display()
                ))
            })?;
            if !parent.is_dir() {
                if !create_parents {
                    return Err(failed(format!(
                        "parent directory does not exist: {}",
                        parent.display()
                    )));
                }
                std::fs::create_dir_all(parent).map_err(|e| failed(e.to_string()))?;
                parent_created = true;
            }
        }
        std::fs::write(&path, content.as_bytes()).map_err(|e| failed(e.to_string()))?;
        let after_hash = sha256_hex(content.as_bytes());
        Ok(ToolOutput::new(serde_json::json!({
            "path": path.to_string_lossy(),
            "created": created,
            "parent_created": parent_created,
            "bytes": content.len(),
            "hash": after_hash,
        }))
        .with_mutation(tool_core::MutationEvidence {
            path: path.to_string_lossy().into_owned(),
            before_sha256: before_hash,
            after_sha256: Some(after_hash),
        }))
    }
}

/// Strict `root` argument parsing shared by search/glob: absolute paths
/// only, so calls never resolve against an ambient working directory.
fn search_root(args: &serde_json::Value) -> Result<PathBuf, ToolError> {
    let root = args
        .get("root")
        .and_then(|r| r.as_str())
        .ok_or_else(|| ToolError::InvalidArgs {
            tool: "filesystem".to_string(),
            message: "missing string 'root' argument".to_string(),
        })?;
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return Err(ToolError::InvalidArgs {
            tool: "filesystem".to_string(),
            message: "root must be absolute".to_string(),
        });
    }
    Ok(path)
}

fn capped_max(args: &serde_json::Value, limits: &SearchLimits) -> usize {
    args.get("max_results")
        .and_then(|v| v.as_u64())
        .map(|v| v.min(limits.max_results as u64) as usize)
        .unwrap_or(limits.max_results)
}

struct CappedBytes {
    content: Vec<u8>,
    truncated: bool,
}

fn read_capped(path: &Path, offset: u64, length: u64) -> Result<CappedBytes, ToolError> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path).map_err(|e| failed(e.to_string()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|e| failed(e.to_string()))?;
    let mut content = Vec::new();
    // One byte over the cap detects truncation without over-reading.
    let n = file
        .take(length + 1)
        .read_to_end(&mut content)
        .map_err(|e| failed(e.to_string()))?;
    let truncated = n as u64 > length;
    content.truncate(length as usize);
    Ok(CappedBytes { content, truncated })
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, InvocationId, Principal, ResourceScope, TicketId};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;
    use tool_core::Tool;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);
    impl TestDir {
        fn create() -> Self {
            let sequence = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "utsuwa-fs-test-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                sequence
            ));
            std::fs::create_dir_all(path.join("sub")).unwrap();
            std::fs::write(path.join("hello.txt"), "hello world").unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Mint a ticket the way the agent does on policy Allow: scoped to the
    /// exact requested resource, bound to a fresh invocation.
    fn ticket_for(
        principal: Principal,
        capability: Capability,
        resource: Resource,
    ) -> (ToolContext, InvocationId) {
        let invocation = InvocationId::fresh();
        let ticket = CapabilityTicket {
            id: TicketId::fresh(),
            principal: principal.clone(),
            capability,
            scope: ResourceScope::new(vec![resource]),
            invocation_id: invocation.clone(),
            expires_at: std::time::Instant::now() + Duration::from_secs(60),
        };
        (
            ToolContext {
                principal,
                invocation_id: invocation.clone(),
                ticket: Some(ticket),
            },
            invocation,
        )
    }

    fn agent_ctx(path: &Path) -> ToolContext {
        agent_ctx_for(path, Capability::FilesystemRead)
    }

    fn agent_ctx_for(path: &Path, capability: Capability) -> ToolContext {
        let (ctx, _) = ticket_for(
            Principal::Agent(AgentId::new("a")),
            capability,
            Resource::Path(path.to_path_buf()),
        );
        ctx
    }

    #[tokio::test]
    async fn list_and_read_inside_scope() {
        let dir = TestDir::create();
        let list = ListTool {
            limits: FilesystemLimits::default(),
        };
        let out = list
            .invoke(
                agent_ctx(&dir.0),
                serde_json::json!({"path": dir.0.to_string_lossy()}),
            )
            .await
            .unwrap();
        let names: Vec<&str> = out.content["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"hello.txt") && names.contains(&"sub"));

        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };
        let file = dir.0.join("hello.txt");
        // Ticket scopes the exact file (least privilege, as the agent mints).
        let out = read
            .invoke(
                agent_ctx(&file),
                serde_json::json!({"path": file.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["content"], "hello world");
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn read_outside_ticket_scope_is_denied() {
        let dir = TestDir::create();
        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };
        // Ticket covers the test dir only; asking for /etc/hostname fails
        // even though the path exists and parses.
        let (mut ctx, _) = ticket_for(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(dir.0.clone()),
        );
        // Invocation binding must match the ticket under test.
        ctx.invocation_id = ctx.ticket.as_ref().unwrap().invocation_id.clone();
        let err = read
            .invoke(ctx, serde_json::json!({"path": "/etc/hostname"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn no_ticket_means_no_access() {
        let dir = TestDir::create();
        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };
        let file = dir.0.join("hello.txt");
        let err = read
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("a"))),
                serde_json::json!({"path": file.to_string_lossy()}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_escape_is_denied() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::create();
        symlink("/etc", dir.0.join("escaped")).unwrap();
        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };
        // Ticket covers the dir; the symlink target does not.
        let target = dir.0.join("escaped/hostname");
        let (mut ctx, _) = ticket_for(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(dir.0.clone()),
        );
        ctx.invocation_id = ctx.ticket.as_ref().unwrap().invocation_id.clone();
        let err = read
            .invoke(ctx, serde_json::json!({"path": target.to_string_lossy()}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    }

    #[cfg(unix)]
    #[test]
    fn required_capability_resolves_symlink_before_policy() {
        use std::os::unix::fs::symlink;
        let dir = TestDir::create();
        symlink("/etc", dir.0.join("link")).unwrap();
        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };

        let escaped = dir.0.join("link/hostname");
        let requirement = read
            .required_capability(&serde_json::json!({"path": escaped}))
            .expect("valid absolute path");
        assert_eq!(
            requirement.resource,
            Resource::Path(PathBuf::from("/etc/hostname").canonicalize().unwrap())
        );
        let Resource::Path(resolved) = &requirement.resource else {
            panic!("filesystem requirement must be a path")
        };
        assert!(!resolved.starts_with(&dir.0));

        let valid = dir.0.join("hello.txt");
        let requirement = read
            .required_capability(&serde_json::json!({"path": valid}))
            .expect("valid workspace file");
        assert_eq!(
            requirement.resource,
            Resource::Path(dir.0.join("hello.txt").canonicalize().unwrap())
        );
    }

    fn search_tree() -> TestDir {
        let dir = TestDir::create();
        std::fs::write(
            dir.0.join("main.rs"),
            "fn main() {\n    println!(\"hi\");\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.0.join("sub").join("lib.rs"),
            "fn helper() {}\n// needle here\n",
        )
        .unwrap();
        std::fs::write(dir.0.join("sub").join("notes.txt"), "needle in text\n").unwrap();
        std::fs::write(
            dir.0.join("binary.bin"),
            vec![0u8, 1, 2, b'n', b'e', b'e', b'd', b'l', b'e'],
        )
        .unwrap();
        dir
    }

    fn search_ctx(dir: &TestDir) -> ToolContext {
        agent_ctx(&dir.0)
    }

    #[tokio::test]
    async fn search_finds_matches_with_lines_and_skips_binaries() {
        let dir = search_tree();
        let search = SearchTextTool {
            limits: SearchLimits::default(),
        };
        let out = search
            .invoke(
                search_ctx(&dir),
                serde_json::json!({"root": dir.0.to_string_lossy(), "pattern": "needle"}),
            )
            .await
            .unwrap();
        assert!(!out.truncated);
        let matches = out.content["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 2, "{matches:?}");
        assert!(matches
            .iter()
            .any(|m| m["path"] == "sub/lib.rs" && m["line"] == 2));
        assert!(matches
            .iter()
            .any(|m| m["path"] == "sub/notes.txt" && m["line"] == 1));
        // The binary containing the same bytes is skipped.
        assert!(!matches.iter().any(|m| m["path"] == "binary.bin"));
    }

    #[tokio::test]
    async fn search_respects_result_cap() {
        let dir = search_tree();
        let search = SearchTextTool {
            limits: SearchLimits::default(),
        };
        let out = search
            .invoke(
                search_ctx(&dir),
                serde_json::json!({
                    "root": dir.0.to_string_lossy(),
                    "pattern": "needle",
                    "max_results": 1,
                }),
            )
            .await
            .unwrap();
        assert!(out.truncated);
        assert_eq!(out.content["matches"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn search_outside_scope_is_denied() {
        let dir = search_tree();
        let search = SearchTextTool {
            limits: SearchLimits::default(),
        };
        // Ticket covers an empty subdir; searching the parent tree fails.
        let (mut ctx, _) = ticket_for(
            Principal::Agent(AgentId::new("a")),
            Capability::FilesystemRead,
            Resource::Path(dir.0.join("sub")),
        );
        ctx.invocation_id = ctx.ticket.as_ref().unwrap().invocation_id.clone();
        let err = search
            .invoke(
                ctx,
                serde_json::json!({"root": dir.0.to_string_lossy(), "pattern": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
    }

    #[tokio::test]
    async fn glob_star_star_and_question() {
        let dir = search_tree();
        let glob = GlobTool {
            limits: SearchLimits::default(),
        };
        let run = |pattern: &str| {
            let dir_path = dir.0.to_string_lossy().into_owned();
            let ctx = search_ctx(&dir);
            glob.invoke(
                ctx,
                serde_json::json!({"root": dir_path, "pattern": pattern}),
            )
        };
        let out = run("**/*.rs").await.unwrap();
        let mut paths: Vec<&str> = out.content["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["main.rs", "sub/lib.rs"]);
        assert!(!out.truncated);

        let out = run("sub/*.???").await.unwrap();
        let paths: Vec<&str> = out.content["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap())
            .collect();
        assert_eq!(paths, vec!["sub/notes.txt"]);

        let out = run("*.nomatch").await.unwrap();
        assert!(out.content["paths"].as_array().unwrap().is_empty());
        assert!(!out.truncated);
    }

    #[test]
    fn component_matcher_cases() {
        assert!(component_matches("*.rs", "main.rs"));
        assert!(!component_matches("*.rs", "main.txt"));
        assert!(!component_matches("lib.???", "lib.rs"));
        assert!(component_matches("lib.???", "lib.rsx"));
        assert!(component_matches("*", "anything"));
        assert!(component_matches("a*b*c", "axxbxxc"));
        assert!(!component_matches("a*b*c", "axxbxd"));
        assert!(component_matches("**", "deep"));
    }

    fn patch_ctx(file: &Path) -> ToolContext {
        agent_ctx_for(file, Capability::FilesystemWrite)
    }

    #[tokio::test]
    async fn patch_applies_with_matching_hash() {
        let dir = TestDir::create();
        let file = dir.0.join("hello.txt");
        let before = std::fs::read(&file).unwrap();
        let hash = sha256_hex(&before);
        let patch = PatchTool {
            limits: FilesystemLimits::default(),
        };
        let out = patch
            .invoke(
                patch_ctx(&file),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "expected_hash": hash,
                    "replacements": [{"old": "world", "new": "rust"}],
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["replacements"], 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello rust");
        // Returned hash describes the new content.
        assert_eq!(
            out.content["hash"].as_str().unwrap(),
            &sha256_hex(b"hello rust")
        );
        // Mutation evidence witnesses the change for the audit log.
        let evidence = out.mutation.as_ref().expect("patch attaches evidence");
        assert_eq!(evidence.path, file.to_string_lossy());
        assert_eq!(evidence.before_sha256.as_deref(), Some(hash.as_str()));
        assert_eq!(
            evidence.after_sha256.as_deref(),
            Some(sha256_hex(b"hello rust").as_str())
        );
    }

    #[tokio::test]
    async fn patch_rejects_stale_hash_and_ambiguous_blocks() {
        let dir = TestDir::create();
        let file = dir.0.join("hello.txt");
        std::fs::write(&file, "aaa bbb aaa").unwrap();
        let patch = PatchTool {
            limits: FilesystemLimits::default(),
        };
        // Wrong hash: file untouched.
        let err = patch
            .invoke(
                patch_ctx(&file),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "expected_hash": "deadbeef",
                    "replacements": [{"old": "aaa", "new": "ccc"}],
                }),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("hash mismatch"), "{err:?}");
        // Ambiguous block (occurs twice): file untouched.
        let err = patch
            .invoke(
                patch_ctx(&file),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "replacements": [{"old": "aaa", "new": "ccc"}],
                }),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("exactly once"), "{err:?}");
        // Missing block: file untouched.
        let err = patch
            .invoke(
                patch_ctx(&file),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "replacements": [{"old": "zzz", "new": "ccc"}],
                }),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("exactly once"), "{err:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "aaa bbb aaa");
    }

    fn write_ctx(path: &Path) -> ToolContext {
        agent_ctx_for(path, Capability::FilesystemWrite)
    }

    #[tokio::test]
    async fn write_creates_new_files_in_existing_parent() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let target = dir.0.join("note.txt");
        let out = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({"path": target.to_string_lossy(), "content": "hello"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["created"], true);
        assert_eq!(out.content["bytes"], 5);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
        let evidence = out.mutation.as_ref().expect("write attaches evidence");
        assert!(evidence.before_sha256.is_none());
        assert_eq!(
            evidence.after_sha256.as_deref(),
            Some(sha256_hex(b"hello").as_str())
        );
    }

    #[tokio::test]
    async fn write_does_not_create_missing_parent_by_default() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let missing_parent = dir.0.join("Desktop");
        let target = missing_parent.join("hello.txt");
        let err = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({"path": target.to_string_lossy(), "content": "hello"}),
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            ToolError::Failed {
                tool: "filesystem".to_string(),
                message: format!(
                    "parent directory does not exist: {}",
                    missing_parent.display()
                ),
            }
        );
        assert!(!missing_parent.exists());
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn write_can_create_parents_only_when_explicitly_requested() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let parent = dir.0.join("new").join("nested");
        let target = parent.join("note.txt");
        let out = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "content": "hello",
                    "create_parents": true,
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["created"], true);
        assert_eq!(out.content["parent_created"], true);
        assert!(parent.is_dir());
        assert_eq!(std::fs::read_to_string(target).unwrap(), "hello");
    }

    #[tokio::test]
    async fn explicit_parent_creation_needs_a_directory_scoped_ticket() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let parent = dir.0.join("new").join("nested");
        let target = parent.join("note.txt");
        let err = write
            .invoke(
                write_ctx(&target),
                serde_json::json!({
                    "path": target.to_string_lossy(),
                    "content": "hello",
                    "create_parents": true,
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
        assert!(!parent.exists());
    }

    #[tokio::test]
    async fn write_covers_file_scoped_grants_for_missing_files() {
        // The agent declares the exact file it wants; policy grants that
        // path before it exists. The grant cannot canonicalize, so the
        // write must match it lexically.
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let target = dir.0.join("planned.txt");
        let out = write
            .invoke(
                write_ctx(&target),
                serde_json::json!({"path": target.to_string_lossy(), "content": "x"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["created"], true);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "x");
    }

    #[tokio::test]
    async fn write_overwrites_existing_files() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        let file = dir.0.join("hello.txt");
        let out = write
            .invoke(
                write_ctx(&file),
                serde_json::json!({"path": file.to_string_lossy(), "content": "replaced"}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["created"], false);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "replaced");
        let evidence = out.mutation.as_ref().expect("write attaches evidence");
        assert_eq!(
            evidence.before_sha256.as_deref(),
            Some(sha256_hex(b"hello world").as_str())
        );
    }

    #[tokio::test]
    async fn write_refuses_escapes_dirs_symlinks_and_oversize() {
        let dir = TestDir::create();
        let write = WriteTool {
            limits: FilesystemLimits::default(),
        };
        // `..` escapes the granted tree.
        let err = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({"path": dir.0.join("..").join("evil").to_string_lossy(), "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
        // Existing directories are not files.
        let err = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({"path": dir.0.join("sub").to_string_lossy(), "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("directory"), "{err:?}");
        // Symlinks are never followed, dangling or not.
        #[cfg(unix)]
        {
            let link = dir.0.join("link");
            std::os::unix::fs::symlink(dir.0.join("nowhere"), &link).unwrap();
            let err = write
                .invoke(
                    write_ctx(&dir.0),
                    serde_json::json!({"path": link.to_string_lossy(), "content": "x"}),
                )
                .await
                .unwrap_err();
            assert!(format!("{err:?}").contains("symlink"), "{err:?}");
        }
        // Oversized payloads never reach the backend.
        let err = write
            .invoke(
                write_ctx(&dir.0),
                serde_json::json!({"path": dir.0.join("big").to_string_lossy(), "content": "x".repeat(300_000)}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "{err:?}");
        // No ticket, no write.
        let err = write
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("a"))),
                serde_json::json!({"path": dir.0.join("nope").to_string_lossy(), "content": "x"}),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
        assert!(!dir.0.join("nope").exists());
    }

    #[tokio::test]
    async fn patch_without_ticket_is_blocked() {
        let dir = TestDir::create();
        let file = dir.0.join("hello.txt");
        let patch = PatchTool {
            limits: FilesystemLimits::default(),
        };
        let err = patch
            .invoke(
                ToolContext::new(Principal::Agent(AgentId::new("a"))),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "replacements": [{"old": "world", "new": "rust"}],
                }),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Denied { .. }), "{err:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hello world");
    }

    #[tokio::test]
    async fn relative_paths_rejected_and_ranges_work() {
        let dir = TestDir::create();
        let read = ReadTool {
            limits: FilesystemLimits::default(),
        };
        let file = dir.0.join("hello.txt");
        let err = read
            .invoke(agent_ctx(&file), serde_json::json!({"path": "relative/x"}))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::InvalidArgs { .. }), "{err:?}");

        let range = ReadRangeTool {
            limits: FilesystemLimits::default(),
        };
        let out = range
            .invoke(
                agent_ctx(&file),
                serde_json::json!({
                    "path": file.to_string_lossy(),
                    "offset": 6,
                    "length": 5,
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["content"], "world");
    }
}
