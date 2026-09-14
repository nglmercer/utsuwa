//! Document and image helpers (`document.*`, `image.*`, `pdf.*`).
//!
//! Higher-level inspection over files the model is already authorized to
//! read: metadata, UTF-8 text extraction, best-effort PDF literal-text
//! extraction, image dimensions, and bounded image resizing into a fresh
//! artifact. Filesystem authorization is never bypassed — every tool
//! requires a read ticket for the file, and `image.resize` additionally
//! requires a write ticket for the destination.

use artifact_core::{ArtifactSource, ArtifactStore, ContentPart, ImageArtifactRef};
use std::path::PathBuf;
use std::sync::Arc;
use tool_core::{
    CapabilityRequirement, Tool, ToolContext, ToolEffect, ToolError, ToolMetadata, ToolOutput,
};

const MAX_TEXT_BYTES: usize = 512 * 1024;

fn invalid(tool: &str, message: impl Into<String>) -> ToolError {
    ToolError::InvalidArgs {
        tool: tool.to_string(),
        message: message.into(),
    }
}

fn failed(tool: &str, code: &str, message: String) -> ToolError {
    ToolError::structured(tool, code, message)
}

fn require_read(tool: &str, ctx: &ToolContext, path: &std::path::Path) -> Result<(), ToolError> {
    let resource = capability_core::Resource::Path(path.to_path_buf());
    if ctx.has_ticket(capability_core::Capability::FilesystemRead, resource) {
        Ok(())
    } else {
        Err(ToolError::structured_with_details(
            tool,
            "permission_required",
            "no filesystem-read ticket authorizes this file",
            serde_json::json!({ "capability": "FilesystemRead" }),
        ))
    }
}

fn path_arg(args: &serde_json::Value, tool: &str) -> Result<PathBuf, ToolError> {
    args.get("path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| invalid(tool, "missing non-empty string 'path'"))
}

fn read_capability(args: &serde_json::Value) -> Option<CapabilityRequirement> {
    let path = args.get("path")?.as_str()?;
    Some(CapabilityRequirement {
        capability: capability_core::Capability::FilesystemRead,
        resource: capability_core::Resource::Path(PathBuf::from(path)),
    })
}

fn mime_guess(path: &std::path::Path) -> String {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => "image/png".to_string(),
        Some("jpg" | "jpeg") => "image/jpeg".to_string(),
        Some("gif") => "image/gif".to_string(),
        Some("webp") => "image/webp".to_string(),
        Some("pdf") => "application/pdf".to_string(),
        Some("txt" | "md" | "rs" | "toml" | "json" | "log") => "text/plain".to_string(),
        Some("wav") => "audio/wav".to_string(),
        Some("mp3") => "audio/mpeg".to_string(),
        Some("mp4") => "video/mp4".to_string(),
        _ => "application/octet-stream".to_string(),
    }
}

pub struct DocumentMetadataTool;
pub struct DocumentExtractTextTool;
pub struct PdfExtractTextTool;
pub struct PdfPageImageTool {
    pub artifacts: Arc<dyn ArtifactStore>,
}
pub struct ImageMetadataTool;
pub struct ImageResizeTool {
    pub artifacts: Arc<dyn ArtifactStore>,
}

/// Best-effort text extraction from a PDF: collects literal `(…)` strings
/// from content streams. Works for common uncompressed PDFs; compressed or
/// encrypted documents return an explicit `unsupported_operation` error
/// instead of garbage.
fn pdf_literal_text(bytes: &[u8]) -> Result<String, ToolError> {
    if !bytes.starts_with(b"%PDF-") {
        return Err(failed(
            "pdf.extract_text",
            "invalid_target",
            "not a PDF document".to_string(),
        ));
    }
    if bytes.windows(6).any(|window| window == b"/Crypt") {
        return Err(failed(
            "pdf.extract_text",
            "unsupported_operation",
            "encrypted PDFs are not supported".to_string(),
        ));
    }
    let mut out = String::new();
    let mut index = 0;
    let mut in_string = false;
    let mut depth: usize = 0;
    let mut current = Vec::new();
    while index < bytes.len() {
        let byte = bytes[index];
        if !in_string {
            if byte == b'(' {
                in_string = true;
                depth = 1;
                current.clear();
            }
            index += 1;
            continue;
        }
        match byte {
            b'\\' if index + 1 < bytes.len() => {
                let next = bytes[index + 1];
                match next {
                    b'n' => current.push(b'\n'),
                    b'r' => current.push(b'\r'),
                    b't' => current.push(b'\t'),
                    other => current.push(other),
                }
                index += 2;
            }
            b'(' => {
                depth += 1;
                current.push(byte);
                index += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    in_string = false;
                    let text = String::from_utf8_lossy(&current);
                    if !text.trim().is_empty() {
                        if !out.is_empty() {
                            out.push(' ');
                        }
                        out.push_str(text.trim());
                    }
                } else {
                    current.push(byte);
                }
                index += 1;
            }
            _ => {
                current.push(byte);
                index += 1;
            }
        }
        if out.len() > MAX_TEXT_BYTES {
            out.truncate(MAX_TEXT_BYTES);
            out.push_str("…[truncated]");
            break;
        }
    }
    if out.trim().is_empty() {
        return Err(failed(
            "pdf.extract_text",
            "unsupported_operation",
            "no extractable literal text found (compressed or image-only PDF)".to_string(),
        ));
    }
    Ok(out)
}

