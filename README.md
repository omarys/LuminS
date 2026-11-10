<h1 align="center">LuminS</h1>
<h3 align="center">Luminous Synchronize</h3>
<h4 align="center">A fast and reliable alternative to rsync for synchronizing local files</h4>

<p align="center">
  <a href="https://github.com/omarys/LuminS/actions/workflows/CI.yml"><img src="https://github.com/omarys/LuminS/actions/workflows/CI.yml/badge.svg?branch=master" alt="Build Status" /></a>
  <a href="https://codecov.io/gh/omarys/LuminS"><img src="https://codecov.io/gh/omarys/LuminS/branch/master/graph/badge.svg" alt="Code Coverage" /></a>
  <a href="https://crates.io/crates/lms"><img src="https://img.shields.io/crates/v/lms.svg" alt="Crate" /></a>
  <a href="https://docs.rs/lms"><img src="https://docs.rs/lms/badge.svg" alt="Docs" /></a>
</p>
<p align="center">
  <img src="examples/lumins.gif" alt="Demo"
</p>

<h3 align="center"><b>Note</b>: This project is maintained on a best-effort basis. Issues and pull requests are welcome.</h3>

## Features

<table>
    <tr><td><b>100% Rust</b></td></tr>
    <tr><td><b>Powered by the <a href="https://github.com/rayon-rs/rayon">Rayon</a> library for high parallel perfomance</b></td></tr>
    <tr><td><b>Supported on Unix-based platforms or Windows</b></td></tr>
    <tr><td><b>Extremely fast at synchronizing directories with large quantities of files</b></td></tr>
    <tr><td><b>Multithreaded copy, remove, and sync</b></td></tr>
    <tr><td><b>A progress bar using <a href="https://github.com/mitsuhiko/indicatif">indicatif</a></b></td></tr>
</table>


## Usage

> **`lms sync` deletes destination files that are not in the source.** Pass
> `-n/--nodelete` to keep them. The exit status is `0` on success and `1` if any
> copy, delete, or argument was rejected, so a backup script can branch on it.

