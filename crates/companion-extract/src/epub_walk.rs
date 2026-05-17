//! EPUB container walk (ADR-0043 § C.4b).
//!
//! An EPUB is a ZIP containing:
//!
//! - `META-INF/container.xml` — points at the root OPF package
//!   document.
//! - `<rootfile>/content.opf` (path varies) — has the spine
//!   ordering, manifest item declarations, and `dc:language`.
//! - Spine items — XHTML chapter documents.
//!
//! This module walks that structure, returning the concatenated
//! body text + the OPF-declared language. It hands the body to
//! [`crate::extract_name_dict`] for the C.4 name-dict step.
//!
//! ## Scope
//!
//! Read-only. No EPUB validation beyond what's needed to find
//! the spine. Bad EPUBs return [`EpubWalkError`]; the C.4
//! pipeline stage logs + skips them rather than failing the
//! whole batch.
//!
//! ## License posture
//!
//! Hand-rolled on top of the `zip` + `roxmltree` crates rather
//! than pulling in a dedicated EPUB crate, because the dedicated
//! options on crates.io are either GPL (`epub`) or have an API
//! shape that doesn't fit our pure-function model (`rbook`).
//! Apache-2.0 / MIT throughout.

use std::io::{Cursor, Read, Seek};

use roxmltree::Document;
use thiserror::Error;
use zip::ZipArchive;

use crate::{NameEntry, extract_name_dict_from_html};

/// EPUB body + declared language, as parsed from the container.
#[derive(Debug, Clone)]
pub struct EpubBody {
    /// Concatenated spine HTML, in spine order. The caller feeds
    /// this into [`crate::extract_name_dict_from_html`] (or calls
    /// [`extract_name_dict_from_epub`] which does it in one go).
    pub spine_html: String,
    /// `dc:language` from the OPF, lowercased. `None` if the
    /// metadata is missing or empty.
    pub language: Option<String>,
}

/// Errors raised by the EPUB walker. All variants are recoverable
/// — the C.4 pipeline stage maps them to a per-companion skip +
/// `tracing::warn!` rather than a batch abort.
#[derive(Debug, Error)]
pub enum EpubWalkError {
    /// ZIP container couldn't be opened or had a malformed central
    /// directory. Common for truncated downloads.
    #[error("epub zip open: {0}")]
    Zip(String),
    /// A required ZIP entry was missing (e.g. `META-INF/container.xml`).
    #[error("epub entry missing: {0}")]
    EntryMissing(String),
    /// A required XML document failed to parse.
    #[error("epub xml parse ({path}): {message}")]
    Xml {
        /// ZIP entry path that failed to parse.
        path: String,
        /// Parser-supplied detail.
        message: String,
    },
    /// XML structure didn't match the OPF / container schema we
    /// expect (missing `<rootfile>`, no spine, etc.).
    #[error("epub schema ({path}): {message}")]
    Schema {
        /// ZIP entry path with the schema violation.
        path: String,
        /// Human-readable description.
        message: String,
    },
}

const CONTAINER_XML: &str = "META-INF/container.xml";

/// Open an EPUB ZIP and return the concatenated spine HTML + the
/// declared `dc:language`. Pure function over the byte slice;
/// the caller owns the bytes (read from disk, S3, wherever).
pub fn walk_spine(epub_bytes: &[u8]) -> Result<EpubBody, EpubWalkError> {
    let cursor = Cursor::new(epub_bytes);
    let mut zip = ZipArchive::new(cursor).map_err(|e| EpubWalkError::Zip(e.to_string()))?;

    let opf_path = find_opf_path(&mut zip)?;
    let opf_xml = read_zip_entry(&mut zip, &opf_path)?;
    let opf_doc = Document::parse(&opf_xml).map_err(|e| EpubWalkError::Xml {
        path: opf_path.clone(),
        message: e.to_string(),
    })?;

    let language = read_language(&opf_doc);
    let spine_items = read_spine_hrefs(&opf_doc, &opf_path)?;

    let mut spine_html = String::new();
    for item_path in &spine_items {
        if let Ok(html) = read_zip_entry(&mut zip, item_path) {
            spine_html.push_str(&html);
            spine_html.push('\n');
        }
        // Missing spine items get logged-and-skipped by the
        // caller; the walker carries on so a single bad chapter
        // doesn't tank the whole book.
    }

    Ok(EpubBody {
        spine_html,
        language,
    })
}

/// One-shot helper: walk the EPUB then run [`extract_name_dict_from_html`]
/// on the concatenated spine. Returns the name dictionary +
/// language alongside it (the C.5 stage gates on language equality).
pub fn extract_name_dict_from_epub(
    epub_bytes: &[u8],
) -> Result<(Vec<NameEntry>, Option<String>), EpubWalkError> {
    let body = walk_spine(epub_bytes)?;
    Ok((extract_name_dict_from_html(&body.spine_html), body.language))
}

