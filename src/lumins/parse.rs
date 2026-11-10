//! Some utilities for command line parsing.

use std::env;
use std::fs;
use std::io;
use std::io::IsTerminal;
use std::path::Path;

use bitflags::bitflags;
use clap::{Arg, ArgAction, ArgMatches, Command};
use env_logger::Builder;
use log::LevelFilter;
use thiserror::Error;

use crate::progress;

/// How many changes a machine-readable report lists before it starts
/// summarising the rest per directory
pub const DEFAULT_MAX_REPORT_FILES: usize = 50;

bitflags! {
    /// Enum to represent command line flags
    ///
    /// bitflags 2 no longer derives these traits implicitly, so they are spelled out here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Flag: u32 {
        const NO_DELETE             = 0x1;
        const SECURE                = 0x2;
        const VERBOSE               = 0x4;
        const SEQUENTIAL            = 0x8;
        const JSON                  = 0x10;
        const LLM                   = 0x20;
        const IGNORE_VCS_AND_BUILD  = 0x40;
        const DRY_RUN               = 0x80;
        const PRESERVE_VCS          = 0x100;
    }
}

impl Flag {
    /// Whether the run must emit a structured report instead of terminal output
    ///
    /// `--json` and `--llm` are the same reporting engine; they differ only in
    /// the default for `--max-report-files`, which `parse_args` resolves.
    pub fn reports(self) -> bool {
        self.intersects(Flag::JSON | Flag::LLM)
    }
}

/// A subcommand, parsed and validated
///
/// Each variant carries exactly the directories its subcommand works on, so a
/// copy cannot be handed a target list and a remove cannot be handed a source.
#[derive(Debug, PartialEq, Eq)]
pub enum SubCommand<'a> {
    /// `lms cp <src> <dest>`
    Copy { src: &'a str, dest: String },
    /// `lms sync <src> <dest>`
    Synchronize { src: &'a str, dest: String },
    /// `lms rm <target>...`
    Remove { targets: Vec<String> },
}

/// The result of parsing args
#[derive(Debug, PartialEq, Eq)]
pub struct ParseResult<'a> {
    pub sub_command: SubCommand<'a>,
    pub flags: Flag,
    /// How many changes a report lists before truncating, `None` for no limit
    pub max_report_files: Option<usize>,
}

