# PROGRESS

Sprint-by-sprint status: **what is proven, and by which test**. A claim here without a named test
that proves it is a violation of I-10, so every row below points at something runnable.

| Sprint | Status |
| --- | --- |
| **C0** — the oracle, the harness, and the rules | **complete; exit gate green in CI** |
| **C1** — linear operators + the first real circuit | **complete; exit gate green in CI** |
| **C2** — join | **complete; exit gate green in CI** |
| **C3** — aggregates and distinct | **complete; exit gate green in CI** |
| **C4** — durability | **exit gate green in CI; `RocksBackend` NOT delivered — see below** |
| **C5** — the SQL frontend and the incrementalizer | **complete; exit gate green in CI** |
| **C6** — the memo: standing queries and shared circuitry | **complete; exit gate green in CI** |
| **C7** — one-shot queries, Parquet ground truth, compaction | **complete; exit gate green in CI** |
| **C8** — state spill and cold-start honesty | **complete; exit gate green in CI; D-18 amended additively by D-25** |
| **C9** — `schweepd`: the server | **complete; exit gate green in CI; the real `kill -9` now exists** |
| **C10** — performance | **implementation complete; required CI checks are the exit gate** |
| C11 … C13 | not started |

> **Correction, made in the rename session (2026-08-11).** This table read `C5 … C13 | not started`
> while C5, C6, C7 and C8 were each complete with a green gate and a full section below. Four sprints'
> worth of status was wrong in the one place a reader looks first, and the sections underneath were
> right the whole time — which is the failure mode a summary table has and its sections do not.
>
> Corrected visibly rather than silently, because a status table that has been wrong once should say so:
> a reader who trusted it in that window deserves to know the window existed. **The rule this leaves
> behind:** the table is part of the sprint's exit, not a thing to update afterwards, and a sprint that
> adds a section without adding its row has not finished.

**How to read this document.** Each sprint below states what its gate proved, what it did *not* prove,
and what the next sprint needs. The "does not prove" sections are the load-bearing ones: they are where a
limitation is recorded before a user finds it (I-10), and several of them carry a **Scheduled: Cn** marker
naming the sprint that must resolve them.

---

## C0 — the oracle, the harness, and the rules

**Objective (§6):** stand up the workspace with the correctness machinery *before any engine code
exists.* That is what happened: there is no engine in this repository, deliberately.

### The exit gate

§6 C0 names four conditions. All four are met.

| Gate condition | Proven by | Result |
| --- | --- | --- |
| CI green (fmt, clippy `-D warnings`, test, no-network) | `.github/workflows/ci.yml` | all green, plus the aggregate `ci` check — 5 jobs |
| Harness runs oracle-vs-oracle over 1,000 randomized scenarios | `oracle_vs_oracle_over_one_thousand_randomized_scenarios` | 1,000 scenarios, 4,668 epochs, 5,668 answer comparisons, 0 divergences |
| Property tests for Z-set algebra pass | `crates/schweep-zset/tests/properties.rs` | 13 property tests |
| A seeded scenario is reproducible byte-for-byte from its seed | `a_seed_reproduces_its_scenario_byte_for_byte`, `a_seed_reproduces_its_run_byte_for_byte` | byte-identical scenario *and* run |

**122 tests**, zero ignored, zero skipped, zero flaky. (The workspace total is now 152; the
extra 30 arrived with C1 and the refactor that preceded it.)

### What is proven, and by which test

**Z-set algebra** — `crates/schweep-zset/`

| Claim | Test |
| --- | --- |
| Addition is commutative (§5.2) | `addition_is_commutative`, and byte-identically after consolidate in `commutativity_is_byte_identical_after_consolidate` |
| Addition is associative (§5.2) | `addition_is_associative` |
| `consolidate` is idempotent (§5.2) | `consolidate_is_idempotent` |
| `negate ∘ negate = identity` (§5.2) | `double_negation_is_identity` |
| **I-5** as arithmetic: a retraction of everything cancels everything | `a_plus_negative_a_is_empty` |
| **I-2**: canonical form depends on the data, not on entry order | `canonical_form_is_invariant_under_permutation` |
| Canonical form is sorted, deduplicated, zero-free (S-8) | `canonical_form_is_sorted_deduplicated_and_zero_free` |
| Consolidation preserves total weight | `consolidate_preserves_total_weight` |
| The total order on values, nulls first (S-7) | `s7_null_sorts_before_every_non_null`, `s7_within_type_orders`, `ordering_is_total_and_antisymmetric_across_variants` |
| Weight overflow is refused, not wrapped (D-11) | `negate_refuses_i64_min_rather_than_saturating`, `consolidate_reports_weight_overflow_rather_than_wrapping` |
| Arrow round-trips entries exactly (D-2) | `arrow_round_trip_preserves_entries`, `from_arrow_agrees_with_from_entries` |

**Semantics of rungs 1–3** — `crates/schweep-oracle/tests/semantics.rs` (39 tests)

| Claim | Test |
| --- | --- |
| Join multiplies weights (S-26) | `s26_join_multiplies_weights` |
| A null join key never matches, not even another null (S-13, S-26) | `s26_a_null_join_key_never_matches_even_another_null` |
| Grouping puts all nulls in **one** group (S-28) — the other side of the same coin | `s28_grouping_puts_all_nulls_in_one_group` |
| A drained group vanishes; no phantom `(key, 0)` row (S-29) | `s29_a_group_drained_to_zero_rows_vanishes_rather_than_zeroing` |
| Retracting the current MIN reveals the second-smallest (S-30) | `s30_retracting_the_current_min_reveals_the_second_smallest` |
| A value retracted to weight zero stops being the MIN | `s30_a_value_retracted_to_weight_zero_is_no_longer_the_min` |
| `COUNT(x)` of an all-null group is 0; `SUM` is NULL (S-30) | `s30_an_all_null_group_counts_zero_and_sums_to_null` |
| AVG lands exactly on the weighted quotient under retraction (S-31) | `s31_avg_lands_exactly_on_the_weighted_quotient_under_retraction` |
| Weights are multiplicities in every aggregate (S-30) | `s30_weights_are_multiplicities`, `s30_count_star_counts_weights_and_count_x_skips_nulls` |
| Projection merges rows and sums weights (S-25) | `s25_projection_merges_rows_and_sums_their_weights` |
| Filter preserves weights exactly (S-24) | `s24_filter_preserves_weights_exactly` |
| `WHERE NOT p` is not the complement of `WHERE p` (S-17) | `s17_where_not_p_is_not_the_complement_of_where_p` |
| Kleene truth tables, including `F AND N = F` (S-15) | `s15_kleene_truth_tables` |
| `AND`/`OR` do not short-circuit (S-15) | `s15_and_does_not_short_circuit` |
| CASE takes the first TRUE branch and evaluates only that branch (S-18) | `s18_case_takes_the_first_true_branch_and_skips_null_conditions`, `s18_case_does_not_evaluate_the_branch_it_did_not_take` |
| Overflow and division by zero are errors, not wraps or nulls (S-20, S-21, D-11) | `s20_overflow_is_an_error_not_a_wrap`, `s21_division_and_modulo_by_zero_and_the_min_over_minus_one_case` |
| Retracting a row that was never there is a malformed history (S-5, D-12) | `s5_retracting_a_row_that_is_not_there_is_a_malformed_history`, `s5_retracting_more_copies_than_exist_is_a_malformed_history` |
| An empty epoch does not move the answer (S-6) | `s6_an_empty_epoch_does_not_move_the_answer` |
| Every refusal names its construct (S-12) | `s10_…_refused_as_unqualified`, `s19_there_are_no_implicit_conversions`, `s19_an_untyped_null_literal_is_refused_…`, `s33_a_group_by_with_no_keys_is_refused`, `s26_a_cross_join_is_refused_by_name`, `s3_a_float_column_cannot_be_declared_…` |
| Binding fails identically on an empty and a populated database (S-12) | `s12_binding_fails_the_same_way_on_an_empty_database` |
| **I-2**: two oracles fed the same log answer byte-identically | `i2_two_oracles_fed_the_same_log_give_byte_identical_answers` |

**The harness** — `testing/differential/`

| Claim | Test |
| --- | --- |
| 1,000 randomized scenarios, oracle vs oracle, compared at every sealed epoch | `oracle_vs_oracle_over_one_thousand_randomized_scenarios` |
| The harness **catches a wrong implementation** — it is not comparing nothing | `the_harness_catches_a_deliberately_wrong_implementation` (155 of 155 sabotaged runs caught) |
| A divergence report is actionable alone: seed, epoch, both answers, whole scenario | `a_divergence_report_contains_everything_needed_to_reproduce_it` |
| Divergence is reported at the **first** epoch answers part, not the last (I-3) | `divergence_is_reported_at_the_first_epoch_where_answers_part` |
| A seed reproduces its scenario and its run byte-for-byte (I-2) | `a_seed_reproduces_its_scenario_byte_for_byte`, `a_seed_reproduces_its_run_byte_for_byte` |
| Different seeds produce different scenarios | `different_seeds_produce_different_scenarios` |
| Entry order within an epoch changes no answer (S-6, I-2) | `shuffling_the_entries_within_an_epoch_does_not_change_any_answer` |
| The RNG stream is value-stable, so recorded seeds keep their meaning | `the_stream_is_value_stable_for_a_known_seed` |

**Generator coverage** — asserted, not assumed. §7 requires certain shapes always be produced;
the gate fails if any of them stops appearing.

| Required shape (§7) | Test |
| --- | --- |
| Retractions **in epoch one** (the §6 C0 pitfall) | `retractions_appear_in_the_first_epoch_of_some_scenarios` |
| Retractions common, not rare (> 300 of 1,000 scenarios) | same test |
| Weight multiplicities above 1 (> 150 of 500 scenarios) | `weights_above_one_are_common` |
| Same-epoch retract-and-insert of the same row | `Operation::ChurnSameEpoch` asserted in the gate |
| Update in place (retract + insert, one epoch) | `Operation::UpdateInPlace` asserted in the gate |
| Empty epochs, and empty inputs | asserted in the gate; `an_empty_epoch_never_changes_the_answer` |
| All four query families (rungs 1, 2, 3, and 2→3) | asserted in the gate |
| Scenarios that produce a **non-empty answer** ≥ 40% (measured: 53%) | `a_healthy_share_of_scenarios_produce_a_non_empty_answer` |

### What C0 does **not** prove

Stated plainly, because a progress document that only lists wins is marketing.

- **Nothing about incremental evaluation.** There is no engine: no operators, no circuit, no
  scheduler, no result stores. Every answer in this repository is produced by recomputing from
  scratch.
- **Nothing about the oracle's correctness against SQL.** The oracle *is* the spec (§5.1); the
  tests pin it to `docs/SEMANTICS.md`, and both could be wrong together about what a user expects.
  That risk is real and is what the dialect ladder and, later, real workloads reduce.
- **Oracle-vs-oracle does not test the oracle.** It tests the harness. Both sides run the same
  code, so agreement is guaranteed and only the machinery around it is under test. The
  `SaboteurEngine` is what stops that from being vacuous.
- **Nothing about durability, crash recovery, concurrency, or the network.** C4 and C9.
- **No performance claim of any kind**, and no benchmark artifact exists.
  `testing/evidence/registry.json` is empty because nothing is tuned. Both `schweep-zset` and
  `schweep-oracle` are knowingly slow: consolidation materialises rows out of the columnar batch,
  and the oracle replays the entire log prefix on every question, with a nested-loop join.
- **I-1 is not yet exercised in anger.** The oracle law needs two different implementations. It
  gets one in C1.
- **I-3, I-4, I-6, I-7, I-8, I-9 have no engine to hold to them yet.** I-2, I-5, and I-10 are
  exercised at the level C0 has.

### Decisions taken during C0

Recorded in `docs/DECISIONS.md`: **D-10** `Float64` is result-only (`AVG` is the sole source);
**D-11** arithmetic errors are errors, not wraps or nulls; **D-12** the oracle rejects malformed
history rather than defining an answer for it; **D-13** nulls sort first, everywhere, with no
modifier.

Deliberately *not* decided, each with the sprint that must settle it: **Q-1** fixed-point decimals
for non-integer arithmetic; **Q-2** what an evaluation error does to a standing query (by C5);
**Q-3** grand-total aggregation over an empty input (by C5).

Three semantics rules were added mid-sprint, when writing the oracle exposed questions the first
draft of `docs/SEMANTICS.md` had not answered: null literals carry a type (S-19), `AND`/`OR` do not
short-circuit (S-15), every output column is declared nullable (S-11). In each case the document
moved first and the code followed, which is the order §10 requires.

### What C1 needs

C1 is *linear operators + the first real circuit*. Everything it needs from C0 exists:

- **The seam.** `EngineUnderTest` in `testing/differential/src/engine.rs` is what `schweep-circuit`
  implements. Add an adapter, put it on one side of `compare`, and the 1,000-scenario gate becomes
  a real engine-vs-oracle gate — that is the C1 exit gate.

  *Correction, made in C1:* this section originally claimed "nothing in the harness mentions the
  oracle's types". That was false — the trait imported `schweep_oracle::Query`, and a comment in
  `engine.rs` asserted the opposite of what the file did. It cost C1 a preparatory refactor
  (D-14) rather than "one file". The claim is true now because the types moved to a neutral
  crate, not because the wording was softened.
- **The scenarios.** The generator already emits retractions, multiplicities, churn, updates, and
  empty epochs across four query families. C1's gate ("randomized filter/project scenarios
  including retractions") is a filter over `Family::FilterProject`, not new generator work.
- **The I-2 gate.** C1 must show that two runs of one scenario produce byte-identical state and
  answers. `a_seed_reproduces_its_run_byte_for_byte` is that test with the engine substituted in.
- **The spec.** `docs/SEMANTICS.md` S-23, S-24, S-25 define scan, filter, and projection, and
  `crates/schweep-oracle/tests/semantics.rs` pins them. C1's operators are held to those rules and
  do not get to reinterpret them.

One thing C1 will have to decide, flagged now rather than discovered later: `EpochInput` and the
oracle's `EpochDeltas` are two spellings of the same idea, and they exist separately only because
`schweep-log` does not arrive until C4. When the circuit lands, one of them should become the
shared type — most naturally in `schweep-zset`, since it is the delta representation and every
crate already depends on it.

Per the sprint protocol in `CLAUDE.md`, **C1 does not begin in the session that finished C0.**

---

## C1 — linear operators and the first real circuit

**Objective (§6):** the smallest true incremental engine. There is now an engine: it maintains an
answer from deltas and never looks at the whole input, and it is checked against the oracle at
every sealed epoch.

### The exit gate

§6 C1 names two conditions. Both are met.

| Gate condition | Proven by | Result |
| --- | --- | --- |
| Differential harness green, engine-vs-oracle, over randomized filter/project scenarios **including retractions** | `engine_vs_oracle_over_a_thousand_filter_project_scenarios` | 1,118 rung-1 scenarios drawn from 4,400 seeds, 5,187 epochs, 6,305 answer comparisons, **0 divergences** |
| I-2 gate: two runs of the same scenario produce byte-identical **state and answers** | `i2_two_runs_of_a_scenario_produce_byte_identical_state_and_answers` | 400 scenarios, identical fingerprints and answers, including from a scenario regenerated from its seed |

The "including retractions" clause is measured on the population the gate actually ran, not on the
generator as a whole: of those 1,118 scenarios, **894 contain a retraction, 312 retract in epoch
one, and 863 use a weight above 1** (`the_gate_population_is_full_of_retractions`). A family filter
that quietly selected a corner without retractions would fail that test.

**152 tests across the workspace**, zero ignored, zero skipped, zero flaky (two consecutive full
runs, identical results).

### This is the first time I-1 has meant anything

C0's harness compared the oracle to itself, which tested the harness. C1 puts two genuinely
different implementations on the two sides:

- the **circuit** sees only what changed, pushes it through stateless operators, and folds the
  output delta into a maintained integral — reading the answer is a lookup;
- the **oracle** replays the entire log from epoch 1 and recomputes from scratch, every time.

They agree byte for byte at every sealed epoch over 6,305 comparisons.

**And the gate has teeth — checked, not assumed.** Two deliberate mutations were introduced and
the gate caught both before being reverted:

| Mutation | Caught |
| --- | --- |
| Filter admits rows whose predicate is `NULL` (the classic S-17 bug) | seed 11, epoch 1 |
| Result store overwrites instead of accumulating — an error only a multi-epoch history reveals | seed 21, epoch 1 |

