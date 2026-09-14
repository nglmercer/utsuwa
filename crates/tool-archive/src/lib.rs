//! Safe archive tools (`archive.*`).
//!
//! Supported formats: `zip`, `tar`, `tar.gz`/`tgz`. Every extraction
//! enforces: no absolute paths, no `..` escapes, no symlink escape, a
//! bounded entry count, and a bounded total output size (decompression
//! bombs fail with `archive_too_large`, never fill the disk). Extraction
//! and creation require filesystem write/create tickets for the target
//! scope; listing requires a read ticket for the archive itself.

use capability_core::{Capability, Resource};
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

/// Bounds for archive operations.
#[derive(Debug, Clone)]
pub struct ArchiveLimits {
    pub max_entries: usize,
    pub max_total_bytes: u64,
    pub max_entry_bytes: u64,
}

impl Default for ArchiveLimits {
    fn default() -> Self {
        Self {
            max_entries: 10_000,
            max_total_bytes: 512 * 1024 * 1024,
            max_entry_bytes: 128 * 1024 * 1024,
        }
    }
}

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn failed(tool: &str, code: &str, message: String) -> ToolError {
    ToolError::structured(tool, code, message)
}

/// Resolve `name` inside `base` without escape: rejects absolute paths,
/// parent-component escapes, and (for tar) symlink entries.
fn safe_join(tool: &str, base: &Path, name: &str) -> Result<PathBuf, ToolError> {
    let requested = Path::new(name);
    if requested.is_absolute() {
        return Err(failed(
            tool,
            "archive_path_traversal",
            format!("archive entry is absolute: '{name}'"),
        ));
    }
    let mut joined = PathBuf::from(base);
    for component in requested.components() {
        match component {
            Component::Normal(part) => joined.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(failed(
                    tool,
                    "archive_path_traversal",
                    format!("archive entry escapes its directory: '{name}'"),
                ));
            }
        }
    }
    Ok(joined)
}

fn check_budget(
    tool: &str,
    total: &mut u64,
    add: u64,
    limits: &ArchiveLimits,
) -> Result<(), ToolError> {
    *total = total.saturating_add(add);
    if *total > limits.max_total_bytes {
        return Err(ToolError::structured_with_details(
            tool,
            "archive_too_large",
            format!(
                "archive output exceeds the {}-byte limit (possible decompression bomb)",
                limits.max_total_bytes
            ),
            serde_json::json!({ "limit_bytes": limits.max_total_bytes }),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
}

fn detect_kind(path: &Path) -> Result<ArchiveKind, ToolError> {
    let name = path.to_string_lossy().to_ascii_lowercase();
    if name.ends_with(".zip") {
        Ok(ArchiveKind::Zip)
    } else if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(ArchiveKind::TarGz)
    } else if name.ends_with(".tar") {
        Ok(ArchiveKind::Tar)
    } else {
        Err(invalid(
            "archive.list",
            "unsupported archive format (expected .zip, .tar, .tar.gz, or .tgz)",
        ))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct EntryInfo {
    path: String,
    size_bytes: u64,
    is_dir: bool,
}

fn list_entries(
    tool: &str,
    path: &Path,
    limits: &ArchiveLimits,
) -> Result<Vec<EntryInfo>, ToolError> {
    let kind = detect_kind(path).map_err(|_| {
        invalid(
            tool,
            "unsupported archive format (expected .zip, .tar, .tar.gz, or .tgz)",
        )
    })?;
    let mut entries = Vec::new();
    match kind {
        ArchiveKind::Zip => {
            let file = std::fs::File::open(path)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            if archive.len() > limits.max_entries {
                return Err(failed(
                    tool,
                    "archive_too_large",
                    format!(
                        "archive has {} entries (limit {})",
                        archive.len(),
                        limits.max_entries
                    ),
                ));
            }
            for index in 0..archive.len() {
                let entry = archive
                    .by_index(index)
                    .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                entries.push(EntryInfo {
                    path: entry.name().to_string(),
                    size_bytes: entry.size(),
                    is_dir: entry.is_dir(),
                });
            }
        }
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            let file = std::fs::File::open(path)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let mut entry_count = 0;
            if kind == ArchiveKind::TarGz {
                let decoder = flate2::read::GzDecoder::new(file);
                for entry in tar::Archive::new(decoder)
                    .entries()
                    .map_err(|error| failed(tool, "action_failed", error.to_string()))?
                {
                    let entry =
                        entry.map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                    entry_count += 1;
                    if entry_count > limits.max_entries {
                        return Err(failed(
                            tool,
                            "archive_too_large",
                            format!("archive exceeds the {}-entry limit", limits.max_entries),
                        ));
                    }
                    entries.push(EntryInfo {
                        path: entry
                            .path()
                            .map_err(|error| failed(tool, "action_failed", error.to_string()))?
                            .to_string_lossy()
                            .into_owned(),
                        size_bytes: entry.size(),
                        is_dir: entry.header().entry_type().is_dir(),
                    });
                }
            } else {
                for entry in tar::Archive::new(file)
                    .entries()
                    .map_err(|error| failed(tool, "action_failed", error.to_string()))?
                {
                    let entry =
                        entry.map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                    entry_count += 1;
                    if entry_count > limits.max_entries {
                        return Err(failed(
                            tool,
                            "archive_too_large",
                            format!("archive exceeds the {}-entry limit", limits.max_entries),
                        ));
                    }
                    entries.push(EntryInfo {
                        path: entry
                            .path()
                            .map_err(|error| failed(tool, "action_failed", error.to_string()))?
                            .to_string_lossy()
                            .into_owned(),
                        size_bytes: entry.size(),
                        is_dir: entry.header().entry_type().is_dir(),
                    });
                }
            }
        }
    }
    Ok(entries)
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
            "no capability ticket authorizes this archive operation",
            serde_json::json!({
                "capability": format!("{capability:?}"),
                "resource": format!("{resource:?}"),
            }),
        ))
    }
}

