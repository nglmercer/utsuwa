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
