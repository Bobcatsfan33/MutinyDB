# SEMANTICS — what a query means in Schweep

**Scope: dialect rungs 1–3** (ARCHITECTURE.md §5.6), **plus `DISTINCT`**:

1. SELECT / WHERE / projection with the scalar expression library
2. INNER equi-JOIN
3. GROUP BY + SUM/COUNT/MIN/MAX/AVG + HAVING
4. …of which `DISTINCT` only — §6 C3's build list names it, so it arrives with the aggregates
   rather than with the rest of rung 4 (S-34, D-17)

The rest of rung 4 (`UNION ALL`, `ORDER BY`/`LIMIT`) and rung 5 (`LEFT JOIN`, decorrelatable
subqueries) are **not** defined here and are not implemented. Anything not defined in this document is refused by
name, never silently accepted.

**This document is written before the code.** Per §10, semantics change here first, in
`schweep-oracle` second, in the engine third. When the differential harness reports a
disagreement, this document decides who is wrong; if this document is wrong, it is corrected here
before either implementation moves.

Every rule below is numbered `S-n` so that tests and commits can cite it.

---

## 1. Data model

### S-1 · Values

A value is one of:

| Value | Notes |
| --- | --- |
| `Null` | the absence of a value; see §3 |
| `Int(i64)` | 64-bit signed integer |
| `Str(String)` | UTF-8 string |
| `Bool(bool)` | |
| `Float(f64)` | **result-only** — see S-3 |

### S-2 · Types and schemas

A schema is an ordered list of `(name, type, nullable)`. Types are `Int64`, `Utf8`, `Boolean`,
`Float64`. Column names within a schema are unique. A value of type `T` in a column is either a
`T` value or `Null`; `nullable: false` is an assertion the oracle checks on ingest and reports as
a named error if violated — it is never used to skip null handling in an operator.

### S-3 · Float64 is a result-only type in v1

**No table column may be declared `Float64`, and no scalar expression produces a `Float64`.** The
only source of a `Float64` in the entire system is `AVG` (S-31).

*Why.* Floating-point addition is not associative. An incremental `SUM` maintains a running
total; the oracle recomputes the total from scratch in a different order. For `f64` those two
answers differ in the low bits, and I-1 demands byte-for-byte equality — so a `Float64` sum would
put the load-bearing invariant of the project in permanent, unwinnable conflict with the laws of
IEEE-754. Integers do not have this problem: `i64` addition is associative and exact.

`AVG` is safe because it is not accumulated: both the oracle and (later) the engine maintain an
exact integer `SUM` and an exact integer `COUNT`, and perform **exactly one** division at emit
time (S-31). A single division of two identically-derived integers is bit-identical everywhere.

Decimal/fixed-point arithmetic is the honest long-term answer for non-integer data. It is
deferred, deliberately, and is recorded as an open decision in `docs/DECISIONS.md`.

### S-4 · Rows, Z-sets, and weights

A row is a value per schema column. All data is a **Z-set**: a multiset of rows in which each row
carries an `i64` **weight**. Weight `+3` means three copies of the row; `-1` means one copy is
removed. There is no separate delete or update machinery: an update is `-1` for the old row and
`+1` for the new row, in the same Z-set (§1 of ARCHITECTURE.md).

A Z-set is **consolidated** when no two entries hold equal rows and no entry has weight zero.
Consolidation is the canonical form; equality of Z-sets is defined on it (S-8).

### S-5 · Table contents are non-negative; a negative integral is malformed history

The contents of a table at epoch N are the **integral** of its deltas: the epoch-by-epoch sum of
weights for each row, from epoch 1 through N.

**A table's integral must have non-negative weights at every sealed epoch.** A history that
retracts a row that is not present — or retracts more copies than are present — is *malformed*,
and the oracle rejects it with a named error (`NegativeIntegral`) naming the table, the row, and
the epoch.

*Why this is a rule and not a definition.* Defining an answer for "-2 copies of a row" would be
inventing semantics no user asked for, and it would let a generator bug or an ingest bug travel
silently through the whole engine and come out the far end as a plausible-looking number. The
scenario generator maintains a model of current contents and only ever retracts rows that are
present; if the generator ever violates that, the oracle says so loudly.

Intermediate and output Z-sets are **not** subject to this rule: a *delta* on a table's output
naturally carries negative weights, and that is the entire point (I-5).

### S-6 · Epochs

An epoch is the unit of time and atomicity. Input deltas are assembled into an epoch; sealing the
epoch makes them visible together. Epochs are dense integers starting at 1. An answer is always
"as of epoch N", never a mixture (I-3). An epoch may be **empty** (no deltas for any table); its
answer equals the previous epoch's answer.

The oracle computes the answer at epoch N by integrating all input deltas for epochs 1..=N and
recomputing the query from scratch over that integral. That is the whole implementation strategy,
and it is why the oracle is trustworthy.

