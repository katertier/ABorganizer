//! EPUB navigation (`ToC`) chapter-title extraction.
//!
//! Sibling module to [`crate::epub_walk`] (which pulls spine HTML
//! for the C.4 name-dict path). This one returns the chapter
//! titles declared in the EPUB's navigation document — the
//! upstream chapter-list view that powers the eBook reader's `ToC`
//! UI.
//!
//! ## EPUB versions
//!
//! Two flavours exist in the wild:
//!
//! * **EPUB 3 nav document.** A manifest item with
//!   `properties="nav"` points at an XHTML file containing
//!   `<nav epub:type="toc">…<ol><li><a>Title</a></li>…</ol></nav>`.
//!   Modern default.
//! * **EPUB 2 NCX.** The OPF spine carries `toc="<id>"` referencing
//!   a manifest item that points at an `.ncx` XML document with
//!   `<navMap><navPoint><navLabel><text>Title</text></navLabel></navPoint></navMap>`.
//!   Older format, still common in catalogue exports.
//!
//! We try EPUB 3 nav first, fall back to EPUB 2 NCX. Returned
//! order is top-level entries only — no hierarchy flattening,
//! no per-sub-chapter splitting. The downstream pipeline stage
//! (`ab_catalog::epub_chapters`) needs a flat 1:1 mapping
//! against the audio files, so nested entries would be noise
//! anyway.

use std::io::Cursor;
use std::path::Path;

use roxmltree::Document;
use zip::ZipArchive;

use crate::EpubWalkError;

/// Read the EPUB nav doc and return the top-level chapter titles
/// in document order. Empty `Vec` when no nav document exists or
/// it parses to zero entries.
///
/// # Errors
///
/// Returns [`EpubWalkError`] when the ZIP can't be opened, the
/// container.xml / OPF entries are missing, or the OPF schema
/// doesn't match.
pub fn read_chapter_titles(epub_bytes: &[u8]) -> Result<Vec<String>, EpubWalkError> {
    let cursor = Cursor::new(epub_bytes);
    let mut zip = ZipArchive::new(cursor).map_err(|e| EpubWalkError::Zip(e.to_string()))?;

    let opf_path = crate::epub_walk::find_opf_path(&mut zip)?;
    let opf_xml = crate::epub_walk::read_zip_entry(&mut zip, &opf_path)?;
    let opf_doc = Document::parse(&opf_xml).map_err(|e| EpubWalkError::Xml {
        path: opf_path.clone(),
        message: e.to_string(),
    })?;

    let opf_dir = opf_directory(&opf_path);

    // EPUB 3: manifest item with properties="nav".
    if let Some(nav_href) = find_nav_href(&opf_doc) {
        let nav_path = resolve_href(opf_dir, &nav_href);
        if let Ok(nav_xml) = crate::epub_walk::read_zip_entry(&mut zip, &nav_path) {
            if let Ok(doc) = Document::parse(&nav_xml) {
                let titles = parse_epub3_nav(&doc);
                if !titles.is_empty() {
                    return Ok(titles);
                }
            }
        }
    }

    // EPUB 2: NCX referenced from <spine toc="...">.
    if let Some(ncx_href) = find_ncx_href(&opf_doc) {
        let ncx_path = resolve_href(opf_dir, &ncx_href);
        if let Ok(ncx_xml) = crate::epub_walk::read_zip_entry(&mut zip, &ncx_path) {
            if let Ok(doc) = Document::parse(&ncx_xml) {
                return Ok(parse_ncx_nav_map(&doc));
            }
        }
    }

    Ok(Vec::new())
}

/// Convenience: read the EPUB bytes from a path and run
/// [`read_chapter_titles`].
///
/// # Errors
///
/// Returns [`EpubWalkError::Zip`] for read failures (the I/O
/// error is wrapped into the same variant the ZIP layer uses).
pub fn read_chapter_titles_from_path(path: &Path) -> Result<Vec<String>, EpubWalkError> {
    let bytes = std::fs::read(path).map_err(|e| EpubWalkError::Zip(format!("read: {e}")))?;
    read_chapter_titles(&bytes)
}

