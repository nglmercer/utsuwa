use crate::append::AppendFileTool;
use crate::append_user::AppendUserFileTool;
use crate::common::DEFAULT_USER_FILENAME;
use crate::create::CreateUserFileTool;
use crate::edit::EditTool;
use crate::edit_args::looks_like_clock_text;
use crate::edit_file::EditFileTool;
use crate::edit_user_file::EditUserFileTool;
use crate::read::HostAwarePathTool;
use crate::replace::ReplaceUserFileTool;
use crate::resolve::ResolveUserDirectoryTool;
use crate::user_write::UserDirectoryWriteTool;
use crate::write::{enrich_write_error, HostAwareWriteTool};
use capability_core::{
    AgentId, Capability, CapabilityTicket, InvocationId, Principal, Resource, ResourceScope,
};
use file_target::{ConversationFileContext, FileRef, FileTarget};
use host_core::{HostEnvironment, UserDirectory};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tool_core::Tool;
use tool_core::{ToolContext, ToolError};

fn environment(home: &Path, desktop: &Path) -> HostEnvironment {
    HostEnvironment {
        os: "linux",
        architecture: "x86_64",
        home: Some(home.to_path_buf()),
        cwd: Some(home.to_path_buf()),
        user_dirs: host_core::UserDirectories {
            desktop: Some(desktop.to_path_buf()),
            ..Default::default()
        },
        xdg_config_source: Some(home.join(".config/user-dirs.dirs")),
        path_style: "POSIX",
        path_separator: "/",
    }
}

fn ticketed_context(path: &Path) -> ToolContext {
    let principal = Principal::Agent(AgentId::new("user-directory-test"));
    let invocation = InvocationId::fresh();
    let ticket = CapabilityTicket::mint(
        principal.clone(),
        Capability::FilesystemWrite,
        ResourceScope::new(vec![Resource::Path(path.to_path_buf())]),
        invocation,
        Duration::from_secs(60),
    );
    ToolContext {
        principal,
        invocation_id: invocation,
        ticket: Some(ticket),
    }
}

fn read_ticketed_context(path: &Path) -> ToolContext {
    let principal = Principal::Agent(AgentId::new("user-directory-read-test"));
    let invocation = InvocationId::fresh();
    let ticket = CapabilityTicket::mint(
        principal.clone(),
        Capability::FilesystemRead,
        ResourceScope::new(vec![Resource::Path(path.to_path_buf())]),
        invocation,
        Duration::from_secs(60),
    );
    ToolContext {
        principal,
        invocation_id: invocation,
        ticket: Some(ticket),
    }
}

