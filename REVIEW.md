# ABorganizer Code Review

> Review date: 2026-05-13  
> Scope: code vs. plan (`~/dev/ABorganizer-docs`) up to current implementation.  
> Rule: only judge what *is* implemented against what the plan says *should exist by now*. Stubs for future themes are noted only when their absence creates a structural problem today.

---

## 1. Deviations from Plan (implemented code that diverges)

### 1.1 Config file loading — documented but not wired

**Plan:** (`PROJECT.md`, `tunables.rs` doc comment)
> Resolution order: CLI flag → env var (`AB_*`) → config TOML → default.

**Code:** `bins/aborg-daemon/src/main.rs:227`
```rust
let tunables = Tunables::default();
```

The daemon accepts `--config <path>` in `Args`, but the value is never passed to `figment` (already in `Cargo.toml`). Every tunable is hardcoded. This means:
- `abs_enabled` is permanently `false` despite the plan describing ABS as a core bridge.
- `bind` is permanently `127.0.0.1` — operators who want LAN access must recompile.
- `idle_wait_secs`, `dispatcher_check_secs`, and concurrency limits cannot be tuned without a rebuild.

**This is a deviation, not a missing feature:** the plan explicitly says config resolution is part of the v0 scaffold.

### 1.2 `library_scan` accepts arbitrary paths — violates explicit-roots policy

**Plan:** (`PROJECT.md` § Library import)
> No auto-discovery of `~/Music`, iCloud Drive, etc. Explicit user-supplied directories only.

**Code:** `crates/api/src/router.rs:271-275`
```rust
async fn library_scan(State(state): State<ApiState>, Json(req): Json<ScanRequest>) -> … {
    let report = ab_scan::scan(&req.path, &state.inner.library).await?;
```

`ScanRequest.path` is a raw `PathBuf` with zero validation. The plan's threat model explicitly defends against "Path traversal via user-supplied paths" with "Canonicalize + check against allowed roots before any FS op." That check does not exist.

### 1.3 Retry endpoint ignores daemon cancellation

**Plan:** (`ARCHITECTURE.md` § Signals)
> `SIGTERM` → graceful shutdown (cancellation token cancelled, axum servers shut down with `with_graceful_shutdown`).

**Code:** `crates/api/src/router.rs:1069-1074`
```rust
let stage_ctx = ab_pipeline::StageContext {
    library: state.inner.library.clone(),
    ephemeral: state.inner.ephemeral.clone(),
    cancel: tokio_util::sync::CancellationToken::new(), // ← new token!
    stage_name: "",
};
```

The retry handler creates a **new** `CancellationToken` instead of cloning the daemon's. A `transcribe-full` retry triggered via API will not stop when the daemon receives SIGTERM. The plan says every long-running task participates in graceful shutdown; this one doesn't.

### 1.4 `ab-shelf` defaults to disabled — contradicts plan's ABS positioning

**Plan:** (`PROJECT.md` § Purpose)
> Expose to mobile clients via an Audiobookshelf-compatible API.

