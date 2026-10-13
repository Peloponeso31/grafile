//! Feature discovery, orchestration, allow-list handling and the CI gate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use indicatif::{ProgressBar, ProgressStyle};
use rayon::prelude::*;

use crate::allowlist::{AllowList, Detail, Reason};
use crate::report::{FailureEntry, Report, ScenarioResult, Status, Totals, Verdict};
use crate::scenario::{self, GraphScripts, Scenario};
use crate::steps;

/// The openCypher revision the vendored TCK is pinned to.
pub const TCK_VERSION: &str = "2024.3";

/// Runner configuration.
#[derive(Debug, Clone)]
pub struct Config {
    /// Directory containing the TCK `.feature` files.
    pub features_dir: PathBuf,
    /// Directory containing the named-graph scripts.
    pub graphs_dir: PathBuf,
    /// Allow-list path.
    pub allowlist_path: PathBuf,
    /// Report output path.
    pub report_path: PathBuf,
    /// Only run scenarios whose `file: name` contains this substring.
    pub filter: Option<String>,
    /// Regenerate the allow-list from the current results instead of gating.
    pub write_allowlist: bool,
    /// Suppress the progress bar.
    pub quiet: bool,
}

impl Config {
    /// Configuration with default paths relative to the repository root.
    pub fn new() -> Self {
        Self {
            features_dir: PathBuf::from("third_party/opencypher/tck/features"),
            graphs_dir: PathBuf::from("third_party/opencypher/tck/graphs"),
            allowlist_path: PathBuf::from("tck/allowlist.json"),
            report_path: PathBuf::from("tck/report.json"),
            filter: None,
            write_allowlist: false,
            quiet: false,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

/// Run the TCK suite and return whether the gate passes.
///
/// # Errors
///
/// Returns an error when features cannot be discovered or parsed, or when the
/// allow-list or report cannot be read or written.
pub fn run(config: &Config) -> Result<bool> {
    if !config.features_dir.is_dir() {
        anyhow::bail!(
            "features directory `{}` does not exist; run `git submodule update --init`",
            config.features_dir.display()
        );
    }
    let scenarios = collect(config)?;
    if scenarios.is_empty() {
        anyhow::bail!("no scenarios found in `{}`", config.features_dir.display());
    }
    let graphs = Arc::new(GraphScripts::load(&config.graphs_dir)?);
    let results = execute_all(&scenarios, &graphs, config);

    let mut allowlist = AllowList::load(&config.allowlist_path)?;
    if config.write_allowlist {
        let harness_errors: Vec<&ScenarioResult> = results
            .iter()
            .filter(|result| matches!(result.status, Status::HarnessError(_)))
            .collect();
        if !harness_errors.is_empty() {
            eprintln!(
                "refusing to write the allow-list: {} harness error(s) must be fixed first",
                harness_errors.len()
            );
            for result in harness_errors.iter().take(20) {
                if let Status::HarnessError(message) = &result.status {
                    eprintln!("  {}: {}: {message}", result.file, result.name);
                }
            }
            return Ok(false);
        }
        let fresh = regenerate_allowlist(&results, &allowlist);
        fresh.save(&config.allowlist_path)?;
        println!(
            "wrote {} allow-list entries to {}",
            fresh
                .files
                .values()
                .map(|entries| entries.len())
                .sum::<usize>(),
            config.allowlist_path.display()
        );
        allowlist = fresh;
    }

    let filtering = config.filter.is_some();
    let verdicts = compute_verdicts(&results, &allowlist, filtering);
    let report = Report::build(TCK_VERSION, &results, &verdicts);
    report.save(&config.report_path)?;
    print_summary(&report, config);
    Ok(report.gate_passes())
}

fn collect(config: &Config) -> Result<Vec<Scenario>> {
    let mut scenarios = collect_scenarios(&config.features_dir)?;
    if let Some(filter) = &config.filter {
        scenarios.retain(|scenario| {
            format!("{}: {}", scenario.file, scenario.name).contains(filter.as_str())
        });
    }
    Ok(scenarios)
}

/// Parse every `.feature` file under `features_dir` into scenarios.
///
/// # Errors
///
/// Returns an error when a feature file cannot be read or parsed.
pub fn collect_scenarios(features_dir: &Path) -> Result<Vec<Scenario>> {
    let mut paths = Vec::new();
    walk(features_dir, &mut paths)?;
    paths.sort();
    let mut scenarios = Vec::new();
    for path in &paths {
        let relative = path
            .strip_prefix(features_dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read `{}`", path.display()))?;
        let text = escape_table_backslashes(&text);
        let feature = gherkin::Feature::parse(&text, gherkin::GherkinEnv::default())
            .with_context(|| format!("cannot parse `{}`", path.display()))?;
        collect_feature(&feature, &relative, &mut scenarios);
    }
    Ok(scenarios)
}

/// Rewrite table-cell backslashes into Gherkin escapes the parser accepts.
///
/// Gherkin table cells only allow the escapes `\\`, `\|` and `\n`; the TCK
/// also writes Cypher string escapes such as `\'` and `\t` inside cells,
/// which strict parsers reject. Doubling those backslashes keeps the cell
/// text intact after Gherkin unescaping, so the expected-value parser can
/// apply Cypher escape rules to it.
fn escape_table_backslashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut in_docstring = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("\"\"\"") || trimmed.starts_with("'''") {
            in_docstring = !in_docstring;
            out.push_str(line);
            continue;
        }
        if in_docstring || !trimmed.starts_with('|') {
            out.push_str(line);
            continue;
        }
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.peek() {
                Some('\\') | Some('|') => {
                    out.push('\\');
                    out.push(chars.next().expect("peeked character is present"));
                }
                _ => out.push_str("\\\\"),
            }
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("cannot read `{}`", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("cannot read `{}`", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "feature") {
            out.push(path);
        }
    }
    Ok(())
}

fn collect_feature(feature: &gherkin::Feature, file: &str, out: &mut Vec<Scenario>) {
    let background = feature
        .background
        .as_ref()
        .map(|background| background.steps.clone())
        .unwrap_or_default();
    for scenario in &feature.scenarios {
        collect_scenario(&background, scenario, file, out);
    }
    for rule in &feature.rules {
        for scenario in &rule.scenarios {
            collect_scenario(&background, scenario, file, out);
        }
    }
}

fn collect_scenario(
    background: &[gherkin::Step],
    scenario: &gherkin::Scenario,
    file: &str,
    out: &mut Vec<Scenario>,
) {
    let examples = example_rows(scenario);
    if examples.is_empty() {
        out.push(Scenario {
            file: file.to_owned(),
            name: scenario.name.clone(),
            steps: classify_steps(background, &scenario.steps),
        });
        return;
    }
    for (index, values) in examples.iter().enumerate() {
        let substituted: Vec<gherkin::Step> = background
            .iter()
            .chain(&scenario.steps)
            .map(|step| substitute(step, values))
            .collect();
        out.push(Scenario {
            file: file.to_owned(),
            name: format!("{} (example {})", scenario.name, index + 1),
            steps: classify_steps(&[], &substituted),
        });
    }
}

fn classify_steps(
    background: &[gherkin::Step],
    steps_list: &[gherkin::Step],
) -> std::result::Result<Vec<steps::Step>, String> {
    background
        .iter()
        .chain(steps_list)
        .map(steps::classify)
        .collect()
}

fn example_rows(scenario: &gherkin::Scenario) -> Vec<Vec<(String, String)>> {
    let mut rows = Vec::new();
    for examples in &scenario.examples {
        let Some(table) = &examples.table else {
            continue;
        };
        let Some(header) = table.rows.first() else {
            continue;
        };
        for row in &table.rows[1..] {
            rows.push(
                header
                    .iter()
                    .cloned()
                    .zip(row.iter().cloned())
                    .collect::<Vec<_>>(),
            );
        }
    }
    rows
}

fn substitute(step: &gherkin::Step, values: &[(String, String)]) -> gherkin::Step {
    let mut out = step.clone();
    out.value = replace_placeholders(&out.value, values);
    out.docstring = out
        .docstring
        .as_ref()
        .map(|text| replace_placeholders(text, values));
    if let Some(table) = &mut out.table {
        for row in &mut table.rows {
            for cell in row {
                *cell = replace_placeholders(cell, values);
            }
        }
    }
    out
}

fn replace_placeholders(text: &str, values: &[(String, String)]) -> String {
    let mut out = text.to_owned();
    for (name, value) in values {
        out = out.replace(&format!("<{name}>"), value);
    }
    out
}

fn execute_all(
    scenarios: &[Scenario],
    graphs: &GraphScripts,
    config: &Config,
) -> Vec<ScenarioResult> {
    let progress = if config.quiet {
        ProgressBar::hidden()
    } else {
        let bar = ProgressBar::new(scenarios.len() as u64);
        bar.set_style(
            ProgressStyle::with_template("{bar:40} {pos}/{len} {msg}")
                .expect("static progress template is valid"),
        );
        bar
    };
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let results: Vec<ScenarioResult> = scenarios
        .par_iter()
        .map(|scenario| {
            progress.set_message(format!("{}: {}", scenario.file, scenario.name));
            // A panic in the engine must not abort the whole run: record it
            // as a failure of the scenario that triggered it.
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                scenario::run(scenario, graphs)
            }))
            .unwrap_or_else(|payload| ScenarioResult {
                file: scenario.file.clone(),
                name: scenario.name.clone(),
                status: Status::Failed(format!("panicked: {}", panic_message(&payload))),
                notes: Vec::new(),
            });
            progress.inc(1);
            result
        })
        .collect();
    std::panic::set_hook(hook);
    progress.finish_and_clear();
    results
}

fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic payload".to_owned()
    }
}

fn regenerate_allowlist(results: &[ScenarioResult], previous: &AllowList) -> AllowList {
    let mut per_file: std::collections::BTreeMap<&str, Vec<&ScenarioResult>> =
        std::collections::BTreeMap::new();
    for result in results {
        per_file.entry(&result.file).or_default().push(result);
    }
    let mut fresh = AllowList::default();
    for (file, scenarios) in per_file {
        let all_failed = scenarios
            .iter()
            .all(|result| matches!(result.status, Status::Failed(_)));
        if all_failed {
            // Collapse whole files into one entry to keep the list reviewable.
            let detail = previous.detail(file, "*").cloned().unwrap_or_else(|| {
                let message = scenarios
                    .first()
                    .and_then(|result| match &result.status {
                        Status::Failed(message) => Some(message.as_str()),
                        _ => None,
                    })
                    .unwrap_or("all scenarios in this file fail");
                Detail {
                    reason: Reason::NotImplemented,
                    note: Some(first_line(message)),
                }
            });
            fresh.insert(file, "*", detail);
            continue;
        }
        for result in scenarios {
            let Status::Failed(message) = &result.status else {
                continue;
            };
            let detail = previous
                .detail(file, &result.name)
                .cloned()
                .unwrap_or_else(|| Detail {
                    reason: Reason::NotImplemented,
                    note: Some(first_line(message)),
                });
            fresh.insert(file, &result.name, detail);
        }
    }
    fresh
}

