//! Immutable image ingestion for provider-neutral media references.
//!
//! Image bytes are validated from their actual content (never the file name),
//! content-addressed, and written once into the session artifact store. Events,
//! messages, logs, and transcripts carry only the compact [`MediaRef`]
//! metadata; the bytes themselves exist only in artifact storage and are
//! resolved inline at the provider boundary. This keeps SQLite and the event
//! log free of raw binary/base64 and makes the provider-visible image version
//! independent of the original filesystem path.

use crate::provider::MediaBytesProvider;
use anyhow::{Result, anyhow, bail};
use latch_protocol::{MediaKind, MediaRef};
use sha2::{Digest, Sha256};
use std::path::{Component, Path, PathBuf};

/// Conservative per-image input cap. The Claude API accepts 10 MB base64 (5 MB
/// on Bedrock/Google Cloud), and OpenAI/DeepSeek accept much larger inline
/// images; 5 MiB fits every currently supported provider path.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// Largest accepted side. Anthropic rejects above 8000x8000; DeepSeek above
/// 8192. Oversized images are rejected honestly rather than silently resized.
pub const MAX_IMAGE_DIMENSION: u32 = 8000;

/// Image formats supported by every currently implemented provider path.
/// Animated GIF is deliberately rejected in this version: the serializers and
/// providers do not agree on animation handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Png,
    Jpeg,
    WebP,
}

impl ImageFormat {
    #[must_use]
    pub const fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::WebP => "image/webp",
        }
    }

    #[must_use]
    pub const fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::WebP => "webp",
        }
    }
}

impl std::fmt::Display for ImageFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Png => "PNG",
            Self::Jpeg => "JPEG",
            Self::WebP => "WebP",
        })
    }
}

/// Detects the real image format from content bytes. The file name extension
/// and any declared MIME type are never consulted.
#[must_use]
pub fn detect_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some(ImageFormat::Png);
    }
    if bytes.starts_with(b"\xff\xd8\xff") {
        return Some(ImageFormat::Jpeg);
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageFormat::WebP);
    }
    None
}

/// Whether bytes look like an unsupported-but-recognized image container
/// (currently GIF), so callers can give an honest, specific error.
#[must_use]
pub fn detected_unsupported_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("GIF");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return None;
    }
    if bytes.starts_with(b"BM") {
        return Some("BMP");
    }
    None
}

/// Validates the container structure and parses pixel dimensions from the
/// header without decoding pixels. Fails closed on truncated or malformed
/// headers instead of passing unvalidated bytes through to a provider.
pub fn image_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32)> {
    match format {
        ImageFormat::Png => png_dimensions(bytes),
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::WebP => webp_dimensions(bytes),
    }
}

