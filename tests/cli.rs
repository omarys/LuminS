//! Command line contract tests.
//!
//! These run the real binary, which in-process tests cannot do: they pin the
//! exit codes and the clap 4 error plumbing that `main` is responsible for.
//! `CARGO_BIN_EXE_lms` is supplied by cargo, so no test shells out to
//! `cargo build` itself.

use std::path::PathBuf;
use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lms"))
        .args(args)
        .output()
        .expect("lms runs")
}

/// clap 2 exited 1 on a usage error; clap 4 would exit 2 on its own, so `main`
/// prints the error and exits 1 itself. This keeps that promise honest.
const USAGE_ERROR: Option<i32> = Some(1);

#[test]
fn no_args_is_a_usage_error() {
    let output = run(&[]);
    assert_eq!(output.status.code(), USAGE_ERROR);
}

#[test]
fn unknown_subcommand_is_a_usage_error() {
    let output = run(&["bogus"]);
    assert_eq!(output.status.code(), USAGE_ERROR);
}

#[test]
fn unknown_flag_is_a_usage_error() {
    let output = run(&["sync", "--bogus", ".", "."]);
    assert_eq!(output.status.code(), USAGE_ERROR);
}

#[test]
fn missing_destination_is_a_usage_error() {
    let output = run(&["sync", "."]);
    assert_eq!(output.status.code(), USAGE_ERROR);
}

#[test]
fn too_many_arguments_is_a_usage_error() {
    let output = run(&["sync", ".", ".", "."]);
    assert_eq!(output.status.code(), USAGE_ERROR);
}

#[test]
fn help_succeeds() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Usage:"));
}

#[test]
fn version_succeeds() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn missing_source_is_reported_and_fails() {
    let output = run(&["cp", "lms_no_such_directory", "."]);
    assert_eq!(output.status.code(), USAGE_ERROR);
    assert!(String::from_utf8_lossy(&output.stderr).contains("Source Error"));
}

/// A scratch tree under the system temp dir, removed when dropped
///
/// Every reporting test needs two real directories, because the report is made
/// from what a scan found on disk - there is nothing to stub out.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join("lms_cli_tests").join(name);
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }

    /// Creates `relative` under this scratch root, writing `contents` if given
    fn path(&self, relative: &str) -> PathBuf {
        let path = self.0.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        path
    }

    fn dir(&self, relative: &str) -> String {
        let path = self.path(relative);
        std::fs::create_dir_all(&path).unwrap();
        path.to_string_lossy().to_string()
    }

    fn file(&self, relative: &str, contents: &str) -> String {
        let path = self.path(relative);
        std::fs::write(&path, contents).unwrap();
        path.to_string_lossy().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The report a reporting run printed, whether or not the run succeeded
///
/// Parsed with `serde_json` on purpose: a hand-rolled expectation would agree
/// with a hand-rolled serialiser even when both are wrong.
fn report(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("not one JSON document ({e}): {stdout:?}"))
}

/// Every path a report lists, honouring the lists a caller may look at
fn listed(report: &serde_json::Value, kind: &str) -> Vec<String> {
    report["changes"][kind]
        .as_array()
        .unwrap_or_else(|| panic!("changes.{kind} is a list: {report}"))
        .iter()
        .map(|entry| match entry {
            serde_json::Value::String(path) => path.clone(),
            entry => entry["path"].as_str().expect("a path").to_string(),
        })
        .collect()
}

#[test]
fn a_report_is_one_clean_line_of_json() {
    let scratch = Scratch::new("clean_line");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/new_file.rs", "fn main() {}\n");

    let output = run(&["sync", &src, &dest, "--llm"]);
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout.clone()).expect("stdout is utf-8");
    assert_eq!(stdout.lines().count(), 1, "one line, got {stdout:?}");
    assert!(stdout.ends_with('\n'), "one terminated line: {stdout:?}");
    // Carriage returns, backspaces and escape sequences are what a progress bar
    // repaints with. None of them may reach a caller reading the output.
    for noise in ['\r', '\u{8}', '\u{1b}'] {
        assert!(
            !stdout.contains(noise),
            "{noise:?} reached stdout: {stdout:?}"
        );
    }

    let stderr = String::from_utf8(output.stderr.clone()).expect("stderr is utf-8");
    for noise in ['\r', '\u{8}', '\u{1b}'] {
        assert!(
            !stderr.contains(noise),
            "{noise:?} reached stderr: {stderr:?}"
        );
    }

    let report = report(&output);
    assert_eq!(report["status"], "synced");
    assert_eq!(listed(&report, "added"), vec!["new_file.rs"]);
    // "fn main() {}\n" is 13 bytes.
    assert_eq!(report["stats"]["transferred_bytes"], 13);
}

