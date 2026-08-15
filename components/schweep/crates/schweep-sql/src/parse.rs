//! Text → AST, and then straight to the one AST shape this dialect has (`docs/SEMANTICS.md` S-35).
//!
//! sqlparser is a **parser**, not a specification. It accepts ClickHouse's `PREWHERE`, Snowflake's
//! `QUALIFY`, Hive's `SORT BY`, MSSQL's `TOP`, and dozens of other constructs, none of which mean
//! anything here. So this module's job is not to translate — it is to **refuse**, by name, every
//! field of the AST that is set and should not be.
//!
//! That is done exhaustively and by hand rather than by matching only the fields we want, because
//! the failure mode of the alternative is silence: a query with a clause nobody looked at would run
//! as though the clause were not there, and answer a question the person did not ask. Silently
//! ignoring `LIMIT 10` is worse than refusing it.

use sqlparser::ast::{
    Distinct, GroupByExpr, Query as AstQuery, Select, SelectFlavor, SetExpr, Statement,
};
use sqlparser::dialect::GenericDialect;
use sqlparser::parser::Parser;

use crate::error::{Result, SqlError};

/// The one query shape the dialect has (S-9), with every other AST field already proved absent.
///
/// Holding borrowed references keeps the AST as the single owner of the text's structure; there is
/// no second copy to drift.
#[derive(Debug)]
pub struct SelectStatement<'a> {
    pub select: &'a Select,
    /// `SELECT DISTINCT` (S-34).
    pub distinct: bool,
}

/// Parse one SELECT statement and refuse everything around it.
pub fn parse(sql: &str) -> Result<AstQuery> {
    let dialect = GenericDialect {};
    let mut statements =
        Parser::parse_sql(&dialect, sql).map_err(|e| SqlError::Parse(e.to_string()))?;
    if statements.len() != 1 {
        return Err(SqlError::NotOneStatement {
            found: statements.len(),
        });
    }
    match statements.pop() {
        Some(Statement::Query(query)) => Ok(*query),
        // Every other statement kind is a write, a DDL, or a session command. D-1 makes Current a
        // read surface over a log it does not own, so there is no INSERT to support, and saying so
        // is more useful than "unsupported statement".
        Some(Statement::Insert(_)) => Err(SqlError::NotAQuery("INSERT")),
        Some(Statement::Update { .. }) => Err(SqlError::NotAQuery("UPDATE")),
        Some(Statement::Delete(_)) => Err(SqlError::NotAQuery("DELETE")),
        Some(Statement::CreateTable(_)) => Err(SqlError::NotAQuery("CREATE TABLE")),
        Some(Statement::CreateView { .. }) => Err(SqlError::NotAQuery("CREATE VIEW")),
        Some(Statement::Explain { .. }) => Err(SqlError::NotAQuery("EXPLAIN")),
        Some(_) => Err(SqlError::NotAQuery("that statement")),
        // `parse_statements` returned exactly one statement, checked above.
        None => Err(SqlError::NotOneStatement { found: 0 }),
    }
}

/// Reduce a parsed query to its `SELECT`, refusing every clause the dialect does not have.
pub fn select_of(query: &AstQuery) -> Result<SelectStatement<'_>> {
    let AstQuery {
        with,
        body,
        order_by,
        limit_clause,
        fetch,
        locks,
        for_clause,
        settings,
        format_clause,
        pipe_operators,
    } = query;

    // Destructured above so that a new sqlparser field is a *compile* error here rather than a
    // clause that silently stops being checked. That is the point of listing them all.
    if with.is_some() {
        return Err(SqlError::NotInDialect("WITH (common table expressions)"));
    }
    if order_by.is_some() {
        // Ordering is a read-time concern, not a maintained-answer concern (D-7): a standing
        // computation maintains a *set*, and the order is chosen when someone reads it.
        return Err(SqlError::NotInDialect("ORDER BY"));
    }
    if limit_clause.is_some() {
        return Err(SqlError::NotInDialect("LIMIT"));
    }
    if fetch.is_some() {
        return Err(SqlError::NotInDialect("FETCH"));
    }
    if !locks.is_empty() {
        return Err(SqlError::NotInDialect("FOR UPDATE / FOR SHARE"));
    }
    if for_clause.is_some() {
        return Err(SqlError::NotInDialect("FOR XML / FOR JSON"));
    }
    if settings.is_some() {
        return Err(SqlError::NotInDialect("SETTINGS"));
    }
    if format_clause.is_some() {
        return Err(SqlError::NotInDialect("FORMAT"));
    }
    if !pipe_operators.is_empty() {
        return Err(SqlError::NotInDialect("pipe operators"));
    }

    let select = match body.as_ref() {
        SetExpr::Select(select) => select.as_ref(),
        SetExpr::SetOperation { .. } => {
            return Err(SqlError::NotInDialect("UNION / EXCEPT / INTERSECT"))
        }
        SetExpr::Query(_) => return Err(SqlError::NotInDialect("a parenthesised subquery")),
        SetExpr::Values(_) => return Err(SqlError::NotInDialect("VALUES")),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_) => {
            return Err(SqlError::NotAQuery("a write inside a query"))
        }
        SetExpr::Table(_) => return Err(SqlError::NotInDialect("TABLE")),
    };
    let distinct = check_select_clauses(select)?;
    Ok(SelectStatement { select, distinct })
}

