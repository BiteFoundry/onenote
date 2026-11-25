//! Shared resource & attachment safety helpers (filename sanitization, deduplication,
//! Bearer origin guard, image format sniffing) and OneNote resource download helpers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::GraphClient;
use crate::models::{DownloadedResource, OnenoteResource};

#[allow(dead_code)]
pub(crate) const MAX_IN_MEMORY_RESOURCE_BYTES: usize = 32 * 1024 * 1024; // 32 MiB

impl GraphClient {
    /// Fetches a OneNote page resource (image or embedded file) into memory with origin-safe Bearer handling.
    pub async fn fetch_onenote_resource_bytes(
        &self,
        resource_url: &str,
    ) -> Result<(String, Vec<u8>)> {
        let trimmed = resource_url.trim();
        if trimmed.is_empty() {
            bail!("OneNote resource URL is empty.");
        }
        if trimmed.len() >= 11 && trimmed[..11].eq_ignore_ascii_case("data:image/") {
            let decoded = bitefoundry_html::graphics::decode_data_image_uri(trimmed)
                .context("Failed to decode inline data:image URI")?;
            if decoded.is_svg {
                let raster = bitefoundry_html::graphics::rasterize_svg_to_png(
                    &decoded.bytes,
                    1024,
                    1024,
                    None,
                )
                .context("Failed to rasterize inline data:image/svg+xml URI")?;
                return Ok(("image/png".to_string(), raster.png_bytes.to_vec()));
            }
            return Ok((decoded.mime, decoded.bytes));
        }
        let use_bearer = should_send_bearer(&self.config.graph_base_url, trimmed);
        let bytes = self
            .get_bytes_with_limit(trimmed, use_bearer, MAX_IN_MEMORY_RESOURCE_BYTES)
            .await?;
        let mime = if let Some(ext) = sniff_image_extension(&bytes) {
            match ext {
                "png" => "image/png",
                "jpg" => "image/jpeg",
                "gif" => "image/gif",
                "webp" => "image/webp",
                _ => "application/octet-stream",
            }
            .to_string()
        } else if crate::img_preview::looks_like_html_bytes(&bytes) {
            "text/html".to_string()
        } else if crate::img_preview::looks_like_svg_bytes(&bytes) {
            "image/svg+xml".to_string()
        } else {
            "application/octet-stream".to_string()
        };
        Ok((mime, bytes))
    }

    /// Streams a OneNote page resource (image or embedded file) directly to `dest_dir` and returns
    /// structured metadata (`{ ok, path, file_name, mime_type, size_bytes, is_image }`).
    pub async fn download_onenote_resource(
        &self,
        resource: &OnenoteResource,
        dest_dir: &Path,
    ) -> Result<DownloadedResource> {
        let target_url = resource
            .fullres_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&resource.resource_url);
        self.download_onenote_resource_url(
            target_url,
            resource.file_name.as_deref(),
            Some(resource.mime_type.as_str()),
            dest_dir,
        )
        .await
    }

    /// Streams any OneNote resource URL (or `/me/onenote/resources/{id}/content` path) to `dest_dir`.
    pub async fn download_onenote_resource_url(
        &self,
        resource_url: &str,
        file_name_hint: Option<&str>,
        mime_hint: Option<&str>,
        dest_dir: &Path,
    ) -> Result<DownloadedResource> {
        let trimmed = resource_url.trim();
        if trimmed.is_empty() {
            bail!("OneNote resource URL is empty.");
        }
        tokio::fs::create_dir_all(dest_dir)
            .await
            .with_context(|| format!("Failed to create download directory: {}", dest_dir.display()))?;

        let raw_name = file_name_hint
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| derive_resource_filename(trimmed, mime_hint))
            .unwrap_or_else(|| {
                let ext = mime_hint
                    .and_then(extension_for_content_type)
                    .unwrap_or("bin");
                format!("resource.{ext}")
            });
        let safe_name = sanitize_filename(&raw_name);
        let initial_path = deduplicate_dest_path(dest_dir, &safe_name);
        let initial_name = initial_path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&safe_name)
            .to_string();

        let use_bearer = should_send_bearer(&self.config.graph_base_url, trimmed);
        let size_bytes = self.get_to_file(trimmed, &initial_path, use_bearer).await?;

        let (final_path, final_name) =
            match maybe_refine_image_extension(&initial_path, dest_dir, &safe_name, &initial_name)
                .await
            {
                Some(refined) => refined,
                None => (initial_path, initial_name),
            };

        let (header_bytes, sniffed_mime) = sniff_file_header_and_mime(&final_path).await;
        let mime_type = sniffed_mime
            .map(str::to_string)
            .or_else(|| {
                mime_hint
                    .map(str::trim)
                    .filter(|s| !s.is_empty() && *s != "application/octet-stream")
                    .map(str::to_string)
            })
            .unwrap_or_else(|| guess_mime_type(&final_path).to_string());
        let is_image = is_raster_image_resource(&mime_type, &final_name, &header_bytes);

        Ok(DownloadedResource {
            ok: true,
            path: final_path.to_string_lossy().into_owned(),
            file_name: final_name,
            mime_type,
            size_bytes,
            is_image,
        })
    }
}

