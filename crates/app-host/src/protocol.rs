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

/// Initial page: the app route, served through the SPA fallback.
pub fn initial_url() -> String {
    format!("{APP_SCHEME}://{APP_HOST}/app")
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
        if segments.iter().any(|s| *s == "..") {
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
            if let Some(name) = ancestor.file_name() {
                tail.push(name.to_os_string());
                ancestor.pop();
            } else {
                return None;
            }
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

    pub fn handle(&self, request: Request<Vec<u8>>) -> Response<Cow<'static, [u8]>> {
        let uri = request.uri().clone();
        let Some(mut path) = self.resolve(&uri) else {
            return status(StatusCode::FORBIDDEN, "forbidden");
        };
        if path.is_dir() {
            path.push("index.html");
        }
        if !path.is_file()
            && uri
                .path()
                .split('/')
                .next_back()
                .is_some_and(|last| !last.contains('.'))
        {
            // SPA route (e.g. `/app`, `/overlay`) → client-side router.
            path = self.root.join("index.html");
        }
        match std::fs::read(&path) {
            Ok(bytes) => body(StatusCode::OK, mime_for(&path), Cow::Owned(bytes)),
            Err(_) => status(StatusCode::NOT_FOUND, "not found"),
        }
    }
}

fn status(code: StatusCode, message: &'static str) -> Response<Cow<'static, [u8]>> {
    body(code, "text/plain", Cow::Borrowed(message.as_bytes()))
}

fn body(
    code: StatusCode,
    content_type: &'static str,
    bytes: Cow<'static, [u8]>,
) -> Response<Cow<'static, [u8]>> {
    Response::builder()
        .status(code)
        .header("Content-Type", content_type)
        // Tight scope: assets are local-only; no shared reference needed.
        .header("Access-Control-Allow-Origin", "null")
        .body(bytes)
        // Builder inputs are hardcoded-valid; fall back to an empty body
        // instead of unwrapping on the serving path.
        .unwrap_or_else(|_| Response::new(Cow::Borrowed(&[])))
}

fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html",
        Some("js" | "mjs") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("mp3") => "audio/mpeg",
        Some("mp4") => "video/mp4",
        Some("webm") => "video/webm",
        Some("wasm") => "application/wasm",
        Some("txt") => "text/plain",
        Some("xml") => "application/xml",
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
            && parsed.path().starts_with('/')
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
            std::fs::write(dir.path().join(name), content).unwrap();
        }
        let server = AssetServer::new(dir.path().to_path_buf()).unwrap();
        (dir, server)
    }

    fn get(server: &AssetServer, url: &str) -> Response<Cow<'static, [u8]>> {
        let request = Request::builder().uri(url).body(Vec::new()).unwrap();
        server.handle(request)
    }

    #[test]
    fn serves_files_with_mime_types() {
        let (_d, s) = server_with(&[("app.js", "console.log(1)")]);
        let res = get(&s, "companion://app/app.js");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"console.log(1)");
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
        let res = get(&s, "companion://app/app");
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(res.body().as_ref(), b"<app>");
        // Unknown extension-bearing file is a real 404, not the fallback.
        assert_eq!(
            get(&s, "companion://app/missing.js").status(),
            StatusCode::NOT_FOUND
        );
    }

    #[test]
    fn bad_asset_root_is_rejected() {
        assert!(AssetServer::new(PathBuf::from("/nonexistent-utsuwa-root")).is_err());
    }

    #[test]
    fn navigation_policy() {
        assert!(is_navigation_allowed("companion://app/index.html", false));
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
