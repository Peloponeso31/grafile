//! Run results, gate verdicts and the JSON report.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::allowlist::Detail;

/// Outcome of running one scenario.
#[derive(Debug, Clone)]
pub enum Status {
    /// The scenario passed.
    Passed,
    /// The scenario failed (a language gap or a deviation).
    Failed(String),
    /// The harness could not evaluate the scenario.
    HarnessError(String),
}

/// Result of running one scenario.
#[derive(Debug, Clone)]
pub struct ScenarioResult {
    /// Feature file path relative to the features directory.
    pub file: String,
    /// Scenario name, with an example suffix for expanded outlines.
    pub name: String,
    /// Outcome.
    pub status: Status,
    /// Informational notes (for example, error-detail differences).
    pub notes: Vec<String>,
}

/// The gate verdict for one scenario.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Passed and not on the allow-list.
    Passed,
    /// Failed and on the allow-list.
    Allowlisted(Detail),
    /// Failed and not on the allow-list.
    UnexpectedFailure,
    /// The harness could not evaluate the scenario; never allow-listable.
    HarnessError,
    /// Passed but still on the allow-list.
    StaleAllowlist(Detail),
}

/// Totals for a group of scenarios.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Totals {
    /// Scenarios executed.
    pub total: usize,
    /// Scenarios that passed.
    pub passed: usize,
    /// Failures covered by the allow-list.
    pub allowlisted: usize,
    /// Failures not covered by the allow-list.
    pub unexpected_failures: usize,
    /// Scenarios the harness could not evaluate.
    pub harness_errors: usize,
    /// Allow-list entries whose scenario now passes.
    pub stale_allowlist: usize,
}

/// One entry in a report failure list.
#[derive(Debug, Clone, Serialize)]
pub struct FailureEntry {
    /// Feature file.
    pub file: String,
    /// Scenario name.
    pub name: String,
    /// Failure or harness message.
    pub message: String,
}

/// The JSON report.
#[derive(Debug, Serialize)]
pub struct Report {
    /// openCypher TCK revision, when known.
    pub tck_version: String,
    /// Overall totals.
    pub totals: Totals,
    /// Passing scenarios over applicable scenarios (total minus allow-listed).
    pub pass_rate: f64,
    /// Failures not covered by the allow-list.
    pub unexpected_failures: Vec<FailureEntry>,
    /// Scenarios the harness could not evaluate.
    pub harness_errors: Vec<FailureEntry>,
    /// Allow-list entries whose scenario now passes.
    pub stale_allowlist: Vec<FailureEntry>,
    /// Informational notes, such as error-detail differences.
    pub notes: Vec<FailureEntry>,
    /// Totals per feature file.
    pub per_file: BTreeMap<String, Totals>,
}

impl Report {
    /// Assemble a report from results and verdicts.
    pub fn build(tck_version: &str, results: &[ScenarioResult], verdicts: &[Verdict]) -> Self {
        let mut totals = Totals::default();
        let mut per_file: BTreeMap<String, Totals> = BTreeMap::new();
        let mut unexpected_failures = Vec::new();
        let mut harness_errors = Vec::new();
        let mut stale_allowlist = Vec::new();
        let mut notes = Vec::new();
        for (result, verdict) in results.iter().zip(verdicts) {
            totals.total += 1;
            let file_totals = per_file.entry(result.file.clone()).or_default();
            file_totals.total += 1;
            let message = match &result.status {
                Status::Passed => String::new(),
                Status::Failed(message) | Status::HarnessError(message) => message.clone(),
            };
            let entry = || FailureEntry {
                file: result.file.clone(),
                name: result.name.clone(),
                message: message.clone(),
            };
            match verdict {
                Verdict::Passed => {
                    totals.passed += 1;
                    file_totals.passed += 1;
                }
                Verdict::Allowlisted(_) => {
                    totals.allowlisted += 1;
                    file_totals.allowlisted += 1;
                }
                Verdict::UnexpectedFailure => {
                    totals.unexpected_failures += 1;
                    file_totals.unexpected_failures += 1;
                    unexpected_failures.push(entry());
                }
                Verdict::HarnessError => {
                    totals.harness_errors += 1;
                    file_totals.harness_errors += 1;
                    harness_errors.push(entry());
                }
                Verdict::StaleAllowlist(_) => {
                    totals.stale_allowlist += 1;
                    file_totals.stale_allowlist += 1;
                    stale_allowlist.push(entry());
                }
            }
            for note in &result.notes {
                notes.push(FailureEntry {
                    file: result.file.clone(),
                    name: result.name.clone(),
                    message: note.clone(),
                });
            }
        }
        let applicable = totals.total.saturating_sub(totals.allowlisted);
        let pass_rate = if applicable == 0 {
            1.0
        } else {
            totals.passed as f64 / applicable as f64
        };
        Self {
            tck_version: tck_version.to_owned(),
            totals,
            pass_rate,
            unexpected_failures,
            harness_errors,
            stale_allowlist,
            notes,
            per_file,
        }
    }

    /// Whether the gate passes: no unexpected failures, no harness errors and
    /// no stale allow-list entries.
    pub fn gate_passes(&self) -> bool {
        self.totals.unexpected_failures == 0
            && self.totals.harness_errors == 0
            && self.totals.stale_allowlist == 0
    }

    /// Write the report as pretty JSON.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be written.
    pub fn save(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create `{}`", parent.display()))?;
        }
        let text = serde_json::to_string_pretty(self).context("cannot serialize report")?;
        std::fs::write(path, format!("{text}\n"))
            .with_context(|| format!("cannot write report `{}`", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_pass_rate() {
        let results = vec![
            ScenarioResult {
                file: "a.feature".into(),
                name: "one".into(),
                status: Status::Passed,
                notes: Vec::new(),
            },
            ScenarioResult {
                file: "a.feature".into(),
                name: "two".into(),
                status: Status::Failed("nope".into()),
                notes: Vec::new(),
            },
            ScenarioResult {
                file: "b.feature".into(),
                name: "three".into(),
                status: Status::Failed("gap".into()),
                notes: Vec::new(),
            },
        ];
        let verdicts = vec![
            Verdict::Passed,
            Verdict::UnexpectedFailure,
            Verdict::Allowlisted(Detail {
                reason: crate::allowlist::Reason::NotImplemented,
                note: None,
            }),
        ];
        let report = Report::build("2024.3", &results, &verdicts);
        assert_eq!(report.totals.total, 3);
        assert_eq!(report.totals.passed, 1);
        assert_eq!(report.totals.allowlisted, 1);
        assert_eq!(report.totals.unexpected_failures, 1);
        assert!((report.pass_rate - 0.5).abs() < f64::EPSILON);
        assert!(!report.gate_passes());
        assert_eq!(report.per_file["a.feature"].total, 2);
    }
}
