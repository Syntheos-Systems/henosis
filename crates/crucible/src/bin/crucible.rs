//! Primary executable entry point for the Crucible quality-gate service.

/// Run the shared Crucible command-line adapter under the public product name.
fn main() {
    crucible::cli::run("crucible");
}
