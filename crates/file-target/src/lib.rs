//! Canonical, host-owned file identity and target resolution.
//!
//! Models may name a file with a semantic user directory and relative path,
//! or reuse a `file_ref` returned by an earlier successful operation.  This
//! module deliberately keeps path normalization and reference parsing next to
//! the host environment instead of letting individual tools grow their own
//! variants.

use host_core::{HostEnvironment, UserDirectory};
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::{Component, Path, PathBuf};

/// Canonical model-facing target. `relative_path` is always relative to the
/// selected host-resolved user directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileTarget {
    pub directory: UserDirectory,
    pub relative_path: PathBuf,
}

/// Stable session-facing file identity. The semantic form is intentionally
/// readable for diagnostics (`file:desktop:note.txt`), but callers must still
/// parse and validate it through [`FileResolver`] before use.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FileRef(String);

impl FileRef {
    pub fn from_target(target: &FileTarget) -> Result<Self, FileTargetError> {
        let relative = normalize_relative_path(&target.relative_path)?;
        let relative = slash_path(&relative);
        Ok(Self(format!(
            "file:{}:{relative}",
            target.directory.json_key()
        )))
    }

    /// Compatibility identity for an explicit path that is not inside a
    /// configured semantic user directory. It is not a capability: the
    /// broker must re-authorize the decoded path on every use.
    pub fn from_absolute_path(path: &Path) -> Result<Self, FileTargetError> {
        if !path.is_absolute() {
            return Err(FileTargetError::new(
                FilesystemErrorCode::InvalidRelativePath,
                "file reference paths must be absolute",
                Some(path.to_string_lossy().into_owned()),
                false,
            ));
        }
        Ok(Self(format!("file:path:{}", path.to_string_lossy())))
    }

