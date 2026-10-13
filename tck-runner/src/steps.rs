//! Classification of TCK Gherkin steps into executable steps.
//!
//! Unknown step text is a harness error, never a silent skip: the gate must
//! fail until the step is implemented here.

use std::fmt;

use gherkin::{Step as GherkinStep, StepType};

/// A classified scenario step.
#[derive(Debug, Clone)]
pub enum Step {
    /// `Given an empty graph`.
    EmptyGraph,
    /// `Given any graph`: the scenario does not depend on graph content.
    AnyGraph,
    /// `Given the binary-tree-N graph`.
    NamedGraph(String),
    /// `Given having executed:` with the setup query.
    HavingExecuted(String),
    /// `Given parameters are:` with name/value cell pairs.
    Parameters(Vec<(String, String)>),
    /// `Given there exists a procedure ...`: recognized, but grafile has no
    /// procedures, so the scenario is not applicable.
    Procedure(String),
    /// `When executing query:`.
    ExecutingQuery(String),
    /// `When executing control query:`: the control query's result replaces
    /// the primary result under test.
    ExecutingControlQuery(String),
    /// `Then the result should be ...` with an expected table.
    Result {
        /// Raw expected table, header row included.
        table: Vec<Vec<String>>,
        /// Whether row order is significant.
        order: RowOrder,
        /// How lists inside cells are compared.
        list_order: ListOrder,
    },
    /// `Then the result should be empty`.
    ResultEmpty,
    /// `Then no side effects`.
    NoSideEffects,
    /// `Then the side effects should be:` with raw metric/count pairs.
    SideEffects(Vec<(String, String)>),
    /// `Then a TYPE should be raised at PHASE[: DETAIL]`.
    Error {
        /// Expected error type name, as written in the TCK.
        kind: String,
        /// Expected phase.
        phase: ExpectedPhase,
        /// Expected detail, if any (informational only).
        detail: Option<String>,
    },
}

/// Whether result rows are compared in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowOrder {
    /// `the result should be, in any order`
    Any,
    /// `the result should be, in order`
    In,
}

/// Whether lists inside result cells are compared in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListOrder {
    /// Lists must match element by element.
    Ordered,
    /// `(ignoring element order for lists)`
    IgnoreOrder,
}

/// The phase at which an error is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExpectedPhase {
    /// `compile time`
    Compile,
    /// `runtime`
    Runtime,
    /// `any time`
    Any,
}

impl fmt::Display for ExpectedPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Compile => "compile time",
            Self::Runtime => "runtime",
            Self::Any => "any time",
        })
    }
}

/// Classify one Gherkin step.
///
/// # Errors
///
/// Returns the step text when it is not recognized.
pub fn classify(step: &GherkinStep) -> Result<Step, String> {
    let text = step.value.trim();
    match step.ty {
        StepType::Given => classify_given(text, step),
        StepType::When => classify_when(text, step),
        StepType::Then => classify_then(text, step),
    }
}

fn classify_given(text: &str, step: &GherkinStep) -> Result<Step, String> {
    if text == "an empty graph" {
        return Ok(Step::EmptyGraph);
    }
    if text == "any graph" {
        return Ok(Step::AnyGraph);
    }
    if let Some(name) = text
        .strip_prefix("the ")
        .and_then(|rest| rest.strip_suffix(" graph"))
        && name.starts_with("binary-tree-")
    {
        return Ok(Step::NamedGraph(name.to_owned()));
    }
    if text.starts_with("having executed") {
        return Ok(Step::HavingExecuted(docstring(step)?));
    }
    if text.starts_with("parameters are") {
        return Ok(Step::Parameters(rows(step)?));
    }
    if text.starts_with("there exists a procedure") {
        return Ok(Step::Procedure(text.to_owned()));
    }
    Err(text.to_owned())
}

fn classify_when(text: &str, step: &GherkinStep) -> Result<Step, String> {
    if text.starts_with("executing control query") {
        return Ok(Step::ExecutingControlQuery(docstring(step)?));
    }
    if text.starts_with("executing query") {
        return Ok(Step::ExecutingQuery(docstring(step)?));
    }
    Err(text.to_owned())
}