#[test]
fn a_dry_run_reports_what_would_change() {
    let scratch = Scratch::new("dry_run");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/changed.txt", "hello world\n");
    scratch.file("src/sub/fresh.txt", "fresh\n");
    scratch.file("dest/changed.txt", "hello\n");
    scratch.file("dest/gone.txt", "gone\n");

    let output = run(&["sync", &src, &dest, "--llm", "--dry-run"]);
    assert!(output.status.success());

    let report = report(&output);
    assert_eq!(report["status"], "dry_run");
    assert_eq!(listed(&report, "added"), vec!["sub", "sub/fresh.txt"]);
    assert_eq!(listed(&report, "deleted"), vec!["gone.txt"]);
    assert_eq!(report["stats"]["transferred_bytes"], 0);

    let modified = &report["changes"]["modified"][0];
    assert_eq!(modified["path"], "changed.txt");
    // "hello world\n" is 12 bytes, "hello\n" is 6.
    assert_eq!(modified["delta_bytes"], 6);
    assert_eq!(modified["hunks"], 1);

    // A dry run writes nothing at all.
    assert!(!scratch.path("dest/sub").exists());
    assert!(!scratch.path("dest/fresh.txt").exists());
}

#[test]
fn a_second_run_reports_no_changes() {
    let scratch = Scratch::new("idempotent");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/a.txt", "a\n");
    scratch.file("src/b/c.txt", "c\n");

    assert!(run(&["sync", &src, &dest, "--llm"]).status.success());

    let output = run(&["sync", &src, &dest, "--llm", "--dry-run"]);
    assert!(output.status.success());

    let report = report(&output);
    assert_eq!(report["status"], "dry_run");
    assert_eq!(report["changes"]["added"].as_array().unwrap().len(), 0);
    assert_eq!(report["changes"]["modified"].as_array().unwrap().len(), 0);
    assert_eq!(report["changes"]["deleted"].as_array().unwrap().len(), 0);
}

#[test]
fn json_and_machine_readable_are_the_same_report() {
    let scratch = Scratch::new("aliases");
    let src = scratch.dir("src");
    scratch.file("src/a.txt", "a\n");

    for flag in ["--json", "--machine-readable"] {
        let dest = scratch.dir(flag.trim_start_matches('-'));
        let output = run(&["sync", &src, &dest, flag]);
        assert!(output.status.success(), "{flag} failed");

        let report = report(&output);
        assert_eq!(report["status"], "synced", "{flag}");
        assert_eq!(listed(&report, "added"), vec!["a.txt"], "{flag}");
    }
}

#[test]
fn a_verbose_report_keeps_logs_off_stdout() {
    let scratch = Scratch::new("verbose");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/a.txt", "a\n");

    let output = run(&["sync", &src, &dest, "--llm", "--verbose"]);
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    assert_eq!(stdout.lines().count(), 1, "logs leaked: {stdout:?}");
    assert!(!stdout.contains('\r'));

    let stderr = String::from_utf8(output.stderr).expect("stderr is utf-8");
    assert!(
        stderr.contains("Copying file"),
        "logs are on stderr: {stderr:?}"
    );
    assert!(!stderr.contains('\r'));
}

