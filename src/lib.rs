//! LuminS (lms) is a fast and reliable alternative to rsync for synchronizing local files
//!
//! ```text
//! LuminS 0.4.0
//! Luminous Synchronize - A fast and reliable multithreaded alternative to rsync for synchronizing local files
//!
//! Usage: lms [COMMAND]
//!
//! Commands:
//!   cp    Multithreaded directory copy
//!   rm    Multithreaded directory remove
//!   sync  Multithreaded directory synchronization [alias: s]
//!   help  Print this message or the help of the given subcommand(s)
//!
//! Options:
//!   -h, --help     Print help
//!   -V, --version  Print version
//! ```

mod lumins;
pub use lumins::{core, file_ops, parse, progress};
