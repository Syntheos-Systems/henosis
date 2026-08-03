//! Native session and cache state that keeps Rift tokens outside the webview.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};

use crate::gateway::RiftGateway;
use crate::model::{
    CommandError, CommandErrorKind, ConnectionProfile, DirectorySource, RoomDirectorySnapshot,
    RoomStatus,
};
use crate::rift::AuthenticatedRiftClient;

/// Fixed application-data filename for non-secret per-room read cursors.
const ROOM_READ_MARKERS_FILENAME: &str = "rift-room-read-markers.json";

/// Maximum number of recently updated room cursors retained on disk.
const MAX_ROOM_READ_MARKERS: usize = 500;

/// One non-secret read cursor scoped to an origin, human, and room tuple.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RoomReadMarker {
    /// Normalized Rift HTTP or HTTPS origin without credentials or a path.
    pub rift_origin: String,
    /// Signed-in Rift human identifier.
    pub user_id: String,
    /// Rift room identifier.
    pub room_id: String,
    /// Most recent loaded message explicitly marked read.
    pub last_read_message_id: String,
    /// UTC timestamp used for deterministic retention ordering.
    pub read_at: DateTime<Utc>,
}

/// Process-local secret state for the current Rift login.
pub struct AppState {
    /// Authenticated client and its sole gateway owner replaced under one lock.
    session: Mutex<Option<ActiveRiftSession>>,
}

/// One authenticated native session and its optional live room gateway.
struct ActiveRiftSession {
    /// Opaque shared native HTTP and token-rotation client.
    client: AuthenticatedRiftClient,
    /// Sole cancellable gateway for the currently open room.
    gateway: Option<RiftGateway>,
}

/// Native session access and mutation operations.
impl AppState {
    /// Create an application state without an authenticated Rift session.
    pub fn new() -> Self {
        Self {
            session: Mutex::new(None),
        }
    }

    /// Clone the current native client handle without cloning independent tokens.
    pub fn session(&self) -> Result<Option<AuthenticatedRiftClient>, CommandError> {
        self.session
            .lock()
            .map(|guard| guard.as_ref().map(|active| active.client.clone()))
            .map_err(|_| {
                CommandError::new(
                    CommandErrorKind::Storage,
                    "Henosis could not access the native session.",
                )
            })
    }

    /// Replace the current client and invalidate any previously cloned handle.
    pub fn set_session(&self, session: AuthenticatedRiftClient) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not update the native session.",
            )
        })?;
        if let Some(mut previous) = guard.replace(ActiveRiftSession {
            client: session,
            gateway: None,
        }) {
            if let Some(gateway) = previous.gateway.take() {
                gateway.cancel();
            }
            previous.client.invalidate();
        }
        Ok(())
    }

    /// Atomically install one gateway only when its client is still current.
    pub(crate) fn replace_gateway(
        &self,
        session: &AuthenticatedRiftClient,
        gateway: RiftGateway,
    ) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not update the native gateway.",
            )
        })?;
        let Some(active) = guard.as_mut() else {
            return Err(CommandError::new(
                CommandErrorKind::ConnectionRequired,
                "Connect to Rift before opening a room.",
            ));
        };
        if !active.client.same_session(session) {
            return Err(CommandError::new(
                CommandErrorKind::ConnectionRequired,
                "The Rift session changed while the room was opening. Open the room again.",
            ));
        }
        if let Some(previous) = active.gateway.replace(gateway) {
            previous.cancel();
        }
        Ok(())
    }

    /// Cancel and remove the current room gateway without logging out of Rift.
    pub(crate) fn clear_gateway(&self) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not clear the native gateway.",
            )
        })?;
        if let Some(gateway) = guard.as_mut().and_then(|active| active.gateway.take()) {
            gateway.cancel();
        }
        Ok(())
    }

    /// Invalidate and remove all process-local Rift token state.
    pub fn clear_session(&self) -> Result<(), CommandError> {
        let mut guard = self.session.lock().map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not clear the native session.",
            )
        })?;
        if let Some(mut session) = guard.take() {
            if let Some(gateway) = session.gateway.take() {
                gateway.cancel();
            }
            session.client.invalidate();
        }
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

/// Normalize and validate the Rift origin stored in a read-marker key.
fn normalize_marker_origin(origin: &str) -> Result<String, CommandError> {
    crate::rift::normalize_endpoint(origin)
        .map(|url| url.origin().ascii_serialization())
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis found an invalid Rift origin in room history state. Reconnect to Rift and try again.",
            )
        })
}

