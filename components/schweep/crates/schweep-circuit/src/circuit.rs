//! The circuit: DAG wiring, epochs, and the step scheduler (`ARCHITECTURE.md` §5, C1).
//!
//! > **Circuit** — the compiled form of a query: a directed acyclic graph of operators through
//! > which deltas flow. One step of a circuit consumes one epoch's input deltas and produces one
//! > epoch's output deltas.
//!
//! ## The step, in one paragraph
//!
//! Sealing an epoch hands the circuit that epoch's input deltas. Each source node turns its
//! table's entries into a Z-set batch; each operator node is stepped once, in order, consuming
//! the outputs of the nodes it was wired to; the sink's output delta is added into the result
//! store. The epoch counter advances only when all of that has succeeded, so a reader never sees
//! a partial epoch (I-3).
//!
//! ## Why the schedule is trivially deterministic
//!
//! Nodes are evaluated in index order, and the builder only lets a node take input from a node
//! that already exists. Index order is therefore a topological order, and it is the *same*
//! topological order on every run and in every process — not merely *a* valid one. Nothing here
//! consults a hash map, a thread, or a clock (I-2, D-6).
//!
//! This is the single-threaded scheduler §6 C1 asks for. When it grows a work queue, the ordering
//! guarantee has to be restated and re-proven; it is written down here so that whoever does that
//! knows what they are on the hook for.
//!
//! ## One scheduler, one or many sinks (C6)
//!
//! C1 through C5 needed exactly one query per circuit. C6's memo needs many queries sharing one
//! dataflow, and the temptation is to give the memo its own step loop. It does not get one: a second
//! scheduler would be a second place for epoch discipline, state accounting and error attribution to
//! be wrong, and I-8 would then be comparing two runtimes rather than one runtime with sharing on and
//! off. So the *same* `Circuit` grew three capabilities instead:
//!
//! - **many sinks** — each with its own result store, error store, and the set of nodes whose errors
//!   belong to it ([`Circuit::add_sink`]);
//! - **a mutable topology** — nodes can be appended after `build`, and removed when nothing consumes
//!   them ([`Circuit::attach`], [`Circuit::remove`]);
//! - **a partial pass** — stepping a named subset of nodes, which is how a query registered mid-history
//!   catches up without re-stepping the nodes it is about to share ([`Circuit::catch_up`]).
//!
//! Nothing about the single-sink path changed: `build` makes one sink, `answer` reads it, and every
//! C1–C5 test still exercises the same code.
//!
//! Removal leaves a **hole** rather than renumbering, because node ids are handles the memo holds in
//! its hash and refcount maps. Holes are skipped by the pass; nothing can reference one, because a node
//! is only removed when its refcount reaches zero.

use std::collections::BTreeMap;

use schweep_ops::{error_schema, Operator, StateBound};
use schweep_zset::{Canonical, EpochDeltas, Row, Schema, Value, ZSetBatch};

use crate::error::{CircuitError, Result};
use crate::result_store::ResultStore;

/// Epochs are dense integers starting at 1 (S-6). Epoch 0 means "nothing has been sealed".
pub type Epoch = u64;

/// A handle to a node in a circuit under construction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(usize);

impl NodeId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

/// A node id can be built from a raw index.
///
/// That sounds like a hole — anyone can name a node that does not exist — and it is not, because
/// [`CircuitBuilder::add`] validates every id it is handed: an index at or beyond the node being
/// added is refused as [`CircuitError::NodeOutOfOrder`], and one past the end is refused as
/// [`CircuitError::UnknownNode`] at build time. The validation is what makes the constructor
/// safe, so the constructor is public: a planner that computes wiring before building it (C5)
/// needs to name nodes, and the defensive checks need to be testable.
impl From<usize> for NodeId {
    fn from(index: usize) -> NodeId {
        NodeId(index)
    }
}

#[derive(Debug)]
enum Node {
    /// An input: one table's deltas, presented under the query's column names.
    Source {
        /// The table this node reads. Names the *catalog* entry.
        table: String,
        /// The alias this node reads it under. Sources are keyed by alias, not table, so one table
        /// can feed two nodes — which is what a self-join is (S-26, and the oracle already supports
        /// it). Keying by table would have refused `FROM t a JOIN t b` as a duplicate source.
        alias: String,
        /// The schema the node emits — the table's columns under their `alias.column` names
        /// (S-10, S-23). Rows are positional, so this is a pure rename of the table's schema.
        schema: Schema,
    },
    Operator {
        op: Box<dyn Operator>,
        inputs: Vec<NodeId>,
    },
}

/// What one operator node is holding — the per-operator row of `EXPLAIN STATE` (C8, I-9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeState {
    pub operator: &'static str,
    /// The I-9 declaration, rendered.
    pub declared: String,
    /// Entries held, as the backend reports them. A measurement, not an estimate.
    pub entries: usize,
    /// The budget the declaration allows, or `None` for an admitted-unbounded operator.
    pub budget: Option<usize>,
    /// How many state backends this operator was handed (a join has two).
    pub backends: usize,
}

/// One query's view of the dataflow: where its answer comes from, and what it holds.
///
/// A sink owns the two integrals a standing query needs — its answer and its live errors — and the
/// set of nodes whose errors are *its* errors. With one query per circuit that set is every node; with
/// sharing it is the query's ancestors, which is what stops a second query's evaluation error from
/// appearing in this query's answer (I-8: sharing may change counters, never a result byte).
#[derive(Debug)]
struct Sink {
    node: NodeId,
    result: ResultStore,
    errors: ResultStore,
    ancestors: std::collections::BTreeSet<NodeId>,
}

/// A handle to one standing query's output within a circuit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SinkId(usize);

impl SinkId {
    #[must_use]
    pub fn index(self) -> usize {
        self.0
    }
}

