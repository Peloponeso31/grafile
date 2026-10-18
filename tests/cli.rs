//! CLI behavior: flags, scripts, piped input, exit codes and plain output.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_grafile"));
    command.args(args);
    if let Some(input) = stdin {
        command.stdin(Stdio::piped());
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("write stdin");
        child.wait_with_output().expect("wait")
    } else {
        command.output().expect("output")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn query_flag_executes_and_persists() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("cli.db");
    let db = db.to_str().expect("path");

    let created = run(&[db, "-c", "CREATE (:Person {name: 'alice'})"], None);
    assert!(created.status.success(), "{}", stderr(&created));
    assert_eq!(stdout(&created), "OK\n", "writes acknowledge without rows");

    let queried = run(&[db, "-c", "MATCH (n:Person) RETURN n.name AS name"], None);
    assert!(queried.status.success(), "{}", stderr(&queried));
    assert!(stdout(&queried).contains("alice"), "{}", stdout(&queried));
}

#[test]
fn syntax_errors_report_to_stderr_and_fail() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("bad.db");
    let db = db.to_str().expect("path");
    let output = run(&[db, "-c", "MATCH (n RETURN n"], None);
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("error:"), "{message}");
    assert!(message.contains("Syntax"), "{message}");
}

#[test]
fn missing_script_files_are_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("script.db");
    let script = dir.path().join("nope.cypher");
    let output = run(
        &[db.to_str().expect("path"), script.to_str().expect("path")],
        None,
    );
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("cannot read script"), "{message}");
}

#[test]
fn script_files_run_every_statement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("runner.db");
    let script = dir.path().join("setup.cypher");
    std::fs::write(
        &script,
        "CREATE (:Person {name: 'alice'});\nMATCH (n:Person) RETURN n.name AS name;\n",
    )
    .expect("write script");
    let output = run(
        &[db.to_str().expect("path"), script.to_str().expect("path")],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("alice"), "{}", stdout(&output));
}

#[test]
fn piped_input_is_executed_and_ansi_free() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("piped.db");
    let output = run(
        &[db.to_str().expect("path")],
        Some("RETURN 1 AS x;\n.quit\n"),
    );
    let text = stdout(&output);
    assert!(text.contains('x'), "{text}");
    assert!(
        !text.contains('\u{1b}'),
        "piped output must not carry ANSI escapes: {text:?}"
    );
    assert!(
        Path::new(db.as_path()).exists(),
        "the database is created on first use"
    );
}