/// Deduplicate, normalize, deterministically order, and bound read markers.
fn canonicalize_room_read_markers(
    markers: &[RoomReadMarker],
) -> Result<Vec<RoomReadMarker>, CommandError> {
    let mut keyed = BTreeMap::<(String, String, String), RoomReadMarker>::new();
    for source in markers {
        if source.user_id.is_empty()
            || source.room_id.is_empty()
            || source.last_read_message_id.is_empty()
        {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis found an incomplete room history marker. Reopen the room and mark it read again.",
            ));
        }
        let mut marker = source.clone();
        marker.rift_origin = normalize_marker_origin(&marker.rift_origin)?;
        let key = (
            marker.rift_origin.clone(),
            marker.user_id.clone(),
            marker.room_id.clone(),
        );
        match keyed.get_mut(&key) {
            Some(current)
                if marker.read_at > current.read_at
                    || (marker.read_at == current.read_at
                        && marker.last_read_message_id > current.last_read_message_id) =>
            {
                *current = marker;
            }
            Some(_) => {}
            None => {
                keyed.insert(key, marker);
            }
        }
    }

    let mut canonical = keyed.into_values().collect::<Vec<_>>();
    canonical.sort_by(|left, right| {
        right
            .read_at
            .cmp(&left.read_at)
            .then_with(|| left.rift_origin.cmp(&right.rift_origin))
            .then_with(|| left.user_id.cmp(&right.user_id))
            .then_with(|| left.room_id.cmp(&right.room_id))
    });
    canonical.truncate(MAX_ROOM_READ_MARKERS);
    Ok(canonical)
}

/// Read and validate markers from one fixed native path.
fn read_room_read_markers_at(path: &Path) -> Result<Vec<RoomReadMarker>, CommandError> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(_) => {
            return Err(CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not read room history state. Check application-data permissions and try again.",
            ));
        }
    };
    let markers = serde_json::from_slice::<Vec<RoomReadMarker>>(&content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            format!(
                "Henosis found invalid room history state. Move {ROOM_READ_MARKERS_FILENAME} aside and reopen Henosis."
            ),
        )
    })?;
    canonicalize_room_read_markers(&markers)
}

/// Atomically replace one marker file through a synced temporary sibling.
fn write_room_read_markers_at(path: &Path, markers: &[RoomReadMarker]) -> Result<(), CommandError> {
    let parent = path.parent().ok_or_else(|| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not locate room history storage. Reopen the application and try again.",
        )
    })?;
    fs::create_dir_all(parent).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not prepare room history storage. Check application-data permissions and try again.",
        )
    })?;
    let canonical = canonicalize_room_read_markers(markers)?;
    let content = serde_json::to_vec_pretty(&canonical).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not encode room history state. Reopen the room and try again.",
        )
    })?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".rift-room-read-markers-")
        .suffix(".tmp")
        .tempfile_in(parent)
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis could not create temporary room history storage. Check free space and application-data permissions, then try again.",
            )
        })?;
    temporary.write_all(&content).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not write room history state. Check free space and try again.",
        )
    })?;
    temporary.as_file().sync_all().map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not flush room history state. Check the application-data filesystem and try again.",
        )
    })?;
    temporary.persist(path).map_err(|_| {
        CommandError::new(
            CommandErrorKind::Storage,
            "Henosis could not replace room history state. Check application-data permissions and try again.",
        )
    })?;
    #[cfg(unix)]
    fs::File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| {
            CommandError::new(
                CommandErrorKind::Storage,
                "Henosis saved room history but could not flush its directory. Check the application-data filesystem before retrying.",
            )
        })?;
    Ok(())
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

/// Load bounded non-secret room read markers from native application data.
pub fn read_room_read_markers(app: &AppHandle) -> Result<Vec<RoomReadMarker>, CommandError> {
    read_room_read_markers_at(&app_data_file(app, ROOM_READ_MARKERS_FILENAME)?)
}

/// Canonicalize and atomically save bounded non-secret room read markers.
pub fn write_room_read_markers(
    app: &AppHandle,
    markers: &[RoomReadMarker],
) -> Result<(), CommandError> {
    write_room_read_markers_at(&app_data_file(app, ROOM_READ_MARKERS_FILENAME)?, markers)
}

#[cfg(test)]
/// Exercises bounded, identity-scoped native room read-marker persistence.
mod tests {
    use std::collections::BTreeSet;

    use chrono::{Duration, TimeZone, Utc};

    use super::*;

