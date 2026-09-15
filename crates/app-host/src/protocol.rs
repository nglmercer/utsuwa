//! Production asset serving over the custom scheme (plan Phase 3).
//!
//! - Only `companion://app/*` is served, from a single canonicalized root.
//! - `..` escapes and symlinks pointing outside the root fail closed.
//! - Extensionless paths fall back to `index.html` (SvelteKit SPA fallback).
//! - Navigation outside the scheme is blocked; external links must go
//!   through an explicit `host.open_external_url` approval (Task 6+).

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use wry::http::{Request, Response, StatusCode};

pub const APP_SCHEME: &str = "companion";
pub const APP_HOST: &str = "app";
/// Serialized origin of the bundled app, the only origin the asset server
/// ever reflects in `Access-Control-Allow-Origin`.
pub const APP_ORIGIN: &str = "companion://app";

/// Initial page: the app route, served through the SPA fallback.
pub fn initial_url() -> String {
    format!("{APP_SCHEME}://{APP_HOST}/app")
}

/// Bounded startup diagnostics for the bundled asset root.
#[derive(Debug, Clone)]
pub struct AssetReport {
    pub root: PathBuf,
    pub index_exists: bool,
    pub index_size: u64,
    pub index_modified_secs: Option<u64>,
    pub file_count: usize,
    /// True when the walk hit its entry cap (count is a lower bound).
    pub truncated: bool,
}

#[derive(Debug, Clone)]
pub struct AssetServer {
    root: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum AssetError {
    #[error("asset root is not a directory: {0}")]
    BadRoot(PathBuf),
}

impl AssetServer {
    pub fn new(root: PathBuf) -> Result<Self, AssetError> {
        let canonical = root.canonicalize().map_err(|_| AssetError::BadRoot(root))?;
        if !canonical.is_dir() {
            return Err(AssetError::BadRoot(canonical));
        }
        Ok(Self { root: canonical })
    }

    /// Resolve `companion://app/<path>` to a file inside the root.
    /// Returns `None` when the URL is outside the scheme/host or escapes.
    ///
    /// `..` segments are rejected outright (asset names never need them).
    /// Existing paths are canonicalized and prefix-checked so a symlink
    /// inside the root cannot point outside it; missing paths (SPA routes,
    /// 404s) are verified through their nearest existing ancestor, whose
    /// non-existent tail cannot traverse a symlink.
    fn resolve(&self, uri: &wry::http::Uri) -> Option<PathBuf> {
        if uri.scheme_str() != Some(APP_SCHEME) {
            return None;
        }
        if uri.host() != Some(APP_HOST) {
            return None;
        }
        let decoded = percent_decode(uri.path());
        let segments: Vec<&str> = decoded.split('/').filter(|s| !s.is_empty()).collect();
        if segments.contains(&"..") {
            return None;
        }
        let mut candidate = self.root.clone();
        for segment in &segments {
            if *segment != "." {
                candidate.push(segment);
            }
        }
        // Walk up to the nearest existing ancestor and verify it is inside
        // the root; re-append the (plain-name, symlink-free) tail.
        let mut ancestor = candidate.clone();
        let mut tail = Vec::new();
        while !ancestor.exists() {
            if ancestor == self.root {
                break;
            }
            tail.push(ancestor.file_name()?.to_os_string());
            ancestor.pop();
        }
        let canonical = ancestor.canonicalize().ok()?;
        if !canonical.starts_with(&self.root) {
            return None;
        }
        let mut resolved = canonical;
        for part in tail.iter().rev() {
            resolved.push(part);
        }
        Some(resolved)
    }

