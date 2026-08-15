//! Per-source admission and bounded queues (§6 C9).
//!
//! > per-source admission + backpressure (bounded queues, never unbounded buffering)
//!
//! **The rule this enforces is one sentence: a queue with no bound is a memory leak with a schedule.**
//! A server that accepts everything a fast client sends buys latency for that client and pays for it in
//! resident memory, and the bill arrives as an OOM kill at the worst possible moment — under load, which
//! is exactly when it was accepting the most. So every source has a bound, and a source at its bound is
//! **refused** with the one retryable error kind (D-23's `Overloaded`).
//!
//! ## Why per source and not one global bound
//!
//! One global bound lets a single fast source consume the whole allowance and starve every other. The
//! per-source bound makes a noisy neighbour a problem for its own queue and nobody else's, which is the
//! only arrangement in which "the server stayed up" and "your ingest kept working" can both be true.
//!
//! ## What is *not* here
//!
//! No time-based decision. Nothing expires, nothing flushes on a timer, and there is no deadline after
//! which a queued batch is dropped — D-23 keeps the wall clock out of the server entirely, and an
//! admission policy that consulted it would be the first exception. A batch leaves a queue when the
//! engine takes it, and not otherwise.

use std::collections::BTreeMap;

/// How many batches one source may have queued but not yet sealed.
///
/// **A tuned constant, and therefore in the ledger with its receipt** (`DEFAULT_SOURCE_QUEUE_BOUND`,
/// `testing/evidence/c9-bounds.json`). What it steers is *fairness and latency*: a source that has 64
/// batches waiting has already been told to slow down long before the byte bound below matters, and a
/// batch that sits behind 64 others is a batch whose client should have been refused sooner.
///
/// It does **not** bound memory, and pretending otherwise was the first version of this file. See
/// [`DEFAULT_SOURCE_QUEUE_BYTES`].
pub const DEFAULT_SOURCE_QUEUE_BOUND: usize = 64;

/// How many **bytes** one source may have queued but not yet sealed.
///
/// **This is the bound that makes the queue bounded**, and it exists because measuring the other one
/// showed it does not. A count bound admits 64 batches of *any* size: at the widest batch
/// `testing/evidence/c9-bounds.json` measures — 1,000 rows with a 480-byte column, 514,051 bytes — 64
/// batches is 32.9 MB for a single source, and a client sending wider rows makes that figure whatever it
/// likes. "Bounded queue" then means bounded in a unit nobody's memory is measured in, which is I-9's
/// definition of an undeclared unbounded state.
///
/// 8 MiB per source, and the two bounds bind in different places by design: at the widest measured batch
/// the byte bound admits 16 batches (the count bound never fires), and at a 100-row narrow batch — 3,451
/// bytes — the count bound fires first at 64 batches, or 220 KiB. Small batches are governed by count,
/// large ones by bytes, and neither leaves the other unbounded.
pub const DEFAULT_SOURCE_QUEUE_BYTES: usize = 8 * 1024 * 1024;

/// What a source is allowed to have in flight.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Batches per source, queued and unsealed.
    pub queue_bound: usize,
    /// Bytes per source, queued and unsealed — the bound that actually bounds memory.
    pub queue_bytes: usize,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            queue_bound: DEFAULT_SOURCE_QUEUE_BOUND,
            queue_bytes: DEFAULT_SOURCE_QUEUE_BYTES,
        }
    }
}

/// The verdict on one append.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Admit,
    /// The source is at its bound. The client must back off and retry (D-23).
    Overloaded {
        source_depth: usize,
        bound: usize,
    },
    /// The source has too many bytes queued. Also retryable: a seal frees them.
    OverloadedBytes {
        queued_bytes: usize,
        batch_bytes: usize,
        bound: usize,
    },
    /// One batch is larger than a source's whole byte budget. **Not** retryable, and that distinction is
    /// the point: `Overloaded` promises that backing off helps, and no amount of waiting makes this batch
    /// fit. A client must split it.
    TooLarge {
        batch_bytes: usize,
        bound: usize,
    },
}

