//! Schemas (`docs/SEMANTICS.md` S-2) and their translation to Arrow.

use crate::error::{Result, ZSetError};
use crate::value::{DataType, Value};
use std::fmt;
use std::sync::Arc;

/// One column: a name, a type, and whether nulls are permitted.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Field {
    pub name: String,
    pub data_type: DataType,
    pub nullable: bool,
}

impl Field {
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Field {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    /// A nullable column, which is the common case: nulls are ordinary values here (S-13..S-18).
    pub fn nullable(name: impl Into<String>, data_type: DataType) -> Self {
        Field::new(name, data_type, true)
    }

    /// A `NOT NULL` column. The assertion is *checked* on construction of every batch and
    /// reported as an error (S-2); it is never used to skip null handling in an operator.
    pub fn not_null(name: impl Into<String>, data_type: DataType) -> Self {
        Field::new(name, data_type, false)
    }
}

/// An ordered list of uniquely-named columns.
///
/// Schema equality is part of answer equality (S-8): two Z-sets holding the same rows under
/// different schemas are not the same answer.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Schema {
    fields: Vec<Field>,
}

impl Schema {
    /// Build a schema, rejecting an empty field list and duplicate names.
    ///
    /// A zero-column schema is refused because a Z-set of zero-column rows cannot express a row
    /// count in Arrow, and because no query in the rung-1–3 dialect produces one.
    pub fn new(fields: Vec<Field>) -> Result<Schema> {
        if fields.is_empty() {
            return Err(ZSetError::EmptySchema);
        }
        for (i, f) in fields.iter().enumerate() {
            if let Some(prior) = fields.get(..i) {
                if prior.iter().any(|p| p.name == f.name) {
                    return Err(ZSetError::DuplicateFieldName(f.name.clone()));
                }
            }
        }
        Ok(Schema { fields })
    }

    /// Build a schema for a stored *table*, additionally refusing result-only types (S-3).
    pub fn new_table(fields: Vec<Field>) -> Result<Schema> {
        for f in &fields {
            if !f.data_type.is_storable() {
                return Err(ZSetError::UnstorableColumnType {
                    name: f.name.clone(),
                    declared: f.data_type,
                });
            }
        }
        Schema::new(fields)
    }

    #[must_use]
    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        // Unreachable in practice — `new` refuses an empty field list — but `len` without
        // `is_empty` is a clippy lint, and a truthful implementation costs one line.
        self.fields.is_empty()
    }

    #[must_use]
    pub fn field(&self, index: usize) -> Option<&Field> {
        self.fields.get(index)
    }

    #[must_use]
    pub fn index_of(&self, name: &str) -> Option<usize> {
        self.fields.iter().position(|f| f.name == name)
    }

    pub fn field_named(&self, name: &str) -> Result<&Field> {
        self.fields
            .iter()
            .find(|f| f.name == name)
            .ok_or_else(|| ZSetError::UnknownColumn(name.to_owned()))
    }

    /// Validate one value against the column at `index` (type, then nullability).
    pub fn check_value(&self, index: usize, value: &Value) -> Result<()> {
        let field = self.fields.get(index).ok_or(ZSetError::ArityMismatch {
            expected: self.fields.len(),
            found: index + 1,
        })?;
        if value.is_null() {
            if field.nullable {
                return Ok(());
            }
            return Err(ZSetError::NullInNonNullable {
                column: field.name.clone(),
            });
        }
        match value.data_type() {
            Some(actual) if actual == field.data_type => Ok(()),
            Some(actual) => Err(ZSetError::ValueTypeMismatch {
                column: field.name.clone(),
                expected: field.data_type,
                found: actual,
            }),
            // `data_type()` returns `None` only for `Null`, handled above.
            None => Err(ZSetError::NullInNonNullable {
                column: field.name.clone(),
            }),
        }
    }

    /// The equivalent Arrow schema (D-2). Column order and names are preserved exactly.
    #[must_use]
    pub fn to_arrow(&self) -> Arc<arrow_schema::Schema> {
        let fields: Vec<arrow_schema::Field> = self
            .fields
            .iter()
            .map(|f| {
                let ty = match f.data_type {
                    DataType::Int64 => arrow_schema::DataType::Int64,
                    DataType::Utf8 => arrow_schema::DataType::Utf8,
                    DataType::Boolean => arrow_schema::DataType::Boolean,
                    DataType::Float64 => arrow_schema::DataType::Float64,
                };
                arrow_schema::Field::new(f.name.clone(), ty, f.nullable)
            })
            .collect();
        Arc::new(arrow_schema::Schema::new(fields))
    }
}

