//! Identity-resolve tier hierarchy scaffolding (ADR-0038, slice B.10).
//!
//! Defines the trait + types every future identity tier plugs into.
//! The existing [`crate::identity::IdentityResolveStage`] still owns
//! the production path; this scaffolding ships first so the new
//! tiers (`transcript_verify`, `canonical_author_audnexus`,
//! `author_disambig`, `asin_auto_learn`) land as
//! `impl IdentityTier` slices without touching working code each
//! time. The migration of the existing in-stage heuristics into
//! formal tier impls is the next slice in this thread.

use ab_core::BookId;
use std::sync::Arc;

/// Stable identifier for one tier. Used in `tier_log` audit rows
/// and operator-facing debug output.
pub type TierName = &'static str;

/// Sort key controlling cascade order — **lower wins**.
pub type Precedence = u32;

/// Suggested precedence values for the v1.0 tier lineup. The
/// concrete tier impls reference these so reordering happens in
/// one place rather than in scattered consts.
pub mod precedence {
    use super::Precedence;
    pub const ASIN_EXACT: Precedence = 10;
    pub const CANONICAL_AUTHOR_AUDNEXUS: Precedence = 20;
    pub const TRANSCRIPT_VERIFY: Precedence = 30;
    pub const NAME_EXACT: Precedence = 40;
    pub const NAME_FUZZY: Precedence = 50;
    pub const DISAMBIG: Precedence = 60;
    pub const ASIN_AUTO_LEARN: Precedence = 70;
}

/// A single ASIN candidate emitted by an upstream source. Tiers
/// accumulate these in [`IdentityCandidate::asin_hints`] so later
/// tiers (notably `asin_auto_learn`) can act on them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AsinHint {
    pub asin: String,
    pub source: &'static str,
    pub confidence: u16, // basis points 0..=10_000
}

/// One candidate-row spotted by an enrichment pass.
///
/// The shape mirrors `book_field_provenance` rows but is held
/// in-memory while tiers iterate; once a tier locks a field, the
/// winning row is written back to the DB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvenanceRow {
    pub field: &'static str,
    pub value: String,
    pub source: &'static str,
    pub external_id: Option<String>,
    pub confidence: u16, // basis points 0..=10_000
}

/// Identity hit returned when a tier could match multiple
/// candidates. Wrapped in [`TierResult::Ambiguous`] so the
/// disambig tier (or operator review) can pick a winner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityHit {
    pub kind: IdentityKind,
    pub id: i64,
    pub display: String,
    pub score: u16, // basis points
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityKind {
    Author,
    Narrator,
    Series,
}

/// Audit entry — one row per tier that touched the candidate.
/// Persisted as a small JSON column on `books` so the operator can
/// see exactly how each field was resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TierLogEntry {
    pub tier: TierName,
    pub status: TierStatus,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TierStatus {
    Resolved,
    NoMatch,
    Ambiguous,
    Skipped,
}

/// State threaded through every tier.
///
/// Tiers may mutate the `locked_*` fields once they reach a
/// [`TierResult::Resolved`] verdict; downstream tiers see those
/// locks and skip fields that already have a winner.
#[derive(Debug, Clone)]
pub struct IdentityCandidate {
    pub book_id: BookId,
    pub author_provenance: Vec<ProvenanceRow>,
    pub narrator_provenance: Vec<ProvenanceRow>,
    pub series_provenance: Vec<ProvenanceRow>,
    pub asin_hints: Vec<AsinHint>,
    pub locked_author_id: Option<i64>,
    pub locked_narrator_ids: Option<Vec<i64>>,
    pub locked_series_id: Option<i64>,
    pub tier_log: Vec<TierLogEntry>,
}

impl IdentityCandidate {
    /// Construct an empty candidate for the given book.
    #[must_use]
    pub const fn new(book_id: BookId) -> Self {
        Self {
            book_id,
            author_provenance: Vec::new(),
            narrator_provenance: Vec::new(),
            series_provenance: Vec::new(),
            asin_hints: Vec::new(),
            locked_author_id: None,
            locked_narrator_ids: None,
            locked_series_id: None,
            tier_log: Vec::new(),
        }
    }
}

/// Pool + tunable handles a tier may need. Kept narrow — tiers
/// should reach for new context fields via this struct rather than
/// taking direct dependencies, so the cascade stays trait-bound.
#[derive(Clone)]
pub struct TierCtx {
    pub library: sqlx::SqlitePool,
}

/// Outcome of one tier's attempt at the cascade.
#[derive(Debug, Clone)]
pub enum TierResult {
    /// Tier resolved at least one field. The `confidence` value
    /// (basis points `0..=10_000`) is recorded on the
    /// `book_field_provenance` row the tier writes.
    Resolved { confidence: u16 },
    /// Tier saw nothing actionable; cascade continues with the
    /// next tier.
    NoMatch,
    /// Tier found multiple candidates with similar scores; the
    /// cascade short-circuits and the candidate set is handed off
    /// to the disambig path / operator review queue.
    Ambiguous { candidates: Vec<IdentityHit> },
}

/// One step of the identity cascade.
#[async_trait::async_trait]
pub trait IdentityTier: Send + Sync {
    fn name(&self) -> TierName;
    fn precedence(&self) -> Precedence;