fn opf_directory(opf_path: &str) -> &str {
    opf_path.rsplit_once('/').map_or("", |(d, _)| d)
}

fn resolve_href(dir: &str, href: &str) -> String {
    if dir.is_empty() {
        href.to_owned()
    } else {
        format!("{dir}/{href}")
    }
}

fn find_nav_href(opf: &Document<'_>) -> Option<String> {
    opf.root_element()
        .descendants()
        .filter(|n| n.has_tag_name("item"))
        .find(|n| {
            n.attribute("properties")
                .is_some_and(|p| p.split_whitespace().any(|word| word == "nav"))
        })
        .and_then(|n| n.attribute("href"))
        .map(str::to_owned)
}

fn find_ncx_href(opf: &Document<'_>) -> Option<String> {
    let root = opf.root_element();
    let toc_id = root
        .descendants()
        .find(|n| n.has_tag_name("spine"))
        .and_then(|n| n.attribute("toc"))?;
    root.descendants()
        .filter(|n| n.has_tag_name("item"))
        .find(|n| n.attribute("id") == Some(toc_id))
        .and_then(|n| n.attribute("href"))
        .map(str::to_owned)
}

fn parse_epub3_nav(doc: &Document<'_>) -> Vec<String> {
    // Find `<nav epub:type="toc">` (the `epub:type` attribute has
    // a namespace, but roxmltree exposes attributes by local name
    // via `attribute(("namespace", "name"))` — easiest is to scan
    // all `nav` elements and look for any attribute named "type"
    // whose value is "toc").
    let nav = doc
        .root_element()
        .descendants()
        .filter(|n| n.has_tag_name("nav"))
        .find(is_toc_nav);
    let Some(nav) = nav else {
        return Vec::new();
    };
    // Top-level <ol> direct child; collect each <li>'s first <a>
    // text content.
    let Some(ol) = nav.children().find(|c| c.has_tag_name("ol")) else {
        return Vec::new();
    };
    ol.children()
        .filter(|c| c.has_tag_name("li"))
        .filter_map(|li| li.descendants().find(|d| d.has_tag_name("a")))
        .filter_map(|a| Some(a.text()?.trim().to_owned()))
        .filter(|t| !t.is_empty())
        .collect()
}

fn is_toc_nav(n: &roxmltree::Node<'_, '_>) -> bool {
    n.attributes()
        .any(|a| a.name() == "type" && a.value() == "toc")
}