#[tokio::test]
async fn host_read_accepts_equivalent_target_selectors() {
    let (home, desktop) = stale_home("host-read-equivalent-targets");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let file_ref = FileRef::from_target(&FileTarget {
        directory: UserDirectory::Desktop,
        relative_path: PathBuf::from("note.txt"),
    })
    .unwrap();
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let output = tool
        .invoke(
            read_ticketed_context(&target),
            serde_json::json!({
                "file_ref": file_ref,
                "target": {
                    "directory": "desktop",
                    "relative_path": "note.txt"
                },
                "path": target.to_string_lossy(),
            }),
        )
        .await
        .unwrap();

    assert_eq!(output.content["content"], "2026-09-08\n");
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_read_marks_mismatched_target_selectors_as_retryable() {
    let (home, desktop) = stale_home("host-read-mismatched-targets");
    let target = desktop.join("note.txt");
    let other = desktop.join("other.txt");
    std::fs::write(&target, "note\n").unwrap();
    std::fs::write(&other, "other\n").unwrap();
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let error = tool
        .invoke(
            read_ticketed_context(&target),
            serde_json::json!({
                "target": {
                    "directory": "desktop",
                    "relative_path": "note.txt"
                },
                "path": other.to_string_lossy(),
            }),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, ToolError::RetryRequired { .. }),
        "{error:?}"
    );
    assert!(error.model_message().contains("invalid_file_target"));
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_read_ignores_redundant_configured_directory_path_hint() {
    let (home, desktop) = stale_home("host-read-directory-hint");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "line\n").unwrap();
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let output = tool
        .invoke(
            read_ticketed_context(&target),
            serde_json::json!({
                "target": {
                    "directory": "desktop",
                    "relative_path": "note.txt"
                },
                "path": desktop.to_string_lossy(),
            }),
        )
        .await
        .unwrap();

    assert_eq!(output.content["content"], "line\n");
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_read_reuses_active_file_when_target_is_omitted() {
    let (home, desktop) = stale_home("host-read-active-file");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let context = Arc::new(Mutex::new(ConversationFileContext::default()));
    context.lock().unwrap().record_success(
        FileRef::from_target(&FileTarget {
            directory: UserDirectory::Desktop,
            relative_path: PathBuf::from("note.txt"),
        })
        .unwrap(),
    );
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    )
    .with_active_context(Some(context));

    let output = tool
        .invoke(read_ticketed_context(&target), serde_json::json!({}))
        .await
        .unwrap();

    assert_eq!(output.content["content"], "2026-09-08\n");
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn write_user_file_resolves_localized_desktop_without_creating_wrong_path() {
    let home =
        std::env::temp_dir().join(format!("utsuwa-user-directory-tool-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join("hello.txt");
    let wrong = home.join("Desktop");
    let tool = UserDirectoryWriteTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let args = serde_json::json!({
        "directory_id": "desktop",
        "relative_path": "hello.txt",
        "content": "hello",
    });
    let requirement = tool
        .required_capability(&args)
        .expect("resolved user file needs write capability");
    assert_eq!(requirement.capability, Capability::FilesystemWrite);
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["directory_id"], "desktop");
    assert_eq!(
        output.content["resolved_directory"],
        desktop.to_string_lossy().as_ref()
    );
    assert_eq!(output.content["directory"], "desktop");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    assert!(!wrong.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn write_user_file_normalizes_exact_path_and_configured_basename() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-user-directory-normalization-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let tool = UserDirectoryWriteTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let exact_target = desktop.join("hello.txt");
    let exact_path = desktop.to_string_lossy().into_owned();
    let exact_args = serde_json::json!({
        "directory": exact_path,
        "relative_path": "hello.txt",
        "content": "hello",
    });
    let exact_requirement = tool
        .required_capability(&exact_args)
        .expect("an exact configured directory path needs write capability");
    assert_eq!(
        exact_requirement.resource,
        Resource::Path(exact_target.canonicalize().unwrap_or(exact_target.clone()))
    );
    let exact_output = tool
        .invoke(ticketed_context(&exact_target), exact_args)
        .await
        .unwrap();
    assert_eq!(exact_output.content["directory_id"], "desktop");
    assert_eq!(
        exact_output.content["resolved_directory"],
        desktop.to_string_lossy().as_ref()
    );
    assert_eq!(
        exact_output.content["path"],
        exact_target.to_string_lossy().as_ref()
    );

    let basename_target = desktop.join("hello2.txt");
    let basename_args = serde_json::json!({
        "directory_id": "Escritorio",
        "relative_path": "hello2.txt",
        "content": "hello2",
    });
    tool.invoke(ticketed_context(&basename_target), basename_args)
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&basename_target).unwrap(), "hello2");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn create_user_file_resolves_location_without_constructing_a_path() {
    let home = std::env::temp_dir().join(format!("utsuwa-create-user-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join("date.txt");
    let wrong = home.join("Desktop");
    let tool = CreateUserFileTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let args = serde_json::json!({
        "location": "desktop",
        "filename": "date.txt",
        "content": "today",
    });
    let requirement = tool
        .required_capability(&args)
        .expect("the host resolved target needs filesystem write");
    assert_eq!(requirement.capability, Capability::FilesystemWrite);
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
    assert_eq!(output.content["location"], "desktop");
    assert_eq!(output.content["filename"], "date.txt");
    assert_eq!(
        output.content["resolved_directory"],
        desktop.to_string_lossy().as_ref()
    );
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "today");
    assert!(!wrong.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn create_user_file_normalizes_an_absolute_filename_only_inside_selected_dir() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-create-user-file-absolute-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join("date.txt");
    let tool = CreateUserFileTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&desktop),
            serde_json::json!({
                "location": "desktop",
                "filename": target.to_string_lossy(),
                "content": "absolute compatibility",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["filename"], "date.txt");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "absolute compatibility"
    );

    let error = tool
        .invoke(
            ticketed_context(&home.join("outside.txt")),
            serde_json::json!({
                "location": "desktop",
                "filename": "/etc/date.txt",
                "content": "must reject",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert!(error
        .to_string()
        .contains("inside the selected host directory"));
    assert!(!home.join("outside.txt").exists());

    let parent_escape = tool
        .invoke(
            ticketed_context(&home.join("escape.txt")),
            serde_json::json!({
                "location": "desktop",
                "filename": "../escape.txt",
                "content": "must reject",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(parent_escape, ToolError::InvalidArgs { .. }));
    assert!(!home.join("escape.txt").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn create_user_file_allows_explicit_nested_parent_creation() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-create-user-file-nested-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join("notes/hello.txt");
    let tool = CreateUserFileTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&desktop),
            serde_json::json!({
                "location": "desktop",
                "filename": "notes/hello.txt",
                "content": "nested",
                "create_parents": true,
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert!(target.is_file());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn create_user_file_uses_a_safe_default_filename_when_omitted() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-create-user-file-default-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let target = desktop.join(DEFAULT_USER_FILENAME);
    let tool = CreateUserFileTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "content": "default name",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["filename"], DEFAULT_USER_FILENAME);
    assert_eq!(output.content["generated_filename"], true);
    assert!(target.is_file());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn write_user_file_rejects_arbitrary_absolute_directory_path() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-user-directory-reject-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let arbitrary = home.join("random");
    let target = arbitrary.join("hello.txt");
    let tool = UserDirectoryWriteTool::with_environment(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "directory": arbitrary.to_string_lossy(),
                "relative_path": "hello.txt",
                "content": "hello",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        &error,
        ToolError::Filesystem {
            code,
            retryable: true,
            ..
        } if code == "invalid_directory"
    ));
    let message = error.model_message();
    assert!(message.contains("invalid_directory"));
    assert!(message.contains("valid_directory_ids"));
    assert!(message.contains("Escritorio"));
    assert!(!arbitrary.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn resolve_user_dir_returns_the_exact_configured_path() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-user-directory-resolve-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    let tool = ResolveUserDirectoryTool::with_environment(environment(&home, &desktop));
    let output = tool
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("resolve-test"))),
            serde_json::json!({"directory_id": "desktop"}),
        )
        .await
        .unwrap();
    assert_eq!(output.content["directory_id"], "desktop");
    assert_eq!(
        output.content["resolved_path"],
        desktop.to_string_lossy().as_ref()
    );
    assert_eq!(output.content["directory"], "desktop");
    assert_eq!(output.content["path"], desktop.to_string_lossy().as_ref());

    let legacy_exact = tool
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("resolve-test-legacy"))),
            serde_json::json!({"directory": desktop.to_string_lossy()}),
        )
        .await
        .unwrap();
    assert_eq!(legacy_exact.content["directory_id"], "desktop");
    assert_eq!(
        legacy_exact.content["resolved_path"],
        desktop.to_string_lossy().as_ref()
    );
    std::fs::remove_dir_all(&home).unwrap();
}

fn edit_harness(home: &Path) -> (PathBuf, PathBuf) {
    let _ = std::fs::remove_dir_all(home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    (home.to_path_buf(), desktop)
}

#[tokio::test]
async fn edit_user_file_updates_the_existing_file_without_creating_a_new_one() {
    let home = std::env::temp_dir().join(format!("utsuwa-edit-user-file-{}", std::process::id()));
    let (_home, desktop) = edit_harness(&home);
    let target = desktop.join("note.txt");
    std::fs::write(&target, "hello").unwrap();
    let wrong = home.join("Desktop").join("note.txt");
    let tool = EditUserFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let args = serde_json::json!({
        "location": "desktop",
        "filename": "note.txt",
        "old_text": "hello",
        "new_text": "hello world",
    });
    let requirement = tool
        .required_capability(&args)
        .expect("the host resolved target needs filesystem write");
    assert_eq!(requirement.capability, Capability::FilesystemWrite);
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["updated"], true);
    assert_eq!(output.content["location"], "desktop");
    assert_eq!(output.content["filename"], "note.txt");
    assert_eq!(
        output.content["resolved_directory"],
        desktop.to_string_lossy().as_ref()
    );
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
    assert!(!wrong.exists());
    assert!(!home.join("Desktop").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_user_file_rejects_missing_old_text_without_overwriting() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-edit-user-file-whole-{}",
        std::process::id()
    ));
    let (_home, desktop) = edit_harness(&home);
    let target = desktop.join("note.txt");
    std::fs::write(&target, "old contents").unwrap();
    let tool = EditUserFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "complete replacement",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert!(error.to_string().contains("filesystem.read"));
    assert!(error.to_string().contains("Updated date"));
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old contents");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_user_file_rejects_unknown_and_ambiguous_old_text_but_resolves_date_placeholder() {
    let home = std::env::temp_dir().join(format!(
        "utsuwa-edit-user-file-safety-{}",
        std::process::id()
    ));
    let (_home, desktop) = edit_harness(&home);
    let target = desktop.join("note.txt");
    std::fs::write(&target, "alpha beta alpha").unwrap();
    let tool = EditUserFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let missing = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "gamma",
                "new_text": "delta",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(missing, ToolError::RetryRequired { .. }),
        "{missing:?}"
    );
    assert!(missing.to_string().contains("old_text_mismatch"));
    assert!(missing.to_string().contains("filesystem.read"));
    let ambiguous = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "alpha",
                "new_text": "delta",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(ambiguous, ToolError::RetryRequired { .. }),
        "{ambiguous:?}"
    );
    assert!(ambiguous.to_string().contains("old_text_mismatch"));
    let placeholder = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "alpha beta alpha",
                "new_text": "Updated date",
            }),
        )
        .await
        .unwrap();
    assert_eq!(placeholder.content["updated"], true);
    let expected_date = chrono::Local::now().format("%Y-%m-%d").to_string();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), expected_date);
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_file_updates_an_arbitrary_absolute_path() {
    let dir = std::env::temp_dir().join(format!("utsuwa-edit-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("arbitrary.txt");
    std::fs::write(&target, "old value here").unwrap();
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&dir, &dir.join("Escritorio")),
    );
    let args = serde_json::json!({
        "path": target.to_string_lossy(),
        "old_text": "old",
        "new_text": "new",
    });
    let requirement = tool
        .required_capability(&args)
        .expect("an absolute path needs filesystem write");
    assert_eq!(requirement.capability, Capability::FilesystemWrite);
    let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
    assert_eq!(output.content["updated"], true);
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new value here");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn edit_file_rejects_relative_paths_like_the_broker() {
    let home =
        std::env::temp_dir().join(format!("utsuwa-edit-file-relative-{}", std::process::id()));
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &home.join("Escritorio")),
    );
    let error = tool
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("edit-relative"))),
            serde_json::json!({
                "path": "relative/note.txt",
                "old_text": "a",
                "new_text": "b",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            ToolError::InvalidArgs { .. } | ToolError::Denied { .. }
        ),
        "{error:?}"
    );
}

