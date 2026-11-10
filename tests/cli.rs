//! CLI behavior: flags, scripts, piped input, exit codes and plain output.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Run the shell with an empty home directory, so no developer
/// `~/.grafilerc` or history file leaks into a test.
fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let home = Path::new(env!("CARGO_TARGET_TMPDIR")).join("cli-empty-home");
    std::fs::create_dir_all(&home).expect("empty home");
    run_in(&home, args, stdin)
}

/// Run the shell with `home` as the home directory.
fn run_in(home: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_grafile"));
    command
        .args(args)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("GRAFILE_HISTORY");
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
fn script_files_ignore_semicolons_inside_comments() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("comments.db");
    let script = dir.path().join("comments.cypher");
    std::fs::write(
        &script,
        "// MATCH (n) CALL { WITH n DETACH DELETE n } IN TRANSACTIONS OF 10000 ROWS;\n\
         CREATE (:Person {name: 'alice'});\n\
         /* block; comment; */\n\
         MATCH (n:Person) RETURN n.name AS name;\n",
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
fn script_errors_name_the_failing_statement() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("failing.db");
    let script = dir.path().join("failing.cypher");
    std::fs::write(&script, "RETURN 1;\nMATCH (n RETURN n;\n").expect("write script");
    let output = run(
        &[db.to_str().expect("path"), script.to_str().expect("path")],
        None,
    );
    assert!(!output.status.success());
    let message = stderr(&output);
    assert!(message.contains("statement 2 failed"), "{message}");
    assert!(message.contains("Syntax"), "{message}");
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

#[test]
fn check_reports_a_clean_file_and_rebuilds() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("check.db");
    let db_arg = db.to_str().expect("utf-8 path");
    let output = run(
        &[
            db_arg,
            "-c",
            "UNWIND range(1, 300) AS i CREATE (:N {v: i})-[:R]->(:M)",
        ],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    run(&[db_arg, "-c", "CREATE INDEX n_v FOR (n:N) ON (n.v)"], None);

    let output = run(&["check", db_arg], None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stdout(&output).starts_with("0 findings:"),
        "{}",
        stdout(&output)
    );
    assert!(stdout(&output).contains("600 nodes, 300 edges"));

    let output = run(&["check", db_arg, "--rebuild"], None);
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(
        text.contains("rebuilt fragment store, node index `n_v`"),
        "{text}"
    );
    assert!(text.contains("0 findings:"), "{text}");
}

#[test]
fn check_exits_with_status_one_on_damage() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("damaged.db");
    let db_arg = db.to_str().expect("utf-8 path");
    run(
        &[
            db_arg,
            "-c",
            "UNWIND range(1, 2000) AS i CREATE (:N {v: i})",
        ],
        None,
    );
    // Flip a byte in the last page: its checksum no longer verifies.
    let mut bytes = std::fs::read(&db).expect("read");
    let last = bytes.len() - 100;
    bytes[last] ^= 0xFF;
    std::fs::write(&db, bytes).expect("write");

    let output = run(&["check", db_arg], None);
    assert_eq!(output.status.code(), Some(1), "{}", stderr(&output));
    assert!(stdout(&output).contains("checksum"), "{}", stdout(&output));

    let output = run(&["check", "missing.db"], None);
    assert_eq!(output.status.code(), Some(1));
    assert!(stderr(&output).contains("cannot open `missing.db`"));
}

#[test]
fn readonly_sessions_read_but_never_write_or_create() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("ro.db");
    let path = db.to_str().expect("path");
    assert!(
        run(&[path, "-c", "CREATE (:Person)"], None)
            .status
            .success()
    );
    let read = run(
        &["--readonly", path, "-c", "MATCH (n) RETURN count(n) AS c"],
        None,
    );
    assert!(read.status.success(), "{}", stderr(&read));
    assert!(stdout(&read).contains('1'), "{}", stdout(&read));
    let write = run(&["--readonly", path, "-c", "CREATE (:Person)"], None);
    assert!(!write.status.success());
    assert!(stderr(&write).contains("ReadOnly"), "{}", stderr(&write));
    let missing = dir.path().join("missing.db");
    let open = run(&["--readonly", missing.to_str().expect("path")], None);
    assert!(!open.status.success());
    assert!(!missing.exists(), "--readonly never creates a file");
}

