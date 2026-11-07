//! grafile shell: an interactive Cypher REPL and script runner.
//!
//! ```text
//! grafile [database]              open (or create) a database and start the REPL
//! grafile [database] -c "QUERY"   run a single query
//! grafile [database] script.cypher
//! grafile check FILE [--rebuild]   verify integrity; exit status 1 on findings
//! ```
//!
//! Options: `--readonly`, `--param NAME=EXPR`, `--init FILE`, `--no-init`.
//! The startup file (`~/.grafilerc`) and scripts may mix dot commands and
//! statements; see [`DOT_COMMANDS`] for the dot commands.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use grafile::api::{Connection, Params, Rows};
use grafile::cypher::lexer::{KEYWORDS, TokenClass, highlight_spans};
use grafile::cypher::split_statements;
use grafile::error::did_you_mean;
use grafile::json::{JsonColors, JsonOptions, format_unix_utc, row_object_json, value_json};
use grafile::planner::procedure_names;
use grafile::record::Value;
use grafile::vdbe::format_value;
use grafile::vdbe::func::FUNCTIONS;
use grafile::vfs::OpenMode;
use grafile::vfs::StdVfs;
use indicatif::{ProgressBar, ProgressStyle};
use rustyline::completion::Completer;
use rustyline::error::ReadlineError;
use rustyline::highlight::{CmdKind, Highlighter};
use rustyline::hint::Hinter;
use rustyline::history::DefaultHistory;
use rustyline::validate::Validator;
use rustyline::{Config, Context as LineContext, Editor};

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--help" || arg == "-h") {
        print_help();
        return Ok(ExitCode::SUCCESS);
    }
    if args.iter().any(|arg| arg == "--version" || arg == "-V") {
        println!("grafile {}", env!("CARGO_PKG_VERSION"));
        return Ok(ExitCode::SUCCESS);
    }
    if args.first().is_some_and(|arg| arg == "check") {
        return run_check(&args[1..]);
    }
    let cli = Cli::parse(&args)?;
    let path = cli.path.as_deref().unwrap_or("grafile.db");
    let mut conn = if cli.readonly {
        Connection::open_with_vfs(Arc::new(StdVfs), Path::new(path), OpenMode::ReadOnly)
            .with_context(|| format!("cannot open `{path}` read-only"))?
    } else {
        open_or_create(path)?
    };
    let mut settings = Settings::new();
    if let Some(init) = cli.init.resolve()? {
        run_startup_file(&mut conn, &init, &mut settings);
    }
    for (name, expression) in &cli.params {
        set_param(&mut conn, &mut settings, name, expression)?;
    }
    if let Some(query) = cli.command {
        run_query(&mut conn, &query, &settings)?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(script) = cli.script {
        let text = std::fs::read_to_string(&script)
            .with_context(|| format!("cannot read script `{script}`"))?;
        let options = ScriptOptions {
            origin: &script,
            stop_on_error: true,
            progress: true,
        };
        run_script(&mut conn, &text, &mut settings, &options)?;
        return Ok(ExitCode::SUCCESS);
    }
    repl(&mut conn, &mut settings)?;
    Ok(ExitCode::SUCCESS)
}

/// Command-line options of the shell (everything but `check`).
#[derive(Debug, Default, PartialEq)]
struct Cli {
    path: Option<String>,
    command: Option<String>,
    script: Option<String>,
    readonly: bool,
    init: InitFile,
    /// `--param NAME=EXPR` bindings, in order.
    params: Vec<(String, String)>,
}

/// Which startup file runs before the session.
#[derive(Debug, Default, PartialEq)]
enum InitFile {
    /// `~/.grafilerc` when it exists.
    #[default]
    Default,
    /// `--init FILE`, which must exist.
    Path(String),
    /// `--no-init`.
    Skip,
}

