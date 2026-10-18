use std::cmp::Ordering;
use std::io::{self, BufWriter, Read, Write};
use std::time::Duration;

use colored::Colorize;
use csv::{ByteRecord, Reader, ReaderBuilder};

const READ_BUFFER_BYTES: usize = 1024 * 1024;
const WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// Failure while reading the CSV or writing a log line.
#[derive(Debug)]
pub enum LogError {
    /// The caller did not provide any header name to watch.
    NoHeaders,
    /// A whitelist and a blacklist were both provided.
    ConflictingSelection,
    /// The input has no header record.
    EmptyCsv,
    /// The CSV reader failed.
    Csv(csv::Error),
    /// Writing a log line failed.
    Io(io::Error),
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogError::NoHeaders => write!(f, "no header names were provided"),
            LogError::ConflictingSelection => {
                write!(f, "header whitelist and blacklist cannot be used together")
            }
            LogError::EmptyCsv => write!(f, "CSV file is empty or has no header row"),
            LogError::Csv(err) => write!(f, "failed to read CSV: {err}"),
            LogError::Io(err) => write!(f, "failed to write log: {err}"),
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            LogError::Csv(err) => Some(err),
            LogError::Io(err) => Some(err),
            LogError::NoHeaders | LogError::ConflictingSelection | LogError::EmptyCsv => None,
        }
    }
}

impl From<csv::Error> for LogError {
    fn from(err: csv::Error) -> Self {
        LogError::Csv(err)
    }
}

impl From<io::Error> for LogError {
    fn from(err: io::Error) -> Self {
        LogError::Io(err)
    }
}

/// Split a comma-separated header list. Empty segments are preserved so the
/// caller can warn about them; surrounding whitespace is removed.
pub fn split_headers(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(|name| name.trim().to_string())
        .collect()
}

/// What to log while streaming a CSV file.
///
/// `headers` is a whitelist. `except` is a blacklist. Leave both unset to watch
/// every column except the first, which is always the row time. The two lists
/// cannot be set together.
///
/// `from` and `to` are inclusive bounds on the time column. Clock times such as
/// `9:59:59` and `10:00:00` are ordered by time of day, not raw text. Dated
/// timestamps such as `2024-01-01T09:59:59` are ordered by date and time.
/// Unrecognized values fall back to byte order. When either bound is set, rows
/// outside the window are not logged. The file is assumed to be non-decreasing
/// in time; a backward step disables the early stop at `to`.
#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    /// Header names to watch, in report order. `None` means all data columns.
    pub headers: Option<Vec<String>>,
    /// Header names to skip when watching all data columns.
    pub except: Option<Vec<String>>,
    /// Skip the initial-value lines.
    pub no_init: bool,
    /// Inclusive start of the incident window.
    pub from: Option<String>,
    /// Inclusive end of the incident window.
    pub to: Option<String>,
}

impl LogOptions {
    fn window_active(&self) -> bool {
        self.from.is_some() || self.to.is_some()
    }
}

struct Watched {
    name: String,
    /// Index into each data record. Compared and copied as bytes only.
    index: usize,
}

/// Read `input` record by record and write colored change logs to `output`.
///
/// The first CSV column is always treated as the row time.
pub fn log_changes<R, W>(input: R, options: &LogOptions, output: &mut W) -> Result<(), LogError>
where
    R: Read,
    W: Write,
{
    let mut writer = BufWriter::with_capacity(WRITE_BUFFER_BYTES, output);
    let result = log_changes_buffered(input, options, &mut writer);
    // Flush even when scanning fails so a partial report is not discarded.
    let flush_result = writer.flush().map_err(LogError::Io);
    match (result, flush_result) {
        (Err(err), _) => Err(err),
        (Ok(()), flush) => flush,
    }
}

fn log_changes_buffered<R, W>(
    input: R,
    options: &LogOptions,
    output: &mut W,
) -> Result<(), LogError>
where
    R: Read,
    W: Write,
{
    if options.headers.is_some() && options.except.is_some() {
        return Err(LogError::ConflictingSelection);
    }
    if options
        .headers
        .as_ref()
        .is_some_and(|headers| headers.iter().all(|name| name.trim().is_empty()))
    {
        return Err(LogError::NoHeaders);
    }

    let mut reader = ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .buffer_capacity(READ_BUFFER_BYTES)
        .from_reader(input);

    let header_record = reader.byte_headers()?.clone();
    if header_record.is_empty() {
        return Err(LogError::EmptyCsv);
    }

    let watched = select_columns(&header_record, options, output)?;
    if watched.is_empty() {
        if options.headers.is_none() {
            write_warn(output, "no columns left to watch")?;
        }
        return Ok(());
    }

    scan_records(&mut reader, &watched, options, output)
}