### S-7 · Total order on values

Every ordering in Schweep is total (D-7). The order on values is:

1. **`Null` sorts before every non-null value.** (Chosen, not inherited: SQL leaves it
   implementation-defined. Nulls-first is a single fixed rule with no `NULLS FIRST/LAST` modifier
   in the dialect, so there is nothing to disagree about.)
2. Non-null values of the same type compare within the type:
   - `Int64`: numeric order.
   - `Boolean`: `false < true`.
   - `Utf8`: byte-wise lexicographic order of the UTF-8 encoding (equivalently, code-point order).
     **Not** locale- or collation-aware. There is no collation in the v1 dialect.
   - `Float64`: IEEE-754 total order (`f64::total_cmp`). `NaN` cannot arise (S-31), but the order
     is total regardless so that no comparison is ever undefined.
3. Values of different types never occur in the same column (S-2), so cross-type comparison does
   not arise in ordering.

### S-8 · Canonical form and answer equality

Two answers are equal iff their **canonical forms** are identical. The canonical form of a Z-set
is:

1. **consolidate** — merge entries with equal rows by summing weights, then drop every entry
   whose weight is zero;
2. **sort** — order the remaining entries by all columns in schema order, using S-7.

After consolidation rows are unique, so step 2 yields a total order with no ties and no tiebreak
is needed beyond "all columns in schema order" — which is exactly the D-7 rule.

Answer equality also requires **schema equality**: same column names, in the same order, with the
same types. Two Z-sets with equal rows and different schemas are not equal answers.

This canonical form is what the differential harness compares (I-1), and it is what "byte for
byte" means at rungs 1–3. `ORDER BY` is a rung-4, read-time concern and does not change it.

---

## 2. Queries

### S-9 · Query shape

A rung-1–3 query is:

```
FROM     Scan(table, alias)  |  Join(left, right, ON key-pairs)
WHERE    optional predicate
GROUP BY optional { keys, aggregates, optional HAVING }
SELECT   optional projection
```

evaluated strictly in that order: **from → where → group → having → select**. Each stage consumes
the Z-set the previous stage produced.

### S-10 · Column references and aliases

Every scan carries an alias. Inside a query, a column is referenced as `alias.column`. This is the
only form; unqualified references are refused (`UnqualifiedColumn`) rather than resolved by
search. After a GROUP BY, the columns available are exactly the declared output names of the group
keys and aggregates, referenced unqualified — grouping erases the input schema (S-27).

### S-11 · Output names are explicit

Every projection element, group key, and aggregate declares its output name explicitly. Schweep
does not derive names from expressions.

Duplicate output names in one schema are refused (`DuplicateOutputName`).

**SQL name derivation (C5).** SQL text does not always declare a name, so the binder needs a rule.
It is deliberately the shortest rule that can be stated:

| SQL select element | Output name |
| --- | --- |
| `<expr> AS n` | `n`, verbatim |
| `t.c` — a bare column reference | `c`, the column name without its qualifier |
| `c` — after a GROUP BY, an output name | `c` |
| anything else | refused: `MissingOutputName` |

`SELECT a + 1 FROM t` is refused, not named `a + 1` or `?column?` or `expr_1`. Every dialect invents
a different name there, none of them are names anyone wanted, and a *standing* computation's output
schema is not the place for a name nobody chose: the schema is part of answer equality (S-8), so a
derived name is a derived answer. Writing `AS` costs four characters.

**`SELECT *` is refused** (`SelectStarNotSupported`). This is not laziness about expansion — it is
the one refusal in this document that exists specifically because queries here are *standing*. A
`SELECT *` fixes its schema from the catalog at bind time, so adding a column to a table would
silently change the schema, and therefore the answer, of a query that is already running and whose
consumers are already reading it. Star expansion is a convenience for queries that are typed, run,
and thrown away. Nothing here is thrown away.

**Identifiers are taken verbatim, and comparison is case-sensitive** — quoted and unquoted alike.
`SELECT T.A FROM t AS T` resolves `T.A` against a column literally named `A`. Standard SQL folds
unquoted identifiers to upper case, PostgreSQL folds to lower, and MySQL's behaviour depends on the
filesystem; every one of those rules produces a name the user did not type. Since the typed API takes
names verbatim, folding in the SQL door would mean the two doors disagree about what a column is
called — and I-6 requires them to compile to the *same plan*. Verbatim is the only rule that makes
that possible without the typed API growing a folding rule of its own.

Keywords and **function names** do fold: `count(*)` and `COUNT(*)` are one function, and `select` is
`SELECT`. The line is between naming something the user created and naming something the dialect
defines — an identifier is data, a keyword is language, and SQL has always folded the second.