/// Declared requirements for `archive.extract`: the archive must be
/// readable and the destination must be creatable *and* writable
/// (extraction creates directories and overwrites files). The agent
/// preflight authorizes exactly these; `invoke` enforces the same set.
fn extract_requirements(archive: PathBuf, destination: PathBuf) -> Vec<CapabilityRequirement> {
    vec![
        CapabilityRequirement {
            capability: Capability::FilesystemRead,
            resource: Resource::Path(archive),
        },
        CapabilityRequirement {
            capability: Capability::FilesystemCreate,
            resource: Resource::Path(destination.clone()),
        },
        CapabilityRequirement {
            capability: Capability::FilesystemWrite,
            resource: Resource::Path(destination),
        },
    ]
}

/// Declared requirements for `archive.create`: every source file is read
/// through the source scope, and the output archive may be created or
/// overwritten.
fn create_requirements(source: PathBuf, archive: PathBuf) -> Vec<CapabilityRequirement> {
    vec![
        CapabilityRequirement {
            capability: Capability::FilesystemRead,
            resource: Resource::Path(source),
        },
        CapabilityRequirement {
            capability: Capability::FilesystemCreate,
            resource: Resource::Path(archive.clone()),
        },
        CapabilityRequirement {
            capability: Capability::FilesystemWrite,
            resource: Resource::Path(archive),
        },
    ]
}

fn path_arg(args: &serde_json::Value, field: &str, tool: &str) -> Result<PathBuf, ToolError> {
    let raw = args
        .get(field)
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid(tool, format!("missing non-empty string '{field}'")))?;
    Ok(PathBuf::from(raw))
}

struct ArchiveListTool {
    limits: ArchiveLimits,
}

struct ArchiveExtractTool {
    limits: ArchiveLimits,
}

struct ArchiveCreateTool {
    limits: ArchiveLimits,
}

#[async_trait::async_trait]
impl Tool for ArchiveListTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("archive.list"),
            description:
                "List archive entries (path, size) without extracting. Supports zip, tar, tar.gz."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let path = args.get("path")?.as_str()?;
        Some(CapabilityRequirement {
            capability: Capability::FilesystemRead,
            resource: Resource::Path(PathBuf::from(path)),
        })
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "path", "archive.list")?;
        require_ticket(
            "archive.list",
            &ctx,
            Capability::FilesystemRead,
            Resource::Path(path.clone()),
        )?;
        let entries = tokio::task::spawn_blocking({
            let limits = self.limits.clone();
            move || list_entries("archive.list", &path, &limits)
        })
        .await
        .map_err(|error| failed("archive.list", "action_failed", error.to_string()))??;
        Ok(ToolOutput::json(serde_json::json!({
            "entries": entries,
            "entry_count": entries.len(),
        })))
    }
}