/// Everything that can stop the command line from becoming a `SubCommand`
///
/// `main` prints it and exits non-zero; parsing no longer prints and swallows.
#[derive(Debug, Error)]
pub enum ParseError {
    #[error("no subcommand given")]
    MissingSubCommand,
    #[error("Unknown subcommand -- {0}")]
    UnknownSubCommand(String),
    #[error("Missing argument -- {0}")]
    MissingArgument(&'static str),
    #[error("Source Error -- {0} is not a directory")]
    InvalidSource(String),
    #[error("Source Error -- {src}: {source}")]
    SourceMetadata { src: String, source: io::Error },
    #[error("Destination Error -- {dest}: {source}")]
    CreateDestination { dest: String, source: io::Error },
    #[error("Target Error -- no target is a directory")]
    NoValidTargets,
    #[error("Argument Error -- {0}: {1}")]
    InvalidArgument(&'static str, String),
}

/// Builds the command line interface.
///
/// Lives next to the parser so `parse_args` can be tested against the real
/// interface; `main` only calls this and dispatches.
///
/// Replaces the clap 2 `src/cli.yml` + `load_yaml!`/`App::from_yaml` setup:
/// clap 4 has no YAML feature, so the whole interface is declared here.
pub fn command() -> Command {
    let verbose = || {
        Arg::new("verbose")
            .short('v')
            .long("verbose")
            .action(ArgAction::SetTrue)
            .help("Verbose outputs")
    };

    let sequential = |help: &'static str| {
        Arg::new("sequential")
            .short('S')
            .long("sequential")
            .action(ArgAction::SetTrue)
            .help(help)
    };

    let json = || {
        Arg::new("json")
            .long("json")
            .action(ArgAction::SetTrue)
            .help("Emit one compact JSON report on stdout and suppress the progress bar")
    };

    let llm = || {
        Arg::new("llm")
            .long("llm")
            .visible_alias("machine-readable")
            .action(ArgAction::SetTrue)
            .help(
                "Agent-first reporting: compact JSON, no progress bar, and at most \
                 --max-report-files changes listed (default 50)",
            )
    };

    let max_report_files = || {
        Arg::new("max-report-files")
            .long("max-report-files")
            .value_name("N")
            .value_parser(clap::value_parser!(usize))
            .help(
                "List at most N changes in a report, summarising the rest per directory \
                 (default 50 under --llm, unlimited otherwise; 0 keeps counts only)",
            )
    };

    // Only on the subcommands that read a source tree. On `rm` it would mean the
    // opposite of what it says: a cleanup that is asked to skip build artifacts
    // has to leave `node_modules/` and `target/` on disk and still exit 0, which
    // reads as success while gigabytes survive.
    let ignore_vcs_and_build = || {
        Arg::new("ignore-vcs-and-build")
            .long("ignore-vcs-and-build")
            .action(ArgAction::SetTrue)
            .help(
                "Skip VCS and build artifacts: .git/, target/, node_modules/, dist/, .cache/, \
                 *.pyc",
            )
    };

    let dry_run = || {
        Arg::new("dry-run")
            .short('N')
            .long("dry-run")
            .visible_alias("dryrun")
            .action(ArgAction::SetTrue)
            .help("Report what would change without touching the destination")
    };

    // `rm` cannot offer `--ignore-vcs-and-build`: it would spare the build tree
    // and exit 0, which reads as a clean sweep. What a destructive verb does
    // need is the opposite of that preset's `.git` half - spare the history,
    // delete everything else - so the flag is named for what it keeps.
    let preserve_vcs = || {
        Arg::new("preserve-vcs")
            .long("preserve-vcs")
            .action(ArgAction::SetTrue)
            .help("Leave .git/ alone; every other directory is still deleted")
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
                .arg(sequential("Copy files sequentially instead of in parallel"))
                .arg(json())
                .arg(llm())
                .arg(max_report_files())
                .arg(ignore_vcs_and_build())
                .arg(dry_run())
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
                .arg(sequential(
                    "Delete files sequentially instead of in parallel",
                ))
                .arg(json())
                .arg(llm())
                .arg(max_report_files())
                .arg(preserve_vcs())
                .arg(dry_run())
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
                .arg(sequential("Copy files sequentially instead of in parallel"))
                .arg(json())
                .arg(llm())
                .arg(max_report_files())
                .arg(ignore_vcs_and_build())
                .arg(dry_run())
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

/// Returns whether the `SetTrue` flag `name` was passed
///
/// clap 4 removed `ArgMatches::is_present`. `get_flag` would be the direct
/// replacement but it panics on an id the active subcommand does not define -
/// `nodelete` is a `sync` flag, so probing it while parsing `cp` would abort.
/// `try_get_one` answers `Ok(None)` there, which is exactly clap 2's
/// `is_present`: absent.
fn is_present(args: &ArgMatches, name: &str) -> bool {
    args.try_get_one::<bool>(name)
        .ok()
        .flatten()
        .copied()
        .unwrap_or(false)
}

/// Whether the arguments asked for a report, even when parsing them afterwards
/// failed
///
/// A run that dies on a bad source still has to answer on stdout when a caller
/// asked for a report: silence is what a run that changed nothing looks like.
/// `parse_args` returns no flags on that path, so the request is read from the
/// matches directly.
pub fn reports_requested(args: &ArgMatches) -> bool {
    let Some((_, sub_command_args)) = args.subcommand() else {
        return false;
    };
    is_present(sub_command_args, "json") || is_present(sub_command_args, "llm")
}

/// Parses command line arguments into a validated `SubCommand` and its flags,
/// creating the destination directory if it does not exist
///
/// # Errors
/// Returns a `ParseError` when the source is not a directory, the destination
/// cannot be created, or a remove target list holds no directory.
pub fn parse_args(args: &ArgMatches) -> Result<ParseResult<'_>, ParseError> {
    let (sub_command_name, args) = args.subcommand().ok_or(ParseError::MissingSubCommand)?;

    // Each flag is paired with its own bit, so the order of this list is not
    // load-bearing and a reordering cannot silently mint the wrong bits.
    const FLAGS: [(Flag, &str); 9] = [
        (Flag::NO_DELETE, "nodelete"),
        (Flag::SECURE, "secure"),
        (Flag::VERBOSE, "verbose"),
        (Flag::SEQUENTIAL, "sequential"),
        (Flag::JSON, "json"),
        (Flag::LLM, "llm"),
        (Flag::IGNORE_VCS_AND_BUILD, "ignore-vcs-and-build"),
        (Flag::DRY_RUN, "dry-run"),
        (Flag::PRESERVE_VCS, "preserve-vcs"),
    ];

    let mut flags = Flag::empty();
    for (flag, flag_name) in FLAGS {
        if is_present(args, flag_name) {
            flags |= flag;
        }
    }

    let sub_command = match sub_command_name {
        "cp" => {
            let src = required(args, "SOURCE")?;
            let dest = prepare_dest(src, required(args, "DESTINATION")?, true, flags)?;
            SubCommand::Copy { src, dest }
        }
        "sync" => {
            let src = required(args, "SOURCE")?;
            let dest = prepare_dest(src, required(args, "DESTINATION")?, false, flags)?;
            SubCommand::Synchronize { src, dest }
        }
        "rm" => SubCommand::Remove {
            targets: valid_targets(args)?,
        },
        other => return Err(ParseError::UnknownSubCommand(other.to_string())),
    };

    Ok(ParseResult {
        sub_command,
        flags,
        max_report_files: max_report_files(args, flags)?,
    })
}

/// Resolves `--max-report-files`
///
/// A report is truncated by default only in `--llm` mode: that is the mode meant
/// to be read by a model, so an unbounded list is the failure it exists to
/// prevent. `--json` keeps the explicit limit the caller asked for, and no
/// machine-readable mode means no truncation at all.
fn max_report_files(args: &ArgMatches, flags: Flag) -> Result<Option<usize>, ParseError> {
    match args.try_get_one::<usize>("max-report-files") {
        Ok(Some(&max)) => Ok(Some(max)),
        Ok(None) if flags.contains(Flag::LLM) => Ok(Some(DEFAULT_MAX_REPORT_FILES)),
        Ok(None) => Ok(None),
        // `rm` has no `--max-report-files`; absent is the answer there.
        Err(_) => Ok(None),
    }
}

/// Returns the value of the required argument `name`
///
/// `command()` marks these `required(true)`, so `None` means the interface and
/// this parser disagree; the error names the argument that is missing.
fn required<'a>(args: &'a ArgMatches, name: &'static str) -> Result<&'a str, ParseError> {
    args.get_one::<String>(name)
        .map(String::as_str)
        .ok_or(ParseError::MissingArgument(name))
}

/// Validates `src` and returns the destination directory to work in, creating it
/// when it does not exist yet
///
/// With `append_src_name`, an existing `dest` becomes `dest/<src name>`, which is
/// what `cp` does when it copies a directory into an existing one.
fn prepare_dest(
    src: &str,
    dest: &str,
    append_src_name: bool,
    flags: Flag,
) -> Result<String, ParseError> {
    let metadata = fs::metadata(src).map_err(|source| ParseError::SourceMetadata {
        src: src.to_string(),
        source,
    })?;

    if !metadata.is_dir() {
        return Err(ParseError::InvalidSource(src.to_string()));
    }

    if fs::metadata(dest).is_ok() {
        if append_src_name {
            if let Some(name) = Path::new(src).file_name() {
                return Ok(Path::new(dest).join(name).to_string_lossy().to_string());
            }
        }
        return Ok(dest.to_string());
    }

    fs::create_dir_all(dest).map_err(|source| ParseError::CreateDestination {
        dest: dest.to_string(),
        source,
    })?;

    if flags.contains(Flag::VERBOSE) && !flags.reports() {
        println!("Creating dir {dest}");
    }

    Ok(dest.to_string())
}

/// Collects the `rm` targets that are directories, warning about the rest
fn valid_targets(args: &ArgMatches) -> Result<Vec<String>, ParseError> {
    let targets = args
        .get_many::<String>("TARGET")
        .ok_or(ParseError::MissingArgument("TARGET"))?;

    // Target directory must be a valid directory
    let targets: Vec<String> = targets
        .filter(|target| match fs::metadata(target.as_str()) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    eprintln!("Target Error -- {target} is not a directory");
                }
                metadata.is_dir()
            }
            Err(e) => {
                eprintln!("Target Error -- {target}: {e}");
                false
            }
        })
        .cloned()
        .collect();

    if targets.is_empty() {
        return Err(ParseError::NoValidTargets);
    }

    Ok(targets)
}

