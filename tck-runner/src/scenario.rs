//! Execution of one TCK scenario against a fresh in-memory database.
//!
//! The harness maps scenario steps onto grafile's public API. Setup steps,
//! the query under test and its optional control query run through
//! [`grafile::api::Connection::query_with_phase`], so compile-time and runtime
//! failures are distinguished. Comparisons and side-effect metrics use engine
//! introspection rather than Cypher, so the harness itself does not depend on
//! query features grafile may not support yet.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result as AnyResult};
use grafile::api::{Connection, Params, Phase, PhaseError, Rows};
use grafile::cypher::split_statements;
use grafile::error::ErrorCode;
use grafile::pager::DEFAULT_PAGE_SIZE;
use grafile::vfs::mem::MemVfs;

use crate::expected::{self, Expected};
use crate::metrics::{self, Delta};
use crate::report::{ScenarioResult, Status};
use crate::steps::{ExpectedPhase, ListOrder, RowOrder, Step};
use crate::value;

/// A scenario prepared for execution: classified steps, or the text of the
/// first step the harness does not understand.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// Feature file path relative to the features directory.
    pub file: String,
    /// Scenario name, with an example suffix for expanded outlines.
    pub name: String,
    /// Classified steps, or the unrecognized step text.
    pub steps: std::result::Result<Vec<Step>, String>,
}

/// The named graph setup scripts, loaded once per run.
#[derive(Debug, Default)]
pub struct GraphScripts {
    scripts: HashMap<String, String>,
}

impl GraphScripts {
    /// Load every `<graphs_dir>/<name>/<name>.cypher` script.
    ///
    /// # Errors
    ///
    /// Returns an error when the graphs directory cannot be read.
    pub fn load(dir: &Path) -> AnyResult<Self> {
        let mut scripts = HashMap::new();
        let entries = std::fs::read_dir(dir)
            .with_context(|| format!("cannot read graphs directory `{}`", dir.display()))?;
        for entry in entries {
            let entry = entry.with_context(|| format!("cannot read `{}`", dir.display()))?;
            if !entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let script_path = entry.path().join(format!("{name}.cypher"));
            if script_path.exists() {
                let text = std::fs::read_to_string(&script_path)
                    .with_context(|| format!("cannot read `{}`", script_path.display()))?;
                scripts.insert(name, text);
            }
        }
        Ok(Self { scripts })
    }

    /// The script for a named graph, if present.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.scripts.get(name).map(String::as_str)
    }
}

/// Run a scenario and produce its result.
pub fn run(scenario: &Scenario, graphs: &GraphScripts) -> ScenarioResult {
    let steps = match &scenario.steps {
        Ok(steps) => steps,
        Err(text) => {
            return ScenarioResult {
                file: scenario.file.clone(),
                name: scenario.name.clone(),
                status: Status::HarnessError(format!("unrecognized step: `{text}`")),
                notes: Vec::new(),
            };
        }
    };
    let mut runner = match Runner::new(graphs) {
        Ok(runner) => runner,
        Err(message) => {
            return ScenarioResult {
                file: scenario.file.clone(),
                name: scenario.name.clone(),
                status: Status::HarnessError(message),
                notes: Vec::new(),
            };
        }
    };
    match runner.execute(steps) {
        Err(message) => ScenarioResult {
            file: scenario.file.clone(),
            name: scenario.name.clone(),
            status: Status::HarnessError(message),
            notes: runner.notes,
        },
        Ok(()) if runner.failures.is_empty() => ScenarioResult {
            file: scenario.file.clone(),
            name: scenario.name.clone(),
            status: Status::Passed,
            notes: runner.notes,
        },
        Ok(()) => ScenarioResult {
            file: scenario.file.clone(),
            name: scenario.name.clone(),
            status: Status::Failed(runner.failures.join("; ")),
            notes: runner.notes,
        },
    }
}

