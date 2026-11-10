//! Contains core copy, remove, synchronize functions

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use hashbrown::HashSet;
use rayon::prelude::*;

use crate::lumins::file_ops::FileOps;
use crate::lumins::report::{self, Modified, Report, Status};
use crate::lumins::{file_ops, file_ops::Dir, parse::Flag};
use crate::progress::{self, PROGRESS_BAR};

/// The status a report of this run carries
fn status(flags: Flag) -> Status {
    if flags.contains(Flag::DRY_RUN) {
        Status::DryRun
    } else {
        Status::Synced
    }
}

/// Renders a relative path the way a report carries it
fn display(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Collects the paths that `src` holds and `dest` does not, and the reverse
/// Records the files a run rewrote, with the size delta and hunk count of each
///
/// Both lists are passed in resolved form - the files the scan already knows
/// were resized, and the same-size files the comparison has yet to check - so
/// the report is built while the destination still holds the bytes it is being
/// measured against. Only a machine-readable mode gets here; a plain run never
/// reads a file just to describe it.
fn record_modified(
    resized: Vec<&file_ops::File>,
    compared: &[&file_ops::File],
    src: &str,
    dest: &str,
    report: &mut Report,
) {
    for file in resized.into_iter().chain(compared.iter().copied()) {
        let src_file = Path::new(src).join(file.path());
        let dest_file = Path::new(dest).join(file.path());
        let Ok(diff) = report::diff(&src_file, &dest_file) else {
            continue;
        };
        // A same-size file the comparison will leave alone is not a change.
        if !diff.differs() {
            continue;
        }
        report.changes.modified.push(Modified {
            path: display(file.path()),
            delta_bytes: diff.delta_bytes,
            hunks: diff.hunks,
        });
    }
}

/// The bytes a set of rewritten paths moves out of `src`
fn bytes_of(paths: &[std::path::PathBuf], src: &str) -> u64 {
    paths
        .iter()
        .map(|path| std::fs::metadata(Path::new(src).join(path)).map_or(0, |meta| meta.len()))
        .sum()
}

/// Synchronizes all files, directories, and symlinks in `dest` with `src`
///
/// # Arguments
/// * `src`: Source directory
/// * `dest`: Destination directory
/// * `flags`: set for Flag's
///
/// # Returns
/// A report of what the run found, and changed unless `Flag::DRY_RUN` is set
///
/// # Errors
/// This function will return an error in the following situations,
/// but is not limited to just these cases:
/// * `src` is an invalid directory
/// * `dest` is an invalid directory
pub fn synchronize(src: &str, dest: &str, flags: Flag) -> Result<Report, io::Error> {
    let started = Instant::now();

    // Retrieve data from src directory about files, dirs, symlinks
    let src_file_sets = file_ops::get_all_files_with(src, flags)?;
    let src_files = src_file_sets.files();
    let src_dirs = src_file_sets.dirs();
    let src_symlinks = src_file_sets.symlinks();

    // Retrieve data from dest directory about files, dirs, symlinks
    let dest_file_sets = file_ops::get_all_files_with(dest, flags)?;
    let dest_files = dest_file_sets.files();
    let dest_dirs = dest_file_sets.dirs();
    let dest_symlinks = dest_file_sets.symlinks();

    // Initialize progress bar
    progress::progress_init(
        (src_files.len()
            + src_dirs.len()
            + src_symlinks.len()
            + dest_files.len()
            + dest_dirs.len()
            + dest_symlinks.len()) as u64,
    );

    // Determine whether or not to delete or write
    let delete = !flags.contains(Flag::NO_DELETE);
    let apply = !flags.contains(Flag::DRY_RUN);

    // The plan is built once and then executed, so what the report says happened
    // is exactly the work that was queued - not a second, drifting guess at it.
    //
    // hashbrown 0.17 dropped the rayon-backed `par_difference`/`par_intersection`
    // set operations (it now only ships `par_iter`/`into_par_iter`), so the same
    // operations are expressed as parallel filters over `par_iter`.
    let symlinks_to_delete: Vec<&file_ops::Symlink> = if delete {
        dest_symlinks
            .par_iter()
            .filter(|symlink| !src_symlinks.contains(*symlink))
            .collect()
    } else {
        Vec::new()
    };
    let files_to_delete: Vec<&file_ops::File> = if delete {
        dest_files
            .par_iter()
            .filter(|file| !src_files.contains(*file))
            .collect()
    } else {
        Vec::new()
    };

    let dirs_to_copy: Vec<&file_ops::Dir> = src_dirs
        .par_iter()
        .filter(|dir| !dest_dirs.contains(*dir))
        .collect();
    let symlinks_to_copy: Vec<&file_ops::Symlink> = src_symlinks
        .par_iter()
        .filter(|symlink| !dest_symlinks.contains(*symlink))
        .collect();
    let files_to_copy: Vec<&file_ops::File> = src_files
        .par_iter()
        .filter(|file| !dest_files.contains(*file))
        .collect();
    let files_to_compare: Vec<&file_ops::File> = src_files
        .par_iter()
        .filter(|file| dest_files.contains(*file))
        .collect();

    // The report classifies by path, while the plan above compares whole
    // entries (a `File` carries its size). A file whose size changed is one
    // modification to a reader, not a deletion plus an addition, so the report
    // never reuses those sets.
    let src_dir_paths = paths(src_dirs);
    let dest_dir_paths = paths(dest_dirs);
    let src_symlink_paths = paths(src_symlinks);
    let dest_symlink_paths = paths(dest_symlinks);
    let src_file_paths = paths(src_files);
    let dest_file_paths = paths(dest_files);

    let mut report = Report::new(status(flags));
    report.stats.scanned = src_files.len() + src_dirs.len() + src_symlinks.len();
    // Sizes come straight out of the scan, so the byte total of everything that
    // has to be created is known before a single byte moves.
    let added_bytes: u64 = files_to_copy.iter().map(|file| file.size()).sum();
    let resized_files: Vec<&file_ops::File> = files_to_copy
        .iter()
        .copied()
        .filter(|file| dest_file_paths.contains(file.path()))
        .collect();

    report.changes.added = src_dirs
        .iter()
        .filter(|dir| !dest_dir_paths.contains(dir.path()))
        .map(|dir| display(dir.path()))
        .chain(
            src_symlinks
                .iter()
                .filter(|link| !dest_symlink_paths.contains(link.path()))
                .map(|link| display(link.path())),
        )
        .chain(
            src_files
                .iter()
                .filter(|file| !dest_file_paths.contains(file.path()))
                .map(|file| display(file.path())),
        )
        .collect();

    if delete {
        report.changes.deleted = dest_symlinks
            .iter()
            .filter(|link| !src_symlink_paths.contains(link.path()))
            .map(|link| display(link.path()))
            .chain(
                dest_files
                    .iter()
                    .filter(|file| !src_file_paths.contains(file.path()))
                    .map(|file| display(file.path())),
            )
            .chain(
                dest_dirs
                    .iter()
                    .filter(|dir| !src_dir_paths.contains(dir.path()))
                    .map(|dir| display(dir.path())),
            )
            .collect();
    }

    // Modified files are the ones a single path cannot classify: both sides have
    // the entry, and what it holds differs. The destination still holds the old
    // bytes at this point, so this is where they are measured.
    if flags.reports() {
        record_modified(resized_files, &files_to_compare, src, dest, &mut report);
        for link in src_symlinks.iter().filter(|link| {
            dest_symlink_paths.contains(link.path()) && !dest_symlinks.contains(*link)
        }) {
            report.changes.modified.push(Modified {
                path: display(link.path()),
                delta_bytes: 0,
                hunks: 1,
            });
        }
    }

    report.changes.sort();

    let transferred = if apply {
        file_ops::delete_files(symlinks_to_delete.into_par_iter(), dest)?;
        file_ops::delete_files(files_to_delete.into_par_iter(), dest)?;

        file_ops::copy_files(dirs_to_copy.into_par_iter(), src, dest)?;
        file_ops::copy_files(symlinks_to_copy.into_par_iter(), src, dest)?;
        file_ops::copy_files(files_to_copy.into_par_iter(), src, dest)?;

        let differing =
            file_ops::compare_and_copy_files(files_to_compare.into_par_iter(), src, dest, flags)?;

        // Delete dirs in the correct order
        if delete {
            let dirs_to_delete = dest_dirs.par_iter().filter(|dir| !src_dirs.contains(*dir));
            let dirs_to_delete: Vec<&file_ops::Dir> = file_ops::sort_files(dirs_to_delete);
            file_ops::delete_files_sequential(dirs_to_delete, dest)?;
        }

        added_bytes + bytes_of(&differing, src)
    } else {
        // Nothing is written, but the comparison still has to run: it is the only
        // way to know which files a real run would rewrite.
        file_ops::compare_and_copy_files(files_to_compare.into_par_iter(), src, dest, flags)?;
        0
    };

    report.stats.transferred_bytes = transferred;
    report.stats.duration_ms = started.elapsed().as_millis();
    report.changes.sort();
    Ok(report)
}

/// The paths of a set of entries
fn paths<'a, T>(entries: &'a HashSet<T>) -> HashSet<&'a Path>
where
    T: file_ops::FileOps + 'a,
{
    entries.iter().map(|entry| entry.path()).collect()
}

