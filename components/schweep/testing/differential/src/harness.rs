//! The differential harness (`ARCHITECTURE.md` §7).
//!
//! > scenario (seeded, reproducible) → run engine → run oracle → compare byte-for-byte at every
//! > sealed epoch.
//!
//! That is the whole shape, and this module is that sentence in code. Two things it does that
//! are easy to skip and expensive to skip:
//!
//! 1. **It compares at *every* sealed epoch, including epoch 0** — before any data exists. An
//!    engine that is right at the end and wrong in the middle is wrong (I-3), and an engine that
//!    is wrong on an empty input is wrong before it starts.
//! 2. **It compares errors, not just answers.** If one side raises and the other does not, that
//!    is a divergence; if both raise, the messages must match. Otherwise the easiest way to pass
//!    a differential suite would be to fail on everything (S-22, I-1).

use crate::engine::EngineUnderTest;
use crate::scenario::Scenario;

/// What a comparison produced when the two sides agreed all the way through.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub seed: u64,
    /// Number of epochs sealed.
    pub epochs: usize,
    /// Number of answers compared: one per sealed epoch, plus the one before any epoch.
    pub comparisons: usize,
    /// The rendered answer at each comparison point, in order. Two runs of one seed produce
    /// identical vectors — that is the reproducibility gate.
    pub answers: Vec<String>,
}

/// Where and how two implementations disagreed.
///
/// The message is written to be actionable on its own: the seed re-creates the run, the epoch
/// says when it went wrong, and both answers are printed in full so the difference is visible
/// without a debugger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Divergence {
    pub seed: u64,
    /// The epoch after which the answers differed. `0` means before any epoch was sealed.
    pub epoch: usize,
    pub left_name: &'static str,
    pub right_name: &'static str,
    pub left: String,
    pub right: String,
    pub scenario: String,
    pub kind: DivergenceKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DivergenceKind {
    /// Both produced an answer; the answers differ.
    Answer,
    /// One raised an error and the other did not, or both raised different errors.
    Error,
    /// One failed to seal an epoch the other sealed.
    Seal,
    /// One failed to build.
    Build,
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "differential divergence ({:?}) at epoch {} — seed {}",
            self.kind, self.epoch, self.seed
        )?;
        writeln!(f, "--- {} ---\n{}", self.left_name, self.left)?;
        writeln!(f, "--- {} ---\n{}", self.right_name, self.right)?;
        writeln!(
            f,
            "--- scenario (reproduce with this seed) ---\n{}",
            self.scenario
        )
    }
}

/// Run one scenario through two implementations and compare them at every sealed epoch.
pub fn compare<L: EngineUnderTest, R: EngineUnderTest>(
    scenario: &Scenario,
) -> Result<Report, Box<Divergence>> {
    let describe = || scenario.render();
    let diverge = |epoch: usize, kind: DivergenceKind, left: String, right: String| {
        Box::new(Divergence {
            seed: scenario.seed,
            epoch,
            left_name: L::name(),
            right_name: R::name(),
            left,
            right,
            scenario: describe(),
            kind,
        })
    };

    let left = L::build(&scenario.tables, &scenario.query);
    let right = R::build(&scenario.tables, &scenario.query);
    let (mut left, mut right) = match (left, right) {
        (Ok(l), Ok(r)) => (l, r),
        (l, r) => {
            let lm = build_outcome(l.as_ref().err());
            let rm = build_outcome(r.as_ref().err());
            // Both refusing to build, with the same message, means the scenario is not buildable
            // by either side — the same answer, not a disagreement. Anything else is one.
            if lm == rm {
                return Ok(Report {
                    seed: scenario.seed,
                    epochs: 0,
                    comparisons: 0,
                    answers: Vec::new(),
                });
            }
            return Err(diverge(0, DivergenceKind::Build, lm, rm));
        }
    };

    let mut answers = Vec::with_capacity(scenario.epochs.len() + 1);

    // Epoch 0: before anything is sealed. An engine that is wrong on empty input is wrong.
    let first = compare_answers(&left, &right);
    match first {
        Ok(rendered) => answers.push(rendered),
        Err((kind, l, r)) => return Err(diverge(0, kind, l, r)),
    }

    for (index, input) in scenario.epochs.iter().enumerate() {
        let epoch = index + 1;
        match (left.seal_epoch(input), right.seal_epoch(input)) {
            (Ok(()), Ok(())) => {}
            (l, r) => {
                // A failure to seal is reported whether or not the two sides agree about it. If
                // they disagree, that is a divergence; if they agree, the generator produced a
                // malformed history, which S-5 says has no defined answer — a bug in the
                // generator, and one that must never be silently skipped.
                return Err(diverge(
                    epoch,
                    DivergenceKind::Seal,
                    seal_outcome(l.err()),
                    seal_outcome(r.err()),
                ));
            }
        }

        match compare_answers(&left, &right) {
            Ok(rendered) => answers.push(rendered),
            Err((kind, l, r)) => return Err(diverge(epoch, kind, l, r)),
        }
    }

    Ok(Report {
        seed: scenario.seed,
        epochs: scenario.epochs.len(),
        comparisons: answers.len(),
        answers,
    })
}

