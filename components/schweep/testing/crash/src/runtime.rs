//! A durable run of a scenario: log, circuit, checkpoints, and recovery.
//!
//! This is the object a crash lands in the middle of. It owns the three durable things —
//! `schweep-log`'s segment, the checkpoint directory, and the circuit whose state they protect — and
//! sequences them in the orderings `docs/DURABILITY.md` numbers.

use std::path::{Path, PathBuf};

use schweep_circuit::{checkpoint, Circuit};
use schweep_differential::{CircuitEngine, EngineUnderTest, Scenario};
use schweep_log::{Ack, FaultInjector, FaultPlan, Log, Seam, SyncPolicy};
use schweep_zset::{EpochDeltas, Schema};

use crate::scenario_fault::Fault;

/// What one run produced, for comparison against a twin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutcome {
    /// The state fingerprint after every sealed epoch, in order.
    pub fingerprints: Vec<String>,
    /// The answer, or the live error, after every sealed epoch.
    pub answers: Vec<String>,
    /// What the log **means**: its epoch, the tokens it has acknowledged, and the input it has
    /// accumulated.
    ///
    /// Not `Log::render()`, which lists the records it currently holds. Compaction legitimately
    /// discards records, so two twins can hold different records and mean the same thing — and after
    /// C7 they routinely do, because whether a cycle got as far as its compaction depends on where the
    /// crash landed. Comparing the meaning is both compaction-invariant *and* a stronger statement
    /// about I-4 than the old rendering, which counted tokens without naming them.
    pub log: String,
    /// The highest epoch the circuit reached.
    pub epoch: u64,
    /// Batches the log accepted, by token. Used for the I-4 check: exactly one epoch each.
    pub accepted_tokens: Vec<String>,
    /// True if recovery rebuilt the circuit from the snapshot rather than from a checkpoint (C7).
    ///
    /// The one case whose **emission counters** legitimately differ from an uncrashed twin's: the state
    /// is the same state, reached by one delta instead of many. The gate compares a counter-stripped
    /// fingerprint on these cycles and counts them separately, rather than weakening the comparison for
    /// every cycle.
    pub bootstrapped: bool,
}

/// A fingerprint with the I-9 emission counts removed.
///
/// Used only where a bootstrap makes them legitimately different — see [`RunOutcome::bootstrapped`].
/// Everywhere else the full fingerprint is compared, counters included.
#[must_use]
pub fn without_emission_counts(fingerprint: &str) -> String {
    fingerprint
        .lines()
        .map(|line| {
            let mut kept: Vec<&str> = Vec::new();
            for part in line.split(' ') {
                if !part.starts_with("emitted=") && !part.starts_with("budget=") {
                    kept.push(part);
                }
            }
            kept.join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// A durable run.
pub struct Durable {
    dir: PathBuf,
    tables: Vec<(String, Schema)>,
    query: schweep_plan::Query,
    log: Log,
    circuit: Circuit,
    sync: SyncPolicy,
    /// True if this run's recovery had to rebuild the circuit from the snapshot rather than from a
    /// checkpoint. Reported, because it is the one path whose emission counters legitimately differ.
    bootstrapped: bool,
}

impl std::fmt::Debug for Durable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Durable")
            .field("dir", &self.dir)
            .field("epoch", &self.circuit.epoch())
            .finish()
    }
}

/// Which store operator state lives in (C8).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// `MemBackend`: what every C1–C7 gate ran on.
    Memory,
    /// `RedbBackend`: state on disk, in one redb file per operator (D-19).
    Redb,
}

