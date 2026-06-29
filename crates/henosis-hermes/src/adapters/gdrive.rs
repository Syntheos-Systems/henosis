//! Google Drive adapters: list, upload, download, get_metadata.
//!
//! All tools authenticate via Google OAuth tokens resolved from credd under
//! the `google` provider tag. The upload adapter builds a hand-crafted
//! `multipart/related` body to avoid pulling in reqwest's multipart feature.

use async_trait::async_trait;
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Value};
use tracing::warn;

use crate::adapters::common::{
    build_http, credd_error_to_response, send_with_retry, send_with_retry_bytes, truncate,
};
use crate::tool::{
    err, error_response, InvokeContext, InvokeRequest, InvokeResponse, Tool, ToolSchema,
};

/// Tool ID for the list adapter.
const TOOL_ID: &str = "gdrive.list";
/// credd provider tag for all Google Drive tools.
const PROVIDER: &str = "google";
/// Drive v3 files list endpoint.
const DRIVE_LIST_URL: &str = "https://www.googleapis.com/drive/v3/files";
/// Drive v3 files base URL (used for metadata and media fetch).
const DRIVE_FILES_URL: &str = "https://www.googleapis.com/drive/v3/files";
/// Drive v3 multipart upload endpoint.
const DRIVE_UPLOAD_URL: &str = "https://www.googleapis.com/upload/drive/v3/files";

/// List files in Google Drive for the authenticated tenant.
pub struct GDriveListTool;