#[test]
fn startup_file_and_params_shape_every_mode() {
    let home = tempfile::tempdir().expect("home");
    std::fs::write(
        home.path().join(".grafilerc"),
        ".mode csv\n.param set greeting 'hi'\n.bogus\n",
    )
    .expect("rc");
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("rc.db");
    let path = db.to_str().expect("path");
    let query = "RETURN $greeting AS g";
    let output = run_in(home.path(), &[path, "-c", query], None);
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "g\r\nhi\r\n");
    assert!(
        stderr(&output).contains(".grafilerc:3"),
        "{}",
        stderr(&output)
    );
    let skipped = run_in(home.path(), &["--no-init", path, "-c", query], None);
    assert!(!skipped.status.success());
    assert!(
        stderr(&skipped).contains("greeting"),
        "{}",
        stderr(&skipped)
    );
    let other = dir.path().join("other.rc");
    std::fs::write(&other, ".param set greeting 'yo'\n").expect("other rc");
    let replaced = run_in(
        home.path(),
        &["--init", other.to_str().expect("path"), path, "-c", query],
        None,
    );
    assert!(stdout(&replaced).contains("yo"), "{}", stdout(&replaced));
    assert!(
        !stdout(&replaced).contains("g\r\nyo"),
        "the rc's .mode csv is replaced"
    );
    let flagged = run(
        &[
            path,
            "--param",
            "x=41",
            "-p",
            "y=$x + 1",
            "-c",
            "RETURN $y AS y",
        ],
        None,
    );
    assert!(stdout(&flagged).contains("42"), "{}", stdout(&flagged));
}

#[test]
fn scripts_mix_dot_commands_and_keep_no_piped_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mixed.db");
    let script = dir.path().join("mixed.cypher");
    std::fs::write(&script, ".mode csv\nRETURN 1 AS a;\n").expect("script");
    let output = run(
        &[db.to_str().expect("path"), script.to_str().expect("path")],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output), "a\r\n1\r\n");
    let history = dir.path().join("history");
    let mut command = Command::new(env!("CARGO_BIN_EXE_grafile"));
    let piped = command
        .arg(db.to_str().expect("path"))
        .env("GRAFILE_HISTORY", &history)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    piped
        .stdin
        .as_ref()
        .expect("stdin")
        .write_all(b"RETURN 1;\n.quit\n")
        .expect("write");
    assert!(piped.wait_with_output().expect("wait").status.success());
    assert!(!history.exists(), "piped sessions keep no history");
}

#[test]
fn dot_lines_inside_strings_and_comments_stay_cypher() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("literals.db");
    let script = dir.path().join("literals.cypher");
    std::fs::write(
        &script,
        "RETURN 'a;\n.headers off\n' AS x;\nRETURN 1 AS y /* note;\n.quit\n*/;\nRETURN 2 AS z;\n",
    )
    .expect("write script");
    let output = run(
        &[db.to_str().expect("path"), script.to_str().expect("path")],
        None,
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains(".headers off"), "{text}");
    assert!(text.contains("| z |"), "{text}");
}

#[test]
fn piped_input_waits_for_the_end_of_a_string() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("piped.db");
    let output = run(
        &[db.to_str().expect("path")],
        Some("RETURN 'a;\nb' AS x;\nRETURN \"c\n\nd\" AS y;\n"),
    );
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));
    let text = stdout(&output);
    assert!(text.contains("| a; |"), "{text}");
    assert!(text.contains("| c |"), "{text}");
}

#[test]
fn parameters_cannot_smuggle_in_writes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("params.db");
    let db = db.to_str().expect("path");
    let output = run(
        &[
            db,
            "-p",
            "x=1 AS a UNION CREATE (:Hacked) RETURN 2 AS a",
            "-c",
            "RETURN $x",
        ],
        None,
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("exactly one value"),
        "{}",
        stderr(&output)
    );
    let output = run(&[db, "-c", "MATCH (n) RETURN count(n) AS c"], None);
    assert!(stdout(&output).contains("| 0 |"), "{}", stdout(&output));
}