Worth recording alongside that: under both mutations the **I-2 test still passed**. A deterministic
bug is still deterministic. I-2 proves reproducibility, never correctness; only I-1 does that, and
the two gates are not substitutes.

### What is proven, and by which test

**The operators** — `crates/schweep-ops/`

| Claim | Test |
| --- | --- |
| Filter keeps TRUE only, weights untouched (S-17, S-24) | the differential gate; `a_hand_built_circuit_maintains_its_answer_from_deltas` |
| Projection merges rows and sums weights (S-25) | `a_hand_built_circuit_maintains_its_answer_from_deltas` |
| A non-Boolean predicate is refused at construction, not at data time (S-17) | `a_non_boolean_predicate_is_refused_at_construction` |
| **Linear operators declare and hold no state** — §6 C1's pitfall, as an assertion | `linear_operators_declare_and_hold_no_state`, plus the runtime check in every step |
| Projection's output schema comes from the shared binder, so it cannot drift from the oracle's (S-11, D-14) | `Project::new` calls `schweep_plan::projection_schema`; the gate would show any drift as a schema mismatch |

**The circuit** — `crates/schweep-circuit/`

| Claim | Test |
| --- | --- |
| A hand-built circuit maintains its answer from deltas across epochs, including retractions | `a_hand_built_circuit_maintains_its_answer_from_deltas` |
| A row inserted and retracted in one epoch leaves no trace | `same_epoch_churn_leaves_no_trace` |
| A drained row leaves no zero-weight tombstone | `a_row_retracted_to_zero_leaves_no_tombstone` |
| An empty epoch advances the clock and nothing else (S-6, I-3) | `an_empty_epoch_advances_the_clock_and_nothing_else` |
| A circuit ignores deltas for tables it does not read | `deltas_for_a_table_this_circuit_does_not_read_are_ignored` |
| Wiring out of dependency order is refused, which is what makes the schedule deterministic (I-2) | `the_builder_refuses_wiring_that_is_not_in_dependency_order` |
| Arity is checked at wiring time, not discovered at step time | `the_builder_refuses_an_operator_wired_to_the_wrong_number_of_inputs` |
| **A failed step advances nothing** — the epoch and the result store are exactly where they were (I-3) | `an_evaluation_error_aborts_the_step_without_advancing_the_epoch` |
| Result store: integral maintained by addition, order-independent, overflow refused not wrapped | seven tests in `result_store.rs` |
| The state fingerprint is stable and reports wiring, declarations, and store | `the_state_fingerprint_is_stable_and_reports_what_is_held` |

**The engine, against the oracle** — `testing/differential/tests/c1_engine_vs_oracle.rs`

| Claim | Test |
| --- | --- |
| **I-1** over 1,118 randomized rung-1 scenarios, every sealed epoch | `engine_vs_oracle_over_a_thousand_filter_project_scenarios` |
| **I-2** byte-identical state and answers across runs and across regeneration from seed | `i2_two_runs_of_a_scenario_produce_byte_identical_state_and_answers` |
| A one-shot query is the degenerate standing query (§0): the whole history as one epoch gives the same answer as epoch-by-epoch | `feeding_the_whole_history_as_one_epoch_gives_the_same_answer` |
| What the engine cannot run it refuses **by name**, naming the sprint that brings it | `the_engine_refuses_beyond_rung_one_and_names_the_sprint` |
| The harness can still fail against a real circuit, not only against the oracle | `the_gate_would_catch_a_wrong_circuit` (150 of 150) |

**I-9, at the level C1 has.** Every operator declares a `StateBound` and reports its actual state
size, and `Circuit::step` checks the declaration against the report after *every* step. In C1 every
declaration is `Stateless` and every report is zero, so the check is the executable form of §6 C1's
pitfall rather than a warning in a comment. Real bounds — and the accounting that checks them —
arrive with the join in C2, which is the first sprint with state to account for.

### What C1 does **not** prove

- **Nothing about join, aggregation, or distinct.** The engine refuses all three by name. Of the
  4,400 seeds swept, 3,282 were skipped as outside rung 1; that number is printed by the gate
  rather than hidden, because "1,000 scenarios passed" and "three quarters were not attempted" are
  the same sentence.
- **The hard part of incrementality is still ahead.** Filter and project are *linear*:
  `f(a + b) = f(a) + f(b)`, so the incremental form is a one-line consequence rather than a
  theorem. C1 proves the machinery — wiring, scheduling, epoch discipline, result stores, state
  accounting — before C2 introduces an operator where the equality has three terms and one of them
  is the one everybody forgets.
- **Errors are not settled, and the gate stays away from them.** C1 found that the oracle and the
  circuit disagree about an evaluation error's *lifetime*: the oracle recomputes over the integral
  so a bad row raises forever, while the circuit sees each row once so it raises once. Neither is
  wrong; nothing has decided what a standing query does with an error. Recorded under **Q-2** in
  `docs/DECISIONS.md`, and the gate asserts that zero scenarios raised, so it never silently
  depends on the undecided part.
- **Shared scalar code is not covered by I-1.** The oracle and the engine call the same expression
  evaluator (D-14), so a bug inside it produces the same wrong answer on both sides and the harness
  cannot see it. `schweep-plan`'s own unit tests pin that code to `docs/SEMANTICS.md` directly.
- **No durability, no sharing, no SQL, no network.** C4, C6, C5, C9. Circuits are hand-built by
  design (§6 C1); the incrementalizer that compiles a plan into one is C5.
- **No performance claim.** The engine has never been benchmarked and no artifact exists. Both
  implementations are knowingly slow: operators materialise rows out of the columnar batch, and the
  oracle replays the whole log per question. `testing/evidence/registry.json`'s engine-constant list
  is still empty, and `no_engine_constant_steers_behaviour_without_a_receipt` fails if that changes
  without a receipt.

### The pre-C1 refactor

Before any engine code, three things from review (one commit, no behaviour change):

- **D-14 · `schweep-plan`.** The plan IR, the binder, and the scalar expression library left
  `schweep-oracle` for a neutral crate — recorded in `docs/DECISIONS.md` *before* the move, because
  it extends §5's crate map. From C1 there are two implementations of the query surface and neither
  may own the definition of what a query is.
- **One delta type.** `schweep-zset::EpochDeltas` replaced the harness's `EpochInput` and the
  oracle's private copy. This corrected a comment in `engine.rs` that asserted the opposite of what
  the file did, and a claim in C0's section of this document that repeated it. Both now say what is
  true, and say that they were wrong.
- **Ledger receipts (I-10).** The scenario generator's nine tuned constants are in
  `testing/evidence/registry.json` with the measured number that justifies each, backed by
  `c0-generator-coverage.json` — regenerable by a committed binary and checked by
  `the_committed_coverage_artifact_still_matches_the_generator`, so the receipt cannot go stale
  quietly.

### What C2 needs

C2 is *join* — the first bilinear operator, and §6 calls it the hardest correctness class in the
engine. What C1 leaves it:

- **The `Operator` trait already fits it.** `step(&[&ZSetBatch])` takes a slice, so a binary
  operator needs no trait change; `StateBound::ProportionalToInputs { inputs: ["left", "right"] }`
  is already the vocabulary for declaring O(|A| + |B|), and `Circuit::step` already calls the
  check. What C2 must add is the *accounting* — `check_state_declarations` currently accepts any
  actual size for a non-`Stateless` declaration, because nothing declares one yet. That is the one
  place in the C1 code that is deliberately unfinished, and it is named here rather than left to be
  discovered.
- **The wiring is already a DAG.** `CircuitBuilder::add` takes a vector of inputs and validates
  arity and ordering, so a two-input node needs no builder change.
- **The scenarios exist.** `Family::Join` and `Family::JoinAggregate` are already generated —
  3,282 of the 4,400 seeds C1 skipped are mostly these. C2's gate widens `CircuitEngine::claims`
  to include `Family::Join` and the same sweep starts exercising it.
- **The delta-delta term needs its own scenario.** §6 C2's pitfall is that `ΔA⋈ΔB` is the term
  every implementer forgets, and the gate must have a scenario that fails if it is missing — both
  sides inserting matching rows in the *same* epoch. The generator produces multi-table epochs
  today, but nothing yet *isolates* that case or asserts it occurred. Writing that scenario family
  first, before the operator, is C2's first task.
- **Join weights multiply, and the oracle already says so.** `s26_join_multiplies_weights` and
  `s26_a_null_join_key_never_matches_even_another_null` pin the semantics C2's operator must match.

Per the sprint protocol in `CLAUDE.md`, **C2 does not begin in the session that finished C1.**

---

## C2 — join

**Objective (§6):** the first bilinear operator — "the hardest correctness class in the engine".

### The exit gate

| Gate condition (§6 C2) | Proven by | Result |
| --- | --- | --- |
| Differential harness green over join scenarios | `engine_vs_oracle_over_randomized_join_scenarios` | 1,090 join scenarios from 4,400 seeds · 5,161 epochs · **6,251 answer comparisons · 0 divergences** |
| multi-key batches | `a_multi_key_batch_joins_only_the_matching_keys` | joins only the matching keys |
| retractions of joined rows | `retracting_a_joined_row_retracts_the_output`, `retracting_one_side_retracts_the_joined_rows` | both joined rows retract from one retraction |
| updates (retract+insert same epoch) | `a_same_epoch_update_moves_the_joined_row`, `a_same_epoch_update_on_both_sides` | |
| weight multiplicities > 1 | `weights_multiply`, `both_sides_inserting_together_with_multiplicities` | 3 × 2 = 6 |
| **the delta-delta term, with a scenario that isolates it** | `the_delta_delta_term_is_the_whole_answer_when_both_sides_insert_together`, `both_sides_inserting_a_matching_row_in_one_epoch` | see below |
| state-bound declarations (I-9) **and the runtime accounting that checks them** | five tests in `circuit.rs::accounting`, plus `the_joins_state_is_accounted_against_its_declaration` | see below |

**196 tests across the workspace**, zero ignored, zero skipped, zero flaky (two consecutive full
runs, identical results).

### The three-term rule, and the term everybody forgets

`ΔOut = ΔA ⋈ B + A ⋈ ΔB + ΔA ⋈ ΔB`, written literally as three probes in `Join::step`, with the
derivation in the module docs. `A` and `B` are the integrals **as they were before this epoch**, and
the code integrates only after all three probes — probing updated indexes would count this epoch's
rows twice.

Each term is isolated by its own test, so a failure names the term rather than a seed:

| Term | Isolating test | How it is isolated |
| --- | --- | --- |
| `ΔA ⋈ B` | `the_left_delta_probes_the_right_integral` | right side arrives in an earlier epoch |
| `A ⋈ ΔB` | `the_right_delta_probes_the_left_integral` | mirror image |
| `ΔA ⋈ ΔB` | `the_delta_delta_term_is_the_whole_answer_when_both_sides_insert_together` | one epoch, both indexes empty, so terms 1 and 2 probe nothing |
| order of operations | `probing_happens_before_integrating_so_nothing_is_counted_twice` | both sides already populated, both gain a row; must emit 3 new pairs, not 6 |

**And the gate has teeth — checked, not assumed.** Two deliberate mutations, both reverted:

