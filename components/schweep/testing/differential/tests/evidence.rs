//! The ledger's receipts must still describe the thing they justify (I-10).
//!
//! `testing/evidence/registry.json` records the scenario generator's tuned constants, and
//! `testing/evidence/c0-generator-coverage.json` is the measurement that justifies them. A
//! committed artifact is only evidence while it is still true; the moment the generator changes
//! and the artifact does not, the ledger is decoration.
//!
//! So the numbers are recomputed here and compared byte for byte. If this fails, either the
//! generator changed on purpose — in which case regenerate the artifact and re-read the
//! constants' justifications, because the reason for them may have changed too — or it changed by
//! accident, which is what this test is for.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_differential::coverage::{measure, ARTIFACT_SEEDS};

const ARTIFACT: &str = include_str!("../../evidence/c0-generator-coverage.json");
const REGISTRY: &str = include_str!("../../evidence/registry.json");
const STATE_COSTS: &str = include_str!("../../evidence/c8-state-costs.json");
const C9_BOUNDS: &str = include_str!("../../evidence/c9-bounds.json");
/// Every artifact the ledger is allowed to cite. A constant citing anything else is citing a file that
/// nobody committed.
const ARTIFACTS: &[&str] = &[
    "testing/evidence/c0-generator-coverage.json",
    "testing/evidence/c8-state-costs.json",
    "testing/evidence/c8-cache-sweep.json",
    "testing/evidence/c9-bounds.json",
    "testing/evidence/c9-memo-ceiling.json",
    "testing/evidence/c9-soak.json",
    "testing/evidence/c10-residency.json",
];

#[test]
fn the_committed_coverage_artifact_still_matches_the_generator() {
    let measured = measure(ARTIFACT_SEEDS)
        .expect("measuring the generator must succeed")
        .to_json();

    assert_eq!(
        measured, ARTIFACT,
        "\ntesting/evidence/c0-generator-coverage.json no longer describes the generator.\n\
         If the generator changed deliberately, regenerate it with:\n  \
         cargo run -p schweep-differential --bin generator-coverage \
         > testing/evidence/c0-generator-coverage.json\n\
         and re-read the justifications in registry.json — the numbers that motivated those \
         constants have moved.\n"
    );
}

/// Every generator constant in the ledger points at an artifact that exists, and every claim the
/// ledger makes about a measured number matches the artifact.
#[test]
fn every_ledger_entry_cites_the_artifact_that_exists() {
    assert!(
        REGISTRY.contains("c0-generator-coverage.json"),
        "the ledger must cite the coverage artifact"
    );
    // The ledger quotes these two numbers as the reason the join-key domain is narrow. If the
    // artifact moves, the quoted numbers must move with it, or the ledger is telling a story the
    // evidence no longer supports.
    let measured = measure(ARTIFACT_SEEDS).unwrap();
    for quoted in [
        format!(
            "\"join_both_tables_populated\": {}",
            measured.join_both_tables_populated
        ),
        format!(
            "\"join_bare_join_non_empty\": {}",
            measured.join_bare_join_non_empty
        ),
    ] {
        assert!(
            ARTIFACT.contains(&quoted),
            "the artifact should contain {quoted}"
        );
    }
}

