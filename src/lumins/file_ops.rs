//! Contains utilities for copying, deleting, sorting, hashing files.

use std::hash::Hasher;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::{fs, io};

use blake2::{Blake2b512, Digest};
use hashbrown::HashSet;
use log::{error, info};
use rayon::prelude::*;
use seahash::SeaHasher;

use crate::lumins::parse::Flag;
use crate::progress::PROGRESS_BAR;

/// Removes whatever currently sits at `dest` so that a new entry can take its
/// place.
///
/// Every create below (`fs::copy`, `symlink`, `create_dir_all`) either fails
/// with `EEXIST` or leaves the stale entry untouched when the destination is of
/// a different type or points somewhere else. That is precisely the case a sync
/// exists to fix, so the conflict has to be cleared first.
///
/// Only called after a create has already failed, never as a pre-check: a
/// `symlink_metadata` on every file costs more than the whole copy of a small
/// file (measured at ~2.5 us/file, +32% on a 7262-file tree).
fn clear_existing(dest: &Path) -> io::Result<()> {
    match fs::symlink_metadata(dest) {
        // A directory symlink is removed like a file on unix, but Windows only
        // lets it go through `remove_dir`.
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => fs::remove_dir_all(dest),
        Ok(_) => match fs::remove_file(dest) {
            Err(error) if error.kind() != io::ErrorKind::NotFound => {
                fs::remove_dir_all(dest).map_err(|_| error)
            }
            other => other,
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// Interface for all file structs to perform common operations
///
/// Ensures that all files (file, dir, symlink) have
/// a way of obtaining their path, copying, and deleting
pub trait FileOps {
    fn path(&self) -> &Path;
    /// Removes the entry at `path`
    ///
    /// # Errors
    /// Returns the failure instead of logging it, so a delete that did not
    /// happen still reaches the caller: `lms` must not report success on a
    /// partially deleted tree.
    fn remove(&self, path: &Path) -> io::Result<()>;
    /// Copies the entry to `dest`
    ///
    /// # Errors
    /// Returns the failure for the same reason as `remove`.
    fn copy(&self, src: &Path, dest: &Path) -> io::Result<()>;
}

/// A struct that represents a single file
#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub struct File {
    path: PathBuf,
    size: u64,
}

impl FileOps for File {
    fn path(&self) -> &Path {
        &self.path
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)?;
        info!("Deleting file {:?}", path);
        Ok(())
    }
    fn copy(&self, src: &Path, dest: &Path) -> io::Result<()> {
        // `fs::copy` overwrites a file. Only a directory in its way is a type
        // change, and it is worth clearing - but the check belongs on the error
        // path, where it costs nothing per file.
        if let Err(error) = fs::copy(src, dest) {
            if !fs::symlink_metadata(dest).is_ok_and(|meta| meta.is_dir()) {
                return Err(error);
            }
            clear_existing(dest)?;
            fs::copy(src, dest)?;
        }
        info!("Copying file {:?} -> {:?}", src, dest);
        Ok(())
    }
}

impl File {
    pub fn from(path: &str, size: u64) -> Self {
        File {
            path: PathBuf::from(path),
            size,
        }
    }
}

/// A struct that represents a single directory
#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub struct Dir {
    path: PathBuf,
}

impl FileOps for Dir {
    fn path(&self) -> &Path {
        &self.path
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_dir(path)?;
        info!("Deleting dir {:?}", path);
        Ok(())
    }
    fn copy(&self, _src: &Path, dest: &Path) -> io::Result<()> {
        // `create_dir_all` accepts an existing directory, so only a file in its
        // way needs clearing.
        if let Err(error) = fs::create_dir_all(dest) {
            if !fs::symlink_metadata(dest).is_ok_and(|meta| !meta.is_dir()) {
                return Err(error);
            }
            clear_existing(dest)?;
            fs::create_dir_all(dest)?;
        }
        info!("Creating dir {:?}", dest);
        Ok(())
    }
}

impl Dir {
    pub fn from(dir: &str) -> Self {
        Dir {
            path: PathBuf::from(dir),
        }
    }
}

/// A struct that represents a single symbolic link
#[derive(Hash, Eq, PartialEq, Debug, Clone)]
pub struct Symlink {
    path: PathBuf,
    target: PathBuf,
}

impl FileOps for Symlink {
    fn path(&self) -> &Path {
        &self.path
    }
    fn remove(&self, path: &Path) -> io::Result<()> {
        fs::remove_file(path)?;
        info!("Deleting symlink {:?}", path);
        Ok(())
    }
    #[cfg(target_family = "unix")]
    fn copy(&self, _src: &Path, dest: &Path) -> io::Result<()> {
        use std::os::unix::fs;

        // `symlink` never overwrites: a link that merely points somewhere else
        // is still an entry at `dest`, and has to go first or the old target
        // silently wins.
        if let Err(error) = fs::symlink(&self.target, dest) {
            // `fs` here is `std::os::<platform>::fs`, which has no metadata
            if std::fs::symlink_metadata(dest).is_err() {
                return Err(error);
            }
            clear_existing(dest)?;
            fs::symlink(&self.target, dest)?;
        }
        info!("Creating symlink {:?} -> {:?}", dest, self.target);
        Ok(())
    }
    #[cfg(target_family = "windows")]
    fn copy(&self, _src: &Path, dest: &Path) -> io::Result<()> {
        use std::os::windows::fs;

        // A dangling target is created as a file symlink; `is_dir` is the only
        // distinction Windows makes.
        let create = || {
            if self.target.is_dir() {
                fs::symlink_dir(&self.target, dest)
            } else {
                fs::symlink_file(&self.target, dest)
            }
        };
        if let Err(error) = create() {
            // `fs` here is `std::os::<platform>::fs`, which has no metadata
            if std::fs::symlink_metadata(dest).is_err() {
                return Err(error);
            }
            clear_existing(dest)?;
            create()?;
        }
        if self.target.is_dir() {
            info!("Creating symlink dir {:?} -> {:?}", dest, self.target);
        } else {
            info!("Creating symlink file {:?} -> {:?}", dest, self.target);
        }
        Ok(())
    }
}

impl Symlink {
    pub fn from(path: &str, target: &str) -> Self {
        Symlink {
            path: PathBuf::from(path),
            target: PathBuf::from(target),
        }
    }
}

/// A struct that represents sets of different types of files
#[derive(Eq, PartialEq, Debug)]
pub struct FileSets {
    files: HashSet<File>,
    dirs: HashSet<Dir>,
    symlinks: HashSet<Symlink>,
}