fn be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.try_into().ok()?))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let mut offset = 8usize;
    let mut dimensions = None;
    let mut saw_iend = false;
    while offset + 12 <= bytes.len() {
        let length =
            be_u32(&bytes[offset..offset + 4]).ok_or_else(|| anyhow!("invalid PNG"))? as usize;
        let kind = &bytes[offset + 4..offset + 8];
        let data_start = offset + 8;
        let data_end = data_start
            .checked_add(length)
            .ok_or_else(|| anyhow!("invalid PNG chunk length"))?;
        if data_end + 4 > bytes.len() {
            bail!("truncated PNG chunk {}", String::from_utf8_lossy(kind));
        }
        let expected = be_u32(&bytes[data_end..data_end + 4]).unwrap_or(0);
        let actual = crc32(&bytes[offset + 4..data_end]);
        if expected != actual {
            bail!(
                "corrupt PNG chunk {} (CRC mismatch)",
                String::from_utf8_lossy(kind)
            );
        }
        match kind {
            b"IHDR" => {
                if length != 13 {
                    bail!("invalid PNG IHDR chunk");
                }
                let width = be_u32(&bytes[data_start..data_start + 4]).unwrap_or(0);
                let height = be_u32(&bytes[data_start + 4..data_start + 8]).unwrap_or(0);
                if width == 0 || height == 0 {
                    bail!("invalid PNG dimensions");
                }
                dimensions = Some((width, height));
            }
            b"IEND" => {
                saw_iend = true;
                break;
            }
            _ => {}
        }
        offset = data_end + 4;
    }
    match (dimensions, saw_iend) {
        (Some(dimensions), true) => Ok(dimensions),
        (Some(_), false) => bail!("truncated PNG: missing IEND"),
        (None, _) => bail!("invalid PNG: missing IHDR"),
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    let mut index = 2usize;
    while index + 4 <= bytes.len() {
        if bytes[index] != 0xff {
            bail!("invalid JPEG marker stream");
        }
        // Fill bytes are allowed between markers.
        while index < bytes.len() && bytes[index] == 0xff {
            index += 1;
        }
        let Some(&marker) = bytes.get(index) else {
            break;
        };
        index += 1;
        match marker {
            // Standalone markers without a length.
            0x01 | 0xd0..=0xd9 => continue,
            0xda => break, // start of scan; dimensions must already be known
            // SOF markers that carry frame dimensions.
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                if index + 7 > bytes.len() {
                    bail!("truncated JPEG SOF segment");
                }
                let height = u16::from_be_bytes([bytes[index + 3], bytes[index + 4]]) as u32;
                let width = u16::from_be_bytes([bytes[index + 5], bytes[index + 6]]) as u32;
                if width == 0 || height == 0 {
                    bail!("invalid JPEG dimensions");
                }
                return Ok((width, height));
            }
            _ => {
                if index + 2 > bytes.len() {
                    bail!("truncated JPEG segment");
                }
                let length = u16::from_be_bytes([bytes[index], bytes[index + 1]]) as usize;
                if length < 2 {
                    bail!("invalid JPEG segment length");
                }
                index = index
                    .checked_add(length)
                    .ok_or_else(|| anyhow!("invalid JPEG segment length"))?;
            }
        }
    }
    bail!("invalid JPEG: no frame header")
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    if bytes.len() < 16 {
        bail!("truncated WebP");
    }
    let declared = u32::from_le_bytes(bytes[4..8].try_into().unwrap_or([0; 4])) as usize;
    if declared + 8 > bytes.len() {
        bail!("truncated WebP RIFF container");
    }
    let mut offset = 12usize;
    let mut lossless = None;
    let mut lossy = None;
    let mut extended = None;
    while offset + 8 <= bytes.len() {
        let kind = &bytes[offset..offset + 4];
        let length =
            u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap_or([0; 4])) as usize;
        let data_start = offset + 8;
        let data_end = data_start
            .checked_add(length)
            .ok_or_else(|| anyhow!("invalid WebP chunk length"))?;
        if data_end > bytes.len() {
            bail!("truncated WebP chunk");
        }
        let data = &bytes[data_start..data_end];
        match kind {
            b"VP8 " => {
                if data.len() >= 10 && data[3..6] == [0x9d, 0x01, 0x2a] {
                    let width = (u16::from_le_bytes([data[6], data[7]]) & 0x3fff) as u32;
                    let height = (u16::from_le_bytes([data[8], data[9]]) & 0x3fff) as u32;
                    lossy = Some((width, height));
                }
            }
            b"VP8L" => {
                if data.len() >= 5 && data[0] == 0x2f {
                    let bits = u32::from_le_bytes([data[1], data[2], data[3], data[4]]);
                    let width = (bits & 0x3fff) + 1;
                    let height = ((bits >> 14) & 0x3fff) + 1;
                    lossless = Some((width, height));
                }
            }
            b"VP8X" => {
                if data.len() >= 10 {
                    let width = u32::from_le_bytes([data[4], data[5], data[6], 0]) + 1;
                    let height = u32::from_le_bytes([data[7], data[8], data[9], 0]) + 1;
                    extended = Some((width, height));
                }
            }
            b"ANIM" | b"ANMF" => {
                bail!("animated WebP is not supported")
            }
            _ => {}
        }
        // Chunks are padded to an even byte boundary.
        offset = data_end + (length & 1);
    }
    let dimensions = extended.or(lossless).or(lossy);
    match dimensions {
        Some((width, height)) if width > 0 && height > 0 => Ok((width, height)),
        _ => bail!("invalid WebP: no image dimensions"),
    }
}

/// Standard CRC-32 (IEEE 802.3) used by PNG chunks.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn validate_reference(reference: &MediaRef) -> Result<()> {
    if reference.sha256 != reference.id {
        bail!("media reference id does not match its content hash");
    }
    let path = Path::new(&reference.artifact_path);
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("invalid media artifact path");
    }
    if !reference.artifact_path.starts_with("media/") {
        bail!("media artifact path escapes the media store");
    }
    Ok(())
}