/// Returns `true` when the resource MIME/filename represents a supported image (raster or SVG)
/// and the downloaded header bytes are not spoofed HTML markup.
pub(crate) fn is_raster_image_resource(
    mime_type: &str,
    file_name: &str,
    header_bytes: &[u8],
) -> bool {
    if crate::img_preview::looks_like_html_bytes(header_bytes) {
        return false;
    }
    crate::img_preview::is_image_mime_or_name(mime_type, Some(file_name))
}

fn derive_resource_filename(url_or_path: &str, mime_hint: Option<&str>) -> Option<String> {
    let without_query = url_or_path.split(['?', '#']).next()?;
    let segments: Vec<&str> = without_query
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(pos) = segments.iter().position(|s| s.eq_ignore_ascii_case("resources")) {
        if let Some(res_id) = segments.get(pos + 1) {
            let ext = mime_hint
                .and_then(extension_for_content_type)
                .unwrap_or("bin");
            let short_id: String = res_id
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .take(24)
                .collect();
            if !short_id.is_empty() {
                return Some(format!("resource_{short_id}.{ext}"));
            }
        }
    }
    let last = *segments.last()?;
    if last.eq_ignore_ascii_case("$value") || last.eq_ignore_ascii_case("content") {
        let ext = mime_hint
            .and_then(extension_for_content_type)
            .unwrap_or("bin");
        return Some(format!("resource.{ext}"));
    }
    if last.contains('.') {
        Some(last.to_string())
    } else {
        let ext = mime_hint.and_then(extension_for_content_type)?;
        Some(format!("{last}.{ext}"))
    }
}

async fn sniff_file_header_and_mime(path: &Path) -> (Vec<u8>, Option<&'static str>) {
    let mut header = [0u8; 512];
    let n = match tokio::fs::File::open(path).await {
        Ok(mut f) => {
            let mut read = 0usize;
            while read < header.len() {
                match f.read(&mut header[read..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(chunk) => read += chunk,
                }
            }
            read
        }
        Err(_) => 0,
    };
    let slice = &header[..n];
    let mime = match sniff_image_extension(slice) {
        Some("png") => Some("image/png"),
        Some("jpg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        _ if crate::img_preview::looks_like_html_bytes(slice) => Some("text/html"),
        _ if crate::img_preview::looks_like_svg_bytes(slice) => Some("image/svg+xml"),
        _ => None,
    };
    (slice.to_vec(), mime)
}

pub fn should_send_bearer(graph_base_url: &str, target_url: &str) -> bool {
    let trimmed = target_url.trim();
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        return true;
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return !trimmed.starts_with("//") && !trimmed.contains("://") && !trimmed.is_empty();
    }
    let Ok(target) = url::Url::parse(trimmed) else {
        return false;
    };
    let Some(target_host) = target.host_str() else {
        return false;
    };
    if let Ok(base) = url::Url::parse(graph_base_url.trim()) {
        if let Some(base_host) = base.host_str() {
            if target.scheme() == base.scheme()
                && target.port_or_known_default() == base.port_or_known_default()
            {
                if target_host.eq_ignore_ascii_case(base_host) {
                    return true;
                }
                let is_loopback = |h: &str| {
                    h.eq_ignore_ascii_case("127.0.0.1") || h.eq_ignore_ascii_case("localhost")
                };
                if is_loopback(base_host) && is_loopback(target_host) {
                    return true;
                }
            }
        }
    }
    if target.scheme() == "https" {
        let lower = target_host.to_ascii_lowercase();
        if lower == "microsoft.com" || lower.ends_with(".microsoft.com") {
            return true;
        }
    }
    false
}

#[allow(dead_code)]
pub(crate) fn is_auth_or_forbidden_error(err: &anyhow::Error) -> bool {
    let msg = err.to_string();
    let detail = msg
        .strip_prefix(crate::graph::GRAPH_API_ERROR_PREFIX)
        .and_then(|rest| rest.split_once("): ").map(|(_, d)| d))
        .unwrap_or(&msg);
    let lower = detail.to_ascii_lowercase();
    detail.starts_with("401 ")
        || detail.starts_with("403 ")
        || lower.contains("unauthorized")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("erroraccessdenied")
        || lower.contains("insufficient privileges")
}

pub fn sanitize_filename(raw: &str) -> String {
    let leaf = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let mut cleaned: String = leaf
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "resource".to_string()
    } else {
        cleaned
    }
}

