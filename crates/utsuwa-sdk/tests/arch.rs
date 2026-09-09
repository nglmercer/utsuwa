//! Architecture guard: `app-host` is the composition root.
//!
//! Practically everything may be imported *by* `app-host`, but almost
//! nothing may import *from* it. This test scans every workspace member
//! manifest and fails if any crate besides `app-host` itself declares a
//! dependency on it (normal, dev, build, or target-specific).

use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..")
}

fn section_is_deps(header: &str) -> bool {
    header == "dependencies"
        || header == "dev-dependencies"
        || header == "build-dependencies"
        || (header.starts_with("target.") && header.ends_with(".dependencies"))
}

fn section_is_app_host(header: &str) -> bool {
    header == "dependencies.app-host"
        || header == "dev-dependencies.app-host"
        || header == "build-dependencies.app-host"
        || (header.starts_with("target.") && header.ends_with(".dependencies.app-host"))
}

fn manifest_depends_on_app_host(manifest: &Path) -> bool {
    let content = std::fs::read_to_string(manifest)
        .unwrap_or_else(|_| panic!("cannot read {}", manifest.display()));
    let mut in_deps = false;
    for raw in content.lines() {
        let line = raw.trim();
        if let Some(header) = line.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
            let header = header.trim().to_ascii_lowercase();
            if section_is_app_host(&header) {
                return true;
            }
            in_deps = section_is_deps(&header);
            continue;
        }
        if !in_deps {
            continue;
        }
        let code = line.split('#').next().unwrap_or("").trim();
        if let Some((key, _)) = code.split_once('=') {
            let key = key.trim();
            if key == "app-host" || key.starts_with("app-host.") {
                return true;
            }
        }
    }
    false
}

#[test]
fn leaf_crates_do_not_depend_on_app_host() {
    let root = workspace_root();
    let crates_dir = root.join("crates");
    let mut checked = 0;
    let mut entries: Vec<_> = std::fs::read_dir(&crates_dir)
        .expect("workspace crates dir must exist")
        .filter_map(|entry| entry.ok())
        .collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let manifest = entry.path().join("Cargo.toml");
        if !manifest.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "app-host" {
            continue;
        }
        assert!(
            !manifest_depends_on_app_host(&manifest),
            "crate '{name}' must not depend on app-host (composition root direction)"
        );
        checked += 1;
    }
    assert!(checked > 10, "expected to check many workspace crates");
}