#[test]
fn a_report_can_be_truncated_and_grouped_by_directory() {
    let scratch = Scratch::new("truncated");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    for index in 0..6 {
        scratch.file(&format!("src/gen/codegen_{index}.rs"), "generated\n");
    }
    for index in 0..2 {
        scratch.file(&format!("src/assets/asset_{index}.png"), "png\n");
    }

    let output = run(&["sync", &src, &dest, "--llm", "--max-report-files", "2"]);
    assert!(output.status.success());

    let report = report(&output);
    // Two entries survive: "assets", then "assets/asset_0.png" - the sorted head.
    assert_eq!(
        listed(&report, "added"),
        vec!["assets", "assets/asset_0.png"]
    );
    assert_eq!(report["truncated"], true);

    let overflow = report["total_overflow"].as_u64().expect("a count");
    assert_eq!(overflow, 8);

    // Every category says what it lost, so a collapsed directory is never
    // mistaken for mass deletions when it only held new files.
    let added = &report["overflow"]["added"];
    assert_eq!(added["count"], 8);
    assert_eq!(added["by_dir"]["gen/"], 6);
    assert_eq!(added["by_dir"]["assets/"], 1);
    assert_eq!(added["by_dir"]["./"], 1);
    assert_eq!(report["overflow"]["modified"]["count"], 0);
    assert_eq!(report["overflow"]["deleted"]["count"], 0);

    let grouped: u64 = report["overflow"]
        .as_object()
        .expect("a category map")
        .values()
        .flat_map(|category| category["by_dir"].as_object().unwrap().values())
        .map(|count| count.as_u64().unwrap())
        .sum();
    assert_eq!(grouped, overflow, "every dropped change is counted once");
}

#[test]
fn deletions_and_rewrites_survive_a_flood_of_additions() {
    let scratch = Scratch::new("truncated_signal");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    for index in 0..40 {
        scratch.file(&format!("src/gen/new_{index}.rs"), "generated\n");
    }
    scratch.file("src/src/edited.rs", "before\n");
    scratch.file("dest/src/edited.rs", "before\n");
    scratch.file("dest/src/gone_a.rs", "old\n");
    scratch.file("dest/src/gone_b.rs", "old\n");
    scratch.file("src/src/edited.rs", "after, and longer\n");

    // A budget of 4 leaves 40 additions competing for whatever is left after
    // the two categories a caller can least afford to miss.
    let output = run(&["sync", &src, &dest, "--llm", "--max-report-files", "4"]);
    assert!(output.status.success());

    let report = report(&output);
    assert_eq!(
        listed(&report, "deleted"),
        vec!["src/gone_a.rs", "src/gone_b.rs"]
    );
    assert_eq!(report["changes"]["modified"][0]["path"], "src/edited.rs");
    assert_eq!(listed(&report, "added").len(), 1);
    // 41 additions compete (40 files and the `gen/` directory itself); 40 lose
    assert_eq!(report["overflow"]["added"]["count"], 40);
}

#[test]
fn a_zero_budget_reports_counts_without_paths() {
    let scratch = Scratch::new("truncated_to_counts");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/a.rs", "a\n");
    scratch.file("src/b.rs", "b\n");
    scratch.file("dest/gone.rs", "c\n");

    let output = run(&["sync", &src, &dest, "--llm", "--max-report-files", "0"]);
    assert!(output.status.success());

    let report = report(&output);
    assert_eq!(report["truncated"], true);
    assert_eq!(listed(&report, "added"), Vec::<String>::new());
    assert_eq!(report["overflow"]["added"]["count"], 2);
    assert_eq!(report["overflow"]["deleted"]["by_dir"]["./"], 1);
    assert_eq!(report["total_overflow"], 3);
}

#[test]
fn truncation_is_off_without_a_limit() {
    let scratch = Scratch::new("untruncated");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    for index in 0..6 {
        scratch.file(&format!("src/file_{index}.txt"), "x\n");
    }

    let output = run(&["sync", &src, &dest, "--json"]);
    assert!(output.status.success());

    let report = report(&output);
    // The truncation keys are absent unless something was dropped, so a caller
    // never has to read a `"truncated": false` branch it does not need.
    assert!(report.get("truncated").is_none(), "{report}");
    assert!(report.get("total_overflow").is_none(), "{report}");
    assert_eq!(listed(&report, "added").len(), 6);
}

#[test]
fn the_vcs_and_build_preset_skips_artifacts() {
    let scratch = Scratch::new("preset");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    let kept = ["keep.txt", "app.py", "src/lib.rs"];
    for path in kept {
        scratch.file(&format!("src/{path}"), "kept\n");
    }
    let skipped = [
        ".git/config",
        "target/debug/build/out.rs",
        "node_modules/pkg/index.js",
        "dist/bundle.js",
        ".cache/blob",
        "mod.pyc",
        "src/nested/other.pyc",
    ];
    for path in skipped {
        scratch.file(&format!("src/{path}"), "skipped\n");
    }

    let output = run(&["cp", &src, &dest, "--llm", "--ignore-vcs-and-build"]);
    assert!(output.status.success());

    let report = report(&output);
    let added = listed(&report, "added");
    let noise = [".git", "target", "node_modules", "dist", ".cache"];
    for path in &added {
        assert!(
            !path.split('/').any(|part| noise.contains(&part)),
            "{path} should have been skipped"
        );
        assert!(!path.ends_with(".pyc"), "{path} should have been skipped");
    }
    for path in kept {
        assert!(added.iter().any(|added| added == path), "{path} is missing");
    }

    // And nothing was written for the skipped trees either.
    assert!(!scratch.path("dest/target").exists());
    assert!(!scratch.path("dest/node_modules").exists());
}