#[async_trait]
impl Tool for GDriveListTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: TOOL_ID.to_string(),
            name: "List Drive Files".to_string(),
            description: "List files in Google Drive for the authenticated tenant.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Drive search query string" },
                    "page_size": { "type": "integer", "description": "Max results to return (default 25, max 1000)" },
                    "folder_id": { "type": "string", "description": "Limit to files in this folder" },
                    "page_token": { "type": "string", "description": "Token from a previous response for pagination" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "files": { "type": "array" },
                    "next_page_token": { "type": "string" }
                }
            }),
            category: "storage".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response(TOOL_ID, "bad_request", "tenant_id is required", None),
        };

        let args = match parse_args(&req.args) {
            Ok(a) => a,
            Err(msg) => return error_response(TOOL_ID, "bad_request", msg, None),
        };

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response(TOOL_ID, &e),
        };

        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response(TOOL_ID, "internal_error", e.to_string(), None),
        };

        // Drive's `q` syntax: combine the user's `query` with a folder filter.
        let q = match (args.query.as_deref(), args.folder_id.as_deref()) {
            (Some(q), Some(folder)) => Some(format!("{q} and '{folder}' in parents")),
            (Some(q), None) => Some(q.to_string()),
            (None, Some(folder)) => Some(format!("'{folder}' in parents")),
            (None, None) => None,
        };
        let page_size = args.page_size.unwrap_or(25).clamp(1, 1000);

        let mut request = http
            .get(DRIVE_LIST_URL)
            .bearer_auth(&token)
            .query(&[
                ("pageSize", page_size.to_string()),
                (
                    "fields",
                    "nextPageToken,files(id,name,mimeType,modifiedTime,size)".into(),
                ),
            ]);
        if let Some(q) = q {
            request = request.query(&[("q", q)]);
        }
        if let Some(token) = &args.page_token {
            request = request.query(&[("pageToken", token.clone())]);
        }

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => {
                return InvokeResponse {
                    tool_id: TOOL_ID.into(),
                    success: false,
                    result: None,
                    error: Some(err(
                        "gdrive_unreachable",
                        format!("drive api request failed: {e}"),
                        None,
                    )),
                    duration_ms: 0,
                }
            }
        };

        let status = outcome.status;
        let body_text = outcome.body;

        if !status.is_success() {
            warn!(status = %status, body = %truncate(&body_text, 256), "drive api error");
            return InvokeResponse {
                tool_id: TOOL_ID.into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gdrive_api_error",
                    "message": format!("drive returned HTTP {}", status.as_u16()),
                    "status": status.as_u16(),
                    "body": truncate(&body_text, 512),
                })),
                duration_ms: 0,
            };
        }

        let parsed: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);
        let files = parsed
            .get("files")
            .cloned()
            .unwrap_or_else(|| Value::Array(vec![]));
        let next = parsed
            .get("nextPageToken")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        InvokeResponse {
            tool_id: TOOL_ID.into(),
            success: true,
            result: Some(json!({
                "files": files,
                "next_page_token": next,
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `gdrive.list`.
#[derive(Debug, Default)]
struct GDriveArgs {
    /// Drive search query string.
    query: Option<String>,
    /// Folder ID to filter by.
    folder_id: Option<String>,
    /// Max results per page.
    page_size: Option<u32>,
    /// Pagination token from a prior response.
    page_token: Option<String>,
}

/// Parse and validate `gdrive.list` arguments.
fn parse_args(args: &Value) -> Result<GDriveArgs, String> {
    let obj = match args {
        Value::Null => return Ok(GDriveArgs::default()),
        Value::Object(m) => m,
        _ => return Err("args must be a JSON object".to_string()),
    };

    let pull_str = |k: &str| -> Option<String> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };
    let page_size = obj
        .get("page_size")
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);

    Ok(GDriveArgs {
        query: pull_str("query"),
        folder_id: pull_str("folder_id"),
        page_size,
        page_token: pull_str("page_token"),
    })
}

// ---------------------------------------------------------------------------
// gdrive.upload -- create a file via a hand-built multipart/related request.
// ---------------------------------------------------------------------------

/// Upload a file to Google Drive for the authenticated tenant.
pub struct GDriveUploadTool;

#[async_trait]
impl Tool for GDriveUploadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gdrive.upload".to_string(),
            name: "Upload Drive File".to_string(),
            description: "Upload a file to Google Drive for the authenticated tenant.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["name", "content"],
                "properties": {
                    "name": { "type": "string", "description": "File name" },
                    "content": { "type": "string", "description": "File content (plaintext or base64)" },
                    "encoding": { "type": "string", "enum": ["text", "base64"], "description": "How `content` is encoded (default text)" },
                    "folder_id": { "type": "string", "description": "Parent folder id" },
                    "mime_type": { "type": "string", "description": "MIME type (auto-detected from name if omitted)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "name": { "type": "string" },
                    "web_view_link": { "type": "string" }
                }
            }),
            category: "storage".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    /// Uploading creates a new file each call; not safe to blindly replay.
    fn retry_policy(&self) -> crate::tool::RetryPolicy {
        crate::tool::RetryPolicy::non_idempotent()
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gdrive.upload", "bad_request", "tenant_id is required", None),
        };
        let args = match parse_upload_args(&req.args) {
            Ok(a) => a,
            Err(msg) => return error_response("gdrive.upload", "bad_request", msg, None),
        };
        let bytes = match args.encoding.as_str() {
            "base64" => match STANDARD.decode(args.content.as_bytes()) {
                Ok(b) => b,
                Err(e) => return error_response("gdrive.upload", "bad_request", format!("content is not valid base64: {e}"), None),
            },
            _ => args.content.into_bytes(),
        };
        let mime = args
            .mime_type
            .unwrap_or_else(|| detect_mime(&args.name).to_string());

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gdrive.upload", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gdrive.upload", "internal_error", e.to_string(), None),
        };

        let mut metadata = json!({ "name": args.name });
        if let Some(folder) = &args.folder_id {
            metadata["parents"] = json!([folder]);
        }
        let boundary = "hermes-boundary-7f3a2b1c";
        let body = build_multipart_related(boundary, &metadata, &mime, &bytes);

        let request = http
            .post(DRIVE_UPLOAD_URL)
            .bearer_auth(&token)
            .query(&[("uploadType", "multipart"), ("fields", "id,name,webViewLink")])
            .header("Content-Type", format!("multipart/related; boundary={boundary}"))
            .body(body);

        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => return error_response("gdrive.upload", "gdrive_unreachable", format!("drive api request failed: {e}"), None),
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "drive upload error");
            return InvokeResponse {
                tool_id: "gdrive.upload".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gdrive_api_error",
                    "message": format!("drive returned HTTP {}", outcome.status.as_u16()),
                    "status": outcome.status.as_u16(),
                    "body": truncate(&outcome.body, 512),
                })),
                duration_ms: 0,
            };
        }
        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gdrive.upload".into(),
            success: true,
            result: Some(json!({
                "id": parsed.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                "name": parsed.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "web_view_link": parsed.get("webViewLink").and_then(|v| v.as_str()).unwrap_or(""),
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

/// Parsed arguments for `gdrive.upload`.
struct UploadArgs {
    /// Destination file name.
    name: String,
    /// File content (text or base64-encoded).
    content: String,
    /// Encoding of `content`: "text" or "base64".
    encoding: String,
    /// Optional parent folder ID.
    folder_id: Option<String>,
    /// Optional explicit MIME type; auto-detected from name when absent.
    mime_type: Option<String>,
}

/// Parse and validate `gdrive.upload` arguments.
fn parse_upload_args(args: &Value) -> Result<UploadArgs, String> {
    let obj = args.as_object().ok_or_else(|| "args must be a JSON object".to_string())?;
    let name = obj
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "'name' is required and must be a non-empty string".to_string())?;
    // `content` may legitimately be empty (zero-byte file), so don't trim/reject.
    let content = obj
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "'content' is required and must be a string".to_string())?;
    let encoding = obj.get("encoding").and_then(|v| v.as_str()).unwrap_or("text").to_string();
    if encoding != "text" && encoding != "base64" {
        return Err(format!("invalid encoding '{encoding}' (use text or base64)"));
    }
    let folder_id = obj.get("folder_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    let mime_type = obj.get("mime_type").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string);
    Ok(UploadArgs { name, content, encoding, folder_id, mime_type })
}

/// Build a `multipart/related` body: a JSON metadata part followed by the raw
/// media part. Done by hand to avoid pulling in reqwest's `multipart` feature.
fn build_multipart_related(boundary: &str, metadata: &Value, mime: &str, media: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Type: application/json; charset=UTF-8\r\n\r\n");
    body.extend_from_slice(metadata.to_string().as_bytes());
    body.extend_from_slice(format!("\r\n--{boundary}\r\n").as_bytes());
    body.extend_from_slice(format!("Content-Type: {mime}\r\n\r\n").as_bytes());
    body.extend_from_slice(media);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    body
}

/// Best-effort MIME type from a filename extension.
fn detect_mime(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "txt" | "text" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "html" | "htm" => "text/html",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// gdrive.download -- fetch file metadata + media content.
// ---------------------------------------------------------------------------

/// Download a Google Drive file's content and metadata.
pub struct GDriveDownloadTool;

#[async_trait]
impl Tool for GDriveDownloadTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gdrive.download".to_string(),
            name: "Download Drive File".to_string(),
            description: "Download a Google Drive file's content and metadata.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["file_id"],
                "properties": {
                    "file_id": { "type": "string", "description": "Drive file id" },
                    "encoding": { "type": "string", "enum": ["text", "base64"], "description": "How to return content (default text)" }
                }
            }),
            output_schema: json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string" },
                    "mime_type": { "type": "string" },
                    "name": { "type": "string" },
                    "size": { "type": "integer" }
                }
            }),
            category: "storage".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gdrive.download", "bad_request", "tenant_id is required", None),
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => return error_response("gdrive.download", "bad_request", "args must be a JSON object", None),
        };
        let file_id = match obj.get("file_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => f.to_string(),
            None => return error_response("gdrive.download", "bad_request", "'file_id' is required", None),
        };
        let encoding = match obj.get("encoding").and_then(|v| v.as_str()) {
            Some(e) if e == "text" || e == "base64" => e,
            Some(e) => return error_response("gdrive.download", "bad_request", format!("invalid encoding '{e}'"), Some("use text or base64")),
            None => "text",
        };

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gdrive.download", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gdrive.download", "internal_error", e.to_string(), None),
        };

        // Metadata prefetch for name/mime/size.
        let meta_req = http
            .get(format!("{DRIVE_FILES_URL}/{file_id}"))
            .bearer_auth(&token)
            .query(&[("fields", "name,mimeType,size")]);
        let meta = match send_with_retry(meta_req, &self.retry_policy()).await {
            Ok(o) if o.status.is_success() => serde_json::from_str::<Value>(&o.body).unwrap_or(Value::Null),
            Ok(o) => {
                return InvokeResponse {
                    tool_id: "gdrive.download".into(),
                    success: false,
                    result: None,
                    error: Some(json!({
                        "code": "gdrive_api_error",
                        "message": format!("drive returned HTTP {}", o.status.as_u16()),
                        "status": o.status.as_u16(),
                        "body": truncate(&o.body, 512),
                    })),
                    duration_ms: 0,
                };
            }
            Err(e) => return error_response("gdrive.download", "gdrive_unreachable", format!("drive api request failed: {e}"), None),
        };

        // Media download (raw bytes preserved).
        let media_req = http
            .get(format!("{DRIVE_FILES_URL}/{file_id}"))
            .bearer_auth(&token)
            .query(&[("alt", "media")]);
        let media = match send_with_retry_bytes(media_req, &self.retry_policy()).await {
            Ok(o) if o.status.is_success() => o.body,
            Ok(o) => {
                return InvokeResponse {
                    tool_id: "gdrive.download".into(),
                    success: false,
                    result: None,
                    error: Some(json!({
                        "code": "gdrive_api_error",
                        "message": format!("drive returned HTTP {} for media", o.status.as_u16()),
                        "status": o.status.as_u16(),
                    })),
                    duration_ms: 0,
                };
            }
            Err(e) => return error_response("gdrive.download", "gdrive_unreachable", format!("drive media request failed: {e}"), None),
        };

        let content = match encoding {
            "base64" => STANDARD.encode(&media),
            _ => String::from_utf8_lossy(&media).into_owned(),
        };

        InvokeResponse {
            tool_id: "gdrive.download".into(),
            success: true,
            result: Some(json!({
                "content": content,
                "mime_type": meta.get("mimeType").and_then(|v| v.as_str()).unwrap_or(""),
                "name": meta.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                "size": meta.get("size").and_then(|v| v.as_str()).and_then(|s| s.parse::<i64>().ok()).unwrap_or(media.len() as i64),
            })),
            error: None,
            duration_ms: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// gdrive.get_metadata -- full file metadata.
// ---------------------------------------------------------------------------

/// Fetch full metadata for a Google Drive file.
pub struct GDriveGetMetadataTool;

#[async_trait]
impl Tool for GDriveGetMetadataTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            tool_id: "gdrive.get_metadata".to_string(),
            name: "Get Drive File Metadata".to_string(),
            description: "Fetch full metadata for a Google Drive file.".to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["file_id"],
                "properties": {
                    "file_id": { "type": "string", "description": "Drive file id" }
                }
            }),
            output_schema: json!({ "type": "object" }),
            category: "storage".to_string(),
            requires_auth: true,
        }
    }

    fn provider(&self) -> &'static str {
        PROVIDER
    }

    async fn invoke(&self, ctx: &InvokeContext, req: InvokeRequest) -> InvokeResponse {
        let tenant_id = match req.tenant_id.as_deref().filter(|s| !s.is_empty()) {
            Some(t) => t.to_string(),
            None => return error_response("gdrive.get_metadata", "bad_request", "tenant_id is required", None),
        };
        let obj = match req.args.as_object() {
            Some(o) => o,
            None => return error_response("gdrive.get_metadata", "bad_request", "args must be a JSON object", None),
        };
        let file_id = match obj.get("file_id").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            Some(f) => f.to_string(),
            None => return error_response("gdrive.get_metadata", "bad_request", "'file_id' is required", None),
        };

        let token = match ctx.credd.fetch_token(&tenant_id, PROVIDER).await {
            Ok(t) => t,
            Err(e) => return credd_error_to_response("gdrive.get_metadata", &e),
        };
        let http = match build_http() {
            Ok(c) => c,
            Err(e) => return error_response("gdrive.get_metadata", "internal_error", e.to_string(), None),
        };

        let request = http
            .get(format!("{DRIVE_FILES_URL}/{file_id}"))
            .bearer_auth(&token)
            .query(&[("fields", "*")]);
        let outcome = match send_with_retry(request, &self.retry_policy()).await {
            Ok(o) => o,
            Err(e) => return error_response("gdrive.get_metadata", "gdrive_unreachable", format!("drive api request failed: {e}"), None),
        };
        if !outcome.status.is_success() {
            warn!(status = %outcome.status, body = %truncate(&outcome.body, 256), "drive metadata error");
            return InvokeResponse {
                tool_id: "gdrive.get_metadata".into(),
                success: false,
                result: None,
                error: Some(json!({
                    "code": "gdrive_api_error",
                    "message": format!("drive returned HTTP {}", outcome.status.as_u16()),
                    "status": outcome.status.as_u16(),
                    "body": truncate(&outcome.body, 512),
                })),
                duration_ms: 0,
            };
        }
        let parsed: Value = serde_json::from_str(&outcome.body).unwrap_or(Value::Null);
        InvokeResponse {
            tool_id: "gdrive.get_metadata".into(),
            success: true,
            result: Some(parsed),
            error: None,
            duration_ms: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_args() {
        let a = parse_args(&Value::Null).unwrap();
        assert!(a.query.is_none());
        assert!(a.page_size.is_none());
    }

    #[test]
    fn parse_full_args() {
        let v = json!({
            "query": "name contains 'plan'",
            "folder_id": "abc",
            "page_size": 50
        });
        let a = parse_args(&v).unwrap();
        assert_eq!(a.query.as_deref(), Some("name contains 'plan'"));
        assert_eq!(a.folder_id.as_deref(), Some("abc"));
        assert_eq!(a.page_size, Some(50));
    }

    #[test]
    fn parse_rejects_non_object() {
        assert!(parse_args(&json!("bad")).is_err());
    }

    #[test]
    fn upload_args_defaults_encoding_to_text() {
        let v = json!({ "name": "a.txt", "content": "hello" });
        let a = parse_upload_args(&v).unwrap();
        assert_eq!(a.encoding, "text");
        assert!(a.folder_id.is_none());
    }

    #[test]
    fn upload_args_rejects_missing_content() {
        let v = json!({ "name": "a.txt" });
        assert!(parse_upload_args(&v).is_err());
    }

    #[test]
    fn upload_args_allows_empty_content() {
        let v = json!({ "name": "a.txt", "content": "" });
        let a = parse_upload_args(&v).unwrap();
        assert_eq!(a.content, "");
    }

    #[test]
    fn upload_args_rejects_bad_encoding() {
        let v = json!({ "name": "a.txt", "content": "x", "encoding": "hex" });
        assert!(parse_upload_args(&v).is_err());
    }

    #[test]
    fn detect_mime_known_and_default() {
        assert_eq!(detect_mime("notes.md"), "text/markdown");
        assert_eq!(detect_mime("photo.JPG"), "image/jpeg");
        assert_eq!(detect_mime("data.bin"), "application/octet-stream");
        assert_eq!(detect_mime("noext"), "application/octet-stream");
    }

    #[test]
    fn multipart_related_has_both_parts() {
        let body = build_multipart_related("B", &json!({"name": "f"}), "text/plain", b"hi");
        let s = String::from_utf8_lossy(&body);
        assert!(s.contains("--B\r\n"));
        assert!(s.contains("Content-Type: application/json"));
        assert!(s.contains("\"name\":\"f\""));
        assert!(s.contains("Content-Type: text/plain"));
        assert!(s.contains("hi"));
        assert!(s.ends_with("--B--\r\n"));
    }
}