/// Validates a declared byte length against the image size limit without
/// reading the file, so callers can refuse an oversized source early.
pub fn ensure_size(len: u64) -> Result<()> {
    if len == 0 {
        bail!("image is empty");
    }
    if len > MAX_IMAGE_BYTES {
        bail!("image is {len} bytes; the maximum accepted image size is {MAX_IMAGE_BYTES} bytes");
    }
    Ok(())
}

/// Validates and ingests image bytes into the artifact store, returning the
/// durable reference. Identical bytes deduplicate to the same artifact.
pub fn ingest_image_bytes(
    artifacts: &Path,
    bytes: &[u8],
    display_name: Option<String>,
) -> Result<MediaRef> {
    ensure_size(bytes.len() as u64)?;
    let Some(format) = detect_format(bytes) else {
        if let Some(kind) = detected_unsupported_kind(bytes) {
            bail!("unsupported image format {kind}; supported formats are PNG, JPEG, and WebP");
        }
        bail!("not a supported image file (PNG, JPEG, or WebP)");
    };
    let (width, height) = image_dimensions(bytes, format)
        .map_err(|error| anyhow!("invalid {} image: {error}", format))?;
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        bail!(
            "image is {width}×{height}; the maximum accepted dimension is {MAX_IMAGE_DIMENSION} px"
        );
    }
    let sha256 = hex::encode(Sha256::digest(bytes));
    let artifact_path = format!("media/{sha256}.{}", format.extension());
    let path = artifacts.join(&artifact_path);
    if !path.exists() {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Write-then-rename keeps a concurrently observed artifact complete;
        // content addressing makes the last writer's bytes identical anyway.
        let temp = artifacts.join(format!("media/.{sha256}.{}.tmp", std::process::id()));
        std::fs::write(&temp, bytes)?;
        std::fs::rename(&temp, &path).inspect_err(|_| {
            let _ = std::fs::remove_file(&temp);
        })?;
    }
    Ok(MediaRef {
        id: sha256.clone(),
        kind: MediaKind::Image,
        mime_type: format.mime_type().to_owned(),
        artifact_path,
        sha256,
        byte_len: bytes.len() as u64,
        width: Some(width),
        height: Some(height),
        display_name,
    })
}

/// Reads the immutable bytes of a durable media reference, refusing any path
/// that escapes the session artifact store.
pub fn read_image_bytes(artifacts: &Path, reference: &MediaRef) -> Result<Vec<u8>> {
    validate_reference(reference)?;
    let path = artifacts.join(&reference.artifact_path);
    let bytes = std::fs::read(&path)
        .map_err(|error| anyhow!("read media artifact {}: {error}", reference.id))?;
    Ok(bytes)
}

/// Filesystem-backed [`MediaBytesProvider`] rooted at one session's artifact
/// store. Provider adapters use it to resolve durable references inline.
#[derive(Debug, Clone)]
pub struct ArtifactMediaStore {
    root: PathBuf,
}

impl ArtifactMediaStore {
    #[must_use]
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl MediaBytesProvider for ArtifactMediaStore {
    fn read(&self, reference: &MediaRef) -> Result<Vec<u8>> {
        read_image_bytes(&self.root, reference)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1x1 red PNG, structurally valid with correct chunk CRCs.
    const TINY_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00, 0x00, 0x90,
        0x77, 0x53, 0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41, 0x54, 0x08, 0xd7, 0x63, 0xf8,
        0xcf, 0xc0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xdd, 0x8d, 0xb0, 0x00, 0x00, 0x00,
        0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// 1x1 white JPEG, decoded from a known-good base64 fixture.
    const TINY_JPEG_B64: &str = "/9j/4AAQSkZJRgABAQEAYABgAAD/2wBDAAgGBgcGBQgHBwcJCQgKDBQNDAsLDBkSEw8UHRofHh0aHBwgJC4nICIsIxwcKDcpLDAxNDQ0Hyc5PTgyPC4zNDL/wAALCAABAAEBAREA/8QAFAABAAAAAAAAAAAAAAAAAAAACf/EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAD8AKp//2Q==";

    fn tiny_jpeg() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(TINY_JPEG_B64)
            .unwrap()
    }

    /// 1x1 lossy WebP, decoded from a known-good base64 fixture.
    const TINY_WEBP_B64: &str = "UklGRiIAAABXRUJQVlA4IBYAAAAwAQCdASoBAAEADsD+JaQAA3AAAAAA";

    fn tiny_webp() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(TINY_WEBP_B64)
            .unwrap()
    }