pub fn deduplicate_dest_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
    let candidate = dest_dir.join(safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let p = Path::new(safe_name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());

    for idx in 1..10_000 {
        let next_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({idx}).{e}"),
            _ => format!("{stem} ({idx})"),
        };
        let next_path = dest_dir.join(&next_name);
        if !next_path.exists() {
            return next_path;
        }
    }
    dest_dir.join(format!("{stem}_{}", Uuid::new_v4()))
}

pub fn guess_mime_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("log") => "text/plain",
        Some("csv") => "text/csv",
        Some("html") | Some("htm") => "text/html",
        Some("md") => "text/markdown",
        Some("json") => "application/json",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

pub fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 3 {
        return None;
    }
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("png"),
        image::ImageFormat::Jpeg => Some("jpg"),
        image::ImageFormat::Gif => Some("gif"),
        image::ImageFormat::WebP => Some("webp"),
        _ => None,
    }
}

pub(crate) fn extension_for_content_type(content_type: &str) -> Option<&'static str> {
    let ct = content_type
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match ct.as_str() {
        "image/png" => Some("png"),
        "image/jpeg" | "image/jpg" => Some("jpg"),
        "image/gif" => Some("gif"),
        "image/webp" => Some("webp"),
        "image/svg+xml" => Some("svg"),
        "application/pdf" => Some("pdf"),
        "text/plain" => Some("txt"),
        "text/html" => Some("html"),
        "application/json" => Some("json"),
        _ => None,
    }
}

