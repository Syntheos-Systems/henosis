//! Compatibility executable for automation that still invokes `agent-forge`.

/// Run the shared Crucible command-line adapter under the legacy executable name.
fn main() {
    crucible::cli::run("agent-forge");
}