fn select_columns<W: Write>(
    header_record: &ByteRecord,
    options: &LogOptions,
    output: &mut W,
) -> Result<Vec<Watched>, LogError> {
    let csv_headers: Vec<String> = header_record
        .iter()
        .enumerate()
        .map(|(index, cell)| {
            let cell = if index == 0 { strip_bom(cell) } else { cell };
            String::from_utf8_lossy(cell).trim().to_string()
        })
        .collect();

    if let Some(requested) = &options.headers {
        return select_named(&csv_headers, requested, output);
    }
    select_all(
        &csv_headers,
        options.except.as_deref().unwrap_or(&[]),
        output,
    )
}

fn select_all<W: Write>(
    csv_headers: &[String],
    except: &[String],
    output: &mut W,
) -> Result<Vec<Watched>, LogError> {
    let mut excluded = Vec::new();
    for name in except {
        let name = name.trim();
        if name.is_empty() {
            write_warn(output, "ignoring an empty header name in the exclude list")?;
            continue;
        }
        if excluded.iter().any(|existing: &String| existing == name) {
            write_warn_header(output)?;
            write_quoted(output, name.as_bytes())?;
            writeln!(
                output,
                " was excluded more than once; ignoring the duplicate"
            )?;
            continue;
        }
        excluded.push(name.to_string());
        if csv_headers.iter().skip(1).any(|header| header == name) {
            continue;
        }
        write_warn_header(output)?;
        write_quoted(output, name.as_bytes())?;
        if csv_headers.first().is_some_and(|header| header == name) {
            writeln!(output, " is the time column and is never watched")?;
        } else {
            writeln!(output, " does not exist in the CSV file")?;
        }
    }

    let mut watched = Vec::new();
    let mut seen = Vec::new();
    for (index, name) in csv_headers.iter().enumerate().skip(1) {
        if name.is_empty() {
            write_warn(output, "ignoring an empty CSV header")?;
            continue;
        }
        if excluded.iter().any(|excluded| excluded == name) {
            continue;
        }
        if seen.iter().any(|existing: &String| existing == name) {
            write_warn_header(output)?;
            write_quoted(output, name.as_bytes())?;
            writeln!(
                output,
                " occurs more than once in the CSV; watching the first column only"
            )?;
            continue;
        }
        seen.push(name.clone());
        watched.push(Watched {
            name: name.clone(),
            index,
        });
    }
    Ok(watched)
}

