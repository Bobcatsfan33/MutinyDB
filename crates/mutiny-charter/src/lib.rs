//! The charter crate. **It contains no engine code and never will.**
//!
//! MutinyDB's M0 phase (`CONSOLIDATION-ROADMAP.md` §4) produces four decision records and a CI
//! skeleton, and nothing else. This crate is the skeleton's spine: it validates the records that
//! M0 exists to produce, so that the `test` job at M0 proves something instead of proving that
//! zero tests passed.
//!
//! # Why the gate is code and not a checklist
//!
//! MD-1 through MD-4 and MD-6 are load-bearing for the imported component source. The failure they
//! are written to
//! prevent — a plane quietly depending sideways, a bridge schema drifting, a dialect growing a
//! second meaning, a name that has to be surrendered after launch — is a *slow* failure: it does
//! not announce itself, and by the time it does, the code that assumed the wrong thing is
//! everywhere. A record that can be silently emptied, left in `Proposed`, or deleted during a
//! refactor is a record nobody is actually held to. So the records' shape is asserted by a test
//! that runs on every push, exactly like every other law in these repositories.
//!
//! This validator checks *shape and status*, never prose quality. It cannot tell whether MD-2's
//! schema is the right schema — only that MD-2 exists, weighs its options, states a decision, and
//! has not quietly reverted to a draft. That is the honest limit of what a linter can gate, and it
//! is stated here rather than implied.

use std::fmt;
use std::path::{Path, PathBuf};

/// The decision records M0 must produce, in order. Adding a record to the charter means adding it
/// here; a record on disk that is not named here is not part of the gate.
pub const REQUIRED_RECORDS: &[&str] = &["MD-1", "MD-2", "MD-3", "MD-4", "MD-6"];

/// The sections every record carries, in this order. The order is part of the contract: a reader
/// who opens any record finds the options before the decision, and the consequences after it.
pub const REQUIRED_SECTIONS: &[&str] = &[
    "## Context",
    "## Options considered",
    "## Decision",
    "## Consequences",
];

/// A record must weigh at least this many options. One "option" is a decision already made
/// elsewhere and written down here to look deliberate.
pub const MINIMUM_OPTIONS: usize = 2;

/// Why a record failed the gate. Every variant names the file, because a failure that does not say
/// which record broke sends the reader to all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CharterError {
    /// The record is not on disk at all.
    Missing { record: String, path: PathBuf },
    /// The file could not be read.
    Unreadable { record: String, reason: String },
    /// The first line is not `# MD-N · <title>`.
    BadTitle { record: String, found: String },
    /// The `**Status:**` line is absent or names a status the charter does not accept.
    BadStatus { record: String, found: String },
    /// A required section is absent.
    MissingSection { record: String, section: String },
    /// The required sections appear in the wrong order.
    SectionOutOfOrder {
        record: String,
        section: String,
        after: String,
    },
    /// Fewer than [`MINIMUM_OPTIONS`] options were weighed.
    TooFewOptions { record: String, found: usize },
    /// The index does not link the record.
    NotIndexed { record: String },
}

impl fmt::Display for CharterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CharterError::Missing { record, path } => {
                write!(f, "{record}: no such record at {}", path.display())
            }
            CharterError::Unreadable { record, reason } => {
                write!(f, "{record}: could not be read: {reason}")
            }
            CharterError::BadTitle { record, found } => write!(
                f,
                "{record}: first line must be `# {record} · <title>`, found {found:?}"
            ),
            CharterError::BadStatus { record, found } => write!(
                f,
                "{record}: status must be `**Status:** Accepted` or `**Status:** Superseded by \
                 MD-N`, found {found:?}"
            ),
            CharterError::MissingSection { record, section } => {
                write!(f, "{record}: missing required section `{section}`")
            }
            CharterError::SectionOutOfOrder {
                record,
                section,
                after,
            } => write!(
                f,
                "{record}: section `{section}` appears before `{after}`; the charter order is \
                 {REQUIRED_SECTIONS:?}"
            ),
            CharterError::TooFewOptions { record, found } => write!(
                f,
                "{record}: weighs {found} option(s); a decision record must weigh at least \
                 {MINIMUM_OPTIONS} (`### Option N — ...`)"
            ),
            CharterError::NotIndexed { record } => {
                write!(f, "{record}: not linked from docs/decisions/README.md")
            }
        }
    }
}

/// Validate one record's text. Split from the filesystem so the rules can be tested against
/// deliberately broken input — a gate nobody has watched fail is a gate nobody should trust.
pub fn validate_record(record: &str, text: &str) -> Result<(), CharterError> {
    let first = text.lines().next().unwrap_or_default().trim();
    let expected_prefix = format!("# {record} · ");
    if !first.starts_with(&expected_prefix) || first.len() <= expected_prefix.len() {
        return Err(CharterError::BadTitle {
            record: record.to_owned(),
            found: first.to_owned(),
        });
    }

    let status = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("**Status:**"))
        .unwrap_or_default();
    let body = status.trim_start_matches("**Status:**").trim();
    let accepted = body == "Accepted"
        || (body.starts_with("Superseded by MD-") && body.len() > "Superseded by MD-".len());
    if !accepted {
        return Err(CharterError::BadStatus {
            record: record.to_owned(),
            found: status.to_owned(),
        });
    }

    let mut cursor = 0usize;
    let mut previous = "the title";
    for section in REQUIRED_SECTIONS {
        match text[cursor..].find(section) {
            None if text.contains(section) => {
                return Err(CharterError::SectionOutOfOrder {
                    record: record.to_owned(),
                    section: (*section).to_owned(),
                    after: previous.to_owned(),
                })
            }
            None => {
                return Err(CharterError::MissingSection {
                    record: record.to_owned(),
                    section: (*section).to_owned(),
                })
            }
            Some(offset) => {
                cursor += offset + section.len();
                previous = section;
            }
        }
    }

    let options = text
        .lines()
        .filter(|line| line.trim_start().starts_with("### Option "))
        .count();
    if options < MINIMUM_OPTIONS {
        return Err(CharterError::TooFewOptions {
            record: record.to_owned(),
            found: options,
        });
    }

    Ok(())
}

/// Validate every required record under `root` (the repository root), plus the index.
///
/// Returns every failure rather than the first: a contributor fixing the charter should see the
/// whole list once, not discover it one CI run at a time.
pub fn validate_charter(root: &Path) -> Vec<CharterError> {
    let decisions = root.join("docs").join("decisions");
    let index = std::fs::read_to_string(decisions.join("README.md")).unwrap_or_default();
    let mut failures = Vec::new();

    for record in REQUIRED_RECORDS {
        let path = decisions.join(format!("{record}.md"));
        if !path.is_file() {
            failures.push(CharterError::Missing {
                record: (*record).to_owned(),
                path,
            });
            continue;
        }
        match std::fs::read_to_string(&path) {
            Err(error) => failures.push(CharterError::Unreadable {
                record: (*record).to_owned(),
                reason: error.to_string(),
            }),
            Ok(text) => {
                if let Err(failure) = validate_record(record, &text) {
                    failures.push(failure);
                }
            }
        }
        if !index.contains(&format!("{record}.md")) {
            failures.push(CharterError::NotIndexed {
                record: (*record).to_owned(),
            });
        }
    }

    failures
}

/// The repository root, derived from this crate's manifest directory (`crates/mutiny-charter`).
pub fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}