    /// Canonical asset root (already canonicalized at construction).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Bounded verification snapshot for startup logging: index presence,
    /// size, mtime, and a capped file count. Never follows symlinks out of
    /// the root (the walk stays within canonicalized entries).
    pub fn report(&self) -> AssetReport {
        const MAX_ENTRIES: usize = 50_000;
        const MAX_DEPTH: usize = 12;
        let index = self.root.join("index.html");
        let (index_exists, index_size, index_modified_secs) =
            std::fs::metadata(&index).map_or((false, 0, None), |meta| {
                let modified = meta
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs());
                (meta.is_file(), meta.len(), modified)
            });
        let mut file_count = 0usize;
        let mut truncated = false;
        let mut stack = vec![(self.root.clone(), 0usize)];
        'walk: while let Some((dir, depth)) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if file_count >= MAX_ENTRIES {
                    truncated = true;
                    break 'walk;
                }
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                if kind.is_file() {
                    file_count += 1;
                } else if kind.is_dir() && depth < MAX_DEPTH {
                    stack.push((entry.path(), depth + 1));
                }
            }
        }
        AssetReport {
            root: self.root.clone(),
            index_exists,
            index_size,
            index_modified_secs,
            file_count,
            truncated,
        }
    }

    pub fn handle(&self, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
        let started = std::time::Instant::now();
        let method = request.method().to_string();
        let uri = request.uri().clone();
        let allow_origin = cors_allow_origin(&request);
        let (path_label, response) = match self.resolve(&uri) {
            None => (
                "<rejected>".to_string(),
                status(StatusCode::FORBIDDEN, "forbidden", allow_origin),
            ),
            Some(mut path) => {
                if path.is_dir() {
                    path.push("index.html");
                }
                if !path.is_file() && is_spa_route(uri.path()) {
                    // SPA route (e.g. `/app`, `/app/settings`) → client-side router.
                    path = self.root.join("index.html");
                }
                let label = path.display().to_string();
                let response = match std::fs::read(&path) {
                    Ok(bytes) => body(
                        StatusCode::OK,
                        mime_for(&path),
                        Cow::Owned(bytes),
                        allow_origin,
                    ),
                    Err(_) => status(StatusCode::NOT_FOUND, "not found", allow_origin),
                };
                (label, response)
            }
        };
        // Per-request trace (method/URI/status/MIME/bytes/elapsed); failures
        // additionally surface at debug so `--debug` shows them without
        // `--trace`. No file contents or secrets are ever logged.
        let status = response.status();
        let mime = response
            .headers()
            .get("Content-Type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("?");
        tracing::trace!(
            method = %method,
            uri = %uri,
            path = %path_label,
            status = status.as_u16(),
            mime,
            bytes = response.body().len(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "webview.asset.request"
        );
        if status != StatusCode::OK {
            tracing::debug!(
                method = %method,
                uri = %uri,
                status = status.as_u16(),
                "webview.asset.failed"
            );
        }
        response
    }
}

/// Path looks like a client-side route (last segment has no file extension)
/// rather than a real asset reference.
fn is_spa_route(path: &str) -> bool {
    path.split('/')
        .next_back()
        .is_some_and(|last| !last.contains('.'))
}

fn status(
    code: StatusCode,
    message: &'static str,
    allow_origin: &'static str,
) -> Response<Cow<'static, [u8]>> {
    body(
        code,
        "text/plain",
        Cow::Borrowed(message.as_bytes()),
        allow_origin,
    )
}

fn body(
    code: StatusCode,
    content_type: &'static str,
    bytes: Cow<'static, [u8]>,
    allow_origin: &'static str,
) -> Response<Cow<'static, [u8]>> {
    let mut builder = Response::builder()
        .status(code)
        .header("Content-Type", content_type)
        .header("Access-Control-Allow-Origin", allow_origin);
    if allow_origin == APP_ORIGIN {
        builder = builder.header("Vary", "Origin");
    }
    builder
        .body(bytes)
        // Builder inputs are hardcoded-valid; fall back to an empty body
        // instead of unwrapping on the serving path.
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[])))
}

/// Narrow CORS policy for the custom origin.
///
/// Same-origin subresource loads (scripts, styles, modules, workers) from
/// `companion://app` never consult this header — it only matters for
/// CORS-mode fetches. When the request carries the single expected app
/// origin, reflect it so credentialed same-app fetches succeed; navigations
/// (no `Origin` header) and opaque contexts (`Origin: null`) keep the
/// historical `null`. Never `*`: no other origin may read app assets.
fn cors_allow_origin(request: &Request<Vec<u8>>) -> &'static str {
    let origin = request
        .headers()
        .get("Origin")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    if origin == APP_ORIGIN {
        APP_ORIGIN
    } else {
        "null"
    }
}

fn mime_for(path: &Path) -> &'static str {
    // Case-insensitive: some pipelines emit `.JS`/`.CSS`. Standards-compatible
    // values throughout (`text/javascript` per RFC 9239, not `application/javascript`).
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        "xml" => "application/xml",
        _ => "application/octet-stream",
    }
}

fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut bytes = input.as_bytes().iter();
    while let Some(&b) = bytes.next() {
        if b == b'%' {
            let (Some(&hi), Some(&lo)) = (bytes.next(), bytes.next()) else {
                out.push(b'%');
                continue;
            };
            if let (Some(h), Some(l)) = (hex_val(hi), hex_val(lo)) {
                out.push(h << 4 | l);
            } else {
                out.extend_from_slice(&[b'%', hi, lo]);
            }
        } else {
            out.push(b);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Navigation policy (plan Phase 3): the WebView may only navigate inside
/// `companion://app/*`. Dev mode additionally allows `http://localhost:*`.
/// `https://*`, `http://*` (non-localhost), `file://*`, and unknown schemes
/// are blocked — external links become explicit host operations.
pub fn is_navigation_allowed(url: &str, dev_mode: bool) -> bool {
    let custom_scheme = url::Url::parse(url).ok().is_some_and(|parsed| {
        parsed.scheme() == APP_SCHEME
            && parsed.host_str() == Some(APP_HOST)
            // Bare `companion://app` (empty path) is the same document as
            // `companion://app/`; both resolve to the bundled index.
            && (parsed.path().is_empty() || parsed.path().starts_with('/'))
    });
    if custom_scheme {
        return true;
    }
    if dev_mode && crate::is_dev_url(url) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_with(files: &[(&str, &str)]) -> (tempfile_like::Dir, AssetServer) {
        let dir = tempfile_like::Dir::create();
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        let server = AssetServer::new(dir.path().to_path_buf()).unwrap();
        (dir, server)
    }

    fn get(server: &AssetServer, url: &str) -> Response<Cow<'static, [u8]>> {
        get_with_origin(server, url, None)
    }

    fn get_with_origin(
        server: &AssetServer,
        url: &str,
        origin: Option<&str>,
    ) -> Response<Cow<'static, [u8]>> {
        let mut builder = Request::builder().uri(url);
        if let Some(origin) = origin {
            builder = builder.header("Origin", origin);
        }
        server.handle(builder.body(Vec::new()).unwrap())
    }

    #[test]
    fn serves_files_with_mime_types() {
        let (_d, s) = server_with(&[("app.js", "console.log(1)")]);
        let res = get(&s, "companion://app/app.js");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"console.log(1)");
    }

    #[test]
    fn app_assets_still_go_to_the_asset_server() {
        let (_d, s) = server_with(&[("app.js", "console.log(1)")]);
        assert!(!crate::audio::is_media_path("/app.js"));
        let res = get(&s, "companion://app/app.js");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["Content-Type"], "text/javascript");
    }

    #[test]
    fn traversal_and_wrong_host_fail_closed() {
        let (_d, s) = server_with(&[("index.html", "hi")]);
        assert_eq!(
            get(&s, "companion://app/../secret").status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&s, "companion://app/%2e%2e/secret").status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&s, "companion://evil/index.html").status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            get(&s, "https://example.com/").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_root_cannot_point_out() {
        use std::os::unix::fs::symlink;
        let (d, s) = server_with(&[("index.html", "hi")]);
        symlink("/etc", d.path().join("escaped")).unwrap();
        assert_eq!(
            get(&s, "companion://app/escaped/passwd").status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn spa_routes_fall_back_to_index() {
        let (_d, s) = server_with(&[("index.html", "<app>")]);
        for route in [
            "companion://app/app",
            "companion://app/app/",
            "companion://app/app/settings",
            "companion://app/",
            "companion://app",
        ] {
            let res = get(&s, route);
            assert_eq!(res.status(), StatusCode::OK, "{route}");
            assert_eq!(res.body().as_ref(), b"<app>", "{route}");
            assert_eq!(res.headers()["Content-Type"], "text/html", "{route}");
        }
        // Unknown extension-bearing file is a real 404, not the fallback.
        assert_eq!(
            get(&s, "companion://app/missing.js").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn nested_generated_assets_are_served_not_fallen_back() {
        let (_d, s) = server_with(&[
            ("index.html", "<app>"),
            ("_app/immutable/entry/app.HASH.js", "js"),
            ("_app/immutable/chunks/x.HASH.css", "css"),
            ("favicon.svg", "<svg/>"),
        ]);
        let res = get(&s, "companion://app/_app/immutable/entry/app.HASH.js");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"js");
        assert_eq!(res.headers()["Content-Type"], "text/javascript");
        let res = get(&s, "companion://app/_app/immutable/chunks/x.HASH.css");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["Content-Type"], "text/css");
        let res = get(&s, "companion://app/favicon.svg");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.headers()["Content-Type"], "image/svg+xml");
        // Missing nested asset stays a 404 (no index fallback for files).
        assert_eq!(
            get(&s, "companion://app/_app/immutable/missing.js").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn query_strings_do_not_change_resolution() {
        let (_d, s) = server_with(&[("index.html", "<app>"), ("app.js", "js")]);
        let res = get(&s, "companion://app/app.js?v=123");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"js");
        let res = get(&s, "companion://app/app?onboarding=1");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"<app>");
    }

    #[test]
    fn mime_table_covers_bundled_frontend_types() {
        for (name, mime) in [
            ("a.html", "text/html"),
            ("a.htm", "text/html"),
            ("a.js", "text/javascript"),
            ("a.mjs", "text/javascript"),
            ("a.JS", "text/javascript"),
            ("a.css", "text/css"),
            ("a.CSS", "text/css"),
            ("a.json", "application/json"),
            ("a.map", "application/json"),
            ("a.wasm", "application/wasm"),
            ("a.svg", "image/svg+xml"),
            ("a.png", "image/png"),
            ("a.webp", "image/webp"),
            ("a.avif", "image/avif"),
            ("a.woff", "font/woff"),
            ("a.woff2", "font/woff2"),
            ("a.ttf", "font/ttf"),
            ("a.mp4", "video/mp4"),
            ("a.webm", "video/webm"),
            ("a.bin", "application/octet-stream"),
        ] {
            let (_d, s) = server_with(&[("index.html", "<app>"), (name, "x")]);
            let res = get(&s, &format!("companion://app/{name}"));
            assert_eq!(res.status(), StatusCode::OK, "{name}");
            assert_eq!(res.headers()["Content-Type"], mime, "{name}");
        }
    }

    #[test]
    fn cors_policy_reflects_only_the_app_origin() {
        let (_d, s) = server_with(&[("index.html", "<app>")]);
        // Navigation-style request (no Origin): historical `null`.
        let res = get(&s, "companion://app/app");
        assert_eq!(res.headers()["Access-Control-Allow-Origin"], "null");
        assert!(res.headers().get("Vary").is_none());
        // Same-app fetch: narrow reflection + Vary.
        let res = get_with_origin(&s, "companion://app/app", Some(APP_ORIGIN));
        assert_eq!(res.headers()["Access-Control-Allow-Origin"], APP_ORIGIN);
        assert_eq!(res.headers()["Vary"], "Origin");
        // Foreign origins (and opaque `null`) never get a reflection.
        for origin in ["https://example.com", "null", "companion://evil"] {
            let res = get_with_origin(&s, "companion://app/app", Some(origin));
            assert_eq!(
                res.headers()["Access-Control-Allow-Origin"],
                "null",
                "{origin}"
            );
        }
    }

    #[test]
    fn encoded_traversal_variants_fail_closed() {
        let (_d, s) = server_with(&[("index.html", "hi")]);
        for url in [
            "companion://app/%2e%2e/secret",
            "companion://app/%2E%2E/secret",
            "companion://app/%2e%2E%2fsecret",
            "companion://app/a/../../secret",
        ] {
            assert_eq!(get(&s, url).status(), StatusCode::FORBIDDEN, "{url}");
        }
    }

    #[test]
    fn asset_report_describes_index_and_counts_files() {
        let (_d, s) = server_with(&[
            ("index.html", "<app>"),
            ("app.js", "js"),
            ("_app/immutable/x.js", "js"),
        ]);
        let report = s.report();
        assert!(report.index_exists);
        assert_eq!(report.index_size, 5);
        assert!(report.index_modified_secs.is_some());
        assert_eq!(report.file_count, 3);
        assert!(!report.truncated);
        assert!(report.root.is_absolute());
    }

    #[test]
    fn asset_report_marks_missing_index() {
        let (_d, s) = server_with(&[("app.js", "js")]);
        let report = s.report();
        assert!(!report.index_exists);
        assert_eq!(report.index_size, 0);
        assert_eq!(report.file_count, 1);
    }

    #[test]
    fn bad_asset_root_is_rejected() {
        assert!(AssetServer::new(PathBuf::from("/nonexistent-utsuwa-root")).is_err());
    }

    #[test]
    fn navigation_policy() {
        assert!(is_navigation_allowed("companion://app/index.html", false));
        assert!(is_navigation_allowed("companion://app/app", false));
        assert!(is_navigation_allowed("companion://app/app/", false));
        assert!(is_navigation_allowed("companion://app/app/settings", false));
        assert!(is_navigation_allowed(
            "companion://app/app?onboarding=1",
            false
        ));
        assert!(is_navigation_allowed("companion://app/app#section", false));
        assert!(is_navigation_allowed("companion://app/", false));
        assert!(is_navigation_allowed("companion://app", false));
        assert!(is_navigation_allowed("COMPANION://app/app", false));
        assert!(!is_navigation_allowed(
            "companion://app.attacker/index.html",
            false
        ));
        assert!(!is_navigation_allowed(
            "companion://app@attacker/index.html",
            false
        ));
        assert!(!is_navigation_allowed("https://example.com/", false));
        assert!(!is_navigation_allowed("http://localhost:5173/", false));
        assert!(is_navigation_allowed("http://localhost:5173/", true));
        assert!(!is_navigation_allowed("https://example.com/", true));
        assert!(!is_navigation_allowed("file:///etc/passwd", true));
    }

    // Minimal tempdir helper: std-only, no new dev-dependency.
    mod tempfile_like {
        use std::path::{Path, PathBuf};
        pub struct Dir(PathBuf);
        impl Dir {
            pub fn create() -> Self {
                let path = std::env::temp_dir().join(format!(
                    "utsuwa-protocol-test-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Self(path)
            }
            pub fn path(&self) -> &Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }
}