#[tokio::test]
async fn replace_user_file_replaces_whole_contents_but_never_creates() {
    let home =
        std::env::temp_dir().join(format!("utsuwa-replace-user-file-{}", std::process::id()));
    let (_home, desktop) = edit_harness(&home);
    let target = desktop.join("note.txt");
    std::fs::write(&target, "stale contents").unwrap();
    let tool = ReplaceUserFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "content": "entire new contents",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["updated"], true);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "entire new contents"
    );

    let missing = tool
        .invoke(
            ticketed_context(&desktop.join("absent.txt")),
            serde_json::json!({
                "location": "desktop",
                "filename": "absent.txt",
                "content": "must not create",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, ToolError::Failed { .. }), "{missing:?}");
    assert!(!desktop.join("absent.txt").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn append_user_file_appends_without_reproducing_the_file() {
    let home = std::env::temp_dir().join(format!("utsuwa-append-user-file-{}", std::process::id()));
    let (_home, desktop) = edit_harness(&home);
    let target = desktop.join("note.txt");
    std::fs::write(&target, "line one\n").unwrap();
    let tool = AppendUserFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "content": "line two\n",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["appended"], true);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "line one\nline two\n"
    );
    assert!(output.mutation.is_some());

    // Appending to a missing file creates it with the appended text.
    let fresh = desktop.join("fresh.txt");
    tool.invoke(
        ticketed_context(&fresh),
        serde_json::json!({
            "location": "desktop",
            "filename": "fresh.txt",
            "content": "first line\n",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "first line\n");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn append_file_appends_at_an_arbitrary_absolute_path() {
    let dir = std::env::temp_dir().join(format!("utsuwa-append-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("log.txt");
    std::fs::write(&target, "a").unwrap();
    let tool = AppendFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&dir, &dir.join("Escritorio")),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target.to_string_lossy(),
                "content": "b",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["appended"], true);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "ab");
    std::fs::remove_dir_all(&dir).unwrap();
}

fn stale_home(name: &str) -> (PathBuf, PathBuf) {
    let home = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let desktop = home.join("Escritorio");
    std::fs::create_dir_all(&desktop).unwrap();
    (home, desktop)
}

#[tokio::test]
async fn edit_file_normalizes_a_stale_conventional_desktop_path() {
    let (home, desktop) = stale_home("utsuwa-edit-remap");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "old content here").unwrap();
    // The conventional directory is never created.
    let stale = home.join("Desktop").join("note.txt");
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let args = serde_json::json!({
        "path": stale.to_string_lossy(),
        "old_text": "old",
        "new_text": "new",
    });
    // The ticket must scope the normalized target, not the stale path.
    let requirement = tool
        .required_capability(&args)
        .expect("the normalized target needs filesystem write");
    assert_eq!(requirement.capability, Capability::FilesystemWrite);
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool.invoke(ticketed_context(&target), args).await.unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["updated"], true);
    assert_eq!(
        output.content["normalized_from"],
        stale.to_string_lossy().as_ref()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "new content here"
    );
    assert!(!home.join("Desktop").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_file_keeps_an_explicit_path_when_both_locations_exist() {
    let (home, desktop) = stale_home("utsuwa-edit-ambiguous");
    let conventional_dir = home.join("Desktop");
    std::fs::create_dir_all(&conventional_dir).unwrap();
    let conventional = conventional_dir.join("note.txt");
    std::fs::write(&conventional, "conventional old").unwrap();
    let configured = desktop.join("note.txt");
    std::fs::write(&configured, "configured old").unwrap();
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&conventional),
            serde_json::json!({
                "path": conventional.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap();
    assert_eq!(
        output.content["path"],
        conventional.to_string_lossy().as_ref()
    );
    assert!(output.content.get("normalized_from").is_none());
    assert_eq!(
        std::fs::read_to_string(&conventional).unwrap(),
        "conventional new"
    );
    assert_eq!(
        std::fs::read_to_string(&configured).unwrap(),
        "configured old"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_file_reports_a_structured_error_when_nothing_exists() {
    let (home, desktop) = stale_home("utsuwa-edit-missing");
    let stale = home.join("Desktop").join("absent.txt");
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("edit-missing"))),
            serde_json::json!({
                "path": stale.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(
            &error,
            ToolError::Filesystem {
                code,
                retryable: true,
                ..
            } if code == "file_not_found"
        ),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(
        message.contains(&stale.to_string_lossy().into_owned()),
        "{message}"
    );
    assert!(
        message.contains(&desktop.to_string_lossy().into_owned()),
        "{message}"
    );
    assert!(message.contains("filesystem.edit_user_file"), "{message}");
    let model_message = error.model_message();
    assert!(model_message.contains("file_not_found"), "{model_message}");
    assert!(model_message.contains("filesystem.create_user_file"));
    // No capability is minted for a call that cannot resolve a target.
    assert!(tool
        .required_capability(&serde_json::json!({
            "path": stale.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        }))
        .is_none());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_missing_target_explains_create_or_reselect_recovery() {
    let (home, desktop) = stale_home("utsuwa-edit-missing-recovery");
    let missing = desktop.join("resumen_hoy.txt");
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ToolContext::new(Principal::Agent(AgentId::new("edit-missing-recovery"))),
            serde_json::json!({
                "path": missing.to_string_lossy(),
                "old_text": "old",
                "new_text": "new",
            }),
        )
        .await
        .unwrap_err();

    assert!(
        matches!(error, ToolError::Filesystem { ref code, retryable: true, .. } if code == "file_not_found"),
        "{error:?}"
    );
    let model_error: serde_json::Value = serde_json::from_str(&error.model_message()).unwrap();
    let details = &model_error["error"];
    assert_eq!(details["retry_action"], "change_operation_or_target");
    assert_eq!(details["retry_same_arguments"], false);
    assert_eq!(details["next_tool"], "filesystem.stat");
    assert!(details["next_tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool == "filesystem.create_user_file"));
    assert_eq!(
        details["suggested_target"],
        serde_json::json!({
            "directory": "desktop",
            "relative_path": "resumen_hoy.txt"
        })
    );
    assert!(!missing.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_reports_read_retry_guidance_for_unknown_old_text() {
    let (home, desktop) = stale_home("utsuwa-edit-retry");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "exact current contents").unwrap();
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target.to_string_lossy(),
                "old_text": "guessed text",
                "new_text": "new",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::RetryRequired { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    assert!(message.contains("old_text_mismatch"), "{message}");
    assert!(message.contains("filesystem.read"), "{message}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "exact current contents"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_rejects_missing_old_text_without_overwriting() {
    let (home, desktop) = stale_home("utsuwa-edit-missing-old");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "exact current contents").unwrap();
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let metadata = tool.metadata();
    let required = metadata.input_schema["required"]
        .as_array()
        .expect("edit schema required must be an array");
    assert!(required.iter().any(|field| field == "old_text"));

    // Missing old_text must never turn a partial edit into a whole-file overwrite.
    let missing = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target.to_string_lossy(),
                "new_text": "new",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(missing, ToolError::InvalidArgs { .. }),
        "{missing:?}"
    );
    assert!(missing.to_string().contains("filesystem.read"));
    assert!(!missing.to_string().contains("exact current contents"));
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "exact current contents"
    );

    // Empty old_text would match everywhere, so it stays rejected with guidance.
    let empty = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target.to_string_lossy(),
                "old_text": "",
                "new_text": "new",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(empty, ToolError::InvalidArgs { .. }), "{empty:?}");
    assert!(empty.to_string().contains("filesystem.read"), "{empty:?}");
    // Missing new_text names the fix as well.
    let no_new = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target.to_string_lossy(),
                "old_text": "exact current contents",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(no_new, ToolError::InvalidArgs { .. }),
        "{no_new:?}"
    );
    assert!(no_new.to_string().contains("new_text"), "{no_new:?}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "exact current contents"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn edit_file_does_not_remap_nested_or_foreign_lookalikes() {
    let (home, desktop) = stale_home("utsuwa-edit-lookalike");
    // Nested lookalike: the parent is not directly $HOME/Desktop.
    let nested_dir = home.join("projects").join("Desktop");
    std::fs::create_dir_all(&nested_dir).unwrap();
    let nested = nested_dir.join("note.txt");
    std::fs::write(&nested, "nested old").unwrap();
    let tool = EditFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    tool.invoke(
        ticketed_context(&nested),
        serde_json::json!({
            "path": nested.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&nested).unwrap(), "nested new");

    // Foreign root: an explicit path outside $HOME is never rewritten.
    let foreign_root =
        std::env::temp_dir().join(format!("utsuwa-edit-foreign-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&foreign_root);
    let foreign_dir = foreign_root.join("Desktop");
    std::fs::create_dir_all(&foreign_dir).unwrap();
    let foreign = foreign_dir.join("note.txt");
    std::fs::write(&foreign, "foreign old").unwrap();
    tool.invoke(
        ticketed_context(&foreign),
        serde_json::json!({
            "path": foreign.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&foreign).unwrap(), "foreign new");
    std::fs::remove_dir_all(&home).unwrap();
    std::fs::remove_dir_all(&foreign_root).unwrap();
}

#[tokio::test]
async fn append_file_normalizes_a_stale_conventional_path() {
    let (home, desktop) = stale_home("utsuwa-append-remap");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "line one\n").unwrap();
    let stale = home.join("Desktop").join("note.txt");
    let tool = AppendFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let requirement = tool
        .required_capability(&serde_json::json!({
            "path": stale.to_string_lossy(),
            "content": "line two\n",
        }))
        .expect("the normalized target needs filesystem write");
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": stale.to_string_lossy(),
                "content": "line two\n",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["appended"], true);
    assert_eq!(
        output.content["normalized_from"],
        stale.to_string_lossy().as_ref()
    );
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "line one\nline two\n"
    );
    assert!(!home.join("Desktop").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn append_file_keeps_explicit_path_when_conventional_parent_exists() {
    let (home, desktop) = stale_home("utsuwa-append-ambiguous");
    let conventional_dir = home.join("Desktop");
    std::fs::create_dir_all(&conventional_dir).unwrap();
    let conventional = conventional_dir.join("note.txt");
    std::fs::write(&conventional, "conventional\n").unwrap();
    let configured = desktop.join("note.txt");
    std::fs::write(&configured, "configured\n").unwrap();
    let tool = AppendFileTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    tool.invoke(
        ticketed_context(&conventional),
        serde_json::json!({
            "path": conventional.to_string_lossy(),
            "content": "more\n",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&conventional).unwrap(),
        "conventional\nmore\n"
    );
    assert_eq!(
        std::fs::read_to_string(&configured).unwrap(),
        "configured\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_aware_write_normalizes_a_stale_conventional_path() {
    let (home, desktop) = stale_home("utsuwa-write-remap");
    let stale = home.join("Desktop").join("fresh.txt");
    let candidate = desktop.join("fresh.txt");
    let tool = HostAwareWriteTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let requirement = tool
        .required_capability(&serde_json::json!({
            "path": stale.to_string_lossy(),
            "content": "hello",
        }))
        .expect("the normalized target needs filesystem write");
    // The missing file resolves against its nearest existing ancestor.
    assert_eq!(
        requirement.resource,
        Resource::Path(desktop.canonicalize().unwrap().join("fresh.txt"))
    );
    let output = tool
        .invoke(
            ticketed_context(&candidate),
            serde_json::json!({
                "path": stale.to_string_lossy(),
                "content": "hello",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["path"], candidate.to_string_lossy().as_ref());
    assert_eq!(
        output.content["normalized_from"],
        stale.to_string_lossy().as_ref()
    );
    assert_eq!(std::fs::read_to_string(&candidate).unwrap(), "hello");
    assert!(!home.join("Desktop").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_aware_write_keeps_explicit_path_when_both_directories_exist() {
    let (home, desktop) = stale_home("utsuwa-write-ambiguous");
    let conventional_dir = home.join("Desktop");
    std::fs::create_dir_all(&conventional_dir).unwrap();
    let conventional = conventional_dir.join("fresh.txt");
    let configured = desktop.join("fresh.txt");
    let tool = HostAwareWriteTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    tool.invoke(
        ticketed_context(&conventional),
        serde_json::json!({
            "path": conventional.to_string_lossy(),
            "content": "explicit",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&conventional).unwrap(), "explicit");
    assert!(!configured.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_routes_location_style_to_the_configured_directory() {
    let (home, desktop) = stale_home("utsuwa-edit-unified-location");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "hello").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "hello",
                "new_text": "hello world",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["updated"], true);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello world");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_repairs_a_displayed_file_path_in_the_directory_field() {
    let (home, desktop) = stale_home("utsuwa-edit-directory-file-path");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": target.to_string_lossy(),
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09\n");
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    assert_eq!(output.content["directory"], "desktop");
    assert_eq!(output.content["relative_path"], "note.txt");
    assert_eq!(
        output.content["file"]["display_path"],
        target.to_string_lossy().as_ref()
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_accepts_a_file_ref_for_follow_up_edits() {
    let (home, desktop) = stale_home("utsuwa-edit-file-ref");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Version 1\n").unwrap();
    let host = environment(&home, &desktop);
    let tool = EditTool::new(tool_filesystem::FilesystemLimits::default(), host);
    let first = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "target": {
                    "directory": "desktop",
                    "relative_path": "note.txt"
                },
                "operation": {
                    "type": "replace",
                    "old_text": "Version 1",
                    "new_text": "Version 2"
                }
            }),
        )
        .await
        .unwrap();
    let file_ref = first.content["file_ref"].as_str().unwrap().to_string();
    assert_eq!(file_ref, "file:desktop:note.txt");

    let second = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "file_ref": file_ref,
                "operation": {
                    "type": "append",
                    "text": "follow-up\n"
                }
            }),
        )
        .await
        .unwrap();
    assert_eq!(second.content["file_ref"], "file:desktop:note.txt");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "Version 2\nfollow-up\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_rejects_a_stale_file_ref_without_recreating_the_file() {
    let (home, desktop) = stale_home("utsuwa-edit-stale-ref");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Version 1").unwrap();
    let file_ref = FileRef::from_target(&FileTarget {
        directory: UserDirectory::Desktop,
        relative_path: PathBuf::from("note.txt"),
    })
    .unwrap();
    std::fs::remove_file(&target).unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "file_ref": file_ref,
                "old_text": "Version 1",
                "new_text": "Version 2"
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ToolError::Filesystem {
            code,
            retryable: false,
            ..
        } if code == "stale_file_ref"
    ));
    assert!(!target.exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_uses_active_file_context_for_a_targetless_follow_up() {
    let (home, desktop) = stale_home("utsuwa-edit-active-file");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "first\n").unwrap();
    let context = Arc::new(Mutex::new(ConversationFileContext::default()));
    context.lock().unwrap().record_success(
        FileRef::from_target(&FileTarget {
            directory: UserDirectory::Desktop,
            relative_path: PathBuf::from("note.txt"),
        })
        .unwrap(),
    );
    let tool = EditTool::new_with_context(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
        Some(context),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "operation": { "type": "append", "text": "second\n" }
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\nsecond\n");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn failed_explicit_target_does_not_replace_the_active_file() {
    let (home, desktop) = stale_home("utsuwa-edit-active-failure");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "first\n").unwrap();
    let context = Arc::new(Mutex::new(ConversationFileContext::default()));
    context.lock().unwrap().record_success(
        FileRef::from_target(&FileTarget {
            directory: UserDirectory::Desktop,
            relative_path: PathBuf::from("note.txt"),
        })
        .unwrap(),
    );
    let tool = EditTool::new_with_context(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
        Some(Arc::clone(&context)),
    );
    let failed = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "target": {
                    "directory": "desktop",
                    "relative_path": "../outside.txt"
                },
                "new_text": "bad"
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(failed, ToolError::Filesystem { .. }));
    assert_eq!(
        context.lock().unwrap().active().unwrap().as_str(),
        "file:desktop:note.txt"
    );

    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "old_text": "first",
            "new_text": "updated"
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "updated\n");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_infers_the_only_non_empty_line_without_reading() {
    let (home, desktop) = stale_home("utsuwa-edit-single-line-inference");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let schema = tool.metadata().input_schema;
    assert!(schema.get("required").is_none());
    assert_eq!(
        schema["properties"]["new_text_source"]["enum"],
        serde_json::json!(["current_date", "current_time", "current_datetime"])
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["updated"], true);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09\n");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_updates_one_unambiguous_date_line_from_file_ref_and_location() {
    let (home, desktop) = stale_home("utsuwa-edit-date-follow-up");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2023-10-05\nCurrent hour: 14\n").unwrap();
    let file_ref = FileRef::from_target(&FileTarget {
        directory: UserDirectory::Desktop,
        relative_path: PathBuf::from("note.txt"),
    })
    .unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "file_ref": file_ref,
                "location": "desktop",
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["updated"], true);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "2026-09-09\nCurrent hour: 14\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_recovers_the_real_path_then_completes_the_date_retry() {
    let (home, desktop) = stale_home("utsuwa-edit-real-path-retry");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2023-10-05\nCurrent hour: 14\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let first = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": target,
                "filename": "note.txt",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(first, ToolError::RetryRequired { .. }),
        "{first:?}"
    );
    assert!(!first.to_string().contains("same file"));

    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "new_text": "2026-09-09",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "2026-09-09\nCurrent hour: 14\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_appends_current_time_when_replace_old_text_is_missing() {
    let (home, desktop) = stale_home("utsuwa-edit-clock-append");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-09\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text_source": "current_time",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["appended"], true);
    let contents = std::fs::read_to_string(&target).unwrap();
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "2026-09-09");
    assert!(looks_like_clock_text(lines[1]), "{contents:?}");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resolves_current_hour_placeholder_before_appending() {
    let (home, desktop) = stale_home("utsuwa-edit-clock-placeholder");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "header\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "new_text": "current hour",
        }),
    )
    .await
    .unwrap();
    let contents = std::fs::read_to_string(&target).unwrap();
    let lines = contents.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0], "header");
    assert!(looks_like_clock_text(lines[1]), "{contents:?}");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_accepts_a_full_filename_path_hint() {
    let (home, desktop) = stale_home("utsuwa-edit-full-filename-hint");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "before").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "path": target,
            "filename": target,
            "old_text": "before",
            "new_text": "after",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_merges_new_text_across_a_multiline_retry() {
    let (home, desktop) = stale_home("utsuwa-edit-merge-new");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Version 1\nDetails\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let first = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "Version 2",
            }),
        )
        .await
        .unwrap_err();
    match &first {
        ToolError::RetryRequired { recovery, .. } => {
            assert_eq!(recovery["error"], "old_text_required");
            assert_eq!(recovery["preserved"], serde_json::json!(["new_text"]));
            assert_eq!(recovery["next_tool"], "filesystem.read");
        }
        other => panic!("expected structured retry, got {other:?}"),
    }
    assert!(!first.to_string().contains("Version 1"));

    let retry = serde_json::json!({ "old_text": "Version 1" });
    let requirement = tool
        .required_capability(&retry)
        .expect("pending target must authorize the retry");
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    tool.invoke(ticketed_context(&target), retry).await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "Version 2\nDetails\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_preserves_old_text_for_a_reverse_partial_retry() {
    let (home, desktop) = stale_home("utsuwa-edit-merge-old");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Version 1\nDetails\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let first = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "old_text": "Version 1",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(first, ToolError::RetryRequired { .. }),
        "{first:?}"
    );
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({ "new_text": "Version 2" }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "Version 2\nDetails\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resets_pending_values_when_the_target_changes() {
    let (home, desktop) = stale_home("utsuwa-edit-target-reset");
    let documents = home.join("Documentos");
    std::fs::create_dir_all(&documents).unwrap();
    let note = desktop.join("note.txt");
    let report = documents.join("report.txt");
    std::fs::write(&note, "Note 1\nDetails\n").unwrap();
    std::fs::write(&report, "Report 1\nDetails\n").unwrap();
    let mut host = environment(&home, &desktop);
    host.user_dirs.documents = Some(documents.clone());
    let tool = EditTool::new(tool_filesystem::FilesystemLimits::default(), host);

    let first = tool
        .invoke(
            ticketed_context(&note),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "Note 2",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(first, ToolError::RetryRequired { .. }),
        "{first:?}"
    );

    let changed_target = tool
        .invoke(
            ticketed_context(&report),
            serde_json::json!({
                "location": "documents",
                "filename": "report.txt",
                "old_text": "Report 1",
            }),
        )
        .await
        .unwrap_err();
    match changed_target {
        ToolError::RetryRequired { recovery, .. } => {
            assert_eq!(recovery["target"], report.to_string_lossy().as_ref());
            assert_eq!(recovery["preserved"], serde_json::json!(["old_text"]));
        }
        other => panic!("expected report retry, got {other:?}"),
    }

    tool.invoke(
        ticketed_context(&report),
        serde_json::json!({ "new_text": "Report 2" }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&note).unwrap(), "Note 1\nDetails\n");
    assert_eq!(
        std::fs::read_to_string(&report).unwrap(),
        "Report 2\nDetails\n"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resolves_new_text_from_the_native_clock() {
    let (home, desktop) = stale_home("utsuwa-edit-text-source");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Version 1\nDetails\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let expected = chrono::Local::now().format("%Y-%m-%d").to_string();
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "old_text": "Version 1",
            "new_text_source": "current_date",
        }),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        format!("{expected}\nDetails\n")
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resolves_a_unique_bare_filename_to_desktop() {
    let (home, desktop) = stale_home("utsuwa-edit-bare-filename");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "old date").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "filename": "note.txt",
                "old_text": "old date",
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["location"], "desktop");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resolves_a_relative_path_alias_to_desktop() {
    let (home, desktop) = stale_home("utsuwa-edit-relative-path-alias");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "old date").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let requirement = tool
        .required_capability(&serde_json::json!({
            "path": "note.txt",
            "old_text": "old date",
            "new_text": "2026-09-09",
        }))
        .expect("the relative compatibility target needs filesystem write");
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "path": "note.txt",
                "old_text": "old date",
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(output.content["location"], "desktop");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "2026-09-09");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_reuses_the_last_target_for_a_targetless_retry() {
    let (home, desktop) = stale_home("utsuwa-edit-targetless-retry");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "Updated date\nDetails").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let first = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "2026-09-09",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(first, ToolError::RetryRequired { .. }),
        "{first:?}"
    );

    let retry_args = serde_json::json!({
        "old_text": "Updated date",
        "new_text": "2026-09-09",
    });
    let requirement = tool
        .required_capability(&retry_args)
        .expect("the remembered target needs filesystem write");
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    let output = tool
        .invoke(ticketed_context(&target), retry_args)
        .await
        .unwrap();
    assert_eq!(output.content["path"], target.to_string_lossy().as_ref());
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "2026-09-09\nDetails"
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_routes_path_style_through_normalization() {
    let (home, desktop) = stale_home("utsuwa-edit-unified-path");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "old value").unwrap();
    let stale = home.join("Desktop").join("note.txt");
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let requirement = tool
        .required_capability(&serde_json::json!({
            "path": stale.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        }))
        .expect("the normalized target needs filesystem write");
    assert_eq!(
        requirement.resource,
        Resource::Path(target.canonicalize().unwrap_or(target.clone()))
    );
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "path": stale.to_string_lossy(),
            "old_text": "old",
            "new_text": "new",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new value");
    assert!(!home.join("Desktop").exists());
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_rejects_mixed_or_missing_target_styles() {
    let (home, desktop) = stale_home("utsuwa-edit-unified-styles");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "hello").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    // A small model may copy `resolved_directory` into `path` while
    // also providing the semantic file target. The directory hint is
    // safe to ignore because location+filename identifies the file.
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "path": desktop.to_string_lossy(),
            "old_text": "hello",
            "new_text": "hi",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    std::fs::write(&target, "hello").unwrap();

    // A redundant matching path is safe and keeps older/model-generated
    // calls from failing before the edit reaches the filesystem broker.
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "path": target.to_string_lossy(),
            "old_text": "hello",
            "new_text": "hi",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    std::fs::write(&target, "hello").unwrap();

    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "filename": "note.txt",
            "path": "note.txt",
            "old_text": "hello",
            "new_text": "hi",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    std::fs::write(&target, "hello").unwrap();

    // A follow-up model call may include both the stable reference and
    // the semantic target copied from the prior result. Equal selectors
    // must be collapsed instead of rejected.
    let file_ref = FileRef::from_target(&FileTarget {
        directory: UserDirectory::Desktop,
        relative_path: PathBuf::from("note.txt"),
    })
    .unwrap();
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "file_ref": file_ref,
            "target": {
                "directory": "desktop",
                "relative_path": "note.txt"
            },
            "old_text": "hello",
            "new_text": "hi",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    std::fs::write(&target, "hello").unwrap();

    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "path": home.join("other.txt").to_string_lossy(),
                "old_text": "hello",
                "new_text": "hi",
            }),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ToolError::RetryRequired { .. }),
        "{error:?}"
    );
    assert!(error.to_string().contains("same file"));

    // A location-only call can infer the sole direct file in the
    // configured directory, which is useful for small-model retries.
    tool.invoke(
        ticketed_context(&target),
        serde_json::json!({
            "location": "desktop",
            "old_text": "hello",
            "new_text": "hi",
        }),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi");
    std::fs::write(&target, "hello").unwrap();

    let error = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "old_text": "hello",
                "new_text": "hi",
            }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn unified_edit_resolves_model_date_placeholder() {
    let (home, desktop) = stale_home("utsuwa-edit-date-placeholder");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let tool = EditTool::new(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );
    let expected = chrono::Local::now().format("%Y-%m-%d").to_string();
    let output = tool
        .invoke(
            ticketed_context(&target),
            serde_json::json!({
                "location": "desktop",
                "filename": "note.txt",
                "new_text": "Updated date",
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["updated"], true);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        format!("{expected}\n")
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn raw_write_error_explains_the_configured_localized_desktop() {
    let home = PathBuf::from("/home/meme");
    let environment = environment(&home, &home.join("Escritorio"));
    let error = ToolError::Failed {
        tool: "filesystem".to_string(),
        message: "parent directory does not exist: /home/meme/Desktop".to_string(),
    };
    let enriched = enrich_write_error(error, &environment);
    assert!(enriched.to_string().contains("/home/meme/Escritorio"));
    assert!(enriched.to_string().contains("filesystem.create_user_file"));
}

fn list_tool(home: &Path, desktop: &Path) -> HostAwarePathTool {
    HostAwarePathTool::list(
        tool_filesystem::FilesystemLimits::default(),
        environment(home, desktop),
    )
}

#[tokio::test]
async fn host_list_accepts_semantic_directory_target() {
    let (home, desktop) = stale_home("host-list-semantic-target");
    std::fs::write(desktop.join("note.txt"), "hi\n").unwrap();
    let tool = list_tool(&home, &desktop);

    let output = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({
                "target": { "directory": "desktop", "relative_path": "" },
            }),
        )
        .await
        .unwrap();
    let names: Vec<&str> = output.content["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|entry| entry["name"].as_str())
        .collect();
    assert!(names.contains(&"note.txt"));
    assert_eq!(output.content["file_ref"], "file:desktop:");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_list_resolves_absolute_directory_and_tags_it() {
    let (home, desktop) = stale_home("host-list-absolute-dir");
    std::fs::write(desktop.join("note.txt"), "hi\n").unwrap();
    let tool = list_tool(&home, &desktop);

    let output = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({ "path": desktop.to_string_lossy() }),
        )
        .await
        .unwrap();
    assert!(output.content["entries"].as_array().unwrap().len() >= 1);
    assert_eq!(output.content["file_ref"], "file:desktop:");
    assert_eq!(output.content["directory"], "desktop");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn host_list_follows_directory_file_ref() {
    let (home, desktop) = stale_home("host-list-dir-ref");
    std::fs::write(desktop.join("note.txt"), "hi\n").unwrap();
    let tool = list_tool(&home, &desktop);

    let first = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({ "path": desktop.to_string_lossy() }),
        )
        .await
        .unwrap();
    let file_ref = first.content["file_ref"].as_str().unwrap().to_string();

    let second = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({ "file_ref": file_ref }),
        )
        .await
        .unwrap();
    assert_eq!(second.content["entries"], first.content["entries"]);
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn malformed_file_ref_with_valid_path_resolves_the_path() {
    // A small model echoed a bare filename as file_ref while also sending
    // the real path. The malformed reference identifies no file, so the
    // call resolves the valid selector instead of failing.
    let (home, desktop) = stale_home("host-read-malformed-ref");
    let target = desktop.join("note.txt");
    std::fs::write(&target, "2026-09-08\n").unwrap();
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let output = tool
        .invoke(
            read_ticketed_context(&target),
            serde_json::json!({
                "file_ref": "note.txt",
                "path": target.to_string_lossy(),
            }),
        )
        .await
        .unwrap();
    assert_eq!(output.content["content"], "2026-09-08\n");
    assert_eq!(output.content["file_ref"], "file:desktop:note.txt");
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn malformed_file_ref_alone_still_fails_without_context() {
    let (home, desktop) = stale_home("host-read-malformed-alone");
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let error = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({ "file_ref": "note.txt" }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Filesystem { .. }));
    assert!(error.model_message().contains("invalid_file_ref"));
    std::fs::remove_dir_all(&home).unwrap();
}

#[tokio::test]
async fn read_on_directory_still_rejected_as_retryable() {
    // Directories keep their file-oriented error for file tools; only
    // `filesystem.list` resolves them.
    let (home, desktop) = stale_home("host-read-dir");
    let tool = HostAwarePathTool::read(
        tool_filesystem::FilesystemLimits::default(),
        environment(&home, &desktop),
    );

    let error = tool
        .invoke(
            read_ticketed_context(&desktop),
            serde_json::json!({ "path": desktop.to_string_lossy() }),
        )
        .await
        .unwrap_err();
    assert!(matches!(error, ToolError::Filesystem { .. }));
    assert!(error.model_message().contains("directory"));
    std::fs::remove_dir_all(&home).unwrap();
}
