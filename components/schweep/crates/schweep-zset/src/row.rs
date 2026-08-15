//! Rows (`docs/SEMANTICS.md` S-4).

use crate::value::Value;
use std::fmt;

/// One row: a value per schema column, in schema order.
///
/// `Ord` is derived, which gives lexicographic comparison over the values in schema order —
/// exactly the sort key S-8 requires for canonical form, and exactly the total tiebreak D-7
/// mandates ("the declared sort keys, then all remaining columns in schema order").
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Row {
    values: Vec<Value>,
}

impl Row {
    #[must_use]
    pub fn new(values: Vec<Value>) -> Row {
        Row { values }
    }

    #[must_use]
    pub fn values(&self) -> &[Value] {
        &self.values
    }

    #[must_use]
    pub fn into_values(self) -> Vec<Value> {
        self.values
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.values.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// The value at `index`, or `None` if the row is shorter than that.
    ///
    /// There is no indexing operator on `Row` on purpose: an out-of-bounds index is a panic
    /// wearing a `[]`, and library code here does not panic (§10).
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }
}

impl From<Vec<Value>> for Row {
    fn from(values: Vec<Value>) -> Row {
        Row::new(values)
    }
}

impl fmt::Display for Row {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        for (i, v) in self.values.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{v}")?;
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    #[test]
    fn rows_order_lexicographically_by_column() {
        let a = Row::new(vec![Value::Int(1), Value::Int(2)]);
        let b = Row::new(vec![Value::Int(1), Value::Int(3)]);
        let c = Row::new(vec![Value::Int(2), Value::Int(0)]);
        assert!(a < b);
        assert!(b < c);
    }

    #[test]
    fn s7_null_first_applies_positionally() {
        let a = Row::new(vec![Value::Null, Value::Int(9)]);
        let b = Row::new(vec![Value::Int(0), Value::Int(0)]);
        assert!(a < b, "a null in the first column sorts the row first");
    }

    #[test]
    fn out_of_bounds_get_is_none_not_a_panic() {
        let r = Row::new(vec![Value::Int(1)]);
        assert_eq!(r.get(0), Some(&Value::Int(1)));
        assert_eq!(r.get(1), None);
    }
}