/// Per-source queue depths, and the policy that bounds them.
#[derive(Clone, Debug, Default)]
pub struct Admission {
    policy: Policy,
    /// Source → batches queued and not yet sealed. Ordered, so `render` is stable (I-2).
    depths: BTreeMap<String, usize>,
    /// Source → bytes queued and not yet sealed.
    bytes: BTreeMap<String, usize>,
    /// Appends refused, per source. Reported, because a server silently shedding load looks healthy.
    refused: BTreeMap<String, u64>,
}

impl Admission {
    #[must_use]
    pub fn new(policy: Policy) -> Admission {
        Admission {
            policy,
            depths: BTreeMap::new(),
            bytes: BTreeMap::new(),
            refused: BTreeMap::new(),
        }
    }

    #[must_use]
    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Ask whether one more batch of `batch_bytes` from `source` may be queued.
    ///
    /// Asking does not change anything: [`Admission::admitted`] records the batch once the append has
    /// actually succeeded. Counting on the way in would inflate the depth for a batch the log then
    /// refused, and the depth is the number the memory bound is argued from.
    ///
    /// The byte bound is checked **before** the count bound, because a batch that can never fit deserves
    /// the answer that says so rather than an invitation to retry.
    pub fn check(&mut self, source: &str, batch_bytes: usize) -> Verdict {
        if batch_bytes > self.policy.queue_bytes {
            *self.refused.entry(source.to_owned()).or_insert(0) += 1;
            return Verdict::TooLarge {
                batch_bytes,
                bound: self.policy.queue_bytes,
            };
        }
        let queued_bytes = self.bytes.get(source).copied().unwrap_or(0);
        if queued_bytes.saturating_add(batch_bytes) > self.policy.queue_bytes {
            *self.refused.entry(source.to_owned()).or_insert(0) += 1;
            return Verdict::OverloadedBytes {
                queued_bytes,
                batch_bytes,
                bound: self.policy.queue_bytes,
            };
        }
        let depth = self.depths.get(source).copied().unwrap_or(0);
        if depth >= self.policy.queue_bound {
            *self.refused.entry(source.to_owned()).or_insert(0) += 1;
            return Verdict::Overloaded {
                source_depth: depth,
                bound: self.policy.queue_bound,
            };
        }
        Verdict::Admit
    }

    /// Record that a batch from `source` was appended and is now pending a seal.
    pub fn admitted(&mut self, source: &str, batch_bytes: usize) {
        *self.depths.entry(source.to_owned()).or_insert(0) += 1;
        *self.bytes.entry(source.to_owned()).or_insert(0) += batch_bytes;
    }

    /// Every queue empties when an epoch seals: that is what a seal *is* (S-6).
    pub fn sealed(&mut self) {
        self.depths.clear();
        self.bytes.clear();
    }

