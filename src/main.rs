//! grafile shell: an interactive Cypher REPL and script runner.
//!
//! ```text
//! grafile [database]              open (or create) a database and start the REPL
//! grafile [database] -c "QUERY"   run a single query
//! grafile [database] script.cypher
//! ```
//!
//! Dot commands: `.help`, `.schema`, `.indexes`, `.read FILE`, `.timer on|off`,
//! `.quit`.

use std::path::Path;
use std::process::ExitCode;
use std::time::Instant;

use anyhow::{Context, Result};
use grafile::api::{Connection, Params, Rows};
use grafile::record::Value;
use grafile::vdbe::format_value;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("grafile {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let mut path: Option<String> = None;
    let mut command: Option<String> = None;
    let mut script: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "-c" => {
                index += 1;
                command = Some(
                    args.get(index)
                        .context("-c requires a query argument")?
                        .clone(),
                );
            }
            other => {
                if path.is_none() {
                    path = Some(other.to_owned());
                } else if script.is_none() {
                    script = Some(other.to_owned());
                } else {
                    anyhow::bail!("unexpected argument `{other}`");
                }
            }
        }
        index += 1;
    }
    let path = path.unwrap_or_else(|| "grafile.db".to_owned());
    let mut conn = open_or_create(&path)?;

    if let Some(query) = command {
        run_query(&mut conn, &query, false)?;
        return Ok(());
    }
    if let Some(script) = script {
        let text = std::fs::read_to_string(&script)
            .with_context(|| format!("cannot read script `{script}`"))?;
        for statement in split_statements(&text) {
            if statement.trim().is_empty() {
                continue;
            }
            run_query(&mut conn, &statement, false)?;
        }
        return Ok(());
    }
    repl(&mut conn)
}

fn open_or_create(path: &str) -> Result<Connection> {
    let exists = Path::new(path).exists();
    let conn = if exists {
        Connection::open(path).with_context(|| format!("cannot open `{path}`"))?
    } else {
        Connection::create(path).with_context(|| format!("cannot create `{path}`"))?
    };
    Ok(conn)
}

fn repl(conn: &mut Connection) -> Result<()> {
    let mut editor = DefaultEditor::new()?;
    let mut timer = false;
    let mut buffer = String::new();
    println!(
        "grafile {} (type .help for commands)",
        env!("CARGO_PKG_VERSION")
    );
    loop {
        let prompt = if buffer.is_empty() {
            "grafile> "
        } else {
            "     ...> "
        };
        match editor.readline(prompt) {
            Ok(line) => {
                let trimmed = line.trim();
                if buffer.is_empty() && trimmed.starts_with('.') {
                    match run_dot_command(conn, trimmed, &mut timer) {
                        Ok(DotOutcome::Continue) => continue,
                        Ok(DotOutcome::Quit) => break,
                        Err(err) => {
                            eprintln!("error: {err:#}");
                            continue;
                        }
                    }
                }
                if buffer.is_empty() && trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line.as_str());
                buffer.push_str(&line);
                buffer.push('\n');
                // Execute when terminated by ';' or an empty line.
                if trimmed.ends_with(';') || trimmed.is_empty() {
                    let statements = split_statements(&buffer);
                    for statement in statements {
                        if statement.trim().is_empty() {
                            continue;
                        }
                        if let Err(err) = run_query(conn, &statement, timer) {
                            eprintln!("error: {err:#}");
                        }
                    }
                    buffer.clear();
                }
            }
            Err(ReadlineError::Interrupted) => {
                buffer.clear();
                println!("^C");
            }
            Err(ReadlineError::Eof) => break,
            Err(err) => return Err(err.into()),
        }
    }
    Ok(())
}

enum DotOutcome {
    Continue,
    Quit,
}

fn run_dot_command(conn: &mut Connection, line: &str, timer: &mut bool) -> Result<DotOutcome> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let argument = parts.next().unwrap_or("").trim();
    match command {
        ".quit" | ".exit" => return Ok(DotOutcome::Quit),
        ".help" => print_help(),
        ".timer" => match argument {
            "on" => {
                *timer = true;
                println!("timer on");
            }
            "off" => {
                *timer = false;
                println!("timer off");
            }
            _ => println!("usage: .timer on|off"),
        },
        ".schema" => print_schema(conn),
        ".indexes" => print_indexes(conn),
        ".read" => {
            let text = std::fs::read_to_string(argument)
                .with_context(|| format!("cannot read `{argument}`"))?;
            for statement in split_statements(&text) {
                if statement.trim().is_empty() {
                    continue;
                }
                run_query(conn, &statement, *timer)?;
            }
        }
        other => println!("unknown command `{other}`; try .help"),
    }
    Ok(DotOutcome::Continue)
}

