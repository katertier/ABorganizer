# Deep-Dive Review — PR #43 (`feat/transcode-m4b`)

Scope: the 5 commits on `feat/transcode-m4b` against `main` —
`aafabe2` (4C transcript-aided detection), `6bdde09` (4D review
workflow), `5791592` (sqlx-prepare tooling fix), `a252c79`
(ADR-0027 transcode skeleton), `3f0aaba` (ADR-0028 tag-write
skeleton).

Plans consulted: `ABorganizer-docs/DECISIONS/0027-transcode-and-file-refcount.md`,
`0028-two-pass-tag-write.md`, `0024-audiologo-detection-pipeline.md`,
`.claude/CLAUDE.md` (project rules + drift discipline).

---

## 1. Divergences from the plan

### 1.1 `PostTranscodeSourcesTarget::category()` returns `Category::Disk`; ADR-0027 says `Category: Audio`

`crates/transcode/src/cleanup.rs:121` returns `Category::Disk`.
ADR-0027 § "Source-file removal" line 107 specifies "Category:
Audio". The `Category` enum (`crates/core/src/cleanup.rs:22`)
has no `Audio` variant — only `Disk`, `Db`, `Queue` — so the ADR
as written is **unrealizable** today.

`Disk` is defensible (the target frees disk space), but the
mismatch is silent. Either the ADR needs updating to `Disk` or
the enum needs an `Audio` variant. **Pick one and reconcile.**

### 1.2 `book_file_refs::acquire` signature ignores the typed primitives the ADR specifies

ADR-0027 § "Lifecycle helpers" prescribes:

```rust
pub async fn acquire_file_ref(
    tx: &mut Transaction<'_, Sqlite>,
    file_id: FileId,
    holder_stage: StageId,
    holder_book_id: BookId,
) -> Result<RefHandle>;
```

Code (`crates/db/src/book_file_refs.rs:74`):

```rust
pub async fn acquire(
    pool: &SqlitePool,
    file_id: i64,
    holder_stage: &str,
    holder_book_id: i64,
) -> Result<RefHandle>;
```

Four divergences:

