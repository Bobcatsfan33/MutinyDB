//! **THE REAL `kill -9`** — a subprocess `schweepd`, killed under load (§6 C9, discharging C4's pointer).
//!
//! C4 built 26 named kill seams and ran ten thousand cycles through them, and then said out loud what it
//! could not reach: the process never actually died. `SIGKILL` at an arbitrary machine instruction is not a
//! seam, and the difference is the whole class of bugs that live between "the write returned" and "the
//! write is on the disk" — including the one C4 named as unreachable: **an acknowledgement sent before the
//! durable append returns**. That mutation is observable here and nowhere else in this repository.
//!
//! ## What each cycle does
//!
//! 1. Spawn `schweepd` on a fresh data directory, wait for the port file it writes **after** binding.
//! 2. Register a standing query, then run three threads at it: one appending and sealing, one reading, one
//!    subscribing. Every append's token is recorded the moment the server acknowledges it.
//! 3. `SIGKILL` the process — not `/shutdown`, not a flag — after a seeded number of acknowledged appends,
//!    while all three threads are still going.
//! 4. Restart on the same directory, seal once, and assert three things:
//!    - **I-4**: every token the server acknowledged before the kill is applied in exactly one epoch. The
//!      workload makes this readable: token *i* appends the single row `(i, 1)`, so the answer must hold
//!      `(i, 1) => 1` — a batch applied twice would read `(i, 2) => 1`.
//!    - **I-7**: the recovered state equals a **never-crashed twin** — a fresh engine fed the recovered
//!      log's epochs, in order, one seal each — **byte for byte, emission counters included.** C4's crash
//!      gate had to strip the I-9 counters on its bootstrap cycles, because a bootstrap reaches the same
//!      state by emitting the whole history as one delta. `schweepd` does not bootstrap that way: catch-up
//!      replays the log an epoch at a time (`Memo::register_from_chunks`), which is the same sequence of
//!      passes the live path took, so there is nothing left to relax.
//!    - the recovered epoch is the log's, and no answer is served from a partial epoch (I-3).
//!
//! ## What is seeded, and what is honestly not
//!
//! Seeded: the workload (which rows, in which order), how many acknowledged appends precede the kill, and
//! the read/subscribe cadence. **Not** seeded, and deliberately: where in the server's own instruction
//! stream the kill lands. That is the property under test. Every assertion here is therefore written to
//! hold for *any* kill position, which is what makes a nondeterministic kill point compatible with the
//! zero-flake rule — a flake would mean an assertion that depends on timing, and that would be the bug.
//!
//! ## What this retires, and what it does not
//!
//! It retires "the process never really dies" (`docs/DURABILITY.md` §7). It does **not** retire power
//! loss: `SIGKILL` leaves the kernel's page cache intact, so a write this process made is still visible to
//! the next one even if no `fsync` had returned. Torn media, a lying disk cache, and a power cut remain
//! untested, and DURABILITY.md still says so.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use schweep_differential::Rng;
use schweep_log::{FaultInjector, Log, SyncPolicy};
use schweep_memo::{Admission, Memo, Sharing};
use schweep_server::{Client, Engine, Policy};
use schweep_zset::{EpochDeltas, Row, Value};

use common::{catalog, row, SUM};

/// Cycles in the matrix. §6 C9 asks for "at least 1,000 random points"; the default run does exactly that,
/// and `SCHWEEP_KILL9_CYCLES` lowers it for a local loop without changing what the gate means in CI.
const CYCLES: u64 = 1_000;

/// The most appends one cycle makes. Bounded so the answer stays small enough to compare cheaply, and so a
/// cycle that is never killed still terminates.
const MAX_APPENDS: u64 = 48;

/// A `schweepd` subprocess and the client that reaches it.
struct Subprocess {
    child: Child,
    client: Client,
}