fn select_named<W: Write>(
    csv_headers: &[String],
    requested: &[String],
    output: &mut W,
) -> Result<Vec<Watched>, LogError> {
    let mut watched = Vec::new();
    let mut seen = Vec::new();

    for name in requested {
        let name = name.trim();
        if name.is_empty() {
            write_warn(output, "ignoring an empty header name in the input list")?;
            continue;
        }
        if seen.iter().any(|existing: &String| existing == name) {
            write_warn_header(output)?;
            write_quoted(output, name.as_bytes())?;
            writeln!(output, " was provided more than once; watching it once")?;
            continue;
        }
        seen.push(name.to_string());

        let matches: Vec<usize> = csv_headers
            .iter()
            .enumerate()
            .filter(|(_, header)| header.as_str() == name)
            .map(|(index, _)| index)
            .collect();

        match matches.as_slice() {
            [] => {
                write_warn_header(output)?;
                write_quoted(output, name.as_bytes())?;
                writeln!(output, " does not exist in the CSV file")?;
            }
            [index, extra @ ..] => {
                if !extra.is_empty() {
                    write_warn_header(output)?;
                    write_quoted(output, name.as_bytes())?;
                    writeln!(
                        output,
                        " occurs more than once in the CSV; watching the first column ({})",
                        index + 1
                    )?;
                }
                watched.push(Watched {
                    name: name.to_string(),
                    index: *index,
                });
            }
        }
    }

    Ok(watched)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WindowPos {
    Before,
    Inside,
    After,
}

fn scan_records<R, W>(
    reader: &mut Reader<R>,
    watched: &[Watched],
    options: &LogOptions,
    output: &mut W,
) -> Result<(), LogError>
where
    R: Read,
    W: Write,
{
    let from = options.from.as_ref().map(|bound| bound.as_bytes());
    let to = options.to.as_ref().map(|bound| bound.as_bytes());
    let mut record = ByteRecord::new();
    // Only the watched cells are retained, and only the previous row of them.
    let mut previous: Vec<Vec<u8>> = Vec::with_capacity(watched.len());
    let mut warned_short_row = false;
    let mut warned_backward_time = false;
    let mut stop_at_end = to.is_some();
    let mut last_time: Vec<u8> = Vec::new();
    let mut held_since: Vec<Option<ParsedTime>> = Vec::with_capacity(watched.len());
    let mut logged_init = false;

    while reader.read_byte_record(&mut record)? {
        let line = record.position().map(|pos| pos.line()).unwrap_or(0);
        let time = record.get(0).unwrap_or(b"");
        if !last_time.is_empty() && cmp_time(time, &last_time) == Ordering::Less {
            stop_at_end = false;
            if options.window_active() && !warned_backward_time {
                write_warn(
                    output,
                    "time column went backwards; the rest of the file will be scanned",
                )?;
                warned_backward_time = true;
            }
        }
        last_time.clear();
        last_time.extend_from_slice(time);

        let pos = window_pos(time, from, to);
        if pos == WindowPos::After && stop_at_end {
            break;
        }

        let parsed_time = parse_time(time);
        if previous.is_empty() {
            for column in watched {
                let value = field_at(&record, column.index, line, &mut warned_short_row, output)?;
                if pos == WindowPos::Inside && !options.no_init {
                    write_init(output, &column.name, value)?;
                    logged_init = true;
                }
                previous.push(value.to_vec());
                held_since.push(parsed_time);
            }
            continue;
        }

        // Own only the cells that differ. The count decides the log layout.
        let mut changed: Vec<Change> = Vec::new();
        for (index, (column, prev)) in watched.iter().zip(previous.iter()).enumerate() {
            let value = field_at(&record, column.index, line, &mut warned_short_row, output)?;
            // Byte comparison: do not decode or allocate unless the cell changed.
            if prev.as_slice() != value {
                let held = held_since
                    .get(index)
                    .and_then(|start| start.zip(parsed_time))
                    .and_then(|(start, end)| duration_between(start, end));
                changed.push(Change {
                    index,
                    after: value.to_vec(),
                    held,
                });
            }
        }

        if pos == WindowPos::Inside {
            if !logged_init && !options.no_init {
                for (column, value) in watched.iter().zip(previous.iter()) {
                    write_init(output, &column.name, value)?;
                }
                logged_init = true;
            }
            log_changes_on_row(output, line, time, watched, &previous, &changed)?;
        }

        for change in changed {
            let prev = &mut previous[change.index];
            prev.clear();
            prev.extend_from_slice(&change.after);
            if let Some(since) = held_since.get_mut(change.index) {
                *since = parsed_time;
            }
        }
    }

    Ok(())
}

fn window_pos(time: &[u8], from: Option<&[u8]>, to: Option<&[u8]>) -> WindowPos {
    if from.is_some_and(|bound| cmp_time(time, bound) == Ordering::Less) {
        WindowPos::Before
    } else if to.is_some_and(|bound| cmp_time(time, bound) == Ordering::Greater) {
        WindowPos::After
    } else {
        WindowPos::Inside
    }
}

/// Order two time cells. Parsed clock times and timestamps compare by their
/// parts, so an unpadded hour does not sort after a later hour.
fn cmp_time(left: &[u8], right: &[u8]) -> Ordering {
    match (parse_time(left), parse_time(right)) {
        (Some(left_time), Some(right_time)) if left_time.kind == right_time.kind => {
            left_time.cmp(&right_time)
        }
        _ => left.cmp(right),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum TimeKind {
    Clock,
    Date,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct ParsedTime {
    kind: TimeKind,
    /// `YYYYMMDD` for dated timestamps; unused for clock times.
    date: u32,
    secs: u32,
    nanos: u32,
}

fn parse_time(raw: &[u8]) -> Option<ParsedTime> {
    let text = std::str::from_utf8(raw).ok()?.trim();
    let bytes = text.as_bytes();
    let mut index = 0;
    if looks_like_date(bytes) {
        let year = read_uint(bytes, &mut index, 4, 4)?;
        let sep = *bytes.get(index)?;
        if sep != b'-' && sep != b'/' {
            return None;
        }
        index += 1;
        let month = read_uint(bytes, &mut index, 1, 2)?;
        let sep = *bytes.get(index)?;
        if sep != b'-' && sep != b'/' {
            return None;
        }
        index += 1;
        let day = read_uint(bytes, &mut index, 1, 2)?;
        if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
            return None;
        }
        let sep = *bytes.get(index)?;
        if sep != b'T' && sep != b't' && sep != b' ' {
            return None;
        }
        index += 1;
        let (secs, nanos) = read_hms(bytes, &mut index)?;
        skip_zone(bytes, &mut index)?;
        if index != bytes.len() {
            return None;
        }
        return Some(ParsedTime {
            kind: TimeKind::Date,
            date: year * 10_000 + month * 100 + day,
            secs,
            nanos,
        });
    }

    let (secs, nanos) = read_hms(bytes, &mut index)?;
    skip_zone(bytes, &mut index)?;
    if index != bytes.len() {
        return None;
    }
    Some(ParsedTime {
        kind: TimeKind::Clock,
        date: 0,
        secs,
        nanos,
    })
}

fn looks_like_date(bytes: &[u8]) -> bool {
    bytes.len() >= 5
        && bytes[..4].iter().all(u8::is_ascii_digit)
        && (bytes[4] == b'-' || bytes[4] == b'/')
}

fn read_hms(bytes: &[u8], index: &mut usize) -> Option<(u32, u32)> {
    let hour = read_uint(bytes, index, 1, 2)?;
    if bytes.get(*index).copied() != Some(b':') {
        return None;
    }
    *index += 1;
    let minute = read_uint(bytes, index, 1, 2)?;
    if bytes.get(*index).copied() != Some(b':') {
        return None;
    }
    *index += 1;
    let second = read_uint(bytes, index, 1, 2)?;
    if hour > 99 || minute > 59 || second > 59 {
        return None;
    }
    let nanos = read_nanos(bytes, index)?;
    Some((hour * 3600 + minute * 60 + second, nanos))
}

fn read_nanos(bytes: &[u8], index: &mut usize) -> Option<u32> {
    if bytes
        .get(*index)
        .is_none_or(|byte| *byte != b'.' && *byte != b',')
    {
        return Some(0);
    }
    *index += 1;
    let mut digits = 0u32;
    let mut count = 0u32;
    while count < 9 && bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        digits = digits * 10 + u32::from(bytes[*index] - b'0');
        *index += 1;
        count += 1;
    }
    if count == 0 {
        return None;
    }
    while bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        *index += 1;
    }
    Some(digits * 10u32.pow(9 - count))
}

fn read_uint(bytes: &[u8], index: &mut usize, min_digits: usize, max_digits: usize) -> Option<u32> {
    let start = *index;
    let mut value = 0u32;
    while *index - start < max_digits && bytes.get(*index).is_some_and(u8::is_ascii_digit) {
        value = value
            .checked_mul(10)?
            .checked_add(u32::from(bytes[*index] - b'0'))?;
        *index += 1;
    }
    (*index - start >= min_digits).then_some(value)
}

fn skip_zone(bytes: &[u8], index: &mut usize) -> Option<()> {
    if *index >= bytes.len() {
        return Some(());
    }
    if bytes[*index] == b'Z' || bytes[*index] == b'z' {
        *index += 1;
        return (*index == bytes.len()).then_some(());
    }
    if bytes[*index] == b'+' || bytes[*index] == b'-' {
        *index += 1;
        let hour = read_uint(bytes, index, 2, 2)?;
        if bytes.get(*index).copied() == Some(b':') {
            *index += 1;
        }
        let minute = read_uint(bytes, index, 2, 2)?;
        if hour > 23 || minute > 59 || *index != bytes.len() {
            return None;
        }
        return Some(());
    }
    None
}

struct Change {
    index: usize,
    after: Vec<u8>,
    held: Option<Duration>,
}

fn log_changes_on_row<W: Write>(
    output: &mut W,
    line: u64,
    time: &[u8],
    watched: &[Watched],
    previous: &[Vec<u8>],
    changed: &[Change],
) -> Result<(), LogError> {
    match changed {
        [] => Ok(()),
        [change] => write_single_change(
            output,
            line,
            time,
            &watched[change.index].name,
            &previous[change.index],
            &change.after,
            change.held,
        ),
        _ => {
            write_change_header(output, line, time)?;
            writeln!(output)?;
            for change in changed {
                write_change_item(
                    output,
                    &watched[change.index].name,
                    &previous[change.index],
                    &change.after,
                    change.held,
                )?;
            }
            Ok(())
        }
    }
}

fn field_at<'a, W: Write>(
    record: &'a ByteRecord,
    index: usize,
    line: u64,
    warned_short_row: &mut bool,
    output: &mut W,
) -> Result<&'a [u8], LogError> {
    match record.get(index) {
        Some(value) => Ok(value),
        None => {
            if !*warned_short_row {
                write_warn(
                    output,
                    &format!(
                        "row at line {line} has fewer columns than a watched header; missing cells are treated as empty"
                    ),
                )?;
                *warned_short_row = true;
            }
            Ok(b"")
        }
    }
}

