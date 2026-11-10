//! Keeps track of LuminS' progress

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::LazyLock;

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Whether a progress bar may be drawn at all
///
/// A bar is drawn with carriage returns and repaints; that is noise for a
/// program reading the output, and it is worthless when nobody is watching. It
/// is switched off before the first draw, so no repaint ever reaches the
/// terminal in a machine-readable mode.
static ENABLED: AtomicBool = AtomicBool::new(true);

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

/// Turns the progress bar off, for good
///
/// Called once, before any work starts. `PROGRESS_BAR` is hidden rather than
/// skipped so the rest of the code can keep calling `inc`/`println` without
/// checking: a hidden draw target accepts and discards every repaint.
pub fn disable() {
    ENABLED.store(false, Ordering::Relaxed);
    PROGRESS_BAR.set_draw_target(ProgressDrawTarget::hidden());
}

/// Whether the progress bar is still drawing
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Initializes PROGRESS_BAR with `length`
/// # Arguments
/// * `length`: Length fo the bar to set
pub fn progress_init(length: u64) {
    PROGRESS_BAR.set_length(length);
    PROGRESS_BAR.set_position(0);
}
