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

fn project_asset_key(project_id: &str) -> String {
    format!("project_{}", project_id)
}

/// POST /projects/{id}/attachments — multipart/form-data upload
pub async fn upload_project_attachment(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    mut multipart: Multipart,
) -> Result<Json<Attachment>, AppError> {
    state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let asset_key = project_asset_key(&project_id);
    let max_size = DEFAULT_MAX_FILE_SIZE;
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

    if bytes.len() as u64 > max_size {
        return Err(AoError::ValidationError(format!(
            "File size {} bytes exceeds maximum allowed {} bytes",
            bytes.len(),
            max_size
        ))
        .into());
    }

    if !is_mime_allowed(&mime_type) {
        return Err(
            AoError::ValidationError(format!("MIME type '{}' is not allowed", mime_type)).into(),
        );
    }

    validate_magic_bytes(&bytes, &mime_type)?;

    let attachment = state
        .persistence
        .assets
        .store_file(&asset_key, &filename, &mime_type, &bytes)
        .await?;

    Ok(Json(attachment))
}

/// POST /projects/{id}/attachments/folder — JSON body { path: String }
pub async fn upload_project_folder_reference(
    State(state): State<Arc<AppState>>,
    Path(project_id): Path<String>,
    Json(req): Json<FolderReferenceRequest>,
) -> Result<Json<Attachment>, AppError> {
    state
        .persistence
        .projects
        .get(&project_id)
        .await?
        .ok_or_else(|| AoError::ProjectNotFound(project_id.clone()))?;

    let asset_key = project_asset_key(&project_id);
    let attachment = state
        .persistence
        .assets
        .store_folder_reference(&asset_key, &req.path)
        .await?;

    Ok(Json(attachment))
}

/// GET /projects/{id}/attachments/{attachment_id} — serves the file
pub async fn serve_project_attachment(
    State(state): State<Arc<AppState>>,
    Path((project_id, attachment_id)): Path<(String, String)>,
) -> Result<Response<Body>, AppError> {
    let asset_key = project_asset_key(&project_id);
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

/// GET /projects/{id}/attachments/{attachment_id}/info — returns Attachment metadata
pub async fn get_project_attachment_info(
    State(state): State<Arc<AppState>>,
    Path((project_id, attachment_id)): Path<(String, String)>,
) -> Result<Json<Attachment>, AppError> {
    let asset_key = project_asset_key(&project_id);
    let attachments = state.persistence.assets.list_files(&asset_key).await?;
    let attachment = attachments
        .into_iter()
        .find(|a| a.id == attachment_id)
        .ok_or_else(|| AoError::AttachmentNotFound(attachment_id))?;
    Ok(Json(attachment))
}

/// DELETE /projects/{id}/attachments/{attachment_id} — deletes file
pub async fn delete_project_attachment(
    State(state): State<Arc<AppState>>,
    Path((project_id, attachment_id)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let asset_key = project_asset_key(&project_id);
    state
        .persistence
        .assets
        .delete_file(&asset_key, &attachment_id)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