/// Every engine constant in the ledger cites an artifact that exists, and every value in the ledger
/// matches the constant in the code (I-10).
///
/// C8 filled this section: the redb page cache, the two state-cost constants `EXPLAIN STATE` reports
/// with, and the soak harness's thresholds. The claim the test used to pin — "nothing is tuned" — is no
/// longer true, so what it pins now is the rule that made it worth pinning: **a constant may not steer
/// behaviour without a receipt, and the receipt must still say what the ledger claims it says.**
#[test]
fn every_engine_constant_cites_an_artifact_and_matches_the_code() {
    let registry: serde_free::Registry = serde_free::parse(REGISTRY);
    assert!(
        !registry.constants.is_empty(),
        "the engine-constant list is empty, but C8 added tuned constants; either they were removed or \
         the ledger lost them"
    );

    for entry in &registry.constants {
        assert!(
            !entry.artifact.is_empty(),
            "{}: a constant may not steer behaviour without a committed artifact (I-10)",
            entry.name
        );
        assert!(
            ARTIFACTS.contains(&entry.artifact.as_str()),
            "{}: cites {:?}, which is not one of the committed artifacts {ARTIFACTS:?}",
            entry.name,
            entry.artifact
        );
        assert!(
            !entry.justification.is_empty() && entry.justification.len() > 80,
            "{}: the justification must say why this value and not another, in terms of a measured \
             number",
            entry.name
        );
        assert!(
            !entry.measured_on.is_empty(),
            "{}: every measurement was taken somewhere, and a figure with no provenance is folklore",
            entry.name
        );
    }

    // The values the ledger records must be the values the code uses. A ledger that drifts from the
    // code is worse than no ledger: it is a receipt for something else.
    let cache = registry.value_of("CACHE_BYTES");
    assert_eq!(
        cache,
        Some(schweep_state::redb_backend::CACHE_BYTES.to_string()),
        "the ledger's CACHE_BYTES does not match the constant in the code"
    );
    assert_eq!(
        registry.value_of("BYTES_PER_ENTRY_LOW"),
        Some(schweep_memo::costs::BYTES_PER_ENTRY_LOW.to_string())
    );
    assert_eq!(
        registry.value_of("BYTES_PER_BACKEND"),
        Some(schweep_memo::costs::BYTES_PER_BACKEND.to_string())
    );
    assert_eq!(
        registry.value_of("WARM_UP_SAMPLES"),
        Some(schweep_soak::Curve::WARM_UP_SAMPLES.to_string())
    );
    assert_eq!(
        registry.value_of("DEFAULT_SOURCE_QUEUE_BOUND"),
        Some(schweep_server::DEFAULT_SOURCE_QUEUE_BOUND.to_string())
    );
    assert_eq!(
        registry.value_of("DEFAULT_SOURCE_QUEUE_BYTES"),
        Some(schweep_server::DEFAULT_SOURCE_QUEUE_BYTES.to_string())
    );
    assert_eq!(
        registry.value_of("SUBSCRIPTION_RING"),
        Some(schweep_server::SUBSCRIPTION_RING.to_string())
    );
    assert_eq!(
        registry.value_of("SUBSCRIPTION_RING_BYTES"),
        Some(schweep_server::SUBSCRIPTION_RING_BYTES.to_string())
    );
}

/// **C9's measurements are deterministic, so they are recomputed** (I-10).
///
/// A framed record's length and a rendered delta's length are pure functions of their inputs: no clock, no
/// allocator, no machine. So `c9-bounds.json` is held to the same standard as the C0 coverage artifact
/// rather than to the weaker standard `c8-cache-sweep.json` has to accept. If the wire encoding changes, or
/// the delta rendering changes, this fails — and the ledger entries that quote these numbers to justify
/// four constants have to be re-read, because the reason for them will have moved.
#[test]
fn the_c9_bounds_artifact_still_describes_the_wire() {
    let measured = schweep_server::costs::measure().to_json();
    assert_eq!(
        measured, C9_BOUNDS,
        "\ntesting/evidence/c9-bounds.json no longer describes the wire.\n\
         If the encoding or the bounds changed deliberately, regenerate it with:\n  \
         cargo run --release -p schweep-server --bin c9-costs > testing/evidence/c9-bounds.json\n\
         and re-read the justifications for DEFAULT_SOURCE_QUEUE_BOUND, DEFAULT_SOURCE_QUEUE_BYTES, \
         SUBSCRIPTION_RING and SUBSCRIPTION_RING_BYTES.\n"
    );

    // And the ledger's own arithmetic: the two bounds must still meet where the entries say they do.
    let measured = schweep_server::costs::measure();
    let widest = measured.widest_batch().expect("a batch was measured");
    assert!(
        widest.frame_bytes * schweep_server::DEFAULT_SOURCE_QUEUE_BOUND
            > schweep_server::DEFAULT_SOURCE_QUEUE_BYTES,
        "the count bound alone would admit {} bytes, which is under the byte bound — the ledger's \
         justification for having both no longer holds",
        widest.frame_bytes * schweep_server::DEFAULT_SOURCE_QUEUE_BOUND
    );
    let delta = measured.widest_delta().expect("a delta was measured");
    assert!(
        delta.rendered_bytes * schweep_server::SUBSCRIPTION_RING
            <= schweep_server::SUBSCRIPTION_RING_BYTES,
        "the ring's byte bound must sit above what its count bound implies at the widest measured \
         delta, or a narrow query silently loses history it was promised"
    );
}

