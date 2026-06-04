use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
};
use chrono::Utc;
use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, multipart};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use syncmyfonts_core::{
    API_VERSION, DEFAULT_API_KEY_HEADER, DeviceCheckInRequest, FontFormat, FontManifestEntry,
    HealthResponse, ManifestResponse, RegisterFontRequest, RegisterFontResponse,
};
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;
use walkdir::WalkDir;

#[derive(Parser)]
#[command(name = "syncmyfonts")]
#[command(about = "Cross-platform font sync agent for SyncMyFonts")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Scan local user fonts and print JSON inventory.
    Scan {
        #[arg(long)]
        include_managed: bool,
    },
    /// Upload local user fonts to the configured sync server.
    Push {
        #[arg(long, env = "SYNCMYFONTS_SERVER")]
        server: String,
        #[arg(long, env = "SYNCMYFONTS_API_KEY")]
        api_key: Option<String>,
    },
    /// Download and install fonts missing from this machine.
    Sync {
        #[arg(long, env = "SYNCMYFONTS_SERVER")]
        server: String,
        #[arg(long, env = "SYNCMYFONTS_API_KEY")]
        api_key: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Serve this machine's user-installed fonts to paired LAN devices.
    LanServe {
        #[arg(long, default_value = "0.0.0.0:7370")]
        listen: SocketAddr,
        #[arg(long, env = "SYNCMYFONTS_LAN_KEY")]
        lan_key: Option<String>,
    },
    /// Pull missing fonts directly from another SyncMyFonts LAN peer.
    LanSync {
        #[arg(long)]
        peer: String,
        #[arg(long, env = "SYNCMYFONTS_LAN_KEY")]
        lan_key: Option<String>,
        #[arg(long)]
        dry_run: bool,
    },
    /// Save a LAN peer so app wrappers can sync without retyping URLs.
    LanAddPeer {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long, env = "SYNCMYFONTS_LAN_KEY")]
        lan_key: Option<String>,
    },
    /// List saved LAN peers.
    LanPeers,
    /// Pull missing fonts from every saved LAN peer.
    LanSyncAll {
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Serialize)]
struct ScanOutput {
    platform: &'static str,
    schema: u8,
    fonts: Vec<LocalFont>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
struct LocalFont {
    path: PathBuf,
    file_name: String,
    file_size: u64,
    content_sha256: String,
    metadata_hash: String,
    format: FontFormat,
}

impl LocalFont {
    fn to_manifest_entry(&self) -> FontManifestEntry {
        let now = Utc::now();
        FontManifestEntry {
            id: stable_font_id(&self.content_sha256),
            sha256: self.content_sha256.clone(),
            file_name: self.file_name.clone(),
            family_name: None,
            postscript_name: None,
            style_name: None,
            full_name: None,
            format: self.format.clone(),
            size_bytes: self.file_size,
            archived: false,
            created_at: now,
            updated_at: now,
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Scan { include_managed } => {
            print_json(&scan(include_managed)?)?;
        }
        Commands::Push { server, api_key } => {
            let report = push(&server, api_key.as_deref())?;
            print_json(&report)?;
        }
        Commands::Sync {
            server,
            api_key,
            dry_run,
        } => {
            let report = sync(&server, api_key.as_deref(), dry_run)?;
            print_json(&report)?;
        }
        Commands::LanServe { listen, lan_key } => {
            let runtime = tokio::runtime::Runtime::new().context("starting LAN peer runtime")?;
            runtime.block_on(lan_serve(listen, lan_key))?;
        }
        Commands::LanSync {
            peer,
            lan_key,
            dry_run,
        } => {
            let report = lan_sync(&peer, lan_key.as_deref(), dry_run)?;
            print_json(&report)?;
        }
        Commands::LanAddPeer { name, url, lan_key } => {
            let peer = add_lan_peer(name, url, lan_key)?;
            print_json(&peer)?;
        }
        Commands::LanPeers => {
            print_json(&load_app_config()?.peers)?;
        }
        Commands::LanSyncAll { dry_run } => {
            let report = lan_sync_all(dry_run)?;
            print_json(&report)?;
        }
    }
    Ok(())
}

fn scan(include_managed: bool) -> Result<ScanOutput> {
    let mut warnings = Vec::new();
    let user_font_dir = user_font_dir()?;
    let managed_dir = managed_font_dir()?;
    let mut fonts = Vec::new();

    if !user_font_dir.exists() {
        return Ok(ScanOutput {
            platform: platform_name(),
            schema: 1,
            fonts,
            warnings,
        });
    }

    for entry in WalkDir::new(&user_font_dir).follow_links(false).into_iter() {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };
        let path = entry.path();
        if entry.file_type().is_dir() {
            if !include_managed && path == managed_dir {
                continue;
            }
            continue;
        }
        if is_hidden(path) || is_temp_file(path) {
            continue;
        }
        let Some(file_name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        let format = FontFormat::from_file_name(&file_name);
        if !format.is_installable_desktop_font() {
            continue;
        }
        match inspect_font(path, file_name, format) {
            Ok(font) => fonts.push(font),
            Err(error) => warnings.push(format!("{}: {}", path.display(), error)),
        }
    }

    fonts.sort_by(|a, b| {
        a.file_name
            .cmp(&b.file_name)
            .then(a.content_sha256.cmp(&b.content_sha256))
    });
    Ok(ScanOutput {
        platform: platform_name(),
        schema: 1,
        fonts,
        warnings,
    })
}

#[derive(Debug, Serialize)]
struct PushReport {
    scanned: usize,
    registered: usize,
    uploaded: usize,
    skipped: usize,
    warnings: Vec<String>,
}

fn push(server: &str, api_key: Option<&str>) -> Result<PushReport> {
    let scan = scan(false)?;
    let client = http_client()?;
    let mut report = PushReport {
        scanned: scan.fonts.len(),
        registered: 0,
        uploaded: 0,
        skipped: 0,
        warnings: scan.warnings,
    };

    for font in scan.fonts {
        let request = RegisterFontRequest {
            sha256: font.content_sha256.clone(),
            file_name: font.file_name.clone(),
            family_name: None,
            postscript_name: None,
            style_name: None,
            full_name: None,
            format: font.format.clone(),
            size_bytes: font.file_size,
        };
        let response: RegisterFontResponse =
            authed(client.post(api_url(server, "/api/v1/fonts")?), api_key)
                .json(&request)
                .send()
                .context("registering font")?
                .error_for_status()
                .context("server rejected font registration")?
                .json()
                .context("parsing register response")?;

        report.registered += 1;
        if response.upload_required {
            let form = multipart::Form::new()
                .file("file", &font.path)
                .context("staging font upload")?;
            authed(
                client.post(api_url(
                    server,
                    &format!("/api/v1/fonts/{}/blob", font.content_sha256),
                )?),
                api_key,
            )
            .multipart(form)
            .send()
            .context("uploading font blob")?
            .error_for_status()
            .context("server rejected font upload")?;
            report.uploaded += 1;
        } else {
            report.skipped += 1;
        }
    }

    Ok(report)
}

#[derive(Debug, Serialize)]
struct SyncReport {
    known_local: usize,
    server_fonts: usize,
    installed: Vec<PathBuf>,
    skipped: Vec<String>,
    dry_run: bool,
}

fn sync(server: &str, api_key: Option<&str>, dry_run: bool) -> Result<SyncReport> {
    let client = http_client()?;
    let local = scan(true)?;
    let local_hashes = local
        .fonts
        .iter()
        .map(|font| font.content_sha256.clone())
        .collect::<Vec<_>>();
    let manifest: ManifestResponse = authed(client.get(api_url(server, "/api/v1/fonts")?), api_key)
        .send()
        .context("fetching server manifest")?
        .error_for_status()
        .context("server rejected manifest request")?
        .json()
        .context("parsing server manifest")?;

    let check_in = DeviceCheckInRequest {
        device_name: device_name(),
        os: platform_name().to_string(),
        installed_hashes: local_hashes.clone(),
    };
    let _ = authed(
        client.post(api_url(server, "/api/v1/devices/check-in")?),
        api_key,
    )
    .json(&check_in)
    .send()
    .context("checking device in")?
    .error_for_status()
    .context("server rejected device check-in")?;

    let local_hash_set = local_hashes
        .into_iter()
        .collect::<std::collections::HashSet<_>>();
    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    for font in &manifest.fonts {
        if local_hash_set.contains(&font.sha256) {
            skipped.push(format!("{} already present", font.file_name));
            continue;
        }
        if !font.format.is_installable_desktop_font() {
            skipped.push(format!("{} unsupported format", font.file_name));
            continue;
        }
        if dry_run {
            skipped.push(format!("would install {}", font.file_name));
            continue;
        }

        let bytes = authed(
            client.get(api_url(
                server,
                &format!("/api/v1/fonts/{}/blob", font.sha256),
            )?),
            api_key,
        )
        .send()
        .context("downloading font blob")?
        .error_for_status()
        .context("server rejected font download")?
        .bytes()
        .context("reading font bytes")?;
        let path = install_font(&font.file_name, &font.sha256, &bytes)?;
        installed.push(path);
    }

    Ok(SyncReport {
        known_local: local.fonts.len(),
        server_fonts: manifest.fonts.len(),
        installed,
        skipped,
        dry_run,
    })
}

#[derive(Debug, Clone)]
struct LanState {
    lan_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct LanSyncReport {
    known_local: usize,
    peer_fonts: usize,
    installed: Vec<PathBuf>,
    skipped: Vec<String>,
    dry_run: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppConfig {
    schema: u8,
    device_id: Option<Uuid>,
    peers: Vec<LanPeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LanPeerConfig {
    name: String,
    url: String,
    lan_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct LanSyncAllReport {
    peers: Vec<LanPeerSyncReport>,
    dry_run: bool,
}

#[derive(Debug, Serialize)]
struct LanPeerSyncReport {
    name: String,
    url: String,
    ok: bool,
    installed: Vec<PathBuf>,
    skipped: Vec<String>,
    error: Option<String>,
}

async fn lan_serve(listen: SocketAddr, lan_key: Option<String>) -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let state = Arc::new(LanState { lan_key });
    let app = Router::new()
        .route("/health", get(lan_health))
        .route("/api/lan/v1/health", get(lan_health))
        .route("/api/lan/v1/manifest", get(lan_manifest))
        .route("/api/lan/v1/blobs/{sha256}", get(lan_blob))
        .route("/api/lan/v1/fonts/{sha256}/blob", get(lan_blob))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding LAN peer listener at {listen}"))?;
    tracing::info!("syncmyfonts LAN peer listening on http://{listen}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn lan_health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        api_version: API_VERSION,
    })
}

async fn lan_manifest(
    State(state): State<Arc<LanState>>,
    headers: HeaderMap,
) -> Result<Json<ManifestResponse>, LanApiError> {
    authorize_lan(&state, &headers)?;
    let scan = scan(true).map_err(LanApiError::internal)?;
    let fonts = scan
        .fonts
        .into_iter()
        .map(|font| font.to_manifest_entry())
        .collect();
    Ok(Json(ManifestResponse { fonts }))
}

async fn lan_blob(
    State(state): State<Arc<LanState>>,
    headers: HeaderMap,
    AxumPath(sha256): AxumPath<String>,
) -> Result<Response, LanApiError> {
    authorize_lan(&state, &headers)?;
    validate_sha256(&sha256).map_err(LanApiError::bad_request)?;
    let font = find_local_font_by_hash(&sha256).map_err(LanApiError::internal)?;
    let Some(font) = font else {
        return Err(LanApiError::not_found("font blob not found"));
    };
    let bytes = fs::read(&font.path).map_err(LanApiError::internal)?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != sha256 {
        return Err(LanApiError::internal("local font hash changed during read"));
    }
    Response::builder()
        .header("content-type", "application/octet-stream")
        .body(Body::from(bytes))
        .map_err(|error| LanApiError::internal(error.to_string()))
}

fn lan_sync(peer: &str, lan_key: Option<&str>, dry_run: bool) -> Result<LanSyncReport> {
    let client = http_client()?;
    let local = scan(true)?;
    let local_hash_set = local
        .fonts
        .iter()
        .map(|font| font.content_sha256.clone())
        .collect::<std::collections::HashSet<_>>();
    let manifest: ManifestResponse =
        lan_authed(client.get(api_url(peer, "/api/lan/v1/manifest")?), lan_key)
            .send()
            .context("fetching LAN peer manifest")?
            .error_for_status()
            .context("LAN peer rejected manifest request")?
            .json()
            .context("parsing LAN peer manifest")?;

    let mut installed = Vec::new();
    let mut skipped = Vec::new();

    for font in &manifest.fonts {
        if local_hash_set.contains(&font.sha256) {
            skipped.push(format!("{} already present", font.file_name));
            continue;
        }
        if !font.format.is_installable_desktop_font() {
            skipped.push(format!("{} unsupported format", font.file_name));
            continue;
        }
        if dry_run {
            skipped.push(format!("would install {}", font.file_name));
            continue;
        }
        let bytes = lan_authed(
            client.get(api_url(
                peer,
                &format!("/api/lan/v1/blobs/{}", font.sha256),
            )?),
            lan_key,
        )
        .send()
        .context("downloading LAN peer font blob")?
        .error_for_status()
        .context("LAN peer rejected font download")?
        .bytes()
        .context("reading LAN peer font bytes")?;
        let path = install_font(&font.file_name, &font.sha256, &bytes)?;
        installed.push(path);
    }

    Ok(LanSyncReport {
        known_local: local.fonts.len(),
        peer_fonts: manifest.fonts.len(),
        installed,
        skipped,
        dry_run,
    })
}

fn add_lan_peer(name: String, url: String, lan_key: Option<String>) -> Result<LanPeerConfig> {
    let mut config = load_app_config()?;
    let peer = LanPeerConfig {
        name,
        url: normalize_peer_url(&url),
        lan_key,
    };
    if let Some(existing) = config
        .peers
        .iter_mut()
        .find(|existing| existing.name == peer.name)
    {
        *existing = peer.clone();
    } else {
        config.peers.push(peer.clone());
    }
    save_app_config(&config)?;
    Ok(peer)
}

fn lan_sync_all(dry_run: bool) -> Result<LanSyncAllReport> {
    let config = load_app_config()?;
    let mut peers = Vec::new();
    for peer in config.peers {
        match lan_sync(&peer.url, peer.lan_key.as_deref(), dry_run) {
            Ok(report) => peers.push(LanPeerSyncReport {
                name: peer.name,
                url: peer.url,
                ok: true,
                installed: report.installed,
                skipped: report.skipped,
                error: None,
            }),
            Err(error) => peers.push(LanPeerSyncReport {
                name: peer.name,
                url: peer.url,
                ok: false,
                installed: Vec::new(),
                skipped: Vec::new(),
                error: Some(error.to_string()),
            }),
        }
    }
    Ok(LanSyncAllReport { peers, dry_run })
}

fn find_local_font_by_hash(sha256: &str) -> Result<Option<LocalFont>> {
    Ok(scan(true)?
        .fonts
        .into_iter()
        .find(|font| font.content_sha256 == sha256))
}

fn load_app_config() -> Result<AppConfig> {
    let path = app_config_path()?;
    if !path.exists() {
        return Ok(AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            peers: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut config: AppConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if config.schema == 0 {
        config.schema = 1;
    }
    if config.device_id.is_none() {
        config.device_id = Some(Uuid::new_v4());
        save_app_config(&config)?;
    }
    Ok(config)
}

fn save_app_config(config: &AppConfig) -> Result<()> {
    let path = app_config_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(config).context("serializing app config")?;
    fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, &path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn app_config_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use directories::UserDirs;
        let home = UserDirs::new()
            .ok_or_else(|| anyhow!("user home directory unavailable"))?
            .home_dir()
            .to_path_buf();
        return Ok(home
            .join("Library/Application Support/SyncMyFonts")
            .join("config.json"));
    }
    #[cfg(target_os = "windows")]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("APPDATA unavailable"))?;
        return Ok(base.config_dir().join("SyncMyFonts").join("config.json"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("user config directory unavailable"))?;
        Ok(base.config_dir().join("syncmyfonts").join("config.json"))
    }
}

fn normalize_peer_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn inspect_font(path: &Path, file_name: String, format: FontFormat) -> Result<LocalFont> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let content_sha256 = hex::encode(Sha256::digest(&bytes));
    let file_size = bytes.len() as u64;
    let metadata_hash = metadata_hash(&file_name, file_size, &content_sha256)?;
    Ok(LocalFont {
        path: path.to_path_buf(),
        file_name,
        file_size,
        content_sha256,
        metadata_hash,
        format,
    })
}

fn metadata_hash(file_name: &str, file_size: u64, content_sha256: &str) -> Result<String> {
    let mut metadata = BTreeMap::new();
    metadata.insert("schema", "1".to_string());
    metadata.insert("file_name_lower", file_name.to_ascii_lowercase());
    metadata.insert("file_size", file_size.to_string());
    metadata.insert("content_sha256", content_sha256.to_string());
    let json = serde_json::to_vec(&metadata)?;
    Ok(hex::encode(Sha256::digest(json)))
}

fn install_font(remote_file_name: &str, expected_sha256: &str, bytes: &[u8]) -> Result<PathBuf> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual != expected_sha256 {
        bail!("hash-mismatch: downloaded font did not match expected sha256");
    }

    let format = FontFormat::from_file_name(remote_file_name);
    if !format.is_installable_desktop_font() {
        bail!("unsupported-format: {}", remote_file_name);
    }

    let install_dir = managed_install_dir()?;
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let destination = unique_destination(&install_dir, remote_file_name, expected_sha256)?;
    let temp = destination.with_extension(format!(
        "{}.tmp",
        destination
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("font")
    ));
    {
        let mut file =
            fs::File::create(&temp).with_context(|| format!("creating {}", temp.display()))?;
        file.write_all(bytes)
            .with_context(|| format!("writing {}", temp.display()))?;
        file.sync_all().ok();
    }
    fs::rename(&temp, &destination)
        .with_context(|| format!("installing {}", destination.display()))?;
    platform_post_install(&destination)?;
    Ok(destination)
}

fn unique_destination(
    install_dir: &Path,
    remote_file_name: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let safe_name = safe_file_name(remote_file_name, expected_sha256);
    let candidate = install_dir.join(&safe_name);
    if !candidate.exists() {
        return Ok(candidate);
    }
    if hex::encode(Sha256::digest(fs::read(&candidate)?)) == expected_sha256 {
        return Ok(candidate);
    }
    let extension = Path::new(&safe_name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("ttf");
    let stem = Path::new(&safe_name)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("font");
    Ok(install_dir.join(format!(
        "{}.syncmyfonts-{}.{}",
        stem,
        &expected_sha256[..8],
        extension
    )))
}

fn safe_file_name(remote_file_name: &str, expected_sha256: &str) -> String {
    let name = Path::new(remote_file_name)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("font.ttf");
    let mut cleaned = String::new();
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            cleaned.push(ch);
        } else {
            cleaned.push('-');
        }
    }
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        format!("font-{}.ttf", &expected_sha256[..12])
    } else {
        cleaned
    }
}

fn user_font_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        use directories::UserDirs;
        let home = UserDirs::new()
            .ok_or_else(|| anyhow!("user home directory unavailable"))?
            .home_dir()
            .to_path_buf();
        return Ok(home.join("Library/Fonts"));
    }
    #[cfg(target_os = "windows")]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("LOCALAPPDATA unavailable"))?;
        return Ok(base.data_local_dir().join("Microsoft/Windows/Fonts"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("user data directory unavailable"))?;
        Ok(base.data_local_dir().join("syncmyfonts/fonts"))
    }
}

