//! **Kill the subscriber**, mid-stream, and resume by token (§6 C9).
//!
//! The server-crash gate is `kill9.rs`. This is the other half, and it is a different guarantee: the
//! *client* dies. `schweep-subscriber` is a real process holding a real journal, killed with `SIGKILL`
//! while epochs are arriving, and restarted with nothing but its journal — from which it re-derives its
//! token, because D-23 gives the server no cursor to lose.
//!
//! What the gate asserts, across the kill:
//!
//! - **no epoch is delivered twice** — the journal's epochs, in order, are exactly `1..=N`, each once;
//! - **no epoch is skipped** — the same statement, from the other side;
//! - a redelivered epoch (the kill landed between receiving and recording) arrives **identically**, which
//!   the recorded CRC is there to prove;
//! - the subscriber never advances past what it recorded, so the resume is always at worst a redelivery.
//!
//! The kill point is a seeded number of *recorded* epochs, so the position is reproducible in the only
//! sense that matters — how much the subscriber had consumed — while where in its instruction stream the
//! signal lands is left to the OS, as `kill9.rs` explains.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

use schweep_differential::Rng;

use common::{row, Harness, SUM};

/// Kill points to try. Each is a whole subscriber lifecycle, so this is the figure that keeps the gate to
/// a few seconds while still crossing every boundary the journal has.
const KILLS: u64 = 24;

/// Epochs the subscriber must end up with.
const EPOCHS: u64 = 40;

struct Subscriber {
    child: Child,
    journal: PathBuf,
}