fn classify_then(text: &str, step: &GherkinStep) -> Result<Step, String> {
    if let Some(rest) = text.strip_prefix("the result should be") {
        if rest.starts_with(", in order (ignoring element order for lists)") {
            return result(step, RowOrder::In, ListOrder::IgnoreOrder);
        }
        if rest.starts_with(", in any order (ignoring element order for lists)") {
            return result(step, RowOrder::Any, ListOrder::IgnoreOrder);
        }
        if rest.starts_with(" (ignoring element order for lists)") {
            return result(step, RowOrder::Any, ListOrder::IgnoreOrder);
        }
        if rest.starts_with(", in order") {
            return result(step, RowOrder::In, ListOrder::Ordered);
        }
        if rest.starts_with(", in any order") {
            return result(step, RowOrder::Any, ListOrder::Ordered);
        }
        if rest.is_empty() {
            return result(step, RowOrder::Any, ListOrder::Ordered);
        }
        if rest == " empty" {
            return Ok(Step::ResultEmpty);
        }
    }
    if text == "no side effects" {
        return Ok(Step::NoSideEffects);
    }
    if text.starts_with("the side effects should be") {
        return Ok(Step::SideEffects(rows(step)?));
    }
    if let Some(error) = parse_error_step(text) {
        return Ok(error);
    }
    Err(text.to_owned())
}

fn parse_error_step(text: &str) -> Option<Step> {
    let rest = text
        .strip_prefix("a ")
        .or_else(|| text.strip_prefix("an "))?;
    let (kind, rest) = rest.split_once(" should be raised at ")?;
    let (phase, detail) = match rest.split_once(':') {
        Some((phase, detail)) => (phase.trim(), Some(detail.trim().to_owned())),
        None => (rest.trim(), None),
    };
    let phase = match phase {
        "compile time" => ExpectedPhase::Compile,
        "runtime" => ExpectedPhase::Runtime,
        "any time" => ExpectedPhase::Any,
        _ => return None,
    };
    Some(Step::Error {
        kind: kind.to_owned(),
        phase,
        detail,
    })
}

fn result(step: &GherkinStep, order: RowOrder, list_order: ListOrder) -> Result<Step, String> {
    let table = step
        .table
        .as_ref()
        .ok_or_else(|| format!("`{}` without a result table", step.value.trim()))?;
    Ok(Step::Result {
        table: table.rows.clone(),
        order,
        list_order,
    })
}

fn rows(step: &GherkinStep) -> Result<Vec<(String, String)>, String> {
    let table = step
        .table
        .as_ref()
        .ok_or_else(|| format!("`{}` without a table", step.value.trim()))?;
    let mut out = Vec::with_capacity(table.rows.len());
    for row in &table.rows {
        if row.len() != 2 {
            return Err(format!(
                "expected two columns in `{}`, found {}",
                step.value.trim(),
                row.len()
            ));
        }
        out.push((row[0].trim().to_owned(), row[1].trim().to_owned()));
    }
    Ok(out)
}

fn docstring(step: &GherkinStep) -> Result<String, String> {
    step.docstring
        .clone()
        .ok_or_else(|| format!("`{}` without a query", step.value.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(ty: StepType, value: &str, docstring: Option<&str>) -> GherkinStep {
        GherkinStep {
            keyword: String::new(),
            ty,
            value: value.to_owned(),
            docstring: docstring.map(str::to_owned),
            table: None,
            span: gherkin::Span::default(),
            position: gherkin::LineCol::default(),
        }
    }

    #[test]
    fn classifies_setup_steps() {
        assert!(matches!(
            classify(&step(StepType::Given, "an empty graph", None)),
            Ok(Step::EmptyGraph)
        ));
        assert!(matches!(
            classify(&step(StepType::Given, "any graph", None)),
            Ok(Step::AnyGraph)
        ));
        assert!(matches!(
            classify(&step(StepType::Given, "the binary-tree-1 graph", None)),
            Ok(Step::NamedGraph(name)) if name == "binary-tree-1"
        ));
        assert!(matches!(
            classify(&step(StepType::Given, "having executed", Some("CREATE ()"))),
            Ok(Step::HavingExecuted(query)) if query == "CREATE ()"
        ));
    }

    #[test]
    fn classifies_error_steps() {
        let parsed = classify(&step(
            StepType::Then,
            "a TypeError should be raised at runtime: InvalidArgumentType",
            None,
        ))
        .expect("classify");
        match parsed {
            Step::Error {
                kind,
                phase,
                detail,
            } => {
                assert_eq!(kind, "TypeError");
                assert_eq!(phase, ExpectedPhase::Runtime);
                assert_eq!(detail.as_deref(), Some("InvalidArgumentType"));
            }
            other => panic!("expected error step, got {other:?}"),
        }
    }

    #[test]
    fn unknown_steps_are_errors() {
        assert!(classify(&step(StepType::When, "doing something else", None)).is_err());
    }
}