impl Subprocess {
    /// Spawn on `dir` and return once the server is accepting connections.
    ///
    /// Readiness is the port file the child writes *after* it binds, plus a connect that succeeds. Neither
    /// is a sleep: both are the event itself.
    fn spawn(dir: &Path, catalog_file: &Path) -> Subprocess {
        let port_file = dir.join("PORT");
        let _ = std::fs::remove_file(&port_file);
        std::fs::create_dir_all(dir).unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_schweepd"))
            .arg(dir)
            .arg(catalog_file)
            .arg("--port-file")
            .arg(&port_file)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("schweepd must be spawnable");

        let mut child = child;
        let mut spins = 0u64;
        loop {
            if let Some(port) = std::fs::read_to_string(&port_file)
                .ok()
                .and_then(|text| text.trim().parse::<u16>().ok())
            {
                let client = Client::new(([127, 0, 0, 1], port).into());
                if client.reachable() {
                    return Subprocess { child, client };
                }
            }
            // A child that died before binding is a failure to report, not a loop to spin in.
            if let Ok(Some(status)) = child.try_wait() {
                panic!("schweepd exited before binding: {status}");
            }
            spins += 1;
            assert!(
                spins < 2_000_000,
                "schweepd never bound a port; something is wrong with the binary, not the timing"
            );
            std::hint::spin_loop();
        }
    }

    /// `SIGKILL`. Not a shutdown, not a flag — the whole point.
    fn kill_9(&mut self) {
        self.child.kill().expect("the child must be killable");
        let _ = self.child.wait();
    }
}

impl Drop for Subprocess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn write_catalog(dir: &Path) -> PathBuf {
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join("catalog.txt");
    let mut file = std::fs::File::create(&path).unwrap();
    file.write_all(b"t: k:int:notnull, n:int:notnull\n")
        .unwrap();
    file.sync_all().unwrap();
    path
}

/// The workload one cycle runs, and what it acknowledged.
struct Load {
    /// Tokens the server acknowledged. Only these carry an I-4 obligation.
    acked: Arc<Mutex<Vec<u64>>>,
    /// Acknowledged appends so far — what the kill point counts.
    progress: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    /// Every epoch the subscriber thread was delivered, in delivery order. The exactly-once claim on the
    /// read side is a statement about this list.
    delivered: Arc<Mutex<Vec<u64>>>,
    /// The token the subscriber thread had reached, so the resume after recovery starts where it stopped.
    token: Arc<AtomicU64>,
}

impl Load {
    fn new() -> Load {
        Load {
            acked: Arc::new(Mutex::new(Vec::new())),
            progress: Arc::new(AtomicU64::new(0)),
            stop: Arc::new(AtomicBool::new(false)),
            delivered: Arc::new(Mutex::new(Vec::new())),
            token: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Run appends, reads and subscribes concurrently until the load is stopped or the appends run out.
fn drive(client: &Client, handle: u64, load: &Load) -> Vec<std::thread::JoinHandle<()>> {
    let mut threads = Vec::new();

    // The ingest thread: token `i` appends the single row `(i, 1)`, and every fourth append seals.
    {
        let client = client.clone();
        let acked = Arc::clone(&load.acked);
        let progress = Arc::clone(&load.progress);
        let stop = Arc::clone(&load.stop);
        threads.push(std::thread::spawn(move || {
            for i in 1..=MAX_APPENDS {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let entries = [(Row::new(vec![Value::Int(i as i64), Value::Int(1)]), 1i64)];
                match client.ingest("load", "t", &format!("t{i}"), &entries) {
                    // The token is recorded **after** the acknowledgement and before anything else, so the
                    // recorded set is exactly the set the server promised.
                    Ok(response) if response.is_ok() => {
                        acked.lock().unwrap().push(i);
                        progress.fetch_add(1, Ordering::SeqCst);
                    }
                    // A killed server refuses connections; that is the cycle ending, not a failure.
                    Ok(_) | Err(_) => return,
                }
                if i % 4 == 0 && client.seal().is_err() {
                    return;
                }
            }
        }));
    }

    // A reader, so the kill lands while the server is answering as well as ingesting.
    {
        let client = client.clone();
        let stop = Arc::clone(&load.stop);
        threads.push(std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if client.read(handle).is_err() {
                    return;
                }
            }
        }));
    }

