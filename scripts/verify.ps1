$ErrorActionPreference = "Stop"

$repoRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
Push-Location $repoRoot
try {
    # The host's bundled frontend is checked by the frontend commands below.
    $env:UTSUWA_SKIP_WEB_BUILD = "1"

    cargo fmt --all -- --check
    cargo check --workspace --locked
    cargo test --workspace --locked
    cargo clippy --workspace --all-targets --all-features --locked -- -D warnings

    pnpm check
    pnpm test
}
finally {
    Pop-Location
}
