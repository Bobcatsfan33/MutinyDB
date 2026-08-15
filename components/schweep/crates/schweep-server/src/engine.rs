//! The embedded engine `schweepd` drives: log, checkpoints, memo, subscriptions (§6 C9).
//!
//! Everything durable is already built — the log (C4), the checkpoint protocol (C4), the snapshot and
//! compaction (C7), the memo (C6) — so this file is a *composition*, and the interesting part is what it
//! composes and in which order. It adds no engine behaviour and no nondeterminism (D-6, D-23).
//!
//! ```text
//!   POST /ingest ──► admission ──► log.append (A1–A8) ──► pending
//!   POST /seal   ──► log.seal_epoch (S1–S4) ──► memo.seal_epoch (S3) ──► subscriptions advance
//!                                                    │
//!   POST /register ─► compile ─► catch up from SNAPSHOT + LOG SUFFIX ─► registry file (D-22)
//!   GET  /subscribe?from=T ─► every sealed epoch after T, plus the next token (D-23)
//! ```
//!
//! ## Catch-up comes from disk, and that is C8's gap closed
//!
//! C8 named the one part of its claim a memo could not make: a `Memo` kept the accumulated input in
//! memory for mid-history catch-up, so its footprint tracked the *data* rather than the state. Here the
//! memo is built with [`Memo::without_input_cache`] and every registration is caught up **one epoch at a
//! time** out of the log on disk (`Memo::register_from_chunks`). Neither the memo nor this file ever holds
//! the accumulated history, which is what lets a late registration catch up over more input than the
//! process is allowed to keep resident — measured by `testing/soak/tests/c9_memo_ceiling.rs`.
//!
//! ## Subscriptions hold no server-side cursor
//!
//! Per-epoch deltas are kept in a bounded ring, and the client's token is the only cursor (D-23). A
//! subscriber that crashes and resumes at its token sees exactly the epochs it has not consumed, because
//! the server never recorded where it had got to and therefore cannot get that record wrong.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};

use schweep_circuit::{checkpoint, Epoch};
use schweep_log::{Ack, FaultInjector, Log, SyncPolicy};
use schweep_memo::{Admission, Handle, Memo, Sharing};
use schweep_plan::bind::Catalog;
use schweep_state::RedbFactory;
use schweep_zset::{EpochDeltas, Row};

use crate::admission::{Admission as Gate, Policy, Verdict};
use crate::error::{ServerError, ServerResult};
use crate::registry_file::{Entry, Registry};

/// How many sealed epochs' deltas one query keeps for subscribers.
///
/// **A tuned constant, in the ledger** (`SUBSCRIPTION_RING`, `testing/evidence/c9-bounds.json`). It is
/// part of what makes a subscription *not* a memory leak: a subscriber that stops consuming cannot make
/// the server grow, because the ring drops its oldest epoch rather than the server dropping its
/// guarantees. A token behind the ring is refused (D-23's rule for a gap), never served a re-baseline
/// pretending to be a delta.
///
/// Only *part*, because a count bounds epochs and not bytes — see [`SUBSCRIPTION_RING_BYTES`].
pub const SUBSCRIPTION_RING: usize = 256;

/// How many **bytes** of retained deltas one query keeps.
///
/// The same discovery as [`crate::DEFAULT_SOURCE_QUEUE_BYTES`], on the read side: a delta is as large as
/// the change it describes, and the change can be as large as the answer. At the widest delta
/// `testing/evidence/c9-bounds.json` measures — 1,000 rows changing value, 31,780 bytes — a full ring is
/// 8.1 MB for one query, and a wider answer makes that whatever it likes. So the ring drops its oldest
/// epoch when *either* bound is exceeded.
///
/// 8 MiB, chosen to sit just above the 8,135,680 bytes the count bound already implied at that widest
/// measured delta: the two bounds meet at the measured worst case, so a narrow query gets its full 256
/// epochs of history and a pathologically wide one gets fewer epochs rather than unbounded memory.
pub const SUBSCRIPTION_RING_BYTES: usize = 8 * 1024 * 1024;

/// One sealed epoch's delta for one query.
#[derive(Clone, Debug)]
pub struct EpochDelta {
    pub epoch: Epoch,
    /// The delta, rendered — the same canonical form the differential harness compares (S-8).
    pub rendered: String,
}

/// Auditable outcome of one source-scoped retraction (D-27).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetractionReceipt {
    /// `None` means the source/predicate selected no current contribution and no epoch was created.
    pub sealed_epoch: Option<Epoch>,
    pub tables: usize,
    pub rows: usize,
    pub multiplicity: u128,
}