    const GIF: &[u8] = b"GIF89a\x01\x00\x01\x00\x00\x00\x00;";

    fn ingest(bytes: &[u8]) -> (tempfile::TempDir, MediaRef) {
        let dir = tempfile::tempdir().unwrap();
        let reference = ingest_image_bytes(dir.path(), bytes, Some("img".into())).unwrap();
        (dir, reference)
    }

    #[test]
    fn detects_and_validates_supported_formats_and_dimensions() {
        let jpeg = tiny_jpeg();
        let webp = tiny_webp();
        for (bytes, format, dimensions) in [
            (TINY_PNG, ImageFormat::Png, (1, 1)),
            (jpeg.as_slice(), ImageFormat::Jpeg, (1, 1)),
            (webp.as_slice(), ImageFormat::WebP, (1, 1)),
        ] {
            assert_eq!(detect_format(bytes), Some(format));
            assert_eq!(
                image_dimensions(bytes, format).unwrap(),
                dimensions,
                "{format}"
            );
        }
        assert_eq!(detect_format(GIF), None);
        assert_eq!(detected_unsupported_kind(GIF), Some("GIF"));
    }

    #[test]
    fn ingestion_is_content_addressed_and_deduplicates() {
        let (dir, reference) = ingest(TINY_PNG);
        assert_eq!(reference.kind, MediaKind::Image);
        assert_eq!(reference.mime_type, "image/png");
        assert_eq!(reference.dimensions().as_deref(), Some("1×1"));
        assert_eq!(
            reference.artifact_path,
            format!("media/{}.png", reference.sha256)
        );
        let stored = dir.path().join(&reference.artifact_path);
        assert_eq!(std::fs::read(&stored).unwrap(), TINY_PNG);

        let duplicate = ingest_image_bytes(dir.path(), TINY_PNG, None).unwrap();
        assert_eq!(duplicate.id, reference.id);
        assert_eq!(duplicate.artifact_path, reference.artifact_path);
        assert_eq!(
            std::fs::read_dir(dir.path().join("media")).unwrap().count(),
            1,
            "identical bytes must not create a second artifact"
        );
        assert_eq!(read_image_bytes(dir.path(), &reference).unwrap(), TINY_PNG);
    }

    #[test]
    fn ingestion_rejects_invalid_and_unsupported_input_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            ingest_image_bytes(dir.path(), b"", None)
                .unwrap_err()
                .to_string()
                .contains("empty")
        );
        assert!(
            ingest_image_bytes(dir.path(), b"not an image", None)
                .unwrap_err()
                .to_string()
                .contains("not a supported image")
        );
        assert!(
            ingest_image_bytes(dir.path(), GIF, None)
                .unwrap_err()
                .to_string()
                .contains("GIF")
        );
        // A truncated PNG is recognized but structurally invalid.
        let truncated = &TINY_PNG[..20];
        let error = ingest_image_bytes(dir.path(), truncated, None).unwrap_err();
        assert!(
            error.to_string().contains("truncated") || error.to_string().contains("invalid"),
            "{error}"
        );
        // Oversized input is refused before any provider call.
        let oversized = vec![0u8; (MAX_IMAGE_BYTES + 1) as usize];
        let error = ingest_image_bytes(dir.path(), &oversized, None).unwrap_err();
        assert!(
            error.to_string().contains("maximum accepted image size"),
            "{error}"
        );
    }

    #[test]
    fn corrupt_png_crc_is_rejected() {
        let mut corrupt = TINY_PNG.to_vec();
        // Flip a byte inside the IHDR data so the chunk CRC no longer matches.
        corrupt[20] ^= 0xff;
        let error = image_dimensions(&corrupt, ImageFormat::Png).unwrap_err();
        assert!(error.to_string().contains("CRC"), "{error}");
    }

    #[test]
    fn media_store_refuses_escaping_artifact_paths() {
        let dir = tempfile::tempdir().unwrap();
        let mut reference = ingest(TINY_PNG).1;
        reference.artifact_path = "../secret".into();
        assert!(read_image_bytes(dir.path(), &reference).is_err());
        reference.artifact_path = "/etc/passwd".into();
        assert!(read_image_bytes(dir.path(), &reference).is_err());
        reference.artifact_path = "media/other.png".into();
        reference.id = "mismatch".into();
        assert!(read_image_bytes(dir.path(), &reference).is_err());
    }
}
