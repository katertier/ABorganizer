//! Known-fingerprint seed DB for the audiologo-audit binary
//! (ADR-0054, Phase 2B).
//!
//! ## Why a seed DB
//!
//! The operator's cascade rule (ADR-0054, Phase 2 wiring) prefers
//! *known* publisher jingles over freshly-detected candidates.
//! When the audit binary sees a clip whose chromaprint fingerprint
//! matches an entry here, it can short-circuit the slower
//! transcript/silence cascade.
//!
//! ## Sources
//!
//! At present the only seed source is **ABtagger findings JSON**
//! at `~/dev/ABtagger/tmp/audiologo_findings_macmini_*.json`
//! (and siblings produced during the ABtagger reference corpus
//! audit). Each entry has an `intro_fingerprint_b64` and/or
//! `outro_fingerprint_b64` already extracted by chromaprint.
//!
//! Future sources (operator-marked-confirmed-in-audit-UI,
//! upstream community feeds) plug in by emitting the same
//! [`SeedFingerprint`] shape.
//!
//! ## Confirmation
//!
//! Every imported fingerprint is `confirmed: false` until the
//! operator signs it off (Phase 2G UI work). Unconfirmed seeds
//! still feed the cascade as candidates — the matched-but-
//! unconfirmed state lets the report flag *"this looks like a
//! known Audible jingle, please confirm"* rather than asserting
//! it outright.

#![allow(
    clippy::missing_errors_doc,
    clippy::missing_const_for_fn,
    clippy::doc_markdown
)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// Which end of the audiobook this fingerprint anchors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Position {
    Intro,
    Outro,
}

impl Position {
    /// Short label for log messages + report rendering.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intro => "intro",
            Self::Outro => "outro",
        }
    }
}

/// One known-fingerprint record.
#[derive(Debug, Clone)]
pub struct SeedFingerprint {
    /// Publisher tag string (raw, untouched). Empty if the source
    /// row had no publisher attribution.
    pub publisher: Option<String>,
    /// Publisher-match label from the source detector (e.g.
    /// `"Audible"`, `"Brilliance Audio"`). Distinct from
    /// `publisher` because the detector may classify into a
    /// canonical bucket different from the file's raw tag.
    pub publisher_match: Option<String>,
    /// Whether this is an intro or outro fingerprint.
    pub position: Position,
    /// Base64-encoded chromaprint fingerprint (8-bit packed).
    pub fingerprint_b64: String,
    /// Duration of the audio span the fingerprint covers.
    pub duration_ms: u32,
    /// Short transcript excerpt covering the same span (helpful
    /// for the operator to see *what* the fingerprint represents
    /// without re-listening). May be `None` when the source did
    /// not record a transcript.
    pub transcript_excerpt: Option<String>,
    /// Where this row came from. Useful for debugging which seed
    /// file contributed a match.
    pub source: SeedSource,
    /// `false` until an operator signs off on this seed via the
    /// audit-UI confirm action (Phase 2G). Unconfirmed seeds
    /// still feed the cascade as candidates.
    pub confirmed: bool,
}

/// Provenance for a seed row.
#[derive(Debug, Clone)]
pub enum SeedSource {
    /// Loaded from an ABtagger `audiologo_findings_*.json` file.
    AbtaggerFindings { path: PathBuf },
    /// Operator-marked-confirmed in a prior audit run (lands in
    /// Phase 2G).
    OperatorConfirmed,
}

/// In-memory seed DB. Optimised for *match by publisher* (the
/// cascade's first-cut: narrow to candidates whose publisher tag
/// matches the book's, then chromaprint-compare).
#[derive(Debug, Default, Clone)]
pub struct SeedDb {
    pub fingerprints: Vec<SeedFingerprint>,
}

impl SeedDb {
    /// Empty seed DB. Useful as a default when no `--seed-fingerprints`
    /// path is passed.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Load every path's contents and merge into a single DB.
    /// Paths whose extension is `.json` are parsed as ABtagger
    /// findings JSON. Unknown extensions produce a hard error so
    /// the operator catches typos at startup.
    pub fn load(paths: &[PathBuf]) -> Result<Self> {
        let mut out = Self::default();
        for path in paths {
            out.extend(Self::load_one(path)?);
        }
        Ok(out)
    }