    // A subscriber, recording every epoch it is delivered and the token it reached. The token is
    // published **after** the epochs are recorded, which is the same discipline `schweep-subscriber` uses
    // and for the same reason: a cursor that moves before the data is consumed can lose an epoch.
    {
        let client = client.clone();
        let stop = Arc::clone(&load.stop);
        let delivered = Arc::clone(&load.delivered);
        let token = Arc::clone(&load.token);
        threads.push(std::thread::spawn(move || {
            let mut cursor = 0u64;
            while !stop.load(Ordering::SeqCst) {
                let Ok(response) = client.subscribe(handle, cursor) else {
                    return;
                };
                let Ok(body) = response.body() else { return };
                let epochs = epochs_in(body);
                if !epochs.is_empty() {
                    delivered.lock().unwrap().extend(epochs.iter().copied());
                }
                if let Some(next) = body
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("token "))
                    .and_then(|n| n.parse().ok())
                {
                    cursor = next;
                    token.store(cursor, Ordering::SeqCst);
                }
            }
        }));
    }
    threads
}

/// The epochs a `/subscribe` body delivered.
fn epochs_in(body: &str) -> Vec<u64> {
    body.lines()
        .skip(2)
        .filter_map(|line| line.strip_prefix("epoch ")?.trim().parse().ok())
        .collect()
}

/// The never-crashed twin: a fresh engine fed the recovered log's epochs, in order, one seal each.
///
/// This is I-7's own wording — "byte-identical to a process that never crashed" — made checkable. The
/// input history is the log, so a twin that saw the log epoch by epoch *is* that process.
fn twin_fingerprint(log_dir: &Path, sealed: u64) -> String {
    let mut faults = FaultInjector::inert();
    let log = Log::open(log_dir, catalog(), &mut faults, SyncPolicy::Deferred).unwrap();
    let mut memo = Memo::with_sharing(catalog(), Sharing::On).unwrap();
    let plan = schweep_sql::compile(SUM, &catalog()).unwrap();
    memo.register(&plan, Admission::bounded()).unwrap();
    for epoch in 1..=sealed {
        let mut deltas = EpochDeltas::new();
        for batch in log.epoch(epoch).unwrap() {
            deltas.extend(batch.table.clone(), batch.entries.iter().cloned());
        }
        memo.seal_epoch(&deltas).unwrap();
    }
    memo.dataflow().state_fingerprint().unwrap()
}

fn cycles() -> u64 {
    std::env::var("SCHWEEP_KILL9_CYCLES")
        .ok()
        .and_then(|raw| raw.parse().ok())
        .unwrap_or(CYCLES)
}

