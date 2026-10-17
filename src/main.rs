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
use grafile::cypher::split_statements;
use grafile::json::{JsonColors, JsonOptions, format_unix_utc, row_object_json, value_json};
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

/// How the `.meta` command presents the database header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeaderView {
    /// Raw values as stored in the file.
    Raw,
    /// Interpreted, human-readable view.
    Human,
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

/// CLI palette for JSON token classes.
const JSON_COLORS: JsonColors = JsonColors {
    key: "\x1b[1;34m",
    string: "\x1b[32m",
    number: "\x1b[33m",
    literal: "\x1b[35m",
    reset: RESET,
};

fn json_options(settings: &Settings) -> JsonOptions {
    JsonOptions {
        pretty: settings.pretty,
        colors: settings.colors_enabled().then_some(JSON_COLORS),
    }
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
        ".meta" => match argument {
            "" | "human" => print!("{}", render_header(conn, HeaderView::Human)),
            "raw" => print!("{}", render_header(conn, HeaderView::Raw)),
            other => println!("unknown option `{other}`; usage: .meta [raw|human]"),
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
        let options = json_options(settings);
        return Ok(value_json(conn.database_mut(), value, options, 0)?
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
    let options = json_options(settings);
    let mut out = String::new();
    for row in rows.rows() {
        out.push_str(&row_object_json(
            conn.database_mut(),
            rows.columns(),
            row,
            options,
            0,
        )?);
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

// ---- introspection and help ----

/// Render the database header for the `.meta` command.
fn render_header(conn: &Connection, view: HeaderView) -> String {
    let header = conn.database().header();
    let mut out = String::new();
    match view {
        HeaderView::Raw => {
            out.push_str("magic: GRAFILE\\0\n");
            out.push_str(&format!("format_major: {}\n", header.format_major));
            out.push_str(&format!("format_minor: {}\n", header.format_minor));
            out.push_str(&format!("page_size: {}\n", header.page_size));
            out.push_str(&format!(
                "reserved_per_page: {}\n",
                header.reserved_per_page
            ));
            out.push_str(&format!("encoding: {}\n", header.encoding));
            out.push_str(&format!("page_count: {}\n", header.page_count));
            out.push_str(&format!("freelist_trunk: {}\n", header.freelist_trunk));
            out.push_str(&format!("freelist_count: {}\n", header.freelist_count));
            out.push_str(&format!("catalog_root: {}\n", header.catalog_root));
            out.push_str(&format!("node_root: {}\n", header.node_root));
            out.push_str(&format!("edge_root: {}\n", header.edge_root));
            out.push_str(&format!("fragment_root: {}\n", header.fragment_root));
            out.push_str(&format!("next_node_id: {}\n", header.next_node_id));
            out.push_str(&format!("next_edge_id: {}\n", header.next_edge_id));
            out.push_str(&format!("next_label_id: {}\n", header.next_label_id));
            out.push_str(&format!("next_type_id: {}\n", header.next_type_id));
            out.push_str(&format!("next_key_id: {}\n", header.next_key_id));
            out.push_str(&format!("change_counter: {}\n", header.change_counter));
            out.push_str(&format!(
                "schema_generation: {}\n",
                header.schema_generation
            ));
            out.push_str(&format!("created_at: {}\n", header.created_at));
            out.push_str(&format!("uuid: {}\n", hex(&header.uuid)));
            out.push_str(&format!("user_version: {}\n", header.user_version));
            out.push_str(&format!("application_id: {}\n", header.application_id));
        }
        HeaderView::Human => {
            let logical_size = header.page_count * u64::from(header.page_size);
            let uuid = uuid::Uuid::from_bytes(header.uuid);
            let freelist = if header.freelist_count == 0 {
                "empty".to_owned()
            } else {
                format!(
                    "{} free page{} (trunk page {})",
                    header.freelist_count,
                    if header.freelist_count == 1 { "" } else { "s" },
                    header.freelist_trunk
                )
            };
            out.push_str("grafile database header\n");
            let rows: [(&str, String); 12] = [
                (
                    "file",
                    format!(
                        "{} ({}, {} page{} of {} bytes)",
                        conn.database().path().display(),
                        format_size(logical_size),
                        header.page_count,
                        if header.page_count == 1 { "" } else { "s" },
                        header.page_size
                    ),
                ),
                (
                    "format version",
                    format!("{}.{}", header.format_major, header.format_minor),
                ),
                (
                    "encoding",
                    match header.encoding {
                        0 => "UTF-8".to_owned(),
                        other => format!("unknown ({other})"),
                    },
                ),
                ("created", format_unix_utc(header.created_at)),
                ("database uuid", uuid.to_string()),
                ("change counter", header.change_counter.to_string()),
                ("schema generation", header.schema_generation.to_string()),
                ("user version", header.user_version.to_string()),
                ("application id", header.application_id.to_string()),
                ("freelist", freelist),
                (
                    "roots",
                    format!(
                        "catalog {}, nodes {}, edges {}, fragments {}",
                        header.catalog_root,
                        header.node_root,
                        header.edge_root,
                        header.fragment_root
                    ),
                ),
                (
                    "allocated",
                    format!(
                        "nodes {}, edges {}, labels {}, types {}, keys {}",
                        header.next_node_id.saturating_sub(1),
                        header.next_edge_id.saturating_sub(1),
                        header.next_label_id.saturating_sub(1),
                        header.next_type_id.saturating_sub(1),
                        header.next_key_id.saturating_sub(1)
                    ),
                ),
            ];
            for (label, value) in rows {
                out.push_str(&format!("  {:<20}{}\n", format!("{label}:"), value));
            }
            out.push_str(&format!(
                "  {:<20}{} bytes\n",
                "reserved per page:", header.reserved_per_page
            ));
        }
    }
    out
}

/// Format a byte count with binary units.
fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} bytes")
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
  .meta [raw|human] database header metadata (default human)
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

cypher (v1): MATCH, OPTIONAL MATCH, WHERE, RETURN, WITH, UNWIND,
             CREATE, MERGE, SET, REMOVE, DELETE/DETACH DELETE, FOREACH,
             CALL, UNION, ORDER BY, SKIP, LIMIT, DISTINCT,
             aggregations (count/sum/avg/min/max/collect/percentile/
             stDev) and a built-in function library, parameters ($name)
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
    #[test]
    fn size_formatting_uses_binary_units() {
        assert_eq!(format_size(512), "512 bytes");
        assert_eq!(format_size(8192), "8.0 KiB");
        assert_eq!(format_size(32 * 1024), "32.0 KiB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MiB");
    }

    #[test]
    fn meta_raw_lists_stored_values() {
        let conn = connection("meta-raw.db");
        let header = conn.database().header();
        let raw = render_header(&conn, HeaderView::Raw);
        assert!(raw.contains("magic: GRAFILE\\0"), "{raw}");
        assert!(
            raw.contains(&format!("page_size: {}\n", header.page_size)),
            "{raw}"
        );
        assert!(raw.contains("encoding: 0\n"), "{raw}");
        assert!(
            raw.contains(&format!("created_at: {}\n", header.created_at)),
            "{raw}"
        );
        assert!(
            raw.contains(&format!("uuid: {}\n", hex(&header.uuid))),
            "{raw}"
        );
        assert!(
            raw.contains(&format!("change_counter: {}\n", header.change_counter)),
            "{raw}"
        );
    }

    #[test]
    fn meta_human_interpretation() {
        let conn = connection("meta-human.db");
        let header = conn.database().header();
        let human = render_header(&conn, HeaderView::Human);
        assert!(human.contains("grafile database header"), "{human}");
        assert!(human.contains("UTF-8"), "{human}");
        assert!(human.contains("meta-human.db"), "{human}");
        assert!(human.contains("KiB"), "{human}");
        assert!(human.contains("catalog"), "{human}");
        assert!(
            human.contains(&format_unix_utc(header.created_at)),
            "{human}"
        );
        assert!(human.contains("freelist:"), "{human}");
        assert!(human.contains("empty"), "{human}");
        // Default-constructed database: only catalog/nodes/edges/fragments roots
        // exist, no user nodes yet.
        assert!(human.contains("allocated:"), "{human}");
        assert!(human.contains("nodes 0"), "{human}");
    }

    #[test]
    fn meta_dot_command_accepts_views_and_rejects_unknown() {
        let mut conn = connection("meta-dot.db");
        let mut settings = settings(OutputMode::Table);
        for command in [".meta", ".meta human", ".meta raw", ".meta nope"] {
            let outcome = run_dot_command(&mut conn, command, &mut settings).expect("command");
            assert!(matches!(outcome, DotOutcome::Continue));
        }
    }
}
