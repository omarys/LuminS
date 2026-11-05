use std::process;

use clap::{Arg, ArgAction, Command};

use lms::core;
use lms::parse::{self, SubCommandType};
use lms::progress::PROGRESS_BAR;

/// Builds the command line interface.
///
/// Replaces the clap 2 `src/cli.yml` + `load_yaml!`/`App::from_yaml` setup:
/// clap 4 has no YAML feature, so the whole interface is declared here.
fn build_cli() -> Command {
    let verbose = || {
        Arg::new("verbose")
            .short('v')
            .long("verbose")
            .action(ArgAction::SetTrue)
            .help("Verbose outputs")
    };

    Command::new("LuminS")
        .version(env!("CARGO_PKG_VERSION"))
        // clap 2 propagated the version to subcommands, so `lms cp -V` worked.
        // clap 4 only does that on request.
        .propagate_version(true)
        .about(
            "Luminous Synchronize - A fast and reliable multithreaded alternative to rsync \
             for synchronizing local files",
        )
        .help_template("{name} {version}\n{about}\n\n{usage-heading} {usage}\n\n{all-args}")
        .arg_required_else_help(true)
        .subcommand(
            Command::new("cp")
                .about("Multithreaded directory copy")
                .arg_required_else_help(true)
                .arg(verbose())
                .arg(
                    Arg::new("sequential")
                        .short('S')
                        .long("sequential")
                        .action(ArgAction::SetTrue)
                        .help("Copy files sequentially instead of in parallel"),
                )
                .arg(
                    Arg::new("SOURCE")
                        .help("Source directory")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("DESTINATION")
                        .help("Destination directory")
                        .required(true)
                        .index(2),
                ),
        )
        .subcommand(
            Command::new("rm")
                .about("Multithreaded directory remove")
                .arg_required_else_help(true)
                .arg(verbose())
                .arg(
                    Arg::new("sequential")
                        .short('S')
                        .long("sequential")
                        .action(ArgAction::SetTrue)
                        .help("Delete files sequentially instead of in parallel"),
                )
                .arg(
                    Arg::new("TARGET")
                        .help("Target directory")
                        .required(true)
                        .num_args(1..)
                        .index(1),
                ),
        )
        .subcommand(
            Command::new("sync")
                .about("Multithreaded directory synchronization")
                .visible_alias("s")
                .arg_required_else_help(true)
                .arg(
                    Arg::new("nodelete")
                        .short('n')
                        .long("nodelete")
                        .action(ArgAction::SetTrue)
                        .help("Do not delete any destination files"),
                )
                .arg(
                    Arg::new("secure")
                        .short('s')
                        .long("secure")
                        .action(ArgAction::SetTrue)
                        .help("Use a cryptographic hash function for hashing similar files"),
                )
                .arg(verbose())
                .arg(
                    Arg::new("sequential")
                        .short('S')
                        .long("sequential")
                        .action(ArgAction::SetTrue)
                        .help("Copy files sequentially instead of in parallel"),
                )
                .arg(
                    Arg::new("SOURCE")
                        .help("Source directory")
                        .required(true)
                        .index(1),
                )
                .arg(
                    Arg::new("DESTINATION")
                        .help("Destination directory")
                        .required(true)
                        .index(2),
                ),
        )
}

