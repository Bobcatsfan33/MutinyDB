//! `EXPLAIN MAINTENANCE`: measured work counters for standing queries (C10, D-26).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use schweep_circuit::NodeId;

use crate::{Handle, Memo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaintenanceNode {
    pub node: NodeId,
    pub operator: &'static str,
    pub steps: usize,
    pub emitted_entries: usize,
    pub shared_with: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryMaintenance {
    pub handle: Handle,
    pub nodes: Vec<MaintenanceNode>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplainMaintenance {
    pub epoch: u64,
    pub queries: Vec<QueryMaintenance>,
    pub distinct_nodes: usize,
    pub distinct_steps: usize,
    pub distinct_emitted_entries: usize,
}

impl ExplainMaintenance {
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "EXPLAIN MAINTENANCE\nepoch {}\n\
             counters are cumulative measured work; timings: testing/evidence/c10-benchmarks.json\n",
            self.epoch
        );
        for query in &self.queries {
            let _ = writeln!(out, "\nquery {}", query.handle.id());
            for node in &query.nodes {
                let sharing = if node.shared_with == 0 {
                    "private".to_owned()
                } else {
                    format!("shared with {} other quer(y|ies)", node.shared_with)
                };
                let _ = writeln!(
                    out,
                    "  node {:<3} {:<10} steps {:>10}  emitted {:>10}  {}",
                    node.node.index(),
                    node.operator,
                    node.steps,
                    node.emitted_entries,
                    sharing
                );
            }
        }
        let _ = writeln!(
            out,
            "\ndataflow: {} distinct nodes · {} steps · {} emitted entries; counted once each",
            self.distinct_nodes, self.distinct_steps, self.distinct_emitted_entries
        );
        out
    }
}

/// Build the report from the circuit's counters and the registry's actual node membership.
#[must_use]
pub fn explain_maintenance(memo: &Memo) -> ExplainMaintenance {
    let mut memberships: BTreeMap<NodeId, usize> = BTreeMap::new();
    for registration in memo.registrations().values() {
        for node in registration.nodes.iter().copied().collect::<BTreeSet<_>>() {
            *memberships.entry(node).or_default() += 1;
        }
    }

    let steps = memo.dataflow().step_counters();
    let emitted = memo.dataflow().counters();
    let node_record = |node: NodeId| MaintenanceNode {
        node,
        operator: memo.dataflow().node_label(node).unwrap_or("freed"),
        steps: steps.get(node.index()).copied().unwrap_or(0),
        emitted_entries: emitted.get(node.index()).copied().unwrap_or(0),
        shared_with: memberships
            .get(&node)
            .copied()
            .unwrap_or(1)
            .saturating_sub(1),
    };

    let queries = memo
        .registrations()
        .values()
        .map(|registration| QueryMaintenance {
            handle: registration.handle,
            nodes: registration
                .nodes
                .iter()
                .copied()
                .map(node_record)
                .collect(),
        })
        .collect();
    let distinct: BTreeSet<NodeId> = memberships.keys().copied().collect();
    ExplainMaintenance {
        epoch: memo.epoch(),
        queries,
        distinct_nodes: distinct.len(),
        distinct_steps: distinct
            .iter()
            .map(|node| steps.get(node.index()).copied().unwrap_or(0))
            .sum(),
        distinct_emitted_entries: distinct
            .iter()
            .map(|node| emitted.get(node.index()).copied().unwrap_or(0))
            .sum(),
    }
}

impl Memo {
    #[must_use]
    pub fn explain_maintenance(&self) -> ExplainMaintenance {
        explain_maintenance(self)
    }
}