fn managed_font_dir() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Ok(user_font_dir()?.join("SyncMyFonts"))
    }
    #[cfg(target_os = "windows")]
    {
        user_font_dir()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        user_font_dir()
    }
}

fn managed_install_dir() -> Result<PathBuf> {
    managed_font_dir()
}

fn platform_post_install(path: &Path) -> Result<()> {
    #[cfg(target_os = "windows")]
    {
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or("SyncMyFonts Font");
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| anyhow!("invalid installed font path"))?;
        let status = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
                "/v",
                &format!("{} (SyncMyFonts)", stem),
                "/t",
                "REG_SZ",
                "/d",
                file_name,
                "/f",
            ])
            .status()
            .context("registering font in HKCU")?;
        if !status.success() {
            bail!("RegistryWriteFailed: reg.exe returned {}", status);
        }
    }
    #[cfg(target_os = "macos")]
    {
        let _ = path;
        eprintln!(
            "Installed font. Some macOS apps may need to be restarted before the font appears."
        );
    }
    Ok(())
}

fn api_url(server: &str, path: &str) -> Result<String> {
    let base = server.trim_end_matches('/');
    Ok(format!("{}{}", base, path))
}

fn http_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(120))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .context("building HTTP client")
}

fn authed(
    builder: reqwest::blocking::RequestBuilder,
    api_key: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match api_key {
        Some(api_key) => builder.header(DEFAULT_API_KEY_HEADER, api_key),
        None => builder,
    }
}

