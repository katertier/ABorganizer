//! HTML report emitter for the audiologo-audit binary
//! (ADR-0054).
//!
//! Vanilla HTML + minimal CSS + inline JS. No SPA framework, no
//! bundler — opens directly in Safari / Chrome from the `--out`
//! directory.
//!
//! ## Pagination (Phase 2A)
//!
//! Single-file output didn't scale: a 20k-book library produced
//! a 68 MB `index.html` that loaded slowly and scrolled with
//! visible lag. Reports now split into `page-NN.html` files of
//! 50 books each, sorted by publisher (with `(no publisher)`
//! bucketed last). `index.html` becomes a TOC linking
//! `page-NN.html#{slug}` per book grouped under its publisher.
//!
//! Ratings persist in `localStorage` keyed by slug, so the
//! operator's annotations stay consistent across pages.

#![allow(
    clippy::needless_raw_string_hashes,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::missing_const_for_fn
)]

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::seed::SeedDb;
use super::{AuditEntry, DetailClip, DetectionInfo, SeedMatchSummary};

/// Books per HTML page. Picked to keep per-page DOM count under
/// what Safari handles smoothly even with inline SVG waveforms
/// (50 books × 2 waveforms = 100 inline SVGs ≈ 4 MB per page).
const PAGE_SIZE: usize = 50;

/// Sentinel for "(no publisher)" bucket — sorts after any real
/// publisher string. `\u{10FFFF}` is the highest code point;
/// no real publisher tag uses it.
const NO_PUBLISHER_SORT_KEY: &str = "\u{10FFFF}\u{10FFFF}\u{10FFFF}";
const NO_PUBLISHER_DISPLAY: &str = "(no publisher)";

/// Write paginated report (`index.html` TOC + `page-NN.html` pages
/// + `data.json` backup) to `out_dir`.
///
/// # Errors
///
/// I/O errors creating the output files surface as anyhow.
pub fn write_report(
    out_dir: &Path,
    corpus_path: &Path,
    entries: &[AuditEntry],
    seeds: &SeedDb,
) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    // Sort: publisher ASC (no-publisher bucket last), then title ASC.
    let mut sorted: Vec<&AuditEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        let pa = publisher_sort_key(a.publisher.as_deref());
        let pb = publisher_sort_key(b.publisher.as_deref());
        pa.cmp(&pb).then_with(|| a.title.cmp(&b.title))
    });

    let total_books = sorted.len();
    let total_pages = total_books.div_ceil(PAGE_SIZE).max(1);

    let seed_counts = seed_publisher_counts(seeds);

    // toc_rows[page_idx] = Vec<(publisher_display, &AuditEntry)>
    // We also build a global TOC view grouped by publisher → pages.
    let mut pages: Vec<Vec<&AuditEntry>> = Vec::with_capacity(total_pages);
    for chunk in sorted.chunks(PAGE_SIZE) {
        pages.push(chunk.to_vec());
    }
    if pages.is_empty() {
        pages.push(Vec::new());
    }

    for (idx, page_entries) in pages.iter().enumerate() {
        let page_num = idx + 1;
        let page_name = page_filename(page_num);
        let html = render_page(
            corpus_path,
            page_entries,
            page_num,
            total_pages,
            &seed_counts,
        );
        let page_path = out_dir.join(&page_name);
        fs::write(&page_path, html).with_context(|| format!("write {}", page_path.display()))?;
    }

    let toc_html = render_toc(corpus_path, &pages, total_books, &seed_counts);
    let toc_path = out_dir.join("index.html");
    fs::write(&toc_path, toc_html).with_context(|| format!("write {}", toc_path.display()))?;

    let data = serde_json::json!({
        "schema": "audiologo-audit-v2",
        "corpus_path": corpus_path.display().to_string(),
        "total_books": total_books,
        "total_pages": total_pages,
        "page_size": PAGE_SIZE,
        "seed_publishers": seeds.group_by_publisher().len(),
        "seed_fingerprints": seeds.len(),
        "books": sorted.iter().map(|e| book_to_json(e, &seed_counts)).collect::<Vec<_>>(),
    });
    let data_path = out_dir.join("data.json");
    fs::write(&data_path, serde_json::to_string_pretty(&data)?)
        .with_context(|| format!("write {}", data_path.display()))?;

    Ok(())
}

/// `publisher_display_string -> count`. Lookup helper for the
/// per-book + TOC seed-coverage badge. Reads via
/// [`publisher_display`] so the "(no publisher)" sentinel
/// matches what the report renders.
fn seed_publisher_counts(seeds: &SeedDb) -> HashMap<String, usize> {
    seeds
        .group_by_publisher()
        .into_iter()
        .map(|(k, v)| (k, v.len()))
        .collect()
}

fn seed_count_for(counts: &HashMap<String, usize>, publisher: Option<&str>) -> usize {
    let key = publisher_display(publisher);
    counts.get(key).copied().unwrap_or(0)
}