impl FileSets {
    /// Initializes FileSets with the given sets
    ///
    /// # Arguments
    /// * `files`: a set of files
    /// * `dirs`: a set of dirs
    /// * `symlinks`: a set of symlinks
    ///
    /// # Returns
    /// A newly created FileSets struct
    pub fn with(files: HashSet<File>, dirs: HashSet<Dir>, symlinks: HashSet<Symlink>) -> Self {
        FileSets {
            files,
            dirs,
            symlinks,
        }
    }
    /// Gets the set of files
    ///
    /// # Returns
    /// The FileSets set of files
    pub fn files(&self) -> &HashSet<File> {
        &self.files
    }
    /// Gets the set of dirs
    ///
    /// # Returns
    /// The FileSets set of dirs
    pub fn dirs(&self) -> &HashSet<Dir> {
        &self.dirs
    }
    /// Gets the set of symlinks
    ///
    /// # Returns
    /// The FileSets set of symlinks
    pub fn symlinks(&self) -> &HashSet<Symlink> {
        &self.symlinks
    }
}

/// Compares all files in `files_to_compare` in `src` with all files in `files_to_compare` in `dest`
/// and copies them over if they are different, in parallel
///
/// # Arguments
/// * `files_to_compare`: files to compare
/// * `src`: base directory of the files to copy from, such that for all `file` in
///   `files_to_compare`, `src + file.path()` is the absolute path of the source file
/// * `dest`: base directory of the files to copy to, such that for all `file` in
///   `files_to_compare`, `dest + file.path()` is the absolute path of the destination file
/// * `flags`: set for Flag's
///
/// # Errors
/// Returns an error naming the entries that could not be compared or copied;
/// every other file is still processed.
pub fn compare_and_copy_files<'a, T, S>(
    files_to_compare: T,
    src: &str,
    dest: &str,
    flags: Flag,
) -> io::Result<()>
where
    T: ParallelIterator<Item = &'a S>,
    S: FileOps + Sync + 'a,
{
    let failures = Failures::default();

    files_to_compare.for_each(|file| {
        if let Err(e) = compare_and_copy_file(file, src, dest, flags) {
            failures.record(&Path::new(src).join(file.path()), e);
        }
        PROGRESS_BAR.inc(2);
    });

    failures.into_result("copy")
}

/// Compares the given file and copies the src file over if it differs from the dest file
///
/// # Arguments
/// * `file_to_compare`: file to compare
/// * `src`: base directory of the file to copy from, such that `src + file.path()`
///   is the absolute path of the source file
/// * `dest`: base directory of the files to copy to, such that `dest + file.path()`
///   is the absolute path of the destination file
/// * `flags`: set for Flag's
///
/// # Errors
/// Returns an error if the copy was needed and failed.
fn compare_and_copy_file<S>(
    file_to_compare: &S,
    src: &str,
    dest: &str,
    flags: Flag,
) -> io::Result<()>
where
    S: FileOps,
{
    // An unhashable source is copied rather than skipped: it may be the only
    // copy left, and copying it again is cheaper than losing it.
    let copy = if flags.contains(Flag::SECURE) {
        match hash_file_secure(file_to_compare, src) {
            None => true,
            Some(src_hash) => Some(src_hash) != hash_file_secure(file_to_compare, dest),
        }
    } else {
        match hash_file(file_to_compare, src) {
            None => true,
            Some(src_hash) => Some(src_hash) != hash_file(file_to_compare, dest),
        }
    };

    if copy {
        copy_file(file_to_compare, src, dest)
    } else {
        Ok(())
    }
}

/// Copies all given files from `src` to `dest` in parallel
///
/// # Arguments
/// * `files_to_copy`: files to copy
/// * `src`: base directory of the files to copy from, such that for all `file` in
///   `files_to_copy`, `src + file.path()` is the absolute path of the source file
/// * `dest`: base directory of the files to copy to, such that for all `file` in
///   `files_to_copy`, `dest + file.path()` is the absolute path of the destination file
///
/// # Errors
/// Returns an error naming the entries that could not be copied; every other
/// file is still copied.
pub fn copy_files<'a, T, S>(files_to_copy: T, src: &str, dest: &str) -> io::Result<()>
where
    T: ParallelIterator<Item = &'a S>,
    S: FileOps + Sync + 'a,
{
    let failures = Failures::default();

    files_to_copy.for_each(|file| {
        if let Err(e) = copy_file(file, src, dest) {
            failures.record(&Path::new(src).join(file.path()), e);
        }
        PROGRESS_BAR.inc(1);
    });

    failures.into_result("copy")
}

/// Copies a single file from `src` to `dest`
///
/// # Arguments
/// * `files_to_copy`: file to copy
/// * `src`: base directory of the files to copy from, such that `src + file_to_copy.path()`
///   is the absolute path of the source file
/// * `dest`: base directory of the files to copy to, such that `dest + file.path()`
///   is the absolute path of the destination file
fn copy_file<S>(file_to_copy: &S, src: &str, dest: &str) -> io::Result<()>
where
    S: FileOps,
{
    let src_file = Path::new(src).join(file_to_copy.path());
    let dest_file = Path::new(dest).join(file_to_copy.path());

    file_to_copy.copy(&src_file, &dest_file)
}

/// Deletes all given files in parallel
///
/// There is no guarantee that this function will delete the files in the given order
///
/// # Arguments
/// * `files_to_delete`: files to delete
/// * `location`: base directory of the files to delete, such that for all `file` in
///   `files_to_delete`, `location + file.path()` is the absolute path of the file
///
/// # Errors
/// Returns an error naming the entries that could not be deleted; every other
/// file is still deleted.
pub fn delete_files<'a, T, S>(files_to_delete: T, location: &str) -> io::Result<()>
where
    T: ParallelIterator<Item = &'a S>,
    S: FileOps + Sync + 'a,
{
    let failures = Failures::default();

    files_to_delete.for_each(|file| {
        let path = Path::new(location).join(file.path());
        if let Err(e) = file.remove(&path) {
            failures.record(&path, e);
        }
        PROGRESS_BAR.inc(1);
    });

    failures.into_result("delete")
}

/// Deletes all given files sequentially
///
/// This function ensures that the files are deleted in the exact order given
///
/// # Arguments
/// * `files_to_delete`: files to delete, or sorted empty directories
/// * `location`: base directory of the files to delete, such that for all `file` in
///   `files_to_delete`, `location + file.path()` is the absolute path of the file
///
/// # Errors
/// Returns an error naming the directories that could not be deleted; the
/// remaining directories are still attempted.
pub fn delete_files_sequential<'a, T, S>(files_to_delete: T, location: &str) -> io::Result<()>
where
    T: IntoIterator<Item = &'a S>,
    S: FileOps + 'a,
{
    let failures = Failures::default();

    for file in files_to_delete {
        let path = Path::new(location).join(file.path());
        if let Err(e) = file.remove(&path) {
            failures.record(&path, e);
        }
        PROGRESS_BAR.inc(1);
    }

    failures.into_result("delete")
}

