//! The query IR for dialect rungs 1–3 (`docs/SEMANTICS.md` S-9 … S-11).
//!
//! This is a *typed API*, not SQL. There is no parser in C0 (D-4 puts sqlparser behind the
//! binder in C5), so a query is constructed directly. The shape mirrors S-9 exactly:
//!
//! ```text
//! FROM     Scan | Join
//! WHERE    optional predicate
//! GROUP BY optional { keys, aggregates, optional HAVING }
//! SELECT   optional projection
//! ```
//!
//! evaluated strictly in that order.

use schweep_zset::{DataType, Value};

/// What a query reads from (S-9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    /// A base table under an alias. Its columns are visible as `alias.column` (S-10, S-23).
    Scan { table: String, alias: String },
    /// An INNER equi-join (S-26). At least one key pair is required; a join with none is a
    /// cross join and is refused.
    Join {
        left: Box<Source>,
        right: Box<Source>,
        /// Pairs of column names, each already qualified: `("l.id", "r.id")`.
        on: Vec<(String, String)>,
    },
}

impl Source {
    pub fn scan(table: impl Into<String>, alias: impl Into<String>) -> Source {
        Source::Scan {
            table: table.into(),
            alias: alias.into(),
        }
    }

    pub fn join(left: Source, right: Source, on: Vec<(String, String)>) -> Source {
        Source::Join {
            left: Box::new(left),
            right: Box::new(right),
            on,
        }
    }

    /// Every alias this source introduces, in left-to-right order.
    pub fn aliases(&self) -> Vec<&str> {
        match self {
            Source::Scan { alias, .. } => vec![alias.as_str()],
            Source::Join { left, right, .. } => {
                let mut out = left.aliases();
                out.extend(right.aliases());
                out
            }
        }
    }
}

/// Binary operators (S-19). Arithmetic is `Int64`-only; comparison is same-type-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl BinOp {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Mod => "%",
            BinOp::Eq => "=",
            BinOp::Ne => "<>",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
        }
    }

    #[must_use]
    pub fn is_arithmetic(self) -> bool {
        matches!(
            self,
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Mod
        )
    }
}

/// A scalar expression (S-18, S-19).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    /// A column in the current scope, by its exact name: `alias.column` before a GROUP BY, and
    /// a declared output name after one (S-10).
    Column(String),
    /// A non-null literal. A `Value::Null` here is refused as `UntypedNullLiteral` — write
    /// [`Expr::Null`] instead (S-19).
    Literal(Value),
    /// A null of a stated type (S-19).
    Null(DataType),
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Not(Box<Expr>),
    /// Kleene `AND`; both operands are always evaluated (S-15).
    And(Box<Expr>, Box<Expr>),
    /// Kleene `OR`; both operands are always evaluated (S-15).
    Or(Box<Expr>, Box<Expr>),
    IsNull(Box<Expr>),
    IsNotNull(Box<Expr>),
    /// `CASE WHEN c THEN r ... ELSE e END`, short-circuiting on the first `true` (S-18).
    Case {
        whens: Vec<(Expr, Expr)>,
        otherwise: Option<Box<Expr>>,
    },
}

impl Expr {
    pub fn column(name: impl Into<String>) -> Expr {
        Expr::Column(name.into())
    }

    pub fn int(v: i64) -> Expr {
        Expr::Literal(Value::Int(v))
    }

    pub fn string(v: impl Into<String>) -> Expr {
        Expr::Literal(Value::Str(v.into()))
    }

    pub fn boolean(v: bool) -> Expr {
        Expr::Literal(Value::Bool(v))
    }

    pub fn binary(op: BinOp, left: Expr, right: Expr) -> Expr {
        Expr::Binary {
            op,
            left: Box::new(left),
            right: Box::new(right),
        }
    }

    pub fn and(left: Expr, right: Expr) -> Expr {
        Expr::And(Box::new(left), Box::new(right))
    }

    pub fn or(left: Expr, right: Expr) -> Expr {
        Expr::Or(Box::new(left), Box::new(right))
    }

    pub fn is_null(inner: Expr) -> Expr {
        Expr::IsNull(Box::new(inner))
    }

    pub fn is_not_null(inner: Expr) -> Expr {
        Expr::IsNotNull(Box::new(inner))
    }
}

/// `!expr` builds a Kleene `NOT` (S-15), so `!NULL` is `NULL` rather than `true`.
///
/// This is `std::ops::Not` rather than an `Expr::not` constructor because a method called `not`
/// on a type that does not implement the trait is exactly the ambiguity the trait exists to
/// remove.
impl std::ops::Not for Expr {
    type Output = Expr;

    fn not(self) -> Expr {
        Expr::Not(Box::new(self))
    }
}

/// The five aggregates, plus `COUNT(*)` (S-30).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AggFunc {
    /// Counts *entries by weight*, not distinct rows: a row at weight 3 counts 3 (S-30).
    CountStar,
    Count(Expr),
    Sum(Expr),
    Min(Expr),
    Max(Expr),
    Avg(Expr),
}

impl AggFunc {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            AggFunc::CountStar => "COUNT(*)",
            AggFunc::Count(_) => "COUNT",
            AggFunc::Sum(_) => "SUM",
            AggFunc::Min(_) => "MIN",
            AggFunc::Max(_) => "MAX",
            AggFunc::Avg(_) => "AVG",
        }
    }

    #[must_use]
    pub fn argument(&self) -> Option<&Expr> {
        match self {
            AggFunc::CountStar => None,
            AggFunc::Count(e) | AggFunc::Sum(e) | AggFunc::Min(e) | AggFunc::Max(e) => Some(e),
            AggFunc::Avg(e) => Some(e),
        }
    }
}

/// One named output column: `(name, expression)` (S-11).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Named<T> {
    pub name: String,
    pub value: T,
}

impl<T> Named<T> {
    pub fn new(name: impl Into<String>, value: T) -> Named<T> {
        Named {
            name: name.into(),
            value,
        }
    }
}

/// GROUP BY with its aggregates and optional HAVING (S-27, S-32).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GroupBy {
    /// At least one key. Zero keys is refused as `EmptyGroupKeys` (S-33).
    pub keys: Vec<Named<Expr>>,
    pub aggregates: Vec<Named<AggFunc>>,
    /// Evaluated over the GROUP BY output schema, never over the input rows (S-32).
    pub having: Option<Expr>,
}

/// A rung-1–3 query (S-9).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Query {
    pub source: Source,
    pub filter: Option<Expr>,
    pub group_by: Option<GroupBy>,
    pub project: Option<Vec<Named<Expr>>>,
    /// `SELECT DISTINCT`: keep one copy of every row present in the answer (S-34).
    ///
    /// Applied **last**, after the projection. It changes no schema, only weights.
    pub distinct: bool,
}

impl Query {
    /// The bare rung-1 shape: everything from one source, no filter, group, or projection.
    pub fn from(source: Source) -> Query {
        Query {
            source,
            filter: None,
            group_by: None,
            project: None,
            distinct: false,
        }
    }

    #[must_use]
    pub fn filter(mut self, predicate: Expr) -> Query {
        self.filter = Some(predicate);
        self
    }

    #[must_use]
    pub fn group_by(mut self, group_by: GroupBy) -> Query {
        self.group_by = Some(group_by);
        self
    }

    #[must_use]
    pub fn project(mut self, project: Vec<Named<Expr>>) -> Query {
        self.project = Some(project);
        self
    }

    /// `SELECT DISTINCT` (S-34).
    #[must_use]
    pub fn distinct(mut self) -> Query {
        self.distinct = true;
        self
    }
}
