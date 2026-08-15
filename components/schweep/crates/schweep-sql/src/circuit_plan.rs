//! The circuit plan: what the incrementalizer produces, and what I-6 compares.
//!
//! A [`CircuitPlan`] is a **description** of a circuit, not a circuit. It holds no state, no
//! backends, and no epoch; it can be built, compared, hashed, and printed without allocating a byte
//! of operator state. [`crate::instantiate`] turns one into a running circuit.
//!
//! That split exists for three reasons, and all three are load-bearing:
//!
//! 1. **I-6** — "the typed API and the SQL door produce identical plans". Comparing *circuits* would
//!    mean comparing operator objects that own state and backends; comparing plans is comparing two
//!    values, and the comparison is exact.
//! 2. **C6's memo (I-8)** — sharing sub-circuits between standing queries requires a structural hash
//!    of every *subtree*, which is a property of the description, not of the running thing.
//! 3. **Diagnosis** — [`CircuitNode::structural_form`] renders a plan as an s-expression, so a failed
//!    I-6 comparison prints two readable trees instead of two different 64-bit numbers.
//!
//! The hash is FNV-1a over that rendering rather than `std::hash::Hash`: the standard hasher's output
//! is explicitly not stable across releases, and a plan hash that changes when the toolchain changes
//! is not something a memo — or a committed evidence artifact — can rely on.

use std::fmt::Write as _;

use schweep_plan::bind::Naming;
use schweep_plan::plan::{AggFunc, BinOp, Expr, Named};
use schweep_zset::Schema;

/// The DBSP rule that justifies one node's incremental form (`ARCHITECTURE.md` §5.6).
///
/// This is recorded on every node rather than left in a comment because it is a claim about
/// correctness, and a claim in a comment is a claim nothing checks. `incremental.rs` documents each
/// rule in full; `rule_of` in this crate's tests asserts that every node kind still carries the rule
/// its documentation says it does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rule {
    /// A delta arrives from the log; there is nothing to incrementalize.
    Input,
    /// **Linear**: `f(a + b) = f(a) + f(b)`, so `f` applied to a delta *is* the delta of `f` applied
    /// to the whole. Filter and Project. No state.
    Linear,
    /// **Bilinear**: linear in each argument separately, so the delta expands to three terms
    /// (S-26, §5.6). Join. State proportional to both inputs.
    Bilinear,
    /// **Neither**: the answer for a group is a function of the group's whole contents, so the
    /// operator keeps those contents and recomputes the groups a delta touched (S-29, S-30).
    StatefulPerGroup,
    /// **Neither**: presence is a step function of accumulated weight, so the operator keeps the
    /// accumulated weight per row and emits only where presence flipped (S-34).
    StatefulPerRow,
}

/// A node of a circuit plan. The tree mirrors the circuit exactly — one node, one operator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CircuitNode {
    Source {
        table: String,
        alias: String,
        schema: Schema,
    },
    Filter {
        input: Box<CircuitNode>,
        naming: Naming,
        predicate: Expr,
    },
    Project {
        input: Box<CircuitNode>,
        naming: Naming,
        items: Vec<Named<Expr>>,
        schema: Schema,
    },
    Join {
        left: Box<CircuitNode>,
        right: Box<CircuitNode>,
        /// Column positions, left index paired with right index (resolved by the incrementalizer so
        /// the operator never looks a name up).
        keys: Vec<(usize, usize)>,
        schema: Schema,
    },
    Aggregate {
        input: Box<CircuitNode>,
        keys: Vec<Named<Expr>>,
        aggregates: Vec<Named<AggFunc>>,
        schema: Schema,
    },
    Distinct {
        input: Box<CircuitNode>,
    },
}

impl CircuitNode {
    /// The schema this node emits.
    #[must_use]
    pub fn schema(&self) -> &Schema {
        match self {
            CircuitNode::Source { schema, .. }
            | CircuitNode::Project { schema, .. }
            | CircuitNode::Join { schema, .. }
            | CircuitNode::Aggregate { schema, .. } => schema,
            // Filter and Distinct change weights, never columns (S-24, S-34).
            CircuitNode::Filter { input, .. } | CircuitNode::Distinct { input } => input.schema(),
        }
    }

    /// The rule that justifies this node's incremental form.
    #[must_use]
    pub fn rule(&self) -> Rule {
        match self {
            CircuitNode::Source { .. } => Rule::Input,
            CircuitNode::Filter { .. } | CircuitNode::Project { .. } => Rule::Linear,
            CircuitNode::Join { .. } => Rule::Bilinear,
            CircuitNode::Aggregate { .. } => Rule::StatefulPerGroup,
            CircuitNode::Distinct { .. } => Rule::StatefulPerRow,
        }
    }

    /// This node and every node beneath it, deepest-last.
    #[must_use]
    pub fn nodes(&self) -> Vec<&CircuitNode> {
        let mut out = Vec::new();
        self.walk(&mut out);
        out
    }

