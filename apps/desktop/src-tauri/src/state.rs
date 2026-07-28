//! Native session and cache state that keeps Rift tokens outside the webview.

use std::fs;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;
use serde::de::DeserializeOwned;
use tauri::{AppHandle, Manager};

use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, RoomDirectorySnapshot,
    RoomStatus,
};
use crate::rift::RiftSession;

/// Process-local secret state for the current Rift login.
pub struct AppState {
    /// Session tokens protected by a synchronous lock never held across await.
    session: Mutex<Option<RiftSession>>,
}

/// Native session access and mutation operations.
impl AppState {
    /// Create an application state without an authenticated Rift session.
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    /// Clone the current session without exposing it to serialization.
    pub fn session(&self) -> Result<Option<RiftSession>, CommandError> {
        self.session.lock().map(|guard| guard.clone()).map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not access the native session.",
            )
        })
    }

    /// Replace the current native Rift session after successful authentication.
    pub fn set_session(&self, session: RiftSession) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not update the native session.",
            )
        })?;
        *guard = Some(session);
        Ok(())
    }

    /// Remove all process-local Rift token state.
    pub fn clear_session(&self) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not clear the native session.",
            )
        })?;
        *guard = None;
        Ok(())
    }
}

/// Resolve one application-data file without accepting caller-controlled paths.
fn app_data_file(app: &AppHandle, filename: &str) -> Result<PathBuf, CommandError> {
    let directory = app.path().app_data_dir().map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not locate its application data directory.",
        )
    })?;
    fs::create_dir_all(&directory).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not prepare its application data directory.",
        )
    })?;
    Ok(directory.join(filename))
}

/// Read one optional native JSON record.
fn read_json<T: DeserializeOwned>(
    app: &AppHandle,
    filename: &str,
) -> Result<Option<T>, CommandError> {
    let path = app_data_file(app, filename)?;
    let content = match fs::read(&path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not read its saved connection data.",
            ));
        }
    };
    serde_json::from_slice(&content).map(Some).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis found invalid saved connection data.",
        )
    })
}

/// Write one native JSON record without placing it in browser storage.
fn write_json<T: Serialize>(
    app: &AppHandle,
    filename: &str,
    value: &T,
) -> Result<(), CommandError> {
    let path = app_data_file(app, filename)?;
    let content = serde_json::to_vec_pretty(value).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not encode its connection data.",
        )
    })?;
    fs::write(path, content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not save its connection data.",
        )
    })
}

/// Load the saved non-secret Rift endpoint and username.
pub fn read_profile(app: &AppHandle) -> Result<Option<ConnectionProfile>, CommandError> {
    read_json(app, "rift-profile.json")
}

/// Save only non-secret Rift profile fields.
pub fn write_profile(app: &AppHandle, profile: &ConnectionProfile) -> Result<(), CommandError> {
    write_json(app, "rift-profile.json", profile)
}

/// Load cached rooms and force their provenance and connectivity flags offline.
pub fn read_room_cache(app: &AppHandle) -> Result<Option<RoomDirectorySnapshot>, CommandError> {
    read_json::<RoomDirectorySnapshot>(app, "rift-room-cache.json").map(|cached| {
        cached.map(|mut snapshot| {
            snapshot.source = DirectorySource::Cached;
            snapshot.connected = false;
            snapshot.rooms.iter_mut().for_each(|room| {
                if !matches!(room.status, RoomStatus::Paused) {
                    room.status = RoomStatus::Disconnected;
                }
            });
            snapshot
        })
    })
}

/// Save a sanitized room snapshot that contains no Rift tokens.
pub fn write_room_cache(
    app: &AppHandle,
    snapshot: &RoomDirectorySnapshot,
) -> Result<(), CommandError> {
    write_json(app, "rift-room-cache.json", snapshot)
}
