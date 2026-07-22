//! Authenticated attachment ingestion and pending-upload cleanup for Rift.

use axum::{
    Json,
    extract::{Multipart, State},
};
use chrono::{Duration, Utc};
use serde::Serialize;
use uuid::Uuid;

use crate::auth::middleware::AuthUser;
use crate::config::Config;
use crate::error::AppError;

/// Metadata returned after an attachment is staged for a message.
#[derive(Debug, Serialize)]
pub struct UploadedFile {
    /// Opaque identifier used to attach the staged file to a message.
    pub upload_id: Uuid,
    /// Original user-visible filename retained as metadata only.
    pub filename: String,
    /// Same-origin URL for the opaque stored object.
    pub url: String,
    /// Caller-declared media type retained as metadata only.
    pub content_type: Option<String>,
    /// Stored object size in bytes.
    pub size_bytes: i64,
}

/// In-memory map of staged files that are not yet linked to a message.
pub type PendingUploads = std::sync::Arc<dashmap::DashMap<Uuid, PendingUpload>>;

/// Server-owned metadata for one staged attachment.
#[derive(Debug, Clone)]
pub struct PendingUpload {
    /// User that staged the file.
    pub uploader_id: Uuid,
    /// Original filename retained for message metadata.
    pub filename: String,
    /// Opaque filename used on disk.
    pub stored_filename: String,
    /// Opaque same-origin URL returned to clients.
    pub url: String,
    /// Caller-declared media type retained for message metadata.
    pub content_type: Option<String>,
    /// Stored object size in bytes.
    pub size_bytes: i64,
    /// Timestamp used to expire unlinked uploads.
    pub created_at: chrono::DateTime<Utc>,
}

/// Stage one or more authenticated attachment uploads and return their metadata.
pub async fn upload_files(
    State(config): State<Config>,
    State(pending): State<PendingUploads>,
    auth: AuthUser,
    mut multipart: Multipart,
) -> Result<Json<Vec<UploadedFile>>, AppError> {
    let mut results = Vec::new();

    cleanup_stale_uploads(&config, &pending).await;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::BadRequest(format!("Multipart error: {e}")))?
    {
        let original_filename = field
            .file_name()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "unnamed".into());

        let content_type = field.content_type().map(|s| s.to_string());

        let data = field
            .bytes()
            .await
            .map_err(|e| AppError::BadRequest(format!("Failed to read file: {e}")))?;

        if data.len() > config.max_upload_bytes {
            return Err(AppError::BadRequest(format!(
                "File too large (max {} bytes)",
                config.max_upload_bytes
            )));
        }

        if data.is_empty() {
            continue;
        }

        let upload_id = Uuid::new_v4();
        let stored_filename = stored_attachment_filename(upload_id);

        let file_path = std::path::Path::new(&config.upload_dir).join(&stored_filename);
        tokio::fs::write(&file_path, &data)
            .await
            .map_err(|e| AppError::Internal(format!("Failed to save file: {e}")))?;

        let url = format!("/uploads/{stored_filename}");
        let size_bytes = data.len() as i64;

        // Store pending upload metadata
        pending.insert(
            upload_id,
            PendingUpload {
                uploader_id: auth.user_id,
                filename: original_filename.clone(),
                stored_filename: stored_filename.clone(),
                url: url.clone(),
                content_type: content_type.clone(),
                size_bytes,
                created_at: Utc::now(),
            },
        );

        results.push(UploadedFile {
            upload_id,
            filename: original_filename,
            url,
            content_type,
            size_bytes,
        });
    }

    if results.is_empty() {
        return Err(AppError::BadRequest("No files uploaded".into()));
    }

    Ok(Json(results))
}

/// Delete the opaque disk object for a staged upload, ignoring an already-absent file.
pub async fn delete_pending_upload_file(config: &Config, pending_upload: &PendingUpload) {
    let file_path = std::path::Path::new(&config.upload_dir).join(&pending_upload.stored_filename);
    if let Err(err) = tokio::fs::remove_file(file_path).await
        && err.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            "Failed to delete stale upload {}: {err}",
            pending_upload.stored_filename
        );
    }
}

/// Remove staged uploads that have remained unlinked for more than 24 hours.
async fn cleanup_stale_uploads(config: &Config, pending: &PendingUploads) {
    let cutoff = Utc::now() - Duration::hours(24);
    let stale_ids: Vec<Uuid> = pending
        .iter()
        .filter_map(|entry| (entry.value().created_at < cutoff).then_some(*entry.key()))
        .collect();

    for upload_id in stale_ids {
        if let Some((_, pending_upload)) = pending.remove(&upload_id) {
            delete_pending_upload_file(config, &pending_upload).await;
        }
    }
}

/// Derive an extension-free disk name so user input cannot control browser MIME inference.
fn stored_attachment_filename(upload_id: Uuid) -> String {
    upload_id.to_string()
}

#[cfg(test)]
/// Exercises opaque storage naming for potentially active attachment types.
mod tests {
    use super::stored_attachment_filename;
    use uuid::Uuid;

    /// Hostile original extensions cannot appear because storage names depend only on UUIDs.
    #[test]
    fn stored_attachment_names_are_extension_free() {
        let upload_id =
            Uuid::parse_str("00000000-0000-0000-0000-000000000123").expect("static UUID is valid");
        let stored = stored_attachment_filename(upload_id);
        assert_eq!(stored, "00000000-0000-0000-0000-000000000123");
        assert!(!stored.contains('.'));
        assert!(!stored.contains("html"));
        assert!(!stored.contains("svg"));
    }
}
