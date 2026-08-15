//! The binder's semantics, asserted against **hand-written** SQL and hand-written typed queries.
//!
//! This is where the SQL door's content is. The C5 fuzzer renders the typed population to SQL and
//! compares the plans, which proves the two doors agree — but both sides of that comparison are
//! written by the same author, so agreement is not by itself evidence that either matches
//! `docs/SEMANTICS.md`. These tests are the evidence: for each rule, SQL text on one side and the
//! plan the rule says it means on the other, written out in full.
//!
//! Rules covered here: S-11 (names, `SELECT *`, verbatim identifiers), S-19 (`CAST(NULL AS T)`),
//! S-27/S-33 (grouping, the grand total, `ColumnNotGrouped`), S-32 (aggregates in the wrong place),
//! S-36 (when a projection is emitted), and I-6 on curated pairs including shapes the fuzzer's
//! renderer declines.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_plan::bind::{Catalog, Naming};
use schweep_plan::plan::{AggFunc, BinOp, Expr, GroupBy, Named, Query, Source};
use schweep_plan::PlanError;
use schweep_sql::{bind_sql, compile, incrementalize_typed, SqlError};
use schweep_zset::{DataType, Field, Schema};

fn catalog() -> Catalog {
    let t = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("n", DataType::Int64, true),
        Field::new("s", DataType::Utf8, true),
        Field::new("b", DataType::Boolean, true),
    ])
    .expect("t schema");
    let u = Schema::new(vec![
        Field::new("id", DataType::Int64, false),
        Field::new("m", DataType::Int64, true),
    ])
    .expect("u schema");
    // A column whose name would collide with a keyword, and one that differs only in case, so the
    // verbatim rule has something to be verbatim about.
    let odd = Schema::new(vec![
        Field::new("count", DataType::Int64, true),
        Field::new("A", DataType::Int64, true),
        Field::new("a", DataType::Int64, true),
    ])
    .expect("odd schema");
    Catalog::from([
        ("t".to_owned(), t),
        ("u".to_owned(), u),
        ("odd".to_owned(), odd),
    ])
}

fn bound(sql: &str) -> Query {
    match bind_sql(sql, &catalog()) {
        Ok(bound) => bound.query,
        Err(e) => panic!("{sql}\n  did not bind: {e}"),
    }
}

fn refusal(sql: &str) -> SqlError {
    match bind_sql(sql, &catalog()) {
        Ok(bound) => panic!("{sql}\n  was accepted as {:?}", bound.query),
        Err(e) => e,
    }
}

// ---- S-11: names -------------------------------------------------------------------------------

/// `AS` names an output column; a bare column reference names itself; nothing else has a name.
#[test]
fn s11_names_come_from_as_or_from_a_bare_column_reference() {
    assert_eq!(
        bound("SELECT t.n AS total FROM t"),
        Query::from(Source::scan("t", "t")).project(vec![Named::new("total", Expr::column("t.n"))])
    );
    assert_eq!(
        bound("SELECT t.n FROM t"),
        Query::from(Source::scan("t", "t")).project(vec![Named::new("n", Expr::column("t.n"))]),
        "a bare column reference is named by its column, without the qualifier"
    );
    assert_eq!(
        refusal("SELECT t.n + 1 FROM t"),
        SqlError::MissingOutputName("t.n + 1".to_owned()),
        "a computed column has no name of its own, and no name is invented for it"
    );
    assert_eq!(
        refusal("SELECT (t.n) FROM t"),
        SqlError::MissingOutputName("(t.n)".to_owned()),
        "parentheses are not peeled to find a name: the rule must not depend on how many were typed"
    );
}

/// `SELECT *` is refused, and the refusal explains why a *standing* query cannot have one.
#[test]
fn s11_select_star_is_refused_because_a_standing_query_fixes_its_schema() {
    let error = refusal("SELECT * FROM t");
    assert_eq!(error, SqlError::SelectStarNotSupported);
    let message = error.to_string();
    assert!(message.contains("standing query"), "{message}");
    assert_eq!(
        refusal("SELECT t.* FROM t"),
        SqlError::SelectStarNotSupported
    );
}