    fn walk<'a>(&'a self, out: &mut Vec<&'a CircuitNode>) {
        match self {
            CircuitNode::Source { .. } => {}
            CircuitNode::Filter { input, .. }
            | CircuitNode::Project { input, .. }
            | CircuitNode::Aggregate { input, .. }
            | CircuitNode::Distinct { input } => input.walk(out),
            CircuitNode::Join { left, right, .. } => {
                left.walk(out);
                right.walk(out);
            }
        }
        out.push(self);
    }

    /// A deterministic s-expression rendering: the plan's identity, in text.
    ///
    /// Everything that distinguishes two plans is in here, and nothing that does not. Schemas are
    /// included because a schema is part of an answer (S-8); node *order* is included because a
    /// projection that reorders columns is a different query (S-36).
    #[must_use]
    pub fn structural_form(&self) -> String {
        let mut out = String::new();
        self.write_form(&mut out, 0);
        out
    }

    fn write_form(&self, out: &mut String, depth: usize) {
        let pad = "  ".repeat(depth);
        match self {
            CircuitNode::Source {
                table,
                alias,
                schema,
            } => {
                let _ = writeln!(out, "{pad}(source {table} as {alias} {schema})");
            }
            CircuitNode::Filter {
                input,
                naming,
                predicate,
            } => {
                let _ = writeln!(
                    out,
                    "{pad}(filter {} {})",
                    naming_form(*naming),
                    expr_form(predicate)
                );
                input.write_form(out, depth + 1);
            }
            CircuitNode::Project {
                input,
                naming,
                items,
                schema,
            } => {
                let rendered: Vec<String> = items
                    .iter()
                    .map(|item| format!("({} {})", item.name, expr_form(&item.value)))
                    .collect();
                let _ = writeln!(
                    out,
                    "{pad}(project {} [{}] {schema})",
                    naming_form(*naming),
                    rendered.join(" ")
                );
                input.write_form(out, depth + 1);
            }
            CircuitNode::Join {
                left,
                right,
                keys,
                schema,
            } => {
                let rendered: Vec<String> =
                    keys.iter().map(|(l, r)| format!("({l} {r})")).collect();
                let _ = writeln!(out, "{pad}(join [{}] {schema})", rendered.join(" "));
                left.write_form(out, depth + 1);
                right.write_form(out, depth + 1);
            }
            CircuitNode::Aggregate {
                input,
                keys,
                aggregates,
                schema,
            } => {
                let key_form: Vec<String> = keys
                    .iter()
                    .map(|k| format!("({} {})", k.name, expr_form(&k.value)))
                    .collect();
                let agg_form: Vec<String> = aggregates
                    .iter()
                    .map(|a| format!("({} {})", a.name, agg_func_form(&a.value)))
                    .collect();
                let _ = writeln!(
                    out,
                    "{pad}(aggregate keys [{}] aggs [{}] {schema})",
                    key_form.join(" "),
                    agg_form.join(" ")
                );
                input.write_form(out, depth + 1);
            }
            CircuitNode::Distinct { input } => {
                let _ = writeln!(out, "{pad}(distinct)");
                input.write_form(out, depth + 1);
            }
        }
    }

    /// FNV-1a over [`CircuitNode::structural_form`] — the structural hash I-6 compares (§5.7).
    #[must_use]
    pub fn structural_hash(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in self.structural_form().bytes() {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01B3);
        }
        hash
    }
}

/// A whole circuit plan: the root node, and the answer's schema.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CircuitPlan {
    pub root: CircuitNode,
    pub output_schema: Schema,
}

impl CircuitPlan {
    #[must_use]
    pub fn structural_form(&self) -> String {
        self.root.structural_form()
    }

    #[must_use]
    pub fn structural_hash(&self) -> u64 {
        self.root.structural_hash()
    }

    /// Every node, deepest-last — the memo's future unit of sharing, and the counter gate's unit of
    /// comparison.
    #[must_use]
    pub fn nodes(&self) -> Vec<&CircuitNode> {
        self.root.nodes()
    }
}

fn naming_form(naming: Naming) -> &'static str {
    match naming {
        Naming::Qualified => "qualified",
        Naming::Unqualified => "unqualified",
    }
}

/// Render an expression unambiguously — fully parenthesised, so no two distinct expressions can
/// render alike and no precedence rule is needed to read it back.
fn expr_form(expr: &Expr) -> String {
    match expr {
        Expr::Column(name) => format!("#{name}"),
        Expr::Literal(value) => format!("{value}"),
        Expr::Null(ty) => format!("null:{ty}"),
        Expr::Binary { op, left, right } => format!(
            "({} {} {})",
            op_form(*op),
            expr_form(left),
            expr_form(right)
        ),
        Expr::Not(inner) => format!("(not {})", expr_form(inner)),
        Expr::And(l, r) => format!("(and {} {})", expr_form(l), expr_form(r)),
        Expr::Or(l, r) => format!("(or {} {})", expr_form(l), expr_form(r)),
        Expr::IsNull(inner) => format!("(is-null {})", expr_form(inner)),
        Expr::IsNotNull(inner) => format!("(is-not-null {})", expr_form(inner)),
        Expr::Case { whens, otherwise } => {
            let arms: Vec<String> = whens
                .iter()
                .map(|(c, r)| format!("({} {})", expr_form(c), expr_form(r)))
                .collect();
            let else_form = match otherwise {
                None => "none".to_owned(),
                Some(e) => expr_form(e),
            };
            format!("(case [{}] else {else_form})", arms.join(" "))
        }
    }
}

