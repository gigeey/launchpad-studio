use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentType {
    #[serde(alias = "Image")]
    Image,
    #[serde(alias = "Document")]
    Document,
    #[serde(alias = "Spreadsheet")]
    Spreadsheet,
    #[serde(alias = "Code")]
    Code,
    #[serde(alias = "Folder")]
    Folder,
    #[serde(alias = "Other")]
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub file_path: String,
    pub mime_type: String,
    pub original_filename: String,
    pub size_bytes: u64,
    pub attachment_type: AttachmentType,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ImageMode {
    FileReference {
        instruction_template: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileCapability {
    pub supported: bool,
    #[serde(default = "default_max_file_size_bytes")]
    pub max_file_size_bytes: u64,
    #[serde(default = "default_max_attachments_per_message")]
    pub max_attachments_per_message: u32,
    #[serde(default)]
    pub allowed_mime_types: Vec<String>,
    pub image_mode: ImageMode,
}

fn default_max_file_size_bytes() -> u64 {
    10 * 1024 * 1024 // 10MB
}

fn default_max_attachments_per_message() -> u32 {
    5
}