/// Identifiers are verbatim and case-sensitive, quoted or not (S-11).
#[test]
fn s11_identifiers_are_verbatim() {
    assert_eq!(
        bound("SELECT odd.A AS upper, odd.a AS lower FROM odd"),
        Query::from(Source::scan("odd", "odd")).project(vec![
            Named::new("upper", Expr::column("odd.A")),
            Named::new("lower", Expr::column("odd.a")),
        ]),
        "two columns differing only in case are two columns"
    );
    assert_eq!(
        bound("SELECT odd.\"count\" AS c FROM odd"),
        Query::from(Source::scan("odd", "odd"))
            .project(vec![Named::new("c", Expr::column("odd.count"))]),
        "quoting changes which characters are legal, not what the name is"
    );
    // No folding, so a name that differs in case is a name that is not there.
    assert!(matches!(
        refusal("SELECT ODD.A AS x FROM odd"),
        SqlError::Plan(PlanError::UnknownColumn { .. })
    ));
}

/// A duplicate output name is refused by the same rule the typed API is (S-11).
#[test]
fn s11_duplicate_output_names_are_refused() {
    assert_eq!(
        refusal("SELECT t.n AS x, t.id AS x FROM t"),
        SqlError::Plan(PlanError::DuplicateOutputName("x".to_owned()))
    );
}

// ---- S-19: nulls and casts ---------------------------------------------------------------------

#[test]
fn s19_a_null_is_written_with_its_type() {
    assert_eq!(
        bound("SELECT CAST(NULL AS BIGINT) AS z FROM t"),
        Query::from(Source::scan("t", "t"))
            .project(vec![Named::new("z", Expr::Null(DataType::Int64))])
    );
    assert_eq!(
        refusal("SELECT NULL AS z FROM t"),
        SqlError::Plan(PlanError::UntypedNullLiteral)
    );
    // A cast that converts is the implicit conversion S-19 forbids, written out loud.
    assert!(matches!(
        refusal("SELECT CAST(t.n AS TEXT) AS z FROM t"),
        SqlError::UnsupportedCast(_)
    ));
    assert_eq!(
        refusal("SELECT CAST(NULL AS DOUBLE) AS z FROM t"),
        SqlError::UnsupportedTypeName("DOUBLE".to_owned()),
        "Float64 is a result type, so there is no way to write one (S-3)"
    );
}

#[test]
fn s19_typing_is_exact_through_the_sql_door_too() {
    assert!(matches!(
        refusal("SELECT t.n AS x FROM t WHERE t.n = t.s"),
        SqlError::Plan(PlanError::TypeMismatch { .. })
    ));
    assert!(matches!(
        refusal("SELECT t.n AS x FROM t WHERE t.n"),
        SqlError::Plan(PlanError::ExpectedBoolean { .. })
    ));
}

// ---- S-27, S-33: grouping ----------------------------------------------------------------------

/// The canonical grouped query, with no projection emitted (S-36).
#[test]
fn s27_a_group_by_binds_to_keys_then_aggregates_with_no_projection() {
    let query = bound("SELECT t.id AS id, COUNT(*) AS n FROM t GROUP BY t.id");
    assert_eq!(
        query,
        Query::from(Source::scan("t", "t")).group_by(GroupBy {
            keys: vec![Named::new("id", Expr::column("t.id"))],
            aggregates: vec![Named::new("n", AggFunc::CountStar)],
            having: None,
        }),
        "the select list is already the group output, so no projection is emitted (S-36)"
    );
    assert!(query.project.is_none());
}

/// A group key not named in the select list takes its derived name (S-11).
#[test]
fn s27_a_key_absent_from_the_select_list_still_gets_a_name() {
    let query = bound("SELECT COUNT(*) AS n FROM t GROUP BY t.id");
    assert_eq!(
        query.group_by.as_ref().map(|g| g.keys.clone()),
        Some(vec![Named::new("id", Expr::column("t.id"))])
    );
    assert!(
        query.project.is_some(),
        "the select list narrows the group output to just the aggregate, so a projection is \
         emitted (S-36)"
    );
    assert_eq!(
        query.project,
        Some(vec![Named::new("n", Expr::column("n"))])
    );
}

/// Reordering the select list is a different query, and the projection says so (S-36, S-8).
#[test]
fn s36_reordering_the_select_list_emits_a_projection() {
    let query = bound("SELECT COUNT(*) AS n, t.id AS id FROM t GROUP BY t.id");
    assert_eq!(
        query.project,
        Some(vec![
            Named::new("n", Expr::column("n")),
            Named::new("id", Expr::column("id")),
        ])
    );
}

/// Two aliases for one grouping expression both read the key that carries the first alias.
#[test]
fn s36_two_aliases_for_one_key_read_the_same_column() {
    let query = bound("SELECT t.id AS x, t.id AS y, COUNT(*) AS n FROM t GROUP BY t.id");
    assert_eq!(
        query.group_by.as_ref().map(|g| g.keys.clone()),
        Some(vec![Named::new("x", Expr::column("t.id"))]),
        "the key takes the first alias that asked for it"
    );
    assert_eq!(
        query.project,
        Some(vec![
            Named::new("x", Expr::column("x")),
            Named::new("y", Expr::column("x")),
            Named::new("n", Expr::column("n")),
        ]),
        "`y` reads the key, under the key's name"
    );
}