/// How a run is configured.
#[derive(Clone, Copy, Debug)]
pub struct Config {
    /// Which store operator state lives in.
    ///
    /// The C4 gates run twice from C8 on: **the backend that ships must survive the same fire
    /// `MemBackend` did, not inherit its record.**
    pub backend: Backend,
    /// Take a checkpoint every `checkpoint_every` epochs. 0 means never.
    pub checkpoint_every: u64,
    /// Compact every `compact_every` epochs, anchored to the live checkpoint. 0 means never.
    ///
    /// Compaction is in the crash harness rather than beside it because a compaction is the one
    /// operation that *deletes* committed history: a crash in the middle of it is the most expensive
    /// crash the system can have, and the eight seams of `docs/DURABILITY.md` §4 are only tested if the
    /// 10,000-cycle gate reaches them.
    pub compact_every: u64,
    /// Whether the log and checkpoints call `fsync`.
    ///
    /// The 10,000-cycle gate uses `Deferred`, because an in-process crash cannot observe the
    /// difference and `Full` costs hours. See `schweep_log::SyncPolicy` for the argument, and for
    /// what it means the gate does *not* test.
    pub sync: SyncPolicy,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            backend: Backend::Memory,
            checkpoint_every: 2,
            compact_every: 3,
            sync: SyncPolicy::Deferred,
        }
    }
}

impl Config {
    /// The production configuration: every write synced.
    #[must_use]
    pub fn durable() -> Config {
        Config {
            sync: SyncPolicy::Full,
            ..Config::default()
        }
    }

    /// The same run, with operator state spilled to redb (C8).
    #[must_use]
    pub fn on_redb() -> Config {
        Config {
            backend: Backend::Redb,
            ..Config::default()
        }
    }
}

fn log_dir(dir: &Path) -> PathBuf {
    dir.join("log")
}

fn ckpt_dir(dir: &Path) -> PathBuf {
    dir.join("ckpt")
}

/// Where spilled operator state lives, when the backend is `Redb`.
fn state_dir(dir: &Path) -> PathBuf {
    dir.join("state")
}

impl Durable {
    /// Open, recovering whatever is on disk (`docs/DURABILITY.md` R1–R7).
    pub fn open(
        dir: impl AsRef<Path>,
        scenario: &Scenario,
        faults: &mut FaultInjector,
        _config: Config,
    ) -> Result<Durable, String> {
        let dir = dir.as_ref().to_path_buf();
        let catalog: std::collections::BTreeMap<String, Schema> =
            scenario.tables.iter().cloned().collect();

        // R5, R6, R7 · open the log: read `LOG`, discard any torn tail, and rebuild the dedup index —
        // seeded from the snapshot's ledger if a compaction has published one.
        let log =
            Log::open(log_dir(&dir), catalog, faults, _config.sync).map_err(|e| e.to_string())?;

        // R5's cleanup, for compaction's artefacts: `.partial` snapshots from a crashed attempt and
        // published snapshots the pointer no longer names. Idempotent, so a crash here is a non-event.
        let live = log
            .snapshot()
            .and_then(|dir| dir.file_name())
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("snap-"))
            .and_then(|digits| digits.parse::<u64>().ok())
            .unwrap_or(0);
        schweep_batch::compact::cleanup(&log_dir(&dir), live).map_err(|e| e.to_string())?;

        // Build a circuit of the right shape. Recovery restores state into it; it never has to guess
        // the shape, because the plan is the same plan.
        //
        // **The spill directory is cleared first, and that is not a shortcut.** `RedbBackend` puts
        // operator state on disk, but the disk is not how state crosses a restart: C4's protocol says
        // recovery is *checkpoint + replay*, and the frozen trait's `snapshot`/`restore` is how state
        // travels. Leaving redb's files in place would give a recovering circuit stale state that no
        // checkpoint accounted for — a second, unaudited durability path. redb is a **spill target**
        // here, not a second recovery mechanism, and PROGRESS.md says so out loud.
        let engine = match _config.backend {
            Backend::Memory => CircuitEngine::build(&scenario.tables, &scenario.query)
                .map_err(|e| e.to_string())?,
            Backend::Redb => {
                let spill = state_dir(&dir);
                let _ = std::fs::remove_dir_all(&spill);
                let mut factory = schweep_state::RedbFactory::new(&spill);
                CircuitEngine::build_with(&scenario.tables, &scenario.query, &mut factory)
                    .map_err(|e| e.to_string())?
            }
        };
        let mut circuit = engine.into_circuit();