/// Collects the failures of one pass over a tree
///
/// A single unreadable file must not abort a whole sync or delete, but a partial
/// result must not be reported as success either. Failures are gathered here and
/// reported once, when the pass is over.
#[derive(Debug, Default)]
struct Failures(Mutex<Vec<String>>);

impl Failures {
    /// Records the failure of `path`, recovering the list if a worker panicked
    /// while holding the lock
    fn record(&self, path: &Path, err: io::Error) {
        let mut failures = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        failures.push(format!("{path:?}: {err}"));
    }

    /// Consumes the collector, turning `action` failures into an error
    ///
    /// Every failure is reported up to a handful, so that the message stays
    /// readable without needing `RUST_LOG` to find out what went wrong.
    fn into_result(self, action: &str) -> io::Result<()> {
        const SHOWN: usize = 5;

        let failures = self
            .0
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(first) = failures.first() else {
            return Ok(());
        };

        let mut message = format!("failed to {action} {} entries\n  {first}", failures.len());
        let rest = &failures[1..];
        for failure in rest.iter().take(SHOWN - 1) {
            message.push_str(&format!("\n  {failure}"));
        }
        if rest.len() >= SHOWN {
            message.push_str(&format!("\n  ... and {} more", rest.len() - (SHOWN - 1)));
        }

        Err(io::Error::other(message))
    }
}

/// Sorts (unstable) file paths in descending order by number of components, in parallel
///
/// # Arguments
/// `files_to_sort`: files to sort
///
/// # Returns
/// A vector of file paths in descending order by number of components
///
/// # Examples
/// ["a", "a/b", "a/b/c"] becomes ["a/b/c", "a/b", "a"]
/// ["/usr", "/", "/usr/bin", "/etc"] becomes ["/usr/bin", "/usr", "/etc", "/"]
pub fn sort_files<'a, T, S>(files_to_sort: T) -> Vec<&'a S>
where
    T: ParallelIterator<Item = &'a S>,
    S: FileOps + Sync + 'a,
{
    let mut files_to_sort = Vec::from_par_iter(files_to_sort);
    files_to_sort.par_sort_unstable_by(|a, b| {
        b.path()
            .components()
            .count()
            .cmp(&a.path().components().count())
    });
    files_to_sort
}

/// Generates a hash of the given file, using the Seahash non-cryptographic hash function
///
/// # Arguments
/// * `file_to_hash`: file object to hash
/// * `location`: base directory of the file to hash, such that
///   `location + file_to_hash.path()` is the absolute path of the file
///
/// # Returns
/// * Some: The hash of the given file
/// * Err: If the given file cannot be hashed
pub fn hash_file<S>(file_to_hash: &S, location: &str) -> Option<u64>
where
    S: FileOps,
{
    let path = Path::new(location).join(file_to_hash.path());

    let mut file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(e) => {
            error!("Error -- Opening File: {:?}: {}", file_to_hash.path(), e);
            return None;
        }
    };

    // Streaming keeps memory flat: a sync of a large tree used to hold whole
    // files in a `Vec` just to compare them.
    let mut hasher = SeaHasher::new();

    match io::copy(&mut file, &mut HasherWriter(&mut hasher)) {
        Ok(_) => Some(hasher.finish()),
        Err(e) => {
            error!("Error -- Hashing: {:?}: {}", file_to_hash.path(), e);
            None
        }
    }
}

/// Adapts a `Hasher` to `io::Write`, so a file can be hashed as it is read
struct HasherWriter<'a, H: Hasher>(&'a mut H);

impl<H: Hasher> Write for HasherWriter<'_, H> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Generates a hash of the given file, using the BLAKE2b cryptographic hash function
///
/// # Arguments
/// * `file_to_hash`: file object to hash
/// * `location`: base directory of the file to hash, such that
///   `location + file_to_hash.path()` is the absolute path of the file
///
/// # Returns
/// * Some: The hash of the given file
/// * Err: If the given file cannot be hashed
pub fn hash_file_secure<S>(file_to_hash: &S, location: &str) -> Option<Vec<u8>>
where
    S: FileOps,
{
    let file = Path::new(location).join(file_to_hash.path());

    match &mut fs::File::open(&file) {
        Ok(file) => {
            let mut hasher = Blake2b512::new();

            match io::copy(file, &mut hasher) {
                Ok(_) => Some(hasher.finalize().to_vec()),
                Err(e) => {
                    error!("Error -- Hashing: {:?}: {}", file_to_hash.path(), e);
                    None
                }
            }
        }
        Err(e) => {
            error!("Error -- Opening File: {:?}: {}", file_to_hash.path(), e);
            None
        }
    }
}

/// Recursively traverses a directory and all its subdirectories and returns
/// a FileSets that contains all files and all directories
///
/// # Arguments
/// * `src`: directory to traverse
///
/// # Returns
/// * Ok: A `FileSets` containing a set of files a set of directories
/// * Error: If `src` is an invalid directory
pub fn get_all_files(src: &str) -> Result<FileSets, io::Error> {
    get_all_files_helper(Path::new(src), src)
}

/// Recursive helper for `get_all_files`
///
/// # Arguments
/// * `src`: directory to traverse
/// * `base`: directory to traverse, used for recursive calls
///
/// # Returns
/// * Ok: A `FileSets` containing a set of files a set of directories
/// * Error: If `src` is an invalid directory
fn get_all_files_helper(src: &Path, base: &str) -> Result<FileSets, io::Error> {
    let dir = src.read_dir()?;

    let mut files = HashSet::new();
    let mut dirs = HashSet::new();
    let mut symlinks = HashSet::new();

    for file in dir {
        let file = match file {
            Ok(file) => file,
            Err(e) => {
                error!("Error -- Reading entry of {:?}: {}", src, e);
                continue;
            }
        };

        let metadata = match file.metadata() {
            Ok(metadata) => metadata,
            Err(e) => {
                error!("Error -- Reading metadata of {:?}: {}", file.path(), e);
                continue;
            }
        };

        let path = file.path();
        // This is safe to unwrap, since `get_all_files` always calls this helper
        // with `base` equal to `src`
        let relative_path = path.strip_prefix(base).unwrap();

        if metadata.is_dir() {
            dirs.insert(Dir {
                path: relative_path.to_path_buf(),
            });

            // Recursively call `get_all_files_helper` on the subdirectory
            match get_all_files_helper(&file.path(), base) {
                Ok(file_sets) => {
                    // Add subdirectory subdirectories and files to sets
                    files.extend(file_sets.files);
                    dirs.extend(file_sets.dirs);
                    symlinks.extend(file_sets.symlinks);
                }
                Err(e) => {
                    error!("Error - Retrieving files: {}", e);
                    continue;
                }
            }
        } else if metadata.is_file() {
            files.insert(File {
                path: relative_path.to_path_buf(),
                size: metadata.len(),
            });
        } else {
            // If not a file nor dir, must be a symlink
            match fs::read_link(&path) {
                Ok(target) => {
                    symlinks.insert(Symlink {
                        path: relative_path.to_path_buf(),
                        target,
                    });
                }
                Err(e) => {
                    error!("Error - Reading symlink: {}", e);
                    continue;
                }
            }
        }
    }

    Ok(FileSets::with(files, dirs, symlinks))
}