fn build_outcome(error: Option<&String>) -> String {
    error.map_or_else(
        || "built successfully".to_owned(),
        |e| format!("failed to build: {e}"),
    )
}

fn seal_outcome(error: Option<String>) -> String {
    error.map_or_else(
        || "sealed successfully".to_owned(),
        |e| format!("failed to seal: {e}"),
    )
}

/// Compare one pair of answers byte for byte, errors included.
type AnswerMismatch = (DivergenceKind, String, String);

fn compare_answers<L: EngineUnderTest, R: EngineUnderTest>(
    left: &L,
    right: &R,
) -> Result<String, AnswerMismatch> {
    match (left.answer(), right.answer()) {
        (Ok(l), Ok(r)) => {
            // Canonical form carries the schema as well as the rows, so this comparison covers
            // column names, types, and order too (S-8).
            let (lr, rr) = (l.render(), r.render());
            if lr == rr {
                Ok(lr)
            } else {
                Err((DivergenceKind::Answer, lr, rr))
            }
        }
        (Err(l), Err(r)) => {
            if l == r {
                Ok(format!("ERROR: {l}"))
            } else {
                Err((
                    DivergenceKind::Error,
                    format!("ERROR: {l}"),
                    format!("ERROR: {r}"),
                ))
            }
        }
        (Ok(l), Err(r)) => Err((DivergenceKind::Error, l.render(), format!("ERROR: {r}"))),
        (Err(l), Ok(r)) => Err((DivergenceKind::Error, format!("ERROR: {l}"), r.render())),
    }
}

/// Run a range of seeds and return the first divergence, if any.
///
/// The signature returns the *first* failure rather than a count on purpose: one reproducible
/// counterexample is worth more than a tally, and the seed in it is all anyone needs.
pub fn sweep<L: EngineUnderTest, R: EngineUnderTest>(
    seeds: impl IntoIterator<Item = u64>,
) -> Result<SweepReport, Box<Divergence>> {
    sweep_matching::<L, R>(seeds, |_| true)
}

/// Sweep only the scenarios an implementation is expected to handle.
///
/// An engine that supports part of the dialect — C1's circuit does rung 1 and refuses the rest —
/// must be swept over the part it claims, or every skipped scenario would register as a build
/// divergence and drown the real signal. `accept` states the claim; scenarios outside it are
/// counted as skipped, and the count is reported rather than discarded, so "1,000 scenarios
/// passed" can always be read next to "and this many were not attempted".
pub fn sweep_matching<L: EngineUnderTest, R: EngineUnderTest>(
    seeds: impl IntoIterator<Item = u64>,
    accept: impl Fn(&Scenario) -> bool,
) -> Result<SweepReport, Box<Divergence>> {
    let mut report = SweepReport::default();
    for seed in seeds {
        let scenario = Scenario::generate(seed).map_err(|e| {
            Box::new(Divergence {
                seed,
                epoch: 0,
                left_name: L::name(),
                right_name: R::name(),
                left: format!("scenario generation failed: {e}"),
                right: String::new(),
                scenario: String::new(),
                kind: DivergenceKind::Build,
            })
        })?;
        report.considered += 1;
        if !accept(&scenario) {
            report.skipped += 1;
            continue;
        }
        let run = compare::<L, R>(&scenario)?;
        report.scenarios += 1;
        report.error_answers += run
            .answers
            .iter()
            .filter(|a| a.starts_with("ERROR"))
            .count();
        report.epochs += run.epochs;
        report.comparisons += run.comparisons;
        if scenario.is_empty_input() {
            report.empty_input_scenarios += 1;
        }
        if scenario.has_empty_epoch() {
            report.scenarios_with_an_empty_epoch += 1;
        }
        for op in scenario.operations() {
            if !report.operations.contains(&op) {
                report.operations.push(op);
            }
        }
        if !report.families.contains(&scenario.family) {
            report.families.push(scenario.family);
        }
    }
    report.operations.sort_unstable();
    report.families.sort_unstable();
    Ok(report)
}

/// What a sweep covered. Reported so that "1,000 scenarios passed" can be checked against "and
/// they contained the shapes §7 requires" — a suite that passes without exercising retractions
/// would be a suite that proves nothing.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Seeds looked at, including those `accept` rejected.
    pub considered: usize,
    /// Seeds `accept` rejected — outside the implementation's claimed dialect.
    pub skipped: usize,
    pub scenarios: usize,
    pub epochs: usize,
    pub comparisons: usize,
    pub empty_input_scenarios: usize,
    pub scenarios_with_an_empty_epoch: usize,
    /// Comparisons where *both* sides raised the same error. Counted because an error is a
    /// legitimate agreement but a useless one, and a suite that quietly filled with them would
    /// look green while testing nothing.
    pub error_answers: usize,
    pub operations: Vec<crate::scenario::Operation>,
    pub families: Vec<crate::scenario::Family>,
}
