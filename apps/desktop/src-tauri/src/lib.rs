//! Henosis Tauri application assembly and native command registration.

/// Sanitized commands exposed to the React webview.
mod commands;
/// Native Rift WebSocket ownership and event translation.
mod gateway;
/// Serialized native-to-webview contracts.
mod model;
/// Bounded latest and forward-page reconciliation for the open room.
mod reconcile;
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
            commands::get_my_agents,
            commands::create_my_agent,
            commands::claim_agent,
            commands::get_agent_capabilities,
            commands::get_room_permissions,
            commands::get_room_agent_roster,
            commands::apply_room_agent_roster,
            commands::get_room_bridge_status,
            commands::pause_room_bridge,
            commands::resume_room_bridge,
            commands::reconcile_room_bridge,
            commands::disconnect_rift,
            commands::open_room,
            commands::load_older_messages,
            commands::send_room_message,
            commands::edit_room_message,
            commands::delete_room_message,
            commands::select_and_upload_room_attachments,
            commands::send_room_typing,
            commands::mark_room_read,
            commands::close_room,
        ])
        .run(tauri::generate_context!())
        .expect("Henosis failed to start");
}