/// The **deterministic** half of C8's measurements still describes the backend (I-10).
///
/// `c8-state-costs.json` is a function of what was written into a redb file: no clock, no threads, no
/// allocator luck. So it is recomputed and compared, exactly as the C0 coverage artifact is. If redb's
/// on-disk layout changes, or the key codec changes, this fails — which is the difference between a
/// measurement and a memory.
///
/// Its machine-dependent counterpart, `c8-cache-sweep.json`, is **not** recomputed: resident memory is
/// an allocator and kernel figure. Both the artifact and the ledger say so where a reader will see it.
#[test]
fn the_state_cost_artifact_still_describes_the_backend() {
    let recorded: serde_free::Costs = serde_free::parse_costs(STATE_COSTS);

    // An empty backend, measured now.
    let dir = std::env::temp_dir().join(format!("schweep-evidence-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let backend = schweep_state::RedbBackend::open(dir.join("state.redb")).unwrap();
    let empty = backend.bytes_on_disk().unwrap();
    drop(backend);
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        empty, recorded.bytes_when_empty,
        "an empty redb backend now occupies {empty} bytes; the artifact records {}. Regenerate it with:\n  \
         cargo run --release -p schweep-memo --bin state-costs > testing/evidence/c8-state-costs.json\n\
         and re-read the justifications for BYTES_PER_BACKEND and BYTES_PER_ENTRY_LOW — the numbers that \
         motivated them have moved.",
        recorded.bytes_when_empty
    );
    assert_eq!(
        empty,
        schweep_memo::costs::BYTES_PER_BACKEND,
        "BYTES_PER_BACKEND must be the measured empty-file size"
    );

    // And the floor the ledger publishes is still below the narrowest measured per-entry cost.
    assert!(
        schweep_memo::costs::BYTES_PER_ENTRY_LOW < recorded.narrowest_per_entry_at_scale,
        "BYTES_PER_ENTRY_LOW is {} but the narrowest measured per-entry cost is {}; the floor must be a \
         bound, not a fit",
        schweep_memo::costs::BYTES_PER_ENTRY_LOW,
        recorded.narrowest_per_entry_at_scale
    );
}

/// A minimal reader for the two artifacts, so the evidence test needs no JSON dependency.
///
/// The workspace has no serde, deliberately — nothing in the engine serialises anything — and adding
/// one so that a test can read two files it also writes would be the tail wagging the dog.
mod serde_free {
    pub struct Entry {
        pub name: String,
        pub value: String,
        pub artifact: String,
        pub justification: String,
        pub measured_on: String,
    }

    pub struct Registry {
        pub constants: Vec<Entry>,
    }

    impl Registry {
        pub fn value_of(&self, name: &str) -> Option<String> {
            self.constants
                .iter()
                .find(|entry| entry.name == name)
                .map(|entry| entry.value.clone())
        }
    }

    /// Pull the `constants` array's entries out by field, without a parser.
    pub fn parse(text: &str) -> Registry {
        let start = text
            .find("\"constants\": [")
            .expect("the ledger has a constants section");
        let rest = &text[start..];
        let end = rest.find("\"generator_constants\"").unwrap_or(rest.len());
        let section = &rest[..end];

        let mut constants = Vec::new();
        for block in section.split("    {").skip(1) {
            let field = |key: &str| -> String {
                let needle = format!("\"{key}\": ");
                match block.find(&needle) {
                    None => String::new(),
                    Some(at) => {
                        let after = &block[at + needle.len()..];
                        if let Some(stripped) = after.strip_prefix('"') {
                            let mut out = String::new();
                            let mut chars = stripped.chars();
                            while let Some(c) = chars.next() {
                                match c {
                                    '\\' => {
                                        if let Some(next) = chars.next() {
                                            out.push(next);
                                        }
                                    }
                                    '"' => break,
                                    other => out.push(other),
                                }
                            }
                            out
                        } else {
                            after
                                .chars()
                                .take_while(|c| !matches!(c, ',' | '\n'))
                                .collect::<String>()
                                .trim()
                                .to_owned()
                        }
                    }
                }
            };
            let name = field("name");
            if name.is_empty() {
                continue;
            }
            constants.push(Entry {
                name,
                value: field("value"),
                artifact: field("artifact"),
                justification: field("justification"),
                measured_on: field("measured_on"),
            });
        }
        Registry { constants }
    }

    pub struct Costs {
        pub bytes_when_empty: u64,
        pub narrowest_per_entry_at_scale: u64,
    }

    pub fn parse_costs(text: &str) -> Costs {
        let number_after = |needle: &str| -> u64 {
            let at = text.find(needle).unwrap_or_else(|| {
                panic!("the state-cost artifact should contain {needle:?}");
            });
            text[at + needle.len()..]
                .chars()
                .skip_while(|c| !c.is_ascii_digit())
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        };
        let pair = "\"bytes_per_entry_at_scale\": [";
        let at = text
            .find(pair)
            .expect("the artifact records the at-scale range");
        let narrowest = text[at + pair.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap_or(0);
        Costs {
            bytes_when_empty: number_after("\"bytes_when_empty\":"),
            narrowest_per_entry_at_scale: narrowest,
        }
    }
}