/// Sets up the environment based on given flags
pub fn set_env(flags: Flag) {
    // A progress bar repaints with carriage returns and an ETA nobody watching a
    // pipe can see. It is turned off when the output is not a terminal, and
    // whenever the caller asked for something machine-readable.
    if flags.reports() || !std::io::stdout().is_terminal() {
        progress::disable();
    }

    let mut builder = Builder::new();
    if flags.reports() {
        // Log records go straight to stderr: routing them through a hidden
        // progress bar would silently drop them, and a buffered bar could still
        // emit a partial line.
        builder.format(|_, record| {
            eprintln!("{}", record.args());
            Ok(())
        });
    } else {
        builder.format(|_, record| {
            progress::PROGRESS_BAR.println(record.args().to_string());
            Ok(())
        });
    }

    // If verbose, enable info logging
    if flags.contains(Flag::VERBOSE) {
        env::set_var("RUST_LOG", "info");
        builder.filter(None, LevelFilter::Info).init();
    } else {
        // or else enable only error logging
        env::set_var("RUST_LOG", "error");
        builder.filter(None, LevelFilter::Error).init();
    }

    // If sequential, set Rayon to use only 1 thread
    if flags.contains(Flag::SEQUENTIAL) {
        env::set_var("RAYON_NUM_THREADS", "1");
    }
}