impl fmt::Display for Schema {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("(")?;
        for (i, field) in self.fields.iter().enumerate() {
            if i > 0 {
                f.write_str(", ")?;
            }
            write!(f, "{}: {}", field.name, field.data_type)?;
            if !field.nullable {
                f.write_str(" NOT NULL")?;
            }
        }
        f.write_str(")")
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::indexing_slicing)]

    use super::*;

    fn ints(names: &[&str]) -> Vec<Field> {
        names
            .iter()
            .map(|n| Field::nullable(*n, DataType::Int64))
            .collect()
    }

    #[test]
    fn rejects_empty_schema() {
        assert_eq!(Schema::new(vec![]).unwrap_err(), ZSetError::EmptySchema);
    }

    #[test]
    fn rejects_duplicate_names() {
        let err = Schema::new(ints(&["a", "b", "a"])).unwrap_err();
        assert_eq!(err, ZSetError::DuplicateFieldName("a".into()));
    }

    #[test]
    fn s3_table_schema_refuses_float_columns() {
        let err = Schema::new_table(vec![Field::nullable("x", DataType::Float64)]).unwrap_err();
        assert_eq!(
            err,
            ZSetError::UnstorableColumnType {
                name: "x".into(),
                declared: DataType::Float64
            }
        );
        // The same schema is fine for a *result*, which is where AVG lands.
        assert!(Schema::new(vec![Field::nullable("x", DataType::Float64)]).is_ok());
    }

    #[test]
    fn check_value_enforces_type_and_nullability() {
        let s = Schema::new(vec![
            Field::nullable("a", DataType::Int64),
            Field::not_null("b", DataType::Utf8),
        ])
        .unwrap();

        assert!(s.check_value(0, &Value::Null).is_ok());
        assert!(s.check_value(0, &Value::Int(1)).is_ok());
        assert_eq!(
            s.check_value(0, &Value::Str("x".into())).unwrap_err(),
            ZSetError::ValueTypeMismatch {
                column: "a".into(),
                expected: DataType::Int64,
                found: DataType::Utf8
            }
        );
        assert_eq!(
            s.check_value(1, &Value::Null).unwrap_err(),
            ZSetError::NullInNonNullable { column: "b".into() }
        );
    }

    #[test]
    fn arrow_schema_preserves_names_types_and_order() {
        let s = Schema::new(vec![
            Field::nullable("a", DataType::Int64),
            Field::not_null("b", DataType::Utf8),
            Field::nullable("c", DataType::Boolean),
            Field::nullable("d", DataType::Float64),
        ])
        .unwrap();
        let arrow = s.to_arrow();
        let names: Vec<&str> = arrow.fields().iter().map(|f| f.name().as_str()).collect();
        assert_eq!(names, ["a", "b", "c", "d"]);
        assert_eq!(arrow.field(0).data_type(), &arrow_schema::DataType::Int64);
        assert_eq!(arrow.field(1).data_type(), &arrow_schema::DataType::Utf8);
        assert!(!arrow.field(1).is_nullable());
        assert_eq!(arrow.field(2).data_type(), &arrow_schema::DataType::Boolean);
        assert_eq!(arrow.field(3).data_type(), &arrow_schema::DataType::Float64);
    }
}