/// A compiled query — or, from C6, a dataflow shared by several: operators, wiring, an epoch
/// counter, and one result store per sink.
#[derive(Debug)]
pub struct Circuit {
    /// `None` is a hole left by [`Circuit::remove`]. Ids stay stable across removals because the memo
    /// holds them in its hash and refcount maps.
    nodes: Vec<Option<Node>>,
    /// Alias → the source nodes reading under it. Ordered, so fingerprints and errors are stable
    /// (I-2).
    ///
    /// One alias may name several nodes here, which [`CircuitBuilder`] refuses within a single query:
    /// an alias is scoped to the query that declared it (S-10), and two queries sharing a dataflow may
    /// each call their scan `t`. Canonicalization usually collapses those two nodes into one anyway —
    /// but only usually, and never when sharing is switched off.
    sources: BTreeMap<String, Vec<NodeId>>,
    sinks: Vec<Option<Sink>>,
    epoch: Epoch,
    /// Entries each node has ever emitted, indexed by node.
    ///
    /// This is the I-9 accounting ledger. An operator's state budget is the number of entries ever
    /// handed to it — see [`Circuit::check_state_declarations`] for why that is the right bound and
    /// what it does and does not catch.
    emitted_entries: Vec<usize>,
    /// How many times each node has been stepped.
    ///
    /// The I-8 counter proof is about *work*, not about bytes: sharing means the common prefix is
    /// stepped once per epoch instead of once per query, and `emitted_entries` cannot show that
    /// because a node that emits nothing emits nothing either way.
    steps: Vec<usize>,
    /// Nodes whose `Unbounded` state declaration has been explicitly admitted (I-9).
    ///
    /// Empty in every circuit built by [`CircuitBuilder`], which refuses `Unbounded` outright. The
    /// memo's registry is the only thing that can put an id in here, and only when the registration
    /// asked for it in writing.
    admitted_unbounded: std::collections::BTreeSet<NodeId>,
}

/// Builds a circuit in dependency order.
///
/// There is no SQL here and there is not meant to be: §6 C1 asks for "a hand-built (no SQL yet)
/// circuit API". The incrementalizer that compiles a plan into this shape is `schweep-sql`, added in
/// C5; it calls exactly this builder, which is why there is one wiring path and not two.
#[derive(Debug, Default)]
pub struct CircuitBuilder {
    nodes: Vec<Node>,
    sources: BTreeMap<String, NodeId>,
}

impl CircuitBuilder {
    #[must_use]
    pub fn new() -> CircuitBuilder {
        CircuitBuilder::default()
    }

    /// Add an input reading `table` under `alias`, emitting rows under `schema`.
    ///
    /// `schema` is the table's columns renamed to `alias.column`; it must have the same arity and
    /// types as the catalog's schema, which the caller has already established by binding.
    ///
    /// Sources are keyed by **alias**, so one table may feed several nodes. That is what makes a
    /// self-join representable — `FROM t a JOIN t b` needs two source nodes over one table, and the
    /// oracle has supported it since C0.
    pub fn source(
        &mut self,
        table: impl Into<String>,
        alias: impl Into<String>,
        schema: Schema,
    ) -> Result<NodeId> {
        let table = table.into();
        let alias = alias.into();
        if self.sources.contains_key(&alias) {
            return Err(CircuitError::DuplicateSource(alias));
        }
        let id = NodeId(self.nodes.len());
        self.nodes.push(Node::Source {
            table,
            alias: alias.clone(),
            schema,
        });
        self.sources.insert(alias, id);
        Ok(id)
    }

    /// The nodes built so far, for a caller that needs to wrap them in a circuit itself.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Add an operator, wired to inputs that already exist.
    ///
    /// Refusing a forward reference is what makes index order a topological order, which is what
    /// makes the schedule deterministic (I-2). It also makes a cycle unrepresentable rather than
    /// merely undetected.
    pub fn add(&mut self, op: Box<dyn Operator>, inputs: Vec<NodeId>) -> Result<NodeId> {
        let id = NodeId(self.nodes.len());
        // One rule, two doors: the builder and `Circuit::attach` check wiring the same way, and the
        // builder never admits unbounded state — a single query cannot admit anything, because
        // admission is a property of a *registration* (I-9).
        check_wiring(op.as_ref(), &inputs, id, false)?;
        self.nodes.push(Node::Operator { op, inputs });
        Ok(id)
    }

    /// Finish, naming the node whose output stream the result store maintains.
    ///
    /// The single-sink door, unchanged since C1: one query, one sink, and every node in the circuit is
    /// an ancestor of it, so every error raised anywhere is this query's error.
    pub fn build(self, sink: NodeId) -> Result<Circuit> {
        if self.nodes.is_empty() {
            return Err(CircuitError::EmptyCircuit);
        }
        let node_count = self.nodes.len();
        let nodes: Vec<Option<Node>> = self.nodes.into_iter().map(Some).collect();
        let mut sources: BTreeMap<String, Vec<NodeId>> = BTreeMap::new();
        for (alias, id) in self.sources {
            sources.entry(alias).or_default().push(id);
        }
        let mut circuit = Circuit {
            nodes,
            sources,
            sinks: Vec::new(),
            epoch: 0,
            emitted_entries: vec![0; node_count],
            steps: vec![0; node_count],
            admitted_unbounded: std::collections::BTreeSet::new(),
        };
        circuit.add_sink(sink)?;
        circuit.prime()?;
        Ok(circuit)
    }
}