/// A grand total: no GROUP BY clause at all (S-33, D-20).
#[test]
fn s33_an_aggregate_with_no_group_by_is_the_grand_total() {
    assert_eq!(
        bound("SELECT COUNT(*) AS n, SUM(t.n) AS s FROM t"),
        Query::from(Source::scan("t", "t")).group_by(GroupBy {
            keys: vec![],
            aggregates: vec![
                Named::new("n", AggFunc::CountStar),
                Named::new("s", AggFunc::Sum(Expr::column("t.n"))),
            ],
            having: None,
        })
    );
}

/// **The grand total over an empty input, on the engine side of the SQL door.**
///
/// The oracle's side of this is `schweep-oracle/tests/semantics.rs`; this is the same claim through
/// SQL text, at epoch 0, before anything has been inserted (S-33, D-20).
#[test]
fn s33_the_grand_total_answers_before_any_epoch_is_sealed() {
    let plan = match compile("SELECT COUNT(*) AS n, MIN(t.n) AS lo FROM t", &catalog()) {
        Ok(plan) => plan,
        Err(e) => panic!("did not compile: {e}"),
    };
    let circuit = match schweep_sql::instantiate(&plan) {
        Ok(circuit) => circuit,
        Err(e) => panic!("did not instantiate: {e}"),
    };
    let answer = match circuit.answer() {
        Ok(answer) => answer,
        Err(e) => panic!("no answer: {e}"),
    };
    assert_eq!(
        answer.render(),
        "(n: Int64, lo: Int64)\n(0, NULL) => 1\n",
        "one row, always: COUNT is 0 and MIN is NULL over an empty group (S-30, S-33)"
    );
}

/// `HAVING` with no `GROUP BY` and no aggregate computes nothing, and says so.
#[test]
fn s33_a_having_with_nothing_to_aggregate_is_refused() {
    // A `HAVING` makes the query group (S-32), and a query that groups by nothing and aggregates
    // nothing has no group for `id` to belong to. Both refusals would be true; this is the one that
    // names the column the person actually wrote.
    assert_eq!(
        refusal("SELECT t.id AS id FROM t HAVING t.id > 1"),
        SqlError::Plan(PlanError::ColumnNotGrouped {
            name: "id".to_owned()
        })
    );
    // With nothing in the select list to belong to a group either, the refusal is the one that says
    // the GROUP BY computes nothing.
    assert_eq!(
        refusal("SELECT 1 AS one FROM t HAVING TRUE"),
        SqlError::Plan(PlanError::ColumnNotGrouped {
            name: "one".to_owned()
        })
    );
}

/// A select item that is neither grouped nor aggregated belongs to no group (S-33).
#[test]
fn s33_a_column_outside_the_grouping_is_refused() {
    assert_eq!(
        refusal("SELECT t.n AS n, COUNT(*) AS c FROM t GROUP BY t.id"),
        SqlError::Plan(PlanError::ColumnNotGrouped {
            name: "n".to_owned()
        })
    );
    // The workaround is to group by the expression itself, and it binds.
    let query = bound("SELECT t.n + 1 AS x, COUNT(*) AS c FROM t GROUP BY t.n + 1");
    assert_eq!(
        query.group_by.as_ref().map(|g| g.keys.clone()),
        Some(vec![Named::new(
            "x",
            Expr::binary(BinOp::Add, Expr::column("t.n"), Expr::int(1))
        )])
    );
}

// ---- S-32: aggregates in the wrong place -------------------------------------------------------

#[test]
fn s32_each_misplaced_aggregate_has_its_own_refusal() {
    assert_eq!(
        refusal("SELECT t.id AS id, COUNT(*) AS n FROM t GROUP BY t.id HAVING COUNT(*) > 1"),
        SqlError::Plan(PlanError::AggregateInHaving {
            func: "COUNT(*)".to_owned()
        })
    );
    assert_eq!(
        refusal("SELECT COUNT(*) AS n FROM t WHERE COUNT(*) > 1"),
        SqlError::Plan(PlanError::AggregateInWhere {
            func: "COUNT(*)".to_owned()
        })
    );
    assert_eq!(
        refusal("SELECT SUM(COUNT(t.n)) AS n FROM t"),
        SqlError::Plan(PlanError::NestedAggregate {
            outer: "SUM".to_owned(),
            inner: "COUNT(t.n)".to_owned()
        })
    );
    assert_eq!(
        refusal("SELECT COUNT(*) + 1 AS n FROM t"),
        SqlError::Plan(PlanError::AggregateNotTopLevel {
            func: "COUNT(*)".to_owned()
        })
    );
}

