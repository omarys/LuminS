//! Command line contract tests.
//!
//! These run the real binary, which in-process tests cannot do: they pin the
//! exit codes and the clap 4 error plumbing that `main` is responsible for.
//! `CARGO_BIN_EXE_lms` is supplied by cargo, so no test shells out to
//! `cargo build` itself.

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