struct Runner<'a> {
    graphs: &'a GraphScripts,
    conn: Connection,
    params: Params,
    result: Option<std::result::Result<Rows, PhaseError>>,
    side_effects: Option<Delta>,
    failures: Vec<String>,
    notes: Vec<String>,
    aborted: bool,
}

impl<'a> Runner<'a> {
    fn new(graphs: &'a GraphScripts) -> std::result::Result<Self, String> {
        let conn = Connection::create_with_vfs(
            Arc::new(MemVfs::new()),
            Path::new("tck.db"),
            DEFAULT_PAGE_SIZE,
        )
        .map_err(|err| format!("cannot create in-memory database: {err}"))?;
        Ok(Self {
            graphs,
            conn,
            params: Params::new(),
            result: None,
            side_effects: None,
            failures: Vec::new(),
            notes: Vec::new(),
            aborted: false,
        })
    }

    fn execute(&mut self, steps: &[Step]) -> std::result::Result<(), String> {
        let needs_metrics = steps
            .iter()
            .any(|step| matches!(step, Step::NoSideEffects | Step::SideEffects(_)));
        for step in steps {
            match step {
                Step::EmptyGraph => {}
                // The TCK guarantees that scenarios using "any graph" do not
                // depend on graph content; binary-tree-1 provides non-trivial
                // content for queries that iterate without asserting shape.
                Step::AnyGraph => self.setup_script("binary-tree-1")?,
                Step::NamedGraph(name) => self.setup_script(name)?,
                Step::HavingExecuted(query) => self.setup_query(query)?,
                Step::Parameters(pairs) => self.bind_parameters(pairs)?,
                Step::Procedure(text) => {
                    self.failures
                        .push(format!("procedures are not supported: `{text}`"));
                    self.aborted = true;
                }
                Step::ExecutingQuery(query) => self.execute_query(query, needs_metrics)?,
                Step::ExecutingControlQuery(query) => self.execute_query(query, false)?,
                Step::Result {
                    table,
                    order,
                    list_order,
                } => self.check_result(table, *order, *list_order)?,
                Step::ResultEmpty => self.check_empty()?,
                Step::NoSideEffects => self.check_no_side_effects()?,
                Step::SideEffects(pairs) => self.check_side_effects(pairs)?,
                Step::Error {
                    kind,
                    phase,
                    detail,
                } => self.check_error(kind, *phase, detail.as_deref()),
            }
            if self.aborted {
                break;
            }
        }
        Ok(())
    }

    fn setup_script(&mut self, name: &str) -> std::result::Result<(), String> {
        let Some(script) = self.graphs.get(name) else {
            return Err(format!("named graph `{name}` is not available"));
        };
        self.setup_query(script)
    }

    fn setup_query(&mut self, script: &str) -> std::result::Result<(), String> {
        for statement in split_statements(script) {
            if statement.trim().is_empty() {
                continue;
            }
            if let Err(err) = self.conn.query(&statement, &self.params) {
                self.failures.push(format!("setup failed: {err}"));
                self.aborted = true;
                break;
            }
        }
        Ok(())
    }

    fn bind_parameters(&mut self, pairs: &[(String, String)]) -> std::result::Result<(), String> {
        for (name, raw) in pairs {
            let expected = expected::parse(raw)
                .map_err(|err| format!("parameter `{name}` is not valid: {err}"))?;
            let value = value::to_parameter(&expected)
                .map_err(|err| format!("parameter `{name}` cannot be bound: {err}"))?;
            self.params.insert(name.clone(), value);
        }
        Ok(())
    }

    fn execute_query(
        &mut self,
        source: &str,
        measure_metrics: bool,
    ) -> std::result::Result<(), String> {
        let before = if measure_metrics {
            Some(metrics::measure(self.conn.database_mut())?)
        } else {
            None
        };
        let result = self.conn.query_with_phase(source, &self.params);
        if let Some(before) = before {
            let after = metrics::measure(self.conn.database_mut())?;
            self.side_effects = Some(Delta::between(before, after));
        }
        self.result = Some(result);
        Ok(())
    }

