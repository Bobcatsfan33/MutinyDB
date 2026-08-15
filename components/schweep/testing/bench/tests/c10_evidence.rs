//! The C10 artifact/README traceability gate (I-10).

#![allow(clippy::panic)]

const ARTIFACT: &str = include_str!("../../evidence/c10-benchmarks.json");
const README: &str = include_str!("../../../README.md");

#[test]
fn all_four_benchmarks_have_release_evidence_and_the_readme_quotes_the_receipt() {
    for required in [
        "maintenance change volume 100",
        "maintenance change volume 1000",
        "maintenance change volume 10000",
        "standing answer read latency",
        "10,000-query swarm marginal registration",
        "\"scale_factor\": 0.1",
        "\"lineitem_rows\": 600572",
        "\"duckdb\": \"1.5.5\"",
        "\"profile\": \"release\"",
        "paired alternating rounds after one untimed warm-up",
        "this is not the official TPC-H query suite",
    ] {
        assert!(
            ARTIFACT.contains(required),
            "C10 evidence is missing {required:?}"
        );
    }
    assert!(!ARTIFACT.contains("DEBUG BUILD"));

    for quoted in [
        "3.176 µs per changed row",
        "18.068 µs per standing read",
        "1.786 ms for the marginal query",
        "89.46× slower than DuckDB",
        "testing/evidence/c10-benchmarks.json",
    ] {
        assert!(
            README.contains(quoted),
            "README claim {quoted:?} lost its receipt"
        );
    }
}
