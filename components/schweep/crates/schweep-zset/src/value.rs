//! Values and their types (`docs/SEMANTICS.md` S-1, S-2, S-7).
//!
//! The one subtlety in this file is that [`Value`] implements `Eq`, `Ord`, and `Hash` by hand
//! rather than by derive, because it can hold an `f64`. See [`Value::cmp`] for the total order
//! (S-7) and the note on `Float` equality.

use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};

/// A column type (S-2).
///
/// `Float64` is a **result-only** type (S-3, D-10): no table column may be declared `Float64`
/// and no scalar expression produces one. The sole source of a `Float64` value is `AVG` (S-31).
/// The type exists here because `AVG` results must be carried in a schema like any other value.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DataType {
    Int64,
    Utf8,
    Boolean,
    Float64,
}

impl DataType {
    /// True if this type may be used for a *stored table column* (S-3).
    #[must_use]
    pub fn is_storable(self) -> bool {
        !matches!(self, DataType::Float64)
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            DataType::Int64 => "Int64",
            DataType::Utf8 => "Utf8",
            DataType::Boolean => "Boolean",
            DataType::Float64 => "Float64",
        }
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A single value (S-1).
#[derive(Clone, Debug)]
pub enum Value {
    Null,
    Int(i64),
    Str(String),
    Bool(bool),
    /// Result-only (S-3): produced by `AVG` and nothing else.
    Float(f64),
}

impl Value {
    /// The type of a non-null value; `None` for [`Value::Null`], which has no type of its own.
    #[must_use]
    pub fn data_type(&self) -> Option<DataType> {
        match self {
            Value::Null => None,
            Value::Int(_) => Some(DataType::Int64),
            Value::Str(_) => Some(DataType::Utf8),
            Value::Bool(_) => Some(DataType::Boolean),
            Value::Float(_) => Some(DataType::Float64),
        }
    }

    #[must_use]
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// True if this value can be stored in a column of type `ty`. `Null` fits every type;
    /// whether a *null* is permitted is a nullability question, checked separately.
    #[must_use]
    pub fn fits(&self, ty: DataType) -> bool {
        match self.data_type() {
            None => true,
            Some(actual) => actual == ty,
        }
    }

    /// Rank of the value's variant in the total order (S-7). `Null` ranks below everything.
    ///
    /// Cross-type comparison does not arise in practice — values in one column share a type
    /// (S-2) — but the order must be *total*, so it is defined rather than left to chance.
    fn type_rank(&self) -> u8 {
        match self {
            Value::Null => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Str(_) => 4,
        }
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    /// The total order on values (S-7).
    ///
    /// - `Null` sorts before every non-null value.
    /// - `Int64` by numeric order; `Boolean` with `false < true`; `Utf8` byte-wise over the
    ///   UTF-8 encoding (no collation, ever, in v1); `Float64` by IEEE-754 total order.
    ///
    /// `Float` uses [`f64::total_cmp`], which distinguishes `-0.0 < 0.0` and orders `NaN`
    /// deterministically. `PartialEq` is defined as `cmp(..) == Equal`, so equality is *bitwise*
    /// for floats and therefore consistent with this ordering — the requirement `Eq` places on
    /// any type used as a `BTreeMap` key. (`NaN` cannot arise at all; see S-31.)
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Value::Null, Value::Null) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            (Value::Float(a), Value::Float(b)) => a.total_cmp(b),
            (Value::Str(a), Value::Str(b)) => a.as_bytes().cmp(b.as_bytes()),
            _ => self.type_rank().cmp(&other.type_rank()),
        }
    }
}

impl Hash for Value {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.type_rank().hash(state);
        match self {
            Value::Null => {}
            Value::Bool(b) => b.hash(state),
            Value::Int(i) => i.hash(state),
            // Bit pattern, to stay consistent with the bitwise equality above.
            Value::Float(f) => f.to_bits().hash(state),
            Value::Str(s) => s.hash(state),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => f.write_str("NULL"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Float(x) => write!(f, "{x:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn s7_null_sorts_before_every_non_null() {
        let non_nulls = [
            Value::Bool(false),
            Value::Int(i64::MIN),
            Value::Float(f64::NEG_INFINITY),
            Value::Str(String::new()),
        ];
        for v in &non_nulls {
            assert_eq!(
                Value::Null.cmp(v),
                Ordering::Less,
                "Null should precede {v}"
            );
            assert_eq!(v.cmp(&Value::Null), Ordering::Greater);
        }
    }

    #[test]
    fn s7_within_type_orders() {
        assert!(Value::Bool(false) < Value::Bool(true));
        assert!(Value::Int(-1) < Value::Int(0));
        // Byte-wise, not locale-aware: uppercase precedes lowercase.
        assert!(Value::Str("Z".into()) < Value::Str("a".into()));
        // Byte-wise over UTF-8 is code-point order.
        assert!(Value::Str("a".into()) < Value::Str("é".into()));
        assert!(Value::Float(-0.0) < Value::Float(0.0));
    }

    #[test]
    fn ordering_is_total_and_antisymmetric_across_variants() {
        let all = [
            Value::Null,
            Value::Bool(true),
            Value::Int(0),
            Value::Float(0.0),
            Value::Str("x".into()),
        ];
        for a in &all {
            for b in &all {
                assert_eq!(a.cmp(b).reverse(), b.cmp(a));
                assert_eq!(a == b, a.cmp(b) == Ordering::Equal);
            }
        }
    }

    #[test]
    fn eq_is_consistent_with_ord_for_floats() {
        // -0.0 and 0.0 are distinct values in the total order, so they must be distinct under
        // equality too, or BTreeMap keying would be unsound.
        assert_ne!(Value::Float(-0.0), Value::Float(0.0));
        assert_eq!(Value::Float(1.5), Value::Float(1.5));
    }

    #[test]
    fn s3_float64_is_not_storable() {
        assert!(!DataType::Float64.is_storable());
        for ty in [DataType::Int64, DataType::Utf8, DataType::Boolean] {
            assert!(ty.is_storable());
        }
    }

    #[test]
    fn null_fits_every_type() {
        for ty in [
            DataType::Int64,
            DataType::Utf8,
            DataType::Boolean,
            DataType::Float64,
        ] {
            assert!(Value::Null.fits(ty));
        }
        assert!(Value::Int(1).fits(DataType::Int64));
        assert!(!Value::Int(1).fits(DataType::Utf8));
    }
}
