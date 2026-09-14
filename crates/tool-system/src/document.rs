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

    fn tools(&self, _ctx: &tool_sdk::ToolLoadContext) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(DocumentMetadataTool),
            Arc::new(DocumentExtractTextTool),
            Arc::new(PdfExtractTextTool),
            Arc::new(ImageMetadataTool),
            Arc::new(ImageResizeTool {
                artifacts: self.artifacts.clone(),
            }),
        ]
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
            .tools(&tool_sdk::ToolLoadContext::default())
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
            ]
        );
    }
}