#[cfg(test)]
mod test_parse_args {
    use std::path::PathBuf;

    use super::*;

    /// A scratch directory under the system temp dir, removed when dropped
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join("lms_parse_tests").join(name);
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn path(&self) -> String {
            self.0.to_string_lossy().to_string()
        }

        fn sub_dir(&self, name: &str) -> String {
            let dir = self.0.join(name);
            fs::create_dir_all(&dir).unwrap();
            dir.to_string_lossy().to_string()
        }

        fn file(&self, name: &str) -> String {
            let file = self.0.join(name);
            fs::write(&file, b"contents").unwrap();
            file.to_string_lossy().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn matches(args: &[&str]) -> ArgMatches {
        command()
            .try_get_matches_from(args)
            .expect("valid command line")
    }

    #[test]
    fn cp_requires_an_existing_source() {
        let scratch = Scratch::new("cp_requires_an_existing_source");
        let missing = scratch.0.join("missing").to_string_lossy().to_string();
        let args = matches(&["lms", "cp", &missing, &scratch.path()]);

        assert!(matches!(
            parse_args(&args),
            Err(ParseError::SourceMetadata { .. })
        ));
    }

    #[test]
    fn cp_rejects_a_source_that_is_not_a_directory() {
        let scratch = Scratch::new("cp_rejects_a_source_that_is_not_a_directory");
        let file = scratch.file("file.txt");
        let args = matches(&["lms", "cp", &file, &scratch.path()]);

        assert!(matches!(
            parse_args(&args),
            Err(ParseError::InvalidSource(source)) if source == file
        ));
    }

    #[test]
    fn cp_appends_the_source_name_when_the_destination_exists() {
        let scratch = Scratch::new("cp_appends_the_source_name_when_the_destination_exists");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "cp", &src, &dest]);