fn read_bounded(path: &std::path::Path, tool: &str) -> Result<Vec<u8>, ToolError> {
    let metadata = std::fs::metadata(path)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    if metadata.len() > MAX_TEXT_BYTES as u64 {
        return Err(ToolError::structured_with_details(
            tool,
            "response_too_large",
            format!("file is {} bytes (limit {MAX_TEXT_BYTES})", metadata.len()),
            serde_json::json!({ "size_bytes": metadata.len() }),
        ));
    }
    std::fs::read(path).map_err(|error| failed(tool, "action_failed", error.to_string()))
}

#[async_trait::async_trait]
impl Tool for DocumentMetadataTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("document.metadata"),
            description: "Size, guessed MIME type, and kind for one file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "path": {"type": "string"} }, "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "document.metadata")?;
        require_read("document.metadata", &ctx, &path)?;
        let metadata = std::fs::metadata(&path)
            .map_err(|error| failed("document.metadata", "action_failed", error.to_string()))?;
        Ok(ToolOutput::json(serde_json::json!({
            "path": path.to_string_lossy(),
            "size_bytes": metadata.len(),
            "mime_type": mime_guess(&path),
            "is_file": metadata.is_file(),
        })))
    }
}

#[async_trait::async_trait]
impl Tool for DocumentExtractTextTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("document.extract_text"),
            description: "Extract UTF-8 text from a document (plain text directly, PDF via literal-string scan). Bounded output.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "path": {"type": "string"} }, "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "document.extract_text")?;
        require_read("document.extract_text", &ctx, &path)?;
        let bytes = read_bounded(&path, "document.extract_text")?;
        let is_pdf = path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"));
        let text = if is_pdf {
            pdf_literal_text(&bytes)?
        } else {
            String::from_utf8(bytes).map_err(|_| {
                failed(
                    "document.extract_text",
                    "unsupported_operation",
                    "file is not UTF-8 text".to_string(),
                )
            })?
        };
        Ok(ToolOutput::json(
            serde_json::json!({ "path": path.to_string_lossy(), "text": text }),
        ))
    }
}

#[async_trait::async_trait]
impl Tool for PdfExtractTextTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("pdf.extract_text"),
            description: "Alias of document.extract_text scoped to PDFs.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "path": {"type": "string"} }, "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "pdf.extract_text")?;
        require_read("pdf.extract_text", &ctx, &path)?;
        let bytes = read_bounded(&path, "pdf.extract_text")?;
        Ok(ToolOutput::json(serde_json::json!({
            "path": path.to_string_lossy(),
            "text": pdf_literal_text(&bytes)?,
        })))
    }
}

#[async_trait::async_trait]
impl Tool for ImageMetadataTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("image.metadata"),
            description:
                "Decode image dimensions and format without loading full pixels into context."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": { "path": {"type": "string"} }, "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "image.metadata")?;
        require_read("image.metadata", &ctx, &path)?;
        let path_for_task = path.clone();
        let (width, height, format) = tokio::task::spawn_blocking(move || {
            let reader = image::ImageReader::open(&path_for_task)
                .map_err(|error| failed("image.metadata", "action_failed", error.to_string()))?;
            let reader = reader
                .with_guessed_format()
                .map_err(|error| failed("image.metadata", "action_failed", error.to_string()))?;
            let format = reader.format().map(|format| format!("{format:?}"));
            let (width, height) = reader
                .into_dimensions()
                .map_err(|error| failed("image.metadata", "action_failed", error.to_string()))?;
            Ok::<_, ToolError>((width, height, format))
        })
        .await
        .map_err(|error| failed("image.metadata", "action_failed", error.to_string()))??;
        Ok(ToolOutput::json(serde_json::json!({
            "path": path.to_string_lossy(),
            "width": width,
            "height": height,
            "format": format,
        })))
    }
}

