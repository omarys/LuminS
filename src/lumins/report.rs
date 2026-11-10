//! The machine-readable run report behind `--json` / `--llm`.
//!
//! A terminal progress bar and a line of `Copying file ...` per entry are the
//! right output for a human watching a sync; they are the wrong output for a
//! program reading it, because every repaint and every unchanged path spends
//! context on nothing. This module turns the same run into one compact JSON
//! document describing only what changed.
//!
//! The report is built from sets LuminS already has in hand, so it costs no
//! extra traversal. Only the hunk count of a modified file needs to read bytes
//! that the copy path did not already read, and that happens only when a
//! machine-readable mode asked for the report.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::Path;

/// Size of one comparison window, in bytes
///
/// A hunk is a run of consecutive windows that differ, so this is the
/// granularity of the reported count: it is a cheap, honest "how scattered is
/// this change", not a line-accurate diff.
const BLOCK: usize = 8 * 1024;

/// How far the destination got
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Nothing was written; `--dry-run` only computed the plan
    DryRun,
    /// The destination was brought in line with the source
    Synced,
}

impl Status {
    /// The wire name of this status
    pub fn as_str(self) -> &'static str {
        match self {
            Status::DryRun => "dry_run",
            Status::Synced => "synced",
        }
    }
}

/// A file whose contents the run had to rewrite
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Modified {
    /// Path relative to the source root
    pub path: String,
    /// Size of the source minus the size of the destination, in bytes
    ///
    /// Negative when the file shrank. Equal sizes with different contents
    /// report `0`; `hunks` is what says the file changed.
    pub delta_bytes: i64,
    /// Number of runs of differing 8 KiB windows
    pub hunks: u32,
}

/// Everything the run added, rewrote, or removed
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Changes {
    /// Paths that exist in the source and not in the destination
    pub added: Vec<String>,
    /// Paths that exist in both and were rewritten
    pub modified: Vec<Modified>,
    /// Paths that exist in the destination and not in the source
    pub deleted: Vec<String>,
}

impl Changes {
    /// Total number of reported changes, across all three lists
    pub fn len(&self) -> usize {
        self.added.len() + self.modified.len() + self.deleted.len()
    }

    /// Whether the run changed nothing
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Sorts every list by path
    ///
    /// The sets the report is built from are unordered, so this is what makes
    /// two runs over the same tree produce the same bytes - and what makes
    /// truncation cut a stable, predictable tail instead of a random one.
    pub fn sort(&mut self) {
        self.added.sort();
        self.deleted.sort();
        self.modified.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

/// Counters for the run as a whole
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Entries examined in the source tree
    pub scanned: usize,
    /// Bytes written to the destination
    pub transferred_bytes: u64,
    /// Wall-clock time the command took
    pub duration_ms: u128,
}

/// One run, summarised
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// Whether the run applied its plan
    pub status: Status,
    /// What changed
    pub changes: Changes,
    /// Totals
    pub stats: Stats,
    /// Whether `changes` was cut down to fit `--max-report-files`
    pub truncated: bool,
    /// Number of changes dropped by that cut
    pub total_overflow: usize,
    /// Dropped changes, counted by the directory that holds them
    pub overflow_by_dir: BTreeMap<String, usize>,
}

impl Report {
    /// An empty, successful report
    pub fn new(status: Status) -> Self {
        Report {
            status,
            changes: Changes::default(),
            stats: Stats::default(),
            truncated: false,
            total_overflow: 0,
            overflow_by_dir: BTreeMap::new(),
        }
    }