    /// Stable identity for a configured user directory itself, used to
    /// list it. The trailing empty segment is intentional: `semantic_parts`
    /// splits `desktop:` into `("desktop", "")`, which
    /// [`FileResolver::resolve_target`] reads as the directory root.
    pub fn from_directory(directory: UserDirectory) -> Self {
        Self(format!("file:{}:", directory.json_key()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn parse(value: &str) -> Result<Self, FileTargetError> {
        if value.starts_with("file:") && value.len() > "file:".len() {
            Ok(Self(value.to_string()))
        } else {
            Err(FileTargetError::new(
                FilesystemErrorCode::InvalidFileRef,
                "file_ref must use the file: prefix",
                Some(value.to_string()),
                true,
            ))
        }
    }

    fn semantic_parts(&self) -> Result<(UserDirectory, PathBuf), FileTargetError> {
        let value = self
            .0
            .strip_prefix("file:")
            .ok_or_else(|| invalid_ref(self.as_str()))?;
        let Some((directory, relative)) = value.split_once(':') else {
            return Err(invalid_ref(self.as_str()));
        };
        let directory =
            UserDirectory::from_json_key(directory).ok_or_else(|| invalid_ref(self.as_str()))?;
        // An empty segment names the directory itself (`file:desktop:`);
        // `resolve_target` reads it as the root, file operations keep
        // rejecting it downstream.
        if relative.is_empty() {
            return Ok((directory, PathBuf::new()));
        }
        let relative = PathBuf::from(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
        let relative = normalize_relative_path(&relative)?;
        Ok((directory, relative))
    }

    fn absolute_path(&self) -> Result<PathBuf, FileTargetError> {
        let value = self
            .0
            .strip_prefix("file:path:")
            .ok_or_else(|| invalid_ref(self.as_str()))?;
        let path = PathBuf::from(value);
        if !path.is_absolute() {
            return Err(invalid_ref(self.as_str()));
        }
        Ok(path)
    }

    pub fn is_semantic(&self) -> bool {
        self.0
            .strip_prefix("file:")
            .is_some_and(|value| !value.starts_with("path:"))
    }
}

impl std::fmt::Display for FileRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// A resolved target returned by the host-owned resolver. The absolute path
/// is for the native broker; model-facing output should prefer `file_ref` and
/// `display_path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFileTarget {
    pub file_ref: FileRef,
    pub directory: Option<UserDirectory>,
    pub relative_path: Option<PathBuf>,
    pub absolute_path: PathBuf,
    pub display_path: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetPurpose {
    Existing,
    Create,
}

/// Stable machine-readable filesystem failures produced by target
/// normalization. Human text is deliberately separate from this code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilesystemErrorCode {
    InvalidDirectory,
    DirectoryUnavailable,
    InvalidRelativePath,
    OutsideAllowedDirectory,
    PathTraversal,
    SymlinkEscape,
    FileNotFound,
    DirectoryNotFound,
    PermissionDenied,
    TargetIsDirectory,
    TargetIsFile,
    InvalidFileRef,
    StaleFileRef,
    Conflict,
    InvalidEdit,
    AmbiguousTarget,
    IoFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileTargetError {
    pub code: FilesystemErrorCode,
    pub message: String,
    pub received: Option<String>,
    pub retryable: bool,
    pub suggested_target: Option<FileTarget>,
}

impl FileTargetError {
    fn new(
        code: FilesystemErrorCode,
        message: impl Into<String>,
        received: Option<String>,
        retryable: bool,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            received,
            retryable,
            suggested_target: None,
        }
    }

    pub fn with_suggested_target(mut self, target: FileTarget) -> Self {
        self.suggested_target = Some(target);
        self
    }
}

fn invalid_ref(value: &str) -> FileTargetError {
    FileTargetError::new(
        FilesystemErrorCode::InvalidFileRef,
        "file_ref is malformed or unsafe",
        Some(value.to_string()),
        true,
    )
}

fn slash_path(path: &Path) -> String {
    path.to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

pub fn validate_relative_path(path: &Path) -> Result<(), FileTargetError> {
    if path.as_os_str().is_empty()
        || path.file_name().is_none()
        || path.to_string_lossy().contains('\0')
    {
        return Err(FileTargetError::new(
            FilesystemErrorCode::InvalidRelativePath,
            "relative_path must name a file",
            Some(path.to_string_lossy().into_owned()),
            true,
        ));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::Prefix(_) | Component::RootDir | Component::ParentDir
        ) {
            return Err(FileTargetError::new(
                FilesystemErrorCode::PathTraversal,
                "relative_path must not contain absolute or parent-directory components",
                Some(path.to_string_lossy().into_owned()),
                false,
            ));
        }
    }
    Ok(())
}

/// True when a relative path carries no file name (empty, `.`, `/`),
/// i.e. the caller means the directory root rather than a file inside it.
/// [`normalize_relative_path`] still rejects these for file operations;
/// [`FileResolver::resolve_target`] accepts them for reads.
fn is_directory_root_request(path: &Path) -> bool {
    path.components().all(|component| {
        matches!(
            component,
            Component::CurDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

pub fn normalize_relative_path(path: &Path) -> Result<PathBuf, FileTargetError> {
    validate_relative_path(path)?;
    let mut normalized = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(component) = component {
            normalized.push(component);
        }
    }
    if normalized.as_os_str().is_empty() {
        return Err(FileTargetError::new(
            FilesystemErrorCode::InvalidRelativePath,
            "relative_path must name a file",
            Some(path.to_string_lossy().into_owned()),
            true,
        ));
    }
    Ok(normalized)
}

/// One canonical resolver for semantic targets, references, and compatible
/// absolute paths. It never grants access; the native capability broker still
/// authorizes the final absolute path for every operation.
#[derive(Debug, Clone)]
pub struct FileResolver {
    environment: HostEnvironment,
}

impl FileResolver {
    pub fn new(environment: HostEnvironment) -> Self {
        Self { environment }
    }

    pub fn environment(&self) -> &HostEnvironment {
        &self.environment
    }

    pub fn resolve_target(
        &self,
        target: &FileTarget,
        purpose: TargetPurpose,
    ) -> Result<ResolvedFileTarget, FileTargetError> {
        let relative_path = match normalize_relative_path(&target.relative_path) {
            Ok(relative_path) => Some(relative_path),
            Err(error) if is_directory_root_request(&target.relative_path) => {
                // `{"directory": "desktop", "relative_path": ""}` (or ".")
                // names the directory itself for listings. Only reads accept
                // this: creation keeps the strict error so a missing filename
                // can never resolve to its parent directory.
                if !matches!(purpose, TargetPurpose::Existing) {
                    return Err(error);
                }
                None
            }
            Err(error) => return Err(error),
        };
        let root = self
            .environment
            .user_dirs
            .get(target.directory)
            .ok_or_else(|| {
                FileTargetError::new(
                    FilesystemErrorCode::DirectoryUnavailable,
                    format!(
                        "{} directory is not available on this host",
                        target.directory.json_key()
                    ),
                    Some(target.directory.json_key().to_string()),
                    true,
                )
            })?;
        let root = canonical_root(root)?;
        let Some(relative_path) = relative_path else {
            if !root.is_dir() {
                return Err(FileTargetError::new(
                    FilesystemErrorCode::DirectoryUnavailable,
                    format!(
                        "{} directory is not available on this host",
                        target.directory.json_key()
                    ),
                    Some(target.directory.json_key().to_string()),
                    true,
                ));
            }
            return Ok(ResolvedFileTarget {
                file_ref: FileRef::from_directory(target.directory),
                directory: Some(target.directory),
                relative_path: None,
                display_path: root.to_string_lossy().into_owned(),
                absolute_path: root,
            });
        };
        let lexical = root.join(&relative_path);
        let absolute = match validate_target(&root, &lexical, purpose) {
            Ok(absolute) => absolute,
            Err(error) if error.code == FilesystemErrorCode::FileNotFound => {
                return Err(error.with_suggested_target(FileTarget {
                    directory: target.directory,
                    relative_path: relative_path.clone(),
                }));
            }
            Err(error) => return Err(error),
        };
        let file_ref = FileRef::from_target(&FileTarget {
            directory: target.directory,
            relative_path: relative_path.clone(),
        })?;
        Ok(ResolvedFileTarget {
            file_ref,
            directory: Some(target.directory),
            relative_path: Some(relative_path),
            display_path: absolute.to_string_lossy().into_owned(),
            absolute_path: absolute,
        })
    }

    pub fn resolve_ref(
        &self,
        file_ref: &FileRef,
        purpose: TargetPurpose,
    ) -> Result<ResolvedFileTarget, FileTargetError> {
        if file_ref.is_semantic() {
            let (directory, relative_path) = file_ref.semantic_parts()?;
            return self
                .resolve_target(
                    &FileTarget {
                        directory,
                        relative_path,
                    },
                    purpose,
                )
                .map_err(|mut error| {
                    if matches!(error.code, FilesystemErrorCode::FileNotFound) {
                        error.code = FilesystemErrorCode::StaleFileRef;
                        error.message =
                            "file_ref no longer resolves to an existing file".to_string();
                        error.retryable = false;
                    }
                    error
                });
        }
        let path = file_ref.absolute_path()?;
        let absolute = if matches!(purpose, TargetPurpose::Existing) {
            match canonical_existing(&path) {
                Ok(absolute) => absolute,
                // A compatibility reference to a directory (issued by a
                // listing tag) resolves to a directory descriptor instead
                // of going stale: the broker still authorizes the path,
                // and file tools reject directories at their own layer.
                Err(error) if error.code == FilesystemErrorCode::TargetIsDirectory => {
                    return self.descriptor_for_directory(&path);
                }
                Err(mut error) => {
                    error.code = FilesystemErrorCode::StaleFileRef;
                    error.message = "file_ref no longer resolves to an existing file".to_string();
                    error.retryable = false;
                    return Err(error);
                }
            }
        } else {
            canonical_creation(&path)?
        };
        Ok(ResolvedFileTarget {
            file_ref: file_ref.clone(),
            directory: None,
            relative_path: None,
            display_path: absolute.to_string_lossy().into_owned(),
            absolute_path: absolute,
        })
    }

    /// Normalize an absolute path into a semantic target only when its
    /// canonical destination lies inside one configured user directory.
    pub fn normalize_absolute_path(
        &self,
        path: &Path,
        purpose: TargetPurpose,
    ) -> Result<Option<ResolvedFileTarget>, FileTargetError> {
        if !path.is_absolute() {
            return Ok(None);
        }
        let lexical_path = lexical_absolute_path(path);
        let canonical = match purpose {
            TargetPurpose::Existing => match canonical_existing(path) {
                Ok(canonical) => canonical,
                Err(error) if error.code == FilesystemErrorCode::FileNotFound => {
                    if let Some(target) = self.suggested_target_for_absolute_path(path) {
                        return Err(error.with_suggested_target(target));
                    }
                    return Err(error);
                }
                Err(error) => return Err(error),
            },
            TargetPurpose::Create => canonical_creation(path)?,
        };
        let mut lexically_inside = false;
        for directory in UserDirectory::ALL {
            let Some(root) = self.environment.user_dirs.get(directory) else {
                continue;
            };
            let root = match canonical_root(root) {
                Ok(root) => root,
                // A missing optional user directory must not make an
                // unrelated explicit path unusable. Existing absolute paths
                // are still canonicalized and authorized below; the missing
                // directory is simply not a semantic mapping candidate.
                Err(_) if matches!(purpose, TargetPurpose::Existing) => continue,
                Err(error) => return Err(error),
            };
            lexically_inside = lexically_inside
                || lexical_path
                    .as_deref()
                    .is_some_and(|lexical| lexical.starts_with(&root));
            let Ok(relative) = canonical.strip_prefix(&root) else {
                continue;
            };
            if validate_relative_path(relative).is_err() {
                continue;
            }
            let target = FileTarget {
                directory,
                relative_path: relative.to_path_buf(),
            };
            let resolved = self.resolve_target(&target, purpose)?;
            return Ok(Some(resolved));
        }
        if lexically_inside {
            return Err(FileTargetError::new(
                if contains_symlink_component_for_absolute(path) {
                    FilesystemErrorCode::SymlinkEscape
                } else {
                    FilesystemErrorCode::OutsideAllowedDirectory
                },
                "absolute target resolves outside the selected allowed directory",
                Some(path.to_string_lossy().into_owned()),
                false,
            ));
        }
        Ok(None)
    }

    /// Return a semantic target for a missing absolute path when its parent
    /// is safely inside a configured user directory. This is only a recovery
    /// hint; it never creates a file or authorizes an operation.
    pub fn suggested_target_for_absolute_path(&self, path: &Path) -> Option<FileTarget> {
        if !path.is_absolute() {
            return None;
        }
        let parent = path.parent()?.canonicalize().ok()?;
        let file_name = path.file_name()?.to_os_string();
        for directory in UserDirectory::ALL {
            let Some(configured_root) = self.environment.user_dirs.get(directory) else {
                continue;
            };
            let Ok(root) = configured_root.canonicalize() else {
                continue;
            };
            if !parent.starts_with(&root) {
                continue;
            }
            let relative_path = parent.strip_prefix(&root).ok()?.join(file_name.clone());
            if validate_relative_path(&relative_path).is_err() {
                continue;
            }
            return Some(FileTarget {
                directory,
                relative_path,
            });
        }
        None
    }

    /// Resolve any absolute path to a stable descriptor. Paths inside a
    /// configured user directory use the semantic reference; other paths use
    /// a compatibility path reference and remain subject to broker policy.
    pub fn descriptor_for_absolute(
        &self,
        path: &Path,
        purpose: TargetPurpose,
    ) -> Result<ResolvedFileTarget, FileTargetError> {
        if let Some(resolved) = self.normalize_absolute_path(path, purpose)? {
            return Ok(resolved);
        }
        let absolute = match purpose {
            TargetPurpose::Existing => canonical_existing(path)?,
            TargetPurpose::Create => canonical_creation(path)?,
        };
        Ok(ResolvedFileTarget {
            file_ref: FileRef::from_absolute_path(&absolute)?,
            directory: None,
            relative_path: None,
            display_path: absolute.to_string_lossy().into_owned(),
            absolute_path: absolute,
        })
    }

    /// Resolve a configured user directory itself (for listings). The
    /// descriptor carries no relative path; reads resolve it to the root.
    pub fn resolve_directory(
        &self,
        directory: UserDirectory,
    ) -> Result<ResolvedFileTarget, FileTargetError> {
        self.resolve_target(
            &FileTarget {
                directory,
                relative_path: PathBuf::new(),
            },
            TargetPurpose::Existing,
        )
    }

    /// Resolve an absolute path that names an existing directory. Exact
    /// configured roots map to semantic directory descriptors (which
    /// round-trip through [`FileResolver::resolve_target`]); anything else
    /// maps to a compatibility descriptor (round-trips through the
    /// compatibility branch of [`FileResolver::resolve_ref`]). Missing
    /// paths keep their file-oriented errors; nothing is created.
    pub fn descriptor_for_directory(
        &self,
        path: &Path,
    ) -> Result<ResolvedFileTarget, FileTargetError> {
        if !path.is_absolute() {
            return Err(FileTargetError::new(
                FilesystemErrorCode::InvalidRelativePath,
                "directory target must be absolute",
                Some(path.to_string_lossy().into_owned()),
                true,
            ));
        }
        let absolute = path.canonicalize().map_err(|error| {
            FileTargetError::new(
                FilesystemErrorCode::FileNotFound,
                format!("target directory is unavailable: {error}"),
                Some(path.to_string_lossy().into_owned()),
                true,
            )
        })?;
        if !absolute.is_dir() {
            return Err(FileTargetError::new(
                FilesystemErrorCode::TargetIsFile,
                "target is not a directory",
                Some(path.to_string_lossy().into_owned()),
                true,
            ));
        }
        for directory in UserDirectory::ALL {
            let Some(root) = self.environment.user_dirs.get(directory) else {
                continue;
            };
            let Ok(root) = root.canonicalize() else {
                continue;
            };
            if absolute == root {
                return Ok(ResolvedFileTarget {
                    file_ref: FileRef::from_directory(directory),
                    directory: Some(directory),
                    relative_path: None,
                    display_path: absolute.to_string_lossy().into_owned(),
                    absolute_path: absolute,
                });
            }
        }
        Ok(ResolvedFileTarget {
            file_ref: FileRef::from_absolute_path(&absolute)?,
            directory: None,
            relative_path: None,
            display_path: absolute.to_string_lossy().into_owned(),
            absolute_path: absolute,
        })
    }
}

fn canonical_root(root: &Path) -> Result<PathBuf, FileTargetError> {
    root.canonicalize().map_err(|error| {
        FileTargetError::new(
            FilesystemErrorCode::DirectoryUnavailable,
            format!("configured directory is unavailable: {error}"),
            Some(root.to_string_lossy().into_owned()),
            true,
        )
    })
}

fn canonical_existing(path: &Path) -> Result<PathBuf, FileTargetError> {
    let canonical = path.canonicalize().map_err(|error| {
        FileTargetError::new(
            FilesystemErrorCode::FileNotFound,
            format!("target file is unavailable: {error}"),
            Some(path.to_string_lossy().into_owned()),
            true,
        )
    })?;
    if canonical.is_dir() {
        return Err(FileTargetError::new(
            FilesystemErrorCode::TargetIsDirectory,
            "target is a directory, not a file",
            Some(path.to_string_lossy().into_owned()),
            false,
        ));
    }
    Ok(canonical)
}

fn canonical_creation(path: &Path) -> Result<PathBuf, FileTargetError> {
    if !path.is_absolute() {
        return Err(FileTargetError::new(
            FilesystemErrorCode::InvalidRelativePath,
            "creation target must be absolute after normalization",
            Some(path.to_string_lossy().into_owned()),
            true,
        ));
    }
    if path.file_name().is_none() {
        return Err(FileTargetError::new(
            FilesystemErrorCode::InvalidRelativePath,
            "creation target must name a file",
            Some(path.to_string_lossy().into_owned()),
            false,
        ));
    }
    let mut remainder = Vec::new();
    let mut cursor = path;
    loop {
        if let Ok(existing) = cursor.canonicalize() {
            let mut result = existing;
            for component in remainder.iter().rev() {
                result.push(component);
            }
            return Ok(result);
        }
        let Some(name) = cursor.file_name() else {
            return Err(FileTargetError::new(
                FilesystemErrorCode::DirectoryNotFound,
                "no existing parent directory could be canonicalized",
                Some(path.to_string_lossy().into_owned()),
                true,
            ));
        };
        remainder.push(name.to_os_string());
        cursor = cursor.parent().ok_or_else(|| {
            FileTargetError::new(
                FilesystemErrorCode::DirectoryNotFound,
                "no existing parent directory could be canonicalized",
                Some(path.to_string_lossy().into_owned()),
                true,
            )
        })?;
    }
}

fn lexical_absolute_path(path: &Path) -> Option<PathBuf> {
    if !path.is_absolute() {
        return None;
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    Some(normalized)
}

fn contains_symlink_component_for_absolute(path: &Path) -> bool {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn validate_target(
    root: &Path,
    lexical: &Path,
    purpose: TargetPurpose,
) -> Result<PathBuf, FileTargetError> {
    let canonical = match purpose {
        TargetPurpose::Existing => canonical_existing(lexical)?,
        TargetPurpose::Create => canonical_creation(lexical)?,
    };
    if !canonical.starts_with(root) {
        let code = if lexical.exists() || contains_symlink_component(root, lexical) {
            FilesystemErrorCode::SymlinkEscape
        } else {
            FilesystemErrorCode::OutsideAllowedDirectory
        };
        return Err(FileTargetError::new(
            code,
            "target resolves outside the selected allowed directory",
            Some(lexical.to_string_lossy().into_owned()),
            false,
        ));
    }
    Ok(canonical)
}

fn contains_symlink_component(root: &Path, target: &Path) -> bool {
    let Ok(relative) = target.strip_prefix(root) else {
        return false;
    };
    let mut current = root.to_path_buf();
    for component in relative.components() {
        current.push(component.as_os_str());
        if std::fs::symlink_metadata(&current)
            .map(|metadata| metadata.file_type().is_symlink())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

/// Conversation-local context. It stores identity, not authority; every
/// reuse goes back through `FileResolver` and the capability broker.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConversationFileContext {
    pub active_file: Option<FileRef>,
    pub recent_files: VecDeque<FileRef>,
}

impl ConversationFileContext {
    pub fn record_success(&mut self, file_ref: FileRef) {
        self.recent_files.retain(|candidate| candidate != &file_ref);
        self.recent_files.push_front(file_ref.clone());
        while self.recent_files.len() > 5 {
            self.recent_files.pop_back();
        }
        self.active_file = Some(file_ref);
    }

    pub fn active(&self) -> Option<&FileRef> {
        self.active_file.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use host_core::UserDirectories;

    fn environment(root: &Path) -> HostEnvironment {
        HostEnvironment {
            os: "linux",
            architecture: "x86_64",
            home: Some(root.to_path_buf()),
            cwd: Some(root.to_path_buf()),
            user_dirs: UserDirectories {
                desktop: Some(root.join("Escritorio")),
                documents: Some(root.join("Documentos")),
                ..Default::default()
            },
            xdg_config_source: None,
            path_style: "POSIX",
            path_separator: "/",
        }
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("utsuwa-file-target-{name}-{}", std::process::id()))
    }

    #[test]
    fn semantic_file_ref_round_trips_unicode_and_nested_paths() {
        let root = test_root("ref");
        let _ = std::fs::remove_dir_all(&root);
        let documents = root.join("Documentos").join("日本語");
        std::fs::create_dir_all(&documents).unwrap();
        let relative_path = PathBuf::from("日本語/notes file ñ.txt");
        let target = FileTarget {
            directory: UserDirectory::Documents,
            relative_path: relative_path.clone(),
        };
        let file_ref = FileRef::from_target(&target).unwrap();
        assert_eq!(file_ref.as_str(), "file:documents:日本語/notes file ñ.txt");
        let parsed = FileRef::parse(file_ref.as_str()).unwrap();
        assert_eq!(parsed, file_ref);
        assert_eq!(serde_json::to_value(&file_ref).unwrap(), file_ref.as_str());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn absolute_paths_normalize_to_semantic_localized_targets() {
        let root = test_root("absolute");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        let documents = root.join("Documentos").join("project").join("src");
        std::fs::create_dir_all(&desktop).unwrap();
        std::fs::create_dir_all(&documents).unwrap();
        let desktop_file = desktop.join("note.txt");
        let document_file = documents.join("config.toml");
        std::fs::write(&desktop_file, "note").unwrap();
        std::fs::write(&document_file, "config").unwrap();
        let resolver = FileResolver::new(environment(&root));

        let desktop_target = resolver
            .normalize_absolute_path(&desktop_file, TargetPurpose::Existing)
            .unwrap()
            .unwrap();
        assert_eq!(desktop_target.directory, Some(UserDirectory::Desktop));
        assert_eq!(
            desktop_target.relative_path,
            Some(PathBuf::from("note.txt"))
        );
        assert_eq!(desktop_target.file_ref.as_str(), "file:desktop:note.txt");

        let document_target = resolver
            .normalize_absolute_path(&document_file, TargetPurpose::Existing)
            .unwrap()
            .unwrap();
        assert_eq!(document_target.directory, Some(UserDirectory::Documents));
        assert_eq!(
            document_target.relative_path,
            Some(PathBuf::from("project/src/config.toml"))
        );
        assert_eq!(
            document_target.file_ref.as_str(),
            "file:documents:project/src/config.toml"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_paths_inside_configured_directories_get_semantic_recovery_targets() {
        let root = test_root("missing-suggestion");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let missing = desktop.join("resumen_hoy.txt");
        let resolver = FileResolver::new(environment(&root));

        let error = resolver
            .normalize_absolute_path(&missing, TargetPurpose::Existing)
            .unwrap_err();
        assert_eq!(error.code, FilesystemErrorCode::FileNotFound);
        assert_eq!(error.retryable, true);
        assert_eq!(
            error.suggested_target,
            Some(FileTarget {
                directory: UserDirectory::Desktop,
                relative_path: PathBuf::from("resumen_hoy.txt"),
            })
        );

        let semantic_error = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("resumen_hoy.txt"),
                },
                TargetPurpose::Existing,
            )
            .unwrap_err();
        assert_eq!(semantic_error.suggested_target, error.suggested_target);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn missing_paths_outside_configured_directories_get_no_creation_hint() {
        let root = test_root("missing-outside-suggestion");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let resolver = FileResolver::new(environment(&root));
        assert!(resolver
            .suggested_target_for_absolute_path(&root.join("project/resumen_hoy.txt"))
            .is_none());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn traversal_is_rejected_before_path_resolution() {
        let root = test_root("traversal");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let resolver = FileResolver::new(environment(&root));
        for relative_path in ["../secret", "foo/../../secret", "./../secret"] {
            let error = resolver
                .resolve_target(
                    &FileTarget {
                        directory: UserDirectory::Desktop,
                        relative_path: PathBuf::from(relative_path),
                    },
                    TargetPurpose::Create,
                )
                .unwrap_err();
            assert_eq!(error.code, FilesystemErrorCode::PathTraversal);
        }
        let nul = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("bad\0name"),
                },
                TargetPurpose::Create,
            )
            .unwrap_err();
        assert_eq!(nul.code, FilesystemErrorCode::InvalidRelativePath);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn redundant_current_directory_components_are_normalized() {
        let root = test_root("dot");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let path = desktop.join("note.txt");
        std::fs::write(&path, "note").unwrap();
        let resolved = FileResolver::new(environment(&root)).resolve_target(
            &FileTarget {
                directory: UserDirectory::Desktop,
                relative_path: PathBuf::from("./nested/../note.txt"),
            },
            TargetPurpose::Existing,
        );
        // Parent components remain forbidden even when they would cancel a
        // preceding component; callers must provide a genuinely relative path.
        assert_eq!(
            resolved.unwrap_err().code,
            FilesystemErrorCode::PathTraversal
        );
        let resolved = FileResolver::new(environment(&root))
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("./note.txt"),
                },
                TargetPurpose::Existing,
            )
            .unwrap();
        assert_eq!(resolved.relative_path, Some(PathBuf::from("note.txt")));
        assert_eq!(resolved.file_ref.as_str(), "file:desktop:note.txt");
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_but_in_root_symlink_is_allowed() {
        use std::os::unix::fs::symlink;

        let root = test_root("symlink");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        let outside = root.join("outside");
        let inside = desktop.join("subdir");
        std::fs::create_dir_all(&inside).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::fs::write(inside.join("safe.txt"), "safe").unwrap();
        symlink(&outside, desktop.join("outside-link")).unwrap();
        symlink(&inside, desktop.join("inside-link")).unwrap();
        let resolver = FileResolver::new(environment(&root));

        let escaped = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("outside-link/secret.txt"),
                },
                TargetPurpose::Existing,
            )
            .unwrap_err();
        assert_eq!(escaped.code, FilesystemErrorCode::SymlinkEscape);

        let escaped_new = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("outside-link/new.txt"),
                },
                TargetPurpose::Create,
            )
            .unwrap_err();
        assert_eq!(escaped_new.code, FilesystemErrorCode::SymlinkEscape);

        let safe = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::from("inside-link/safe.txt"),
                },
                TargetPurpose::Existing,
            )
            .unwrap();
        assert_eq!(
            safe.absolute_path,
            inside.join("safe.txt").canonicalize().unwrap()
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn deleted_semantic_reference_becomes_stale() {
        let root = test_root("stale");
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let path = desktop.join("note.txt");
        std::fs::write(&path, "note").unwrap();
        let resolver = FileResolver::new(environment(&root));
        let file_ref = FileRef::from_target(&FileTarget {
            directory: UserDirectory::Desktop,
            relative_path: PathBuf::from("note.txt"),
        })
        .unwrap();
        std::fs::remove_file(&path).unwrap();
        let error = resolver
            .resolve_ref(&file_ref, TargetPurpose::Existing)
            .unwrap_err();
        assert_eq!(error.code, FilesystemErrorCode::StaleFileRef);
        assert!(!error.retryable);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn conversation_context_tracks_only_successful_recent_refs() {
        let mut context = ConversationFileContext::default();
        for index in 0..7 {
            context
                .record_success(FileRef::parse(&format!("file:desktop:file-{index}.txt")).unwrap());
        }
        assert_eq!(
            context.active().unwrap().as_str(),
            "file:desktop:file-6.txt"
        );
        assert_eq!(context.recent_files.len(), 5);
        context.record_success(FileRef::parse("file:desktop:file-4.txt").unwrap());
        assert_eq!(
            context.recent_files.front().unwrap().as_str(),
            "file:desktop:file-4.txt"
        );
        assert_eq!(context.recent_files.len(), 5);
    }

    fn directory_root(name: &str) -> (PathBuf, HostEnvironment) {
        let root = test_root(name);
        let _ = std::fs::remove_dir_all(&root);
        let desktop = root.join("Escritorio");
        std::fs::create_dir_all(&desktop).unwrap();
        let environment = environment(&root);
        (root, environment)
    }

    #[test]
    fn empty_relative_path_resolves_the_directory_root_for_reads() {
        let (_root, environment) = directory_root("dir-root");
        let resolver = FileResolver::new(environment.clone());
        for relative in [PathBuf::new(), PathBuf::from(""), PathBuf::from(".")] {
            let resolved = resolver
                .resolve_target(
                    &FileTarget {
                        directory: UserDirectory::Desktop,
                        relative_path: relative,
                    },
                    TargetPurpose::Existing,
                )
                .unwrap();
            assert_eq!(resolved.directory, Some(UserDirectory::Desktop));
            assert_eq!(resolved.relative_path, None);
            assert_eq!(resolved.file_ref.as_str(), "file:desktop:");
            assert!(resolved.absolute_path.is_dir());
            assert_eq!(
                resolved.absolute_path,
                environment
                    .user_dirs
                    .desktop
                    .as_deref()
                    .unwrap()
                    .canonicalize()
                    .unwrap()
            );
        }
    }

    #[test]
    fn empty_relative_path_stays_strict_for_creation() {
        let (_root, environment) = directory_root("dir-create");
        let resolver = FileResolver::new(environment);
        let error = resolver
            .resolve_target(
                &FileTarget {
                    directory: UserDirectory::Desktop,
                    relative_path: PathBuf::new(),
                },
                TargetPurpose::Create,
            )
            .unwrap_err();
        assert_eq!(error.code, FilesystemErrorCode::InvalidRelativePath);
    }

    #[test]
    fn directory_file_ref_round_trips_through_the_resolver() {
        let (_root, environment) = directory_root("dir-ref");
        let resolver = FileResolver::new(environment);
        let file_ref = FileRef::from_directory(UserDirectory::Desktop);
        assert_eq!(file_ref.as_str(), "file:desktop:");
        assert!(file_ref.is_semantic());
        let parsed = FileRef::parse(file_ref.as_str()).unwrap();
        let resolved = resolver
            .resolve_ref(&parsed, TargetPurpose::Existing)
            .unwrap();
        assert_eq!(resolved.directory, Some(UserDirectory::Desktop));
        assert!(resolved.absolute_path.is_dir());
    }

    #[test]
    fn descriptor_for_directory_maps_roots_and_subdirs() {
        let (_root, environment) = directory_root("dir-descriptor");
        std::fs::create_dir_all(
            environment
                .user_dirs
                .desktop
                .as_deref()
                .unwrap()
                .join("sub"),
        )
        .unwrap();
        let resolver = FileResolver::new(environment.clone());
        let desktop = environment.user_dirs.desktop.as_deref().unwrap();

        let root = resolver.descriptor_for_directory(desktop).unwrap();
        assert_eq!(root.directory, Some(UserDirectory::Desktop));
        assert_eq!(root.file_ref.as_str(), "file:desktop:");

        let sub = resolver
            .descriptor_for_directory(&desktop.join("sub"))
            .unwrap();
        assert_eq!(sub.directory, None);
        assert!(sub.file_ref.as_str().starts_with("file:path:"));
        // Compatibility directory references resolve back for reads.
        let back = resolver
            .resolve_ref(&sub.file_ref, TargetPurpose::Existing)
            .unwrap();
        assert_eq!(back.absolute_path, sub.absolute_path);

        let missing = resolver.descriptor_for_directory(&desktop.join("missing"));
        assert_eq!(missing.unwrap_err().code, FilesystemErrorCode::FileNotFound);

        let file = desktop.join("note.txt");
        std::fs::write(&file, "x").unwrap();
        let not_dir = resolver.descriptor_for_directory(&file);
        assert_eq!(not_dir.unwrap_err().code, FilesystemErrorCode::TargetIsFile);
    }
}
