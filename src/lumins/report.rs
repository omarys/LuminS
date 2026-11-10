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
///
/// A symlink whose target changed lands here too, with no byte delta and one
/// hunk. That is rsync's model as well: it itemizes a retargeted link as `cLc`,
/// a single changed entry, never a deletion plus a creation.
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
    ///
    /// A symlink has no windows, so its single target counts as one.
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

/// What one category of changes lost to the size limit
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overflow {
    /// How many changes of this kind were dropped
    pub count: usize,
    /// Those changes, counted by the directory that holds them
    pub by_dir: BTreeMap<String, usize>,
}

impl Overflow {
    /// Drops `path` into this category's summary
    fn add(&mut self, path: &str) {
        self.count += 1;
        *self.by_dir.entry(directory(path)).or_insert(0) += 1;
    }
}

/// What each category lost to the size limit
///
/// Kept per category rather than merged: a collapsed directory holding new
/// assets and a collapsed directory holding deletions call for opposite
/// reactions, and one merged bucket cannot tell them apart.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Overflowed {
    /// Dropped additions
    pub added: Overflow,
    /// Dropped modifications
    pub modified: Overflow,
    /// Dropped deletions
    pub deleted: Overflow,
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
    /// Dropped changes, counted by category and directory
    pub overflow: Overflowed,
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
            overflow: Overflowed::default(),
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

        let (keep_added, keep_modified, keep_deleted) = partition_budget(
            max,
            self.changes.added.len(),
            self.changes.modified.len(),
            self.changes.deleted.len(),
        );

        let mut added = std::mem::take(&mut self.changes.added);
        for path in added.split_off(keep_added) {
            self.overflow.added.add(&path);
        }
        self.changes.added = added;

        let mut modified = std::mem::take(&mut self.changes.modified);
        for change in modified.split_off(keep_modified) {
            self.overflow.modified.add(&change.path);
        }
        self.changes.modified = modified;

        let mut deleted = std::mem::take(&mut self.changes.deleted);
        for path in deleted.split_off(keep_deleted) {
            self.overflow.deleted.add(&path);
        }
        self.changes.deleted = deleted;

        self.truncated = true;
        self.total_overflow =
            self.overflow.added.count + self.overflow.modified.count + self.overflow.deleted.count;
    }

    /// Renders a failed run as one compact JSON object
    ///
    /// A caller reading stdout cannot tell a run that changed nothing from one
    /// that died before reporting, so the failure is stated where the report
    /// would have been. Two keys, on purpose: stderr carries the detail.
    pub fn error_json(message: &str) -> String {
        let mut out = String::from("{\"status\":\"error\",\"error\":");
        write_json_string(&mut out, message);
        out.push('}');
        out
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
                ",\"truncated\":true,\"total_overflow\":{},\"overflow\":{{\"added\":",
                self.total_overflow
            );
            write_overflow(&mut out, &self.overflow.added);
            out.push_str(",\"modified\":");
            write_overflow(&mut out, &self.overflow.modified);
            out.push_str(",\"deleted\":");
            write_overflow(&mut out, &self.overflow.deleted);
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

/// Splits a report budget across the three categories
///
/// Guarantees a share to the changes that carry the most signal - what was
/// destroyed, then what was rewritten - and lets a category that does not need
/// its share spill the surplus to the others. Returned as
/// `(added, modified, deleted)`.
///
/// The reservations are 20% deleted, 50% modified and the rest added, with at
/// least one deletion listed whenever there is one: a caller that sees
/// `deleted: []` on a run that destroyed files cannot tell that apart from a
/// clean run. Leftover capacity drains modified, then deleted, then added, so an
/// idle category costs the others nothing.
pub fn partition_budget(
    total_budget: usize,
    num_added: usize,
    num_modified: usize,
    num_deleted: usize,
) -> (usize, usize, usize) {
    if total_budget == 0 {
        return (0, 0, 0);
    }

    let cap_deleted = (total_budget * 2 / 10).max(usize::from(num_deleted > 0));
    let cap_modified = total_budget * 5 / 10;
    let cap_added = total_budget.saturating_sub(cap_deleted + cap_modified);

    let take_deleted = num_deleted.min(cap_deleted);
    let take_modified = num_modified.min(cap_modified);
    let take_added = num_added.min(cap_added);

    let mut remaining = total_budget.saturating_sub(take_deleted + take_modified + take_added);

    let extra_modified = (num_modified - take_modified).min(remaining);
    remaining -= extra_modified;

    let extra_deleted = (num_deleted - take_deleted).min(remaining);
    remaining -= extra_deleted;

    let extra_added = (num_added - take_added).min(remaining);

    (
        take_added + extra_added,
        take_modified + extra_modified,
        take_deleted + extra_deleted,
    )
}