        // R1, R2, R4 · choose and verify a checkpoint, deleting partials.
        let mut restored_from_checkpoint = false;
        if let Some(loaded) = checkpoint::load(ckpt_dir(&dir)).map_err(|e| e.to_string())? {
            circuit.restore(&loaded.state).map_err(|e| e.to_string())?;
            restored_from_checkpoint = true;
        }

        // The compaction case R1 alone cannot handle: no checkpoint survived, and the log's prefix is
        // in a snapshot. Refusing here would be unavailability where the data is intact, so recovery
        // *bootstraps* — hydrate from the snapshot as one delta (B1–B3), then replay the suffix.
        //
        // **What this recovers, and what it does not.** The operator state is the same state: every
        // operator's contents are a function of the accumulated input (I-2), and the snapshot plus the
        // suffix is that input. What differs is the *emission counters* — the I-9 accounting ledger,
        // which records how much each node has ever emitted. A bootstrap emits the whole input in one
        // delta, so it reaches the same state by a shorter route and counts it differently. That is a
        // real difference and it is why `run_with_fault` compares a counter-stripped fingerprint on
        // exactly these cycles, and says so.
        let mut bootstrapped = false;
        if circuit.epoch() < log.retained_from() {
            if log.snapshot().is_none() {
                return Err(format!(
                    "checkpoint at epoch {} is older than the compacted prefix (retained from {}), \
                     and there is no snapshot to bootstrap from",
                    circuit.epoch(),
                    log.retained_from()
                ));
            }
            let catalog: std::collections::BTreeMap<String, Schema> =
                scenario.tables.iter().cloned().collect();
            let integrals =
                schweep_batch::hydrate::accumulated_upto(&log, &catalog, log.retained_from())
                    .map_err(|e| e.to_string())?;
            let one_delta =
                schweep_batch::hydrate::as_one_delta(&integrals).map_err(|e| e.to_string())?;
            circuit.step(&one_delta).map_err(|e| e.to_string())?;
            // The circuit is now as of the snapshot's epoch, however many epochs that took to build.
            circuit
                .set_epoch(log.retained_from())
                .map_err(|e| e.to_string())?;
            bootstrapped = true;
        }
        let _ = restored_from_checkpoint;

        if faults.reached(Seam::RecoveryAfterCheckpointBeforeReplay) {
            return Err(format!(
                "injected fault at seam {}",
                Seam::RecoveryAfterCheckpointBeforeReplay.name()
            ));
        }

        let mut durable = Durable {
            dir,
            tables: scenario.tables.clone(),
            query: scenario.query.clone(),
            log,
            circuit,
            sync: _config.sync,
            bootstrapped,
        };

