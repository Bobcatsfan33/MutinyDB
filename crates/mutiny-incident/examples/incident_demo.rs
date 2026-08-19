//! **The M4 incident demo** — the frozen corpus, end to end, narrated for a stranger.
//!
//! Dev-only. This is NOT the supported `mutinyd` binary (that is M6, and it does not exist yet);
//! it composes quarantined component trees for development and demonstration, and running it
//! changes no component's release admission. Deterministic: two runs print identical bytes.
//!
//! ```sh
//! cargo run -p mutiny-incident --example incident_demo
//! ```

use loom_core::SourceRef;
use mutiny_incident::corpus;
use mutiny_incident::host::{Host, HostPaths};

const HELP: &str = "\
incident_demo — MutinyDB's M4 flagship moment, on the frozen incident corpus.

  A dev-only example. NOT the supported mutinyd binary; nothing here is a release.

  It ingests the corpus through the real write path (substrate commit + Loom envelope +
  bridge audit -> one compute epoch), shows the standing answers staying current, executes
  one real external action through Loom's gateway, then discovers the poisoned source and
  makes ONE call: taint(web:scraped-page-77). Every plane heals; the recall report leads
  with the one thing no engine can undo.

  Options: --help prints this text. There are no other options; the corpus is frozen.
";

fn main() {
    if std::env::args().any(|arg| arg == "--help" || arg == "-h") {
        print!("{HELP}");
        return;
    }
    if let Err(error) = run() {
        eprintln!("incident_demo failed: {error}");
        std::process::exit(1);
    }
}

fn banner(text: &str) {
    println!("\n────────────────────────────────────────────────────────────────────────");
    println!("{text}");
    println!("────────────────────────────────────────────────────────────────────────");
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    println!("MutinyDB · M4 incident demo (dev-only; NOT the supported mutinyd)");

    let corpus = corpus::parse(corpus::CORPUS)?;
    let storage = tempfile::tempdir()?;
    let compute = tempfile::tempdir()?;
    let paths = HostPaths {
        storage: storage.path().to_path_buf(),
        compute: compute.path().to_path_buf(),
    };

    banner("1 · INGEST — every write takes the real front door");
    println!(
        "A substrate commit with its Loom envelope becomes exactly one compute epoch; the\n\
         derivation relation records what every row was derived from. Two sessions, one\n\
         hypothesis fork, {} writes:",
        corpus.commits.len()
    );
    let mut host = Host::open(&paths, &corpus)?;
    for commit in &corpus.commits {
        host.ingest_commit(commit)?;
        let cited = commit
            .sources
            .iter()
            .map(|source| format!("{}:{}", source.system, source.record_id))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "  epoch {:>2} · [{}] {}/{} ← {}",
            host.commit_seq, commit.branch, commit.table, commit.key, cited
        );
    }
    for action in &corpus.actions {
        let record = host.execute_action(action)?;
        println!(
            "\nAnd one thing that is not a write: the agent proposes, the operator executes.\n\
             ACTION {} on {} — receipt {:?}",
            action.action_type,
            action.target,
            record.receipt().unwrap_or("<none>")
        );
    }

    banner("2 · STANDING ANSWERS — continuously current, no recomputation");
    println!(
        "Memory claims, analytical rollups, and branch-scoped semantic rankings — every one\n\
         a standing computation, maintained at O(change):\n"
    );
    print!("{}", host.standing_answers()?);

    banner("3 · THE POISON — web:scraped-page-77 was attacker-controlled");
    println!(
        "The compromise claim, the suspicion it seeded on the hypothesis branch, the account\n\
         suspension it justified — all of it is downstream of one scraped page that turned\n\
         out to be a lie. Every other database now answers: \"we don't know what it touched.\""
    );

    banner("4 · ONE CALL — taint(web:scraped-page-77)");
    let outcome = host.taint(&SourceRef::new("web", "scraped-page-77"))?;
    println!(
        "Resolved through the mutiny_derivation standing relation ({} rounds — the two-hop\n\
         claim was found by query, not by a graph walk), journaled, and retracted through\n\
         the ordinary delta path:\n",
        outcome.resolution_rounds
    );
    for receipt in &outcome.receipts {
        println!(
            "  retract {} → epoch {} ({} rows)",
            receipt.channel,
            receipt
                .receipt
                .sealed_epoch
                .map_or("none".to_owned(), |epoch| epoch.to_string()),
            receipt.receipt.rows
        );
    }
    println!("\nThe report. The thing no engine can undo is printed first:\n");
    print!("{}", outcome.report);

    banner("5 · HEALED — every standing answer corrected itself");
    println!(
        "The same propagation that keeps dashboards current has already repaired them. The\n\
         bystander session and the clean sources are untouched:\n"
    );
    print!("{}", host.standing_answers()?);

    println!(
        "\nThat is taint-as-retraction: un-touching what one poisoned source touched,\n\
         database-wide, and saying honestly what could not be un-touched. The M4 gate\n\
         (crates/mutiny-incident/tests/m4_gate.rs) proves this world equals one that never\n\
         ingested the poison — byte for byte."
    );
    Ok(())
}