///////////////////////////////////////////////////////////////////////////////////////////////////
// Tests
///////////////////////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test_file_ops {
    use super::*;

    #[test]
    fn create_dir() {
        assert_eq!(
            Dir::from("."),
            Dir {
                path: PathBuf::from("."),
            }
        )
    }

    #[test]
    fn create_file() {
        assert_eq!(
            File::from(".", 10),
            File {
                path: PathBuf::from("."),
                size: 10,
            }
        )
    }

    #[test]
    fn create_symlink() {
        assert_eq!(
            Symlink::from(".", "file"),
            Symlink {
                path: PathBuf::from("."),
                target: PathBuf::from("file"),
            }
        )
    }
}

#[cfg(test)]
mod test_get_all_files {
    use super::*;
    use std::process::Command;

    #[test]
    fn invalid_dir() {
        assert!(get_all_files("/?").is_err());
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn dir_insufficient_permissions() {
        assert!(get_all_files("/root").is_err());
    }

    #[test]
    fn empty_dir() {
        const TEST_DIR: &str = "test_get_all_files_empty_dir";

        fs::create_dir(TEST_DIR).unwrap();

        let file_sets = get_all_files(TEST_DIR).unwrap();

        assert_eq!(file_sets.files(), &HashSet::new());
        assert_eq!(file_sets.dirs(), &HashSet::new());

        fs::remove_dir(TEST_DIR).unwrap();
    }

    #[test]
    fn single_dir() {
        const TEST_DIR: &str = "test_get_all_files_single_dir";
        const TEST_SUB_DIR: &str = "test";

        fs::create_dir_all([TEST_DIR, TEST_SUB_DIR].join("/")).unwrap();

        let file_sets = get_all_files(TEST_DIR).unwrap();
        let mut dir_set = HashSet::new();
        dir_set.insert(Dir {
            path: PathBuf::from(&TEST_SUB_DIR),
        });

        assert_eq!(file_sets.files(), &HashSet::new());
        assert_eq!(file_sets.dirs(), &dir_set);

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[test]
    fn single_file() {
        const TEST_DIR: &str = "test_get_all_files_single_file";
        const TEST_FILE: &str = "file.txt";

        fs::create_dir_all(TEST_DIR).unwrap();

        fs::File::create([TEST_DIR, TEST_FILE].join("/")).unwrap();
        fs::write([TEST_DIR, TEST_FILE].join("/"), b"1234").unwrap();

        let file_sets = get_all_files(TEST_DIR).unwrap();
        let mut file_set = HashSet::new();
        file_set.insert(File {
            path: PathBuf::from(TEST_FILE),
            size: 4,
        });

        assert_eq!(file_sets.files(), &file_set);
        assert_eq!(file_sets.dirs(), &HashSet::new());

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn single_symlink() {
        use std::os::unix::fs::symlink;
        const TEST_DIR: &str = "test_get_all_files_single_symlink";
        const TEST_LINK: &str = "test_get_all_files_single_symlink/file";
        const TEST_FILE: &str = "test_get_all_files_single_symlink/test.txt";

        fs::create_dir_all(TEST_DIR).unwrap();
        symlink(TEST_FILE, TEST_LINK).unwrap();

        let mut symlink_set = HashSet::new();
        symlink_set.insert(Symlink {
            path: PathBuf::from("file"),
            target: PathBuf::from(TEST_FILE),
        });

        let file_sets = get_all_files(TEST_DIR).unwrap();

        assert_eq!(
            file_sets,
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: symlink_set,
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[test]
    fn multi_level() {
        const TEST_DIR: &str = "test_get_all_files_multi_level";
        const SUB_DIRS: [&str; 2] = ["dir1", "dir1/dir2"];
        const TEST_FILES: [&str; 3] = ["file.txt", "dir1/file.txt", "dir1/dir2/file2.txt"];
        const TEST_DATA: [&[u8]; 3] = [b"1", b"", b"1234567890"];

        fs::create_dir_all([TEST_DIR, SUB_DIRS[1]].join("/")).unwrap();

        for i in 0..TEST_FILES.len() {
            let path = [TEST_DIR, TEST_FILES[i]].join("/");
            fs::File::create(&path).unwrap();
            fs::write(&path, TEST_DATA[i]).unwrap();
        }

        let file_sets = get_all_files(TEST_DIR).unwrap();
        let mut file_set = HashSet::new();
        let mut dir_set = HashSet::new();

        for i in 0..TEST_FILES.len() {
            file_set.insert(File {
                path: PathBuf::from(TEST_FILES[i]),
                size: TEST_DATA[i].len() as u64,
            });
        }

        for sub_dir in SUB_DIRS {
            dir_set.insert(Dir {
                path: PathBuf::from(sub_dir),
            });
        }

        assert_eq!(file_sets.files(), &file_set);
        assert_eq!(file_sets.dirs(), &dir_set);

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn multi_level_insufficient_permissions() {
        const TEST_DIR: &str = "test_get_all_files_multi_level_insufficient_permissions";
        const SUB_DIR: &str = "dir";
        const TEST_FILE: &str = "file.txt";

        let file_path = [TEST_DIR, TEST_FILE].join("/");
        let dir_path = [TEST_DIR, SUB_DIR].join("/");

        fs::create_dir_all(&dir_path).unwrap();
        fs::File::create(&file_path).unwrap();

        Command::new("chmod")
            .args(["000", &file_path])
            .output()
            .unwrap();
        Command::new("chmod")
            .args(["000", &dir_path])
            .output()
            .unwrap();

        let file_sets = get_all_files(TEST_DIR).unwrap();

        let mut file_set = HashSet::new();
        file_set.insert(File {
            path: PathBuf::from(&TEST_FILE),
            size: 0,
        });
        let mut dir_set = HashSet::new();
        dir_set.insert(Dir {
            path: PathBuf::from(&SUB_DIR),
        });

        assert_eq!(file_sets.files(), &file_set);
        assert_eq!(file_sets.dirs(), &dir_set);

        Command::new("chmod")
            .arg("777")
            .args(["777", &dir_path])
            .output()
            .unwrap();
        fs::remove_dir_all(TEST_DIR).unwrap();
    }
}

#[cfg(test)]
mod test_sort_files {
    use super::*;

    #[test]
    fn no_dir() {
        let no_dir: HashSet<Dir> = HashSet::new();
        assert_eq!(sort_files(no_dir.par_iter()), Vec::<&Dir>::new());
    }

    #[test]
    fn single_dir() {
        let mut single_dir: HashSet<Dir> = HashSet::new();
        let dir = Dir {
            path: PathBuf::from("/"),
        };
        single_dir.insert(dir.clone());
        let expected: Vec<&Dir> = vec![&dir];

        assert_eq!(sort_files(single_dir.par_iter()), expected);
    }

    #[test]
    fn multi_dir_unique() {
        let mut multi_dir: HashSet<Dir> = HashSet::new();
        let dir1 = Dir {
            path: PathBuf::from("/"),
        };
        let dir2 = Dir {
            path: PathBuf::from("/a"),
        };
        let dir3 = Dir {
            path: PathBuf::from("/a/b"),
        };
        multi_dir.insert(dir1.clone());
        multi_dir.insert(dir2.clone());
        multi_dir.insert(dir3.clone());
        let expected: Vec<&Dir> = vec![&dir3, &dir2, &dir1];

        assert_eq!(sort_files(multi_dir.par_iter()), expected);
    }

    #[test]
    fn multi_dir() {
        let mut multi_dir: HashSet<Dir> = HashSet::new();
        let dir1 = Dir {
            path: PathBuf::from("/"),
        };
        let dir2 = Dir {
            path: PathBuf::from("/a/c"),
        };
        let dir3 = Dir {
            path: PathBuf::from("/a/b"),
        };
        multi_dir.insert(dir1.clone());
        multi_dir.insert(dir2.clone());
        multi_dir.insert(dir3.clone());
        let expected: Vec<&Dir> = vec![&dir2, &dir3, &dir1];

        assert_eq!(
            sort_files(multi_dir.par_iter()).get(2).unwrap(),
            &expected[2]
        );
    }
}

#[cfg(test)]
mod test_hash_file {
    use super::*;

    #[test]
    fn invalid_file() {
        assert_eq!(
            hash_file(
                &File {
                    path: PathBuf::from("test"),
                    size: 0,
                },
                "."
            ),
            None
        );
    }

    #[test]
    fn empty_file() {
        const TEST_FILE1: &str = "test_hash_file_empty_file1.txt";
        const TEST_FILE2: &str = "test_hash_file_empty_file2.txt";

        fs::File::create(TEST_FILE1).unwrap();
        fs::File::create(TEST_FILE2).unwrap();

        assert_eq!(
            hash_file(
                &File {
                    path: PathBuf::from(TEST_FILE1),
                    size: 0,
                },
                "."
            ),
            hash_file(
                &File {
                    path: PathBuf::from(TEST_FILE2),
                    size: 0,
                },
                "."
            )
        );
        assert_eq!(
            hash_file_secure(
                &File {
                    path: PathBuf::from(TEST_FILE1),
                    size: 0,
                },
                "."
            ),
            hash_file_secure(
                &File {
                    path: PathBuf::from(TEST_FILE2),
                    size: 0,
                },
                "."
            )
        );

        fs::remove_file(TEST_FILE1).unwrap();
        fs::remove_file(TEST_FILE2).unwrap();
    }

    #[test]
    fn equal_files() {
        const TEST_DIR: &str = "test_hash_file_equal_files";
        const TEST_FILE1: &str = "file1.txt";
        const TEST_FILE2: &str = "file2.txt";

        let path1 = [TEST_DIR, TEST_FILE1].join("/");
        let path2 = [TEST_DIR, TEST_FILE2].join("/");

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::File::create(&path1).unwrap();
        fs::File::create(&path2).unwrap();
        fs::write(path1, b"1234567890").unwrap();
        fs::write(path2, b"1234567890").unwrap();

        assert_eq!(
            hash_file(
                &File {
                    path: PathBuf::from(TEST_FILE1),
                    size: 10,
                },
                "."
            ),
            hash_file(
                &File {
                    path: PathBuf::from(TEST_FILE2),
                    size: 10,
                },
                "."
            )
        );
        assert_eq!(
            hash_file_secure(
                &File {
                    path: PathBuf::from(TEST_FILE1),
                    size: 10,
                },
                "."
            ),
            hash_file_secure(
                &File {
                    path: PathBuf::from(TEST_FILE2),
                    size: 10,
                },
                "."
            )
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[test]
    fn different_files() {
        assert_ne!(
            hash_file(
                &File {
                    path: PathBuf::from("lumins/file_ops.rs"),
                    size: 0,
                },
                "src"
            ),
            hash_file(
                &File {
                    path: PathBuf::from("main.rs"),
                    size: 0,
                },
                "src"
            )
        );
        assert_ne!(
            hash_file_secure(
                &File {
                    path: PathBuf::from("lumins/file_ops.rs"),
                    size: 0,
                },
                "src"
            ),
            hash_file_secure(
                &File {
                    path: PathBuf::from("main.rs"),
                    size: 0,
                },
                "src"
            )
        );
    }
}

#[cfg(test)]
mod test_delete_files {
    use super::*;

    #[test]
    fn delete_no_files() {
        const TEST_DIR: &str = "test_delete_files_delete_no_files";
        const TEST_FILES: [&str; 2] = ["file1.txt", "file2.txt"];

        fs::create_dir_all(TEST_DIR).unwrap();

        let files_to_delete: HashSet<File> = HashSet::new();
        let files_to_delete_sequential: Vec<&File> = Vec::new();
        let mut file_set = HashSet::new();

        for test_file in TEST_FILES {
            fs::File::create([TEST_DIR, test_file].join("/")).unwrap();
            let file = File {
                path: PathBuf::from(test_file),
                size: 0,
            };
            file_set.insert(file);
        }

        delete_files(files_to_delete.par_iter(), TEST_DIR).unwrap();
        delete_files_sequential(files_to_delete_sequential, TEST_DIR).unwrap();

        assert_eq!(
            get_all_files(TEST_DIR).unwrap(),
            FileSets {
                files: file_set,
                dirs: HashSet::new(),
                symlinks: HashSet::new(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn delete_invalid_file_and_link() {
        use std::os::unix::fs::symlink;

        const TEST_DIR: &str = "test_delete_files_delete_invalid_file_and_link";
        const TEST_DIR_SEQ: &str = "test_delete_files_delete_invalid_file_and_link_seq";
        const TEST_FILES: [&str; 2] = ["file1.txt", "file2.txt"];

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_SEQ).unwrap();

        let mut files_to_delete: HashSet<File> = HashSet::new();
        let mut files_to_delete_sequential: Vec<&File> = Vec::new();
        let mut file_set = HashSet::new();

        fs::File::create([TEST_DIR, TEST_FILES[0]].join("/")).unwrap();
        fs::File::create([TEST_DIR_SEQ, TEST_FILES[0]].join("/")).unwrap();
        let file = File {
            path: PathBuf::from([TEST_FILES[0], "a"].join("/")),
            size: 0,
        };
        let expected_file = File {
            path: PathBuf::from(TEST_FILES[0]),
            size: 0,
        };
        file_set.insert(expected_file);
        files_to_delete.insert(file.clone());
        files_to_delete_sequential.push(&file);

        let mut links_to_delete: HashSet<Symlink> = HashSet::new();
        let mut links_to_delete_sequential: Vec<&Symlink> = Vec::new();
        let mut link_set = HashSet::new();

        symlink(TEST_FILES[1], [TEST_DIR, "file"].join("/")).unwrap();
        symlink(TEST_FILES[1], [TEST_DIR_SEQ, "file"].join("/")).unwrap();
        let link = Symlink {
            path: PathBuf::from("filea"),
            target: PathBuf::from(TEST_FILES[1]),
        };
        let expected_link = Symlink {
            path: PathBuf::from("file"),
            target: PathBuf::from(TEST_FILES[1]),
        };
        link_set.insert(expected_link);
        links_to_delete.insert(link.clone());
        links_to_delete_sequential.push(&link);

        // `file1.txt/a` is below a file and `filea` has no such link, so both
        // deletes fail - and are reported. The valid entries stay untouched.
        let error = delete_files(files_to_delete.par_iter(), TEST_DIR)
            .expect_err("deleting a path below a file fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );
        let error = delete_files_sequential(files_to_delete_sequential, TEST_DIR_SEQ)
            .expect_err("deleting a path below a file fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );
        let error = delete_files(links_to_delete.par_iter(), TEST_DIR)
            .expect_err("deleting a missing symlink fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );
        let error = delete_files_sequential(links_to_delete_sequential, TEST_DIR_SEQ)
            .expect_err("deleting a missing symlink fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );

        assert_eq!(
            get_all_files(TEST_DIR).unwrap(),
            FileSets {
                files: file_set.clone(),
                dirs: HashSet::new(),
                symlinks: link_set.clone(),
            }
        );
        assert_eq!(
            get_all_files(TEST_DIR_SEQ).unwrap(),
            FileSets {
                files: file_set,
                dirs: HashSet::new(),
                symlinks: link_set,
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_SEQ).unwrap();
    }

    #[cfg(target_family = "unix")]
    #[test]
    fn delete_file_and_link() {
        use std::os::unix::fs::symlink;

        const TEST_DIR: &str = "test_delete_files_delete_file_and_link";
        const TEST_DIR_SEQ: &str = "test_delete_files_delete_file_and_link_seq";
        const TEST_FILES: [&str; 2] = ["file1.txt", "file2.txt"];

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_SEQ).unwrap();

        let mut files_to_delete: HashSet<File> = HashSet::new();
        let mut files_to_delete_sequential: Vec<&File> = Vec::new();
        let mut file_set = HashSet::new();

        fs::File::create([TEST_DIR, TEST_FILES[0]].join("/")).unwrap();
        fs::File::create([TEST_DIR_SEQ, TEST_FILES[0]].join("/")).unwrap();
        let file = File {
            path: PathBuf::from(TEST_FILES[0]),
            size: 0,
        };
        file_set.insert(file.clone());
        files_to_delete.insert(file.clone());
        files_to_delete_sequential.push(&file);

        let mut links_to_delete: HashSet<Symlink> = HashSet::new();
        let mut links_to_delete_sequential: Vec<&Symlink> = Vec::new();
        let mut link_set = HashSet::new();

        symlink(TEST_FILES[1], [TEST_DIR, "file"].join("/")).unwrap();
        symlink(TEST_FILES[1], [TEST_DIR_SEQ, "file"].join("/")).unwrap();
        let link = Symlink {
            path: PathBuf::from("file"),
            target: PathBuf::from(TEST_FILES[1]),
        };
        link_set.insert(link.clone());
        links_to_delete.insert(link.clone());
        links_to_delete_sequential.push(&link);

        delete_files(files_to_delete.par_iter(), TEST_DIR).unwrap();
        delete_files_sequential(files_to_delete_sequential, TEST_DIR_SEQ).unwrap();
        delete_files(links_to_delete.par_iter(), TEST_DIR).unwrap();
        delete_files_sequential(links_to_delete_sequential, TEST_DIR_SEQ).unwrap();

        assert_eq!(
            get_all_files(TEST_DIR).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: HashSet::new(),
            }
        );
        assert_eq!(
            get_all_files(TEST_DIR_SEQ).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: HashSet::new(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_SEQ).unwrap();
    }

    #[test]
    fn delete_partial_dirs() {
        const TEST_DIR: &str = "test_delete_files_delete_partial_dirs";
        const TEST_DIR_SEQ: &str = "test_delete_files_delete_partial_dirs_seq";
        const TEST_SUB_DIRS: [&str; 3] = ["dir0", "dir1", "dir2"];

        fs::create_dir_all([TEST_DIR, TEST_SUB_DIRS[0], TEST_SUB_DIRS[1]].join("/")).unwrap();
        fs::create_dir_all([TEST_DIR_SEQ, TEST_SUB_DIRS[0], TEST_SUB_DIRS[1]].join("/")).unwrap();
        fs::create_dir_all([TEST_DIR, TEST_SUB_DIRS[2]].join("/")).unwrap();
        fs::create_dir_all([TEST_DIR_SEQ, TEST_SUB_DIRS[2]].join("/")).unwrap();

        let mut dirs_to_delete: HashSet<Dir> = HashSet::new();
        let mut dirs_to_delete_sequential: Vec<&Dir> = Vec::new();
        let mut file_set: HashSet<Dir> = HashSet::new();

        let dir0 = Dir {
            path: PathBuf::from(TEST_SUB_DIRS[0]),
        };
        let dir2 = Dir {
            path: PathBuf::from(TEST_SUB_DIRS[2]),
        };

        dirs_to_delete.insert(dir0.clone());
        dirs_to_delete.insert(dir2.clone());
        dirs_to_delete_sequential.push(&dir0);
        dirs_to_delete_sequential.push(&dir2);

        // `dir0` still holds `dir1`, so removing it fails and is reported;
        // `dir2` is removed all the same.
        let error = delete_files(dirs_to_delete.par_iter(), TEST_DIR)
            .expect_err("removing a directory that still holds a subdirectory fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );
        let error = delete_files_sequential(dirs_to_delete_sequential, TEST_DIR_SEQ)
            .expect_err("removing a directory that still holds a subdirectory fails");
        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );

        file_set.insert(Dir {
            path: PathBuf::from(TEST_SUB_DIRS[0]),
        });
        file_set.insert(Dir {
            path: PathBuf::from([TEST_SUB_DIRS[0], TEST_SUB_DIRS[1]].join("/")),
        });

        assert_eq!(
            get_all_files(TEST_DIR).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: file_set.clone(),
                symlinks: HashSet::new(),
            }
        );
        assert_eq!(
            get_all_files(TEST_DIR_SEQ).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: file_set,
                symlinks: HashSet::new(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_SEQ).unwrap();
    }
}

#[cfg(test)]
mod test_copy_files {
    use super::*;
    use std::process::Command;

    #[test]
    fn no_files() {
        const TEST_DIR: &str = "test_copy_files_no_files";
        const TEST_DIR_OUT: &str = "test_copy_files_no_files_out";

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_OUT).unwrap();

        copy_files(HashSet::<File>::new().par_iter(), TEST_DIR, TEST_DIR_OUT).unwrap();

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: HashSet::new(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }

    #[test]
    fn regular_files_dirs() {
        const TEST_DIR: &str = "src";
        const TEST_DIR_OUT: &str = "test_copy_files_regular_files_dirs_out";

        fs::create_dir_all(TEST_DIR_OUT).unwrap();

        copy_files(
            get_all_files(TEST_DIR).unwrap().dirs().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();
        copy_files(
            get_all_files(TEST_DIR).unwrap().files().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            get_all_files(TEST_DIR).unwrap()
        );

        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn insufficient_output_permissions() {
        const TEST_DIR: &str = "src";
        const TEST_DIR_OUT: &str = "test_copy_files_insufficient_output_permissions_out";
        const SUB_DIR: &str = "lumins";

        fs::create_dir_all([TEST_DIR_OUT, SUB_DIR].join("/")).unwrap();
        fs::File::create([TEST_DIR_OUT, "main.rs"].join("/")).unwrap();
        fs::File::create([TEST_DIR_OUT, "cli.yml"].join("/")).unwrap();
        fs::File::create([TEST_DIR_OUT, "lib.rs"].join("/")).unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR_OUT, SUB_DIR].join("/"))
            .output()
            .unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR_OUT, "main.rs"].join("/"))
            .output()
            .unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR_OUT, "cli.yml"].join("/"))
            .output()
            .unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR_OUT, "lib.rs"].join("/"))
            .output()
            .unwrap();

        copy_files(
            get_all_files(TEST_DIR).unwrap().dirs().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();
        // The destination files are read-only, so every copy into them fails and
        // the run reports it instead of exiting 0.
        let error = copy_files(
            get_all_files(TEST_DIR).unwrap().files().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .expect_err("copying over a file with no write permission fails");
        assert!(error.to_string().contains("failed to copy"), "{error}");

        let mut files = HashSet::new();
        files.insert(File {
            path: PathBuf::from("main.rs"),
            size: 0,
        });
        files.insert(File {
            path: PathBuf::from("cli.yml"),
            size: 0,
        });
        files.insert(File {
            path: PathBuf::from("lib.rs"),
            size: 0,
        });
        let mut dirs = HashSet::new();
        dirs.insert(Dir {
            path: PathBuf::from("lumins"),
        });

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            FileSets {
                files: files.clone(),
                dirs: dirs.clone(),
                symlinks: HashSet::new(),
            }
        );

        Command::new("rm")
            .arg("-rf")
            .arg(TEST_DIR_OUT)
            .output()
            .unwrap();
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn insufficient_input_permissions() {
        const TEST_DIR: &str = "test_copy_files_insufficient_input_permissions";
        const TEST_DIR_OUT: &str = "test_copy_files_insufficient_input_permissions_out";

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_OUT).unwrap();

        Command::new("cp")
            .args(["-r", "src/lumins", TEST_DIR])
            .output()
            .unwrap();
        Command::new("cp")
            .args(["src/main.rs", TEST_DIR])
            .output()
            .unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR, "lumins"].join("/"))
            .output()
            .unwrap();
        Command::new("chmod")
            .arg("000")
            .arg([TEST_DIR, "main.rs"].join("/"))
            .output()
            .unwrap();

        copy_files(
            get_all_files(TEST_DIR).unwrap().dirs().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();
        let error = copy_files(
            get_all_files(TEST_DIR).unwrap().files().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .expect_err("copying a file with no read permission fails");
        assert!(error.to_string().contains("failed to copy"), "{error}");

        let files = HashSet::new();
        let mut dirs = HashSet::new();
        dirs.insert(Dir {
            path: PathBuf::from("lumins"),
        });

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            FileSets {
                files: files.clone(),
                dirs: dirs.clone(),
                symlinks: HashSet::new(),
            }
        );

        Command::new("chmod")
            .arg("777")
            .arg([TEST_DIR, "lumins"].join("/"))
            .output()
            .unwrap();
        Command::new("rm").args(["-rf", TEST_DIR]).output().unwrap();
        Command::new("rm")
            .args(["-rf", TEST_DIR_OUT])
            .output()
            .unwrap();
    }

    #[test]
    #[cfg(target_family = "unix")]
    fn copy_symlink() {
        use std::os::unix::fs::symlink;
        const TEST_DIR: &str = "test_copy_files_copy_symlink";
        const TEST_DIR_OUT: &str = "test_copy_files_copy_symlink_out_seq";

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_OUT).unwrap();
        symlink("src/main.rs", [TEST_DIR, "file"].join("/")).unwrap();

        copy_files(
            get_all_files(TEST_DIR).unwrap().symlinks().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();

        let mut links_set = HashSet::new();
        links_set.insert(Symlink {
            path: PathBuf::from("file"),
            target: PathBuf::from("src/main.rs"),
        });

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: links_set.clone(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }

    #[test]
    #[cfg(target_family = "windows")]
    fn copy_symlink() {
        use std::env;
        use std::os::windows::fs as wfs;
        const TEST_DIR: &str = "test_copy_files_copy_symlink";
        const TEST_DIR_OUT: &str = "test_copy_files_copy_symlink_out_seq";
        let CURRENT_PATH: PathBuf = env::current_dir().unwrap();

        fs::create_dir_all(TEST_DIR).unwrap();
        fs::create_dir_all(TEST_DIR_OUT).unwrap();
        wfs::symlink_file("src/main.rs", [TEST_DIR, "file"].join("/")).unwrap();
        wfs::symlink_dir("src", [TEST_DIR, "dir"].join("/")).unwrap();

        copy_files(
            get_all_files(TEST_DIR).unwrap().symlinks().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
        )
        .unwrap();

        let mut links_set = HashSet::new();
        links_set.insert(Symlink {
            path: PathBuf::from("file"),
            target: PathBuf::from("src/main.rs"),
        });

        links_set.insert(Symlink {
            path: PathBuf::from("dir"),
            target: PathBuf::from("src/"),
        });

        assert_eq!(
            get_all_files(TEST_DIR_OUT).unwrap(),
            FileSets {
                files: HashSet::new(),
                dirs: HashSet::new(),
                symlinks: links_set.clone(),
            }
        );

        fs::remove_dir_all(TEST_DIR).unwrap();
        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }
}

#[cfg(test)]
mod test_compare_and_copy_files {
    use super::*;

    #[test]
    fn single_same() {
        const TEST_DIR: &str = "src";
        const TEST_DIR_OUT: &str = "test_compare_and_copy_files_single_same_out";

        fs::create_dir_all(TEST_DIR_OUT).unwrap();

        fs::copy(
            [TEST_DIR, "main.rs"].join("/"),
            [TEST_DIR_OUT, "main.rs"].join("/"),
        )
        .unwrap();

        let file_to_compare = File {
            path: PathBuf::from("main.rs"),
            size: fs::metadata([TEST_DIR, "main.rs"].join("/")).unwrap().len(),
        };

        let mut files_to_compare = HashSet::new();
        files_to_compare.insert(file_to_compare.clone());

        let mut flags = Flag::empty();
        flags |= Flag::SECURE;

        compare_and_copy_files(
            files_to_compare.clone().par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
            Flag::empty(),
        )
        .unwrap();

        compare_and_copy_files(files_to_compare.par_iter(), TEST_DIR, TEST_DIR_OUT, flags).unwrap();

        let actual = fs::read([TEST_DIR_OUT, "main.rs"].join("/")).unwrap();
        let expected = fs::read([TEST_DIR, "main.rs"].join("/")).unwrap();
        assert_eq!(actual, expected);

        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }

    #[test]
    fn single_different() {
        const TEST_DIR: &str = "src";
        const TEST_DIR_OUT: &str = "test_compare_and_copy_files_single_different_out";

        fs::create_dir_all(TEST_DIR_OUT).unwrap();
        fs::File::create([TEST_DIR_OUT, "main.rs"].join("/")).unwrap();

        let file_to_compare = File {
            path: PathBuf::from("main.rs"),
            size: fs::metadata([TEST_DIR, "main.rs"].join("/")).unwrap().len(),
        };
        let mut files_to_compare = HashSet::new();
        files_to_compare.insert(file_to_compare.clone());

        compare_and_copy_files(
            files_to_compare.par_iter(),
            TEST_DIR,
            TEST_DIR_OUT,
            Flag::empty(),
        )
        .unwrap();

        let actual = fs::read([TEST_DIR_OUT, "main.rs"].join("/")).unwrap();
        let expected = fs::read([TEST_DIR, "main.rs"].join("/")).unwrap();

        assert_eq!(actual, expected);

        fs::remove_dir_all(TEST_DIR_OUT).unwrap();
    }
}

#[cfg(test)]
mod test_hashing_and_failures {
    use super::*;

    /// A scratch directory under the system temp dir, removed when dropped
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join("lms_file_ops_tests").join(name);
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }

        fn location(&self) -> String {
            self.0.to_string_lossy().to_string()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn hash_file_matches_the_one_shot_hash() {
        let scratch = Scratch::new("hash_file_matches_the_one_shot_hash");
        let data: Vec<u8> = (0..20_000u32).map(|i| (i * 31 % 251) as u8).collect();
        fs::write(scratch.0.join("data.bin"), &data).unwrap();

        let file = File::from("data.bin", data.len() as u64);

        assert_eq!(
            hash_file(&file, &scratch.location()),
            Some(seahash::hash(&data))
        );
    }

    #[test]
    fn streaming_hash_ignores_chunk_boundaries() {
        let data: Vec<u8> = (0..1_000u32).map(|i| (i % 251) as u8).collect();

        let mut hasher = SeaHasher::new();
        for chunk in data.chunks(7) {
            hasher.write(chunk);
        }

        assert_eq!(hasher.finish(), seahash::hash(&data));
    }

    #[test]
    fn hash_file_is_none_for_a_missing_file() {
        let scratch = Scratch::new("hash_file_is_none_for_a_missing_file");
        let missing = File::from("missing.bin", 0);

        assert_eq!(hash_file(&missing, &scratch.location()), None);
    }

    #[test]
    fn delete_files_reports_a_delete_that_did_not_happen() {
        let scratch = Scratch::new("delete_files_reports_a_delete_that_did_not_happen");
        let missing = File::from("missing.txt", 0);

        let error = delete_files(vec![&missing].into_par_iter(), &scratch.location())
            .expect_err("deleting a missing file must not be reported as success");

        assert!(
            error.to_string().contains("failed to delete 1 entries"),
            "{error}"
        );
    }

    #[test]
    fn copy_files_reports_a_copy_that_did_not_happen() {
        let src = Scratch::new("copy_files_reports_a_copy_that_did_not_happen");
        let dest = Scratch::new("copy_files_reports_a_copy_that_did_not_happen_out");
        let missing = File::from("missing.txt", 0);

        let error = copy_files(
            vec![&missing].into_par_iter(),
            &src.location(),
            &dest.location(),
        )
        .expect_err("copying a missing file must not be reported as success");

        assert!(
            error.to_string().contains("failed to copy 1 entries"),
            "{error}"
        );
    }
}

#[cfg(test)]
mod test_replacing_stale_entries {
    use super::*;

    /// A scratch directory under the system temp dir, removed when dropped
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join("lms_file_ops_tests").join(name);
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).unwrap();
            Scratch(path)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A file that changed into a directory has to replace it, not fail on it
    #[test]
    fn dir_copy_replaces_a_file() {
        let scratch = Scratch::new("dir_copy_replaces_a_file");
        let dest = scratch.0.join("entry");
        fs::write(&dest, b"stale file").unwrap();

        Dir::from("entry").copy(&dest, &dest).unwrap();

        assert!(fs::metadata(&dest).unwrap().is_dir());
    }

    /// ... and the other way round
    #[test]
    fn file_copy_replaces_a_dir() {
        let src = Scratch::new("file_copy_replaces_a_dir");
        let dest = Scratch::new("file_copy_replaces_a_dir_out");
        fs::write(src.0.join("entry"), b"fresh file").unwrap();
        fs::create_dir_all(dest.0.join("entry")).unwrap();

        File::from("entry", 10)
            .copy(&src.0.join("entry"), &dest.0.join("entry"))
            .unwrap();

        assert_eq!(
            fs::read_to_string(dest.0.join("entry")).unwrap(),
            "fresh file"
        );
    }

    /// A link whose target changed kept pointing at the old target before
    #[test]
    #[cfg(target_family = "unix")]
    fn symlink_copy_retargets_an_existing_link() {
        let scratch = Scratch::new("symlink_copy_retargets_an_existing_link");
        fs::write(scratch.0.join("old"), b"").unwrap();
        fs::write(scratch.0.join("new"), b"").unwrap();
        let dest = scratch.0.join("entry");
        std::os::unix::fs::symlink(scratch.0.join("old"), &dest).unwrap();

        Symlink::from("entry", &scratch.0.join("new").to_string_lossy())
            .copy(&dest, &dest)
            .unwrap();

        assert_eq!(
            fs::read_link(&dest).unwrap(),
            scratch.0.join("new").canonicalize().unwrap()
        );
    }
}