#[test]
fn removing_reports_the_deleted_tree() {
    let scratch = Scratch::new("remove");
    let target = scratch.dir("target");
    scratch.file("target/gone.txt", "gone\n");
    scratch.file("target/sub/nested.txt", "nested\n");

    let dry = run(&["rm", &target, "--llm", "--dry-run"]);
    assert!(dry.status.success());
    let dry_report = report(&dry);
    assert_eq!(dry_report["status"], "dry_run");
    assert!(dry_report["changes"]["deleted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|path| path == "gone.txt"));
    assert!(scratch.path("target/gone.txt").exists(), "dry run deleted");

    let real = run(&["rm", &target, "--llm"]);
    assert!(real.status.success());
    assert_eq!(report(&real)["status"], "synced");
    assert!(!scratch.path("target").exists());
}

#[test]
fn removing_several_targets_reports_one_document_each() {
    let scratch = Scratch::new("remove_many");
    let first = scratch.dir("first");
    let second = scratch.dir("second");
    scratch.file("first/a.txt", "a\n");
    scratch.file("second/b.txt", "b\n");

    let output = run(&["rm", &first, &second, "--llm", "--dry-run"]);
    assert!(output.status.success());

    let reports = report(&output);
    let reports = reports.as_array().expect("one report per target");
    assert_eq!(reports.len(), 2);
    assert_eq!(listed(&reports[0], "deleted"), vec!["a.txt"]);
    assert_eq!(listed(&reports[1], "deleted"), vec!["b.txt"]);
}

#[test]
fn a_failed_run_reports_on_stdout_as_well_as_stderr() {
    let scratch = Scratch::new("error_report");
    let dest = scratch.dir("dest");

    let output = run(&["sync", "lms_no_such_source", &dest, "--llm"]);
    assert_eq!(output.status.code(), USAGE_ERROR);

    // Silence on stdout is what a successful run with nothing to do looks like,
    // so a failure has to say so where the report would have been.
    let document = report(&output);
    assert_eq!(document["status"], "error");
    assert!(document["error"]
        .as_str()
        .expect("a message")
        .contains("Source Error"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Source Error"));
}

#[test]
fn a_failed_run_stays_quiet_without_a_reporting_mode() {
    let scratch = Scratch::new("error_quiet");
    let dest = scratch.dir("dest");

    let output = run(&["sync", "lms_no_such_source", &dest]);
    assert_eq!(output.status.code(), USAGE_ERROR);
    assert!(output.stdout.is_empty(), "a plain run prints no report");
}

#[test]
fn rm_refuses_the_build_artifact_preset() {
    let output = run(&["rm", "some-target", "--ignore-vcs-and-build"]);

    // On a destructive verb the flag would mean "leave the build tree behind and
    // exit 0", which reads as a clean sweep while gigabytes survive.
    assert_eq!(output.status.code(), USAGE_ERROR);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ignore-vcs-and-build"), "{stderr}");
    assert!(output.stdout.is_empty());
}

#[test]
fn the_build_artifact_preset_still_applies_to_a_sync() {
    let scratch = Scratch::new("preset_on_sync");
    let src = scratch.dir("src");
    let dest = scratch.dir("dest");
    scratch.file("src/target/debug/junk", "junk\n");
    scratch.file("src/node_modules/pkg/index.js", "js\n");
    scratch.file("src/.git/HEAD", "ref\n");
    scratch.file("src/keep.rs", "fn main() {}\n");

    let output = run(&["sync", &src, &dest, "--llm", "--ignore-vcs-and-build"]);
    assert!(output.status.success());

    let report = report(&output);
    assert_eq!(listed(&report, "added"), vec!["keep.rs"]);
}
