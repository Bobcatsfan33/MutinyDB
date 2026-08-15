//! The standing-query registry: register, read, deregister (`ARCHITECTURE.md` §5.7, §6 C6).
//!
//! ```text
//!   register(plan) ─► canonicalize ─► instantiate the plan's nodes ─► catch up ─► attach & free
//!                                     (all of them, fresh)          (to now)     (the duplicates)
//! ```
//!
//! One [`Memo`] owns one dataflow and many queries. A query is a **sink** on that dataflow; the nodes
//! feeding it may be its own or shared with any number of other queries, and it cannot tell which.
//! That indistinguishability is I-8: *whether a subplan is shared or private may change counters and
//! cost, never a result byte.*
//!
//! ## Registration, in the order it happens, and why that order
//!
//! 1. **Canonicalize** ([`crate::canonical`]) and hash every subtree.
//! 2. **Instantiate every node fresh** — even the ones that will turn out to be duplicates.
//! 3. **Catch up**: feed the accumulated input to those fresh nodes alone, bringing the new query
//!    from nothing to the current epoch ([`schweep_circuit::Circuit::catch_up`]).
//! 4. **Attach**: walk the fresh nodes bottom-up; where a canonical hash already names a live node,
//!    rewire this registration's consumers onto it and free the duplicate.
//!
//! Steps 2 and 3 look wasteful, and they are: a query registering into a dataflow that already
//! computes most of it builds that part twice and throws one away. The alternative — resolve the
//! sharing first, then catch up only the novel suffix — cannot work, and the reason is worth writing
//! down because it is the crux of mid-history attach:
//!
//! > A novel suffix needs its **input's accumulated contents** to build its own state. Its input is a
//! > shared node, and a shared node emits *deltas*; it does not keep an integral of its output. Asking
//! > the shared prefix to replay would corrupt it — a join fed its own history a second time would
//! > double its index.
//!
//! So the new query gets a private copy of the prefix, is brought up to date through it, and only then
//! is spliced onto the shared one. What makes the splice sound is that the two copies are the *same
//! function of the same accumulated input*: identical operators, identical input, therefore identical
//! state (I-2). The private copy is then redundant, and freeing it is the last step rather than the
//! first.
//!
//! **The cost, stated once and plainly: registering a standing query costs one recomputation over the
//! accumulated input. Maintaining it costs O(change).** That is the trade the whole engine is built
//! on, and registration is the one place the O(data) side of it is paid.
//!
//! ## What this crate does not do
//!
//! No durability. A memo is in-memory: its shape is the set of queries registered right now, and
//! `Circuit::snapshot` deliberately does not carry a topology. Recovering a *registry* means
//! re-registering, which is correct but not free, and wiring it to the log is C9's job.
//!
//! No cross-query optimisation, no rewriting, no admission of anything the canonicalizer is not sure
//! about. See [`crate::canonical`] for the rule inventory and the sharing each omission costs.

use std::collections::BTreeMap;

use schweep_circuit::{Circuit, Epoch, NodeId, SinkId};
use schweep_plan::bind::Catalog;
use schweep_sql::{CircuitNode, CircuitPlan};
use schweep_zset::{Canonical, EpochDeltas, Row, Schema, ZSetBatch};

use crate::canonical::{canonicalize, subtree_hash};
use crate::error::{MemoError, Result};

/// A handle to a registered standing query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Handle(u64);

impl Handle {
    #[must_use]
    pub fn id(self) -> u64 {
        self.0
    }

    /// The handle a circuit reported on its own has: there is no registration behind it.
    ///
    /// `u64::MAX` rather than 0, so it can never collide with a real handle, and named rather than
    /// written as a literal wherever it is needed.
    #[must_use]
    pub const fn unregistered() -> Handle {
        Handle(u64::MAX)
    }
}

/// Whether the memo shares circuitry at all.
///
/// `Off` exists for the I-8 gate, which runs the same battery both ways and demands byte-identical
/// answers. It is a **switch inside one implementation** rather than a second implementation, so what
/// the gate compares is sharing itself and not two code paths that might differ for other reasons.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    On,
    Off,
}

/// What a registration is allowed to hold (I-9).
///
/// > Unbounded-by-nature constructs (e.g., aggregation over an unbounded key space) must be admitted
/// > explicitly at query registration. — I-9
///
/// C2's state checker refused `StateBound::Unbounded` outright, with a note that it would become
/// admissible "when C6's registry can admit it". This is that admission, and the default has not
/// moved: [`Admission::bounded`] is what a caller gets by saying nothing, and an unbounded operator
/// under it is refused by name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    unbounded_state: Option<String>,
}

impl Admission {
    /// The default: no unbounded state. An operator that declares it is refused.
    #[must_use]
    pub fn bounded() -> Admission {
        Admission {
            unbounded_state: None,
        }
    }