fn write_warn_header<W: Write>(output: &mut W) -> Result<(), LogError> {
    write!(output, "{} header ", "WARN".bold().yellow())?;
    Ok(())
}

fn write_warn<W: Write>(output: &mut W, message: &str) -> Result<(), LogError> {
    writeln!(output, "{} {message}", "WARN".bold().yellow())?;
    Ok(())
}

fn write_init<W: Write>(output: &mut W, header: &str, value: &[u8]) -> Result<(), LogError> {
    write!(output, "{} ", "INIT".bold().cyan())?;
    write!(output, "{}", escape_cell(header.as_bytes()).bold())?;
    write!(output, " = ")?;
    write!(output, "{}", escape_cell(value).green())?;
    writeln!(output)?;
    Ok(())
}

fn write_single_change<W: Write>(
    output: &mut W,
    line: u64,
    time: &[u8],
    header: &str,
    before: &[u8],
    after: &[u8],
    held: Option<Duration>,
) -> Result<(), LogError> {
    write_change_header(output, line, time)?;
    write!(output, " ")?;
    write_delta(output, header, before, after, held)?;
    writeln!(output)?;
    Ok(())
}

fn write_change_header<W: Write>(output: &mut W, line: u64, time: &[u8]) -> Result<(), LogError> {
    write!(output, "{}", "[".dimmed())?;
    write!(output, "{}", format!("L{line}").blue())?;
    write!(output, "{}", ", ".dimmed())?;
    write!(output, "{}", escape_cell(time).cyan())?;
    write!(output, "{}", "]".dimmed())?;
    Ok(())
}