    fn check_result(
        &mut self,
        table: &[Vec<String>],
        order: RowOrder,
        list_order: ListOrder,
    ) -> std::result::Result<(), String> {
        let rows = match self.result.as_ref() {
            None => return Err("no query result is available".to_owned()),
            Some(Err(err)) => {
                let message = format!("expected rows, got a {} error: {err}", err.phase());
                self.failures.push(message);
                return Ok(());
            }
            Some(Ok(rows)) => rows.clone(),
        };
        let Some(header) = table.first() else {
            return Err("expected result table is empty".to_owned());
        };
        if header.len() != rows.columns().len() {
            self.failures.push(format!(
                "expected {} columns ({header:?}), got {} ({:?})",
                header.len(),
                rows.columns().len(),
                rows.columns()
            ));
            return Ok(());
        }
        for (index, name) in header.iter().enumerate() {
            if name.trim() != rows.columns()[index].trim() {
                self.failures.push(format!(
                    "expected column `{}`, got `{}`",
                    name.trim(),
                    rows.columns()[index].trim()
                ));
                return Ok(());
            }
        }
        let mut expected_rows = Vec::with_capacity(table.len().saturating_sub(1));
        for row in &table[1..] {
            let mut cells = Vec::with_capacity(row.len());
            for cell in row {
                cells.push(
                    expected::parse(cell)
                        .map_err(|err| format!("expected cell `{cell}`: {err}"))?,
                );
            }
            expected_rows.push(cells);
        }
        let mut actual_rows = Vec::with_capacity(rows.len());
        for row in rows.rows() {
            let mut cells = Vec::with_capacity(row.len());
            for value in row {
                cells.push(
                    value::normalize(self.conn.database_mut(), value)
                        .map_err(|err| format!("cannot normalize result value: {err}"))?,
                );
            }
            actual_rows.push(cells);
        }
        if expected_rows.len() != actual_rows.len() {
            self.failures.push(format!(
                "expected {} rows, got {}",
                expected_rows.len(),
                actual_rows.len()
            ));
            return Ok(());
        }
        match order {
            RowOrder::In => {
                for (index, (expected_row, actual_row)) in
                    expected_rows.iter().zip(&actual_rows).enumerate()
                {
                    if let Err(err) = compare_row(expected_row, actual_row, list_order) {
                        self.failures.push(format!("row {index}: {err}"));
                    }
                }
            }
            RowOrder::Any => {
                let mut expected_keys: Vec<String> = expected_rows
                    .iter()
                    .map(|row| row_key_expected(row))
                    .collect();
                let mut actual_keys: Vec<String> =
                    actual_rows.iter().map(|row| row_key_actual(row)).collect();
                expected_keys.sort();
                actual_keys.sort();
                if expected_keys != actual_keys {
                    let index = expected_keys
                        .iter()
                        .zip(&actual_keys)
                        .position(|(a, b)| a != b)
                        .unwrap_or(0);
                    let expected_row = &expected_rows[index.min(expected_rows.len() - 1)];
                    let actual_row = &actual_rows[index.min(actual_rows.len() - 1)];
                    self.failures.push(format!(
                        "rows differ: expected {}, got {}",
                        row_display_expected(expected_row),
                        row_display_actual(actual_row)
                    ));
                }
            }
        }
        Ok(())
    }

    fn check_empty(&mut self) -> std::result::Result<(), String> {
        let Some(result) = self.result.as_ref() else {
            return Err("no query result is available".to_owned());
        };
        match result {
            Ok(rows) if rows.is_empty() => Ok(()),
            Ok(rows) => {
                self.failures
                    .push(format!("expected no rows, got {}", rows.len()));
                Ok(())
            }
            Err(err) => {
                self.failures.push(format!(
                    "expected no rows, got a {} error: {err}",
                    err.phase()
                ));
                Ok(())
            }
        }
    }

