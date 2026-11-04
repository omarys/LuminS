//! Keeps track of LuminS' progress

use std::sync::LazyLock;

use indicatif::{ProgressBar, ProgressStyle};

/// Provides a bar that shows the number of files
/// copied, synchronized, or deleted, out of the total number of files
pub static PROGRESS_BAR: LazyLock<ProgressBar> = LazyLock::new(|| {
    let progress_bar = ProgressBar::new(0);
    progress_bar.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] [{bar:40.green/blue}] {pos}/{len} ({eta})",
        )
        .expect("hard-coded progress bar template is valid"),
    );
    progress_bar
});

/// Initializes PROGRESS_BAR with `length`
/// # Arguments
/// * `length`: Length fo the bar to set
pub fn progress_init(length: u64) {
    PROGRESS_BAR.set_length(length);
    PROGRESS_BAR.set_position(0);
}