fn write_change_item<W: Write>(
    output: &mut W,
    header: &str,
    before: &[u8],
    after: &[u8],
    held: Option<Duration>,
) -> Result<(), LogError> {
    write!(output, "\t{} ", "*".dimmed())?;
    write_delta(output, header, before, after, held)?;
    writeln!(output)?;
    Ok(())
}

fn write_delta<W: Write>(
    output: &mut W,
    header: &str,
    before: &[u8],
    after: &[u8],
    held: Option<Duration>,
) -> Result<(), LogError> {
    write!(output, "{}", escape_cell(header.as_bytes()).bold())?;
    write!(output, " ")?;
    write!(output, "{}", escape_cell(before).red())?;
    write!(output, " {} ", "->".dimmed())?;
    write!(output, "{}", escape_cell(after).green())?;
    if let Some(held) = held {
        write!(
            output,
            " {}",
            format!("held {}", format_duration(held)).dimmed()
        )?;
    }
    Ok(())
}

fn format_duration(duration: Duration) -> String {
    let mut output = String::new();
    write_duration(&mut output, duration);
    output
}

fn write_duration(output: &mut String, duration: Duration) {
    let nanos = duration.subsec_nanos();
    let total_secs = duration.as_secs();
    if total_secs == 0 {
        if nanos == 0 {
            output.push_str("0s");
        } else if nanos.is_multiple_of(1_000_000) {
            output.push_str(&format!("{}ms", nanos / 1_000_000));
        } else if nanos.is_multiple_of(1_000) {
            output.push_str(&format!("{}us", nanos / 1_000));
        } else {
            output.push_str(&format!("{nanos}ns"));
        }
        return;
    }
    if total_secs < 60 {
        if nanos == 0 {
            output.push_str(&format!("{total_secs}s"));
        } else {
            output.push_str(&format!("{total_secs}.{:03}s", nanos / 1_000_000));
        }
        return;
    }

    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let mins = (total_secs % 3_600) / 60;
    let secs = total_secs % 60;
    if days > 0 {
        output.push_str(&format!("{days}d"));
    }
    if days > 0 || hours > 0 {
        output.push_str(&format!("{hours}h"));
    }
    output.push_str(&format!("{mins}m{secs}s"));
}

