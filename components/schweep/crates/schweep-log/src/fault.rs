//! The fault seams the crash harness lands on (`docs/DURABILITY.md` §5).
//!
//! Every seam here is named in `docs/DURABILITY.md`, and the mapping is the point: a seam that is not
//! in the document is a seam nothing tests, and a document entry with no seam is a claim nothing
//! checks. The two are meant to be read side by side.
//!
//! Faults are **deterministic**, selected by a seed, and are never timers. A crash test that depends
//! on timing is worse than no crash test, because a flake teaches people to press re-run.

use std::fmt;

/// A named instant between two durable operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Seam {
    // ---- the ack sequence (§1) ----
    AckBeforeValidate,
    AckBeforeAppend,
    AckAfterAppendBeforeFsync,
    AckAfterFsyncBeforeIndex,
    AckAfterFsyncBeforeAck,
    // ---- the seal sequence (§2) ----
    SealBeforeRecord,
    SealAfterRecordBeforeFsync,
    SealAfterFsyncBeforeStep,
    SealAfterStepBeforeCounter,
    // ---- the checkpoint sequence (§3) ----
    CheckpointBeforeStateFlush,
    CheckpointAfterStateFlushBeforeFsync,
    CheckpointAfterFsyncBeforeManifest,
    CheckpointAfterManifestBeforePublish,
    CheckpointAfterPublishBeforeCurrent,
    CheckpointAfterCurrentBeforeTrim,
    CheckpointAfterTrimBeforeCleanup,
    // ---- the compaction sequence (§4, C7) ----
    CompactBeforeSnapshot,
    CompactAfterWriteBeforeFsync,
    CompactAfterFsyncBeforeManifest,
    CompactAfterManifestBeforePublish,
    CompactAfterPublishBeforeSegment,
    CompactAfterSegmentBeforePointer,
    CompactAfterPointerBeforeTrim,
    CompactAfterTrimBeforeCleanup,
    // ---- recovery (§5) ----
    /// A crash *during* recovery. Recovery must be idempotent: recovering again must reach the same
    /// state. This is a bug class that has bitten sibling systems, so it is a seam rather than an
    /// argument.
    RecoveryAfterCheckpointBeforeReplay,
    RecoveryMidReplay,
}

impl Seam {
    /// Every seam, in a fixed order. The gate asserts that each one was reached at least once, so
    /// this list is also the coverage checklist.
    #[must_use]
    pub fn all() -> &'static [Seam] {
        &[
            Seam::AckBeforeValidate,
            Seam::AckBeforeAppend,
            Seam::AckAfterAppendBeforeFsync,
            Seam::AckAfterFsyncBeforeIndex,
            Seam::AckAfterFsyncBeforeAck,
            Seam::SealBeforeRecord,
            Seam::SealAfterRecordBeforeFsync,
            Seam::SealAfterFsyncBeforeStep,
            Seam::SealAfterStepBeforeCounter,
            Seam::CheckpointBeforeStateFlush,
            Seam::CheckpointAfterStateFlushBeforeFsync,
            Seam::CheckpointAfterFsyncBeforeManifest,
            Seam::CheckpointAfterManifestBeforePublish,
            Seam::CheckpointAfterPublishBeforeCurrent,
            Seam::CheckpointAfterCurrentBeforeTrim,
            Seam::CheckpointAfterTrimBeforeCleanup,
            Seam::CompactBeforeSnapshot,
            Seam::CompactAfterWriteBeforeFsync,
            Seam::CompactAfterFsyncBeforeManifest,
            Seam::CompactAfterManifestBeforePublish,
            Seam::CompactAfterPublishBeforeSegment,
            Seam::CompactAfterSegmentBeforePointer,
            Seam::CompactAfterPointerBeforeTrim,
            Seam::CompactAfterTrimBeforeCleanup,
            Seam::RecoveryAfterCheckpointBeforeReplay,
            Seam::RecoveryMidReplay,
        ]
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Seam::AckBeforeValidate => "AckBeforeValidate",
            Seam::AckBeforeAppend => "AckBeforeAppend",
            Seam::AckAfterAppendBeforeFsync => "AckAfterAppendBeforeFsync",
            Seam::AckAfterFsyncBeforeIndex => "AckAfterFsyncBeforeIndex",
            Seam::AckAfterFsyncBeforeAck => "AckAfterFsyncBeforeAck",
            Seam::SealBeforeRecord => "SealBeforeRecord",
            Seam::SealAfterRecordBeforeFsync => "SealAfterRecordBeforeFsync",
            Seam::SealAfterFsyncBeforeStep => "SealAfterFsyncBeforeStep",
            Seam::SealAfterStepBeforeCounter => "SealAfterStepBeforeCounter",
            Seam::CheckpointBeforeStateFlush => "CheckpointBeforeStateFlush",
            Seam::CheckpointAfterStateFlushBeforeFsync => "CheckpointAfterStateFlushBeforeFsync",
            Seam::CheckpointAfterFsyncBeforeManifest => "CheckpointAfterFsyncBeforeManifest",
            Seam::CheckpointAfterManifestBeforePublish => "CheckpointAfterManifestBeforePublish",
            Seam::CheckpointAfterPublishBeforeCurrent => "CheckpointAfterPublishBeforeCurrent",
            Seam::CheckpointAfterCurrentBeforeTrim => "CheckpointAfterCurrentBeforeTrim",
            Seam::CheckpointAfterTrimBeforeCleanup => "CheckpointAfterTrimBeforeCleanup",
            Seam::CompactBeforeSnapshot => "CompactBeforeSnapshot",
            Seam::CompactAfterWriteBeforeFsync => "CompactAfterWriteBeforeFsync",
            Seam::CompactAfterFsyncBeforeManifest => "CompactAfterFsyncBeforeManifest",
            Seam::CompactAfterManifestBeforePublish => "CompactAfterManifestBeforePublish",
            Seam::CompactAfterPublishBeforeSegment => "CompactAfterPublishBeforeSegment",
            Seam::CompactAfterSegmentBeforePointer => "CompactAfterSegmentBeforePointer",
            Seam::CompactAfterPointerBeforeTrim => "CompactAfterPointerBeforeTrim",
            Seam::CompactAfterTrimBeforeCleanup => "CompactAfterTrimBeforeCleanup",
            Seam::RecoveryAfterCheckpointBeforeReplay => "RecoveryAfterCheckpointBeforeReplay",
            Seam::RecoveryMidReplay => "RecoveryMidReplay",
        }
    }
}