    /// Construct one marker at a stable time offset for ordering assertions.
    fn marker(
        rift_origin: &str,
        user_id: &str,
        room_id: &str,
        message_id: &str,
        seconds: i64,
    ) -> RoomReadMarker {
        RoomReadMarker {
            rift_origin: rift_origin.into(),
            user_id: user_id.into(),
            room_id: room_id.into(),
            last_read_message_id: message_id.into(),
            read_at: Utc
                .timestamp_opt(1_700_000_000, 0)
                .single()
                .expect("base marker time must exist")
                + Duration::seconds(seconds),
        }
    }

    /// Resolve a marker file inside an isolated temporary sibling directory.
    fn marker_test_path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("temporary marker directory must exist");
        let path = directory.path().join(ROOM_READ_MARKERS_FILENAME);
        (directory, path)
    }

    /// Compound scope keeps accounts and origins separate while newest duplicates win.
    #[test]
    fn room_read_markers_key_by_origin_user_and_room() {
        let markers = canonicalize_room_read_markers(&[
            marker(
                "https://rift.example/",
                "user-1",
                "room-1",
                "message-old",
                1,
            ),
            marker("https://rift.example", "user-1", "room-1", "message-new", 2),
            marker(
                "https://rift.example",
                "user-2",
                "room-1",
                "message-other-user",
                3,
            ),
            marker(
                "https://other.example",
                "user-1",
                "room-1",
                "message-other-origin",
                4,
            ),
        ])
        .expect("valid markers must canonicalize");

        assert_eq!(markers.len(), 3);
        assert_eq!(markers[0].last_read_message_id, "message-other-origin");
        assert_eq!(markers[1].last_read_message_id, "message-other-user");
        assert_eq!(markers[2].last_read_message_id, "message-new");
        assert_eq!(markers[2].rift_origin, "https://rift.example");
    }

    /// Only the five hundred most recently updated room keys survive.
    #[test]
    fn room_read_markers_keep_five_hundred_most_recent_rooms() {
        let markers = (0..502)
            .map(|index| {
                marker(
                    "https://rift.example",
                    "user-1",
                    &format!("room-{index}"),
                    &format!("message-{index}"),
                    index,
                )
            })
            .collect::<Vec<_>>();

        let bounded =
            canonicalize_room_read_markers(&markers).expect("generated markers must canonicalize");
        assert_eq!(bounded.len(), MAX_ROOM_READ_MARKERS);
        assert_eq!(bounded.first().expect("newest marker").room_id, "room-501");
        assert_eq!(
            bounded.last().expect("oldest retained marker").room_id,
            "room-2"
        );
        assert!(!bounded.iter().any(|marker| marker.room_id == "room-1"));
    }

    /// Repeated writes replace the file and persist only the approved marker fields.
    #[test]
    fn room_read_markers_replace_through_a_temporary_sibling() {
        let (_directory, path) = marker_test_path();
        write_room_read_markers_at(
            &path,
            &[marker(
                "https://rift.example",
                "user-1",
                "room-1",
                "message-old",
                1,
            )],
        )
        .expect("initial marker file must be written");
        write_room_read_markers_at(
            &path,
            &[marker(
                "https://rift.example",
                "user-1",
                "room-1",
                "message-new",
                2,
            )],
        )
        .expect("marker file must be replaced");

        let stored = read_room_read_markers_at(&path).expect("markers must be readable");
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].last_read_message_id, "message-new");
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).expect("marker JSON must remain on disk"))
                .expect("marker JSON must parse");
        let keys = value[0]
            .as_object()
            .expect("marker must be an object")
            .keys()
            .map(String::as_str)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            keys,
            BTreeSet::from([
                "lastReadMessageId",
                "readAt",
                "riftOrigin",
                "roomId",
                "userId",
            ])
        );
        assert_eq!(
            fs::read_dir(path.parent().expect("marker parent must exist"))
                .expect("marker parent must be readable")
                .count(),
            1
        );
    }

    /// Invalid saved JSON produces actionable storage guidance.
    #[test]
    fn room_read_markers_report_invalid_saved_state() {
        let (_directory, path) = marker_test_path();
        fs::write(&path, b"{not-json").expect("invalid fixture must be written");

        let error =
            read_room_read_markers_at(&path).expect_err("invalid marker JSON must be rejected");
        assert!(matches!(error.kind, CommandErrorKind::Storage));
        assert!(error.message.contains(ROOM_READ_MARKERS_FILENAME));
        assert!(error.message.contains("Move"));
    }
}