#[async_trait::async_trait]
impl Tool for ImageResizeTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("image.resize"),
            description: "Resize an image into a fresh PNG artifact (bounded dimensions). Original file is untouched.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 8192},
                },
                "required": ["path", "max_width"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        let path = path_arg(&args, "image.resize")?;
        let max_width = args
            .get("max_width")
            .and_then(|value| value.as_u64())
            .and_then(|width| u32::try_from(width).ok())
            .filter(|width| (1..=8192).contains(width))
            .ok_or_else(|| invalid("image.resize", "max_width must be between 1 and 8192"))?;
        require_read("image.resize", &ctx, &path)?;
        let path_for_task = path.clone();
        let png_bytes = tokio::task::spawn_blocking(move || {
            let image = image::open(&path_for_task)
                .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?;
            let resized = if image.width() > max_width {
                let height = ((image.height() as f32 * max_width as f32 / image.width() as f32)
                    .round() as u32)
                    .max(1);
                image.resize(max_width, height, image::imageops::FilterType::Triangle)
            } else {
                image
            };
            let mut bytes = Vec::new();
            resized
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?;
            Ok::<_, ToolError>(bytes)
        })
        .await
        .map_err(|error| failed("image.resize", "action_failed", error.to_string()))??;
        let artifact = self
            .artifacts
            .put_with_source("image/png", png_bytes, ArtifactSource::Generated, false)
            .await
            .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?;
        let reader = image::ImageReader::new(std::io::Cursor::new(
            self.artifacts
                .get(&artifact.id)
                .await
                .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?,
        ))
        .with_guessed_format()
        .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?;
        let (width, height) = reader
            .into_dimensions()
            .map_err(|error| failed("image.resize", "action_failed", error.to_string()))?;
        let image = ImageArtifactRef::new(artifact.clone(), width, height);
        Ok(ToolOutput::multipart(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "artifact_id": artifact.id,
                "width": width,
                "height": height,
            }),
            vec![ContentPart::Image(image)],
        ))
    }
}

