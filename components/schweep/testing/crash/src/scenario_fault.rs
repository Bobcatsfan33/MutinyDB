//! Choosing a fault from a seed (`docs/DURABILITY.md` §5).

use schweep_differential::Rng;
use schweep_log::{FaultPlan, Seam};

/// The two kinds of fault the harness injects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// A named seam, at a chosen occurrence.
    ///
    /// The occurrence matters: a crash on the *third* checkpoint reaches code a crash on the first
    /// never does — a checkpoint superseding another, a `CURRENT` already written once.
    Seam(FaultPlan),
    /// Truncate or flip a byte of a published checkpoint's state file.
    ///
    /// The faults no seam enumeration can predict, and what exercises the torn-checkpoint path (R2).
    Bytes {
        epoch_index: usize,
        offset: usize,
        truncate: bool,
    },
    /// No fault: the clean run. Present as a variant so that "which fault fired" always has an
    /// answer, including "none", rather than being an `Option` that callers forget to check.
    None,
}

/// What a seed selected.
#[derive(Clone, Copy, Debug)]
pub struct FaultChoice {
    pub fault: Fault,
}

impl FaultChoice {
    /// Choose a fault for `seed`.
    ///
    /// Seam faults dominate — they are the enumerated, named cases §6 C4 asks to be covered — with
    /// byte-boundary faults one time in five, and a clean run one time in sixteen so that the
    /// harness's own no-fault path stays exercised.
    #[must_use]
    pub fn for_seed(seed: u64) -> FaultChoice {
        let mut rng = Rng::from_seed(seed ^ 0xC4_C4_C4_C4);
        if rng.chance(1, 16) {
            return FaultChoice { fault: Fault::None };
        }
        if rng.chance(1, 5) {
            return FaultChoice {
                fault: Fault::Bytes {
                    epoch_index: rng.below(4) as usize,
                    offset: rng.below(4096) as usize,
                    truncate: rng.chance(1, 2),
                },
            };
        }
        let seams = Seam::all();
        let index = rng.below(seams.len() as u64) as usize;
        let seam = seams.get(index).copied().unwrap_or(Seam::AckBeforeAppend);
        FaultChoice {
            fault: Fault::Seam(FaultPlan {
                seam,
                // 1..=3: enough to reach the second and third time a seam is hit without making
                // most plans unreachable.
                occurrence: (rng.below(3) + 1) as u32,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn a_seed_chooses_the_same_fault_every_time() {
        for seed in 0..200 {
            let a = FaultChoice::for_seed(seed).fault;
            let b = FaultChoice::for_seed(seed).fault;
            assert_eq!(a, b, "seed {seed} chose differently");
        }
    }

    /// Over enough seeds, every named seam is selected — otherwise the gate's coverage claim would
    /// rest on luck.
    #[test]
    fn every_seam_is_selected_by_some_seed() {
        let mut chosen: BTreeSet<Seam> = BTreeSet::new();
        for seed in 0..4000 {
            if let Fault::Seam(plan) = FaultChoice::for_seed(seed).fault {
                chosen.insert(plan.seam);
            }
        }
        for seam in Seam::all() {
            assert!(chosen.contains(seam), "no seed selects {seam}");
        }
    }

    #[test]
    fn all_three_kinds_of_choice_occur() {
        let mut seams = 0;
        let mut bytes = 0;
        let mut none = 0;
        for seed in 0..1000 {
            match FaultChoice::for_seed(seed).fault {
                Fault::Seam(_) => seams += 1,
                Fault::Bytes { .. } => bytes += 1,
                Fault::None => none += 1,
            }
        }
        assert!(seams > 500, "seam faults should dominate, got {seams}");
        assert!(bytes > 50, "byte faults should occur, got {bytes}");
        assert!(none > 10, "clean runs should occur, got {none}");
    }
}