/// **The gate.** A real `SIGKILL`, at a thousand points, under concurrent ingest, read and subscribe.
#[test]
fn a_killed_schweepd_recovers_exactly_once_and_matches_its_never_crashed_twin() {
    let root = std::env::temp_dir().join(format!("schweep-c9-kill9-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let catalog_file = write_catalog(&root);

    let cycles = cycles();
    let mut killed_before_any_ack = 0u64;
    let mut acked_total = 0u64;
    let mut epochs_recovered = 0u64;
    let mut cycles_with_a_pending_append = 0u64;
    let mut subscriber_epochs = 0u64;
    let mut refused_resumes = 0u64;

    for cycle in 0..cycles {
        let dir = root.join(format!("cycle-{cycle}"));
        let _ = std::fs::remove_dir_all(&dir);

        // Seeded: the kill point, as a count of acknowledged appends. Not seeded: where in the server's
        // own work that acknowledgement number lands.
        let mut rng = Rng::from_seed(cycle);
        let kill_after = rng.below(MAX_APPENDS);

        let mut server = Subprocess::spawn(&dir, &catalog_file);
        let handle: u64 = server
            .client
            .register(SUM)
            .unwrap()
            .body()
            .unwrap()
            .trim()
            .parse()
            .unwrap();

        let load = Load::new();
        let threads = drive(&server.client, handle, &load);

        // Wait for the workload to reach the kill point by *asking the counter*, not by sleeping. A cycle
        // whose target is 0 is killed immediately, which is a real and interesting position: mid-register,
        // mid-first-append, or before either.
        let mut spins = 0u64;
        while load.progress.load(Ordering::SeqCst) < kill_after {
            spins += 1;
            assert!(
                spins < 200_000_000,
                "the load thread stopped making progress"
            );
            std::hint::spin_loop();
        }
        server.kill_9();

        load.stop.store(true, Ordering::SeqCst);
        for thread in threads {
            let _ = thread.join();
        }
        let acked = load.acked.lock().unwrap().clone();
        let delivered = load.delivered.lock().unwrap().clone();
        let subscriber_token = load.token.load(Ordering::SeqCst);
        if acked.is_empty() {
            killed_before_any_ack += 1;
        }
        acked_total += acked.len() as u64;

        // **No epoch delivered twice, before the kill.** The subscriber advanced its cursor only after
        // recording, so its delivery history must be strictly increasing.
        for pair in delivered.windows(2) {
            if let [previous, next] = pair {
                assert!(
                    next > previous,
                    "cycle {cycle}: the subscriber was delivered epoch {next} after {previous}; \
                     delivery must be strictly increasing, and a repeat is a duplicate\n{delivered:?}"
                );
            }
        }
        if let Some(last) = delivered.last() {
            subscriber_epochs += delivered.len() as u64;
            assert!(
                *last <= subscriber_token,
                "cycle {cycle}: epoch {last} was delivered but the token only reached \
                 {subscriber_token}"
            );
        }

        // --- recovery -------------------------------------------------------------------------------
        let recovered = Subprocess::spawn(&dir, &catalog_file);
        let health = recovered
            .client
            .health()
            .unwrap()
            .body()
            .unwrap()
            .to_owned();
        if health.contains("pending_appends") && !health.contains("pending_appends 0") {
            cycles_with_a_pending_append += 1;
        }

        // One seal, so every durable-but-unsealed append lands in an epoch. Nothing is invented here: the
        // appends were already acknowledged, and this is the epoch that was going to contain them.
        recovered.client.seal().unwrap();

        let answer = recovered.client.answer(handle).unwrap().unwrap();
        let epoch = recovered.client.epoch_of(handle).unwrap().unwrap();
        epochs_recovered += epoch;

        // **No epoch delivered twice, across the kill.** Resuming at the token the subscriber reached must
        // either deliver only epochs *after* it, or be refused because the ring did not survive the
        // restart (D-23's addendum: the deltas are not durable, the answer is). What must never happen is
        // an epoch the subscriber already consumed arriving again.
        let resumed = recovered
            .client
            .subscribe(handle, subscriber_token)
            .unwrap();
        match resumed.body() {
            Ok(body) => {
                for delivered_again in epochs_in(body) {
                    assert!(
                        delivered_again > subscriber_token,
                        "cycle {cycle}: resuming at token {subscriber_token} redelivered epoch \
                         {delivered_again}, which the subscriber had already consumed"
                    );
                }
            }
            Err((kind, message)) => {
                assert_eq!(
                    kind,
                    schweep_server::ErrorKind::Rejected,
                    "cycle {cycle}: a resume that cannot be served must be Rejected, not {kind:?}: \
                     {message}"
                );
                refused_resumes += 1;
            }
        }

        // **I-4.** Every acknowledged token, in exactly one epoch.
        for token in &acked {
            let expected = format!("({token}, 1) => 1\n");
            assert!(
                answer.contains(&expected),
                "cycle {cycle}: token t{token} was acknowledged before the kill and is not in the \
                 recovered answer exactly once.\nkill after {kill_after} acks, recovered at epoch \
                 {epoch}\nanswer:\n{answer}"
            );
        }
        // A doubled application would read `(i, 2)`; naming the shape makes the failure legible even if
        // the row for a token is missing for a different reason.
        for token in &acked {
            assert!(
                !answer.contains(&format!("({token}, 2) => 1")),
                "cycle {cycle}: token t{token} was applied twice — I-4 is broken.\nanswer:\n{answer}"
            );
        }

        // **I-7.** The recovered state equals a twin that saw the same log and never crashed — the full
        // fingerprint, counters included. If this ever needs relaxing, the reason is a change in how
        // catch-up feeds the history, and the change is the thing to look at first.
        let fingerprint = recovered
            .client
            .fingerprint()
            .unwrap()
            .body()
            .unwrap()
            .to_owned();
        drop(recovered);
        let twin = twin_fingerprint(&dir.join("log"), epoch);
        assert_eq!(
            fingerprint, twin,
            "cycle {cycle}: the recovered state is not the never-crashed twin's state at epoch {epoch}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    println!(
        "C9 kill -9 matrix: {cycles} real SIGKILLs · {acked_total} acknowledged appends verified \
         exactly-once · {epochs_recovered} epochs recovered"
    );
    println!(
        "  cycles killed before any acknowledgement: {killed_before_any_ack} · cycles that recovered a \
         durable-but-unsealed append: {cycles_with_a_pending_append}"
    );
    println!(
        "  {subscriber_epochs} epochs delivered to the in-process subscriber, none twice · \
         {refused_resumes} resumes refused because the ring did not survive the restart"
    );

    // A matrix that never reached the interesting positions is not a matrix. Both of these are *positions*
    // the kill must have landed in, not timings, so asserting them is a statement about coverage.
    assert!(
        acked_total > cycles,
        "the workload barely ran: {acked_total} acknowledged appends across {cycles} cycles"
    );
    assert!(
        subscriber_epochs > 0,
        "the subscriber was never delivered anything, so the no-duplicate claim is about nothing"
    );
    assert!(
        cycles_with_a_pending_append > 0,
        "no cycle was killed between an acknowledgement and a seal, which is the position the \
         'ack before the durable append' mutation lives at"
    );
}

/// The counters, answers and epoch a recovered server reports must agree with each other.
///
/// Cheap, and it catches the recovery that half-works: a server that reports an epoch it cannot answer at,
/// or an answer at an epoch the log does not have.
#[test]
fn a_recovered_server_agrees_with_its_own_log() {
    let root = std::env::temp_dir().join(format!("schweep-c9-agree-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let catalog_file = write_catalog(&root);
    let dir = root.join("data");

    let mut server = Subprocess::spawn(&dir, &catalog_file);
    let handle: u64 = server
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    for i in 1..=6u64 {
        server
            .client
            .ingest("a", "t", &format!("t{i}"), &[(row(i as i64, 1), 1)])
            .unwrap();
        if i % 2 == 0 {
            server.client.seal().unwrap();
        }
    }
    server.kill_9();

    let recovered = Subprocess::spawn(&dir, &catalog_file);
    let epoch = recovered.client.epoch_of(handle).unwrap().unwrap();
    assert_eq!(
        epoch, 3,
        "three seals completed, so recovery must be at epoch 3 — not at the appends' count"
    );
    let answer = recovered.client.answer(handle).unwrap().unwrap();
    let rows: BTreeMap<i64, i64> = answer
        .lines()
        .filter_map(|line| {
            let inner = line.strip_prefix('(')?;
            let (k, rest) = inner.split_once(", ")?;
            let (s, _) = rest.split_once(')')?;
            Some((k.parse().ok()?, s.parse().ok()?))
        })
        .collect();
    assert_eq!(
        rows,
        (1..=6).map(|k| (k, 1)).collect::<BTreeMap<i64, i64>>(),
        "every append was acknowledged and sealed, so all six keys must be present exactly once"
    );

    // And the same directory opened in-process reads the same thing — the embedded engine and the server
    // are one engine, which is what makes the network door the same door (I-6).
    drop(recovered);
    let engine = Engine::open(&dir, catalog(), Policy::default(), SyncPolicy::Deferred, 8).unwrap();
    let (embedded_epoch, embedded) = engine.read(handle).unwrap();
    assert_eq!(embedded_epoch, epoch);
    assert_eq!(
        embedded.lines().skip(1).collect::<Vec<&str>>(),
        answer.lines().skip(1).collect::<Vec<&str>>()
    );

    let _ = std::fs::remove_dir_all(&root);
}
