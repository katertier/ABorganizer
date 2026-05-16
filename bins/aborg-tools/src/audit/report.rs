//! HTML report emitter for the audiologo-audit binary
//! (ADR-0054).
//!
//! Single-file, vanilla HTML + minimal CSS + inline JS. No SPA
//! framework, no bundler — opens directly in Safari / Chrome
//! from the `--out` directory.

#![allow(
    clippy::needless_raw_string_hashes,
    clippy::str_to_string,
    clippy::too_many_lines,
    clippy::missing_const_for_fn
)]

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};

use super::{AuditEntry, DetectionInfo};

/// Write `index.html` + `data.json` to `out_dir`.
///
/// `index.html` contains every book's audio + waveform + rating
/// UI inline. `data.json` is a backup of the per-book detection
/// metadata in machine-readable form (the HTML's JS doesn't
/// need to fetch it; the operator can use it for diffs).
///
/// # Errors
///
/// I/O errors creating the output files surface as anyhow.
pub fn write_report(out_dir: &Path, corpus_path: &Path, entries: &[AuditEntry]) -> Result<()> {
    fs::create_dir_all(out_dir).with_context(|| format!("create {}", out_dir.display()))?;

    let html = render_index(corpus_path, entries);
    let html_path = out_dir.join("index.html");
    fs::write(&html_path, html).with_context(|| format!("write {}", html_path.display()))?;

    let data = serde_json::json!({
        "schema": "audiologo-audit-v1",
        "corpus_path": corpus_path.display().to_string(),
        "books": entries.iter().map(book_to_json).collect::<Vec<_>>(),
    });
    let data_path = out_dir.join("data.json");
    fs::write(&data_path, serde_json::to_string_pretty(&data)?)
        .with_context(|| format!("write {}", data_path.display()))?;

    Ok(())
}

fn book_to_json(e: &AuditEntry) -> serde_json::Value {
    let detection = match &e.detection {
        DetectionInfo::Stub => serde_json::json!({"state": "stub"}),
        DetectionInfo::NoCandidate => serde_json::json!({"state": "no_candidate"}),
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
        "copyright": e.copyright,
        "detection": detection,
        "front_clip": e.front_clip_rel,
        "end_clip": e.end_clip_rel,
    })
}

fn render_index(corpus_path: &Path, entries: &[AuditEntry]) -> String {
    let mut sections = String::new();
    for e in entries {
        sections.push_str(&render_book_section(e));
    }

    let head = include_head();
    let scripts = include_scripts();
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
{head}
<title>Audiologo audit — {corpus}</title>
</head>
<body>
<header>
  <h1>Audiologo audit</h1>
  <div class="meta">
    <span>Corpus: <code>{corpus}</code></span>
    <span id="progress" class="progress"></span>
  </div>
  <div class="toolbar">
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

fn render_book_section(e: &AuditEntry) -> String {
    let (method_html, trigger_html, front_cut_disp, end_cut_disp) = match &e.detection {
        DetectionInfo::Stub => (
            r#"<span class="badge stub">Phase 1 — detection wiring pending</span>"#.to_string(),
            "<em>Operator rates the clips themselves. Real detection results land in Phase 2 wiring.</em>".to_string(),
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
        r##"<section class="book" data-slug="{slug}" data-rating="">
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
      <div>{publisher}</div>
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
    </div>
    <div class="cut-half">
      <h3>End · cut @ {end_cut_disp}</h3>
      <div class="wave">{end_waveform}</div>
      <audio controls preload="none" src="{end_clip}"></audio>
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
    )
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
}
.toolbar {
  margin-top: 12px;
  display: flex;
  gap: 16px;
  align-items: center;
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
    progress.textContent = `${reviewed} / ${sections.length} reviewed`;
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

  function exportReport() {
    const books = sections.map(s => {
      const slug = s.dataset.slug;
      const d = load(slug);
      return {
        slug,
        title: s.querySelector("h2").textContent,
        rating: d.rating || null,
        comment: d.comment || null,
        updated_at: d.updated_at || null,
      };
    });
    const out = {
      schema: "audiologo-audit-annotations-v1",
      exported_at: new Date().toISOString(),
      total: sections.length,
      reviewed: books.filter(b => b.rating).length,
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