    /// Admit unbounded state, on the record.
    ///
    /// `reason` is stored in the registry and reported by [`Memo::registrations`], because an
    /// admission nobody can find later is indistinguishable from no admission at all — and I-9 is a
    /// rule about *declarations*, which means the declaration has to be readable.
    #[must_use]
    pub fn with_unbounded_state(reason: impl Into<String>) -> Admission {
        Admission {
            unbounded_state: Some(reason.into()),
        }
    }

    #[must_use]
    pub fn admits_unbounded(&self) -> bool {
        self.unbounded_state.is_some()
    }

    #[must_use]
    pub fn unbounded_reason(&self) -> Option<&str> {
        self.unbounded_state.as_deref()
    }
}

/// One registered query, and what the memo needs to take it away again.
#[derive(Clone, Debug)]
pub struct Registration {
    pub handle: Handle,
    pub sink: SinkId,
    /// The canonical plan. Kept because deregistration walks it, and because a reader asking "what is
    /// registered?" deserves the plan the memo actually built rather than the text it came from.
    pub plan: CircuitPlan,
    /// The dataflow node each plan node resolved to, deepest-first — shared nodes included.
    pub nodes: Vec<NodeId>,
    /// The nodes this registration was the first to need. Deregistering frees exactly these, and only
    /// those of them nothing else has picked up since.
    pub private: Vec<NodeId>,
    pub admission: Admission,
    /// The epoch this query was registered at. A query registered at epoch 7 answers as though it had
    /// been there since epoch 0 — that is what catch-up is for — and this records that it was not.
    pub registered_at: Epoch,
}

/// Where a registration's catch-up input comes from (C9).
///
/// Two shapes, one code path. A single accumulated delta is what C6's registry has always used and what a
/// memo with an input cache produces; a *stream* of per-epoch deltas is what a caller with the data on
/// disk can produce without holding the history in memory. Both end in the same `catch_up` pass over the
/// same nodes, so neither is a second implementation of catching up.
pub trait CatchUp {
    /// Hand each chunk to `apply`, in order.
    ///
    /// An implementation that yields nothing must still call `apply` once, with an empty delta: that pass
    /// is the *prime* a grand total's always-present row depends on (S-33, D-20).
    fn feed(self, apply: &mut dyn FnMut(&EpochDeltas) -> Result<()>) -> Result<()>;
}

impl CatchUp for &EpochDeltas {
    fn feed(self, apply: &mut dyn FnMut(&EpochDeltas) -> Result<()>) -> Result<()> {
        apply(self)
    }
}

/// Catch up from a stream of per-epoch deltas, holding one epoch at a time.
#[derive(Debug)]
pub struct Chunks<I>(pub I);

impl<I: IntoIterator<Item = EpochDeltas>> CatchUp for Chunks<I> {
    fn feed(self, apply: &mut dyn FnMut(&EpochDeltas) -> Result<()>) -> Result<()> {
        let mut fed = false;
        for chunk in self.0 {
            fed = true;
            apply(&chunk)?;
        }
        if !fed {
            apply(&EpochDeltas::new())?;
        }
        Ok(())
    }
}

/// The memo: one dataflow, many standing queries.
#[derive(Debug)]
pub struct Memo {
    dataflow: Circuit,
    sharing: Sharing,
    catalog: Catalog,
    /// Canonical subtree hash → the live node computing it. The sharing index.
    by_hash: BTreeMap<u64, NodeId>,
    /// Node → how many things read it: one per input edge from a live node, plus one per live sink.
    ///
    /// Maintained rather than recomputed, because a refcount that is derived cannot be wrong and
    /// therefore cannot be *tested*, and the leak gate's whole job is to test it. It is checked against
    /// a recomputation in [`Memo::audit`].
    refs: BTreeMap<NodeId, usize>,
    /// The accumulated input, per table — what a query registering mid-history is caught up with.
    ///
    /// `None` when the memo keeps **no** input cache: a caller that has the accumulated input elsewhere
    /// — a server with C7's snapshot and log suffix on disk — passes it to [`Memo::register_from`] and
    /// this memo holds none of the data. That is what makes a memo runnable under a memory ceiling its
    /// data exceeds, which C8 named as the one part of its claim a memo could not make (C9's discharge
    /// of that pointer).
    ///
    /// With a cache, this is the memory price of mid-history attach: the *data*, once, not per node.
    inputs: Option<BTreeMap<String, ZSetBatch>>,
    registrations: BTreeMap<Handle, Registration>,
    next_handle: u64,
    /// Where every operator's state comes from. One factory for the memo's whole life: a shared node's
    /// backend outlives the registration that created it, so whoever registers second must not be able
    /// to give the node it attached to a different store.
    factory: Box<dyn schweep_state::BackendFactory>,
}

impl Memo {
    /// A memo over a catalog, with sharing on.
    pub fn new(catalog: Catalog) -> Result<Memo> {
        Memo::with_sharing(catalog, Sharing::On)
    }