/// Writes one category's dropped changes as `{"count":N,"by_dir":{...}}`
fn write_overflow(out: &mut String, overflow: &Overflow) {
    let _ = write!(out, "{{\"count\":{},\"by_dir\":{{", overflow.count);
    for (i, (dir, count)) in overflow.by_dir.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        write_json_string(out, dir);
        let _ = write!(out, ":{count}");
    }
    out.push_str("}}");
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
        assert_eq!(report.overflow, Overflowed::default());
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
        assert_eq!(report.overflow.added.count, 3);
        assert_eq!(
            report.overflow.added.by_dir,
            BTreeMap::from([
                ("target/debug/build/".to_string(), 2),
                ("./".to_string(), 1),
            ])
        );
        // Nothing was dropped from the empty categories, and they say so
        assert_eq!(report.overflow.modified.count, 0);
        assert_eq!(report.overflow.deleted.count, 0);
    }

    #[test]
    fn truncation_reserves_a_share_for_each_category() {
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

        // One of each, rather than two additions and nothing else
        assert_eq!(report.changes.added, vec!["a"]);
        assert_eq!(report.changes.modified.len(), 1);
        assert_eq!(report.changes.deleted, vec!["d"]);
        assert_eq!(report.total_overflow, 1);
        assert_eq!(report.overflow.added.by_dir["./"], 1);
    }

    #[test]
    fn deletions_survive_a_flood_of_additions() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = (0..100).map(|i| format!("gen/new_{i}.rs")).collect();
        report.changes.modified = (0..3)
            .map(|i| Modified {
                path: format!("src/edit_{i}.rs"),
                delta_bytes: i,
                hunks: 1,
            })
            .collect();
        report.changes.deleted = vec!["src/gone_a.rs".to_string(), "src/gone_b.rs".to_string()];
        report.changes.sort();

        report.truncate(10);

        assert_eq!(report.changes.deleted.len(), 2);
        assert_eq!(report.changes.modified.len(), 3);
        assert_eq!(report.changes.added.len(), 5);
        assert_eq!(report.changes.len(), 10);
        assert_eq!(report.overflow.added.count, 95);
        assert_eq!(report.overflow.added.by_dir["gen/"], 95);
    }

    #[test]
    fn a_zero_budget_keeps_the_counts_only() {
        let mut report = Report::new(Status::Synced);
        report.changes.added = vec!["a".to_string(), "b".to_string()];
        report.changes.modified = vec![Modified {
            path: "c".to_string(),
            delta_bytes: 1,
            hunks: 1,
        }];
        report.changes.deleted = vec!["gone.rs".to_string()];
        report.changes.sort();

        report.truncate(0);

        assert!(report.truncated);
        assert_eq!(report.changes.len(), 0);
        assert_eq!(report.total_overflow, 4);
        assert_eq!(report.overflow.added.count, 2);
        assert_eq!(report.overflow.modified.count, 1);
        assert_eq!(report.overflow.deleted.count, 1);
        assert!(report.to_json().contains("\"total_overflow\":4"));
    }

    #[test]
    fn the_budget_never_overruns_or_invents_entries() {
        for budget in 0..25 {
            for counts in [
                (0, 0, 0),
                (1, 0, 0),
                (0, 1, 0),
                (0, 0, 1),
                (100, 5, 3),
                (3, 100, 2),
            ] {
                let (a, m, d) = partition_budget(budget, counts.0, counts.1, counts.2);
                assert!(a + m + d <= budget, "{counts:?} at {budget} overspent");
                assert!(a <= counts.0 && m <= counts.1 && d <= counts.2);
                // A deletion is never the category that gets starved
                if counts.2 > 0 && budget > 0 {
                    assert!(d >= 1, "{counts:?} at {budget} hid its deletions");
                }
            }
        }
    }

    #[test]
    fn surplus_from_an_idle_category_is_spent_elsewhere() {
        // No deletions, nothing modified: additions may use the whole budget
        assert_eq!(partition_budget(10, 100, 0, 0), (10, 0, 0));
        // Deletions only: they may use the whole budget too
        assert_eq!(partition_budget(10, 0, 0, 100), (0, 0, 10));
    }

    #[test]
    fn an_error_is_reported_as_one_json_object() {
        let json = Report::error_json("Source Error -- /nope\n");

        assert_eq!(
            json,
            r#"{"status":"error","error":"Source Error -- /nope\n"}"#
        );
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