#[async_trait::async_trait]
impl Tool for ArchiveExtractTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("archive.extract"),
            description: "Extract a zip/tar/tar.gz archive into a directory. Path traversal, absolute paths, symlinks, and decompression bombs are rejected.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "archive": {"type": "string"},
                    "destination": {"type": "string"},
                },
                "required": ["archive", "destination"],
            }),
            effects: vec![ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let destination = args.get("destination")?.as_str()?;
        Some(CapabilityRequirement {
            capability: Capability::FilesystemWrite,
            resource: Resource::Path(PathBuf::from(destination)),
        })
    }

    fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
        let (Some(archive), Some(destination)) = (
            args.get("archive").and_then(|value| value.as_str()),
            args.get("destination").and_then(|value| value.as_str()),
        ) else {
            return self.required_capability(args).into_iter().collect();
        };
        extract_requirements(PathBuf::from(archive), PathBuf::from(destination))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let archive_path = path_arg(&args, "archive", "archive.extract")?;
        let destination = path_arg(&args, "destination", "archive.extract")?;
        // Enforce exactly the declared requirements: read the archive,
        // create destination entries, and overwrite existing files.
        for requirement in extract_requirements(archive_path.clone(), destination.clone()) {
            require_ticket(
                "archive.extract",
                &ctx,
                requirement.capability,
                requirement.resource,
            )?;
        }
        let limits = self.limits.clone();
        let destination_for_task = destination.clone();
        let extracted = tokio::task::spawn_blocking(move || {
            extract_archive(
                "archive.extract",
                &archive_path,
                &destination_for_task,
                &limits,
            )
        })
        .await
        .map_err(|error| failed("archive.extract", "action_failed", error.to_string()))??;
        Ok(ToolOutput::json(serde_json::json!({
            "extracted_files": extracted,
            "destination": destination.to_string_lossy(),
        })))
    }
}

fn extract_archive(
    tool: &str,
    archive_path: &Path,
    destination: &Path,
    limits: &ArchiveLimits,
) -> Result<usize, ToolError> {
    let kind = detect_kind(archive_path).map_err(|_| {
        invalid(
            tool,
            "unsupported archive format (expected .zip, .tar, .tar.gz, or .tgz)",
        )
    })?;
    std::fs::create_dir_all(destination)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    let canonical_base = destination
        .canonicalize()
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    let mut total: u64 = 0;
    let mut count = 0;
    match kind {
        ArchiveKind::Zip => {
            let file = std::fs::File::open(archive_path)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let mut archive = zip::ZipArchive::new(file)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            let len = archive.len();
            if len > limits.max_entries {
                return Err(failed(
                    tool,
                    "archive_too_large",
                    format!("archive has {len} entries (limit {})", limits.max_entries),
                ));
            }
            for index in 0..len {
                let mut entry = archive
                    .by_index(index)
                    .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                let name = entry.name().to_string();
                if entry.is_dir() {
                    let dir = safe_join(tool, &canonical_base, &name)?;
                    std::fs::create_dir_all(&dir)
                        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                    continue;
                }
                let target = safe_join(tool, &canonical_base, &name)?;
                if entry.size() > limits.max_entry_bytes {
                    return Err(failed(
                        tool,
                        "archive_too_large",
                        format!("entry '{name}' exceeds the per-entry limit"),
                    ));
                }
                // Stream with a hard cap: the header size is untrusted.
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                }
                let mut out = std::fs::File::create(&target)
                    .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                let mut written: u64 = 0;
                let mut buffer = [0u8; 32 * 1024];
                loop {
                    let read = entry
                        .read(&mut buffer)
                        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                    if read == 0 {
                        break;
                    }
                    written = written.saturating_add(read as u64);
                    if written > limits.max_entry_bytes {
                        let _ = std::fs::remove_file(&target);
                        return Err(failed(
                            tool,
                            "archive_too_large",
                            format!("entry '{name}' exceeds the per-entry limit"),
                        ));
                    }
                    check_budget(tool, &mut total, read as u64, limits)?;
                    out.write_all(&buffer[..read])
                        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                }
                count += 1;
            }
        }
        ArchiveKind::Tar | ArchiveKind::TarGz => {
            let file = std::fs::File::open(archive_path)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            extract_tar_entries(
                tool,
                file,
                kind,
                &canonical_base,
                limits,
                &mut total,
                &mut count,
            )?;
        }
    }
    Ok(count)
}