    pub fn with_sharing(catalog: Catalog, sharing: Sharing) -> Result<Memo> {
        Memo::with_backends(catalog, sharing, Box::new(schweep_state::MemFactory::new()))
    }

    /// A memo whose operators take their state from `factory` (C8).
    pub fn with_backends(
        catalog: Catalog,
        sharing: Sharing,
        factory: Box<dyn schweep_state::BackendFactory>,
    ) -> Result<Memo> {
        Memo::build(catalog, sharing, factory, true)
    }

    /// A memo that keeps **no** accumulated input in memory (C9).
    ///
    /// Catch-up for a late registration then has to come from the caller — [`Memo::register_from`] — and
    /// the caller is expected to source it from C7's snapshot plus the retained log suffix, on disk.
    /// [`Memo::register`] refuses on such a memo rather than silently registering a query that would
    /// answer only for epochs after it arrived, which is the failure this constructor exists to make
    /// impossible.
    ///
    /// This is what lets a memo run under a memory ceiling its *data* exceeds — the gap C8 named.
    pub fn without_input_cache(
        catalog: Catalog,
        sharing: Sharing,
        factory: Box<dyn schweep_state::BackendFactory>,
    ) -> Result<Memo> {
        Memo::build(catalog, sharing, factory, false)
    }

    fn build(
        catalog: Catalog,
        sharing: Sharing,
        factory: Box<dyn schweep_state::BackendFactory>,
        cache_inputs: bool,
    ) -> Result<Memo> {
        Ok(Memo {
            dataflow: Circuit::empty()?,
            sharing,
            catalog,
            by_hash: BTreeMap::new(),
            refs: BTreeMap::new(),
            inputs: if cache_inputs {
                Some(BTreeMap::new())
            } else {
                None
            },
            registrations: BTreeMap::new(),
            next_handle: 0,
            factory,
        })
    }

    /// Whether this memo keeps the accumulated input in memory.
    #[must_use]
    pub fn caches_inputs(&self) -> bool {
        self.inputs.is_some()
    }

    /// What the operators' state is kept in — for `EXPLAIN STATE`'s header and its reconciliation.
    #[must_use]
    pub fn backends(&self) -> &dyn schweep_state::BackendFactory {
        self.factory.as_ref()
    }