fn duration_between(start: ParsedTime, end: ParsedTime) -> Option<Duration> {
    if start.kind != end.kind {
        return None;
    }
    let mut delta = absolute_secs(end)?.checked_sub(absolute_secs(start)?)?;
    let mut nanos = i64::from(end.nanos) - i64::from(start.nanos);
    if nanos < 0 {
        delta = delta.checked_sub(1)?;
        nanos += 1_000_000_000;
    }
    if delta < 0 || nanos < 0 {
        return None;
    }
    Some(Duration::new(delta as u64, nanos as u32))
}

fn absolute_secs(time: ParsedTime) -> Option<i64> {
    match time.kind {
        TimeKind::Clock => Some(i64::from(time.secs)),
        TimeKind::Date => days_from_civil(time.date)?
            .checked_mul(86_400)?
            .checked_add(i64::from(time.secs)),
    }
}

/// Howard Hinnant's `days_from_civil`, returned as days since 1970-01-01.
fn days_from_civil(date: u32) -> Option<i64> {
    let year = (date / 10_000) as i32;
    let month = (date / 100) % 100;
    let day = date % 100;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u32;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = yoe * 365 + yoe / 4 - yoe / 100 + day_of_year;
    Some(i64::from(era) * 146_097 + i64::from(day_of_era) - 719_468)
}

fn escape_cell(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str(r"\\"),
            '\n' => escaped.push_str(r"\n"),
            '\r' => escaped.push_str(r"\r"),
            '\t' => escaped.push_str(r"\t"),
            c if c.is_control() => {
                escaped.push_str(&format!("\\u{{{:04x}}}", u32::from(c)));
            }
            c => escaped.push(c),
        }
    }
    escaped
}