fn seed_match_to_json(m: &SeedMatchSummary) -> serde_json::Value {
    serde_json::json!({
        "publisher": m.publisher,
        "confidence": m.confidence,
        "seed_windows": m.seed_windows,
        "window_offset": m.window_offset,
        "approx_offset_ms": m.approx_offset_ms,
    })
}

fn publisher_sort_key(p: Option<&str>) -> String {
    match p {
        Some(s) if !s.trim().is_empty() => s.to_lowercase(),
        _ => NO_PUBLISHER_SORT_KEY.to_string(),
    }
}

fn publisher_display(p: Option<&str>) -> &str {
    match p {
        Some(s) if !s.trim().is_empty() => s,
        _ => NO_PUBLISHER_DISPLAY,
    }
}

fn page_filename(page_num: usize) -> String {
    format!("page-{page_num:02}.html")
}

fn book_to_json(e: &AuditEntry, seed_counts: &HashMap<String, usize>) -> serde_json::Value {
    let detection = match &e.detection {
        DetectionInfo::Stub => serde_json::json!({"state": "stub"}),
        DetectionInfo::NoCandidate => serde_json::json!({"state": "no_candidate"}),
        DetectionInfo::SeedMatch { front, end } => serde_json::json!({
            "state": "seed_match",
            "front": front.as_ref().map(seed_match_to_json),
            "end": end.as_ref().map(seed_match_to_json),
        }),
        DetectionInfo::Detected {
            method_label,
            trigger_summary,
            front_cut_ms,
            end_cut_ms,
        } => serde_json::json!({
            "state": "detected",
            "method": method_label,
            "trigger": trigger_summary,
            "front_cut_ms": front_cut_ms,
            "end_cut_ms": end_cut_ms,
        }),
    };
    serde_json::json!({
        "slug": e.slug,
        "title": e.title,
        "source_path": e.source_path.display().to_string(),
        "duration_ms": e.duration_ms,
        "publisher": e.publisher,
        "publisher_seed_count": seed_count_for(seed_counts, e.publisher.as_deref()),
        "copyright": e.copyright,
        "detection": detection,
        "front_clip": e.front_clip_rel,
        "end_clip": e.end_clip_rel,
    })
}

fn render_toc(
    corpus_path: &Path,
    pages: &[Vec<&AuditEntry>],
    total_books: usize,
    seed_counts: &HashMap<String, usize>,
) -> String {
    // Group every book by publisher → list of (page_num, &AuditEntry).
    let mut by_publisher: BTreeMap<String, Vec<(usize, &AuditEntry)>> = BTreeMap::new();
    for (page_idx, page_entries) in pages.iter().enumerate() {
        let page_num = page_idx + 1;
        for entry in page_entries {
            let key = publisher_sort_key(entry.publisher.as_deref());
            by_publisher.entry(key).or_default().push((page_num, entry));
        }
    }

    let mut groups_html = String::new();
    for items in by_publisher.values() {
        let pub_display = publisher_display(items[0].1.publisher.as_deref());
        let label = if items.len() == 1 { "book" } else { "books" };
        let seeds = seed_count_for(seed_counts, items[0].1.publisher.as_deref());
        let seed_badge = if seeds > 0 {
            format!(
                r#" <span class="seed-badge" title="known fingerprint seeds for this publisher">{seeds} seed{plural}</span>"#,
                plural = if seeds == 1 { "" } else { "s" },
            )
        } else {
            String::new()
        };
        let _ = write!(
            groups_html,
            r#"<section class="pub-group">
  <h2>{pub} <span class="count">({count} {label})</span>{seed_badge}</h2>
  <ul class="book-list">
"#,
            pub = html_escape(pub_display),
            count = items.len(),
        );
        for (page_num, entry) in items {
            let page_name = page_filename(*page_num);
            let slug_escaped = html_escape(&entry.slug);
            let _ = writeln!(
                groups_html,
                r#"    <li><a href="{page_name}#{slug_escaped}">{title}</a> <span class="muted">p.{page_num}</span></li>"#,
                title = html_escape(&entry.title),
            );
        }
        groups_html.push_str("  </ul>\n</section>\n");
    }

    // "Seed coverage" addendum: publishers we have seed fingerprints
    // for whose audiobooks AREN'T in this corpus — useful for the
    // operator to spot library gaps + over-broad seed sources.
    let corpus_publishers: std::collections::HashSet<String> = by_publisher
        .values()
        .map(|v| publisher_display(v[0].1.publisher.as_deref()).to_owned())
        .collect();
    let mut orphan_seeds: Vec<(&String, &usize)> = seed_counts
        .iter()
        .filter(|(k, _)| !corpus_publishers.contains(*k))
        .collect();
    orphan_seeds.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
    let orphan_seeds_html = if orphan_seeds.is_empty() {
        String::new()
    } else {
        let mut s =
            r#"<section class="pub-group orphans">
  <h2>Seeds without books in corpus <span class="count">(seed publishers absent from this corpus)</span></h2>
  <ul class="book-list">
"#
            .to_owned();
        for (publisher, count) in orphan_seeds {
            let _ = writeln!(
                s,
                r#"    <li>{publisher} <span class="muted">{count} seed{plural}</span></li>"#,
                publisher = html_escape(publisher),
                plural = if *count == 1 { "" } else { "s" },
            );
        }
        s.push_str("  </ul>\n</section>\n");
        s
    };

    let total_pages = pages.len();
    let total_seeds: usize = seed_counts.values().sum();
    let head = include_toc_head();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
{head}
<title>Audiologo audit — {corpus}</title>
</head>
<body>
<header>
  <h1>Audiologo audit · TOC</h1>
  <div class="meta">
    <span>Corpus: <code>{corpus}</code></span>
    <span>{total_books} books · {total_pages} pages · {page_size}/page</span>
    <span>{seed_pubs} seed publishers · {total_seeds} fingerprints</span>
  </div>
</header>
<main class="toc">
{groups_html}{orphan_seeds_html}
</main>
</body>
</html>"#,
        corpus = html_escape(&corpus_path.display().to_string()),
        page_size = PAGE_SIZE,
        seed_pubs = seed_counts.len(),
    )
}

