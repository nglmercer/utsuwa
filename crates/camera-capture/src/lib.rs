//! Camera device capture (`camera-capture`).
//!
//! [`CameraBackend`] lists devices and captures still photos as PNG bytes.
//! [`NokhwaBackend`] implements it over real OS camera APIs (V4L2, Media
//! Foundation, AVFoundation via nokhwa); [`StubBackend`] reports honest
//! unavailability where no camera stack exists. Photos are returned as raw
//! bytes — persistence and artifact lifecycle live in `tool-camera`, which
//! stores them as expiring sensitive artifacts and never persists them
//! automatically.

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
use nokhwa::utils::{ApiBackend, CameraIndex, RequestedFormat, RequestedFormatType};

/// One camera device.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CameraDevice {
    pub id: String,
    pub label: String,
    pub description: String,
}

/// A captured still photo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Photo {
    pub width: u32,
    pub height: u32,
    pub png_bytes: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("camera backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("unknown camera '{0}'")]
    UnknownCamera(String),
    #[error("capture failed: {0}")]
    CaptureFailed(String),
}

/// Camera control surface. Implementations must never synthesize a photo:
/// without OS access they return [`CameraError::BackendUnavailable`].
pub trait CameraBackend: Send + Sync {
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError>;
    fn capture_photo(&self, camera_id: &str, max_width: Option<u32>) -> Result<Photo, CameraError>;
}

/// Real capture over nokhwa (V4L2 / MediaFoundation / AVFoundation).
/// Available on Linux, Windows, and macOS; other platforms use
/// [`StubBackend`].
#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
pub struct NokhwaBackend;

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
impl NokhwaBackend {
    fn index_for(
        cameras: &[nokhwa::utils::CameraInfo],
        camera_id: &str,
    ) -> Result<CameraIndex, CameraError> {
        cameras
            .iter()
            .find(|camera| {
                camera.index().to_string() == camera_id || camera.human_name() == camera_id
            })
            .map(|camera| camera.index().clone())
            .ok_or_else(|| CameraError::UnknownCamera(camera_id.to_string()))
    }
}

#[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
impl CameraBackend for NokhwaBackend {
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError> {
        let infos = nokhwa::query(ApiBackend::Auto)
            .map_err(|error| CameraError::BackendUnavailable(error.to_string()))?;
        Ok(infos
            .iter()
            .map(|info| CameraDevice {
                id: info.index().to_string(),
                label: info.human_name(),
                description: info.description().to_string(),
            })
            .collect())
    }

    fn capture_photo(&self, camera_id: &str, max_width: Option<u32>) -> Result<Photo, CameraError> {
        if let Some(max_width) = max_width {
            if !(1..=8192).contains(&max_width) {
                return Err(CameraError::CaptureFailed(
                    "max_width must be between 1 and 8192".to_string(),
                ));
            }
        }
        let infos = nokhwa::query(ApiBackend::Auto)
            .map_err(|error| CameraError::BackendUnavailable(error.to_string()))?;
        if infos.is_empty() {
            return Err(CameraError::BackendUnavailable(
                "no camera devices found".to_string(),
            ));
        }
        let index = Self::index_for(&infos, camera_id)?;
        let format = RequestedFormat::new::<nokhwa::pixel_format::RgbFormat>(
            RequestedFormatType::AbsoluteHighestFrameRate,
        );
        let mut camera = nokhwa::Camera::new(index, format)
            .map_err(|error| CameraError::CaptureFailed(error.to_string()))?;
        camera
            .open_stream()
            .map_err(|error| CameraError::CaptureFailed(error.to_string()))?;
        let frame = camera
            .frame()
            .map_err(|error| CameraError::CaptureFailed(error.to_string()))?;
        let decoded = frame
            .decode_image::<nokhwa::pixel_format::RgbFormat>()
            .map_err(|error| CameraError::CaptureFailed(error.to_string()))?;
        let image = maybe_downscale(decoded, max_width);
        let mut png_bytes = Vec::new();
        image
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .map_err(|error| CameraError::CaptureFailed(error.to_string()))?;
        // Report the delivered dimensions, not the sensor resolution.
        let (width, height) = png_dimensions(&png_bytes).unwrap_or((image.width(), image.height()));
        Ok(Photo {
            width,
            height,
            png_bytes,
        })
    }
}

fn maybe_downscale(image: image::RgbImage, max_width: Option<u32>) -> image::RgbImage {
    let Some(max_width) = max_width else {
        return image;
    };
    if image.width() <= max_width {
        return image;
    }
    let scale = max_width as f32 / image.width() as f32;
    let height = ((image.height() as f32 * scale).round() as u32).max(1);
    image::imageops::resize(
        &image,
        max_width,
        height,
        image::imageops::FilterType::Triangle,
    )
}

fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    use std::io::Cursor;
    let decoder = png::Decoder::new(Cursor::new(bytes));
    let reader = decoder.read_info().ok()?;
    let info = reader.info();
    Some((info.width, info.height))
}

/// Honest placeholder where no camera stack exists.
pub struct StubBackend;

impl CameraBackend for StubBackend {
    fn list_cameras(&self) -> Result<Vec<CameraDevice>, CameraError> {
        Err(CameraError::BackendUnavailable(
            "no camera backend on this host".to_string(),
        ))
    }

    fn capture_photo(
        &self,
        _camera_id: &str,
        _max_width: Option<u32>,
    ) -> Result<Photo, CameraError> {
        Err(CameraError::BackendUnavailable(
            "no camera backend on this host".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stub_reports_unavailability_honestly() {
        let backend = StubBackend;
        assert!(matches!(
            backend.list_cameras(),
            Err(CameraError::BackendUnavailable(_))
        ));
        assert!(matches!(
            backend.capture_photo("0", None),
            Err(CameraError::BackendUnavailable(_))
        ));
    }

    #[test]
    fn downscale_keeps_aspect_within_bounds() {
        let image = image::RgbImage::new(200, 100);
        let small = maybe_downscale(image, Some(100));
        assert_eq!((small.width(), small.height()), (100, 50));
        let image = image::RgbImage::new(50, 50);
        let same = maybe_downscale(image, Some(100));
        assert_eq!((same.width(), same.height()), (50, 50));
    }
}
