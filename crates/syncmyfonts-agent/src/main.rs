use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    net::{SocketAddr, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const LAN_DISCOVERY_REQUEST: &[u8] = b"SYNCMYFONTS_DISCOVER_V1";
const LAN_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1400);
const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);

use anyhow::{Context, Result, anyhow, bail};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
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
        #[arg(long, env = "SYNCMYFONTS_PAIRING_CODE")]
        pairing_code: Option<String>,
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
    /// Discover SyncMyFonts peers sharing fonts on this LAN.
    LanDiscover {
        #[arg(long, default_value = "7370")]
        port: u16,
    },
    /// Pull missing fonts from every saved LAN peer.
    LanSyncAll {
        #[arg(long)]
        dry_run: bool,
    },
    /// Print a redacted support report for troubleshooting.
    Diagnostics,
    /// Run the local desktop control surface.
    App {
        #[arg(long, default_value = "127.0.0.1:7380")]
        listen: SocketAddr,
        #[arg(long)]
        no_open: bool,
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
        Commands::LanServe {
            listen,
            lan_key,
            pairing_code,
        } => {
            let runtime = tokio::runtime::Runtime::new().context("starting LAN peer runtime")?;
            runtime.block_on(lan_serve(listen, lan_key, pairing_code))?;
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
        Commands::LanDiscover { port } => {
            print_json(&discover_lan_peers(port)?)?;
        }
        Commands::LanSyncAll { dry_run } => {
            let report = lan_sync_all(dry_run)?;
            print_json(&report)?;
        }
        Commands::Diagnostics => {
            print_json(&diagnostics()?)?;
        }
        Commands::App { listen, no_open } => {
            let runtime = tokio::runtime::Runtime::new().context("starting app runtime")?;
            runtime.block_on(app_serve(listen, !no_open))?;
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
        record_managed_install(
            &font.file_name,
            &font.sha256,
            &path,
            &format!("server:{server}"),
            bytes.len() as u64,
        )?;
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
    lan_key: String,
    pairing: Option<PairingState>,
}

#[derive(Debug, Clone)]
struct PairingState {
    code: String,
    expires_at: Instant,
}

#[derive(Debug, Serialize, Deserialize)]
struct LanPairRequest {
    pairing_code: String,
    device_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct LanPairResponse {
    lan_key: String,
    device_name: String,
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

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LanDiscoveryWireResponse {
    schema: u8,
    api_version: String,
    device_name: String,
    port: u16,
    requires_lan_key: bool,
}

#[derive(Debug, Clone, Serialize)]
struct LanDiscoveredPeer {
    name: String,
    url: String,
    requires_lan_key: bool,
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

#[derive(Clone)]
struct AppState {
    share: Arc<Mutex<Option<RunningShare>>>,
}

struct RunningShare {
    child: Child,
    listen: SocketAddr,
}

#[derive(Debug, Serialize)]
struct AppStatus {
    platform: &'static str,
    device_name: String,
    config_path: PathBuf,
    user_font_dir: PathBuf,
    managed_font_dir: PathBuf,
    sharing: bool,
    share_urls: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AddPeerRequest {
    name: String,
    url: String,
    lan_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PairPeerRequest {
    name: String,
    url: String,
    pairing_code: String,
}

#[derive(Debug, Deserialize)]
struct PeerSyncRequest {
    url: String,
    lan_key: Option<String>,
    dry_run: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct ShareRequest {
    listen: Option<String>,
    lan_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct ShareResponse {
    sharing: bool,
    message: String,
    urls: Vec<String>,
    pairing_code: Option<String>,
    pairing_expires_seconds: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct DiscoverPeersRequest {
    port: Option<u16>,
}

#[derive(Debug, Serialize)]
struct PeerTestResponse {
    ok: bool,
    message: String,
    peer_fonts: usize,
    would_install_or_skip: usize,
}

#[derive(Debug, Serialize)]
struct DiagnosticsReport {
    version: &'static str,
    platform: &'static str,
    device_name: String,
    config_path: PathBuf,
    managed_manifest_path: PathBuf,
    user_font_dir: PathBuf,
    managed_font_dir: PathBuf,
    saved_peer_count: usize,
    saved_peers: Vec<RedactedPeer>,
    user_font_count: usize,
    managed_manifest_count: usize,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RedactedPeer {
    name: String,
    url: String,
    has_lan_key: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ManagedManifest {
    schema: u8,
    installed: Vec<ManagedFontRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManagedFontRecord {
    sha256: String,
    file_name: String,
    path: PathBuf,
    source: String,
    installed_at: String,
    size_bytes: u64,
}

async fn lan_serve(
    listen: SocketAddr,
    lan_key: Option<String>,
    pairing_code: Option<String>,
) -> Result<()> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let provided_lan_key = lan_key.filter(|key| !key.trim().is_empty());
    let pairing_code = pairing_code
        .filter(|code| !code.trim().is_empty())
        .map(|code| normalize_pairing_code(&code))
        .or_else(|| {
            if provided_lan_key.is_some() {
                None
            } else {
                generate_pairing_code()
            }
        });
    let lan_key = provided_lan_key.unwrap_or_else(generate_lan_token);
    if let Some(code) = &pairing_code {
        eprintln!("SyncMyFonts pairing code: {code}");
    }
    let state = Arc::new(LanState {
        lan_key,
        pairing: pairing_code.map(|code| PairingState {
            code,
            expires_at: Instant::now() + PAIRING_CODE_TTL,
        }),
    });
    spawn_lan_discovery_responder(listen);
    let app = Router::new()
        .route("/health", get(lan_health))
        .route("/api/lan/v1/health", get(lan_health))
        .route("/api/lan/v1/pair", post(lan_pair))
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

fn spawn_lan_discovery_responder(listen: SocketAddr) {
    tokio::spawn(async move {
        if let Err(error) = lan_discovery_responder(listen).await {
            tracing::warn!("LAN discovery responder stopped: {error}");
        }
    });
}

async fn lan_discovery_responder(listen: SocketAddr) -> Result<()> {
    let socket = tokio::net::UdpSocket::bind(listen)
        .await
        .with_context(|| format!("binding LAN discovery responder at {listen}"))?;
    let mut buffer = [0_u8; 512];
    loop {
        let (length, sender) = socket
            .recv_from(&mut buffer)
            .await
            .context("receiving LAN discovery packet")?;
        if &buffer[..length] != LAN_DISCOVERY_REQUEST {
            continue;
        }
        let response = LanDiscoveryWireResponse {
            schema: 1,
            api_version: API_VERSION.to_string(),
            device_name: device_name(),
            port: listen.port(),
            requires_lan_key: true,
        };
        let bytes = serde_json::to_vec(&response).context("serializing LAN discovery response")?;
        socket
            .send_to(&bytes, sender)
            .await
            .context("sending LAN discovery response")?;
    }
}

async fn lan_health() -> Json<HealthResponse> {
    Json(HealthResponse {
        ok: true,
        api_version: API_VERSION,
    })
}

async fn lan_pair(
    State(state): State<Arc<LanState>>,
    Json(request): Json<LanPairRequest>,
) -> Result<Json<LanPairResponse>, LanApiError> {
    let Some(pairing) = &state.pairing else {
        return Err(LanApiError::unauthorized("pairing is not enabled"));
    };
    if Instant::now() > pairing.expires_at {
        return Err(LanApiError::unauthorized("pairing code expired"));
    }
    if normalize_pairing_code(&request.pairing_code) != pairing.code {
        return Err(LanApiError::unauthorized("invalid pairing code"));
    }
    let _requesting_device = request.device_name;
    Ok(Json(LanPairResponse {
        lan_key: state.lan_key.clone(),
        device_name: device_name(),
    }))
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
        record_managed_install(
            &font.file_name,
            &font.sha256,
            &path,
            &format!("lan:{peer}"),
            bytes.len() as u64,
        )?;
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

fn pair_lan_peer(name: String, url: String, pairing_code: String) -> Result<LanPeerConfig> {
    let url = normalize_peer_url(&url);
    let response: LanPairResponse = http_client()?
        .post(api_url(&url, "/api/lan/v1/pair")?)
        .json(&LanPairRequest {
            pairing_code,
            device_name: Some(device_name()),
        })
        .send()
        .context("sending LAN pairing request")?
        .error_for_status()
        .context("LAN peer rejected pairing request")?
        .json()
        .context("parsing LAN pairing response")?;
    let peer_name = if name.trim().is_empty() {
        response.device_name
    } else {
        name
    };
    add_lan_peer(peer_name, url, Some(response.lan_key))
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

fn discover_lan_peers(port: u16) -> Result<Vec<LanDiscoveredPeer>> {
    let socket = UdpSocket::bind("0.0.0.0:0").context("binding LAN discovery socket")?;
    socket
        .set_broadcast(true)
        .context("enabling LAN discovery broadcast")?;
    socket
        .set_read_timeout(Some(LAN_DISCOVERY_TIMEOUT))
        .context("setting LAN discovery timeout")?;
    socket
        .send_to(LAN_DISCOVERY_REQUEST, ("255.255.255.255", port))
        .with_context(|| format!("broadcasting LAN discovery on UDP port {port}"))?;

    let started = Instant::now();
    let mut peers = BTreeMap::new();
    let mut buffer = [0_u8; 1024];
    while started.elapsed() < LAN_DISCOVERY_TIMEOUT {
        match socket.recv_from(&mut buffer) {
            Ok((length, sender)) => {
                let response =
                    match serde_json::from_slice::<LanDiscoveryWireResponse>(&buffer[..length]) {
                        Ok(response) => response,
                        Err(_) => continue,
                    };
                if response.schema != 1 || response.api_version != API_VERSION {
                    continue;
                }
                let url = format!("http://{}:{}", sender.ip(), response.port);
                peers.insert(
                    url.clone(),
                    LanDiscoveredPeer {
                        name: response.device_name,
                        url,
                        requires_lan_key: response.requires_lan_key,
                    },
                );
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => return Err(error).context("receiving LAN discovery response"),
        }
    }

    Ok(peers.into_values().collect())
}

fn diagnostics() -> Result<DiagnosticsReport> {
    let config = load_app_config()?;
    let scan = scan(true)?;
    let manifest_result = load_managed_manifest();
    let managed_manifest_count = manifest_result
        .as_ref()
        .map(|manifest| manifest.installed.len())
        .unwrap_or(0);
    let saved_peers = config
        .peers
        .iter()
        .map(|peer| RedactedPeer {
            name: peer.name.clone(),
            url: peer.url.clone(),
            has_lan_key: peer.lan_key.is_some(),
        })
        .collect::<Vec<_>>();
    Ok(DiagnosticsReport {
        version: env!("CARGO_PKG_VERSION"),
        platform: platform_name(),
        device_name: device_name(),
        config_path: app_config_path()?,
        managed_manifest_path: managed_manifest_path()?,
        user_font_dir: user_font_dir()?,
        managed_font_dir: managed_font_dir()?,
        saved_peer_count: config.peers.len(),
        saved_peers,
        user_font_count: scan.fonts.len(),
        managed_manifest_count,
        warnings: diagnostics_warnings(scan.warnings, manifest_result),
    })
}

async fn app_serve(listen: SocketAddr, open_browser_on_start: bool) -> Result<()> {
    let state = AppState {
        share: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/", get(app_index))
        .route("/api/status", get(app_status))
        .route("/api/scan", get(app_scan))
        .route("/api/diagnostics", get(app_diagnostics))
        .route("/api/peers", get(app_peers).post(app_add_peer))
        .route("/api/peers/discover", post(app_discover_peers))
        .route("/api/peer/pair", post(app_pair_peer))
        .route("/api/peer/test", post(app_peer_test))
        .route("/api/peer/sync", post(app_peer_sync))
        .route("/api/sync-all", post(app_sync_all))
        .route("/api/sync-all/dry-run", post(app_sync_all_dry_run))
        .route("/api/share/start", post(app_share_start))
        .route("/api/share/stop", post(app_share_stop))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("binding app control surface at {listen}"))?;
    eprintln!("SyncMyFonts app running at http://{listen}");
    if open_browser_on_start {
        open_browser(&format!("http://{listen}"));
    }
    axum::serve(listener, app).await?;
    Ok(())
}

async fn app_index() -> Html<&'static str> {
    Html(APP_HTML)
}

async fn app_status(State(state): State<AppState>) -> Result<Json<AppStatus>, LanApiError> {
    let share_listen = current_share_listen(&state)?;
    Ok(Json(AppStatus {
        platform: platform_name(),
        device_name: device_name(),
        config_path: app_config_path().map_err(LanApiError::internal)?,
        user_font_dir: user_font_dir().map_err(LanApiError::internal)?,
        managed_font_dir: managed_font_dir().map_err(LanApiError::internal)?,
        sharing: share_listen.is_some(),
        share_urls: share_listen.map(share_urls).unwrap_or_default(),
    }))
}

async fn app_scan() -> Result<Json<ScanOutput>, LanApiError> {
    scan(true).map(Json).map_err(LanApiError::internal)
}

async fn app_diagnostics() -> Result<Json<DiagnosticsReport>, LanApiError> {
    diagnostics().map(Json).map_err(LanApiError::internal)
}

async fn app_peers() -> Result<Json<Vec<LanPeerConfig>>, LanApiError> {
    load_app_config()
        .map(|config| Json(config.peers))
        .map_err(LanApiError::internal)
}

async fn app_add_peer(
    Json(request): Json<AddPeerRequest>,
) -> Result<Json<LanPeerConfig>, LanApiError> {
    add_lan_peer(request.name, request.url, request.lan_key)
        .map(Json)
        .map_err(LanApiError::internal)
}

async fn app_discover_peers(
    Json(request): Json<DiscoverPeersRequest>,
) -> Result<Json<Vec<LanDiscoveredPeer>>, LanApiError> {
    let port = request.port.unwrap_or(7370);
    tokio::task::spawn_blocking(move || discover_lan_peers(port))
        .await
        .map_err(LanApiError::internal)?
        .map(Json)
        .map_err(LanApiError::internal)
}

async fn app_pair_peer(
    Json(request): Json<PairPeerRequest>,
) -> Result<Json<LanPeerConfig>, LanApiError> {
    tokio::task::spawn_blocking(move || {
        pair_lan_peer(request.name, request.url, request.pairing_code)
    })
    .await
    .map_err(LanApiError::internal)?
    .map(Json)
    .map_err(LanApiError::internal)
}

async fn app_peer_test(
    Json(request): Json<PeerSyncRequest>,
) -> Result<Json<PeerTestResponse>, LanApiError> {
    let url = request.url;
    let lan_key = request.lan_key;
    let report = tokio::task::spawn_blocking(move || lan_sync(&url, lan_key.as_deref(), true))
        .await
        .map_err(LanApiError::internal)?
        .map_err(LanApiError::internal)?;
    Ok(Json(PeerTestResponse {
        ok: true,
        message: format!("Connected. Peer reported {} fonts.", report.peer_fonts),
        peer_fonts: report.peer_fonts,
        would_install_or_skip: report.skipped.len(),
    }))
}

async fn app_peer_sync(
    Json(request): Json<PeerSyncRequest>,
) -> Result<Json<LanSyncReport>, LanApiError> {
    let url = request.url;
    let lan_key = request.lan_key;
    let dry_run = request.dry_run.unwrap_or(false);
    tokio::task::spawn_blocking(move || lan_sync(&url, lan_key.as_deref(), dry_run))
        .await
        .map_err(LanApiError::internal)?
        .map(Json)
        .map_err(LanApiError::internal)
}

async fn app_sync_all() -> Result<Json<LanSyncAllReport>, LanApiError> {
    tokio::task::spawn_blocking(|| lan_sync_all(false))
        .await
        .map_err(LanApiError::internal)?
        .map(Json)
        .map_err(LanApiError::internal)
}

async fn app_sync_all_dry_run() -> Result<Json<LanSyncAllReport>, LanApiError> {
    tokio::task::spawn_blocking(|| lan_sync_all(true))
        .await
        .map_err(LanApiError::internal)?
        .map(Json)
        .map_err(LanApiError::internal)
}

async fn app_share_start(
    State(state): State<AppState>,
    Json(request): Json<ShareRequest>,
) -> Result<Json<ShareResponse>, LanApiError> {
    let mut guard = state
        .share
        .lock()
        .map_err(|_| LanApiError::internal("share state lock poisoned"))?;
    if guard.is_some() {
        let urls = guard
            .as_ref()
            .map(|share| share_urls(share.listen))
            .unwrap_or_default();
        return Ok(Json(ShareResponse {
            sharing: true,
            message: "Already sharing fonts on the LAN.".to_string(),
            urls,
            pairing_code: None,
            pairing_expires_seconds: None,
        }));
    }
    let exe = std::env::current_exe().map_err(LanApiError::internal)?;
    let listen: SocketAddr = request
        .listen
        .unwrap_or_else(|| "0.0.0.0:7370".to_string())
        .parse()
        .map_err(LanApiError::bad_request)?;
    let mut command = Command::new(exe);
    command.args(["lan-serve", "--listen", &listen.to_string()]);
    let provided_key = request.lan_key.filter(|key| !key.trim().is_empty());
    let pairing_code = if provided_key.is_some() {
        None
    } else {
        generate_pairing_code()
    };
    let lan_key = provided_key.unwrap_or_else(generate_lan_token);
    command.env("SYNCMYFONTS_LAN_KEY", lan_key);
    if let Some(code) = &pairing_code {
        command.env("SYNCMYFONTS_PAIRING_CODE", code);
    }
    let child = command.spawn().map_err(LanApiError::internal)?;
    let child = wait_for_share_start(child, listen).map_err(LanApiError::internal)?;
    let urls = share_urls(listen);
    *guard = Some(RunningShare { child, listen });
    let pairing_expires_seconds = pairing_code.as_ref().map(|_| PAIRING_CODE_TTL.as_secs());
    Ok(Json(ShareResponse {
        sharing: true,
        message: format!("Sharing fonts at {}.", urls.join(", ")),
        urls,
        pairing_code,
        pairing_expires_seconds,
    }))
}

async fn app_share_stop(State(state): State<AppState>) -> Result<Json<ShareResponse>, LanApiError> {
    let mut guard = state
        .share
        .lock()
        .map_err(|_| LanApiError::internal("share state lock poisoned"))?;
    let Some(mut share) = guard.take() else {
        return Ok(Json(ShareResponse {
            sharing: false,
            message: "Sharing was not running.".to_string(),
            urls: Vec::new(),
            pairing_code: None,
            pairing_expires_seconds: None,
        }));
    };
    let _ = share.child.kill();
    let _ = share.child.wait();
    Ok(Json(ShareResponse {
        sharing: false,
        message: "Stopped sharing fonts.".to_string(),
        urls: Vec::new(),
        pairing_code: None,
        pairing_expires_seconds: None,
    }))
}

fn current_share_listen(state: &AppState) -> Result<Option<SocketAddr>, LanApiError> {
    let mut guard = state
        .share
        .lock()
        .map_err(|_| LanApiError::internal("share state lock poisoned"))?;
    let Some(share) = guard.as_mut() else {
        return Ok(None);
    };
    if let Ok(Some(_status)) = share.child.try_wait() {
        *guard = None;
        return Ok(None);
    }
    Ok(guard.as_ref().map(|share| share.listen))
}

fn wait_for_share_start(mut child: Child, listen: SocketAddr) -> Result<Child> {
    let probe = SocketAddr::from(([127, 0, 0, 1], listen.port()));
    let started = Instant::now();
    loop {
        if let Some(status) = child
            .try_wait()
            .context("checking LAN share child status")?
        {
            bail!("LAN share exited before it was ready: {status}");
        }
        if TcpStream::connect_timeout(&probe, Duration::from_millis(150)).is_ok() {
            return Ok(child);
        }
        if started.elapsed() > Duration::from_secs(3) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("LAN share did not answer at {probe}");
        }
        thread::sleep(Duration::from_millis(75));
    }
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
        if let Some(legacy_path) = legacy_app_config_path()? {
            if legacy_path.exists() {
                let bytes = fs::read(&legacy_path)
                    .with_context(|| format!("reading {}", legacy_path.display()))?;
                let mut config: AppConfig = serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", legacy_path.display()))?;
                normalize_app_config(&mut config);
                save_app_config(&config)?;
                return Ok(config);
            }
        }
        return Ok(AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            peers: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut config: AppConfig =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if normalize_app_config(&mut config) {
        save_app_config(&config)?;
    }
    Ok(config)
}

fn normalize_app_config(config: &mut AppConfig) -> bool {
    let mut changed = false;
    if config.schema == 0 {
        config.schema = 1;
        changed = true;
    }
    if config.device_id.is_none() {
        config.device_id = Some(Uuid::new_v4());
        changed = true;
    }
    changed
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
    Ok(app_data_dir()?.join("config.json"))
}

fn managed_manifest_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("managed-fonts.json"))
}

fn app_data_dir() -> Result<PathBuf> {
    if let Ok(config_dir) = std::env::var("SYNCMYFONTS_CONFIG_DIR") {
        return Ok(PathBuf::from(config_dir));
    }

    #[cfg(target_os = "macos")]
    {
        use directories::UserDirs;
        let home = UserDirs::new()
            .ok_or_else(|| anyhow!("user home directory unavailable"))?
            .home_dir()
            .to_path_buf();
        return Ok(home.join("Library/Application Support/SyncMyFonts"));
    }
    #[cfg(target_os = "windows")]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("LOCALAPPDATA unavailable"))?;
        return Ok(base.data_local_dir().join("SyncMyFonts"));
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("user config directory unavailable"))?;
        Ok(base.config_dir().join("syncmyfonts"))
    }
}

fn legacy_app_config_path() -> Result<Option<PathBuf>> {
    #[cfg(target_os = "windows")]
    {
        if std::env::var("SYNCMYFONTS_CONFIG_DIR").is_ok() {
            return Ok(None);
        }
        use directories::BaseDirs;
        let base = BaseDirs::new().ok_or_else(|| anyhow!("APPDATA unavailable"))?;
        return Ok(Some(
            base.config_dir().join("SyncMyFonts").join("config.json"),
        ));
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(None)
    }
}

fn load_managed_manifest() -> Result<ManagedManifest> {
    let path = managed_manifest_path()?;
    if !path.exists() {
        return Ok(ManagedManifest {
            schema: 1,
            installed: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut manifest: ManagedManifest =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if manifest.schema == 0 {
        manifest.schema = 1;
    }
    Ok(manifest)
}

fn save_managed_manifest(manifest: &ManagedManifest) -> Result<()> {
    let path = managed_manifest_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(manifest).context("serializing managed font manifest")?;
    fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, &path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn record_managed_install(
    file_name: &str,
    sha256: &str,
    path: &Path,
    source: &str,
    size_bytes: u64,
) -> Result<()> {
    let mut manifest = load_managed_manifest()?;
    let record = ManagedFontRecord {
        sha256: sha256.to_string(),
        file_name: file_name.to_string(),
        path: path.to_path_buf(),
        source: source.to_string(),
        installed_at: Utc::now().to_rfc3339(),
        size_bytes,
    };
    if let Some(existing) = manifest
        .installed
        .iter_mut()
        .find(|existing| existing.sha256 == sha256)
    {
        *existing = record;
    } else {
        manifest.installed.push(record);
    }
    manifest
        .installed
        .sort_by(|a, b| a.file_name.cmp(&b.file_name));
    save_managed_manifest(&manifest)
}

fn diagnostics_warnings(
    mut warnings: Vec<String>,
    manifest_result: Result<ManagedManifest>,
) -> Vec<String> {
    if let Err(error) = manifest_result {
        warnings.push(format!("managed manifest unavailable: {error}"));
    }
    warnings
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
    if let Ok(font_dir) = std::env::var("SYNCMYFONTS_USER_FONT_DIR") {
        return Ok(PathBuf::from(font_dir));
    }

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
    if std::env::var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION").as_deref() == Ok("1") {
        let _ = path;
        return Ok(());
    }

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
        notify_windows_font_change();
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

#[cfg(target_os = "windows")]
fn notify_windows_font_change() {
    use windows_sys::Win32::Foundation::{LPARAM, WPARAM};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_FONTCHANGE,
    };

    let mut result = 0;
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_FONTCHANGE,
            WPARAM::default(),
            LPARAM::default(),
            SMTO_ABORTIFHUNG,
            5000,
            &mut result,
        );
    }
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
    let provided = headers
        .get(DEFAULT_API_KEY_HEADER)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| LanApiError::unauthorized("missing LAN key"))?;
    if provided == state.lan_key {
        Ok(())
    } else {
        Err(LanApiError::unauthorized("invalid LAN key"))
    }
}

fn generate_lan_token() -> String {
    format!("smf-{}", Uuid::new_v4().simple())
}

fn generate_pairing_code() -> Option<String> {
    let digits = u128::from_be_bytes(*Uuid::new_v4().as_bytes()) % 100_000_000;
    Some(format!("{digits:08}"))
}

fn normalize_pairing_code(code: &str) -> String {
    code.chars().filter(|ch| ch.is_ascii_digit()).collect()
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

fn share_urls(listen: SocketAddr) -> Vec<String> {
    let port = listen.port();
    let ip = listen.ip();
    if ip.is_unspecified() {
        let mut urls = Vec::new();
        if let Some(local_ip) = likely_lan_ip() {
            urls.push(format!("http://{local_ip}:{port}"));
        }
        urls.push(format!("http://127.0.0.1:{port}"));
        urls
    } else {
        vec![format!("http://{listen}")]
    }
}

fn likely_lan_ip() -> Option<std::net::IpAddr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("8.8.8.8:80").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() { None } else { Some(ip) }
}

fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    {
        let _ = Command::new("open").arg(url).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
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

const APP_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>SyncMyFonts</title>
  <style>
    :root {
      color-scheme: light dark;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      line-height: 1.4;
    }
    body {
      margin: 0;
      background: Canvas;
      color: CanvasText;
    }
    main {
      max-width: 980px;
      margin: 0 auto;
      padding: 28px;
    }
    header {
      display: flex;
      align-items: end;
      justify-content: space-between;
      gap: 16px;
      border-bottom: 1px solid color-mix(in oklab, CanvasText 18%, Canvas);
      padding-bottom: 18px;
      margin-bottom: 22px;
    }
    h1 {
      margin: 0;
      font-size: 30px;
    }
    h2 {
      margin: 0 0 12px;
      font-size: 18px;
    }
    section {
      padding: 18px 0;
      border-bottom: 1px solid color-mix(in oklab, CanvasText 12%, Canvas);
    }
    .grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
      gap: 12px;
    }
    label {
      display: grid;
      gap: 6px;
      font-size: 13px;
      color: color-mix(in oklab, CanvasText 72%, Canvas);
    }
    input {
      font: inherit;
      padding: 10px;
      border: 1px solid color-mix(in oklab, CanvasText 28%, Canvas);
      background: Canvas;
      color: CanvasText;
      border-radius: 6px;
    }
    button {
      font: inherit;
      padding: 10px 12px;
      border: 1px solid color-mix(in oklab, CanvasText 30%, Canvas);
      background: color-mix(in oklab, CanvasText 8%, Canvas);
      color: CanvasText;
      border-radius: 6px;
      cursor: pointer;
    }
    button.primary {
      background: #116149;
      border-color: #116149;
      color: white;
    }
    button.danger {
      background: #8f1f1f;
      border-color: #8f1f1f;
      color: white;
    }
    .row {
      display: flex;
      flex-wrap: wrap;
      gap: 10px;
      align-items: center;
    }
    .stack {
      display: grid;
      gap: 10px;
    }
    .statusline {
      margin-top: 10px;
      padding: 10px;
      border-radius: 6px;
      background: color-mix(in oklab, CanvasText 7%, Canvas);
      overflow-wrap: anywhere;
    }
    .nextstep {
      margin-top: 12px;
      padding: 12px;
      border-left: 4px solid #116149;
      border-radius: 6px;
      background: color-mix(in oklab, #116149 10%, Canvas);
      overflow-wrap: anywhere;
    }
    .result {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(130px, 1fr));
      gap: 10px;
      margin-top: 12px;
    }
    .metric {
      padding: 10px;
      border: 1px solid color-mix(in oklab, CanvasText 16%, Canvas);
      border-radius: 6px;
    }
    .metric strong {
      display: block;
      font-size: 22px;
    }
    pre {
      white-space: pre-wrap;
      overflow-wrap: anywhere;
      background: color-mix(in oklab, CanvasText 7%, Canvas);
      border-radius: 6px;
      padding: 12px;
      min-height: 120px;
    }
    .muted {
      color: color-mix(in oklab, CanvasText 64%, Canvas);
      font-size: 13px;
    }
  </style>
</head>
<body>
  <main>
    <header>
      <div>
        <h1>SyncMyFonts</h1>
        <div class="muted" id="status">Loading local app status...</div>
      </div>
      <button onclick="refresh()">Refresh</button>
    </header>

    <section>
      <h2>Local Font Library</h2>
      <div class="row">
        <button onclick="scanFonts()">Scan Fonts</button>
        <button onclick="diagnostics()">Diagnostics</button>
        <button class="primary" onclick="syncAll(false)">Sync Saved Peers</button>
        <button onclick="syncAll(true)">Dry Run</button>
      </div>
    </section>

    <section>
      <h2>Saved LAN Peer</h2>
      <div class="grid">
        <label>Name <input id="peerName" placeholder="Workshop PC"></label>
        <label>URL <input id="peerUrl" placeholder="http://192.168.1.50:7370"></label>
        <label>Shared Key <input id="peerKey" placeholder="saved after pairing"></label>
        <label>Pairing Code <input id="pairingCode" placeholder="8 digits from sharing computer"></label>
      </div>
      <p class="row">
        <button onclick="discoverPeers()">Find LAN Peers</button>
        <button class="primary" onclick="pairPeer()">Pair Peer</button>
        <button onclick="testPeer()">Test Peer</button>
        <button onclick="syncPeer(true)">Preview From Peer</button>
        <button onclick="syncPeer(false)">Get Missing Fonts</button>
        <button onclick="savePeer()">Save Peer</button>
        <button onclick="loadPeers()">List Peers</button>
      </p>
      <div id="discoveredPeers" class="statusline muted">No peers discovered yet.</div>
    </section>

    <section>
      <h2>Share This Device</h2>
      <div class="grid">
        <label>Listen Address <input id="listen" value="0.0.0.0:7370"></label>
        <label>Shared Key <input id="shareKey" placeholder="optional; blank creates pairing code"></label>
      </div>
      <p class="row">
        <button class="primary" onclick="startShare()">Share Fonts On LAN</button>
        <button class="danger" onclick="stopShare()">Stop Sharing</button>
      </p>
      <div id="shareUrls" class="statusline muted">Sharing is off.</div>
      <p class="muted">Only use sharing on trusted local networks. No port forwarding is required.</p>
    </section>

    <section>
      <h2>Result</h2>
      <div id="nextStep" class="nextstep">Start by sharing fonts on one computer, then find and pair from the other computer.</div>
      <div id="summary" class="result"></div>
      <pre id="output">Ready.</pre>
    </section>
  </main>
  <script>
    const out = document.getElementById('output');
    const summary = document.getElementById('summary');
    const nextStep = document.getElementById('nextStep');
    function setNextStep(message) {
      nextStep.textContent = message;
    }
    function show(value) {
      out.textContent = typeof value === 'string' ? value : JSON.stringify(value, null, 2);
    }
    function metric(label, value) {
      return `<div class="metric"><strong>${value}</strong>${label}</div>`;
    }
    function summarize(value) {
      summary.innerHTML = '';
      if (!value || typeof value !== 'object') return;
      if (Array.isArray(value.fonts)) {
        summary.innerHTML = metric('fonts found', value.fonts.length) + metric('warnings', value.warnings?.length ?? 0);
      } else if (Array.isArray(value.peers)) {
        const installed = value.peers.reduce((count, peer) => count + (peer.installed?.length ?? 0), 0);
        const failed = value.peers.filter(peer => !peer.ok).length;
        summary.innerHTML = metric('peers', value.peers.length) + metric('installed', installed) + metric('failed', failed);
      } else if (Array.isArray(value.installed)) {
        summary.innerHTML = metric('peer fonts', value.peer_fonts ?? 0) + metric('installed', value.installed.length) + metric('skipped', value.skipped?.length ?? 0);
      } else if (typeof value.peer_fonts === 'number') {
        summary.innerHTML = metric('peer fonts', value.peer_fonts) + metric('results', value.would_install_or_skip ?? 0);
      } else if (Array.isArray(value)) {
        summary.innerHTML = metric('items', value.length);
      }
    }
    function showResult(value) {
      summarize(value);
      show(value);
    }
    function showShareResult(value) {
      showResult(value);
      if (value?.pairing_code) {
        setNextStep(`Pairing code ${value.pairing_code} is ready for about ${Math.round((value.pairing_expires_seconds ?? 600) / 60)} minutes. On the other computer, click Find LAN Peers, select this device, enter the code, and click Pair Peer.`);
        out.textContent += `\n\nPairing code: ${value.pairing_code}\nValid for about ${Math.round((value.pairing_expires_seconds ?? 600) / 60)} minutes.`;
      } else if (value?.sharing) {
        setNextStep('Sharing is on. On the other computer, enter this URL and shared key, or use Find LAN Peers if it can discover this device.');
      }
    }
    async function request(path, options = {}) {
      const response = await fetch(path, options);
      const text = await response.text();
      let body;
      try { body = text ? JSON.parse(text) : null; } catch { body = text; }
      if (!response.ok) throw new Error(typeof body === 'string' ? body : JSON.stringify(body));
      return body;
    }
    async function refresh() {
      try {
        const status = await request('/api/status');
        document.getElementById('status').textContent =
          `${status.device_name} · ${status.platform} · sharing: ${status.sharing ? 'on' : 'off'}`;
        document.getElementById('shareUrls').textContent = status.share_urls.length
          ? `Use this URL from another computer: ${status.share_urls.join(' or ')}`
          : 'Sharing is off.';
      } catch (error) { show(error.message); }
    }
    async function scanFonts() {
      try { showResult(await request('/api/scan')); } catch (error) { show(error.message); }
    }
    async function diagnostics() {
      try { showResult(await request('/api/diagnostics')); } catch (error) { show(error.message); }
    }
    async function loadPeers() {
      try { showResult(await request('/api/peers')); } catch (error) { show(error.message); }
    }
    async function discoverPeers() {
      try {
        const listen = document.getElementById('listen').value || '0.0.0.0:7370';
        const port = Number(listen.split(':').pop()) || 7370;
        const peers = await request('/api/peers/discover', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ port })
        });
        const box = document.getElementById('discoveredPeers');
        if (!peers.length) {
          box.textContent = 'No sharing SyncMyFonts peers answered on this LAN.';
          setNextStep('No peers answered. Make sure the other computer is sharing, both computers are on the same trusted LAN/VPN, and Windows Firewall allows Private network access if Windows is sharing.');
        } else {
          box.textContent = '';
          for (const peer of peers) {
            const button = document.createElement('button');
            button.type = 'button';
            button.textContent = `${peer.name} · ${peer.url}${peer.requires_lan_key ? ' · key required' : ''}`;
            button.addEventListener('click', () => useDiscoveredPeer(peer.name, peer.url));
            box.appendChild(button);
            box.appendChild(document.createTextNode(' '));
          }
          setNextStep('Select the sharing computer below, enter its pairing code, then click Pair Peer.');
        }
        showResult(peers);
      } catch (error) { show(error.message); }
    }
    function useDiscoveredPeer(name, url) {
      document.getElementById('peerName').value = name;
      document.getElementById('peerUrl').value = url;
      setNextStep(`Selected ${name}. Enter the pairing code from that computer, then click Pair Peer.`);
    }
    function peerPayload(dryRun) {
      return {
        url: document.getElementById('peerUrl').value,
        lan_key: document.getElementById('peerKey').value || null,
        dry_run: dryRun
      };
    }
    async function testPeer() {
      try {
        const result = await request('/api/peer/test', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(peerPayload(true))
        });
        showResult(result);
        setNextStep(`Connected. This peer reports ${result.peer_fonts} fonts. Use Preview From Peer to see what would happen, or Get Missing Fonts to install missing fonts.`);
      } catch (error) { show(error.message); }
    }
    async function pairPeer() {
      try {
        const peer = await request('/api/peer/pair', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            name: document.getElementById('peerName').value,
            url: document.getElementById('peerUrl').value,
            pairing_code: document.getElementById('pairingCode').value
          })
        });
        document.getElementById('peerName').value = peer.name;
        document.getElementById('peerUrl').value = peer.url;
        document.getElementById('peerKey').value = peer.lan_key ?? '';
        showResult(peer);
        setNextStep(`${peer.name} is paired and saved. Click Test Peer or Preview From Peer next.`);
      } catch (error) { show(error.message); }
    }
    async function syncPeer(dryRun) {
      try {
        const result = await request('/api/peer/sync', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify(peerPayload(dryRun))
        });
        showResult(result);
        if (dryRun) {
          const wouldInstall = result.skipped?.filter(line => line.startsWith('would install ')).length ?? 0;
          setNextStep(wouldInstall
            ? `${wouldInstall} fonts are missing locally. Click Get Missing Fonts to install them.`
            : 'No missing installable fonts were found from this peer.');
        } else if (result.installed?.length) {
          setNextStep('Installed fonts are ready. Reopen design apps if they do not appear yet.');
          out.textContent += '\n\nInstalled fonts are ready. Reopen design apps if they do not appear yet.';
        } else {
          setNextStep('No new fonts were installed. The peer may already match this computer.');
        }
      } catch (error) { show(error.message); }
    }
    async function savePeer() {
      try {
        const peer = await request('/api/peers', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            name: document.getElementById('peerName').value,
            url: document.getElementById('peerUrl').value,
            lan_key: document.getElementById('peerKey').value || null
          })
        });
        showResult(peer);
        setNextStep(`${peer.name} is saved. Use Sync Saved Peers for repeat syncs.`);
      } catch (error) { show(error.message); }
    }
    async function syncAll(dryRun) {
      try {
        const result = await request(dryRun ? '/api/sync-all/dry-run' : '/api/sync-all', { method: 'POST' });
        showResult(result);
        if (!dryRun && result.peers?.some(peer => peer.installed?.length)) {
          setNextStep('Installed fonts are ready. Reopen design apps if they do not appear yet.');
          out.textContent += '\n\nInstalled fonts are ready. Reopen design apps if they do not appear yet.';
        } else if (dryRun) {
          setNextStep('Dry run complete. Review the peer results below before syncing saved peers.');
        } else {
          setNextStep('Saved peer sync finished. No new fonts were installed.');
        }
      } catch (error) { show(error.message); }
    }
    async function startShare() {
      try {
        showShareResult(await request('/api/share/start', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({
            listen: document.getElementById('listen').value,
            lan_key: document.getElementById('shareKey').value || null
          })
        }));
        refresh();
      } catch (error) { show(error.message); }
    }
    async function stopShare() {
      try {
        showResult(await request('/api/share/stop', { method: 'POST' }));
        setNextStep('Sharing is off. Start sharing again when another computer needs fonts from this one.');
        refresh();
      } catch (error) { show(error.message); }
    }
    refresh();
  </script>