    #[must_use]
    pub fn sharing(&self) -> Sharing {
        self.sharing
    }

    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.dataflow.epoch()
    }

    /// Declare the epoch represented by a bootstrapped snapshot before replaying its suffix.
    ///
    /// This is deliberately the same narrow operation as [`Circuit::set_epoch`]. It does not apply
    /// data and it cannot move backwards; C10 exposes it here because the server owns the snapshot
    /// while the memo owns the circuit clock. Without this seam a compacted server can recover the
    /// right answer under the wrong epoch number, violating I-3.
    pub fn set_epoch(&mut self, epoch: Epoch) -> Result<()> {
        Ok(self.dataflow.set_epoch(epoch)?)
    }

    #[must_use]
    pub fn dataflow(&self) -> &Circuit {
        &self.dataflow
    }

    /// Every live registration, by handle.
    #[must_use]
    pub fn registrations(&self) -> &BTreeMap<Handle, Registration> {
        &self.registrations
    }

    /// Register SQL text as a standing query, with the default (bounded) admission.
    pub fn register_sql(&mut self, sql: &str) -> Result<Handle> {
        let plan = schweep_sql::compile(sql, &self.catalog)?;
        self.register(&plan, Admission::bounded())
    }

    /// Register a plan as a standing query, catching it up from the memo's own input cache.
    pub fn register(&mut self, plan: &CircuitPlan, admission: Admission) -> Result<Handle> {
        if self.inputs.is_none() {
            return Err(MemoError::NoInputCache);
        }
        let catch_up = self.accumulated_deltas()?;
        self.register_from(plan, admission, &catch_up)
    }

    /// Register a plan, catching it up from an input the **caller** supplies (C9).
    ///
    /// The caller owns the accumulated input, which is what lets it come from C7's snapshot and the
    /// retained log suffix rather than from this memo's memory. Passing an input that is *not* the
    /// accumulated history would register a query that answers something no oracle agrees with, so the
    /// obligation is the caller's and it is stated: **`catch_up` must be the accumulated contents of
    /// every table as of the memo's current epoch.**
    pub fn register_from(
        &mut self,
        plan: &CircuitPlan,
        admission: Admission,
        catch_up: &EpochDeltas,
    ) -> Result<Handle> {
        self.register_catching_up(plan, admission, catch_up)
    }

    /// Register a plan, catching it up from a **stream** of per-epoch deltas (C9).
    ///
    /// The difference from [`Memo::register_from`] is memory, and it is the difference C8's forward
    /// pointer was about: one accumulated delta is O(history) resident, while a chunk per epoch is
    /// O(largest epoch). A late registration can therefore catch up over more input than the process is
    /// allowed to hold, which is what `testing/soak/tests/c9_memo_ceiling.rs` measures.
    ///
    /// **The chunks must be the epochs, in order.** Each chunk is applied by the same pass the live path
    /// takes, so N chunks reach the state N sealed epochs would — which is not merely equivalent to one
    /// accumulated delta but *identical to the live path*, emission counters included. An out-of-order or
    /// overlapping chunk sequence would build a state no history explains, and no oracle would agree
    /// with it.
    ///
    /// An empty stream still primes (S-33, D-20): a grand total's always-present row exists before any
    /// epoch is sealed, and the prime is the pass that puts it there.
    pub fn register_from_chunks(
        &mut self,
        plan: &CircuitPlan,
        admission: Admission,
        chunks: impl IntoIterator<Item = EpochDeltas>,
    ) -> Result<Handle> {
        self.register_catching_up(plan, admission, Chunks(chunks))
    }

    fn register_catching_up(
        &mut self,
        plan: &CircuitPlan,
        admission: Admission,
        catch_up: impl CatchUp,
    ) -> Result<Handle> {
        let canonical = canonicalize(plan);

        // ---- 1. instantiate every node of the plan, fresh -------------------------------------
        //
        // Deepest-first, so a node's inputs exist before it does — the same order the circuit's
        // index-is-topological rule requires (I-2).
        let plan_nodes = canonical.root.nodes();
        let mut fresh: Vec<NodeId> = Vec::with_capacity(plan_nodes.len());
        let mut hashes: Vec<u64> = Vec::with_capacity(plan_nodes.len());
        for node in &plan_nodes {
            let inputs = self.resolve_inputs(node, &plan_nodes, &fresh)?;
            let id = self.attach_fresh(node, &inputs, admission.admits_unbounded())?;
            fresh.push(id);
            hashes.push(subtree_hash(node));
        }
        let root = *fresh.last().ok_or(MemoError::EmptyPlan)?;
        let sink = self.dataflow.add_sink(root)?;
        self.bump(root);

        // ---- 2. catch up ----------------------------------------------------------------------
        //
        // Always, not only when the epoch is past zero: at epoch 0 the accumulated input is empty and
        // this pass is the *prime* that gives a grand total its always-present row (S-33, D-20).
        let dataflow = &mut self.dataflow;
        let outcome = catch_up.feed(&mut |deltas| {
            dataflow
                .catch_up(deltas, &fresh)
                .map_err(MemoError::Circuit)
        });
        if let Err(error) = outcome {
            // A failed registration must leave nothing behind. Everything it built is private by
            // definition — nothing else has had a chance to reference it yet.
            self.abandon(sink, &fresh);
            return Err(error);
        }

        // ---- 3. attach to what already exists, free the duplicates ----------------------------
        let mut resolved: Vec<NodeId> = fresh.clone();
        let mut private: Vec<NodeId> = Vec::new();
        if self.sharing == Sharing::On {
            for index in 0..fresh.len() {
                let (Some(id), Some(hash)) =
                    (fresh.get(index).copied(), hashes.get(index).copied())
                else {
                    return Err(MemoError::EmptyPlan);
                };
                match self.by_hash.get(&hash).copied() {
                    // Something already computes this subtree: point this registration's consumers at
                    // it and free the copy just built.
                    Some(existing) if existing != id => {
                        self.rewire_within(&mut resolved, index, id, existing, sink)?;
                        self.free_duplicate(id)?;
                    }
                    Some(_) => private.push(id),
                    None => {
                        self.by_hash.insert(hash, id);
                        private.push(id);
                    }
                }
            }
            // The sink may now read a shared node, so its ancestor set — which decides whose
            // evaluation errors are this query's errors (S-22, I-8) — has to be recomputed.
            self.dataflow.refresh_ancestors(sink)?;
        } else {
            private = fresh.clone();
        }

        let handle = Handle(self.next_handle);
        self.next_handle += 1;
        self.registrations.insert(
            handle,
            Registration {
                handle,
                sink,
                plan: canonical,
                nodes: resolved,
                private,
                admission,
                registered_at: self.dataflow.epoch(),
            },
        );
        Ok(handle)
    }

    /// The answer to one registered query, as of the latest sealed epoch (I-3).
    ///
    /// Returns the epoch alongside the answer, because a reader that does not know *which* epoch it
    /// saw cannot honour I-3 when it reads a second query. There is no way to ask for an *older*
    /// epoch: the memo keeps one integral per query, not a history of them, and pretending otherwise
    /// would need MVCC that v1 does not have.
    pub fn read(&self, handle: Handle) -> Result<(Epoch, Canonical)> {
        let registration = self.registration(handle)?;
        let answer = self.dataflow.answer_of(registration.sink)?;
        Ok((self.dataflow.epoch(), answer))
    }

    /// Deregister a query: drop its sink, then free exactly the nodes nothing else reads.
    ///
    /// "Exactly" is the claim §6 C6's teardown gate checks: a node another query picked up survives, a
    /// node only this query ever needed is freed, and the state accounting returns to what it was
    /// before the registration.
    pub fn deregister(&mut self, handle: Handle) -> Result<()> {
        let registration = self
            .registrations
            .remove(&handle)
            .ok_or(MemoError::UnknownHandle(handle.id()))?;

        self.dataflow.remove_sink(registration.sink)?;
        let root = *registration.nodes.last().ok_or(MemoError::EmptyPlan)?;
        self.drop_ref(root)?;
        Ok(())
    }

    /// Seal one epoch across every registered query (S-6, I-3).
    ///
    /// The input is accumulated first, so a query registering later can be caught up to here; then the
    /// dataflow takes one pass and every sink folds in its own delta.
    pub fn seal_epoch(&mut self, deltas: &EpochDeltas) -> Result<Epoch> {
        if self.inputs.is_some() {
            for (table, entries) in deltas.tables() {
                let schema = self.table_schema(table)?;
                let batch = ZSetBatch::from_entries(schema, entries.clone())?;
                let held = self
                    .inputs
                    .as_ref()
                    .and_then(|cache| cache.get(table))
                    .cloned();
                let merged = match held {
                    Some(held) => held.add(&batch)?.consolidate()?,
                    None => batch.consolidate()?,
                };
                if let Some(cache) = self.inputs.as_mut() {
                    cache.insert(table.clone(), merged);
                }
            }
        }
        Ok(self.dataflow.step(deltas)?)
    }

    // ---- accounting, for the gates -------------------------------------------------------------

    /// Live nodes, live sinks, total operator state, and total operator steps.
    ///
    /// The teardown gate compares this to a baseline. Every field is a *number the runtime holds*
    /// rather than a number the memo believes: nodes and state come from the dataflow, not from the
    /// memo's own bookkeeping, so a leak the memo does not know about still shows up.
    #[must_use]
    pub fn accounting(&self) -> Accounting {
        Accounting {
            live_nodes: self.dataflow.live_nodes(),
            live_sinks: self.dataflow.live_sinks(),
            state_entries: self.dataflow.total_state_size(),
            shared_subtrees: self.by_hash.len(),
            tracked_refs: self.refs.values().filter(|count| **count > 0).count(),
            operator_steps: self.dataflow.operator_steps(),
            registrations: self.registrations.len(),
        }
    }

    /// Check the memo's own bookkeeping against the dataflow it describes.
    ///
    /// The refcounts are maintained incrementally, which means they can be wrong; this recomputes them
    /// from the wiring and the sinks and says so if they disagree. It is what turns "the leak gate
    /// passed" into "the leak gate checked".
    pub fn audit(&self) -> Result<()> {
        let truth = self.dataflow.reference_counts();
        for (node, expected) in &truth {
            let held = self.refs.get(node).copied().unwrap_or(0);
            if held != *expected {
                return Err(MemoError::RefcountDisagrees {
                    node: node.index(),
                    held,
                    actual: *expected,
                });
            }
        }
        for (node, held) in &self.refs {
            if *held > 0 && !truth.contains_key(node) {
                return Err(MemoError::RefcountDisagrees {
                    node: node.index(),
                    held: *held,
                    actual: 0,
                });
            }
        }
        // A hash index entry pointing at a freed node would hand the next registration a dead node.
        for (hash, node) in &self.by_hash {
            if self.dataflow.node_output_schema(*node).is_err() {
                return Err(MemoError::StaleShareIndex {
                    hash: *hash,
                    node: node.index(),
                });
            }
        }
        Ok(())
    }

    // ---- internals -----------------------------------------------------------------------------

    fn registration(&self, handle: Handle) -> Result<&Registration> {
        self.registrations
            .get(&handle)
            .ok_or(MemoError::UnknownHandle(handle.id()))
    }

    /// The dataflow nodes a plan node's children resolved to.
    fn resolve_inputs(
        &self,
        node: &CircuitNode,
        plan_nodes: &[&CircuitNode],
        fresh: &[NodeId],
    ) -> Result<Vec<NodeId>> {
        let mut inputs = Vec::with_capacity(2);
        for child in schweep_sql::children(node) {
            // `nodes()` is deepest-last and every child appears before its parent, so a child's
            // position in that list is the position of its freshly built node. Matching by *identity*
            // — the same `&CircuitNode` — rather than by equality, so two structurally equal siblings
            // cannot be confused for one another.
            let position = plan_nodes
                .iter()
                .position(|candidate| std::ptr::eq(*candidate, child))
                .ok_or(MemoError::PlanNodeNotFound)?;
            inputs.push(*fresh.get(position).ok_or(MemoError::PlanNodeNotFound)?);
        }
        Ok(inputs)
    }

    /// Build one plan node as a new dataflow node.
    fn attach_fresh(
        &mut self,
        node: &CircuitNode,
        inputs: &[NodeId],
        admit_unbounded: bool,
    ) -> Result<NodeId> {
        let id = match node {
            CircuitNode::Source {
                table,
                alias,
                schema,
            } => self
                .dataflow
                .attach_source(table.clone(), alias.clone(), schema.clone())?,
            _ => {
                // The label the backend is handed out under: the dataflow node it belongs to. Node ids
                // are stable across removals (C6), so a label is never reused for a different operator.
                let label = format!("n{}-{}", self.dataflow.live_nodes(), node_kind(node));
                let op = schweep_sql::operator_for_with(node, &label, self.factory.as_mut())?
                    .ok_or(MemoError::PlanNodeNotFound)?;
                self.dataflow.attach(op, inputs.to_vec(), admit_unbounded)?
            }
        };
        for input in inputs {
            self.bump(*input);
        }
        self.refs.entry(id).or_insert(0);
        Ok(id)
    }

    /// Move this registration's edges off `from` and onto `to`.
    ///
    /// Only *this* registration's nodes are rewired: a node built by an earlier registration is never
    /// touched, which is what makes attaching a new query invisible to the queries already running.
    fn rewire_within(
        &mut self,
        resolved: &mut [NodeId],
        index: usize,
        from: NodeId,
        to: NodeId,
        sink: SinkId,
    ) -> Result<()> {
        for slot in resolved.iter_mut() {
            if *slot == from {
                *slot = to;
            }
        }
        let moved = self.dataflow.rewire_inputs(from, to)?;
        for _ in 0..moved {
            self.drop_ref_shallow(from)?;
            self.bump(to);
        }
        // If the duplicate was the root, the sink reads it and has to move too.
        if self.dataflow.sink_node(sink)? == from {
            self.dataflow.repoint_sink(sink, to)?;
            self.drop_ref_shallow(from)?;
            self.bump(to);
        }
        let _ = index;
        Ok(())
    }

    /// Free a node this registration built and then discovered it did not need.
    ///
    /// It has no consumers left — `rewire_within` just moved them — and it is not in the share index,
    /// because the index already named the node it duplicated.
    fn free_duplicate(&mut self, id: NodeId) -> Result<()> {
        let held = self.refs.get(&id).copied().unwrap_or(0);
        if held != 0 {
            return Err(MemoError::RefcountDisagrees {
                node: id.index(),
                held,
                actual: 0,
            });
        }
        self.free(id)
    }

    /// Drop one reference to a node, freeing it — and, transitively, its inputs — at zero.
    fn drop_ref(&mut self, id: NodeId) -> Result<()> {
        let held = self.refs.get(&id).copied().unwrap_or(0);
        let left = held.saturating_sub(1);
        self.refs.insert(id, left);
        if left == 0 {
            self.free(id)?;
        }
        Ok(())
    }

    /// Drop one reference without cascading — used while rewiring, where the node is about to be
    /// freed explicitly and its inputs must not be released twice.
    fn drop_ref_shallow(&mut self, id: NodeId) -> Result<()> {
        let held = self.refs.get(&id).copied().unwrap_or(0);
        self.refs.insert(id, held.saturating_sub(1));
        Ok(())
    }

    /// Remove a node from the dataflow and release its inputs.
    fn free(&mut self, id: NodeId) -> Result<()> {
        let inputs = self.dataflow.inputs_of(id)?;
        self.dataflow.remove(id)?;
        self.refs.remove(&id);
        self.by_hash.retain(|_, node| *node != id);
        for input in inputs {
            self.drop_ref(input)?;
        }
        Ok(())
    }

    fn bump(&mut self, id: NodeId) {
        *self.refs.entry(id).or_insert(0) += 1;
    }

    /// Undo a registration that failed part-way: drop the sink and every node it built.
    ///
    /// Nothing here can be shared — the failure happened before the attach step — so the order is
    /// simply "roots first", which is what reversing the deepest-last list gives.
    fn abandon(&mut self, sink: SinkId, fresh: &[NodeId]) {
        let _ = self.dataflow.remove_sink(sink);
        for id in fresh.iter().rev() {
            let _ = self.dataflow.remove(*id);
            self.refs.remove(id);
            self.by_hash.retain(|_, node| node != id);
        }
    }

    /// The whole accumulated input, as one delta — from the cache, when there is one.
    fn accumulated_deltas(&self) -> Result<EpochDeltas> {
        let mut deltas = EpochDeltas::new();
        let cache = self.inputs.as_ref().ok_or(MemoError::NoInputCache)?;
        for (table, batch) in cache {
            let entries: Vec<(Row, i64)> = batch.canonical()?.entries().to_vec();
            deltas.extend(table.clone(), entries);
        }
        Ok(deltas)
    }

    fn table_schema(&self, table: &str) -> Result<Schema> {
        self.catalog
            .get(table)
            .cloned()
            .ok_or_else(|| MemoError::UnknownTable(table.to_owned()))
    }
}

