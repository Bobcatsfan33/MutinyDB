//! **Every refusal names its construct** (S-12, S-35) — the exhaustive table.
//!
//! §6 C5's exit gate asks for exactly this, and it is a table rather than prose because the property
//! is not "the binder rejects things", it is "the binder says *which* thing". A person handed
//! `unsupported query` learns nothing; a person handed `LEFT JOIN is not in the v1 dialect` knows
//! what to change and can look the construct up in `docs/SEMANTICS.md` §8.
//!
//! Each row is a piece of SQL that must be refused, and a substring the refusal must contain. The
//! substring is the construct's name as a person would say it.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use schweep_plan::bind::Catalog;
use schweep_sql::compile;
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
    Catalog::from([("t".to_owned(), t), ("u".to_owned(), u)])
}

/// Everything SQL has that this dialect does not, each refused by its own name.
#[test]
fn every_construct_outside_the_dialect_is_refused_by_name() {
    let cases: &[(&str, &str)] = &[
        // ---- query-level clauses (§8) ----
        ("SELECT t.n AS n FROM t ORDER BY t.n", "ORDER BY"),
        ("SELECT t.n AS n FROM t LIMIT 5", "LIMIT"),
        ("SELECT t.n AS n FROM t FETCH FIRST 5 ROWS ONLY", "FETCH"),
        (
            "WITH x AS (SELECT t.n AS n FROM t) SELECT x.n AS n FROM x",
            "WITH",
        ),
        (
            "SELECT t.n AS n FROM t UNION ALL SELECT u.m AS n FROM u",
            "UNION",
        ),
        (
            "SELECT t.n AS n FROM t EXCEPT SELECT u.m AS n FROM u",
            "EXCEPT",
        ),
        ("SELECT t.n AS n FROM t FOR UPDATE", "FOR UPDATE"),
        ("VALUES (1)", "VALUES"),
        // ---- SELECT-level clauses ----
        ("SELECT TOP 2 t.n AS n FROM t", "TOP"),
        ("SELECT DISTINCT ON (t.n) t.n AS n FROM t", "DISTINCT ON"),
        ("SELECT t.n AS n FROM t QUALIFY t.n > 1", "QUALIFY"),
        ("SELECT t.n AS n FROM t CLUSTER BY t.n", "CLUSTER BY"),
        ("SELECT t.n AS n FROM t SORT BY t.n", "SORT BY"),
        ("SELECT t.n AS n FROM t GROUP BY ALL", "GROUP BY ALL"),
        (
            "SELECT t.n AS n, COUNT(*) AS c FROM t GROUP BY ROLLUP (t.n)",
            "ROLLUP / CUBE / GROUPING SETS",
        ),
        // ---- FROM and joins (S-26) ----
        (
            "SELECT t.n AS n FROM t LEFT JOIN u ON t.id = u.id",
            "LEFT JOIN",
        ),
        (
            "SELECT t.n AS n FROM t RIGHT JOIN u ON t.id = u.id",
            "RIGHT JOIN",
        ),
        (
            "SELECT t.n AS n FROM t FULL OUTER JOIN u ON t.id = u.id",
            "FULL OUTER JOIN",
        ),
        ("SELECT t.n AS n FROM t CROSS JOIN u", "CROSS JOIN"),
        ("SELECT t.n AS n FROM t NATURAL JOIN u", "NATURAL JOIN"),
        ("SELECT t.n AS n FROM t JOIN u USING (id)", "JOIN USING"),
        ("SELECT t.n AS n FROM t, u", "comma join"),
        (
            "SELECT x.n AS n FROM (SELECT t.n AS n FROM t) AS x",
            "derived table",
        ),
        ("SELECT t.n AS n FROM t AS t (a, b, c, d)", "column aliases"),
        // ---- expressions (S-13 … S-19) ----
        ("SELECT t.s AS s FROM t WHERE t.s LIKE 'a%'", "LIKE"),
        (
            "SELECT t.n AS n FROM t WHERE t.n BETWEEN 1 AND 2",
            "BETWEEN",
        ),
        ("SELECT t.n AS n FROM t WHERE t.n IN (1, 2)", "IN"),
        (
            "SELECT t.n AS n FROM t WHERE t.n IN (SELECT u.m FROM u)",
            "subquery",
        ),
        (
            "SELECT t.n AS n FROM t WHERE EXISTS (SELECT u.m AS m FROM u)",
            "subquery",
        ),
        ("SELECT t.b AS b FROM t WHERE t.b IS TRUE", "IS TRUE"),
        (
            "SELECT t.n AS n FROM t WHERE t.n IS DISTINCT FROM 1",
            "IS [NOT] DISTINCT FROM",
        ),
        ("SELECT (t.s || t.s) AS s FROM t", "||"),
        ("SELECT (-t.n) AS n FROM t", "unary minus"),
        ("SELECT LOWER(t.s) AS s FROM t", "LOWER(t.s)"),
        (
            "SELECT CASE t.n WHEN 1 THEN 2 END AS x FROM t",
            "CASE with an operand",
        ),
        ("SELECT 1.5 AS x FROM t", "FLOAT literal"),
        ("SELECT ? AS x FROM t", "bind placeholder"),
        ("SELECT t.s COLLATE \"C\" AS s FROM t", "COLLATE"),
        // ---- aggregates (S-30, S-32) ----
        (
            "SELECT COUNT(DISTINCT t.n) AS n FROM t",
            "DISTINCT inside an aggregate",
        ),
        (
            "SELECT COUNT(t.n) FILTER (WHERE t.n > 1) AS n FROM t",
            "FILTER on an aggregate",
        ),
        ("SELECT SUM(t.n) OVER () AS n FROM t", "window function"),
        (
            "SELECT t.id AS id, COUNT(*) AS n FROM t GROUP BY t.id HAVING COUNT(*) > 1",
            "HAVING is evaluated after aggregation",
        ),
        (
            "SELECT COUNT(*) AS n FROM t WHERE COUNT(*) > 1",
            "WHERE is evaluated before aggregation",
        ),
        (
            "SELECT SUM(COUNT(t.n)) AS n FROM t",
            "cannot take the aggregate",
        ),
        (
            "SELECT COUNT(*) * 2 AS n FROM t",
            "must be the whole output column",
        ),
        // ---- names and nulls (S-11, S-19) ----
        ("SELECT * FROM t", "SELECT *"),
        ("SELECT t.* FROM t", "SELECT *"),
        ("SELECT t.n + 1 FROM t", "has no name"),
        ("SELECT NULL AS z FROM t", "untyped NULL"),
        ("SELECT CAST(t.n AS TEXT) AS z FROM t", "CAST"),
        ("SELECT CAST(NULL AS DOUBLE) AS z FROM t", "type name"),
        // ---- statements that are not queries (D-1) ----
        ("INSERT INTO t VALUES (1, 2, 'a', TRUE)", "INSERT"),
        ("UPDATE t SET n = 1", "UPDATE"),
        ("DELETE FROM t", "DELETE"),
        ("CREATE TABLE v (a BIGINT)", "CREATE TABLE"),
        ("EXPLAIN SELECT t.n AS n FROM t", "EXPLAIN"),
        // ---- the catalog ----
        ("SELECT v.n AS n FROM v", "no table named"),
        ("SELECT t.zzz AS n FROM t", "no column named"),
        ("SELECT public.t.n AS n FROM public.t", "qualified"),
        ("SELECT FROM t", "no output columns"),
    ];

    let catalog = catalog();
    let mut anonymous: Vec<String> = Vec::new();
    for (sql, construct) in cases {
        match compile(sql, &catalog) {
            Ok(plan) => panic!(
                "{sql}\n  was accepted, compiling to\n{}",
                plan.structural_form()
            ),
            Err(error) => {
                let message = error.to_string();
                if !message.contains(construct) {
                    anonymous.push(format!(
                        "  {sql}\n    refused as: {message}\n    expected to name: {construct}"
                    ));
                }
            }
        }
    }
    assert!(
        anonymous.is_empty(),
        "{} refusals did not name their construct:\n{}",
        anonymous.len(),
        anonymous.join("\n")
    );
    println!(
        "{} constructs outside the dialect, each refused by name",
        cases.len()
    );
}