fn parse_ncx_nav_map(doc: &Document<'_>) -> Vec<String> {
    let Some(nav_map) = doc
        .root_element()
        .descendants()
        .find(|n| n.has_tag_name("navMap"))
    else {
        return Vec::new();
    };
    nav_map
        .children()
        .filter(|c| c.has_tag_name("navPoint"))
        .filter_map(|p| {
            p.children()
                .find(|c| c.has_tag_name("navLabel"))?
                .children()
                .find(|c| c.has_tag_name("text"))?
                .text()
                .map(|t| t.trim().to_owned())
        })
        .filter(|t| !t.is_empty())
        .collect()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    fn make_epub(
        opf_dir: &str,
        opf_xml: &str,
        nav_path_in_zip: Option<(&str, &str)>,
        ncx_path_in_zip: Option<(&str, &str)>,
    ) -> Vec<u8> {
        let mut buf = Vec::new();
        let mut zip = ZipWriter::new(Cursor::new(&mut buf));
        let opts = SimpleFileOptions::default();

        // mimetype + container.xml + OPF.
        zip.start_file("mimetype", opts).unwrap();
        zip.write_all(b"application/epub+zip").unwrap();

        zip.start_file("META-INF/container.xml", opts).unwrap();
        let opf_path = if opf_dir.is_empty() {
            "content.opf".to_owned()
        } else {
            format!("{opf_dir}/content.opf")
        };
        let container = format!(
            r#"<?xml version="1.0"?>
<container xmlns="urn:oasis:names:tc:opendocument:xmlns:container">
  <rootfiles><rootfile full-path="{opf_path}" media-type="application/oebps-package+xml"/></rootfiles>
</container>"#
        );
        zip.write_all(container.as_bytes()).unwrap();

        zip.start_file(&opf_path, opts).unwrap();
        zip.write_all(opf_xml.as_bytes()).unwrap();

        if let Some((name, body)) = nav_path_in_zip {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        if let Some((name, body)) = ncx_path_in_zip {
            zip.start_file(name, opts).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }

        let _ = zip.finish().unwrap();
        buf
    }

    #[test]
    fn epub3_nav_doc_top_level_li_titles() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine/>
</package>"#;
        let nav = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body>
<nav epub:type="toc">
  <ol>
    <li><a href="ch1.xhtml">Chapter One</a></li>
    <li><a href="ch2.xhtml">Chapter Two</a></li>
    <li><a href="ch3.xhtml">Chapter Three</a></li>
  </ol>
</nav>
</body>
</html>"#;
        let bytes = make_epub("", opf, Some(("nav.xhtml", nav)), None);
        let titles = read_chapter_titles(&bytes).expect("read titles");
        assert_eq!(titles, vec!["Chapter One", "Chapter Two", "Chapter Three"]);
    }

    #[test]
    fn epub2_ncx_nav_map_titles() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="2.0">
  <manifest>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"/>
</package>"#;
        let ncx = r#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap>
    <navPoint id="np1"><navLabel><text>Intro</text></navLabel><content src="ch1.html"/></navPoint>
    <navPoint id="np2"><navLabel><text>The Way</text></navLabel><content src="ch2.html"/></navPoint>
  </navMap>
</ncx>"#;
        let bytes = make_epub("", opf, None, Some(("toc.ncx", ncx)));
        let titles = read_chapter_titles(&bytes).expect("read titles");
        assert_eq!(titles, vec!["Intro", "The Way"]);
    }

    #[test]
    fn epub3_takes_precedence_over_ncx() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
    <item id="ncx" href="toc.ncx" media-type="application/x-dtbncx+xml"/>
  </manifest>
  <spine toc="ncx"/>
</package>"#;
        let nav = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><nav epub:type="toc"><ol>
  <li><a href="a.xhtml">From Nav</a></li>
</ol></nav></body></html>"#;
        let ncx = r#"<?xml version="1.0"?>
<ncx xmlns="http://www.daisy.org/z3986/2005/ncx/" version="2005-1">
  <navMap><navPoint><navLabel><text>From NCX</text></navLabel></navPoint></navMap>
</ncx>"#;
        let bytes = make_epub("", opf, Some(("nav.xhtml", nav)), Some(("toc.ncx", ncx)));
        let titles = read_chapter_titles(&bytes).expect("read titles");
        assert_eq!(titles, vec!["From Nav"]);
    }

    #[test]
    fn opf_in_subdir_resolves_nav_path() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest>
    <item id="nav" href="nav.xhtml" media-type="application/xhtml+xml" properties="nav"/>
  </manifest>
  <spine/>
</package>"#;
        let nav = r#"<?xml version="1.0"?>
<html xmlns="http://www.w3.org/1999/xhtml" xmlns:epub="http://www.idpf.org/2007/ops">
<body><nav epub:type="toc"><ol>
  <li><a href="x.xhtml">In Subdir</a></li>
</ol></nav></body></html>"#;
        let bytes = make_epub("OEBPS", opf, Some(("OEBPS/nav.xhtml", nav)), None);
        let titles = read_chapter_titles(&bytes).expect("read titles");
        assert_eq!(titles, vec!["In Subdir"]);
    }

    #[test]
    fn missing_nav_and_ncx_returns_empty() {
        let opf = r#"<?xml version="1.0"?>
<package xmlns="http://www.idpf.org/2007/opf" version="3.0">
  <manifest/>
  <spine/>
</package>"#;
        let bytes = make_epub("", opf, None, None);
        let titles = read_chapter_titles(&bytes).expect("read titles");
        assert!(titles.is_empty());
    }
}