fn extract_tar_entries(
    tool: &str,
    file: std::fs::File,
    kind: ArchiveKind,
    base: &Path,
    limits: &ArchiveLimits,
    total: &mut u64,
    count: &mut usize,
) -> Result<(), ToolError> {
    // Shared extraction core: entries are validated before any byte lands.
    enum Stream {
        Plain(tar::Archive<std::fs::File>),
        Gz(tar::Archive<flate2::read::GzDecoder<std::fs::File>>),
    }
    let mut stream = match kind {
        ArchiveKind::TarGz => Stream::Gz(tar::Archive::new(flate2::read::GzDecoder::new(file))),
        _ => Stream::Plain(tar::Archive::new(file)),
    };
    fn handle_entry<R: Read>(
        tool: &str,
        entry: &mut tar::Entry<'_, R>,
        base: &Path,
        limits: &ArchiveLimits,
        total: &mut u64,
        count: &mut usize,
    ) -> Result<(), ToolError> {
        let entry_type = entry.header().entry_type();
        if entry_type.is_symlink() || entry_type.is_hard_link() {
            return Err(failed(
                tool,
                "archive_path_traversal",
                "archive contains a symlink/hardlink entry".to_string(),
            ));
        }
        let name = entry
            .path()
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?
            .to_string_lossy()
            .into_owned();
        if entry.header().entry_type().is_dir() {
            let dir = safe_join(tool, base, &name)?;
            std::fs::create_dir_all(&dir)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            return Ok(());
        }
        let target = safe_join(tool, base, &name)?;
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        }
        let mut out = std::fs::File::create(&target)
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        let mut written: u64 = 0;
        let mut buffer = [0u8; 32 * 1024];
        loop {
            let read = entry
                .read(&mut buffer)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            if read == 0 {
                break;
            }
            written = written.saturating_add(read as u64);
            if written > limits.max_entry_bytes {
                let _ = std::fs::remove_file(&target);
                return Err(failed(
                    tool,
                    "archive_too_large",
                    format!("entry '{name}' exceeds the per-entry limit"),
                ));
            }
            check_budget(tool, total, read as u64, limits)?;
            out.write_all(&buffer[..read])
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        }
        *count += 1;
        if *count > limits.max_entries {
            return Err(failed(
                tool,
                "archive_too_large",
                format!("archive exceeds the {}-entry limit", limits.max_entries),
            ));
        }
        Ok(())
    }
    match &mut stream {
        Stream::Plain(archive) => {
            let entries = archive
                .entries()
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            for entry in entries {
                let mut entry =
                    entry.map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                handle_entry(tool, &mut entry, base, limits, total, count)?;
            }
        }
        Stream::Gz(archive) => {
            let entries = archive
                .entries()
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
            for entry in entries {
                let mut entry =
                    entry.map_err(|error| failed(tool, "action_failed", error.to_string()))?;
                handle_entry(tool, &mut entry, base, limits, total, count)?;
            }
        }
    }
    Ok(())
}