/// The dialect's own surface, accepted. The table above proves what is refused; this proves the
/// refusals are not simply "everything".
#[test]
fn the_dialect_itself_is_accepted() {
    let catalog = catalog();
    let cases = [
        "SELECT t.n AS n FROM t",
        "SELECT t.n AS n FROM t WHERE t.n > 1",
        "SELECT t.n AS n FROM t WHERE t.n IS NULL OR NOT (t.b)",
        "SELECT DISTINCT t.n AS n FROM t",
        "SELECT t.n AS n, u.m AS m FROM t JOIN u ON t.id = u.id",
        "SELECT a.n AS an, b.n AS bn FROM t AS a JOIN t AS b ON a.id = b.id",
        "SELECT t.id AS id, COUNT(*) AS c FROM t GROUP BY t.id",
        "SELECT t.id AS id, SUM(t.n) AS s, AVG(t.n) AS a, MIN(t.s) AS lo, MAX(t.b) AS hi \
         FROM t GROUP BY t.id",
        "SELECT COUNT(*) AS c FROM t",
        "SELECT t.id AS id, COUNT(*) AS c FROM t GROUP BY t.id HAVING c > 1",
        "SELECT CASE WHEN t.n > 1 THEN 'big' ELSE 'small' END AS size FROM t",
        "SELECT CAST(NULL AS BIGINT) AS z, t.n AS n FROM t",
        "SELECT t.n AS n FROM t WHERE t.n = -1",
        "select t.n as n from t where t.n > 1",
    ];
    for sql in cases {
        if let Err(e) = compile(sql, &catalog) {
            panic!("{sql}\n  was refused: {e}");
        }
    }
    println!("{} queries inside the dialect, all accepted", cases.len());
}