fn main() {
    // Parse command args
    //
    // `try_get_matches` plus an explicit exit keeps clap 2's exit codes: 0 for
    // `--help`/`--version`, 1 for every usage error. clap 4 exits 2 on its own.
    let args = match build_cli().try_get_matches() {
        Ok(matches) => matches,
        Err(e) => {
            let _ = e.print();
            process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };

    // Determine subcommands and flags from args
    let (sub_command, flags) = match parse::parse_args(&args) {
        Ok(f) => (f.sub_command, f.flags),
        Err(_) => process::exit(1),
    };

    parse::set_env(flags);

    // Call correct core function depending on subcommand
    let result = match sub_command.sub_command_type {
        SubCommandType::Copy => core::copy(sub_command.src.unwrap(), &sub_command.dest[0], flags),
        SubCommandType::Remove => sub_command
            .dest
            .iter()
            .try_for_each(|dest| core::remove(dest, flags)),
        SubCommandType::Synchronize => {
            core::synchronize(sub_command.src.unwrap(), &sub_command.dest[0], flags)
        }
    };

    // End and remove progress bars
    PROGRESS_BAR.finish_and_clear();

    // If error, print to stderr and exit
    if let Err(e) = result {
        eprintln!("{}", e);
        process::exit(1);
    }
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test_main {
    use std::fs;
    use std::process::Command;

    #[cfg(debug_assertions)]
    const BUILD_DIR: &str = "target/debug";

    #[cfg(not(debug_assertions))]
    const BUILD_DIR: &str = "target/release";

    #[test]
    fn test_no_args() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        let output = Command::new("target/release/lms").output().unwrap();

        assert!(!output.status.success());
    }

    #[test]
    fn test_no_dest() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        let output = Command::new("target/release/lms")
            .args(["sync", "src"])
            .output()
            .unwrap();

        assert!(!output.status.success());
    }

    #[test]
    fn test_too_many_args() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        let output = Command::new("target/release/lms")
            .args(["sync", "src", "dest", "dest"])
            .output()
            .unwrap();

        assert!(!output.status.success());
    }

    #[test]
    fn test_invalid_args() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        let output = Command::new("target/release/lms")
            .args(["sync", "a", "dest"])
            .output()
            .unwrap();

        assert!(!output.status.success());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_copy() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_copy";

        Command::new("target/release/lms")
            .args(["cp", "-v", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DEST).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_secure() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_secure";
        fs::create_dir_all(TEST_DEST).unwrap();

        Command::new("target/release/lms")
            .args(["sync", "-s", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DEST).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_sequential() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_sequential";

        Command::new("target/release/lms")
            .args(["sync", "-S", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DEST).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_sequential_copy() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_sequential_copy";

        Command::new("target/release/lms")
            .args(["cp", "-S", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DEST).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_no_delete() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE1: &str = "test_main_test_no_delete_source1";
        const TEST_SOURCE2: &str = "test_main_test_no_delete_source2";
        const TEST_DEST: &str = "test_main_test_no_delete_out";
        const TEST_EXPECTED: &str = "test_main_test_no_delete_expected";
        const TEST_FILE1: &str = "Cargo.toml";
        const TEST_FILE2: &str = "Cargo.lock";

        fs::create_dir_all(TEST_SOURCE1).unwrap();
        fs::create_dir_all(TEST_SOURCE2).unwrap();
        fs::create_dir_all(TEST_EXPECTED).unwrap();

        fs::copy(TEST_FILE1, [TEST_SOURCE1, TEST_FILE1].join("/")).unwrap();
        fs::copy(TEST_FILE2, [TEST_SOURCE2, TEST_FILE2].join("/")).unwrap();
        fs::copy(TEST_FILE1, [TEST_EXPECTED, TEST_FILE1].join("/")).unwrap();
        fs::copy(TEST_FILE2, [TEST_EXPECTED, TEST_FILE2].join("/")).unwrap();

        Command::new("target/release/lms")
            .args(["cp", TEST_SOURCE1, TEST_DEST])
            .output()
            .unwrap();

        Command::new("target/release/lms")
            .args(["sync", "-n", TEST_SOURCE2, TEST_DEST])
            .output()
            .unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_DEST, TEST_EXPECTED])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_SOURCE1).unwrap();
        fs::remove_dir_all(TEST_SOURCE2).unwrap();
        fs::remove_dir_all(TEST_DEST).unwrap();
        fs::remove_dir_all(TEST_EXPECTED).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_remove() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_remove";
        fs::create_dir_all(TEST_DEST).unwrap();

        Command::new("cp")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        Command::new("target/release/lms")
            .args(["rm", TEST_DEST])
            .output()
            .unwrap();

        assert!(fs::read_dir(TEST_DEST).is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_remove_multiple() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: [&str; 2] = ["test_main_test_remove1", "test_main_test_remove2"];
        fs::create_dir_all(TEST_DEST[0]).unwrap();
        fs::create_dir_all(TEST_DEST[1]).unwrap();

        Command::new("cp")
            .args(["-r", TEST_SOURCE, TEST_DEST[0]])
            .output()
            .unwrap();

        Command::new("cp")
            .args(["-r", TEST_SOURCE, TEST_DEST[1]])
            .output()
            .unwrap();

        Command::new("target/release/lms")
            .args(["rm", TEST_DEST[0], TEST_DEST[1]])
            .output()
            .unwrap();

        assert!(fs::read_dir(TEST_DEST[0]).is_err());
        assert!(fs::read_dir(TEST_DEST[1]).is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn test_sequential_remove() {
        Command::new("cargo")
            .args(["build", "--release"])
            .output()
            .unwrap();

        const TEST_SOURCE: &str = BUILD_DIR;
        const TEST_DEST: &str = "test_main_test_sequential_remove";
        fs::create_dir_all(TEST_DEST).unwrap();

        Command::new("cp")
            .args(["-r", TEST_SOURCE, TEST_DEST])
            .output()
            .unwrap();

        Command::new("target/release/lms")
            .args(["rm", "-S", TEST_DEST])
            .output()
            .unwrap();

        assert!(fs::read_dir(TEST_DEST).is_err());
    }
}