#[async_trait::async_trait]
impl Tool for ArchiveCreateTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("archive.create"),
            description: "Create a zip archive from files inside one source directory. Rejects inputs outside the source scope.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "archive": {"type": "string"},
                    "source": {"type": "string"},
                    "files": {"type": "array", "items": {"type": "string"}},
                },
                "required": ["archive", "source", "files"],
            }),
            effects: vec![ToolEffect::FilesystemWrite],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        let archive = args.get("archive")?.as_str()?;
        Some(CapabilityRequirement {
            capability: Capability::FilesystemCreate,
            resource: Resource::Path(PathBuf::from(archive)),
        })
    }

    fn required_capabilities(&self, args: &serde_json::Value) -> Vec<CapabilityRequirement> {
        let (Some(source), Some(archive)) = (
            args.get("source").and_then(|value| value.as_str()),
            args.get("archive").and_then(|value| value.as_str()),
        ) else {
            return self.required_capability(args).into_iter().collect();
        };
        create_requirements(PathBuf::from(source), PathBuf::from(archive))
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let archive_path = path_arg(&args, "archive", "archive.create")?;
        let source = path_arg(&args, "source", "archive.create")?;
        let files = args
            .get("files")
            .and_then(|value| value.as_array())
            .ok_or_else(|| invalid("archive.create", "missing array 'files'"))?;
        if files.is_empty() || files.len() > self.limits.max_entries {
            return Err(invalid(
                "archive.create",
                "files must be a non-empty bounded array",
            ));
        }
        // Enforce exactly the declared requirements: the source scope
        // covers every listed file, and the output may be created or
        // overwritten.
        for requirement in create_requirements(source.clone(), archive_path.clone()) {
            require_ticket(
                "archive.create",
                &ctx,
                requirement.capability,
                requirement.resource,
            )?;
        }
        let mut names = Vec::with_capacity(files.len());
        for file in files {
            let name = file
                .as_str()
                .ok_or_else(|| invalid("archive.create", "files must be strings"))?;
            if name.trim().is_empty() {
                return Err(invalid("archive.create", "file names must be non-empty"));
            }
            names.push(name.to_string());
        }
        let file_count = names.len();
        let created = tokio::task::spawn_blocking(move || {
            create_zip("archive.create", &archive_path, &source, &names)
        })
        .await
        .map_err(|error| failed("archive.create", "action_failed", error.to_string()))??;
        Ok(ToolOutput::json(serde_json::json!({
            "archive": created.to_string_lossy(),
            "files": file_count,
        })))
    }
}

fn create_zip(
    tool: &str,
    archive_path: &Path,
    source: &Path,
    files: &[String],
) -> Result<PathBuf, ToolError> {
    if !archive_path
        .to_string_lossy()
        .to_ascii_lowercase()
        .ends_with(".zip")
    {
        return Err(invalid(tool, "archive.create only writes .zip archives"));
    }
    let canonical_source = source
        .canonicalize()
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    if let Some(parent) = archive_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        }
    }
    let file = std::fs::File::create(archive_path)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for name in files {
        // `safe_join` rejects escapes; canonical comparison rejects
        // symlinks pointing outside the source scope.
        let candidate = safe_join(tool, &canonical_source, name)?;
        let canonical = candidate
            .canonicalize()
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        if !canonical.starts_with(&canonical_source) {
            return Err(failed(
                tool,
                "archive_path_traversal",
                format!("input escapes the source directory: '{name}'"),
            ));
        }
        if !canonical.is_file() {
            return Err(invalid(tool, format!("not a file: '{name}'")));
        }
        let bytes = std::fs::read(&canonical)
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        writer
            .start_file(name.as_str(), options)
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
        writer
            .write_all(&bytes)
            .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    }
    writer
        .finish()
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    Ok(archive_path.to_path_buf())
}

/// Static archive tool group.
pub struct ArchiveToolPack {
    pub limits: ArchiveLimits,
}

impl ArchiveToolPack {
    pub fn new() -> Self {
        Self {
            limits: ArchiveLimits::default(),
        }
    }
}

