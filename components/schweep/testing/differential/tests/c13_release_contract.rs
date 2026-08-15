//! C13's release surface and fail-closed evidence gate.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::process::Command;

const API: &str = include_str!("../../../docs/current-api.md");
const CI: &str = include_str!("../../../.github/workflows/ci.yml");
const RELEASE: &str = include_str!("../../../.github/workflows/release.yml");
const INVARIANTS: &str = include_str!("../../evidence/c13-invariants.json");
const NIGHTLY: &str = include_str!("../../evidence/c13-nightly-streak.json");
const AUDIT: &str = include_str!("../../evidence/c13-ci-audit.json");
const EXTENDED: &str = include_str!("../../evidence/c13-extended-hosted.json");
const README: &str = include_str!("../../../README.md");

#[test]
fn the_supported_v01_surface_is_explicit() {
    for endpoint in [
        "/ingest",
        "/seal",
        "/txn",
        "/retract-source",
        "/register",
        "/deregister",
        "/read",
        "/oneshot",
        "/subscribe",
        "/plan",
        "/counters",
        "/fingerprint",
        "/explain-state",
        "/explain-maintenance",
        "/health",
        "/shutdown",
    ] {
        assert!(API.contains(endpoint), "v0.1 API omitted {endpoint}");
    }
    for kind in ["Refused", "NotFound", "Rejected", "Overloaded", "Internal"] {
        assert!(API.contains(kind), "v0.1 API omitted error kind {kind}");
    }
    assert!(API.contains("Patch releases `0.1.x`"));
    assert!(API.contains("Snapshot v1 and v2"));
    assert!(API.contains("plaintext and unauthenticated"));
}

#[test]
fn every_architecture_invariant_has_a_named_ci_check() {
    for number in 1..=10 {
        let id = format!("I-{number}");
        assert!(CI.contains(&format!("invariant: {id}")), "CI omitted {id}");
        assert!(INVARIANTS.contains(&format!("\"id\": \"{id}\"")));
        assert!(INVARIANTS.contains(&format!("\"ci_job\": \"invariant {id}\"")));
    }
    assert!(CI.contains(
        "needs: [fmt, clippy, test, no-network, state-ceiling, memo-ceiling, invariants]"
    ));
}

#[test]
fn the_honesty_pass_is_issue_sourced_and_audited() {
    for issue in 4..=17 {
        assert!(
            README.contains(&format!("/issues/{issue}")),
            "README limitations omitted issue #{issue}"
        );
    }
    assert!(AUDIT.contains("\"requested_window\": 50"));
    assert!(AUDIT.contains("\"available_runs\": 36"));
    assert!(AUDIT.contains("\"unresolved_flakes\": 0"));
    assert_eq!(AUDIT.matches("\"green_proof\"").count(), 4);
}

#[test]
fn the_hosted_extended_populations_are_retained_without_nightly_credit() {
    assert!(EXTENDED.contains("\"workflow_run_id\": 31906947809"));
    assert!(EXTENDED.contains("\"head_sha\": \"5de862dd71d26c23cf45f37af463efd25a10635f\""));
    assert!(EXTENDED.contains("\"comparisons\": 248321"));
    assert!(EXTENDED.contains("\"divergences\": 0"));
    assert!(EXTENDED.contains("\"cycles\": 100000"));
    assert!(EXTENDED.contains("\"named_seams_fired\": 26"));
    assert!(EXTENDED.contains("\"nightly_streak_credit\": false"));
}

#[test]
fn a_premature_release_is_mechanically_blocked() {
    assert!(NIGHTLY.contains("\"status\": \"pending\""));
    assert!(NIGHTLY.contains("\"release_blocked\": true"));
    assert_eq!(NIGHTLY.matches("\"qualifies\": true").count(), 4);
    assert!(RELEASE.contains("scripts/verify_c13_release.py"));

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root");
    let output = Command::new("python3")
        .arg(root.join("scripts/verify_c13_release.py"))
        .arg("current-v0.1")
        // GitHub sets this to the PR branch. The release workflow intentionally trusts that live tag
        // value, but this test is exercising the explicit candidate argument and must not inherit an
        // unrelated runner ref that changes which fail-closed check fires first.
        .env_remove("GITHUB_REF_NAME")
        .current_dir(root)
        .output()
        .expect("run release verifier");
    assert!(!output.status.success(), "a 4/7 release passed its gate");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("nightly evidence is not marked complete"));
}