    fn extend(&mut self, other: Self) {
        self.fingerprints.extend(other.fingerprints);
    }

    fn load_one(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "json" {
            anyhow::bail!(
                "seed path {} has unsupported extension {:?}; expected .json",
                path.display(),
                ext,
            );
        }

        let bytes =
            std::fs::read(path).with_context(|| format!("read seed file {}", path.display()))?;
        let rows: Vec<AbtaggerRow> = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse ABtagger findings JSON {}", path.display()))?;

        let mut fingerprints = Vec::with_capacity(rows.len() * 2);
        for row in rows {
            if let Some(fp) = row.intro_fp() {
                fingerprints.push(SeedFingerprint {
                    publisher: row.publisher.clone(),
                    publisher_match: row.intro_publisher_match.clone(),
                    position: Position::Intro,
                    fingerprint_b64: fp,
                    duration_ms: row.intro_fingerprint_duration_ms.unwrap_or(0),
                    transcript_excerpt: row.intro_transcript.clone(),
                    source: SeedSource::AbtaggerFindings {
                        path: path.to_path_buf(),
                    },
                    confirmed: false,
                });
            }
            if let Some(fp) = row.outro_fp() {
                fingerprints.push(SeedFingerprint {
                    publisher: row.publisher.clone(),
                    publisher_match: row.outro_publisher_match.clone(),
                    position: Position::Outro,
                    fingerprint_b64: fp,
                    duration_ms: row.outro_fingerprint_duration_ms.unwrap_or(0),
                    transcript_excerpt: row.outro_transcript.clone(),
                    source: SeedSource::AbtaggerFindings {
                        path: path.to_path_buf(),
                    },
                    confirmed: false,
                });
            }
        }

        Ok(Self { fingerprints })
    }

    pub fn is_empty(&self) -> bool {
        self.fingerprints.is_empty()
    }

    pub fn len(&self) -> usize {
        self.fingerprints.len()
    }

    /// Group fingerprints by raw publisher tag for log emission +
    /// future cascade narrowing. `None`-publisher rows collect
    /// under the literal key `"(no publisher)"`.
    pub fn group_by_publisher(&self) -> HashMap<String, Vec<&SeedFingerprint>> {
        let mut out: HashMap<String, Vec<&SeedFingerprint>> = HashMap::new();
        for fp in &self.fingerprints {
            let key = fp
                .publisher
                .clone()
                .unwrap_or_else(|| "(no publisher)".to_owned());
            out.entry(key).or_default().push(fp);
        }
        out
    }
}

/// Schema for one row in an ABtagger `audiologo_findings_*.json`
/// file. Fields the audit binary doesn't use are deliberately
/// omitted — serde tolerates extra fields by default.
#[derive(Debug, Deserialize)]
struct AbtaggerRow {
    publisher: Option<String>,
    #[allow(dead_code)] // surfaces in future per-match logging
    title: Option<String>,
    intro_publisher_match: Option<String>,
    outro_publisher_match: Option<String>,
    intro_transcript: Option<String>,
    outro_transcript: Option<String>,
    intro_fingerprint_b64: Option<String>,
    outro_fingerprint_b64: Option<String>,
    intro_fingerprint_duration_ms: Option<u32>,
    outro_fingerprint_duration_ms: Option<u32>,
}

impl AbtaggerRow {
    fn intro_fp(&self) -> Option<String> {
        non_empty(self.intro_fingerprint_b64.as_deref())
    }
    fn outro_fp(&self) -> Option<String> {
        non_empty(self.outro_fingerprint_b64.as_deref())
    }
}

