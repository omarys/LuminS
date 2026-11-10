use std::process;

use lms::core;
use lms::parse::{self, SubCommand};
use lms::progress::PROGRESS_BAR;
use lms::report::Report;

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
    let (sub_command, flags, max_report_files) = match parse::parse_args(&args) {
        Ok(parsed) => (parsed.sub_command, parsed.flags, parsed.max_report_files),
        Err(e) => {
            eprintln!("{e}");
            // The flags never made it out of parsing, and `--ignore-vcs-and-build`
            // is not on every subcommand, so the request is read from the matches.
            if parse::reports_requested(&args) {
                println!("{}", Report::error_json(&e.to_string()));
            }
            process::exit(1);
        }
    };

    parse::set_env(flags);

    // Call correct core function depending on subcommand
    //
    // Every branch yields the reports of the directories it worked on, so the
    // reporting mode has one place to render them. Without a reporting mode the
    // reports are dropped and the run prints exactly what it always printed.
    let reports = match sub_command {
        SubCommand::Copy { src, dest } => core::copy(src, &dest, flags).map(|report| vec![report]),
        SubCommand::Synchronize { src, dest } => {
            core::synchronize(src, &dest, flags).map(|report| vec![report])
        }
        SubCommand::Remove { targets } => targets
            .iter()
            .map(|target| core::remove(target, flags))
            .collect(),
    };

    // End and remove progress bars
    PROGRESS_BAR.finish_and_clear();

    // If error, print to stderr and exit
    let mut reports: Vec<Report> = match reports {
        Ok(reports) => reports,
        Err(e) => {
            eprintln!("{}", e);
            // Under a reporting mode the failure also has to appear on stdout:
            // a caller reading a pipe would otherwise get the same silence as a
            // run that found nothing to do. The exit code still says it failed.
            if flags.reports() {
                println!("{}", Report::error_json(&e.to_string()));
            }
            process::exit(1);
        }
    };

    if flags.reports() {
        emit(&mut reports, max_report_files);
    }
}

/// Writes the run to stdout as one line of JSON
///
/// One line, one `write` call: a caller reading a pipe gets the whole document
/// or nothing, never a partial line to stitch back together.
fn emit(reports: &mut [Report], max_report_files: Option<usize>) {
    if let Some(max) = max_report_files {
        for report in reports.iter_mut() {
            report.truncate(max);
        }
    }

    let json = match reports {
        [report] => report.to_json(),
        _ => {
            let parts: Vec<String> = reports.iter().map(Report::to_json).collect();
            format!("[{}]", parts.join(","))
        }
    };

    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{json}");
    let _ = stdout.flush();
}