/// Refuse every `Select` field outside the dialect, and return whether `DISTINCT` was asked for.
fn check_select_clauses(select: &Select) -> Result<bool> {
    let Select {
        select_token: _,
        optimizer_hints,
        distinct,
        select_modifiers,
        top,
        top_before_distinct: _,
        projection: _,
        exclude,
        into,
        from: _,
        lateral_views,
        prewhere,
        selection: _,
        connect_by,
        group_by,
        cluster_by,
        distribute_by,
        sort_by,
        having: _,
        named_window,
        qualify,
        window_before_qualify: _,
        value_table_mode,
        flavor,
    } = select;

    if !optimizer_hints.is_empty() {
        return Err(SqlError::NotInDialect("optimizer hints"));
    }
    if select_modifiers.is_some() {
        return Err(SqlError::NotInDialect("MySQL SELECT modifiers"));
    }
    if top.is_some() {
        return Err(SqlError::NotInDialect("TOP"));
    }
    if exclude.is_some() {
        return Err(SqlError::NotInDialect("EXCLUDE"));
    }
    if into.is_some() {
        return Err(SqlError::NotInDialect("SELECT INTO"));
    }
    if !lateral_views.is_empty() {
        return Err(SqlError::NotInDialect("LATERAL VIEW"));
    }
    if prewhere.is_some() {
        return Err(SqlError::NotInDialect("PREWHERE"));
    }
    if !connect_by.is_empty() {
        return Err(SqlError::NotInDialect("CONNECT BY"));
    }
    if !cluster_by.is_empty() {
        return Err(SqlError::NotInDialect("CLUSTER BY"));
    }
    if !distribute_by.is_empty() {
        return Err(SqlError::NotInDialect("DISTRIBUTE BY"));
    }
    if !sort_by.is_empty() {
        return Err(SqlError::NotInDialect("SORT BY"));
    }
    if !named_window.is_empty() {
        return Err(SqlError::NotInDialect("WINDOW"));
    }
    if qualify.is_some() {
        return Err(SqlError::NotInDialect("QUALIFY"));
    }
    if value_table_mode.is_some() {
        return Err(SqlError::NotInDialect("SELECT AS VALUE / AS STRUCT"));
    }
    if *flavor != SelectFlavor::Standard {
        return Err(SqlError::NotInDialect("a FROM-first SELECT"));
    }

    // GROUP BY's *contents* are the binder's business; its modifiers are the dialect's.
    match group_by {
        GroupByExpr::All(_) => return Err(SqlError::NotInDialect("GROUP BY ALL")),
        GroupByExpr::Expressions(_, modifiers) if !modifiers.is_empty() => {
            return Err(SqlError::NotInDialect("ROLLUP / CUBE / GROUPING SETS"))
        }
        GroupByExpr::Expressions(_, _) => {}
    }

    match distinct {
        None => Ok(false),
        Some(Distinct::All) => Ok(false),
        Some(Distinct::Distinct) => Ok(true),
        Some(Distinct::On(_)) => Err(SqlError::NotInDialect("DISTINCT ON")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn refusal(sql: &str) -> SqlError {
        let query = match parse(sql) {
            Ok(query) => query,
            Err(e) => return e,
        };
        match select_of(&query) {
            Ok(_) => panic!("{sql} was accepted"),
            Err(e) => e,
        }
    }

    #[test]
    fn a_plain_select_survives_the_clause_sweep() {
        let query = parse("SELECT t.a AS a FROM t WHERE t.a = 1").unwrap();
        let statement = select_of(&query).unwrap();
        assert!(!statement.distinct);
        assert_eq!(statement.select.projection.len(), 1);
    }

    #[test]
    fn distinct_is_read_from_the_select() {
        let query = parse("SELECT DISTINCT t.a AS a FROM t").unwrap();
        assert!(select_of(&query).unwrap().distinct);
    }

    /// Each refusal names its construct, and the name is the point (S-12, S-35).
    #[test]
    fn every_clause_outside_the_dialect_is_refused_by_name() {
        let cases = [
            ("SELECT t.a AS a FROM t ORDER BY t.a", "ORDER BY"),
            ("SELECT t.a AS a FROM t LIMIT 1", "LIMIT"),
            ("WITH x AS (SELECT 1 AS a) SELECT x.a AS a FROM x", "WITH"),
            (
                "SELECT t.a AS a FROM t UNION ALL SELECT u.a AS a FROM u",
                "UNION",
            ),
            ("SELECT t.a AS a FROM t GROUP BY ALL", "GROUP BY ALL"),
            ("SELECT DISTINCT ON (t.a) t.a AS a FROM t", "DISTINCT ON"),
            ("SELECT t.a AS a FROM t QUALIFY t.a > 1", "QUALIFY"),
            ("SELECT TOP 3 t.a AS a FROM t", "TOP"),
            ("VALUES (1)", "VALUES"),
        ];
        for (sql, construct) in cases {
            let error = refusal(sql);
            let message = error.to_string();
            assert!(
                message.contains(construct),
                "{sql} was refused as {message:?}, which does not name {construct:?}"
            );
        }
    }

    #[test]
    fn writes_are_refused_as_writes_rather_than_as_syntax() {
        for (sql, construct) in [
            ("INSERT INTO t VALUES (1)", "INSERT"),
            ("UPDATE t SET a = 1", "UPDATE"),
            ("DELETE FROM t", "DELETE"),
            ("CREATE TABLE t (a BIGINT)", "CREATE TABLE"),
        ] {
            let error = parse(sql).unwrap_err();
            assert!(
                error.to_string().contains(construct),
                "{sql} was refused as {error}"
            );
        }
    }

    #[test]
    fn two_statements_are_refused() {
        assert_eq!(
            parse("SELECT t.a AS a FROM t; SELECT t.b AS b FROM t").unwrap_err(),
            SqlError::NotOneStatement { found: 2 }
        );
    }

    #[test]
    fn a_syntax_error_keeps_the_parsers_own_message() {
        let error = parse("SELECT t.a AS a FROM t WHERE").unwrap_err();
        assert!(matches!(error, SqlError::Parse(_)), "{error}");
    }
}