        assert_eq!(
            parse_args(&args)
                .expect("source and destination exist")
                .sub_command,
            SubCommand::Copy {
                src: &src,
                dest: PathBuf::from(&dest)
                    .join("source")
                    .to_string_lossy()
                    .to_string(),
            }
        );
    }

    #[test]
    fn cp_creates_a_missing_destination() {
        let scratch = Scratch::new("cp_creates_a_missing_destination");
        let src = scratch.sub_dir("source");
        let dest = scratch.0.join("created").to_string_lossy().to_string();
        let args = matches(&["lms", "cp", &src, &dest]);

        let parsed = parse_args(&args).expect("destination is created");
        assert!(Path::new(&dest).is_dir());
        assert_eq!(parsed.sub_command, SubCommand::Copy { src: &src, dest });
    }

    #[test]
    fn sync_keeps_the_destination_as_given() {
        let scratch = Scratch::new("sync_keeps_the_destination_as_given");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", &src, &dest]);

        assert_eq!(
            parse_args(&args)
                .expect("source and destination exist")
                .sub_command,
            SubCommand::Synchronize {
                src: &src,
                dest: dest.clone(),
            }
        );
    }

    #[test]
    fn rm_keeps_only_the_directory_targets() {
        let scratch = Scratch::new("rm_keeps_only_the_directory_targets");
        let dir = scratch.sub_dir("dir");
        let file = scratch.file("file.txt");
        let args = matches(&["lms", "rm", &dir, &file]);

        assert_eq!(
            parse_args(&args)
                .expect("one target is a directory")
                .sub_command,
            SubCommand::Remove {
                targets: vec![dir.clone()],
            }
        );
    }

    #[test]
    fn rm_without_a_valid_target_errors() {
        let scratch = Scratch::new("rm_without_a_valid_target_errors");
        let file = scratch.file("file.txt");
        let args = matches(&["lms", "rm", &file]);

        assert!(matches!(parse_args(&args), Err(ParseError::NoValidTargets)));
    }

    #[test]
    fn flags_are_read_from_the_subcommand() {
        let scratch = Scratch::new("flags_are_read_from_the_subcommand");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", "-v", "-S", "-n", "-s", &src, &dest]);

        let flags = parse_args(&args)
            .expect("source and destination exist")
            .flags;
        assert!(flags.contains(Flag::VERBOSE));
        assert!(flags.contains(Flag::SEQUENTIAL));
        assert!(flags.contains(Flag::NO_DELETE));
        assert!(flags.contains(Flag::SECURE));
    }

    #[test]
    fn flags_default_to_none() {
        let scratch = Scratch::new("flags_default_to_none");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "cp", &src, &dest]);

        let flags = parse_args(&args)
            .expect("source and destination exist")
            .flags;
        assert!(flags.is_empty());
    }

    #[test]
    fn reporting_flags_are_read_from_the_subcommand() {
        let scratch = Scratch::new("reporting_flags_are_read_from_the_subcommand");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&[
            "lms",
            "sync",
            "--llm",
            "--ignore-vcs-and-build",
            "--dry-run",
            &src,
            &dest,
        ]);

        let flags = parse_args(&args)
            .expect("source and destination exist")
            .flags;
        assert!(flags.reports());
        assert!(flags.contains(Flag::LLM));
        assert!(flags.contains(Flag::IGNORE_VCS_AND_BUILD));
        assert!(flags.contains(Flag::DRY_RUN));
    }

    #[test]
    fn preserve_vcs_is_read_from_a_remove() {
        let scratch = Scratch::new("preserve_vcs_is_read_from_a_remove");
        let target = scratch.sub_dir("target");
        let args = matches(&["lms", "rm", "--preserve-vcs", &target]);

        let flags = parse_args(&args).expect("the target exists").flags;
        assert!(flags.contains(Flag::PRESERVE_VCS));
        // The two presets are opposites: one spares the build tree, the other
        // spares only the history, so neither implies the other.
        assert!(!flags.contains(Flag::IGNORE_VCS_AND_BUILD));
    }

    #[test]
    fn preserve_vcs_is_not_offered_to_a_transfer() {
        let scratch = Scratch::new("preserve_vcs_is_not_offered_to_a_transfer");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");

        for sub_command in ["cp", "sync"] {
            let args =
                command().try_get_matches_from(["lms", sub_command, "--preserve-vcs", &src, &dest]);
            assert!(args.is_err(), "{sub_command} accepted --preserve-vcs");
        }
    }

    #[test]
    fn machine_readable_is_an_alias_for_llm() {
        let scratch = Scratch::new("machine_readable_is_an_alias_for_llm");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", "--machine-readable", &src, &dest]);

        let flags = parse_args(&args)
            .expect("source and destination exist")
            .flags;
        assert!(flags.contains(Flag::LLM));
        assert!(flags.reports());
    }

    #[test]
    fn llm_mode_caps_the_report_at_fifty_files() {
        let scratch = Scratch::new("llm_mode_caps_the_report_at_fifty_files");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", "--llm", &src, &dest]);

        let parsed = parse_args(&args).expect("source and destination exist");
        assert_eq!(parsed.max_report_files, Some(DEFAULT_MAX_REPORT_FILES));
        assert_eq!(parsed.max_report_files, Some(50));
    }

    #[test]
    fn an_explicit_limit_wins_over_the_default() {
        let scratch = Scratch::new("an_explicit_limit_wins_over_the_default");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&[
            "lms",
            "sync",
            "--llm",
            "--max-report-files",
            "3",
            &src,
            &dest,
        ]);

        let parsed = parse_args(&args).expect("source and destination exist");
        assert_eq!(parsed.max_report_files, Some(3));
    }

    #[test]
    fn json_mode_does_not_truncate_unless_asked() {
        let scratch = Scratch::new("json_mode_does_not_truncate_unless_asked");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", "--json", &src, &dest]);

        let parsed = parse_args(&args).expect("source and destination exist");
        assert!(parsed.flags.reports());
        assert_eq!(parsed.max_report_files, None);
    }

    #[test]
    fn a_plain_run_never_truncates() {
        let scratch = Scratch::new("a_plain_run_never_truncates");
        let src = scratch.sub_dir("source");
        let dest = scratch.sub_dir("dest");
        let args = matches(&["lms", "sync", &src, &dest]);

        let parsed = parse_args(&args).expect("source and destination exist");
        assert!(!parsed.flags.reports());
        assert_eq!(parsed.max_report_files, None);
    }
}