fn first_line(message: &str) -> String {
    let line = message.lines().next().unwrap_or(message);
    if line.chars().count() > 200 {
        let mut truncated: String = line.chars().take(200).collect();
        truncated.push_str("...");
        truncated
    } else {
        line.to_owned()
    }
}

fn compute_verdicts(
    results: &[ScenarioResult],
    allowlist: &AllowList,
    filtering: bool,
) -> Vec<Verdict> {
    results
        .iter()
        .map(|result| {
            let detail = allowlist.detail(&result.file, &result.name);
            match (&result.status, detail) {
                (Status::HarnessError(_), _) => Verdict::HarnessError,
                (Status::Passed, None) => Verdict::Passed,
                (Status::Passed, Some(detail)) => {
                    if filtering {
                        Verdict::Passed
                    } else {
                        Verdict::StaleAllowlist(detail.clone())
                    }
                }
                (Status::Failed(_), Some(detail)) => Verdict::Allowlisted(detail.clone()),
                (Status::Failed(_), None) => Verdict::UnexpectedFailure,
            }
        })
        .collect()
}

fn print_summary(report: &Report, config: &Config) {
    let totals = &report.totals;
    let applicable = totals.total.saturating_sub(totals.allowlisted);
    println!("openCypher TCK {}", report.tck_version);
    println!(
        "  scenarios: {} ({} applicable, {} allow-listed)",
        totals.total, applicable, totals.allowlisted
    );
    println!(
        "  passed:    {} of {} applicable ({:.1}%)",
        totals.passed,
        applicable,
        report.pass_rate * 100.0
    );
    if !report.notes.is_empty() {
        println!("  notes:     {} (see the report)", report.notes.len());
    }
    print_entries("unexpected failures", &report.unexpected_failures);
    print_entries("harness errors", &report.harness_errors);
    print_entries("stale allow-list entries", &report.stale_allowlist);
    println!("  report:    {}", config.report_path.display());
    if let Some(filter) = &config.filter {
        println!("  filter:    `{filter}` (stale entries are not checked)");
    }
}