impl Default for ArchiveToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl tool_sdk::ToolPack for ArchiveToolPack {
    fn id(&self) -> &'static str {
        "archive"
    }

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(ArchiveListTool {
                limits: self.limits.clone(),
            }),
            Arc::new(ArchiveExtractTool {
                limits: self.limits.clone(),
            }),
            Arc::new(ArchiveCreateTool {
                limits: self.limits.clone(),
            }),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    fn ctx() -> ToolContext {
        ToolContext::new(Principal::Agent(AgentId::new("test")))
    }

    fn ticket_for(capability: Capability, resource: Resource, ctx: &ToolContext) -> ToolContext {
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            capability,
            capability_core::ResourceScope::new(vec![resource]),
            ctx.invocation_id,
            std::time::Duration::from_secs(120),
        );
        ctx.clone().with_ticket(ticket)
    }

    fn write_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let file = std::fs::File::create(path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            writer.start_file(*name, options).unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    }

    #[test]
    fn traversal_and_absolute_paths_are_rejected() {
        let base = Path::new("/tmp/utsuwa-archive-test-base");
        assert!(safe_join("archive.extract", base, "../evil.txt").is_err());
        assert!(safe_join("archive.extract", base, "/etc/passwd").is_err());
        // Any parent component is rejected fail-closed, even when it would
        // resolve inside the base: `..` in archives is a traversal vector.
        assert!(safe_join("archive.extract", base, "sub/../ok.txt").is_err());
        assert!(safe_join("archive.extract", base, "sub/dir/file.txt")
            .unwrap()
            .starts_with(base));
    }

    #[tokio::test]
    async fn round_trip_zip_list_extract_create() {
        let dir = std::env::temp_dir().join(format!("utsuwa-archive-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("test.zip");
        write_zip(
            &archive_path,
            &[("hello.txt", b"hello"), ("sub/nested.txt", b"nested")],
        );

        let pack = ArchiveToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let list = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.list")
            .unwrap();
        let out = list
            .invoke(
                ticket_for(
                    Capability::FilesystemRead,
                    Resource::Path(archive_path.clone()),
                    &ctx(),
                ),
                serde_json::json!({"path": archive_path.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["entry_count"], 2);

        let extract = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.extract")
            .unwrap();
        let destination = dir.join("out");
        let mut extract_ctx = ctx();
        extract_ctx = ticket_for(
            Capability::FilesystemRead,
            Resource::Path(archive_path.clone()),
            &extract_ctx,
        );
        extract_ctx = ticket_for(
            Capability::FilesystemCreate,
            Resource::Path(destination.clone()),
            &extract_ctx,
        );
        extract_ctx = ticket_for(
            Capability::FilesystemWrite,
            Resource::Path(destination.clone()),
            &extract_ctx,
        );
        let out = extract
            .invoke(
                extract_ctx,
                serde_json::json!({
                    "archive": archive_path.to_string_lossy(),
                    "destination": destination.to_string_lossy(),
                }),
            )
            .await
            .unwrap();
        assert_eq!(out.content["extracted_files"], 2);
        assert_eq!(
            std::fs::read(destination.join("hello.txt")).unwrap(),
            b"hello"
        );

        // A malicious archive cannot escape the destination.
        let evil = dir.join("evil.zip");
        write_zip(&evil, &[("../../evil.txt", b"evil")]);
        let mut evil_ctx = ctx();
        evil_ctx = ticket_for(
            Capability::FilesystemRead,
            Resource::Path(evil.clone()),
            &evil_ctx,
        );
        evil_ctx = ticket_for(
            Capability::FilesystemCreate,
            Resource::Path(destination.clone()),
            &evil_ctx,
        );
        evil_ctx = ticket_for(
            Capability::FilesystemWrite,
            Resource::Path(destination.clone()),
            &evil_ctx,
        );
        let err = extract
            .invoke(
                evil_ctx,
                serde_json::json!({
                    "archive": evil.to_string_lossy(),
                    "destination": destination.to_string_lossy(),
                }),
            )
            .await
            .unwrap_err();
        assert_eq!(err.code(), Some("archive_path_traversal"), "{err:?}");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn unsupported_formats_are_rejected() {
        assert!(detect_kind(Path::new("archive.7z")).is_err());
        assert!(detect_kind(Path::new("backup.rar")).is_err());
    }

    /// Agent-preflight contract: `required_capabilities` must return every
    /// ticket `invoke` enforces, so the real agent loop can authorize the
    /// whole call before execution starts.
    #[test]
    fn extract_declares_read_create_and_write() {
        let pack = ArchiveToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let extract = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.extract")
            .unwrap();
        let args = serde_json::json!({"archive": "/work/a.zip", "destination": "/work/out"});
        let declared = extract.required_capabilities(&args);
        let kinds = declared
            .iter()
            .map(|requirement| {
                (
                    format!("{:?}", requirement.capability),
                    requirement.resource.clone(),
                )
            })
            .collect::<Vec<_>>();
        assert!(kinds.contains(&(
            "FilesystemRead".to_string(),
            Resource::Path(PathBuf::from("/work/a.zip"))
        )));
        assert!(kinds.contains(&(
            "FilesystemCreate".to_string(),
            Resource::Path(PathBuf::from("/work/out"))
        )));
        assert!(kinds.contains(&(
            "FilesystemWrite".to_string(),
            Resource::Path(PathBuf::from("/work/out"))
        )));
    }

    #[test]
    fn create_declares_source_read_and_output_create_write() {
        let pack = ArchiveToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let create = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.create")
            .unwrap();
        let args = serde_json::json!({
            "archive": "/work/out.zip",
            "source": "/work/src",
            "files": ["a.txt"],
        });
        let declared = create.required_capabilities(&args);
        assert_eq!(declared.len(), 3);
        assert!(declared.iter().any(|requirement| {
            requirement.capability == Capability::FilesystemRead
                && requirement.resource == Resource::Path(PathBuf::from("/work/src"))
        }));
    }

    #[tokio::test]
    async fn extract_missing_any_ticket_stops_before_execution() {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-archive-perm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("test.zip");
        write_zip(&archive_path, &[("hello.txt", b"hello")]);
        let destination = dir.join("out");

        let pack = ArchiveToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let extract = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.extract")
            .unwrap();
        let args = serde_json::json!({
            "archive": archive_path.to_string_lossy(),
            "destination": destination.to_string_lossy(),
        });
        // Each declared requirement denies on its own when missing: drop
        // exactly one ticket at a time from the full set.
        let full = extract.required_capabilities(&args);
        assert_eq!(full.len(), 3);
        for missing in 0..full.len() {
            let mut partial = ctx();
            for (index, requirement) in full.iter().enumerate() {
                if index == missing {
                    continue;
                }
                partial = ticket_for(
                    requirement.capability.clone(),
                    requirement.resource.clone(),
                    &partial,
                );
            }
            let err = extract.invoke(partial, args.clone()).await.unwrap_err();
            assert_eq!(
                err.code(),
                Some("permission_required"),
                "missing {missing}: {err:?}"
            );
            assert!(
                !destination.exists(),
                "no bytes may land without full authority"
            );
        }
        // Outside-scope tickets never authorize, even when all three
        // capabilities are present.
        let mut outside = ctx();
        for requirement in &full {
            let scoped = match &requirement.resource {
                Resource::Path(_) => Resource::Path(PathBuf::from("/elsewhere")),
                other => other.clone(),
            };
            outside = ticket_for(requirement.capability.clone(), scoped, &outside);
        }
        let err = extract.invoke(outside, args.clone()).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn create_missing_source_read_is_denied() {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-archive-create-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.txt"), b"data").unwrap();
        let pack = ArchiveToolPack::new();
        let tools = pack.tools(&tool_sdk::ToolLoadContext::default());
        let create = tools
            .iter()
            .find(|tool| tool.metadata().id.0 == "archive.create")
            .unwrap();
        let args = serde_json::json!({
            "archive": dir.join("out.zip").to_string_lossy(),
            "source": dir.join("src").to_string_lossy(),
            "files": ["a.txt"],
        });
        // Destination-only tickets must not authorize reading the source.
        let mut partial = ctx();
        partial = ticket_for(
            Capability::FilesystemCreate,
            Resource::Path(dir.clone()),
            &partial,
        );
        partial = ticket_for(
            Capability::FilesystemWrite,
            Resource::Path(dir.clone()),
            &partial,
        );
        let err = create.invoke(partial, args).await.unwrap_err();
        assert_eq!(err.code(), Some("permission_required"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn tar_symlink_entries_are_rejected() {
        let dir =
            std::env::temp_dir().join(format!("utsuwa-archive-symlink-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("link.tar");
        {
            let file = std::fs::File::create(&archive_path).unwrap();
            let mut builder = tar::Builder::new(file);
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(tar::EntryType::Symlink);
            header.set_size(0);
            header.set_cksum();
            builder
                .append_link(&mut header, "evil-link", "/etc/passwd")
                .unwrap();
            builder.finish().unwrap();
        }
        let destination = dir.join("out");
        let err = extract_archive(
            "archive.extract",
            &archive_path,
            &destination,
            &ArchiveLimits::default(),
        )
        .unwrap_err();
        assert_eq!(err.code(), Some("archive_path_traversal"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decompression_bomb_is_rejected_before_filling_disk() {
        // Output far above the budget must trip `archive_too_large`
        // instead of filling the disk.
        let dir =
            std::env::temp_dir().join(format!("utsuwa-archive-bomb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let archive_path = dir.join("bomb.zip");
        let zeros = vec![0u8; 256 * 1024];
        write_zip(&archive_path, &[("zeros.bin", zeros.as_slice())]);
        let destination = dir.join("out");
        let limits = ArchiveLimits {
            max_total_bytes: 64 * 1024,
            ..ArchiveLimits::default()
        };
        let err =
            extract_archive("archive.extract", &archive_path, &destination, &limits).unwrap_err();
        assert_eq!(err.code(), Some("archive_too_large"), "{err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
