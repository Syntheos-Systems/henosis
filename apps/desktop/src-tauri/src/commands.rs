//! Tauri commands that expose sanitized Henosis operations to the webview.

use tauri::{AppHandle, State};

use crate::model::{
    BootstrapResult, CommandError, CommandErrorKind, RiftConnectionInput, RoomDirectorySnapshot,
};
use crate::rift::{self, RiftError};
use crate::state::{AppState, read_profile, read_room_cache, write_profile, write_room_cache};

/// Build a setup result from non-secret profile and cached-room state.
fn setup_bootstrap(app: &AppHandle) -> Result<BootstrapResult, CommandError> {
    Ok(BootstrapResult {
        saved_profile: read_profile(app)?,
        directory: read_room_cache(app)?,
        requires_authentication: true,
    })
}

/// Load live native state when possible and otherwise expose honest cached setup data.
#[tauri::command]
pub async fn bootstrap(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<BootstrapResult, CommandError> {
    let Some(session) = state.session()? else {
        return setup_bootstrap(&app);
    };

    match rift::fetch_room_directory(&session).await {
        Ok(directory) => {
            write_room_cache(&app, &directory)?;
            Ok(BootstrapResult {
                saved_profile: Some(rift::profile_for(&session)),
                directory: Some(directory),
                requires_authentication: false,
            })
        }
        Err(RiftError::Authentication) => {
            state.clear_session()?;
            setup_bootstrap(&app)
        }
        Err(RiftError::Network(_)) => Ok(BootstrapResult {
            saved_profile: read_profile(&app)?,
            directory: read_room_cache(&app)?,
            requires_authentication: false,
        }),
        Err(error) => Err(error.into()),
    }
}

/// Authenticate to Rift, cache sanitized room data, and retain tokens natively.
#[tauri::command]
pub async fn connect_rift(
    app: AppHandle,
    state: State<'_, AppState>,
    input: RiftConnectionInput,
) -> Result<RoomDirectorySnapshot, CommandError> {
    let session = rift::login(&input).await?;
    let directory = rift::fetch_room_directory(&session).await?;
    write_profile(&app, &rift::profile_for(&session))?;
    write_room_cache(&app, &directory)?;
    state.set_session(session)?;
    Ok(directory)
}

/// Refresh room summaries through the authenticated native session.
#[tauri::command]
pub async fn get_room_directory(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<RoomDirectorySnapshot, CommandError> {
    let session = state.session()?.ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::ConnectionRequired,
            "Connect to Rift before refreshing rooms.",
        )
    })?;

    match rift::fetch_room_directory(&session).await {
        Ok(directory) => {
            write_room_cache(&app, &directory)?;
            Ok(directory)
        }
        Err(RiftError::Network(_)) => read_room_cache(&app)?.ok_or_else(|| {
            CommandError::new(
                CommandErrorKind::Network,
                "Henosis could not reach Rift and has no cached room directory.",
            )
        }),
        Err(RiftError::Authentication) => {
            state.clear_session()?;
            Err(CommandError::new(
                CommandErrorKind::Authentication,
                "Your Rift session expired. Sign in again to continue.",
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Clear native Rift tokens and end the remote refresh session when reachable.
#[tauri::command]
pub async fn disconnect_rift(state: State<'_, AppState>) -> Result<(), CommandError> {
    let session = state.session()?;
    state.clear_session()?;
    if let Some(session) = session {
        rift::logout(&session).await;
    }
    Ok(())
}
