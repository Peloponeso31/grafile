//! grafile shell: an interactive Cypher REPL and script runner.
//!
//! ```text
//! grafile [database]              open (or create) a database and start the REPL
//! grafile [database] -c "QUERY"   run a single query
//! grafile [database] script.cypher
//! ```
//!
//! Dot commands: `.help`, `.schema`, `.indexes`, `.read FILE`, `.timer on|off`,
//! `.mode [NAME]`, `.headers on|off`, `.colors on|off`, `.pretty on|off`,
//! `.clear` (alias `.cls`), `.quit`.

use std::borrow::Cow;
use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Instant;

use anyhow::{Context, Result};
use grafile::api::{Connection, Params, Rows};
use grafile::cypher::lexer::{TokenClass, highlight_spans};
use grafile::record::Value;
use grafile::vdbe::format_value;
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Editor};

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
    let settings = Settings::new();

    if let Some(query) = command {
        run_query(&mut conn, &query, &settings)?;
        return Ok(());
    }
    if let Some(script) = script {
        let text = std::fs::read_to_string(&script)
            .with_context(|| format!("cannot read script `{script}`"))?;
        for statement in split_statements(&text) {
            if statement.trim().is_empty() {
                continue;
            }
            run_query(&mut conn, &statement, &settings)?;
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

// ---- settings and highlighting ----

/// Which format query results are printed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputMode {
    /// ASCII-art table (default).
    Table,
    /// Unicode box-drawing table.
    Box,
    /// Unicode box table with quoted values.
    Qbox,
    /// PostgreSQL `psql` style: `|` separators, no outer border.
    Psql,
    /// Markdown table.
    Markdown,
    /// Fixed-width columns with an underline.
    Column,
    /// `|`-separated values, one row per line.
    List,
    /// Tab-separated values.
    Tabs,
    /// RFC 4180 CSV.
    Csv,
    /// SQL/Cypher literals.
    Quote,
    /// One `name: value` pair per line.
    Line,
    /// One JSON object per row (NDJSON).
    Json,
}

impl OutputMode {
    /// All modes with their names, in help order.
    const ALL: [(&'static str, OutputMode); 12] = [
        ("table", OutputMode::Table),
        ("box", OutputMode::Box),
        ("qbox", OutputMode::Qbox),
        ("psql", OutputMode::Psql),
        ("markdown", OutputMode::Markdown),
        ("column", OutputMode::Column),
        ("list", OutputMode::List),
        ("tabs", OutputMode::Tabs),
        ("csv", OutputMode::Csv),
        ("quote", OutputMode::Quote),
        ("line", OutputMode::Line),
        ("json", OutputMode::Json),
    ];

    fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|(mode_name, _)| *mode_name == name)
            .map(|(_, mode)| *mode)
    }

    fn name(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(_, mode)| *mode == self)
            .map(|(name, _)| *name)
            .unwrap_or("table")
    }
}

/// Session settings shared by the REPL, dot commands and renderers.
struct Settings {
    colors: Rc<Cell<bool>>,
    mode: OutputMode,
    headers: bool,
    row_separators: bool,
    pretty: bool,
    timer: bool,
}

impl Settings {
    fn new() -> Self {
        let is_tty = std::io::stdout().is_terminal();
        let no_color = std::env::var_os("NO_COLOR").is_some();
        Self {
            colors: Rc::new(Cell::new(default_colors(is_tty, no_color))),
            mode: OutputMode::Table,
            headers: true,
            row_separators: true,
            pretty: false,
            timer: false,
        }
    }

    fn colors_enabled(&self) -> bool {
        self.colors.get()
    }
}

/// Colorize by default only on a terminal and when `NO_COLOR` is unset.
fn default_colors(is_tty: bool, no_color_set: bool) -> bool {
    is_tty && !no_color_set
}

const RESET: &str = "\x1b[0m";

fn class_color(class: TokenClass) -> &'static str {
    match class {
        TokenClass::Keyword => "\x1b[1;36m",
        TokenClass::Function => "\x1b[35m",
        TokenClass::String => "\x1b[32m",
        TokenClass::Number => "\x1b[33m",
        TokenClass::Parameter => "\x1b[1;35m",
        TokenClass::Comment => "\x1b[2;37m",
        TokenClass::Operator => "\x1b[37m",
        TokenClass::Label => "\x1b[34m",
        TokenClass::Property => "\x1b[1;34m",
        TokenClass::Punctuation | TokenClass::Variable => "",
    }
}

/// ANSI-colorize one input line for the rustyline highlighter.
fn highlight_line(line: &str) -> String {
    let spans = highlight_spans(line);
    let mut out = String::with_capacity(line.len() + spans.len() * 8);
    let mut cursor = 0usize;
    for (start, end, class) in spans {
        if start > cursor {
            out.push_str(&line[cursor..start]);
        }
        let code = class_color(class);
        if code.is_empty() {
            out.push_str(&line[start..end]);
        } else {
            out.push_str(code);
            out.push_str(&line[start..end]);
            out.push_str(RESET);
        }
        cursor = end;
    }
    if cursor < line.len() {
        out.push_str(&line[cursor..]);
    }
    out
}