fn render_page(
    corpus_path: &Path,
    entries: &[&AuditEntry],
    page_num: usize,
    total_pages: usize,
    seed_counts: &HashMap<String, usize>,
) -> String {
    let mut sections = String::new();
    for e in entries {
        sections.push_str(&render_book_section(e, seed_counts));
    }

    let head = include_head();
    let scripts = include_scripts();
    let prev_link = if page_num > 1 {
        format!(
            r#"<a class="pager" href="{href}">← prev</a>"#,
            href = page_filename(page_num - 1)
        )
    } else {
        r#"<span class="pager disabled">← prev</span>"#.to_string()
    };
    let next_link = if page_num < total_pages {
        format!(
            r#"<a class="pager" href="{href}">next →</a>"#,
            href = page_filename(page_num + 1)
        )
    } else {
        r#"<span class="pager disabled">next →</span>"#.to_string()
    };

    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
{head}
<title>Audiologo audit — page {page_num}/{total_pages}</title>
</head>
<body>
<header>
  <h1>Audiologo audit · page {page_num}/{total_pages}</h1>
  <div class="meta">
    <span>Corpus: <code>{corpus}</code></span>
    <span id="progress" class="progress"></span>
  </div>
  <div class="toolbar">
    <a class="pager" href="index.html">↑ TOC</a>
    {prev_link}
    {next_link}
    <label>Filter:
      <select id="filter">
        <option value="all">All</option>
        <option value="unreviewed">Unreviewed</option>
        <option value="good">Good</option>
        <option value="improve">Improve</option>
        <option value="bad">Bad</option>
      </select>
    </label>
    <button type="button" id="export">Export Report</button>
  </div>
</header>
<main>
{sections}
</main>
{scripts}
</body>
</html>"#,
        corpus = html_escape(&corpus_path.display().to_string()),
    )
}

