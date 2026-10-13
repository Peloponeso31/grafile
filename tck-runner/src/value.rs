//! Normalization of engine values into a comparable model, and comparison
//! against expected values.
//!
//! Nodes and relationships are compared structurally (labels, type and
//! properties), ignoring their internal ids, exactly as the TCK specifies.
//! Property and label order never matters; list order matters unless the
//! scenario says otherwise.

use std::fmt;

use grafile::engine::Database;
use grafile::record::Value;

use crate::expected::{Expected, PathElem};

/// A result value normalized for comparison.
#[derive(Debug, Clone, PartialEq)]
pub enum Actual {
    /// The `null` value.
    Null,
    /// A boolean.
    Bool(bool),
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Text(String),
    /// Raw bytes.
    Bytes(Vec<u8>),
    /// An ordered list.
    List(Vec<Actual>),
    /// A map.
    Map(Vec<(String, Actual)>),
    /// A node with resolved labels and properties.
    Node {
        /// Label names, sorted.
        labels: Vec<String>,
        /// Properties sorted by key name.
        props: Vec<(String, Actual)>,
    },
    /// A relationship with resolved type and properties.
    Rel {
        /// Relationship type name.
        ty: String,
        /// Properties sorted by key name.
        props: Vec<(String, Actual)>,
    },
}

impl fmt::Display for Actual {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Null => f.write_str("null"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Int(value) => write!(f, "{value}"),
            Self::Float(value) => write_float(f, *value),
            Self::Text(value) => write!(f, "'{value}'"),
            Self::Bytes(value) => write!(f, "<{} bytes>", value.len()),
            Self::List(items) => {
                f.write_str("[")?;
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Self::Map(entries) => write_entries(f, entries),
            Self::Node { labels, props } => {
                f.write_str("(")?;
                for label in labels {
                    write!(f, ":{label}")?;
                }
                if !props.is_empty() {
                    if !labels.is_empty() {
                        f.write_str(" ")?;
                    }
                    write_entries(f, props)?;
                }
                f.write_str(")")
            }
            Self::Rel { ty, props } => {
                write!(f, "[:{ty}")?;
                if !props.is_empty() {
                    f.write_str(" ")?;
                    write_entries(f, props)?;
                }
                f.write_str("]")
            }
        }
    }
}

fn write_float(f: &mut fmt::Formatter<'_>, value: f64) -> fmt::Result {
    if value.is_nan() {
        return f.write_str("NaN");
    }
    if value.is_infinite() {
        return f.write_str(if value > 0.0 { "Inf" } else { "-Inf" });
    }
    let mut buffer = ryu::Buffer::new();
    f.write_str(buffer.format(value))
}

fn write_entries(f: &mut fmt::Formatter<'_>, entries: &[(String, Actual)]) -> fmt::Result {
    f.write_str("{")?;
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            f.write_str(", ")?;
        }
        write!(f, "{key}: {value}")?;
    }
    f.write_str("}")
}

