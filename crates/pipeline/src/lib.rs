//! Pipeline runtime — one pipeline, declarative stage ordering.
//!
//! # Model
//!
//! * A [`Stage`] is an async unit of work over a single [`BookId`].
//! * Stages declare what they require from earlier stages (`after`).
//! * The [`Dag`] is the registry; topological order is computed once.
//! * The [`Scheduler`] drains a priority-aware queue, calling stages
//!   in the order their requirements complete.
//!
//! # Invariants (see `docs/POLICIES.md`)
//!
//! * **Stage outputs persist before stage returns.** The `Output` type
//!   carries identifiers only — heavy data goes to the DB/filesystem
//!   inside [`Stage::run`]. The scheduler never holds book data
//!   in-memory across stages.
//! * **Stage retries are explicit.** A `Result::Err` is recorded in
//!   `pipeline_progress.failure_reason`; the scheduler does not loop.
//!   Retry policy lives in the job-runner, not the executor.
//! * **Stages are cancellation-safe.** Receiving a shutdown signal
//!   while running aborts cleanly; partial writes are rolled back via
//!   transactions.

pub mod dag;
pub mod scheduler;
pub mod stage;

pub use dag::{Dag, DagBuildError};
pub use scheduler::{Priority, Scheduler};
pub use stage::{Stage, StageContext, StageId, StageOutcome};
