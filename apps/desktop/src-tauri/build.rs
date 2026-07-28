//! Build-time integration between Cargo and the Tauri application manifest.

/// Generate Tauri build metadata for the Henosis desktop application.
fn main() {
    tauri_build::build();
}
