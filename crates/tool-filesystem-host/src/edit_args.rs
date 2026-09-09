//! Shared edit-target parsing and retry machinery.

use crate::common::{
    failed, file_target_descriptor, filesystem_error, invalid_args, is_configured_directory_alias,
    normalize_filename, normalize_user_target_args, parse_directory_value, parse_location,
    retryable_target_args, tag_file_output, EDIT_TOOL, EDIT_USER_FILE_TOOL,
};
use file_target::{
    normalize_relative_path, FileRef, FileResolver, FileTarget, FilesystemErrorCode,
    ResolvedFileTarget, TargetPurpose,
};
use host_core::{HostEnvironment, UserDirectory};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use tool_core::{ToolError, ToolOutput};

/// One validated edit target inside an OS-configured user directory: the
/// semantic directory, its resolved host path, the relative filename, and
/// the exact absolute target the broker will mutate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResolvedEditTarget {
    pub(crate) directory: UserDirectory,
    pub(crate) resolved_directory: PathBuf,
    pub(crate) relative_path: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) file_ref: FileRef,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingEditState {
    pub(crate) target: ResolvedEditTarget,
    pub(crate) old_text: Option<String>,
    pub(crate) new_text: Option<String>,
}

pub(crate) struct MergedEditArgs {
    pub(crate) args: serde_json::Value,
    pub(crate) target: Option<ResolvedEditTarget>,
    pub(crate) explicit_target: bool,
    pub(crate) supplied_old_text: Option<String>,
    pub(crate) supplied_new_text: Option<String>,
}

pub(crate) fn required_string_field(
    object: &Map<String, Value>,
    tool: &str,
    field: &str,
) -> Result<String, ToolError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| invalid_args(tool, format!("missing string '{field}' argument")))
}

pub(crate) fn required_filename(
    object: &Map<String, Value>,
    tool: &str,
) -> Result<String, ToolError> {
    let filename = required_string_field(object, tool, "filename")?;
    if filename.trim().is_empty() {
        return Err(invalid_args(tool, "'filename' must name a file"));
    }
    Ok(filename)
}