impl Circuit {
    /// The highest sealed epoch; 0 before anything is sealed.
    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.epoch
    }

    /// An empty circuit, for a caller that builds its nodes one at a time (C6's memo).
    ///
    /// There is no sink yet, so there is nothing to answer; `add_sink` comes later, once the nodes it
    /// reads exist. Priming is the caller's business too, because the caller knows which nodes are new
    /// — see [`Circuit::catch_up`].
    pub fn empty() -> Result<Circuit> {
        Ok(Circuit {
            nodes: Vec::new(),
            sources: BTreeMap::new(),
            sinks: Vec::new(),
            epoch: 0,
            emitted_entries: Vec::new(),
            steps: Vec::new(),
            admitted_unbounded: std::collections::BTreeSet::new(),
        })
    }

    /// The first sink — the only one a circuit built by [`CircuitBuilder`] has.
    fn only_sink(&self) -> Result<&Sink> {
        self.sinks
            .first()
            .and_then(Option::as_ref)
            .ok_or(CircuitError::UnknownSink(0))
    }

    fn sink_at(&self, id: SinkId) -> Result<&Sink> {
        self.sinks
            .get(id.0)
            .and_then(Option::as_ref)
            .ok_or(CircuitError::UnknownSink(id.0))
    }

    /// The schema of the answer this circuit maintains.
    ///
    /// `Err` only for a circuit with no sink — one the memo is still assembling. Every circuit
    /// [`CircuitBuilder::build`] produces has exactly one.
    pub fn output_schema(&self) -> Result<&Schema> {
        Ok(self.only_sink()?.result.schema())
    }

    /// The schema one sink's answer has.
    pub fn output_schema_of(&self, id: SinkId) -> Result<&Schema> {
        Ok(self.sink_at(id)?.result.schema())
    }

    /// The maintained answer as of the latest sealed epoch — or the live error, if there is one.
    ///
    /// The query has no answer while data that raises is present (S-22). The reported error is the
    /// least live message, which is the first row of the error store's canonical form because
    /// canonical order sorts by the message column (S-22c).
    pub fn answer(&self) -> Result<Canonical> {
        self.answer_of(SinkId(0))
    }

    /// The same, for one sink of a shared dataflow.
    pub fn answer_of(&self, id: SinkId) -> Result<Canonical> {
        let sink = self.sink_at(id)?;
        if !sink.errors.is_empty() {
            let live = sink.errors.canonical()?;
            let least = live
                .entries()
                .first()
                .and_then(|(row, _)| row.get(0))
                .and_then(|value| match value {
                    Value::Str(message) => Some(message.clone()),
                    _ => None,
                })
                .ok_or(CircuitError::CorruptErrorStore)?;
            return Err(CircuitError::LiveEvaluationError(least));
        }
        sink.result.canonical()
    }

    /// The live errors as a Z-set, for tests and for state fingerprints.
    pub fn error_store(&self) -> Result<&ResultStore> {
        Ok(&self.only_sink()?.errors)
    }

    pub fn result_store(&self) -> Result<&ResultStore> {
        Ok(&self.only_sink()?.result)
    }

    /// One sink's live errors.
    pub fn error_store_of(&self, id: SinkId) -> Result<&ResultStore> {
        Ok(&self.sink_at(id)?.errors)
    }

    /// One sink's answer store.
    pub fn result_store_of(&self, id: SinkId) -> Result<&ResultStore> {
        Ok(&self.sink_at(id)?.result)
    }

    /// The aliases this circuit reads under. Two aliases may name one table (a self-join).
    pub fn source_aliases(&self) -> impl Iterator<Item = &str> {
        self.sources.keys().map(String::as_str)
    }

    // ---- the mutable topology C6 needs ---------------------------------------------------------

    /// Append a source node to a live circuit (C6).
    ///
    /// Unlike [`CircuitBuilder::source`] this does **not** refuse a repeated alias: an alias belongs to
    /// the query that declared it, and two queries sharing a dataflow may each name their scan `t`.
    pub fn attach_source(
        &mut self,
        table: impl Into<String>,
        alias: impl Into<String>,
        schema: Schema,
    ) -> Result<NodeId> {
        let id = NodeId(self.nodes.len());
        let alias = alias.into();
        self.nodes.push(Some(Node::Source {
            table: table.into(),
            alias: alias.clone(),
            schema,
        }));
        self.emitted_entries.push(0);
        self.steps.push(0);
        self.sources.entry(alias).or_default().push(id);
        Ok(id)
    }

    /// Append an operator to a live circuit, wired to nodes that already exist (C6).
    ///
    /// `admit_unbounded` is the I-9 admission: an operator that declares `Unbounded` state is refused
    /// unless the registration that asked for it said so explicitly. Everything else about the
    /// declaration is checked exactly as [`CircuitBuilder::add`] checks it — one rule, two doors.
    pub fn attach(
        &mut self,
        op: Box<dyn Operator>,
        inputs: Vec<NodeId>,
        admit_unbounded: bool,
    ) -> Result<NodeId> {
        let id = NodeId(self.nodes.len());
        check_wiring(op.as_ref(), &inputs, id, admit_unbounded)?;
        for input in &inputs {
            if self.node_at(*input).is_err() {
                return Err(CircuitError::UnknownNode(input.0));
            }
        }
        if admit_unbounded {
            self.admitted_unbounded.insert(id);
        }
        self.nodes.push(Some(Node::Operator { op, inputs }));
        self.emitted_entries.push(0);
        self.steps.push(0);
        Ok(id)
    }

    /// Remove a node. The id becomes a hole; nothing may reference it.
    ///
    /// Refused if any live node still takes input from it, or any sink reads it — a memo with a
    /// refcount bug would otherwise cut a query's dataflow out from under it and the symptom would be
    /// an empty answer rather than an error.
    pub fn remove(&mut self, id: NodeId) -> Result<()> {
        if self.node_at(id).is_err() {
            return Err(CircuitError::UnknownNode(id.0));
        }
        for (index, node) in self.nodes.iter().enumerate() {
            if let Some(Node::Operator { inputs, .. }) = node {
                if inputs.contains(&id) {
                    return Err(CircuitError::NodeStillConsumed {
                        node: id.0,
                        consumer: index,
                    });
                }
            }
        }
        for sink in self.sinks.iter().flatten() {
            if sink.node == id {
                return Err(CircuitError::NodeStillConsumed {
                    node: id.0,
                    consumer: sink.node.0,
                });
            }
        }
        if let Some(slot) = self.nodes.get_mut(id.0) {
            if let Some(Node::Source { alias, .. }) = slot {
                let alias = alias.clone();
                if let Some(ids) = self.sources.get_mut(&alias) {
                    ids.retain(|held| *held != id);
                    if ids.is_empty() {
                        self.sources.remove(&alias);
                    }
                }
            }
            *slot = None;
        }
        self.admitted_unbounded.remove(&id);
        // The counters are *not* reset. They are the I-9 accounting ledger and the I-8 work ledger,
        // and both are histories: "this node emitted 40 entries before it was freed" stays true.
        Ok(())
    }

    /// Add a sink reading `node`, with its own answer and error stores.
    ///
    /// The sink's errors are the errors of `node` and everything upstream of it — computed here, once,
    /// because that ancestor set is what keeps one query's evaluation error out of another's answer.
    pub fn add_sink(&mut self, node: NodeId) -> Result<SinkId> {
        let schema = self.node_output_schema(node)?.clone();
        let ancestors = self.ancestors_of(node)?;
        let id = SinkId(self.sinks.len());
        self.sinks.push(Some(Sink {
            node,
            result: ResultStore::new(schema),
            errors: ResultStore::new(error_schema()?),
            ancestors,
        }));
        Ok(id)
    }

    /// Drop a sink. Its stores go with it; its nodes do not — that is the memo's refcount to spend.
    pub fn remove_sink(&mut self, id: SinkId) -> Result<()> {
        let slot = self
            .sinks
            .get_mut(id.0)
            .ok_or(CircuitError::UnknownSink(id.0))?;
        if slot.is_none() {
            return Err(CircuitError::UnknownSink(id.0));
        }
        *slot = None;
        Ok(())
    }

    /// Recompute one sink's ancestor set, after the memo has rewired it onto shared nodes.
    pub fn refresh_ancestors(&mut self, id: SinkId) -> Result<()> {
        let node = self.sink_at(id)?.node;
        let ancestors = self.ancestors_of(node)?;
        if let Some(Some(sink)) = self.sinks.get_mut(id.0) {
            sink.ancestors = ancestors;
        }
        Ok(())
    }

    /// Point a sink at a different node — the last step of attaching to a shared subtree.
    ///
    /// The new node must emit the same schema, because the sink's answer store already holds rows of
    /// the old one and a schema change would make the two halves of one integral disagree (S-8).
    pub fn repoint_sink(&mut self, id: SinkId, node: NodeId) -> Result<()> {
        let schema = self.node_output_schema(node)?.clone();
        let current = self.sink_at(id)?;
        if current.result.schema() != &schema {
            return Err(CircuitError::SinkSchemaMismatch {
                held: current.result.schema().to_string(),
                offered: schema.to_string(),
            });
        }
        if let Some(Some(sink)) = self.sinks.get_mut(id.0) {
            sink.node = node;
        }
        self.refresh_ancestors(id)
    }

    /// The nodes one node takes input from, left to right.
    pub fn inputs_of(&self, id: NodeId) -> Result<Vec<NodeId>> {
        match self.node_at(id)? {
            Node::Source { .. } => Ok(Vec::new()),
            Node::Operator { inputs, .. } => Ok(inputs.clone()),
        }
    }

    /// The node a sink reads.
    pub fn sink_node(&self, id: SinkId) -> Result<NodeId> {
        Ok(self.sink_at(id)?.node)
    }

    /// Move every input edge that points at `from` so it points at `to`, and say how many moved.
    ///
    /// This is the splice at the end of attaching a query to a shared subtree. `to` must be an older
    /// node than every consumer being rewired, or index order would stop being a topological order and
    /// the schedule would stop being deterministic (I-2) — so that is checked rather than assumed.
    pub fn rewire_inputs(&mut self, from: NodeId, to: NodeId) -> Result<usize> {
        let offered = self.node_output_schema(to)?.clone();
        let held = self.node_output_schema(from)?.clone();
        if offered != held {
            return Err(CircuitError::SinkSchemaMismatch {
                held: held.to_string(),
                offered: offered.to_string(),
            });
        }
        let mut moved = 0usize;
        for index in 0..self.nodes.len() {
            let consumes = matches!(
                self.nodes.get(index),
                Some(Some(Node::Operator { inputs, .. })) if inputs.contains(&from)
            );
            if !consumes {
                continue;
            }
            if to.0 >= index {
                return Err(CircuitError::NodeOutOfOrder {
                    node: index,
                    input: to.0,
                });
            }
            if let Some(Some(Node::Operator { inputs, .. })) = self.nodes.get_mut(index) {
                for input in inputs.iter_mut() {
                    if *input == from {
                        *input = to;
                        moved += 1;
                    }
                }
            }
        }
        Ok(moved)
    }

    /// How many things read each live node: one per input edge from a live node, plus one per live
    /// sink.
    ///
    /// Computed from the dataflow itself. The memo maintains its own refcounts incrementally — which
    /// is what makes an off-by-one possible, and therefore what makes it testable — and compares them
    /// against this.
    #[must_use]
    pub fn reference_counts(&self) -> BTreeMap<NodeId, usize> {
        let mut counts: BTreeMap<NodeId, usize> = BTreeMap::new();
        for index in 0..self.nodes.len() {
            if matches!(self.nodes.get(index), Some(Some(_))) {
                counts.entry(NodeId(index)).or_insert(0);
            }
        }
        for node in self.nodes.iter().flatten() {
            if let Node::Operator { inputs, .. } = node {
                for input in inputs {
                    *counts.entry(*input).or_insert(0) += 1;
                }
            }
        }
        for sink in self.sinks.iter().flatten() {
            *counts.entry(sink.node).or_insert(0) += 1;
        }
        counts
    }

    /// Every node at or upstream of `node`, itself included.
    fn ancestors_of(&self, node: NodeId) -> Result<std::collections::BTreeSet<NodeId>> {
        let mut out = std::collections::BTreeSet::new();
        let mut stack = vec![node];
        while let Some(id) = stack.pop() {
            if !out.insert(id) {
                continue;
            }
            if let Node::Operator { inputs, .. } = self.node_at(id)? {
                stack.extend(inputs.iter().copied());
            }
        }
        Ok(out)
    }

    fn node_at(&self, id: NodeId) -> Result<&Node> {
        self.nodes
            .get(id.0)
            .and_then(Option::as_ref)
            .ok_or(CircuitError::UnknownNode(id.0))
    }

    /// The schema a node emits.
    pub fn node_output_schema(&self, id: NodeId) -> Result<&Schema> {
        match self.node_at(id)? {
            Node::Source { schema, .. } => Ok(schema),
            Node::Operator { op, .. } => Ok(op.output_schema()),
        }
    }

    /// What one node is holding, or `None` for a source (which holds nothing).
    ///
    /// The per-operator half of `EXPLAIN STATE` (C8). Everything here is a number the runtime already
    /// enforced — the entries the backend reports, the budget `check_state_declarations` compares
    /// against — so the report cannot drift from the accounting: they read the same fields.
    pub fn node_state(&self, id: NodeId) -> Result<Option<NodeState>> {
        match self.node_at(id)? {
            Node::Source { .. } => Ok(None),
            Node::Operator { op, inputs } => {
                let declared = op.state_bound();
                let budget = if matches!(declared, StateBound::Unbounded { .. })
                    && self.admitted_unbounded.contains(&id)
                {
                    None
                } else {
                    Some(self.state_budget(declared, inputs, op.name())?)
                };
                Ok(Some(NodeState {
                    operator: op.name(),
                    declared: declared.to_string(),
                    entries: op.state_size(),
                    budget,
                    backends: op.backend_count(),
                }))
            }
        }
    }

    /// The nodes whose `Unbounded` state declaration a registration admitted (I-9).
    ///
    /// Readable so that "the admission reached the runtime" is a fact a test can check, rather than a
    /// line of plumbing nobody looks at.
    #[must_use]
    pub fn admitted_unbounded(&self) -> &std::collections::BTreeSet<NodeId> {
        &self.admitted_unbounded
    }

    /// The highest node index plus one — the range `node_state` may be asked about.
    #[must_use]
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// How many nodes are live (holes excluded).
    #[must_use]
    pub fn live_nodes(&self) -> usize {
        self.nodes.iter().flatten().count()
    }

    /// How many sinks are live.
    #[must_use]
    pub fn live_sinks(&self) -> usize {
        self.sinks.iter().flatten().count()
    }

    /// Every live node's state, summed — the number the teardown gate returns to baseline.
    #[must_use]
    pub fn total_state_size(&self) -> usize {
        self.nodes
            .iter()
            .flatten()
            .map(|node| match node {
                Node::Source { .. } => 0,
                Node::Operator { op, .. } => op.state_size(),
            })
            .sum()
    }

    /// Bring a subset of nodes up to date by feeding them one delta, without stepping the rest.
    ///
    /// This is how a query registered after N epochs catches up (§5.7, C6). The subset must be
    /// **closed under inputs** — a complete little circuit of its own — because nothing outside it is
    /// stepped, so nothing outside it has an output to offer.
    ///
    /// **Why one delta carrying the whole accumulated input is the right answer, and not an
    /// approximation.** Every operator here holds state that is a function of the *accumulated* input
    /// it has been given, not of how that input was divided into epochs: a join's index, an
    /// aggregate's value multisets, a distinct's per-row weights. And every answer is the integral of
    /// the sink's output deltas, so feeding `Δ₁ + Δ₂ + … + Δₙ` at once integrates to what feeding them
    /// one at a time integrates to. That is I-2 restated — the state and every answer at epoch N is a
    /// pure function of the log prefix — and it is why a query can join a running dataflow at all.
    ///
    /// The epoch counter does not move: catching up is not sealing an epoch.
    pub fn catch_up(&mut self, deltas: &EpochDeltas, subset: &[NodeId]) -> Result<()> {
        self.pass(deltas, Some(subset))
    }

    /// Give the circuit its defined initial state, without advancing the epoch (S-33, D-20).
    ///
    /// Every answer in Current is accumulated from deltas and therefore starts empty — except one. A
    /// grand total has one group that exists whether or not any row does, so its row must be in the
    /// answer *before* any epoch is sealed. Priming runs the operator chain once with empty inputs,
    /// which is exactly the path a real epoch takes, and folds whatever falls out into the result
    /// store. The epoch counter does not move: nothing has been sealed.
    ///
    /// For every other circuit this is a no-op — empty inputs produce empty outputs — and the cost is
    /// one pass over a DAG at construction time.
    fn prime(&mut self) -> Result<()> {
        let empty = EpochDeltas::new();
        self.pass(&empty, None)?;
        Ok(())
    }

    /// Declare which epoch a **bootstrapped** circuit is as of (`docs/DURABILITY.md` B2).
    ///
    /// Bootstrap feeds the accumulated input of epochs `1..=E` as one delta, so the circuit has taken
    /// one step and is as of epoch `E`. Nothing else may use this: an epoch counter that any caller can
    /// set is an epoch counter that means nothing, so it refuses to move *backwards* — the direction a
    /// bug would take it — and it is documented as bootstrap's alone.
    pub fn set_epoch(&mut self, epoch: Epoch) -> Result<()> {
        if epoch < self.epoch {
            return Err(CircuitError::EpochWouldGoBackwards {
                held: self.epoch,
                offered: epoch,
            });
        }
        self.epoch = epoch;
        Ok(())
    }

    /// Seal one epoch: push its deltas through the circuit and fold each sink's output delta into its
    /// result store (S-6, I-3).
    ///
    /// Deltas for tables this circuit does not read are ignored — a circuit only sees the inputs
    /// it was wired to, and a scenario may well change tables no standing query touches. A delta
    /// for a table that exists nowhere is a different matter and is caught when the source is
    /// built, not here.
    pub fn step(&mut self, deltas: &EpochDeltas) -> Result<Epoch> {
        self.pass(deltas, None)?;

        // The epoch advances only now, after everything above succeeded. A step that fails leaves
        // the circuit on the previous epoch rather than half-way into a new one (I-3).
        self.epoch += 1;
        Ok(self.epoch)
    }

    /// One pass of the DAG: the shared body of [`Circuit::step`], [`Circuit::prime`] and
    /// [`Circuit::catch_up`].
    ///
    /// Priming and catching up are deliberately the *same* code as stepping. A separate
    /// initialisation path would be a second place for the answer to be computed, and the two could
    /// disagree. `subset` restricts which nodes are stepped and which sinks absorb; `None` means all
    /// of them.
    fn pass(&mut self, deltas: &EpochDeltas, subset: Option<&[NodeId]>) -> Result<()> {
        let mut outputs: Vec<Option<ZSetBatch>> = Vec::with_capacity(self.nodes.len());
        let mut errors: Vec<(NodeId, ZSetBatch)> = Vec::new();

        for index in 0..self.nodes.len() {
            let id = NodeId(index);
            let included = subset.is_none_or(|only| only.contains(&id));
            let produced = match (included, self.nodes.get(index)) {
                // A hole, or a node this pass does not touch: no output to offer. A consumer that
                // asked for one would be reading outside its own subset, which `catch_up` documents
                // as the caller's obligation and `UnknownNode` reports if it is broken.
                (false, _) | (_, Some(None)) | (_, None) => {
                    outputs.push(None);
                    continue;
                }
                (true, Some(Some(Node::Source { table, schema, .. }))) => {
                    source_batch(schema, deltas.entries_for(table))?
                }
                (true, Some(Some(Node::Operator { .. }))) => {
                    // Collect the inputs first so the borrow of `outputs` ends before the
                    // operator is borrowed mutably.
                    let input_ids = match self.nodes.get(index) {
                        Some(Some(Node::Operator { inputs, .. })) => inputs.clone(),
                        _ => return Err(CircuitError::UnknownNode(index)),
                    };
                    let mut inputs: Vec<&ZSetBatch> = Vec::with_capacity(input_ids.len());
                    for input in &input_ids {
                        let slot = outputs
                            .get(input.0)
                            .ok_or(CircuitError::UnknownNode(input.0))?
                            .as_ref()
                            .ok_or(CircuitError::UnknownNode(input.0))?;
                        inputs.push(slot);
                    }
                    match self.nodes.get_mut(index) {
                        Some(Some(Node::Operator { op, .. })) => {
                            let out = op.step(&inputs)?;
                            errors.push((id, out.errors));
                            out.data
                        }
                        _ => return Err(CircuitError::UnknownNode(index)),
                    }
                }
            };
            let emitted = produced.len();
            if let Some(slot) = self.emitted_entries.get_mut(index) {
                *slot = slot.saturating_add(emitted);
            }
            if let Some(slot) = self.steps.get_mut(index) {
                *slot = slot.saturating_add(1);
            }
            outputs.push(Some(produced));
        }

        // I-9: every operator declared what it would remember, so check it against what it holds.
        self.check_state_declarations()?;

        // Each sink folds in the delta of the node it reads. A sink whose node was not stepped this
        // pass — every sink but the new one, during a catch-up — folds in nothing.
        for index in 0..self.sinks.len() {
            let Some(Some(sink)) = self.sinks.get(index) else {
                continue;
            };
            let node = sink.node;
            let Some(Some(delta)) = outputs.get(node.0) else {
                continue;
            };
            let delta = delta.clone();
            if let Some(Some(sink)) = self.sinks.get_mut(index) {
                sink.result.absorb(&delta)?;
            }
        }

        // Every operator's error delta is folded into the live-error set of each sink downstream of
        // it (S-22b, I-8). Absorbed after the answers so that a pass which fails outright leaves no
        // store touched.
        for (node, delta) in &errors {
            for index in 0..self.sinks.len() {
                let belongs = matches!(self.sinks.get(index), Some(Some(sink)) if sink.ancestors.contains(node));
                if !belongs {
                    continue;
                }
                if let Some(Some(sink)) = self.sinks.get_mut(index) {
                    sink.errors.absorb(delta)?;
                }
            }
        }
        Ok(())
    }

    /// Account every operator's actual state against its declaration (I-9).
    ///
    /// > Every stateful operator declares its state bound as a function of its input (e.g., join
    /// > state is O(|A| + |B|)); the runtime accounts actual state against declarations, and an
    /// > operator exceeding its declaration is a bug, not a tuning problem.
    ///
    /// **The budget.** For `ProportionalToInputs`, the bound is the number of entries ever handed
    /// to the operator on those inputs, times the declared factor, plus the declared constant. That
    /// is a sound upper bound on "O(|A| + |B|)" as an operator can actually satisfy it: an index over
    /// a side's integral holds one entry per *distinct* row, and distinct rows can never outnumber the
    /// entries that delivered them. The factor covers operators that keep several entries per row
    /// for a stated reason — an aggregate keeps a value multiset per aggregate slot.
    ///
    /// **What it catches.** Anything whose state grows faster than its input. A join that stored
    /// the cross product would hold |A|·|B| entries against a budget of |A|+|B| and fail as soon as
    /// either side passes two rows. So would an operator that re-stored its whole input every
    /// epoch, or one that kept a tombstone per row it had ever seen.
    ///
    /// **What it does not catch.** A constant-factor overshoot — state of 2(|A|+|B|), say, from
    /// keeping a second copy of an index — sits inside the budget whenever retractions and
    /// multiplicities mean entries outnumber distinct rows. Tightening that needs the real
    /// per-operator input integrals, which is `EXPLAIN STATE` in C8; the honest position here is
    /// that this catches the wrong *complexity*, not every wasted byte.
    fn check_state_declarations(&self) -> Result<()> {
        for (index, node) in self.nodes.iter().enumerate() {
            let Some(Node::Operator { op, inputs }) = node else {
                continue;
            };
            let declared = op.state_bound();
            let actual = op.state_size();

            // An admitted `Unbounded` operator is exempt from the *budget* and from nothing else
            // (I-9): it was declared unbounded, the registration admitted it in writing, and there is
            // no bound left to check. Its size is still reported by the fingerprint, because
            // "admitted" means someone accepted the growth, not that nobody may see it.
            if matches!(declared, StateBound::Unbounded { .. })
                && self.admitted_unbounded.contains(&NodeId(index))
            {
                continue;
            }

            let budget = self.state_budget(declared, inputs, op.name())?;

            if actual > budget {
                return Err(CircuitError::StateBoundViolated {
                    op: op.name(),
                    declared: declared.to_string(),
                    actual,
                    budget,
                });
            }
        }
        Ok(())
    }

    /// The entries an operator is allowed to hold, given its declaration and what it has been fed.
    ///
    /// One function, used by both the check and the state fingerprint, so the number a reader sees
    /// is the number the runtime enforced. They were computed separately once, and the fingerprint
    /// quietly reported a budget without the declared factor — a discrepancy that made the printed
    /// accounting wrong while the check was right.
    fn state_budget(
        &self,
        declared: StateBound,
        inputs: &[NodeId],
        op: &'static str,
    ) -> Result<usize> {
        match declared {
            StateBound::Stateless => Ok(0),
            StateBound::ProportionalToInputs {
                factor, constant, ..
            } => {
                let mut total = 0usize;
                for input in inputs {
                    let emitted = self
                        .emitted_entries
                        .get(input.0)
                        .copied()
                        .ok_or(CircuitError::UnknownNode(input.0))?;
                    total = total.saturating_add(emitted);
                }
                Ok(total.saturating_mul(factor).saturating_add(constant))
            }
            // Refused at wiring time unless a registration admitted it (I-9). Reported rather than
            // silently allowed, so that an unadmitted one cannot slip through by another route.
            StateBound::Unbounded { reason } => {
                Err(CircuitError::UnboundedStateNotAdmissible { op, reason })
            }
        }
    }

    /// Serialise everything this circuit holds, for a checkpoint (`docs/DURABILITY.md` C1).
    ///
    /// **All of it**, and the list is worth reading because a recovery that restored some of it would
    /// pass every answer test and then misbehave later:
    ///
    /// - the epoch, so a recovered circuit knows where the log suffix starts;
    /// - every sink's result store — the answers themselves;
    /// - every sink's live-error store, or a recovered query would forget that it has no answer (S-22);
    /// - `emitted_entries`, which is I-9 accounting. Restoring the stores but not the counter would
    ///   leave every operator's state budget wrong, and the failure would look like a state-bound
    ///   violation rather than a lost counter;
    /// - each operator's own state, in node order.
    ///
    /// What it does **not** carry is the topology: a snapshot restores into a circuit of the same
    /// shape, which recovery guarantees by rebuilding from the same plan. A *memo* is therefore not
    /// checkpointable through this door, because its shape is the set of queries registered at the
    /// time — that is named as a gap in `docs/PROGRESS.md` rather than half-built here.
    pub fn snapshot(&self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.epoch.to_be_bytes());
        out.extend_from_slice(&(self.emitted_entries.len() as u32).to_be_bytes());
        for count in &self.emitted_entries {
            out.extend_from_slice(&(*count as u64).to_be_bytes());
        }
        out.extend_from_slice(&(self.sinks.len() as u32).to_be_bytes());
        for sink in &self.sinks {
            match sink {
                Some(sink) => {
                    push_block(&mut out, &sink.result.snapshot()?);
                    push_block(&mut out, &sink.errors.snapshot()?);
                }
                None => {
                    push_block(&mut out, &[]);
                    push_block(&mut out, &[]);
                }
            }
        }
        for node in &self.nodes {
            let block = match node {
                Some(Node::Source { .. }) | None => Vec::new(),
                Some(Node::Operator { op, .. }) => op.snapshot()?,
            };
            push_block(&mut out, &block);
        }
        Ok(out)
    }

    /// Restore a circuit from a snapshot. The circuit must have the same shape it had when the
    /// snapshot was taken — recovery rebuilds it from the same plan, so it does.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<()> {
        let mut epoch_raw = [0u8; 8];
        epoch_raw.copy_from_slice(bytes.get(0..8).ok_or(CircuitError::CorruptSnapshot)?);
        self.epoch = u64::from_be_bytes(epoch_raw);

        let mut count_raw = [0u8; 4];
        count_raw.copy_from_slice(bytes.get(8..12).ok_or(CircuitError::CorruptSnapshot)?);
        let counters = u32::from_be_bytes(count_raw) as usize;
        if counters != self.nodes.len() {
            return Err(CircuitError::CorruptSnapshot);
        }
        let mut at = 12usize;
        self.emitted_entries.clear();
        for _ in 0..counters {
            let mut raw = [0u8; 8];
            raw.copy_from_slice(bytes.get(at..at + 8).ok_or(CircuitError::CorruptSnapshot)?);
            self.emitted_entries.push(u64::from_be_bytes(raw) as usize);
            at += 8;
        }

        let mut sink_raw = [0u8; 4];
        sink_raw.copy_from_slice(bytes.get(at..at + 4).ok_or(CircuitError::CorruptSnapshot)?);
        let sink_count = u32::from_be_bytes(sink_raw) as usize;
        if sink_count != self.sinks.len() {
            return Err(CircuitError::CorruptSnapshot);
        }
        at += 4;
        for index in 0..sink_count {
            let (result_block, next) = take_block(bytes, at)?;
            let (error_block, next) = take_block(bytes, next)?;
            if let Some(Some(sink)) = self.sinks.get_mut(index) {
                sink.result.restore(result_block)?;
                sink.errors.restore(error_block)?;
            }
            at = next;
        }

        for index in 0..self.nodes.len() {
            let (block, next) = take_block(bytes, at)?;
            if let Some(Some(Node::Operator { op, .. })) = self.nodes.get_mut(index) {
                op.restore(block)?;
            }
            at = next;
        }
        Ok(())
    }

    /// Entries each node has ever emitted, indexed by node.
    ///
    /// I-6 is a counter gate: two circuits that should be the same are compared not only by their
    /// answers but by how much each node emitted. Counters catch divergence *before* answers diverge,
    /// which matters because a sharing bug or a mis-incrementalized operator can produce the right
    /// answer by a wrong route for a long time before it produces a wrong one.
    #[must_use]
    pub fn counters(&self) -> &[usize] {
        &self.emitted_entries
    }

    /// How many times each node has been stepped, indexed by node.
    #[must_use]
    pub fn step_counters(&self) -> &[usize] {
        &self.steps
    }

    /// A live node's stable operator label, for operator-facing accounting reports.
    #[must_use]
    pub fn node_label(&self, node: NodeId) -> Option<&'static str> {
        match self.nodes.get(node.index())? {
            Some(Node::Source { .. }) => Some("source"),
            Some(Node::Operator { op, .. }) => Some(op.name()),
            None => None,
        }
    }

    /// Total operator steps executed — **the I-8 counter**.
    ///
    /// Sharing means a common prefix is stepped once per epoch rather than once per query, so this
    /// number is strictly smaller with sharing on for any battery that actually overlaps. Sources are
    /// excluded: a source does no work beyond presenting its table's delta, and counting them would
    /// let "one fewer duplicated scan" stand in for "one fewer join executed".
    #[must_use]
    pub fn operator_steps(&self) -> usize {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, node)| matches!(node, Some(Node::Operator { .. })))
            .map(|(index, _)| self.steps.get(index).copied().unwrap_or(0))
            .sum()
    }

    /// A deterministic rendering of everything this circuit holds.
    ///
    /// This is what the I-2 gate compares between two runs of one scenario. Answers alone are not
    /// enough: two runs can agree on every answer while holding different state, and that
    /// difference becomes a wrong answer later — or, from C4, a recovery that does not match its
    /// uncrashed twin (I-7).
    pub fn state_fingerprint(&self) -> Result<String> {
        let mut out = format!("circuit @ epoch {}\n", self.epoch);
        for (index, node) in self.nodes.iter().enumerate() {
            match node {
                None => out.push_str(&format!("node {index} freed\n")),
                Some(Node::Source {
                    table,
                    alias,
                    schema,
                }) => {
                    out.push_str(&format!(
                        "node {index} source table={table} alias={alias} emitted={} schema={schema}\n",
                        self.emitted_entries.get(index).copied().unwrap_or(0)
                    ));
                }
                Some(Node::Operator { op, inputs }) => {
                    let wiring: Vec<String> =
                        inputs.iter().map(|i| format!("node {}", i.0)).collect();
                    let budget = match self.state_budget(op.state_bound(), inputs, op.name()) {
                        Ok(budget) => budget.to_string(),
                        // An admitted `Unbounded` operator has no budget to print, and saying so is
                        // more honest than printing a number nobody enforced.
                        Err(_) => "admitted-unbounded".to_owned(),
                    };
                    out.push_str(&format!(
                        "node {index} {} inputs=[{}] state_bound={} state_size={} budget={} emitted={} schema={}\n",
                        op.name(),
                        wiring.join(", "),
                        op.state_bound(),
                        op.state_size(),
                        budget,
                        self.emitted_entries.get(index).copied().unwrap_or(0),
                        op.output_schema()
                    ));
                    // An operator's own state, if it has any. This is what makes the I-2 gate a
                    // comparison of *state* and not only of answers: a join holding different
                    // indexes with the same answer must still register as different.
                    out.push_str(&op.render_state()?);
                }
            }
        }
        for (index, sink) in self.sinks.iter().enumerate() {
            match sink {
                None => out.push_str(&format!("sink {index} freed\n")),
                Some(sink) => {
                    out.push_str(&format!(
                        "sink node {} · result store holds {} row(s) · {} live error(s)\n",
                        sink.node.0,
                        sink.result.len(),
                        sink.errors.len()
                    ));
                    out.push_str(&sink.result.canonical()?.render());
                    if !sink.errors.is_empty() {
                        out.push_str("live errors:\n");
                        out.push_str(&sink.errors.canonical()?.render());
                    }
                }
            }
        }
        Ok(out)
    }
}