/// `HAVING` over a declared aggregate output is the accepted form, and the refusal above names it.
#[test]
fn s32_having_references_an_aggregate_by_its_declared_name() {
    let query = bound("SELECT t.id AS id, COUNT(*) AS n FROM t GROUP BY t.id HAVING n > 1");
    assert_eq!(
        query.group_by.and_then(|g| g.having),
        Some(Expr::binary(BinOp::Gt, Expr::column("n"), Expr::int(1)))
    );
}

// ---- joins -------------------------------------------------------------------------------------

#[test]
fn s26_an_inner_equi_join_binds_and_everything_else_is_refused_by_name() {
    assert_eq!(
        bound("SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id"),
        Query::from(Source::join(
            Source::scan("t", "t"),
            Source::scan("u", "u"),
            vec![("t.id".to_owned(), "u.id".to_owned())]
        ))
        .project(vec![
            Named::new("n", Expr::column("t.n")),
            Named::new("m", Expr::column("u.m")),
        ])
    );
    assert_eq!(
        bound("SELECT t.n AS n FROM t JOIN u ON u.id = t.id"),
        Query::from(Source::join(
            Source::scan("t", "t"),
            Source::scan("u", "u"),
            vec![("t.id".to_owned(), "u.id".to_owned())]
        ))
        .project(vec![Named::new("n", Expr::column("t.n"))]),
        "`ON u.id = t.id` is the same join written the other way round"
    );
    for (sql, construct) in [
        (
            "SELECT t.n AS n FROM t LEFT JOIN u ON t.id = u.id",
            "LEFT JOIN",
        ),
        ("SELECT t.n AS n FROM t CROSS JOIN u", "CROSS JOIN"),
        ("SELECT t.n AS n FROM t JOIN u USING (id)", "JOIN USING"),
        ("SELECT t.n AS n FROM t NATURAL JOIN u", "NATURAL JOIN"),
        ("SELECT t.n AS n FROM t, u", "comma join"),
    ] {
        let message = refusal(sql).to_string();
        assert!(
            message.contains(construct),
            "{sql}\n  was refused as {message:?}, which does not name {construct:?}"
        );
    }
    assert!(matches!(
        refusal("SELECT t.n AS n FROM t JOIN u ON t.id > u.id"),
        SqlError::NotAnEquiJoin(_)
    ));
    assert!(matches!(
        refusal("SELECT t.n AS n FROM t JOIN u ON t.id = t.n"),
        SqlError::JoinKeysOnOneSide(_, _)
    ));
}

/// A self-join needs aliases, and they are what the columns are qualified by (S-10, S-26).
#[test]
fn a_self_join_is_two_scans_of_one_table() {
    assert_eq!(
        bound("SELECT a.n AS an, b.n AS bn FROM t AS a JOIN t AS b ON a.id = b.id"),
        Query::from(Source::join(
            Source::scan("t", "a"),
            Source::scan("t", "b"),
            vec![("a.id".to_owned(), "b.id".to_owned())]
        ))
        .project(vec![
            Named::new("an", Expr::column("a.n")),
            Named::new("bn", Expr::column("b.n")),
        ])
    );
    assert_eq!(
        refusal("SELECT t.n AS n FROM t JOIN t ON t.id = t.id"),
        SqlError::Plan(PlanError::DuplicateAlias("t".to_owned())),
        "without aliases the two scans have one name, and the binder says which rule that breaks"
    );
}

// ---- I-6 on curated pairs ----------------------------------------------------------------------