| Mutation | Caught by |
| --- | --- |
| **Drop `ΔA ⋈ ΔB`** (§6 C2's named pitfall) | 2 operator tests + 10 handwritten differential scenarios + the randomized gate at **seed 2, epoch 6** — 12 failures |
| **Integrate before probing** (the double-count bug) | 5 operator tests + the gate at **seeds 90003, 90009 and seed 2, epoch 6** |

Under both mutations the I-2 gate and the state accounting still passed. That is the same lesson C1
recorded: a deterministic bug is still deterministic, and state accounting measures size, not
correctness. Only I-1 catches a wrong answer.

The delta-delta case is also **common in the randomized population, not just present in a
handwritten test**: of 1,090 join scenarios, 946 change both sides in one epoch and **790 insert
matching keys on both sides in one epoch** (`the_gate_population_contains_the_shapes_c2_names`). If
the generator ever drifted so both sides stopped moving together, term 3 would go barely exercised
and that test would say so.

### I-9: the placeholder is gone

C1 left `check_state_declarations` accepting any state size for a non-`Stateless` declaration,
because nothing declared one. It no longer does. Every variant now has a real check:

| Declaration | What the runtime does | Test |
| --- | --- | --- |
| `Stateless` | requires actual state of exactly 0 | `a_stateless_declaration_that_holds_anything_is_caught` |
| `ProportionalToInputs` | budgets actual state against the entries ever handed to the operator | `state_growing_faster_than_its_input_is_caught`, `state_proportional_to_its_input_is_accepted` |
| `Unbounded` | **refused at wiring time** — admission needs the registry, which is C6 | `an_unbounded_declaration_is_not_admissible_yet` |
| any, mismatched | a declaration naming a different number of inputs than the operator takes is refused at wiring time | `a_declaration_that_does_not_match_the_arity_is_refused` |

The join declares `ProportionalToInputs { inputs: ["left", "right"] }` and reports the entries held
across both indexes. The budget is the entries ever delivered on those inputs, which is a sound
upper bound on O(|A| + |B|): an index over a side's integral holds one entry per *distinct* row, and
distinct rows can never outnumber the entries that delivered them.

**What that catches and what it does not**, stated in the code and repeated here: it catches the
wrong *complexity* — a join storing the cross product holds |A|·|B| against a budget of |A|+|B| and
fails as soon as either side passes two rows. It does not catch a constant-factor overshoot, because
retractions and multiplicities mean entries usually outnumber distinct rows. Tightening that needs
real per-operator input integrals, which is `EXPLAIN STATE` in C8.

### Also in C2

- **`schweep-state`** (§5.5, §6 C2's "MemBackend"): the `StateBackend` trait and `MemBackend`. The
  join reaches its indexes only through the trait, so C4 can hand it a `RocksBackend` without the
  operator changing (§2). Keys are `Vec<Value>` ordered by S-7 rather than bytes — **D-15**, with
  the reasoning and the cost. Named snapshots are deliberately absent until C4 designs the
  checkpoint protocol, and that gap is labelled rather than guessed at.
- **`WriteBatch` has `add` and no `delete`.** Every change to operator state in this engine is the
  addition of a weight; a row leaves when its weight reaches zero. An interface with `delete` would
  invite an operator to treat a retraction as a deletion, which is the special case I-5 forbids.
- **Sources are keyed by alias, not table**, so a self-join is representable: `FROM t a JOIN t b`
  needs two source nodes over one table. The oracle has supported that since C0 and the circuit
  refused it until now (`a_table_joined_to_itself_agrees_with_the_oracle`).
- **C1's gate kept its own meaning.** `CircuitEngine::claims` widened to include joins, so C1's gate
  now filters on `Family::FilterProject` directly. Had it kept using `claims`, C1's numbers would
  have silently become C1-and-C2's, and neither sprint's section here would describe its own gate.

### What C2 does **not** prove

- **Nothing about aggregation or distinct.** `GROUP BY` is refused by name, pointing at C3. Of
  4,400 seeds, 3,310 were skipped as outside rung 2 — printed by the gate, not hidden.
- **Nothing about outer joins or cross joins.** A join with no key pairs is refused
  (`a_join_with_no_key_pairs_is_refused`); `LEFT JOIN` is rung 5.
- **Nothing about state that outgrows memory.** `MemBackend` is a `BTreeMap` and
  `scan_prefix` is a filtered walk — O(n) per probe, which is the wrong complexity for a join and is
  knowingly left that way. C2 is the correctness sprint; the ordered-range fix is C10's, when there
  is a benchmark to justify it. **No performance claim is made** and the engine-constant section of
  the ledger is still empty.
- **Nothing about durability.** The join's indexes are in memory and have no checkpoint. C4.
- **Errors are still fenced off, and the fence is now scheduled to come down.** Both gates assert
  that no scenario raised an evaluation error. That is sound only while no generated expression can
  raise. See below.

### Q-2 is open, and now scheduled

C1 found that the oracle and the circuit disagree about an evaluation error's *lifetime*: the oracle
recomputes over the integral so a bad row raises forever, the circuit sees each row once so it
raises once. C2 did not touch it, and both gates continue to assert `error_answers == 0`.

**It is decided at the start of C3, doc-first**, ahead of its C5 deadline — the full plan is in
`docs/DECISIONS.md` under Q-2. Briefly: the aggregates make the question harder (`SUM` overflows,
`AVG` divides, and an error inside an aggregate is an error about a *group*), so the rule is settled
in `docs/SEMANTICS.md` before any aggregate code exists, then the oracle, then the engine. After
that, **error-raising expressions enter the gate population**: the `error_answers == 0` assertions
are replaced by assertions that both sides agree about which epochs raise and what they say, and the
ledger's generator entries are regenerated because the population will have moved.

### What C3 needs

- **`GROUP BY` semantics are already decided and pinned.** S-27 through S-32, and 12 tests in
  `crates/schweep-oracle/tests/semantics.rs` — drained groups vanish, MIN reveals the
  second-smallest under retraction, `COUNT` of an all-null group is 0 while `SUM` is NULL, AVG lands
  exactly on the weighted quotient. The engine is held to those, and does not get to reinterpret
  them.
- **The state vocabulary fits.** An aggregate declares
  `ProportionalToInputs { inputs: ["input"] }` — one entry per group is bounded by the entries that
  created the groups — and the runtime already budgets it. `StateBound::Unbounded` exists for
  aggregation over an unbounded key space and is currently **refused**, which is correct until C6's
  registry can admit it; if C3 needs it sooner, that is a decision to record, not a check to remove.
- **MIN/MAX need a per-group multiset**, not a single value (§5.3, S-30). `MemBackend`'s ordered
  prefix scan is exactly the shape for it: key the state as `[group key…, value]` and the smallest
  live value is the first entry under the prefix.
- **The families exist.** `Family::Aggregate` and `Family::JoinAggregate` are already generated —
  most of the 3,310 seeds C2 skipped. Widening `CircuitEngine::claims` turns them on.
- **Q-2 first, before any of it.**

Per the sprint protocol in `CLAUDE.md`, **C3 does not begin in the session that finished C2.**

---

## C3 — aggregates, distinct, and the error rule

**Objective (§6):** complete the stateful core. The engine now implements the whole surface
`docs/SEMANTICS.md` defines, and the differential gate sweeps **every** scenario the generator
produces.

C3 ran in three parts, in this order: settle **Q-2** doc-first; prove float rendering lossless
*before* the first float could flow; then build the aggregates.

### Part 1 — Q-2, closed by D-16

C1 found the oracle and the circuit disagreeing about an evaluation error's lifetime. C3 opened by
deciding it in `docs/SEMANTICS.md` before touching operator code (S-22, S-22a…S-22d), recording the
reasoning as **D-16**, then implementing oracle-first and engine-second.

**The rule.** The answer at epoch N is either a Z-set or an error, determined by the *contents* at
epoch N. Data that raises means the query has no answer while it is present; retract it and the
answer returns.

**Why the alternative lost.** An error as a property of the *change* is not merely different — it is
incompatible with **I-3**. Dropping the epoch that raised means the next epoch lands on contents
that never absorbed it, leaving the answer a mixture of epoch N−1 and N+1. The epoch now seals and
only the *answer* is an error.

**The mechanism is a Z-set**, which is why it is small: a row that raises contributes its message at
the row's weight, so retracting the row retracts the error by the same arithmetic (I-5 applied to
errors). The engine integrates the error stream into a result store exactly like the answer stream,
and "the least live message" (S-22c) is the first row of its canonical form.

| Claim | Test |
| --- | --- |
| An error lasts exactly while the offending data is present, and no longer | `s22_an_error_lasts_while_the_offending_data_is_present_and_no_longer` |
| With several live errors the least message is reported | `s22c_the_least_live_error_message_is_reported` |
| For an aggregate the unit is the group | `s22a_a_group_whose_aggregate_overflows_makes_the_answer_an_error` |
| Batching the history differently changes neither answer nor error | `s22d_batching_does_not_change_the_answer_or_the_error` |
| The epoch **seals** and the answer is the error; retraction restores everything the erroring epochs carried | `an_evaluation_error_seals_its_epoch_and_lasts_while_the_row_does` |

**A C1 test was wrong and was replaced, not deleted.**
`an_evaluation_error_aborts_the_step_without_advancing_the_epoch` asserted the I-3-violating
behaviour. Its replacement asserts the opposite and says why.

**The gate population moved, deliberately.** Raising expressions now enter the generator — division
by a column (2/3 of divisions) and `i64::MAX` literals (1/12), so two *kinds* of error can be live
at once, which is what exercises S-22c. The `error_answers == 0` fences are replaced by
`error_answers > 0`: the sweep passing already means both sides agreed at every comparison, error
text included, and the assertion now says the population is not vacuous. Every quoted number in
`testing/evidence/registry.json` was regenerated and two new generator constants recorded — with the
honest note that the raising rate is set by how often arithmetic appears at all, not by the knob
(moving the column-divisor rate from 1/3 to 5/6 shifted the count only from 14 to 15 scenarios), so
specific error behaviours are pinned by handwritten scenarios instead.

### Part 2 — float rendering, proven lossless before the first float

`AVG` is the only source of a `Float64` (S-3), and its exemption from the no-floats rule rests
entirely on both implementations doing one identical division and producing identical bits (D-10,
S-31). That is worth nothing if the *comparison* throws bits away — and the harness compares
**rendered strings**.

| Claim | Test |
| --- | --- |
| Distinct bit patterns never render identically | `distinct_bit_patterns_never_render_identically` |
| Rendering round-trips to the same bits | `rendering_round_trips_to_the_same_bits` |
| `-0.0` and `0.0` render apart, as S-7 orders them apart | `negative_zero_renders_differently_from_positive_zero` |
| 200,000 seeded arbitrary bit patterns, injective and round-tripping | `a_large_sweep_of_bit_patterns_renders_losslessly` |
| The property AVG's exemption rests on | `avgs_arithmetic_is_bit_stable_through_rendering` |

Every check goes through a real `ZSetBatch` canonical form, not through `format!` directly, so it
proves the path an answer actually takes. `NaN` is the one value where rendering is legitimately not
injective; it cannot arise (S-31) and is skipped for a stated reason.

### Part 3 — the exit gate

| Gate condition (§6 C3) | Proven by | Result |
| --- | --- | --- |
| Differential green over aggregate scenarios **heavy on retractions** | `engine_vs_oracle_over_randomized_aggregate_scenarios` | 2,192 aggregate scenarios · 10,154 epochs · **12,346 comparisons · 0 divergences**, 85 of them a shared live error |
| Retract the current MIN, second-smallest surfaces | `retracting_the_current_min_reveals_the_second_smallest` | and `a_multiplicity_must_be_drained_before_the_min_moves` |
| Drain a group to zero, the row **vanishes** (not zeroes) | `a_group_drained_to_zero_vanishes_leaving_no_phantom_row` | plus vanish-and-return, and churn-within-one-epoch |
| AVG over retractions lands exactly on the oracle's value | `avg_over_retractions_lands_exactly_on_the_oracles_value` | |

Of the 2,192 gate scenarios, **1,801 contain a retraction**, 1,739 use a weight above 1, 522 use
`DISTINCT`, and 943 of the 1,084 join-aggregate scenarios change both join sides in one epoch — so
C2's delta-delta coverage assertion extends to the new families rather than lapsing.

**224 tests across the workspace**, zero ignored, zero skipped, zero flaky (two consecutive full
runs, identical).

### The cliffs, each with its own isolating test

| Cliff | Test |
| --- | --- |
| MIN/MAX keep an ordered multiset (S-30, §5.3) | `retracting_the_current_min_reveals_the_second_smallest`, `min_and_max_work_on_strings_and_use_the_total_order` |
| A drained group vanishes (S-29) | `a_group_drained_to_zero_vanishes_leaving_no_phantom_row`, `a_group_can_vanish_and_return`, `a_group_created_and_drained_in_one_epoch_never_appears` |
| SUM transits `i128`, lands in `i64`, or raises | `a_sum_that_transits_out_of_range_and_returns_is_correct`, `a_sum_that_does_not_fit_raises_and_the_error_clears_when_the_data_leaves` |
| AVG is one division of two exact integers (S-31) | `avg_over_retractions_lands_exactly_on_the_oracles_value` |
| Grouping uses not-distinct while `ON` uses `=` — **in one query** | `grouping_groups_nulls_together_while_a_join_key_never_matches_a_null` |
| `COUNT(x)` is 0 where `SUM` is NULL (S-30's asymmetry) | `avg_of_an_all_null_group_is_null` |
| HAVING filters groups both ways as they change (S-32) | `having_filters_groups_and_a_null_predicate_rejects` |
| DISTINCT collapses weights and tracks presence incrementally (S-34) | `distinct_collapses_weights_and_tracks_presence_incrementally` |

**The state layout is chosen by MIN/MAX.** Per group and per aggregate slot, an *ordered multiset* of
the argument's values, keyed `[slot, group key…, value]` so a prefix scan returns them in value order
(D-15, S-7). MIN is the first entry, MAX the last, and retracting the current minimum reveals the
next because the next was never thrown away. The same multiset serves SUM, COUNT and AVG by folding
it — O(distinct values in the changed group), which is the honest cost of a layout chosen for
correctness under retraction, and a C10 concern. **No performance claim is made.**

**Aggregation is deliberately *not* shared with the oracle.** The scalar expression library is shared
(D-14) because §6 C5 says so; aggregation is implemented twice, because the cliffs above are exactly
what I-1 is for and sharing the code would have removed the signal.

### The gate's teeth, and a lesson about proving them

Two canonical mutations, both reverted:

| Mutation | Caught by |
| --- | --- |
| **MIN/MAX never forget a retracted value** (the single-value bug in effect) | 8 tests, randomized gate at **seed 4, epoch 3** |
| **A drained group emits a phantom `(key, 0)` row** (§6 C3's named pitfall) | 14 tests, including the whole-population sweep |

**A first attempt at the MIN/MAX mutation silently failed to apply** — `rustfmt` had collapsed the
target expression onto one line, so the patch matched nothing and the suite passed. A mutation that
does not land proves the opposite of what it appears to. Both mutations are now applied with a marker
that is grepped for before the run, and that check is the reason the first attempt was caught rather
than believed.

### I-9: no new placeholder

| Operator | Declares | Why that factor |
| --- | --- | --- |
| `Aggregate` | `1 + aggregates` × input | one entry per group for the total, plus one per (slot, distinct value) |
| `Distinct` | 1 × input | one entry per distinct input row |

`ProportionalToInputs` gained a **declared constant factor**, because a four-aggregate operator
legitimately keeps more entries than it received rows and the C2 check would have failed it. The
factor must be *justified* — a reader should be able to count the entries it claims — not raised
until the check passes; a wrong *complexity* still fails whatever the constant.

One real inconsistency was found and fixed while writing this: the state fingerprint computed its
budget **without** the factor while the checker applied it, so the printed accounting disagreed with
the enforced one. Both now come from one function (`Circuit::state_budget`).

### What C3 does **not** prove

- **Nothing about durability.** Aggregate and distinct state is in memory with no checkpoint. C4.
- **Nothing about SQL.** Circuits are still hand-built; the incrementalizer is C5. `DISTINCT` arrived
  in C3 because §6 C3's build list names it, ahead of its rung — recorded as **D-17**, and the rest
  of rung 4 (`UNION ALL`, `ORDER BY`/`LIMIT`) is not implemented.
- **Grand-total aggregation is still refused** (`EmptyGroupKeys`, S-33) and **Q-3 is still open**,
  now the only open question that C5 must settle.
- **No performance claim.** `MemBackend`'s prefix scan is a linear walk and the aggregate folds a
  changed group's whole multiset. The engine-constant section of the ledger is still empty and
  `no_engine_constant_steers_behaviour_without_a_receipt` fails if that changes without a receipt.
- **A bug in shared code is still invisible to I-1.** The scalar library and the binder are shared
  (D-14); `schweep-plan`'s own tests pin them to `docs/SEMANTICS.md` directly.

### What C4 needs

- **The seam is already in place.** Operator state lives behind `StateBackend` (§5.5, D-15), so
  `RocksBackend` slots in without touching an operator. C4's job on the trait is to add the **named
  snapshots** D-15 deliberately left out, once the checkpoint protocol is designed.
- **What must be checkpointed is enumerable.** Three operators hold state — join (two indexes),
  aggregate (one backend), distinct (one backend) — plus the circuit's result store, its live-error
  store, and `emitted_entries`, which is I-9 accounting and is state too. A recovery that restored
  the stores but not the counter would pass every answer test and then mis-account.
- **I-2 is already the shape I-7 needs.** `state_fingerprint` renders every operator's state and both
  stores deterministically, and the I-2 gates compare it across runs. Comparing a recovered circuit
  to its uncrashed twin is the same comparison with a crash in the middle.
- **fsync ordering must be written down before it is implemented** (§6 C4's pitfall): state flush →
  checkpoint record → log trim, in a doc comment, with the crash harness killing between each pair.
- **`EpochDeltas` should move to `schweep-log`.** It sits in `schweep-zset` because C4 had not
  happened yet (D-14); C4 is when the write path arrives and it can go where §5.4 puts it.

Per the sprint protocol in `CLAUDE.md`, **C4 does not begin in the session that finished C3.**

---

## C4 — durability

**Objective (§6):** survive death. `docs/DURABILITY.md` was written **first**, numbering every step of
the ack, seal, checkpoint and recovery sequences and naming the instant between each pair; the crash
harness lands on those instants.

**Read the honest summary first:** the exit gate is green and `RocksBackend` is **not delivered**. The
reasons are in **D-18** and repeated below. Everything else on §6 C4's list is done.

### The exit gate

| Gate condition (§6 C4) | Proven by | Result |
| --- | --- | --- |
| ≥10,000 randomized crash-and-recover cycles | `ten_thousand_crash_and_recover_cycles` | **10,000 cycles** · 5,767 seam faults fired · 1,832 byte-boundary faults · 604 clean runs · **18 of 18 named seams fired** |
| Every recovery byte-identical to the never-crashed twin (I-7) | same test | state fingerprints **and** answers **and** the log's rendering, all compared |
| Every acked batch appears exactly once (I-4) | `a_replayed_token_is_acknowledged_and_dropped`, plus the gate re-offering every token after recovery | a re-offered token that is not dropped fails the cycle |
| A torn checkpoint is detected and the previous one used | `a_torn_checkpoint_is_detected_and_the_previous_one_is_used` | 150 scenarios, byte-corrupted checkpoints |
| Recovery is idempotent | `recovery_is_idempotent_under_a_crash_during_recovery` | crash *during* recovery, twice, then recover: same state |
| `StateBackend` frozen at exit | **D-18** | frozen **provisionally**, with its compatibility promise — final when a second backend validates it, no later than C8 entry (**D-19**) |

**256 tests across the workspace**, zero ignored, zero skipped, zero flaky (two consecutive full
runs, identical). The crash gate runs in ~42 s.

### The gate's own assertions caught two harness bugs — before any engine bug

This is the C3 mutation lesson, applied to crashes, and it paid immediately.

1. **`0 of 10,000 cycles fired a seam fault.`** The first run of the gate reported that and failed on
   the fault-count assertion. The cause was in the harness: `run_with_fault` returned the *recovery*
   injector's `fired()`, which is always inert, so every cycle reported "no fault". Without that
   assertion the gate would have passed, green, having injected nothing.
2. **`seam RecoveryMidReplay was planned but never fired.`** The seam-coverage assertion then caught
   that recovery seams were unreachable, because the recovery phase used an inert injector. Fixing it
   needed a third phase — crash, crash-during-recovery, then recover for real.
3. Fixing (2) surfaced a third bug the idempotency test caught: a run over an already-recovered
   directory re-sealed every epoch and stepped the circuit twice, doubling every weight. Phase 1 now
   resumes from the log rather than replaying from zero.

Three real bugs, all in the test apparatus, all found by assertions about the apparatus rather than
about the engine.

### Teeth: the two canonical mutations

Both applied with a marker grepped before the run — the C3 discipline — and reverted.

| Mutation | Caught by |
| --- | --- |
| **(a) Acknowledge before the batch is durable** | all 3 crash tests fail; the recovered log differs from the twin's (I-4) |
| **(b) Skip the torn-checkpoint detection** | `a_torn_checkpoint_is_detected_and_the_previous_one_is_used` and the 10,000-cycle gate, at seed 2 |

**An honest note on mutation (a).** §6 C4 asks for "ack before the fsync completes". That exact bug is
**not observable to an in-process harness**: `write_all` has already put the bytes in the page cache,
which survives a simulated crash, so recovery finds the record either way. The observable form of the
same bug class was used instead — acknowledge before the record is written at all — and it is caught.
Detecting the literal fsync-ordering bug needs a filesystem-level fault injector or a VM that can be
cut off mid-write. That is named as remaining work, not implied by a green gate.

### What is simulated, and what is not

The 10,000 cycles use **in-process fault injection**: abort at a named seam, drop every in-memory
object, recover from disk. What that faithfully models is loss of everything not yet written, at a
named instant. What it does **not** model is kernel-level write reordering or power loss.

Consequently the gate runs with `SyncPolicy::Deferred` — `fsync` skipped — because `fsync` changes
nothing an in-process crash can observe while costing hours on macOS. `SyncPolicy::Full` is the
default, is what production uses, and is what the log's own durability tests use. **Nothing here tests
power loss**, and no count in this document should be read as if it did.

**There is no real-`kill -9` subprocess test.** It was planned as the check that the in-process model
is faithful, and it is not delivered.

**Where it lands: C9.** §6 C9's exit gate is precisely this test, under load and over the network:
"kill -9 under load at 1,000 random points — every ack honored on recovery, no duplicate epochs
delivered to subscribers". So the gap is not merely named, it is *scheduled*: C9 must kill a real
process, and when it does it becomes the check that C4's in-process model was faithful. If the two
ever disagree, C4's simulation is what is wrong, and C9 is where that shows up.

Until then, a **nightly job** runs the crash gate at `SyncPolicy::Full`
(`testing/crash/tests/nightly_full_sync.rs`, schedule-triggered). It observes nothing an in-process
crash could not observe with fsync deferred — it is not a power-loss test and is labelled as such in
its own module docs — and it exists so that every `sync_all` call in the log and the checkpoint
protocol is exercised in bulk rather than by a handful of unit tests. A path never run in bulk is a
path that quietly stops being reached.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| A replayed token is acknowledged and dropped (A3, I-4) | `a_replayed_token_is_acknowledged_and_dropped` |
| The same token with **different content** is refused loudly, never rewritten (A4, I-4) | `the_same_token_with_different_content_is_refused_loudly` |
| Dedup survives a reopen, because the index is rebuilt from the log (R6) | `dedup_survives_a_reopen` |
| A malformed batch is refused and writes nothing (A1) | `a_malformed_batch_is_refused_and_writes_nothing` |
| A torn tail is discarded; the prefix survives (R5) | `a_torn_tail_is_discarded_and_the_prefix_survives`, `a_truncated_frame_reads_as_a_torn_tail`, `a_flipped_byte_fails_the_crc` |
| `source_id` travels with every batch (§5.4, MutinyDB seam) | `the_source_id_survives_a_reopen` |
| Byte order equals value order (D-15) | `byte_order_equals_value_order`, `a_seeded_sweep_agrees_on_order`, `a_component_prefix_is_a_byte_prefix` |
| A snapshot restores to an identical backend, replacing not merging | `a_snapshot_restores_to_an_identical_backend` |
| Faults are deterministic by seed; every seam is selectable | `a_seed_chooses_the_same_fault_every_time`, `every_seam_is_selected_by_some_seed` |

### What C4 does **not** deliver

- **Any non-memory backend.** §6 C4 names `RocksBackend` and D-5 mandated it. **D-19 has since amended
  D-5 to `redb`**, a pure-Rust B-tree store, with the RocksDB build cost as the trigger — a debug
  `librocksdb-sys` build produces over 2.1 GiB of object files and exhausted the machine's disk. (The
  `libclang` half of C4's original diagnosis was **wrong** and is corrected in D-18: with
  `bindgen-runtime` enabled, bindgen ran fine.) `RedbBackend` is C8-entry work; the order-preserving
  byte codec it will need *was* built and tested here, so the riskiest part is done. The trait freeze is
  **provisional** until it exists.
- **Power-loss testing**, and the literal ack-before-fsync mutation. Needs a filesystem fault injector
  or a VM.
- **A real-kill subprocess test.**
- **Log segment rotation and trimming.** The C6 trim step is a no-op in v1: one segment, and recovery
  replays only the suffix after the checkpoint's epoch, so trimming would save disk and change no
  behaviour. The seam exists and is exercised so the ordering is right; the work is C7's compaction.
- **No performance claim.** Nothing is benchmarked; the engine-constant ledger is still empty.

### What C5 needs

- **The plan type is already shared** (`schweep-plan`, D-14), which is what I-6 will be checked
  against: SQL text and the typed API must produce the same `schweep_plan::Query`.
- **Q-3 is the only open question left**, and C5 must settle it: grand-total aggregation over an empty
  input (S-33). Doc first.
- **The gate infrastructure is ready.** `sweep_matching` takes a predicate, so a SQL door adds a second
  `EngineUnderTest` rather than a second harness.
- **`EpochDeltas` can now move to `schweep-log`**, where §5.4 puts it. It has lived in `schweep-zset`
  since C1 only because the log did not exist.

Per the sprint protocol in `CLAUDE.md`, **C5 does not begin in the session that finished C4.**

---

## C5 — the SQL frontend and the incrementalizer

**Objective (§6):** the same-door moment — SQL in, circuits out.

Everything on §6 C5's list is delivered: `schweep-sql` (parser gate, binder, incrementalizer, plan
type, instantiator), the SQL fuzzer, the I-6 plan and counter gates, and both canonical mutations. Two
things are worth reading before the tables: **Q-3 is closed** (D-20), and **the SQL door is narrower
than the typed API** in one specific way that is counted rather than glossed.

### Pre-work carried into this sprint

- **D-19** amends D-5 to **redb**, with the RocksDB blockers as the trigger and fjall recorded as
  considered and rejected. The implementation stays C8-entry work; **D-18's freeze is provisional**
  until a second backend validates the trait.
- **D-18 corrected, visibly.** `libclang` was never the blocker — `--no-default-features` had disabled
  `bindgen-runtime`. Disk was. The wrong reason is quoted in D-18 above the correction rather than
  deleted, for the same reason C1's seam claim was corrected in the open.
- **A nightly `SyncPolicy::Full` crash job** (`.github/workflows/ci.yml`, `nightly-full-sync`, cron
  `17 3 * * *`, 400 cycles at `Config::durable()`). It observes nothing an in-process crash cannot
  observe with `fsync` deferred, and its own module docs say so; it exists so the `fsync` path cannot
  rot. Deliberately absent from `ci`'s `needs`: a scheduled job must not gate a push.
- **C4's kill -9 gap now forward-points to C9's gate**, which runs exactly that test under load.

### Part 1 — Q-3, closed by D-20

`SELECT COUNT(*) FROM t` over an empty input returns **one row**, not zero. Doc first (S-33 rewritten),
then the oracle, then the engine.

The oracle side is three lines: seed the keyless group with no members, and exempt it from S-29's
"a drained group vanishes" guard. The engine side is the interesting half, because a grand total is an
answer that must exist **before any epoch is sealed**, and every other answer starts empty. So
`CircuitBuilder::build` now *primes* the circuit by running an empty epoch through the same `run()`
path a real step takes — no second code path — and emission is made idempotent by a `primed` marker in
state. That marker costs one state entry, which is declared through a new **`constant` term** on
`StateBound::ProportionalToInputs`; I-9's vocabulary had no way to say "O(1) state" before, and adding
one number to the accounting is cheaper than exempting an operator from the accounting.

| Claim | Test |
| --- | --- |
| A grand total returns one row over an empty input, on the **oracle** side | `s33_a_grand_total_returns_one_row_even_over_an_empty_input` |
| The same, on the **engine** side, at epoch 0, through SQL text | `s33_the_grand_total_answers_before_any_epoch_is_sealed` |
| `HAVING COUNT(*) > 0` filters the grand total away, with no special case | `s33_having_can_filter_a_grand_total_away` |
| A GROUP BY that computes nothing is still refused | `s33_a_group_by_with_neither_keys_nor_aggregates_is_refused` |

### Part 2 — binder semantics, doc first

Three rules that C0 deferred to "the binder in C5" now say what they mean, and two new rules were
needed to say it: **S-35** (the SQL door translates and can only shrink) and **S-36** (a projection is
emitted only when the select list is not already the answer).

| Rule | Decision | Why |
| --- | --- | --- |
| **S-11** name derivation | `AS n`, or a bare column reference's own name. Nothing else. | Every derived name is a name nobody chose, and the schema is part of the answer (S-8) |
| **S-11** `SELECT *` | refused (`SelectStarNotSupported`) | The one refusal that exists *because* queries are standing: adding a column to a table must not change a running query's schema |
| **S-11** identifiers | verbatim, case-sensitive, quoted or not; keywords and function names fold | Dialects disagree about which way to fold, and folding in one door only would make the doors disagree about what a column is called |
| **S-19** untyped `NULL` | refused; write `CAST(NULL AS <type>)`, the only accepted cast | Inference would be a second analysis of the query, living in one door, that must agree with S-19's table |
| **S-32** `AggregateInHaving` | a real refusal now, with `AggregateInWhere`, `NestedAggregate` and `AggregateNotTopLevel` beside it | SQL text can write what the typed API cannot represent; each place gets its own name |

### Part 3 — `schweep-sql`, and where the documentation went

`crates/schweep-sql/src/incremental.rs` is the best-documented file in the repository, as §5.6 requires:
the three DBSP rules (linear, bilinear, stateful) each stated with the algebra, the reason it holds for
the operators it covers, and the trap it sets. Every plan node carries its rule as **data**
(`CircuitNode::rule`), so "this operator is linear" is a claim a test checks rather than a comment.

The pipeline is split at a seam that pays for itself three times: **incrementalize** (pure, hashable,
no state) then **instantiate** (allocates operators and one backend each). I-6 compares plans rather
than circuits; C6's memo will hash subtrees; and a failed comparison prints two s-expression trees
instead of two 64-bit numbers.

Two honest notes about that file:

- It performs **no general `δ`/`∫` rewrite**. Each logical operator has exactly one incremental
  implementation, already in `schweep-ops`, so the incrementalizer chooses it and records why. The
  file says this out loud, and says when a general rewriter would earn its keep (an open operator set,
  several forms per operator, nested time domains — none of which v1 has).
- It performs **no optimisation**. No pushdown, no reordering, no CSE. An optimiser today would change
  what I-6 compares and what the harness covers, for a benefit nobody has measured (I-10).

The old ad-hoc wiring in `testing/differential/src/circuit_engine.rs` is **deleted**: the typed door now
calls `incrementalize_typed`, the SQL door calls `compile`, and both end in `instantiate`. There is one
path from a query to a circuit, which is the only way I-6 can mean anything.

### The exit gate

| Gate condition (§6 C5) | Proven by | Result |
| --- | --- | --- |
| SQL fuzzer: hundreds of shapes, thousands of runs, green engine-vs-oracle | `the_sql_door_agrees_with_the_oracle_over_the_whole_renderable_population` | **2,028 scenarios**, 9,516 epochs, **11,544 answer comparisons**; all four families; every operation kind including retractions; 249 empty-input scenarios, 1,270 with an empty epoch, 122 error answers |
| I-6: both doors produce structurally identical plans (hash equality) | `i6_the_two_doors_compile_to_structurally_identical_plans` | **2,028 plan pairs**, compared as s-expressions *and* by FNV-1a hash *and* by answer schema |
| I-6: identical counters | `i6_the_two_doors_execute_identical_counters` | **470 scenarios** stepped through both doors, per-node counters compared after **every** epoch, 10,022 entries emitted |
| Every refusal names its construct | `every_construct_outside_the_dialect_is_refused_by_name` | **60 constructs**, each refused by a message containing its name; `the_dialect_itself_is_accepted` proves the refusals are not "everything" |
| Scalar expression library shared, tested differentially anyway | `schweep-plan` (D-14) + the fuzzer | unchanged from C3; the SQL door reaches the same `eval` |

**315 tests across the workspace**, zero ignored except the scheduled nightly, zero skipped, zero
flaky. The C5 gate runs in under a second.

### The population, and the part of it that has no SQL form

The fuzzer drives the SQL door by rendering the *existing* typed population back to SQL. That choice is
what makes I-6 checkable over thousands of shapes — there is a typed query to compare each SQL plan
against — and it puts the renderer under the I-6 assertion, so a renderer that writes SQL meaning
something else fails the gate with both trees printed.

Not every typed query has a SQL form, and the census is printed rather than implied:

| Reason | Count (of 4,400 seeds) |
| --- | --- |
| **renderable** | **2,028** |
| no projection and no GROUP BY — would need `SELECT *` | 1,110 |
| a projection over a GROUP BY | 1,099 |
| two group keys with one expression | 163 |

`every_scenario_either_has_a_sql_form_or_a_named_reason` asserts that the four numbers account for
every seed, and that the two large reasons still occur — so a change that made one unreachable is
noticed rather than celebrated as improved coverage.

**The middle row is a real difference in reach.** In SQL a group key's output name comes from the select
list (S-11), so a query that both groups *and* projects would need to name its keys twice and has one
select list to do it in. The typed API can express it; the SQL door cannot. That is recorded here, in
`NoSqlForm::ProjectionOverGroupBy`'s own documentation, and in S-33's note about `ColumnNotGrouped`.

### The gate's teeth

Both mutations applied with a marker **grepped before the run**, and reverted; `grep -rn MUTANT` over
`crates/` and `testing/` returns nothing.

| Mutation | Caught by |
| --- | --- |
| **(a) binder invents a name** — `SELECT t.n + 1` accepted, named by its own SQL text | `s11_names_come_from_as_or_from_a_bare_column_reference` **and** `every_construct_outside_the_dialect_is_refused_by_name` (two independent tests) |
| **(b) mis-incrementalized pipeline** — `DISTINCT` applied *before* the projection instead of last (S-34) | C1's gate at seed 0 epoch 4, **and** the C5 SQL-door gate at seed 0 epoch 4 |

Mutation (b) is the interesting one: it still type-checks, still emits the right output schema, and so
passes both wiring checks — the plan and the circuit both agree with the binder about the answer's
schema. Only the *answers* change. Nothing but a differential comparison against a recompute-from-
scratch oracle would have caught it, which is the argument for I-1 in one line.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| Names come from `AS`, or from a bare column reference, or not at all (S-11) | `s11_names_come_from_as_or_from_a_bare_column_reference` |
| `SELECT *` is refused, with the standing-query reason in the message (S-11) | `s11_select_star_is_refused_because_a_standing_query_fixes_its_schema` |
| Identifiers are verbatim: `A` and `a` are two columns; quoting changes only legality (S-11) | `s11_identifiers_are_verbatim` |
| A null is written `CAST(NULL AS T)`, and that is the only cast (S-19) | `s19_a_null_is_written_with_its_type`, `a_cast_that_converts_is_refused` |
| A negative literal is a literal, including `i64::MIN` (S-19) | `negative_integer_literals_fold_into_the_literal` |
| A grouped query emits no projection when the select list is already the group output (S-36) | `s27_a_group_by_binds_to_keys_then_aggregates_with_no_projection` |
| Reordering or narrowing the select list emits one (S-36) | `s36_reordering_the_select_list_emits_a_projection`, `s27_a_key_absent_from_the_select_list_still_gets_a_name` |
| Two aliases for one key both read the key (S-36) | `s36_two_aliases_for_one_key_read_the_same_column` |
| An aggregate with no `GROUP BY` is the grand total (S-33) | `s33_an_aggregate_with_no_group_by_is_the_grand_total` |
| A column outside the grouping belongs to no group, and the workaround binds (S-33) | `s33_a_column_outside_the_grouping_is_refused` |
| Each misplaced aggregate has its own refusal (S-32) | `s32_each_misplaced_aggregate_has_its_own_refusal` |
| The plan has one node per stage, in pipeline order, with the right DBSP rule on each (§5.6) | `the_plan_has_one_node_per_stage_in_pipeline_order` |
| Naming switches at the GROUP BY: `WHERE` sees `t.n`, `HAVING` sees `n` (S-10, S-27) | `naming_switches_at_the_group_by` |
| Every clause of the sqlparser AST outside the dialect is refused by name | `every_clause_outside_the_dialect_is_refused_by_name`, `every_construct_outside_the_dialect_is_refused_by_name` |
| A plan's structural form and hash distinguish plans that differ anywhere | `the_structural_form_distinguishes_plans_that_differ`, `a_column_and_a_string_literal_render_differently` |

### What C5 does **not** prove

- **The differential SQL sweep is not an independent check of the answers.** Because the two doors
  compile to *identical* plans — which is exactly what I-6 asserts — a green SQL sweep follows from a
  green typed sweep. Its value is that compile-and-build succeeds across the population and that the
  identity holds at runtime, not that the answers were checked twice. The independent content is I-6
  itself, plus the hand-written binder tests, where SQL text sits on one side and the plan the rule
  says it means sits on the other.
- **The fuzzer's SQL is written by the same author as the binder.** `sql_render` is a renderer, not an
  independent SQL generator; a shared misconception about SQL would render and bind consistently and
  the gate would stay green. What guards against that is `crates/schweep-sql/tests/dialect.rs` — 60
  hand-written constructs — and `binder.rs`'s hand-written plans, not the fuzzer.
- **No grand total, and no `HAVING`, is reached by the fuzzer.** The generator always makes at least
  one group key and sets `having` only through the typed path; both shapes are covered by hand-written
  tests instead, including the I-6 pairs.
- **No `Float64` flows through the SQL door except from `AVG`.** There is no way to write a float
  literal (S-3), which is the point, but it means the SQL door's float handling is exactly AVG's.
- **No performance claim, again.** The parser, binder and incrementalizer are not benchmarked, and the
  engine-constant ledger is still empty. C8 owns that.
- **No memo, no sharing.** Two identical standing queries build two circuits. `CircuitPlan::nodes` and
  the structural hash exist for C6 to use; C6 has not happened.

### What C6 needs

- **The structural hash is ready**, and it is stable by construction: FNV-1a over a rendering, not
  `std::hash::Hash`, whose output is explicitly not stable across releases. A memo that shares
  sub-circuits needs subtree hashes, and `CircuitNode::structural_hash` is one call per node.
- **Counters are public** (`Circuit::counters`), which is what I-8's counter gate will assert on, and
  what I-6 already does.
- **The incrementalizer is the place sharing will attach**, and it now has one caller for both doors,
  so a memo lookup inserted there is inserted once.
- **`EpochDeltas` still has not moved to `schweep-log`**, where §5.4 puts it. Named in C4's list and
  still true.

Per the sprint protocol in `CLAUDE.md`, **C6 does not begin in the session that finished C5.**

---

## C6 — the memo: standing queries and shared circuitry

**Objective (§6):** many queries, one dataflow.

Everything on §6 C6's list is delivered: canonicalization and structural hashing, the standing-query
registry (register / read / deregister), attach-to-live-subtree with only the novel suffix
instantiated, reference-counted teardown, the I-9 admission C2 deferred, and both halves of the I-8
gate. Read two things before the tables: **sharing fails silently in two opposite directions**, and
**one scheduler was generalized rather than a second one written**.

### Recorded first: the SQL door's semantic gate

C5's flag is now **rule 11 in `CLAUDE.md`** and a section at the top of `schweep-sql`'s crate docs:
`crates/schweep-sql/tests/binder.rs` is the semantic gate for the SQL door, and the differential
harness cannot do that job. I-6 makes both doors compile to identical plans, so a binder that turns
text into a **valid but wrong** plan produces the same plan through both doors, the same answer as the
oracle for the query it actually compiled, and a green sweep. Every dialect change adds rows to
`binder.rs` and to `dialect.rs`.

### One step scheduler, one or many sinks

The memo needed many queries over one dataflow. The obvious way to get that is a second step loop
inside `schweep-memo`; it does not have one. A second scheduler would be a second place for epoch
discipline, state accounting and error attribution to be wrong, and I-8 would then be comparing two
runtimes instead of one runtime with sharing on and off. So `Circuit` grew three capabilities and kept
its single-sink door byte-for-byte intact:

| Capability | Why the memo needs it |
| --- | --- |
| **many sinks**, each with its own answer store, error store, and *ancestor set* | the ancestor set is what keeps one query's evaluation error out of another's answer (S-22, I-8) |
| **a mutable topology** — `attach`, `remove`, holes instead of renumbering | node ids are handles the memo holds in its hash and refcount maps; renumbering on removal would invalidate them |
| **a partial pass** — `catch_up(deltas, subset)` | a query registered mid-history must be brought up to date *without* re-stepping the nodes it is about to share |

Every C1–C5 test runs unchanged through `CircuitBuilder::build`, which now makes a one-sink circuit.
`CircuitBuilder::add` and `Circuit::attach` share one `check_wiring` function, so the wiring rules
cannot drift between the two doors.

### Part 1 — canonicalization, conservative on purpose

§5.7 says "share only exact sub-tree matches; no cross-predicate cleverness in v1", and the reason the
rules are so few is that the two failure modes are not symmetric:

- too **weak** costs sharing — answers stay right, the engine is slower and fatter;
- too **strong** is cross-contamination — one query reads another's answer.

So v1 normalizes exactly **one** thing, and `crates/schweep-memo/src/canonical.rs` carries the full
inventory of what it does *not* normalize with the sharing each omission costs:

| Rule | Test asserting the hash **hit** |
| --- | --- |
| join key pairs sorted (a conjunction has no order, S-26) | `reordered_join_keys_are_one_hash` |
| the same query twice is one hash | `the_same_query_hashes_the_same` |
| a common prefix hashes equal while the roots differ — the subtree property partial sharing stands on | `a_common_prefix_hashes_equal_while_the_roots_differ` |

And the recorded costs, asserted as *misses* so they stay decisions rather than folklore:
`a_swapped_comparison_does_not_share_and_that_is_the_recorded_cost` (`a = b` vs `b = a` hash apart),
plus `different_queries_hash_apart` over eleven queries that must never collide.

### Part 2 — the registry, and why registration is deliberately wasteful

Registration instantiates **every** node of the plan fresh — duplicates included — catches those nodes
up to the current epoch, and only then splices the query onto the nodes that already existed, freeing
the copies. The order looks backwards and it is forced:

> A novel suffix needs its input's **accumulated contents** to build its own state. Its input is a
> shared node, and a shared node emits *deltas*; it keeps no integral of its output. Asking the shared
> prefix to replay would corrupt it — a join fed its own history twice would double its index.

What makes the splice sound is that the private copy and the shared original are the *same function of
the same accumulated input*: identical operators, identical input, therefore identical state. That is
I-2 restated, and it is the whole argument for mid-history attach.

**The cost, stated plainly: registering a standing query costs one recomputation over the accumulated
input; maintaining it costs O(change).** The memo keeps the accumulated input per table to pay it —
the data, once, not per node. C7's Parquet ground truth and log compaction are where that stops being
a `BTreeMap` in RAM.

### Part 3 — the I-9 admission C2 deferred

C2's state checker refused `StateBound::Unbounded` outright, noting it would become admissible "when
C6's registry can admit it". It now is: `Admission::bounded()` is the default and refuses by name;
`Admission::with_unbounded_state(reason)` admits, the reason is stored in the registry where someone
can find it, and the admitted node is exempt from the *budget* and from nothing else — its size is
still reported, with `budget=admitted-unbounded` in the fingerprint rather than a number nobody
enforced.

**Honest note: no v1 operator declares `Unbounded`.** The join, the aggregate and the distinct all keep
state proportional to their input and say so. So the mechanism is tested with a probe operator at the
circuit boundary (`unbounded_state_is_refused_by_default_and_admissible_on_request`) and by the
registry's recording of the admission — including the seam, since
`unbounded_state_is_admitted_per_registration_and_recorded` asserts that the flag reached the runtime
for every node the registration built. Nothing here is load-bearing today; it exists because I-9
requires it and because the next operator that needs it should find the door already built.

### The exit gate

| Gate condition (§6 C6) | Proven by | Result |
| --- | --- | --- |
| Overlapping battery, sharing on and off, **byte-identical answers** | `i8_sharing_changes_the_counters_and_not_one_answer_byte` | 12 overlapping queries × 5 epochs = **60 readings**, compared byte for byte |
| **Counter proof** that sharing actually shared | same test | **64 operator steps shared vs 104 private** (38% fewer), **18 live nodes vs 41**; asserted as a floor of ≥ 25% saved, not merely `<` |
| Every answer is the oracle's answer | `every_query_in_the_battery_agrees_with_the_oracle_at_every_epoch` | 60 answers vs a from-scratch recomputation, under sharing |
| The memo's plumbing under I-1 | `the_memo_answers_the_whole_generated_population_as_the_oracle_does` | **4,400 scenarios, 24,747 comparisons, 204 error answers** |
| Mid-history attach is correct | `a_query_attaching_mid_history_answers_as_though_it_had_always_been_there` | three latecomers at epoch 4 — a duplicate, a suffix, a new aggregate — each equal to the oracle for the *whole* history, then equal again after a further epoch |
| Mid-history attach onto an **erroring** core (D-16) | `a_query_attaching_to_an_erroring_core_inherits_the_error_and_recovers_with_it` | the latecomer inherits the live division-by-zero, a bystander query that shares nothing is unaffected, and both recover when the offending row is retracted (S-22b) |
| Teardown frees **exactly** the private suffix | `teardown_frees_exactly_the_private_suffix` | every holding back to baseline; the resident queries' answers unchanged to the byte |
| 1,000 register/deregister cycles leak nothing | `a_thousand_cycles_over_a_live_dataflow_leak_nothing`, `a_thousand_register_deregister_cycles_leak_nothing` | 1,000 cycles rotating through the battery over a *live* dataflow, accounting asserted every 100 rounds and audited against the dataflow's own wiring |
| A failed registration holds nothing | `a_refused_registration_holds_nothing` | four refusals, each leaving accounting at baseline |

**337 tests across the workspace**, zero ignored except the scheduled nightly, zero skipped, zero
flaky. The C6 gate runs in under a second.

### The gate's teeth — and which half caught what

Both mutations applied with a marker **grepped before the run**, and reverted; `grep -rn MUTANT` over
`crates/` and `testing/` returns nothing.

| Mutation | Caught by | Blind to it |
| --- | --- | --- |
| **(a) a hash that ignores one plan field** — `subtree_hash` erases filter predicates, so `WHERE t.k > 1` and `WHERE t.k > 2` collide | the **answer-equality** half of I-8, immediately: `SELECT ... WHERE t.k > 2` returned `t.k > 1`'s rows. Also `every_query_..._agrees_with_the_oracle`, `different_queries_hash_apart`, and `a_swapped_comparison_...` | — |
| **(b) a refcount off by one** — every release leaves one reference behind, so nothing is ever freed | the **accounting** half: `audit()` reported `RefcountDisagrees { node: 0, held: 11, actual: 4 }`, failing the teardown gate, the leak gate, and every test that audits | **both other halves of I-8.** With the audit temporarily removed, the answer comparison *and* the counter comparison passed under the leak |

That last cell was **measured, not assumed**: the audit call was removed, the mutation applied, and
`i8_sharing_changes_the_counters_and_not_one_answer_byte` passed. It is exactly why the gate has three
instruments and not one. A leaked node still computes the right thing for the queries that remain, and
both sharing settings leak equally, so neither answers nor counters move.

Symmetrically, mutation (a) is invisible to the counter half — a colliding hash *increases* sharing, so
the step count goes **down**, which is the direction the counter half calls success.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| A query registers, answers, and deregisters | `a_query_registers_reads_and_deregisters` |
| An overlapping query adds only its novel suffix | `an_overlapping_query_attaches_to_the_existing_subtree` |
| Sharing off builds strictly more and answers identically | `sharing_off_builds_twice_and_answers_the_same` |
| Deregistering one of two sharing queries leaves the other untouched | `deregistering_frees_exactly_the_private_suffix` |
| A query registered mid-history holds the same answer as one that was always there | `a_query_registered_mid_history_catches_up` |
| A grand total registered into a memo has its row before any epoch (S-33) | the battery's `SELECT COUNT(*) AS c, MIN(t.n) AS lo FROM t`, read at epoch 0 |
| Unbounded state: refused by default, admitted explicitly, recorded, exempt from the budget only | `unbounded_state_is_refused_by_default_and_admissible_on_request`, `unbounded_state_is_admitted_per_registration_and_recorded` |
| `Unbounded` is still never admissible through the single-query door | `an_unbounded_declaration_is_never_admissible_through_the_builder` |
| A node is never freed while something reads it | `CircuitError::NodeStillConsumed`, raised by `Circuit::remove` |
| The memo's refcounts agree with the dataflow's wiring | `Memo::audit`, called by every gate test |

### What C6 does **not** prove

- **The memo is not durable.** `Circuit::snapshot` carries state, not topology, and a memo's shape *is*
  the set of queries registered right now. Recovering a registry means re-registering — correct, but it
  costs one recomputation per query, and nothing wires it to the log yet. C4's checkpointing still
  covers exactly what it covered: one circuit of a known shape. Named here rather than half-built.
  **Scheduled: C9, doc-first.** Registry durability is not an implementation detail to be added
  wherever it fits — a registration is a *client-facing surface*. What a handle means across a
  restart, whether a client's standing query survives one, and what a resume token addresses are the
  same question, and §6 C9 already owns the resume token. So the decision is recorded in
  `docs/DURABILITY.md` and `docs/DECISIONS.md` at C9 entry, before any code, and C7's compaction is
  built to not prejudge it.
- **Sharing is not measured for cost, only for count.** "64 steps instead of 104" is a count of
  operator invocations, not time or memory. There is still no benchmark artifact and the engine-constant
  ledger is still empty (I-10). **Scheduled: C10.** §6 C10's exit gate is "every number in the README
  traces to a committed benchmark artifact", and sharing is the number that matters most there —
  (d) *the swarm benchmark*, the cost of the marginal query among 10,000 near-duplicate standing
  queries, which §6 calls "Schweep's game, and the benchmark that defines the product". Until then the
  honest claim is a step count, and that is all this document claims.
- **Canonicalization shares less than it could**, deliberately, and the inventory in `canonical.rs`
  names each case: operand order, aliases, filter merging. Every one of those is a *missed* sharing
  opportunity with a recorded reason, not an unknown.
- **No concurrency.** One thread, one epoch clock. Registering while an epoch is being sealed is not a
  case that exists yet, because there is nothing to be concurrent with (C9 brings the server).
- **Read-at-epoch means "at the latest sealed epoch"**, and `Memo::read` returns which epoch that was so
  a reader can honour I-3 across two reads. There is no way to ask for an *older* epoch: the memo keeps
  one integral per query, not a history of them. MVCC is not in v1.
- **The battery is hand-written.** The generated population goes through the memo one query at a time
  (`MemoEngine`), which exercises registration and catch-up over 4,400 scenarios but shares nothing.
  A fuzzer that generated *overlapping* query sets would be a better I-8 gate than twelve queries
  chosen by hand; it does not exist, and the twelve were chosen to overlap at every rung.
  **Scheduled: C10**, where it earns its keep twice. The swarm benchmark needs exactly this generator
  — 10,000 near-duplicate standing queries is a *generated overlapping set*, not a hand-written one —
  so building it once serves both the benchmark and a strengthened I-8 gate. Two consumers, one
  generator, and the I-8 gate inherits the population the benchmark is measured on.

### What C7 needs

- **The accumulated input already exists**, per table, inside the memo — kept for mid-history catch-up.
  C7's Parquet ground truth is that same integral written down, and log compaction is what makes
  keeping it unnecessary.
- **One-shot queries are a registration and a deregistration.** `register` → `read` → `deregister`
  already does it; what C7 adds is the ephemeral-circuit path that skips the sink bookkeeping, and the
  measurement to say whether skipping it matters.
- **`EpochDeltas` still has not moved to `schweep-log`**, where §5.4 puts it. Named in C4's list, named
  in C5's, still true.

Per the sprint protocol in `CLAUDE.md`, **C7 does not begin in the session that finished C6.**

---

## C7 — one-shot queries, Parquet ground truth, compaction

**Objective (§6):** be a database, not only a subscription engine.

Everything on §6 C7's list is delivered: `schweep-batch` with one-shot execution through ephemeral
circuits, Parquet snapshots of the input integrals, log compaction, and bootstrap-from-snapshot. Two
things to read first: **compaction is the only operation in this repository that deletes committed
history**, and **the crash gate caught the design document being wrong before any of it shipped.**

### The compaction sequence went into DURABILITY.md first — and was then corrected by the gate

`docs/DURABILITY.md` §4 numbers P1–P9 and names all eight kill points between them, exactly as A/S/C/R
were numbered before C4's code. Then compaction was wired into the crash harness, and within minutes it
failed on the document's own rule.

The draft said the compaction anchor is the epoch of the **live** checkpoint, and argued that an earlier
anchor "would be pointless". It is not pointless: R1/R2 **fall back** to an older checkpoint when the
newest fails to verify, which is exactly what a torn checkpoint produces. Anchoring to the newest
deletes the records an older checkpoint needs, so the fallback lands on a checkpoint whose suffix is
gone — and the recovered state is missing an epoch entirely, silently, because every *remaining* epoch
replays fine. The gate reported a torn-checkpoint cycle recovering to epoch 3 where its twin was at
epoch 5.

**The corrected rule: the anchor is the *oldest* published checkpoint's epoch**, because every
checkpoint on disk is one recovery may still choose. The correction is in the document with the reason
and the story, not quietly swapped.

### The shape of it

```text
   P1 anchor ─ P2 write ─ P3 fsync ─ P4 manifest ─ P5 publish ─┐  the snapshot exists
   P6 write the retained suffix ────────────────────────────────┤  the old log is STILL authoritative
   P7 swap the pointer ─────────────────────────────────────────┤  ← the one commit point
   P8 delete the old segment ─ P9 delete old snapshots ─────────┘
```

**One pointer names both artefacts.** The snapshot and the retained segment are useless apart — the
snapshot without the suffix is stale, the suffix without the snapshot has lost its prefix. Publishing
them with two commits would leave an instant between them at which neither pairing was complete, so
`LOG` names both and one rename moves from one consistent pair to another. Seven of the eight kill
points leave the whole log authoritative; the eighth leaves a snapshot that is already complete and
already paired.

**The old segment is deleted at P8, not P6.** Between P6 and P7 both a whole log and a complete
snapshot+suffix exist, and either would answer identically. That overlap is the safety margin.

### The edge that makes compaction dangerous, and the model change it forced

R7 rebuilt the dedup index by scanning every `Append` in the log. Compaction throws part of that log
away, so a token acknowledged in the discarded prefix and re-offered afterwards would look new and the
batch would be applied twice — **I-4 broken by a space optimisation**, with no error and no crash. The
ledger of acknowledged tokens therefore rides the snapshot, and `Log::open` seeds from it before
scanning the retained segment.

Writing the test for that exposed a real modelling gap: `Batch` did not carry its `dedup_token`, so
compaction — which *rewrites* records rather than copying bytes — had no token to write and invented
one. The invented tokens then polluted the rebuilt index. The fix was the model, not the symptom: a
batch now carries the token it was acknowledged under, and a rewritten record carries the token the
original did. `Log::tokens()` names them, so I-4 is compared by identity rather than by count.

### The gates

| Gate condition (§6 C7) | Proven by | Result |
| --- | --- | --- |
| One-shot answers equal the oracle over the fuzz suite | `one_shot_answers_equal_the_oracle_over_the_population` | **4,400 scenarios, 24,747 comparisons, 204 error answers**, through ephemeral circuits |
| One-shot and standing agree | `a_one_shot_and_a_standing_query_agree` | 500 scenarios answered both ways, identically |
| Answers byte-identical before/after compaction — live standing queries | `answers_are_byte_identical_across_a_compaction` | 5 standing queries over a 5-epoch history, compacted at epoch 3: every answer unchanged, and equal to the oracle |
| …and for fresh registrations | same test | a memo registered *after* the compaction, hydrated from snapshot + suffix, answers for the whole history |
| …and for one-shots over a compacted log | same test | all 5 queries |
| The snapshot says what the log says | same test | a row inserted and retracted is **absent**; a multiplicity partly retracted is present **at its net weight** |
| **Four materializations**, Schweep edition | `four_materializations_of_one_history_agree` | registered at epoch 1 · registered mid-history pre-compaction · registered post-compaction · a one-shot at the end — 4 × 5 answers, all identical, all equal to the oracle |
| I-4 across a compaction | `a_token_acked_before_a_compaction_is_still_dropped_after_it` | every token re-offered live *and* after a reopen is dropped as a replay; a new token is still accepted |
| Crash injection extends to every new seam | `ten_thousand_crash_and_recover_cycles` | **26 of 26 named seams fired** (18 + 8 new), 4,780 seam faults, 1,832 byte faults, **1,423 cycles recovered by bootstrap** |
| Recovery mid-compaction lands on the old log | the eight compaction kill points, asserted by the same twin comparison | a crash at any seam before P7 recovers to a state and a log that mean what the uncrashed twin's do |
| A compaction that cannot be anchored is refused | `compaction_refuses_what_it_cannot_anchor` | no checkpoint, an already-compacted prefix, and an anchor past the sealed epoch |

**352 tests across the workspace**, zero ignored except the scheduled nightly, zero skipped, zero
flaky. The crash gate runs in ~75 s; the nightly `SyncPolicy::Full` job compacts too, and was run once
by hand to prove it.

### Two honest notes about what the crash gate now compares

1. **The twin comparison compares what the log *means*, not which records it holds.** Before C7,
   `Log::render()` was a fair proxy: nothing removed records. Compaction removes them, and whether a
   given cycle got as far as its compaction depends on where the crash landed — so two twins routinely
   hold different records and mean the same thing. The comparison is now epoch + **named** tokens +
   the accumulated input per table, which is compaction-invariant *and* a stronger statement about I-4
   than the old rendering, which counted tokens without naming them.
2. **A recovery that bootstraps has different I-9 emission counters, and the gate says so.** When every
   checkpoint is torn *and* the log is compacted, there is nothing to restore and no prefix to replay —
   so recovery rebuilds from the snapshot (B1–B3). The operator state is the same state, because state is
   a function of the accumulated input (I-2). The *emission counts* differ, because one delta emits
   differently from many, and those counts are a history of how the state was reached rather than part of
   it. Those cycles compare a counter-stripped fingerprint, are counted separately (1,423 of 10,000), and
   are named here rather than folded in silently.

### A coverage loss, caught and repaired

Turning compaction on in the crash harness made `a_torn_checkpoint_is_detected_and_the_previous_one_is_used`
pass **150/150 through the bootstrap path** — because with compaction on, superseded checkpoints are
deleted and a torn one leaves nothing to fall back to. It was green, and it had stopped testing the
fallback its name claims. It now runs with `compact_every: 0` and asserts that the bootstrap path is
*not* taken, and a new test — `a_torn_checkpoint_over_a_compacted_log_recovers_by_bootstrapping` —
covers the compacted case and asserts that it *is*. A test whose name has stopped describing what it
does is worse than no test.

### The gate's teeth

Three mutations, each applied with a marker **grepped before the run** and reverted; `grep -rn MUTANT`
over `crates/` and `testing/` returns nothing.

| Mutation | Caught by | Blind to it |
| --- | --- | --- |
| **(a) consolidation drops a survivor** — the snapshot keeps only weight-1 rows, so a row at weight 2 (two rows present, S-4) is lost | the **before/after identity**: `answers_are_byte_identical_across_a_compaction` failed on "a multiplicity partly retracted keeps its net weight", and `four_materializations_of_one_history_agree` failed too. The crash gate's bootstrap comparison also caught it, on state | the I-4 test and the population sweeps — neither compacts |
| **(b) the dedup ledger omitted from the snapshot** — an empty `DEDUP`, everything else complete and verifiable | the **I-4-across-compaction test**: a reopened compacted log knew 4 tokens instead of 7. The 10,000-cycle gate caught it independently, at seed 1, as "re-offering token `epoch-0-t0` was not dropped as a replay" | **every answer test.** The data is right; only the exactly-once contract is gone |
| **(c) in-place swap** — truncate the live segment and write the suffix over it, skipping publish-then-swap | the **crash harness**, at seed 20, as an I-4 violation after recovery | **all six C7 gates**, and they should be: nothing crashes in them, and an in-place swap is correct whenever nothing crashes. That is exactly what makes it a trap |

The three mutations landed in three different instruments, which is the point of having three.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| A Z-set round-trips through Parquet, negative weights and nulls included | `a_table_round_trips_through_parquet` |
| A row whose net weight is zero is not written | `consolidation_drops_what_is_not_there` |
| A table cannot have a `__weight` column of its own | `a_reserved_column_name_is_refused` |
| A snapshot manifest detects its own damage | `a_manifest_round_trips_and_detects_damage` |
| The dedup ledger round-trips, is byte-stable, and refuses damage | `a_ledger_round_trips`, `encoding_is_byte_stable`, `a_damaged_ledger_is_refused` |
| An epoch the log no longer holds is reported as compacted, not as out of range | `LogError::EpochCompacted`, raised by `Log::epoch` |
| A `LOG` pointer that does not verify is treated as absent | `Pointer::decode` plus `read_pointer`'s fallback; the crash gate's byte faults exercise it |
| Bootstrap and mid-history attach are the same mechanism | `four_materializations_of_one_history_agree` (the post-compaction registration is C6's attach, sourced from a snapshot) |
| Recovery from a snapshot when every checkpoint is torn | `a_torn_checkpoint_over_a_compacted_log_recovers_by_bootstrapping` (150/150 bootstrapped) |
| An epoch counter never moves backwards | `CircuitError::EpochWouldGoBackwards`, guarding `Circuit::set_epoch` |

### What C7 does **not** prove

- **Compaction is not automatic.** Nothing decides *when* to compact: the crash harness compacts on an
  interval because that is how the seams get exercised, and `schweep-batch::compact` is a function
  somebody calls. A policy — by log size, by age, by pressure — is a tuning question with a measured
  answer, and C8 owns tuning with the ledger behind it.
- **A snapshot is per-table, not per-source.** `source_id` travels with every batch and is *not* carried
  into the snapshot's integrals, because the integral is a Z-set of rows and source-scoped retraction is
  C11's subject. When C11 arrives it will need the snapshot to carry provenance, and this is the sentence
  that says so.
- **No performance claim.** Parquet write throughput, snapshot size, compaction cost, one-shot latency —
  none of it is measured, and §6 C10 expects one-shot to lose to DuckDB and says to state that rather
  than chase it. The engine-constant ledger is still empty.
- **The nightly job was run once by hand, not on a schedule yet observed.** It compacts with real
  `fsync` and passed in 63 s; the scheduled runs will accumulate from tonight.
- **Still no real `kill -9`.** `docs/DURABILITY.md` §6 used to claim, in the present tense, that a
  subprocess kill test existed. **It never has.** That false claim is corrected in the document with the
  correction visible, and the test is **scheduled at C9**, whose exit gate is kill -9 under load at 1,000
  random points. No count in this document is a count of process kills.
- **`EpochDeltas` still has not moved to `schweep-log`**, where §5.4 puts it. Named in C4, C5 and C6.

### What C8 needs

- **The state backend decision is due at C8 entry** (D-19: redb), and D-18's trait freeze is provisional
  until a second backend validates it. C7 added no new `StateBackend` implementations and no new
  requirements on the trait, so that entry condition is unchanged.
- **Spill has a ground truth to spill to.** C8's state spill needs somewhere durable for cold state, and
  the snapshot format — Parquet, verified, published-then-swapped — is the shape that machinery should
  reuse rather than reinvent.
- **Compaction policy is a tuning constant**, so it belongs in `testing/evidence/registry.json` with a
  receipt when it arrives, not in a `const` chosen by taste.

Per the sprint protocol in `CLAUDE.md`, **C8 does not begin in the session that finished C7.**

---

## C8 — state spill and cold-start honesty

**Objective (§6):** state larger than RAM, and honest numbers about it.

Everything on §6 C8's list is delivered: `RedbBackend` (D-19), the C4 gates re-run on it, backend
invariance, `EXPLAIN STATE` with a reconciliation gate, the soak harness, and the ceiling gate in CI at a
fixed cgroup limit. **D-18's freeze is now FINAL.**

This sprint was mostly a sequence of measurements correcting guesses, so the honest summary is that
list: five constants I got wrong before measuring, two gate designs the measurements broke, and a
mutation the shape-based leak check failed to catch. All of it is below.

### D-18's provisional clause: discharged, not waived

`RedbBackend` implements `StateBackend` **unchanged** — not one method added, removed, or widened. The
freeze is recorded FINAL in D-18, with what the second implementation found:

| Finding | Detail |
| --- | --- |
| One method caused friction | `len` returns `usize`, not `Result`; redb cannot count a table without a transaction. The backend maintains the count inside the write transaction — eight lines, and arguably what the signature was always asking for |
| Two mapped **better** than to `MemBackend` | `write`'s atomicity is a redb transaction; `scan_prefix`'s ordering is a B-tree range, because C4's order-preserving codec makes a key prefix a *byte* prefix. The codec built for a backend that never arrived is what made this one straightforward |
| `snapshot() -> Vec<u8>` is the freeze's real cost | A checkpoint materialises every entry. So C8 spills state larger than RAM but **cannot checkpoint it**. Named, not worked around: working around it means unfreezing the trait |

**redb is a spill target, not a second durability mechanism.** State crosses a restart through the
checkpoint protocol, as it did on `MemBackend`; the spill directory is *cleared* when a circuit is built,
because a run inheriting stale redb files would be reading state no checkpoint accounted for.
`docs/DURABILITY.md` §5a says this, and says why the eight compaction seams did not grow: a backend's
transaction lands inside an existing seam pair, and recovery replaces its contents wholesale.

### The gates

| Gate condition (§6 C8) | Proven by | Result |
| --- | --- | --- |
| A scenario with operator state 10× RAM completes with flat memory | `operator_state_many_times_the_ceiling_completes_with_flat_memory`, run by the `state-ceiling` CI job under `systemd-run -p MemoryMax=128M` | **measured in CI, under the real cgroup:** ceiling read back as 134,217,728 bytes; **2.16 GB of operator state — 16× it**; **peak RSS 14.3 MiB, a 144:1 ratio**; RSS +0.7% while state grew +1,500%; 626 samples in 24 s |
| RSS sampled across the run, leak fails the job | same test | **626 samples in CI** (578 after warm-up); shape asserted after a stated warm-up, **and** an absolute budget, because the shape alone failed to catch a leak (below) |
| The ceiling is fixed in the job, not inherited | the job applies it; the test **reads it back** from the cgroup and `CURRENT_CEILING_REQUIRED=1` makes its absence a failure | a ceiling gate on a machine with free memory proves nothing, so it refuses to claim the gate without one |
| `EXPLAIN STATE` numbers reconcile with actual backend usage | `explain_state_reconciles_with_what_the_backend_actually_occupies`, six rounds as state grows; and again inside the ceiling gate | entries checked against an independent count, bytes against a measured floor and a presence condition. In CI at 2.16 GB: **844,050 entries reported / 844,050 held**, at 2,554 bytes per entry — an order of magnitude above the 67…205 measured for ordinary keys, which is exactly why no byte *ceiling* is claimed |
| The C4 gates on the backend that ships | `crash_and_recover_on_redb` | **600 cycles, 283 faults, 26 of 26 named seams fired**, twin comparison and I-4 re-offer unchanged |
| Backend invariance | `the_two_backends_agree_on_every_answer_and_every_logical_state` | **1,200 scenarios**, answers *and* logical state fingerprints identical; `the_spilled_engine_agrees_with_the_oracle` adds 800 against the oracle |
| Admission control at registration for undeclarable bounds (I-9) | delivered in C6 (`Admission`), unchanged here | — |

**The fingerprint was already logical and needed no fixing**, which is recorded rather than assumed:
`Operator::render_state` prints decoded keys and weights obtained through the trait, never the store's
bytes or its file name. The gate asserts it over 1,200 scenarios anyway.

### Five constants I got wrong before measuring

Every one is now in `testing/evidence/registry.json` with an artifact. The engine-constant section of the
ledger is no longer empty, and the evidence test that used to pin "nothing is tuned" now pins the rule
that made it worth pinning: **every entry cites a committed artifact, and the ledger's values must equal
the constants in the code.**

| Guess | Measurement | Where the guess would have gone wrong |
| --- | --- | --- |
| an empty redb file costs 12,288 bytes | **1,056,768** | a hundredfold error in every byte figure `EXPLAIN STATE` publishes |
| 96 bytes per entry | **67…205**, by key width | a single constant cannot describe both |
| file size grows with entries | at 1,500 entries the file is **smaller than when empty** — redb truncates on commit | a marginal per-entry figure underflowed `u64` |
| a per-entry ceiling bounds bytes | key width is unbounded; the ceiling gate hit **2,556 bytes/entry** with a 480-character column | the reconciliation gate failed, correctly |
| redb's cache is a detail | **it is the dominant term**: at 1/8/32 MiB the peak RSS was 38/67/106 MB | the ceiling gate would have failed on redb's 1 GiB default |

`c8-state-costs.json` is **deterministic** — a redb file's size is a function of what was written into it
— so `the_state_cost_artifact_still_describes_the_backend` recomputes and compares it, exactly as C0's
coverage artifact is. `c8-cache-sweep.json` is **machine-dependent**, because resident memory is an
allocator and kernel figure, and no test recomputes it. Both the artifacts and the ledger say which is
which.

### Two gate designs the measurements broke

1. **`EXPLAIN STATE`'s byte column started as an estimate with a tolerance, became a measured envelope,
   and ended as a floor plus a presence check.** The envelope died when the ceiling gate reached 269 MB
   of state against a 36 MB ceiling. What survives is what can be true: every entry costs *at least* so
   much, and no entries means no footprint beyond the empty files. A tighter model needs a trait that
   reports bytes, which is a decision above this sprint.
2. **The ceiling gate's shape needed three attempts**, and the two failures are real limits of the engine,
   recorded in the test where the next person will look:
   - `GROUP BY` with a group per row — state is large but the **answer** is one row per group, and a result
     store is an in-memory integral. 1.5 GiB of RSS against 85 MiB of state.
   - `GROUP BY` with few groups and many rows — the answer is tiny but `Aggregate` folds a changed group's
     whole ordered multiset to recompute it (S-30, §5.3's requirement so MIN/MAX survive retraction). A
     million-row group costs a million entries per epoch that touches it. **C10's name is on that.**
   What works is a join with near-unique keys behind a selective filter: state unbounded, answer under 100
   rows, one entry per probe.

   A third limitation surfaced with them: **a `Memo` cannot run under a ceiling its data exceeds**, because
   it keeps the accumulated input in memory for C7's mid-history catch-up. The ceiling gate drives a
   circuit directly for that reason. Sourcing catch-up from the log and the snapshot instead of RAM is
   C9's, where the server owns both.

### The gate's teeth — and the one that a gate half missed

Three mutations, each applied with a marker **grepped before the run** and reverted; `grep -rn MUTANT`
over `crates/` and `testing/` returns nothing.

| Mutation | Caught by | Blind to it |
| --- | --- | --- |
| **(a) spill silently drops an entry under pressure** — one entry in ~1,000 skipped once the store passes 2,000 entries, no error, count unchanged | the **ceiling gate's state-count check**: 210,814 entries held against 210,900 the generator inserted | **every answer test, including the ceiling gate's own answer check.** In this shape each key is probed in the epoch it arrives, so a row lost from one side's integral is never asked for again. The count is the only witness — which is why the check was added |
| **(b) `EXPLAIN STATE` under-reports** — every operator's entries divided by 64 | the **reconciliation gate**, once it had an independent count: `46 entries reported / 2977 held` | the **byte floor**, which passed it: under-reporting *lowers* the floor, and a lower floor is easier to clear. This is why claim 1 exists |
| **(c) an injected per-step leak** — every batch retained forever | the **absolute RSS budget**: peak 158 MB against a 96 MB budget | the **shape check**, which passed it at +1.0% growth. The machine was under memory pressure and the OS reclaimed the leaked pages half-way through: the curve climbed 6 → 214 MiB and fell back to 160, so the quartile means came out 1% apart. **A shape can be flattened by the kernel; an absolute budget cannot** |

Two of the three exposed a gate that was weaker than it looked, and in both cases the fix was a new
instrument rather than a looser threshold. That is the whole reason for applying them.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| A prefix scan returns the prefix in S-7 order, from the B-tree, without sorting | `a_prefix_scan_returns_the_prefix_in_order` |
| The maintained count follows the table through additions, removals and pass-through-zero | `len_tracks_the_table_through_additions_and_removals` |
| A batch nets within itself, as `MemBackend` does | `a_batch_nets_within_itself` |
| An overflowing batch leaves the store untouched | `an_overflowing_batch_leaves_the_store_untouched` |
| A snapshot taken on one backend restores into the other | `a_snapshot_crosses_between_backends` |
| State survives a reopen and the count is rebuilt | `state_survives_a_reopen` |
| A prefix at the top of the key space still scans to the end | `a_prefix_at_the_top_of_the_key_space_still_scans` |
| Each operator gets its own labelled file; a join gets two, distinguishably | `every_operator_gets_its_own_labelled_file` |
| A memo on redb answers as a memo in memory does, retractions included | `a_memo_on_redb_answers_as_a_memo_in_memory_does` |
| `EXPLAIN STATE` reports every operator, marks sharing, and never counts a shared operator twice as usage | `explain_state_reports_each_operator_of_each_query` |
| An in-memory backend reports honestly that there is nothing to reconcile | `explain_state_reconciles_with_what_the_backend_actually_occupies` |
| The ledger's values equal the constants in the code, and every entry cites a committed artifact | `every_engine_constant_cites_an_artifact_and_matches_the_code` |
| The deterministic cost artifact still describes the backend | `the_state_cost_artifact_still_describes_the_backend` |

### What C8 does **not** prove

- **A checkpoint of spilled state does not fit in memory.** `snapshot() -> Vec<u8>` materialises
  everything, so the ceiling gate takes no checkpoints and a deployment with state larger than RAM cannot
  checkpoint it through this trait. This is the frozen interface's cost, stated in D-18, and the first
  thing a future unfreeze should address.
- **A single operation is not bounded.** `scan_prefix` returns a `Vec`, so a join probe against a key with
  a million matches materialises a million entries whatever the backend, and an aggregate folds a changed
  group's whole multiset. The ceiling gate's shape has many keys with few rows each — the realistic case —
  and both limits are named in the test rather than discovered later.
- **A `Memo` under a ceiling is untested**, because it holds the accumulated input in RAM (C7). Scheduled
  at C9, with the server sourcing catch-up from the log and the snapshot.
- **No throughput number.** The cache size was chosen for *memory*, not for read performance, and
  §6 C10 owns the trade with a benchmark rather than an argument. Nothing here is a performance claim.
- **The ceiling gate runs one shape.** It is a join behind a filter, chosen because it is the only shape
  that satisfies all three constraints at once. A generated population under a ceiling would be better;
  the differential gates run the population *without* one.
- **Still no real `kill -9`**, and no compaction policy. Both named in C7, both still true.
- **The ceiling gate's numbers come from CI, and the local ones are a smoke test.** A developer machine
  has no cgroup, so locally the gate declines to claim anything and holds itself to a measured RSS budget
  instead. The two environments differ by more than noise — 14.3 MiB peak on the Linux runner against
  38 MB on macOS, at four times the state — so any figure quoted here says which one it came from.

### What C9 needs

- **Registry durability is C9's, doc-first** — the forward-pointer recorded at C7's entry. C8 changed
  nothing about it: `Circuit::snapshot` still carries state, not topology.
- **The memo's in-RAM input cache is the thing to remove**, and C9 is where it can be: a server owns the
  log and the snapshot, so catch-up can read from them instead. That also makes a memo runnable under a
  ceiling, which is the one part of C8's claim that a memo cannot make today.
- **`EXPLAIN STATE` is a client-facing surface waiting for a door.** It is a function returning a struct;
  C9's endpoints are where it becomes something a user can ask for.
- **`EpochDeltas` still has not moved to `schweep-log`.** Named in C4, C5, C6 and C7.

Per the sprint protocol in `CLAUDE.md`, **C9 does not begin in the session that finished C8.**

---

## C9 — `schweepd`: one process over the embedded engine

**Objective (§6):** put the engine behind a wire. Two decisions doc-first — what a registration means
across a crash, and what the wire contract *is* — then one process, per-source admission with real
backpressure, and the gates that make "it survives a crash" a measured claim rather than a design.

This is also the sprint that pays four standing debts: C6's registry-durability pointer, C4's *"no real
`kill -9` test"*, C8's *"a `Memo` under a ceiling is untested"*, and MutinyDB's MD-2 ask 3.

### The two decisions, recorded before any server code

| Decision | What it settles |
| --- | --- |
| **D-22** · a registration is server-owned and durable | `schweepd` persists the registry and rebuilds by bootstrap; a handle means the same query after a crash; an unbindable plan is **quarantined**, not dropped. Client-leasing rejected on two grounds, both stated |
| **D-23** · the wire contract | HTTP/1.1 by hand (Arrow Flight **deferred to C13**, with reasons); five error kinds of which exactly one is retryable; the resume token **is** the epoch number and the server holds no cursor; pull, not push. **MD-2 ask 3 shipped** as `POST /txn` |

Both grew addenda during implementation, and each addendum exists because a gate disproved a sentence
rather than because prose was tidied:

- **D-22 addendum** — recovery is **bootstrap only**. The first restart test answered `SUM = 20` where it
  should have answered `10`: the redb state store survived on disk *and* the bootstrap hydrated the same
  input on top of it. `Circuit::snapshot`'s own doc says a memo cannot be restored through it (its shape is
  the set of queries registered at the time), so the checkpoint is not the recovery path here — the state
  store is deleted on open, and the checkpoint's remaining job is C7's compaction anchor. Said out loud,
  because a checkpoint recovery ignores would otherwise read as a durability guarantee.
- **D-23 addendum, one** — **deltas are not durable; the answer is.** The retained deltas are an in-memory
  ring, so a server restart empties them and a *lagging* subscriber is refused (`TokenTooOld`) and must
  re-baseline. Exactly-once-per-epoch holds within a server's lifetime and across a *subscriber's* crash.
- **D-23 addendum, two** — **one bound is not a bound.** Measuring the queue bound for the ledger showed a
  count of batches bounds nothing: 64 batches of the widest measured batch is 32.9 MB for one source, and
  a client picks the width. Both queues now carry a byte bound beside the count, and a batch larger than
  the whole budget is `Refused` rather than `Overloaded` — because the one promise `Overloaded` makes is
  that backing off can work.

### The exit gate

| Gate condition (§6 C9) | Proven by | Result |
| --- | --- | --- |
| The differential harness runs **over the network**, green vs the oracle | `the_network_door_agrees_with_the_oracle_over_the_whole_renderable_population` | **2,028 scenarios, 9,516 epochs, 11,544 answer comparisons** over real sockets, every family, retractions included, 122 error answers crossing as errors |
| Same-door extends: network, SQL and typed doors produce identical **plans** | `the_three_doors_produce_one_plan` | 52 scenarios, structural hash **and** structural form identical through all three |
| … and identical **counters** (I-6) | `the_network_door_does_the_same_work_as_the_typed_door` | 52 scenarios, 248 epochs compared counter for counter |
| **THE REAL `kill -9`**: ≥1,000 random points under concurrent ingest + query + subscribe | `a_killed_schweepd_recovers_exactly_once_and_matches_its_never_crashed_twin` | **1,000 real `SIGKILL`s**, 24,219 acknowledged appends verified exactly-once, 6,459 epochs recovered, 5,459 epochs delivered to the subscriber with none twice; 968 cycles killed *between* an ack and a seal and 32 before any acknowledgement at all. **CI reproduced every one of those counts**, on Linux, in 28 s where macOS took 188 |
| Every acked batch in exactly one epoch after restart (I-4) | the same gate, per cycle | the workload makes it readable: token *i* appends row `(i, 1)`, so a doubled application reads `(i, 2)` |
| Recovery equals the never-crashed twin (I-7) | the same gate, per cycle | the **full** fingerprint, emission counters included — no relaxation |
| No subscriber receives an epoch twice after resume | `every_sealed_epoch_is_delivered_exactly_once_on_every_polling_schedule`, and the kill matrix's own subscriber | four polling schedules × 12 epochs, delivered epochs exactly `1..=12` on each; and in the matrix, delivery is strictly increasing before the kill and a resume at the recorded token either delivers only later epochs or is `Rejected` |
| Subscriber crash: kill the **subscriber** mid-stream, resume by token | `a_killed_subscriber_resumes_by_token_without_a_duplicate_or_a_gap` | **24 real `SIGKILL`s of a real subscriber process**, 40 epochs each, all 24 landing mid-stream; journal across the resume is every epoch exactly once |
| A `Memo` registers a late query under the ceiling, input exceeding it | `a_late_registration_catches_up_over_more_input_than_the_ceiling_inside_its_budget`, run by the `memo-ceiling` CI job under `systemd-run -p MemoryMax=128M` | **measured in CI, under the real cgroup:** ceiling read back as 134,217,728 bytes; **384 MB of accumulated input streamed and 1.08 GB of state built, in a process whose resident memory peaked at 14.7 MB** — 26:1 against the input, 73:1 against the state — with 139,952 bytes of growth across the catch-up, 0.03% of the input, in 26.7 s |
| … and answers what the oracle answers | `a_late_registration_answers_what_the_oracle_answers` | oracle, an epoch-0 registration and a late registration agree byte for byte over 30 epochs |
| Soak: a full window under load, RSS curve within shape **and** budget | `a_server_under_load_for_a_full_window_does_not_leak_per_epoch` | 3,000 epochs, **1,513–1,810 bytes/epoch against a 4,096 budget** over three runs, attributed — and the *shape* half is the slow-consumer gate's budget plus the coefficient here, because a server's curve is not flat and cannot be (see below) |
| `DURABILITY.md` records which limits this retires and which remain | `docs/DURABILITY.md` §"C9: the real kills exist" | a four-row table; power loss still **not covered**, and said so |

**What is reproducible about the kill matrix, and what is not.** Three 1,000-cycle runs of the finished code
— two on macOS, one on the Linux CI runner — reported the *same* figures: 24,219 acknowledged appends,
6,459 epochs recovered, 5,459 epochs delivered to the subscriber, 32 cycles killed before any
acknowledgement, 968 killed between an acknowledgement and a seal. That is the seeded half doing its job:
the workload and the kill *point* (a count of acknowledged appends) come from the seed, so the set of batches
the server promised is a function of the seed and not of the machine — across two operating systems and a
seven-fold difference in wall clock. What is **not** reproducible, deliberately, is the machine instruction
the signal lands on. That is the property under test, and the reason every assertion is written to hold for
any position.

Zero-flake held: every network test binds `127.0.0.1:0` and reads the port back, no test sleeps, and
readiness is always an event (a port file the child wrote after binding, or a response) rather than a wait.
Every RSS-measuring test is its own test binary, for the reason finding 4 below records.

**CI gained two jobs and a nightly.** `memo-ceiling` runs the C9 ceiling gate under a fixed 128 MiB cgroup,
the same discipline as C8's `state-ceiling`, and both are in the aggregate `ci` check's `needs`.
`nightly-soak` runs a 10,000-epoch window on a schedule and deliberately does not gate a push. The
`no-network` job needed one change: a fresh network namespace has a loopback interface that exists but is
*down*, so it brings `lo` up and proves it can — loopback is not the network that job is about, and the step
that establishes the outside world is unreachable is unaffected by it.

### The teeth: three mutations, marker-grepped and reverted

`grep -rn MUTANT crates testing` was run before each cycle and after each revert, and is empty now.

| Mutation | Caught by | What it also revealed |
| --- | --- | --- |
| **(a) resume-token off-by-one** — `delta.epoch >= from` instead of `> from` | the exactly-once gate (`epochs != 1..=12` on every schedule), the redelivery gate, **and** the real subscriber-crash gate, which failed on cycle 0 | nothing new: three instruments, all of them looking at the right thing |
| **(b) ack before the durable append returns** — the batch is held and appended on the *next* ingest | the **kill -9 matrix**, naming the token: *"t12 was acknowledged before the kill and is not in the recovered answer exactly once"* | it is also caught by ordinary answer tests, because deferring an append by one batch changes what the next epoch answers |
| **(b′) ack before the `fsync` returns** — the log at `SyncPolicy::Deferred` | **nothing.** 60 real `SIGKILL`s pass, green, "verifying" 1,620 acknowledged appends | `SIGKILL` does not touch the page cache, so this class is **invisible to this harness by construction**. Measured rather than reasoned about, and now the standing limit in `DURABILITY.md` |
| **(c) backpressure removed** — `check` always admits | the slow-consumer gate's **RSS budget** (45.9 MB against 32 MiB) *and* its refusal count | the budget was 160 MiB at first and the mutation slipped under it: only the refusal count caught it, and the budget — the instrument §6 asked for — watched it happen. It is now set between the two measured peaks |

Mutation (b′) is the most valuable result in this sprint. C4 said the `kill -9` test would close the
ack-before-durable-write question; it closes half of it, and the other half is now *demonstrated* to be out
of reach here rather than assumed to be covered.

### What is proven, and by which test

| Claim | Test |
| --- | --- |
| Every endpoint round-trips: register, ingest, seal, read, subscribe, deregister | `the_endpoints_round_trip` |
| Every error kind is reachable over the wire, and exactly one is retryable | `every_error_kind_is_reachable_and_only_one_is_retryable` |
| A source at its bound is refused, a seal frees it, and a noisy source does not starve a quiet one | `a_full_queue_refuses_and_a_seal_frees_it` |
| A source is bounded in **bytes**, and an unfittable batch is `Refused` rather than invited to retry | `a_source_is_bounded_in_bytes_and_an_oversized_batch_is_refused_not_retried` |
| A registration, its admission and its answer survive a restart; handles are never reissued (D-22) | `a_registration_and_its_answer_survive_a_restart` |
| A registration that no longer binds is quarantined, names what broke, and can be cleared | `a_registration_that_no_longer_binds_is_quarantined_and_not_dropped` |
| `/txn` seals N batches into one epoch, and a refusal partway seals nothing and rolls nothing back | `a_transaction_seals_its_batches_into_one_epoch_and_says_what_it_does_not_guarantee` |
| `/txn` and N×append+seal produce the same epoch and the same counters | `the_transaction_and_the_append_seal_path_produce_the_same_epoch` |
| Shutdown reports its drain, and the next start continues from it | `shutdown_reports_its_drain_and_recovery_continues_from_it` |
| A one-shot needs no registration and agrees with an incremental answer over the same data | `a_one_shot_query_needs_no_registration` |
| Re-subscribing from an old token redelivers the same bytes | `resubscribing_from_an_old_token_redelivers_the_same_bytes` |
| A token from the future comes back unchanged, never rewound | `a_token_from_the_future_is_not_silently_accepted` |
| A token behind the ring is refused and names the oldest epoch the server has | `a_token_behind_the_ring_is_refused_and_names_the_oldest_epoch_it_has` |
| A subscriber behind the ring exits on the refusal rather than believing itself caught up | `a_subscriber_that_falls_behind_the_ring_is_refused_rather_than_told_it_is_caught_up` |
| A recovered server agrees with its own log, and with the same directory opened in-process | `a_recovered_server_agrees_with_its_own_log` |
| A streamed segment yields exactly what the log holds, and stops where the log stops | `crates/schweep-log/tests/stream.rs` (4 tests, including every truncation point and a flipped byte) |
| A damaged registry is an error and never an empty one; an admission reason survives the file's own delimiters | `a_damaged_registry_is_an_error_and_never_an_empty_one`, `an_admission_reason_survives_spaces_percents_and_the_files_own_delimiters` |
| A slow consumer is refused rather than buffered, inside an RSS budget | `a_slow_consumer_is_refused_rather_than_buffered` |
| Two servers fed the same requests hold byte-identical state, counters and plans (I-2) | `two_servers_fed_the_same_requests_are_byte_identical` |
| C9's deterministic measurements still describe the wire, and the ledger's values match the code | `the_c9_bounds_artifact_still_describes_the_wire` |

### Seven things the gates found

1. **Recovery applied every input twice** (D-22 addendum). Found by the first restart test, before any
   crash gate existed. The redb store survived a restart and bootstrap hydrated on top of it.
2. **`pending_appends` reported 0 after a restart.** Found by the kill -9 matrix's *coverage* assertion —
   "no cycle was killed between an acknowledgement and a seal" — which was true only because the number it
   read was wrong: the count was tracked in memory and reset on open. It is now read from the log, which is
   the thing that survived. A server holding acknowledged, unsealed batches used to describe itself as
   holding none.
3. **`Log` holds the whole history resident.** Found by the memo-ceiling gate, which peaked at 342 MB
   streaming 269 MB of input *through a `Log`* while the catch-up's own footprint was a fraction of it. `Log` keeps every sealed
   batch in memory (`sealed: Vec<Vec<Batch>>`) plus a dedup token per append. The gate now streams the
   segment (`schweep_log::stream::Epochs`, new in C9) and peaks at 47.2 MB. **The limit itself is not
   fixed** — see below.
4. **An RSS gate cannot share a test binary.** The memo-ceiling gate passed alone and failed the
   full-workspace run at 123.9 MB against a 96 MiB budget: resident memory is a property of the *process*,
   and its correctness-test sibling in the same binary had already grown the allocator. Worse, the earlier
   "flat curve" reading was itself an artifact of that pollution — the baseline was pre-warmed, so a real
   step looked like a plateau. Every RSS-measuring test is now its own binary, and both files say why.
5. **Two of C9's own growth assertions were in the wrong unit.** `Curve::growth` returns a *fraction*, and
   the new gates compared it against `25.0` and `10.0` while printing it as a percentage — allowances of
   2,500% and 1,000%, displayed as "+2.2%" and "+1.4%". C8's gate had it right (`growth <= 0.10`, printed
   `* 100.0`), so this was new code getting an old thing wrong, and it surfaced only when a *different*
   assertion finally fired and the failure message was read closely. Both are fixed, and both sites now
   state that the value is a fraction. **The lesson is about the print, not the threshold:** a number
   displayed in the wrong unit made a broken assertion look reasonable every time it passed.

6. **A memory cgroup charges the page cache, so a test that writes its own fixture inside the ceiling must
   sync as it goes.** The memo-ceiling job passed one CI run and was killed with **exit code 137** — the
   cgroup's OOM killer — on the next, having got no further than printing its own header. Nothing about the
   engine was involved: the fixture writes 384 MB of segment, and dirty pages cannot be reclaimed until
   writeback has cleaned them, so dirtying faster than the kernel writes back reaches `memory.max` and the
   killer fires. It is timing-dependent, which is why it passed once. `write_segment` now flushes and
   `sync_data`s every epoch, bounding the dirty set at one epoch's frames. **A flake is a bug in the test or
   a bug in the engine** — this one was the former, and finding out which took reading an exit code rather
   than re-running.

   The same move fixed a duplication the earlier split had created: the fixture now lives once, in
   `testing/soak/tests/common/mod.rs`. It spent one commit in the crate's *library*, where clippy correctly
   refused it — a fixture that panics belongs in test code (`CLAUDE.md` rule 1).

7. **The soak's slope check compared unequal spans**, so it could not have reported what it claimed. It
   measured mean(Q2) − mean(Q1) against mean(Q4) − mean(Q2) — one quarter against two — which means
   perfectly linear growth reports a ratio of **2**, and a check written as "no more than twice" only ever
   fired on a *fourfold* acceleration. It surfaced from a **passing** run: the first nightly at 10,000
   epochs printed "4,140,080 then 8,185,777", which reads as an accelerating leak and is in fact linear
   growth. Quarter against quarter now, and at 3,000 epochs it reports 1,188,750 then 1,106,778 — the same
   twice, which is what linear growth should say. *A metric that cannot report 1 for the null hypothesis
   cannot report anything.*

   The nightly's own result is worth keeping next to it: **1,651 bytes an epoch at 10,000 epochs**, against
   1,513–1,810 at 3,000. The coefficient is stable across a window three times longer, which is what makes
   it a coefficient rather than an artifact of the window.

### What C9 does **not** prove

- **A catch-up costs a small residue per *pass*, and what the residue is has not been established.** Three
  probes in `testing/evidence/c9-memo-ceiling.json` separate the cause: 349 chunks of five rows still climb
  in every quarter (~1.2 KB a chunk), 349 chunks of 750 padded rows climb further (~12 KB a chunk), and 60
  chunks carrying the same total data **do not climb at all**. So it is per pass, not per byte of state, and
  it is a few percent of whatever a chunk carries — the signature of retained allocator arenas or of redb's
  per-commit bookkeeping, and *not* proven to be either. It bounds how long a history one catch-up can
  cross, so the gate asserts a coefficient (growth under 12.5% of the input streamed; measured 0–3.1%)
  rather than the flatness it cannot honestly claim. **Scheduled: C10.**
- **`schweepd`'s resident memory is O(retained log), and no soak can make it flat.** The finding above is a
  property of `Log`, not of the gate that found it: a server that has been up for a million epochs holds a
  million epochs' batches. The soak therefore asserts a per-epoch *coefficient* (1,513–1,810 bytes measured
  over three runs against a 4,096 budget, attributed in `testing/evidence/c9-soak.json`) rather than
  flatness. **Scheduled:
  C10** — the fix is a log that holds an index and reads batches on demand, and `stream::Epochs` is already
  the reader such a log would use.
- **Compaction is refused in the server, deliberately.** `Engine::open` returns `Unsupported` if the log has
  a compacted prefix, because recovery derives its epoch by replaying retained epochs and would report
  answers under epoch numbers short by the prefix. Wrong epoch numbers on right answers is what I-3 exists
  to prevent, so it stops instead of papering over it. **Scheduled: C10.**
- **An ack that precedes the `fsync` is invisible here**, demonstrated by mutation (b′). Power loss, a lying
  disk cache and torn media remain untested. `DURABILITY.md` carries the table.
- **The server is single-threaded on purpose**, and that is not a performance claim — it is what keeps
  everything past the ingest boundary deterministic (D-6). No throughput number is claimed anywhere; §6 C10
  owns that with a benchmark.
- **Arrow Flight does not exist**, and the endpoints are the contract rather than the framing (D-23).
  **Scheduled: C13.**
- **The retained deltas are not durable** (D-23 addendum). A lagging subscriber across a server restart is
  refused, not served.
- **`/explain-state` and `/counters` are diagnostic surfaces with no compatibility promise.** They exist
  because the gates need them; their formats will change.
- **The network differential gate is the renderable population, not the whole one.** 2,028 of 4,400 seeds
  have a SQL form — the same set C5's gate covers, and the skipped count is reported by the gate itself.
- **`EpochDeltas` still has not moved to `schweep-log`.** Named in C4, C5, C6, C7 and C8. Still true.

### What C10 needs

- **The log's resident footprint is the first thing to fix**, and it is now a measured number rather than a
  suspicion: `testing/evidence/c9-soak.json` attributes 1,589 bytes an epoch to it with nothing else
  running. `stream::Epochs` and its four tests are the reader half of the fix.
- **Compaction in the server**, which needs an epoch that survives a compacted prefix — the `Unsupported`
  refusal in `Engine::open` marks the exact spot.
- **Sharing-is-timed and the overlapping-query-set generator**, both promised to C10 at C6's exit, both
  still owed.
- **A benchmark harness**, since every performance question in this document has been deferred to it.

---

## C10 — performance (IMPLEMENTATION COMPLETE; CI-GATED)

All C10 implementation and evidence is present. The required repository checks remain the exit gate;
local success is never substituted for that gate.

### Done, and gated

**0 · The instruments, and the law that they come first.** C9's least comfortable result was that three of
its seven findings were flaws in its own instruments, every one of which passed while being wrong. So no
benchmark in this repository reports a number until `testing/bench/tests/calibration.rs` is green:

| The instrument claims | The gate checks it by | Result here |
| --- | --- | --- |
| the clock can be trusted | it is monotonic over 200,000 readings, and its resolution is **measured** | 41 ns on this machine |
| a workload is measurable | every workload must exceed the resolution 1,000-fold | smallest is 3.2 ms, ~78,000× |
| timing is **linear in work** | 1×, 2×, 4× interleaved; the ratios must land in ±20% bands | 1.950–2.052× and 3.82–4.20× over six runs |
| counting is **exact** | a workload of known count is counted exactly, not approximately | exact at 0, 1, 2, 1,000 and 999,983 |
| the harness is not in the number | an empty round against a real one | under a thousandth |
| a comparison measures the thing, not the order | the same workload paired against itself must report 1.0 | 0.988–1.003× |

Units live in the type — `Nanos`, `Bytes`, `Count`, `Ratio` — they do not mix, dividing by zero returns
`None` rather than an infinity that formats as a plausible number, subtraction saturates, and a `Ratio`
cannot print as a percentage by accident. That last one is C9's bug made impossible rather than fixed.

**The gate immediately found a defect in itself, which is the entire argument for having it.** Timing 1×,
2× and 4× as three consecutive samples gave doubling ratios of 1.73 to 1.98 across five back-to-back runs:
the machine drifts over the ~100 ms between the first size and the last, and the first size wore all of it.
Measured **interleaved** — a rotating N-way generalisation of the paired method — the same measurement is
centred where the truth is. The old numbers are in the doc comment, because the next person to write a
comparison here needs to know that consecutive samples do not compare.

**1 · Residency.** C9's largest named limit, closed. `Log` held every sealed batch resident; it now holds a
**byte range per epoch** and reads the records back when asked. Three paths were O(history) and all three
now stream: the open scan (which also read the whole segment with `read_to_end`), `Log::epoch` (which
returned a borrow of what was held, and now returns owned batches read from the span), and compaction
(which assembled its whole output in a `Vec<u8>`, and now writes incrementally while rebuilding the span
index exactly from the frame lengths).

| Claim | Test | Result |
| --- | --- | --- |
| ten times the history does not cost ten times the memory | `a_logs_resident_memory_does_not_track_its_history` | **226 MB of extra history cost 3.4 MB of RSS — 1.53%**, against a 5% gate and against ~100% for the design it replaces |
| the span index and a full scan agree | `the_span_index_and_a_full_scan_agree_epoch_for_epoch` | epoch for epoch over 241 epochs |
| performance work moved no result byte (I-1) | the 10,000-cycle crash gate, the compaction gate, the 1,000-`SIGKILL` matrix, the whole differential suite | all green against the paged log |

What still grows with history is measured and named rather than left to be discovered: **one dedup token
per acknowledged append** — I-4's price, since a token forgotten is a batch applied twice — and 16 bytes of
span index per epoch. `Log::dedup_len` and `Log::index_bytes` exist so a gate can measure them.

**5(a) · The calibration tooth.** A miscounted workload — the last operation performed but not counted —
which the counting check catches at the smallest case (asked 1, reported 0). Marker-grepped and reverted.

### Completed in the closing pass

**2 · Compacted-server recovery and bounded operations.** `SnapshotChunks` verifies each Parquet file
incrementally and yields record batches of at most 1,024 rows. `Engine::open` declares the compacted epoch,
hydrates the snapshot stream, then replays the retained suffix; a restart test compacts a live server and
proves epoch and answer identity. D-25 adds `visit_prefix`: both state backends stream their ordered range,
MIN stops after the first entry, MAX retains only the last, SUM/AVG/COUNT retain scalar accumulators, and
join probes do not allocate an intermediate match vector. Early-stop tests pin both backends.

**3 · Hot path.** `ZSetBatch::consolidate` is a stable total-order sort followed by one linear neighbour
merge, preserving checked-overflow order while removing a B-tree insertion per row. Arrow columns are
decoded once before the contiguous pass. No `unsafe` was required, so D-1's unsafe inventory remains
empty rather than being weakened to manufacture a vectorization claim.

**4 · Four calibrated benchmarks and the operator report.** `scripts/run_c10_benchmarks.py` builds the
release workers, creates TPC-H SF0.1 through DuckDB 1.5.5, alternates paired rounds, and writes
`testing/evidence/c10-benchmarks.json`. The artifact contains maintenance cost at 100/1,000/10,000 changed
rows, standing-answer reads, a DuckDB comparison over 600,572 real `lineitem` rows, and marginal
registration after 10,000 overlapping standing queries. It publishes every sample, median, fastest,
slowest, machine and caveat. D-26 adds `EXPLAIN MAINTENANCE` and `GET /explain-maintenance`; it reports
measured counters and links timing to the artifact rather than inventing a nanoseconds-per-step constant.

**5(b) · Sharing-regression tooth.** The benchmark and test consume one deterministic 10,000-query
generator. The gate proves all SQL strings are distinct, shared and private memos answer identically, and
the shared memo holds less than half the nodes of the private one. A canonicalization regression that
silently stops sharing therefore moves a correctness-independent assertion.

C11 is the next sprint and is not started in this session, preserving the one-sprint rule.