- **Transaction → Pool.** The ADR's transaction lets callers
  compose `acquire` with other writes atomically (e.g. "acquire
  ref + insert provenance row" as one commit). Code's pool-direct
  signature forecloses that. Real lift gap.
- **`FileId` → `i64`.** `FileId` exists at
  `crates/core/src/ids.rs:21` and is re-exported from
  `ab_core::ids::FileId`. The code skipped it.
- **`StageId` → `&str`.** `StageId` is the typed primitive
  shipped in slice C2; using `&str` here regresses the typing
  story for the first new consumer.
- **`BookId` → `i64`.** Same gap.

The Rust typing direction the workspace has been moving toward
(C1-C3) explicitly says: typed primitives at every API surface.
This slice slipped backwards.

### 1.3 `RefHandle` is not opaque

ADR-0027 line 83: "`RefHandle` is opaque (`ref_id`)."

Code (`crates/db/src/book_file_refs.rs:35`):

```rust
pub struct RefHandle {
    pub ref_id: i64,
}
```

`pub ref_id` means any caller can synthesise
`RefHandle { ref_id: 999 }` and call `release` on it — bypassing
`acquire` entirely. Either make it `pub(crate) ref_id` with a
getter, or wrap in a newtype with no public constructor.

### 1.4 Tag-write `Stage::requires()` is partial; ADR-0028 says it's complete

ADR-0028 § "`TagWriteEarly`" specifies:

```text
requires(): tag-read, identity-resolve, extract-dna-tags
```

Code (`crates/tag-write/src/stage.rs:46`):

```rust
const TAG_WRITE_EARLY_REQUIRES: &[StageId] = &[StageId::new("tag-read")];
```

Same for `TagWriteFinal`. The slice documents the gap ("full set
lands as each upstream `StageId` becomes referenceable") and
mitigates by not registering the stages in the daemon — but a
future operator reading just the code sees a fictitious
2-of-many requires list. **Better:** synthesise the missing
`StageId` constants directly in `tag-write` as stop-gaps
(`StageId::new("identity-resolve")`, etc.) so the requires set
is operationally correct from day one. The fact that those
upstream crates haven't typed their own constants yet doesn't
prevent this stage from being correct.

### 1.5 COURSE-CORRECTION cycle entry deferred against the rule

`.claude/CLAUDE.md` § "Every 5 commits":

> "The audit's structural conclusions land in COURSE-CORRECTION.md
> as the next `## Cycle N` entry once the cycle's 5 commits are
> complete."

This cycle is 5 commits. The last commit body says the entry
lands "once the lofty bodies for both transcode-m4b and tag-write
ship" — which is later, not now. The rule is clear: write it now.

---

## 2. Questionable decisions

### 2.1 The whole cycle ships as `Skipped` scaffolds

Both ADR-0027 and ADR-0028 are about live, observable behaviour
(parallel transcode + source reaping; two-pass tag writes with
sticky user edits). What landed: zero new observable behaviour.
Migration 018 created a table whose only readers/writers are
themselves test code; both new stages return `Skipped` from
their first line; the cleanup target ships with both predicates
that make it a permanent no-op.

This is the established cadence for the workspace and it has
defensible properties (typed `StageId` available for downstream
`requires()` lists, schema deploys ahead of code, helpers tested
in isolation). But it means **PR #43 ships nothing the user
can see.** A more aggressive slice would have included at least
the `book_file_refs` integration into one existing stage
(`fingerprint`, `tag-read`, or `detect-audiologo`) so the
refcount path runs in production from this PR forward.

### 2.2 The cleanup target silently ignores `Policy::force` and `Policy::age_seconds`

`crates/transcode/src/cleanup.rs:144` accepts `_policy: &Policy`
and disregards both fields. The trait contract doesn't require
honouring them, but `aborg clean disk --force --age 7d` will
silently no-op against this target. There's a module-doc note,
but operator visibility is zero.

**Fix:** either honour the fields (force = skip refcount gate
— but the ADR explicitly forbids that) or emit a tracing line
("`post-transcode-sources` target ignores `--force`/`--age`") so
the operator knows. A `notes: Option<String>` field on
`CleanupReport` would be the structured answer.

### 2.3 `select_winners_for_book` logs unknown field strings at `debug`

`crates/tag-write/src/winners.rs:73`:

```rust
tracing::debug!(book_id, field = %r.field, "tag-write.winners.unknown_field");
```

The `book_field_provenance.field` column is a closed `CHECK` set
(migration 011). An unknown string indicates **schema drift** —
exactly the bug class that should be `warn` or `error`, not
`debug`. `debug` is filtered out by default; this signal would
never reach the operator.

### 2.4 `select_winners_for_book` doesn't dedup multiple `is_winner=1` rows per `(book_id, field)`

Schema doesn't enforce one winner per field (migration 011's
table has no partial UNIQUE on `(book_id, field) WHERE is_winner
= 1`). The consensus stage is the only writer, but a race
condition (two consensus runs against the same book) could mark
both rows winner. The `winners` SELECT silently returns both;
the future TagWrite stages would write inconsistent values.

This is a real schema gap and the tag-write slice is the right
place to catch it — either at the schema (partial UNIQUE) or at
the query (`GROUP BY field` + last-wins / highest-confidence
tiebreak).

### 2.5 Hardcoded `/tmp` paths in transcode cleanup tests

`crates/transcode/src/cleanup.rs:`:

- `apply_is_idempotent` uses `let m4b_str = "/tmp/never-touched.m4b";`
- `apply_proceeds_when_file_already_gone` uses
  `"/tmp/does-not-exist.mp3"` and `"/tmp/m4b.m4b"`.

The other tests in the same file correctly use `TempDir`. The
hardcoded paths don't matter for DB-row assertions but they
clash with the rest of the file's hygiene and risk surprise
interactions with operator state in `/tmp`. **Make every test
path `TempDir`-rooted.**

### 2.6 `RefHandle::release` is idempotent but `Drop` is not implemented

The doc-comment says: "Holding a `RefHandle` is the operational
contract; dropping it without calling release does NOT release
the row (Drop can't be async)."

This punts the leak-on-panic problem entirely. ADR-0027 § "Ref
leak" acknowledges the same gap and points at a future `aborg
doctor`. Defensible, but the pattern is fragile — a typed RAII
guard via `scopeguard` (sync, per-callsite) would catch the
common case at compile time. The current shape relies on
discipline.

### 2.7 The `USER_EDIT_SOURCE` constant is one of three string-typed conventions in flight

`crate::USER_EDIT_SOURCE = "user_edit"` joins:

- `audiologo` cut method strings (`"silence"`, `"fingerprint_full"`)
- `book_field_provenance.source` values
  (`"audnexus-enrich"`, `"audible-search"`, `"tag_file"`, `"api-user-edit"`)
- Stage names (mostly now typed via `StageId` post-C2)

ADR-0028 explicitly rejects a typed `Source` enum at this slice,
deferring to BACKLOG.md. Reasonable, but the divergence between
"we typed `StageId` in C2" and "we still ship a free-form
provenance source string" is uncomfortable. **At minimum, an
`xtask brand`-style grep check should pin every writer to the
constant.**

---

## 3. Security findings

### 3.1 Path-traversal surface on `tokio::fs::remove_file(book_files.file_path)`

`crates/transcode/src/cleanup.rs:153`:

```rust
match tokio::fs::remove_file(path).await { ... }
```

`book_files.file_path` is populated by `crates/scan/` from
operator-controlled paths under the configured library root —
trusted. But:

- Any code path with write access to `library.db` (test fixtures,
  future API endpoints, manual `sqlite3 library.db`) could insert
  an absolute path like `/etc/passwd` and `is_active=1`.
- An m4b twin (`format='m4b'`) for the same `book_id` is the
  only other gate — easy to forge.
- The cleanup target then `unlink`s arbitrary files.

**Threat model:** assumes attacker already has DB write
privilege, at which point they have many other paths. But the
principle of least privilege says: **canonicalise + verify the
path is under the configured library root before unlinking.**
A 5-line guard in `apply()` closes this entirely. The current
code has zero defence.

Severity: low; defence-in-depth value: high.

### 3.2 `serde_json::from_slice` on `ai_cache.content` has no size cap

`crates/audiologo/src/transcript_aided.rs:394` does
`serde_json::from_slice(&bytes)` on whatever the transcribe
stage wrote. The transcribe stage is workspace-internal, so the
input is trusted — but pathological-size JSON would still OOM
the decoder before any check fires.

A `bytes.len() > MAX_CACHE_BYTES` guard before deserialisation
(or a `serde_json::Deserializer::from_slice(&bytes).disable_recursion_limit()`
+ explicit limit) costs nothing and bounds the failure mode.

### 3.3 No surface — but worth flagging: review API is unauthenticated

`crates/api/src/audiologo_review.rs` exposes
`GET /api/v1/audiologos/review` and the approve/reject POSTs
with no auth. Inspecting the handler signatures shows just
`State<ApiState>` — no `AuthToken` extractor.

The wider workspace's auth story is in pairing-codes / tokens.
This slice's handlers don't plug into that. **Verify the router
wiring in `bins/aborg-daemon` adds the auth middleware** — if
the router uses a global `.route_layer(require_auth())`, no
problem; if not, the review API is wide open. Not part of this
review's scope to chase further; **flag and confirm.**

---

## 4. DB schema improvements (slice scope only)

### 4.1 Drop the redundant `UNIQUE` on `book_file_refs`

Migration 018 line 30:

```sql
UNIQUE (file_id, holder_stage, holder_book_id, acquired_at)
```

`acquired_at` defaults to `strftime('%s','now')` — 1-second
resolution. Two `acquire` calls from the same stage on the same
file for the same book within one second hit
`SQLITE_CONSTRAINT_UNIQUE` and fail. `ref_id INTEGER PRIMARY KEY
AUTOINCREMENT` already guarantees row-uniqueness; the additional
UNIQUE adds zero value and one failure mode.

**Fix:** drop the UNIQUE clause. The PK suffices.

### 4.2 `book_file_refs.holder_book_id` has no FK

The column is `INTEGER NOT NULL` with no `REFERENCES books`.
Deleting a `books` row leaves `book_file_refs` rows orphaned,
pointing at a missing book.

**Fix:** add `REFERENCES books(book_id) ON DELETE CASCADE` like
`file_id` has.

### 4.3 Partial UNIQUE on `book_field_provenance` winners

```sql
CREATE UNIQUE INDEX ux_book_field_winner
    ON book_field_provenance(book_id, field)
    WHERE is_winner = 1;
```

Converts the "two consensus runs accidentally both winner-flag
the same field" race from "silent inconsistency" to "explicit
INSERT-time conflict that consensus must handle." Tag-write is
the first consumer that would silently misbehave under that race.

### 4.4 `idx_book_file_refs_acquired_at` is YAGNI

Migration 018 line 36-37:

```sql
CREATE INDEX idx_book_file_refs_acquired_at
    ON book_file_refs(acquired_at) WHERE released_at IS NULL;
```

The migration comment says it's preemptive for a future `aborg
doctor` (Theme 6). Until that consumer ships, the index is dead
weight on every INSERT and one more thing the planner has to
consider. **Drop it; add it back in the slice that needs it.**

### 4.5 `book_field_provenance.field` CHECK is duplicated against `ab_core::Field`

The CHECK constraint in migration 011 enumerates 16 strings;
`ab_core::Field` in code enumerates the same 16. Drift happens
when one is edited and the other isn't (it has — slice C3 fixed
the column-rename). A code-gen step (`xtask` task that reads
the migration's CHECK list and verifies the enum matches) would
close the drift permanently. Not a slice ask — just flagging
the standing risk.

---

## 5. Code improvements (in-slice scope)

### 5.1 `cleanup.rs::apply` FS delete + DB update are non-atomic

The current order (unlink → UPDATE) means a process crash
between the two leaves a missing file with `is_active=1`. The
next pass re-discovers eligibility, tries to unlink (NotFound →
proceed), then UPDATEs. Self-healing, but the window is real
and worth a one-line comment in the body acknowledging it.

### 5.2 `m4b` literal lives in two places

`migrations/library/001_initial.sql:122` (the format-column
comment) and `crates/transcode/src/cleanup.rs:91` (the SQL
literal `'m4b'`). One const in `ab_core::audio::FORMAT_M4B` (or
similar) would prevent drift. Same logic applies to
`format != 'm4b'`.

### 5.3 `transcript_aided.rs::scan_for_phrase` carries an unused `book_id`

Line 276: `let _ = book_id; // Surface in tracing once wired
into stage.` Remove the arg until the tracing wires up.
Carrying it surfaces a fictitious dependency in the call graph.

### 5.4 `RefHandle::release` could take `&mut self` and zero the `ref_id`

Currently `&self` + idempotent SQL UPDATE. Taking `&mut self`
and zeroing `ref_id` after the first release would prevent
double-release at the Rust level too — a cheap belt+braces
guard over the SQL-level idempotence.

### 5.5 Test seed helpers should use struct params

Two `#[allow(clippy::too_many_arguments)]` annotations:
`crates/transcode/src/cleanup.rs::seed_file` (6 args) and
`crates/tag-write/src/winners.rs::seed_winner` (6 args). Both
are test-only fixtures mirroring DB schemas. The clippy lint is
suppressed correctly, but a `seed_file(library, BookFileSeed
{ file_id, book_id, ... })` builder is more idiomatic and
exactly what clippy is steering toward.

### 5.6 Idempotent post-conditions deserve assertion in apply

`PostTranscodeSourcesTarget::apply` is documented as idempotent
and tested via `apply_is_idempotent`, but the actual code path
is "select → apply → select-again-and-confirm-empty would have
been nicer." A `debug_assert!(post_select.is_empty())` after the
loop would make the invariant runtime-checkable in debug builds.

---

## 6. What I'd have done differently from the start

### R1. Split the transcode work into three micro-slices

- **T1:** Migration 018 + `book_file_refs` helpers (with typed
  `FileId`/`StageId`/`BookId` + `Transaction` ctor) + tests.
- **T2:** `ab-transcode` crate stage skeleton.
- **T3:** `PostTranscodeSourcesTarget` cleanup target.

Each independently reviewable. PR per slice. The current single
commit conflates three concerns.

### R2. Reconcile `Category::Audio` vs `Category::Disk` against the ADR before writing code

A 60-second check (read the enum) would have caught the
mismatch up front. Either patch the ADR to `Disk` or add the
`Audio` variant in a tiny prelude commit. **Doing it after the
fact creates a stale ADR.**

### R3. Land the sqlx-prepare fix as its own PR ahead of transcode

The fix is independent and standalone. Mixing it into the
transcode PR muddies the diff and the merge-blame.

### R4. Tag-write split into three micro-slices

- **W1:** `USER_EDIT_SOURCE` + `skip_for_final_pass` + tests.
- **W2:** `winners::select_winners_for_book` + tests + the
  partial-UNIQUE schema fix.
- **W3:** Two Stage skeletons.

### R5. Write the COURSE-CORRECTION cycle entry in the closing commit

Per the rule. The current "defer to bodies" reading of the rule
is wrong on a literal read.

### R6. Wire the refcount into one existing stage in this PR

Pick the simplest existing stage that reads a file
(`fingerprint`, probably) and add `acquire`/`release` around its
audio-read block. Now the refcount path runs in production, the
cleanup target has real signal to consume when it activates, and
the slice ships observable behaviour.

---

## 7. Plan-adherence improvements

### P1. One PR per slice

Memory note: "smaller PR cadence — one PR per slice (or tightly-
coupled slice group); ~5-10 commits ceiling." PR #43 bundles
four logically independent slices (4C, 4D, transcode skeleton,
tag-write skeleton) plus a tooling fix. The bundle is at the
upper bound and the slices aren't tightly coupled. Future:
rebase + cherry-pick into separate branches.

### P2. ADR-to-code drift check as a step

Before writing any code for an ADR slice, dump the ADR's
prescribed types/functions/columns and grep the codebase for
each one (`grep -rn 'FileId\|Category::Audio'`). Catches §1.1
and §1.2 above before they hit a commit. Add as an item to the
slice checklist.

### P3. Mandatory observable-behaviour line in slice plans

A slice plan that lists "what the user sees post-merge" forces
honesty about scaffolding-vs-feature. Both transcode-m4b and
tag-write skeletons would have flagged themselves as
"observable-behaviour: none" and prompted either consolidation
(ship both bodies in one slice) or carve-off (refcount-into-
fingerprint per R6 above).

### P4. Drift counter explicit at session start

The 5-commit drift audit fired with the closing commit, but
the counter logic wasn't tracked in session prose — I derived
it ad-hoc. A "drift-counter: N/5" line at session start +
after-each-commit would make the audit point hard to miss.

---

## 8. Structural changes

### S1. `book_file_refs` is misfiled in `ab-db`

The crate's stated purpose ("Persistence layer") is SQLite
primitives — schema, connections, migrations. `book_file_refs`
is a domain-lifecycle concern (refcount-as-a-coordination-
mechanism), not a persistence primitive. Better homes:

- `ab-pipeline::file_refs` (lifecycle is stage-driven), or
- A standalone `ab-file-refs` crate.

The current location reads correctly because of the
`book_file_refs.rs` filename next to other table-named files —
but the public API (`acquire`, `release`, `live_ref_count`) is
about coordination, not SQL.

### S2. `crates/transcode` should grow a `lifecycle` module

The crate has `stage` + `cleanup`. The actual transcode flow
will need: (a) the stage, (b) the file-ref acquire/release dance,
(c) the cleanup, (d) the m4b-row insert. The current two-module
shape implies a binary stage/cleanup split that's going to feel
crowded once (b) and (d) land. A `lifecycle` module that owns
(b) and (d) and is called by `stage::run` cleanly separates the
"what does Stage::run do" surface from "how does the data move"
plumbing.

### S3. Provenance source values deserve a CI check

`xtask` already enforces brand-name conventions and the no-`Manager`
identifier rule. A grep check that:

- Every literal string `"user_edit"` in `crates/**/*.rs` is
  either the `USER_EDIT_SOURCE` const definition or a use site
  that imports it.

…would catch the typo-failure-mode ADR-0028 calls out. Cheap.

### S4. `aborg-tools` (the third binary) doesn't appear in the workspace flow

`scripts/sqlx-prepare.sh` originally missed it; my fix
incidentally caught it via `--workspace --all-targets`. But it's
not in `default-members`, which is the root cause. Either:

- Add `bins/aborg-tools` to `default-members` so every `cargo`
  invocation builds it.
- Or document why it's intentionally out (one-shot operator
  utility, not a daemon).