    /// Bytes `source` has queued and unsealed.
    #[must_use]
    pub fn queued_bytes(&self, source: &str) -> usize {
        self.bytes.get(source).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total_queued_bytes(&self) -> usize {
        self.bytes.values().sum()
    }

    #[must_use]
    pub fn depth(&self, source: &str) -> usize {
        self.depths.get(source).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total_depth(&self) -> usize {
        self.depths.values().sum()
    }

    #[must_use]
    pub fn refused(&self, source: &str) -> u64 {
        self.refused.get(source).copied().unwrap_or(0)
    }

    #[must_use]
    pub fn total_refused(&self) -> u64 {
        self.refused.values().sum()
    }

    /// A deterministic rendering, for `/health` and for the gates.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = format!(
            "admission: bound {} batches / {} bytes per source · {} queued · {} queued_bytes · {} refused\n",
            self.policy.queue_bound,
            self.policy.queue_bytes,
            self.total_depth(),
            self.total_queued_bytes(),
            self.total_refused()
        );
        for (source, depth) in &self.depths {
            out.push_str(&format!(
                "  source {source}: {depth} queued · {} bytes\n",
                self.queued_bytes(source)
            ));
        }
        for (source, refused) in &self.refused {
            out.push_str(&format!("  source {source}: {refused} refused\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A policy with a generous byte bound, for the tests about counting.
    fn counting(queue_bound: usize) -> Policy {
        Policy {
            queue_bound,
            queue_bytes: 1 << 30,
        }
    }

    #[test]
    fn a_source_at_its_bound_is_refused_and_the_refusal_is_counted() {
        let mut admission = Admission::new(counting(3));
        for _ in 0..3 {
            assert_eq!(admission.check("a", 100), Verdict::Admit);
            admission.admitted("a", 100);
        }
        assert_eq!(
            admission.check("a", 100),
            Verdict::Overloaded {
                source_depth: 3,
                bound: 3
            }
        );
        assert_eq!(
            admission.refused("a"),
            1,
            "a server silently shedding load looks healthy, so refusals are counted"
        );
    }

    /// A noisy source must not starve a quiet one. This is the whole argument for per-source bounds.
    #[test]
    fn one_source_at_its_bound_does_not_block_another() {
        let mut admission = Admission::new(counting(2));
        for _ in 0..2 {
            admission.admitted("loud", 100);
        }
        assert!(matches!(
            admission.check("loud", 100),
            Verdict::Overloaded { .. }
        ));
        assert_eq!(
            admission.check("quiet", 100),
            Verdict::Admit,
            "a noisy neighbour is a problem for its own queue and nobody else's"
        );
    }

    #[test]
    fn a_seal_empties_every_queue() {
        let mut admission = Admission::new(counting(2));
        admission.admitted("a", 100);
        admission.admitted("b", 250);
        assert_eq!(admission.total_depth(), 2);
        assert_eq!(admission.total_queued_bytes(), 350);
        admission.sealed();
        assert_eq!(admission.total_depth(), 0);
        assert_eq!(
            admission.total_queued_bytes(),
            0,
            "a seal frees the bytes as well as the count, or the byte bound would only ever tighten"
        );
        assert_eq!(admission.check("a", 100), Verdict::Admit);
    }

    /// Checking is not counting: a batch the log then refuses must not inflate the depth the memory
    /// bound is argued from.
    #[test]
    fn checking_does_not_queue() {
        let mut admission = Admission::new(counting(1));
        assert_eq!(admission.check("a", 100), Verdict::Admit);
        assert_eq!(admission.check("a", 100), Verdict::Admit);
        assert_eq!(admission.depth("a"), 0);
        assert_eq!(admission.queued_bytes("a"), 0);
    }

    /// **The bound that bounds memory.** A count bound admits batches of any size; the byte bound is what
    /// makes "bounded queue" a statement about memory.
    #[test]
    fn a_source_is_bounded_in_bytes_and_not_only_in_batches() {
        let mut admission = Admission::new(Policy {
            queue_bound: 1_000,
            queue_bytes: 1_000,
        });
        admission.admitted("a", 600);
        // Well inside the count bound, and over the byte bound.
        assert_eq!(
            admission.check("a", 600),
            Verdict::OverloadedBytes {
                queued_bytes: 600,
                batch_bytes: 600,
                bound: 1_000
            },
            "1,000 batches of 600 bytes would be 600 KB queued for one source"
        );
        assert_eq!(
            admission.check("a", 300),
            Verdict::Admit,
            "a batch that fits still fits"
        );
        admission.sealed();
        assert_eq!(admission.check("a", 600), Verdict::Admit);
    }

    /// A batch bigger than the whole budget is **refused**, not asked to retry: waiting cannot make it fit.
    #[test]
    fn a_batch_larger_than_the_budget_is_not_invited_to_retry() {
        let mut admission = Admission::new(Policy {
            queue_bound: 64,
            queue_bytes: 1_000,
        });
        assert_eq!(
            admission.check("a", 1_001),
            Verdict::TooLarge {
                batch_bytes: 1_001,
                bound: 1_000
            }
        );
        assert_eq!(
            admission.refused("a"),
            1,
            "it is still a refusal, and refusals are counted"
        );
    }
}