impl RetractionReceipt {
    #[must_use]
    pub fn render(&self) -> String {
        match self.sealed_epoch {
            Some(epoch) => format!(
                "retracted\nepoch {epoch}\ntables {}\nrows {}\nmultiplicity {}\n",
                self.tables, self.rows, self.multiplicity
            ),
            None => "no-op\ntables 0\nrows 0\nmultiplicity 0\n".to_owned(),
        }
    }
}

/// A registration as the server holds it: the memo's handle, the text it came from, and its deltas.
#[derive(Debug)]
struct Standing {
    entry: Entry,
    handle: Handle,
    /// The last `SUBSCRIPTION_RING` epochs' deltas, oldest first, and at most
    /// `SUBSCRIPTION_RING_BYTES` of them.
    ring: VecDeque<EpochDelta>,
    /// Bytes the ring holds — tracked rather than recomputed, so bounding it is not O(ring) per epoch.
    ring_bytes: usize,
    /// The oldest epoch still in the ring; a token below this is a gap.
    oldest: Epoch,
    /// The answer as of the epoch the last delta was computed at, so the next delta is a difference.
    last_answer: String,
    /// Set when a persisted registration could not be rebuilt (D-22: quarantined, not dropped).
    quarantined: Option<String>,
}

/// The engine one `schweepd` process owns.
#[derive(Debug)]
pub struct Engine {
    dir: PathBuf,
    catalog: Catalog,
    log: Log,
    memo: Memo,
    gate: Gate,
    standing: BTreeMap<u64, Standing>,
    registry: Registry,
    sync: SyncPolicy,
    checkpoint_every: u64,
}