/// The word that goes in a backend label.
fn node_kind(node: &CircuitNode) -> &'static str {
    match node {
        CircuitNode::Source { .. } => "source",
        CircuitNode::Filter { .. } => "filter",
        CircuitNode::Project { .. } => "project",
        CircuitNode::Join { .. } => "join",
        CircuitNode::Aggregate { .. } => "aggregate",
        CircuitNode::Distinct { .. } => "distinct",
    }
}

/// What the memo is holding — the numbers the teardown and leak gates compare.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Accounting {
    pub live_nodes: usize,
    pub live_sinks: usize,
    pub state_entries: usize,
    pub shared_subtrees: usize,
    pub tracked_refs: usize,
    pub operator_steps: usize,
    pub registrations: usize,
}

impl Accounting {
    /// Everything except the two counters that are *histories* rather than holdings.
    ///
    /// `operator_steps` only ever grows — it is the I-8 work ledger — so a teardown comparison that
    /// included it could never pass. Separating the two is the difference between "nothing is held"
    /// and "nothing has happened".
    #[must_use]
    pub fn holdings(&self) -> (usize, usize, usize, usize, usize, usize) {
        (
            self.live_nodes,
            self.live_sinks,
            self.state_entries,
            self.shared_subtrees,
            self.tracked_refs,
            self.registrations,
        )
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )]

    use super::*;
    use schweep_zset::{DataType, Field, Value};

    fn catalog() -> Catalog {
        let t = Schema::new(vec![
            Field::new("id", DataType::Int64, false),
            Field::new("k", DataType::Int64, true),
            Field::new("n", DataType::Int64, true),
        ])
        .unwrap();
        Catalog::from([("t".to_owned(), t)])
    }

    fn row(id: i64, k: i64, n: i64) -> Row {
        Row::new(vec![Value::Int(id), Value::Int(k), Value::Int(n)])
    }

    fn epoch(entries: Vec<(Row, i64)>) -> EpochDeltas {
        let mut deltas = EpochDeltas::new();
        deltas.extend("t", entries);
        deltas
    }

    fn answer(memo: &Memo, handle: Handle) -> String {
        memo.read(handle).unwrap().1.render()
    }

    #[test]
    fn a_query_registers_reads_and_deregisters() {
        let mut memo = Memo::new(catalog()).unwrap();
        let handle = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();

        memo.seal_epoch(&epoch(vec![(row(1, 2, 10), 1), (row(2, 0, 20), 1)]))
            .unwrap();
        assert_eq!(memo.epoch(), 1);
        assert_eq!(answer(&memo, handle), "(n: Int64)\n(10) => 1\n");

        memo.deregister(handle).unwrap();
        assert!(
            memo.read(handle).is_err(),
            "a deregistered query has no answer"
        );
        memo.audit().unwrap();
    }

    /// Two queries with a common prefix share it: one filter node, not two.
    #[test]
    fn an_overlapping_query_attaches_to_the_existing_subtree() {
        let mut memo = Memo::new(catalog()).unwrap();
        let a = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        let after_first = memo.accounting();
        let b = memo
            .register_sql("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        let after_second = memo.accounting();

        assert_eq!(
            after_second.live_nodes - after_first.live_nodes,
            1,
            "only the DISTINCT is novel: source, filter and projection were already there"
        );
        memo.audit().unwrap();

        memo.seal_epoch(&epoch(vec![(row(1, 2, 10), 2), (row(2, 0, 20), 1)]))
            .unwrap();
        assert_eq!(answer(&memo, a), "(n: Int64)\n(10) => 2\n");
        assert_eq!(
            answer(&memo, b),
            "(n: Int64)\n(10) => 1\n",
            "DISTINCT keeps one copy (S-34), and it reads the shared prefix"
        );
    }

    /// With sharing off, the same two queries build everything twice — and answer the same.
    #[test]
    fn sharing_off_builds_twice_and_answers_the_same() {
        let mut shared = Memo::with_sharing(catalog(), Sharing::On).unwrap();
        let mut private = Memo::with_sharing(catalog(), Sharing::Off).unwrap();
        let mut handles = Vec::new();
        for memo in [&mut shared, &mut private] {
            let a = memo
                .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
                .unwrap();
            let b = memo
                .register_sql("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1")
                .unwrap();
            handles.push((a, b));
        }
        assert!(
            private.accounting().live_nodes > shared.accounting().live_nodes,
            "sharing off must build more nodes: {:?} vs {:?}",
            private.accounting(),
            shared.accounting()
        );

        let deltas = epoch(vec![(row(1, 2, 10), 2), (row(2, 0, 20), 1)]);
        shared.seal_epoch(&deltas).unwrap();
        private.seal_epoch(&deltas).unwrap();
        for index in 0..2 {
            let (sa, sb) = handles[0];
            let (pa, pb) = handles[1];
            let (s, p) = if index == 0 { (sa, pa) } else { (sb, pb) };
            assert_eq!(
                answer(&shared, s),
                answer(&private, p),
                "I-8: sharing may change counters, never a result byte"
            );
        }
        assert!(
            shared.accounting().operator_steps < private.accounting().operator_steps,
            "and it must change the counters"
        );
    }

    /// Deregistering one of two sharing queries frees its private suffix and nothing else.
    #[test]
    fn deregistering_frees_exactly_the_private_suffix() {
        let mut memo = Memo::new(catalog()).unwrap();
        let a = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        let baseline = memo.accounting();
        let b = memo
            .register_sql("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        memo.deregister(b).unwrap();

        assert_eq!(
            memo.accounting().holdings(),
            baseline.holdings(),
            "the shared prefix stayed and the private DISTINCT went"
        );
        memo.audit().unwrap();

        memo.seal_epoch(&epoch(vec![(row(1, 2, 10), 1)])).unwrap();
        assert_eq!(
            answer(&memo, a),
            "(n: Int64)\n(10) => 1\n",
            "the query that stayed is unharmed"
        );
    }

    /// A query registered after N epochs answers as though it had always been there.
    #[test]
    fn a_query_registered_mid_history_catches_up() {
        let mut memo = Memo::new(catalog()).unwrap();
        let early = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        memo.seal_epoch(&epoch(vec![(row(1, 2, 10), 1), (row(2, 5, 20), 1)]))
            .unwrap();
        memo.seal_epoch(&epoch(vec![(row(2, 5, 20), -1), (row(3, 9, 30), 3)]))
            .unwrap();

        let late = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        assert_eq!(
            answer(&memo, late),
            answer(&memo, early),
            "the same query registered two epochs later holds the same answer"
        );
        assert_eq!(memo.registrations().get(&late).unwrap().registered_at, 2);
        memo.audit().unwrap();

        // And it keeps up from here.
        memo.seal_epoch(&epoch(vec![(row(4, 7, 40), 1)])).unwrap();
        assert_eq!(answer(&memo, late), answer(&memo, early));
    }

    /// **I-9's admission, recorded.** The default refuses; an explicit admission is stored *and*
    /// reaches the runtime.
    #[test]
    fn unbounded_state_is_admitted_per_registration_and_recorded() {
        let mut memo = Memo::new(catalog()).unwrap();

        let default_handle = memo.register_sql("SELECT t.n AS n FROM t").unwrap();
        let default_admission = &memo.registrations().get(&default_handle).unwrap().admission;
        assert!(
            !default_admission.admits_unbounded(),
            "saying nothing admits nothing (I-9)"
        );
        assert_eq!(default_admission.unbounded_reason(), None);

        let plan = schweep_sql::compile(
            "SELECT t.k AS k, COUNT(*) AS c FROM t GROUP BY t.k",
            &catalog(),
        )
        .unwrap();
        let admitted = memo
            .register(
                &plan,
                Admission::with_unbounded_state("k is a user-supplied key space"),
            )
            .unwrap();
        let registration = memo.registrations().get(&admitted).unwrap();
        assert_eq!(
            registration.admission.unbounded_reason(),
            Some("k is a user-supplied key space"),
            "the reason is in the registry, where someone can find it"
        );

        // The seam: the admission reached the runtime for every operator this registration built.
        let admitted_nodes = memo.dataflow().admitted_unbounded();
        let operators: Vec<NodeId> = registration
            .private
            .iter()
            .copied()
            .filter(|id| {
                !memo
                    .dataflow()
                    .inputs_of(*id)
                    .unwrap_or_default()
                    .is_empty()
            })
            .collect();
        assert!(!operators.is_empty());
        for id in operators {
            assert!(
                admitted_nodes.contains(&id),
                "node {id:?} was built under an admission that did not reach the circuit"
            );
        }
    }

    /// 1,000 register/deregister cycles leak nothing — asserted by accounting, not by not panicking.
    #[test]
    fn a_thousand_register_deregister_cycles_leak_nothing() {
        let mut memo = Memo::new(catalog()).unwrap();
        let resident = memo
            .register_sql("SELECT t.n AS n FROM t WHERE t.k > 1")
            .unwrap();
        memo.seal_epoch(&epoch(vec![(row(1, 2, 10), 1)])).unwrap();
        let baseline = memo.accounting();

        for round in 0..1_000 {
            let handle = memo
                .register_sql("SELECT DISTINCT t.n AS n FROM t WHERE t.k > 1")
                .unwrap();
            memo.deregister(handle).unwrap();
            if round % 250 == 0 {
                memo.audit().unwrap();
            }
        }

        assert_eq!(
            memo.accounting().holdings(),
            baseline.holdings(),
            "1,000 cycles must return every holding to baseline"
        );
        memo.audit().unwrap();
        assert_eq!(answer(&memo, resident), "(n: Int64)\n(10) => 1\n");
    }
}
