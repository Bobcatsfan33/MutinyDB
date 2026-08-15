# C12 accelerator protocol

This protocol is the falsifiable boundary for C12. It is committed before the accelerator source or any
measurement exists. D-28 is authoritative if this summary and the decision record ever disagree.

## Hypothesis

A fused GPU kernel can filter two aligned `Int64` Arrow columns and aggregate the selected values fast
enough to justify a later accelerator design phase for Schweep's one-shot cold path.

The spike answers only that question. It does not prove standing-query acceleration, joins, grouped
aggregation, strings, nulls, retractions, spill, multi-GPU execution, or production portability.

## Frozen experiment

- Data sizes: 100,000; 1,000,000; 10,000,000 rows.
- Values: deterministic signed `Int64` keys and measures, represented by Arrow arrays and reused by both
  candidates.
- Query shape: one predicate over the key column fused with one exact integer sum over the measure column.
- Instrument: release build, one warm-up, then eleven alternating paired rounds at every size.
- CPU candidate: the current C10 one-shot circuit implementation.
- GPU candidate: one fused kernel producing bounded partial sums, followed by a host final reduction.
- Included in each GPU sample: Arrow-buffer copy/adoption cost at the device boundary, output allocation,
  command-buffer and encoder construction, dispatch, synchronization, and host final reduction.
- Excluded and reported separately: one-time device discovery, runtime shader compilation, pipeline
  creation, and command-queue creation.
- Correctness: exact `Int64` equality before timing and after every candidate execution.

## Frozen verdict

`GO` requires exactness, at least 2.00x median GPU speedup at both one million and ten million rows,
break-even by one million rows, complete samples with no discarded rounds, and committed-source
reproducibility on a real supported GPU. Anything else is `NO-GO`.

Neither verdict changes the production engine in C12. The evidence file will record the verdict and each
criterion independently so a prose conclusion cannot disagree with the measurements.

## Result

`GO` for a later accelerator design phase; no production GPU code ships. On the recorded Apple M2, the
median speedups were 56.76x / 89.85x / 85.98x at 100,000 / 1,000,000 / 10,000,000 rows. Break-even was at
or below 100,000 rows, and the three warm-up pairs plus all 66 measured executions agreed exactly.

The receipt is `testing/evidence/c12-accelerator.json`. The CI test `c12_evidence` recomputes each median
from the eleven raw samples, recomputes the speedup, and applies the thresholds above. These results are
specific to one fused integer filter/sum on Metal versus the existing general C10 one-shot circuit. They
are not evidence for a production implementation, wider SQL semantics, or another GPU platform.