/// If a location is supplied without a filename, infer it only when that
/// configured directory contains exactly one direct regular file. This keeps
/// malformed small-model calls useful without guessing among multiple files.
pub(crate) fn infer_single_edit_filename(base: &Path, tool: &str) -> Result<String, ToolError> {
    let entries = std::fs::read_dir(base).map_err(|error| {
        failed(
            tool,
            format!(
                "cannot inspect the host-configured edit directory '{}': {error}",
                base.display()
            ),
        )
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| failed(tool, error.to_string()))?;
        if entry
            .file_type()
            .map_err(|error| failed(tool, error.to_string()))?
            .is_file()
        {
            files.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    match files.as_slice() {
        [filename] => Ok(filename.clone()),
        [] => Err(invalid_args(
            tool,
            format!(
                "missing 'filename': no existing direct file was found in '{}'; call filesystem.list or pass filename explicitly",
                base.display()
            ),
        )),
        _ => Err(invalid_args(
            tool,
            format!(
                "missing 'filename': multiple existing files were found in '{}': {}; pass the requested filename explicitly",
                base.display(),
                files.iter().map(|file| format!("'{file}'")).collect::<Vec<_>>().join(", ")
            ),
        )),
    }
}

/// Resolve a user-directory edit to one exact absolute target without ever
/// asking the model to construct the host path. A missing location is
/// accepted only when the relative filename identifies exactly one existing
/// file among the host-configured user directories.
pub(crate) fn infer_edit_directory(
    filename: &str,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<UserDirectory, ToolError> {
    if Path::new(filename).is_absolute() {
        return Err(invalid_args(
            tool,
            "an absolute filename without 'location' is ambiguous; pass location+filename or use path",
        ));
    }

    let mut matches = Vec::new();
    for directory in UserDirectory::ALL {
        let Some(base) = environment.user_dirs.get(directory) else {
            continue;
        };
        let (_, path) = normalize_filename(filename, base, tool)?;
        if path.is_file() {
            matches.push(directory);
        }
    }
    match matches.as_slice() {
        [directory] => Ok(*directory),
        [] => Err(invalid_args(
            tool,
            format!(
                "could not resolve filename '{filename}' without a location; pass location='desktop' for a Desktop file or use an absolute path"
            ),
        )),
        _ => {
            let available = matches
                .iter()
                .map(|directory| format!("'{}'", directory.json_key()))
                .collect::<Vec<_>>()
                .join(", ");
            Err(invalid_args(
                tool,
                format!(
                    "filename '{filename}' exists in multiple configured user directories ({available}); pass the location explicitly"
                ),
            ))
        }
    }
}

/// Resolve `location` + `filename` to one exact absolute target without ever
/// asking the model to construct the host path. Unlike creation, editing
/// always names its file: no default filename is generated.
pub(crate) fn resolve_edit_target(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<ResolvedEditTarget, ToolError> {
    resolve_edit_target_with_purpose(args, environment, tool, TargetPurpose::Existing)
}

pub(crate) fn resolve_edit_target_with_purpose(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
    purpose: TargetPurpose,
) -> Result<ResolvedEditTarget, ToolError> {
    let args = normalize_user_target_args(args, environment, tool, purpose)?;
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let has_location = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory");
    if !has_location {
        if let (Some(path), Some(filename)) = (
            object.get("path").and_then(Value::as_str),
            object.get("filename").and_then(Value::as_str),
        ) {
            if Path::new(path).is_absolute() && !Path::new(filename).is_absolute() {
                if let Some(directory) = UserDirectory::ALL.into_iter().find(|directory| {
                    is_configured_directory_alias(Path::new(path), Some(*directory), environment)
                }) {
                    let relative_path = normalize_relative_path(Path::new(filename))
                        .map_err(|error| filesystem_error(tool, error))?;
                    let resolved = FileResolver::new(environment.clone())
                        .resolve_target(
                            &FileTarget {
                                directory,
                                relative_path: relative_path.clone(),
                            },
                            purpose,
                        )
                        .map_err(|error| filesystem_error(tool, error))?;
                    let resolved_directory = environment
                        .user_dirs
                        .get(directory)
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    return Ok(ResolvedEditTarget {
                        directory,
                        resolved_directory,
                        relative_path,
                        path: resolved.absolute_path,
                        file_ref: resolved.file_ref,
                    });
                }
                let normalized_path = normalize_special_user_path(
                    Path::new(path),
                    environment,
                    tool,
                    if matches!(purpose, TargetPurpose::Create) {
                        SpecialPathPurpose::Write
                    } else {
                        SpecialPathPurpose::ExistingFile
                    },
                    EDIT_USER_FILE_TOOL,
                )?;
                let resolved_path = FileResolver::new(environment.clone())
                    .descriptor_for_absolute(&normalized_path.path, purpose)
                    .map_err(|error| filesystem_error(tool, error))?;
                if let (Some(directory), Some(relative_path)) =
                    (resolved_path.directory, resolved_path.relative_path)
                {
                    let relative_filename = PathBuf::from(filename);
                    let filename_matches = relative_filename == relative_path
                        || relative_filename
                            .file_name()
                            .zip(relative_path.file_name())
                            .is_some_and(|(left, right)| left == right);
                    if !filename_matches {
                        return Err(retryable_target_args(
                            tool,
                            "path and filename identify different files",
                        ));
                    }
                    let resolved_directory = environment
                        .user_dirs
                        .get(directory)
                        .map(Path::to_path_buf)
                        .unwrap_or_default();
                    return Ok(ResolvedEditTarget {
                        directory,
                        resolved_directory,
                        relative_path,
                        path: resolved_path.absolute_path,
                        file_ref: resolved_path.file_ref,
                    });
                }
            }
        }
    }
    // Some models copy the complete displayed path into `filename` while
    // also supplying another path hint. Resolve that value as a target
    // instead of sending it through filename search, which rejects absolute
    // filenames without a semantic location.
    if !has_location {
        if let Some(filename) = object.get("filename").and_then(Value::as_str) {
            if Path::new(filename).is_absolute() {
                let resolved = FileResolver::new(environment.clone())
                    .descriptor_for_absolute(Path::new(filename), purpose)
                    .map_err(|error| filesystem_error(tool, error))?;
                let (Some(directory), Some(relative_path)) =
                    (resolved.directory, resolved.relative_path)
                else {
                    return Err(retryable_target_args(
                        tool,
                        "absolute filename is not inside a configured user directory",
                    ));
                };
                let resolved_directory = environment
                    .user_dirs
                    .get(directory)
                    .map(Path::to_path_buf)
                    .unwrap_or_default();
                return Ok(ResolvedEditTarget {
                    directory,
                    resolved_directory,
                    relative_path,
                    path: resolved.absolute_path,
                    file_ref: resolved.file_ref,
                });
            }
        }
    }
    let directory = if has_location {
        parse_location(&args, environment, tool)?
    } else {
        let filename = required_filename(object, tool)?;
        infer_edit_directory(&filename, environment, tool)?
    };
    let base = environment.user_dirs.get(directory).ok_or_else(|| {
        failed(
            tool,
            format!(
                "the host-configured {} directory is not available",
                directory.prompt_label()
            ),
        )
    })?;
    let filename = if object.contains_key("filename") {
        required_filename(object, tool)?
    } else if has_location {
        infer_single_edit_filename(base, tool)?
    } else {
        required_filename(object, tool)?
    };
    let relative_path = if Path::new(&filename).is_absolute() {
        normalize_filename(&filename, base, tool)?.0
    } else {
        PathBuf::from(&filename)
    };
    let target = FileTarget {
        directory,
        relative_path: relative_path.clone(),
    };
    let resolved = FileResolver::new(environment.clone())
        .resolve_target(&target, purpose)
        .map_err(|error| filesystem_error(tool, error))?;
    Ok(ResolvedEditTarget {
        directory,
        resolved_directory: base.to_path_buf(),
        relative_path,
        path: resolved.absolute_path,
        file_ref: resolved.file_ref,
    })
}

pub(crate) fn patch_args_for(path: &Path, old_text: &str, new_text: &str) -> serde_json::Value {
    serde_json::json!({
        "path": path.to_string_lossy(),
        "replacements": [{ "old": old_text, "new": new_text }],
    })
}

/// How strictly a stale conventional-path compatibility remap must treat
/// file existence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SpecialPathPurpose {
    /// Existing-file mutations (edit): remap only when the attempted path is
    /// missing and the configured candidate exists. An existing attempted
    /// path always wins (including the both-exist case); a missing candidate
    /// is a structured error instead of a guess.
    ExistingFile,
    /// Writes and appends (which may create): remap when the attempted
    /// conventional parent directory is missing and the configured directory
    /// exists. Never remap when the conventional parent exists, even if the
    /// configured directory exists too.
    Write,
}

pub(crate) struct NormalizedSpecialPath {
    pub path: PathBuf,
    /// Whether the attempted path was remapped to the configured location.
    /// Callers surface the original as `normalized_from` for transparency.
    pub mapped: bool,
    pub attempted: PathBuf,
}

pub(crate) fn unchanged_special_path(path: &Path) -> NormalizedSpecialPath {
    NormalizedSpecialPath {
        path: path.to_path_buf(),
        mapped: false,
        attempted: path.to_path_buf(),
    }
}

/// Record a stale-path remap in the tool output so the model (and the
/// receipt UI) can see which explicit path was normalized to the result.
pub(crate) fn tag_normalized_from(
    output: &mut ToolOutput,
    normalized: &NormalizedSpecialPath,
    environment: &HostEnvironment,
    purpose: TargetPurpose,
) {
    tag_file_output(output, &normalized.path, environment, purpose);
    if normalized.mapped {
        if let Some(object) = output.content.as_object_mut() {
            object.insert(
                "normalized_from".to_string(),
                Value::String(normalized.attempted.to_string_lossy().into_owned()),
            );
        }
    }
}

/// Map a stale conventional special-directory path (`$HOME/Desktop/...`)
/// to the OS-configured location (`$HOME/Escritorio/...`) using only
/// `HostEnvironment`/XDG data — never hardcoded translations.
///
/// Only a path directly below the conventional directory is eligible:
/// nested lookalikes (`$HOME/projects/Desktop/x`), other roots
/// (`/tmp/Desktop/x`), and relative paths are returned unchanged. The
/// mapping consults the live filesystem for evidence (see
/// [`SpecialPathPurpose`]) and runs before any capability ticket is minted,
/// so the ticket always scopes the final resolved path.
pub(crate) fn normalize_special_user_path(
    path: &Path,
    environment: &HostEnvironment,
    tool: &str,
    purpose: SpecialPathPurpose,
    next_tool: &str,
) -> Result<NormalizedSpecialPath, ToolError> {
    let Some(home) = environment.home.as_deref() else {
        return Ok(unchanged_special_path(path));
    };
    if !path.is_absolute() {
        return Ok(unchanged_special_path(path));
    }
    let (Some(parent), Some(file_name)) = (path.parent(), path.file_name()) else {
        return Ok(unchanged_special_path(path));
    };
    for directory in UserDirectory::ALL {
        let conventional = home.join(directory.conventional_name());
        if parent != conventional.as_path() {
            continue;
        }
        let Some(configured) = environment.user_dirs.get(directory) else {
            return Ok(unchanged_special_path(path));
        };
        if configured == conventional {
            // The conventional path is already the configured one.
            return Ok(unchanged_special_path(path));
        }
        let candidate = configured.join(file_name);
        match purpose {
            SpecialPathPurpose::ExistingFile => {
                if path.exists() {
                    // The explicit path works (or both exist): never guess.
                    return Ok(unchanged_special_path(path));
                }
                if candidate.exists() {
                    return Ok(NormalizedSpecialPath {
                        path: candidate,
                        mapped: true,
                        attempted: path.to_path_buf(),
                    });
                }
                return Err(failed(
                    tool,
                    format!(
                        "file_not_found: '{}' does not exist. The host-configured {} directory is '{}'; the same file name was not found there either (checked '{}'). Use {next_tool} with location='{}' after creating the file, or retry with the exact absolute path",
                        path.display(),
                        directory.prompt_label(),
                        configured.display(),
                        candidate.display(),
                        directory.json_key(),
                    ),
                ));
            }
            SpecialPathPurpose::Write => {
                if conventional.is_dir() {
                    // The explicit destination (or its parent) exists, or
                    // both directories exist: the explicit path wins.
                    return Ok(unchanged_special_path(path));
                }
                if configured.is_dir() {
                    return Ok(NormalizedSpecialPath {
                        path: candidate,
                        mapped: true,
                        attempted: path.to_path_buf(),
                    });
                }
                return Ok(unchanged_special_path(path));
            }
        }
    }
    Ok(unchanged_special_path(path))
}

/// Convert an exact-match argument failure into structured retry guidance.
/// A wrong text match must never read as missing editing capability, and must
/// never detour into creating another file.
pub(crate) fn with_read_retry_guidance(path: &Path, error: ToolError) -> ToolError {
    let ToolError::Failed { tool, message } = error else {
        return error;
    };
    // Couples to PatchTool's exact diagnostic below; an exact-match count
    // other than one is still a model-argument recovery problem, not a
    // native mutation failure.
    if !message.contains("replacement block occurs") {
        return ToolError::Failed { tool, message };
    }
    ToolError::RetryRequired {
        tool,
        message: "Edit needs more information: old_text did not match exactly once".to_string(),
        recovery: serde_json::json!({
            "error": "old_text_mismatch",
            "target": path.to_string_lossy(),
            "next_tool": "filesystem.read",
        }),
    }
}

pub(crate) fn path_only_args(path: &Path) -> serde_json::Value {
    serde_json::json!({ "path": path.to_string_lossy() })
}

/// Return the exact existing line only when the target is a UTF-8 file with
/// one non-empty line. The caller uses this internally for a safe edit; the
/// contents are never placed in a generic validation diagnostic.
pub(crate) fn single_line_edit_text(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut non_empty = content.lines().filter(|line| !line.trim().is_empty());
    let line = non_empty.next()?;
    if non_empty.next().is_some() {
        return None;
    }
    Some(line.to_string())
}

pub(crate) fn is_iso_date(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

/// A narrow multiline recovery for the common "update the date" shape.
/// This is deliberately limited to one unambiguous ISO date line; arbitrary
/// multiline replacements still require the model to read and provide the
/// exact old_text.
pub(crate) fn unique_iso_date_line(path: &Path, new_text: &str) -> Option<String> {
    if !is_iso_date(new_text) {
        return None;
    }
    let content = std::fs::read_to_string(path).ok()?;
    let mut matches = content
        .lines()
        .filter(|line| is_iso_date(line.trim()))
        .map(str::to_owned);
    let line = matches.next()?;
    matches.next().is_none().then_some(line)
}

pub(crate) fn is_date_placeholder(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "updated date" | "current date" | "today's date" | "today date"
    )
}

pub(crate) fn is_time_placeholder(text: &str) -> bool {
    matches!(
        text.trim().to_ascii_lowercase().as_str(),
        "current time" | "current hour" | "the current time" | "the current hour" | "now"
    )
}

pub(crate) fn current_local_date() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

pub(crate) fn parse_old_new(
    args: &serde_json::Value,
    tool: &str,
    resolved_path: &Path,
) -> Result<(String, String), ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let old_text = required_string_field(object, tool, "old_text").map_err(|_| {
        invalid_args(
            tool,
            format!(
                "missing 'old_text': read '{}' with filesystem.read, then retry with the exact text. Do not guess or use a placeholder such as 'Updated date'.",
                resolved_path.display()
            ),
        )
    })?;
    if old_text.is_empty() {
        return Err(invalid_args(
            tool,
            format!(
                "'old_text' must not be empty: call filesystem.read with path '{}' and copy one exact current line before retrying.",
                resolved_path.display()
            ),
        ));
    }
    let new_text = required_string_field(object, tool, "new_text").map_err(|_| {
        invalid_args(
            tool,
            format!(
                "missing 'new_text': retry filesystem.edit only after both old_text and new_text are present. For the current date, call system.time first and use its returned date; pass an empty string only when deleting the old text. Target: '{}'.",
                resolved_path.display()
            ),
        )
    })?;
    let new_text = if is_date_placeholder(&new_text) {
        current_local_date()
    } else {
        new_text
    };
    Ok((old_text, new_text))
}

pub(crate) fn optional_edit_text(
    object: &Map<String, Value>,
    tool: &str,
    field: &str,
) -> Result<Option<String>, ToolError> {
    let Some(value) = object.get(field) else {
        return Ok(None);
    };
    let text = value
        .as_str()
        .ok_or_else(|| invalid_args(tool, format!("'{field}' must be a string when provided")))?;
    Ok(Some(text.to_string()))
}

pub(crate) fn supplied_edit_texts(
    args: &serde_json::Value,
    tool: &str,
) -> Result<(Option<String>, Option<String>), ToolError> {
    let object = args
        .as_object()
        .ok_or_else(|| invalid_args(tool, "args must be a JSON object"))?;
    let old_text = optional_edit_text(object, tool, "old_text")?;
    if old_text.as_deref().is_some_and(str::is_empty) {
        return Err(invalid_args(tool, "'old_text' must not be empty"));
    }
    let new_text = optional_edit_text(object, tool, "new_text")?;
    Ok((old_text, new_text))
}

pub(crate) fn resolve_new_text_source(
    args: &serde_json::Value,
    tool: &str,
) -> Result<serde_json::Value, ToolError> {
    let Some(object) = args.as_object() else {
        return Ok(args.clone());
    };
    let new_text = if let Some(source) = object.get("new_text_source") {
        let source = source.as_str().ok_or_else(|| {
            invalid_args(tool, "'new_text_source' must be a string when provided")
        })?;
        let local = chrono::Local::now();
        match source {
            "current_date" => local.format("%Y-%m-%d").to_string(),
            "current_time" => local.format("%H:%M:%S").to_string(),
            "current_datetime" => local.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            _ => {
                return Err(invalid_args(
                    tool,
                    "'new_text_source' must be one of: current_date, current_time, current_datetime",
                ));
            }
        }
    } else if let Some(new_text) = object.get("new_text").and_then(Value::as_str) {
        if is_date_placeholder(new_text) {
            current_local_date()
        } else if is_time_placeholder(new_text) {
            chrono::Local::now().format("%H:%M:%S").to_string()
        } else {
            return Ok(args.clone());
        }
    } else {
        return Ok(args.clone());
    };
    let mut normalized = object.clone();
    normalized.remove("new_text_source");
    normalized.insert("new_text".to_string(), Value::String(new_text));
    Ok(Value::Object(normalized))
}

pub(crate) fn looks_like_clock_text(value: &str) -> bool {
    let value = value.trim();
    let value = value
        .strip_prefix("Current hour:")
        .or_else(|| value.strip_prefix("current hour:"))
        .unwrap_or(value)
        .trim();
    let parts = value.split(':').collect::<Vec<_>>();
    (parts.len() == 2 || parts.len() == 3)
        && parts
            .iter()
            .all(|part| part.len() == 2 && part.as_bytes().iter().all(u8::is_ascii_digit))
        && parts[0].parse::<u8>().is_ok_and(|hour| hour < 24)
        && parts[1].parse::<u8>().is_ok_and(|minute| minute < 60)
        && (parts.len() == 2 || parts[2].parse::<u8>().is_ok_and(|second| second < 60))
}

/// Small models sometimes use the replace-shaped edit call for "add the
/// current hour" and omit old_text. That is deterministic only for a native
/// clock value; arbitrary text remains a normal exact replacement request.
pub(crate) fn normalize_clock_replacement_to_append(args: &serde_json::Value) -> serde_json::Value {
    let Some(object) = args.as_object() else {
        return args.clone();
    };
    if object.contains_key("old_text") || edit_operation_kind(args) == EditOperationKind::Append {
        return args.clone();
    }
    let has_current_time_source = object
        .get("new_text_source")
        .and_then(Value::as_str)
        .is_some_and(|source| source == "current_time");
    let is_clock_text = object
        .get("new_text")
        .and_then(Value::as_str)
        .is_some_and(looks_like_clock_text);
    if !has_current_time_source && !is_clock_text {
        return args.clone();
    }
    let Some(new_text) = object.get("new_text").and_then(Value::as_str) else {
        return args.clone();
    };
    let mut normalized = object.clone();
    normalized.remove("old_text");
    normalized.remove("new_text");
    normalized.remove("new_text_source");
    normalized.insert(
        "operation_type".to_string(),
        Value::String("append".to_string()),
    );
    normalized.insert("content".to_string(), Value::String(new_text.to_string()));
    normalized.insert("_auto_next_line".to_string(), Value::Bool(true));
    Value::Object(normalized)
}

pub(crate) fn next_line_append_content(path: &Path, content: String) -> String {
    if content.is_empty() {
        return content;
    }
    let needs_newline = std::fs::read(path)
        .map(|current| !current.is_empty() && !current.ends_with(b"\n"))
        .unwrap_or(false);
    if needs_newline {
        format!("\n{content}")
    } else {
        content
    }
}

/// Validate legacy `location`/`filename` metadata without making it the
/// source of truth when a canonical `file_ref` or structured target is
/// already present. In particular, `file_ref + location` is a valid small
/// model follow-up even when `filename` is omitted.
pub(crate) fn validate_legacy_edit_hint(
    object: &Map<String, Value>,
    canonical: &ResolvedFileTarget,
    environment: &HostEnvironment,
    tool: &str,
    purpose: TargetPurpose,
) -> Result<(), ToolError> {
    let has_location = object.contains_key("location")
        || object.contains_key("directory_id")
        || object.contains_key("directory");
    let supplied_directory = if has_location {
        Some(parse_location(
            &Value::Object(object.clone()),
            environment,
            tool,
        )?)
    } else {
        None
    };

    if let Some(directory) = supplied_directory {
        let directory_matches = canonical.directory == Some(directory)
            || (canonical.directory.is_none()
                && environment
                    .user_dirs
                    .get(directory)
                    .and_then(|root| root.canonicalize().ok())
                    .is_some_and(|root| canonical.absolute_path.starts_with(root)));
        if !directory_matches {
            return Err(retryable_target_args(
                tool,
                "location does not identify the same file as the canonical target",
            ));
        }
    }

    let Some(filename) = object.get("filename") else {
        return Ok(());
    };
    let filename = filename
        .as_str()
        .ok_or_else(|| invalid_args(tool, "'filename' must be a string when provided"))?;
    if filename.trim().is_empty() {
        return Err(invalid_args(tool, "'filename' must name a file"));
    }

    let candidate = if Path::new(filename).is_absolute() {
        FileResolver::new(environment.clone())
            .descriptor_for_absolute(Path::new(filename), purpose)
            .map_err(|error| filesystem_error(tool, error))?
            .absolute_path
    } else if let Some(directory) = canonical.directory {
        FileResolver::new(environment.clone())
            .resolve_target(
                &FileTarget {
                    directory,
                    relative_path: PathBuf::from(filename),
                },
                purpose,
            )
            .map_err(|error| filesystem_error(tool, error))?
            .absolute_path
    } else if filename
        == canonical
            .absolute_path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
    {
        // For an arbitrary explicit path, a bare filename can only be a
        // harmless display-name hint when it matches the final component.
        canonical.absolute_path.clone()
    } else {
        return Err(retryable_target_args(
            tool,
            "filename does not identify the same file as the canonical target",
        ));
    };

    if candidate != canonical.absolute_path {
        return Err(retryable_target_args(
            tool,
            "filename does not identify the same file as the canonical target",
        ));
    }
    Ok(())
}

/// Repair the common model mistake of copying a displayed file path into a
/// directory selector. The repair is performed only when the host can prove
/// the path is an existing file inside one configured user directory.
pub(crate) fn normalize_legacy_directory_shape(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<serde_json::Value, ToolError> {
    let Some(object) = args.as_object() else {
        return Ok(args.clone());
    };
    let mut normalized = object.clone();
    let selector = ["location", "directory_id", "directory", "user_directory"]
        .into_iter()
        .find(|field| normalized.contains_key(*field));
    let Some(selector) = selector else {
        return Ok(args.clone());
    };
    let Some(value) = normalized
        .get(selector)
        .and_then(Value::as_str)
        .map(str::to_owned)
    else {
        return Ok(args.clone());
    };
    if selector == "user_directory" {
        normalized.remove(selector);
        normalized.insert("location".to_string(), Value::String(value.clone()));
    }
    if !Path::new(&value).is_absolute() {
        return Ok(Value::Object(normalized));
    }
    let resolver = FileResolver::new(environment.clone());
    let resolved =
        match resolver.normalize_absolute_path(Path::new(&value), TargetPurpose::Existing) {
            Ok(Some(resolved)) => resolved,
            Ok(None) => return Ok(Value::Object(normalized)),
            Err(error)
                if matches!(
                    error.code,
                    FilesystemErrorCode::FileNotFound
                        | FilesystemErrorCode::DirectoryNotFound
                        | FilesystemErrorCode::TargetIsDirectory
                ) =>
            {
                return Ok(Value::Object(normalized));
            }
            Err(error) => return Err(filesystem_error(tool, error)),
        };
    let Some(directory) = resolved.directory else {
        return Ok(Value::Object(normalized));
    };
    let Some(relative_path) = resolved.relative_path else {
        return Ok(Value::Object(normalized));
    };
    for field in ["location", "directory_id", "directory", "user_directory"] {
        normalized.remove(field);
    }
    normalized.insert(
        "location".to_string(),
        Value::String(directory.json_key().to_string()),
    );
    normalized.insert(
        "filename".to_string(),
        Value::String(relative_path.to_string_lossy().into_owned()),
    );
    if normalized
        .get("path")
        .and_then(Value::as_str)
        .is_some_and(|path| path == relative_path.to_string_lossy())
    {
        normalized.remove("path");
    }
    Ok(Value::Object(normalized))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EditOperationKind {
    Replace,
    Append,
}

pub(crate) fn normalize_structured_edit_args(
    args: &serde_json::Value,
    environment: &HostEnvironment,
    tool: &str,
) -> Result<serde_json::Value, ToolError> {
    let args = normalize_legacy_directory_shape(args, environment, tool)?;
    let Some(object) = args.as_object() else {
        return Ok(args.clone());
    };
    let mut normalized = object.clone();
    let has_file_ref = normalized.contains_key("file_ref");
    let has_target = normalized.contains_key("target");

    let operation = normalized.get("operation").cloned();
    if operation.is_some() && operation.as_ref().and_then(Value::as_object).is_none() {
        return Err(invalid_args(tool, "'operation' must be an object"));
    }
    let operation_kind = operation
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|operation| operation.get("type"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            if operation.is_some() {
                String::new()
            } else {
                "replace".to_string()
            }
        });
    if !matches!(operation_kind.as_str(), "replace" | "append") {
        return Err(invalid_args(
            tool,
            "operation.type must be one of: replace, append",
        ));
    }
    let target_purpose = if operation_kind == "append" {
        TargetPurpose::Create
    } else {
        TargetPurpose::Existing
    };

    // Resolve all target selectors before collapsing them. Follow-up calls
    // from small models often contain a stable file_ref plus copied target
    // metadata (or legacy location/filename). Equivalent selectors are
    // harmless; different selectors remain a retryable model-argument error.
    let resolver = FileResolver::new(environment.clone());
    let resolved_file_ref = if has_file_ref {
        let raw = normalized
            .get("file_ref")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_args(tool, "'file_ref' must be a string"))?;
        let file_ref = FileRef::parse(raw).map_err(|error| filesystem_error(tool, error))?;
        Some(
            resolver
                .resolve_ref(&file_ref, target_purpose)
                .map_err(|error| filesystem_error(tool, error))?,
        )
    } else {
        None
    };
    let resolved_target = if has_target {
        let target = normalized
            .get("target")
            .and_then(Value::as_object)
            .ok_or_else(|| invalid_args(tool, "'target' must be an object"))?;
        let directory = target
            .get("directory")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                invalid_args(tool, "target.directory must be a semantic directory id")
            })?;
        let directory = parse_directory_value(directory, environment, tool)?;
        let relative_path = target
            .get("relative_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .ok_or_else(|| invalid_args(tool, "target.relative_path must be a string"))?;
        Some(
            resolver
                .resolve_target(
                    &FileTarget {
                        directory,
                        relative_path,
                    },
                    target_purpose,
                )
                .map_err(|error| filesystem_error(tool, error))?,
        )
    } else {
        None
    };
    let has_legacy_target = normalized.contains_key("location")
        || normalized.contains_key("directory_id")
        || normalized.contains_key("directory")
        || normalized.contains_key("filename");
    let canonical_selector = resolved_file_ref.as_ref().or(resolved_target.as_ref());
    if let Some(canonical_selector) = canonical_selector {
        // A follow-up commonly contains a stable file_ref plus only copied
        // directory metadata. Do not force that incomplete metadata through
        // filename inference: the stable selector already identifies the
        // exact file. Validate any supplied metadata against it, then drop
        // the redundant fields below.
        validate_legacy_edit_hint(
            &normalized,
            canonical_selector,
            environment,
            tool,
            target_purpose,
        )?;
    }
    let resolved_legacy_target = if has_legacy_target && canonical_selector.is_none() {
        Some(resolve_edit_target_with_purpose(
            &Value::Object(normalized.clone()),
            environment,
            tool,
            target_purpose,
        )?)
    } else {
        None
    };
    let mut selector_paths = Vec::new();
    if let Some(resolved) = &resolved_file_ref {
        selector_paths.push(("file_ref", resolved.absolute_path.clone()));
    }
    if let Some(resolved) = &resolved_target {
        selector_paths.push(("target", resolved.absolute_path.clone()));
    }
    if let Some(resolved) = &resolved_legacy_target {
        selector_paths.push(("location+filename", resolved.path.clone()));
    }
    if let Some((_, canonical_path)) = selector_paths.first() {
        if selector_paths
            .iter()
            .any(|(_, path)| path != canonical_path)
        {
            tracing::debug!(
                tool = %tool,
                selectors = ?selector_paths
                    .iter()
                    .map(|(kind, path)| (*kind, path.display().to_string()))
                    .collect::<Vec<_>>(),
                "filesystem edit selectors resolved to different paths"
            );
            return Err(retryable_target_args(
                tool,
                "target references identify different files; provide one target or equivalent selectors",
            ));
        }
    }
    normalized.insert(
        "operation_type".to_string(),
        Value::String(operation_kind.clone()),
    );
    if let Some(operation) = operation.and_then(|value| value.as_object().cloned()) {
        match operation_kind.as_str() {
            "replace" => {
                for field in ["old_text", "new_text", "new_text_source"] {
                    if let Some(value) = operation.get(field) {
                        normalized.insert(field.to_string(), value.clone());
                    }
                }
            }
            "append" => {
                let text = operation
                    .get("text")
                    .or_else(|| operation.get("content"))
                    .cloned()
                    .ok_or_else(|| invalid_args(tool, "append operation requires text"))?;
                normalized.insert("content".to_string(), text);
            }
            _ => unreachable!("operation kind validated above"),
        }
    }
    normalized.remove("operation");

    normalized.remove("file_ref");
    normalized.remove("target");
    if let Some(resolved) = resolved_file_ref {
        if let (Some(directory), Some(relative_path)) = (resolved.directory, resolved.relative_path)
        {
            normalized.insert(
                "location".to_string(),
                Value::String(directory.json_key().to_string()),
            );
            normalized.insert(
                "filename".to_string(),
                Value::String(relative_path.to_string_lossy().into_owned()),
            );
        } else {
            normalized.insert(
                "path".to_string(),
                Value::String(resolved.absolute_path.to_string_lossy().into_owned()),
            );
        }
    } else if let Some(resolved) = resolved_target {
        normalized.insert(
            "location".to_string(),
            Value::String(
                resolved
                    .directory
                    .expect("semantic target has a directory")
                    .json_key()
                    .to_string(),
            ),
        );
        normalized.insert(
            "filename".to_string(),
            Value::String(
                resolved
                    .relative_path
                    .expect("semantic target has a relative path")
                    .to_string_lossy()
                    .into_owned(),
            ),
        );
    }
    Ok(Value::Object(normalized))
}

pub(crate) fn edit_operation_kind(args: &serde_json::Value) -> EditOperationKind {
    match args
        .get("operation_type")
        .and_then(Value::as_str)
        .unwrap_or("replace")
    {
        "append" => EditOperationKind::Append,
        _ => EditOperationKind::Replace,
    }
}

pub(crate) fn missing_edit_argument(
    path: &Path,
    missing: Vec<&str>,
    preserved: Vec<&str>,
) -> ToolError {
    let mut recovery = serde_json::Map::new();
    recovery.insert(
        "error".to_string(),
        Value::String(if missing.len() == 1 && missing[0] == "old_text" {
            "old_text_required".to_string()
        } else {
            "missing_edit_argument".to_string()
        }),
    );
    recovery.insert(
        "missing".to_string(),
        Value::Array(
            missing
                .iter()
                .map(|field| Value::String((*field).to_string()))
                .collect(),
        ),
    );
    recovery.insert(
        "target".to_string(),
        Value::String(path.to_string_lossy().into_owned()),
    );
    recovery.insert(
        "preserved".to_string(),
        Value::Array(
            preserved
                .iter()
                .map(|field| Value::String((*field).to_string()))
                .collect(),
        ),
    );
    if missing.contains(&"old_text") {
        recovery.insert(
            "next_tool".to_string(),
            Value::String("filesystem.read".to_string()),
        );
    }
    let message = if missing.len() == 1 && missing[0] == "old_text" {
        "Edit needs more information: provide old_text or read the target file"
    } else if missing.len() == 1 && missing[0] == "new_text" {
        "Edit needs more information: provide new_text"
    } else {
        "Edit needs more information: provide old_text and new_text"
    };
    ToolError::RetryRequired {
        tool: EDIT_TOOL.to_string(),
        message: message.to_string(),
        recovery: Value::Object(recovery),
    }
}

/// Validate the unified edit arguments after pending values have been merged.
/// A single-line target is safe to infer; arbitrary multi-line files are not.
pub(crate) fn parse_unified_old_new(
    args: &serde_json::Value,
    resolved_path: &Path,
) -> Result<(String, String), ToolError> {
    let (mut old_text, new_text) = supplied_edit_texts(args, EDIT_TOOL)?;
    if old_text.is_none() && new_text.is_some() {
        old_text = single_line_edit_text(resolved_path).or_else(|| {
            unique_iso_date_line(resolved_path, new_text.as_deref().unwrap_or_default())
        });
    }
    let missing_old = old_text.is_none();
    let missing_new = new_text.is_none();
    if missing_old || missing_new {
        let mut missing = Vec::new();
        if missing_old {
            missing.push("old_text");
        }
        if missing_new {
            missing.push("new_text");
        }
        let mut preserved = Vec::new();
        if old_text.is_some() {
            preserved.push("old_text");
        }
        if new_text.is_some() {
            preserved.push("new_text");
        }
        return Err(missing_edit_argument(resolved_path, missing, preserved));
    }
    Ok((old_text.unwrap_or_default(), new_text.unwrap_or_default()))
}

/// Log only the shape of an edit call. Text values are intentionally omitted:
/// they may contain private document contents or secrets.
pub(crate) fn log_edit_argument_shape(args: &serde_json::Value) {
    let Some(object) = args.as_object() else {
        tracing::debug!(
            tool = EDIT_TOOL,
            "filesystem edit received non-object arguments"
        );
        return;
    };
    let operation = object
        .get("operation")
        .and_then(Value::as_object)
        .and_then(|operation| operation.get("type"))
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            object
                .get("operation_type")
                .and_then(Value::as_str)
                .unwrap_or("replace")
        });
    tracing::debug!(
        tool = EDIT_TOOL,
        operation,
        has_file_ref = object.contains_key("file_ref"),
        has_target = object.contains_key("target"),
        has_path = object.contains_key("path"),
        has_location = object.contains_key("location")
            || object.contains_key("directory_id")
            || object.contains_key("directory"),
        has_filename = object.contains_key("filename"),
        has_old_text = object.contains_key("old_text"),
        has_new_text = object.contains_key("new_text"),
        has_new_text_source = object.contains_key("new_text_source"),
        "filesystem edit argument shape received"
    );
}

