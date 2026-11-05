use std::process;

use lms::core;
use lms::parse::{self, SubCommand};
use lms::progress::PROGRESS_BAR;

fn main() {
    // Parse command args
    //
    // `try_get_matches` plus an explicit exit keeps clap 2's exit codes: 0 for
    // `--help`/`--version`, 1 for every usage error. clap 4 exits 2 on its own.
    let args = match parse::command().try_get_matches() {
        Ok(matches) => matches,
        Err(e) => {
            let _ = e.print();
            process::exit(if e.use_stderr() { 1 } else { 0 });
        }
    };

    // Determine subcommands and flags from args
    let (sub_command, flags) = match parse::parse_args(&args) {
        Ok(parsed) => (parsed.sub_command, parsed.flags),
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

    parse::set_env(flags);

    // Call correct core function depending on subcommand
    let result = match sub_command {
        SubCommand::Copy { src, dest } => core::copy(src, &dest, flags),
        SubCommand::Synchronize { src, dest } => core::synchronize(src, &dest, flags),
        SubCommand::Remove { targets } => targets
            .iter()
            .try_for_each(|target| core::remove(target, flags)),
    };

    // End and remove progress bars
    PROGRESS_BAR.finish_and_clear();

    // If error, print to stderr and exit
    if let Err(e) = result {
        eprintln!("{}", e);
        process::exit(1);
    }
}