fn print_entries(label: &str, entries: &[FailureEntry]) {
    if entries.is_empty() {
        return;
    }
    println!("  {label}: {}", entries.len());
    for entry in entries.iter().take(40) {
        println!("    {}: {}: {}", entry.file, entry.name, entry.message);
    }
    if entries.len() > 40 {
        println!("    ... and {} more (see the report)", entries.len() - 40);
    }
}

/// Per-file totals, used by the console summary and tests.
pub fn per_file_totals(report: &Report) -> &std::collections::BTreeMap<String, Totals> {
    &report.per_file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_table_backslashes() {
        let input = r#"      | literal |
      | 'a\\\\bcn5t\'"\\\\//\\\\"\'' |
"#;
        let output = escape_table_backslashes(input);
        let expected = r#"      | literal |
      | 'a\\\\bcn5t\\'"\\\\//\\\\"\\'' |
"#;
        assert_eq!(output, expected);
    }

    #[test]
    fn doubles_cypher_escapes_in_cells() {
        let input = r#"      | '\''    |
"#;
        assert_eq!(
            escape_table_backslashes(input),
            r#"      | '\\''    |
"#
        );
    }

    #[test]
    fn leaves_docstrings_alone() {
        let input = r#"      """
      RETURN 'a\'b'
      """
      | 'x' |
"#;
        let output = escape_table_backslashes(input);
        assert!(output.contains(r"RETURN 'a\'b'"));
        assert!(output.ends_with("| 'x' |\n"));
    }

    #[test]
    fn substitutes_placeholders() {
        let values = vec![("sort".to_owned(), "bool ASC".to_owned())];
        assert_eq!(
            replace_placeholders("ORDER BY <sort> LIMIT 3", &values),
            "ORDER BY bool ASC LIMIT 3"
        );
    }

    #[test]
    fn collects_example_rows() {
        let scenario = gherkin::Scenario {
            keyword: "Scenario Outline".into(),
            name: "outline".into(),
            description: None,
            steps: Vec::new(),
            examples: vec![gherkin::Examples {
                keyword: "Examples".into(),
                name: None,
                description: None,
                table: Some(gherkin::Table {
                    rows: vec![
                        vec!["a".into(), "b".into()],
                        vec!["1".into(), "2".into()],
                        vec!["3".into(), "4".into()],
                    ],
                    span: gherkin::Span::default(),
                    position: gherkin::LineCol::default(),
                }),
                tags: Vec::new(),
                span: gherkin::Span::default(),
                position: gherkin::LineCol::default(),
            }],
            tags: Vec::new(),
            span: gherkin::Span::default(),
            position: gherkin::LineCol::default(),
        };
        let rows = example_rows(&scenario);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1][1], ("b".to_owned(), "4".to_owned()));
    }
}
