//! C13's release surface and fail-closed evidence gate.
//!
//! # Correction (2026-08-30): this file once froze the world instead of testing the mechanism
//!
//! Until the first real `current-v0.1` release attempt, `a_premature_release_is_mechanically_blocked`
//! asserted that the LIVE evidence file `testing/evidence/c13-nightly-streak.json` contained
//! `"status": "pending"`, `"release_blocked": true`, and exactly four `"qualifies": true` entries, and
//! that the verifier refused it with `nightly evidence is not marked complete`. Every one of those
//! assertions was true on 2026-08-15, when the streak stood at 4/7. None of them was a property of the
//! release gate. They were a photograph of C13's world-state on the day the test was written.
//!
//! The anti-pattern: **a guard that asserts current state instead of behavior expires the day the
//! state legitimately changes.** It looks like coverage — it is red for exactly one reason, and that
//! reason is the thing the project is waiting for. This repository has met it before: the status
//! table at the top of `docs/PROGRESS.md` read `not started` for four completed sprints because it
//! recorded a moment rather than a rule (the 2026-08-11 correction), and the generator-coverage
//! receipt is guarded by `the_committed_coverage_artifact_still_matches_the_generator` precisely
//! because a committed number that is not re-derived from the thing it describes goes stale silently.
//!
//! What it cost: on 2026-08-30 the streak legitimately completed (7 consecutive qualifying nights),
//! the evidence file was honestly updated to `complete`, the verifier approved `current-v0.1` — and
//! the Release workflow (run 33316824138) then failed in "Re-run the frozen workspace gate" because
//! this test still demanded `pending`. Main CI (run 33316820068) went red on the same assertion. The
//! release gate's own guard blocked the first real release attempt, not because the gate was wrong
//! but because the guard had been pinned to the state the gate was designed to leave.
//!
//! What this file asserts now: the MECHANISM. Synthetic evidence fixtures — pending status, a short
//! streak, non-consecutive dates, a night whose crash or soak job is not green, a night whose workflow
//! is not green — are each fed through `scripts/verify_c13_release.py` and must be refused with the
//! verifier's exact message; one complete fixture must be approved. The live file is asserted only to
//! be internally consistent (counts match its runs, dates parse and ascend, `qualifies` follows the job
//! conclusions, `release_blocked` mirrors `status`) and to receive the verdict its own `status` implies.
//! It is never again pinned to a lifecycle state.

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

fn workspace_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .expect("workspace root")
        .to_path_buf()
}