impl fmt::Display for Seam {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which seam to fail at, and on which occurrence.
///
/// The occurrence matters: a crash on the *third* checkpoint reaches code that a crash on the first
/// never does — a checkpoint superseding another, a log already trimmed once. Failing only ever on
/// the first occurrence would leave that untested.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FaultPlan {
    pub seam: Seam,
    /// 1 = fail the first time the seam is reached, 2 = the second, and so on.
    pub occurrence: u32,
}

/// Counts occurrences and fires once, at the planned one.
///
/// Held by whatever might crash. With no plan it is inert, which is the production configuration:
/// the hooks compile to a counter increment and a comparison against `None`.
#[derive(Debug, Default)]
pub struct FaultInjector {
    plan: Option<FaultPlan>,
    seen: std::collections::BTreeMap<Seam, u32>,
    fired: Option<Seam>,
    reached: std::collections::BTreeSet<Seam>,
}

impl FaultInjector {
    /// An injector that never fires — production, and every test that is not about crashing.
    #[must_use]
    pub fn inert() -> FaultInjector {
        FaultInjector::default()
    }

    #[must_use]
    pub fn planned(plan: FaultPlan) -> FaultInjector {
        FaultInjector {
            plan: Some(plan),
            ..FaultInjector::default()
        }
    }

    /// Record that a seam was reached, and say whether to fail here.
    pub fn reached(&mut self, seam: Seam) -> bool {
        self.reached.insert(seam);
        let count = self.seen.entry(seam).or_insert(0);
        *count += 1;
        match self.plan {
            Some(plan) if plan.seam == seam && plan.occurrence == *count => {
                self.fired = Some(seam);
                true
            }
            _ => false,
        }
    }

    /// The seam that fired, if any. The harness asserts on this: a cycle that injected no fault
    /// proves nothing, and a suite of them proves nothing loudly.
    #[must_use]
    pub fn fired(&self) -> Option<Seam> {
        self.fired
    }

    /// Every seam this run reached, whether or not it fired.
    #[must_use]
    pub fn reached_seams(&self) -> &std::collections::BTreeSet<Seam> {
        &self.reached
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn an_inert_injector_never_fires() {
        let mut injector = FaultInjector::inert();
        for _ in 0..5 {
            for seam in Seam::all() {
                assert!(!injector.reached(*seam));
            }
        }
        assert!(injector.fired().is_none());
        assert_eq!(injector.reached_seams().len(), Seam::all().len());
    }

    #[test]
    fn a_plan_fires_once_at_the_planned_occurrence() {
        let mut injector = FaultInjector::planned(FaultPlan {
            seam: Seam::SealAfterFsyncBeforeStep,
            occurrence: 3,
        });
        assert!(!injector.reached(Seam::SealAfterFsyncBeforeStep));
        assert!(!injector.reached(Seam::SealAfterFsyncBeforeStep));
        assert!(
            injector.reached(Seam::SealAfterFsyncBeforeStep),
            "the third time"
        );
        assert!(
            !injector.reached(Seam::SealAfterFsyncBeforeStep),
            "and only once"
        );
        assert_eq!(injector.fired(), Some(Seam::SealAfterFsyncBeforeStep));
    }

    #[test]
    fn a_plan_ignores_other_seams() {
        let mut injector = FaultInjector::planned(FaultPlan {
            seam: Seam::CheckpointAfterCurrentBeforeTrim,
            occurrence: 1,
        });
        assert!(!injector.reached(Seam::AckAfterFsyncBeforeAck));
        assert!(injector.fired().is_none());
    }

    #[test]
    fn every_seam_has_a_distinct_name() {
        let mut names: Vec<&str> = Seam::all().iter().map(|s| s.name()).collect();
        let count = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), count, "seam names must be distinct");
    }
}
