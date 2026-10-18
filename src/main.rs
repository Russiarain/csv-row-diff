use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;

use csv_row_diff::{LogError, LogOptions, log_changes, split_headers};

use colored::Colorize;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Help) => {
            println!("{}", usage());
            ExitCode::SUCCESS
        }
        Err(CliError::Version) => {
            println!("csv-row-diff {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Err(CliError::Usage(message)) => {
            let _ = writeln!(io::stderr(), "{message}");
            ExitCode::from(2)
        }
        Err(err) => {
            let _ = writeln!(io::stderr(), "{} {err}", "ERROR".red().bold());
            ExitCode::from(1)
        }
    }
}

fn run() -> Result<(), CliError> {
    let args = parse_args(env::args().skip(1))?;
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    if args.file == "-" {
        log_changes(stdin.lock(), &args.options, &mut stdout).map_err(CliError::Run)
    } else {
        let file = File::open(&args.file).map_err(|err| CliError::Open {
            path: args.file,
            source: err,
        })?;
        log_changes(file, &args.options, &mut stdout).map_err(CliError::Run)
    }
}

struct Args {
    file: String,
    options: LogOptions,
}

fn parse_args<I>(args: I) -> Result<Args, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut file = None;
    let mut headers = None;
    let mut except = None;
    let mut from = None;
    let mut to = None;
    let mut no_init = false;
    let mut positionals = Vec::new();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-h" | "--help" => return Err(CliError::Help),
            "-V" | "--version" => return Err(CliError::Version),
            "-n" | "--no-init" => no_init = true,
            "-f" | "--file" => {
                file = Some(flag_value(&mut file, &mut args, "--file")?);
            }
            "-H" | "--headers" => {
                headers = Some(flag_value(&mut headers, &mut args, "--headers")?);
            }
            "-x" | "--except" => {
                except = Some(flag_value(&mut except, &mut args, "--except")?);
            }
            "--from" => {
                from = Some(flag_value(&mut from, &mut args, "--from")?);
            }
            "--to" => {
                to = Some(flag_value(&mut to, &mut args, "--to")?);
            }
            "--" => {
                positionals.extend(args);
                break;
            }
            value if value.starts_with('-') && value != "-" => {
                return Err(CliError::Usage(format!(
                    "unknown option '{value}'\n\n{}",
                    usage()
                )));
            }
            value => positionals.push(value.to_string()),
        }
    }

    if file.is_none() {
        file = positionals.first().cloned();
    }
    let header_index = usize::from(file.is_some() && positionals.first() == file.as_ref());
    if let Some(positional_headers) = positionals.get(header_index) {
        if headers.is_some() {
            return Err(CliError::Usage(
                "headers were provided both as an argument and with --headers".to_string(),
            ));
        }
        headers = Some(positional_headers.clone());
    }
    if positionals.len() > header_index + 1 {
        return Err(CliError::Usage(format!(
            "unexpected argument '{}'\n\n{}",
            positionals[header_index + 1],
            usage()
        )));
    }

    if headers.is_some() && except.is_some() {
        return Err(CliError::Usage(
            "--headers and --except cannot be used together; use a whitelist or a blacklist"
                .to_string(),
        ));
    }

    let file = file.ok_or_else(|| CliError::Usage(usage()))?;
    let options = LogOptions {
        headers: headers.map(|value| split_headers(&value)),
        except: except.map(|value| split_headers(&value)),
        no_init,
        from: from.map(|value| value.trim().to_string()),
        to: to.map(|value| value.trim().to_string()),
    };
    Ok(Args { file, options })
}

fn flag_value<I>(slot: &mut Option<String>, args: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    if slot.is_some() {
        return Err(CliError::Usage(format!(
            "{flag} was provided more than once"
        )));
    }
    next_value(args, flag)
}

fn next_value<I>(args: &mut I, flag: &str) -> Result<String, CliError>
where
    I: Iterator<Item = String>,
{
    args.next()
        .ok_or_else(|| CliError::Usage(format!("missing value for {flag}\n\n{}", usage())))
}

fn usage() -> String {
    format!(
"{} - logs value changes between rows of a CSV file

{}
    csv-row-diff <CSV_FILE> [HEADERS]
    csv-row-diff --file <CSV_FILE> [--headers <HEADERS> | --except <HEADERS>]

{}
    <CSV_FILE>    Path to the CSV file, or - to read from stdin
    [HEADERS]     Comma-separated header names to watch

{}
    -f, --file <CSV_FILE>       CSV input path
    -H, --headers <HEADERS>     Watch only these columns
    -x, --except <HEADERS>      Watch every data column except these
    -n, --no-init               Do not log initial values
    --from <TIME>               Inclusive start of the incident window
    --to <TIME>                 Inclusive end of the incident window
    -h, --help                  Show this help
    -V, --version               Show version

{}
    csv-row-diff data.csv
    csv-row-diff data.csv speed,temperature
    csv-row-diff data.csv --except comment --from 2024-01-01T00:00:02 --to 2024-01-01T00:05:00 --no-init
", "csv-row-diff".blue().bold(), "USAGE".bold(), "ARGS".bold(), "OPTIONS".bold(), "EXAMPLE".bold()
    )
}

enum CliError {
    Help,
    Version,
    Usage(String),
    Open { path: String, source: io::Error },
    Run(LogError),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CliError::Help => write!(f, "{}", usage()),
            CliError::Version => write!(f, "csv-row-diff {}", env!("CARGO_PKG_VERSION")),
            CliError::Usage(message) => write!(f, "{message}"),
            CliError::Open { path, source } => {
                write!(f, "failed to open '{path}': {source}")
            }
            CliError::Run(err) => write!(f, "{err}"),
        }
    }
}