pub(crate) fn read_zip_entry<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    name: &str,
) -> Result<String, EpubWalkError> {
    let mut entry = zip
        .by_name(name)
        .map_err(|_| EpubWalkError::EntryMissing(name.to_owned()))?;
    let mut buf = String::new();
    entry
        .read_to_string(&mut buf)
        .map_err(|e| EpubWalkError::Xml {
            path: name.to_owned(),
            message: e.to_string(),
        })?;
    Ok(buf)
}

pub(crate) fn find_opf_path<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
) -> Result<String, EpubWalkError> {
    let xml = read_zip_entry(zip, CONTAINER_XML)?;
    let doc = Document::parse(&xml).map_err(|e| EpubWalkError::Xml {
        path: CONTAINER_XML.to_owned(),
        message: e.to_string(),
    })?;
    let root = doc.root_element();
    // `<container><rootfiles><rootfile full-path="..." />`
    let rootfile = root
        .descendants()
        .find(|n| n.has_tag_name("rootfile"))
        .ok_or_else(|| EpubWalkError::Schema {
            path: CONTAINER_XML.to_owned(),
            message: "no <rootfile> element".to_owned(),
        })?;
    let path = rootfile
        .attribute("full-path")
        .ok_or_else(|| EpubWalkError::Schema {
            path: CONTAINER_XML.to_owned(),
            message: "<rootfile> missing full-path attribute".to_owned(),
        })?;
    Ok(path.to_owned())
}

fn read_language(opf: &Document<'_>) -> Option<String> {
    opf.root_element()
        .descendants()
        .find(|n| n.has_tag_name("language"))
        .and_then(|n| n.text())
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
}

/// Walk `<manifest>` + `<spine>` to produce spine-ordered ZIP
/// entry paths. Resolves hrefs relative to the OPF's directory.
fn read_spine_hrefs(opf: &Document<'_>, opf_path: &str) -> Result<Vec<String>, EpubWalkError> {
    let root = opf.root_element();

    // Build id → href map from <manifest><item id="..." href="..."/>.
    let mut manifest: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(manifest_node) = root.descendants().find(|n| n.has_tag_name("manifest")) {
        for item in manifest_node.children().filter(|c| c.has_tag_name("item")) {
            if let (Some(id), Some(href)) = (item.attribute("id"), item.attribute("href")) {
                manifest.insert(id.to_owned(), href.to_owned());
            }
        }
    }

    let spine_node = root
        .descendants()
        .find(|n| n.has_tag_name("spine"))
        .ok_or_else(|| EpubWalkError::Schema {
            path: opf_path.to_owned(),
            message: "no <spine> element".to_owned(),
        })?;

    let opf_dir = opf_path.rsplit_once('/').map_or("", |(d, _)| d);

    let mut out = Vec::new();
    for itemref in spine_node.children().filter(|c| c.has_tag_name("itemref")) {
        let Some(idref) = itemref.attribute("idref") else {
            continue;
        };
        let Some(href) = manifest.get(idref) else {
            continue;
        };
        out.push(resolve_relative(opf_dir, href));
    }
    Ok(out)
}