**Every column of a query's output schema is declared nullable.** Schema equality is part of
answer equality (S-8), so the oracle and the engine must agree on nullability exactly; a uniform
rule is one they cannot drift apart on, whereas per-expression nullability inference is a second
analysis that could disagree with itself. Where an aggregate genuinely never returns null —
`COUNT` (S-30) — that is a semantic guarantee stated in this document and pinned by a test, not a
decoration on the schema. Nullability is only *enforced* on stored table columns (S-2).

### S-12 · Binding is total and refusals are named

Before evaluation, a query is **bound**: every column reference is resolved, every expression is
type-checked, and every construct is checked against the dialect. Binding either succeeds with a
fully typed plan, or fails with an error that **names the offending construct**. A query is never
partly bound and never silently coerced.

There are no implicit type conversions anywhere in the v1 dialect (S-19).

---

## 3. Three-valued logic and NULL

Null semantics are three-valued (Kleene) **from rung 1**.

### S-13 · Comparison with NULL yields NULL

For `=`, `<>`, `<`, `<=`, `>`, `>=`: if either operand is `Null`, the result is `Null`. In
particular `NULL = NULL` is `NULL`, not `true`.

### S-14 · Arithmetic with NULL yields NULL

For `+`, `-`, `*`, `/`, `%`: if either operand is `Null`, the result is `Null` — and the operation
is not performed, so a `NULL` operand cannot raise a division-by-zero or overflow error.

### S-15 · Boolean connectives are Kleene

| `AND` | T | F | N |     | `OR` | T | F | N |     | `NOT` | |
|---|---|---|---|---|---|---|---|---|---|---|---|
| **T** | T | F | N |     | **T** | T | T | T |     | **T** | F |
| **F** | F | F | F |     | **F** | T | F | N |     | **F** | T |
| **N** | N | F | N |     | **N** | T | N | N |     | **N** | N |

Note the two rows that catch people: `F AND N = F` (not `N`), and `T OR N = T` (not `N`).

**`AND` and `OR` do not short-circuit: both operands are always evaluated.** So
`WHERE x <> 0 AND 100 / x > 1` raises `DivisionByZero` on a row where `x` is `0`, rather than
being saved by the left operand. SQL does not guarantee short-circuiting either, and "sometimes
evaluated" would make whether a query errors depend on evaluation order — which is precisely the
kind of thing I-2 exists to forbid. `CASE` is the one construct that *does* short-circuit, and it
does so by definition rather than as an optimisation (S-18).

### S-16 · IS NULL / IS NOT NULL are two-valued

`e IS NULL` and `e IS NOT NULL` always return `true` or `false`, never `Null`. They are the only
way to observe a null as a boolean.

### S-17 · WHERE and HAVING keep TRUE only

A row survives `WHERE` iff the predicate evaluates to `true`. `false` and `Null` both reject it.
`HAVING` behaves identically over aggregated rows (S-32).

*Consequence worth stating out loud:* `WHERE x = x` drops every row where `x` is null, and
`WHERE NOT (x = 1)` is not the complement of `WHERE x = 1` — the null rows fall out of both.

### S-18 · CASE selects the first TRUE branch

`CASE WHEN c1 THEN r1 WHEN c2 THEN r2 ... [ELSE e] END` evaluates conditions in order and yields
the result of the first condition that is `true`. `false` and `Null` conditions are both skipped.
If no condition is `true`, the result is `ELSE` if present and `Null` otherwise. All `THEN` and
`ELSE` expressions must have the same type (S-19); that type is the type of the `CASE`.

Evaluation is **short-circuiting**: expressions after the selected branch are not evaluated, so
they cannot raise an error. Conditions *are* evaluated in order until one is `true`, so an error
in an earlier condition is raised.

---

## 4. Scalar expressions

### S-19 · Typing is exact; no implicit conversion

| Expression | Operand types | Result |
| --- | --- | --- |
| `+ - * / %` | `Int64, Int64` | `Int64` |
| `= <> < <= > >=` | `T, T` for `T` in {`Int64`, `Utf8`, `Boolean`} | `Boolean` |
| `AND OR NOT` | `Boolean` | `Boolean` |
| `IS [NOT] NULL` | any | `Boolean` (non-null) |
| `CASE` | conditions `Boolean`; branches all `T` | `T` |
| column reference | — | the column's type |
| literal | — | its own type |

Any other combination is a binding error naming the operator and the operand types
(`TypeMismatch`). There is no `Int64`-to-`Utf8` coercion, no truthiness of integers, no
string-to-number parsing. `Float64` never appears as an operand or a result (S-3).

**Null literals carry a type.** A bare, untyped `NULL` literal is refused
(`UntypedNullLiteral`); a null literal is written with its type, and binding then types every
expression without inference.

