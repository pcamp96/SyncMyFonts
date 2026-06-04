use std::{
    fs,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use syncmyfonts_core::{
    API_VERSION, DEFAULT_API_KEY_HEADER, DeviceCheckInRequest, DeviceCheckInResponse, FontFormat,
    FontManifestEntry, HealthResponse, ManifestResponse, RegisterFontRequest, RegisterFontResponse,
};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

#[derive(Clone)]
struct AppState {
    db: Arc<Mutex<Connection>>,
    storage_dir: PathBuf,
    api_key: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let config = Config::from_env()?;
    fs::create_dir_all(&config.storage_dir).context("creating font storage directory")?;

    let db = Connection::open(&config.database_path).context("opening sqlite database")?;
    migrate(&db).context("running migrations")?;

    let state = AppState {
        db: Arc::new(Mutex::new(db)),
        storage_dir: config.storage_dir,
        api_key: config.api_key,
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/fonts", get(list_fonts).post(register_font))
        .route(
            "/api/v1/fonts/{sha256}/blob",
            get(download_font).post(upload_font),
        )
        .route("/api/v1/devices/check-in", post(check_in))
        .layer(DefaultBodyLimit::max(100 * 1024 * 1024))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(config.listen_addr).await?;
    tracing::info!("syncmyfonts server listening on {}", config.listen_addr);
    axum::serve(listener, app).await?;
    Ok(())
}

struct Config {
    listen_addr: SocketAddr,
    database_path: PathBuf,
    storage_dir: PathBuf,
    api_key: Option<String>,
}

impl Config {
    fn from_env() -> Result<Self> {
        let listen_addr = std::env::var("SYNCMYFONTS_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:7368".to_string())
            .parse()
            .context("parsing SYNCMYFONTS_LISTEN")?;
        let data_dir = PathBuf::from(
            std::env::var("SYNCMYFONTS_DATA_DIR").unwrap_or_else(|_| "./data".into()),
        );
        Ok(Self {
            listen_addr,
            database_path: PathBuf::from(
                std::env::var("SYNCMYFONTS_DB")
                    .unwrap_or_else(|_| data_dir.join("syncmyfonts.sqlite").display().to_string()),
            ),
            storage_dir: PathBuf::from(
                std::env::var("SYNCMYFONTS_STORAGE_DIR")
                    .unwrap_or_else(|_| data_dir.join("fonts").display().to_string()),
            ),
            api_key: std::env::var("SYNCMYFONTS_API_KEY").ok(),
        })
    }
}

fn migrate(db: &Connection) -> Result<()> {
    db.execute_batch(
        r#"
        create table if not exists fonts (
            id text primary key,
            sha256 text not null unique,
            file_name text not null,
            family_name text,
            postscript_name text,
            style_name text,
            full_name text,
            format text not null,
            size_bytes integer not null,
            archived integer not null default 0,
            created_at text not null,
            updated_at text not null
        );

        create table if not exists devices (
            id text primary key,
            device_name text not null,
            os text not null,
            last_seen_at text not null,
            unique(device_name, os)
        );

        create table if not exists device_fonts (
            device_id text not null,
            sha256 text not null,
            installed_at text not null,
            primary key(device_id, sha256)
        );
        "#,
    )?;
    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        api_version: API_VERSION,
    })
}

