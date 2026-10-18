# csv-row-diff

`csv-row-diff` streams a CSV file and prints when watched column values change. It reads one record at a time, so it can scan files that are too large to load into memory.

The first row is the header row. Data starts on the next record. The first column is the row time and is printed with each change; it is not itself a watched column.

```csv
time,speed,state,comment
9:59:58,10,ON,idle
9:59:59,10,ON,idle
10:00:00,12,ON,moving
10:00:01,12,OFF,stopped
```

## Install

From this directory:

```powershell
cargo install --path .
```

That installs `csv-row-diff` into `%USERPROFILE%\.cargo\bin`. Cargo adds that directory to your user `PATH`, so both **cmd** and **PowerShell** can run:

```text
csv-row-diff data.csv speed,state
```

To try it without installing:

```powershell
cargo run --release -- data.csv speed,state
```

### Also call it `crd`

Create `%USERPROFILE%\.cargo\bin\crd.cmd`:

```bat
@echo off
"%~dp0csv-row-diff.exe" %*
```

## Usage

```text
csv-row-diff <CSV_FILE> [HEADERS]
csv-row-diff --file <CSV_FILE> [--headers <HEADERS> | --except <HEADERS>]
```

`CSV_FILE` may be `-` to read from stdin.

| Flag | Meaning |
| --- | --- |
| `-f`, `--file <CSV_FILE>` | Input path |
| `-H`, `--headers <HEADERS>` | Watch only these columns |
| `-x`, `--except <HEADERS>` | Watch every data column except these |
| `-n`, `--no-init` | Do not print the starting values |
| `--from <TIME>` | Inclusive start of the incident window |
| `--to <TIME>` | Inclusive end of the incident window |
| `-h`, `--help` | Help |
| `-V`, `--version` | Version |

Header lists are comma-separated. Matching is case-sensitive and ignores surrounding spaces. A name that is not in the file is a warning; scanning continues. If the same header appears twice, the first column is watched.

`--headers` and `--except` cannot be used together. With neither flag, every column except the time column is watched.

```powershell
csv-row-diff data.csv
csv-row-diff data.csv speed,state
csv-row-diff data.csv --except comment --from 10:00:00 --to 10:00:01
csv-row-diff -f data.csv -H speed --no-init
Get-Content data.csv | csv-row-diff -
```

## Output

Initial values, one line per watched column:

```text
INIT speed = 10
INIT state = ON
```

One change stays on one line. The line number is the data-record number, starting at 1 for the first row after the header:

```text
[L3, 10:00:00] speed 10 -> 12 held 2s
```

Several changes on the same row are listed underneath the time:

```text
[L4, 10:00:01]
	* speed 12 -> 15 held 1s
	* state ON -> OFF held 3s
```

`held` is how long the previous value lasted, measured from the row where that column last changed (or from the first data row) to the current row. It is printed only when both times can be parsed and the duration is not negative. Examples: `held 0s`, `held 250ms`, `held 2s`, `held 2.900s`, `held 1m2s`, `held 1h0m2s`, `held 1d1h2m3s`.

Quoted fields, embedded commas, and embedded newlines are read as normal CSV. Control characters in a logged cell are escaped so one record stays readable.

## Incident window

`--from` and `--to` are inclusive. Either one may be omitted.

Rows before the window still update the baseline, but they are not logged. Unless `--no-init` is set, the first in-window row prints `INIT` for the values held at that point. Later changes inside the window are logged as usual.

Clock times such as `9:59:59` and `10:00:00` are compared by time of day, not as text, so `9:59:59` comes before `10:00:00`. A leading zero on the hour is optional. Dated timestamps such as `2024-01-01T09:59:59` or `2024-01-01 09:59:59` are compared by date and time. A trailing time zone is ignored, not applied. Values that do not match either form are compared as raw bytes.

A window does not wrap past midnight. `--from 21:00:00 --to 04:00:00` matches nothing, because no clock time is both at or after 21:00 and at or before 04:00.

The file is assumed to be in non-decreasing time order. After a row passes `--to`, reading stops. A backward step is a warning and disables that early stop. A backward step, including a clock time that crosses midnight, is not turned into a hold duration.

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | Success, help, or version |
| 1 | Could not open the file, or the CSV could not be read |
| 2 | Invalid arguments |

## Limits

- Cells are compared as bytes. `10` and `10.0` are different values, and `on` and `ON` are different values.
- A short row is treated as empty in the missing cells. That warning is printed once.
- Hold time is omitted when a time cannot be parsed, when the two times are not the same kind (clock versus dated), or when the duration would be negative.