impl InitFile {
    /// The startup file to run, if any.
    fn resolve(&self) -> Result<Option<std::path::PathBuf>> {
        Ok(match self {
            Self::Default => std::env::home_dir()
                .map(|home| home.join(".grafilerc"))
                .filter(|path| path.is_file()),
            Self::Path(path) => {
                anyhow::ensure!(Path::new(path).is_file(), "startup file `{path}` not found");
                Some(path.into())
            }
            Self::Skip => None,
        })
    }
}

impl Cli {
    /// Parse the shell arguments: `[database] [script]`, `-c QUERY`,
    /// `--readonly`, `--init FILE`, `--no-init` and `--param NAME=EXPR`.
    fn parse(args: &[String]) -> Result<Self> {
        let mut cli = Self::default();
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            let mut value = |flag: &str| {
                args.next()
                    .cloned()
                    .with_context(|| format!("{flag} requires an argument"))
            };
            match arg.as_str() {
                "-c" => cli.command = Some(value("-c")?),
                "--readonly" => cli.readonly = true,
                "--init" => cli.init = InitFile::Path(value("--init")?),
                "--no-init" => cli.init = InitFile::Skip,
                "-p" | "--param" => {
                    let binding = value("--param")?;
                    let (name, expression) = binding
                        .split_once('=')
                        .context("--param expects NAME=EXPRESSION")?;
                    cli.params
                        .push((name.trim().to_owned(), expression.trim().to_owned()));
                }
                other if other.starts_with('-') && other.len() > 1 => {
                    anyhow::bail!("unknown option `{other}`; try --help")
                }
                other if cli.path.is_none() => cli.path = Some(other.to_owned()),
                other if cli.script.is_none() => cli.script = Some(other.to_owned()),
                other => anyhow::bail!("unexpected argument `{other}`"),
            }
        }
        Ok(cli)
    }
}

