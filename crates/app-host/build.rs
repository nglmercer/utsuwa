use std::{env, path::PathBuf, process::Command};

const WEB_INPUTS: &[&str] = &[
    "package.json",
    "pnpm-lock.yaml",
    "svelte.config.js",
    "tsconfig.json",
    "vite.config.ts",
    "scripts/vite-native.mjs",
    "src",
    "static",
];

fn main() {
    println!("cargo:rerun-if-env-changed=UTSUWA_SKIP_WEB_BUILD");

    let manifest_dir = PathBuf::from(
        env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by Cargo"),
    );
    let workspace_root = manifest_dir.join("../..");

    for input in WEB_INPUTS {
        println!(
            "cargo:rerun-if-changed={}",
            workspace_root.join(input).display()
        );
    }

    // Build-time WebKitGTK/GTK versions for `--debug` startup diagnostics.
    // Best-effort only: a failed probe records "unknown" instead of failing
    // the build (the app must also compile where pkg-config is absent).
    probe_pkg_version("webkit2gtk-4.1", "UTSUWA_WEBKIT_PC_VERSION");
    probe_pkg_version("gtk+-3.0", "UTSUWA_GTK_PC_VERSION");

    let skip_web_build = env::var("UTSUWA_SKIP_WEB_BUILD")
        .map(|value| matches!(value.as_str(), "1" | "true"))
        .unwrap_or(false);
    if skip_web_build {
        println!("cargo:warning=skipping bundled frontend build (UTSUWA_SKIP_WEB_BUILD is set)");
        return;
    }

    let pnpm = if cfg!(windows) { "pnpm.cmd" } else { "pnpm" };
    let status = Command::new(pnpm)
        .arg("build:native")
        .current_dir(&workspace_root)
        .status()
        .unwrap_or_else(|error| {
            panic!(
                "failed to run `{pnpm} build:native` from {}: {error}",
                workspace_root.display()
            )
        });

    if !status.success() {
        panic!(
            "bundled frontend build failed with status {status}; run `pnpm build:native` from {} for details",
            workspace_root.display()
        );
    }
}

/// Best-effort `pkg-config --modversion` probe. Emits
/// `cargo:rustc-env=<ENV>=<version|unknown>`; never fails the build.
fn probe_pkg_version(package: &str, env_name: &str) {
    let version = Command::new("pkg-config")
        .args(["--modversion", package])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|version| !version.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env={env_name}={version}");
}