/// Convert an engine value into the comparable model.
///
/// Node and edge references are resolved through the engine, so the caller
/// must have finished the query that produced the value (the database is
/// needed to look up the entity).
///
/// # Errors
///
/// Returns a message when a referenced node or edge no longer exists or the
/// engine fails to read it.
pub fn normalize(db: &mut Database, value: &Value) -> Result<Actual, String> {
    match value {
        Value::Null => Ok(Actual::Null),
        Value::Bool(inner) => Ok(Actual::Bool(*inner)),
        Value::Int(inner) => Ok(Actual::Int(*inner)),
        Value::Float(inner) => Ok(Actual::Float(*inner)),
        Value::Text(inner) => Ok(Actual::Text(inner.to_string())),
        Value::Bytes(inner) => Ok(Actual::Bytes(inner.to_vec())),
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                out.push(normalize(db, item)?);
            }
            Ok(Actual::List(out))
        }
        Value::Map(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (key, value) in entries.iter() {
                out.push((key.to_string(), normalize(db, value)?));
            }
            Ok(Actual::Map(out))
        }
        Value::Node(id) => {
            let record = db
                .get_node(*id)
                .map_err(|err| format!("cannot read node {id}: {err}"))?
                .ok_or_else(|| format!("node {id} no longer exists"))?;
            let mut labels = Vec::with_capacity(record.labels.len());
            for label in &record.labels {
                labels.push(
                    db.schema()
                        .label_name(*label)
                        .unwrap_or("<unknown>")
                        .to_owned(),
                );
            }
            labels.sort();
            let mut props = Vec::with_capacity(record.props.len());
            for (key, value) in &record.props {
                props.push((
                    db.schema()
                        .property_key_name(*key)
                        .unwrap_or("<unknown>")
                        .to_owned(),
                    normalize(db, value)?,
                ));
            }
            props.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Actual::Node { labels, props })
        }
        Value::Edge(id) => {
            let record = db
                .get_edge(*id)
                .map_err(|err| format!("cannot read edge {id}: {err}"))?
                .ok_or_else(|| format!("edge {id} no longer exists"))?;
            let ty = db
                .schema()
                .edge_type_name(record.edge_type)
                .unwrap_or("<unknown>")
                .to_owned();
            let mut props = Vec::with_capacity(record.props.len());
            for (key, value) in &record.props {
                props.push((
                    db.schema()
                        .property_key_name(*key)
                        .unwrap_or("<unknown>")
                        .to_owned(),
                    normalize(db, value)?,
                ));
            }
            props.sort_by(|a, b| a.0.cmp(&b.0));
            Ok(Actual::Rel { ty, props })
        }
    }
}

/// How list order is treated during comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    /// Lists must match element by element.
    Ordered,
    /// Lists match when they contain the same elements in any order.
    IgnoreOrder,
}

/// Compare an expected value against an actual value.
///
/// # Errors
///
/// Returns a human-readable description of the first difference found.
pub fn compare(expected: &Expected, actual: &Actual, list_order: ListOrder) -> Result<(), String> {
    match (expected, actual) {
        (Expected::Null, Actual::Null) => Ok(()),
        (Expected::Bool(a), Actual::Bool(b)) if a == b => Ok(()),
        (Expected::Int(a), Actual::Int(b)) if a == b => Ok(()),
        (Expected::Float(a), Actual::Float(b)) if a == b || (a.is_nan() && b.is_nan()) => Ok(()),
        (Expected::Text(a), Actual::Text(b)) if a == b => Ok(()),
        (Expected::List(a), Actual::List(b)) => compare_lists(a, b, list_order),
        (Expected::Map(a), Actual::Map(b)) => compare_entries(a, b, list_order),
        (
            Expected::Node {
                labels: expected_labels,
                props: expected_props,
            },
            Actual::Node {
                labels: actual_labels,
                props: actual_props,
            },
        ) => {
            let mut expected_labels = expected_labels.clone();
            expected_labels.sort();
            if expected_labels != *actual_labels {
                return Err(format!(
                    "expected labels {expected_labels:?}, got {actual_labels:?}"
                ));
            }
            compare_entries(expected_props, actual_props, list_order)
        }
        (
            Expected::Rel {
                ty: expected_ty,
                props: expected_props,
            },
            Actual::Rel {
                ty: actual_ty,
                props: actual_props,
            },
        ) => {
            if let Some(expected_ty) = expected_ty
                && expected_ty != actual_ty
            {
                return Err(format!("expected type {expected_ty}, got {actual_ty}"));
            }
            compare_entries(expected_props, actual_props, list_order)
        }
        (Expected::Path(elements), _) => compare_path(elements, actual, list_order),
        _ => Err(format!("expected {expected}, got {actual}")),
    }
}