/// What `dest` already holds, or nothing when it is not there yet
///
/// `cp` creates the destination as it goes, so a destination that cannot be read
/// yet is empty, not an error.
fn existing(dest: &str, flags: Flag) -> file_ops::FileSets {
    file_ops::get_all_files_with(dest, flags).unwrap_or_else(|_| {
        file_ops::FileSets::with(HashSet::new(), HashSet::new(), HashSet::new())
    })
}

/// Copies all files, directories, and symlinks in `src` to `dest`
///
/// # Arguments
/// * `src`: Source directory
/// * `dest`: Destination directory
/// * `flags`: set for Flag's
///
/// # Returns
/// A report of what the run found, and copied unless `Flag::DRY_RUN` is set
///
/// # Errors
/// This function will return an error in the following situations,
/// but is not limited to just these cases:
/// * `src` is an invalid directory
/// * `dest` is an invalid directory
pub fn copy(src: &str, dest: &str, flags: Flag) -> Result<Report, io::Error> {
    let started = Instant::now();

    // Retrieve data from src directory about files, dirs, symlinks
    let src_file_sets = file_ops::get_all_files_with(src, flags)?;
    let src_files = src_file_sets.files();
    let src_dirs = src_file_sets.dirs();
    let src_symlinks = src_file_sets.symlinks();

    // Whatever the destination already holds decides added versus rewritten
    let dest_file_sets = existing(dest, flags);
    let dest_file_paths = paths(dest_file_sets.files());
    let dest_dir_paths = paths(dest_file_sets.dirs());
    let dest_symlink_paths = paths(dest_file_sets.symlinks());

    // Initialize progress bar
    progress::progress_init((src_files.len() + src_dirs.len() + src_symlinks.len()) as u64);

    let mut report = Report::new(status(flags));
    report.stats.scanned = src_files.len() + src_dirs.len() + src_symlinks.len();

    // A copy has no comparison of its own: by path, an entry is either new to the
    // destination or it is about to be rewritten.
    report.changes.added = src_dirs
        .iter()
        .filter(|dir| !dest_dir_paths.contains(dir.path()))
        .map(|dir| display(dir.path()))
        .chain(
            src_symlinks
                .iter()
                .filter(|link| !dest_symlink_paths.contains(link.path()))
                .map(|link| display(link.path())),
        )
        .chain(
            src_files
                .iter()
                .filter(|file| !dest_file_paths.contains(file.path()))
                .map(|file| display(file.path())),
        )
        .collect();

    // Before the copy, or every file would already match itself.
    if flags.reports() {
        let overwritten: Vec<&file_ops::File> = src_files
            .iter()
            .filter(|file| dest_file_paths.contains(file.path()))
            .collect();
        record_overwritten(overwritten, src, dest, &mut report);
    }

    let transferred = if flags.contains(Flag::DRY_RUN) {
        0
    } else {
        // Copy everything
        file_ops::copy_files(src_dirs.into_par_iter(), src, dest)?;
        file_ops::copy_files(src_files.into_par_iter(), src, dest)?;
        file_ops::copy_files(src_symlinks.into_par_iter(), src, dest)?;
        src_files.iter().map(|file| file.size()).sum()
    };

    report.stats.transferred_bytes = transferred;
    report.stats.duration_ms = started.elapsed().as_millis();
    report.changes.sort();
    Ok(report)
}

