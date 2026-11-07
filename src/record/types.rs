//! Cypher value types: what `valueType()` reports and `IS :: TYPE` checks.
//!
//! A [`CypherType`] is a union of base types plus a nullability flag, which
//! covers the GQL type grammar Neo4j 5 accepts (`INTEGER NOT NULL`,
//! `LIST<STRING>`, `INTEGER | FLOAT`, `ANY`, `NOTHING`, `NULL`).

use std::fmt;

use super::{Value, is_path_shaped};

/// One member of a type union, in Neo4j's canonical display order.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum BaseType {
    /// `BOOLEAN`
    Boolean,
    /// `STRING`
    String,
    /// `INTEGER`
    Integer,
    /// `FLOAT`
    Float,
    /// `DATE`
    Date,
    /// `LOCAL TIME`
    LocalTime,
    /// `ZONED TIME`
    ZonedTime,
    /// `LOCAL DATETIME`
    LocalDateTime,
    /// `ZONED DATETIME`
    ZonedDateTime,
    /// `DURATION`
    Duration,
    /// `POINT`
    Point,
    /// `NODE`
    Node,
    /// `RELATIONSHIP`
    Relationship,
    /// `MAP`
    Map,
    /// `LIST<element>`
    List(Box<CypherType>),
    /// `PATH`
    Path,
    /// `ANY`: every value.
    Any,
}

/// A union of base types; `nullable` admits null. No alternatives with
/// `nullable` is `NULL`, and none without it is `NOTHING`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CypherType {
    alternatives: Vec<BaseType>,
    nullable: bool,
}

impl CypherType {
    /// A union of `alternatives` (sorted and deduplicated here).
    ///
    /// # Arguments
    ///
    /// * `alternatives` - Base types of the union; empty means `NULL` when
    ///   `nullable` and `NOTHING` otherwise
    /// * `nullable` - Whether null belongs to the type (no `NOT NULL`)
    pub fn new(mut alternatives: Vec<BaseType>, nullable: bool) -> Self {
        alternatives.sort();
        alternatives.dedup();
        Self {
            alternatives,
            nullable,
        }
    }

    /// The most specific type of `value`, as `valueType()` reports it.
    pub fn of(value: &Value) -> Self {
        let base = match value {
            Value::Null => return Self::new(Vec::new(), true),
            Value::Bool(_) => BaseType::Boolean,
            Value::Int(_) => BaseType::Integer,
            Value::Float(_) => BaseType::Float,
            // Byte arrays have no Cypher type of their own.
            Value::Text(_) | Value::Bytes(_) => BaseType::String,
            Value::List(items) if is_path_shaped(items) => BaseType::Path,
            Value::List(items) => {
                let mut alternatives = Vec::new();
                let mut nullable = false;
                for item in items.iter() {
                    let item_type = Self::of(item);
                    nullable |= item_type.nullable;
                    alternatives.extend(item_type.alternatives);
                }
                BaseType::List(Box::new(Self::new(alternatives, nullable)))
            }
            Value::Map(_) => BaseType::Map,
            Value::Node(_) => BaseType::Node,
            Value::Edge(_) => BaseType::Relationship,
            Value::Date(_) => BaseType::Date,
            Value::LocalTime(_) => BaseType::LocalTime,
            Value::Time(_) => BaseType::ZonedTime,
            Value::LocalDateTime(_) => BaseType::LocalDateTime,
            Value::DateTime(_) => BaseType::ZonedDateTime,
            Value::Duration(_) => BaseType::Duration,
            Value::Point(_) => BaseType::Point,
        };
        Self::new(vec![base], false)
    }

    /// Whether `value` belongs to this type (`value IS :: self`).
    pub fn accepts(&self, value: &Value) -> bool {
        if value.is_null() {
            return self.nullable;
        }
        self.alternatives
            .iter()
            .any(|alternative| alternative.accepts(value))
    }
}

impl BaseType {
    fn accepts(&self, value: &Value) -> bool {
        match (self, value) {
            (Self::Any, _) => true,
            (Self::List(element), Value::List(items)) => {
                items.iter().all(|item| element.accepts(item))
            }
            (Self::Path, Value::List(items)) => is_path_shaped(items),
            (Self::List(_) | Self::Path, _) => false,
            (base, value) => CypherType::of(value).alternatives.first() == Some(base),
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::Boolean => "BOOLEAN",
            Self::String => "STRING",
            Self::Integer => "INTEGER",
            Self::Float => "FLOAT",
            Self::Date => "DATE",
            Self::LocalTime => "LOCAL TIME",
            Self::ZonedTime => "ZONED TIME",
            Self::LocalDateTime => "LOCAL DATETIME",
            Self::ZonedDateTime => "ZONED DATETIME",
            Self::Duration => "DURATION",
            Self::Point => "POINT",
            Self::Node => "NODE",
            Self::Relationship => "RELATIONSHIP",
            Self::Map => "MAP",
            Self::List(_) => "LIST",
            Self::Path => "PATH",
            Self::Any => "ANY",
        }
    }
}

impl fmt::Display for CypherType {
    /// Neo4j's spelling: `INTEGER NOT NULL`, `LIST<STRING> NOT NULL`,
    /// `INTEGER NOT NULL | STRING NOT NULL`, `NULL` or `NOTHING`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.alternatives.is_empty() {
            return f.write_str(if self.nullable { "NULL" } else { "NOTHING" });
        }
        for (index, alternative) in self.alternatives.iter().enumerate() {
            if index > 0 {
                f.write_str(" | ")?;
            }
            f.write_str(alternative.name())?;
            if let BaseType::List(element) = alternative {
                write!(f, "<{element}>")?;
            }
            if !self.nullable {
                f.write_str(" NOT NULL")?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(items: Vec<Value>) -> Value {
        Value::List(items.into())
    }

    #[test]
    fn value_types_use_neo4j_spelling() {
        let cases = [
            (Value::Null, "NULL"),
            (Value::Int(1), "INTEGER NOT NULL"),
            (Value::from("a"), "STRING NOT NULL"),
            (list(vec![]), "LIST<NOTHING> NOT NULL"),
            (
                list(vec![Value::Int(1), Value::Null]),
                "LIST<INTEGER> NOT NULL",
            ),
            (
                list(vec![Value::from("a"), Value::Int(1)]),
                "LIST<STRING NOT NULL | INTEGER NOT NULL> NOT NULL",
            ),
            (
                list(vec![Value::Node(1), Value::Edge(2), Value::Node(3)]),
                "PATH NOT NULL",
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(CypherType::of(&value).to_string(), expected, "{value:?}");
        }
    }

    #[test]
    fn accepts_follows_nullability_unions_and_elements() {
        let integer = CypherType::new(vec![BaseType::Integer], true);
        let number = CypherType::new(vec![BaseType::Float, BaseType::Integer], false);
        let int_list = CypherType::new(
            vec![BaseType::List(Box::new(CypherType::new(
                vec![BaseType::Integer],
                false,
            )))],
            false,
        );
        assert!(integer.accepts(&Value::Null));
        assert!(!number.accepts(&Value::Null));
        assert!(number.accepts(&Value::Float(1.5)));
        assert!(!number.accepts(&Value::from("1")));
        assert!(int_list.accepts(&list(vec![Value::Int(1), Value::Int(2)])));
        assert!(!int_list.accepts(&list(vec![Value::Int(1), Value::Null])));
        assert!(CypherType::new(vec![BaseType::Any], false).accepts(&Value::Node(1)));
        assert!(!CypherType::new(Vec::new(), false).accepts(&Value::Int(1)));
    }
}