Today it's neither in default-members nor documented as
intentionally separate. The sqlx-prepare incident is a symptom.

---

## TL;DR

**What's solid:**

- Migration 018's shape + the refcount-as-mechanism design.
- The partial index choice for `live_ref_count` queries.
- The `USER_EDIT_SOURCE` + `skip_for_final_pass` split (clean
  contract, one place to typo-check).
- Test coverage is thorough for the surface that's there
  (10 + 8 + 4 new tests, all green).
- `transcript_aided.rs`'s segment-boundary localisation and the
  multi-language `phrases.rs` vocabulary are well-built.

**What's worrying:**

- The `Category::Audio` ADR-vs-code mismatch (§1.1) — stale ADR.
- The typed-primitive regression in `book_file_refs::acquire`
  (§1.2) — backslides from C2's direction.
- The hardcoded `/tmp` test paths (§2.5) — sloppy hygiene.
- The `winners` query returning duplicates on a winner race
  (§2.4) — real correctness gap with no schema-level guard.

**What's a real bug:**

- The `UNIQUE` constraint on `book_file_refs` (§4.1) — collides
  under sub-second concurrent acquires and adds no value.
- The path-traversal absence in cleanup-apply (§3.1) — low
  severity, very high defence-in-depth value.

**Headline ask:** reconcile §1.1 and §4.1 before merging #43.
The rest is polish + process.