        // R7 · replay the epochs after the checkpoint's epoch.
        durable.replay_suffix(faults)?;
        Ok(durable)
    }

    fn replay_suffix(&mut self, faults: &mut FaultInjector) -> Result<(), String> {
        let sealed = self.log.sealed_epoch();
        // A circuit restored from a checkpoint at or after the compaction anchor replays only the
        // retained epochs, which is the arrangement P1 exists to guarantee: the anchor is never past
        // the live checkpoint, so the records this loop needs are always still there.
        // A checkpoint older than the snapshot is a violated invariant, not a case to paper over: the
        // epochs between them exist in neither artefact. Skipping them would recover a state that is
        // wrong in a way every later epoch would hide.
        if self.circuit.epoch() < self.log.retained_from() {
            return Err(format!(
                "checkpoint at epoch {} is older than the compacted prefix (retained from {}); \
                 the epochs between them are in neither the checkpoint nor the log",
                self.circuit.epoch(),
                self.log.retained_from()
            ));
        }
        let mut at = self.circuit.epoch();
        while at < sealed {
            at += 1;
            let batches = self.log.epoch(at).map_err(|e| e.to_string())?;
            let mut deltas = EpochDeltas::new();
            for batch in batches {
                deltas.extend(batch.table.clone(), batch.entries.iter().cloned());
            }
            // The same step the live path takes. Replay is not a special mode; that is what makes
            // "crash equals replay" a statement about one code path rather than two.
            self.circuit.step(&deltas).map_err(|e| e.to_string())?;
        }
        let _ = faults;
        Ok(())
    }

    /// Feed one epoch: append its batches, seal, step, and maybe checkpoint.
    pub fn apply_epoch(
        &mut self,
        index: usize,
        deltas: &EpochDeltas,
        faults: &mut FaultInjector,
        config: Config,
    ) -> Result<(), String> {
        // §1 · append each table's entries as one batch, with a token derived from the epoch so a
        // retry after a crash offers the same token and is dropped as a replay (I-4).
        for (table, entries) in deltas.tables() {
            let token = format!("epoch-{index}-{table}");
            let _ack = self
                .log
                .append("src", table, entries.clone(), &token, faults)
                .map_err(|e| e.to_string())?;
        }

        // §2 · seal, then step. The seal record is the commit point (S1–S2); the step is a
        // deterministic function of it (S3–S4).
        self.log.seal_epoch(faults).map_err(|e| e.to_string())?;

        if faults.reached(Seam::SealAfterFsyncBeforeStep) {
            return Err(format!(
                "injected fault at seam {}",
                Seam::SealAfterFsyncBeforeStep.name()
            ));
        }

        self.circuit.step(deltas).map_err(|e| e.to_string())?;

        if faults.reached(Seam::SealAfterStepBeforeCounter) {
            return Err(format!(
                "injected fault at seam {}",
                Seam::SealAfterStepBeforeCounter.name()
            ));
        }

        // §3 · checkpoint, on the configured interval.
        if config.checkpoint_every > 0 && self.circuit.epoch() % config.checkpoint_every == 0 {
            checkpoint::take(ckpt_dir(&self.dir), &self.circuit, faults, self.sync)
                .map_err(|e| e.to_string())?;
        }

        // §4 · compaction, on its own interval. Anchored to the live checkpoint (P1): compacting past
        // it would delete records recovery still needs.
        if config.compact_every > 0 && self.circuit.epoch() % config.compact_every == 0 {
            self.compact(faults)?;
        }
        Ok(())
    }

    /// One compaction, anchored to the newest published checkpoint (`docs/DURABILITY.md` §4).
    ///
    /// A compaction that cannot be anchored, or whose prefix is already gone, is not an error the run
    /// should fail on — it is a compaction that has nothing to do.
    pub fn compact(&mut self, faults: &mut FaultInjector) -> Result<(), String> {
        // P1 · the **oldest** published checkpoint, not the newest. R1/R2 may fall back to any
        // checkpoint on disk, and a compaction past one of them would delete the records that
        // checkpoint needs to replay — see `docs/DURABILITY.md` §4, where the crash gate's discovery of
        // exactly that is recorded.
        let anchor = checkpoint::published_epochs(ckpt_dir(&self.dir))
            .map_err(|e| e.to_string())?
            .into_iter()
            .min()
            .unwrap_or(0);
        if anchor == 0 || anchor <= self.log.retained_from() || anchor > self.log.sealed_epoch() {
            return Ok(());
        }
        let catalog: std::collections::BTreeMap<String, Schema> =
            self.tables.iter().cloned().collect();
        let integrals = schweep_batch::hydrate::accumulated_upto(&self.log, &catalog, anchor)
            .map_err(|e| e.to_string())?;
        schweep_batch::compact(&mut self.log, anchor, &integrals, faults, self.sync)
            .map_err(|e| e.to_string())?;
        Ok(())
    }

    /// What the log means, rendered so two twins can be compared across a compaction.
    pub fn log_meaning(&self) -> Result<String, String> {
        let catalog: std::collections::BTreeMap<String, Schema> =
            self.tables.iter().cloned().collect();
        let integrals =
            schweep_batch::hydrate::accumulated(&self.log, &catalog).map_err(|e| e.to_string())?;
        let mut out = format!(
            "log @ epoch {} · {} token(s)\n",
            self.log.sealed_epoch(),
            self.log.known_tokens()
        );
        for token in self.log.tokens() {
            out.push_str(&format!("  token {token}\n"));
        }
        for (table, integral) in &integrals {
            out.push_str(&format!("input {table}\n"));
            out.push_str(&integral.canonical().map_err(|e| e.to_string())?.render());
        }
        Ok(out)
    }

    /// The outcome so far, for comparison.
    pub fn outcome(&self) -> Result<RunOutcome, String> {
        Ok(RunOutcome {
            fingerprints: vec![self
                .circuit
                .state_fingerprint()
                .map_err(|e| e.to_string())?],
            answers: vec![match self.circuit.answer() {
                Ok(answer) => answer.render(),
                Err(e) => format!("ERROR: {e}"),
            }],
            log: self.log_meaning()?,
            epoch: self.circuit.epoch(),
            accepted_tokens: Vec::new(),
            bootstrapped: self.bootstrapped,
        })
    }

    #[must_use]
    pub fn epoch(&self) -> u64 {
        self.circuit.epoch()
    }

    #[must_use]
    pub fn log_epoch(&self) -> u64 {
        self.log.sealed_epoch()
    }

    #[must_use]
    pub fn checkpoint_root(&self) -> PathBuf {
        ckpt_dir(&self.dir)
    }

    #[must_use]
    pub fn tables(&self) -> &[(String, Schema)] {
        &self.tables
    }

    #[must_use]
    pub fn query(&self) -> &schweep_plan::Query {
        &self.query
    }

    #[must_use]
    pub fn known_tokens(&self) -> usize {
        self.log.known_tokens()
    }

    /// Whether recovery rebuilt this circuit from the snapshot instead of a checkpoint.
    #[must_use]
    pub fn bootstrapped(&self) -> bool {
        self.bootstrapped
    }

    #[must_use]
    pub fn retained_from(&self) -> u64 {
        self.log.retained_from()
    }
}

