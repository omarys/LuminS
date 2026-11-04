//! Some utilities for command line parsing.

use std::env;
use std::fs;
use std::path::PathBuf;

use bitflags::bitflags;
use clap::ArgMatches;
use env_logger::Builder;
use log::LevelFilter;

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

/// Enum to represent subcommand type
#[derive(Eq, PartialEq, Clone)]
pub enum SubCommandType {
    Copy,
    Synchronize,
    Remove,
}

/// Struct to represent subcommands
pub struct SubCommand<'a> {
    pub src: Option<&'a str>,
    pub dest: Vec<String>,
    pub sub_command_type: SubCommandType,
}

/// Struct to represent the result of parsing args
pub struct ParseResult<'a> {
    pub sub_command: SubCommand<'a>,
    pub flags: Flag,
}

/// Returns whether the `SetTrue` flag `name` was passed.
///
/// clap 4 removed `ArgMatches::is_present`. `get_flag` and `get_one` panic on an
/// id the current subcommand does not define (e.g. `nodelete` on `cp`), whereas
/// `try_get_one` answers `Ok(None)` for it, so this is the equivalent of clap 2's
/// `is_present`.
///
/// `try_get_one` only returns `Err` when the id exists but does not hold a
/// `bool`, which would mean `build_cli()` declared a flag without
/// `ArgAction::SetTrue` - a programming error, not an absent flag.
fn is_present(args: &ArgMatches, name: &str) -> bool {
    args.try_get_one::<bool>(name)
        .unwrap_or_else(|err| panic!("`{name}` is not a boolean flag: {err}"))
        .copied()
        .unwrap_or(false)
}

/// Parses command line arguments for source and destination folders and
/// creates the destination folder if it does not exist
///
/// # Errors
/// This function will return an error in the following situations,
/// but is not limited to just these cases:
/// * The source folder is not a valid directory
/// * The destination folder could not be created
pub fn parse_args<'a>(args: &'a ArgMatches) -> Result<ParseResult<'a>, ()> {
    // These are safe to unwrap since subcommands are required
    let sub_command_name = args.subcommand_name().unwrap();
    let args = args.subcommand_matches(sub_command_name).unwrap();

    // Each flag is paired with its own bit, so the order of this list is not
    // load-bearing and a reordering cannot silently mint the wrong bits.
    const FLAGS: [(Flag, &str); 4] = [
        (Flag::NO_DELETE, "nodelete"),
        (Flag::SECURE, "secure"),
        (Flag::VERBOSE, "verbose"),
        (Flag::SEQUENTIAL, "sequential"),
    ];

    // Parse for flags
    let mut flags = Flag::empty();
    for (flag, flag_name) in FLAGS {
        if is_present(args, flag_name) {
            flags |= flag;
        }
    }

    // These values are safe to unwrap since the args are required
    let mut sub_command = match sub_command_name {
        "cp" => SubCommand {
            src: Some(args.get_one::<String>("SOURCE").unwrap()),
            dest: vec![args.get_one::<String>("DESTINATION").unwrap().to_string()],
            sub_command_type: SubCommandType::Copy,
        },
        "rm" => SubCommand {
            src: None,
            dest: args
                .get_many::<String>("TARGET")
                .unwrap()
                .map(|value| value.to_string())
                .collect(),
            sub_command_type: SubCommandType::Remove,
        },
        "sync" => SubCommand {
            src: Some(args.get_one::<String>("SOURCE").unwrap()),
            dest: vec![args.get_one::<String>("DESTINATION").unwrap().to_string()],
            sub_command_type: SubCommandType::Synchronize,
        },
        _ => return Err(()),
    };

    // Validate directories
    match sub_command.sub_command_type {
        SubCommandType::Remove => {
            sub_command.dest.retain(|dest| {
                // Target directory must be a valid directory
                match fs::metadata(dest) {
                    Ok(m) => {
                        if !m.is_dir() {
                            eprintln!("Target Error -- {} is not a directory", dest);
                        }
                        m.is_dir()
                    }
                    Err(e) => {
                        eprintln!("Target Error -- {}: {}", dest, e);
                        false
                    }
                }
            });

            if sub_command.dest.is_empty() {
                return Err(());
            }
        }
        SubCommandType::Copy | SubCommandType::Synchronize => {
            // Check if src is valid
            match fs::metadata(sub_command.src.unwrap()) {
                Ok(m) => {
                    if !m.is_dir() {
                        eprintln!(
                            "Source Error -- {} is not a directory",
                            sub_command.src.unwrap()
                        );
                        return Err(());
                    }
                }
                Err(e) => {
                    eprintln!("Source Error -- {}: {}", sub_command.src.unwrap(), e);
                    return Err(());
                }
            };

            // If the directory already exists, then the directory is directory + src name
            if sub_command.sub_command_type == SubCommandType::Copy
                && fs::metadata(&sub_command.dest[0]).is_ok()
            {
                let mut new_dest = PathBuf::from(&sub_command.dest[0]);
                let src_name = PathBuf::from(sub_command.src.unwrap());
                if let Some(src_name) = src_name.file_name() {
                    new_dest.push(src_name);
                    sub_command.dest = vec![new_dest.to_string_lossy().to_string()];
                }
            }

            if fs::metadata(&sub_command.dest[0]).is_err() {
                // Create destination folder if not already existing
                match fs::create_dir_all(&sub_command.dest[0]) {
                    Ok(_) => {
                        if flags.contains(Flag::VERBOSE) {
                            println!("Creating dir {:?}", sub_command.dest[0]);
                        }
                    }
                    Err(e) => {
                        eprintln!("Destination Error -- {}: {}", sub_command.dest[0], e);
                        return Err(());
                    }
                }
            }
        }
    }

    Ok(ParseResult { sub_command, flags })
}

/// Sets up the environment based on given flags
pub fn set_env(flags: Flag) {
    let mut builder = Builder::new();
    builder.format(|_, record| {
        PROGRESS_BAR.println(format!("{}", record.args()));
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