/// Whether `pdftoppm` (poppler) can render PDF pages. Checked per call
/// (cheap `exec`) rather than cached, so installs during a session take
/// effect.
pub fn pdftoppm_available() -> bool {
    std::process::Command::new("pdftoppm")
        .arg("-v")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

/// Render one PDF page to PNG bytes via `pdftoppm -singlefile`: bounded
/// input size, hard timeout, bounded stderr diagnostics. Pure over the
/// filesystem (no tickets involved); the caller owns authorization.
fn render_pdf_page(
    tool: &str,
    path: &std::path::Path,
    page: u32,
    timeout: std::time::Duration,
) -> Result<Vec<u8>, ToolError> {
    const PDF_MAX_INPUT_BYTES: u64 = 256 * 1024 * 1024;
    const PDF_MAX_PNG_BYTES: u64 = 32 * 1024 * 1024;
    const RENDER_DPI: u32 = 150;

    let size = std::fs::metadata(path)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?
        .len();
    if size > PDF_MAX_INPUT_BYTES {
        return Err(ToolError::structured_with_details(
            tool,
            "response_too_large",
            format!("PDF is {size} bytes (limit {PDF_MAX_INPUT_BYTES})"),
            serde_json::json!({ "size_bytes": size }),
        ));
    }
    if !pdftoppm_available() {
        return Err(failed(
            tool,
            "backend_unavailable",
            "page rendering needs a `pdftoppm` (poppler) binary on PATH, which is not installed"
                .to_string(),
        ));
    }
    let dir = std::env::temp_dir().join(format!("utsuwa-pdf-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir)
        .map_err(|error| failed(tool, "action_failed", error.to_string()))?;
    let prefix = dir.join("page");
    let page_arg = page.to_string();
    let dpi_arg = RENDER_DPI.to_string();
    let mut child = std::process::Command::new("pdftoppm")
        .args([
            "-png",
            "-r",
            &dpi_arg,
            "-f",
            &page_arg,
            "-l",
            &page_arg,
            "-singlefile",
        ])
        .arg(path)
        .arg(&prefix)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|error| {
            failed(
                tool,
                "backend_unavailable",
                format!("cannot run pdftoppm: {error}"),
            )
        })?;
    let deadline = std::time::Instant::now() + timeout;
    let mut status = None;
    while std::time::Instant::now() < deadline {
        match child.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(10)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                std::fs::remove_dir_all(&dir).ok();
                return Err(failed(tool, "action_failed", error.to_string()));
            }
        }
    }
    // The child has exited (or was just reaped), so reading stderr to EOF
    // cannot block; cap it anyway.
    let mut stderr = Vec::new();
    if let Some(mut pipe) = child.stderr.take() {
        use std::io::Read as _;
        let mut chunk = [0u8; 8192];
        loop {
            if stderr.len() >= 64 * 1024 {
                break;
            }
            match pipe.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => stderr.extend_from_slice(&chunk[..read]),
                Err(_) => break,
            }
        }
    }
    let status = match status {
        Some(status) => status,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            std::fs::remove_dir_all(&dir).ok();
            return Err(failed(
                tool,
                "action_failed",
                format!("pdftoppm timed out after {}s", timeout.as_secs()),
            ));
        }
    };
    let diagnostics: String = String::from_utf8_lossy(&stderr).chars().take(500).collect();
    if !status.success() {
        std::fs::remove_dir_all(&dir).ok();
        let lowered = diagnostics.to_lowercase();
        if lowered.contains("encrypt") || lowered.contains("password") {
            return Err(failed(
                tool,
                "unsupported_operation",
                "encrypted PDFs are not supported".to_string(),
            ));
        }
        if lowered.contains("page") || lowered.contains("pages") {
            return Err(failed(
                tool,
                "invalid_target",
                format!("page {page} is not renderable: {diagnostics}"),
            ));
        }
        return Err(failed(
            tool,
            "action_failed",
            format!("pdftoppm failed: {diagnostics}"),
        ));
    }
    let png_path = prefix.with_extension("png");
    let png_size = std::fs::metadata(&png_path)
        .map_err(|error| {
            std::fs::remove_dir_all(&dir).ok();
            failed(
                tool,
                "action_failed",
                format!("pdftoppm produced no output: {error}"),
            )
        })?
        .len();
    if png_size > PDF_MAX_PNG_BYTES {
        std::fs::remove_dir_all(&dir).ok();
        return Err(ToolError::structured_with_details(
            tool,
            "response_too_large",
            format!("rendered page is {png_size} bytes (limit {PDF_MAX_PNG_BYTES})"),
            serde_json::json!({ "size_bytes": png_size }),
        ));
    }
    let bytes =
        std::fs::read(&png_path).map_err(|error| failed(tool, "action_failed", error.to_string()));
    std::fs::remove_dir_all(&dir).ok();
    bytes
}

#[async_trait::async_trait]
impl Tool for PdfPageImageTool {
    fn metadata(&self) -> ToolMetadata {
        ToolMetadata {
            id: capability_core::ToolId::new("pdf.page_image"),
            description: "Render one PDF page to a PNG image artifact (poppler pdftoppm, bounded). Requires a filesystem-read ticket for the file.".to_string(),
            input_schema: serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "path": {"type": "string"},
                    "page": {"type": "integer", "minimum": 1},
                    "max_width": {"type": "integer", "minimum": 1, "maximum": 4096},
                },
                "required": ["path"],
            }),
            effects: vec![ToolEffect::ReadOnly],
        }
    }

    fn required_capability(&self, args: &serde_json::Value) -> Option<CapabilityRequirement> {
        read_capability(args)
    }

    async fn invoke(
        &self,
        ctx: ToolContext,
        args: serde_json::Value,
    ) -> Result<ToolOutput, ToolError> {
        const TOOL: &str = "pdf.page_image";
        let path = path_arg(&args, TOOL)?;
        let page = match args.get("page") {
            None => 1,
            Some(value) => value
                .as_u64()
                .and_then(|page| u32::try_from(page).ok())
                .filter(|page| *page >= 1)
                .ok_or_else(|| invalid(TOOL, "page must be an integer >= 1"))?,
        };
        let max_width = match args.get("max_width") {
            None => 1600,
            Some(value) => value
                .as_u64()
                .and_then(|width| u32::try_from(width).ok())
                .filter(|width| (1..=4096).contains(width))
                .ok_or_else(|| invalid(TOOL, "max_width must be between 1 and 4096"))?,
        };
        require_read(TOOL, &ctx, &path)?;
        let path_for_task = path.clone();
        let png_bytes = tokio::task::spawn_blocking(move || {
            render_pdf_page(
                TOOL,
                &path_for_task,
                page,
                std::time::Duration::from_secs(60),
            )
        })
        .await
        .map_err(|error| failed(TOOL, "action_failed", error.to_string()))??;
        let decoded = image::load_from_memory(&png_bytes)
            .map_err(|error| failed(TOOL, "action_failed", error.to_string()))?;
        let resized = if decoded.width() > max_width {
            let height = ((decoded.height() as f32 * max_width as f32 / decoded.width() as f32)
                .round() as u32)
                .max(1);
            decoded.resize(max_width, height, image::imageops::FilterType::Triangle)
        } else {
            decoded
        };
        let mut out_bytes = Vec::new();
        resized
            .write_to(
                &mut std::io::Cursor::new(&mut out_bytes),
                image::ImageFormat::Png,
            )
            .map_err(|error| failed(TOOL, "action_failed", error.to_string()))?;
        let (width, height) = (resized.width(), resized.height());
        let artifact = self
            .artifacts
            .put_with_source("image/png", out_bytes, ArtifactSource::Generated, false)
            .await
            .map_err(|error| failed(TOOL, "action_failed", error.to_string()))?;
        let image = ImageArtifactRef::new(artifact.clone(), width, height);
        Ok(ToolOutput::multipart(
            serde_json::json!({
                "path": path.to_string_lossy(),
                "page": page,
                "artifact_id": artifact.id,
                "width": width,
                "height": height,
            }),
            vec![ContentPart::Image(image)],
        ))
    }
}

