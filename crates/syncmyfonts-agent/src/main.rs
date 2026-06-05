use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    net::{SocketAddr, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command},
    sync::{Arc, Mutex, mpsc},
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
    /// Pair with a LAN peer using the code shown on the sharing computer.
    LanPair {
        #[arg(long)]
        name: String,
        #[arg(long)]
        url: String,
        #[arg(long)]
        pairing_code: String,
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
    /// Check local app readiness without installing fonts or contacting peers.
    Doctor,
    /// Print a clean-machine validation evidence bundle.
    ValidationReport,
    /// Verify SyncMyFonts-managed installed font files still match the manifest.
    VerifyManaged,
    /// Install a per-user sign-in helper that syncs saved LAN peers.
    InstallStartupSync,
    /// Install per-user app shortcuts for common SyncMyFonts actions.
    InstallAppShortcuts,
    /// Run the native desktop GUI.
    Gui,
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
        Commands::LanPair {
            name,
            url,
            pairing_code,
        } => {
            let peer = pair_lan_peer(name, url, pairing_code)?;
            print_json(&redacted_peer_config(&peer))?;
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
        Commands::Doctor => {
            print_json(&doctor()?)?;
        }
        Commands::ValidationReport => {
            print_json(&validation_report()?)?;
        }
        Commands::VerifyManaged => {
            print_json(&verify_managed_fonts()?)?;
        }
        Commands::InstallStartupSync => {
            print_json(&install_startup_sync()?)?;
        }
        Commands::InstallAppShortcuts => {
            print_json(&install_app_shortcuts()?)?;
        }
        Commands::Gui => {
            run_gui()?;
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
    let skip_managed_dir = !include_managed && managed_dir != user_font_dir;
    let managed_paths = if include_managed {
        HashSet::new()
    } else {
        load_managed_manifest()
            .map(|manifest| {
                manifest
                    .installed
                    .into_iter()
                    .map(|record| record.path)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default()
    };
    let mut fonts = Vec::new();

    if !user_font_dir.exists() {
        return Ok(ScanOutput {
            platform: platform_name(),
            schema: 1,
            fonts,
            warnings,
        });
    }

    for entry in WalkDir::new(&user_font_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| !(skip_managed_dir && entry.path() == managed_dir))
    {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                warnings.push(error.to_string());
                continue;
            }
        };
        let path = entry.path();
        if entry.file_type().is_dir() {
            continue;
        }
        if !include_managed && managed_paths.contains(path) {
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
    friendly_device_name: Option<String>,
    peers: Vec<LanPeerConfig>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct AppHistory {
    schema: u8,
    last_action: Option<ActionRecord>,
    recent: Vec<ActionRecord>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ActionRecord {
    action: String,
    status: String,
    finished_at: String,
    warning_count: usize,
    result: String,
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
struct DeviceNameRequest {
    device_name: String,
}

#[derive(Debug, Serialize)]
struct DeviceNameResponse {
    device_name: String,
    saved: bool,
}

#[derive(Debug, Deserialize)]
struct AddPeerRequest {
    name: String,
    url: String,
    lan_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ForgetPeerRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct ForgetPeerResponse {
    removed: bool,
    saved_peer_count: usize,
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
struct OpenFolderResponse {
    opened: bool,
    path: PathBuf,
    message: String,
}

#[derive(Debug, Serialize)]
struct StartupSyncReport {
    installed: bool,
    platform: &'static str,
    agent_path: PathBuf,
    helper_path: PathBuf,
    registration_path: PathBuf,
    saved_peer_count: usize,
    message: String,
}

#[derive(Debug, Serialize)]
struct AppShortcutReport {
    installed: bool,
    platform: &'static str,
    directory: PathBuf,
    shortcuts: Vec<PathBuf>,
    message: String,
}

#[derive(Debug, Serialize)]
struct DiagnosticsReport {
    version: &'static str,
    platform: &'static str,
    device_name: String,
    config_path: PathBuf,
    log_dir: PathBuf,
    history_path: PathBuf,
    managed_manifest_path: PathBuf,
    user_font_dir: PathBuf,
    managed_font_dir: PathBuf,
    saved_peer_count: usize,
    saved_peers: Vec<RedactedPeer>,
    last_action: Option<ActionRecord>,
    recent_actions: Vec<ActionRecord>,
    user_font_count: usize,
    managed_manifest_count: usize,
    warnings: Vec<String>,
    support_report_text: String,
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    ok: bool,
    checks: Vec<DoctorCheck>,
    next_step: String,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    ok: bool,
    message: String,
}

#[derive(Debug, Serialize)]
struct ValidationReport {
    generated_at: String,
    platform: &'static str,
    version: &'static str,
    device_name: String,
    diagnostics: DiagnosticsReport,
    readiness: DoctorReport,
    managed_fonts: ManagedVerifyReport,
    evidence_summary: Vec<String>,
    manual_validation_steps: Vec<String>,
    pass_criteria: Vec<String>,
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

#[derive(Debug, Serialize)]
struct ManagedVerifyReport {
    manifest_path: PathBuf,
    total: usize,
    ok: usize,
    missing: Vec<ManagedVerifyIssue>,
    modified: Vec<ManagedVerifyIssue>,
    unreadable: Vec<ManagedVerifyIssue>,
}

#[derive(Debug, Serialize)]
struct ManagedVerifyIssue {
    sha256: String,
    file_name: String,
    path: PathBuf,
    message: String,
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

fn forget_lan_peer(name: &str) -> Result<ForgetPeerResponse> {
    let mut config = load_app_config()?;
    let before = config.peers.len();
    config.peers.retain(|peer| peer.name != name);
    let removed = config.peers.len() != before;
    if removed {
        save_app_config(&config)?;
    }
    Ok(ForgetPeerResponse {
        removed,
        saved_peer_count: config.peers.len(),
    })
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
        .map(redacted_peer_config)
        .collect::<Vec<_>>();
    let history = load_app_history().unwrap_or_default();
    let report = DiagnosticsReport {
        version: env!("CARGO_PKG_VERSION"),
        platform: platform_name(),
        device_name: device_name(),
        config_path: app_config_path()?,
        log_dir: app_log_dir()?,
        history_path: app_history_path()?,
        managed_manifest_path: managed_manifest_path()?,
        user_font_dir: user_font_dir()?,
        managed_font_dir: managed_font_dir()?,
        saved_peer_count: config.peers.len(),
        saved_peers,
        last_action: history.last_action,
        recent_actions: history.recent,
        user_font_count: scan.fonts.len(),
        managed_manifest_count,
        warnings: diagnostics_warnings(scan.warnings, manifest_result),
        support_report_text: String::new(),
    };
    Ok(DiagnosticsReport {
        support_report_text: support_report_text(&report),
        ..report
    })
}

fn doctor() -> Result<DoctorReport> {
    let mut checks = Vec::new();

    let agent_path = agent_command_exe();
    checks.push(match &agent_path {
        Ok(path) if path.exists() => doctor_check(
            "agent-binary",
            true,
            format!("Agent helper is available at {}.", path.display()),
        ),
        Ok(path) => doctor_check(
            "agent-binary",
            false,
            format!(
                "Agent helper was resolved to {}, but it does not exist.",
                path.display()
            ),
        ),
        Err(error) => doctor_check(
            "agent-binary",
            false,
            format!("Agent helper could not be resolved: {error}"),
        ),
    });

    let config_path = app_config_path()?;
    checks.push(path_parent_check("config-dir", &config_path));

    let log_dir = app_log_dir()?;
    checks.push(directory_ready_check("log-dir", &log_dir));

    let user_font_dir = user_font_dir()?;
    checks.push(directory_ready_check("user-font-dir", &user_font_dir));

    let managed_font_dir = managed_font_dir()?;
    checks.push(directory_ready_check("managed-font-dir", &managed_font_dir));

    let config = load_app_config()?;
    checks.push(doctor_check(
        "lan-sharing-guidance",
        true,
        platform_lan_sharing_guidance(),
    ));
    checks.push(if config.peers.is_empty() {
        doctor_check(
            "saved-peers",
            false,
            "No saved peers yet. Pair or save another computer before relying on repeat sync.",
        )
    } else {
        doctor_check(
            "saved-peers",
            true,
            format!("{} saved peer(s) are configured.", config.peers.len()),
        )
    });

    checks.push(match startup_sync_helper_path() {
        Ok(path) if path.exists() => doctor_check(
            "sign-in-sync-helper",
            true,
            format!("Sign-in sync helper exists at {}.", path.display()),
        ),
        Ok(path) => doctor_check(
            "sign-in-sync-helper",
            false,
            format!(
                "Sign-in sync helper is not installed yet. Expected location: {}.",
                path.display()
            ),
        ),
        Err(error) => doctor_check(
            "sign-in-sync-helper",
            false,
            format!("Sign-in sync helper path could not be resolved: {error}"),
        ),
    });

    let failed = checks.iter().filter(|check| !check.ok).count();
    let next_step = if failed == 0 {
        "This computer is ready for saved-peer LAN sync.".to_string()
    } else if config.peers.is_empty() {
        "Pair or save a LAN peer, then run Readiness Check again.".to_string()
    } else {
        "Review failed checks, then run Readiness Check again.".to_string()
    };

    Ok(DoctorReport {
        ok: failed == 0,
        checks,
        next_step,
    })
}

fn validation_report() -> Result<ValidationReport> {
    let diagnostics = diagnostics()?;
    let readiness = doctor()?;
    let managed_fonts = verify_managed_fonts()?;
    let evidence_summary = validation_evidence_summary(&diagnostics, &readiness, &managed_fonts);

    Ok(ValidationReport {
        generated_at: Utc::now().to_rfc3339(),
        platform: platform_name(),
        version: env!("CARGO_PKG_VERSION"),
        device_name: device_name(),
        diagnostics,
        readiness,
        managed_fonts,
        evidence_summary,
        manual_validation_steps: manual_validation_steps(),
        pass_criteria: manual_validation_pass_criteria(),
    })
}

fn validation_evidence_summary(
    diagnostics: &DiagnosticsReport,
    readiness: &DoctorReport,
    managed_fonts: &ManagedVerifyReport,
) -> Vec<String> {
    let failed_readiness = readiness.checks.iter().filter(|check| !check.ok).count();
    let managed_issues =
        managed_fonts.missing.len() + managed_fonts.modified.len() + managed_fonts.unreadable.len();
    vec![
        format!("Platform: {}", diagnostics.platform),
        format!("Device: {}", diagnostics.device_name),
        format!("User font dir: {}", diagnostics.user_font_dir.display()),
        format!(
            "Managed font dir: {}",
            diagnostics.managed_font_dir.display()
        ),
        format!("Saved peers: {}", diagnostics.saved_peer_count),
        format!("Local scanned fonts: {}", diagnostics.user_font_count),
        format!(
            "Managed font records: {}",
            diagnostics.managed_manifest_count
        ),
        format!("Readiness failed checks: {failed_readiness}"),
        format!("Managed font verification issues: {managed_issues}"),
        "Secrets are redacted in diagnostics, saved peer summaries, and action history."
            .to_string(),
    ]
}

fn manual_validation_steps() -> Vec<String> {
    vec![
        "Launch the native app on both macOS and Windows.".to_string(),
        "Run Validation Report on both computers before syncing.".to_string(),
        "On the computer that has a non-system test font, click Share Fonts On LAN with Shared Key blank.".to_string(),
        "On the other computer, find or enter the peer URL, enter the pairing code, and click Pair Peer.".to_string(),
        "Run Preview From Peer and confirm the test font is missing while system fonts are not offered.".to_string(),
        "Run Get Missing Fonts and confirm the font installs into the current-user or SyncMyFonts-managed folder.".to_string(),
        "Run the same sync again and confirm the already installed font is skipped.".to_string(),
        "Repeat the flow in the other direction with a different non-system test font.".to_string(),
        "Run Validation Report again on both computers and keep the before/after JSON as evidence.".to_string(),
    ]
}

fn manual_validation_pass_criteria() -> Vec<String> {
    vec![
        "Native GUI launches on both platforms without administrator privileges.".to_string(),
        "Pairing-code LAN sync works from macOS to Windows and Windows to macOS.".to_string(),
        "Fonts install only into current-user or SyncMyFonts-managed locations.".to_string(),
        "System fonts are not listed as missing sync candidates.".to_string(),
        "Re-running sync skips fonts that are already present.".to_string(),
        "Managed font verification has no missing, modified, or unreadable entries after sync."
            .to_string(),
        "Diagnostics and validation reports do not expose LAN keys, pairing codes, or API keys."
            .to_string(),
        "No port forwarding, Docker container, or cloud service is required for the LAN test."
            .to_string(),
    ]
}

fn doctor_check(name: &str, ok: bool, message: impl Into<String>) -> DoctorCheck {
    DoctorCheck {
        name: name.to_string(),
        ok,
        message: message.into(),
    }
}

fn path_parent_check(name: &str, path: &Path) -> DoctorCheck {
    match path.parent() {
        Some(parent) => directory_ready_check(name, parent),
        None => doctor_check(
            name,
            false,
            format!("{} does not have a parent directory.", path.display()),
        ),
    }
}

fn directory_ready_check(name: &str, path: &Path) -> DoctorCheck {
    match fs::create_dir_all(path) {
        Ok(()) => doctor_check(name, true, format!("{} is available.", path.display())),
        Err(error) => doctor_check(
            name,
            false,
            format!("{} is not available: {error}", path.display()),
        ),
    }
}

async fn app_serve(listen: SocketAddr, open_browser_on_start: bool) -> Result<()> {
    let state = AppState {
        share: Arc::new(Mutex::new(None)),
    };
    let app = Router::new()
        .route("/", get(app_index))
        .route("/favicon.ico", get(app_favicon))
        .route("/api/status", get(app_status))
        .route("/api/device-name", post(app_set_device_name))
        .route("/api/scan", get(app_scan))
        .route("/api/diagnostics", get(app_diagnostics))
        .route("/api/managed/verify", get(app_verify_managed))
        .route("/api/managed/open", post(app_open_managed_folder))
        .route("/api/logs/open", post(app_open_logs_folder))
        .route("/api/peers", get(app_peers).post(app_add_peer))
        .route("/api/peers/forget", post(app_forget_peer))
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

async fn app_favicon() -> StatusCode {
    StatusCode::NO_CONTENT
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

async fn app_set_device_name(
    Json(request): Json<DeviceNameRequest>,
) -> Result<Json<DeviceNameResponse>, LanApiError> {
    match set_friendly_device_name(request.device_name) {
        Ok(response) => {
            let saved = response.friendly_device_name.is_some();
            let device_name = device_name();
            record_action_best_effort(
                "Browser Save Device Name",
                "success",
                0,
                &format!("Device name is now {device_name}."),
            );
            Ok(Json(DeviceNameResponse { device_name, saved }))
        }
        Err(error) => {
            record_action_best_effort("Browser Save Device Name", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_scan() -> Result<Json<ScanOutput>, LanApiError> {
    match scan(true) {
        Ok(report) => {
            let warnings = report.warnings.len();
            record_action_best_effort(
                "Browser Scan Fonts",
                "success",
                warnings,
                &format!("Found {} local fonts.", report.fonts.len()),
            );
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort("Browser Scan Fonts", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_diagnostics() -> Result<Json<DiagnosticsReport>, LanApiError> {
    match diagnostics() {
        Ok(report) => {
            record_action_best_effort(
                "Browser Diagnostics",
                "success",
                report.warnings.len(),
                "Diagnostics report generated.",
            );
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort("Browser Diagnostics", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_verify_managed() -> Result<Json<ManagedVerifyReport>, LanApiError> {
    match verify_managed_fonts() {
        Ok(report) => {
            let issues = report.missing.len() + report.modified.len() + report.unreadable.len();
            record_action_best_effort(
                "Browser Verify Managed Fonts",
                "success",
                issues,
                &format!("{issues} managed font issue(s) found."),
            );
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort(
                "Browser Verify Managed Fonts",
                "failed",
                1,
                &error.to_string(),
            );
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_open_managed_folder() -> Result<Json<OpenFolderResponse>, LanApiError> {
    let result = tokio::task::spawn_blocking(|| -> Result<OpenFolderResponse> {
        let folder = managed_font_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating managed font folder {}", path.display()))?;
            Ok(path)
        })?;
        let path = open_path(folder)?;
        Ok(OpenFolderResponse {
            opened: true,
            path,
            message: "Opened the SyncMyFonts managed font folder.".to_string(),
        })
    })
    .await
    .map_err(LanApiError::internal)?;
    match result {
        Ok(response) => {
            record_action_best_effort(
                "Browser Open Managed Folder",
                "success",
                0,
                &response.message,
            );
            Ok(Json(response))
        }
        Err(error) => {
            record_action_best_effort(
                "Browser Open Managed Folder",
                "failed",
                1,
                &error.to_string(),
            );
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_open_logs_folder() -> Result<Json<OpenFolderResponse>, LanApiError> {
    let result = tokio::task::spawn_blocking(|| -> Result<OpenFolderResponse> {
        let folder = app_log_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating log folder {}", path.display()))?;
            Ok(path)
        })?;
        let path = open_path(folder)?;
        Ok(OpenFolderResponse {
            opened: true,
            path,
            message: "Opened the SyncMyFonts log folder.".to_string(),
        })
    })
    .await
    .map_err(LanApiError::internal)?;
    match result {
        Ok(response) => {
            record_action_best_effort("Browser Open Logs", "success", 0, &response.message);
            Ok(Json(response))
        }
        Err(error) => {
            record_action_best_effort("Browser Open Logs", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_peers() -> Result<Json<Vec<RedactedPeer>>, LanApiError> {
    load_app_config()
        .map(|config| Json(config.peers.iter().map(redacted_peer_config).collect()))
        .map_err(LanApiError::internal)
}

async fn app_add_peer(
    Json(request): Json<AddPeerRequest>,
) -> Result<Json<LanPeerConfig>, LanApiError> {
    match add_lan_peer(request.name, request.url, request.lan_key) {
        Ok(peer) => {
            record_action_best_effort("Browser Save Peer", "success", 0, "Saved LAN peer.");
            Ok(Json(peer))
        }
        Err(error) => {
            record_action_best_effort("Browser Save Peer", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_forget_peer(
    Json(request): Json<ForgetPeerRequest>,
) -> Result<Json<ForgetPeerResponse>, LanApiError> {
    match forget_lan_peer(&request.name) {
        Ok(response) => {
            record_action_best_effort("Browser Forget Peer", "success", 0, "Forgot LAN peer.");
            Ok(Json(response))
        }
        Err(error) => {
            record_action_best_effort("Browser Forget Peer", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
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
    let result = tokio::task::spawn_blocking(move || {
        pair_lan_peer(request.name, request.url, request.pairing_code)
    })
    .await
    .map_err(LanApiError::internal)?;
    match result {
        Ok(peer) => {
            record_action_best_effort("Browser Pair Peer", "success", 0, "Paired LAN peer.");
            Ok(Json(peer))
        }
        Err(error) => {
            record_action_best_effort("Browser Pair Peer", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_peer_test(
    Json(request): Json<PeerSyncRequest>,
) -> Result<Json<PeerTestResponse>, LanApiError> {
    let url = request.url;
    let lan_key = request.lan_key;
    let result = tokio::task::spawn_blocking(move || lan_sync(&url, lan_key.as_deref(), true))
        .await
        .map_err(LanApiError::internal)?;
    match result {
        Ok(report) => {
            record_action_best_effort(
                "Browser Test Peer",
                "success",
                0,
                &format!("Connected. Peer reported {} fonts.", report.peer_fonts),
            );
            Ok(Json(PeerTestResponse {
                ok: true,
                message: format!("Connected. Peer reported {} fonts.", report.peer_fonts),
                peer_fonts: report.peer_fonts,
                would_install_or_skip: report.skipped.len(),
            }))
        }
        Err(error) => {
            record_action_best_effort("Browser Test Peer", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_peer_sync(
    Json(request): Json<PeerSyncRequest>,
) -> Result<Json<LanSyncReport>, LanApiError> {
    let url = request.url;
    let lan_key = request.lan_key;
    let dry_run = request.dry_run.unwrap_or(false);
    let action = if dry_run {
        "Browser Preview From Peer"
    } else {
        "Browser Get Missing Fonts"
    };
    let result = tokio::task::spawn_blocking(move || lan_sync(&url, lan_key.as_deref(), dry_run))
        .await
        .map_err(LanApiError::internal)?;
    match result {
        Ok(report) => {
            let result = if dry_run {
                format!("Dry run complete with {} result(s).", report.skipped.len())
            } else {
                format!("Installed {} font(s).", report.installed.len())
            };
            record_action_best_effort(action, "success", 0, &result);
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort(action, "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_sync_all() -> Result<Json<LanSyncAllReport>, LanApiError> {
    let result = tokio::task::spawn_blocking(|| lan_sync_all(false))
        .await
        .map_err(LanApiError::internal)?;
    match result {
        Ok(report) => {
            let warnings = report
                .peers
                .iter()
                .filter(|peer| peer.error.is_some())
                .count();
            let installed = report
                .peers
                .iter()
                .map(|peer| peer.installed.len())
                .sum::<usize>();
            record_action_best_effort(
                "Browser Sync Saved Peers",
                "success",
                warnings,
                &format!("Installed {installed} font(s) from saved peers."),
            );
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort("Browser Sync Saved Peers", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_sync_all_dry_run() -> Result<Json<LanSyncAllReport>, LanApiError> {
    let result = tokio::task::spawn_blocking(|| lan_sync_all(true))
        .await
        .map_err(LanApiError::internal)?;
    match result {
        Ok(report) => {
            let warnings = report
                .peers
                .iter()
                .filter(|peer| peer.error.is_some())
                .count();
            record_action_best_effort(
                "Browser Dry Run Saved Peers",
                "success",
                warnings,
                "Dry run complete for saved peers.",
            );
            Ok(Json(report))
        }
        Err(error) => {
            record_action_best_effort(
                "Browser Dry Run Saved Peers",
                "failed",
                1,
                &error.to_string(),
            );
            Err(LanApiError::internal(error))
        }
    }
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
    let exe = agent_command_exe().map_err(LanApiError::internal)?;
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
    let child = match command.spawn().map_err(anyhow::Error::from) {
        Ok(child) => child,
        Err(error) => {
            record_action_best_effort(
                "Browser Share Fonts On LAN",
                "failed",
                1,
                &error.to_string(),
            );
            return Err(LanApiError::internal(error));
        }
    };
    let child = match wait_for_share_start(child, listen) {
        Ok(child) => child,
        Err(error) => {
            record_action_best_effort(
                "Browser Share Fonts On LAN",
                "failed",
                1,
                &error.to_string(),
            );
            return Err(LanApiError::internal(error));
        }
    };
    let urls = share_urls(listen);
    *guard = Some(RunningShare { child, listen });
    let pairing_expires_seconds = pairing_code.as_ref().map(|_| PAIRING_CODE_TTL.as_secs());
    let response = ShareResponse {
        sharing: true,
        message: format!("Sharing fonts at {}.", urls.join(", ")),
        urls,
        pairing_code,
        pairing_expires_seconds,
    };
    record_action_best_effort(
        "Browser Share Fonts On LAN",
        "success",
        0,
        &response.message,
    );
    Ok(Json(response))
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
    record_action_best_effort(
        "Browser Stop Sharing",
        "success",
        0,
        "Stopped sharing fonts.",
    );
    Ok(Json(ShareResponse {
        sharing: false,
        message: "Stopped sharing fonts.".to_string(),
        urls: Vec::new(),
        pairing_code: None,
        pairing_expires_seconds: None,
    }))
}

fn run_gui() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default().with_inner_size([980.0, 760.0]),
        ..Default::default()
    };
    eframe::run_native(
        "SyncMyFonts",
        options,
        Box::new(|_cc| Ok(Box::new(SyncMyFontsGui::new()))),
    )
    .map_err(|error| anyhow!("running SyncMyFonts GUI: {error}"))
}

struct SyncMyFontsGui {
    status: String,
    next_step: String,
    output: String,
    last_result: String,
    warning_count: usize,
    saved_peer_summary: String,
    device_name_input: String,
    current_action: Option<String>,
    task: Option<mpsc::Receiver<GuiTaskResult>>,
    peer_name: String,
    peer_url: String,
    peer_key: String,
    pairing_code: String,
    listen: String,
    share_key: String,
    share: Option<RunningShare>,
    share_urls: Vec<String>,
}

struct GuiTaskResult {
    output: String,
    next_step: String,
    peer: Option<LanPeerConfig>,
    discovered_peer: Option<LanDiscoveredPeer>,
    clear_peer_key: bool,
    refresh_saved_peers: bool,
    warning_count: usize,
}

impl SyncMyFontsGui {
    fn new() -> Self {
        let mut app = Self {
            status: "Loading...".to_string(),
            next_step: "Start by sharing fonts on one computer, then pair from the other computer."
                .to_string(),
            output: "Ready.".to_string(),
            last_result: "No actions yet.".to_string(),
            warning_count: 0,
            saved_peer_summary: "Saved peers: loading...".to_string(),
            device_name_input: device_name(),
            current_action: None,
            task: None,
            peer_name: String::new(),
            peer_url: String::new(),
            peer_key: String::new(),
            pairing_code: String::new(),
            listen: "0.0.0.0:7370".to_string(),
            share_key: String::new(),
            share: None,
            share_urls: Vec::new(),
        };
        app.refresh_status();
        app.load_saved_peers_into_form();
        app
    }

    fn start_task<F>(&mut self, action: &str, work: F)
    where
        F: FnOnce() -> GuiTaskResult + Send + 'static,
    {
        if self.task.is_some() {
            self.next_step =
                "Another SyncMyFonts action is still running. Wait for it to finish first."
                    .to_string();
            return;
        }

        let (sender, receiver) = mpsc::channel();
        self.current_action = Some(action.to_string());
        self.task = Some(receiver);
        self.next_step = format!("{action} is running...");
        self.output = "Working...".to_string();
        thread::spawn(move || {
            let _ = sender.send(work());
        });
    }

    fn poll_task(&mut self) {
        let Some(receiver) = self.task.take() else {
            return;
        };
        match receiver.try_recv() {
            Ok(result) => {
                let action = self
                    .current_action
                    .clone()
                    .unwrap_or_else(|| "Action".to_string());
                if let Some(peer) = result.peer {
                    self.peer_name = peer.name;
                    self.peer_url = peer.url;
                    self.peer_key = peer.lan_key.unwrap_or_default();
                }
                if let Some(peer) = result.discovered_peer {
                    self.peer_name = peer.name;
                    self.peer_url = peer.url;
                }
                if result.clear_peer_key {
                    self.peer_key.clear();
                }
                self.output = result.output;
                self.next_step = result.next_step;
                self.warning_count = result.warning_count;
                self.last_result = format!(
                    "{} completed at {}",
                    action,
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                if let Err(error) =
                    record_action(&action, "success", self.warning_count, &self.next_step)
                {
                    self.output.push_str(&format!(
                        "\n\nWarning: could not save action history: {error}"
                    ));
                    self.warning_count += 1;
                }
                if result.refresh_saved_peers {
                    self.load_saved_peers_summary();
                }
                self.current_action = None;
            }
            Err(mpsc::TryRecvError::Empty) => {
                self.task = Some(receiver);
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                self.output = "Background action stopped before returning a result.".to_string();
                self.next_step =
                    "That action did not finish cleanly. Try again or run Diagnostics.".to_string();
                self.last_result = "Last action stopped before returning a result.".to_string();
                self.warning_count = 1;
                let action = self
                    .current_action
                    .clone()
                    .unwrap_or_else(|| "Action".to_string());
                let _ = record_action(&action, "failed", self.warning_count, &self.next_step);
                self.current_action = None;
            }
        }
    }

    fn refresh_status(&mut self) {
        self.prune_stopped_share();
        self.device_name_input = device_name();
        self.status = format!(
            "{} · {} · sharing: {}",
            self.device_name_input,
            platform_name(),
            if self.share.is_some() { "on" } else { "off" }
        );
        self.share_urls = self
            .share
            .as_ref()
            .map(|share| share_urls(share.listen))
            .unwrap_or_default();
        self.load_saved_peers_summary();
    }

    fn save_device_name(&mut self) {
        match set_friendly_device_name(self.device_name_input.clone()) {
            Ok(config) => {
                self.device_name_input = device_name();
                self.refresh_status();
                let saved = config.friendly_device_name.is_some();
                self.output = if saved {
                    format!("Saved device name: {}", self.device_name_input)
                } else {
                    format!(
                        "Cleared device name. Using system name: {}",
                        self.device_name_input
                    )
                };
                self.next_step =
                    "This name is used for LAN discovery, pairing, diagnostics, and support reports."
                        .to_string();
                self.last_result = format!(
                    "Save Device Name completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Save Device Name", "success", 0, &self.next_step);
            }
            Err(error) => {
                self.output = error.to_string();
                self.next_step = "SyncMyFonts could not save the device name.".to_string();
                self.last_result = "Save Device Name failed.".to_string();
                self.warning_count = 1;
                let _ = record_action("Save Device Name", "failed", 1, &self.next_step);
            }
        }
    }

    fn load_saved_peers_summary(&mut self) {
        self.saved_peer_summary = match load_app_config() {
            Ok(config) if config.peers.is_empty() => "Saved peers: none yet.".to_string(),
            Ok(config) => format!(
                "Saved peers: {} ({})",
                config.peers.len(),
                config
                    .peers
                    .iter()
                    .map(|peer| peer.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Err(error) => format!("Saved peers unavailable: {error}"),
        };
    }

    fn load_saved_peers_into_form(&mut self) {
        match load_app_config() {
            Ok(config) => {
                if let Some(peer) = config.peers.first() {
                    self.peer_name = peer.name.clone();
                    self.peer_url = peer.url.clone();
                    self.peer_key = peer.lan_key.clone().unwrap_or_default();
                    self.next_step = format!(
                        "Loaded saved peer {}. Use Test Peer, Preview From Peer, or Sync Saved Peers.",
                        peer.name
                    );
                }
            }
            Err(error) => {
                self.next_step = format!("Saved peers could not be loaded: {error}");
            }
        }
        self.load_saved_peers_summary();
    }

    fn scan_fonts(&mut self) {
        self.start_task("Scanning fonts", || match scan(true) {
            Ok(report) => {
                let warnings = report.warnings.len();
                gui_ok_with_warning_count(
                    &report,
                    format!(
                        "Found {} local fonts. Share this device if another computer needs these fonts.",
                        report.fonts.len()
                    ),
                    warnings,
                )
            }
            Err(error) => gui_error(error),
        });
    }

    fn verify_managed(&mut self) {
        self.start_task("Verifying managed fonts", || match verify_managed_fonts() {
            Ok(report) => {
                let issues = report.missing.len() + report.modified.len() + report.unreadable.len();
                let next_step = if issues == 0 {
                    "All SyncMyFonts-managed fonts still match the local manifest.".to_string()
                } else {
                    format!("{issues} managed font issue(s) found. Review before syncing more.")
                };
                gui_ok_with_warning_count(&report, next_step, issues)
            }
            Err(error) => gui_error(error),
        });
    }

    fn run_diagnostics(&mut self) {
        self.start_task("Collecting diagnostics", || match diagnostics() {
            Ok(report) => {
                let warnings = report.warnings.len();
                gui_ok_with_warning_count(
                    &report,
                    "Diagnostics are redacted and safe to paste into a support issue.".to_string(),
                    warnings,
                )
            }
            Err(error) => gui_error(error),
        });
    }

    fn run_doctor(&mut self) {
        self.start_task("Checking readiness", || match doctor() {
            Ok(report) => {
                let warnings = report.checks.iter().filter(|check| !check.ok).count();
                gui_ok_with_warning_count(&report, report.next_step.clone(), warnings)
            }
            Err(error) => gui_error(error),
        });
    }

    fn run_validation_report(&mut self) {
        self.start_task("Collecting validation report", || match validation_report() {
            Ok(report) => {
                let warnings = report
                    .readiness
                    .checks
                    .iter()
                    .filter(|check| !check.ok)
                    .count()
                    + report.managed_fonts.missing.len()
                    + report.managed_fonts.modified.len()
                    + report.managed_fonts.unreadable.len();
                gui_ok_with_warning_count(
                    &report,
                    "Save this before/after report with the clean-machine Mac and Windows sync test evidence."
                        .to_string(),
                    warnings,
                )
            }
            Err(error) => gui_error(error),
        });
    }

    fn open_managed_font_folder(&mut self) {
        let folder = managed_font_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating managed font folder {}", path.display()))?;
            Ok(path)
        });
        self.open_folder_action(
            "Open Managed Folder",
            folder,
            "This is where SyncMyFonts puts fonts it installs for this user.",
            "SyncMyFonts could not open the managed font folder. Diagnostics will show the folder path.",
        );
    }

    fn open_logs_folder(&mut self) {
        let folder = app_log_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating log folder {}", path.display()))?;
            Ok(path)
        });
        self.open_folder_action(
            "Open Logs",
            folder,
            "This folder contains SyncMyFonts action history and support logs.",
            "SyncMyFonts could not open the log folder. Diagnostics will show the folder path.",
        );
    }

    fn open_folder_action(
        &mut self,
        action: &str,
        folder: Result<PathBuf>,
        success_message: &str,
        failure_message: &str,
    ) {
        match folder.and_then(open_path) {
            Ok(path) => {
                self.output = format!("{action}: {}", path.display());
                self.next_step = success_message.to_string();
                self.last_result = format!(
                    "{action} completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action(action, "success", 0, &self.next_step);
            }
            Err(error) => {
                self.output = error.to_string();
                self.next_step = failure_message.to_string();
                self.last_result = format!("{action} failed.");
                self.warning_count = 1;
                let _ = record_action(action, "failed", 1, &self.next_step);
            }
        }
    }

    fn discover_peers(&mut self) {
        let port = self
            .listen
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .unwrap_or(7370);
        self.start_task("Finding LAN peers", move || match discover_lan_peers(port) {
            Ok(peers) => {
                let discovered_peer = peers.first().cloned();
                let next_step = if discovered_peer.is_some() {
                        "Enter the pairing code shown on that computer, then click Pair Peer."
                            .to_string()
                } else {
                    format!(
                        "No sharing peers answered. Make sure the other computer is sharing and on the same trusted LAN. {}",
                        platform_manual_peer_fallback_guidance()
                    )
                };
                gui_ok_with_updates(&peers, next_step, None, discovered_peer, false, false, 0)
            }
            Err(error) => gui_error(error),
        });
    }

    fn pair_peer(&mut self) {
        let name = self.peer_name.clone();
        let url = self.peer_url.clone();
        let pairing_code = self.pairing_code.clone();
        self.start_task("Pairing peer", move || {
            match pair_lan_peer(name, url, pairing_code) {
                Ok(peer) => {
                    let next_step = format!(
                        "{} is paired and saved. Preview from the peer before installing.",
                        peer.name
                    );
                    let output = redacted_peer_config(&peer);
                    gui_ok_with_updates(
                        &output,
                        next_step,
                        Some(peer.clone()),
                        None,
                        false,
                        true,
                        0,
                    )
                }
                Err(error) => gui_error(error),
            }
        });
    }

    fn save_peer(&mut self) {
        let name = self.peer_name.clone();
        let url = self.peer_url.clone();
        let lan_key = empty_to_none(&self.peer_key);
        self.start_task("Saving peer", move || {
            match add_lan_peer(name, url, lan_key) {
                Ok(peer) => {
                    let next_step = format!(
                        "{} is saved. Use Sync Saved Peers for repeat syncs.",
                        peer.name
                    );
                    let output = redacted_peer_config(&peer);
                    gui_ok_with_updates(
                        &output,
                        next_step,
                        Some(peer.clone()),
                        None,
                        false,
                        true,
                        0,
                    )
                }
                Err(error) => gui_error(error),
            }
        });
    }

    fn forget_peer(&mut self) {
        let name = self.peer_name.clone();
        self.start_task("Forgetting peer", move || match forget_lan_peer(&name) {
            Ok(result) => {
                let next_step = if result.removed {
                    "Peer removed. Pair or save it again if you still need it.".to_string()
                } else {
                    "No saved peer matched that name.".to_string()
                };
                let clear_peer_key = result.removed;
                gui_ok_with_updates(&result, next_step, None, None, clear_peer_key, true, 0)
            }
            Err(error) => gui_error(error),
        });
    }

    fn test_peer(&mut self) {
        let peer_url = self.peer_url.clone();
        let peer_key = empty_to_none(&self.peer_key);
        self.start_task("Testing peer", move || {
            match lan_sync(&peer_url, peer_key.as_deref(), true) {
                Ok(report) => {
                    let next_step = format!(
                        "Connected. Peer reports {} fonts. Preview or install missing fonts next.",
                        report.peer_fonts
                    );
                    gui_ok(&report, next_step)
                }
                Err(error) => gui_error(error),
            }
        });
    }

    fn sync_peer(&mut self, dry_run: bool) {
        let peer_url = self.peer_url.clone();
        let peer_key = empty_to_none(&self.peer_key);
        let action = if dry_run {
            "Previewing peer"
        } else {
            "Getting missing fonts"
        };
        self.start_task(action, move || {
            match lan_sync(&peer_url, peer_key.as_deref(), dry_run) {
                Ok(report) => {
                    let next_step = if dry_run {
                        let would_install = report
                            .skipped
                            .iter()
                            .filter(|line| line.starts_with("would install "))
                            .count();
                        if would_install == 0 {
                            "No missing installable fonts were found from this peer.".to_string()
                        } else {
                            format!(
                                "{would_install} missing font(s) can be installed from this peer."
                            )
                        }
                    } else if report.installed.is_empty() {
                        "No new fonts were installed.".to_string()
                    } else {
                        "Installed fonts are ready. Reopen design apps if they do not appear yet."
                            .to_string()
                    };
                    gui_ok(&report, next_step)
                }
                Err(error) => gui_error(error),
            }
        });
    }

    fn sync_saved_peers(&mut self, dry_run: bool) {
        let action = if dry_run {
            "Dry-running saved peers"
        } else {
            "Syncing saved peers"
        };
        self.start_task(action, move || match lan_sync_all(dry_run) {
            Ok(report) => {
                let warnings = report.peers.iter().filter(|peer| peer.error.is_some()).count();
                let installed = report
                    .peers
                    .iter()
                    .map(|peer| peer.installed.len())
                    .sum::<usize>();
                let next_step = if dry_run {
                    "Dry run complete. Review the peer results before syncing.".to_string()
                } else if installed == 0 {
                    "Saved peer sync finished. No new fonts were installed.".to_string()
                } else {
                    format!(
                        "Installed {installed} font(s). Reopen design apps if they do not appear yet."
                    )
                };
                gui_ok_with_warning_count(&report, next_step, warnings)
            }
            Err(error) => gui_error(error),
        });
    }

    fn install_startup_sync(&mut self) {
        self.start_task("Enabling sign-in sync", || match install_startup_sync() {
            Ok(report) => {
                let next_step = if report.saved_peer_count == 0 {
                    "Sign-in sync is installed, but no peers are saved yet. Pair or save a peer next."
                        .to_string()
                } else {
                    "Saved peers will sync when you sign in. Reopen design apps after new fonts install."
                        .to_string()
                };
                gui_ok(&report, next_step)
            }
            Err(error) => gui_error(error),
        });
    }

    fn install_app_shortcuts(&mut self) {
        self.start_task("Installing app shortcuts", || {
            match install_app_shortcuts() {
                Ok(report) => {
                    let next_step = if report.installed {
                        "Shortcuts are installed for common SyncMyFonts actions.".to_string()
                    } else {
                        report.message.clone()
                    };
                    gui_ok(&report, next_step)
                }
                Err(error) => gui_error(error),
            }
        });
    }

    fn start_share(&mut self) {
        if self.share.is_some() {
            self.next_step = "Sharing is already on.".to_string();
            return;
        }
        let listen: SocketAddr = match self.listen.parse() {
            Ok(listen) => listen,
            Err(error) => {
                self.output = format!("invalid listen address: {error}");
                self.last_result = "Share Fonts On LAN failed before starting.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On LAN", "failed", 1, &self.output);
                return;
            }
        };
        let exe = match agent_command_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.output = format!("locating current executable failed: {error}");
                self.last_result = "Share Fonts On LAN failed before starting.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On LAN", "failed", 1, &self.output);
                return;
            }
        };
        let mut command = Command::new(exe);
        command.args(["lan-serve", "--listen", &listen.to_string()]);
        let provided_key = empty_to_none(&self.share_key);
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
        match command
            .spawn()
            .map_err(anyhow::Error::from)
            .and_then(|child| wait_for_share_start(child, listen))
        {
            Ok(child) => {
                self.share = Some(RunningShare { child, listen });
                self.refresh_status();
                let response = ShareResponse {
                    sharing: true,
                    message: format!("Sharing fonts at {}.", self.share_urls.join(", ")),
                    urls: self.share_urls.clone(),
                    pairing_expires_seconds: pairing_code
                        .as_ref()
                        .map(|_| PAIRING_CODE_TTL.as_secs()),
                    pairing_code,
                };
                if let Some(code) = &response.pairing_code {
                    self.next_step = format!(
                        "Pairing code {code} is ready. Enter it on the other computer. {}",
                        platform_lan_sharing_guidance()
                    );
                } else {
                    self.next_step = format!(
                        "Sharing is on. Use the shown LAN URL and shared key from another computer. {}",
                        platform_lan_sharing_guidance()
                    );
                }
                self.output =
                    serde_json::to_string_pretty(&response).unwrap_or_else(|_| "ok".to_string());
                self.last_result = format!(
                    "Share Fonts On LAN completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Share Fonts On LAN", "success", 0, &self.next_step);
            }
            Err(error) => {
                self.output = error.to_string();
                self.next_step = format!(
                    "Sharing failed to start. Check whether another SyncMyFonts share is already using that port. {}",
                    platform_lan_sharing_guidance()
                );
                self.last_result = "Share Fonts On LAN failed.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On LAN", "failed", 1, &self.next_step);
            }
        }
    }

    fn stop_share(&mut self) {
        let Some(mut share) = self.share.take() else {
            self.next_step = "Sharing is already off.".to_string();
            return;
        };
        let _ = share.child.kill();
        let _ = share.child.wait();
        self.refresh_status();
        self.next_step =
            "Sharing is off. Start sharing again when another computer needs fonts.".to_string();
        self.output = "Stopped sharing fonts.".to_string();
        self.last_result = format!(
            "Stop Sharing completed at {}",
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.warning_count = 0;
        let _ = record_action("Stop Sharing", "success", 0, &self.next_step);
    }

    fn prune_stopped_share(&mut self) {
        let stopped = self
            .share
            .as_mut()
            .and_then(|share| share.child.try_wait().ok().flatten())
            .is_some();
        if stopped {
            self.share = None;
        }
    }
}

impl Drop for SyncMyFontsGui {
    fn drop(&mut self) {
        if let Some(mut share) = self.share.take() {
            let _ = share.child.kill();
            let _ = share.child.wait();
        }
    }
}

impl eframe::App for SyncMyFontsGui {
    fn ui(&mut self, ui: &mut eframe::egui::Ui, _frame: &mut eframe::Frame) {
        self.prune_stopped_share();
        self.poll_task();
        let task_running = self.task.is_some();
        if task_running {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        }
        ui.horizontal(|ui| {
            ui.heading("SyncMyFonts");
            if ui.button("Refresh").clicked() {
                self.refresh_status();
            }
        });
        ui.label(&self.status);
        ui.label(&self.saved_peer_summary);
        ui.label(format!(
            "Last result: {} · warnings: {}",
            self.last_result, self.warning_count
        ));
        ui.horizontal(|ui| {
            ui.label("Device Name");
            ui.text_edit_singleline(&mut self.device_name_input);
            if ui.button("Save Name").clicked() {
                self.save_device_name();
            }
        });
        if let Some(action) = &self.current_action {
            ui.label(format!("{action} is still running..."));
        }

        ui.separator();
        ui.heading("Local Font Library");
        ui.add_enabled_ui(!task_running, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Scan Fonts").clicked() {
                    self.scan_fonts();
                }
                if ui.button("Verify Managed Fonts").clicked() {
                    self.verify_managed();
                }
                if ui.button("Diagnostics").clicked() {
                    self.run_diagnostics();
                }
                if ui.button("Readiness Check").clicked() {
                    self.run_doctor();
                }
                if ui.button("Validation Report").clicked() {
                    self.run_validation_report();
                }
                if ui.button("Open Managed Folder").clicked() {
                    self.open_managed_font_folder();
                }
                if ui.button("Open Logs").clicked() {
                    self.open_logs_folder();
                }
                if ui.button("Sync Saved Peers").clicked() {
                    self.sync_saved_peers(false);
                }
                if ui.button("Dry Run Saved Peers").clicked() {
                    self.sync_saved_peers(true);
                }
                if ui.button("Enable Sign-In Sync").clicked() {
                    self.install_startup_sync();
                }
                if ui.button("Install App Shortcuts").clicked() {
                    self.install_app_shortcuts();
                }
            });
        });

        ui.separator();
        ui.heading("Saved LAN Peer");
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.peer_name);
            ui.label("URL");
            ui.text_edit_singleline(&mut self.peer_url);
        });
        ui.horizontal(|ui| {
            ui.label("Shared Key");
            ui.add(eframe::egui::TextEdit::singleline(&mut self.peer_key).password(true));
            ui.label("Pairing Code");
            ui.text_edit_singleline(&mut self.pairing_code);
        });
        ui.add_enabled_ui(!task_running, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Find LAN Peers").clicked() {
                    self.discover_peers();
                }
                if ui.button("Load First Saved Peer").clicked() {
                    self.load_saved_peers_into_form();
                }
                if ui.button("Pair Peer").clicked() {
                    self.pair_peer();
                }
                if ui.button("Test Peer").clicked() {
                    self.test_peer();
                }
                if ui.button("Preview From Peer").clicked() {
                    self.sync_peer(true);
                }
                if ui.button("Get Missing Fonts").clicked() {
                    self.sync_peer(false);
                }
                if ui.button("Save Peer").clicked() {
                    self.save_peer();
                }
                if ui.button("Forget Peer").clicked() {
                    self.forget_peer();
                }
            });
        });

        ui.separator();
        ui.heading("Share This Device");
        ui.horizontal(|ui| {
            ui.label("Listen Address");
            ui.text_edit_singleline(&mut self.listen);
            ui.label("Shared Key");
            ui.add(eframe::egui::TextEdit::singleline(&mut self.share_key).password(true));
        });
        ui.add_enabled_ui(!task_running, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button("Share Fonts On LAN").clicked() {
                    self.start_share();
                }
                if ui.button("Stop Sharing").clicked() {
                    self.stop_share();
                }
            });
        });
        if self.share_urls.is_empty() {
            ui.label("Sharing is off. No port forwarding is required.");
        } else {
            ui.label(format!(
                "Use this URL from another computer: {}",
                self.share_urls.join(" or ")
            ));
        }

        ui.separator();
        ui.heading("Result");
        ui.label(&self.next_step);
        eframe::egui::ScrollArea::vertical()
            .max_height(260.0)
            .show(ui, |ui| {
                ui.add(
                    eframe::egui::TextEdit::multiline(&mut self.output)
                        .desired_width(f32::INFINITY)
                        .desired_rows(14),
                );
            });
    }
}

fn gui_ok<T: Serialize>(value: &T, next_step: String) -> GuiTaskResult {
    gui_ok_with_updates(value, next_step, None, None, false, false, 0)
}

fn gui_ok_with_warning_count<T: Serialize>(
    value: &T,
    next_step: String,
    warning_count: usize,
) -> GuiTaskResult {
    gui_ok_with_updates(value, next_step, None, None, false, false, warning_count)
}

fn gui_ok_with_updates<T: Serialize>(
    value: &T,
    next_step: String,
    peer: Option<LanPeerConfig>,
    discovered_peer: Option<LanDiscoveredPeer>,
    clear_peer_key: bool,
    refresh_saved_peers: bool,
    warning_count: usize,
) -> GuiTaskResult {
    GuiTaskResult {
        output: serde_json::to_string_pretty(value).unwrap_or_else(|_| "ok".to_string()),
        next_step,
        peer,
        discovered_peer,
        clear_peer_key,
        refresh_saved_peers,
        warning_count,
    }
}

fn gui_error(error: anyhow::Error) -> GuiTaskResult {
    GuiTaskResult {
        output: error.to_string(),
        next_step: format!(
            "That action failed. Review the output, then check the peer URL, pairing code, or network access. {}",
            platform_manual_peer_fallback_guidance()
        ),
        peer: None,
        discovered_peer: None,
        clear_peer_key: false,
        refresh_saved_peers: false,
        warning_count: 1,
    }
}

fn redacted_peer_config(peer: &LanPeerConfig) -> RedactedPeer {
    RedactedPeer {
        name: peer.name.clone(),
        url: peer.url.clone(),
        has_lan_key: peer.lan_key.is_some(),
    }
}

fn platform_lan_sharing_guidance() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "If Windows Firewall asks, allow SyncMyFonts on Private networks only. No port forwarding is needed."
    }
    #[cfg(target_os = "macos")]
    {
        "If macOS asks for Local Network access, allow it for SyncMyFonts. No port forwarding is needed."
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "Allow local network access if your OS asks. No port forwarding is needed."
    }
}

fn platform_manual_peer_fallback_guidance() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Client-only sync should not need an inbound firewall prompt; if discovery fails, paste the sharing computer's LAN URL manually."
    }
    #[cfg(target_os = "macos")]
    {
        "If Local Network discovery is denied or unavailable, paste the sharing computer's LAN URL manually."
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "If discovery fails, paste the sharing computer's LAN URL manually."
    }
}

fn empty_to_none(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn open_path(path: PathBuf) -> Result<PathBuf> {
    let status = platform_open_command(&path)
        .status()
        .with_context(|| format!("opening {}", path.display()))?;
    if !status.success() {
        bail!("opening {} failed with {}", path.display(), status);
    }
    Ok(path)
}

fn platform_open_command(path: &Path) -> Command {
    #[cfg(target_os = "macos")]
    {
        let mut command = Command::new("open");
        command.arg(path);
        command
    }
    #[cfg(target_os = "windows")]
    {
        let mut command = Command::new("explorer");
        command.arg(path);
        command
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let mut command = Command::new("xdg-open");
        command.arg(path);
        command
    }
}

fn agent_command_exe() -> Result<PathBuf> {
    let current = std::env::current_exe().context("locating current executable")?;
    let agent_name = if cfg!(target_os = "windows") {
        "syncmyfonts-agent.exe"
    } else {
        "syncmyfonts-agent"
    };
    if current
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case(agent_name))
    {
        return Ok(current);
    }
    let sibling = current.with_file_name(agent_name);
    if sibling.exists() {
        return Ok(sibling);
    }
    Ok(current)
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
            friendly_device_name: None,
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
    if let Some(name) = config.friendly_device_name.clone() {
        let normalized = normalize_friendly_device_name(&name);
        if normalized != config.friendly_device_name {
            config.friendly_device_name = normalized;
            changed = true;
        }
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

fn set_friendly_device_name(name: String) -> Result<AppConfig> {
    let mut config = load_app_config()?;
    config.friendly_device_name = normalize_friendly_device_name(&name);
    save_app_config(&config)?;
    Ok(config)
}

fn normalize_friendly_device_name(name: &str) -> Option<String> {
    let mut normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
    if normalized.len() > 80 {
        normalized.truncate(80);
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn load_app_history() -> Result<AppHistory> {
    let path = app_history_path()?;
    if !path.exists() {
        return Ok(AppHistory {
            schema: 1,
            last_action: None,
            recent: Vec::new(),
        });
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let mut history: AppHistory =
        serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))?;
    if history.schema == 0 {
        history.schema = 1;
    }
    Ok(history)
}

fn save_app_history(history: &AppHistory) -> Result<()> {
    let path = app_history_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let temp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(history).context("serializing app history")?;
    fs::write(&temp, bytes).with_context(|| format!("writing {}", temp.display()))?;
    fs::rename(&temp, &path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn record_action(action: &str, status: &str, warning_count: usize, result: &str) -> Result<()> {
    let record = ActionRecord {
        action: action.to_string(),
        status: status.to_string(),
        finished_at: Utc::now().to_rfc3339(),
        warning_count,
        result: sanitize_action_result(result),
    };
    let mut history = load_app_history().unwrap_or_default();
    history.schema = 1;
    history.last_action = Some(record.clone());
    history.recent.insert(0, record.clone());
    history.recent.truncate(20);
    save_app_history(&history)?;
    append_action_log(&record)?;
    Ok(())
}

fn record_action_best_effort(action: &str, status: &str, warning_count: usize, result: &str) {
    if let Err(error) = record_action(action, status, warning_count, result) {
        eprintln!("SyncMyFonts could not save action history: {error}");
    }
}

fn append_action_log(record: &ActionRecord) -> Result<()> {
    let dir = app_log_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = app_action_log_path()?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    serde_json::to_writer(&mut file, record).context("serializing action log record")?;
    file.write_all(b"\n")
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

fn sanitize_action_result(result: &str) -> String {
    let mut sanitized = result
        .split_whitespace()
        .map(|word| {
            let trimmed = word.trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '-');
            if trimmed.len() == 8 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
                "[redacted-code]".to_string()
            } else if trimmed.starts_with("smf-") {
                "[redacted-token]".to_string()
            } else {
                word.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if sanitized.len() > 500 {
        sanitized.truncate(500);
        sanitized.push_str("...");
    }
    sanitized
}

fn support_report_text(report: &DiagnosticsReport) -> String {
    let mut lines = vec![
        "SyncMyFonts Support Report".to_string(),
        format!("Version: {}", report.version),
        format!("Platform: {}", report.platform),
        format!("Device: {}", report.device_name),
        format!("Config path: {}", report.config_path.display()),
        format!("Log dir: {}", report.log_dir.display()),
        format!("History path: {}", report.history_path.display()),
        format!("User font dir: {}", report.user_font_dir.display()),
        format!("Managed font dir: {}", report.managed_font_dir.display()),
        format!(
            "Managed manifest: {}",
            report.managed_manifest_path.display()
        ),
        format!("Saved peers: {}", report.saved_peer_count),
        format!("User fonts scanned: {}", report.user_font_count),
        format!(
            "Managed manifest records: {}",
            report.managed_manifest_count
        ),
        format!("Warnings: {}", report.warnings.len()),
    ];
    if let Some(action) = &report.last_action {
        lines.push(format!("Last action: {}", action.action));
        lines.push(format!("Last action status: {}", action.status));
        lines.push(format!("Last action finished: {}", action.finished_at));
        lines.push(format!("Last action warnings: {}", action.warning_count));
        lines.push(format!("Last action result: {}", action.result));
    } else {
        lines.push("Last action: none recorded".to_string());
    }
    if !report.saved_peers.is_empty() {
        lines.push("Saved peer summary:".to_string());
        for peer in &report.saved_peers {
            lines.push(format!(
                "- {} at {} (key saved: {})",
                peer.name, peer.url, peer.has_lan_key
            ));
        }
    }
    if !report.warnings.is_empty() {
        lines.push("Warnings:".to_string());
        for warning in &report.warnings {
            lines.push(format!("- {warning}"));
        }
    }
    lines.join("\n")
}

fn app_config_path() -> Result<PathBuf> {
    Ok(app_data_dir()?.join("config.json"))
}

fn app_history_path() -> Result<PathBuf> {
    Ok(app_log_dir()?.join("action-history.json"))
}

fn app_action_log_path() -> Result<PathBuf> {
    Ok(app_log_dir()?.join("action-history.jsonl"))
}

fn startup_sync_helper_path() -> Result<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        Ok(app_data_dir()?.join("run-sign-in-sync.cmd"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        Ok(app_data_dir()?.join("run-sign-in-sync.sh"))
    }
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

fn app_log_dir() -> Result<PathBuf> {
    if let Ok(log_dir) = std::env::var("SYNCMYFONTS_LOG_DIR") {
        return Ok(PathBuf::from(log_dir));
    }

    #[cfg(target_os = "macos")]
    {
        use directories::UserDirs;
        let home = UserDirs::new()
            .ok_or_else(|| anyhow!("user home directory unavailable"))?
            .home_dir()
            .to_path_buf();
        return Ok(home.join("Library/Logs/SyncMyFonts"));
    }
    #[cfg(target_os = "windows")]
    {
        Ok(app_data_dir()?.join("logs"))
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(app_data_dir()?.join("logs"))
    }
}

fn install_startup_sync() -> Result<StartupSyncReport> {
    let agent_path = agent_command_exe()?;
    let config = load_app_config()?;
    let saved_peer_count = config.peers.len();
    let helper_path = startup_sync_helper_path()?;
    let helper_parent = helper_path
        .parent()
        .ok_or_else(|| anyhow!("startup helper parent unavailable"))?;
    fs::create_dir_all(helper_parent)
        .with_context(|| format!("creating {}", helper_parent.display()))?;
    let log_dir = app_log_dir()?;
    fs::create_dir_all(&log_dir).with_context(|| format!("creating {}", log_dir.display()))?;

    #[cfg(target_os = "macos")]
    {
        let registration_path = macos_startup_sync_plist_path()?;
        write_macos_startup_sync_helper(&helper_path, &agent_path, &log_dir)?;
        let plist = render_macos_startup_sync_plist(&helper_path, &log_dir, app_data_dir()?);
        if let Some(parent) = registration_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        fs::write(&registration_path, plist)
            .with_context(|| format!("writing {}", registration_path.display()))?;

        if std::env::var("SYNCMYFONTS_SKIP_STARTUP_REGISTRATION").as_deref() != Ok("1") {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}", unsafe { libc_uid() })])
                .arg(&registration_path)
                .status();
            let status = Command::new("launchctl")
                .args(["bootstrap", &format!("gui/{}", unsafe { libc_uid() })])
                .arg(&registration_path)
                .status()
                .context("registering SyncMyFonts LaunchAgent")?;
            if !status.success() {
                bail!("registering SyncMyFonts LaunchAgent failed with {status}");
            }
            let _ = Command::new("launchctl")
                .args([
                    "enable",
                    &format!("gui/{}/com.syncmyfonts.signin-sync", unsafe { libc_uid() }),
                ])
                .status();
        }

        return Ok(StartupSyncReport {
            installed: true,
            platform: platform_name(),
            agent_path,
            helper_path,
            registration_path,
            saved_peer_count,
            message: "Installed a per-user LaunchAgent for saved-peer sync at sign-in.".to_string(),
        });
    }

    #[cfg(target_os = "windows")]
    {
        let registration_path = windows_startup_sync_shortcut_path()?;
        write_windows_startup_sync_helper(&helper_path, &agent_path, &log_dir)?;
        if let Some(parent) = registration_path.parent() {
            fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let launcher = render_windows_startup_sync_launcher(&helper_path);
        fs::write(&registration_path, launcher)
            .with_context(|| format!("writing {}", registration_path.display()))?;

        return Ok(StartupSyncReport {
            installed: true,
            platform: platform_name(),
            agent_path,
            helper_path,
            registration_path,
            saved_peer_count,
            message: "Installed a per-user Startup folder helper for saved-peer sync at sign-in."
                .to_string(),
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        write_unix_startup_sync_helper(&helper_path, &agent_path, &log_dir)?;
        Ok(StartupSyncReport {
            installed: false,
            platform: platform_name(),
            agent_path,
            helper_path: helper_path.clone(),
            registration_path: helper_path,
            saved_peer_count,
            message: "Wrote a saved-peer sync helper, but automatic sign-in registration is not supported on this platform yet."
                .to_string(),
        })
    }
}

fn install_app_shortcuts() -> Result<AppShortcutReport> {
    #[cfg(target_os = "windows")]
    {
        let agent_path = agent_command_exe()?;
        let directory = windows_app_shortcuts_dir()?;
        fs::create_dir_all(&directory)
            .with_context(|| format!("creating {}", directory.display()))?;

        let shortcuts = vec![
            write_windows_app_shortcut(
                &directory,
                "SyncMyFonts.cmd",
                &agent_path,
                &["gui"],
                false,
            )?,
            write_windows_app_shortcut(
                &directory,
                "Sync Saved Peers.cmd",
                &agent_path,
                &["lan-sync-all"],
                true,
            )?,
            write_windows_app_shortcut(
                &directory,
                "Preview Saved Peers.cmd",
                &agent_path,
                &["lan-sync-all", "--dry-run"],
                true,
            )?,
            write_windows_app_shortcut(
                &directory,
                "Diagnostics.cmd",
                &agent_path,
                &["diagnostics"],
                true,
            )?,
            write_windows_app_shortcut(
                &directory,
                "Readiness Check.cmd",
                &agent_path,
                &["doctor"],
                true,
            )?,
        ];

        return Ok(AppShortcutReport {
            installed: true,
            platform: platform_name(),
            directory,
            shortcuts,
            message: "Installed current-user Start Menu shortcuts for SyncMyFonts.".to_string(),
        });
    }

    #[cfg(not(target_os = "windows"))]
    {
        let directory = app_data_dir()?;
        Ok(AppShortcutReport {
            installed: false,
            platform: platform_name(),
            directory,
            shortcuts: Vec::new(),
            message: "Native app shortcuts are currently only installed on Windows. On macOS, open SyncMyFonts.app from the release folder.".to_string(),
        })
    }
}

#[cfg(target_os = "macos")]
fn macos_startup_sync_plist_path() -> Result<PathBuf> {
    use directories::UserDirs;
    let home = UserDirs::new()
        .ok_or_else(|| anyhow!("user home directory unavailable"))?
        .home_dir()
        .to_path_buf();
    Ok(home
        .join("Library/LaunchAgents")
        .join("com.syncmyfonts.signin-sync.plist"))
}

#[cfg(target_os = "windows")]
fn windows_startup_sync_shortcut_path() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").context("APPDATA unavailable")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft/Windows/Start Menu/Programs/Startup")
        .join("SyncMyFonts Sign-In Sync.cmd"))
}

#[cfg(target_os = "windows")]
fn windows_app_shortcuts_dir() -> Result<PathBuf> {
    let appdata = std::env::var("APPDATA").context("APPDATA unavailable")?;
    Ok(PathBuf::from(appdata)
        .join("Microsoft/Windows/Start Menu/Programs")
        .join("SyncMyFonts"))
}

#[cfg(target_os = "windows")]
fn write_windows_app_shortcut(
    directory: &Path,
    file_name: &str,
    agent_path: &Path,
    args: &[&str],
    pause: bool,
) -> Result<PathBuf> {
    let path = directory.join(file_name);
    fs::write(&path, render_windows_app_shortcut(agent_path, args, pause))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

#[cfg(any(target_os = "windows", test))]
fn render_windows_app_shortcut(agent_path: &Path, args: &[&str], pause: bool) -> String {
    let rendered_args = args
        .iter()
        .map(|arg| format!(" \"{}\"", escape_windows_cmd_arg(arg)))
        .collect::<String>();
    let pause_line = if pause { "echo.\r\npause\r\n" } else { "" };
    format!(
        "@echo off\r\n\"{}\"{}\r\n{}",
        agent_path.display(),
        rendered_args,
        pause_line
    )
}

fn render_unix_startup_sync_helper(agent_path: &Path, log_dir: &Path) -> String {
    format!(
        "#!/bin/sh\nset -eu\n{} lan-sync-all >> {} 2>> {}\n",
        shell_quote(&agent_path.display().to_string()),
        shell_quote(&log_dir.join("signin-sync.log").display().to_string()),
        shell_quote(&log_dir.join("signin-sync.err.log").display().to_string())
    )
}

fn write_unix_startup_sync_helper(
    helper_path: &Path,
    agent_path: &Path,
    log_dir: &Path,
) -> Result<()> {
    fs::write(
        helper_path,
        render_unix_startup_sync_helper(agent_path, log_dir),
    )
    .with_context(|| format!("writing {}", helper_path.display()))?;
    make_executable(helper_path)
}

#[cfg(target_os = "macos")]
fn write_macos_startup_sync_helper(
    helper_path: &Path,
    agent_path: &Path,
    log_dir: &Path,
) -> Result<()> {
    write_unix_startup_sync_helper(helper_path, agent_path, log_dir)
}

#[cfg(target_os = "macos")]
fn render_macos_startup_sync_plist(
    helper_path: &Path,
    log_dir: &Path,
    working_dir: PathBuf,
) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>com.syncmyfonts.signin-sync</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{}</string>
  <key>StandardErrorPath</key>
  <string>{}</string>
  <key>WorkingDirectory</key>
  <string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&helper_path.display().to_string()),
        xml_escape(
            &log_dir
                .join("signin-sync.launchd.log")
                .display()
                .to_string()
        ),
        xml_escape(
            &log_dir
                .join("signin-sync.launchd.err.log")
                .display()
                .to_string()
        ),
        xml_escape(&working_dir.display().to_string())
    )
}

#[cfg(target_os = "windows")]
fn render_windows_startup_sync_helper(agent_path: &Path, log_dir: &Path) -> String {
    format!(
        "@echo off\r\n\"{}\" lan-sync-all >> \"{}\" 2>> \"{}\"\r\n",
        agent_path.display(),
        log_dir.join("signin-sync.log").display(),
        log_dir.join("signin-sync.err.log").display()
    )
}

#[cfg(target_os = "windows")]
fn render_windows_startup_sync_launcher(helper_path: &Path) -> String {
    format!("@echo off\r\ncall \"{}\"\r\n", helper_path.display())
}

#[cfg(target_os = "windows")]
fn write_windows_startup_sync_helper(
    helper_path: &Path,
    agent_path: &Path,
    log_dir: &Path,
) -> Result<()> {
    fs::write(
        helper_path,
        render_windows_startup_sync_helper(agent_path, log_dir),
    )
    .with_context(|| format!("writing {}", helper_path.display()))
}

#[cfg(unix)]
fn make_executable(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = fs::metadata(path)
        .with_context(|| format!("reading permissions for {}", path.display()))?
        .permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(path, permissions)
        .with_context(|| format!("setting executable bit on {}", path.display()))
}

#[cfg(not(unix))]
fn make_executable(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
unsafe fn libc_uid() -> u32 {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    unsafe { getuid() }
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

#[cfg(any(target_os = "windows", test))]
fn escape_windows_cmd_arg(value: &str) -> String {
    value.replace('"', "\"\"")
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

fn verify_managed_fonts() -> Result<ManagedVerifyReport> {
    let manifest_path = managed_manifest_path()?;
    let manifest = load_managed_manifest()?;
    let total = manifest.installed.len();
    let mut report = ManagedVerifyReport {
        manifest_path,
        total,
        ok: 0,
        missing: Vec::new(),
        modified: Vec::new(),
        unreadable: Vec::new(),
    };

    for record in manifest.installed {
        if !record.path.exists() {
            report.missing.push(managed_verify_issue(
                &record,
                "managed font file is missing",
            ));
            continue;
        }

        let bytes = match fs::read(&record.path) {
            Ok(bytes) => bytes,
            Err(error) => {
                report.unreadable.push(managed_verify_issue(
                    &record,
                    &format!("managed font file could not be read: {error}"),
                ));
                continue;
            }
        };
        let actual_sha256 = hex::encode(Sha256::digest(&bytes));
        if actual_sha256 != record.sha256 {
            report.modified.push(managed_verify_issue(
                &record,
                &format!(
                    "managed font hash changed from {} to {}",
                    record.sha256, actual_sha256
                ),
            ));
            continue;
        }
        if bytes.len() as u64 != record.size_bytes {
            report.modified.push(managed_verify_issue(
                &record,
                &format!(
                    "managed font size changed from {} to {} bytes",
                    record.size_bytes,
                    bytes.len()
                ),
            ));
            continue;
        }

        report.ok += 1;
    }

    Ok(report)
}

fn managed_verify_issue(record: &ManagedFontRecord, message: &str) -> ManagedVerifyIssue {
    ManagedVerifyIssue {
        sha256: record.sha256.clone(),
        file_name: record.file_name.clone(),
        path: record.path.clone(),
        message: message.to_string(),
    }
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

    let safe_name = safe_file_name(remote_file_name, expected_sha256);
    if let Some(conflict_path) = system_font_filename_conflict(&safe_name)? {
        bail!(
            "system-font-conflict: {} conflicts with {}",
            safe_name,
            conflict_path.display()
        );
    }

    let install_dir = managed_install_dir()?;
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let destination = unique_destination(&install_dir, &safe_name, expected_sha256)?;
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
    safe_name: &str,
    expected_sha256: &str,
) -> Result<PathBuf> {
    let candidate = install_dir.join(safe_name);
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

fn system_font_filename_conflict(safe_name: &str) -> Result<Option<PathBuf>> {
    let safe_name_lower = safe_name.to_ascii_lowercase();
    for dir in system_font_dirs()? {
        if !dir.exists() {
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::warn!(
                    "skipping unreadable system font directory {}: {error}",
                    dir.display()
                );
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    tracing::warn!("skipping unreadable system font entry: {error}");
                    continue;
                }
            };
            let file_name = entry.file_name();
            if file_name
                .to_str()
                .map(|name| name.eq_ignore_ascii_case(&safe_name_lower))
                .unwrap_or(false)
            {
                return Ok(Some(entry.path()));
            }
        }
        let candidate = dir.join(safe_name);
        if candidate.exists() {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn system_font_dirs() -> Result<Vec<PathBuf>> {
    if let Ok(dirs) = std::env::var("SYNCMYFONTS_SYSTEM_FONT_DIRS") {
        return Ok(std::env::split_paths(&dirs).collect());
    }

    #[cfg(target_os = "macos")]
    {
        Ok(vec![
            PathBuf::from("/System/Library/Fonts"),
            PathBuf::from("/Library/Fonts"),
            PathBuf::from("/Network/Library/Fonts"),
        ])
    }
    #[cfg(target_os = "windows")]
    {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".to_string());
        Ok(vec![PathBuf::from(windir).join("Fonts")])
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(Vec::new())
    }
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
    std::env::var("SYNCMYFONTS_DEVICE_NAME")
        .ok()
        .and_then(|name| normalize_friendly_device_name(&name))
        .or_else(|| {
            load_app_config()
                .ok()
                .and_then(|config| config.friendly_device_name)
        })
        .unwrap_or_else(fallback_device_name)
}

fn fallback_device_name() -> String {
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
      <div class="stack">
        <label>Device Name <input id="deviceName" placeholder="Workshop PC"></label>
        <div class="row">
          <button onclick="saveDeviceName()">Save Name</button>
          <button onclick="refresh()">Refresh</button>
        </div>
      </div>
    </header>

    <section>
      <h2>Local Font Library</h2>
      <div class="row">
        <button onclick="scanFonts()">Scan Fonts</button>
        <button onclick="verifyManaged()">Verify Managed Fonts</button>
        <button onclick="diagnostics()">Diagnostics</button>
        <button onclick="openManagedFolder()">Open Managed Folder</button>
        <button onclick="openLogsFolder()">Open Logs</button>
        <button class="primary" onclick="syncAll(false)">Sync Saved Peers</button>
        <button onclick="syncAll(true)">Dry Run</button>
      </div>
    </section>

    <section>
      <h2>Saved LAN Peer</h2>
      <div class="grid">
        <label>Name <input id="peerName" placeholder="Workshop PC"></label>
        <label>URL <input id="peerUrl" placeholder="http://192.168.1.50:7370"></label>
        <label>Shared Key <input id="peerKey" type="password" placeholder="saved after pairing"></label>
        <label>Pairing Code <input id="pairingCode" placeholder="8 digits from sharing computer"></label>
      </div>
      <p class="row">
        <button onclick="discoverPeers()">Find LAN Peers</button>
        <button class="primary" onclick="pairPeer()">Pair Peer</button>
        <button onclick="testPeer()">Test Peer</button>
        <button onclick="syncPeer(true)">Preview From Peer</button>
        <button onclick="syncPeer(false)">Get Missing Fonts</button>
        <button onclick="savePeer()">Save Peer</button>
        <button onclick="forgetPeer()">Forget Peer</button>
        <button onclick="loadPeers()">List Peers</button>
      </p>
      <div id="discoveredPeers" class="statusline muted">No peers discovered yet.</div>
    </section>

    <section>
      <h2>Share This Device</h2>
      <div class="grid">
        <label>Listen Address <input id="listen" value="0.0.0.0:7370"></label>
        <label>Shared Key <input id="shareKey" type="password" placeholder="optional; blank creates pairing code"></label>
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
      } else if (typeof value.total === 'number' && Array.isArray(value.missing) && Array.isArray(value.modified)) {
        summary.innerHTML =
          metric('managed', value.total) +
          metric('ok', value.ok ?? 0) +
          metric('issues', (value.missing?.length ?? 0) + (value.modified?.length ?? 0) + (value.unreadable?.length ?? 0));
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
    function redactPeer(peer) {
      if (!peer || typeof peer !== 'object') return peer;
      return {
        name: peer.name,
        url: peer.url,
        has_lan_key: Boolean(peer.lan_key || peer.has_lan_key)
      };
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
        document.getElementById('deviceName').value = status.device_name;
        document.getElementById('shareUrls').textContent = status.share_urls.length
          ? `Use this URL from another computer: ${status.share_urls.join(' or ')}`
          : 'Sharing is off.';
      } catch (error) { show(error.message); }
    }
    async function saveDeviceName() {
      try {
        const result = await request('/api/device-name', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ device_name: document.getElementById('deviceName').value })
        });
        showResult(result);
        setNextStep('This name is used for LAN discovery, pairing, diagnostics, and support reports.');
        refresh();
      } catch (error) { show(error.message); }
    }
    async function scanFonts() {
      try { showResult(await request('/api/scan')); } catch (error) { show(error.message); }
    }
    async function diagnostics() {
      try { showResult(await request('/api/diagnostics')); } catch (error) { show(error.message); }
    }
    async function verifyManaged() {
      try {
        const result = await request('/api/managed/verify');
        showResult(result);
        const issues = (result.missing?.length ?? 0) + (result.modified?.length ?? 0) + (result.unreadable?.length ?? 0);
        setNextStep(issues
          ? `${issues} managed font issue${issues === 1 ? '' : 's'} found. Review the report before syncing more fonts.`
          : 'All SyncMyFonts-managed fonts still match the local manifest.');
      } catch (error) { show(error.message); }
    }
    async function openManagedFolder() {
      try {
        const result = await request('/api/managed/open', { method: 'POST' });
        showResult(result);
        setNextStep('This is where SyncMyFonts puts fonts it installs for this user.');
      } catch (error) { show(error.message); }
    }
    async function openLogsFolder() {
      try {
        const result = await request('/api/logs/open', { method: 'POST' });
        showResult(result);
        setNextStep('This folder contains SyncMyFonts action history and support logs.');
      } catch (error) { show(error.message); }
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
        showResult(redactPeer(peer));
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
        showResult(redactPeer(peer));
        setNextStep(`${peer.name} is saved. Use Sync Saved Peers for repeat syncs.`);
      } catch (error) { show(error.message); }
    }
    async function forgetPeer() {
      try {
        const name = document.getElementById('peerName').value;
        const result = await request('/api/peers/forget', {
          method: 'POST',
          headers: { 'content-type': 'application/json' },
          body: JSON.stringify({ name })
        });
        if (result.removed) {
          document.getElementById('peerKey').value = '';
          setNextStep(`${name} was removed from saved peers. Pair or save it again if you still need it.`);
        } else {
          setNextStep(`${name || 'That peer'} was not found in saved peers.`);
        }
        showResult(result);
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
    fn friendly_device_name_persists_and_normalizes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::remove_var("SYNCMYFONTS_DEVICE_NAME");
        }

        let config = set_friendly_device_name("  Shop   PC  ".to_string()).unwrap();
        let loaded = load_app_config().unwrap();
        let name = device_name();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(config.friendly_device_name.as_deref(), Some("Shop PC"));
        assert_eq!(loaded.friendly_device_name.as_deref(), Some("Shop PC"));
        assert_eq!(name, "Shop PC");
    }

    #[test]
    fn device_name_env_override_wins_over_saved_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_DEVICE_NAME", " Event   MacBook ");
        }
        set_friendly_device_name("Shop PC".to_string()).unwrap();
        let name = device_name();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_DEVICE_NAME");
        }

        assert_eq!(name, "Event MacBook");
    }

    #[test]
    fn system_font_filename_conflict_detects_exact_and_case_insensitive_names() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let system_dir = root.join("system-fonts");
        fs::create_dir_all(&system_dir).unwrap();
        fs::write(system_dir.join("ProtectedFont.TTF"), b"system").unwrap();
        unsafe {
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
        }

        let exact = system_font_filename_conflict("ProtectedFont.TTF").unwrap();
        let case_insensitive = system_font_filename_conflict("protectedfont.ttf").unwrap();
        let missing = system_font_filename_conflict("OtherFont.ttf").unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
        }

        assert_eq!(exact, Some(system_dir.join("ProtectedFont.TTF")));
        assert_eq!(case_insensitive, Some(system_dir.join("ProtectedFont.TTF")));
        assert_eq!(missing, None);
    }

    #[test]
    fn install_font_skips_system_font_filename_conflicts() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let system_dir = root.join("system-fonts");
        fs::create_dir_all(&system_dir).unwrap();
        fs::write(system_dir.join("Existing.ttf"), b"system").unwrap();
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let bytes = b"syncmyfonts fake font bytes";
        let sha256 = hex::encode(Sha256::digest(bytes));
        let error = install_font("Existing.ttf", &sha256, bytes).unwrap_err();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert!(error.to_string().contains("system-font-conflict"));
        assert!(!user_dir.join("Existing.ttf").exists());
    }

    #[test]
    fn install_font_writes_only_inside_managed_user_font_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let bytes = b"syncmyfonts managed user font bytes";
        let sha256 = hex::encode(Sha256::digest(bytes));
        let installed = install_font("ManagedOnly.ttf", &sha256, bytes).unwrap();
        let managed_dir = managed_font_dir().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert!(installed.starts_with(&managed_dir));
        assert!(managed_dir.starts_with(&user_dir));
        assert_eq!(fs::read(installed).unwrap(), bytes);
        assert!(!system_dir.exists());
    }

    #[test]
    fn scan_ignores_managed_fonts_by_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let config_dir = root.join("config");
        let user_dir = root.join("user-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        fs::create_dir_all(&user_dir).unwrap();
        fs::write(user_dir.join("UserFont.ttf"), b"user font").unwrap();
        let managed_dir = managed_font_dir().unwrap();
        fs::create_dir_all(&managed_dir).unwrap();
        let managed_path = managed_dir.join("ManagedFont.ttf");
        let managed_bytes = b"managed font";
        let managed_sha = hex::encode(Sha256::digest(managed_bytes));
        fs::write(&managed_path, managed_bytes).unwrap();
        record_managed_install(
            "ManagedFont.ttf",
            &managed_sha,
            &managed_path,
            "lan:http://127.0.0.1:7370",
            managed_bytes.len() as u64,
        )
        .unwrap();

        let default_scan = scan(false).unwrap();
        let managed_scan = scan(true).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert_eq!(
            default_scan
                .fonts
                .iter()
                .map(|font| font.file_name.as_str())
                .collect::<Vec<_>>(),
            vec!["UserFont.ttf"]
        );
        assert_eq!(managed_scan.fonts.len(), 2);
        assert!(
            managed_scan
                .fonts
                .iter()
                .any(|font| font.file_name == "ManagedFont.ttf")
        );
    }

    #[test]
    fn startup_sync_helper_uses_saved_peers_without_embedding_keys() {
        let agent_path =
            PathBuf::from("/Applications/SyncMyFonts.app/Contents/MacOS/syncmyfonts-agent");
        let log_dir = PathBuf::from("/Users/example/Library/Logs/SyncMyFonts");
        let helper = render_unix_startup_sync_helper(&agent_path, &log_dir);

        assert!(helper.contains("lan-sync-all"));
        assert!(helper.contains("signin-sync.log"));
        assert!(!helper.contains("SYNCMYFONTS_LAN_KEY"));
        assert!(!helper.contains("--lan-key"));
    }

    #[test]
    fn windows_app_shortcut_runs_expected_command_without_keys() {
        let agent_path = PathBuf::from(r"C:\Users\example\SyncMyFonts\syncmyfonts-agent.exe");
        let shortcut =
            render_windows_app_shortcut(&agent_path, &["lan-sync-all", "--dry-run"], true);

        assert!(shortcut.contains(
            r#""C:\Users\example\SyncMyFonts\syncmyfonts-agent.exe" "lan-sync-all" "--dry-run""#
        ));
        assert!(shortcut.contains("pause"));
        assert!(!shortcut.contains("SYNCMYFONTS_LAN_KEY"));
        assert!(!shortcut.contains("--lan-key"));
    }

    #[test]
    fn doctor_reports_missing_saved_peers_on_first_run() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }

        let report = doctor().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert!(!report.ok);
        assert!(
            report
                .checks
                .iter()
                .any(|check| check.name == "saved-peers" && !check.ok)
        );
        assert!(report.next_step.contains("Pair or save a LAN peer"));
    }

    #[test]
    fn doctor_includes_lan_sharing_guidance() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }

        let report = doctor().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        let guidance = report
            .checks
            .iter()
            .find(|check| check.name == "lan-sharing-guidance")
            .expect("doctor should include LAN sharing guidance");
        assert!(guidance.ok);
        assert!(guidance.message.contains("No port forwarding is needed"));
    }

    #[test]
    fn validation_report_bundles_clean_machine_evidence_without_secrets() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }
        add_lan_peer(
            "Shop PC".to_string(),
            "http://127.0.0.1:7370".to_string(),
            Some("super-secret-lan-key".to_string()),
        )
        .unwrap();
        record_action(
            "Pair Peer",
            "success",
            0,
            "Paired with code 12345678 and token smf-secret-token",
        )
        .unwrap();

        let report = validation_report().unwrap();
        let json = serde_json::to_string(&report).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert_eq!(report.platform, platform_name());
        assert_eq!(report.diagnostics.saved_peer_count, 1);
        assert!(
            report
                .readiness
                .checks
                .iter()
                .any(|check| check.name == "saved-peers")
        );
        assert!(
            report
                .manual_validation_steps
                .iter()
                .any(|step| step.contains("Repeat the flow in the other direction"))
        );
        assert!(
            report
                .pass_criteria
                .iter()
                .any(|criterion| criterion.contains("No port forwarding"))
        );
        assert!(!json.contains("super-secret-lan-key"));
        assert!(!json.contains("12345678"));
        assert!(!json.contains("smf-secret-token"));
    }

    #[test]
    fn gui_network_errors_include_manual_peer_fallback() {
        let result = gui_error(anyhow!("simulated network failure"));

        assert!(result.next_step.contains("paste the sharing computer"));
        assert!(result.next_step.contains("manually"));
    }

    #[test]
    fn xml_escape_escapes_plist_sensitive_characters() {
        assert_eq!(
            xml_escape("/Users/A&B/Sync<My>\"Fonts\"'"),
            "/Users/A&amp;B/Sync&lt;My&gt;&quot;Fonts&quot;&apos;"
        );
    }

    #[test]
    fn install_font_rejects_hash_mismatch_before_write() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let error = install_font(
            "Mismatch.ttf",
            "0000000000000000000000000000000000000000000000000000000000000000",
            b"actual font bytes",
        )
        .unwrap_err();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert!(error.to_string().contains("hash-mismatch"));
        assert!(!user_dir.exists());
    }

    #[test]
    fn install_font_rejects_unsupported_format_before_write() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let bytes = b"web font bytes";
        let sha256 = hex::encode(Sha256::digest(bytes));
        let error = install_font("WebFont.woff2", &sha256, bytes).unwrap_err();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert!(error.to_string().contains("unsupported-format"));
        assert!(!user_dir.exists());
    }

    #[test]
    fn install_font_suffixes_same_name_with_different_bytes() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let first_bytes = b"first font bytes";
        let second_bytes = b"second font bytes";
        let first_sha = hex::encode(Sha256::digest(first_bytes));
        let second_sha = hex::encode(Sha256::digest(second_bytes));
        let first = install_font("Duplicate.ttf", &first_sha, first_bytes).unwrap();
        let second = install_font("Duplicate.ttf", &second_sha, second_bytes).unwrap();
        let expected_install_dir = managed_font_dir().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert_eq!(first, expected_install_dir.join("Duplicate.ttf"));
        assert_ne!(first, second);
        assert!(
            second
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.contains(&second_sha[..8]))
        );
        assert_eq!(fs::read(first).unwrap(), first_bytes);
        assert_eq!(fs::read(second).unwrap(), second_bytes);
    }

    #[test]
    fn diagnostics_peer_redaction_reports_presence_not_secret() {
        let peer = LanPeerConfig {
            name: "Workshop".to_string(),
            url: "http://192.168.1.50:7370".to_string(),
            lan_key: Some("super-secret".to_string()),
        };

        let redacted = redacted_peer_config(&peer);
        let json = serde_json::to_string(&redacted).unwrap();

        assert!(json.contains("\"has_lan_key\":true"));
        assert!(!json.contains("super-secret"));
    }

    #[test]
    fn action_history_persists_recent_result_for_diagnostics() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let config_dir = root.join("config");
        let log_dir = root.join("logs");
        let font_dir = root.join("fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_LOG_DIR", &log_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &font_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        add_lan_peer(
            "Workshop".to_string(),
            "http://127.0.0.1:7370".to_string(),
            Some("super-secret-lan-key".to_string()),
        )
        .unwrap();
        record_action(
            "Test Sync",
            "success",
            2,
            "Installed 1 font.\nPairing code 12345678",
        )
        .unwrap();
        let report = diagnostics().unwrap();
        let report_json = serde_json::to_string(&report).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        let last_action = report.last_action.unwrap();
        assert_eq!(last_action.action, "Test Sync");
        assert_eq!(last_action.status, "success");
        assert_eq!(last_action.warning_count, 2);
        assert!(report.history_path.ends_with("action-history.json"));
        assert!(
            report
                .support_report_text
                .contains("Last action: Test Sync")
        );
        assert!(
            report
                .support_report_text
                .contains("Last action warnings: 2")
        );
        assert!(!report_json.contains("12345678"));
        assert!(!report.support_report_text.contains("12345678"));
        assert!(!report_json.contains("super-secret-lan-key"));
        assert!(!report.support_report_text.contains("super-secret-lan-key"));
    }

    #[test]
    fn forget_lan_peer_removes_saved_peer_by_name() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        add_lan_peer(
            "Workshop".to_string(),
            "http://127.0.0.1:7370".to_string(),
            Some("key".to_string()),
        )
        .unwrap();
        let result = forget_lan_peer("Workshop").unwrap();
        let config = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert!(result.removed);
        assert_eq!(result.saved_peer_count, 0);
        assert!(config.peers.is_empty());
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

    #[test]
    fn verify_managed_fonts_reports_ok_missing_and_modified_files() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        let fonts_dir = config_dir.join("fonts");
        fs::create_dir_all(&fonts_dir).unwrap();
        let ok_path = fonts_dir.join("ok.ttf");
        let ok_bytes = b"stable managed font";
        let ok_sha = hex::encode(Sha256::digest(ok_bytes));
        fs::write(&ok_path, ok_bytes).unwrap();
        let modified_path = fonts_dir.join("modified.ttf");
        let original_bytes = b"original managed font";
        let modified_sha = hex::encode(Sha256::digest(original_bytes));
        fs::write(&modified_path, b"changed managed font").unwrap();
        let missing_path = fonts_dir.join("missing.ttf");
        let manifest = ManagedManifest {
            schema: 1,
            installed: vec![
                ManagedFontRecord {
                    sha256: ok_sha,
                    file_name: "Ok.ttf".to_string(),
                    path: ok_path,
                    source: "lan:http://127.0.0.1:7370".to_string(),
                    installed_at: Utc::now().to_rfc3339(),
                    size_bytes: ok_bytes.len() as u64,
                },
                ManagedFontRecord {
                    sha256: modified_sha,
                    file_name: "Modified.ttf".to_string(),
                    path: modified_path,
                    source: "lan:http://127.0.0.1:7370".to_string(),
                    installed_at: Utc::now().to_rfc3339(),
                    size_bytes: original_bytes.len() as u64,
                },
                ManagedFontRecord {
                    sha256: "00112233445566778899aabbccddeeff0123456789abcdef0123456789abcdef"
                        .to_string(),
                    file_name: "Missing.ttf".to_string(),
                    path: missing_path,
                    source: "lan:http://127.0.0.1:7370".to_string(),
                    installed_at: Utc::now().to_rfc3339(),
                    size_bytes: 10,
                },
            ],
        };
        save_managed_manifest(&manifest).unwrap();

        let report = verify_managed_fonts().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(report.total, 3);
        assert_eq!(report.ok, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.modified.len(), 1);
        assert!(report.unreadable.is_empty());
        assert_eq!(report.missing[0].file_name, "Missing.ttf");
        assert_eq!(report.modified[0].file_name, "Modified.ttf");
    }
}