pub(crate) async fn maybe_refine_image_extension(
    dest_path: &Path,
    dest_dir: &Path,
    safe_name: &str,
    final_name: &str,
) -> Option<(PathBuf, String)> {
    let cur_ext = Path::new(safe_name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    let is_generic = cur_ext.is_empty()
        || cur_ext.eq_ignore_ascii_case("bin")
        || safe_name.starts_with("inline-image-")
        || safe_name.starts_with("resource_")
        || safe_name.starts_with("resource.");
    if !is_generic {
        return None;
    }

    let mut header = [0u8; 16];
    let n = {
        let mut f = tokio::fs::File::open(dest_path).await.ok()?;
        let mut read = 0usize;
        while read < header.len() {
            let chunk = f.read(&mut header[read..]).await.ok()?;
            if chunk == 0 {
                break;
            }
            read += chunk;
        }
        read
    };
    let detected_ext = sniff_image_extension(&header[..n])?;

    if cur_ext.eq_ignore_ascii_case(detected_ext)
        || (cur_ext.eq_ignore_ascii_case("jpeg") && detected_ext == "jpg")
    {
        return None;
    }

    let candidate_path = Path::new(final_name).with_extension(detected_ext);
    let candidate_name = candidate_path.file_name()?.to_str()?;
    let target_path = deduplicate_dest_path(dest_dir, candidate_name);
    tokio::fs::rename(dest_path, &target_path).await.ok()?;
    let target_name = target_path
        .file_name()
        .and_then(|s| s.to_str())?
        .to_string();
    Some((target_path, target_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sanitize_and_deduplicate_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("re:po*rt?.pdf"), "re_po_rt_.pdf");
        assert_eq!(sanitize_filename(".."), "resource");

        let dir = tempdir().unwrap();
        let first = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        std::fs::write(&first, b"1").unwrap();

        let second = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (1).pdf");
        std::fs::write(&second, b"2").unwrap();

        let third = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(third.file_name().unwrap(), "report (2).pdf");
    }

    #[test]
    fn test_should_send_bearer_origin_guard() {
        let base = "https://graph.microsoft.com/v1.0";
        assert!(should_send_bearer(
            base,
            "/me/onenote/resources/res-1/$value"
        ));
        assert!(should_send_bearer(
            base,
            "https://graph.microsoft.com/v1.0/me/onenote/notebooks"
        ));
        assert!(should_send_bearer(
            base,
            "https://cdn.microsoft.com/file.bin"
        ));
        assert!(!should_send_bearer(
            base,
            "https://evil.example.com/steal"
        ));
        assert!(!should_send_bearer(base, "//evil.example.com/x.png"));
        assert!(!should_send_bearer(
            base,
            "http://graph.microsoft.com/v1.0/me"
        ));

        let mock_base = "http://127.0.0.1:4567/v1.0";
        assert!(should_send_bearer(
            mock_base,
            "http://127.0.0.1:4567/files/a.bin"
        ));
        assert!(should_send_bearer(
            mock_base,
            "http://localhost:4567/files/a.bin"
        ));
        assert!(!should_send_bearer(
            mock_base,
            "http://127.0.0.1:9999/files/a.bin"
        ));
    }

    #[test]
    fn test_sniff_image_extension_and_guess_mime_type() {
        let jpeg_hdr = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, b'J', b'F', b'I', b'F', 0x00, 0x01,
        ];
        assert_eq!(sniff_image_extension(&jpeg_hdr), Some("jpg"));
        let png_hdr = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(sniff_image_extension(&png_hdr), Some("png"));
        assert_eq!(sniff_image_extension(b"plain text"), None);
        assert_eq!(
            guess_mime_type(Path::new("deck.pptx")),
            "application/vnd.openxmlformats-officedocument.presentationml.presentation"
        );
        assert_eq!(guess_mime_type(Path::new("photo.PNG")), "image/png");
    }

    #[tokio::test]
    async fn test_svg_accepted_and_html_rejected_as_image() {
        let dir = tempdir().unwrap();

        // 1. SVG file is natively supported via Pure-Rust resvg
        let svg_path = dir.path().join("diagram.svg");
        let svg_bytes = br#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"><rect width="10" height="10"/></svg>"#;
        std::fs::write(&svg_path, svg_bytes).unwrap();
        let (hdr, sniffed) = sniff_file_header_and_mime(&svg_path).await;
        assert_eq!(sniffed, Some("image/svg+xml"));
        assert!(is_raster_image_resource("image/svg+xml", "diagram.svg", &hdr));

        // 2. HTML error/login page disguised as .png must still be rejected
        let html_path = dir.path().join("photo.png");
        let html_bytes = b"<!DOCTYPE html><html><body>Sign in required</body></html>";
        std::fs::write(&html_path, html_bytes).unwrap();
        let (html_hdr, html_sniffed) = sniff_file_header_and_mime(&html_path).await;
        assert_eq!(html_sniffed, Some("text/html"));
        assert!(!is_raster_image_resource("image/png", "photo.png", &html_hdr));
        assert!(!is_raster_image_resource("text/html", "photo.png", &html_hdr));

        // 3. Valid raster PNG is accepted
        let png_path = dir.path().join("real.png");
        let png_hdr = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 13];
        std::fs::write(&png_path, png_hdr).unwrap();
        let (real_hdr, real_sniffed) = sniff_file_header_and_mime(&png_path).await;
        assert_eq!(real_sniffed, Some("image/png"));
        assert!(is_raster_image_resource("image/png", "real.png", &real_hdr));
    }

    #[tokio::test]
    async fn test_fetch_data_image_uri_decodes_locally_without_http() {
        use base64::Engine as _;

        let mut cfg = crate::config::AppConfig::default();
        cfg.graph_base_url = "http://127.0.0.1:1/v1.0".to_string();
        let auth = crate::auth::AuthManager::empty_for_test(cfg.clone());
        let graph = GraphClient::new(cfg, auth).unwrap();

        // Build a valid 2x2 PNG in-memory and encode as data:image/png;base64,...
        let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([220, 40, 80, 255]));
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
        let data_uri = format!("data:image/png;base64,{b64}");

        let (mime, bytes) = graph
            .fetch_onenote_resource_bytes(&data_uri)
            .await
            .expect("data:image/png should decode locally without HTTP");
        assert_eq!(mime, "image/png");
        assert_eq!(bytes, png_bytes);

        // Percent-encoded SVG data URI is rasterized to PNG locally
        let svg_uri = "data:image/svg+xml,%3Csvg%20width%3D%2216%22%20height%3D%2216%22%3E%3Crect%20width%3D%2216%22%20height%3D%2216%22%20fill%3D%22%23e11d48%22/%3E%3C/svg%3E";
        let (svg_mime, svg_png) = graph
            .fetch_onenote_resource_bytes(svg_uri)
            .await
            .expect("data:image/svg+xml should rasterize to PNG locally");
        assert_eq!(svg_mime, "image/png");
        assert!(svg_png.starts_with(b"\x89PNG"));

        // Spoofed HTML inside data:image/png must be rejected
        let spoofed = "data:image/png;base64,PGh0bWw+PGJvZHk+SGk8L2JvZHk+PC9odG1sPg==";
        assert!(graph.fetch_onenote_resource_bytes(spoofed).await.is_err());
    }
}