    /// Keeps at most `max` changes, summarising the rest per directory
    ///
    /// A sync of a build tree changes thousands of files. Listing them all is
    /// what blows up a caller's context, and none of them are interesting one
    /// by one - which directory they live in is the whole signal.
    ///
    /// The lists are consumed in order (added, modified, deleted) after
    /// `Changes::sort`, so the cut is deterministic.
    pub fn truncate(&mut self, max: usize) {
        if self.changes.len() <= max {
            return;
        }

        let mut budget = max;
        let mut overflow: Vec<String> = Vec::new();

        let added = std::mem::take(&mut self.changes.added);
        let kept = added.len().min(budget);
        budget -= kept;
        overflow.extend(added[kept..].iter().cloned());
        self.changes.added = added[..kept].to_vec();

        let modified = std::mem::take(&mut self.changes.modified);
        let kept = modified.len().min(budget);
        budget -= kept;
        overflow.extend(modified[kept..].iter().map(|m| m.path.clone()));
        self.changes.modified = modified[..kept].to_vec();

        let deleted = std::mem::take(&mut self.changes.deleted);
        let kept = deleted.len().min(budget);
        overflow.extend(deleted[kept..].iter().cloned());
        self.changes.deleted = deleted[..kept].to_vec();

        self.truncated = true;
        self.total_overflow = overflow.len();
        for path in overflow {
            *self.overflow_by_dir.entry(directory(&path)).or_insert(0) += 1;
        }
    }

    /// Renders the report as one compact JSON object
    ///
    /// Compact on purpose: whitespace is tokens, and nothing reads this by eye.
    pub fn to_json(&self) -> String {
        let mut out = String::with_capacity(128 + self.changes.len() * 24);

        let _ = write!(
            out,
            "{{\"status\":\"{}\",\"changes\":{{\"added\":[",
            self.status.as_str()
        );
        for (i, path) in self.changes.added.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(&mut out, path);
        }

        out.push_str("],\"modified\":[");
        for (i, modified) in self.changes.modified.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str("{\"path\":");
            write_json_string(&mut out, &modified.path);
            let _ = write!(
                out,
                ",\"delta_bytes\":{},\"hunks\":{}}}",
                modified.delta_bytes, modified.hunks
            );
        }

        out.push_str("],\"deleted\":[");
        for (i, path) in self.changes.deleted.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            write_json_string(&mut out, path);
        }

        let _ = write!(
            out,
            "]}},\"stats\":{{\"scanned\":{},\"transferred_bytes\":{},\"duration_ms\":{}}}",
            self.stats.scanned, self.stats.transferred_bytes, self.stats.duration_ms
        );

        if self.truncated {
            let _ = write!(
                out,
                ",\"truncated\":true,\"total_overflow\":{},\"overflow_by_dir\":{{",
                self.total_overflow
            );
            for (i, (dir, count)) in self.overflow_by_dir.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_json_string(&mut out, dir);
                let _ = write!(out, ":{count}");
            }
            out.push('}');
        }

        out.push('}');
        out
    }
}

/// The directory bucket a reported path belongs to
///
/// Trailing slash included, so a directory and a file of the same name cannot
/// collapse into one bucket. A top-level path reports `./`.
fn directory(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => {
            format!("{}/", parent.to_string_lossy())
        }
        _ => "./".to_string(),
    }
}

/// Writes `value` as a JSON string literal, escaping what JSON requires
fn write_json_string(out: &mut String, value: &str) {
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// How much a source file differs from its destination
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Diff {
    /// Source size minus destination size, in bytes
    pub delta_bytes: i64,
    /// Number of runs of differing `BLOCK`-sized windows
    pub hunks: u32,
}

impl Diff {
    /// Whether the two files hold different bytes
    pub fn differs(self) -> bool {
        self.hunks > 0
    }
}

/// Compares the files at `src` and `dest` window by window
///
/// Reads both files once, in lockstep, so memory stays flat regardless of file
/// size. A file that only exists on one side is reported as entirely different.
///
/// # Errors
/// Returns the failure when either file cannot be opened or read.
pub fn diff(src: &Path, dest: &Path) -> io::Result<Diff> {
    let src_len = fs::metadata(src).map_or(0, |meta| meta.len());
    let dest_len = fs::metadata(dest).map_or(0, |meta| meta.len());
    let delta_bytes = src_len as i64 - dest_len as i64;

    let mut src_file = fs::File::open(src)?;
    let mut dest_file = match fs::File::open(dest) {
        Ok(file) => file,
        // A destination that vanished is a whole-file change, not a failure.
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            return Ok(Diff {
                delta_bytes,
                hunks: 1,
            })
        }
        Err(e) => return Err(e),
    };

    let mut src_buf = vec![0u8; BLOCK];
    let mut dest_buf = vec![0u8; BLOCK];
    let mut hunks = 0;
    let mut inside_hunk = false;

    loop {
        let src_read = read_window(&mut src_file, &mut src_buf)?;
        let dest_read = read_window(&mut dest_file, &mut dest_buf)?;
        if src_read == 0 && dest_read == 0 {
            break;
        }

        let same = src_read == dest_read && src_buf[..src_read] == dest_buf[..dest_read];
        if same {
            inside_hunk = false;
        } else if !inside_hunk {
            hunks += 1;
            inside_hunk = true;
        }

        // A file that ended is a change until the other one ends too.
        if src_read == 0 || dest_read == 0 {
            break;
        }
    }

    Ok(Diff { delta_bytes, hunks })
}