**In SQL text (C5), a null is written `CAST(NULL AS <type>)`, and that is the only accepted `CAST`.**
The accepted type names are `BIGINT` (`Int64`), `TEXT` and `VARCHAR` (`Utf8`), and `BOOLEAN`; every
other cast — including a cast of anything that is not `NULL`, and including `DOUBLE` — is refused
(`UnsupportedCast`), because a cast that converts is exactly the implicit conversion this rule
forbids, made explicit. `SELECT CAST(a AS TEXT)` is a refusal, not a conversion.

The alternative was to infer a bare `NULL`'s type from its context, as SQL does. It was rejected on
the same grounds as everything else in this document that could have been inferred: inference is a
*second* analysis of the query, it lives only in the SQL door, and the oracle would then be typing
expressions by one set of rules and the binder by another. Two analyses that must agree are a
disagreement waiting to happen, and I-1 would report it as a correctness failure with no hint that
the cause was a type nobody wrote down. `CAST(NULL AS BIGINT)` is more typing and less machinery.

### S-20 · Integer overflow is an error, not a wrap

`+`, `-`, `*` are **checked**. An overflow of the `i64` range raises `ArithmeticOverflow` naming
the operator. Wrapping would be deterministic but wrong; saturating would be deterministic but a
lie. An error is the only answer that never silently corrupts a number.

### S-21 · Division and modulo by zero are errors