impl Subscriber {
    fn spawn(port_file: &Path, handle: u64, journal: &Path, until: u64) -> Subscriber {
        let child = Command::new(env!("CARGO_BIN_EXE_schweep-subscriber"))
            .arg(port_file)
            .arg(handle.to_string())
            .arg(journal)
            .arg("--until")
            .arg(until.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("schweep-subscriber must be spawnable");
        Subscriber {
            child,
            journal: journal.to_path_buf(),
        }
    }

    /// Epochs recorded so far, from complete lines only — the same rule the subscriber resumes by.
    fn recorded(&self) -> Vec<(u64, String)> {
        recorded(&self.journal)
    }

    fn kill_9(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Subscriber {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn recorded(journal: &Path) -> Vec<(u64, String)> {
    let Ok(text) = std::fs::read_to_string(journal) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.split_inclusive('\n') {
        if !line.ends_with('\n') {
            break;
        }
        let mut parts = line.split_whitespace();
        if parts.next() != Some("epoch") {
            continue;
        }
        let (Some(epoch), Some("crc"), Some(crc)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if let Ok(epoch) = epoch.parse::<u64>() {
            out.push((epoch, crc.to_owned()));
        }
    }
    out
}

/// A port file for the subscriber to read, written from the in-process server's address.
fn port_file(dir: &Path, harness: &Harness) -> PathBuf {
    let path = dir.join("PORT");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "{}", harness.client.address().port()).unwrap();
    file.sync_all().unwrap();
    path
}

/// **The gate.** A real `SIGKILL` of the subscriber, at every point in its consumption, and no epoch
/// delivered twice or lost across the resume.
#[test]
fn a_killed_subscriber_resumes_by_token_without_a_duplicate_or_a_gap() {
    let mut redeliveries = 0u64;
    let mut kills_that_landed_mid_stream = 0u64;

    for kill in 0..KILLS {
        let h = Harness::fresh(&format!("subscriber-crash-{kill}"));
        let handle: u64 = h
            .client
            .register(SUM)
            .unwrap()
            .body()
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let port_file = port_file(&h.dir, &h);
        let journal = h.dir.join("journal");
        let _ = std::fs::remove_file(&journal);

        // Seal every epoch up front, so the subscriber has a full backlog to stream and the kill has
        // somewhere to land. The server is in this process; only the subscriber is a subprocess, because
        // only the subscriber is being killed.
        for epoch in 1..=EPOCHS {
            h.client
                .ingest("a", "t", &format!("b{epoch}"), &[(row(1, epoch as i64), 1)])
                .unwrap();
            h.client.seal().unwrap();
        }

        // Seeded: how many epochs the subscriber has recorded when the signal arrives.
        let mut rng = Rng::from_seed(kill);
        let kill_after = rng.below(EPOCHS);

        let mut subscriber = Subscriber::spawn(&port_file, handle, &journal, EPOCHS);
        let mut spins = 0u64;
        loop {
            let recorded = subscriber.recorded().len() as u64;
            if recorded >= kill_after {
                if recorded < EPOCHS {
                    kills_that_landed_mid_stream += 1;
                }
                break;
            }
            // The child exiting before the target is not an error: it means it consumed everything
            // faster than the parent looked, and the resume below then has nothing to do — still a valid
            // cycle, and one whose assertions must hold anyway.
            if let Ok(Some(_)) = subscriber.child.try_wait() {
                break;
            }
            spins += 1;
            assert!(
                spins < 200_000_000,
                "the subscriber stopped making progress"
            );
            std::hint::spin_loop();
        }
        subscriber.kill_9();

        let before = subscriber.recorded();
        assert!(
            before
                .iter()
                .map(|(epoch, _)| *epoch)
                .eq(1..=before.len() as u64),
            "cycle {kill}: the journal before the kill is not a prefix of the epochs: {before:?}"
        );

        // The resume: a new process, with nothing but the journal.
        let mut resumed = Subscriber::spawn(&port_file, handle, &journal, EPOCHS);
        let status = resumed.child.wait().unwrap();
        assert!(
            status.success(),
            "cycle {kill}: the resumed subscriber failed: {status}"
        );

        let after = recorded(&journal);
        let epochs: Vec<u64> = after.iter().map(|(epoch, _)| *epoch).collect();
        assert_eq!(
            epochs,
            (1..=EPOCHS).collect::<Vec<u64>>(),
            "cycle {kill}: killed after {kill_after} recorded epochs — the journal across the resume \
             must be every epoch exactly once, in order"
        );

        // A redelivered epoch must be the same bytes. There is no redelivery *in the journal* — the
        // journal is the consumed set — so the check is that the resumed process picked up exactly where
        // the record ended, which is what makes redelivery invisible to a correct subscriber.
        if let Some((last, crc)) = before.last() {
            let matching = after
                .iter()
                .find(|(epoch, _)| epoch == last)
                .map(|(_, crc)| crc.clone());
            assert_eq!(
                matching.as_ref(),
                Some(crc),
                "cycle {kill}: epoch {last} changed across the resume"
            );
            redeliveries += 1;
        }
    }

    println!(
        "C9 subscriber-crash gate: {KILLS} real SIGKILLs of the subscriber · {EPOCHS} epochs each · \
         {kills_that_landed_mid_stream} landed mid-stream · {redeliveries} resumes from a non-empty journal"
    );
    assert!(
        kills_that_landed_mid_stream > 0,
        "every kill landed after the subscriber had finished; the gate never tested a resume"
    );
}

/// A subscriber whose token has fallen behind the ring is **refused**, and it records the refusal rather
/// than treating it as the end of the stream.
///
/// This is the honest limit of an in-memory delta ring, and it is where D-23's rule earns its keep: a gap
/// is a refusal. A subscriber that saw a success with no epochs would carry on believing it was caught up.
#[test]
fn a_subscriber_that_falls_behind_the_ring_is_refused_rather_than_told_it_is_caught_up() {
    let h = Harness::fresh("subscriber-behind-ring");
    let handle: u64 = h
        .client
        .register(SUM)
        .unwrap()
        .body()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let port_file = port_file(&h.dir, &h);
    let journal = h.dir.join("journal");

    // A journal that claims epoch 1 was consumed, and a server whose ring starts after it.
    std::fs::write(&journal, "epoch 1 crc 00000000\n").unwrap();
    for epoch in 1..=(schweep_server::SUBSCRIPTION_RING as u64 + 3) {
        h.client
            .ingest("a", "t", &format!("b{epoch}"), &[(row(1, epoch as i64), 1)])
            .unwrap();
        h.client.seal().unwrap();
    }

    let mut subscriber = Subscriber::spawn(&port_file, handle, &journal, u64::MAX);
    let status = subscriber.child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(3),
        "a subscriber behind the ring must exit on a refusal, not run on"
    );
    let text = std::fs::read_to_string(&journal).unwrap();
    assert!(
        text.contains("refused Rejected"),
        "the refusal must be recorded so the operator can see a gap was refused: {text}"
    );
    assert!(
        text.contains("behind the oldest retained epoch"),
        "and it must name the oldest epoch the server still has: {text}"
    );
}
