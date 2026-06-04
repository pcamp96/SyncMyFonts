use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const API_VERSION: &str = "v1";
pub const DEFAULT_API_KEY_HEADER: &str = "x-syncmyfonts-key";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FontManifestEntry {
    pub id: Uuid,
    pub sha256: String,
    pub file_name: String,
    pub family_name: Option<String>,
    pub postscript_name: Option<String>,
    pub style_name: Option<String>,
    pub full_name: Option<String>,
    pub format: FontFormat,
    pub size_bytes: u64,
    pub archived: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FontFormat {
    Otf,
    Ttf,
    Ttc,
    Otc,
    Woff,
    Woff2,
    Unknown,
}

impl FontFormat {
    pub fn from_file_name(file_name: &str) -> Self {
        match file_name
            .rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str()
        {
            "otf" => Self::Otf,
            "ttf" => Self::Ttf,
            "ttc" => Self::Ttc,
            "otc" => Self::Otc,
            "woff" => Self::Woff,
            "woff2" => Self::Woff2,
            _ => Self::Unknown,
        }
    }

    pub fn is_installable_desktop_font(&self) -> bool {
        matches!(self, Self::Otf | Self::Ttf | Self::Ttc | Self::Otc)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterFontRequest {
    pub sha256: String,
    pub file_name: String,
    pub family_name: Option<String>,
    pub postscript_name: Option<String>,
    pub style_name: Option<String>,
    pub full_name: Option<String>,
    pub format: FontFormat,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterFontResponse {
    pub font: FontManifestEntry,
    pub upload_required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestResponse {
    pub fonts: Vec<FontManifestEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCheckInRequest {
    pub device_name: String,
    pub os: String,
    pub installed_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceCheckInResponse {
    pub device_id: Uuid,
    pub missing_hashes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub api_version: &'static str,
}
