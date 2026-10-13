//! End-to-end tests for the TCK harness using small fixture features.

use std::path::{Path, PathBuf};

use tck_runner::runner::{self, Config};

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn config(output_dir: &Path) -> Config {
    Config {
        features_dir: fixtures().join("features"),
        graphs_dir: fixtures().join("graphs"),
        allowlist_path: output_dir.join("allowlist.json"),
        report_path: output_dir.join("report.json"),
        filter: None,
        write_allowlist: false,
        quiet: true,
    }
}

fn read_report(path: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(path).expect("read report");
    serde_json::from_str(&text).expect("parse report")
}

#[test]
fn gate_fails_without_allowlist_then_bootstraps() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());

    assert!(!runner::run(&cfg).expect("run"), "gate must fail");
    let report = read_report(&cfg.report_path);
    assert_eq!(report["totals"]["total"], 5);
    assert_eq!(report["totals"]["passed"], 4);
    assert_eq!(report["totals"]["unexpected_failures"], 1);
    assert_eq!(report["totals"]["harness_errors"], 0);
    assert_eq!(
        report["unexpected_failures"][0]["name"],
        "[5] A deliberate mismatch"
    );

    cfg.write_allowlist = true;
    assert!(runner::run(&cfg).expect("bootstrap"), "gate must pass");
    cfg.write_allowlist = false;
    assert!(runner::run(&cfg).expect("rerun"), "gate must stay green");

    let allowlist: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&cfg.allowlist_path).expect("read allow-list"),
    )
    .expect("parse allow-list");
    assert_eq!(
        allowlist["files"]["basic.feature"]["[5] A deliberate mismatch"]["reason"],
        "not-implemented"
    );
}

#[test]
fn stale_allowlist_entries_fail_the_gate() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    cfg.write_allowlist = true;
    assert!(runner::run(&cfg).expect("bootstrap"));
    // Allow-list a passing scenario on top of the generated list.
    let mut allowlist: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(&cfg.allowlist_path).expect("read allow-list"),
    )
    .expect("parse allow-list");
    allowlist["files"]["basic.feature"]["[1] Returning a literal"] =
        serde_json::json!({"reason": "not-implemented"});
    std::fs::write(
        &cfg.allowlist_path,
        serde_json::to_string_pretty(&allowlist).expect("serialize"),
    )
    .expect("write allow-list");

    cfg.write_allowlist = false;
    assert!(
        !runner::run(&cfg).expect("run"),
        "stale entry must fail the gate"
    );
    let report = read_report(&cfg.report_path);
    assert_eq!(report["totals"]["stale_allowlist"], 1);
}

#[test]
fn vendored_tck_parses_and_classifies_every_step() {
    let features =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../third_party/opencypher/tck/features");
    if !features.is_dir() {
        eprintln!("vendored TCK not present; run `git submodule update --init`");
        return;
    }
    let scenarios = runner::collect_scenarios(&features).expect("parse vendored TCK");
    assert!(
        scenarios.len() > 3000,
        "expected the full TCK, found {} scenarios",
        scenarios.len()
    );
    let unrecognized: Vec<String> = scenarios
        .iter()
        .filter_map(|scenario| match &scenario.steps {
            Err(text) => Some(format!("{}: {}: {text}", scenario.file, scenario.name)),
            Ok(_) => None,
        })
        .collect();
    assert!(
        unrecognized.is_empty(),
        "unrecognized steps:\n{}",
        unrecognized.join("\n")
    );
}