fn compare_path(
    elements: &[PathElem],
    actual: &Actual,
    list_order: ListOrder,
) -> Result<(), String> {
    let Actual::List(values) = actual else {
        return Err(format!("expected a path, got {}", summarize(actual)));
    };
    if values.len() != elements.len() {
        return Err(format!(
            "expected a path with {} elements, got {}",
            elements.len(),
            values.len()
        ));
    }
    for (element, value) in elements.iter().zip(values) {
        match element {
            PathElem::Node { labels, props } => {
                let expected = Expected::Node {
                    labels: labels.clone(),
                    props: props.clone(),
                };
                compare(&expected, value, list_order)
                    .map_err(|err| format!("path node mismatch: {err}"))?;
            }
            PathElem::Rel {
                ty,
                direction: _,
                props,
            } => {
                let expected = Expected::Rel {
                    ty: ty.clone(),
                    props: props.clone(),
                };
                compare(&expected, value, list_order)
                    .map_err(|err| format!("path relationship mismatch: {err}"))?;
            }
        }
    }
    Ok(())
}

fn compare_lists(
    expected: &[Expected],
    actual: &[Actual],
    list_order: ListOrder,
) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "expected a list of {} elements, got {}",
            expected.len(),
            actual.len()
        ));
    }
    if list_order == ListOrder::Ordered {
        for (index, (expected, actual)) in expected.iter().zip(actual).enumerate() {
            compare(expected, actual, list_order)
                .map_err(|err| format!("list element {index}: {err}"))?;
        }
        return Ok(());
    }
    let mut expected_keys: Vec<String> = expected.iter().map(canonical_expected).collect();
    let mut actual_keys: Vec<String> = actual.iter().map(canonical_actual).collect();
    expected_keys.sort();
    actual_keys.sort();
    if expected_keys != actual_keys {
        return Err(format!(
            "expected list {} in any order, got {}",
            Expected::List(expected.to_vec()),
            Actual::List(actual.to_vec())
        ));
    }
    Ok(())
}

fn compare_entries(
    expected: &[(String, Expected)],
    actual: &[(String, Actual)],
    list_order: ListOrder,
) -> Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "expected {} properties, got {}",
            expected.len(),
            actual.len()
        ));
    }
    for (key, expected_value) in expected {
        let Some((_, actual_value)) = actual.iter().find(|(name, _)| name == key) else {
            return Err(format!("missing property `{key}`"));
        };
        compare(expected_value, actual_value, list_order)
            .map_err(|err| format!("property `{key}`: {err}"))?;
    }
    Ok(())
}

fn summarize(value: &Actual) -> String {
    value.to_string()
}

/// A canonical string for multiset comparison. Two values that compare equal
/// produce the same string.
pub fn canonical_expected(value: &Expected) -> String {
    match value {
        Expected::Float(value) => canonical_float(*value),
        Expected::List(items) => {
            let mut keys: Vec<String> = items.iter().map(canonical_expected).collect();
            keys.sort();
            format!("[{}]", keys.join(","))
        }
        Expected::Map(entries) => {
            let mut keys: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_expected(value)))
                .collect();
            keys.sort();
            format!("{{{}}}", keys.join(","))
        }
        Expected::Node { labels, props } => {
            let mut labels = labels.clone();
            labels.sort();
            let mut keys: Vec<String> = props
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_expected(value)))
                .collect();
            keys.sort();
            format!("(:{}{})", labels.join(":"), keys.join(","))
        }
        Expected::Rel { ty, props } => {
            let mut keys: Vec<String> = props
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_expected(value)))
                .collect();
            keys.sort();
            format!("[:{}{}]", ty.clone().unwrap_or_default(), keys.join(","))
        }
        other => other.to_string(),
    }
}