async fn list_fonts(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<ManifestResponse>, ApiError> {
    authorize(&state, &headers)?;
    let db = state
        .db
        .lock()
        .map_err(|_| ApiError::internal("database lock poisoned"))?;
    let mut stmt = db
        .prepare(
            "select id, sha256, file_name, family_name, postscript_name, style_name, full_name, format, size_bytes, archived, created_at, updated_at from fonts where archived = 0 order by family_name, file_name",
        )
        .map_err(ApiError::from)?;
    let fonts = stmt
        .query_map([], row_to_font)
        .map_err(ApiError::from)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(ApiError::from)?;
    Ok(Json(ManifestResponse { fonts }))
}

async fn register_font(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<RegisterFontRequest>,
) -> Result<Json<RegisterFontResponse>, ApiError> {
    authorize(&state, &headers)?;
    validate_sha256(&request.sha256)?;

    let now = Utc::now();
    let db = state
        .db
        .lock()
        .map_err(|_| ApiError::internal("database lock poisoned"))?;
    let existing = get_font_by_hash(&db, &request.sha256)?;
    let font = if let Some(font) = existing {
        font
    } else {
        let font = FontManifestEntry {
            id: Uuid::new_v4(),
            sha256: request.sha256.clone(),
            file_name: request.file_name,
            family_name: request.family_name,
            postscript_name: request.postscript_name,
            style_name: request.style_name,
            full_name: request.full_name,
            format: request.format,
            size_bytes: request.size_bytes,
            archived: false,
            created_at: now,
            updated_at: now,
        };
        db.execute(
            "insert into fonts (id, sha256, file_name, family_name, postscript_name, style_name, full_name, format, size_bytes, archived, created_at, updated_at) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11)",
            params![
                font.id.to_string(),
                font.sha256,
                font.file_name,
                font.family_name,
                font.postscript_name,
                font.style_name,
                font.full_name,
                format!("{:?}", font.format),
                font.size_bytes as i64,
                font.created_at.to_rfc3339(),
                font.updated_at.to_rfc3339(),
            ],
        ).map_err(ApiError::from)?;
        font
    };
    let upload_required = !blob_path(&state.storage_dir, &font.sha256).exists();
    Ok(Json(RegisterFontResponse {
        font,
        upload_required,
    }))
}

async fn upload_font(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(sha256): AxumPath<String>,
    mut multipart: Multipart,
) -> Result<StatusCode, ApiError> {
    authorize(&state, &headers)?;
    validate_sha256(&sha256)?;
    let font = {
        let db = state
            .db
            .lock()
            .map_err(|_| ApiError::internal("database lock poisoned"))?;
        get_font_by_hash(&db, &sha256)?
            .ok_or_else(|| ApiError::not_found("font must be registered before upload"))?
    };

    let mut uploaded = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(e.to_string()))?
    {
        if field.name() == Some("file") {
            uploaded = Some(
                field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::bad_request(e.to_string()))?,
            );
            break;
        }
    }
    let bytes =
        uploaded.ok_or_else(|| ApiError::bad_request("multipart field 'file' is required"))?;
    let actual_hash = hex::encode(Sha256::digest(&bytes));
    if actual_hash != sha256 {
        return Err(ApiError::bad_request(
            "uploaded font hash did not match registered sha256",
        ));
    }
    if bytes.len() as u64 != font.size_bytes {
        return Err(ApiError::bad_request(
            "uploaded font size did not match registered size",
        ));
    }
    fs::write(blob_path(&state.storage_dir, &sha256), bytes).map_err(ApiError::from)?;
    Ok(StatusCode::CREATED)
}

async fn download_font(
    State(state): State<AppState>,
    headers: HeaderMap,
    AxumPath(sha256): AxumPath<String>,
) -> Result<Response, ApiError> {
    authorize(&state, &headers)?;
    validate_sha256(&sha256)?;
    let path = blob_path(&state.storage_dir, &sha256);
    let bytes = fs::read(path).map_err(|_| ApiError::not_found("font blob not found"))?;
    Ok(Response::builder()
        .header("content-type", "application/octet-stream")
        .body(Body::from(bytes))
        .map_err(|_| ApiError::internal("failed to build response"))?)
}

async fn check_in(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<DeviceCheckInRequest>,
) -> Result<Json<DeviceCheckInResponse>, ApiError> {
    authorize(&state, &headers)?;
    let now = Utc::now();
    let db = state
        .db
        .lock()
        .map_err(|_| ApiError::internal("database lock poisoned"))?;
    let device_id = get_or_create_device(&db, &request.device_name, &request.os, now)?;
    db.execute(
        "delete from device_fonts where device_id = ?1",
        params![device_id.to_string()],
    )
    .map_err(ApiError::from)?;
    for hash in &request.installed_hashes {
        if validate_sha256(hash).is_ok() {
            db.execute(
                "insert or ignore into device_fonts (device_id, sha256, installed_at) values (?1, ?2, ?3)",
                params![device_id.to_string(), hash, now.to_rfc3339()],
            )
            .map_err(ApiError::from)?;
        }
    }
    let known_hashes = list_known_hashes(&db)?;
    let installed: std::collections::HashSet<_> = request.installed_hashes.into_iter().collect();
    let missing_hashes = known_hashes
        .into_iter()
        .filter(|hash| !installed.contains(hash))
        .collect();
    Ok(Json(DeviceCheckInResponse {
        device_id,
        missing_hashes,
    }))
}