    fn check_no_side_effects(&mut self) -> std::result::Result<(), String> {
        let Some(delta) = self.side_effects else {
            return Err("side effects were not measured".to_owned());
        };
        if !delta.is_zero() {
            self.failures.push(format!(
                "expected no side effects, got +{} nodes, -{} nodes, \
                 +{} relationships, -{} relationships, +{} properties, \
                 -{} properties, +{} labels, -{} labels",
                delta.added_nodes,
                delta.removed_nodes,
                delta.added_relationships,
                delta.removed_relationships,
                delta.added_properties,
                delta.removed_properties,
                delta.added_labels,
                delta.removed_labels
            ));
        }
        Ok(())
    }

    fn check_side_effects(
        &mut self,
        pairs: &[(String, String)],
    ) -> std::result::Result<(), String> {
        let Some(delta) = self.side_effects else {
            return Err("side effects were not measured".to_owned());
        };
        let mut expected: HashMap<(char, &str), i64> = HashMap::new();
        for (raw_name, raw_value) in pairs {
            let (sign, name) = if let Some(name) = raw_name.strip_prefix('+') {
                ('+', name)
            } else if let Some(name) = raw_name.strip_prefix('-') {
                ('-', name)
            } else {
                return Err(format!("malformed side-effect metric `{raw_name}`"));
            };
            if delta.count(sign, name).is_none() {
                return Err(format!("unknown side-effect metric `{raw_name}`"));
            }
            let count: i64 = raw_value
                .parse()
                .map_err(|_| format!("malformed side-effect count `{raw_value}`"))?;
            expected.insert((sign, name), count);
        }
        for name in ["nodes", "relationships", "properties", "labels"] {
            for sign in ['+', '-'] {
                let want = expected.get(&(sign, name)).copied().unwrap_or(0);
                let got = delta.count(sign, name).unwrap_or(0);
                if want != got {
                    self.failures
                        .push(format!("expected {sign}{name} {want}, got {got}"));
                }
            }
        }
        Ok(())
    }

    fn check_error(&mut self, kind: &str, phase: ExpectedPhase, detail: Option<&str>) {
        let Some(result) = self.result.as_ref() else {
            self.failures
                .push("no query result is available".to_owned());
            return;
        };
        match result {
            Ok(rows) => {
                self.failures.push(format!(
                    "expected {kind} at {phase}, but the query returned {} rows",
                    rows.len()
                ));
            }
            Err(err) => {
                let produced = tck_kind(err.phase(), err.error().code());
                let phase_ok = match phase {
                    ExpectedPhase::Any => true,
                    ExpectedPhase::Compile => err.phase() == Phase::Compile,
                    ExpectedPhase::Runtime => err.phase() == Phase::Runtime,
                };
                if !phase_ok || !kind_matches(kind, produced) {
                    self.failures.push(format!(
                        "expected {kind} at {phase}, got {produced} at {}",
                        err.phase()
                    ));
                }
                if let Some(detail) = detail
                    && !err
                        .error()
                        .message()
                        .to_lowercase()
                        .contains(&detail.to_lowercase())
                {
                    self.notes.push(format!(
                        "expected error detail `{detail}`, got `{}`",
                        err.error().message()
                    ));
                }
            }
        }
    }
}

fn compare_row(
    expected: &[Expected],
    actual: &[value::Actual],
    list_order: ListOrder,
) -> std::result::Result<(), String> {
    if expected.len() != actual.len() {
        return Err(format!(
            "expected {} values, got {}",
            expected.len(),
            actual.len()
        ));
    }
    for (expected, actual) in expected.iter().zip(actual) {
        value::compare(expected, actual, to_list_order(list_order))?;
    }
    Ok(())
}

fn to_list_order(list_order: ListOrder) -> value::ListOrder {
    match list_order {
        ListOrder::Ordered => value::ListOrder::Ordered,
        ListOrder::IgnoreOrder => value::ListOrder::IgnoreOrder,
    }
}

