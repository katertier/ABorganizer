//! Priority-aware scheduler. Two channels (interactive + background)
//! with `tokio::select!`-biased polling so interactive work always
//! preempts background drainage.

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::info;

use ab_core::tunables::SchedulerTunables;
use ab_core::{BookId, Result};

use crate::dag::Dag;
use crate::stage::StageContext;

/// Job priority. Interactive (user-initiated) preempts background.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Priority {
    /// User-initiated work — always runs first.
    Interactive,
    /// Daemon background drainage.
    Background,
}

/// Internal job envelope passed through queues.
#[derive(Debug, Clone)]
struct Job {
    book_id: BookId,
    stage: &'static str,
    /// Retained for logging + future re-prioritisation; the executor
    /// already routed based on this when picking the channel.
    #[allow(dead_code)]
    priority: Priority,
}

/// The pipeline scheduler. Holds the DAG and two mpsc senders.
pub struct Scheduler {
    /// DAG reference retained for `Scheduler::dag()` introspection
    /// (gaps reporting, web UI status). Not used directly by the
    /// executor — that owns its own clone.
    #[allow(dead_code)]
    dag: Arc<Dag>,
    interactive_tx: mpsc::Sender<Job>,
    background_tx: mpsc::Sender<Job>,
    cancel: CancellationToken,
}

impl Scheduler {
    /// Construct a scheduler around a DAG. Spawns a worker that
    /// drains the queues; the returned handle is used to submit work.
    ///
    /// Channel buffer sizes come from `tunables`. Real backpressure
    /// lives in the DB jobs table (`max_pending_per_stage`).
    pub fn spawn(dag: Arc<Dag>, ctx: StageContext, tunables: &SchedulerTunables) -> Self {
        let (interactive_tx, mut interactive_rx) =
            mpsc::channel::<Job>(tunables.interactive_buffer);
        let (background_tx, mut background_rx) = mpsc::channel::<Job>(tunables.background_buffer);
        let cancel = ctx.cancel.clone();

        let worker_dag = Arc::clone(&dag);
        tokio::spawn(async move {
            info!(stages = worker_dag.len(), "pipeline.scheduler.started");
            loop {
                tokio::select! {
                    biased;
                    () = ctx.cancel.cancelled() => {
                        info!("pipeline.scheduler.cancelled");
                        break;
                    }
                    Some(job) = interactive_rx.recv() => {
                        Self::execute(&worker_dag, &ctx, job).await;
                    }
                    Some(job) = background_rx.recv() => {
                        Self::execute(&worker_dag, &ctx, job).await;
                    }
                    else => break,
                }
            }
        });

        Self {
            dag,
            interactive_tx,
            background_tx,
            cancel,
        }
    }

    /// Submit a book + stage for processing.
    ///
    /// # Errors
    ///
    /// Returns an error if the channel is closed (scheduler shutting
    /// down).
    pub async fn submit(
        &self,
        book_id: BookId,
        stage: &'static str,
        priority: Priority,
    ) -> Result<()> {
        let job = Job {
            book_id,
            stage,
            priority,
        };
        let sender = match priority {
            Priority::Interactive => &self.interactive_tx,
            Priority::Background => &self.background_tx,
        };
        sender
            .send(job)
            .await
            .map_err(|_| ab_core::Error::Invariant("scheduler channel closed"))?;
        Ok(())
    }

    /// Initiate graceful shutdown.
    pub fn shutdown(&self) {
        self.cancel.cancel();
    }

    async fn execute(dag: &Arc<Dag>, ctx: &StageContext, job: Job) {
        let stage_obj = dag
            .iter_topo()
            .find_map(|(name, s)| (name == job.stage).then_some(s.clone()));
        let Some(stage_obj) = stage_obj else {
            tracing::warn!(stage = job.stage, "pipeline.stage.unknown");
            return;
        };

        let stage_ctx = StageContext {
            stage_name: job.stage,
            ..ctx.clone()
        };

        match stage_obj.run(&stage_ctx, job.book_id).await {
            Ok(outcome) => {
                tracing::info!(
                    stage = job.stage,
                    book = %job.book_id,
                    ?outcome,
                    "pipeline.stage.complete"
                );
            }
            Err(err) => {
                tracing::warn!(
                    stage = job.stage,
                    book = %job.book_id,
                    error = %err,
                    "pipeline.stage.failed"
                );
            }
        }
    }
}