/// See [`canonical_expected`].
pub fn canonical_actual(value: &Actual) -> String {
    match value {
        Actual::Float(value) => canonical_float(*value),
        Actual::List(items) => {
            let mut keys: Vec<String> = items.iter().map(canonical_actual).collect();
            keys.sort();
            format!("[{}]", keys.join(","))
        }
        Actual::Map(entries) => {
            let mut keys: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_actual(value)))
                .collect();
            keys.sort();
            format!("{{{}}}", keys.join(","))
        }
        Actual::Node { labels, props } => {
            let mut keys: Vec<String> = props
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_actual(value)))
                .collect();
            keys.sort();
            format!("(:{}{})", labels.join(":"), keys.join(","))
        }
        Actual::Rel { ty, props } => {
            let mut keys: Vec<String> = props
                .iter()
                .map(|(key, value)| format!("{key}:{}", canonical_actual(value)))
                .collect();
            keys.sort();
            format!("[:{}{}]", ty, keys.join(","))
        }
        other => other.to_string(),
    }
}

fn canonical_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".to_owned();
    }
    let mut buffer = ryu::Buffer::new();
    buffer.format(value).to_owned()
}

/// Convert an expected value into an engine parameter value.
///
/// # Errors
///
/// Returns a message when the expected value is a graph pattern or path,
/// which cannot be used as a parameter.
pub fn to_parameter(value: &Expected) -> Result<Value, String> {
    match value {
        Expected::Null => Ok(Value::Null),
        Expected::Bool(inner) => Ok(Value::Bool(*inner)),
        Expected::Int(inner) => Ok(Value::Int(*inner)),
        Expected::Float(inner) => Ok(Value::Float(*inner)),
        Expected::Text(inner) => Ok(Value::Text(inner.as_str().into())),
        Expected::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(to_parameter(item)?);
            }
            Ok(Value::List(out.into()))
        }
        Expected::Map(entries) => {
            let mut out = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                out.push((key.as_str().into(), to_parameter(value)?));
            }
            Ok(Value::Map(out.into()))
        }
        Expected::Node { .. } | Expected::Rel { .. } | Expected::Path(_) => {
            Err("graph patterns cannot be used as parameters".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actual(value: Value) -> Actual {
        match value {
            Value::Null => Actual::Null,
            Value::Bool(inner) => Actual::Bool(inner),
            Value::Int(inner) => Actual::Int(inner),
            Value::Float(inner) => Actual::Float(inner),
            Value::Text(inner) => Actual::Text(inner.to_string()),
            Value::List(items) => {
                Actual::List(items.iter().map(|item| actual(item.clone())).collect())
            }
            other => panic!("unsupported test value: {other:?}"),
        }
    }

    #[test]
    fn compares_primitives() {
        let expected = crate::expected::parse("1").expect("parse");
        assert!(compare(&expected, &Actual::Int(1), ListOrder::Ordered).is_ok());
        assert!(compare(&expected, &Actual::Int(2), ListOrder::Ordered).is_err());
        assert!(compare(&expected, &Actual::Float(1.0), ListOrder::Ordered).is_err());
    }

    #[test]
    fn compares_nan_as_equal() {
        let expected = crate::expected::parse("NaN").expect("parse");
        assert!(compare(&expected, &Actual::Float(f64::NAN), ListOrder::Ordered).is_ok());
    }

    #[test]
    fn list_order_modes() {
        let expected = crate::expected::parse("[1, 2]").expect("parse");
        let value = actual(Value::List(vec![Value::Int(2), Value::Int(1)].into()));
        assert!(compare(&expected, &value, ListOrder::Ordered).is_err());
        assert!(compare(&expected, &value, ListOrder::IgnoreOrder).is_ok());
    }

    #[test]
    fn parameters_roundtrip() {
        let expected = crate::expected::parse("{a: [1, 'x'], b: null}").expect("parse");
        let value = to_parameter(&expected).expect("parameter");
        match value {
            Value::Map(entries) => assert_eq!(entries.len(), 2),
            other => panic!("expected map, got {other:?}"),
        }
    }
}