/// Join an OPF-directory prefix with a manifest href. Strips
/// the fragment (`#chapter1`) since ZIP entries don't carry one.
fn resolve_relative(opf_dir: &str, href: &str) -> String {
    let href_no_frag = href.split_once('#').map_or(href, |(p, _)| p);
    if opf_dir.is_empty() {
        return href_no_frag.to_owned();
    }
    format!("{opf_dir}/{href_no_frag}")
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::io::Write;

    use zip::{ZipWriter, write::SimpleFileOptions};

    use super::*;

    fn build_minimal_epub(opf_path: &str, opf_body: &str, chapters: &[(&str, &str)]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = ZipWriter::new(cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);

            zip.start_file("mimetype", opts).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();

            zip.start_file(CONTAINER_XML, opts).unwrap();
            let container = format!(
                r#"<?xml version="1.0"?>
<container version="1.0" xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles>
    <rootfile full-path="{opf_path}" media-type="application/oebps-package+xml"/>
  </rootfiles>
</container>"#
            );
            zip.write_all(container.as_bytes()).unwrap();

            zip.start_file(opf_path, opts).unwrap();
            zip.write_all(opf_body.as_bytes()).unwrap();

            let opf_dir = opf_path.rsplit_once('/').map_or("", |(d, _)| d);
            for (href, body) in chapters {
                let full = if opf_dir.is_empty() {
                    (*href).to_owned()
                } else {
                    format!("{opf_dir}/{href}")
                };
                zip.start_file(full, opts).unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            zip.finish().unwrap();
        }
        buf
    }

    const SIMPLE_OPF: &str = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Test</dc:title>
    <dc:language>en</dc:language>
  </metadata>
  <manifest>
    <item id="c1" href="chapter1.xhtml" media-type="application/xhtml+xml"/>
    <item id="c2" href="chapter2.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine>
    <itemref idref="c1"/>
    <itemref idref="c2"/>
  </spine>
</package>"#;

    #[test]
    fn walk_spine_concatenates_chapters_in_order() {
        let epub = build_minimal_epub(
            "OEBPS/content.opf",
            SIMPLE_OPF,
            &[
                (
                    "chapter1.xhtml",
                    "<body>First chapter Kaladin spoke.</body>",
                ),
                (
                    "chapter2.xhtml",
                    "<body>Second chapter Kaladin returned.</body>",
                ),
            ],
        );
        let body = walk_spine(&epub).unwrap();
        assert!(body.spine_html.contains("First chapter"));
        assert!(body.spine_html.contains("Second chapter"));
        assert_eq!(body.language.as_deref(), Some("en"));
        // Order matters for downstream text-context heuristics.
        let first_idx = body.spine_html.find("First chapter").unwrap();
        let second_idx = body.spine_html.find("Second chapter").unwrap();
        assert!(first_idx < second_idx);
    }

    #[test]
    fn missing_container_xml_returns_entry_missing() {
        // Build a ZIP with mimetype only — no META-INF.
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut zip = ZipWriter::new(cursor);
            let opts =
                SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
            zip.start_file("mimetype", opts).unwrap();
            zip.write_all(b"application/epub+zip").unwrap();
            zip.finish().unwrap();
        }
        let err = walk_spine(&buf).unwrap_err();
        assert!(
            matches!(&err, EpubWalkError::EntryMissing(s) if s == CONTAINER_XML),
            "expected EntryMissing({CONTAINER_XML}), got {err:?}",
        );
    }

    #[test]
    fn extract_name_dict_from_epub_finds_proper_nouns() {
        let chapter = "<body>He saw Kaladin again. Later, Kaladin spoke. \
                       The truth was that Kaladin knew.</body>";
        let epub = build_minimal_epub(
            "OEBPS/content.opf",
            SIMPLE_OPF,
            &[
                ("chapter1.xhtml", chapter),
                ("chapter2.xhtml", "<body>more text</body>"),
            ],
        );
        let (dict, lang) = extract_name_dict_from_epub(&epub).unwrap();
        assert!(
            dict.iter()
                .any(|n| n.surface == "Kaladin" && n.frequency >= 3),
            "expected Kaladin >= 3, got {dict:?}",
        );
        assert_eq!(lang.as_deref(), Some("en"));
    }

    #[test]
    fn missing_language_metadata_returns_none() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0" unique-identifier="bookid">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:title>Test</dc:title>
  </metadata>
  <manifest>
    <item id="c1" href="c.xhtml" media-type="application/xhtml+xml"/>
  </manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#;
        let epub = build_minimal_epub("content.opf", opf, &[("c.xhtml", "<body>x</body>")]);
        let body = walk_spine(&epub).unwrap();
        assert!(body.language.is_none());
    }

    #[test]
    fn opf_in_root_resolves_chapter_paths_correctly() {
        // OPF at ZIP root → opf_dir is "" → href is used verbatim.
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/">
    <dc:language>de</dc:language>
  </metadata>
  <manifest><item id="c1" href="ch1.xhtml" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#;
        let epub = build_minimal_epub("content.opf", opf, &[("ch1.xhtml", "<body>hi</body>")]);
        let body = walk_spine(&epub).unwrap();
        assert!(body.spine_html.contains("hi"));
        assert_eq!(body.language.as_deref(), Some("de"));
    }

    #[test]
    fn spine_with_fragment_in_href_is_handled() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <metadata xmlns:dc="http://purl.org/dc/elements/1.1/"><dc:language>en</dc:language></metadata>
  <manifest><item id="c1" href="ch1.xhtml#part1" media-type="application/xhtml+xml"/></manifest>
  <spine><itemref idref="c1"/></spine>
</package>"#;
        let epub = build_minimal_epub("OEBPS/content.opf", opf, &[("ch1.xhtml", "<body>x</body>")]);
        let body = walk_spine(&epub).unwrap();
        assert!(body.spine_html.contains('x'));
    }

    #[test]
    fn corrupted_zip_returns_zip_error() {
        let bad = b"not actually a zip";
        let err = walk_spine(bad).unwrap_err();
        assert!(matches!(err, EpubWalkError::Zip(_)));
    }
}
