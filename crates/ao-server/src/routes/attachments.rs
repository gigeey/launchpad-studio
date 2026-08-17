use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, Response, StatusCode};
use axum::Json;
use serde::Deserialize;

use ao_engine::AppState;
use ao_protocol::attachment::Attachment;
use ao_protocol::error::AoError;

use crate::error::AppError;

/// Allowed MIME type prefixes/values for upload validation.
pub const ALLOWED_MIME_PATTERNS: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/webp",
    "application/pdf",
    "text/",
    "application/json",
    "application/vnd.openxmlformats-officedocument.",
    // Common code file types
    "application/javascript",
    "application/typescript",
    "application/xml",
    "application/x-yaml",
    "application/toml",
    "application/x-sh",
];

pub const DEFAULT_MAX_FILE_SIZE: u64 = 10 * 1024 * 1024; // 10MB

/// Check if a MIME type is allowed for upload.
pub fn is_mime_allowed(mime: &str) -> bool {
    for pattern in ALLOWED_MIME_PATTERNS {
        if mime == *pattern || mime.starts_with(pattern) {
            return true;
        }
    }
    false
}

/// Magic byte signatures for image types.
pub fn validate_magic_bytes(bytes: &[u8], declared_mime: &str) -> Result<(), AoError> {
    if !declared_mime.starts_with("image/") {
        return Ok(());
    }

    if bytes.len() < 12 {
        return Err(AoError::ValidationError(
            "File too small to validate image type".to_string(),
        ));
    }

    let valid = match declared_mime {
        "image/png" => bytes.starts_with(&[0x89, 0x50, 0x4E, 0x47]),
        "image/jpeg" => bytes.starts_with(&[0xFF, 0xD8, 0xFF]),
        "image/gif" => bytes.starts_with(b"GIF8"),
        "image/webp" => bytes.starts_with(b"RIFF") && bytes.len() >= 12 && &bytes[8..12] == b"WEBP",
        _ => true, // Unknown image types pass through
    };

    if !valid {
        return Err(AoError::ValidationError(format!(
            "File content does not match declared MIME type '{}'",
            declared_mime
        )));
    }

    Ok(())
}

/// POST /agents/{agent_id}/attachments — multipart/form-data upload
pub async fn upload_attachment(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    // Get max file size (from agent's file capabilities or default)
    let max_size = DEFAULT_MAX_FILE_SIZE;

    // Extract the 'file' field from multipart
    let mut file_data: Option<(String, String, Vec<u8>)> = None;

    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AoError::ValidationError(format!("Invalid multipart data: {}", e)))?
    {
        let name = field.name().unwrap_or("").to_string();
        if name == "file" {
            let filename = field
                .file_name()
                .unwrap_or("unnamed")
                .to_string();
            let content_type = field
                .content_type()
                .unwrap_or("application/octet-stream")
                .to_string();
            let bytes = field
                .bytes()
                .await
                .map_err(|e| AoError::ValidationError(format!("Failed to read file: {}", e)))?;

            file_data = Some((filename, content_type, bytes.to_vec()));
            break;
        }
    }

    let (filename, mime_type, bytes) = file_data
        .ok_or_else(|| AoError::ValidationError("Missing 'file' field in multipart data".to_string()))?;

    // Validate file size
    if bytes.len() as u64 > max_size {
        return Err(AoError::ValidationError(format!(
            "File size {} bytes exceeds maximum allowed {} bytes",
            bytes.len(),
            max_size
        ))
        .into());
    }

    // Validate MIME type
    if !is_mime_allowed(&mime_type) {
        return Err(AoError::ValidationError(format!(
            "MIME type '{}' is not allowed",
            mime_type
        ))
        .into());
    }

    // Validate magic bytes for images
    validate_magic_bytes(&bytes, &mime_type)?;

    // Store the file
    let attachment = state
        .persistence
        .assets
        .store_file(&agent_id, &filename, &mime_type, &bytes)
        .await?;

    Ok(Json(attachment))
}

#[derive(Debug, Deserialize)]
pub struct FolderReferenceRequest {
    pub path: String,
}

/// POST /agents/{agent_id}/attachments/folder — JSON body { path: String }
pub async fn upload_folder_reference(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
    Json(req): Json<FolderReferenceRequest>,
) -> Result<Json<Attachment>, AppError> {
    // Validate agent exists
    state
        .persistence
        .agents
        .get(&agent_id)
        .await?
        .ok_or_else(|| AoError::AgentNotFound(agent_id.clone()))?;

    let attachment = state
        .persistence
        .assets
        .store_folder_reference(&agent_id, &req.path)
        .await?;

    Ok(Json(attachment))
}

/// GET /agents/{agent_id}/attachments/{attachment_id} — serves the file
pub async fn serve_attachment(
    State(state): State<Arc<AppState>>,
    Path((agent_id, attachment_id)): Path<(String, String)>,
) -> Result<Response<Body>, AppError> {
    let (bytes, mime_type) = state
        .persistence
        .assets
        .get_file(&agent_id, &attachment_id)
        .await?;

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|e| AoError::Internal(format!("Failed to build response: {}", e)))?;

    Ok(response)
}

/// GET /agents/{agent_id}/attachments/{attachment_id}/info — returns Attachment metadata
pub async fn get_attachment_info(
    State(state): State<Arc<AppState>>,
    Path((agent_id, attachment_id)): Path<(String, String)>,
) -> Result<Json<Attachment>, AppError> {
    let attachments = state
        .persistence
        .assets
        .list_files(&agent_id)
        .await?;

    let attachment = attachments
        .into_iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| AoError::AttachmentNotFound(attachment_id))?;

    Ok(Json(attachment))
}

/// GET /agents/{agent_id}/attachments — lists all attachments
pub async fn list_attachments(
    State(state): State<Arc<AppState>>,
    Path(agent_id): Path<String>,
) -> Result<Json<Vec<Attachment>>, AppError> {
    let attachments = state
        .persistence
        .assets
        .list_files(&agent_id)
        .await?;

    Ok(Json(attachments))
}

/// DELETE /agents/{agent_id}/attachments/{attachment_id} — deletes file
pub async fn delete_attachment(
    State(state): State<Arc<AppState>>,
    Path((agent_id, attachment_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    state
        .persistence
        .assets
        .delete_file(&agent_id, &attachment_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}