/// Fills `buf` as far as the file allows and returns the number of bytes read
///
/// `Read::read` is free to return a short read, which would misalign the two
/// windows being compared and invent hunks. Reading until the buffer is full
/// keeps the windows in step.
fn read_window(file: &mut fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(read) => filled += read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test_report {
    use std::collections::BTreeMap;

    use super::*;

    /// A scratch directory under the system temp dir, removed when dropped
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join("lms_report_tests").join(name);
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn write(&self, name: &str, contents: &[u8]) -> std::path::PathBuf {
            let path = self.0.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, contents).unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn report_with(added: &[&str], deleted: &[&str]) -> Report {
        let mut report = Report::new(Status::Synced);
        report.changes.added = added.iter().map(|s| (*s).to_string()).collect();
        report.changes.deleted = deleted.iter().map(|s| (*s).to_string()).collect();
        report.changes.sort();
        report
    }

    #[test]
    fn json_reports_an_empty_run() {
        let report = Report::new(Status::DryRun);

        assert_eq!(
            report.to_json(),
            "{\"status\":\"dry_run\",\"changes\":{\"added\":[],\"modified\":[],\"deleted\":[]},\
             \"stats\":{\"scanned\":0,\"transferred_bytes\":0,\"duration_ms\":0}}"
        );
    }

    #[test]
    fn json_carries_the_full_schema() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = vec!["path/to/new_file.rs".to_string()];
        report.changes.modified = vec![Modified {
            path: "src/main.rs".to_string(),
            delta_bytes: 412,
            hunks: 2,
        }];
        report.stats.scanned = 142;
        report.stats.transferred_bytes = 1024;
        report.stats.duration_ms = 48;

        assert_eq!(
            report.to_json(),
            "{\"status\":\"synced\",\"changes\":{\"added\":[\"path/to/new_file.rs\"],\
             \"modified\":[{\"path\":\"src/main.rs\",\"delta_bytes\":412,\"hunks\":2}],\
             \"deleted\":[]},\"stats\":{\"scanned\":142,\"transferred_bytes\":1024,\
             \"duration_ms\":48}}"
        );
    }

    #[test]
    fn a_negative_delta_survives_serialisation() {
        let mut report = Report::new(Status::Synced);
        report.changes.modified = vec![Modified {
            path: "a.rs".to_string(),
            delta_bytes: -17,
            hunks: 1,
        }];

        assert!(report.to_json().contains("\"delta_bytes\":-17"));
    }

    #[test]
    fn paths_are_escaped() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = vec!["a\"b\\c\nd\u{1}.rs".to_string()];

        assert!(report.to_json().contains(r#""a\"b\\c\nd\u0001.rs""#));
    }

    #[test]
    fn a_report_under_the_threshold_is_untouched() {
        let mut report = report_with(&["a", "b"], &["c"]);
        report.truncate(3);

        assert!(!report.truncated);
        assert_eq!(report.total_overflow, 0);
        assert!(report.overflow_by_dir.is_empty());
        assert_eq!(report.changes.len(), 3);
        assert!(!report.to_json().contains("truncated"));
    }

    #[test]
    fn truncation_keeps_the_first_entries_and_buckets_the_rest() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = vec![
            "target/debug/build/a".to_string(),
            "target/debug/build/b".to_string(),
            "target/debug/build/c".to_string(),
            "assets/logo.png".to_string(),
            "top.rs".to_string(),
        ];
        report.changes.sort();

        report.truncate(2);

        assert!(report.truncated);
        assert_eq!(report.changes.len(), 2);
        assert_eq!(
            report.changes.added,
            vec!["assets/logo.png", "target/debug/build/a"]
        );
        assert_eq!(report.total_overflow, 3);
        assert_eq!(
            report.overflow_by_dir,
            BTreeMap::from([
                ("target/debug/build/".to_string(), 2),
                ("./".to_string(), 1),
            ])
        );
    }

    #[test]
    fn truncation_consumes_the_budget_across_all_three_lists() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = vec!["a".to_string(), "b".to_string()];
        report.changes.modified = vec![Modified {
            path: "c".to_string(),
            delta_bytes: 1,
            hunks: 1,
        }];
        report.changes.deleted = vec!["d".to_string()];
        report.changes.sort();

        report.truncate(3);

        assert_eq!(report.changes.added, vec!["a", "b"]);
        assert_eq!(report.changes.modified.len(), 1);
        assert!(report.changes.deleted.is_empty());
        assert_eq!(report.total_overflow, 1);
        assert_eq!(report.overflow_by_dir["./"], 1);
    }

    #[test]
    fn an_identical_file_has_no_hunks() {
        let scratch = Scratch::new("an_identical_file_has_no_hunks");
        let src = scratch.write("src.rs", b"fn main() {}\n");
        let dest = scratch.write("dest.rs", b"fn main() {}\n");

        let diff = diff(&src, &dest).unwrap();

        assert!(!diff.differs());
        assert_eq!(diff.hunks, 0);
        assert_eq!(diff.delta_bytes, 0);
    }

    #[test]
    fn separate_changes_are_separate_hunks() {
        let scratch = Scratch::new("separate_changes_are_separate_hunks");
        let src = scratch.write("src.rs", &[b'a'; BLOCK * 3]);
        let mut changed = vec![b'a'; BLOCK * 3];
        changed[0] = b'b';
        changed[BLOCK * 2] = b'b';
        let dest = scratch.write("dest.rs", &changed);

        assert_eq!(diff(&src, &dest).unwrap().hunks, 2);
    }

    #[test]
    fn adjacent_changes_are_one_hunk() {
        let scratch = Scratch::new("adjacent_changes_are_one_hunk");
        let src = scratch.write("src.rs", &[b'a'; BLOCK * 2]);
        let mut changed = vec![b'a'; BLOCK * 2];
        changed[0] = b'b';
        changed[BLOCK] = b'b';
        let dest = scratch.write("dest.rs", &changed);

        assert_eq!(diff(&src, &dest).unwrap().hunks, 1);
    }

    #[test]
    fn a_longer_source_reports_its_size_delta() {
        let scratch = Scratch::new("a_longer_source_reports_its_size_delta");
        let src = scratch.write("src.rs", &[b'a'; 100]);
        let dest = scratch.write("dest.rs", &[b'a'; 58]);

        assert_eq!(diff(&src, &dest).unwrap().delta_bytes, 42);
    }

    #[test]
    fn an_unreadable_destination_is_a_whole_file_change() {
        let scratch = Scratch::new("an_unreadable_destination_is_a_whole_file_change");
        let src = scratch.write("src.rs", b"contents");
        let dest = scratch.0.join("missing.rs");

        let diff = diff(&src, &dest).unwrap();

        assert!(diff.differs());
        assert_eq!(diff.delta_bytes, 8);
    }
}