fn op_form(op: BinOp) -> &'static str {
    op.name()
}

fn agg_func_form(func: &AggFunc) -> String {
    match func.argument() {
        None => func.name().to_owned(),
        Some(arg) => format!("{}({})", func.name(), expr_form(arg)),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;
    use schweep_zset::{DataType, Field};

    fn schema(names: &[&str]) -> Schema {
        Schema::new(
            names
                .iter()
                .map(|n| Field::nullable(*n, DataType::Int64))
                .collect(),
        )
        .unwrap()
    }

    fn source() -> CircuitNode {
        CircuitNode::Source {
            table: "t".to_owned(),
            alias: "t".to_owned(),
            schema: schema(&["t.a"]),
        }
    }

    #[test]
    fn a_filter_does_not_change_the_schema() {
        let node = CircuitNode::Filter {
            input: Box::new(source()),
            naming: Naming::Qualified,
            predicate: Expr::boolean(true),
        };
        assert_eq!(node.schema(), &schema(&["t.a"]));
    }

    #[test]
    fn nodes_are_deepest_last() {
        let node = CircuitNode::Distinct {
            input: Box::new(CircuitNode::Filter {
                input: Box::new(source()),
                naming: Naming::Qualified,
                predicate: Expr::boolean(true),
            }),
        };
        let kinds: Vec<Rule> = node.nodes().iter().map(|n| n.rule()).collect();
        assert_eq!(
            kinds,
            vec![Rule::Input, Rule::Linear, Rule::StatefulPerRow],
            "a parent must come after its children, because that is the order a circuit runs in"
        );
    }

    /// Every node kind carries the rule its documentation claims (§5.6).
    #[test]
    fn every_node_kind_carries_its_dbsp_rule() {
        assert_eq!(source().rule(), Rule::Input);
        assert_eq!(
            CircuitNode::Filter {
                input: Box::new(source()),
                naming: Naming::Qualified,
                predicate: Expr::boolean(true),
            }
            .rule(),
            Rule::Linear
        );
        assert_eq!(
            CircuitNode::Project {
                input: Box::new(source()),
                naming: Naming::Qualified,
                items: vec![],
                schema: schema(&["x"]),
            }
            .rule(),
            Rule::Linear
        );
        assert_eq!(
            CircuitNode::Join {
                left: Box::new(source()),
                right: Box::new(source()),
                keys: vec![(0, 0)],
                schema: schema(&["t.a", "u.a"]),
            }
            .rule(),
            Rule::Bilinear
        );
        assert_eq!(
            CircuitNode::Aggregate {
                input: Box::new(source()),
                keys: vec![],
                aggregates: vec![Named::new("n", AggFunc::CountStar)],
                schema: schema(&["n"]),
            }
            .rule(),
            Rule::StatefulPerGroup
        );
        assert_eq!(
            CircuitNode::Distinct {
                input: Box::new(source())
            }
            .rule(),
            Rule::StatefulPerRow
        );
    }

    /// The rendering is the identity: two plans that differ anywhere render differently, and the
    /// hash follows the rendering.
    #[test]
    fn the_structural_form_distinguishes_plans_that_differ() {
        let a = CircuitNode::Filter {
            input: Box::new(source()),
            naming: Naming::Qualified,
            predicate: Expr::binary(BinOp::Gt, Expr::column("t.a"), Expr::int(1)),
        };
        let b = CircuitNode::Filter {
            input: Box::new(source()),
            naming: Naming::Qualified,
            predicate: Expr::binary(BinOp::Gt, Expr::column("t.a"), Expr::int(2)),
        };
        assert_ne!(a.structural_form(), b.structural_form());
        assert_ne!(a.structural_hash(), b.structural_hash());
        assert_eq!(a.structural_hash(), a.clone().structural_hash(), "stable");
    }

    /// A column reference and a string literal of the same text must not render alike, or the memo
    /// could share two sub-circuits that compute different things.
    #[test]
    fn a_column_and_a_string_literal_render_differently() {
        assert_ne!(
            expr_form(&Expr::column("a")),
            expr_form(&Expr::string("a")),
            "#a vs 'a'"
        );
    }

    #[test]
    fn the_form_is_readable() {
        let node = CircuitNode::Aggregate {
            input: Box::new(source()),
            keys: vec![Named::new("a", Expr::column("t.a"))],
            aggregates: vec![Named::new("n", AggFunc::CountStar)],
            schema: schema(&["a", "n"]),
        };
        assert_eq!(
            node.structural_form(),
            "(aggregate keys [(a #t.a)] aggs [(n COUNT(*))] (a: Int64, n: Int64))\n  \
             (source t as t (t.a: Int64))\n"
        );
    }
}
