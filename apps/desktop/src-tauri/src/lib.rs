//! Henosis Tauri application assembly and native command registration.

/// Sanitized commands exposed to the React webview.
mod commands;
/// Serialized native-to-webview contracts.
mod model;
/// Rift HTTP transport and room aggregation.
mod rift;
/// Process-local session and native cache state.
mod state;

use state::AppState;

/// Construct and run the Henosis desktop application.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::bootstrap,
            commands::connect_rift,
            commands::get_room_directory,
            commands::disconnect_rift,
        ])
        .run(tauri::generate_context!())
        .expect("Henosis failed to start");
}