pub(crate) fn tag_user_file_output(
    output: &mut ToolOutput,
    directory: UserDirectory,
    resolved_directory: &Path,
    relative_path: &Path,
    environment: &HostEnvironment,
) {
    let target = FileTarget {
        directory,
        relative_path: relative_path.to_path_buf(),
    };
    let resolved_target = FileResolver::new(environment.clone())
        .resolve_target(&target, TargetPurpose::Existing)
        .ok();
    if let Some(object) = output.content.as_object_mut() {
        object.insert(
            "location".to_string(),
            Value::String(directory.json_key().to_string()),
        );
        object.insert(
            "directory_id".to_string(),
            Value::String(directory.json_key().to_string()),
        );
        object.insert(
            "directory".to_string(),
            Value::String(directory.json_key().to_string()),
        );
        object.insert(
            "resolved_directory".to_string(),
            Value::String(resolved_directory.to_string_lossy().into_owned()),
        );
        object.insert(
            "filename".to_string(),
            Value::String(relative_path.to_string_lossy().into_owned()),
        );
        object.insert(
            "relative_path".to_string(),
            Value::String(relative_path.to_string_lossy().into_owned()),
        );
        if let Some(resolved_target) = resolved_target {
            object.insert(
                "file_ref".to_string(),
                Value::String(resolved_target.file_ref.to_string()),
            );
            object.insert(
                "display_path".to_string(),
                Value::String(resolved_target.display_path.clone()),
            );
            object.insert("file".to_string(), file_target_descriptor(&resolved_target));
        }
    }
}
