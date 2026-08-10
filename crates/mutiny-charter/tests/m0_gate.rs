//! The M0 exit gate, and the proof that it can fail.
//!
//! `CONSOLIDATION-ROADMAP.md` §4 M0: *"Exit: MD-1…MD-4 merged; CI skeleton (fmt/clippy/test/
//! no-egress) green on an empty workspace."* The first half of that sentence is what this file
//! asserts. The second half is the workflow that runs it.
//!
//! Half of these tests exist to watch the gate go red on purpose. A validator that has only ever
//! been run against valid input is an untested validator, and an untested validator on a CI job
//! is worse than no job at all, because the green tick is believed.

// Explicit, as Current's CI comment requires: the workspace denies these in library code, and a
// test that re-enables them says so out loud.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mutiny_charter::{
    repository_root, validate_charter, validate_record, CharterError, MINIMUM_OPTIONS,
    REQUIRED_RECORDS, REQUIRED_SECTIONS,
};

/// A well-formed record, used as the base that each mutation test breaks in exactly one way.
fn well_formed(record: &str) -> String {
    format!(
        "# {record} · A title\n\
         \n\
         **Status:** Accepted\n\
         \n\
         ## Context\n\
         Why this decision was needed.\n\
         \n\
         ## Options considered\n\
         ### Option 1 — the first\n\
         Weighed.\n\
         ### Option 2 — the second\n\
         Weighed.\n\
         \n\
         ## Decision\n\
         Option 2.\n\
         \n\
         ## Consequences\n\
         What this costs.\n"
    )
}

// ---------------------------------------------------------------------------------------------
// The gate itself.
// ---------------------------------------------------------------------------------------------

#[test]
fn the_m0_charter_is_present_well_formed_and_accepted() {
    let failures = validate_charter(&repository_root());
    assert!(
        failures.is_empty(),
        "the M0 charter does not hold:\n{}",
        failures
            .iter()
            .map(|failure| format!("  - {failure}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    println!(
        "M0 charter: {} records, each with {} required sections and \u{2265}{} options weighed",
        REQUIRED_RECORDS.len(),
        REQUIRED_SECTIONS.len(),
        MINIMUM_OPTIONS
    );
}

#[test]
fn the_roadmap_this_repository_serves_is_present_at_the_root() {
    // §6 makes this repository the docs of record for the consolidation. If the roadmap is not
    // here, the four records below have nothing to be records *of*.
    let roadmap = repository_root().join("CONSOLIDATION-ROADMAP.md");
    assert!(roadmap.is_file(), "{} is missing", roadmap.display());
}

#[test]
fn no_phase_after_m0_has_started_in_this_workspace() {
    // The instruction M0 is most likely to be violated by is "do not start M1". A crate that is
    // not the charter crate means someone did. This test is the tripwire; the M1 session deletes
    // it in the same commit that adds `mutiny-bridge`, which is a deliberate, visible act rather
    // than a drift.
    let crates = repository_root().join("crates");
    let members: Vec<String> = std::fs::read_dir(&crates)
        .expect("crates/ should exist")
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        members,
        vec!["mutiny-charter".to_owned()],
        "M0 carries exactly one crate and no engine code; found {members:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The gate can fail. One mutation per rule.
// ---------------------------------------------------------------------------------------------

#[test]
fn a_valid_record_passes() {
    assert_eq!(validate_record("MD-1", &well_formed("MD-1")), Ok(()));
}

#[test]
fn a_record_with_the_wrong_title_is_refused() {
    let text = well_formed("MD-1").replace("# MD-1 · A title", "# MD-1");
    assert!(matches!(
        validate_record("MD-1", &text),
        Err(CharterError::BadTitle { .. })
    ));
}

#[test]
fn a_record_still_in_draft_is_refused() {
    let text = well_formed("MD-2").replace("**Status:** Accepted", "**Status:** Proposed");
    assert!(matches!(
        validate_record("MD-2", &text),
        Err(CharterError::BadStatus { .. })
    ));
}

#[test]
fn a_superseded_record_is_accepted_but_a_bare_supersession_is_not() {
    let superseded =
        well_formed("MD-2").replace("**Status:** Accepted", "**Status:** Superseded by MD-9");
    assert_eq!(validate_record("MD-2", &superseded), Ok(()));

    let bare = well_formed("MD-2").replace("**Status:** Accepted", "**Status:** Superseded by MD-");
    assert!(matches!(
        validate_record("MD-2", &bare),
        Err(CharterError::BadStatus { .. })
    ));
}

#[test]
fn a_record_missing_a_section_is_refused() {
    let text = well_formed("MD-3").replace("## Consequences", "## Notes");
    assert!(matches!(
        validate_record("MD-3", &text),
        Err(CharterError::MissingSection { section, .. }) if section == "## Consequences"
    ));
}

#[test]
fn a_record_that_decides_before_it_weighs_is_refused() {
    // The order is the point: a record whose decision precedes its options is a decision with an
    // options section bolted on afterwards.
    let text = "# MD-3 · A title\n\n**Status:** Accepted\n\n## Context\nc\n\n## Decision\nd\n\n\
                ## Options considered\n### Option 1 — a\n### Option 2 — b\n\n## Consequences\nq\n";
    assert!(matches!(
        validate_record("MD-3", text),
        Err(CharterError::SectionOutOfOrder { .. })
    ));
}

#[test]
fn a_record_that_weighs_one_option_is_refused() {
    let text = well_formed("MD-4").replace("### Option 2 — the second\nWeighed.\n", "");
    assert!(matches!(
        validate_record("MD-4", &text),
        Err(CharterError::TooFewOptions { found: 1, .. })
    ));
}

#[test]
fn a_record_absent_from_disk_is_reported_as_missing_not_skipped() {
    // The most dangerous bug a charter gate can have is treating "no file" as "nothing to check".
    let empty = tempdir();
    let failures = validate_charter(&empty);
    assert_eq!(failures.len(), REQUIRED_RECORDS.len(), "{failures:?}");
    assert!(failures
        .iter()
        .all(|failure| matches!(failure, CharterError::Missing { .. })));
    std::fs::remove_dir_all(&empty).ok();
}

#[test]
fn a_record_that_exists_but_is_unlisted_in_the_index_is_refused() {
    let root = tempdir();
    let decisions = root.join("docs").join("decisions");
    std::fs::create_dir_all(&decisions).unwrap();
    std::fs::write(decisions.join("README.md"), "an index that links nothing").unwrap();
    for record in REQUIRED_RECORDS {
        std::fs::write(decisions.join(format!("{record}.md")), well_formed(record)).unwrap();
    }

    let failures = validate_charter(&root);
    assert_eq!(failures.len(), REQUIRED_RECORDS.len(), "{failures:?}");
    assert!(failures
        .iter()
        .all(|failure| matches!(failure, CharterError::NotIndexed { .. })));
    std::fs::remove_dir_all(&root).ok();
}

/// A scratch directory. Deterministic name per test-thread, no wall clock and no randomness —
/// Current's D-6 discipline, which this repository inherits from the day it has any code at all.
fn tempdir() -> std::path::PathBuf {
    let name = std::thread::current()
        .name()
        .unwrap_or("unnamed")
        .replace("::", "-");
    let path = std::env::temp_dir().join(format!("mutiny-charter-{name}"));
    std::fs::remove_dir_all(&path).ok();
    std::fs::create_dir_all(&path).unwrap();
    path
}