`x / 0` and `x % 0` raise `DivisionByZero`. Integer division truncates toward zero, and `%` takes
the sign of the dividend (Rust's and C's convention, and Postgres's).

`i64::MIN / -1` and `i64::MIN % -1` overflow and raise `ArithmeticOverflow` (S-20).

### S-22 · An error is a property of the *contents*, not of the change

An evaluation error is not a value and does not become a `Null`. Instead:

**The answer at epoch N is either a Z-set or an error, and which one it is depends only on the
contents at epoch N.** If the contents contain data on which the query raises, the query has no
answer at that epoch and the error is reported. When that data is retracted, the answer comes back.

This is the decision that closes the open question C0 left here and C1 sharpened (Q-2 in
`docs/DECISIONS.md`, now **D-16**). The alternative — an error as a property of the *change*, raised
by the epoch that carried the offending row and forgotten afterwards — was rejected. Two reasons,
and the second is fatal:

1. **The engine's whole claim is that an answer is a function of the current contents.** I-2 says
   the state and every answer at epoch N is a pure function of the log prefix up to N. An error that
   depended on *which epoch* delivered a row would make "does this query have an answer" depend on
   the delivery schedule rather than the data.
2. **It cannot be reconciled with I-3.** If an epoch that raises is simply dropped, the next epoch's
   changes land on contents that never absorbed the dropped epoch, and the answer is then a mixture
   of epoch N−1 and epoch N+1 — precisely the partial-epoch view I-3 forbids. Under this rule the
   epoch **seals normally**; only the *answer* is an error.

### S-22a · A row whose evaluation raises is dropped from the flow, and recorded

An expression that raises on a row yields no value for that row, so the row cannot continue: it is
dropped at the stage that raised, and the error is recorded as **live**. Downstream stages never see
it.

**For an aggregate the unit is the group, not the row.** A group whose aggregates cannot all be
evaluated — because an argument raises on one of its members, or because the aggregate itself
overflows (S-30) — produces no output row, and the error is recorded. A group row must have a value
in every aggregate column, so there is no partial group to emit; and making the unit the group means
one rule covers both "a member row raised" and "the total overflowed", which a per-row rule would
not.

Dropping rather than propagating is what makes the two implementations comparable. The oracle
recomputes over the whole contents and the engine sees one delta at a time; if an erroring row were
carried forward with some placeholder, each would have to invent the same placeholder. Dropping is
the same decision on both sides regardless of when the row arrived.

### S-22b · The live errors form a Z-set, so retraction returns the answer for free

The set of live errors is maintained exactly like any other Z-set (S-4): a row that raises
contributes its error at the row's weight, and retracting the row retracts the error by the same
arithmetic. Nothing special-cases the removal of an error, which is I-5 applied to errors, and it is
why "the answer comes back when the data leaves" needs no separate mechanism.

An error's identity is its **message**. Two rows that raise the same error are two copies of one live
error; the answer is an error while the total weight of the live-error Z-set is positive.

### S-22c · With several live errors, the least message is reported

A query may have more than one live error at once — a division by zero in one row and an overflow in
another. The reported error is the **lexicographically least message** among the live errors.

The choice of *which* error to report is arbitrary; being deterministic about it is not. A rule
stated in terms of the messages alone is one that any implementation can follow without agreeing on
scan order, stage order, or which row it happened to look at first — and I-1 requires the two
implementations to produce the same bytes, error text included. The rule is therefore
order-independent by construction rather than by care.

*Consequence worth stating:* the reported message may change when data is inserted or retracted even
though the query is still in error, because the least live message may change. That is a property of
the data, which is what S-22 says an error is.

### S-22d · Errors are deterministic

The same log prefix and the same query always produce the same answer, error included, byte for byte
(I-2). There is no timeout, no partial answer, and no dependence on how the history was batched into
epochs: a history delivered as one big epoch and the same history delivered one row at a time raise
the same error at the end.

---

## 5. Relational operators

### S-23 · Scan

`Scan(table, alias)` yields the table's integral at the current epoch (S-6), with the schema's
columns renamed to `alias.column`. Weights pass through unchanged and are non-negative (S-5).

### S-24 · WHERE preserves weights

Filtering keeps entries whose predicate is `true` (S-17) with their weights **unchanged**. It
never splits, merges, or resigns a weight. Filter is linear: it treats a weight `-1` entry exactly
as it treats a weight `+1` entry (I-5), because it never looks at the weight at all.

### S-25 · Projection preserves multiplicity and may merge rows

Projection evaluates its expressions per entry and keeps the entry's weight. Because a projection
can drop distinguishing columns, two distinct input rows can become the same output row; the
result is then **consolidated** (S-4), summing their weights. So a table holding `(1,'a')` and
`(1,'b')` each at weight 1 projects on the first column to `(1)` at weight **2**.

This is plain SQL multiset semantics: `SELECT` without `DISTINCT` preserves duplicates.
De-duplication is `DISTINCT`, which is rung 4 and not available here.

### S-26 · INNER equi-join multiplies weights

`Join(left, right, ON [(l1,r1), (l2,r2), ...])` pairs an entry from each side when **every** key
pair compares equal by S-13 — that is, when each `li = ri` evaluates to `true`. The output entry's
weight is the **product** of the two input weights.

Consequences, all of them intended:

- **A null key never joins.** If any `li` or `ri` is `Null`, the comparison is `Null`, not `true`,
  so the pair does not match (S-13). A row with a null join key contributes to no output row.
- **Multiplicities multiply.** Three copies of a left row against two matching right rows produce
  six copies of the joined row. This is what makes the join bilinear and is what
  `ΔA⋈B + A⋈ΔB + ΔA⋈ΔB` is computing (D-3); the oracle gets it for free by multiplying, and the
  engine will have to earn it in C2.
- **Output schema** is the left schema followed by the right schema, with columns keeping their
  `alias.column` names. Since aliases are unique within a query, there are no collisions; a
  repeated alias is refused (`DuplicateAlias`).
- At least one key pair is required. A join with no key pairs (a cross join) is refused
  (`CrossJoinNotSupported`) — it is not in the rung-2 dialect.

---

## 6. Aggregation

### S-27 · GROUP BY erases the input schema

The output of a GROUP BY is: the group-key columns, in declared order, followed by the aggregate
columns, in declared order — all under their declared output names (S-11), referenced unqualified
thereafter. Input columns are no longer reachable. There is no "bare column" rule to violate,
because a column that is not a group key simply cannot be named.

**Each group produces exactly one output row, with weight 1.** An aggregate result is a statement
about a group, and a group either exists or does not; it has no multiplicity.

### S-28 · Grouping treats NULLs as equal to each other

Two rows are in the same group iff their key values are **not distinct** — i.e. equal, or both
`Null`, position by position. `Null` forms a group like any other value.

This is deliberately *not* S-13. Comparison in `WHERE`/`ON` is three-valued and `NULL = NULL` is
unknown; grouping is an equivalence relation and must be reflexive, so it uses "not distinct
from". SQL makes the same split, and it is the single most common source of confusion in this
area, so: **`ON` uses `=` (nulls never match); `GROUP BY` uses "not distinct" (nulls group
together).**

### S-29 · A group exists iff its total weight is positive; a drained group vanishes

The group's total weight is the sum of the weights of its member entries. Because table integrals
are non-negative (S-5), that sum is ≥ 0, and:

- total weight > 0 → the group exists and emits exactly one row (weight 1);
- total weight = 0 → **the group does not exist and emits nothing.**

A group drained to zero rows produces **no output row at all** — not a row of zeroes, not
`(key, 0)`, not `(key, NULL)`. The row disappears. (This is the C3 pitfall, decided here in C0 by
the oracle, as §5.1 requires.)

### S-30 · Aggregates respect weights and ignore NULLs

Let a group contain entries `(row_i, w_i)` with `w_i ≥ 1`, and let `x_i` be the aggregated
expression evaluated on `row_i`. Let `P` be the entries where `x_i` is **not** null.

| Aggregate | Definition | Result type | Empty `P` |
| --- | --- | --- | --- |
| `COUNT(*)` | `Σ w_i` over all entries | `Int64` | n/a — group exists, so ≥ 1 |
| `COUNT(x)` | `Σ w_i` over `P` | `Int64` | **`0`**, never null |
| `SUM(x)` | `Σ w_i · x_i` over `P` | `Int64` | **`Null`** |
| `MIN(x)` | least `x_i` over `P` by S-7 | type of `x` | **`Null`** |
| `MAX(x)` | greatest `x_i` over `P` by S-7 | type of `x` | **`Null`** |
| `AVG(x)` | see S-31 | `Float64` | **`Null`** |

- `COUNT` is the only aggregate that never returns `Null`. `COUNT(x)` of an all-null group is
  `0`; `SUM` of an all-null group is `Null`. That asymmetry is SQL's, and it is intentional.
- **Weights are multiplicities, so they count.** `COUNT(*)` of a single row at weight 3 is `3`;
  `SUM(x)` of that row is `3·x`. `MIN`/`MAX` are unaffected by multiplicity — a value present
  once and a value present three times are equally present — but a value must be present
  (weight ≥ 1) to be considered.
- `SUM` and `AVG` accept only `Int64` (S-3). `MIN`/`MAX` accept `Int64`, `Utf8`, `Boolean`.
  `COUNT(x)` accepts any type. `COUNT(*)` takes no argument.
- **`SUM` accumulates in `i128` and must land in `Int64`.** If the exact sum does not fit in
  `i64`, the query raises `AggregateOverflow` (S-20's rule, applied to aggregation). The
  intermediate width means a sum that transits through large partial values but ends in range is
  still correct, which is precisely the property an incremental `SUM` under retraction needs.

### S-31 · AVG is one division of two exact integers

`AVG(x) = (SUM(x) as f64) / (COUNT(x) as f64)`, computed as a **single** IEEE-754 division at emit
time, from the exact `i64` sum and the exact `i64` count. It is never accumulated as a float and
never computed incrementally as a float.

`COUNT(x)` is `0` only when `P` is empty, in which case `AVG` is `Null` (S-30) and the division is
not performed — so `AVG` never divides by zero and never produces `NaN` or an infinity.

The `i64`-to-`f64` conversions are exact for magnitudes below 2⁵³ and round-to-nearest-even above
it; both implementations perform the identical two conversions and the identical division, so both
produce the identical bits. That is what makes `AVG` compatible with I-1 while `SUM(float)` is not.

### S-32 · HAVING filters aggregated rows

`HAVING` is evaluated after aggregation, over the GROUP BY output schema (S-27), and keeps rows
where the predicate is `true` (S-17). It may reference group keys and aggregate outputs by their
declared names, and nothing else. It never re-opens the input rows, so there are no aggregates
inside `HAVING` beyond the ones already declared — an aggregate call in a `HAVING` expression is
refused (`AggregateInHaving`); declare it as an output and reference it by name.

*Through the typed API that refusal has no code, because the API cannot express it:* a `HAVING` is a
scalar expression, and the scalar expression type has no aggregate variant, so the illegal query does
not type-check in Rust. Unrepresentable is a stronger guarantee than refused.

**Through the SQL door (C5) it is a real refusal**, because SQL text can certainly write
`HAVING COUNT(*) > 2`. So can it write an aggregate in two other places the typed API forbids by
construction, and each gets its own name rather than one shared "aggregate not allowed here":

| SQL | Refusal | Why |
| --- | --- | --- |
| `HAVING COUNT(*) > 2` | `AggregateInHaving` | declare it as an output and reference it by name |
| `WHERE COUNT(*) > 2` | `AggregateInWhere` | `WHERE` runs *before* aggregation (S-9), so there is nothing to aggregate yet |
| `SUM(COUNT(a))` | `NestedAggregate` | an aggregate consumes rows, and an aggregate is not a row |
| `COUNT(*) + 1` | `AggregateNotTopLevel` | an aggregate must be the whole output column — see below |

**An aggregate must be the whole output column.** `SELECT COUNT(*) + 1 AS n` is refused, for the same
reason a derived name is (S-11): an expression *over* aggregates is a projection over the group output,
and its inputs would need names nobody wrote. The workaround is to select the aggregate and do the
arithmetic where the answer is read — which, for a maintained answer, is the honest place for it:
`COUNT(*) + 1` changes on exactly the epochs `COUNT(*)` does, so maintaining it separately buys
nothing.

`HAVING COUNT(*) > 2` has an obvious rewrite — `SELECT ..., COUNT(*) AS n ... HAVING n > 2` — and the
refusal message says so. The rewrite is deliberately left to the person writing the query: performing
it in the binder would add an output column the query did not ask for, which would change the output
schema, which is part of the answer (S-8).

### S-34 · `DISTINCT` keeps one copy of every row that is present

`DISTINCT` maps a Z-set with non-negative weights to one in which every row present at all appears
exactly once:

```text
distinct(z)[row] = 1  if z[row] > 0
                   0  otherwise
```

It is applied **last**, after the projection — `SELECT DISTINCT` de-duplicates the rows the query
would otherwise return, not the rows it read.

Three consequences worth stating, because each is a place a naive implementation goes wrong:

- **Weights collapse, they do not saturate.** A row at weight 7 becomes weight 1, and a row at
  weight 1 stays weight 1. `DISTINCT` is the one operator in Schweep whose output weight is not a
  sum or a product of its input weights.
- **It is stateful, and it is the reason.** Incrementally, the output changes only when a row
  crosses between absent and present. That is a question about the row's *integral*, not about the
  delta, so the operator must remember the integral of its input: `Δout[row] = sign(I[row] + Δ[row])
  − sign(I[row])`. An implementation that looked only at the delta would emit a spurious `+1` every
  time an already-present row gained another copy.
- **Nulls are values here.** Two rows that are equal — including equal in their nulls — are one row
  (the same "not distinct from" notion as grouping, S-28, and *not* the three-valued `=` of S-13).
  This falls out of Z-set row equality and needs no special case.

`DISTINCT` over an input that could hold negative weights is not defined, and cannot arise: it is
applied to a query's output, and every stage from a table integral onward preserves
non-negativity — filter and projection carry weights through, a join multiplies non-negatives, and
an aggregate emits weight 1 per group.

### S-33 · Aggregation without GROUP BY is one group that always exists

A query with aggregates and **no group keys** — `SELECT COUNT(*) FROM t`, the *grand total* — has
exactly one group, and **that group exists whether or not the input has any rows.** Over an empty
input it returns one row: `COUNT(*)` is `0`, `COUNT(x)` is `0`, and `SUM`, `MIN`, `MAX` and `AVG` are
all `NULL` (S-30's rules for an empty `P`, applied to an empty group).

This closes the question C0 left open (Q-3 in `docs/DECISIONS.md`, now **D-20**), and it agrees with
standard SQL.

**It is an exception to S-29, and the exception is exactly one sentence wide.** S-29 says a group
exists iff its total weight is positive, so that a group drained to zero rows *vanishes* rather than
emitting `(key, 0)`. That rule is about **keyed** groups: a key is a value that came from the data, so
a group whose rows have all left has nothing left to name it. A grand total has no key. There is
nothing data-dependent about its identity, so there is nothing for its existence to depend on — it is
one group, and it is always there.

Put the other way round: S-29 forbids conjuring a row for a key nobody supplied. A grand total's row
has no key to conjure.

**Why match SQL rather than stay uniform.** The alternative — a grand total that returns nothing over
an empty input — is defensible from S-29 and was rejected. `SELECT COUNT(*) FROM t` returning *no
rows* for an empty table would be read as a broken database by every person who ever ran it, and "the
dialect is ours" (§8) is a licence to omit constructs, not to give a familiar one an unfamiliar
answer. The pitch is *every answer, current*; the answer to "how many rows are there" over an empty
table is `0`, not "there is no answer".

**What it costs, stated.** The engine must have a **defined initial state**: an answer that is
non-empty before any epoch is sealed. That is new — every other answer starts empty and is built up
from deltas — and it means a circuit's result store is primed when the circuit is built rather than
starting at nothing. That is a real change to the runtime, and it is the price of this decision.

**In SQL there is no GROUP BY clause to write.** `SELECT COUNT(*) AS n FROM t` names no keys at all,
so the binder synthesises the keyless `GroupBy` when the select list contains an aggregate and the
query has no `GROUP BY` clause. A select list that mixes an aggregate with anything that is not a
grouping expression is refused (`ColumnNotGrouped`) — `SELECT t.a, COUNT(*) AS n FROM t` names no
group for `a` to belong to, and SQL dialects that answer it anyway are picking a row arbitrarily.

The refusal is by **whole expression**, not by functional dependence: `SELECT t.a + 1 AS o, COUNT(*) AS
n FROM t GROUP BY t.a` is refused too, though standard SQL accepts it. Accepting it would mean
substituting the key into the expression and then projecting over the group output — a rewrite pass
whose only purpose is to let one query be written a second way. The workaround is to group by the
expression the query actually wants: `GROUP BY t.a + 1` binds, and says what it means.

`HAVING` applies to the grand total like any other group: `HAVING COUNT(*) > 0` over an empty input
evaluates `0 > 0`, which is false, so the row is filtered out and the answer *is* empty (S-32, S-17).
That composition is not a special case — it falls out of the two rules — and it is the shape most
likely to surprise, so it is written down.

---

## 7. The SQL surface

### S-35 · The SQL door translates; it never means anything the typed API cannot

SQL text is a second *way of writing* a query, not a second dialect. Every accepted statement binds
to the same `Query` the typed API builds, and I-6 makes that testable rather than aspirational: the
same query written both ways compiles to structurally identical plans, checked by hash equality, and
runs with identical operator counters.

The consequence is worth stating plainly, because it is the opposite of how SQL frontends usually
grow: **the SQL surface can only shrink, never extend.** No construct is accepted because sqlparser
happens to parse it. If SQL can express something the typed API cannot, the answer is a named refusal
here, or a change to the typed API and to this document first — never a special case in the parser.

Everything in §8's table is refused through the SQL door too, by the same name. These refusals exist
only in the SQL door, because only SQL text can write them:

| Construct | Refusal |
| --- | --- |
| `SELECT *` | `SelectStarNotSupported` (S-11) |
| a computed output column with no `AS` | `MissingOutputName` (S-11) |
| a bare `NULL` | `UntypedNullLiteral` (S-19) |
| any `CAST` other than `CAST(NULL AS <type>)` | `UnsupportedCast` (S-19) |
| an aggregate in `HAVING` / `WHERE` / an aggregate | `AggregateInHaving` / `AggregateInWhere` / `NestedAggregate` (S-32) |
| an expression over an aggregate | `AggregateNotTopLevel` (S-32) |
| a select item that is neither grouped nor aggregated | `ColumnNotGrouped` (S-33) |
| a function that is not one of the six aggregates | `UnknownFunction` |
| everything else SQL has and this dialect does not | `NotInDialect(<construct>)` (§8) |

A refusal that does not name its construct is a bug in the binder, not a stylistic matter (S-12).
The gate asserts it: every refusal the SQL fuzzer provokes must name what it refused.

### S-36 · Projection is emitted only when the select list is not already the answer

A `GROUP BY` computes its keys and then its aggregates, in that order, under their declared names
(S-27). When a SQL select list asks for exactly that — the same names, in the same order — the bound
plan carries **no** projection, because a projection to the schema you already have is a node that
does nothing. When the select list reorders or narrows, a projection is emitted.

This is written down because it is the one place where the shape of the bound plan depends on the
*text* rather than only on the meaning, and I-6 compares shapes. `SELECT a, n FROM ... GROUP BY a`
and `SELECT n, a FROM ... GROUP BY a` are different queries with different answers — the column
order is part of the schema, and the schema is part of the answer (S-8) — so they bind to different
plans, and the second one's extra projection is not an artefact.

---

## 8. What this document deliberately does not define

Refused by name, and the name is the point:

| Construct | Refusal | Arrives |
| --- | --- | --- |
| `UNION ALL` | `NotInDialect("UNION ALL")` | rung 4 |
| `ORDER BY` / `LIMIT` | `NotInDialect("ORDER BY")` | rung 4, at read time (D-7) |
| `LEFT`/`RIGHT`/`FULL JOIN` | `NotInDialect("OUTER JOIN")` | rung 5 |
| cross join / no join keys | `CrossJoinNotSupported` | rung 5 evaluation |
| subqueries | `NotInDialect("subquery")` | rung 5 where decorrelatable |
| window functions | `NotInDialect(...)` | post-v1, by evidence of need (§8) |
| user-defined functions | `NotInDialect(...)` | post-v1 (§8) |
| recursive / iterative queries | `NotInDialect(...)` | out of scope for v1 (D-3) |
| `Float64` columns, float arithmetic | `NotInDialect("FLOAT")` | open decision — decimals (S-3) |
| collations, locale-aware comparison | `NotInDialect("COLLATE")` | post-v1 |

---

## 9. Index of rules

S-1 values · S-2 types and schemas · S-3 Float64 is result-only · S-4 rows, Z-sets, weights ·
S-5 non-negative integrals · S-6 epochs · S-7 total order on values · S-8 canonical form and
answer equality · S-9 query shape · S-10 column references · S-11 explicit output names ·
S-12 binding and named refusals · S-13 comparison with NULL · S-14 arithmetic with NULL ·
S-15 Kleene connectives · S-16 IS NULL · S-17 WHERE keeps TRUE only · S-18 CASE · S-19 exact
typing · S-20 overflow is an error · S-21 division by zero · S-22 an error is a property of the
contents · S-22a erroring rows are dropped and recorded · S-22b live errors form a Z-set ·
S-22c the least message is reported · S-22d errors are deterministic ·
S-23 scan · S-24 filter preserves weights · S-25 projection merges rows · S-26 join multiplies
weights · S-27 GROUP BY erases the schema · S-28 grouping treats NULLs as equal · S-29 drained
groups vanish · S-30 aggregates respect weights and ignore NULLs · S-31 AVG is one division ·
S-32 HAVING · S-33 grand-total aggregation is one always-present group · S-34 DISTINCT ·
S-35 the SQL door translates and can only shrink · S-36 projection is emitted only when the select
list is not already the answer.

Three rules were added after the first draft, when writing the oracle exposed questions the draft
had not answered. They are recorded in place rather than appended, and the additions are: null
literals carry a type (in S-19); `AND`/`OR` do not short-circuit (in S-15); every output column is
declared nullable (in S-11). The doc moved first, then the code — which is the order §10 requires,
and the reason the order exists.
