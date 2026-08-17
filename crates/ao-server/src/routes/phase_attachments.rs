use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path, State};
use axum::http::{header, Response, StatusCode};
use axum::Json;

use ao_engine::AppState;
use ao_protocol::attachment::Attachment;
use ao_protocol::error::AoError;

use crate::error::AppError;

use super::attachments::{is_mime_allowed, validate_magic_bytes, FolderReferenceRequest, DEFAULT_MAX_FILE_SIZE};

/// Storage key prefix for phase attachments.
fn phase_asset_key(task_id: &str, phase_id: &str) -> String {
    format!("phase_{}_{}", task_id, phase_id)
}

/// POST /tasks/{task_id}/phases/{phase_id}/attachments — multipart/form-data upload
pub async fn upload_phase_attachment(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, AppError> {
    // Validate task exists
    state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    let asset_key = phase_asset_key(&task_id, &phase_id);
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
            let filename = field.file_name().unwrap_or("unnamed").to_string();
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

    let (filename, mime_type, bytes) = file_data.ok_or_else(|| {
        AoError::ValidationError("Missing 'file' field in multipart data".to_string())
    })?;

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
        return Err(
            AoError::ValidationError(format!("MIME type '{}' is not allowed", mime_type)).into(),
        );
    }

    // Validate magic bytes for images
    validate_magic_bytes(&bytes, &mime_type)?;

    // Store the file using phase asset key
    let attachment = state
        .persistence
        .assets
        .store_file(&asset_key, &filename, &mime_type, &bytes)
        .await?;

    Ok(Json(attachment))
}

/// POST /tasks/{task_id}/phases/{phase_id}/attachments/folder — JSON body { path: String }
pub async fn upload_phase_folder_reference(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
    Json(req): Json<FolderReferenceRequest>,
) -> Result<Json<Attachment>, AppError> {
    // Validate task exists
    state
        .workflow_runner
        .get_task_state(&task_id)
        .await
        .map_err(|_| AoError::TaskNotFound(task_id.clone()))?;

    let asset_key = phase_asset_key(&task_id, &phase_id);

    let attachment = state
        .persistence
        .assets
        .store_folder_reference(&asset_key, &req.path)
        .await?;

    Ok(Json(attachment))
}

/// GET /tasks/{task_id}/phases/{phase_id}/attachments/{attachment_id} — serves the file
pub async fn serve_phase_attachment(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id, attachment_id)): Path<(String, String, String)>,
) -> Result<Response<Body>, AppError> {
    let asset_key = phase_asset_key(&task_id, &phase_id);

    let (bytes, mime_type) = state
        .persistence
        .assets
        .get_file(&asset_key, &attachment_id)
        .await?;

    let response = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime_type)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
        .body(Body::from(bytes))
        .map_err(|e| AoError::Internal(format!("Failed to build response: {}", e)))?;

    Ok(response)
}

/// GET /tasks/{task_id}/phases/{phase_id}/attachments/{attachment_id}/info — returns metadata
pub async fn get_phase_attachment_info(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id, attachment_id)): Path<(String, String, String)>,
) -> Result<Json<Attachment>, AppError> {
    let asset_key = phase_asset_key(&task_id, &phase_id);

    let attachments = state.persistence.assets.list_files(&asset_key).await?;

    let attachment = attachments
        .into_iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| AoError::AttachmentNotFound(attachment_id))?;

    Ok(Json(attachment))
}

/// GET /tasks/{task_id}/phases/{phase_id}/attachments — lists all attachments
pub async fn list_phase_attachments(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id)): Path<(String, String)>,
) -> Result<Json<Vec<Attachment>>, AppError> {
    let asset_key = phase_asset_key(&task_id, &phase_id);

    let attachments = state.persistence.assets.list_files(&asset_key).await?;

    Ok(Json(attachments))
}

/// DELETE /tasks/{task_id}/phases/{phase_id}/attachments/{attachment_id} — deletes file
pub async fn delete_phase_attachment(
    State(state): State<Arc<AppState>>,
    Path((task_id, phase_id, attachment_id)): Path<(String, String, String)>,
) -> Result<StatusCode, AppError> {
    let asset_key = phase_asset_key(&task_id, &phase_id);

    state
        .persistence
        .assets
        .delete_file(&asset_key, &attachment_id)
        .await?;

    Ok(StatusCode::NO_CONTENT)
}
