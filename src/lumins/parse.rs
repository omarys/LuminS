//! Some utilities for command line parsing.

use std::env;
use std::fs;
use std::io;
use std::path::Path;

use bitflags::bitflags;
use clap::{Arg, ArgAction, ArgMatches, Command};
use env_logger::Builder;
use log::LevelFilter;
use thiserror::Error;

use crate::progress::PROGRESS_BAR;

bitflags! {
    /// Enum to represent command line flags
    ///
    /// bitflags 2 no longer derives these traits implicitly, so they are spelled out here.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct Flag: u32 {
        const NO_DELETE     = 0x1;
        const SECURE        = 0x2;
        const VERBOSE       = 0x4;
        const SEQUENTIAL    = 0x8;
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
    const FLAGS: [(Flag, &str); 4] = [
        (Flag::NO_DELETE, "nodelete"),
        (Flag::SECURE, "secure"),
        (Flag::VERBOSE, "verbose"),
        (Flag::SEQUENTIAL, "sequential"),
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

    Ok(ParseResult { sub_command, flags })
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

    if flags.contains(Flag::VERBOSE) {
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
    let mut builder = Builder::new();
    builder.format(|_, record| {
        PROGRESS_BAR.println(record.args().to_string());
        Ok(())
    });

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
}