fn run_query(conn: &mut Connection, source: &str, timer: bool) -> Result<()> {
    let start = Instant::now();
    let rows = conn.query(source, &Params::new())?;
    let elapsed = start.elapsed();
    print_rows(&rows);
    if timer {
        println!("({:.3} ms)", elapsed.as_secs_f64() * 1000.0);
    }
    Ok(())
}

fn print_rows(rows: &Rows) {
    if rows.columns().is_empty() {
        println!("OK");
        return;
    }
    let mut widths: Vec<usize> = rows.columns().iter().map(String::len).collect();
    let rendered: Vec<Vec<String>> = rows
        .rows()
        .iter()
        .map(|row| row.iter().map(cell).collect())
        .collect();
    for row in &rendered {
        for (index, value) in row.iter().enumerate() {
            if index < widths.len() {
                widths[index] = widths[index].max(value.len());
            }
        }
    }
    let header: Vec<String> = rows
        .columns()
        .iter()
        .enumerate()
        .map(|(index, name)| format!("{:width$}", name, width = widths[index]))
        .collect();
    println!("{}", header.join(" | "));
    let separator: Vec<String> = widths.iter().map(|width| "-".repeat(*width)).collect();
    println!("{}", separator.join("-+-"));
    for row in &rendered {
        let line: Vec<String> = row
            .iter()
            .enumerate()
            .map(|(index, value)| format!("{:width$}", value, width = widths[index]))
            .collect();
        println!("{}", line.join(" | "));
    }
    println!(
        "({} row{})",
        rows.len(),
        if rows.len() == 1 { "" } else { "s" }
    );
}

fn cell(value: &Value) -> String {
    match value {
        Value::Text(text) => format!("\"{}\"", text.replace('"', "\\\"")),
        other => format_value(other),
    }
}

fn print_schema(conn: &Connection) {
    let schema = conn.schema();
    println!(
        "labels: {}",
        join_or_dash(schema.labels.iter().map(String::as_str))
    );
    println!(
        "edge types: {}",
        join_or_dash(schema.edge_types.iter().map(String::as_str))
    );
    println!(
        "property keys: {}",
        join_or_dash(schema.property_keys.iter().map(String::as_str))
    );
}

fn print_indexes(conn: &Connection) {
    let schema = conn.schema();
    if schema.node_indexes.is_empty() && schema.edge_indexes.is_empty() {
        println!("(no indexes)");
        return;
    }
    for index in &schema.node_indexes {
        let key = schema.property_key_name(index.key_id).unwrap_or("?");
        let label = if index.label_id == 0 {
            "*".to_owned()
        } else {
            schema.label_name(index.label_id).unwrap_or("?").to_owned()
        };
        println!(
            "node index {} on {label}({key}){}",
            index.id,
            if index.unique { " unique" } else { "" }
        );
    }
    for index in &schema.edge_indexes {
        let key = schema.property_key_name(index.key_id).unwrap_or("?");
        let edge_type = if index.type_id == 0 {
            "*".to_owned()
        } else {
            schema
                .edge_type_name(index.type_id)
                .unwrap_or("?")
                .to_owned()
        };
        println!(
            "edge index {} on {edge_type}({key}){}",
            index.id,
            if index.unique { " unique" } else { "" }
        );
    }
}

fn join_or_dash<'a>(items: impl Iterator<Item = &'a str>) -> String {
    let items: Vec<&str> = items.collect();
    if items.is_empty() {
        "-".to_owned()
    } else {
        items.join(", ")
    }
}

/// Split a script into statements at top-level semicolons.
fn split_statements(text: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut in_backtick = false;
    let mut escaped = false;
    for ch in text.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        match ch {
            '\\' if in_single || in_double => {
                current.push(ch);
                escaped = true;
            }
            '\'' if !in_double && !in_backtick => {
                in_single = !in_single;
                current.push(ch);
            }
            '"' if !in_single && !in_backtick => {
                in_double = !in_double;
                current.push(ch);
            }
            '`' if !in_single && !in_double => {
                in_backtick = !in_backtick;
                current.push(ch);
            }
            ';' if !in_single && !in_double && !in_backtick => {
                statements.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.trim().is_empty() {
        statements.push(current);
    }
    statements
}

fn print_help() {
    println!(
        "grafile: a graph database that lives in a file

usage:
  grafile [database]              open or create a database and start the REPL
  grafile [database] -c \"QUERY\"   run one query and exit
  grafile [database] script.cypher

repl:
  queries end with ';' or an empty line
  .help             this message
  .schema           labels, edge types and property keys
  .indexes          index definitions
  .read FILE        run statements from a file
  .timer on|off     show query timing
  .quit             exit

cypher (v1): MATCH, WHERE, RETURN, WITH, UNWIND, CREATE, MERGE,
             SET, REMOVE, DELETE/DETACH DELETE, ORDER BY, SKIP, LIMIT,
             DISTINCT, aggregations (count/sum/avg/min/max/collect),
             parameters ($name)
ddl (extension): CREATE INDEX ON :Label(prop) / :TYPE(prop) via API"
    );
}
