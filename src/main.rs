use std::env;
use std::fs::File;
use std::io::{self, Write};
use std::process::ExitCode;

use csv_row_diff::{LogError, LogOptions, log_changes, split_headers};

const RESET: &str = "\u{1b}[0m";
const BOLD: &str = "\u{1b}[1m";
const RED: &str = "\u{1b}[31m";
const CYAN: &str = "\u{1b}[36m";

fn main() -> ExitCode {
    enable_ansi();
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
            let _ = writeln!(io::stderr(), "{BOLD}{RED}ERROR{RESET} {err}{RESET}");
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
        "{BOLD}{CYAN}csv-row-diff{RESET} logs value changes between rows of a CSV file

{BOLD}USAGE{RESET}
    csv-row-diff <CSV_FILE> [HEADERS]
    csv-row-diff --file <CSV_FILE> [--headers <HEADERS> | --except <HEADERS>]

{BOLD}ARGS{RESET}
    <CSV_FILE>    Path to the CSV file, or - to read from stdin
    [HEADERS]     Comma-separated header names to watch

{BOLD}OPTIONS{RESET}
    -f, --file <CSV_FILE>       CSV input path
    -H, --headers <HEADERS>     Watch only these columns
    -x, --except <HEADERS>      Watch every data column except these
    -n, --no-init               Do not log initial values
    --from <TIME>               Inclusive start of the incident window
    --to <TIME>                 Inclusive end of the incident window
    -h, --help                  Show this help
    -V, --version               Show version

The first row is the header row. Data starts on the next record.
The first column is the row time and is included with each change.
With no header list, every column except the time column is watched.
--headers is a whitelist and --except is a blacklist; they cannot be combined.
Header matching is case-sensitive. Missing names are warnings.
--from and --to match clock times (9:59:59, 10:00:00) or timestamps
(2024-01-01T09:59:59) by time, not raw text. Unrecognized values use byte order.
A window suppresses logs outside that range and prints a per-column summary.
Initial values are still logged unless --no-init is set; with a window they are the values held when the window starts.
When the time column can be parsed, each change shows how long the previous value was held.
Rows are read incrementally; a change is detected by comparing field bytes.

{BOLD}EXAMPLE{RESET}
    csv-row-diff data.csv
    csv-row-diff data.csv speed,temperature
    csv-row-diff data.csv --except comment --from 2024-01-01T00:00:02 --to 2024-01-01T00:05:00 --no-init
"
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

/// Classic Windows consoles ignore ANSI unless virtual-terminal mode is enabled.
#[cfg(windows)]
fn enable_ansi() {
    const STD_OUTPUT_HANDLE: i32 = -11;
    const STD_ERROR_HANDLE: i32 = -12;
    const ENABLE_VIRTUAL_TERMINAL_PROCESSING: u32 = 0x0004;

    unsafe extern "system" {
        fn GetStdHandle(n_std_handle: i32) -> *mut core::ffi::c_void;
        fn GetConsoleMode(handle: *mut core::ffi::c_void, mode: *mut u32) -> i32;
        fn SetConsoleMode(handle: *mut core::ffi::c_void, mode: u32) -> i32;
    }

    unsafe {
        for handle_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let handle = GetStdHandle(handle_id);
            if handle.is_null() || handle == (-1isize as *mut core::ffi::c_void) {
                continue;
            }
            let mut mode = 0;
            if GetConsoleMode(handle, &mut mode) != 0 {
                let _ = SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING);
            }
        }
    }
}

#[cfg(not(windows))]
fn enable_ansi() {}