fn render_book_section(e: &AuditEntry, seed_counts: &HashMap<String, usize>) -> String {
    let pub_seeds = seed_count_for(seed_counts, e.publisher.as_deref());
    let publisher_seed_chip = if pub_seeds > 0 {
        format!(
            r#" <span class="seed-badge">{pub_seeds} known seed{plural}</span>"#,
            plural = if pub_seeds == 1 { "" } else { "s" },
        )
    } else {
        String::new()
    };
    let (method_html, trigger_html, front_cut_disp, end_cut_disp) = match &e.detection {
        DetectionInfo::Stub => (
            r#"<span class="badge stub">No seed match</span>"#.to_string(),
            "<em>No publisher-compatible seed matched. Operator rates the clips; transcript / silence cascade not yet wired.</em>".to_string(),
            "—".to_string(),
            "—".to_string(),
        ),
        DetectionInfo::NoCandidate => (
            r#"<span class="badge none">No candidate</span>"#.to_string(),
            "Detection ran; no method fired above threshold. Book likely clean (no audiologo)."
                .to_string(),
            "—".to_string(),
            "—".to_string(),
        ),
        DetectionInfo::SeedMatch { front, end } => {
            let trigger = format_seed_match_trigger(front.as_ref(), end.as_ref());
            let front_disp = front
                .as_ref()
                .map_or_else(|| "—".to_string(), |m| format_ms(Some(m.approx_offset_ms)));
            let end_disp = end
                .as_ref()
                .map_or_else(|| "—".to_string(), |m| format_ms(Some(m.approx_offset_ms)));
            (
                r#"<span class="badge seed-match">Seed match</span>"#.to_string(),
                trigger,
                front_disp,
                end_disp,
            )
        }
        DetectionInfo::Detected {
            method_label,
            trigger_summary,
            front_cut_ms,
            end_cut_ms,
        } => (
            format!(
                r#"<span class="badge detected">{}</span>"#,
                html_escape(method_label)
            ),
            html_escape(trigger_summary),
            format_ms(*front_cut_ms),
            format_ms(*end_cut_ms),
        ),
    };

    format!(
        r##"<section class="book" id="{slug}" data-slug="{slug}" data-rating="">
  <h2>{title}</h2>
  <div class="src"><code>{path}</code></div>
  <div class="row">
    <div class="cell">
      <div class="label">Detection method</div>
      <div>{method_html}</div>
    </div>
    <div class="cell">
      <div class="label">Trigger</div>
      <div>{trigger_html}</div>
    </div>
    <div class="cell">
      <div class="label">Duration</div>
      <div>{duration}</div>
    </div>
  </div>
  <div class="row">
    <div class="cell">
      <div class="label">Publisher</div>
      <div>{publisher}{publisher_seed_chip}</div>
    </div>
    <div class="cell">
      <div class="label">Copyright</div>
      <div>{copyright}</div>
    </div>
  </div>
  <div class="cuts">
    <div class="cut-half">
      <h3>Front · cut @ {front_cut_disp}</h3>
      <div class="wave">{front_waveform}</div>
      <audio controls preload="none" src="{front_clip}"></audio>
      {front_detail_html}
    </div>
    <div class="cut-half">
      <h3>End · cut @ {end_cut_disp}</h3>
      <div class="wave">{end_waveform}</div>
      <audio controls preload="none" src="{end_clip}"></audio>
      {end_detail_html}
    </div>
  </div>
  <div class="rating">
    <label><input type="radio" name="rating-{slug}" value="good"> Good</label>
    <label><input type="radio" name="rating-{slug}" value="improve"> Improve</label>
    <label><input type="radio" name="rating-{slug}" value="bad"> Bad</label>
    <span class="save-state" data-state="unsaved">unsaved</span>
  </div>
  <div class="comment">
    <textarea placeholder="Notes for this book (visible in exported report)…"></textarea>
  </div>
</section>
"##,
        slug = html_escape(&e.slug),
        title = html_escape(&e.title),
        path = html_escape(&e.source_path.display().to_string()),
        duration = format_ms(Some(e.duration_ms)),
        publisher = html_escape(e.publisher.as_deref().unwrap_or("—")),
        copyright = html_escape(e.copyright.as_deref().unwrap_or("—")),
        front_waveform = e.front_waveform_svg,
        end_waveform = e.end_waveform_svg,
        front_clip = html_escape(&e.front_clip_rel),
        end_clip = html_escape(&e.end_clip_rel),
        front_detail_html = render_detail_clip(e.front_detail.as_ref()),
        end_detail_html = render_detail_clip(e.end_detail.as_ref()),
    )
}

/// Render the optional 15s detail clip alongside the 60s overview.
/// `None` collapses to the empty string so the operator sees a
/// clean two-clip layout only when there's a match worth focusing
/// on.
fn render_detail_clip(detail: Option<&DetailClip>) -> String {
    let Some(d) = detail else {
        return String::new();
    };
    format!(
        r##"<div class="detail-clip">
        <h4>Detail · {duration}s from {start} <span class="muted">in the overview</span></h4>
        <div class="wave">{waveform}</div>
        <audio controls preload="none" src="{clip}"></audio>
      </div>"##,
        duration = d.duration_secs,
        start = format_ms(Some(d.start_offset_in_overview_ms)),
        waveform = d.waveform_svg,
        clip = html_escape(&d.clip_rel),
    )
}

/// Build the per-book "Trigger" cell body for a `SeedMatch`.
/// Surfaces the matched publisher + confidence per side so the
/// operator sees at a glance which jingle matched (and how
/// confidently).
fn format_seed_match_trigger(
    front: Option<&SeedMatchSummary>,
    end: Option<&SeedMatchSummary>,
) -> String {
    let mut out = String::new();
    if let Some(f) = front {
        let _ = write!(
            out,
            r#"<div>Front: <strong>{pub}</strong> · cosine {conf:.3} <span class="muted">({windows} × 100ms windows)</span></div>"#,
            pub = html_escape(f.publisher.as_deref().unwrap_or("?")),
            conf = f.confidence,
            windows = f.seed_windows,
        );
    }
    if let Some(e) = end {
        let _ = write!(
            out,
            r#"<div>End: <strong>{pub}</strong> · cosine {conf:.3} <span class="muted">({windows} × 100ms windows)</span></div>"#,
            pub = html_escape(e.publisher.as_deref().unwrap_or("?")),
            conf = e.confidence,
            windows = e.seed_windows,
        );
    }
    if out.is_empty() {
        "<em>(no side matched)</em>".to_owned()
    } else {
        out
    }
}

fn format_ms(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "—".to_string();
    };
    let total_secs = ms / 1000;
    let mm = total_secs / 60;
    let ss = total_secs % 60;
    let mmm = ms % 1000;
    format!("{mm:02}:{ss:02}.{mmm:03}")
}

fn html_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

fn include_head() -> &'static str {
    // Page (per-book) CSS — shared base + book-section styling.
    r##"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