/// I-6 on pairs written by hand, including shapes the fuzzer's renderer declines.
///
/// The fuzzer compares thousands of plans, but only over the shapes its renderer can write. These
/// are the ones it cannot: a grand total, `DISTINCT` over a grouped query, and a `HAVING`.
#[test]
fn i6_hand_written_pairs_compile_to_identical_plans() {
    let catalog = catalog();
    let cases: Vec<(&str, Query)> = vec![
        (
            "SELECT COUNT(*) AS n FROM t",
            Query::from(Source::scan("t", "t")).group_by(GroupBy {
                keys: vec![],
                aggregates: vec![Named::new("n", AggFunc::CountStar)],
                having: None,
            }),
        ),
        (
            "SELECT DISTINCT t.id AS id, MAX(t.s) AS hi FROM t GROUP BY t.id HAVING hi IS NOT NULL",
            Query::from(Source::scan("t", "t"))
                .group_by(GroupBy {
                    keys: vec![Named::new("id", Expr::column("t.id"))],
                    aggregates: vec![Named::new("hi", AggFunc::Max(Expr::column("t.s")))],
                    having: Some(Expr::is_not_null(Expr::column("hi"))),
                })
                .distinct(),
        ),
        (
            "SELECT CASE WHEN t.n IS NULL THEN 'none' ELSE t.s END AS label \
             FROM t WHERE NOT (t.b) OR t.id = 3",
            Query::from(Source::scan("t", "t"))
                .filter(Expr::or(
                    !Expr::column("t.b"),
                    Expr::binary(BinOp::Eq, Expr::column("t.id"), Expr::int(3)),
                ))
                .project(vec![Named::new(
                    "label",
                    Expr::Case {
                        whens: vec![(Expr::is_null(Expr::column("t.n")), Expr::string("none"))],
                        otherwise: Some(Box::new(Expr::column("t.s"))),
                    },
                )]),
        ),
    ];

    for (sql, typed) in cases {
        assert_eq!(bound(sql), typed, "{sql}\n  bound to a different query");
        let sql_plan = match compile(sql, &catalog) {
            Ok(plan) => plan,
            Err(e) => panic!("{sql}\n  did not compile: {e}"),
        };
        let typed_plan = match incrementalize_typed(&typed, &catalog) {
            Ok(plan) => plan,
            Err(e) => panic!("the typed twin of {sql} did not compile: {e}"),
        };
        assert_eq!(
            sql_plan.structural_form(),
            typed_plan.structural_form(),
            "{sql}\n  compiled to a different plan than its typed twin (I-6)"
        );
        assert_eq!(sql_plan.structural_hash(), typed_plan.structural_hash());
    }
}

/// The plan's shape follows the query's shape: one node per stage, in pipeline order (§5.6).
#[test]
fn the_plan_has_one_node_per_stage_in_pipeline_order() {
    let plan = match compile(
        "SELECT DISTINCT t.id AS id, COUNT(*) AS n FROM t JOIN u ON t.id = u.id \
         WHERE t.n > 1 GROUP BY t.id HAVING n > 2",
        &catalog(),
    ) {
        Ok(plan) => plan,
        Err(e) => panic!("did not compile: {e}"),
    };
    assert_eq!(
        plan.structural_form(),
        "\
(distinct)
  (filter unqualified (> #n 2))
    (aggregate keys [(id #t.id)] aggs [(n COUNT(*))] (id: Int64, n: Int64))
      (filter qualified (> #t.n 1))
        (join [(0 0)] (t.id: Int64 NOT NULL, t.n: Int64, t.s: Utf8, t.b: Boolean, u.id: Int64 NOT NULL, u.m: Int64))
          (source t as t (t.id: Int64 NOT NULL, t.n: Int64, t.s: Utf8, t.b: Boolean))
          (source u as u (u.id: Int64 NOT NULL, u.m: Int64))
",
        "no optimisation, no reordering, no invented nodes"
    );
    let rules: Vec<schweep_sql::Rule> = plan.nodes().iter().map(|n| n.rule()).collect();
    assert_eq!(
        rules,
        vec![
            schweep_sql::Rule::Input,
            schweep_sql::Rule::Input,
            schweep_sql::Rule::Bilinear,
            schweep_sql::Rule::Linear,
            schweep_sql::Rule::StatefulPerGroup,
            schweep_sql::Rule::Linear,
            schweep_sql::Rule::StatefulPerRow,
        ],
        "each node carries the DBSP rule that justifies its incremental form (§5.6)"
    );
}

/// Naming follows the pipeline: qualified before a GROUP BY, unqualified after one (S-10, S-27).
#[test]
fn naming_switches_at_the_group_by() {
    let plan = match compile(
        "SELECT t.id AS id, COUNT(*) AS n FROM t WHERE t.n > 1 GROUP BY t.id HAVING n > 2",
        &catalog(),
    ) {
        Ok(plan) => plan,
        Err(e) => panic!("did not compile: {e}"),
    };
    let namings: Vec<Naming> = plan
        .nodes()
        .iter()
        .filter_map(|node| match node {
            schweep_sql::CircuitNode::Filter { naming, .. } => Some(*naming),
            _ => None,
        })
        .collect();
    assert_eq!(
        namings,
        vec![Naming::Qualified, Naming::Unqualified],
        "WHERE sees `t.n`; HAVING sees `n`"
    );
}