/// Runs the release verifier exactly as the Release workflow does, against `evidence`, and returns
/// `(approved, stdout, stderr)`.
fn run_verifier(args: &[&str], evidence: Option<&std::path::Path>) -> (bool, String, String) {
    let root = workspace_root();
    let mut command = Command::new("python3");
    command
        .arg(root.join("scripts/verify_c13_release.py"))
        .args(args);
    if let Some(path) = evidence {
        command.arg("--evidence").arg(path);
    }
    let output = command
        // GitHub sets this to the PR branch. The release workflow intentionally trusts that live tag
        // value, but this test is exercising the explicit candidate argument and must not inherit an
        // unrelated runner ref that changes which fail-closed check fires first.
        .env_remove("GITHUB_REF_NAME")
        .current_dir(&root)
        .output()
        .expect("run release verifier");
    (
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// One night's entry as the evidence file records it. `workflow`, `crash`, and `soak` are job
/// conclusions; `qualifies` is what the file CLAIMS, which the verifier must not take on faith.
fn night(
    date: &str,
    run_id: u64,
    workflow: &str,
    crash: &str,
    soak: &str,
    qualifies: bool,
) -> String {
    format!(
        concat!(
            "{{\"date\": \"{date}\", \"run_id\": {run_id}, ",
            "\"url\": \"https://example.invalid/runs/{run_id}\", ",
            "\"workflow_conclusion\": \"{workflow}\", \"nightly_crash\": \"{crash}\", ",
            "\"nightly_soak\": \"{soak}\", \"qualifies\": {qualifies}}}"
        ),
        date = date,
        run_id = run_id,
        workflow = workflow,
        crash = crash,
        soak = soak,
        qualifies = qualifies,
    )
}

fn green_night(date: &str, run_id: u64) -> String {
    night(date, run_id, "success", "success", "success", true)
}

fn evidence(status: &str, runs: &[String]) -> String {
    let complete = status == "complete";
    let soak_nights = runs
        .iter()
        .filter(|run| run.contains("\"nightly_soak\": \"success\""))
        .count();
    format!(
        concat!(
            "{{\"schema_version\": 1, \"as_of\": \"2026-08-30\", \"required_consecutive_nights\": 7, ",
            "\"observed_scheduled_workflow_days\": {days}, \"observed_complete_soak_nights\": {soak}, ",
            "\"status\": \"{status}\", \"release_blocked\": {blocked}, ",
            "\"definition\": \"synthetic fixture\", \"runs\": [{runs}]}}"
        ),
        days = runs.len(),
        soak = soak_nights,
        status = status,
        blocked = !complete,
        runs = runs.join(", "),
    )
}

/// Seven consecutive green nights, 2026-08-24 … 2026-08-30 — the shape the gate is designed to approve.
fn seven_consecutive_green() -> Vec<String> {
    (0..7)
        .map(|offset| green_night(&format!("2026-08-{}", 24 + offset), 33_000_000_000 + offset))
        .collect()
}

/// `runs` with the entry at `index` replaced by `replacement`; a new vector, the input untouched.
fn with_night(runs: &[String], index: usize, replacement: String) -> Vec<String> {
    runs.iter()
        .enumerate()
        .map(|(position, run)| {
            if position == index {
                replacement.clone()
            } else {
                run.clone()
            }
        })
        .collect()
}

/// Writes a fixture under the target directory (never the repository's evidence tree) and returns its path.
fn fixture(name: &str, body: &str) -> std::path::PathBuf {
    let dir = workspace_root().join("target").join("c13-release-fixtures");
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join(format!("{name}-{}.json", std::process::id()));
    std::fs::write(&path, body).expect("write fixture");
    path
}

fn assert_refused(name: &str, body: &str, expected_stderr: &str) {
    let path = fixture(name, body);
    let (approved, stdout, stderr) = run_verifier(&["current-v0.1"], Some(&path));
    assert!(
        !approved,
        "{name}: the verifier approved a release it must refuse\nstdout: {stdout}"
    );
    assert!(
        stderr.contains(expected_stderr),
        "{name}: expected `{expected_stderr}` in stderr, got:\n{stderr}"
    );
}

#[test]
fn the_release_workflow_runs_the_verifier_before_anything_else() {
    assert!(RELEASE.contains("scripts/verify_c13_release.py"));
    let verifier_step = RELEASE.find("scripts/verify_c13_release.py").unwrap();
    let test_step = RELEASE.find("cargo test --workspace").unwrap();
    let publish_step = RELEASE.find("gh release create").unwrap();
    assert!(verifier_step < test_step && test_step < publish_step);
}

#[test]
fn pending_evidence_is_refused_even_with_seven_green_nights() {
    // The lifecycle flag is the operator's explicit "complete" — nights alone must not release.
    assert_refused(
        "pending",
        &evidence("pending", &seven_consecutive_green()),
        "nightly evidence is not marked complete",
    );
}

#[test]
fn a_short_streak_is_refused() {
    let runs: Vec<String> = seven_consecutive_green().into_iter().take(4).collect();
    assert_refused(
        "short",
        &evidence("complete", &runs),
        "need 7 qualifying nightly runs, found 4",
    );
}

#[test]
fn non_consecutive_qualifying_dates_are_refused() {
    // Seven qualifying nights whose dates skip a day — a week of nights is not a week.
    let mut runs = with_night(
        &seven_consecutive_green(),
        3,
        green_night("2026-08-31", 33_000_000_099),
    );
    runs.sort();
    assert_refused(
        "gap",
        &evidence("complete", &runs),
        "qualifying dates are not consecutive: 2026-08-26 then 2026-08-28",
    );
}

#[test]
fn a_night_without_a_green_crash_job_is_refused() {
    // The file claims `qualifies: true`; the job conclusion says otherwise. The claim loses.
    let runs = with_night(
        &seven_consecutive_green(),
        5,
        night(
            "2026-08-29",
            33_000_000_005,
            "success",
            "failure",
            "success",
            true,
        ),
    );
    assert_refused(
        "crash",
        &evidence("complete", &runs),
        "workflow 33000000005 lacks a green required nightly job",
    );
}

#[test]
fn a_night_without_a_green_soak_job_is_refused() {
    let runs = with_night(
        &seven_consecutive_green(),
        2,
        night(
            "2026-08-26",
            33_000_000_002,
            "success",
            "success",
            "not_present",
            true,
        ),
    );
    assert_refused(
        "soak",
        &evidence("complete", &runs),
        "workflow 33000000002 lacks a green required nightly job",
    );
}

#[test]
fn a_night_whose_workflow_did_not_succeed_is_refused() {
    let runs = with_night(
        &seven_consecutive_green(),
        6,
        night(
            "2026-08-30",
            33_000_000_006,
            "failure",
            "success",
            "success",
            true,
        ),
    );
    assert_refused(
        "workflow",
        &evidence("complete", &runs),
        "workflow 33000000006 is not successful",
    );
}

#[test]
fn the_wrong_tag_is_refused_before_the_evidence_is_read() {
    let path = fixture(
        "wrong-tag",
        &evidence("complete", &seven_consecutive_green()),
    );
    let (approved, _, stderr) = run_verifier(&["current-v0.2"], Some(&path));
    assert!(!approved);
    assert!(
        stderr.contains("tag must be current-v0.1, got current-v0.2"),
        "{stderr}"
    );
}

#[test]
fn a_complete_consecutive_week_is_approved() {
    let path = fixture(
        "complete",
        &evidence("complete", &seven_consecutive_green()),
    );
    let (approved, stdout, stderr) = run_verifier(&["current-v0.1"], Some(&path));
    assert!(approved, "a complete week was refused:\n{stderr}");
    assert!(
        stdout.contains("release approved: current-v0.1, 7 qualifying nights"),
        "{stdout}"
    );
}

#[test]
fn the_live_evidence_is_internally_consistent_and_never_pinned_to_a_lifecycle_state() {
    // Consistency: counts match the runs, dates parse and ascend, `qualifies` follows the job
    // conclusions, `release_blocked` mirrors `status`. Nothing here says which state the file is in.
    let (consistent, stdout, stderr) = run_verifier(&["--consistency"], None);
    assert!(consistent, "live evidence contradicts itself:\n{stderr}");
    assert!(stdout.starts_with("evidence consistent:"), "{stdout}");

    // The verdict must follow the file's own status — whichever it is — through the real gate.
    let (approved, _, stderr) = run_verifier(&["current-v0.1"], None);
    let says_complete = NIGHTLY.contains("\"status\": \"complete\"");
    let says_pending = NIGHTLY.contains("\"status\": \"pending\"");
    assert!(
        says_complete ^ says_pending,
        "live evidence must be in exactly one lifecycle state"
    );
    assert_eq!(
        approved, says_complete,
        "the verifier's verdict disagrees with the live file's status:\n{stderr}"
    );
    if says_pending {
        assert!(stderr.contains("nightly evidence is not marked complete"));
    }
}