/// The wiring rules, applied by both doors: [`CircuitBuilder::add`] and [`Circuit::attach`].
fn check_wiring(
    op: &dyn Operator,
    inputs: &[NodeId],
    id: NodeId,
    admit_unbounded: bool,
) -> Result<()> {
    if op.arity() != inputs.len() {
        return Err(CircuitError::WiringArity {
            op: op.name(),
            expected: op.arity(),
            found: inputs.len(),
        });
    }
    // A state declaration that does not describe the operator cannot be checked, so it is
    // rejected here rather than accepted and quietly ignored later (I-9).
    match op.state_bound() {
        StateBound::ProportionalToInputs {
            inputs: declared, ..
        } if declared.len() != op.arity() => {
            return Err(CircuitError::StateDeclarationArityMismatch {
                op: op.name(),
                declared: declared.len(),
                arity: op.arity(),
            });
        }
        // I-9's admission: unbounded state is refused **by default**, and accepted only where a
        // registration said so explicitly. C2 deferred this to "when C6's registry can admit it";
        // this is that admission, and the default has not moved.
        StateBound::Unbounded { reason } if !admit_unbounded => {
            return Err(CircuitError::UnboundedStateNotAdmissible {
                op: op.name(),
                reason,
            });
        }
        StateBound::Stateless
        | StateBound::ProportionalToInputs { .. }
        | StateBound::Unbounded { .. } => {}
    }
    for input in inputs {
        if input.0 >= id.0 {
            return Err(CircuitError::NodeOutOfOrder {
                node: id.0,
                input: input.0,
            });
        }
    }
    Ok(())
}

fn push_block(out: &mut Vec<u8>, block: &[u8]) {
    out.extend_from_slice(&(block.len() as u32).to_be_bytes());
    out.extend_from_slice(block);
}

fn take_block(bytes: &[u8], at: usize) -> Result<(&[u8], usize)> {
    let mut raw = [0u8; 4];
    raw.copy_from_slice(bytes.get(at..at + 4).ok_or(CircuitError::CorruptSnapshot)?);
    let len = u32::from_be_bytes(raw) as usize;
    let block = bytes
        .get(at + 4..at + 4 + len)
        .ok_or(CircuitError::CorruptSnapshot)?;
    Ok((block, at + 4 + len))
}

/// Turn one table's raw entries into the batch its source node emits.
///
/// The rows are the table's rows; the schema is the query's `alias.column` naming. Rows are
/// positional, so this is a rename and nothing else — no reordering, no coercion. Validation
/// against the schema happens inside `ZSetBatch::from_entries`, so a row of the wrong shape is
/// refused here rather than misread downstream.
fn source_batch(schema: &Schema, entries: &[(Row, i64)]) -> Result<ZSetBatch> {
    Ok(ZSetBatch::from_entries(schema.clone(), entries.to_vec())?)
}