/// `grafile check FILE [--rebuild]`: print the integrity report and exit
/// with status 1 when findings remain. `--rebuild` first regenerates the
/// derived structures from the records.
fn run_check(args: &[String]) -> Result<ExitCode> {
    let mut rebuild = false;
    let mut path: Option<&str> = None;
    for arg in args {
        match arg.as_str() {
            "--rebuild" => rebuild = true,
            other if path.is_none() => path = Some(other),
            other => anyhow::bail!("unexpected argument `{other}`"),
        }
    }
    let path = path.context("usage: grafile check FILE [--rebuild]")?;
    // A plain check opens read-only so it can never modify the file.
    let mode = if rebuild {
        OpenMode::ReadWrite
    } else {
        OpenMode::ReadOnly
    };
    let mut conn = Connection::open_with_vfs(Arc::new(StdVfs), Path::new(path), mode)
        .with_context(|| format!("cannot open `{path}`"))?;
    let db = conn.database_mut();
    if rebuild {
        let summary = db
            .rebuild_derived(&record_progress("rebuilding"))
            .context("rebuild failed; the file is unchanged")?;
        for abandoned in summary.abandoned() {
            println!("abandoned damaged {abandoned}");
        }
        println!("rebuilt {}", summary.rebuilt().join(", "));
    }
    let report = db.check_integrity_with_progress(&record_progress("checking"))?;
    println!("{report}");
    Ok(if report.is_clean() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Progress over records for `check`; hidden when stderr is not a terminal.
fn record_progress(message: &'static str) -> ProgressBar {
    if !std::io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let style =
        ProgressStyle::with_template("{spinner:.green} {msg} {pos}/{len} [{elapsed_precise}]")
            .expect("valid check progress template");
    let progress = ProgressBar::new(0);
    progress.set_style(style);
    progress.set_message(message);
    progress
}

/// Progress for the script runner; hidden when stderr is not a terminal.
fn script_progress(total: usize) -> ProgressBar {
    if !std::io::stderr().is_terminal() {
        return ProgressBar::hidden();
    }
    let style =
        ProgressStyle::with_template("{spinner:.green} {msg} {pos}/{len} [{elapsed_precise}]")
            .expect("valid script progress template");
    let progress = ProgressBar::new(total as u64);
    progress.set_style(style);
    progress.set_message("running script");
    progress
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
    /// Parameters bound with `.param` or `--param`.
    params: Params,
    /// Suppress dot-command confirmations (scripts and the startup file).
    quiet: bool,
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
            params: Params::new(),
            quiet: false,
        }
    }

    fn colors_enabled(&self) -> bool {
        self.colors.get()
    }

    /// Print a dot-command confirmation unless running quietly.
    fn note(&self, message: &str) {
        if !self.quiet {
            println!("{message}");
        }
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

/// rustyline helper providing Cypher syntax highlighting and completion.
struct GrafileHelper {
    colors: Rc<Cell<bool>>,
    names: Rc<RefCell<CompletionNames>>,
}

impl Completer for GrafileHelper {
    type Candidate = String;

    fn complete(
        &self,
        line: &str,
        pos: usize,
        _ctx: &LineContext<'_>,
    ) -> rustyline::Result<(usize, Vec<String>)> {
        Ok(completions(line, pos, &self.names.borrow()))
    }
}

/// Database and session names offered by completion, refreshed after each
/// statement.
#[derive(Debug, Default, Clone, PartialEq)]
struct CompletionNames {
    /// Labels and relationship types (completed after `:`).
    symbols: Vec<String>,
    /// Property keys (completed after `name.`).
    property_keys: Vec<String>,
    /// Bound parameters (completed after `$`).
    params: Vec<String>,
}

impl CompletionNames {
    fn of(conn: &Connection, settings: &Settings) -> Self {
        let schema = conn.schema();
        Self {
            symbols: schema
                .labels
                .iter()
                .chain(&schema.edge_types)
                .cloned()
                .collect(),
            property_keys: schema.property_keys.clone(),
            params: settings.params.values().keys().cloned().collect(),
        }
    }
}

/// Completion candidates for the word ending at `pos`, and where it starts:
/// dot commands at the start of a line, labels and types after `:`,
/// parameters after `$`, property keys after `name.`, and otherwise
/// keywords, functions and procedures.
fn completions(line: &str, pos: usize, names: &CompletionNames) -> (usize, Vec<String>) {
    let before = &line[..pos];
    let start = before
        .char_indices()
        .rev()
        .find(|(_, c)| !(c.is_alphanumeric() || *c == '_' || *c == '.'))
        .map_or(0, |(index, c)| index + c.len_utf8());
    let word = &before[start..];
    let prefixed = |candidates: &mut dyn Iterator<Item = &str>, prefix: &str| {
        let lowered = prefix.to_lowercase();
        let mut matched: Vec<String> = candidates
            .filter(|candidate| candidate.to_lowercase().starts_with(&lowered))
            .map(str::to_owned)
            .collect();
        matched.sort_unstable();
        matched.dedup();
        matched
    };
    if before[..start].trim().is_empty() && word.starts_with('.') {
        let commands = &mut DOT_COMMANDS.iter().map(|(name, _, _)| *name);
        return (start, prefixed(commands, word));
    }
    match before[..start].chars().next_back() {
        Some(':') => {
            return (
                start,
                prefixed(&mut names.symbols.iter().map(String::as_str), word),
            );
        }
        Some('$') => {
            return (
                start,
                prefixed(&mut names.params.iter().map(String::as_str), word),
            );
        }
        _ => {}
    }
    let callables = || {
        FUNCTIONS
            .iter()
            .map(|(name, _)| *name)
            .chain(procedure_names())
    };
    if let Some((owner, member)) = word.rsplit_once('.') {
        let namespace = format!("{}.", owner.to_lowercase());
        if !callables().any(|name| name.to_lowercase().starts_with(&namespace)) {
            let keys = &mut names.property_keys.iter().map(String::as_str);
            return (start + owner.len() + 1, prefixed(keys, member));
        }
    }
    if word.is_empty() {
        return (start, Vec::new());
    }
    let words = &mut KEYWORDS.iter().copied().chain(callables());
    (start, prefixed(words, word))
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

/// Where interactive history lives: `GRAFILE_HISTORY` (empty disables it)
/// or `~/.grafile_history`. Piped input keeps no history.
fn history_path() -> Option<std::path::PathBuf> {
    if !std::io::stdin().is_terminal() {
        return None;
    }
    match std::env::var_os("GRAFILE_HISTORY") {
        Some(path) if path.is_empty() => None,
        Some(path) => Some(path.into()),
        None => std::env::home_dir().map(|home| home.join(".grafile_history")),
    }
}

fn repl(conn: &mut Connection, settings: &mut Settings) -> Result<()> {
    let mut editor: Editor<GrafileHelper, DefaultHistory> = Editor::with_config(Config::default())?;
    let names = Rc::new(RefCell::new(CompletionNames::of(conn, settings)));
    editor.set_helper(Some(GrafileHelper {
        colors: settings.colors.clone(),
        names: Rc::clone(&names),
    }));
    let history = history_path();
    if let Some(path) = &history {
        // A missing history file just means a first session.
        let _ = editor.load_history(path);
    }
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
                if buffer.is_empty() && trimmed.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(line.as_str());
                if let Some(path) = &history
                    && let Err(err) = editor.append_history(path)
                {
                    eprintln!("warning: cannot save history to {}: {err}", path.display());
                }
                if buffer.is_empty() && trimmed.starts_with('.') {
                    let outcome = run_dot_command(conn, trimmed, settings);
                    *names.borrow_mut() = CompletionNames::of(conn, settings);
                    match outcome {
                        Ok(DotOutcome::Continue) => continue,
                        Ok(DotOutcome::Quit) => break,
                        Err(err) => {
                            eprintln!("error: {err:#}");
                            continue;
                        }
                    }
                }
                buffer.push_str(&line);
                buffer.push('\n');
                // Execute when terminated by ';' or an empty line.
                if trimmed.ends_with(';') || trimmed.is_empty() {
                    let statements = split_statements(&buffer);
                    for statement in statements {
                        if statement.trim().is_empty() {
                            continue;
                        }
                        if let Err(err) = run_query(conn, &statement, settings) {
                            eprintln!("error: {err:#}");
                        }
                    }
                    buffer.clear();
                    *names.borrow_mut() = CompletionNames::of(conn, settings);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
        ".timer" | ".colors" | ".headers" | ".separators" | ".pretty" => {
            let on = match argument {
                "on" => true,
                "off" => false,
                _ => {
                    println!("usage: {command} on|off");
                    return Ok(DotOutcome::Continue);
                }
            };
            let label = match command {
                ".timer" => {
                    settings.timer = on;
                    "timer"
                }
                ".colors" => {
                    settings.colors.set(on);
                    "colors"
                }
                ".headers" => {
                    settings.headers = on;
                    "headers"
                }
                ".separators" => {
                    settings.row_separators = on;
                    "row separators"
                }
                _ => {
                    settings.pretty = on;
                    "pretty"
                }
            };
            settings.note(&format!("{label} {}", if on { "on" } else { "off" }));
        }
        ".mode" => match argument {
            "" => println!("mode: {}", settings.mode.name()),
            "--list" => print_modes(),
            name => match OutputMode::parse(name) {
                Some(mode) => {
                    settings.mode = mode;
                    settings.note(&format!("mode: {}", mode.name()));
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
        ".indexes" => {
            run_query(conn, "SHOW INDEXES", settings)?;
            run_query(conn, "SHOW CONSTRAINTS", settings)?;
        }
        ".clear" | ".cls" => clear_screen(),
        ".param" => run_param_command(conn, argument, settings)?,
        ".read" => {
            let text = std::fs::read_to_string(argument)
                .with_context(|| format!("cannot read `{argument}`"))?;
            let options = ScriptOptions {
                origin: argument,
                stop_on_error: true,
                progress: false,
            };
            return run_script(conn, &text, settings, &options);
        }
        other => anyhow::bail!(
            "unknown command `{other}`{}; try .help",
            did_you_mean(other, DOT_COMMANDS.iter().map(|(name, _, _)| *name))
        ),
    }
    Ok(DotOutcome::Continue)
}

/// `.param set NAME EXPR`, `.param unset NAME`, `.param list` and
/// `.param clear`.
fn run_param_command(conn: &mut Connection, argument: &str, settings: &mut Settings) -> Result<()> {
    let mut words = argument.splitn(2, char::is_whitespace);
    let action = words.next().unwrap_or("");
    let rest = words.next().unwrap_or("").trim();
    match action {
        "" | "list" => {
            let mut bound: Vec<(&String, &Value)> = settings.params.values().iter().collect();
            bound.sort_by(|a, b| a.0.cmp(b.0));
            for (name, value) in bound {
                println!("${name} = {}", quote_literal(value));
            }
        }
        "set" => {
            let (name, expression) = rest
                .split_once(char::is_whitespace)
                .context("usage: .param set NAME EXPRESSION")?;
            set_param(conn, settings, name, expression.trim())?;
        }
        "unset" => {
            if settings.params.remove(rest).is_none() {
                println!("no parameter `{rest}`");
            }
        }
        "clear" => settings.params.clear(),
        other => println!("unknown .param action `{other}`; use set, unset, list or clear"),
    }
    Ok(())
}

/// Bind `name` to the value of a Cypher `expression`, evaluated as
/// `RETURN expression` with the parameters already bound.
fn set_param(
    conn: &mut Connection,
    settings: &mut Settings,
    name: &str,
    expression: &str,
) -> Result<()> {
    let name = name.strip_prefix('$').unwrap_or(name);
    anyhow::ensure!(
        !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_'),
        "invalid parameter name `{name}`"
    );
    let rows = conn
        .query(&format!("RETURN {expression}"), &settings.params)
        .with_context(|| format!("cannot evaluate `{expression}`"))?;
    let [row] = rows.rows() else {
        anyhow::bail!("`{expression}` must produce exactly one value");
    };
    let [value] = row.as_slice() else {
        anyhow::bail!("`{expression}` must produce exactly one value");
    };
    settings.params.insert(name, value.clone());
    Ok(())
}

/// How [`run_script`] labels and reacts to its items.
struct ScriptOptions<'a> {
    /// File name used in error messages.
    origin: &'a str,
    /// Stop at the first failing item (otherwise report it and go on).
    stop_on_error: bool,
    /// Show a progress bar over the statements.
    progress: bool,
}

/// One dot-command line or Cypher statement of a script, with the 1-based
/// line it starts on.
#[derive(Debug, Clone, PartialEq)]
struct ScriptItem {
    line: usize,
    text: String,
    dot: bool,
}

/// Split a script into dot-command lines and statements. A line starting
/// with `.` is a dot command only between statements, so a `.` inside a
/// statement that spans lines stays Cypher.
fn script_items(text: &str) -> Vec<ScriptItem> {
    let mut items = Vec::new();
    let mut chunk_start = 0;
    let mut chunk_line = 1;
    let mut between_statements = true;
    let mut offset = 0;
    for (index, line) in text.split_inclusive('\n').enumerate() {
        let trimmed = line.trim();
        if between_statements && trimmed.starts_with('.') {
            push_statements(&mut items, &text[chunk_start..offset], chunk_line);
            items.push(ScriptItem {
                line: index + 1,
                text: trimmed.to_owned(),
                dot: true,
            });
            chunk_start = offset + line.len();
            chunk_line = index + 2;
        } else if !trimmed.is_empty() && !trimmed.starts_with("//") {
            between_statements = trimmed.ends_with(';');
        }
        offset += line.len();
    }
    push_statements(&mut items, &text[chunk_start..], chunk_line);
    items
}

/// Append the statements of `chunk`, which starts on `first_line`.
fn push_statements(items: &mut Vec<ScriptItem>, chunk: &str, first_line: usize) {
    let mut search = 0;
    for statement in split_statements(chunk) {
        // Statements are verbatim slices of the chunk, so their position
        // gives their line.
        let start = chunk[search..]
            .find(statement.as_str())
            .map_or(search, |found| search + found);
        search = start + statement.len();
        if statement.trim().is_empty() {
            continue;
        }
        let code = start + (statement.len() - statement.trim_start().len());
        items.push(ScriptItem {
            line: first_line + chunk[..code.min(chunk.len())].matches('\n').count(),
            text: statement,
            dot: false,
        });
    }
}

/// Run a script's dot commands and statements in order. Confirmations are
/// silent, as in sqlite; errors name the statement and its line.
fn run_script(
    conn: &mut Connection,
    text: &str,
    settings: &mut Settings,
    options: &ScriptOptions<'_>,
) -> Result<DotOutcome> {
    let items = script_items(text);
    let total = items.iter().filter(|item| !item.dot).count();
    let progress = if options.progress {
        script_progress(total)
    } else {
        ProgressBar::hidden()
    };
    let was_quiet = std::mem::replace(&mut settings.quiet, true);
    let mut statement = 0;
    let mut outcome = Ok(DotOutcome::Continue);
    for item in &items {
        let result = if item.dot {
            run_dot_command(conn, &item.text, settings)
        } else {
            statement += 1;
            progress.set_message(format!("statement {statement}/{total}"));
            let result = run_query(conn, &item.text, settings).map(|()| DotOutcome::Continue);
            progress.inc(1);
            result
        };
        let failure = match result {
            Ok(DotOutcome::Continue) => continue,
            Ok(DotOutcome::Quit) => {
                outcome = Ok(DotOutcome::Quit);
                break;
            }
            Err(error) if item.dot => error.context(format!(
                "{}:{}: `{}` failed",
                options.origin, item.line, item.text
            )),
            Err(error) => error.context(format!(
                "statement {statement} failed at {}:{}",
                options.origin, item.line
            )),
        };
        if options.stop_on_error {
            outcome = Err(failure);
            break;
        }
        eprintln!("error: {failure:#}");
    }
    settings.quiet = was_quiet;
    progress.finish_and_clear();
    outcome
}

/// Run the startup file before the session; failures are reported and the
/// session goes on.
fn run_startup_file(conn: &mut Connection, path: &Path, settings: &mut Settings) {
    let origin = path.display().to_string();
    let options = ScriptOptions {
        origin: &origin,
        stop_on_error: false,
        progress: false,
    };
    let result = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read startup file `{origin}`"))
        .and_then(|text| run_script(conn, &text, settings, &options));
    if let Err(error) = result {
        eprintln!("error: {error:#}");
    }
}

fn print_modes() {
    let names: Vec<&str> = OutputMode::ALL.iter().map(|(name, _)| *name).collect();
    println!("available modes: {}", names.join(" "));
}

fn run_query(conn: &mut Connection, source: &str, settings: &Settings) -> Result<()> {
    let start = Instant::now();
    let rows = conn.query(source, &settings.params)?;
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
        Value::Date(_)
        | Value::LocalTime(_)
        | Value::Time(_)
        | Value::LocalDateTime(_)
        | Value::DateTime(_)
        | Value::Duration(_)
        | Value::Point(_) => format_value(value),
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

/// Every dot command: name, usage and description, for `.help` and
/// completion.
const DOT_COMMANDS: &[(&str, &str, &str)] = &[
    (".help", ".help", "this message"),
    (".schema", ".schema", "labels, edge types and property keys"),
    (".indexes", ".indexes", "SHOW INDEXES and SHOW CONSTRAINTS"),
    (
        ".meta",
        ".meta [raw|human]",
        "database header metadata (default human)",
    ),
    (
        ".mode",
        ".mode [NAME]",
        "output mode (.mode --list to see all)",
    ),
    (
        ".headers",
        ".headers on|off",
        "show column titles (default on)",
    ),
    (
        ".separators",
        ".separators on|off",
        "lines between table rows (default on)",
    ),
    (
        ".colors",
        ".colors on|off",
        "syntax highlighting (default: on when a terminal)",
    ),
    (
        ".pretty",
        ".pretty on|off",
        "render nodes/edges as highlighted JSON",
    ),
    (
        ".param",
        ".param set NAME EXPR",
        "bind $NAME (also unset NAME, list, clear)",
    ),
    (
        ".read",
        ".read FILE",
        "run statements and dot commands from a file",
    ),
    (".timer", ".timer on|off", "show query timing"),
    (".clear", ".clear (.cls)", "clear the terminal screen"),
    (".cls", ".cls", "clear the terminal screen"),
    (".quit", ".quit (.exit)", "exit"),
    (".exit", ".exit", "exit"),
];

fn print_help() {
    let commands: String = DOT_COMMANDS
        .iter()
        .filter(|(name, usage, _)| usage.starts_with(name) && !matches!(*name, ".cls" | ".exit"))
        .map(|(_, usage, description)| format!("  {usage:<22}{description}\n"))
        .collect();
    println!(
        "grafile: a graph database that lives in a file

usage:
  grafile [options] [database]              open or create a database; start the REPL
  grafile [options] [database] -c \"QUERY\"   run one query and exit
  grafile [options] [database] script.cypher
  grafile check FILE [--rebuild]            verify integrity (exit status 1 on findings);
                                            --rebuild regenerates bitmaps and indexes

options:
  --readonly            open an existing database read-only
  --param NAME=EXPR     bind $NAME to a Cypher expression (repeatable; also -p)
  --init FILE           run FILE instead of ~/.grafilerc before the session
  --no-init             skip the startup file

repl (queries end with ';' or an empty line; Tab completes):
{commands}
history is kept in ~/.grafile_history (GRAFILE_HISTORY overrides; empty disables)

modes: table (default), box, qbox, psql, markdown, column,
       list, tabs, csv, quote, line, json

cypher: MATCH, OPTIONAL MATCH, WHERE, RETURN, WITH, UNWIND, LOAD CSV,
        CREATE, MERGE, SET, REMOVE, DELETE/DETACH DELETE, FOREACH, CALL,
        CALL {{ }}, UNION, ORDER BY, SKIP, LIMIT, DISTINCT, aggregations,
        EXISTS/COUNT subqueries, map projections, IS :: TYPE, a built-in
        function library, temporal and spatial values, parameters ($name),
        EXPLAIN
ddl:    CREATE [FULLTEXT] INDEX|CONSTRAINT name [IF NOT EXISTS] ...,
        DROP INDEX|CONSTRAINT name [IF EXISTS], SHOW INDEXES|CONSTRAINTS"
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
            params: Params::new(),
            quiet: false,
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
    fn unknown_command_is_an_error_with_a_suggestion() {
        let mut conn = connection("clear-unknown.db");
        let mut settings = settings(OutputMode::Table);
        let error = run_dot_command(&mut conn, ".mdoe", &mut settings).expect_err("unknown");
        assert!(
            error.to_string().contains("did you mean `.mode`"),
            "{error}"
        );
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

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn script_items_separate_dot_commands_and_track_lines() {
        let text = ".mode csv\nCREATE (:A {x: 1});\n// note\nMATCH (n)\n.x = 1\nRETURN n;\n  .headers off\n";
        let items = script_items(text);
        let summary: Vec<(usize, bool, &str)> = items
            .iter()
            .map(|item| (item.line, item.dot, item.text.trim()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (1, true, ".mode csv"),
                (2, false, "CREATE (:A {x: 1})"),
                (3, false, "// note\nMATCH (n)\n.x = 1\nRETURN n"),
                (7, true, ".headers off"),
            ]
        );
    }

    #[test]
    fn scripts_stop_or_continue_and_restore_quiet() {
        let mut conn = connection("script.db");
        let mut settings = settings(OutputMode::Table);
        let options = ScriptOptions {
            origin: "test.cypher",
            stop_on_error: true,
            progress: false,
        };
        let text = ".headers off\nRETURN 1;\nRETURN nope(;\nCREATE (:Never);\n";
        let error = run_script(&mut conn, text, &mut settings, &options).expect_err("stops");
        assert!(format!("{error:#}").contains("statement 2 failed at test.cypher:3"));
        assert!(!settings.headers);
        assert!(!settings.quiet);
        let options = ScriptOptions {
            stop_on_error: false,
            ..options
        };
        run_script(&mut conn, text, &mut settings, &options).expect("continues");
        let rows = conn
            .query("MATCH (n:Never) RETURN count(n)", &Params::new())
            .expect("count");
        assert_eq!(rows.rows()[0][0], Value::Int(1));
    }

    #[test]
    fn param_command_binds_lists_and_clears() {
        let mut conn = connection("params.db");
        let mut settings = settings(OutputMode::Table);
        for command in [
            ".param set tenant 'acme'",
            ".param set $limit 2 + 3",
            ".param set doubled $limit * 2",
            ".param list",
        ] {
            run_dot_command(&mut conn, command, &mut settings).expect(command);
        }
        let values = settings.params.values();
        assert_eq!(values.get("tenant"), Some(&Value::from("acme")));
        assert_eq!(values.get("limit"), Some(&Value::Int(5)));
        assert_eq!(values.get("doubled"), Some(&Value::Int(10)));
        let rows = conn
            .query("RETURN $tenant", &settings.params)
            .expect("bound parameter");
        assert_eq!(rows.rows()[0][0], Value::from("acme"));
        run_dot_command(&mut conn, ".param unset tenant", &mut settings).expect("unset");
        assert!(!settings.params.values().contains_key("tenant"));
        run_dot_command(&mut conn, ".param clear", &mut settings).expect("clear");
        assert!(settings.params.values().is_empty());
        for bad in [
            ".param set",
            ".param set x",
            ".param set x-y 1",
            ".param set x [1, 2] + nope",
        ] {
            assert!(
                run_dot_command(&mut conn, bad, &mut settings).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn completions_follow_the_cursor_context() {
        let names = CompletionNames {
            symbols: strings(&["Person", "KNOWS"]),
            property_keys: strings(&["name", "age"]),
            params: strings(&["tenant"]),
        };
        assert_eq!(completions(".mo", 3, &names), (0, strings(&[".mode"])));
        assert_eq!(
            completions("MATCH (n:Pe", 11, &names),
            (9, strings(&["Person"]))
        );
        assert_eq!(
            completions("RETURN $te", 10, &names),
            (8, strings(&["tenant"]))
        );
        assert_eq!(
            completions("RETURN n.na", 11, &names),
            (9, strings(&["name"]))
        );
        assert_eq!(
            completions("RETURN toUp", 11, &names),
            (7, strings(&["toUpper"]))
        );
        assert_eq!(
            completions("RETURN date.tr", 14, &names),
            (7, strings(&["date.transaction", "date.truncate"]))
        );
        assert_eq!(completions("ret", 3, &names), (0, strings(&["RETURN"])));
        assert_eq!(completions("MATCH ", 6, &names), (6, Vec::new()));
    }

    #[test]
    fn cli_parses_flags_and_positionals() {
        let args = strings(&[
            "--readonly",
            "db.grafile",
            "--param",
            "x = 1 + 2",
            "-p",
            "y='a=b'",
            "--no-init",
            "-c",
            "RETURN $x",
        ]);
        let cli = Cli::parse(&args).expect("parse");
        assert_eq!(
            cli,
            Cli {
                path: Some("db.grafile".to_owned()),
                command: Some("RETURN $x".to_owned()),
                script: None,
                readonly: true,
                init: InitFile::Skip,
                params: vec![
                    ("x".to_owned(), "1 + 2".to_owned()),
                    ("y".to_owned(), "'a=b'".to_owned())
                ],
            }
        );
        assert!(Cli::parse(&strings(&["--bogus"])).is_err());
        assert!(Cli::parse(&strings(&["--param", "novalue"])).is_err());
        assert!(Cli::parse(&strings(&["--init"])).is_err());
    }
}
