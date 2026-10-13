//! Command-line entry point for the openCypher TCK harness.

use std::path::PathBuf;
use std::process::ExitCode;

use tck_runner::runner::{self, Config};

const USAGE: &str = "\
tck-runner: openCypher TCK harness for grafile

Usage: tck-runner [OPTIONS]

Options:
  --features-dir <path>  TCK feature files [third_party/opencypher/tck/features]
  --graphs-dir <path>    named graph scripts [third_party/opencypher/tck/graphs]
  --allowlist <path>     allow-list file [tck/allowlist.json]
  --report <path>        report output [tck/report.json]
  --filter <text>        only run scenarios whose `file: name` contains <text>
  --write-allowlist      regenerate the allow-list from the current results
  --quiet                suppress the progress bar
  -h, --help             print this help
";

fn main() -> ExitCode {
    let mut config = Config::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--features-dir" => match args.next() {
                Some(value) => config.features_dir = PathBuf::from(value),
                None => return usage_error("--features-dir requires a value"),
            },
            "--graphs-dir" => match args.next() {
                Some(value) => config.graphs_dir = PathBuf::from(value),
                None => return usage_error("--graphs-dir requires a value"),
            },
            "--allowlist" => match args.next() {
                Some(value) => config.allowlist_path = PathBuf::from(value),
                None => return usage_error("--allowlist requires a value"),
            },
            "--report" => match args.next() {
                Some(value) => config.report_path = PathBuf::from(value),
                None => return usage_error("--report requires a value"),
            },
            "--filter" => match args.next() {
                Some(value) => config.filter = Some(value),
                None => return usage_error("--filter requires a value"),
            },
            "--write-allowlist" => config.write_allowlist = true,
            "--quiet" => config.quiet = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => return usage_error(&format!("unknown argument `{other}`")),
        }
    }

    match runner::run(&config) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("tck-runner: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("tck-runner: {message}\n\n{USAGE}");
    ExitCode::FAILURE
}
