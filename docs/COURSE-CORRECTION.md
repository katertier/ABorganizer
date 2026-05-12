# Course-correction log

Working journal of structural decisions and the slices that
re-shaped them. Every fifth feature commit triggers a reflection
pass (per the project's working-style policy) — outcomes land here
so the next session can read the history without scrolling commit
logs.

The audience is "fresh me with no chat memory": each entry should
tell you _what_ changed structurally, _why_ at that moment, and
_what to remember_ when adding new code in the same region.

---

## Cycle 1 reflection — slices A1 → B3 (2026-05-12)

Reflection after the first five course-correction commits. Surfaced
during the 3K LLM-extractor work when patterns from 3K.3 looked
like they'd compound badly across 3K.4-6.

### What landed

| Slice | Commit | Surface |
|---|---|---|
| A1 | `462f0cd` | Split Apple Speech FFI bridge out of `ab-transcript` into a new `ab-speech` crate. |
| A2 | `8a3ef0d` | Split DNA-tag stage (and the rest of the LLM extractor stages) out of `ab-foundation-models` into a new `ab-llm-extractors` crate. |
| B1 | `391a9be` | `ab_core::CacheKey` enum replaces inline `"transcript_head"` cache-type strings. `ab_core::TagKind` + `TAG_PREFIX_*` consts replace inline `format!("#{...}")` / `format!("!{...}")` prefix construction in tag writers. |
| B2 | `efce6c3` | Migration 004: `ai_cache.locale TEXT` column; `ai_cache.model_version` renamed to `ai_cache.extractor_version`. Locale no longer embedded inside the JSON payload — read from the column. |
| B3 | `5c17bfa` | Drift-detection integration test in `crates/catalog/tests/promote_drift.rs` asserting `books.X == winner_value(book_field_provenance.field=X)` for every promotable scalar field. Two test cases: full mixed-confidence run + zero-provenance no-op. |

### Why these, in this order

The first two slices fixed crate-graph cleanliness: a CLI tool or
test harness that only needs `ab_speech::transcribe_window` or
`ab_foundation_models::complete` no longer pulls in `ab-db`,
`ab-pipeline`, `sqlx`, `async-trait`. The bridge crates are now
swappable backends conceptually — a hypothetical whisper-cpp /
llama-cpp replacement is a parallel impl crate, not a sweep of
the orchestration crate.

The next three closed three structural failure modes:

1. **Typo bites typed string usage** (B1) — eight cache producers
   spread across two crates wrote `"transcript_head"`-style
   literals to `ai_cache.cache_type`. A typo at any one of them
   meant the freshness check never hit and the stage re-ran every
   scheduler tick forever, invisibly. The `CacheKey` enum makes it
   a compile error.

2. **Locale-in-JSON couples freshness to BLOB decode** (B2) — the
   transcribe stages wrote `{locale, segments}` into the
   `ai_cache.content` BLOB and the freshness check decoded the
   JSON just to read `locale`. A column moves it into the
   structured surface and lets freshness be a single SELECT. The
   rename `model_version → extractor_version` came along because
   the same column now serves Speech (`speech-26.0-v1`) and LLM
   (`fm-26.0-v1`) versions — the abstract name fits both.

3. **Drift between `books.X` and the provenance winner is
   invisible** (B3) — the dual-storage pattern (provenance row
   _and_ promoted column) depends on the consensus stage keeping
   both in sync. Nothing structural enforced that invariant; a
   future stage that wrote `books.title` directly without
   updating provenance, or vice versa, would have produced an
   inconsistent DB with no test surfacing it. The integration test
   pins the invariant.

### What I'd have done differently if I had planned

Documented at reflection time, fixed in **Cycle 2** (C1-C4 below):

- **Tunable field name out of sync with column name.** After B2
  the DB column is `extractor_version` but
  `TranscribeTunables.model_version` / `LlmTunables.model_version`
  still carried the old Rust name. I left the rename out for "less
  churn." On reflection that was wrong — keeping two names for
  the same concept is the kind of confusion that costs real time
  for every future reader.
- **`Stage::requires() -> &'static [&'static str]` is
  refactor-unsafe.** Renaming a stage's `STAGE_NAME` const means
  every dependent's `&["other-name"]` silently goes stale; no
  build error, the stage just never runs. The fix is the
  `StageId` newtype.
- **`book_field_provenance.field` was free-form `&str`
  everywhere.** Parallel problem to `CacheKey`. The extractor
  writes "publisher", consensus reads "publisher" — a typo at
  either end means the candidate never gets promoted. Fix is a
  closed `Field` enum.
- **No paper trail for course-corrections.** Every reflection
  ended in commit-message-prose; finding "what did we decide
  about X last cycle" meant `git log`. This file is the fix.

### Naming + structural facts to remember

- Apple Speech / NaturalLanguage FFI lives in **`ab-speech`**
  (not the old `ab-transcript`).
- Apple Intelligence Foundation Models FFI lives in
  **`ab-foundation-models`**.
- Speech-stage orchestration (transcribe-head-tail, samples, full,
  extract, description_lang, idle-install-loop) lives in
  **`ab-transcript`** (depends on `ab-speech`).
- LLM extractor stages (DNA, summary, arc, characters) live in
  **`ab-llm-extractors`** (depends on `ab-foundation-models`).
- Swift sources: `swift/aborg_speech.swift`, `swift/aborg_fm.swift`.
- `ai_cache` schema is **`(book_id, cache_type, content,
  compressed, confidence, extractor_version, locale)`**. The
  locale lives in a column, not embedded in the JSON.
- `book_field_provenance.field` values come from the `ab_core::Field`
  enum. `ai_cache.cache_type` values come from the
  `ab_core::CacheKey` enum. Tag prefixes come from
  `ab_core::tags::{TAG_PREFIX_GENRE, _DNA, _SPOILER}` /
  `TagKind::format_tag(body)`. **No inline string literals at
  call sites for any of these.**
- Each pipeline stage exposes `pub const STAGE_ID:
  ab_pipeline::StageId`. `Stage::requires() -> &'static [StageId]`.
  Cross-stage dependencies are typed; renaming a stage propagates
  as a compile-time error at every dependent.

---

## Cycle 2 — slices C1 → C4 (2026-05-12)

Mechanical cleanup of the four items the cycle-1 reflection
surfaced. No new features.

| Slice | Outcome |
|---|---|
| C1 | `TranscribeTunables.model_version` and `LlmTunables.model_version` renamed to `extractor_version`. Removes the Rust-field / DB-column name mismatch from B2. |
| C2 | `ab_pipeline::StageId` newtype. `Stage::requires()` returns `&'static [StageId]`. Every stage exposes `pub const STAGE_ID: StageId`. Cross-crate deps added: `ab-catalog → ab-tag-read`; `ab-transcript → ab-tag-read + ab-catalog`; `ab-llm-extractors → ab-transcript`. |
| C3 | `ab_core::Field` enum. `Candidate.field`, `PromotableField.provenance_field` now typed. |
| C4 | This file. |

### Still string-keyed (deferred)

- `crates/api/src/router.rs` `stage_priorities` table — submits
  jobs by string name. Would need every stage crate as an `ab-api`
  dep to use `STAGE_ID`. Not worth the dependency-graph cost yet.
- The catalog enrichment writes (`enrich.rs`, `audible_search.rs`,
  `identity.rs`) still bind inline `"author"` / `"narrator"`
  literals into sqlx `field = ?` placeholders. The `Field` enum
  is available; the migration is straightforward. Drift-detection
  test (B3) catches the worst outcome already.

---

## Reflection cadence

Per project working-style: every 5 commits the next session does a
reflection pass and appends a new "Cycle N" section here. The
counter resets after each reflection.

Format for new entries:

```
## Cycle N reflection — slices X1 → X5 (YYYY-MM-DD)

### What landed
| Slice | Commit | Surface |
...

### Why these, in this order
...

### What I'd have done differently
...

### Naming + structural facts to remember
...
```

Items in "what I'd have done differently" become the next
cycle's slice candidates (if their compounding cost justifies
the rework — see the cycle-1 entry for the calculus).