/// Records the files a copy overwrote
fn record_overwritten(files: Vec<&file_ops::File>, src: &str, dest: &str, report: &mut Report) {
    for file in files {
        let src_file = Path::new(src).join(file.path());
        let dest_file = Path::new(dest).join(file.path());
        let Ok(diff) = report::diff(&src_file, &dest_file) else {
            continue;
        };
        // A file the copy rewrites byte for byte is not a change worth reporting.
        if !diff.differs() {
            continue;
        }
        report.changes.modified.push(Modified {
            path: display(file.path()),
            delta_bytes: diff.delta_bytes,
            hunks: diff.hunks,
        });
    }
}

/// Deletes directory `target`
///
/// # Arguments
/// * `target`: Target directory
/// * `flags`: set for Flag's
///
/// # Returns
/// A report of what the run found, and deleted unless `Flag::DRY_RUN` is set
///
/// # Errors
/// This function will return an error in the following situations,
/// but is not limited to just these cases:
/// * `target` is an invalid directory
pub fn remove(target: &str, flags: Flag) -> Result<Report, io::Error> {
    let started = Instant::now();

    // Retrieve data from target directory about files, dirs, symlinks
    let target_file_sets = file_ops::get_all_files_with(target, flags)?;
    let target_files = target_file_sets.files();
    let target_dirs = target_file_sets.dirs();
    let target_symlinks = target_file_sets.symlinks();

    // Initialize progress bar
    progress::progress_init(
        (target_files.len() + target_dirs.len() + target_symlinks.len()) as u64,
    );
    if progress::is_enabled() {
        PROGRESS_BAR.enable_steady_tick(Duration::from_millis(1000));
    }

    let mut report = Report::new(status(flags));
    report.stats.scanned = target_files.len() + target_dirs.len() + target_symlinks.len();
    report.changes.deleted = target_files
        .iter()
        .map(|file| display(file.path()))
        .chain(target_symlinks.iter().map(|link| display(link.path())))
        .chain(target_dirs.iter().map(|dir| display(dir.path())))
        .collect();

    if flags.contains(Flag::DRY_RUN) {
        report.stats.duration_ms = started.elapsed().as_millis();
        report.changes.sort();
        return Ok(report);
    }

    // Delete everything
    file_ops::delete_files(target_files.into_par_iter(), target)?;
    file_ops::delete_files(target_symlinks.into_par_iter(), target)?;

    // Directories must always be deleted sequentially so that they are deleted in the correct order
    let mut target_dirs: Vec<&file_ops::Dir> = file_ops::sort_files(target_dirs.into_par_iter());

    // Delete the target directory last
    let root_dir = Dir::from("");
    target_dirs.push(&root_dir);

    file_ops::delete_files_sequential(target_dirs, target)?;

    report.stats.duration_ms = started.elapsed().as_millis();
    report.changes.sort();
    Ok(report)
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test_synchronize {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[cfg(debug_assertions)]
    const BUILD_DIR: &str = "target/debug";

    #[cfg(not(debug_assertions))]
    const BUILD_DIR: &str = "target/release";

    #[test]
    fn invalid_src() {
        assert!(synchronize("/?", "src", Flag::empty()).is_err());
    }

    #[test]
    fn invalid_dest() {
        assert!(synchronize("src", "/?", Flag::empty()).is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn dir_1() {
        const TEST_DIR: &str = "test_synchronize_dir1";
        fs::create_dir_all(TEST_DIR).unwrap();

        assert!(synchronize("src", TEST_DIR, Flag::empty()).is_ok());

        let diff = Command::new("diff")
            .args(["-r", "src", TEST_DIR])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn dir_2() {
        const TEST_DIR: &str = "test_synchronize_dir2";
        fs::create_dir_all(TEST_DIR).unwrap();

        assert!(synchronize(BUILD_DIR, TEST_DIR, Flag::empty()).is_ok());

        let diff = Command::new("diff")
            .args(["-r", BUILD_DIR, TEST_DIR])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::File::create([BUILD_DIR, "file.txt"].join("/")).unwrap();
        fs::remove_dir_all([BUILD_DIR, "build"].join("/")).unwrap();

        let diff = Command::new("diff")
            .args(["-r", BUILD_DIR, TEST_DIR])
            .output()
            .unwrap();

        assert!(!diff.status.success());

        assert!(synchronize(BUILD_DIR, TEST_DIR, Flag::empty()).is_ok());

        let diff = Command::new("diff")
            .args(["-r", BUILD_DIR, TEST_DIR])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn change_symlink() {
        use std::os::unix::fs::symlink;

        const TEST_SRC: &str = "test_synchronize_change_symlink_src";
        const TEST_DEST: &str = "test_synchronize_change_symlink_dest";
        fs::create_dir_all(TEST_SRC).unwrap();
        fs::create_dir_all(TEST_DEST).unwrap();

        symlink("../Cargo.lock", [TEST_SRC, "file"].join("/")).unwrap();
        symlink("../Cargo.toml", [TEST_DEST, "file"].join("/")).unwrap();

        let diff = Command::new("diff")
            .args(["-r", TEST_SRC, TEST_DEST])
            .output()
            .unwrap();

        assert!(!diff.status.success());

        assert!(synchronize(TEST_SRC, TEST_DEST, Flag::empty()).is_ok());

        let diff = Command::new("diff")
            .args(["-r", TEST_SRC, TEST_DEST])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DEST).unwrap();
        fs::remove_dir_all(TEST_SRC).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn flags() {
        const TEST_DIR: &str = "test_synchronize_flags";
        const TEST_DIR_OUT: &str = "test_synchronize_flags_out";
        const TEST_DIR_EXPECTED: &str = "test_synchronize_flags_expected";
        const TEST_FILES: [&str; 2] = ["file1.txt", "file2.txt"];

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_OUT).unwrap();
        fs::create_dir_all(TEST_DIR_EXPECTED).unwrap();

        fs::File::create([TEST_DIR, TEST_FILES[0]].join("/")).unwrap();
        fs::File::create([TEST_DIR_EXPECTED, TEST_FILES[0]].join("/")).unwrap();
        fs::File::create([TEST_DIR_EXPECTED, TEST_FILES[1]].join("/")).unwrap();

        assert!(synchronize(TEST_DIR, TEST_DIR_OUT, Flag::empty()).is_ok());

        fs::File::create([TEST_DIR, TEST_FILES[1]].join("/")).unwrap();

        let mut flags = Flag::empty();
        flags.insert(Flag::VERBOSE);
        flags.insert(Flag::NO_DELETE);
        flags.insert(Flag::SECURE);
        flags.insert(Flag::SEQUENTIAL);

        assert!(synchronize(TEST_DIR, TEST_DIR_OUT, flags).is_ok());

        let diff = Command::new("diff")
            .args(["-r", TEST_DIR_OUT, TEST_DIR_EXPECTED])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
        fs::remove_dir_all(TEST_DIR_EXPECTED).unwrap();
    }
}

#[cfg(test)]
mod test_copy {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[test]
    fn invalid_src() {
        assert!(copy("/?", "src", Flag::empty()).is_err());
    }

    #[test]
    fn invalid_dest() {
        const TEST_DIR: &str = "test_copy_invalid_dest";
        assert!(copy("src", TEST_DIR, Flag::empty()).is_ok());
        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn dir1() {
        const TEST_DIR: &str = "test_copy_dir1";
        fs::create_dir_all(TEST_DIR).unwrap();

        assert!(copy("src", TEST_DIR, Flag::empty()).is_ok());

        let diff = Command::new("diff")
            .args(["-r", "src", TEST_DIR])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn flags() {
        const TEST_DIR: &str = "test_copy_flags";
        fs::create_dir_all(TEST_DIR).unwrap();

        let mut flags = Flag::empty();
        flags.insert(Flag::SEQUENTIAL);

        assert!(copy("src", TEST_DIR, flags).is_ok());

        let diff = Command::new("diff")
            .args(["-r", "src", TEST_DIR])
            .output()
            .unwrap();

        assert!(diff.status.success());

        fs::remove_dir_all(TEST_DIR).unwrap();
    }
}

#[cfg(test)]
mod test_remove {
    use super::*;
    use std::fs;
    use std::process::Command;

    #[cfg(debug_assertions)]
    const BUILD_DIR: &str = "target/debug";

    #[cfg(not(debug_assertions))]
    const BUILD_DIR: &str = "target/release";

    #[test]
    fn invalid_target() {
        assert!(remove("/?", Flag::empty()).is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn dir1() {
        const TEST_DIR: &str = "test_remove_dir1";
        fs::create_dir_all(TEST_DIR).unwrap();

        Command::new("cp")
            .args(["-r", BUILD_DIR, TEST_DIR])
            .output()
            .unwrap();

        assert!(remove(TEST_DIR, Flag::empty()).is_ok());

        assert!(fs::read_dir(TEST_DIR).is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn flags() {
        const TEST_DIR: &str = "test_remove_flags";
        fs::create_dir_all(TEST_DIR).unwrap();

        let mut flags = Flag::empty();
        flags.insert(Flag::SEQUENTIAL);

        Command::new("cp")
            .args(["-r", "src", TEST_DIR])
            .output()
            .unwrap();

        assert!(remove(TEST_DIR, flags).is_ok());

        assert!(fs::read_dir(TEST_DIR).is_err());
    }
}