**Code:** `crates/core/src/tunables.rs:163`
```rust
pub abs_enabled: bool,
```
defaults to `false`. The plan treats ABS compat as a core deliverable (it's in the project purpose statement), not an optional extra. Defaulting to off means every operator must discover the toggle and rebuild/relaunch to use mobile clients.

### 1.5 `books_list` query shape diverges from documented API contract

**Plan:** (`API.md`)
> `GET /books` — List/search books. Query: `q`, `author`, `series`, `tag`, `genre`, `language`, `limit`, `offset`.

**Code:** `crates/api/src/router.rs:591-645`

The handler:
- Ignores **all** query parameters (`q`, `author`, `series`, `tag`, etc.).
- Returns **every book** in the library with no pagination.
- Uses five correlated subqueries per row (`file_path`, `author`, `narrators`, `series`) instead of JOINs.

The endpoint exists but does not implement the documented contract. For a 10k-book library this is a full-table scan + 50k subqueries.

---

## 2. Questionable Decisions

### 2.1 `names.rs` — dynamic SQL for table/column names

`IdentityKind` is a closed enum, so the `format!` interpolation is *controlled*, but it still bypasses `sqlx::query!()` compile-time checking for those query shapes. A typo in a `const fn` (e.g., `"author_alias"` instead of `"author_aliases"`) would be a runtime error, not a compile error.

**Better:** Keep the dispatch enum, but use a macro that generates separate `sqlx::query!()` calls per variant so the SQL is still checked at compile time.

### 2.2 `blake3` for token hashing — too fast for the threat model

**Plan:** (`SECURITY.md`)
> Tokens stored hashed in `library.db` `tokens` table (blake3 of raw token).

blake3 is optimized for speed (≈1 GB/s per core). An attacker with read access to `library.db` can brute-force a token at billions of guesses per second on a GPU. The plan's threat model says "Another local app on the user's machine talking to our API without permission" is a defended threat; fast hashing weakens that defense.

**Better:** Use `argon2id` with a per-token random salt, or at minimum HMAC-SHA256 with a pepper key.

### 2.3 `audiologo_status` as free-form `&str` in Rust

Migration 010 adds a `CHECK` constraint on `books.audiologo_status`, but the Rust code (`audiologo_apply.rs`, `router.rs`) reads and writes raw `&str`. There's no `AudiologoStatus` enum in `ab_core`. This is the exact anti-pattern the typed-primitives cycles (B1, C2, C3, C5) were designed to eliminate.

**Better:** Add `ab_core::AudiologoStatus` with `as_str()` / `from_str()`, mirroring `Field` and `CacheKey`.

### 2.4 `ab-audio` crate description claims it handles Speech + FM

**Code:** `crates/audio/Cargo.toml:9`
> "Pure-Rust audio decode (Symphonia + Lofty) plus Swift FFI bridge for AVFoundation transcode/encode and Apple Intelligence (SpeechAnalyzer, NLLanguageRecognizer, FoundationModels)."

The actual crate exports only `read_samples_window` and `probe_duration_ms`. The Speech and FoundationModels bridges live in separate crates (`ab-speech`, `ab-foundation-models`). This description is misleading — it implies `ab-audio` subsumes the other FFI crates, which it does not.

### 2.5 `book_file_refs` migration exists with zero consumers

Migration 018 creates a refcount table for transcode safety (ADR-0027). The `transcode` crate is a stub stage that returns `Skipped`. No code acquires or releases refs. A migration that nothing uses creates schema drift risk: if the design changes before transcode is implemented, the migration may need to be rewritten or supplemented.

**Better:** Ship the migration in the same PR that introduces the first `acquire_ref`/`release_ref` call site.

---

## 3. Security Issues

### 3.1 Path traversal in `library_scan`

**Severity: HIGH**

Any process with network access to the daemon can pass an arbitrary path to `POST /library/scan`. The daemon will recursively walk it, open audio files, and insert rows into `library.db`.

**Exploit:** Point scan at `/System/Library/Extensions` or another directory with non-audio files. The daemon logs parser errors and creates empty `books` rows for every file it can't parse, polluting the library.

**Fix:** Add a `library_roots` table (or config key). Canonicalize the path and verify it starts with an allowed root.

### 3.2 No authentication on mutating endpoints

**Severity: CRITICAL**

The plan describes a full auth layer (`SECURITY.md`, `API.md` § Pairing). The code has zero middleware. Every endpoint — `POST /library/scan`, `POST /books/{id}/retry`, `POST /books/{id}/audiologo`, `POST /clean/run` — is open.

**Note:** This is not "missing code for a future theme." Auth is documented as part of the v0 scaffold (`PROJECT.md` § Authentication + pairing). The `tokens` and `pairing_codes` tables exist, the `ApiError` variants exist, but the enforcement layer does not.

**Fix:** Add a Tower `RequireAuth` layer. Even a hardcoded admin token in `config.toml` would close the gap until the pairing flow ships.

### 3.3 `version` and `health` leak information without auth

**Severity: LOW**

`GET /version` returns the exact app version. `GET /health` returns uptime. These are public in the current code. The plan says `/health` and `/version` are "No auth" — so this is intentional for those two endpoints. However, if the daemon is exposed to LAN (operator flips `bind`), version fingerprinting helps an attacker identify known vulnerabilities.

**Fix:** Consider rate-limiting these endpoints or requiring auth when the bind address is not loopback.

### 3.4 `mass_edit_history` lacks referential integrity

**Severity: MEDIUM**

Migration 001:
```sql
CREATE TABLE mass_edit_history (
    target_kind TEXT NOT NULL,  -- "book", "book_files", "tags"
    target_id   INTEGER NOT NULL,
    ...
) STRICT;
```

No FK constraint. Deleting a book leaves orphaned audit rows. Worse, `target_id` is polymorphic — a `target_id` of `5` could mean book 5, file 5, or tag 5 depending on `target_kind`. There's no way to enforce consistency.

**Fix:** Split into `book_edit_history` and `file_edit_history` tables, or add a composite trigger that validates `target_kind` against the right parent table.

### 3.5 SQL injection risk in `names.rs` (mitigated but present)

**Severity: LOW**

```rust
let sql = format!("SELECT 1 FROM {table} WHERE {pk} = ? LIMIT 1");
```

`table` and `pk` come from `IdentityKind`'s `const fn` methods. The enum is closed, so an attacker cannot inject arbitrary strings through the API. But if a developer adds a new variant and makes a typo in the `const fn`, the malformed SQL is only caught at runtime.

**Fix:** Use a macro or `match` arms with literal SQL strings so `sqlx::query!()` can still compile-time-check each variant.

### 3.6 `AudiologoCutRequest.kind` validated manually instead of by Serde

**Severity: LOW**

```rust
if req.kind != "intro" && req.kind != "outro" { … }
```

This manual check is unnecessary. A `#[derive(Deserialize)]` enum with `#[serde(rename_all = "lowercase")]` would reject invalid values at the deserialization layer, returning a structured 400 automatically.

---

## 4. DB Schema & Code Improvements

### 4.1 Schema fixes

| # | Issue | Migration | Fix |
|---|---|---|---|
| 1 | `jobs.priority` CHECK missing `'idle'` | `ephemeral/001_initial.sql:16` | Add `'idle'` to the CHECK list. The scheduler writes `Priority::Idle` for `transcribe-full`; the DB would reject it if the enum were enforced. |
| 2 | `pipeline_progress.status` CHECK missing `'running'` | `ephemeral/001_initial.sql:39` | Add `'running'`. `write_progress_start` inserts this status. |
| 3 | `pairing_codes` missing index for cleanup target | `ephemeral/001_initial.sql:58` | Add `CREATE INDEX idx_pairing_codes_cleanup ON pairing_codes(expires_at, consumed_token_id)` — the `ExpiredPairingCodesTarget` does `WHERE consumed_token_id IS NULL AND expires_at < ?` on every cleanup tick. |
| 4 | `books.audiologo_status` has no index | `library/010_audiologo_per_file_splice.sql:86` | Add `CREATE INDEX idx_books_audiologo_status ON books(audiologo_status) WHERE audiologo_status != 'unknown'` for library listing queries. |
| 5 | `book_file_refs` table has no consumers | `library/018_book_file_refs.sql` | Either add the first acquire/release helper in `ab_db`, or drop the migration until transcode lands. Dead schema accumulates technical debt. |

### 4.2 Code improvements

| # | Issue | Location | Fix |
|---|---|---|---|
| 1 | `books_list` uses 5 correlated subqueries | `api/src/router.rs:599-627` | Rewrite as `LEFT JOIN`s. On a large library the current query does O(n × 5) work. |
| 2 | `pre_reset_signal` does 3 round-trips | `api/src/router.rs:1128-1165` | Combine into one `UNION ALL` query or a CTE. |
| 3 | `resolve_stages` rebuilds name list unnecessarily | `api/src/router.rs:1093-1101` | `dag.stage_id_by_name()` already exists — use it directly instead of `known_stage_names().into_iter().find(...)`. |
| 4 | `shift_chapters_for_cut` computes `cumulative_before` with raw SQL | `api/src/audiologo_apply.rs:228-252` | The SQL is correct but complex. Extract a named helper or view (`book_file_cumulative_offsets`) so the logic is testable in isolation. |
| 5 | `run_retry_for_each` leaks on panic | `api/src/router.rs:1064-1121` | The `stage_ctx` uses a throw-away `CancellationToken`. If a stage panics, the retry task is orphaned. Use the daemon's token. |

---

## 5. What I Would Have Done Differently

### 5.1 Implement auth middleware before the first mutating endpoint

The plan documents auth as part of the scaffold, but the implementation treats it as a "Theme 7" afterthought. The problem: every new endpoint since 1A was added without auth, so retrofitting means touching every handler.

**What I'd do:** In slice 1A, add a `RequireAuth` Tower layer that rejects everything except `/health` and `/version` with `401`. Use a single hardcoded token in `config.toml` for v0. This is ~30 lines and makes every future endpoint "secure by default." The full pairing flow can replace the hardcoded token in Theme 7 without touching handlers.

### 5.2 Load config from file before wiring the pipeline

`Tunables::default()` is fine for tests, but using it in `main.rs` means the daemon silently ignores `config.toml`. I would have wired `figment` in `main.rs` immediately after `Args::parse()`, before `build_pipeline_stages()`. This would have:
- Let operators tune `idle_wait_secs` and concurrency without recompiling.
- Made `abs_enabled` a runtime toggle, matching the plan's "opt-in" description.
- Surfaced config errors at boot time instead of compile time.

### 5.3 Add `library_roots` before `scan`

The plan says "Explicit user-supplied directories only." I would have created a `library_roots` table and a `POST /library/roots` endpoint in slice 1A, then made `POST /library/scan` accept `{ root_id }` instead of `{ path }`. This would have:
- Enforced the path-traversal defense from day 1.
- Made the "scan all roots" feature (`{ all: true }`) trivial.
- Provided a natural place for per-root tunables (e.g., "this root is on a slow NAS").

### 5.4 Ship ABS bridge endpoints incrementally, not as a monolithic theme

The `shelf` crate is a 48-line stub with 2 routes. Rather than deferring the entire bridge to "Theme 5," I would have implemented `/api/libraries`, `/api/items/{id}`, and `/api/items/{id}/file/{ino}` as soon as the player UI needed them. This provides continuous integration feedback with real ABS clients instead of a big-bang test later.

### 5.5 Use `argon2` for token hashing from the start

The plan says `blake3` for token hashing. I would have pushed back on this during the security review. blake3 is a general-purpose hash, not a password hash. For a handful of tokens per user, `argon2id` adds negligible latency and provides meaningful brute-force resistance if the DB is exfiltrated.

---

## 6. Changes to Improve Adherence to Plan

### 6.1 Immediate (closes deviations)

1. **Wire `figment` config loading** in `main.rs`.
   - Read `args.config` or default to `storage_root/config.toml`.
   - Use `figment::Jail` with `Toml` + `Env` layers.
   - Log the resolved path at `info!` on boot.

2. **Add `library_roots` table + path validation**.
   - Table: `library_roots(root_id, path TEXT UNIQUE, label, added_at)`.
   - `POST /library/roots` to add a root.
   - `POST /library/scan` validates the path against the roots table.

3. **Add `RequireAuth` middleware** (even with a hardcoded token).
   - Allow `GET /health`, `GET /version` without auth.
   - Read `Authorization: Bearer <token>` header.
   - Match against `tokens.token_hash` (or a hardcoded fallback for v0).
   - Return existing `ApiError::Unauthorized` / `ApiError::Forbidden` variants.

### 6.2 Short-term (closes security gaps)

4. **Fix `run_retry_for_each` cancellation**.
   - Clone the daemon's `CancellationToken` into `ApiState`.
   - Pass it to `StageContext` in the retry handler.

5. **Add `AudiologoStatus` typed enum** in `ab_core`.
   - Mirror the `Field` / `CacheKey` pattern.
   - Migrate `audiologo_apply.rs` and `router.rs` to use it.

6. **Rewrite `books_list` with JOINs** and add query parameter support.
   - Implement `q`, `author`, `series`, `limit`, `offset` as documented in `API.md`.

### 6.3 Medium-term (structural)

7. **Implement the three critical ABS endpoints** (`/api/libraries`, `/api/items/{id}`, `/api/items/{id}/file/{ino}`).
   - These are needed for the mobile client story; the schema already supports them.

8. **Split `api/src/router.rs`** into per-domain modules (`library.rs`, `books.rs`, `doctor.rs`, `clean.rs`).
   - `router.rs` is 1388 lines and mixes 8 domains.

9. **Add `library_roots` to the config schema** and remove raw-path scan.
   - This is the minimal change to close the path-traversal hole.

---

## 7. Structural Changes

### 7.1 Add `auth.rs` + `pairing.rs` inside `crates/api/src/`

Current: auth is scattered across doc comments and schema. Proposed:
```
crates/api/src/
  auth.rs       # Tower middleware + token validation
  pairing.rs    # /pair/init, /pair/poll, /pair/approve handlers
  router.rs     # .layer(auth::RequireAuth) on protected routes
```

### 7.2 Move config loading into `ab_core` or a new `ab_config` crate

`Tunables` lives in `ab_core`, but the config file loader lives in `aborg-daemon`. The CLI binary cannot load the same config without depending on the daemon. Moving `figment` resolution into `ab_core` (behind a `config` feature) would let both binaries share the loading logic.

### 7.3 Gate unimplemented crates from `default-members`

`ab-tag-write`, `ab-transcode`, and `ab-audio` are compiled on every `cargo build` but contribute nothing to the running daemon. Change `Cargo.toml`:

```toml
default-members = [
    "bins/aborg",
    "bins/aborg-daemon",
    # "crates/tag-write",   # uncomment when wired
    # "crates/transcode",   # uncomment when wired
    # "crates/audio",       # uncomment when wired
]
```

This cuts compile time and `Cargo.lock` churn without losing the source.

### 7.4 Add `library_roots` table and make scan root-aware

Schema:
```sql
CREATE TABLE library_roots (
    root_id INTEGER PRIMARY KEY AUTOINCREMENT,
    path TEXT NOT NULL UNIQUE,
    label TEXT,
    added_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
) STRICT;
```

- `POST /library/roots` — add a root.
- `GET /library/roots` — list roots.
- `POST /library/scan` body changes from `{ path }` to `{ root_id? | all: true }`.
- Scan validates the resolved path against the roots table.

This closes the path-traversal vulnerability and aligns with the plan's "Explicit user-supplied directories only" rule.

---

## Bottom Line

The **pipeline, typed primitives, and drift-detection infrastructure** are excellent — they match the plan closely and show disciplined engineering. The biggest gaps are **security** (missing auth enforcement, missing path validation) and **config loading** (documented but unwired). These are not "future theme" items; they are scaffold infrastructure that the plan explicitly says should exist now.

Fix priority:
1. **Auth middleware** (closes the largest attack surface with minimal code).
2. **Config file loading** (unlocks every other tunable).
3. **Path validation on scan** (closes path traversal).
4. **Typed `AudiologoStatus`** (closes a gap in the typed-primitives work).
5. **ABS bridge MVP** (unblocks the mobile client story).