> **Running this from an agent?** Add `--llm` and you get one line of JSON
> instead of a progress bar. See [Agent-friendly reporting](#agent-friendly-reporting).

```bash
LuminS 0.4.0
Luminous Synchronize - A fast and reliable multithreaded alternative to rsync for synchronizing local files

Usage: lms [COMMAND]

Commands:
  cp    Multithreaded directory copy
  rm    Multithreaded directory remove
  sync  Multithreaded directory synchronization [alias: s]
  help  Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```
#### Sync

```bash
Multithreaded directory synchronization

Usage: lms sync [OPTIONS] <SOURCE> <DESTINATION>

Arguments:
  <SOURCE>       Source directory
  <DESTINATION>  Destination directory

Options:
  -n, --nodelete              Do not delete any destination files
  -s, --secure                Use a cryptographic hash function for hashing similar files
  -v, --verbose               Verbose outputs
  -S, --sequential            Copy files sequentially instead of in parallel
      --json                  Emit one compact JSON report on stdout and suppress the progress bar
      --llm                   Agent-first reporting: compact JSON, no progress bar, and at most --max-report-files changes listed (default 50) [alias: --machine-readable]
      --max-report-files <N>  List at most N changes in a report, summarising the rest per directory (default 50 under --llm, unlimited otherwise)
      --ignore-vcs-and-build  Skip VCS and build artifacts: .git/, target/, node_modules/, dist/, .cache/, *.pyc
  -N, --dry-run               Report what would change without touching the destination [alias: --dryrun]
  -h, --help                  Print help
  -V, --version               Print version
```

#### Copy

```bash
Multithreaded directory copy

Usage: lms cp [OPTIONS] <SOURCE> <DESTINATION>

Arguments:
  <SOURCE>       Source directory
  <DESTINATION>  Destination directory

Options:
  -v, --verbose               Verbose outputs
  -S, --sequential            Copy files sequentially instead of in parallel
      --json                  Emit one compact JSON report on stdout and suppress the progress bar
      --llm                   Agent-first reporting: compact JSON, no progress bar, and at most --max-report-files changes listed (default 50) [alias: --machine-readable]
      --max-report-files <N>  List at most N changes in a report, summarising the rest per directory (default 50 under --llm, unlimited otherwise)
      --ignore-vcs-and-build  Skip VCS and build artifacts: .git/, target/, node_modules/, dist/, .cache/, *.pyc
  -N, --dry-run               Report what would change without touching the destination [alias: --dryrun]
  -h, --help                  Print help
  -V, --version               Print version
```

#### Remove

```bash
Multithreaded directory remove

Usage: lms rm [OPTIONS] <TARGET>...

Arguments:
  <TARGET>...  Target directory

Options:
  -v, --verbose               Verbose outputs
  -S, --sequential            Delete files sequentially instead of in parallel
      --json                  Emit one compact JSON report on stdout and suppress the progress bar
      --llm                   Agent-first reporting: compact JSON, no progress bar, and at most --max-report-files changes listed (default 50) [alias: --machine-readable]
      --max-report-files <N>  List at most N changes in a report, summarising the rest per directory (default 50 under --llm, unlimited otherwise)
      --ignore-vcs-and-build  Skip VCS and build artifacts: .git/, target/, node_modules/, dist/, .cache/, *.pyc
  -N, --dry-run               Report what would change without touching the destination [alias: --dryrun]
  -h, --help                  Print help
  -V, --version               Print version
```

## Agent-friendly reporting

A progress bar and a `Copying file ...` line per entry are the right output for a
human watching a sync, and the wrong output for a program reading one: every
repaint and every unchanged path spends context on nothing.

`--llm` (alias `--machine-readable`) and `--json` replace that output with a
single line of JSON on stdout. Both run the same engine; `--llm` only adds a
default `--max-report-files 50`. The progress bar is disabled whenever one of
these flags is passed, or when stdout is not a terminal, so no carriage return or
half-drawn line ever reaches a pipe.

```console
$ lms sync src/ dest/ --llm --dry-run
{"status":"dry_run","changes":{"added":["sub","sub/fresh.txt"],"modified":[{"path":"changed.txt","delta_bytes":6,"hunks":1}],"deleted":["gone.txt"]},"stats":{"scanned":4,"transferred_bytes":0,"duration_ms":2}}
```

| Field | Meaning |
| ----- | ------- |
| `status` | `dry_run` when nothing was written, `synced` when the plan was applied |
| `changes.added` | Paths in the source that the destination did not have |
| `changes.modified[].delta_bytes` | Source size minus destination size; negative when the file shrank, `0` when only the contents changed |
| `changes.modified[].hunks` | Number of runs of differing 8 KiB windows - how scattered the change is |
| `changes.deleted` | Paths the destination had and the source did not |
| `stats.scanned` | Entries the source scan examined |
| `stats.transferred_bytes` | Bytes written to the destination |
| `stats.duration_ms` | Wall-clock time of the command |

Classification is by path: a file that grew is one `modified` entry, never a
`deleted` plus an `added` one.

### Truncation

A sync of a build tree changes thousands of files, and listing them all is what
blows up a caller's context - which directory they live in is the whole signal.
`--max-report-files N` keeps the first `N` changes (sorted, so the cut is stable)
and summarises the rest per directory:

```console
$ lms sync src/ dest/ --llm --max-report-files 2
{"status":"synced","changes":{"added":["assets","assets/asset_0.png"],"modified":[],"deleted":[]},"stats":{"scanned":10,"transferred_bytes":68,"duration_ms":2},"truncated":true,"total_overflow":8,"overflow_by_dir":{"./":1,"assets/":1,"gen/":6}}
```

The truncation keys are absent unless something was dropped, so a caller never
has to read a `"truncated": false` branch it does not need.

### Skipping build and VCS noise

`--ignore-vcs-and-build` skips `.git/`, `target/`, `node_modules/`, `dist/`,
`.cache/` and `*.pyc`. Only entries inside the walked tree are skipped: pointing
`lms` at `.git` or `target` still syncs it.

```console
$ lms sync src/ dest/ --llm --ignore-vcs-and-build
```


## Benchmarks

Measured with [`tools/bench.sh`](tools/bench.sh), which regenerates the fixtures
and times `lms` against `rsync`, `cp -r` and `rm -rf` on the same trees. Numbers
are machine-dependent; run `mise run bench` to get yours.

Linux, 24 vCPU, both trees on tmpfs, so these are warm-cache CPU and syscall
costs rather than disk throughput. Medians of 15 runs, destination reset before
each run:

| Directory | Size | Files |
| --------- | ---- | ----- |
| 1 | 89.8 MiB | 7262 small files |
| 2 | 103.6 MiB | 252 large files |

| Command | Directory 1 | Directory 2 |
| ------- | ----------- | ----------- |
| **`lms sync`** (into an absent destination) | **59.7 ms** | **11.5 ms** |
| `rsync -r --delete` | 167.4 ms | 104.1 ms |
| **`lms cp`** | **59.3 ms** | **11.1 ms** |
| `cp -r` | 81.9 ms | 33.6 ms |
| **`lms rm`** | **31.3 ms** | **5.1 ms** |
| `rm -rf` | 32.8 ms | 8.2 ms |
| **`lms sync`** (over an identical tree) | **32.4 ms** | **10.3 ms** |
| `rsync -r --delete` | 70.9 ms | 45.8 ms |

The gap opens on trees with a large number of small files, where per-file
overhead dominates: `rsync` is 2.8x slower on directory 1 and 9x slower on
directory 2. Against `cp -r` the win is smaller (1.4x and 3x) because both are
doing the same syscalls. `lms rm` is barely ahead of `rm -rf` on many small
files and 1.6x ahead on few large ones.

Re-syncing an already identical tree is the interesting row: nothing is written,
so it measures the scan-and-compare path alone, and it is why `lms sync` is
cheap to run on a schedule.

## Build

First [install](https://www.rust-lang.org/tools/install) Rust (recommended using rustup). The minimum supported Rust version is 1.85.

```zsh
$ git clone https://github.com/omarys/LuminS.git
$ cd LuminS
$ cargo build --release
```

Or with [mise](https://mise.jdx.dev) for a pinned toolchain and the same tasks CI
runs: `mise run check` (format, clippy, build, test), `mise run msrv`, `mise run
bench`.

## Install

Using cargo,

```bash
$ cargo install lms
```

This installs the release binary into `~/.cargo/bin`, or `${CARGO_HOME}/bin` when
`CARGO_HOME` is set. In a checkout, `mise run install` does the same from source.

## Contributions

Suggestions, issues, and pull requests are welcome!

## License

MIT, see [LICENSE](LICENSE).