fn get_or_create_device(
    db: &Connection,
    device_name: &str,
    os: &str,
    now: DateTime<Utc>,
) -> Result<Uuid, ApiError> {
    if let Some(id) = db
        .query_row(
            "select id from devices where device_name = ?1 and os = ?2",
            params![device_name, os],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(ApiError::from)?
    {
        db.execute(
            "update devices set last_seen_at = ?1 where id = ?2",
            params![now.to_rfc3339(), id],
        )
        .map_err(ApiError::from)?;
        return Uuid::parse_str(&id).map_err(|_| ApiError::internal("invalid stored device id"));
    }
    let id = Uuid::new_v4();
    db.execute(
        "insert into devices (id, device_name, os, last_seen_at) values (?1, ?2, ?3, ?4)",
        params![id.to_string(), device_name, os, now.to_rfc3339()],
    )
    .map_err(ApiError::from)?;
    Ok(id)
}

fn get_font_by_hash(db: &Connection, sha256: &str) -> Result<Option<FontManifestEntry>, ApiError> {
    db.query_row(
        "select id, sha256, file_name, family_name, postscript_name, style_name, full_name, format, size_bytes, archived, created_at, updated_at from fonts where sha256 = ?1",
        params![sha256],
        row_to_font,
    )
    .optional()
    .map_err(ApiError::from)
}

fn list_known_hashes(db: &Connection) -> Result<Vec<String>, ApiError> {
    let mut stmt = db
        .prepare("select sha256 from fonts where archived = 0")
        .map_err(ApiError::from)?;
    stmt.query_map([], |row| row.get::<_, String>(0))
        .map_err(ApiError::from)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(ApiError::from)
}

fn row_to_font(row: &rusqlite::Row<'_>) -> rusqlite::Result<FontManifestEntry> {
    let format_name: String = row.get(7)?;
    Ok(FontManifestEntry {
        id: parse_uuid_row(row.get::<_, String>(0)?)?,
        sha256: row.get(1)?,
        file_name: row.get(2)?,
        family_name: row.get(3)?,
        postscript_name: row.get(4)?,
        style_name: row.get(5)?,
        full_name: row.get(6)?,
        format: parse_format(&format_name),
        size_bytes: row.get::<_, i64>(8)? as u64,
        archived: row.get::<_, i64>(9)? != 0,
        created_at: parse_datetime_row(row.get::<_, String>(10)?)?,
        updated_at: parse_datetime_row(row.get::<_, String>(11)?)?,
    })
}

fn parse_uuid_row(value: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&value).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_datetime_row(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
        })
}

fn parse_format(value: &str) -> FontFormat {
    match value {
        "Otf" => FontFormat::Otf,
        "Ttf" => FontFormat::Ttf,
        "Ttc" => FontFormat::Ttc,
        "Otc" => FontFormat::Otc,
        "Woff" => FontFormat::Woff,
        "Woff2" => FontFormat::Woff2,
        _ => FontFormat::Unknown,
    }
}

fn blob_path(storage_dir: &Path, sha256: &str) -> PathBuf {
    storage_dir.join(sha256)
}

fn validate_sha256(value: &str) -> Result<(), ApiError> {
    if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "sha256 must be a 64-character hex string",
        ))
    }
}

fn authorize(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    let Some(expected) = &state.api_key else {
        return Ok(());
    };
    let provided = headers
        .get(DEFAULT_API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing api key"))?;
    if provided == expected {
        Ok(())
    } else {
        Err(ApiError::unauthorized("invalid api key"))
    }
}

struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    fn unauthorized(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for ApiError {
    fn from(value: rusqlite::Error) -> Self {
        Self::internal(value.to_string())
    }
}

impl From<std::io::Error> for ApiError {
    fn from(value: std::io::Error) -> Self {
        Self::internal(value.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}