/// rustyline helper providing Cypher syntax highlighting.
struct GrafileHelper {
    colors: Rc<Cell<bool>>,
}

impl Completer for GrafileHelper {
    type Candidate = String;
}

impl Hinter for GrafileHelper {
    type Hint = String;
}

impl Validator for GrafileHelper {}

impl Highlighter for GrafileHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        if !self.colors.get() {
            return Cow::Borrowed(line);
        }
        Cow::Owned(highlight_line(line))
    }

    fn highlight_char(&self, _line: &str, _pos: usize, _kind: CmdKind) -> bool {
        self.colors.get()
    }
}

impl rustyline::Helper for GrafileHelper {}

// ---- REPL ----

fn repl(conn: &mut Connection) -> Result<()> {
    let mut settings = Settings::new();
    let mut editor: Editor<GrafileHelper, DefaultHistory> = Editor::with_config(Config::default())?;
    editor.set_helper(Some(GrafileHelper {
        colors: settings.colors.clone(),
    }));
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
                    match run_dot_command(conn, trimmed, &mut settings) {
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
                        if let Err(err) = run_query(conn, &statement, &settings) {
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

fn run_dot_command(
    conn: &mut Connection,
    line: &str,
    settings: &mut Settings,
) -> Result<DotOutcome> {
    let mut parts = line.splitn(2, char::is_whitespace);
    let command = parts.next().unwrap_or("");
    let argument = parts.next().unwrap_or("").trim();
    match command {
        ".quit" | ".exit" => return Ok(DotOutcome::Quit),
        ".help" => print_help(),
        ".timer" => match argument {
            "on" => {
                settings.timer = true;
                println!("timer on");
            }
            "off" => {
                settings.timer = false;
                println!("timer off");
            }
            _ => println!("usage: .timer on|off"),
        },
        ".colors" => match argument {
            "on" => {
                settings.colors.set(true);
                println!("colors on");
            }
            "off" => {
                settings.colors.set(false);
                println!("colors off");
            }
            _ => println!("usage: .colors on|off"),
        },
        ".headers" => match argument {
            "on" => {
                settings.headers = true;
                println!("headers on");
            }
            "off" => {
                settings.headers = false;
                println!("headers off");
            }
            _ => println!("usage: .headers on|off"),
        },
        ".separators" => match argument {
            "on" => {
                settings.row_separators = true;
                println!("row separators on");
            }
            "off" => {
                settings.row_separators = false;
                println!("row separators off");
            }
            _ => println!("usage: .separators on|off"),
        },
        ".pretty" => match argument {
            "on" => {
                settings.pretty = true;
                println!("pretty on");
            }
            "off" => {
                settings.pretty = false;
                println!("pretty off");
            }
            _ => println!("usage: .pretty on|off"),
        },
        ".mode" => match argument {
            "" => println!("mode: {}", settings.mode.name()),
            "--list" => print_modes(),
            name => match OutputMode::parse(name) {
                Some(mode) => {
                    settings.mode = mode;
                    println!("mode: {}", mode.name());
                }
                None => {
                    println!("unknown mode `{name}`");
                    print_modes();
                }
            },
        },
        ".schema" => print_schema(conn),
        ".indexes" => print_indexes(conn),
        ".clear" | ".cls" => clear_screen(),
        ".read" => {
            let text = std::fs::read_to_string(argument)
                .with_context(|| format!("cannot read `{argument}`"))?;
            for statement in split_statements(&text) {
                if statement.trim().is_empty() {
                    continue;
                }
                run_query(conn, &statement, settings)?;
            }
        }
        other => println!("unknown command `{other}`; try .help"),
    }
    Ok(DotOutcome::Continue)
}

fn print_modes() {
    let names: Vec<&str> = OutputMode::ALL.iter().map(|(name, _)| *name).collect();
    println!("available modes: {}", names.join(" "));
}

fn run_query(conn: &mut Connection, source: &str, settings: &Settings) -> Result<()> {
    let start = Instant::now();
    let rows = conn.query(source, &Params::new())?;
    let elapsed = start.elapsed();
    let output = render_rows(conn, &rows, settings)?;
    print!("{output}");
    let _ = std::io::stdout().flush();
    if settings.timer {
        println!("({:.3} ms)", elapsed.as_secs_f64() * 1000.0);
    }
    Ok(())
}

// ---- rendering ----

/// Render all result rows in the selected output mode.
fn render_rows(conn: &mut Connection, rows: &Rows, settings: &Settings) -> Result<String> {
    if rows.columns().is_empty() {
        return Ok("OK\n".to_owned());
    }
    match settings.mode {
        OutputMode::Json => render_json(conn, rows, settings),
        OutputMode::Csv => render_delimited(conn, rows, settings, Delimiter::Csv),
        OutputMode::List => render_delimited(conn, rows, settings, Delimiter::Text("|")),
        OutputMode::Tabs => render_delimited(conn, rows, settings, Delimiter::Text("\t")),
        OutputMode::Quote => render_quote(conn, rows, settings),
        OutputMode::Line => render_line(conn, rows, settings),
        mode => render_grid(conn, rows, settings, mode),
    }
}

/// Build the lines of one cell (multi-line for pretty JSON).
fn cell_lines(
    conn: &mut Connection,
    value: &Value,
    settings: &Settings,
    mode: OutputMode,
) -> Result<Vec<String>> {
    let pretty_json =
        settings.pretty && (value_contains_entity(value) || matches!(value, Value::Map(_)));
    if pretty_json {
        return Ok(json_value(conn, value, settings, 0)?
            .split('\n')
            .map(str::to_owned)
            .collect());
    }
    if matches!(value, Value::Node(_) | Value::Edge(_)) {
        return Ok(vec![compact_entity(value)]);
    }
    if mode == OutputMode::Csv && value.is_null() {
        return Ok(vec![String::new()]);
    }
    let text = match mode {
        OutputMode::Qbox => qbox_value(value),
        OutputMode::Quote => quote_literal(value),
        _ => format_value(value),
    };
    Ok(text.split('\n').map(str::to_owned).collect())
}

fn compact_entity(value: &Value) -> String {
    match value {
        Value::Node(id) => format!("(node {id})"),
        Value::Edge(id) => format!("[edge {id}]"),
        other => format_value(other),
    }
}

fn value_contains_entity(value: &Value) -> bool {
    match value {
        Value::Node(_) | Value::Edge(_) => true,
        Value::List(items) => items.iter().any(value_contains_entity),
        Value::Map(entries) => entries.iter().any(|(_, item)| value_contains_entity(item)),
        _ => false,
    }
}

fn qbox_value(value: &Value) -> String {
    match value {
        Value::Text(_) => quote_literal(value),
        other => format_value(other),
    }
}

fn quote_literal(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(value) => value.to_string(),
        Value::Int(_) | Value::Float(_) => format_value(value),
        Value::Text(text) => format!("'{}'", text.replace('\'', "''")),
        Value::Bytes(bytes) => format!("x'{}'", hex(bytes)),
        Value::List(items) => {
            let inner: Vec<String> = items.iter().map(quote_literal).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Map(entries) => {
            let inner: Vec<String> = entries
                .iter()
                .map(|(key, item)| format!("{key}: {}", quote_literal(item)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
        Value::Node(_) | Value::Edge(_) => compact_entity(value),
    }
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

enum Delimiter {
    Text(&'static str),
    Csv,
}

fn render_delimited(
    conn: &mut Connection,
    rows: &Rows,
    settings: &Settings,
    delimiter: Delimiter,
) -> Result<String> {
    let terminator = match delimiter {
        Delimiter::Csv => "\r\n",
        Delimiter::Text(_) => "\n",
    };
    let mut out = String::new();
    if settings.headers {
        let header: Vec<String> = rows.columns().to_vec();
        out.push_str(&join_fields(header.iter().map(String::as_str), &delimiter));
        out.push_str(terminator);
    }
    for row in rows.rows() {
        let mut fields = Vec::with_capacity(row.len());
        for value in row {
            let lines = cell_lines(conn, value, settings, settings.mode)?;
            fields.push(lines.join("\n"));
        }
        out.push_str(&join_fields(fields.iter().map(String::as_str), &delimiter));
        out.push_str(terminator);
    }
    Ok(out)
}

fn join_fields<'a>(fields: impl Iterator<Item = &'a str>, delimiter: &Delimiter) -> String {
    match delimiter {
        Delimiter::Text(separator) => fields.collect::<Vec<_>>().join(separator),
        Delimiter::Csv => {
            let escaped: Vec<String> = fields.map(csv_escape).collect();
            escaped.join(",")
        }
    }
}

fn csv_escape(field: &str) -> String {
    if field.contains(',') || field.contains('"') || field.contains('\n') || field.contains('\r') {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_owned()
    }
}

fn render_quote(conn: &mut Connection, rows: &Rows, settings: &Settings) -> Result<String> {
    let mut out = String::new();
    for row in rows.rows() {
        let mut fields = Vec::with_capacity(row.len());
        for value in row {
            let lines = cell_lines(conn, value, settings, OutputMode::Quote)?;
            fields.push(lines.join("\n"));
        }
        out.push_str(&fields.join(","));
        out.push('\n');
    }
    Ok(out)
}

fn render_line(conn: &mut Connection, rows: &Rows, settings: &Settings) -> Result<String> {
    let mut out = String::new();
    for (row_index, row) in rows.rows().iter().enumerate() {
        if row_index > 0 {
            out.push('\n');
        }
        for (column, value) in row.iter().enumerate() {
            let name = rows
                .columns()
                .get(column)
                .map(String::as_str)
                .unwrap_or("?");
            let lines = cell_lines(conn, value, settings, OutputMode::Line)?;
            match lines.split_first() {
                Some((first, rest)) => {
                    out.push_str(name);
                    out.push_str(": ");
                    out.push_str(first);
                    out.push('\n');
                    for line in rest {
                        out.push_str("  ");
                        out.push_str(line);
                        out.push('\n');
                    }
                }
                None => {
                    out.push_str(name);
                    out.push_str(": \n");
                }
            }
        }
    }
    Ok(out)
}

fn render_json(conn: &mut Connection, rows: &Rows, settings: &Settings) -> Result<String> {
    let mut out = String::new();
    for row in rows.rows() {
        let fields: Vec<(String, Value)> = rows
            .columns()
            .iter()
            .cloned()
            .zip(row.iter().cloned())
            .collect();
        out.push_str(&json_fields_object(conn, &fields, settings, 0)?);
        out.push('\n');
    }
    Ok(out)
}

fn render_grid(
    conn: &mut Connection,
    rows: &Rows,
    settings: &Settings,
    mode: OutputMode,
) -> Result<String> {
    let mut cells: Vec<Vec<Vec<String>>> = Vec::with_capacity(rows.len());
    for row in rows.rows() {
        let mut row_cells = Vec::with_capacity(row.len());
        for value in row {
            row_cells.push(cell_lines(conn, value, settings, mode)?);
        }
        cells.push(row_cells);
    }
    let mut widths: Vec<usize> = rows
        .columns()
        .iter()
        .map(|name| visible_width(name))
        .collect();
    for row in &cells {
        for (column, cell) in row.iter().enumerate() {
            if column >= widths.len() {
                continue;
            }
            for line in cell {
                widths[column] = widths[column].max(visible_width(line));
            }
        }
    }

    let mut out = String::new();
    let header_cells: Vec<Vec<String>> = rows
        .columns()
        .iter()
        .map(|name| vec![name.clone()])
        .collect();
    match mode {
        OutputMode::Table => {
            let border = ascii_border(&widths);
            out.push_str(&border);
            out.push('\n');
            if settings.headers {
                push_bordered_row(&mut out, &header_cells, &widths, '|', '|', '|');
                out.push_str(&border);
                out.push('\n');
            }
            for (index, row) in cells.iter().enumerate() {
                if index > 0 && settings.row_separators {
                    out.push_str(&border);
                    out.push('\n');
                }
                push_bordered_row(&mut out, row, &widths, '|', '|', '|');
            }
            out.push_str(&border);
            out.push('\n');
        }
        OutputMode::Box | OutputMode::Qbox => {
            out.push_str(&unicode_border(&widths, '╭', '─', '┬', '╮'));
            out.push('\n');
            if settings.headers {
                push_bordered_row(&mut out, &header_cells, &widths, '│', '│', '│');
                out.push_str(&unicode_border(&widths, '╞', '═', '╪', '╡'));
                out.push('\n');
            }
            let row_separator = unicode_border(&widths, '├', '─', '┼', '┤');
            for (index, row) in cells.iter().enumerate() {
                if index > 0 && settings.row_separators {
                    out.push_str(&row_separator);
                    out.push('\n');
                }
                push_bordered_row(&mut out, row, &widths, '│', '│', '│');
            }
            out.push_str(&unicode_border(&widths, '╰', '─', '┴', '╯'));
            out.push('\n');
        }
        OutputMode::Psql => {
            if settings.headers {
                push_joined_row(&mut out, &header_cells, &widths, " | ");
                let separator: Vec<String> =
                    widths.iter().map(|width| "-".repeat(width + 2)).collect();
                out.push_str(&separator.join("+"));
                out.push('\n');
            }
            for row in &cells {
                push_joined_row(&mut out, row, &widths, " | ");
            }
        }
        OutputMode::Column => {
            if settings.headers {
                push_joined_row(&mut out, &header_cells, &widths, " ");
                let separator: Vec<String> =
                    widths.iter().map(|width| "-".repeat(*width)).collect();
                out.push_str(&separator.join(" "));
                out.push('\n');
            }
            for row in &cells {
                push_joined_row(&mut out, row, &widths, " ");
            }
        }
        OutputMode::Markdown => {
            if settings.headers {
                push_markdown_row(&mut out, &header_cells, &widths);
                let separator: Vec<String> =
                    widths.iter().map(|width| "-".repeat(width + 2)).collect();
                out.push('|');
                out.push_str(&separator.join("|"));
                out.push('|');
                out.push('\n');
            }
            for row in &cells {
                push_markdown_row(&mut out, row, &widths);
            }
        }
        _ => return Err(anyhow::anyhow!("mode `{}` is not tabular", mode.name())),
    }
    Ok(out)
}

fn ascii_border(widths: &[usize]) -> String {
    let mut out = String::from("+");
    for width in widths {
        out.push_str(&"-".repeat(width + 2));
        out.push('+');
    }
    out
}

fn unicode_border(widths: &[usize], left: char, fill: char, middle: char, right: char) -> String {
    let mut out = String::new();
    out.push(left);
    for (index, width) in widths.iter().enumerate() {
        out.push_str(&fill.to_string().repeat(width + 2));
        out.push(if index + 1 == widths.len() {
            right
        } else {
            middle
        });
    }
    out
}

fn push_bordered_row(
    out: &mut String,
    cells: &[Vec<String>],
    widths: &[usize],
    left: char,
    middle: char,
    right: char,
) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line_index in 0..height {
        out.push(left);
        for (column, width) in widths.iter().enumerate() {
            out.push(' ');
            let line = cells
                .get(column)
                .and_then(|lines| lines.get(line_index))
                .map(String::as_str)
                .unwrap_or("");
            out.push_str(&pad_to(line, *width));
            out.push(' ');
            out.push(if column + 1 == widths.len() {
                right
            } else {
                middle
            });
        }
        out.push('\n');
    }
}

fn push_joined_row(out: &mut String, cells: &[Vec<String>], widths: &[usize], separator: &str) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line_index in 0..height {
        let parts: Vec<String> = widths
            .iter()
            .enumerate()
            .map(|(column, width)| {
                let line = cells
                    .get(column)
                    .and_then(|lines| lines.get(line_index))
                    .map(String::as_str)
                    .unwrap_or("");
                pad_to(line, *width)
            })
            .collect();
        out.push_str(&parts.join(separator));
        out.push('\n');
    }
}

fn push_markdown_row(out: &mut String, cells: &[Vec<String>], widths: &[usize]) {
    let height = cells.iter().map(Vec::len).max().unwrap_or(1).max(1);
    for line_index in 0..height {
        out.push('|');
        for (column, width) in widths.iter().enumerate() {
            out.push(' ');
            let line = cells
                .get(column)
                .and_then(|lines| lines.get(line_index))
                .map(String::as_str)
                .unwrap_or("");
            out.push_str(&pad_to(line, *width));
            out.push_str(" |");
        }
        out.push('\n');
    }
}

/// Display width of a line, ignoring ANSI escape sequences and honoring
/// wide characters.
fn visible_width(text: &str) -> usize {
    let mut width = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            // Skip a CSI sequence: ESC '[' ... final byte in '@'..='~'.
            if chars.peek() == Some(&'[') {
                chars.next();
            }
            for next in chars.by_ref() {
                if ('@'..='~').contains(&next) {
                    break;
                }
            }
            continue;
        }
        width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
    }
    width
}

fn pad_to(text: &str, width: usize) -> String {
    let padding = width.saturating_sub(visible_width(text));
    format!("{text}{}", " ".repeat(padding))
}

// ---- JSON rendering ----

fn json_value(
    conn: &mut Connection,
    value: &Value,
    settings: &Settings,
    indent: usize,
) -> Result<String> {
    let colors = settings.colors_enabled();
    Ok(match value {
        Value::Null => paint(colors, "\x1b[35m", "null"),
        Value::Bool(value) => paint(colors, "\x1b[35m", if *value { "true" } else { "false" }),
        Value::Int(value) => paint(colors, "\x1b[33m", &value.to_string()),
        Value::Float(value) => {
            if value.is_finite() {
                let mut buffer = ryu::Buffer::new();
                paint(colors, "\x1b[33m", buffer.format(*value))
            } else {
                // JSON has no NaN/Infinity.
                paint(colors, "\x1b[35m", "null")
            }
        }
        Value::Text(text) => paint(colors, "\x1b[32m", &json_escape(text)),
        Value::Bytes(bytes) => paint(
            colors,
            "\x1b[32m",
            &json_escape(&format!("0x{}", hex(bytes))),
        ),
        Value::List(items) => {
            if items.is_empty() {
                "[]".to_owned()
            } else {
                let pretty = settings.pretty;
                let mut out = String::from("[");
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    if pretty {
                        out.push('\n');
                        out.push_str(&indent_str(indent + 1));
                    }
                    out.push_str(&json_value(conn, item, settings, indent + 1)?);
                }
                if pretty {
                    out.push('\n');
                    out.push_str(&indent_str(indent));
                }
                out.push(']');
                out
            }
        }
        Value::Map(entries) => {
            let fields: Vec<(String, Value)> = entries
                .iter()
                .map(|(key, item)| (key.to_string(), item.clone()))
                .collect();
            json_fields_object(conn, &fields, settings, indent)?
        }
        Value::Node(id) => {
            let fields = node_fields(conn, *id)?;
            json_fields_object(conn, &fields, settings, indent)?
        }
        Value::Edge(id) => {
            let fields = edge_fields(conn, *id)?;
            json_fields_object(conn, &fields, settings, indent)?
        }
    })
}

fn node_fields(conn: &mut Connection, id: u64) -> Result<Vec<(String, Value)>> {
    let record = conn.database_mut().get_node(id)?;
    let mut labels = Vec::new();
    let mut properties: Vec<(std::sync::Arc<str>, Value)> = Vec::new();
    if let Some(record) = &record {
        for label in &record.labels {
            if let Some(name) = conn.schema().label_name(*label) {
                labels.push(Value::from(name));
            }
        }
        for (key, value) in &record.props {
            if let Some(name) = conn.schema().property_key_name(*key) {
                properties.push((std::sync::Arc::from(name), value.clone()));
            }
        }
    }
    Ok(vec![
        ("id".to_owned(), Value::Int(id as i64)),
        ("labels".to_owned(), Value::List(labels.into())),
        ("properties".to_owned(), Value::Map(properties.into())),
    ])
}

fn edge_fields(conn: &mut Connection, id: u64) -> Result<Vec<(String, Value)>> {
    let record = conn.database_mut().get_edge(id)?;
    let mut edge_type = Value::Null;
    let mut start = Value::Null;
    let mut end = Value::Null;
    let mut properties: Vec<(std::sync::Arc<str>, Value)> = Vec::new();
    if let Some(record) = &record {
        edge_type = conn
            .schema()
            .edge_type_name(record.edge_type)
            .map(Value::from)
            .unwrap_or(Value::Null);
        start = Value::Int(record.src as i64);
        end = Value::Int(record.dst as i64);
        for (key, value) in &record.props {
            if let Some(name) = conn.schema().property_key_name(*key) {
                properties.push((std::sync::Arc::from(name), value.clone()));
            }
        }
    }
    Ok(vec![
        ("id".to_owned(), Value::Int(id as i64)),
        ("type".to_owned(), edge_type),
        ("start".to_owned(), start),
        ("end".to_owned(), end),
        ("properties".to_owned(), Value::Map(properties.into())),
    ])
}

fn json_fields_object(
    conn: &mut Connection,
    fields: &[(String, Value)],
    settings: &Settings,
    indent: usize,
) -> Result<String> {
    if fields.is_empty() {
        return Ok("{}".to_owned());
    }
    let pretty = settings.pretty;
    let colors = settings.colors_enabled();
    let mut out = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        if pretty {
            out.push('\n');
            out.push_str(&indent_str(indent + 1));
        }
        out.push_str(&paint(colors, "\x1b[1;34m", &json_escape(key)));
        out.push(':');
        if pretty {
            out.push(' ');
        }
        out.push_str(&json_value(conn, value, settings, indent + 1)?);
    }
    if pretty {
        out.push('\n');
        out.push_str(&indent_str(indent));
    }
    out.push('}');
    Ok(out)
}

fn indent_str(level: usize) -> String {
    "  ".repeat(level)
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ch if (ch as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", ch as u32)),
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

fn paint(colors: bool, code: &'static str, text: &str) -> String {
    if colors {
        format!("{code}{text}{RESET}")
    } else {
        text.to_owned()
    }
}

// ---- introspection and help ----

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

/// ANSI sequence: clear the visible screen and home the cursor.
const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

/// Clear the terminal screen. A no-op when stdout is not a terminal, so
/// piped or redirected sessions never see escape codes.
fn clear_screen() {
    if std::io::stdout().is_terminal() {
        print!("{CLEAR_SCREEN}");
        let _ = std::io::stdout().flush();
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
  .mode [NAME]      output mode (.mode --list to see all)
  .headers on|off   show column titles (default on)
  .separators on|off  lines between table rows (default on)
  .colors on|off    syntax highlighting (default: on when a terminal)
  .pretty on|off    render nodes/edges as highlighted JSON
  .read FILE        run statements from a file
  .timer on|off     show query timing
  .clear (.cls)     clear the terminal screen
  .quit             exit

modes: table (default), box, qbox, psql, markdown, column,
       list, tabs, csv, quote, line, json

cypher (v1): MATCH, WHERE, RETURN, WITH, UNWIND, CREATE, MERGE,
             SET, REMOVE, DELETE/DETACH DELETE, ORDER BY, SKIP, LIMIT,
             DISTINCT, aggregations (count/sum/avg/min/max/collect),
             parameters ($name)
ddl (extension): CREATE INDEX ON :Label(prop) / :TYPE(prop) via API"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use grafile::vfs::mem::MemVfs;
    use std::sync::Arc;

    fn connection(name: &str) -> Connection {
        Connection::create_with_vfs(Arc::new(MemVfs::new()), Path::new(name), 4096)
            .expect("connection")
    }

    fn settings(mode: OutputMode) -> Settings {
        Settings {
            colors: Rc::new(Cell::new(false)),
            mode,
            headers: true,
            row_separators: true,
            pretty: false,
            timer: false,
        }
    }

    fn render(conn: &mut Connection, query: &str, settings: &Settings) -> String {
        let rows = conn.query(query, &Params::new()).expect("query");
        render_rows(conn, &rows, settings).expect("render")
    }

    #[test]
    fn default_colors_follow_tty_and_no_color() {
        assert!(default_colors(true, false));
        assert!(!default_colors(true, true));
        assert!(!default_colors(false, false));
    }

    #[test]
    fn table_mode_renders_ascii_borders() {
        let mut conn = connection("table.db");
        let out = render(
            &mut conn,
            "UNWIND [1, 2] AS x RETURN x, x * 10 AS ten",
            &settings(OutputMode::Table),
        );
        assert_eq!(
            out,
            "+---+-----+\n\
             | x | ten |\n\
             +---+-----+\n\
             | 1 | 10  |\n\
             +---+-----+\n\
             | 2 | 20  |\n\
             +---+-----+\n"
        );
        let mut plain = settings(OutputMode::Table);
        plain.row_separators = false;
        let out = render(
            &mut conn,
            "UNWIND [1, 2] AS x RETURN x, x * 10 AS ten",
            &plain,
        );
        assert_eq!(
            out,
            "+---+-----+\n\
             | x | ten |\n\
             +---+-----+\n\
             | 1 | 10  |\n\
             | 2 | 20  |\n\
             +---+-----+\n"
        );
    }

    #[test]
    fn box_mode_uses_unicode_borders() {
        let mut conn = connection("box.db");
        let out = render(
            &mut conn,
            "UNWIND [1] AS x RETURN x",
            &settings(OutputMode::Box),
        );
        assert_eq!(out, "╭───╮\n│ x │\n╞═══╡\n│ 1 │\n╰───╯\n");
        let out = render(
            &mut conn,
            "UNWIND [1, 2] AS x RETURN x",
            &settings(OutputMode::Box),
        );
        assert_eq!(out, "╭───╮\n│ x │\n╞═══╡\n│ 1 │\n├───┤\n│ 2 │\n╰───╯\n");
    }

    #[test]
    fn qbox_quotes_text_values() {
        let mut conn = connection("qbox.db");
        let out = render(
            &mut conn,
            "UNWIND ['hi'] AS s RETURN s",
            &settings(OutputMode::Qbox),
        );
        assert!(out.contains("│ 'hi' │"), "{out}");
    }

    #[test]
    fn column_and_psql_modes() {
        let mut conn = connection("column.db");
        let out = render(
            &mut conn,
            "UNWIND ['ab'] AS s RETURN s, 7 AS n",
            &settings(OutputMode::Column),
        );
        assert_eq!(out, "s  n\n-- -\nab 7\n");
        let out = render(
            &mut conn,
            "UNWIND ['ab'] AS s RETURN s, 7 AS n",
            &settings(OutputMode::Psql),
        );
        assert_eq!(out, "s  | n\n----+---\nab | 7\n");
    }

    #[test]
    fn markdown_mode() {
        let mut conn = connection("markdown.db");
        let out = render(
            &mut conn,
            "UNWIND [1] AS x RETURN x",
            &settings(OutputMode::Markdown),
        );
        assert_eq!(out, "| x |\n|---|\n| 1 |\n");
    }

    #[test]
    fn list_tabs_csv_and_quote_modes() {
        let mut conn = connection("delimited.db");
        let query = "UNWIND ['a,b'] AS s RETURN s, 2 AS n";
        assert_eq!(
            render(&mut conn, query, &settings(OutputMode::List)),
            "s|n\na,b|2\n"
        );
        assert_eq!(
            render(&mut conn, query, &settings(OutputMode::Tabs)),
            "s\tn\na,b\t2\n"
        );
        assert_eq!(
            render(&mut conn, query, &settings(OutputMode::Csv)),
            "s,n\r\n\"a,b\",2\r\n"
        );
        assert_eq!(
            render(&mut conn, query, &settings(OutputMode::Quote)),
            "'a,b',2\n"
        );
    }

    #[test]
    fn csv_escapes_quotes_and_newlines() {
        assert_eq!(csv_escape("plain"), "plain");
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("two\nlines"), "\"two\nlines\"");
    }

    #[test]
    fn line_mode_prints_name_value_pairs() {
        let mut conn = connection("line.db");
        let out = render(
            &mut conn,
            "UNWIND [1, 2] AS x RETURN x, x * 10 AS ten",
            &settings(OutputMode::Line),
        );
        assert_eq!(out, "x: 1\nten: 10\n\nx: 2\nten: 20\n");
    }

    #[test]
    fn json_mode_emits_ndjson() {
        let mut conn = connection("json.db");
        let out = render(
            &mut conn,
            "UNWIND [1, 2] AS x RETURN x, x * 10 AS ten",
            &settings(OutputMode::Json),
        );
        assert_eq!(out, "{\"x\":1,\"ten\":10}\n{\"x\":2,\"ten\":20}\n");
    }

    #[test]
    fn json_mode_expands_entities() {
        let mut conn = connection("json-entity.db");
        conn.execute("CREATE (:Person {name: 'alice'})")
            .expect("seed");
        let out = render(
            &mut conn,
            "MATCH (n:Person) RETURN n",
            &settings(OutputMode::Json),
        );
        assert_eq!(
            out,
            "{\"n\":{\"id\":1,\"labels\":[\"Person\"],\"properties\":{\"name\":\"alice\"}}}\n"
        );
    }

    #[test]
    fn pretty_expands_nodes_with_attributes() {
        let mut conn = connection("pretty.db");
        conn.execute("CREATE (:Person {name: 'alice', age: 30})")
            .expect("seed");
        let mut pretty = settings(OutputMode::Table);
        pretty.pretty = true;
        let out = render(&mut conn, "MATCH (n:Person) RETURN n", &pretty);
        assert!(out.contains("\"labels\""), "{out}");
        assert!(out.contains("\"Person\""), "{out}");
        assert!(out.contains("\"name\": \"alice\""), "{out}");
        assert!(out.contains("\"age\": 30"), "{out}");
        // Multi-line cell inside a bordered table.
        assert!(out.starts_with("+"), "{out}");
        assert!(out.contains("| {"), "{out}");
    }

    #[test]
    fn pretty_edges_include_endpoints() {
        let mut conn = connection("pretty-edge.db");
        conn.execute("CREATE (:Person {name: 'a'})-[:KNOWS {since: 2020}]->(:Person {name: 'b'})")
            .expect("seed");
        let mut pretty = settings(OutputMode::Line);
        pretty.pretty = true;
        let out = render(&mut conn, "MATCH ()-[r:KNOWS]->() RETURN r", &pretty);
        assert!(out.contains("\"type\": \"KNOWS\""), "{out}");
        assert!(out.contains("\"start\": 1"), "{out}");
        assert!(out.contains("\"end\": 2"), "{out}");
        assert!(out.contains("\"since\": 2020"), "{out}");
    }

    #[test]
    fn pretty_off_keeps_compact_entities() {
        let mut conn = connection("compact.db");
        conn.execute("CREATE (:Person)").expect("seed");
        let out = render(
            &mut conn,
            "MATCH (n:Person) RETURN n",
            &settings(OutputMode::Table),
        );
        assert!(out.contains("(node 1)"), "{out}");
    }

    #[test]
    fn visible_width_ignores_ansi_and_counts_wide_chars() {
        assert_eq!(visible_width("hello"), 5);
        assert_eq!(visible_width("héllo"), 5);
        assert_eq!(visible_width("日本語"), 6);
        assert_eq!(visible_width("\x1b[32mhi\x1b[0m"), 2);
    }

    #[test]
    fn highlight_line_preserves_visible_text() {
        let line = "MATCH (n:Person {name: 'x'}) // note";
        let colored = highlight_line(line);
        let stripped: String = {
            let mut out = String::new();
            let mut chars = colored.chars().peekable();
            while let Some(ch) = chars.next() {
                if ch == '\x1b' {
                    if chars.peek() == Some(&'[') {
                        chars.next();
                    }
                    for next in chars.by_ref() {
                        if ('@'..='~').contains(&next) {
                            break;
                        }
                    }
                    continue;
                }
                out.push(ch);
            }
            out
        };
        assert_eq!(stripped, line);
    }

    #[test]
    fn headers_toggle_hides_titles() {
        let mut conn = connection("headers.db");
        let mut no_headers = settings(OutputMode::List);
        no_headers.headers = false;
        let out = render(&mut conn, "UNWIND [1] AS x RETURN x", &no_headers);
        assert_eq!(out, "1\n");
    }

    #[test]
    fn dot_commands_update_settings() {
        let mut conn = connection("dot.db");
        let mut settings = settings(OutputMode::Table);
        for (command, check) in [(".mode box", 0), (".mode markdown", 0), (".mode nope", 0)] {
            let outcome = run_dot_command(&mut conn, command, &mut settings).expect("command");
            assert!(matches!(outcome, DotOutcome::Continue));
            let _ = check;
        }
        assert_eq!(settings.mode, OutputMode::Markdown);
        run_dot_command(&mut conn, ".headers off", &mut settings).expect("headers");
        assert!(!settings.headers);
        run_dot_command(&mut conn, ".separators off", &mut settings).expect("separators");
        assert!(!settings.row_separators);
        run_dot_command(&mut conn, ".separators on", &mut settings).expect("separators");
        assert!(settings.row_separators);
        run_dot_command(&mut conn, ".pretty on", &mut settings).expect("pretty");
        assert!(settings.pretty);
        run_dot_command(&mut conn, ".colors on", &mut settings).expect("colors");
        assert!(settings.colors_enabled());
        run_dot_command(&mut conn, ".colors off", &mut settings).expect("colors");
        assert!(!settings.colors_enabled());
    }

    #[test]
    fn clear_and_cls_are_dispatched_without_output_when_not_a_tty() {
        let mut conn = connection("clear.db");
        let mut settings = settings(OutputMode::Table);
        for command in [".clear", ".cls"] {
            let outcome = run_dot_command(&mut conn, command, &mut settings).expect("command");
            assert!(matches!(outcome, DotOutcome::Continue));
        }
    }

    #[test]
    fn unknown_command_continues() {
        let mut conn = connection("clear-unknown.db");
        let mut settings = settings(OutputMode::Table);
        let outcome = run_dot_command(&mut conn, ".nope", &mut settings).expect("command");
        assert!(matches!(outcome, DotOutcome::Continue));
    }
}