fn lan_authed(
    builder: reqwest::blocking::RequestBuilder,
    lan_key: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match lan_key {
        Some(lan_key) => builder.header(DEFAULT_API_KEY_HEADER, lan_key),
        None => builder,
    }
}

fn authorize_lan(state: &LanState, headers: &HeaderMap) -> Result<(), LanApiError> {
    let Some(expected) = &state.lan_key else {
        return Ok(());
    };
    let provided = headers
        .get(DEFAULT_API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| LanApiError::unauthorized("missing LAN key"))?;
    if provided == expected {
        Ok(())
    } else {
        Err(LanApiError::unauthorized("invalid LAN key"))
    }
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() == 64 && value.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(())
    } else {
        bail!("sha256 must be a 64-character hex string")
    }
}

fn stable_font_id(sha256: &str) -> Uuid {
    let mut bytes = [0_u8; 16];
    if let Ok(decoded) = hex::decode(sha256) {
        for (index, byte) in decoded.into_iter().take(16).enumerate() {
            bytes[index] = byte;
        }
    }
    Uuid::from_bytes(bytes)
}

struct LanApiError {
    status: StatusCode,
    message: String,
}

impl LanApiError {
    fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: error.to_string(),
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

    fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: error.to_string(),
        }
    }
}

impl IntoResponse for LanApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

fn platform_name() -> &'static str {
    std::env::consts::OS
}

fn device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "unknown-device".to_string())
}

fn is_hidden(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

fn is_temp_file(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| {
            matches!(
                ext.to_ascii_lowercase().as_str(),
                "tmp" | "download" | "partial"
            )
        })
        .unwrap_or(false)
}