/// Run a scenario cleanly to the end, returning the outcome after every epoch.
pub fn run_clean(
    dir: impl AsRef<Path>,
    scenario: &Scenario,
    config: Config,
) -> Result<RunOutcome, String> {
    let mut faults = FaultInjector::inert();
    let mut durable = Durable::open(&dir, scenario, &mut faults, config)?;
    let mut fingerprints = Vec::new();
    let mut answers = Vec::new();
    for (index, deltas) in scenario.epochs.iter().enumerate() {
        durable.apply_epoch(index, deltas, &mut faults, config)?;
        let out = durable.outcome()?;
        fingerprints.extend(out.fingerprints);
        answers.extend(out.answers);
    }
    Ok(RunOutcome {
        fingerprints,
        answers,
        log: durable.log_meaning()?,
        epoch: durable.epoch(),
        accepted_tokens: Vec::new(),
        bootstrapped: durable.bootstrapped(),
    })
}

/// Run a scenario with a fault, then recover and finish. Returns the recovered outcome and which
/// fault actually fired.
pub fn run_with_fault(
    dir: impl AsRef<Path>,
    scenario: &Scenario,
    fault: Fault,
    config: Config,
) -> Result<(RunOutcome, Option<&'static str>), String> {
    let dir = dir.as_ref().to_path_buf();
    let mut injector = match fault {
        Fault::Seam(plan) => FaultInjector::planned(plan),
        _ => FaultInjector::inert(),
    };

    // Phase 1: run until the fault fires or the scenario ends. Every error is treated as a crash:
    // the objects are dropped and nothing in memory survives.
    //
    // Epochs already sealed on disk are skipped. Without that, a run over a recovered directory
    // re-seals every epoch and steps it again — the batches are dropped as replays by the log, but
    // the circuit would still be stepped twice and every weight would double. The idempotency test
    // found exactly that.
    let mut crashed_at: Option<usize> = None;
    {
        match Durable::open(&dir, scenario, &mut injector, config) {
            Ok(mut durable) => {
                for (index, deltas) in scenario.epochs.iter().enumerate() {
                    if (index as u64) < durable.log_epoch() {
                        continue;
                    }
                    if durable
                        .apply_epoch(index, deltas, &mut injector, config)
                        .is_err()
                    {
                        crashed_at = Some(index);
                        break;
                    }
                }
            }
            Err(_) => crashed_at = Some(0),
        }
        // `durable` is dropped here: every in-memory object is gone, which is the information loss a
        // process death causes.
    }

    // A byte-boundary fault is applied to what is on disk, after the run.
    if let Fault::Bytes {
        epoch_index,
        offset,
        truncate,
    } = fault
    {
        let epochs = checkpoint::published_epochs(ckpt_dir(&dir)).map_err(|e| e.to_string())?;
        if let Some(epoch) = epochs.get(epoch_index % epochs.len().max(1)) {
            let _ = checkpoint::corrupt_for_test(ckpt_dir(&dir), *epoch, offset, truncate);
        }
    }

    // Phase 2: recover, still carrying the fault plan. A recovery seam can only fire here — the log
    // is empty when phase 1 opens, so nothing is replayed then — and that is what makes "crash
    // during recovery" reachable at all. A failure here is another crash, and is expected.
    let _ = Durable::open(&dir, scenario, &mut injector, config);

    // What actually fired. Captured *before* the final injector shadows it — the first version of
    // this function returned the final, inert injector's `fired()`, so every cycle reported "no
    // fault" and the gate's fault-count assertion caught it on the first run. That assertion exists
    // for exactly this.
    let fired = injector.fired().map(Seam::name);

    // Phase 3: recover for real and finish the scenario from wherever the log left off.
    let mut injector = FaultInjector::inert();
    let mut durable = Durable::open(&dir, scenario, &mut injector, config)?;
    let mut fingerprints = Vec::new();
    let mut answers = Vec::new();

    // Everything the log already sealed is replayed by `open`; re-offer every epoch so that a batch
    // whose ack was lost is retried with the same token and dropped as a replay (I-4, A3).
    for (index, deltas) in scenario.epochs.iter().enumerate() {
        if (index as u64) < durable.log_epoch() {
            continue;
        }
        durable.apply_epoch(index, deltas, &mut injector, config)?;
    }
    // Re-offer the already-sealed epochs' batches too, to prove the dedup path drops them.
    for (index, deltas) in scenario.epochs.iter().enumerate() {
        for (table, entries) in deltas.tables() {
            let token = format!("epoch-{index}-{table}");
            let ack = durable
                .log
                .append("src", table, entries.clone(), &token, &mut injector)
                .map_err(|e| e.to_string())?;
            if ack != Ack::DroppedAsReplay {
                return Err(format!(
                    "re-offering token {token:?} was not dropped as a replay; exactly-once ingest \
                     is broken (I-4)"
                ));
            }
        }
    }

    let out = durable.outcome()?;
    fingerprints.extend(out.fingerprints);
    answers.extend(out.answers);
    let _ = crashed_at;
    Ok((
        RunOutcome {
            fingerprints,
            answers,
            log: out.log,
            epoch: durable.epoch(),
            accepted_tokens: Vec::new(),
            bootstrapped: durable.bootstrapped(),
        },
        fired,
    ))
}

/// The seam a plan would fire at, for reporting.
#[must_use]
pub fn planned_seam(plan: FaultPlan) -> &'static str {
    plan.seam.name()
}