impl Engine {
    /// Open, recovering what is on disk: the log (R5–R7) and the registry (D-22).
    ///
    /// **Not the checkpoint.** Recovery here is bootstrap — the retained log plus the C7 snapshot,
    /// hydrated per registration — because a memo's shape is the set of queries registered at the time
    /// and `Circuit::restore` requires the shape it was taken from. The D-22 addendum records why, and
    /// what the checkpoint is still for.
    pub fn open(
        dir: impl AsRef<Path>,
        catalog: Catalog,
        policy: Policy,
        sync: SyncPolicy,
        checkpoint_every: u64,
    ) -> ServerResult<Engine> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir).map_err(|e| ServerError::Io(e.to_string()))?;

        let mut faults = FaultInjector::inert();
        let log = Log::open(dir.join("log"), catalog.clone(), &mut faults, sync)?;

        // **The state store is deleted before it is opened**, and that is not a shortcut — it is the
        // D-22 addendum, which exists because skipping it applied every input twice. State crosses a
        // restart by *bootstrap*: the retained log plus the C7 snapshot, hydrated per registration. A
        // redb store that survived would be a second, unaudited source of the same state, and hydrating
        // on top of it double-counts every row. C4's crash harness clears its spill directory for the
        // same reason and in the same words: redb is a spill target, not a recovery mechanism.
        let state = dir.join("state");
        if state.exists() {
            std::fs::remove_dir_all(&state).map_err(|e| ServerError::Io(e.to_string()))?;
        }

        // R4, and only R4: `checkpoint::load` deletes abandoned `.partial` directories on its way to
        // choosing a checkpoint, and a server killed a thousand times would otherwise leave a thousand of
        // them. The checkpoint it *finds* is deliberately discarded — recovery here is bootstrap, per the
        // D-22 addendum — and discarding it in one visible line is better than a cleanup that silently
        // duplicates R4's rules.
        let _unused_by_recovery = checkpoint::load(dir.join("ckpt"))?;

        // The memo keeps no data: catch-up is sourced from the log and the snapshot below (C8's gap).
        let mut memo = Memo::without_input_cache(
            catalog.clone(),
            Sharing::On,
            Box::new(RedbFactory::new(&state)),
        )?;
        // A compacted prefix is represented by the published snapshot, not by replayable epochs. Set
        // the clock to that anchor before replaying the suffix; otherwise the data would recover but
        // every answer would be labelled with an epoch short by `retained_from` (I-3).
        memo.set_epoch(log.retained_from())?;

        let registry = Registry::load(&dir)?;
        let mut engine = Engine {
            dir,
            catalog,
            log,
            memo,
            gate: Gate::new(policy),
            standing: BTreeMap::new(),
            registry,
            sync,
            checkpoint_every,
        };

        // Replay the log into the memo *before* rebuilding registrations, so a rebuilt query is caught
        // up to the same epoch every other one is. Doing it the other way round would catch each
        // registration up to a moving target.
        engine.replay()?;
        engine.rebuild_registrations()?;
        Ok(engine)
    }

    /// Step the memo through every epoch the log holds. The memo is fresh; the log is not.
    ///
    /// One epoch at a time, so a log larger than memory replays inside it.
    fn replay(&mut self) -> ServerResult<()> {
        for epoch in (self.log.retained_from() + 1)..=self.log.sealed_epoch() {
            let deltas = epoch_deltas(&self.log, epoch)?;
            self.memo.seal_epoch(&deltas)?;
        }
        Ok(())
    }

    /// Rebuild every persisted registration (D-22), quarantining the ones that no longer bind.
    fn rebuild_registrations(&mut self) -> ServerResult<()> {
        let entries: Vec<Entry> = self.registry.entries.values().cloned().collect();
        for entry in entries {
            match self.install(&entry) {
                Ok(()) => {}
                Err(error) => {
                    // D-22: quarantined, not dropped. A registration that silently disappeared would
                    // leave the server looking healthy while answering nothing.
                    let message = error.to_string();
                    self.standing.insert(
                        entry.handle,
                        Standing {
                            entry,
                            handle: Handle::unregistered(),
                            ring: VecDeque::new(),
                            ring_bytes: 0,
                            oldest: 0,
                            last_answer: String::new(),
                            quarantined: Some(message),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    /// Compile a registration and catch it up from disk, **one epoch at a time**.
    ///
    /// Not one accumulated delta, and that is the point C8's forward pointer was about: the accumulated
    /// history is O(data) resident, while a chunk per epoch is O(largest epoch). A query registered late
    /// against a log larger than the process's memory ceiling therefore completes, which is exactly what
    /// `testing/soak/tests/c9_memo_ceiling.rs` measures.
    ///
    /// It buys a second thing for free: the chunks *are* the epochs, in order, so a caught-up registration
    /// takes the same passes the live path took. The recovered server is then identical to a never-crashed
    /// twin down to the emission counters, which is why the kill -9 gate compares full fingerprints rather
    /// than counter-stripped ones.
    fn install(&mut self, entry: &Entry) -> ServerResult<()> {
        let plan = schweep_sql::compile(&entry.sql, &self.catalog)?;

        // Lazy on purpose: `map_while` reads one epoch, hands it over, and drops it. Collecting the
        // epochs first would put the whole history back in memory and undo the reason this is chunked.
        //
        // A read error ends the stream, and `failure` carries it out — because a stream that stopped
        // early would otherwise register a query caught up to *less* than the history, which is the one
        // failure mode a silent short read has and the worst one available: a query that answers
        // confidently and wrongly forever.
        let mut failure: Option<ServerError> = None;
        let log = &self.log;
        let memo = &mut self.memo;
        // Snapshot first, one Parquet record batch at a time; then the retained log suffix, one epoch
        // at a time. Both halves are bounded by their largest chunk, never by database size.
        let snapshot = match log.snapshot() {
            Some(path) => Some(schweep_batch::snapshot::chunks(path, &self.catalog)?),
            None => None,
        };
        let snapshot_chunks = snapshot
            .into_iter()
            .flatten()
            .map(|chunk| chunk.map_err(ServerError::from));
        let suffix_chunks =
            ((log.retained_from() + 1)..=log.sealed_epoch()).map(|epoch| epoch_deltas(log, epoch));
        let chunks = snapshot_chunks
            .chain(suffix_chunks)
            .map_while(|chunk| match chunk {
                Ok(deltas) => Some(deltas),
                Err(error) => {
                    failure = Some(error);
                    None
                }
            });
        let handle = memo.register_from_chunks(&plan, entry.admission.clone(), chunks)?;
        if let Some(error) = failure {
            self.memo.deregister(handle)?;
            return Err(error);
        }
        let answer = self.render_answer(handle);
        self.standing.insert(
            entry.handle,
            Standing {
                entry: entry.clone(),
                handle,
                ring: VecDeque::new(),
                ring_bytes: 0,
                oldest: self.log.sealed_epoch(),
                last_answer: answer,
                quarantined: None,
            },
        );
        Ok(())
    }

    fn render_answer(&self, handle: Handle) -> String {
        match self.memo.read(handle) {
            Ok((_, answer)) => answer.render(),
            Err(error) => format!("ERROR: {error}\n"),
        }
    }

    // ---- the endpoints' engine side ------------------------------------------------------------

    /// One append (A1–A8), behind per-source admission.
    ///
    /// The batch's size is its framed length — the log's own encoding, which is what the pending queue
    /// holds — so the byte bound is argued in the unit the memory is actually in (`costs.rs`).
    pub fn ingest(
        &mut self,
        source: &str,
        table: &str,
        token: &str,
        entries: Vec<(Row, i64)>,
    ) -> ServerResult<Ack> {
        let batch_bytes = crate::server::encode_batch(table, token, &entries).len();
        match self.gate.check(source, batch_bytes) {
            Verdict::Overloaded {
                source_depth,
                bound,
            } => {
                return Err(ServerError::Overloaded {
                    source_id: source.to_owned(),
                    depth: source_depth,
                    bound,
                })
            }
            Verdict::OverloadedBytes {
                queued_bytes,
                batch_bytes,
                bound,
            } => {
                return Err(ServerError::OverloadedBytes {
                    source_id: source.to_owned(),
                    queued_bytes,
                    batch_bytes,
                    bound,
                })
            }
            Verdict::TooLarge { batch_bytes, bound } => {
                return Err(ServerError::BatchTooLarge { batch_bytes, bound })
            }
            Verdict::Admit => {}
        }
        let mut faults = FaultInjector::inert();
        let ack = self
            .log
            .append(source, table, entries, token, &mut faults)?;
        if ack == Ack::Appended {
            self.gate.admitted(source, batch_bytes);
        }
        Ok(ack)
    }

    /// Seal the epoch (S1–S4), step the memo (S3), and advance every subscription.
    pub fn seal(&mut self) -> ServerResult<Epoch> {
        let mut faults = FaultInjector::inert();
        let batches: Vec<schweep_log::Batch> = self.log.pending_batches().to_vec();
        let epoch = self.log.seal_epoch(&mut faults)?;

        let mut deltas = EpochDeltas::new();
        for batch in &batches {
            deltas.extend(batch.table.clone(), batch.entries.iter().cloned());
        }
        self.memo.seal_epoch(&deltas)?;
        self.gate.sealed();

        // Each query's delta for this epoch is the difference its answer just made. Computed here, once,
        // rather than per subscriber: a subscriber's arrival must not change what an epoch contained.
        let handles: Vec<u64> = self.standing.keys().copied().collect();
        for id in handles {
            let (handle, previous) = match self.standing.get(&id) {
                Some(standing) if standing.quarantined.is_none() => {
                    (standing.handle, standing.last_answer.clone())
                }
                _ => continue,
            };
            let answer = self.render_answer(handle);
            let rendered = delta_between(&previous, &answer);
            if let Some(standing) = self.standing.get_mut(&id) {
                standing.last_answer = answer;
                standing.ring_bytes += rendered.len();
                standing.ring.push_back(EpochDelta { epoch, rendered });
                // Either bound evicts. The count keeps history from growing without end; the bytes keep
                // one wide epoch from doing in a single step what the count would have taken 256 to do.
                while standing.ring.len() > SUBSCRIPTION_RING
                    || (standing.ring.len() > 1 && standing.ring_bytes > SUBSCRIPTION_RING_BYTES)
                {
                    if let Some(dropped) = standing.ring.pop_front() {
                        standing.ring_bytes =
                            standing.ring_bytes.saturating_sub(dropped.rendered.len());
                    }
                    standing.oldest += 1;
                }
            }
        }

        if self.checkpoint_every > 0 && epoch % self.checkpoint_every == 0 {
            self.checkpoint()?;
        }
        Ok(epoch)
    }

    /// Append N batches and seal, with the epoch boundary all-or-nothing (MD-2 ask 3, D-23).
    ///
    /// If any append is refused, **no seal happens** and the appends already made are left pending —
    /// which is exactly what the log does with a crash between A8 and S1. The refusal is returned, and a
    /// client that needs the stronger property retries with the same dedup tokens: I-4 drops the ones
    /// that landed.
    pub fn transaction(
        &mut self,
        source: &str,
        batches: Vec<crate::WireBatch>,
    ) -> ServerResult<Epoch> {
        for (table, token, entries) in batches {
            self.ingest(source, &table, &token, entries)?;
        }
        self.seal()
    }

    /// Remove a source's current net contribution through the ordinary delta path (C11, D-27).
    ///
    /// `predicate` is valid only with `table` and is bound by the same SQL implementation as WHERE.
    /// The negative batches retain `source` as their source id, so a completed retry sees net zero.
    pub fn retract_source(
        &mut self,
        source: &str,
        table: Option<&str>,
        predicate: Option<&str>,
    ) -> ServerResult<RetractionReceipt> {
        if predicate.is_some() && table.is_none() {
            return Err(ServerError::Sql(schweep_sql::SqlError::Parse(
                "a source-retraction predicate requires a table".to_owned(),
            )));
        }
        if let Some(table) = table {
            if !self.catalog.contains_key(table) {
                return Err(ServerError::Log(schweep_log::LogError::UnknownTable(
                    table.to_owned(),
                )));
            }
        }

        let bound_predicate = match (table, predicate) {
            (Some(table), Some(predicate)) if !predicate.trim().is_empty() => Some(
                schweep_sql::bind_where_predicate(table, predicate, &self.catalog)?,
            ),
            _ => None,
        };
        let contributions = schweep_batch::source_integral(&self.log, &self.catalog, source)?;
        let mut batches = Vec::new();
        let token_prefix = format!("retract:{source}:");
        let pending_retractions: Vec<_> = self
            .log
            .pending_batches()
            .iter()
            .filter(|batch| {
                batch.source_id == source && batch.dedup_token.starts_with(&token_prefix)
            })
            .cloned()
            .collect();
        let mut tables_seen: BTreeSet<String> = pending_retractions
            .iter()
            .map(|batch| batch.table.clone())
            .collect();
        let mut rows: usize = pending_retractions
            .iter()
            .map(|batch| batch.entries.len())
            .sum();
        let mut multiplicity: u128 = pending_retractions
            .iter()
            .flat_map(|batch| batch.entries.iter())
            .map(|(_, weight)| u128::from(weight.unsigned_abs()))
            .sum();
        let target_epoch = self.log.sealed_epoch() + 1;

        for (candidate, integral) in contributions {
            if table.is_some_and(|selected| selected != candidate) {
                continue;
            }
            let mut selected = Vec::new();
            for (row, weight) in integral
                .entries()
                .map_err(schweep_batch::BatchError::ZSet)?
            {
                let matches = match &bound_predicate {
                    None => true,
                    Some(predicate) => {
                        schweep_plan::is_true(&predicate.expression, &row, &predicate.scope)
                            .map_err(schweep_sql::SqlError::Plan)?
                    }
                };
                if matches {
                    let negative = weight.checked_neg().ok_or({
                        schweep_batch::BatchError::ZSet(schweep_zset::ZSetError::WeightOverflow {
                            while_doing: "negating a source contribution",
                        })
                    })?;
                    rows += 1;
                    multiplicity += u128::from(weight.unsigned_abs());
                    selected.push((row, negative));
                }
            }
            if selected.is_empty() {
                continue;
            }
            let identity = schweep_log::Record::Append {
                source_id: source.to_owned(),
                dedup_token: String::new(),
                table: candidate.clone(),
                entries: selected.clone(),
            }
            .content_hash();
            let token = format!("retract:{source}:{target_epoch}:{candidate}:{identity:016x}");
            tables_seen.insert(candidate.clone());
            batches.push((candidate, token, selected));
        }

        if batches.is_empty() {
            if !pending_retractions.is_empty() {
                let epoch = self.seal()?;
                return Ok(RetractionReceipt {
                    sealed_epoch: Some(epoch),
                    tables: tables_seen.len(),
                    rows,
                    multiplicity,
                });
            }
            return Ok(RetractionReceipt {
                sealed_epoch: None,
                tables: 0,
                rows: 0,
                multiplicity: 0,
            });
        }
        let epoch = self.transaction(source, batches)?;
        Ok(RetractionReceipt {
            sealed_epoch: Some(epoch),
            tables: tables_seen.len(),
            rows,
            multiplicity,
        })
    }

    /// Register a standing query (D-22): compile, catch up from disk, persist, then answer.
    pub fn register(&mut self, sql: &str, admission: Admission) -> ServerResult<u64> {
        // Compiled before a handle is issued, so a query that cannot bind never enters the registry.
        schweep_sql::compile(sql, &self.catalog)?;

        let id = self.registry.next_handle;
        let entry = Entry {
            handle: id,
            sql: sql.to_owned(),
            admission,
        };
        self.install(&entry)?;
        self.registry.entries.insert(id, entry);
        self.registry.next_handle = id + 1;
        // Persisted *after* the install succeeded: a registry entry for a query that failed to build
        // would quarantine itself on every subsequent restart.
        self.registry.store(&self.dir)?;
        Ok(id)
    }

    pub fn deregister(&mut self, id: u64) -> ServerResult<()> {
        let standing = self
            .standing
            .remove(&id)
            .ok_or(ServerError::UnknownHandle(id))?;
        if standing.quarantined.is_none() {
            self.memo.deregister(standing.handle)?;
        }
        self.registry.entries.remove(&id);
        self.registry.store(&self.dir)?;
        Ok(())
    }

    /// Read at the latest sealed epoch (I-3).
    pub fn read(&self, id: u64) -> ServerResult<(Epoch, String)> {
        let standing = self
            .standing
            .get(&id)
            .ok_or(ServerError::UnknownHandle(id))?;
        if let Some(reason) = &standing.quarantined {
            return Err(ServerError::Quarantined {
                handle: id,
                reason: reason.clone(),
            });
        }
        match self.memo.read(standing.handle) {
            Ok((epoch, answer)) => Ok((epoch, answer.render())),
            Err(error) => Err(ServerError::Memo(error)),
        }
    }

    /// The same answer as [`Engine::read`], in the log's own frame — the form the differential
    /// harness needs (I-1 over the network).
    ///
    /// A *rendered* answer is what a human reads and what S-8 compares, but reconstructing the rows from
    /// it would need a value parser, and a bug in that parser could hide a divergence rather than show
    /// one. So the rows cross the wire in the encoding the log already writes and C4 already tests:
    /// one `Append` frame whose entries **are** the answer's entries, in the answer's order, with the
    /// schema in the `table` slot and the epoch in the token slot. No new serialization format (D-23).
    ///
    /// The order is not re-sorted here, deliberately: the client re-canonicalizes what it decodes and
    /// requires the result to re-render to exactly these bytes, so a server that emitted rows out of
    /// canonical order fails the gate instead of being quietly tidied up by the client (D-7).
    pub fn read_frames(&self, id: u64) -> ServerResult<Vec<u8>> {
        let standing = self
            .standing
            .get(&id)
            .ok_or(ServerError::UnknownHandle(id))?;
        if let Some(reason) = &standing.quarantined {
            return Err(ServerError::Quarantined {
                handle: id,
                reason: reason.clone(),
            });
        }
        let (epoch, answer) = self.memo.read(standing.handle)?;
        Ok(schweep_log::record::frame(
            &schweep_log::Record::Append {
                source_id: "answer".to_owned(),
                dedup_token: epoch.to_string(),
                table: answer.schema().to_string(),
                entries: answer.entries().to_vec(),
            }
            .encode(),
        ))
    }

    /// The dataflow's state fingerprint — what I-2 and I-7 compare (C4's `state_fingerprint`).
    pub fn fingerprint(&self) -> ServerResult<String> {
        Ok(self.memo.dataflow().state_fingerprint()?)
    }

    /// Every sealed epoch strictly after `from`, and the token to use next (D-23).
    pub fn subscribe(&self, id: u64, from: Epoch) -> ServerResult<(Epoch, Vec<EpochDelta>)> {
        let standing = self
            .standing
            .get(&id)
            .ok_or(ServerError::UnknownHandle(id))?;
        if let Some(reason) = &standing.quarantined {
            return Err(ServerError::Quarantined {
                handle: id,
                reason: reason.clone(),
            });
        }
        // A token behind the ring is a **gap**, and D-23 refuses it rather than serving a re-baseline
        // dressed as a delta.
        if from < standing.oldest {
            return Err(ServerError::TokenTooOld {
                handle: id,
                token: from,
                oldest: standing.oldest,
            });
        }
        let deltas: Vec<EpochDelta> = standing
            .ring
            .iter()
            .filter(|delta| delta.epoch > from)
            .cloned()
            .collect();
        // The next token is the last epoch delivered, or the caller's own when nothing was: a caught-up
        // subscriber is not a subscriber in trouble.
        let next = deltas.last().map_or(from, |delta| delta.epoch);
        Ok((next, deltas))
    }

    /// Answer once through an ephemeral circuit, over the log's accumulated input (C7).
    pub fn oneshot(&self, sql: &str) -> ServerResult<String> {
        let bound = schweep_sql::bind_sql(sql, &self.catalog)?;
        let answer =
            schweep_batch::oneshot::answer_over_log(&self.log, &self.catalog, &bound.query)?;
        Ok(answer.render())
    }

    /// The canonical plan's structural form and hash — half of I-6's network comparison.
    pub fn plan_of(&self, id: u64) -> ServerResult<String> {
        let standing = self
            .standing
            .get(&id)
            .ok_or(ServerError::UnknownHandle(id))?;
        let registration = self
            .memo
            .registrations()
            .get(&standing.handle)
            .ok_or(ServerError::UnknownHandle(id))?;
        Ok(format!(
            "hash {:016x}\n{}",
            registration.plan.structural_hash(),
            registration.plan.structural_form()
        ))
    }

    /// Per-node execution counters — the other half of I-6.
    pub fn counters(&self) -> String {
        let counters = self.memo.dataflow().counters();
        let steps = self.memo.dataflow().step_counters();
        let mut out = format!("operator steps {}\n", self.memo.dataflow().operator_steps());
        for (index, emitted) in counters.iter().enumerate() {
            out.push_str(&format!(
                "node {index} emitted {emitted} stepped {}\n",
                steps.get(index).copied().unwrap_or(0)
            ));
        }
        out
    }

    pub fn explain_state(&self) -> ServerResult<String> {
        Ok(self
            .memo
            .explain_state(schweep_memo::CostModel::redb())?
            .render())
    }

    #[must_use]
    pub fn explain_maintenance(&self) -> String {
        self.memo.explain_maintenance().render()
    }

    /// Checkpoint the dataflow (C1–C7 of the checkpoint sequence).
    ///
    /// **Recovery does not read this**, and the D-22 addendum says why: a memo cannot be restored through
    /// `Circuit::restore`, whose contract is a circuit of the same shape. What the checkpoint is for here
    /// is the role C7 gives it — the published anchor compaction needs (P1, oldest published checkpoint).
    /// Said out loud because a checkpoint nobody reads would otherwise read as a guarantee.
    pub fn checkpoint(&mut self) -> ServerResult<()> {
        let mut faults = FaultInjector::inert();
        checkpoint::take(
            self.dir.join("ckpt"),
            self.memo.dataflow(),
            &mut faults,
            self.sync,
        )?;
        Ok(())
    }

    /// Compact the sealed prefix into the Parquet ground-truth snapshot and keep serving.
    ///
    /// The checkpoint is published first because it is the recovery anchor (P1). The snapshot and
    /// suffix then replace the prefix through C7's publish-then-swap protocol. Recovery exercises the
    /// streaming path above, so compaction is not considered successful merely because the current
    /// in-memory memo kept answering.
    pub fn compact(&mut self) -> ServerResult<schweep_batch::Compacted> {
        self.checkpoint()?;
        let anchor = self.log.sealed_epoch();
        let integrals = schweep_batch::hydrate::accumulated_upto(&self.log, &self.catalog, anchor)?;
        let mut faults = FaultInjector::inert();
        Ok(schweep_batch::compact(
            &mut self.log,
            anchor,
            &integrals,
            &mut faults,
            self.sync,
        )?)
    }

    /// Graceful shutdown: **checkpoint, then drain** (§6 C9).
    ///
    /// Draining first would checkpoint a state the log had already moved past; checkpointing first and
    /// then refusing new work means the epoch the checkpoint names is the epoch the log ends at. What is
    /// *not* done is sealing the pending appends: they are durable (A6) and unsealed, and the next start
    /// seals them with whatever arrives next — which is the same thing a crash here would leave, so
    /// shutdown and crash produce the same recovery path rather than two.
    pub fn shutdown(&mut self) -> ServerResult<Drained> {
        self.checkpoint()?;
        Ok(Drained {
            epoch: self.log.sealed_epoch(),
            pending_appends: self.pending(),
            registrations: self.standing.len(),
        })
    }

    // ---- reporting -----------------------------------------------------------------------------

    #[must_use]
    pub fn health(&self) -> String {
        let mut out = format!(
            "epoch {}\nretained_from {}\nregistrations {}\npending_appends {}\n",
            self.log.sealed_epoch(),
            self.log.retained_from(),
            self.standing.len(),
            self.pending()
        );
        out.push_str(&self.gate.render());
        for (id, standing) in &self.standing {
            match &standing.quarantined {
                None => out.push_str(&format!(
                    "handle {id}: live · ring {} · ring_bytes {} · oldest {} · {}\n",
                    standing.ring.len(),
                    standing.ring_bytes,
                    standing.oldest,
                    standing.entry.sql
                )),
                Some(reason) => out.push_str(&format!(
                    "handle {id}: QUARANTINED · {reason} · {}\n",
                    standing.entry.sql
                )),
            }
        }
        out
    }

    /// Appends that are durable and not yet sealed.
    ///
    /// **Read from the log, never counted in memory.** A counter incremented per append reports 0 after a
    /// restart, which is how the kill -9 matrix found this: a recovered server holding acknowledged,
    /// unsealed batches described itself as having none. The log knows, and the log is the thing that
    /// survived.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.log.pending_batches().len()
    }

    #[must_use]
    pub fn epoch(&self) -> Epoch {
        self.log.sealed_epoch()
    }

    #[must_use]
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }
}

/// What a graceful shutdown drained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Drained {
    pub epoch: Epoch,
    pub pending_appends: usize,
    pub registrations: usize,
}

/// One sealed epoch's input, read out of the log.
fn epoch_deltas(log: &Log, epoch: Epoch) -> ServerResult<EpochDeltas> {
    let mut deltas = EpochDeltas::new();
    for batch in log.epoch(epoch)? {
        deltas.extend(batch.table.clone(), batch.entries.iter().cloned());
    }
    Ok(deltas)
}

/// The difference between two rendered answers, as a delta a subscriber can apply.
///
/// Rendered rather than computed as a Z-set difference, because the *answer* is what crosses the wire
/// (S-8) and a subscriber's job is to reproduce it. Lines present in the new answer and not the old are
/// additions; lines present in the old and not the new are retractions. Both are printed with the sign,
/// so applying a delta is line arithmetic and not a parse of the engine's internals.
pub fn delta_between(previous: &str, current: &str) -> String {
    let old: Vec<&str> = previous.lines().collect();
    let new: Vec<&str> = current.lines().collect();
    let mut out = String::new();
    for line in &new {
        if !old.contains(line) {
            out.push_str(&format!("+ {line}\n"));
        }
    }
    for line in &old {
        if !new.contains(line) {
            out.push_str(&format!("- {line}\n"));
        }
    }
    if out.is_empty() {
        out.push_str("(no change)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use schweep_zset::{DataType, Field, Schema, Value};

    #[test]
    fn a_delta_names_what_arrived_and_what_left() {
        let before = "(n: Int64)\n(1) => 1\n(2) => 1\n";
        let after = "(n: Int64)\n(2) => 1\n(3) => 1\n";
        let delta = delta_between(before, after);
        assert!(delta.contains("+ (3) => 1"), "{delta}");
        assert!(delta.contains("- (1) => 1"), "{delta}");
        assert!(
            !delta.contains("(2) => 1"),
            "unchanged rows are not in a delta"
        );
    }

    #[test]
    fn an_epoch_that_changed_nothing_says_so_rather_than_being_empty() {
        // An empty body would be indistinguishable from a transport that dropped it.
        assert_eq!(
            delta_between("(n: Int64)\n", "(n: Int64)\n"),
            "(no change)\n"
        );
    }

    #[test]
    fn a_compacted_server_restarts_at_the_right_epoch_with_the_same_answer() {
        let dir = std::env::temp_dir().join(format!(
            "schweep-c10-server-compaction-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let schema = Schema::new_table(vec![
            Field::not_null("k", DataType::Int64),
            Field::not_null("n", DataType::Int64),
        ])
        .unwrap();
        let catalog = Catalog::from([("t".to_owned(), schema)]);
        let mut engine = Engine::open(
            &dir,
            catalog.clone(),
            Policy::default(),
            SyncPolicy::Deferred,
            2,
        )
        .unwrap();
        let handle = engine
            .register(
                "SELECT t.k AS k, SUM(t.n) AS s FROM t GROUP BY t.k",
                Admission::with_unbounded_state("test fixture has a bounded key set"),
            )
            .unwrap();
        for (token, value) in [("b1", 10), ("b2", 5), ("b3", -2)] {
            engine
                .ingest(
                    "source",
                    "t",
                    token,
                    vec![(Row::new(vec![Value::Int(1), Value::Int(value)]), 1)],
                )
                .unwrap();
            engine.seal().unwrap();
        }
        let before = engine.read(handle).unwrap();
        let compacted = engine.compact().unwrap();
        assert_eq!(compacted.anchor, 3);
        assert_eq!(engine.read(handle).unwrap(), before);
        drop(engine);

        let recovered =
            Engine::open(&dir, catalog, Policy::default(), SyncPolicy::Deferred, 2).unwrap();
        assert_eq!(recovered.read(handle).unwrap(), before);
        assert_eq!(recovered.epoch(), 3);
        assert!(recovered.health().contains("retained_from 3"));
    }
}