fn write_quoted<W: Write>(output: &mut W, bytes: &[u8]) -> Result<(), LogError> {
    output.write_all(b"\"")?;
    let text = String::from_utf8_lossy(bytes);
    for ch in text.chars() {
        match ch {
            '\\' => output.write_all(br"\\")?,
            '"' => output.write_all(br#"\""#)?,
            '\n' => output.write_all(br"\n")?,
            '\r' => output.write_all(br"\r")?,
            '\t' => output.write_all(br"\t")?,
            c if c.is_control() => {
                write!(output, "\\u{{{:04x}}}", u32::from(c))?;
            }
            c => {
                let mut buf = [0u8; 4];
                output.write_all(c.encode_utf8(&mut buf).as_bytes())?;
            }
        }
    }
    output.write_all(b"\"")?;
    Ok(())
}

fn strip_bom(bytes: &[u8]) -> &[u8] {
    bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use colored::Colorize;

    fn render(csv_text: &str, headers: &[&str]) -> Result<String, LogError> {
        let options = LogOptions {
            headers: Some(headers.iter().map(|name| (*name).to_string()).collect()),
            ..LogOptions::default()
        };
        render_options(csv_text, &options)
    }

    fn render_options(csv_text: &str, options: &LogOptions) -> Result<String, LogError> {
        let mut output = Vec::new();
        log_changes(csv_text.as_bytes(), options, &mut output)?;
        Ok(String::from_utf8(output).expect("log output is utf-8"))
    }

    fn plain(csv_text: &str, headers: &[&str]) -> String {
        strip_ansi(&render(csv_text, headers).unwrap())
    }

    fn strip_ansi(text: &str) -> String {
        let mut cleaned = String::with_capacity(text.len());
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' && chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                cleaned.push(ch);
            }
        }
        cleaned
    }

    #[test]
    fn logs_initial_values_and_later_changes() {
        let csv = "\
time,speed,temp
2024-01-01T00:00:00,10,21.5
2024-01-01T00:00:01,10,21.5
2024-01-01T00:00:02,12,22.0
";
        let text = plain(csv, &["temp", "speed"]);
        assert_eq!(
            text,
            "\
INIT temp = 21.5
INIT speed = 10
[L4, 2024-01-01T00:00:02]
\t* temp 21.5 -> 22.0 held 2s
\t* speed 10 -> 12 held 2s
"
        );
    }

    #[test]
    fn warns_when_a_requested_header_is_missing() {
        let text = plain("time,speed\nt0,1\n", &["speed", "missing", "speed"]);
        assert!(text.contains("WARN header \"missing\" does not exist in the CSV file\n"));
        assert!(
            text.contains("WARN header \"speed\" was provided more than once; watching it once\n")
        );
        assert!(text.contains("INIT speed = 1\n"));
        assert!(!text.contains("header \"missing\" value"));
    }

    #[test]
    fn colors_every_log_line() {
        colored::control::set_override(true);
        let raw = render("time,speed\nt0,1\nt1,2\n", &["speed", "nope"]).unwrap();
        assert!(raw.contains(&"WARN".bold().yellow().to_string()));
        assert!(raw.contains(&"INIT".bold().cyan().to_string()));
        assert!(raw.contains(&"1".green().to_string()));
        assert!(raw.contains(&"1".red().to_string()));
        assert!(raw.lines().all(|line| line.contains('\u{1b}')));
    }

    #[test]
    fn compares_decoded_fields_without_treating_quotes_as_data() {
        let csv = "\
time,note
t0,\"a,b\"
t1,\"a,b\"
t2,\"a,\"\"b\"\"\"
";
        let text = plain(csv, &["note"]);
        assert_eq!(
            text,
            "\
INIT note = a,b
[L4, t2] note a,b -> a,\"b\"
"
        );
    }

    #[test]
    fn keeps_multiline_values_on_one_log_line_and_reports_start_line() {
        let csv = "time,note\nt0,\"hello\nworld\"\nt1,\"hello\nworld\"\nt2,\"hello world\"\n";
        let text = plain(csv, &["note"]);
        assert!(text.contains("INIT note = hello\\nworld\n"));
        assert!(text.contains("[L6, t2] note hello\\nworld -> hello world\n"));
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn does_not_log_unwatched_columns_across_many_rows() {
        let mut csv = String::from("time,value,other\n");
        for i in 0..20_000 {
            csv.push_str(&format!("t{i},same,x{i}\n"));
        }
        csv.push_str("t-end,changed,x\n");

        let text = plain(&csv, &["value"]);
        assert_eq!(text.matches("INIT").count(), 1);
        assert_eq!(text.matches(" -> ").count(), 1);
        assert!(text.contains("[L20002, t-end] value same -> changed\n"));
    }

    #[test]
    fn whitespace_inside_a_cell_is_a_real_change() {
        let text = plain("time,value\nt0,ab\nt1,a b\n", &["value"]);
        assert!(text.contains("[L3, t1] value ab -> a b"));
    }

    #[test]
    fn trims_header_names_and_strips_bom() {
        let csv = "\u{feff}time, speed \nt0,1\nt1,2\n";
        let text = plain(csv, &[" speed "]);
        assert!(text.contains("INIT speed = 1"));
        assert!(text.contains("[L3, t1] speed 1 -> 2"));
        assert!(!text.contains("does not exist"));
    }

    #[test]
    fn warns_once_when_a_header_is_duplicated_in_the_file() {
        let text = plain("time,speed,speed\nt0,1,9\nt1,1,8\n", &["speed"]);
        assert!(text.contains(
            "WARN header \"speed\" occurs more than once in the CSV; watching the first column (2)\n"
        ));
        assert!(text.contains("INIT speed = 1\n"));
        assert!(!text.contains("->"));
    }

    #[test]
    fn treats_a_short_row_as_an_empty_value() {
        let text = plain("time,speed,temp\nt0,1,2\nt1,1\n", &["temp"]);
        assert!(text.contains("WARN row at line 3 has fewer columns"));
        assert!(text.contains("[L3, t1] temp 2 -> \n"));
    }

    #[test]
    fn rejects_empty_input_and_empty_header_list() {
        assert!(matches!(render("", &["speed"]), Err(LogError::EmptyCsv)));
        assert!(matches!(
            render("time,speed\n", &["", " "]),
            Err(LogError::NoHeaders)
        ));
    }

    #[test]
    fn no_init_skips_initial_values_but_logs_changes() {
        let options = LogOptions {
            headers: Some(vec!["speed".to_string()]),
            no_init: true,
            ..LogOptions::default()
        };
        let text = strip_ansi(&render_options("time,speed\nt0,1\nt1,2\n", &options).unwrap());
        assert_eq!(text, "[L3, t1] speed 1 -> 2\n");
    }

    #[test]
    fn omitted_headers_watch_every_data_column() {
        let text = strip_ansi(
            &render_options("time,speed,temp\nt0,1,a\nt1,1,b\n", &LogOptions::default()).unwrap(),
        );
        assert_eq!(
            text,
            "\
INIT speed = 1
INIT temp = a
[L3, t1] temp a -> b
"
        );
    }

    #[test]
    fn except_drops_columns_and_cannot_combine_with_headers() {
        let options = LogOptions {
            except: Some(vec!["temp".to_string(), "missing".to_string()]),
            ..LogOptions::default()
        };
        let text =
            strip_ansi(&render_options("time,speed,temp\nt0,1,a\nt1,2,b\n", &options).unwrap());
        assert!(text.contains("WARN header \"missing\" does not exist"));
        assert_eq!(
            text.lines()
                .filter(|line| !line.starts_with("WARN"))
                .collect::<Vec<_>>(),
            vec!["INIT speed = 1", "[L3, t1] speed 1 -> 2"]
        );

        let conflict = LogOptions {
            headers: Some(vec!["speed".to_string()]),
            except: Some(vec!["temp".to_string()]),
            ..LogOptions::default()
        };
        assert!(matches!(
            render_options("time,speed\nt0,1\n", &conflict),
            Err(LogError::ConflictingSelection)
        ));
    }

    #[test]
    fn incident_window_logs_only_inside_rows() {
        let csv = "\
time,speed,temp
2024-01-01T00:00:00,10,21
2024-01-01T00:00:01,10,21
2024-01-01T00:00:02,12,22
2024-01-01T00:00:03,12,23
2024-01-01T00:00:04,15,23
";
        let options = LogOptions {
            headers: Some(vec!["speed".to_string(), "temp".to_string()]),
            from: Some("2024-01-01T00:00:02".to_string()),
            to: Some("2024-01-01T00:00:03".to_string()),
            ..LogOptions::default()
        };
        let text = strip_ansi(&render_options(csv, &options).unwrap());
        assert_eq!(
            text,
            "\
INIT speed = 10
INIT temp = 21
[L4, 2024-01-01T00:00:02]
\t* speed 10 -> 12 held 2s
\t* temp 21 -> 22 held 2s
[L5, 2024-01-01T00:00:03] temp 22 -> 23 held 1s
"
        );
        assert!(text.contains("INIT speed = 10"));
        assert!(!text.contains("2024-01-01T00:00:04"));
    }

    #[test]
    fn window_orders_unpadded_clock_times() {
        use std::cmp::Ordering::{Equal, Greater, Less};

        assert_eq!(cmp_time(b"9:59:59", b"10:00:00"), Less);
        assert_eq!(cmp_time(b"09:59:59", b"9:59:59"), Equal);
        assert_eq!(cmp_time(b"10:00:00.1", b"10:00:00.09"), Greater);
        assert_eq!(
            cmp_time(b"2024-01-01T09:59:59", b"2024-01-01T10:00:00"),
            Less
        );

        let csv = "\
time,speed
9:59:58,1
9:59:59,1
10:00:00,2
10:00:01,3
10:00:02,3
";
        let options = LogOptions {
            headers: Some(vec!["speed".to_string()]),
            from: Some("9:59:59".to_string()),
            to: Some("10:00:01".to_string()),
            no_init: true,
            ..LogOptions::default()
        };
        let text = strip_ansi(&render_options(csv, &options).unwrap());
        assert_eq!(
            text,
            "\
[L4, 10:00:00] speed 1 -> 2 held 2s
[L5, 10:00:01] speed 2 -> 3 held 1s
"
        );
    }

    #[test]
    fn hold_time_is_per_column_and_omitted_when_unparsed() {
        let csv = "\
time,speed,temp
10:00:00.100,1,a
10:00:00.350,1,b
10:00:03,2,b
";
        let text = plain(csv, &["speed", "temp"]);
        assert!(text.contains("[L3, 10:00:00.350] temp a -> b held 250ms"));
        assert!(text.contains("[L4, 10:00:03] speed 1 -> 2 held 2.900s"));
        assert!(!text.contains("temp b ->"));

        let dated = plain(
            "time,speed\n2024-01-01T00:00:00,1\n2024-01-02T01:02:03,2\n",
            &["speed"],
        );
        assert!(dated.contains("[L3, 2024-01-02T01:02:03] speed 1 -> 2 held 1d1h2m3s"));

        let unparsed = plain("time,speed\nt0,1\nt1,2\n", &["speed"]);
        assert!(unparsed.contains("[L3, t1] speed 1 -> 2\n"));
        assert!(!unparsed.contains("held"));
    }
}
