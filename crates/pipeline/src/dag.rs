//! Build a topologically-sorted DAG from registered stages.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use thiserror::Error;

use crate::stage::{Stage, StageEntry};

/// DAG-construction errors.
#[derive(Debug, Error)]
pub enum DagBuildError {
    /// Two stages declared the same `name()`.
    #[error("duplicate stage name: {0}")]
    DuplicateStage(&'static str),
    /// A stage requires a name that was never registered.
    #[error("stage {stage} requires unknown stage {missing}")]
    UnknownDependency {
        /// Stage that has the bad dependency.
        stage: &'static str,
        /// The name it asked for.
        missing: &'static str,
    },
    /// A cycle exists in the requirement graph.
    #[error("cycle detected in pipeline: {0:?}")]
    Cycle(Vec<&'static str>),
}

/// The set of registered stages, ordered topologically.
pub struct Dag {
    /// Stages keyed by name.
    stages: BTreeMap<&'static str, Arc<dyn Stage>>,
    /// Topological order — guaranteed valid by construction.
    order: Vec<&'static str>,
}

impl Dag {
    /// Build a `Dag` from a set of registered stages.
    ///
    /// # Errors
    ///
    /// Returns [`DagBuildError`] on duplicates, missing dependencies,
    /// or cycles.
    pub fn build(stages: Vec<Arc<dyn Stage>>) -> Result<Self, DagBuildError> {
        let mut by_name: BTreeMap<&'static str, StageEntry> = BTreeMap::new();
        for s in stages {
            let name = s.name();
            if by_name
                .insert(
                    name,
                    StageEntry {
                        stage: Arc::clone(&s),
                    },
                )
                .is_some()
            {
                return Err(DagBuildError::DuplicateStage(name));
            }
        }

        // Verify all `requires()` references exist.
        for entry in by_name.values() {
            for dep in entry.stage.requires() {
                if !by_name.contains_key(dep.as_str()) {
                    return Err(DagBuildError::UnknownDependency {
                        stage: entry.stage.name(),
                        missing: dep.as_str(),
                    });
                }
            }
        }

        // Kahn's algorithm for topo sort.
        let mut in_degree: HashMap<&'static str, usize> =
            by_name.keys().copied().map(|k| (k, 0_usize)).collect();
        for entry in by_name.values() {
            for _dep in entry.stage.requires() {
                if let Some(c) = in_degree.get_mut(entry.stage.name()) {
                    *c += 1;
                }
            }
        }

        let mut ready: BTreeSet<&'static str> = in_degree
            .iter()
            .filter(|(_, c)| **c == 0)
            .map(|(k, _)| *k)
            .collect();
        let mut order: Vec<&'static str> = Vec::with_capacity(by_name.len());

        while let Some(&name) = ready.iter().next() {
            ready.remove(name);
            order.push(name);
            // For every stage `s` that requires `name`, decrement its
            // in-degree; if it hits 0, mark ready.
            let names: Vec<&'static str> = by_name
                .values()
                .filter(|e| e.stage.requires().iter().any(|d| d.as_str() == name))
                .map(|e| e.stage.name())
                .collect();
            for s in names {
                if let Some(c) = in_degree.get_mut(s) {
                    *c -= 1;
                    if *c == 0 {
                        ready.insert(s);
                    }
                }
            }
        }

        if order.len() != by_name.len() {
            let remaining: Vec<&'static str> = in_degree
                .iter()
                .filter(|(_, c)| **c > 0)
                .map(|(k, _)| *k)
                .collect();
            return Err(DagBuildError::Cycle(remaining));
        }

        let stages = by_name.into_iter().map(|(k, e)| (k, e.stage)).collect();
        Ok(Self { stages, order })
    }

    /// Topological iteration of stages. Skips any name that has gone
    /// missing from the map between construction and iteration (which
    /// can't happen unless something has reached in and mutated us
    /// through `unsafe`, since both fields are private and immutable
    /// post-`build`).
    pub fn iter_topo(&self) -> impl Iterator<Item = (&'static str, &Arc<dyn Stage>)> {
        self.order
            .iter()
            .filter_map(move |name| self.stages.get(name).map(|s| (*name, s)))
    }

    /// Number of registered stages.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// True if no stages are registered.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::panic, clippy::expect_used)]
mod tests {
    use super::*;
    use async_trait::async_trait;

    use ab_core::{BookId, Result};
    // StageId lives in the sibling `stage` module — needed for the
    // typed-requires() in the test stages below.
    use crate::stage::StageId;

    struct S(&'static str, &'static [StageId]);
    #[async_trait]
    impl Stage for S {
        fn name(&self) -> &'static str {
            self.0
        }
        fn requires(&self) -> &'static [StageId] {
            self.1
        }
        async fn run(
            &self,
            _ctx: &crate::StageContext,
            _book_id: BookId,
        ) -> Result<crate::StageOutcome> {
            Ok(crate::StageOutcome::Done)
        }
    }

    // Test-only stage IDs. Declared as `const` so the slice
    // literals below can reference them statically.
    const SCAN: StageId = StageId::new("scan");
    const FP: StageId = StageId::new("fingerprint");
    const TR: StageId = StageId::new("tag-read");
    const A: StageId = StageId::new("a");
    const B: StageId = StageId::new("b");

    #[test]
    fn linear_dag_orders_correctly() {
        let stages: Vec<Arc<dyn Stage>> = vec![
            Arc::new(S("scan", &[])),
            Arc::new(S("fingerprint", &[SCAN])),
            Arc::new(S("tag-read", &[FP])),
        ];
        let dag = Dag::build(stages).expect("valid");
        let order: Vec<&str> = dag.iter_topo().map(|(n, _)| n).collect();
        assert_eq!(order, vec!["scan", "fingerprint", "tag-read"]);
        let _ = TR;
    }

    #[test]
    fn detects_cycle() {
        let stages: Vec<Arc<dyn Stage>> = vec![Arc::new(S("a", &[B])), Arc::new(S("b", &[A]))];
        // `expect_err` requires `T: Debug` on the Ok branch; our Dag
        // can't easily derive Debug because it holds `Arc<dyn Stage>`.
        // Match instead.
        match Dag::build(stages) {
            Ok(_) => panic!("expected cycle error"),
            Err(e) => assert!(matches!(e, DagBuildError::Cycle(_))),
        }
    }

    #[test]
    fn detects_missing_dep() {
        const MISSING: StageId = StageId::new("missing");
        let stages: Vec<Arc<dyn Stage>> = vec![Arc::new(S("a", &[MISSING]))];
        match Dag::build(stages) {
            Ok(_) => panic!("expected unknown-dep error"),
            Err(e) => assert!(matches!(e, DagBuildError::UnknownDependency { .. })),
        }
    }
}