:root {
  color-scheme: light dark;
  --bg: #ffffff;
  --fg: #1f2937;
  --muted: #6b7280;
  --accent: #2563eb;
  --good: #16a34a;
  --improve: #f59e0b;
  --bad: #dc2626;
  --card-bg: #f9fafb;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0f172a;
    --fg: #e5e7eb;
    --muted: #94a3b8;
    --card-bg: #1e293b;
  }
}
body {
  font-family: -apple-system, system-ui, sans-serif;
  background: var(--bg);
  color: var(--fg);
  margin: 0;
  padding: 0;
}
header {
  position: sticky;
  top: 0;
  background: var(--bg);
  border-bottom: 1px solid var(--muted);
  padding: 16px 24px;
  z-index: 10;
}
header h1 {
  margin: 0 0 6px;
  font-size: 1.4rem;
}
.meta {
  display: flex;
  gap: 18px;
  font-size: 0.9rem;
  color: var(--muted);
  flex-wrap: wrap;
}
.toolbar {
  margin-top: 12px;
  display: flex;
  gap: 16px;
  align-items: center;
  flex-wrap: wrap;
}
.toolbar button {
  background: var(--accent);
  color: white;
  border: 0;
  padding: 6px 14px;
  border-radius: 4px;
  font-size: 0.9rem;
  cursor: pointer;
}
.pager {
  font-size: 0.9rem;
  color: var(--accent);
  text-decoration: none;
  padding: 4px 8px;
  border-radius: 4px;
  border: 1px solid var(--muted);
}
.pager:hover { background: var(--card-bg); }
.pager.disabled { color: var(--muted); border-color: var(--muted); opacity: 0.5; }
main {
  padding: 24px;
  display: flex;
  flex-direction: column;
  gap: 32px;
}
.book {
  background: var(--card-bg);
  border-radius: 8px;
  padding: 20px;
  border: 1px solid transparent;
  scroll-margin-top: 120px;
}
.book[data-rating="good"] { border-color: var(--good); }
.book[data-rating="improve"] { border-color: var(--improve); }
.book[data-rating="bad"] { border-color: var(--bad); }
.book h2 { margin: 0 0 4px; font-size: 1.1rem; }
.src code {
  font-size: 0.8rem;
  color: var(--muted);
}
.row { display: flex; gap: 18px; margin: 14px 0; flex-wrap: wrap; }
.cell { flex: 1; min-width: 200px; }
.label {
  font-size: 0.75rem;
  text-transform: uppercase;
  letter-spacing: 0.04em;
  color: var(--muted);
  margin-bottom: 4px;
}
.badge {
  display: inline-block;
  padding: 2px 8px;
  border-radius: 3px;
  font-size: 0.85rem;
  font-family: monospace;
}
.badge.stub { background: #fef3c7; color: #92400e; }
.badge.none { background: #e0e7ff; color: #3730a3; }
.badge.detected { background: #dcfce7; color: #166534; }
.badge.seed-match { background: #ede9fe; color: #5b21b6; }
.detail-clip {
  margin-top: 14px;
  padding: 12px;
  border-radius: 6px;
  background: rgba(91, 33, 182, 0.07);
  border-left: 3px solid #5b21b6;
}
.detail-clip h4 {
  margin: 0 0 8px;
  font-size: 0.85rem;
  font-weight: 600;
  color: #5b21b6;
}
@media (prefers-color-scheme: dark) {
  .detail-clip {
    background: rgba(167, 139, 250, 0.1);
    border-left-color: #a78bfa;
  }
  .detail-clip h4 { color: #c4b5fd; }
}
.seed-badge {
  display: inline-block;
  margin-left: 8px;
  padding: 1px 6px;
  border-radius: 3px;
  font-size: 0.75rem;
  font-family: monospace;
  background: #ddd6fe;
  color: #5b21b6;
}
.cuts { display: flex; gap: 18px; margin: 18px 0 14px; flex-wrap: wrap; }
.cut-half { flex: 1; min-width: 360px; }
.cut-half h3 { font-size: 0.9rem; font-weight: 600; margin: 0 0 8px; }
.wave { background: #f5f7fa; border-radius: 4px; overflow: hidden; }
.wave svg { display: block; }
audio { width: 100%; margin-top: 6px; }
.rating { display: flex; gap: 18px; align-items: center; margin-bottom: 10px; }
.rating label { font-size: 0.95rem; cursor: pointer; }
.save-state {
  font-size: 0.8rem;
  margin-left: auto;
  color: var(--muted);
}
.save-state[data-state="saved"] { color: var(--good); }
.comment textarea {
  width: 100%;
  min-height: 60px;
  font-family: inherit;
  font-size: 0.9rem;
  padding: 8px;
  border: 1px solid var(--muted);
  border-radius: 4px;
  background: var(--bg);
  color: var(--fg);
  box-sizing: border-box;
}
.progress { font-variant-numeric: tabular-nums; }
.hidden { display: none !important; }
</style>"##
}

fn include_toc_head() -> &'static str {
    // TOC CSS — shared base + grouping styles. No book-section
    // markup on the TOC page so its DOM stays tiny.
    r##"<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
:root {
  color-scheme: light dark;
  --bg: #ffffff;
  --fg: #1f2937;
  --muted: #6b7280;
  --accent: #2563eb;
  --card-bg: #f9fafb;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #0f172a;
    --fg: #e5e7eb;
    --muted: #94a3b8;
    --card-bg: #1e293b;
  }
}
body {
  font-family: -apple-system, system-ui, sans-serif;
  background: var(--bg);
  color: var(--fg);
  margin: 0;
  padding: 0;
}
header {
  background: var(--bg);
  border-bottom: 1px solid var(--muted);
  padding: 16px 24px;
}
header h1 { margin: 0 0 6px; font-size: 1.4rem; }
.meta {
  display: flex;
  gap: 18px;
  font-size: 0.9rem;
  color: var(--muted);
  flex-wrap: wrap;
}
main.toc {
  padding: 24px;
  display: flex;
  flex-direction: column;
  gap: 24px;
}
.pub-group {
  background: var(--card-bg);
  border-radius: 8px;
  padding: 16px 20px;
}
.pub-group h2 {
  margin: 0 0 8px;
  font-size: 1.05rem;
}
.pub-group .count {
  color: var(--muted);
  font-weight: normal;
  font-size: 0.85rem;
}
.book-list {
  margin: 0;
  padding: 0;
  list-style: none;
  display: grid;
  grid-template-columns: repeat(auto-fill, minmax(360px, 1fr));
  gap: 4px 18px;
}
.book-list li {
  font-size: 0.92rem;
  display: flex;
  justify-content: space-between;
  gap: 8px;
  padding: 2px 0;
  border-bottom: 1px dotted transparent;
}
.book-list li:hover { border-bottom-color: var(--muted); }
.book-list a {
  color: var(--fg);
  text-decoration: none;
  flex: 1;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
}
.book-list a:hover { color: var(--accent); }
.muted { color: var(--muted); font-size: 0.8rem; font-variant-numeric: tabular-nums; }
.seed-badge {
  display: inline-block;
  margin-left: 8px;
  padding: 1px 6px;
  border-radius: 3px;
  font-size: 0.75rem;
  font-family: monospace;
  background: #ddd6fe;
  color: #5b21b6;
  font-weight: normal;
}
.pub-group.orphans {
  background: transparent;
  border: 1px dashed var(--muted);
}
</style>"##
}

fn include_scripts() -> &'static str {
    r##"<script>
(() => {
  const STORAGE_PREFIX = "aborg.audit.v1.";
  const sections = Array.from(document.querySelectorAll("section.book"));
  const filter = document.getElementById("filter");
  const exportBtn = document.getElementById("export");
  const progress = document.getElementById("progress");

  function key(slug) { return STORAGE_PREFIX + slug; }

  function load(slug) {
    try { return JSON.parse(localStorage.getItem(key(slug))) || {}; }
    catch { return {}; }
  }

  function save(slug, data) {
    localStorage.setItem(key(slug), JSON.stringify(data));
  }

  function hydrate(section) {
    const slug = section.dataset.slug;
    const data = load(slug);
    if (data.rating) {
      const radio = section.querySelector(`input[value="${data.rating}"]`);
      if (radio) { radio.checked = true; }
      section.dataset.rating = data.rating;
    }
    if (data.comment) {
      section.querySelector("textarea").value = data.comment;
    }
    if (data.rating || data.comment) {
      section.querySelector(".save-state").dataset.state = "saved";
      section.querySelector(".save-state").textContent = "saved";
    }
  }

  function persist(section) {
    const slug = section.dataset.slug;
    const rating = section.querySelector('input[name^="rating-"]:checked')?.value || "";
    const comment = section.querySelector("textarea").value;
    save(slug, { rating, comment, updated_at: new Date().toISOString() });
    section.dataset.rating = rating;
    const state = section.querySelector(".save-state");
    state.dataset.state = "saved";
    state.textContent = "saved";
    updateProgress();
    applyFilter();
  }

  function updateProgress() {
    const reviewed = sections.filter(s => s.dataset.rating).length;
    progress.textContent = `${reviewed} / ${sections.length} on this page reviewed`;
  }

  function applyFilter() {
    const v = filter.value;
    sections.forEach(s => {
      const r = s.dataset.rating;
      const show =
        v === "all" ||
        (v === "unreviewed" && !r) ||
        (v === "good" && r === "good") ||
        (v === "improve" && r === "improve") ||
        (v === "bad" && r === "bad");
      s.classList.toggle("hidden", !show);
    });
  }

  // Walk localStorage for every audit annotation, not just this
  // page's slugs — exporting from any page produces a complete
  // report. Older "data not on this page" rows still come along.
  function exportReport() {
    const books = [];
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (!k || !k.startsWith(STORAGE_PREFIX)) continue;
      const slug = k.slice(STORAGE_PREFIX.length);
      let d = {};
      try { d = JSON.parse(localStorage.getItem(k)) || {}; } catch { d = {}; }
      if (!d.rating && !d.comment) continue;
      books.push({
        slug,
        rating: d.rating || null,
        comment: d.comment || null,
        updated_at: d.updated_at || null,
      });
    }
    books.sort((a, b) => a.slug.localeCompare(b.slug));
    const out = {
      schema: "audiologo-audit-annotations-v2",
      exported_at: new Date().toISOString(),
      total: books.length,
      books,
    };
    const blob = new Blob([JSON.stringify(out, null, 2)], { type: "application/json" });
    const url = URL.createObjectURL(blob);
    const a = document.createElement("a");
    a.href = url;
    a.download = `audit-annotations-${Date.now()}.json`;
    document.body.appendChild(a);
    a.click();
    a.remove();
    URL.revokeObjectURL(url);
  }

  // Pause-other-clips on play. Operator complaint: a click can
  // overlap two clips if the previous one was still running.
  // Capture-phase listener catches every audio element on the
  // page (including future ones, though we have none).
  document.addEventListener("play", (e) => {
    if (e.target.tagName === "AUDIO") {
      document.querySelectorAll("audio").forEach((a) => {
        if (a !== e.target && !a.paused) { a.pause(); }
      });
    }
  }, true);

  sections.forEach(s => {
    hydrate(s);
    s.querySelectorAll('input[name^="rating-"]').forEach(r => {
      r.addEventListener("change", () => persist(s));
    });
    let ti = null;
    s.querySelector("textarea").addEventListener("input", () => {
      clearTimeout(ti);
      const state = s.querySelector(".save-state");
      state.dataset.state = "unsaved";
      state.textContent = "unsaved…";
      ti = setTimeout(() => persist(s), 500);
    });
  });

  filter.addEventListener("change", applyFilter);
  exportBtn.addEventListener("click", exportReport);
  updateProgress();
  applyFilter();
})();
</script>"##
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn stub_entry(slug: &str, title: &str, publisher: Option<&str>) -> AuditEntry {
        AuditEntry {
            slug: slug.into(),
            title: title.into(),
            source_path: PathBuf::from(format!("/library/{title}.m4b")),
            duration_ms: 60_000,
            publisher: publisher.map(str::to_owned),
            copyright: None,
            detection: DetectionInfo::Stub,
            front_clip_rel: format!("clips/{slug}-front.m4a"),
            end_clip_rel: format!("clips/{slug}-end.m4a"),
            front_waveform_svg: "<svg/>".into(),
            end_waveform_svg: "<svg/>".into(),
            front_detail: None,
            end_detail: None,
        }
    }

    fn entry_with_front_detail(slug: &str, publisher: &str) -> AuditEntry {
        let mut e = stub_entry(slug, slug, Some(publisher));
        e.front_detail = Some(DetailClip {
            clip_rel: format!("clips/{slug}-front-detail.m4a"),
            waveform_svg: "<svg id=\"detail-wave\"/>".into(),
            start_offset_in_overview_ms: 8_000,
            duration_secs: 15,
        });
        e
    }

    #[test]
    fn publisher_sort_key_buckets_none_last() {
        let a = publisher_sort_key(Some("Audible Studios"));
        let z = publisher_sort_key(None);
        let z2 = publisher_sort_key(Some("   "));
        assert!(a < z);
        assert_eq!(z, z2, "blank string treated as no-publisher");
    }

    #[test]
    fn page_filename_zero_padded() {
        assert_eq!(page_filename(1), "page-01.html");
        assert_eq!(page_filename(50), "page-50.html");
        assert_eq!(page_filename(100), "page-100.html");
    }

    #[test]
    fn write_report_paginates_at_50() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let entries: Vec<AuditEntry> = (0..120)
            .map(|i| {
                let pub_name = match i % 3 {
                    0 => Some("Audible Studios"),
                    1 => Some("Brilliance Audio"),
                    _ => None,
                };
                stub_entry(&format!("book-{i:03}"), &format!("Book {i:03}"), pub_name)
            })
            .collect();

        write_report(tmp.path(), Path::new("/corpus"), &entries, &SeedDb::empty())
            .expect("write report");

        assert!(tmp.path().join("index.html").exists(), "index.html written");
        assert!(tmp.path().join("page-01.html").exists());
        assert!(tmp.path().join("page-02.html").exists());
        assert!(tmp.path().join("page-03.html").exists());
        assert!(
            !tmp.path().join("page-04.html").exists(),
            "120 / 50 = 3 pages"
        );
        assert!(tmp.path().join("data.json").exists());

        let toc = fs::read_to_string(tmp.path().join("index.html")).unwrap();
        assert!(toc.contains("Audible Studios"));
        assert!(toc.contains("Brilliance Audio"));
        assert!(toc.contains("(no publisher)"));
        assert!(toc.contains("page-01.html#"));

        let page1 = fs::read_to_string(tmp.path().join("page-01.html")).unwrap();
        assert!(page1.contains("page 1/3"));
        assert!(page1.contains("section class=\"book\""));
        assert!(
            page1.contains("addEventListener(\"play\""),
            "pause-others JS present"
        );

        let page3 = fs::read_to_string(tmp.path().join("page-03.html")).unwrap();
        assert!(page3.contains("page 3/3"));
    }

    #[test]
    fn write_report_empty_corpus_still_writes_one_page() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        write_report(tmp.path(), Path::new("/corpus"), &[], &SeedDb::empty()).expect("write");
        assert!(tmp.path().join("index.html").exists());
        assert!(tmp.path().join("page-01.html").exists());
    }

    #[test]
    fn write_report_under_50_books_single_page() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let entries: Vec<AuditEntry> = (0..10)
            .map(|i| stub_entry(&format!("b{i}"), &format!("B {i}"), Some("Pub")))
            .collect();
        write_report(tmp.path(), Path::new("/c"), &entries, &SeedDb::empty()).expect("write");
        assert!(tmp.path().join("page-01.html").exists());
        assert!(!tmp.path().join("page-02.html").exists());
    }

    fn write_seed_fixture(dir: &Path, name: &str, body: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn seed_badge_renders_when_publisher_has_seeds() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let seed_path = write_seed_fixture(
            tmp.path(),
            "seed.json",
            r#"[
              {"publisher":"Audible Studios","intro_fingerprint_b64":"FP-A"},
              {"publisher":"Audible Studios","outro_fingerprint_b64":"FP-A-OUT"},
              {"publisher":"Brilliance Audio","intro_fingerprint_b64":"FP-B"}
            ]"#,
        );
        let seeds = SeedDb::load(&[seed_path]).expect("seeds");
        let entries = vec![
            stub_entry("a", "Audible Book", Some("Audible Studios")),
            stub_entry("b", "Brilliance Book", Some("Brilliance Audio")),
            stub_entry("c", "Unknown", Some("Tiny Press")),
        ];

        let out = tmp.path().join("report");
        write_report(&out, Path::new("/corpus"), &entries, &seeds).expect("write");

        let toc = fs::read_to_string(out.join("index.html")).unwrap();
        // Two seeds for Audible Studios -> "2 seeds"
        assert!(toc.contains("2 seeds"), "audible seed badge: {toc:#?}");
        // One seed for Brilliance Audio -> "1 seed" (singular)
        assert!(toc.contains("1 seed"), "brilliance singular: {toc:#?}");
        // Tiny Press has no seeds → no badge
        assert!(
            !toc.contains("Tiny Press</h2>")
                || !toc.contains("Tiny Press <span class=\"seed-badge\"")
        );

        let page1 = fs::read_to_string(out.join("page-01.html")).unwrap();
        // Per-book chip on the publisher cell
        assert!(page1.contains("2 known seeds"), "per-book chip: {page1:#?}");
    }

    #[test]
    fn detail_clip_renders_alongside_overview() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let entries = vec![entry_with_front_detail("a", "Audible")];
        let out = tmp.path().join("report");
        write_report(&out, Path::new("/c"), &entries, &SeedDb::empty()).expect("write");
        let page1 = fs::read_to_string(out.join("page-01.html")).unwrap();
        assert!(
            page1.contains("class=\"detail-clip\""),
            "detail block: {page1:#?}"
        );
        assert!(page1.contains("a-front-detail.m4a"));
        assert!(page1.contains("Detail · 15s"));
        assert!(page1.contains("<svg id=\"detail-wave\"/>"));
    }

    #[test]
    fn detail_clip_omitted_when_absent() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let entries = vec![stub_entry("a", "Plain", Some("Pub"))];
        let out = tmp.path().join("report");
        write_report(&out, Path::new("/c"), &entries, &SeedDb::empty()).expect("write");
        let page1 = fs::read_to_string(out.join("page-01.html")).unwrap();
        assert!(!page1.contains("class=\"detail-clip\""));
        assert!(!page1.contains("-front-detail.m4a"));
    }

    #[test]
    fn orphan_seeds_section_lists_publishers_without_books() {
        let tmp = tempfile::TempDir::new().expect("tmp");
        let seed_path = write_seed_fixture(
            tmp.path(),
            "seed.json",
            r#"[
              {"publisher":"OnlyInSeed","intro_fingerprint_b64":"FP-X"},
              {"publisher":"Audible Studios","intro_fingerprint_b64":"FP-A"}
            ]"#,
        );
        let seeds = SeedDb::load(&[seed_path]).expect("seeds");
        let entries = vec![stub_entry("a", "Book", Some("Audible Studios"))];

        let out = tmp.path().join("report");
        write_report(&out, Path::new("/corpus"), &entries, &seeds).expect("write");
        let toc = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(toc.contains("Seeds without books in corpus"));
        assert!(toc.contains("OnlyInSeed"));
        // Audible Studios is in the corpus -> NOT in orphan list
        // (single occurrence: the main group section).
        let count = toc.matches("Audible Studios").count();
        assert_eq!(
            count, 1,
            "audible should appear once (corpus group): {toc:#?}"
        );
    }
}