fn non_empty(s: Option<&str>) -> Option<String> {
    s.filter(|v| !v.is_empty()).map(str::to_owned)
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(body.as_bytes()).unwrap();
        path
    }

    #[test]
    fn position_label_round_trip() {
        assert_eq!(Position::Intro.as_str(), "intro");
        assert_eq!(Position::Outro.as_str(), "outro");
    }

    #[test]
    fn load_parses_intro_and_outro_separately() {
        let tmp = tempfile::TempDir::new().unwrap();
        let json = r#"[{
            "publisher": "Audible Originals",
            "title": "Test Book",
            "intro_publisher_match": "Audible",
            "outro_publisher_match": "Audible Originals",
            "intro_transcript": "This is Audible.",
            "outro_transcript": "Audible original publishing.",
            "intro_fingerprint_b64": "FP-INTRO-AA",
            "outro_fingerprint_b64": "FP-OUTRO-BB",
            "intro_fingerprint_duration_ms": 2620,
            "outro_fingerprint_duration_ms": 4000
        }]"#;
        let path = write_fixture(tmp.path(), "findings.json", json);

        let db = SeedDb::load(&[path]).expect("load");
        assert_eq!(db.len(), 2);
        let intro = db
            .fingerprints
            .iter()
            .find(|f| f.position == Position::Intro)
            .expect("intro present");
        assert_eq!(intro.fingerprint_b64, "FP-INTRO-AA");
        assert_eq!(intro.duration_ms, 2620);
        assert_eq!(
            intro.transcript_excerpt.as_deref(),
            Some("This is Audible.")
        );
        assert_eq!(intro.publisher_match.as_deref(), Some("Audible"));
        assert!(!intro.confirmed);

        let outro = db
            .fingerprints
            .iter()
            .find(|f| f.position == Position::Outro)
            .expect("outro present");
        assert_eq!(outro.fingerprint_b64, "FP-OUTRO-BB");
        assert_eq!(outro.duration_ms, 4000);
    }

    #[test]
    fn load_skips_missing_fingerprints() {
        let tmp = tempfile::TempDir::new().unwrap();
        let json = r#"[
          {"publisher": "X", "intro_fingerprint_b64": "FP1", "outro_fingerprint_b64": null},
          {"publisher": "Y", "intro_fingerprint_b64": null, "outro_fingerprint_b64": null},
          {"publisher": "Z", "intro_fingerprint_b64": "", "outro_fingerprint_b64": "FP2"}
        ]"#;
        let path = write_fixture(tmp.path(), "findings.json", json);

        let db = SeedDb::load(&[path]).expect("load");
        // X intro + Z outro = 2 rows; empty-string intro on Z dropped.
        assert_eq!(db.len(), 2);
        let publishers: Vec<_> = db
            .fingerprints
            .iter()
            .map(|f| f.publisher.clone())
            .collect();
        assert!(publishers.contains(&Some("X".to_owned())));
        assert!(publishers.contains(&Some("Z".to_owned())));
        assert!(!publishers.contains(&Some("Y".to_owned())));
    }

    #[test]
    fn load_rejects_non_json_extension() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "findings.txt", "[]");
        let err = SeedDb::load(&[path]).expect_err("non-json should fail");
        let msg = format!("{err:?}");
        assert!(msg.contains("unsupported extension"), "got: {msg}");
    }

    #[test]
    fn load_merges_multiple_files() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p1 = write_fixture(
            tmp.path(),
            "a.json",
            r#"[{"publisher":"A","intro_fingerprint_b64":"FPA"}]"#,
        );
        let p2 = write_fixture(
            tmp.path(),
            "b.json",
            r#"[{"publisher":"B","intro_fingerprint_b64":"FPB"}]"#,
        );
        let db = SeedDb::load(&[p1, p2]).expect("load");
        assert_eq!(db.len(), 2);
    }

    #[test]
    fn group_by_publisher_buckets_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let json = r#"[
          {"publisher": "A", "intro_fingerprint_b64": "FP-A"},
          {"publisher": null, "intro_fingerprint_b64": "FP-NONE"}
        ]"#;
        let path = write_fixture(tmp.path(), "f.json", json);
        let db = SeedDb::load(&[path]).expect("load");
        let by_pub = db.group_by_publisher();
        assert_eq!(by_pub.get("A").map(Vec::len), Some(1));
        assert_eq!(by_pub.get("(no publisher)").map(Vec::len), Some(1));
    }

    #[test]
    fn empty_db_has_no_fingerprints() {
        let db = SeedDb::empty();
        assert!(db.is_empty());
        assert_eq!(db.len(), 0);
    }
}