fn row_key_expected(row: &[Expected]) -> String {
    row.iter()
        .map(value::canonical_expected)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn row_key_actual(row: &[value::Actual]) -> String {
    row.iter()
        .map(value::canonical_actual)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn row_display_expected(row: &[Expected]) -> String {
    row.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ")
}

fn row_display_actual(row: &[value::Actual]) -> String {
    row.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ")
}

/// Map a grafile error onto the TCK's error taxonomy.
///
/// The mapping is phase-aware: at compile time a missing entity is an
/// undefined variable or label, which openCypher reports as a syntax error,
/// while at runtime it is a missing entity.
fn tck_kind(phase: Phase, code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Syntax => "SyntaxError",
        ErrorCode::Unsupported => "SemanticError",
        ErrorCode::NotFound => match phase {
            Phase::Compile => "SyntaxError",
            Phase::Runtime => "EntityNotFound",
        },
        ErrorCode::Type => "TypeError",
        ErrorCode::Constraint => "ConstraintVerificationFailed",
        ErrorCode::Exists => "ConstraintValidationFailed",
        ErrorCode::Range | ErrorCode::Misuse => "ArgumentError",
        _ => "Error",
    }
}

fn kind_matches(expected: &str, produced: &str) -> bool {
    if expected == produced {
        return true;
    }
    // The TCK distinguishes finer-grained error kinds than grafile; accept
    // the same family when the produced kind is the generic member.
    if expected.ends_with("NotFound") && produced.ends_with("NotFound") {
        return true;
    }
    if expected.starts_with("Constraint") && produced.starts_with("Constraint") {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_error_kinds() {
        assert_eq!(tck_kind(Phase::Compile, ErrorCode::Syntax), "SyntaxError");
        assert_eq!(
            tck_kind(Phase::Compile, ErrorCode::Unsupported),
            "SemanticError"
        );
        assert_eq!(tck_kind(Phase::Compile, ErrorCode::Range), "ArgumentError");
        assert_eq!(tck_kind(Phase::Compile, ErrorCode::NotFound), "SyntaxError");
        assert_eq!(
            tck_kind(Phase::Runtime, ErrorCode::NotFound),
            "EntityNotFound"
        );
        assert!(kind_matches("EntityNotFound", "EntityNotFound"));
        assert!(kind_matches("PropertyNotFound", "EntityNotFound"));
        assert!(kind_matches(
            "ConstraintValidationFailed",
            "ConstraintVerificationFailed"
        ));
        assert!(!kind_matches("SyntaxError", "TypeError"));
    }

    #[test]
    fn runs_a_passing_scenario() {
        let scenario = Scenario {
            file: "test.feature".into(),
            name: "returns one".into(),
            steps: Ok(vec![
                Step::EmptyGraph,
                Step::ExecutingQuery("RETURN 1 AS n".into()),
                Step::Result {
                    table: vec![vec!["n".into()], vec!["1".into()]],
                    order: RowOrder::Any,
                    list_order: ListOrder::Ordered,
                },
            ]),
        };
        let result = run(&scenario, &GraphScripts::default());
        assert!(matches!(result.status, Status::Passed), "{result:?}");
    }

    #[test]
    fn reports_a_failing_scenario() {
        let scenario = Scenario {
            file: "test.feature".into(),
            name: "returns two".into(),
            steps: Ok(vec![
                Step::EmptyGraph,
                Step::ExecutingQuery("RETURN 1 AS n".into()),
                Step::Result {
                    table: vec![vec!["n".into()], vec!["2".into()]],
                    order: RowOrder::Any,
                    list_order: ListOrder::Ordered,
                },
            ]),
        };
        let result = run(&scenario, &GraphScripts::default());
        assert!(matches!(result.status, Status::Failed(_)), "{result:?}");
    }

    #[test]
    fn unknown_steps_are_harness_errors() {
        let scenario = Scenario {
            file: "test.feature".into(),
            name: "odd".into(),
            steps: Err("do a barrel roll".into()),
        };
        let result = run(&scenario, &GraphScripts::default());
        assert!(
            matches!(result.status, Status::HarnessError(_)),
            "{result:?}"
        );
    }
}