    async fn resolve(&self, ctx: &TierCtx, candidate: &mut IdentityCandidate) -> TierResult;
}

/// Ordered registry — tiers are sorted by precedence on insert.
#[derive(Clone, Default)]
pub struct TierRegistry {
    tiers: Vec<Arc<dyn IdentityTier>>,
}

impl TierRegistry {
    #[must_use]
    pub fn new(mut tiers: Vec<Arc<dyn IdentityTier>>) -> Self {
        tiers.sort_by_key(|t| t.precedence());
        Self { tiers }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tiers.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tiers.is_empty()
    }

    #[must_use]
    pub fn names_in_order(&self) -> Vec<TierName> {
        self.tiers.iter().map(|t| t.name()).collect()
    }

    /// Walk the cascade until a tier returns
    /// [`TierResult::Resolved`] or [`TierResult::Ambiguous`]. The
    /// candidate's `tier_log` is updated in-place. Returns the
    /// final result (the last tier that returned `NoMatch` if no
    /// tier resolved).
    pub async fn run(&self, ctx: &TierCtx, candidate: &mut IdentityCandidate) -> TierResult {
        let mut final_result = TierResult::NoMatch;
        for tier in &self.tiers {
            let result = tier.resolve(ctx, candidate).await;
            let status = match &result {
                TierResult::Resolved { .. } => TierStatus::Resolved,
                TierResult::NoMatch => TierStatus::NoMatch,
                TierResult::Ambiguous { .. } => TierStatus::Ambiguous,
            };
            candidate.tier_log.push(TierLogEntry {
                tier: tier.name(),
                status,
                note: None,
            });
            match result {
                TierResult::Resolved { .. } | TierResult::Ambiguous { .. } => {
                    return result;
                }
                TierResult::NoMatch => {
                    final_result = result;
                }
            }
        }
        final_result
    }
}

impl std::fmt::Debug for TierRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TierRegistry")
            .field("tiers", &self.names_in_order())
            .finish()
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    struct FixedTier {
        name: TierName,
        precedence: Precedence,
        result: TierResult,
    }
    #[async_trait::async_trait]
    impl IdentityTier for FixedTier {
        fn name(&self) -> TierName {
            self.name
        }
        fn precedence(&self) -> Precedence {
            self.precedence
        }
        async fn resolve(&self, _ctx: &TierCtx, _c: &mut IdentityCandidate) -> TierResult {
            self.result.clone()
        }
    }

    fn dummy_ctx() -> TierCtx {
        // Pool is never reached in these tests — fixed tiers ignore it.
        // SqlitePool is constructed lazily by sqlx, so we use a
        // disconnected lazy pool that never opens a connection.
        let pool = sqlx::pool::PoolOptions::<sqlx::Sqlite>::new()
            .min_connections(0)
            .max_connections(1)
            .connect_lazy("sqlite::memory:")
            .expect("lazy pool");
        TierCtx { library: pool }
    }

    #[tokio::test]
    async fn cascade_short_circuits_on_resolved() {
        let registry = TierRegistry::new(vec![
            Arc::new(FixedTier {
                name: "first",
                precedence: 10,
                result: TierResult::NoMatch,
            }),
            Arc::new(FixedTier {
                name: "second",
                precedence: 20,
                result: TierResult::Resolved { confidence: 9_500 },
            }),
            Arc::new(FixedTier {
                name: "third",
                precedence: 30,
                result: TierResult::Resolved { confidence: 5_000 },
            }),
        ]);
        let mut candidate = IdentityCandidate::new(BookId(1));
        let result = registry.run(&dummy_ctx(), &mut candidate).await;
        match result {
            TierResult::Resolved { confidence } => assert_eq!(confidence, 9_500),
            _ => panic!("expected Resolved"),
        }
        // Both the no-match and the winning resolver should be in the log;
        // the third tier should NOT have run.
        assert_eq!(candidate.tier_log.len(), 2);
        assert_eq!(candidate.tier_log[0].tier, "first");
        assert_eq!(candidate.tier_log[1].tier, "second");
    }

    #[tokio::test]
    async fn ambiguous_short_circuits_to_disambig() {
        let hit = IdentityHit {
            kind: IdentityKind::Author,
            id: 7,
            display: "x".into(),
            score: 8_000,
        };
        let registry = TierRegistry::new(vec![Arc::new(FixedTier {
            name: "fuzzy",
            precedence: 50,
            result: TierResult::Ambiguous {
                candidates: vec![hit.clone(), hit.clone()],
            },
        })]);
        let mut candidate = IdentityCandidate::new(BookId(1));
        let result = registry.run(&dummy_ctx(), &mut candidate).await;
        match result {
            TierResult::Ambiguous { candidates } => assert_eq!(candidates.len(), 2),
            _ => panic!("expected Ambiguous"),
        }
    }

    #[test]
    fn registry_sorts_by_precedence_on_insert() {
        let r = TierRegistry::new(vec![
            Arc::new(FixedTier {
                name: "later",
                precedence: 50,
                result: TierResult::NoMatch,
            }),
            Arc::new(FixedTier {
                name: "earlier",
                precedence: 10,
                result: TierResult::NoMatch,
            }),
        ]);
        assert_eq!(r.names_in_order(), vec!["earlier", "later"]);
    }
}