</body>
</html>"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn safe_file_name_removes_path_and_reserved_characters() {
        let name = safe_file_name(
            "../Fancy Font:*?<>.ttf",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );

        assert_eq!(name, "Fancy-Font-----.ttf");
    }

    #[test]
    fn safe_file_name_falls_back_for_empty_names() {
        let name = safe_file_name(
            "",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );

        assert_eq!(name, "font.ttf");
    }

    #[test]
    fn stable_font_id_uses_first_16_hash_bytes() {
        let id = stable_font_id("00112233445566778899aabbccddeeff0123456789abcdef0123456789abcdef");

        assert_eq!(id.to_string(), "00112233-4455-6677-8899-aabbccddeeff");
    }

    #[test]
    fn normalize_peer_url_trims_whitespace_and_trailing_slashes() {
        assert_eq!(
            normalize_peer_url("  http://192.168.1.50:7370///  "),
            "http://192.168.1.50:7370"
        );
    }

    #[test]
    fn pairing_code_normalization_keeps_only_digits() {
        assert_eq!(normalize_pairing_code(" 1234-56 78 "), "12345678");
    }

    #[test]
    fn generated_lan_token_and_pairing_code_have_expected_shape() {
        let token = generate_lan_token();
        let code = generate_pairing_code().unwrap();

        assert!(token.starts_with("smf-"));
        assert_eq!(code.len(), 8);
        assert!(code.chars().all(|ch| ch.is_ascii_digit()));
    }

    #[test]
    fn diagnostics_peer_redaction_reports_presence_not_secret() {
        let peer = LanPeerConfig {
            name: "Workshop".to_string(),
            url: "http://192.168.1.50:7370".to_string(),
            lan_key: Some("super-secret".to_string()),
        };

        let redacted = RedactedPeer {
            name: peer.name,
            url: peer.url,
            has_lan_key: peer.lan_key.is_some(),
        };
        let json = serde_json::to_string(&redacted).unwrap();

        assert!(json.contains("\"has_lan_key\":true"));
        assert!(!json.contains("super-secret"));
    }

    #[test]
    fn managed_manifest_records_and_updates_installed_fonts() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        let font_path = config_dir.join("fonts/example.ttf");
        record_managed_install(
            "Example.ttf",
            "00112233445566778899aabbccddeeff0123456789abcdef0123456789abcdef",
            &font_path,
            "lan:http://127.0.0.1:7370",
            1234,
        )
        .unwrap();
        record_managed_install(
            "Example.ttf",
            "00112233445566778899aabbccddeeff0123456789abcdef0123456789abcdef",
            &font_path,
            "server:http://127.0.0.1:7368",
            1234,
        )
        .unwrap();

        let manifest = load_managed_manifest().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(manifest.schema, 1);
        assert_eq!(manifest.installed.len(), 1);
        assert_eq!(manifest.installed[0].file_name, "Example.ttf");
        assert_eq!(manifest.installed[0].source, "server:http://127.0.0.1:7368");
        assert_eq!(manifest.installed[0].size_bytes, 1234);
    }
}
