//! `GET /api/items/{id}/cover` — stream the embedded cover-art image.
//!
//! Slice C3c (final split of C3 / ADR-0028). ABS clients call
//! this to render the audiobook's cover; the path is what
//! [`crate::items::LibraryItem::media`] points at via the
//! `coverPath` field (populated when an embedded picture
//! exists; absent otherwise).
//!
//! ## Source of truth
//!
//! Reads the first `book_files` row for the book and pulls the
//! first attached `Picture` out of the primary tag via lofty.
//! Multi-file books typically carry the same cover on every
//! file; we pick the first to keep this read fast + bounded.
//!
//! No HTTP fallback to `books.cover_url` — the write-tags-early
//! stage (C3a) is what writes the cover into the file; books
//! that haven't been through that stage 404 here. The clean
//! recovery path is to re-run write-tags-early via the
//! `POST /books/:id/retry` flow; doing the lookup-and-fetch
//! every request would couple the read endpoint to the catalog
//! HTTP path.

use std::path::PathBuf;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use lofty::file::TaggedFileExt as _;
use lofty::picture::MimeType;

use crate::error::ShelfError;
use crate::state::ShelfState;

/// `GET /api/items/{id}/cover`.
///
/// Returns the raw image bytes with a `Content-Type` derived
/// from the picture's MIME field. Falls back to
/// `application/octet-stream` only when the file claims a MIME
/// we can't translate to a `Content-Type` string (lofty's
/// `MimeType::Unknown(...)` carries the original string
/// through, so this is rare).
///
/// # Errors
///
/// - [`ShelfError::BadRequest`] when `id` doesn't parse as `i64`.
/// - [`ShelfError::NotFound`] when the book row doesn't exist,
///   has no active files, or the first file has no embedded
///   picture.
/// - [`ShelfError::FileSystem`] when the file can't be opened
///   or parsed by lofty.
pub async fn get_cover(
    State(state): State<ShelfState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ShelfError> {
    let book_id: i64 = id
        .parse()
        .map_err(|_| ShelfError::BadRequest(format!("invalid item id: {id}")))?;

    // First active file for this book — same ordering as
    // `items::get_item` so the cover and the metadata stay
    // consistent.
    let row = sqlx::query!(
        r#"SELECT file_path AS "file_path!: String"
             FROM book_files
            WHERE book_id = ? AND is_active = 1
            ORDER BY file_id
            LIMIT 1"#,
        book_id,
    )
    .fetch_optional(state.library().pool())
    .await
    .map_err(|e| ShelfError::Database(format!("cover lookup: {e}")))?
    .ok_or_else(|| ShelfError::NotFound(format!("no active files for book_id {book_id}")))?;

    let path = PathBuf::from(&row.file_path);
    // Lofty's read is sync I/O. Run on the blocking pool so
    // the axum runtime worker doesn't stall on an mp4-atom
    // parse for a multi-hundred-MB file. The closure produces
    // `(bytes, mime_string)`; failure flows through as a
    // typed `ShelfError`.
    let extracted = tokio::task::spawn_blocking(move || extract_first_picture(&path))
        .await
        .map_err(|e| ShelfError::FileSystem(format!("cover join: {e}")))??;

    let (bytes, mime) = extracted;
    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(&mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    headers.insert(header::CONTENT_TYPE, content_type);
    // Cache-Control: small images that rarely change. 1 hour
    // gives clients enough to batch render passes; a re-tag
    // (C3a re-running) flips the bytes but operators can
    // hard-refresh.
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("public, max-age=3600"),
    );
    if let Ok(len_header) = HeaderValue::try_from(bytes.len().to_string()) {
        headers.insert(header::CONTENT_LENGTH, len_header);
    }

    Ok((StatusCode::OK, headers, Body::from(bytes)))
}

/// Open `path` with lofty and pull the first attached picture
/// off the primary tag. Returns the raw bytes + a renderable
/// MIME string.
///
/// Lofty's `Picture::mime_type()` returns `Option<&MimeType>`.
/// We mirror the enum's known variants to canonical web MIME
/// strings + pass the `Unknown(String)` payload through
/// verbatim. Missing MIME → `application/octet-stream`.
fn extract_first_picture(path: &std::path::Path) -> Result<(Vec<u8>, String), ShelfError> {
    let tagged = lofty::read_from_path(path)
        .map_err(|e| ShelfError::FileSystem(format!("lofty open {}: {e}", path.display())))?;
    let tag = tagged
        .primary_tag()
        .ok_or_else(|| ShelfError::NotFound(format!("no primary tag in {}", path.display())))?;
    let picture = tag.pictures().first().ok_or_else(|| {
        ShelfError::NotFound(format!("no embedded picture in {}", path.display()))
    })?;
    let mime = picture
        .mime_type()
        .map_or_else(|| "application/octet-stream".to_owned(), mime_to_str);
    Ok((picture.data().to_vec(), mime))
}

/// Convert lofty's [`MimeType`] to a canonical web string. The
/// `Unknown(String)` variant passes through verbatim so future
/// formats (`WebP` / AVIF / ...) keep their original strings.
fn mime_to_str(mime: &MimeType) -> String {
    match mime {
        MimeType::Png => "image/png".to_owned(),
        MimeType::Jpeg => "image/jpeg".to_owned(),
        MimeType::Gif => "image/gif".to_owned(),
        MimeType::Bmp => "image/bmp".to_owned(),
        MimeType::Tiff => "image/tiff".to_owned(),
        MimeType::Unknown(s) => s.clone(),
        // Lofty marks the enum non-exhaustive; fall back to
        // octet-stream so unknown variants don't break the
        // response.
        _ => "application/octet-stream".to_owned(),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use lofty::config::WriteOptions;
    use lofty::file::AudioFile as _;
    use lofty::picture::{Picture, PictureType};
    use lofty::tag::Tag;
    use std::path::Path as StdPath;

    /// Write a 1×1 PNG picture into a fresh m4a tag at `path`.
    /// Returns the embedded bytes so callers can byte-equal
    /// against the endpoint's response.
    fn embed_png_cover(path: &StdPath) -> Vec<u8> {
        // Minimal valid 1×1 PNG. Lofty validates `Picture`'s
        // bytes against the MIME during write — this PNG is a
        // real file byte-for-byte (8-byte signature + IHDR +
        // IDAT + IEND) so the validation succeeds.
        let png: Vec<u8> = vec![
            // PNG signature
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // IHDR
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x48, 0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00,
            0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1F, 0x15, 0xC4, 0x89, // IDAT
            0x00, 0x00, 0x00, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x62, 0x00, 0x01, 0x00,
            0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, // IEND
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        let mut tagged = lofty::read_from_path(path).expect("read");
        if tagged.primary_tag().is_none() {
            let primary_type = tagged.primary_tag_type();
            tagged.insert_tag(Tag::new(primary_type));
        }
        let tag = tagged.primary_tag_mut().expect("tag");
        tag.push_picture(
            Picture::unchecked(png.clone())
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
        tagged
            .save_to_path(path, WriteOptions::default())
            .expect("save");
        png
    }

    #[test]
    fn mime_to_str_maps_known_variants() {
        assert_eq!(mime_to_str(&MimeType::Png), "image/png");
        assert_eq!(mime_to_str(&MimeType::Jpeg), "image/jpeg");
        assert_eq!(mime_to_str(&MimeType::Gif), "image/gif");
        assert_eq!(mime_to_str(&MimeType::Bmp), "image/bmp");
        assert_eq!(
            mime_to_str(&MimeType::Unknown("image/webp".to_owned())),
            "image/webp"
        );
    }

    /// `extract_first_picture` reads back what `embed_png_cover`
    /// wrote. Skips when the bridge / system fixtures aren't
    /// available so Linux / no-swiftc builds stay green.
    #[tokio::test]
    async fn extract_first_picture_round_trips_a_known_png() {
        // Need a real audio container to tag. Synthesize a tiny
        // m4a via afconvert from a system-shipped AIFF, same as
        // the transcode tests use. Skip cleanly when the system
        // fixture is missing.
        let aiff = StdPath::new("/System/Library/Sounds/Submarine.aiff");
        if !aiff.exists() {
            return;
        }
        let tmp = tempfile::TempDir::new().expect("tmpdir");
        let m4a = tmp.path().join("fixture.m4a");
        let status = tokio::process::Command::new("/usr/bin/afconvert")
            .args(["-d", "aac", "-f", "m4af"])
            .arg(aiff)
            .arg(&m4a)
            .status()
            .await
            .expect("afconvert");
        if !status.success() {
            return;
        }
        let original = embed_png_cover(&m4a);
        let (bytes, mime) = extract_first_picture(&m4a).expect("extract");
        assert_eq!(bytes, original, "byte-for-byte round trip");
        assert_eq!(mime, "image/png", "MIME survives the round trip");
    }
}