/// Static document/image tool group.
pub struct DocumentToolPack {
    pub artifacts: Arc<dyn ArtifactStore>,
}

impl DocumentToolPack {
    pub fn new() -> Self {
        Self {
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
        }
    }
}

impl Default for DocumentToolPack {
    fn default() -> Self {
        Self::new()
    }
}

impl tool_sdk::ToolPack for DocumentToolPack {
    fn id(&self) -> &'static str {
        "document"
    }

    /// Pure/static tools are always advertised. `pdf.page_image` appears
    /// only while a `pdftoppm` binary is usable — otherwise the model
    /// would learn it by trial-and-error `backend_unavailable` failures.
    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        self.tools_with_availability(pdftoppm_available())
    }
}

impl DocumentToolPack {
    fn tools_with_availability(&self, renderer_present: bool) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(DocumentMetadataTool),
            Arc::new(DocumentExtractTextTool),
            Arc::new(PdfExtractTextTool),
            Arc::new(ImageMetadataTool),
            Arc::new(ImageResizeTool {
                artifacts: self.artifacts.clone(),
            }),
        ];
        if renderer_present {
            tools.push(Arc::new(PdfPageImageTool {
                artifacts: self.artifacts.clone(),
            }));
        }
        tools
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use capability_core::{AgentId, Principal};
    use tool_sdk::ToolPack as _;

    fn ctx_for(path: &std::path::Path) -> ToolContext {
        let ctx = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let ticket = capability_core::CapabilityTicket::mint(
            ctx.principal.clone(),
            capability_core::Capability::FilesystemRead,
            capability_core::ResourceScope::new(vec![capability_core::Resource::Path(
                path.to_path_buf(),
            )]),
            ctx.invocation_id,
            std::time::Duration::from_secs(120),
        );
        ctx.with_ticket(ticket)
    }

    #[test]
    fn pdf_literal_scan_extracts_text_and_rejects_garbage() {
        let pdf = b"%PDF-1.4\n1 0 obj<</Type/Catalog>>\nstream\nBT /F1 12 Tf (Hello ) Tj (World) Tj ET\nendstream\n";
        let text = pdf_literal_text(pdf).unwrap();
        assert!(text.contains("Hello"), "{text}");
        assert!(text.contains("World"), "{text}");
        assert!(pdf_literal_text(b"not a pdf").is_err());
        // Compressed/image-only PDFs fail honestly instead of garbage.
        let image_only = b"%PDF-1.4\n1 0 obj<</Type/XObject/Subtype/Image>>\n";
        assert!(pdf_literal_text(image_only).is_err());
    }

    #[tokio::test]
    async fn text_extraction_round_trips() {
        let dir = std::env::temp_dir().join(format!("utsuwa-doc-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("note.txt");
        std::fs::write(&path, "hello documents").unwrap();
        let tool = DocumentExtractTextTool;
        let out = tool
            .invoke(
                ctx_for(&path),
                serde_json::json!({"path": path.to_string_lossy()}),
            )
            .await
            .unwrap();
        assert_eq!(out.content["text"], "hello documents");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pack_registers_document_surface() {
        let mut ids = DocumentToolPack::new()
            .tools_with_availability(true)
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect::<Vec<_>>();
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "document.extract_text",
                "document.metadata",
                "image.metadata",
                "image.resize",
                "pdf.extract_text",
                "pdf.page_image",
            ]
        );
    }

    #[test]
    fn page_image_tracks_renderer_availability() {
        let without: Vec<String> = DocumentToolPack::new()
            .tools_with_availability(false)
            .iter()
            .map(|tool| tool.metadata().id.0.clone())
            .collect();
        assert!(!without.iter().any(|id| id == "pdf.page_image"));
        // Static tools stay visible with or without the renderer.
        for expected in [
            "document.extract_text",
            "document.metadata",
            "image.metadata",
            "image.resize",
            "pdf.extract_text",
        ] {
            assert!(
                without.iter().any(|id| id == expected),
                "missing {expected}"
            );
        }
        assert_eq!(
            without.len(),
            DocumentToolPack::new()
                .tools(&tool_sdk::ToolLoadContext::default())
                .len()
                .saturating_sub(usize::from(pdftoppm_available())),
        );
    }

    /// Minimal one-page PDF with a correct xref table, built in-test so
    /// the render path never depends on a fixture file.
    fn minimal_pdf() -> Vec<u8> {
        let objects = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 200 200] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>",
            "<< /Length 44 >>\nstream\nBT /F1 24 Tf 50 150 Td (Hi) Tj ET\nendstream",
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>",
        ];
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (index, body) in objects.iter().enumerate() {
            offsets.push(pdf.len());
            pdf.extend_from_slice(format!("{} 0 obj\n{body}\nendobj\n", index + 1).as_bytes());
        }
        let xref_at = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", objects.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in &offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n",
                objects.len() + 1
            )
            .as_bytes(),
        );
        pdf
    }

    #[tokio::test]
    async fn page_image_validates_args_before_touching_the_backend() {
        let dir = std::env::temp_dir().join(format!("utsuwa-pdf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.pdf");
        std::fs::write(&path, minimal_pdf()).unwrap();
        let tool = PdfPageImageTool {
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
        };
        let args = |extra: serde_json::Value| {
            let mut map = serde_json::json!({"path": path.to_string_lossy()});
            for (key, value) in extra.as_object().unwrap() {
                map[key] = value.clone();
            }
            map
        };
        // page 0 is rejected, not silently coerced to page 1.
        let error = tool
            .invoke(ctx_for(&path), args(serde_json::json!({"page": 0})))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        // max_width outside 1..=4096 is rejected like image.resize.
        let error = tool
            .invoke(ctx_for(&path), args(serde_json::json!({"max_width": 0})))
            .await
            .unwrap_err();
        assert!(matches!(error, ToolError::InvalidArgs { .. }), "{error:?}");
        // Without a read ticket the renderer never runs.
        let bare = ToolContext::new(Principal::Agent(AgentId::new("test")));
        let error = tool
            .invoke(bare, args(serde_json::json!({})))
            .await
            .unwrap_err();
        assert!(
            format!("{error:?}").contains("permission_required"),
            "{error:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn page_image_renders_or_fails_closed() {
        let dir = std::env::temp_dir().join(format!("utsuwa-pdf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("one.pdf");
        std::fs::write(&path, minimal_pdf()).unwrap();
        let tool = PdfPageImageTool {
            artifacts: Arc::new(artifact_core::InMemoryArtifactStore::new()),
        };
        let result = tool
            .invoke(
                ctx_for(&path),
                serde_json::json!({"path": path.to_string_lossy(), "max_width": 200}),
            )
            .await;
        if pdftoppm_available() {
            let out = result.unwrap();
            assert_eq!(out.content["page"], 1);
            assert_eq!(out.content["width"], 200);
            assert!(out.content["height"].as_u64().unwrap() > 0);
            assert!(out.content["artifact_id"].as_str().is_some());
            assert_eq!(out.parts.len(), 1);
            assert!(matches!(out.parts[0], ContentPart::Image(_)));
        } else {
            // No renderer on PATH (CI/macOS/Windows): honest backend
            // error naming the missing helper, never a fake image.
            let error = result.unwrap_err();
            assert!(format!("{error:?}").contains("pdftoppm"), "{error:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
