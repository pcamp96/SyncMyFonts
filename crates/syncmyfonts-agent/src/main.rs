use std::{
    collections::{BTreeMap, HashSet},
    fs,
    io::Write,
    net::{SocketAddr, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{self, Child, Command},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

const LAN_DISCOVERY_REQUEST: &[u8] = b"SYNCMYFONTS_DISCOVER_V1";
const LAN_DISCOVERY_TIMEOUT: Duration = Duration::from_millis(1400);
const PAIRING_CODE_TTL: Duration = Duration::from_secs(10 * 60);
const LAN_KEY_SECRET_SERVICE: &str = "com.syncmyfonts.lan-peer";
const VALIDATION_FONT_URL: &str =
    "https://raw.githubusercontent.com/google/fonts/main/ofl/basic/Basic-Regular.ttf";
const VALIDATION_FONT_FILE_NAME: &str = "SyncMyFontsValidation-Basic-Regular.ttf";

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
use clap::{Parser, error::ErrorKind};
use reqwest::{
    Url,
    blocking::{Client, multipart},
};
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

#[derive(clap::Subcommand)]
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
    /// Print or save a clean-machine validation evidence bundle.
    ValidationReport {
        #[arg(long)]
        write: bool,
    },
    /// Verify SyncMyFonts-managed installed font files still match the manifest.
    VerifyManaged,
    /// Re-run platform registration for intact SyncMyFonts-managed fonts.
    RepairManaged,
    /// Install a known OFL test font into this user's normal font folder.
    InstallValidationFont {
        #[arg(long, default_value = VALIDATION_FONT_URL)]
        url: String,
    },
    /// Install a per-user sign-in helper that syncs saved LAN peers.
    InstallStartupSync,
    /// Remove the per-user sign-in helper that syncs saved LAN peers.
    UninstallStartupSync,
    /// Install per-user app shortcuts for common SyncMyFonts actions.
    InstallAppShortcuts,
    /// Run the native desktop GUI.
    Gui,
    /// Initialize native GUI state without opening a window.
    GuiSelfTest,
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

#[derive(Debug, Serialize)]
struct CliErrorReport {
    ok: bool,
    command: String,
    message: String,
    causes: Vec<String>,
    next_step: String,
}

fn main() {
    match Cli::try_parse() {
        Ok(cli) => {
            let command_name = cli.command.name().to_string();
            if let Err(error) = run_cli(cli) {
                let report = cli_error_report(&command_name, &error);
                if let Err(print_error) = print_json_to_stderr(&report) {
                    eprintln!("SyncMyFonts failed: {error}");
                    eprintln!("Could not print JSON error report: {print_error}");
                }
                process::exit(1);
            }
        }
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            let _ = error.print();
            process::exit(0);
        }
        Err(error) => {
            let report = cli_parse_error_report(&error);
            if let Err(print_error) = print_json_to_stderr(&report) {
                eprintln!("{error}");
                eprintln!("Could not print JSON error report: {print_error}");
            }
            process::exit(2);
        }
    }
}

fn run_cli(cli: Cli) -> Result<()> {
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
            print_json(&redacted_peer_config(&peer))?;
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
            print_json(&redacted_lan_peers()?)?;
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
        Commands::ValidationReport { write } => {
            if write {
                print_json(&write_validation_report()?)?;
            } else {
                print_json(&validation_report()?)?;
            }
        }
        Commands::VerifyManaged => {
            print_json(&verify_managed_fonts()?)?;
        }
        Commands::RepairManaged => {
            print_json(&repair_managed_fonts()?)?;
        }
        Commands::InstallValidationFont { url } => {
            print_json(&install_validation_font(&url)?)?;
        }
        Commands::InstallStartupSync => {
            print_json(&install_startup_sync()?)?;
        }
        Commands::UninstallStartupSync => {
            print_json(&uninstall_startup_sync()?)?;
        }
        Commands::InstallAppShortcuts => {
            print_json(&install_app_shortcuts()?)?;
        }
        Commands::Gui => {
            run_gui()?;
        }
        Commands::GuiSelfTest => {
            print_json(&gui_self_test()?)?;
        }
        Commands::App { listen, no_open } => {
            let runtime = tokio::runtime::Runtime::new().context("starting app runtime")?;
            runtime.block_on(app_serve(listen, !no_open))?;
        }
    }
    Ok(())
}

impl Commands {
    fn name(&self) -> &'static str {
        match self {
            Commands::Scan { .. } => "scan",
            Commands::Push { .. } => "push",
            Commands::Sync { .. } => "sync",
            Commands::LanServe { .. } => "lan-serve",
            Commands::LanSync { .. } => "lan-sync",
            Commands::LanAddPeer { .. } => "lan-add-peer",
            Commands::LanPair { .. } => "lan-pair",
            Commands::LanPeers => "lan-peers",
            Commands::LanDiscover { .. } => "lan-discover",
            Commands::LanSyncAll { .. } => "lan-sync-all",
            Commands::Diagnostics => "diagnostics",
            Commands::Doctor => "doctor",
            Commands::ValidationReport { .. } => "validation-report",
            Commands::VerifyManaged => "verify-managed",
            Commands::RepairManaged => "repair-managed",
            Commands::InstallValidationFont { .. } => "install-validation-font",
            Commands::InstallStartupSync => "install-startup-sync",
            Commands::UninstallStartupSync => "uninstall-startup-sync",
            Commands::InstallAppShortcuts => "install-app-shortcuts",
            Commands::Gui => "gui",
            Commands::GuiSelfTest => "gui-self-test",
            Commands::App { .. } => "app",
        }
    }
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
        let path = match install_font(&font.file_name, &font.sha256, &bytes) {
            Ok(path) => path,
            Err(error) if is_reportable_install_skip(&error) => {
                skipped.push(format!("{} {}", font.file_name, error));
                continue;
            }
            Err(error) => return Err(error),
        };
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
    #[serde(default)]
    preferences: AppPreferences,
    peers: Vec<LanPeerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AppPreferences {
    #[serde(default)]
    auto_sync_saved_peers: bool,
    #[serde(default = "default_auto_sync_interval_minutes")]
    auto_sync_interval_minutes: u64,
    #[serde(default = "default_lan_listen_address")]
    lan_listen_address: String,
}

impl Default for AppPreferences {
    fn default() -> Self {
        Self {
            auto_sync_saved_peers: false,
            auto_sync_interval_minutes: default_auto_sync_interval_minutes(),
            lan_listen_address: default_lan_listen_address(),
        }
    }
}

fn default_auto_sync_interval_minutes() -> u64 {
    15
}

fn default_lan_listen_address() -> String {
    "0.0.0.0:7370".to_string()
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lan_key_secret_id: Option<String>,
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

#[derive(Default)]
struct SkipSummary {
    already_present: usize,
    would_install: usize,
    unsupported: usize,
    system_conflicts: usize,
    other: usize,
}

impl SkipSummary {
    fn from_lines<'a>(lines: impl IntoIterator<Item = &'a String>) -> Self {
        let mut summary = Self::default();
        for line in lines {
            if line.starts_with("would install ") {
                summary.would_install += 1;
            } else if line.contains("already present") {
                summary.already_present += 1;
            } else if line.contains("unsupported format") || line.contains("unsupported-format") {
                summary.unsupported += 1;
            } else if line.contains("system-font-conflict") {
                summary.system_conflicts += 1;
            } else {
                summary.other += 1;
            }
        }
        summary
    }

    fn total(&self) -> usize {
        self.already_present
            + self.would_install
            + self.unsupported
            + self.system_conflicts
            + self.other
    }
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
    helper_removed: bool,
    registration_removed: bool,
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
    preferences: AppPreferences,
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
    sync_validation_matrix: Vec<SyncValidationDirection>,
    pass_criteria: Vec<String>,
}

#[derive(Debug, Serialize)]
struct SyncValidationDirection {
    name: &'static str,
    source_computer: &'static str,
    target_computer: &'static str,
    source_evidence: Vec<&'static str>,
    target_evidence: Vec<&'static str>,
    pass_criteria: Vec<&'static str>,
}

#[derive(Debug, Serialize)]
struct ValidationReportFile {
    path: PathBuf,
    report: ValidationReport,
    message: String,
}

#[derive(Debug, Serialize)]
struct GuiSelfTestReport {
    ok: bool,
    platform: &'static str,
    version: &'static str,
    status: String,
    setup_phase: String,
    role_card_text: String,
    next_step: String,
    first_run_steps: Vec<String>,
    lan_readiness: Vec<String>,
    lan_sharing_guidance: &'static str,
    pre_share_guidance: &'static str,
    manual_peer_fallback_guidance: &'static str,
    sync_validation_matrix: Vec<SyncValidationDirection>,
    validation_checklist_text: String,
    setup_packet_text: String,
    saved_peer_count: usize,
    saved_peer_summary: String,
    saved_peer_sync_ready: bool,
    saved_peer_sync_hint: Option<String>,
    sign_in_sync_installed: bool,
    selected_peer_name: String,
    listen: String,
    auto_sync_enabled: bool,
    auto_sync_interval_minutes: u64,
    listen_address_ready: bool,
    listen_address_detail: String,
    peer_url_ready: bool,
    peer_pairing_ready: bool,
    peer_sync_ready: bool,
    peer_install_ready: bool,
    can_find_lan_peers: bool,
    can_pair_peer: bool,
    can_test_peer: bool,
    can_preview_peer: bool,
    can_get_missing_fonts_from_peer: bool,
    can_save_peer: bool,
    can_load_saved_peer: bool,
    can_enable_saved_peer_automation: bool,
    can_change_auto_sync_preference: bool,
    can_start_sharing: bool,
    can_stop_sharing: bool,
    can_forget_peer: bool,
    peer_action_hint: &'static str,
    peer_pairing_detail: String,
    peer_key_label: &'static str,
    share_key_label: &'static str,
    pairing_instructions_next_step: &'static str,
    config_path: PathBuf,
    log_dir: PathBuf,
    user_font_dir: PathBuf,
    managed_font_dir: PathBuf,
    message: String,
}

#[derive(Debug, Serialize)]
struct RedactedPeer {
    name: String,
    url: String,
    has_lan_key: bool,
    key_storage: &'static str,
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
    registration_issues: Vec<ManagedVerifyIssue>,
}

#[derive(Debug, Serialize)]
struct ManagedVerifyIssue {
    sha256: String,
    file_name: String,
    path: PathBuf,
    message: String,
}

#[derive(Debug, Serialize)]
struct ManagedRepairReport {
    manifest_path: PathBuf,
    total: usize,
    repaired: Vec<ManagedRepairEntry>,
    skipped: Vec<ManagedVerifyIssue>,
    failed: Vec<ManagedVerifyIssue>,
}

#[derive(Debug, Serialize)]
struct ManagedRepairEntry {
    sha256: String,
    file_name: String,
    path: PathBuf,
    message: String,
}

#[derive(Debug, Serialize)]
struct ValidationFontInstallReport {
    source_url: String,
    file_name: String,
    path: PathBuf,
    sha256: String,
    size_bytes: u64,
    already_present: bool,
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
        let path = match install_font(&font.file_name, &font.sha256, &bytes) {
            Ok(path) => path,
            Err(error) if is_reportable_install_skip(&error) => {
                skipped.push(format!("{} {}", font.file_name, error));
                continue;
            }
            Err(error) => return Err(error),
        };
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
    let url = normalize_peer_url(&url);
    let existing_peer = config
        .peers
        .iter()
        .find(|existing| {
            existing.name == normalized_peer_name(&name, &url)
                || normalize_peer_url(&existing.url) == url
        })
        .cloned();
    let mut peer = LanPeerConfig {
        name: normalized_peer_name(&name, &url),
        url,
        lan_key_secret_id: existing_peer
            .as_ref()
            .and_then(|existing| existing.lan_key_secret_id.clone()),
        lan_key: existing_peer.as_ref().and_then(resolve_lan_peer_key),
    };
    let provided_lan_key = lan_key.filter(|key| !key.trim().is_empty());
    if let Some(lan_key) = provided_lan_key {
        save_lan_peer_key(&mut peer, lan_key);
    }
    let stored_peer = peer_for_config(&peer);
    let return_peer = peer.clone();
    if let Some(existing) = config.peers.iter_mut().find(|existing| {
        existing.name == stored_peer.name || normalize_peer_url(&existing.url) == stored_peer.url
    }) {
        *existing = stored_peer;
    } else {
        config.peers.push(stored_peer);
    }
    save_app_config(&config)?;
    Ok(return_peer)
}

fn peer_for_config(peer: &LanPeerConfig) -> LanPeerConfig {
    LanPeerConfig {
        name: peer.name.clone(),
        url: peer.url.clone(),
        lan_key_secret_id: peer.lan_key_secret_id.clone(),
        lan_key: if peer.lan_key_secret_id.is_some() {
            None
        } else {
            peer.lan_key.clone()
        },
    }
}

fn save_lan_peer_key(peer: &mut LanPeerConfig, lan_key: String) {
    let secret_id = peer
        .lan_key_secret_id
        .clone()
        .unwrap_or_else(|| format!("lan-peer-{}", Uuid::new_v4()));
    if store_lan_key_secret(&secret_id, &lan_key).is_ok() {
        peer.lan_key_secret_id = Some(secret_id);
    } else {
        peer.lan_key_secret_id = None;
    }
    peer.lan_key = Some(lan_key);
}

fn resolve_lan_peer_key(peer: &LanPeerConfig) -> Option<String> {
    peer.lan_key.clone().or_else(|| {
        peer.lan_key_secret_id
            .as_deref()
            .and_then(load_lan_key_secret)
    })
}

fn lan_peer_has_key(peer: &LanPeerConfig) -> bool {
    resolve_lan_peer_key(peer)
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
}

fn store_lan_key_secret(secret_id: &str, lan_key: &str) -> Result<()> {
    if !native_secret_store_enabled() {
        bail!("native secret store is disabled");
    }
    native_lan_key_entry(secret_id)?
        .set_password(lan_key)
        .context("storing LAN token in native credential store")
}

fn load_lan_key_secret(secret_id: &str) -> Option<String> {
    if !native_secret_store_enabled() {
        return None;
    }
    native_lan_key_entry(secret_id)
        .ok()
        .and_then(|entry| entry.get_password().ok())
        .filter(|key| !key.trim().is_empty())
}

fn delete_lan_key_secret(secret_id: &str) {
    if !native_secret_store_enabled() {
        return;
    }
    if let Ok(entry) = native_lan_key_entry(secret_id) {
        let _ = entry.delete_credential();
    }
}

fn native_secret_store_enabled() -> bool {
    if std::env::var_os("SYNCMYFONTS_DISABLE_SECRET_STORE").is_some() {
        return false;
    }
    #[cfg(any(test, not(any(target_os = "macos", target_os = "windows"))))]
    {
        false
    }
    #[cfg(all(not(test), any(target_os = "macos", target_os = "windows")))]
    {
        true
    }
}

#[cfg(target_os = "macos")]
fn native_lan_key_entry(secret_id: &str) -> Result<keyring_core::Entry> {
    apple_native_keyring_store::keychain::Cred::build(
        apple_native_keyring_store::keychain::MacKeychainDomain::User,
        LAN_KEY_SECRET_SERVICE,
        secret_id,
    )
    .map_err(anyhow::Error::from)
}

#[cfg(target_os = "windows")]
fn native_lan_key_entry(secret_id: &str) -> Result<keyring_core::Entry> {
    use keyring_core::api::CredentialStoreApi;

    let store = windows_native_keyring_store::Store::new()?;
    let modifiers = std::collections::HashMap::from([("persistence", "Local")]);
    store
        .build(LAN_KEY_SECRET_SERVICE, secret_id, Some(&modifiers))
        .map_err(anyhow::Error::from)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn native_lan_key_entry(_secret_id: &str) -> Result<keyring_core::Entry> {
    bail!("native credential store is not supported on this platform")
}

fn normalized_peer_name(name: &str, normalized_url: &str) -> String {
    let trimmed = name.trim();
    if !trimmed.is_empty() {
        return trimmed.to_string();
    }

    let host = normalized_url
        .strip_prefix("http://")
        .or_else(|| normalized_url.strip_prefix("https://"))
        .unwrap_or(normalized_url)
        .split('/')
        .next()
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .unwrap_or("LAN Peer");
    format!("Peer {host}")
}

fn forget_lan_peer(name: &str) -> Result<ForgetPeerResponse> {
    let mut config = load_app_config()?;
    let before = config.peers.len();
    let forgotten_secret_ids = config
        .peers
        .iter()
        .filter(|peer| peer.name == name)
        .filter_map(|peer| peer.lan_key_secret_id.clone())
        .collect::<Vec<_>>();
    config.peers.retain(|peer| peer.name != name);
    let removed = config.peers.len() != before;
    if removed {
        save_app_config(&config)?;
        for secret_id in forgotten_secret_ids {
            delete_lan_key_secret(&secret_id);
        }
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
        let lan_key = resolve_lan_peer_key(&peer);
        match lan_sync(&peer.url, lan_key.as_deref(), dry_run) {
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
                error: Some(format_error_chain(&error)),
            }),
        }
    }
    Ok(LanSyncAllReport { peers, dry_run })
}

fn format_error_chain(error: &anyhow::Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(": ")
}

fn cli_error_report(command: &str, error: &anyhow::Error) -> CliErrorReport {
    let causes = error
        .chain()
        .map(|cause| redact_secret_text(&cause.to_string()))
        .collect::<Vec<_>>();
    let message = causes
        .first()
        .cloned()
        .unwrap_or_else(|| "SyncMyFonts command failed.".to_string());
    let error_chain = causes.join(": ");
    CliErrorReport {
        ok: false,
        command: command.to_string(),
        message,
        causes,
        next_step: gui_error_next_step(&error_chain),
    }
}

fn cli_parse_error_report(error: &clap::Error) -> CliErrorReport {
    CliErrorReport {
        ok: false,
        command: "parse".to_string(),
        message: redact_secret_text(error.to_string().trim()),
        causes: Vec::new(),
        next_step: "Run syncmyfonts --help or the command-specific help to see required options."
            .to_string(),
    }
}

fn redact_secret_text(value: &str) -> String {
    let mut redacted = Vec::new();
    for token in value.split_whitespace() {
        let lower = token.to_ascii_lowercase();
        if lower.contains("key=")
            || lower.contains("api_key")
            || lower.contains("api-key")
            || lower.contains("lan_key")
            || lower.contains("lan-key")
            || token.starts_with("smf-")
        {
            redacted.push("[redacted]");
        } else {
            redacted.push(token);
        }
    }
    redacted.join(" ")
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
    let warnings = diagnostics_warnings(&config, scan.warnings, manifest_result);
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
        preferences: config.preferences,
        last_action: history.last_action,
        recent_actions: history.recent,
        user_font_count: scan.fonts.len(),
        managed_manifest_count,
        warnings,
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
    checks.push(font_sync_scope_check());
    checks.push(windows_network_profile_check());
    let saved_key_count = saved_lan_key_count(&config);
    checks.push(if config.peers.is_empty() {
        doctor_check(
            "saved-peers",
            false,
            "No saved peers yet. Pair another computer before relying on repeat sync.",
        )
    } else if saved_key_count < config.peers.len() {
        let missing = config.peers.len() - saved_key_count;
        doctor_check(
            "saved-peers",
            false,
            format!(
                "{missing} saved peer(s) still need pairing before repeat sync or automation can run."
            ),
        )
    } else {
        doctor_check(
            "saved-peers",
            true,
            format!(
                "{} saved peer(s) are paired for repeat sync.",
                config.peers.len()
            ),
        )
    });
    checks.push(secret_storage_check(&config));

    checks.push(sign_in_sync_readiness_check());

    let failed = checks.iter().filter(|check| !check.ok).count();
    let next_step = if failed == 0 {
        "This computer is ready for saved-peer LAN sync.".to_string()
    } else if config.peers.is_empty() || saved_key_count < config.peers.len() {
        "Pair saved LAN peers, then run Readiness Check again.".to_string()
    } else {
        "Review failed checks, then run Readiness Check again.".to_string()
    };

    Ok(DoctorReport {
        ok: failed == 0,
        checks,
        next_step,
    })
}

fn secret_storage_check(config: &AppConfig) -> DoctorCheck {
    let saved_key_count = saved_lan_key_count(config);
    let portable_key_count = portable_lan_key_count(config);
    let native_key_count = native_lan_key_ref_count(config);
    if saved_key_count == 0 {
        doctor_check("secret-storage", true, "No LAN tokens are saved yet.")
    } else if portable_key_count == 0 {
        doctor_check(
            "secret-storage",
            true,
            format!(
                "{native_key_count} saved LAN token(s) are referenced from the native credential store and redacted from diagnostics."
            ),
        )
    } else {
        doctor_check(
            "secret-storage",
            true,
            format!(
                "{portable_key_count} saved LAN token(s) are still stored in the per-user SyncMyFonts config fallback and redacted from diagnostics. {native_key_count} token(s) use native credential-store references."
            ),
        )
    }
}

fn font_sync_scope_check() -> DoctorCheck {
    doctor_check(
        "font-sync-scope",
        true,
        "SyncMyFonts scans and installs current-user fonts only; system font directories are excluded from LAN sync.",
    )
}

fn sign_in_sync_readiness_check() -> DoctorCheck {
    match startup_sync_helper_path() {
        Ok(helper_path) => {
            let registration_path =
                startup_sync_registration_path().unwrap_or_else(|_| helper_path.clone());
            sign_in_sync_readiness_check_from_paths(&helper_path, &registration_path)
        }
        Err(error) => doctor_check(
            "sign-in-sync-helper",
            false,
            format!("Sign-in sync helper path could not be resolved: {error}"),
        ),
    }
}

fn startup_sync_registration_path() -> Result<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        macos_startup_sync_plist_path()
    }
    #[cfg(target_os = "windows")]
    {
        windows_startup_sync_shortcut_path()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        startup_sync_helper_path()
    }
}

fn sign_in_sync_readiness_check_from_paths(
    helper_path: &Path,
    registration_path: &Path,
) -> DoctorCheck {
    let helper_exists = helper_path.exists();
    let registration_exists = registration_path.exists();

    match (helper_exists, registration_exists) {
        (true, true) => doctor_check(
            "sign-in-sync-helper",
            true,
            format!(
                "Sign-in sync is installed. Helper: {}; registration: {}.",
                helper_path.display(),
                registration_path.display()
            ),
        ),
        (false, false) => doctor_check(
            "sign-in-sync-helper",
            true,
            format!(
                "Optional sign-in sync helper is not installed. Use Enable Sign-In Sync if you want saved peers to sync when you sign in. Expected helper: {}; expected registration: {}.",
                helper_path.display(),
                registration_path.display()
            ),
        ),
        (true, false) => doctor_check(
            "sign-in-sync-helper",
            false,
            format!(
                "Sign-in sync helper exists at {}, but registration is missing at {}. Use Disable Sign-In Sync, then Enable Sign-In Sync to repair it.",
                helper_path.display(),
                registration_path.display()
            ),
        ),
        (false, true) => doctor_check(
            "sign-in-sync-helper",
            false,
            format!(
                "Sign-in sync registration exists at {}, but helper is missing at {}. Use Disable Sign-In Sync, then Enable Sign-In Sync to repair it.",
                registration_path.display(),
                helper_path.display()
            ),
        ),
    }
}

fn saved_lan_key_count(config: &AppConfig) -> usize {
    config
        .peers
        .iter()
        .filter(|peer| lan_peer_has_key(peer))
        .count()
}

fn portable_lan_key_count(config: &AppConfig) -> usize {
    config
        .peers
        .iter()
        .filter(|peer| {
            peer.lan_key
                .as_deref()
                .is_some_and(|key| !key.trim().is_empty())
        })
        .count()
}

fn native_lan_key_ref_count(config: &AppConfig) -> usize {
    config
        .peers
        .iter()
        .filter(|peer| peer.lan_key_secret_id.is_some())
        .count()
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
        sync_validation_matrix: sync_validation_matrix(),
        pass_criteria: manual_validation_pass_criteria(),
    })
}

fn write_validation_report() -> Result<ValidationReportFile> {
    let report = validation_report()?;
    let path = validation_report_path(&report.generated_at)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&report).context("serializing validation report")?;
    fs::write(&path, bytes).with_context(|| format!("writing {}", path.display()))?;
    Ok(ValidationReportFile {
        path,
        report,
        message:
            "Validation report saved. Keep this JSON with before/after clean-machine test evidence."
                .to_string(),
    })
}

fn validation_report_path(generated_at: &str) -> Result<PathBuf> {
    let safe_stamp = generated_at
        .chars()
        .map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' })
        .collect::<String>()
        .trim_matches('-')
        .to_string();
    Ok(app_log_dir()?.join(format!("validation-report-{safe_stamp}.json")))
}

fn validation_evidence_summary(
    diagnostics: &DiagnosticsReport,
    readiness: &DoctorReport,
    managed_fonts: &ManagedVerifyReport,
) -> Vec<String> {
    let failed_readiness = readiness.checks.iter().filter(|check| !check.ok).count();
    let managed_issues = managed_verify_issue_count(managed_fonts);
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

fn managed_verify_issue_count(report: &ManagedVerifyReport) -> usize {
    report.missing.len()
        + report.modified.len()
        + report.unreadable.len()
        + report.registration_issues.len()
}

fn manual_validation_steps() -> Vec<String> {
    vec![
        "Launch the native app on both macOS and Windows.".to_string(),
        "Run Validation Report on both computers before syncing.".to_string(),
        "On the computer that has a non-system test font, click Share Fonts On This Network with Shared Key blank.".to_string(),
        "On the other computer, find or enter the peer URL, enter the pairing code within its shown validity window, and click Pair Peer.".to_string(),
        "Run Preview From Peer and confirm the test font is missing while system fonts are not offered.".to_string(),
        "Run Get Missing Fonts From Peer and confirm the font installs into the current-user or SyncMyFonts-managed folder.".to_string(),
        "Run the same sync again and confirm the already installed font is skipped.".to_string(),
        "Repeat the flow in the other direction with a different non-system test font.".to_string(),
        "Run Validation Report again on both computers and keep the before/after JSON as evidence.".to_string(),
    ]
}

fn sync_validation_matrix() -> Vec<SyncValidationDirection> {
    vec![
        SyncValidationDirection {
            name: "macOS to Windows",
            source_computer: "macOS computer that already has a licensed non-system test font",
            target_computer: "Windows computer missing that test font",
            source_evidence: vec![
                "Before-sync Validation Report from macOS",
                "Screenshot or copied result after Share Fonts On This Network shows a LAN URL and pairing code",
                "After-sync Validation Report from macOS",
            ],
            target_evidence: vec![
                "Before-sync Validation Report from Windows",
                "Copied Preview From Peer result showing the test font as missing/installable",
                "Copied Get Missing Fonts From Peer result showing the test font installed",
                "After-sync Validation Report from Windows with clean managed-font verification",
            ],
            pass_criteria: vec![
                "Windows installs the font for the current user without an administrator prompt",
                "Windows scan sees the synced font after install",
                "Running the same sync again skips the already installed font",
            ],
        },
        SyncValidationDirection {
            name: "Windows to macOS",
            source_computer: "Windows computer that already has a different licensed non-system test font",
            target_computer: "macOS computer missing that test font",
            source_evidence: vec![
                "Before-sync Validation Report from Windows",
                "Screenshot or copied result after Share Fonts On This Network shows a LAN URL and pairing code",
                "After-sync Validation Report from Windows",
            ],
            target_evidence: vec![
                "Before-sync Validation Report from macOS",
                "Copied Preview From Peer result showing the test font as missing/installable",
                "Copied Get Missing Fonts From Peer result showing the test font installed",
                "After-sync Validation Report from macOS with clean managed-font verification",
            ],
            pass_criteria: vec![
                "macOS installs the font into the SyncMyFonts managed user font folder",
                "macOS scan sees the synced font after install",
                "Running the same sync again skips the already installed font",
            ],
        },
    ]
}

fn validation_checklist_text() -> String {
    let mut lines = vec![
        "SyncMyFonts LAN MVP validation".to_string(),
        "".to_string(),
        "Before syncing on both computers:".to_string(),
    ];
    lines.extend(
        manual_validation_steps()
            .into_iter()
            .take(2)
            .map(|step| format!("- {step}")),
    );
    lines.push(
        "- Confirm the managed font folder is a per-user path and no administrator prompt appears."
            .to_string(),
    );
    lines.push("".to_string());
    lines.push("Required sync directions:".to_string());
    for direction in sync_validation_matrix() {
        lines.push(format!("- {}", direction.name));
        lines.push(format!("  Source: {}", direction.source_computer));
        lines.push(format!("  Target: {}", direction.target_computer));
        lines.push("  Prove:".to_string());
        for criterion in direction.pass_criteria {
            lines.push(format!("  - {criterion}"));
        }
    }
    lines.push("".to_string());
    lines.push("After syncing on both computers:".to_string());
    lines.push("- Run Validation Report and Verify Managed Fonts.".to_string());
    lines.push("- Confirm system fonts were not offered for sync.".to_string());
    lines.push("- Reopen the design app and confirm the synced font appears.".to_string());
    lines.join("\n")
}

fn manual_validation_pass_criteria() -> Vec<String> {
    vec![
        "Native GUI launches on both platforms without administrator privileges.".to_string(),
        "Pairing-code LAN sync works from macOS to Windows and Windows to macOS.".to_string(),
        "Fonts install only into current-user or SyncMyFonts-managed locations.".to_string(),
        "System fonts are not listed as missing sync candidates.".to_string(),
        "Re-running sync skips fonts that are already present.".to_string(),
        "Managed font verification has no missing, modified, unreadable, or registration issue entries after sync."
            .to_string(),
        "Diagnostics and validation reports do not expose LAN keys, pairing codes, or API keys."
            .to_string(),
        "No port forwarding, Docker container, or cloud service is required for the LAN test."
            .to_string(),
    ]
}

fn pairing_code_validity_text(seconds: Option<u64>) -> String {
    let minutes = seconds
        .unwrap_or_else(|| PAIRING_CODE_TTL.as_secs())
        .saturating_add(59)
        / 60;
    let minute_label = if minutes == 1 { "minute" } else { "minutes" };
    format!("valid for about {minutes} {minute_label}")
}

fn pairing_code_remaining_seconds(started_at: Instant, now: Instant) -> Option<u64> {
    PAIRING_CODE_TTL
        .checked_sub(now.saturating_duration_since(started_at))
        .map(|remaining| remaining.as_secs())
        .filter(|remaining| *remaining > 0)
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

fn windows_network_profile_check() -> DoctorCheck {
    #[cfg(target_os = "windows")]
    {
        match windows_network_profile_categories() {
            Ok(categories) => windows_network_profile_check_from_categories(&categories),
            Err(error) => doctor_check(
                "windows-network-profile",
                false,
                format!(
                    "Could not inspect the Windows network profile: {error}. If this PC is sharing fonts, make sure the trusted LAN is Private and allow SyncMyFonts only on Private networks."
                ),
            ),
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        doctor_check(
            "windows-network-profile",
            true,
            "Windows network-profile detection is not needed on this platform.",
        )
    }
}

#[cfg(target_os = "windows")]
fn windows_network_profile_categories() -> Result<Vec<String>> {
    let output = Command::new("powershell")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            "Get-NetConnectionProfile | Select-Object -ExpandProperty NetworkCategory",
        ])
        .output()
        .context("running Get-NetConnectionProfile")?;
    if !output.status.success() {
        bail!(
            "Get-NetConnectionProfile exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(parse_windows_network_profile_categories(
        &String::from_utf8_lossy(&output.stdout),
    ))
}

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn parse_windows_network_profile_categories(output: &str) -> Vec<String> {
    let mut categories = Vec::new();
    for line in output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
    {
        if !categories
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(line))
        {
            categories.push(line.to_string());
        }
    }
    categories
}

#[cfg_attr(not(any(target_os = "windows", test)), allow(dead_code))]
fn windows_network_profile_check_from_categories(categories: &[String]) -> DoctorCheck {
    if categories.is_empty() {
        return doctor_check(
            "windows-network-profile",
            true,
            "No active Windows network profile was reported. If this PC is sharing fonts, use a trusted Private network.",
        );
    }

    let category_list = categories.join(", ");
    if categories
        .iter()
        .any(|category| category.eq_ignore_ascii_case("Public"))
    {
        return doctor_check(
            "windows-network-profile",
            false,
            format!(
                "Active Windows network profile(s): {category_list}. If this PC is sharing fonts, switch the trusted LAN to Private or allow SyncMyFonts only on Private networks."
            ),
        );
    }

    doctor_check(
        "windows-network-profile",
        true,
        format!(
            "Active Windows network profile(s): {category_list}. Hosted LAN sharing is intended for trusted Private or domain networks."
        ),
    )
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
        .route("/api/support/open", post(app_open_app_support_folder))
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

async fn app_open_app_support_folder() -> Result<Json<OpenFolderResponse>, LanApiError> {
    let result = tokio::task::spawn_blocking(|| -> Result<OpenFolderResponse> {
        let folder = app_data_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating app support folder {}", path.display()))?;
            Ok(path)
        })?;
        let path = open_path(folder)?;
        Ok(OpenFolderResponse {
            opened: true,
            path,
            message: "Opened the SyncMyFonts app support folder.".to_string(),
        })
    })
    .await
    .map_err(LanApiError::internal)?;
    match result {
        Ok(response) => {
            record_action_best_effort("Browser Open App Support", "success", 0, &response.message);
            Ok(Json(response))
        }
        Err(error) => {
            record_action_best_effort("Browser Open App Support", "failed", 1, &error.to_string());
            Err(LanApiError::internal(error))
        }
    }
}

async fn app_peers() -> Result<Json<Vec<RedactedPeer>>, LanApiError> {
    redacted_lan_peers()
        .map(Json)
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
                "Browser Test Connection",
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
            record_action_best_effort("Browser Test Connection", "failed", 1, &error.to_string());
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
        "Browser Get Missing Fonts From Peer"
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
                "Browser Share Fonts On This Network",
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
                "Browser Share Fonts On This Network",
                "failed",
                1,
                &error.to_string(),
            );
            return Err(LanApiError::internal(error));
        }
    };
    let urls = share_urls(listen);
    *guard = Some(RunningShare { child, listen });
    let _ = set_lan_listen_preference(listen);
    let pairing_expires_seconds = pairing_code.as_ref().map(|_| PAIRING_CODE_TTL.as_secs());
    let response = ShareResponse {
        sharing: true,
        message: format!("Sharing fonts at {}.", urls.join(", ")),
        urls,
        pairing_code,
        pairing_expires_seconds,
    };
    record_action_best_effort(
        "Browser Share Fonts On This Network",
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

fn gui_self_test() -> Result<GuiSelfTestReport> {
    let _env = IsolatedGuiSelfTestEnv::activate();
    let app = SyncMyFontsGui::new();
    Ok(GuiSelfTestReport {
        ok: true,
        platform: platform_name(),
        version: env!("CARGO_PKG_VERSION"),
        status: app.status.clone(),
        setup_phase: app.setup_phase(),
        role_card_text: app.role_card_text(),
        next_step: app.next_step.clone(),
        first_run_steps: app.first_run_steps(),
        lan_readiness: app.lan_readiness_lines(),
        lan_sharing_guidance: platform_lan_sharing_guidance(),
        pre_share_guidance: platform_pre_share_guidance(),
        manual_peer_fallback_guidance: platform_manual_peer_fallback_guidance(),
        sync_validation_matrix: sync_validation_matrix(),
        validation_checklist_text: validation_checklist_text(),
        setup_packet_text: app.setup_packet_text(),
        saved_peer_count: app.saved_peer_names.len(),
        saved_peer_summary: app.saved_peer_summary.clone(),
        saved_peer_sync_ready: app.saved_peer_sync_ready(),
        saved_peer_sync_hint: app.saved_peer_sync_hint(),
        sign_in_sync_installed: sign_in_sync_installed().unwrap_or(false),
        selected_peer_name: app.selected_peer_name.clone(),
        listen: app.listen.clone(),
        auto_sync_enabled: app.auto_sync_enabled,
        auto_sync_interval_minutes: app.auto_sync_interval_minutes,
        listen_address_ready: app.listen_address_ready(),
        listen_address_detail: app.listen_address_detail(),
        peer_url_ready: app.peer_url_ready(),
        peer_pairing_ready: app.peer_pairing_ready(),
        peer_sync_ready: app.peer_sync_ready(),
        peer_install_ready: app.peer_install_ready(),
        can_find_lan_peers: app.can_find_lan_peers(),
        can_pair_peer: app.can_pair_peer(),
        can_test_peer: app.can_test_peer(),
        can_preview_peer: app.can_preview_peer(),
        can_get_missing_fonts_from_peer: app.can_get_missing_fonts_from_peer(),
        can_save_peer: app.can_save_peer(),
        can_load_saved_peer: app.can_load_saved_peer(),
        can_enable_saved_peer_automation: app.can_enable_saved_peer_automation(),
        can_change_auto_sync_preference: app.can_change_auto_sync_preference(),
        can_start_sharing: app.can_start_sharing(),
        can_stop_sharing: app.can_stop_sharing(),
        can_forget_peer: app.can_forget_peer(),
        peer_action_hint: app.peer_action_hint(),
        peer_pairing_detail: app.peer_pairing_detail(),
        peer_key_label: peer_key_label(),
        share_key_label: share_key_label(),
        pairing_instructions_next_step: pairing_instructions_copied_next_step(),
        config_path: app_config_path()?,
        log_dir: app_log_dir()?,
        user_font_dir: user_font_dir()?,
        managed_font_dir: managed_font_dir()?,
        message:
            "Native GUI state initialized without opening a window. Use clean-machine testing to prove interactive launch."
                .to_string(),
    })
}

struct IsolatedGuiSelfTestEnv {
    previous: Vec<(&'static str, Option<std::ffi::OsString>)>,
}

impl IsolatedGuiSelfTestEnv {
    fn activate() -> Self {
        if std::env::var("SYNCMYFONTS_GUI_SELF_TEST_REAL_ENV").as_deref() == Ok("1") {
            return Self {
                previous: Vec::new(),
            };
        }

        let root =
            std::env::temp_dir().join(format!("syncmyfonts-gui-self-test-{}", Uuid::new_v4()));
        let vars = [
            ("SYNCMYFONTS_CONFIG_DIR", root.join("config")),
            ("SYNCMYFONTS_LOG_DIR", root.join("logs")),
            ("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts")),
        ];
        let previous = vars
            .iter()
            .map(|(name, _)| (*name, std::env::var_os(name)))
            .collect();
        for (name, value) in vars {
            unsafe {
                std::env::set_var(name, value);
            }
        }
        Self { previous }
    }
}

impl Drop for IsolatedGuiSelfTestEnv {
    fn drop(&mut self) {
        for (name, value) in &self.previous {
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
    }
}

pub fn run_native_gui_from_args() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--self-test") {
        print_json(&gui_self_test()?)?;
        return Ok(());
    }
    run_gui()
}

pub fn run_native_gui_entrypoint() {
    if let Err(error) = run_native_gui_from_args() {
        let report = cli_error_report("gui", &error);
        if let Err(print_error) = print_json_to_stderr(&report) {
            eprintln!("SyncMyFonts GUI failed: {error}");
            eprintln!("Could not print JSON error report: {print_error}");
        }
        process::exit(1);
    }
}

struct SyncMyFontsGui {
    status: String,
    next_step: String,
    output: String,
    last_support_report: Option<String>,
    last_result_review: Option<String>,
    last_result: String,
    warning_count: usize,
    saved_peer_summary: String,
    saved_peer_names: Vec<String>,
    saved_peer_key_count: usize,
    selected_peer_name: String,
    device_name_input: String,
    current_action: Option<String>,
    task: Option<mpsc::Receiver<GuiTaskResult>>,
    peer_name: String,
    peer_url: String,
    peer_key: String,
    pairing_code: String,
    discovered_peer_requires_lan_key: bool,
    listen: String,
    share_key: String,
    share: Option<RunningShare>,
    share_urls: Vec<String>,
    last_pairing_code: Option<String>,
    last_pairing_expires_seconds: Option<u64>,
    last_pairing_started_at: Option<Instant>,
    auto_sync_enabled: bool,
    auto_sync_interval_minutes: u64,
    last_auto_sync_at: Option<Instant>,
    last_previewed_peer: Option<PreviewedPeer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PreviewedPeer {
    url: String,
    key_fingerprint: Option<String>,
}

struct GuiTaskResult {
    output: String,
    next_step: String,
    result_summary: Option<String>,
    result_review: Option<String>,
    peer: Option<LanPeerConfig>,
    discovered_peer: Option<LanDiscoveredPeer>,
    clear_peer_key: bool,
    clear_peer_form: bool,
    refresh_saved_peers: bool,
    support_report: Option<String>,
    warning_count: usize,
    previewed_peer: Option<PreviewedPeer>,
}

impl SyncMyFontsGui {
    fn new() -> Self {
        let preferences = load_app_config()
            .map(|config| config.preferences)
            .unwrap_or_default();
        let mut app = Self {
            status: "Loading...".to_string(),
            next_step: "Start by sharing fonts on one computer, then pair from the other computer."
                .to_string(),
            output: "Ready.".to_string(),
            last_support_report: None,
            last_result_review: None,
            last_result: "No actions yet.".to_string(),
            warning_count: 0,
            saved_peer_summary: "Saved peers: loading...".to_string(),
            saved_peer_names: Vec::new(),
            saved_peer_key_count: 0,
            selected_peer_name: String::new(),
            device_name_input: device_name(),
            current_action: None,
            task: None,
            peer_name: String::new(),
            peer_url: String::new(),
            peer_key: String::new(),
            pairing_code: String::new(),
            discovered_peer_requires_lan_key: false,
            listen: preferences.lan_listen_address,
            share_key: String::new(),
            share: None,
            share_urls: Vec::new(),
            last_pairing_code: None,
            last_pairing_expires_seconds: None,
            last_pairing_started_at: None,
            auto_sync_enabled: preferences.auto_sync_saved_peers,
            auto_sync_interval_minutes: preferences.auto_sync_interval_minutes,
            last_auto_sync_at: None,
            last_previewed_peer: None,
        };
        app.refresh_status();
        app.load_saved_peers_into_form();
        app.load_last_action_summary();
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
                    self.discovered_peer_requires_lan_key = !self.peer_key.trim().is_empty();
                    self.last_previewed_peer = None;
                }
                if let Some(peer) = result.discovered_peer {
                    self.peer_name = peer.name;
                    self.peer_url = peer.url;
                    self.discovered_peer_requires_lan_key = peer.requires_lan_key;
                    self.last_previewed_peer = None;
                }
                if result.clear_peer_key {
                    self.peer_key.clear();
                }
                if result.clear_peer_form {
                    self.peer_name.clear();
                    self.peer_url.clear();
                    self.peer_key.clear();
                    self.pairing_code.clear();
                    self.selected_peer_name.clear();
                    self.discovered_peer_requires_lan_key = false;
                    self.last_previewed_peer = None;
                }
                if let Some(previewed_peer) = result.previewed_peer {
                    self.last_previewed_peer = Some(previewed_peer);
                }
                self.output = result.output;
                if let Some(support_report) = result.support_report {
                    self.last_support_report = Some(support_report);
                }
                self.last_result_review = result.result_review;
                self.next_step = result.next_step;
                self.warning_count = result.warning_count;
                let completed_at = format!(
                    "{} completed at {}",
                    action,
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.last_result = result
                    .result_summary
                    .map(|summary| format!("{summary}\n{completed_at}"))
                    .unwrap_or(completed_at);
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
                if action == "Auto-syncing saved peers" {
                    self.last_auto_sync_at = Some(Instant::now());
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
        match load_app_config() {
            Ok(config) if config.peers.is_empty() => {
                self.saved_peer_names.clear();
                self.saved_peer_key_count = 0;
                self.selected_peer_name.clear();
                self.saved_peer_summary = "Saved peers: none yet.".to_string();
            }
            Ok(config) => {
                self.saved_peer_names = config.peers.iter().map(|peer| peer.name.clone()).collect();
                self.saved_peer_key_count = saved_lan_key_count(&config);
                if !self.saved_peer_names.contains(&self.selected_peer_name) {
                    self.selected_peer_name =
                        self.saved_peer_names.first().cloned().unwrap_or_default();
                }
                self.saved_peer_summary = saved_peer_summary_text(&config);
            }
            Err(error) => {
                self.saved_peer_names.clear();
                self.saved_peer_key_count = 0;
                self.selected_peer_name.clear();
                self.saved_peer_summary = format!("Saved peers unavailable: {error}");
            }
        }
    }

    fn load_saved_peers_into_form(&mut self) {
        self.load_selected_saved_peer_into_form();
    }

    fn load_last_action_summary(&mut self) {
        match load_app_history() {
            Ok(history) => {
                if let Some(action) = history.last_action {
                    self.last_result = gui_last_action_summary(&action);
                    self.warning_count = action.warning_count;
                    self.output = action.result.clone();
                    self.next_step = if action.status == "success" {
                        self.last_action_success_next_step()
                    } else {
                        "Last action needs attention. Review the result, then run Diagnostics or try again."
                            .to_string()
                    };
                }
            }
            Err(error) => {
                self.last_result = "Last action unavailable.".to_string();
                self.warning_count = 1;
                self.output = format!("Could not load action history: {error}");
                self.next_step =
                    "Action history could not be loaded. Run Diagnostics if this keeps happening."
                        .to_string();
            }
        }
    }

    fn last_action_success_next_step(&self) -> String {
        if self.saved_peer_sync_ready() {
            "Last action loaded. Continue with Preview From Peer or Sync Saved Peers when both computers are on the same LAN."
                .to_string()
        } else if let Some(hint) = self.saved_peer_sync_hint() {
            format!("Last action loaded. Continue with Preview From Peer, or {hint}")
        } else {
            "Last action loaded. Continue with Preview From Peer when both computers are on the same LAN."
                .to_string()
        }
    }

    fn load_selected_saved_peer_into_form(&mut self) {
        match load_app_config() {
            Ok(config) => {
                let selected_name = self.selected_peer_name.trim();
                let selected_peer = config
                    .peers
                    .iter()
                    .find(|peer| !selected_name.is_empty() && peer.name == selected_name)
                    .or_else(|| config.peers.first());
                if let Some(peer) = selected_peer {
                    let saved_key = resolve_lan_peer_key(peer);
                    let has_saved_key = saved_key
                        .as_deref()
                        .is_some_and(|key| !key.trim().is_empty());
                    self.selected_peer_name = peer.name.clone();
                    self.peer_name = peer.name.clone();
                    self.peer_url = peer.url.clone();
                    self.peer_key = saved_key.unwrap_or_default();
                    self.discovered_peer_requires_lan_key = !has_saved_key;
                    self.last_previewed_peer = None;
                    self.next_step = if has_saved_key {
                        format!(
                            "Loaded paired peer {}. Click Test Connection, then Preview From Peer before Get Missing Fonts From Peer.",
                            peer.name
                        )
                    } else {
                        format!(
                            "Loaded saved peer {} without a saved LAN token. Enter the 8-digit pairing code from that computer, then click Pair Peer.",
                            peer.name
                        )
                    };
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
                let issues = managed_verify_issue_count(&report);
                let next_step = if issues == 0 {
                    "All SyncMyFonts-managed fonts still match the local manifest.".to_string()
                } else {
                    format!("{issues} managed font issue(s) found. Click Repair Managed Fonts if registration is the only problem, then verify again.")
                };
                gui_ok_with_warning_count(&report, next_step, issues)
            }
            Err(error) => gui_error(error),
        });
    }

    fn repair_managed(&mut self) {
        self.start_task(
            "Repairing managed font registration",
            || match repair_managed_fonts() {
                Ok(report) => {
                    let warnings = report.skipped.len() + report.failed.len();
                    let next_step = if warnings == 0 {
                        format!(
                            "Repaired {} managed font registration(s). Run Verify Managed Fonts again, then reopen design apps if needed.",
                            report.repaired.len()
                        )
                    } else {
                        format!(
                            "Repaired {} managed font registration(s); {} item(s) still need attention.",
                            report.repaired.len(),
                            warnings
                        )
                    };
                    gui_ok_with_warning_count(&report, next_step, warnings)
                }
                Err(error) => gui_error(error),
            },
        );
    }

    fn install_validation_font(&mut self) {
        self.start_task(
            "Installing validation font",
            || match install_validation_font(VALIDATION_FONT_URL) {
                Ok(report) => {
                    let next_step = if report.already_present {
                        "The validation font is already installed for this user. Share this computer, then sync from the other computer.".to_string()
                    } else {
                        "Validation font installed. Share this computer, then sync from the other computer and confirm the font appears there.".to_string()
                    };
                    gui_ok(&report, next_step)
                }
                Err(error) => gui_error(error),
            },
        );
    }

    fn run_diagnostics(&mut self) {
        self.start_task("Collecting diagnostics", || match diagnostics() {
            Ok(report) => {
                let warnings = report.warnings.len();
                gui_diagnostics_result(&report, warnings)
            }
            Err(error) => gui_error(error),
        });
    }

    fn run_doctor(&mut self) {
        self.start_task("Checking readiness", || match doctor() {
            Ok(report) => {
                let warnings = report.checks.iter().filter(|check| !check.ok).count();
                let result_summary = Some(gui_readiness_result_summary(&report));
                let result_review = Some(gui_readiness_review(&report));
                gui_ok_with_result_summary_review_and_warning_count(
                    &report,
                    report.next_step.clone(),
                    result_summary,
                    result_review,
                    warnings,
                )
            }
            Err(error) => gui_error(error),
        });
    }

    fn run_validation_report(&mut self) {
        self.start_task("Saving validation report", || match write_validation_report() {
            Ok(file) => {
                let warnings = file
                    .report
                    .readiness
                    .checks
                    .iter()
                    .filter(|check| !check.ok)
                    .count()
                    + file.report.managed_fonts.missing.len()
                    + file.report.managed_fonts.modified.len()
                    + file.report.managed_fonts.unreadable.len()
                    + file.report.managed_fonts.registration_issues.len();
                gui_ok_with_warning_count(
                    &file,
                    format!(
                        "Validation report saved to {}. Keep before/after reports with the clean-machine Mac and Windows sync evidence.",
                        file.path.display()
                    ),
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

    fn open_app_support_folder(&mut self) {
        let folder = app_data_dir().and_then(|path| {
            fs::create_dir_all(&path)
                .with_context(|| format!("creating app support folder {}", path.display()))?;
            Ok(path)
        });
        self.open_folder_action(
            "Open App Support",
            folder,
            "This folder contains SyncMyFonts config, saved peers, preferences, and managed manifest files.",
            "SyncMyFonts could not open the app support folder. Diagnostics will show the config path.",
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
                gui_ok_with_updates(
                    &peers,
                    next_step,
                    None,
                    None,
                    None,
                    discovered_peer,
                    false,
                    false,
                    0,
                )
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
                        None,
                        None,
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
                    let next_step = gui_save_peer_next_step(&peer);
                    let output = redacted_peer_config(&peer);
                    gui_ok_with_updates(
                        &output,
                        next_step,
                        None,
                        None,
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
        let name = self.peer_to_forget_name();
        self.start_task("Forgetting peer", move || match forget_lan_peer(&name) {
            Ok(result) => {
                let next_step = if result.removed {
                    format!(
                        "{name} was removed. Pair or save that computer again if you still need it."
                    )
                } else {
                    format!("No saved peer named {name} was found.")
                };
                let mut gui = gui_ok_with_updates(
                    &result,
                    next_step,
                    None,
                    None,
                    None,
                    None,
                    result.removed,
                    true,
                    0,
                );
                gui.clear_peer_form = result.removed;
                gui
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
                    let next_step = gui_single_peer_sync_next_step(&report, dry_run);
                    let result_summary = Some(gui_single_peer_sync_result_summary(&report));
                    let result_review = Some(gui_single_peer_sync_review(&report));
                    let mut result = gui_ok_with_result_summary_and_review(
                        &report,
                        next_step,
                        result_summary,
                        result_review,
                    );
                    if dry_run {
                        result.previewed_peer =
                            Some(previewed_peer_from_parts(&peer_url, peer_key.as_deref()));
                    }
                    result
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
                let warnings = report
                    .peers
                    .iter()
                    .filter(|peer| peer.error.is_some())
                    .count();
                let next_step = gui_saved_peer_sync_next_step(&report, dry_run);
                let result_summary = Some(gui_saved_peer_sync_result_summary(&report));
                let result_review = Some(gui_saved_peer_sync_review(&report));
                gui_ok_with_result_summary_review_and_warning_count(
                    &report,
                    next_step,
                    result_summary,
                    result_review,
                    warnings,
                )
            }
            Err(error) => gui_error(error),
        });
    }

    fn save_auto_sync_preferences(&mut self) {
        if self.auto_sync_enabled && !self.can_enable_saved_peer_automation() {
            self.auto_sync_enabled = false;
            self.next_step = self.saved_peer_sync_hint().unwrap_or_else(|| {
                "Pair saved LAN peers before turning on auto sync for saved peers.".to_string()
            });
            return;
        }
        let preferences = AppPreferences {
            auto_sync_saved_peers: self.auto_sync_enabled,
            auto_sync_interval_minutes: self.auto_sync_interval_minutes.max(1),
            lan_listen_address: self.listen.clone(),
        };
        self.auto_sync_interval_minutes = preferences.auto_sync_interval_minutes;
        match set_app_preferences(preferences) {
            Ok(_) => {
                self.last_auto_sync_at = None;
                self.next_step = if self.auto_sync_enabled {
                    "Auto sync is on and saved. SyncMyFonts will check saved LAN peers while the app stays open."
                        .to_string()
                } else {
                    "Auto sync is off and saved. Manual saved-peer sync and sign-in sync are still available."
                        .to_string()
                };
            }
            Err(error) => {
                self.output = error.to_string();
                self.next_step = "SyncMyFonts could not save the auto-sync preference.".to_string();
                self.last_result = "Save Auto Sync Preference failed.".to_string();
                self.warning_count = 1;
                let _ = record_action("Save Auto Sync Preference", "failed", 1, &self.next_step);
            }
        }
    }

    fn maybe_auto_sync_saved_peers(&mut self) {
        if !should_auto_sync_saved_peers(
            self.auto_sync_enabled,
            self.task.is_some(),
            saved_peer_repeat_sync_ready().unwrap_or(false),
            self.last_auto_sync_at,
            self.auto_sync_interval_minutes,
            Instant::now(),
        ) {
            return;
        }

        self.start_task("Auto-syncing saved peers", || match lan_sync_all(false) {
            Ok(report) => {
                let warnings = report
                    .peers
                    .iter()
                    .filter(|peer| peer.error.is_some())
                    .count();
                let next_step = gui_saved_peer_sync_next_step(&report, false);
                let next_step = if next_step.starts_with("Installed ") {
                    next_step.replacen("Installed ", "Auto sync installed ", 1)
                } else {
                    format!("Auto sync checked saved peers. {next_step}")
                };
                gui_ok_with_warning_count(&report, next_step, warnings)
            }
            Err(error) => gui_error(error),
        });
    }

    fn install_startup_sync(&mut self) {
        if !self.can_enable_saved_peer_automation() {
            self.next_step = self.saved_peer_sync_hint().unwrap_or_else(|| {
                "Pair saved LAN peers before enabling sign-in sync.".to_string()
            });
            return;
        }
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

    fn uninstall_startup_sync(&mut self) {
        self.start_task("Disabling sign-in sync", || {
            match uninstall_startup_sync() {
                Ok(report) => {
                    let next_step = "Sign-in sync is disabled. You can still sync manually with Sync Saved Peers.".to_string();
                    gui_ok(&report, next_step)
                }
                Err(error) => gui_error(error),
            }
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
                self.last_result =
                    "Share Fonts On This Network failed before starting.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On This Network", "failed", 1, &self.output);
                return;
            }
        };
        let exe = match agent_command_exe() {
            Ok(exe) => exe,
            Err(error) => {
                self.output = format!("locating current executable failed: {error}");
                self.last_result =
                    "Share Fonts On This Network failed before starting.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On This Network", "failed", 1, &self.output);
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
                let _ = set_lan_listen_preference(listen);
                self.refresh_status();
                self.last_pairing_code = pairing_code.clone();
                self.last_pairing_expires_seconds =
                    pairing_code.as_ref().map(|_| PAIRING_CODE_TTL.as_secs());
                self.last_pairing_started_at = pairing_code.as_ref().map(|_| Instant::now());
                let response = ShareResponse {
                    sharing: true,
                    message: format!("Sharing fonts at {}.", self.share_urls.join(", ")),
                    urls: self.share_urls.clone(),
                    pairing_expires_seconds: self.last_pairing_expires_seconds,
                    pairing_code,
                };
                if let Some(code) = &response.pairing_code {
                    self.next_step = format!(
                        "Pairing code {code} is ready and {}. Enter it on the other computer. {}",
                        pairing_code_validity_text(response.pairing_expires_seconds),
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
                    "Share Fonts On This Network completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Share Fonts On This Network", "success", 0, &self.next_step);
            }
            Err(error) => {
                self.last_pairing_code = None;
                self.last_pairing_expires_seconds = None;
                self.last_pairing_started_at = None;
                self.output = error.to_string();
                self.next_step = format!(
                    "Sharing failed to start. Check whether another SyncMyFonts share is already using that port. {}",
                    platform_lan_sharing_guidance()
                );
                self.last_result = "Share Fonts On This Network failed.".to_string();
                self.warning_count = 1;
                let _ = record_action("Share Fonts On This Network", "failed", 1, &self.next_step);
            }
        }
    }

    fn first_run_steps(&self) -> Vec<String> {
        let mut steps = Vec::new();
        steps.push(
            "1. Optional test font: click Install Validation Font on one computer, or use another non-system font you are licensed to sync."
                .to_string(),
        );

        if self.share.is_some() {
            let pairing_hint = self
                .last_pairing_code
                .as_ref()
                .map(|code| format!(" Pairing code {code} is active."))
                .unwrap_or_else(|| {
                    " Use the shared key you typed here, or restart sharing with Shared Key blank for a short pairing code."
                        .to_string()
                });
            steps.push(format!(
                "2. This computer is sharing fonts on the LAN.{} Copy the URL or let the other computer discover it.",
                pairing_hint
            ));
        } else {
            steps.push(
                "2. On the computer that already has the font, leave Shared Key blank and click Share Fonts On This Network."
                    .to_string(),
            );
        }

        if self.peer_url.trim().is_empty() {
            steps.push(
                "3. On the other computer, click Find LAN Peers, select the sharing computer, then enter the pairing code."
                    .to_string(),
            );
        } else if self.peer_key.trim().is_empty() && self.pairing_code.trim().is_empty() {
            steps.push(format!(
                "3. Peer URL loaded for {}. Enter its pairing code, then click Pair Peer.",
                if self.peer_name.trim().is_empty() {
                    "that computer"
                } else {
                    self.peer_name.trim()
                }
            ));
        } else {
            steps.push(
                "3. Peer details are filled in. Click Pair Peer to save the secure LAN token for future syncs."
                    .to_string(),
            );
        }

        if self.saved_peer_names.is_empty() {
            steps.push(
                "4. After pairing, click Preview From Peer first, then Get Missing Fonts From Peer when the preview looks right."
                    .to_string(),
            );
        } else if !self.saved_peer_sync_ready() {
            steps.push(format!(
                "4. Saved peer URLs: {}. Pair each saved peer before using Sync Saved Peers.",
                self.saved_peer_names.join(", ")
            ));
        } else {
            steps.push(format!(
                "4. Saved peers ready: {}. Use Preview From Peer or Sync Saved Peers before installing.",
                self.saved_peer_names.join(", ")
            ));
        }

        steps.push(
            "5. Reopen design apps after installing fonts if they do not appear immediately."
                .to_string(),
        );
        steps
    }

    fn setup_phase(&self) -> String {
        if self.share.is_some() {
            if let Some(code) = &self.last_pairing_code {
                return format!(
                    "Sharing mode: copy this computer's LAN URL and pairing code {code} to the other computer."
                );
            }
            return "Sharing mode: copy this computer's LAN URL and use the shared key on the other computer."
                .to_string();
        }

        if self.peer_url.trim().is_empty() {
            return "Pairing mode: this computer is ready to find or enter the sharing computer's LAN URL."
                .to_string();
        }

        if self.peer_key.trim().is_empty() && self.pairing_code.trim().is_empty() {
            return "Pairing mode: enter the code shown on the sharing computer, then click Pair Peer."
                .to_string();
        }

        if self.saved_peer_names.is_empty() || !self.saved_peer_sync_ready() {
            return "Preview mode: peer details are filled in; preview before installing missing fonts."
                .to_string();
        }

        "Sync mode: saved peers are ready; preview or sync missing fonts, then repeat in the other direction if needed."
            .to_string()
    }

    fn role_card_text(&self) -> String {
        if self.share.is_some() {
            let urls = if self.share_urls.is_empty() {
                "the LAN URL shown in Share This Device".to_string()
            } else {
                self.share_urls.join(" or ")
            };
            let secret_instruction = self
                .last_pairing_code
                .as_ref()
                .map(|code| format!("pairing code {code}"))
                .unwrap_or_else(|| "the shared key you entered here".to_string());
            return format!(
                "This computer: keep sharing on and copy {urls}.\nOther computer: paste or discover that URL, enter {secret_instruction}, click Pair Peer, then Preview From Peer before Get Missing Fonts From Peer."
            );
        }

        if self.peer_url.trim().is_empty() {
            return "This computer: find or enter the sharing computer's LAN URL.\nOther computer: click Share Fonts On This Network with Shared Key blank, then copy its URL and pairing code."
                .to_string();
        }

        if self.peer_key.trim().is_empty() && self.pairing_code.trim().is_empty() {
            return "This computer: enter the pairing code shown on the sharing computer, then click Pair Peer.\nOther computer: keep sharing on until this computer finishes Preview From Peer and Get Missing Fonts From Peer."
                .to_string();
        }

        if self.saved_peer_names.is_empty() || !self.saved_peer_sync_ready() {
            return "This computer: click Preview From Peer, review what will install, then click Get Missing Fonts From Peer.\nOther computer: keep sharing on until this sync finishes."
                .to_string();
        }

        "This computer: use Preview From Peer for one saved peer or Sync Saved Peers for all saved peers.\nOther computer: repeat the same flow in the opposite direction when it has fonts this computer needs."
            .to_string()
    }

    fn lan_readiness_lines(&self) -> Vec<String> {
        let sharing = if self.share.is_some() {
            if self.share_urls.is_empty() {
                "Sharing: on; LAN URL is still loading.".to_string()
            } else {
                format!("Sharing: on at {}", self.share_urls.join(" or "))
            }
        } else {
            "Sharing: off; no port forwarding is required.".to_string()
        };

        let pairing = if self.share.is_some() {
            if let Some(code) = &self.last_pairing_code {
                format!("Pairing: code {code} is ready for the other computer.")
            } else {
                "Pairing: use the shared key entered above.".to_string()
            }
        } else if self.peer_pairing_ready() {
            "Pairing: code entered; Pair Peer is ready.".to_string()
        } else if self.peer_sync_ready() {
            "Pairing: saved token is ready; preview can run.".to_string()
        } else if self.peer_url_ready() {
            "Pairing: peer URL found; enter its pairing code.".to_string()
        } else {
            "Pairing: find a LAN peer or paste its URL.".to_string()
        };

        let saved_peers = if self.saved_peer_names.is_empty() {
            "Saved peers: none yet.".to_string()
        } else if self.saved_peer_key_count < self.saved_peer_names.len() {
            format!(
                "Saved peers: {} saved, {} paired; pair the remaining peer(s) before repeat sync ({})",
                self.saved_peer_names.len(),
                self.saved_peer_key_count,
                self.saved_peer_names.join(", ")
            )
        } else {
            format!(
                "Saved peers: {} ready ({})",
                self.saved_peer_names.len(),
                self.saved_peer_names.join(", ")
            )
        };

        let automation = if self.auto_sync_enabled {
            format!(
                "Automation: auto-sync while app is open every {} minute(s).",
                self.auto_sync_interval_minutes
            )
        } else if self.saved_peer_names.is_empty() {
            "Automation: available after pairing a peer.".to_string()
        } else if self.saved_peer_key_count < self.saved_peer_names.len() {
            "Automation: pair saved peers before enabling repeat sync.".to_string()
        } else {
            "Automation: off; enable after a successful preview.".to_string()
        };

        let secrets = if self.saved_peer_key_count == 0 {
            "Secrets: no saved LAN tokens yet.".to_string()
        } else {
            format!(
                "Secrets: {} saved LAN token(s) are redacted in reports and stored in the native credential store when available, with portable config fallback.",
                self.saved_peer_key_count
            )
        };

        vec![
            sharing,
            pairing,
            "Scope: current-user fonts only; system fonts are excluded.".to_string(),
            saved_peers,
            self.sign_in_sync_status_line(),
            automation,
            secrets,
        ]
    }

    fn sign_in_sync_status_line(&self) -> String {
        match sign_in_sync_installed() {
            Ok(true) => "Sign-in sync: on; saved peers sync when this user signs in.".to_string(),
            Ok(false) if self.can_enable_saved_peer_automation() => {
                "Sign-in sync: off; enable it after a successful saved-peer preview.".to_string()
            }
            Ok(false) => "Sign-in sync: off; available after pairing a peer.".to_string(),
            Err(error) => format!("Sign-in sync: status unavailable ({error})."),
        }
    }

    fn lan_readiness_text(&self) -> String {
        self.lan_readiness_lines().join("\n")
    }

    fn setup_packet_text(&self) -> String {
        let mut lines = vec![
            "SyncMyFonts LAN setup packet".to_string(),
            format!("Device: {}", self.device_name_input.trim()),
            format!("Phase: {}", self.setup_phase()),
            String::new(),
            "Role card:".to_string(),
            self.role_card_text(),
            String::new(),
            "Readiness:".to_string(),
        ];
        lines.extend(self.lan_readiness_lines());
        lines.push(String::new());
        lines.push("First sync steps:".to_string());
        lines.extend(self.first_run_steps());
        lines.push(String::new());
        lines.push("Proof checklist:".to_string());
        lines.push(validation_checklist_text());
        lines.join("\n")
    }

    fn record_copy_url_receipt(&mut self, url: &str) {
        self.output = format!("Copied LAN URL: {url}");
        self.next_step =
            "LAN URL copied. Paste it on the other computer if discovery does not find this device."
                .to_string();
        self.last_result = format!(
            "Copy LAN URL completed at {}",
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.warning_count = 0;
        let _ = record_action("Copy LAN URL", "success", 0, &self.next_step);
    }

    fn record_copy_pairing_code_receipt(&mut self, validity_text: &str) {
        self.output =
            "Copied pairing code. The code is not saved in diagnostics or support reports."
                .to_string();
        self.next_step =
            format!("Pairing code copied and {validity_text}. Enter it on the other computer.");
        self.last_result = format!(
            "Copy Pairing Code completed at {}",
            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
        );
        self.warning_count = 0;
        let _ = record_action("Copy Pairing Code", "success", 0, &self.next_step);
    }

    fn share_invitation_text(&self) -> Option<String> {
        let urls = if self.share_urls.is_empty() {
            return None;
        } else {
            self.share_urls.join(" or ")
        };
        let mut lines = vec![
            "SyncMyFonts LAN pairing".to_string(),
            format!("Sharing computer: {}", self.device_name_input),
            format!("URL: {urls}"),
        ];

        if let Some(code) = &self.last_pairing_code {
            let remaining_seconds = self
                .last_pairing_started_at
                .and_then(|started_at| pairing_code_remaining_seconds(started_at, Instant::now()))
                .or(self.last_pairing_expires_seconds);
            lines.push(format!(
                "Pairing code: {code} ({})",
                pairing_code_validity_text(remaining_seconds)
            ));
            lines.push("On the other computer: paste or discover this URL, enter the pairing code, click Pair Peer, run Preview From Peer, then Get Missing Fonts From Peer.".to_string());
        } else {
            lines.push(
                "Shared key: use the key entered on the sharing computer; it is not copied here."
                    .to_string(),
            );
            lines.push("On the other computer: paste or discover this URL, enter the shared key, run Preview From Peer, then Get Missing Fonts From Peer.".to_string());
        }

        lines.push("No port forwarding is required. Only use this on a trusted LAN.".to_string());
        Some(lines.join("\n"))
    }

    fn peer_url_ready(&self) -> bool {
        peer_url_is_ready(&self.peer_url)
    }

    fn peer_pairing_ready(&self) -> bool {
        self.peer_url_ready() && normalized_pairing_code_is_ready(&self.pairing_code)
    }

    fn peer_sync_ready(&self) -> bool {
        self.peer_url_ready() && !self.peer_key.trim().is_empty()
    }

    fn peer_install_ready(&self) -> bool {
        self.peer_sync_ready()
            && self.last_previewed_peer.as_ref().is_some_and(|previewed| {
                previewed == &previewed_peer_from_parts(&self.peer_url, Some(&self.peer_key))
            })
    }

    fn can_find_lan_peers(&self) -> bool {
        true
    }

    fn can_pair_peer(&self) -> bool {
        self.peer_pairing_ready()
    }

    fn can_test_peer(&self) -> bool {
        self.peer_sync_ready()
    }

    fn can_preview_peer(&self) -> bool {
        self.peer_sync_ready()
    }

    fn can_get_missing_fonts_from_peer(&self) -> bool {
        self.peer_install_ready()
    }

    fn can_save_peer(&self) -> bool {
        self.peer_url_ready()
    }

    fn can_load_saved_peer(&self) -> bool {
        !self.saved_peer_names.is_empty()
    }

    fn peer_to_forget_name(&self) -> String {
        if !self.selected_peer_name.trim().is_empty()
            && self.saved_peer_names.contains(&self.selected_peer_name)
        {
            self.selected_peer_name.clone()
        } else {
            self.peer_name.trim().to_string()
        }
    }

    fn can_forget_peer(&self) -> bool {
        !self.peer_to_forget_name().is_empty()
    }

    fn forget_peer_button_label(&self) -> String {
        let name = self.peer_to_forget_name();
        if name.is_empty() {
            "Forget Peer".to_string()
        } else {
            format!("Forget {name}")
        }
    }

    fn can_enable_saved_peer_automation(&self) -> bool {
        self.saved_peer_sync_ready()
    }

    fn can_change_auto_sync_preference(&self) -> bool {
        self.auto_sync_enabled || self.can_enable_saved_peer_automation()
    }

    fn saved_peer_sync_ready(&self) -> bool {
        !self.saved_peer_names.is_empty()
            && self.saved_peer_key_count == self.saved_peer_names.len()
    }

    fn saved_peer_sync_hint(&self) -> Option<String> {
        if self.saved_peer_names.is_empty() {
            Some("Pair a LAN peer before enabling saved-peer sync.".to_string())
        } else if self.saved_peer_key_count < self.saved_peer_names.len() {
            let missing = self.saved_peer_names.len() - self.saved_peer_key_count;
            Some(format!(
                "Pair {missing} saved peer(s) before using saved-peer sync or automation."
            ))
        } else {
            None
        }
    }

    fn peer_action_hint(&self) -> &'static str {
        if !self.peer_url_ready() {
            "Find a LAN peer or paste the sharing computer's URL first."
        } else if !self.peer_sync_ready() && !self.peer_pairing_ready() {
            "Enter the pairing code from the sharing computer, then pair before previewing."
        } else if self.peer_pairing_ready() && !self.peer_sync_ready() {
            "Pair this peer to save its LAN token, then preview before installing."
        } else if self.peer_sync_ready() && !self.peer_install_ready() {
            "Preview this peer first; Get Missing Fonts From Peer unlocks after preview succeeds."
        } else {
            "Peer preview is current. Get Missing Fonts From Peer installs into this user's font folder."
        }
    }

    fn peer_pairing_detail(&self) -> String {
        if !self.peer_url_ready() {
            return "No peer selected yet. Discovery works only on the same trusted LAN; manual URL entry is still available."
                .to_string();
        }

        let peer_name = if self.peer_name.trim().is_empty() {
            "This peer"
        } else {
            self.peer_name.trim()
        };

        if self.peer_sync_ready() {
            return format!(
                "{peer_name} has a saved LAN token on this computer. Preview again after changing the URL or shared key."
            );
        }

        if self.peer_pairing_ready() {
            return format!(
                "{peer_name} has a pairing code entered. Pairing saves a redacted LAN token so future syncs do not need the short code."
            );
        }

        if !self.pairing_code.trim().is_empty() {
            let digits = normalize_pairing_code(&self.pairing_code).len();
            return format!(
                "{peer_name} needs the full 8-digit pairing code from the sharing computer. {digits}/8 digit(s) entered."
            );
        }

        if self.discovered_peer_requires_lan_key {
            return format!(
                "{peer_name} was discovered and requires pairing. Enter the 8-digit code shown on that computer."
            );
        }

        format!(
            "{peer_name} is selected. If it is sharing with the default setup, enter the 8-digit pairing code shown on that computer."
        )
    }

    fn can_start_sharing(&self) -> bool {
        self.share.is_none() && self.listen_address_ready()
    }

    fn can_stop_sharing(&self) -> bool {
        self.share.is_some()
    }

    fn listen_address_ready(&self) -> bool {
        self.listen.trim().parse::<SocketAddr>().is_ok()
    }

    fn listen_address_detail(&self) -> String {
        if self.listen_address_ready() {
            return format!(
                "Sharing will listen on {}. Use 0.0.0.0:7370 for normal LAN sharing.",
                self.listen.trim()
            );
        }
        "Listen Address must look like 0.0.0.0:7370 or 127.0.0.1:7370 before sharing can start."
            .to_string()
    }

    fn stop_share(&mut self) {
        let Some(mut share) = self.share.take() else {
            self.next_step = "Sharing is already off.".to_string();
            return;
        };
        let _ = share.child.kill();
        let _ = share.child.wait();
        self.refresh_status();
        self.last_pairing_code = None;
        self.last_pairing_expires_seconds = None;
        self.last_pairing_started_at = None;
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
            self.last_pairing_code = None;
            self.last_pairing_expires_seconds = None;
            self.last_pairing_started_at = None;
        }
    }

    fn prune_expired_pairing_code(&mut self) {
        let Some(started_at) = self.last_pairing_started_at else {
            return;
        };
        if pairing_code_remaining_seconds(started_at, Instant::now()).is_some() {
            return;
        }
        self.last_pairing_code = None;
        self.last_pairing_expires_seconds = None;
        self.last_pairing_started_at = None;
        if self.share.is_some() {
            self.next_step =
                "The pairing code expired. Stop sharing and start sharing again to create a fresh code."
                    .to_string();
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
        self.prune_expired_pairing_code();
        self.poll_task();
        self.maybe_auto_sync_saved_peers();
        let task_running = self.task.is_some();
        if task_running {
            ui.ctx().request_repaint_after(Duration::from_millis(100));
        } else if self.last_pairing_started_at.is_some() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        } else if self.auto_sync_enabled {
            ui.ctx().request_repaint_after(Duration::from_secs(5));
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
        ui.heading("First LAN Sync");
        ui.label(self.setup_phase());
        ui.label(self.role_card_text());
        for line in self.lan_readiness_lines() {
            ui.label(line);
        }
        ui.horizontal_wrapped(|ui| {
            if ui.button("Copy Role Card").clicked() {
                let role_card = self.role_card_text();
                ui.ctx().copy_text(role_card.clone());
                self.output = role_card;
                self.next_step =
                    "Role card copied. Use it to coordinate this computer with the other computer."
                        .to_string();
                self.last_result = format!(
                    "Copy Role Card completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Copy Role Card", "success", 0, &self.next_step);
            }
            if ui.button("Copy Readiness").clicked() {
                let readiness = self.lan_readiness_text();
                ui.ctx().copy_text(readiness.clone());
                self.output = readiness;
                self.next_step =
                    "LAN readiness copied. Paste it when checking setup on the other computer."
                        .to_string();
                self.last_result = format!(
                    "Copy Readiness completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Copy Readiness", "success", 0, &self.next_step);
            }
            if ui.button("Copy Setup Packet").clicked() {
                let packet = self.setup_packet_text();
                ui.ctx().copy_text(packet.clone());
                self.output = packet;
                self.next_step =
                    "Setup packet copied. Use it to coordinate this computer and the other LAN peer."
                        .to_string();
                self.last_result = format!(
                    "Copy Setup Packet completed at {}",
                    Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                );
                self.warning_count = 0;
                let _ = record_action("Copy Setup Packet", "success", 0, &self.next_step);
            }
        });
        for step in self.first_run_steps() {
            ui.label(step);
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
                if ui.button("Repair Managed Fonts").clicked() {
                    self.repair_managed();
                }
                if ui.button("Install Validation Font").clicked() {
                    self.install_validation_font();
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
                if ui.button("Copy Validation Plan").clicked() {
                    let checklist = validation_checklist_text();
                    ui.ctx().copy_text(checklist.clone());
                    self.next_step = "Validation plan copied. Use it on the Mac and Windows PC while proving both sync directions.".to_string();
                    self.output = checklist;
                    self.last_result = format!(
                        "Copy Validation Plan completed at {}",
                        Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                    );
                    self.warning_count = 0;
                    let _ = record_action(
                        "Copy Validation Plan",
                        "success",
                        0,
                        &self.next_step,
                    );
                }
                if ui.button("Open Managed Folder").clicked() {
                    self.open_managed_font_folder();
                }
                if ui.button("Open Logs").clicked() {
                    self.open_logs_folder();
                }
                if ui.button("Open App Support").clicked() {
                    self.open_app_support_folder();
                }
                ui.add_enabled_ui(self.saved_peer_sync_ready(), |ui| {
                    if ui.button("Sync Saved Peers").clicked() {
                        self.sync_saved_peers(false);
                    }
                    if ui.button("Dry Run Saved Peers").clicked() {
                        self.sync_saved_peers(true);
                    }
                });
                ui.add_enabled_ui(self.can_enable_saved_peer_automation(), |ui| {
                    if ui.button("Enable Sign-In Sync").clicked() {
                        self.install_startup_sync();
                    }
                });
                if ui.button("Disable Sign-In Sync").clicked() {
                    self.uninstall_startup_sync();
                }
                if ui.button("Install App Shortcuts").clicked() {
                    self.install_app_shortcuts();
                }
            });
            ui.horizontal(|ui| {
                let mut changed = false;
                let mut interval_changed = false;
                ui.add_enabled_ui(self.can_change_auto_sync_preference(), |ui| {
                    changed = ui
                        .checkbox(&mut self.auto_sync_enabled, "Auto Sync Saved Peers")
                        .changed();
                    ui.label("Every");
                    interval_changed = ui
                        .add(
                            eframe::egui::DragValue::new(&mut self.auto_sync_interval_minutes)
                                .range(1..=1440)
                                .speed(1.0),
                        )
                        .changed();
                    ui.label("minutes while this app is open");
                });
                if changed || interval_changed {
                    self.save_auto_sync_preferences();
                }
            });
            if let Some(hint) = self.saved_peer_sync_hint() {
                ui.label(hint);
            }
        });

        ui.separator();
        ui.heading("Saved LAN Peer");
        ui.label(platform_manual_peer_fallback_guidance());
        ui.horizontal(|ui| {
            ui.label("Saved Peer");
            eframe::egui::ComboBox::from_id_salt("saved-peer-selector")
                .selected_text(if self.selected_peer_name.is_empty() {
                    "No saved peers"
                } else {
                    self.selected_peer_name.as_str()
                })
                .show_ui(ui, |ui| {
                    for peer_name in &self.saved_peer_names {
                        ui.selectable_value(
                            &mut self.selected_peer_name,
                            peer_name.clone(),
                            peer_name,
                        );
                    }
                });
            ui.add_enabled_ui(self.can_load_saved_peer(), |ui| {
                if ui.button("Load Saved Peer").clicked() {
                    self.load_selected_saved_peer_into_form();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.label("Name");
            ui.text_edit_singleline(&mut self.peer_name);
            ui.label("URL");
            ui.text_edit_singleline(&mut self.peer_url);
        });
        ui.horizontal(|ui| {
            ui.label(peer_key_label());
            ui.add(eframe::egui::TextEdit::singleline(&mut self.peer_key).password(true));
            ui.label("Pairing Code");
            ui.text_edit_singleline(&mut self.pairing_code);
        });
        ui.label(self.peer_action_hint());
        ui.label(self.peer_pairing_detail());
        ui.add_enabled_ui(!task_running, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(self.can_find_lan_peers(), |ui| {
                    if ui.button("Find LAN Peers").clicked() {
                        self.discover_peers();
                    }
                });
                ui.add_enabled_ui(self.can_pair_peer(), |ui| {
                    if ui.button("Pair Peer").clicked() {
                        self.pair_peer();
                    }
                });
                ui.add_enabled_ui(self.can_test_peer(), |ui| {
                    if ui.button("Test Connection").clicked() {
                        self.test_peer();
                    }
                });
                ui.add_enabled_ui(self.can_preview_peer(), |ui| {
                    if ui.button("Preview From Peer").clicked() {
                        self.sync_peer(true);
                    }
                });
                ui.add_enabled_ui(self.can_get_missing_fonts_from_peer(), |ui| {
                    if ui.button("Get Missing Fonts From Peer").clicked() {
                        self.sync_peer(false);
                    }
                });
                ui.add_enabled_ui(self.can_save_peer(), |ui| {
                    if ui.button("Save Peer").clicked() {
                        self.save_peer();
                    }
                });
                ui.add_enabled_ui(self.can_forget_peer(), |ui| {
                    if ui.button(self.forget_peer_button_label()).clicked() {
                        self.forget_peer();
                    }
                });
            });
        });

        ui.separator();
        ui.heading("Share This Device");
        if self.share.is_some() {
            ui.label(platform_lan_sharing_guidance());
        } else {
            ui.label(platform_pre_share_guidance());
        }
        ui.horizontal(|ui| {
            ui.label("Listen Address");
            ui.text_edit_singleline(&mut self.listen);
            ui.label(share_key_label());
            ui.add(eframe::egui::TextEdit::singleline(&mut self.share_key).password(true));
        });
        ui.label(self.listen_address_detail());
        ui.add_enabled_ui(!task_running, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(self.can_start_sharing(), |ui| {
                    if ui.button("Share Fonts On This Network").clicked() {
                        self.start_share();
                    }
                });
                ui.add_enabled_ui(self.can_stop_sharing(), |ui| {
                    if ui.button("Stop Sharing").clicked() {
                        self.stop_share();
                    }
                });
            });
        });
        if self.share_urls.is_empty() {
            ui.label("Sharing is off. No port forwarding is required.");
        } else {
            ui.horizontal_wrapped(|ui| {
                ui.label(format!(
                    "Use this URL from another computer: {}",
                    self.share_urls.join(" or ")
                ));
                if ui.button("Copy URL").clicked() {
                    if let Some(url) = self.share_urls.first() {
                        ui.ctx().copy_text(url.clone());
                        let copied_url = url.clone();
                        self.record_copy_url_receipt(&copied_url);
                    }
                }
                if let Some(code) = &self.last_pairing_code {
                    let remaining_seconds = self
                        .last_pairing_started_at
                        .and_then(|started_at| {
                            pairing_code_remaining_seconds(started_at, Instant::now())
                        })
                        .or(self.last_pairing_expires_seconds);
                    ui.label(format!(
                        "Pairing code: {code} ({})",
                        pairing_code_validity_text(remaining_seconds)
                    ));
                    if ui.button("Copy Code").clicked() {
                        ui.ctx().copy_text(code.clone());
                        let validity_text = pairing_code_validity_text(remaining_seconds);
                        self.record_copy_pairing_code_receipt(&validity_text);
                    }
                }
                if ui.button("Copy Pairing Instructions").clicked() {
                    if let Some(invitation) = self.share_invitation_text() {
                        ui.ctx().copy_text(invitation.clone());
                        self.output = invitation;
                        self.next_step = pairing_instructions_copied_next_step().to_string();
                        self.last_result = format!(
                            "Copy Pairing Instructions completed at {}",
                            Utc::now().format("%Y-%m-%d %H:%M:%S UTC")
                        );
                        self.warning_count = 0;
                        let _ = record_action(
                            "Copy Pairing Instructions",
                            "success",
                            0,
                            &self.next_step,
                        );
                    }
                }
            });
        }

        ui.separator();
        ui.heading("Result");
        ui.label(&self.next_step);
        ui.horizontal_wrapped(|ui| {
            if ui.button("Copy Result").clicked() {
                ui.ctx().copy_text(self.output.clone());
                self.next_step = "Result copied.".to_string();
            }
            let has_result_review = self.last_result_review.is_some();
            ui.add_enabled_ui(has_result_review, |ui| {
                if ui.button("Copy Review").clicked() {
                    if let Some(review) = &self.last_result_review {
                        ui.ctx().copy_text(review.clone());
                        self.next_step =
                            "Readable install review copied. Use it to check skipped fonts."
                                .to_string();
                    }
                }
            });
            let has_support_report = self.last_support_report.is_some();
            ui.add_enabled_ui(has_support_report, |ui| {
                if ui.button("Copy Support Report").clicked() {
                    if let Some(report) = &self.last_support_report {
                        ui.ctx().copy_text(report.clone());
                        self.next_step = "Redacted support report copied.".to_string();
                    }
                }
            });
        });
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
    gui_ok_with_updates(value, next_step, None, None, None, None, false, false, 0)
}

fn gui_save_peer_next_step(peer: &LanPeerConfig) -> String {
    if peer
        .lan_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        format!(
            "{} is saved with a LAN token. Test Connection, Preview From Peer, then Get Missing Fonts From Peer. Use Sync Saved Peers later for repeat syncs.",
            peer.name
        )
    } else {
        format!(
            "{} is saved as a peer URL. Enter the 8-digit pairing code from that computer, then click Pair Peer before using saved-peer sync.",
            peer.name
        )
    }
}

fn gui_ok_with_warning_count<T: Serialize>(
    value: &T,
    next_step: String,
    warning_count: usize,
) -> GuiTaskResult {
    gui_ok_with_result_summary_and_warning_count(value, next_step, None, warning_count)
}

fn gui_ok_with_result_summary_and_review<T: Serialize>(
    value: &T,
    next_step: String,
    result_summary: Option<String>,
    result_review: Option<String>,
) -> GuiTaskResult {
    gui_ok_with_result_summary_review_and_warning_count(
        value,
        next_step,
        result_summary,
        result_review,
        0,
    )
}

fn gui_ok_with_result_summary_and_warning_count<T: Serialize>(
    value: &T,
    next_step: String,
    result_summary: Option<String>,
    warning_count: usize,
) -> GuiTaskResult {
    gui_ok_with_result_summary_review_and_warning_count(
        value,
        next_step,
        result_summary,
        None,
        warning_count,
    )
}

fn gui_ok_with_result_summary_review_and_warning_count<T: Serialize>(
    value: &T,
    next_step: String,
    result_summary: Option<String>,
    result_review: Option<String>,
    warning_count: usize,
) -> GuiTaskResult {
    gui_ok_with_updates(
        value,
        next_step,
        result_summary,
        result_review,
        None,
        None,
        false,
        false,
        warning_count,
    )
}

fn gui_ok_with_updates<T: Serialize>(
    value: &T,
    next_step: String,
    result_summary: Option<String>,
    result_review: Option<String>,
    peer: Option<LanPeerConfig>,
    discovered_peer: Option<LanDiscoveredPeer>,
    clear_peer_key: bool,
    refresh_saved_peers: bool,
    warning_count: usize,
) -> GuiTaskResult {
    GuiTaskResult {
        output: serde_json::to_string_pretty(value).unwrap_or_else(|_| "ok".to_string()),
        next_step,
        result_summary,
        result_review,
        peer,
        discovered_peer,
        clear_peer_key,
        clear_peer_form: false,
        refresh_saved_peers,
        support_report: None,
        warning_count,
        previewed_peer: None,
    }
}

fn gui_diagnostics_result(report: &DiagnosticsReport, warning_count: usize) -> GuiTaskResult {
    GuiTaskResult {
        output: serde_json::to_string_pretty(report).unwrap_or_else(|_| "ok".to_string()),
        next_step: "Diagnostics are redacted and safe to paste into a support issue.".to_string(),
        result_summary: Some(format!(
            "Diagnostics finished with {warning_count} warning(s)."
        )),
        result_review: None,
        peer: None,
        discovered_peer: None,
        clear_peer_key: false,
        clear_peer_form: false,
        refresh_saved_peers: false,
        support_report: Some(report.support_report_text.clone()),
        warning_count,
        previewed_peer: None,
    }
}

fn gui_readiness_result_summary(report: &DoctorReport) -> String {
    let failed = report.checks.iter().filter(|check| !check.ok).count();
    let passed = report.checks.len().saturating_sub(failed);
    if failed == 0 {
        format!("Readiness: {passed} check(s) passed; LAN sync is ready.")
    } else {
        format!("Readiness: {passed} check(s) passed; {failed} check(s) need attention.")
    }
}

fn gui_readiness_review(report: &DoctorReport) -> String {
    let mut lines = Vec::new();
    lines.push("SyncMyFonts readiness review".to_string());
    lines.push(gui_readiness_result_summary(report));
    lines.push(format!("Next step: {}", report.next_step));
    lines.push(String::new());
    lines.push("Checks:".to_string());
    for check in &report.checks {
        let status = if check.ok { "OK" } else { "Needs attention" };
        lines.push(format!("- {status}: {} - {}", check.name, check.message));
    }
    lines.join("\n")
}

fn gui_last_action_summary(action: &ActionRecord) -> String {
    format!(
        "Last action: {} {} at {} · warnings: {}",
        action.action, action.status, action.finished_at, action.warning_count
    )
}

fn gui_error(error: anyhow::Error) -> GuiTaskResult {
    let output = error.to_string();
    let next_step = gui_error_next_step(&output);
    GuiTaskResult {
        output,
        next_step,
        result_summary: None,
        result_review: None,
        peer: None,
        discovered_peer: None,
        clear_peer_key: false,
        refresh_saved_peers: false,
        support_report: None,
        warning_count: 1,
        clear_peer_form: false,
        previewed_peer: None,
    }
}

fn gui_error_next_step(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("invalid listen address") {
        return "The listen address is invalid. Use an address like 0.0.0.0:7370, then try sharing again.".to_string();
    }
    if lower.contains("lan share did not answer")
        || lower.contains("address already in use")
        || lower.contains("os error 48")
        || lower.contains("os error 10048")
    {
        return format!(
            "Sharing could not start on that port. Stop the other SyncMyFonts share or choose a different Listen Address. {}",
            platform_lan_sharing_guidance()
        );
    }
    if lower.contains("lan peer rejected pairing request")
        || lower.contains("invalid pairing code")
        || lower.contains("pairing code expired")
        || lower.contains("pairing is not enabled")
    {
        return "The pairing code was rejected. Start sharing again on the other computer, copy the fresh 8-digit code, and pair within 10 minutes.".to_string();
    }
    if lower.contains("lan peer rejected manifest request")
        || lower.contains("lan peer rejected font download")
        || lower.contains("401")
        || lower.contains("unauthorized")
    {
        return "The shared key did not match this peer. Pair again with the code shown on the sharing computer, or update the saved peer key.".to_string();
    }
    if lower.contains("failed to lookup address information")
        || lower.contains("connection refused")
        || lower.contains("connection reset")
        || lower.contains("timed out")
        || lower.contains("timeout")
        || lower.contains("error trying to connect")
    {
        return format!(
            "SyncMyFonts could not reach that peer. Make sure the other computer is sharing, both devices are on the same trusted LAN, and the peer URL is correct. {}",
            platform_manual_peer_fallback_guidance()
        );
    }
    if lower.contains("builder error") || lower.contains("relative url without a base") {
        return "The peer URL is invalid. Enter a full URL like http://192.168.1.50:7370, then try again.".to_string();
    }
    format!(
        "That action failed. Review the output, then check the peer URL, pairing code, or network access. {}",
        platform_manual_peer_fallback_guidance()
    )
}

fn gui_single_peer_sync_next_step(report: &LanSyncReport, dry_run: bool) -> String {
    let summary = SkipSummary::from_lines(&report.skipped);
    if dry_run {
        if summary.would_install > 0 {
            return format!(
                "{} missing installable font(s) can be installed from this peer.",
                summary.would_install
            );
        }
        return gui_no_install_summary(
            &summary,
            "No missing installable fonts were found from this peer.",
        );
    }

    if !report.installed.is_empty() {
        let mut next_step = format!(
            "Installed {} font(s). Reopen design apps if they do not appear yet.",
            report.installed.len()
        );
        append_skip_context(&mut next_step, &summary);
        return next_step;
    }

    gui_no_install_summary(&summary, "No new fonts were installed.")
}

fn gui_single_peer_sync_result_summary(report: &LanSyncReport) -> String {
    let summary = SkipSummary::from_lines(&report.skipped);
    let skipped = report
        .skipped
        .len()
        .saturating_sub(summary.would_install + summary.already_present);
    if report.dry_run {
        return format!(
            "Preview: {} missing installable, {} already here, {} skipped, {} peer font(s) checked.",
            summary.would_install, summary.already_present, skipped, report.peer_fonts
        );
    }
    format!(
        "Sync result: {} installed, {} already here, {} skipped, {} peer font(s) checked.",
        report.installed.len(),
        summary.already_present,
        skipped,
        report.peer_fonts
    )
}

fn gui_single_peer_sync_review(report: &LanSyncReport) -> String {
    let mut lines = Vec::new();
    if report.dry_run {
        lines.push("Preview From Peer review".to_string());
    } else {
        lines.push("Get Missing Fonts From Peer review".to_string());
    }
    lines.push(format!(
        "Peer fonts checked: {}; local fonts known: {}",
        report.peer_fonts, report.known_local
    ));
    append_installed_review_lines(&mut lines, &report.installed);
    append_skipped_review_lines(&mut lines, &report.skipped);
    lines.join("\n")
}

fn gui_saved_peer_sync_next_step(report: &LanSyncAllReport, dry_run: bool) -> String {
    let installed = report
        .peers
        .iter()
        .map(|peer| peer.installed.len())
        .sum::<usize>();
    let failed = report
        .peers
        .iter()
        .filter(|peer| peer.error.is_some())
        .count();
    let summary = SkipSummary::from_lines(report.peers.iter().flat_map(|peer| peer.skipped.iter()));

    if dry_run {
        if summary.would_install > 0 {
            let mut next_step = format!(
                "Dry run found {} missing installable font(s) across saved peers.",
                summary.would_install
            );
            append_peer_error_context(&mut next_step, failed);
            append_skip_context(&mut next_step, &summary);
            return next_step;
        }
        let mut next_step = gui_no_install_summary(
            &summary,
            "Dry run finished with no missing installable fonts.",
        );
        append_peer_error_context(&mut next_step, failed);
        return next_step;
    }

    if installed > 0 {
        let mut next_step =
            format!("Installed {installed} font(s). Reopen design apps if they do not appear yet.");
        append_peer_error_context(&mut next_step, failed);
        append_skip_context(&mut next_step, &summary);
        return next_step;
    }

    let mut next_step = gui_no_install_summary(
        &summary,
        "Saved peer sync finished. No new fonts were installed.",
    );
    append_peer_error_context(&mut next_step, failed);
    next_step
}

fn gui_saved_peer_sync_result_summary(report: &LanSyncAllReport) -> String {
    let installed = report
        .peers
        .iter()
        .map(|peer| peer.installed.len())
        .sum::<usize>();
    let failed = report
        .peers
        .iter()
        .filter(|peer| peer.error.is_some())
        .count();
    let summary = SkipSummary::from_lines(report.peers.iter().flat_map(|peer| peer.skipped.iter()));
    let skipped = report
        .peers
        .iter()
        .map(|peer| peer.skipped.len())
        .sum::<usize>()
        .saturating_sub(summary.would_install + summary.already_present);
    if report.dry_run {
        return format!(
            "Saved peer preview: {} missing installable, {} already here, {} skipped, {} failed peer(s), {} peer(s) checked.",
            summary.would_install,
            summary.already_present,
            skipped,
            failed,
            report.peers.len()
        );
    }
    format!(
        "Saved peer sync: {installed} installed, {} already here, {skipped} skipped, {failed} failed peer(s), {} peer(s) checked.",
        summary.already_present,
        report.peers.len()
    )
}

fn gui_saved_peer_sync_review(report: &LanSyncAllReport) -> String {
    let mut lines = Vec::new();
    if report.dry_run {
        lines.push("Saved peer preview review".to_string());
    } else {
        lines.push("Saved peer sync review".to_string());
    }

    if report.peers.is_empty() {
        lines.push("No saved peers were checked.".to_string());
        return lines.join("\n");
    }

    for peer in &report.peers {
        lines.push(format!("Peer: {}", peer.name));
        if let Some(error) = &peer.error {
            lines.push(format!("- Failed: {error}"));
            continue;
        }
        append_installed_review_lines(&mut lines, &peer.installed);
        append_skipped_review_lines(&mut lines, &peer.skipped);
    }
    lines.join("\n")
}

fn gui_no_install_summary(summary: &SkipSummary, fallback: &str) -> String {
    if summary.system_conflicts > 0 {
        return format!(
            "Skipped {} font(s) because their names conflict with system fonts on this computer. Those fonts were not installed.",
            summary.system_conflicts
        );
    }
    if summary.unsupported > 0 {
        return format!(
            "Skipped {} unsupported font file(s). SyncMyFonts installs desktop TTF/OTF/TTC/OTC fonts.",
            summary.unsupported
        );
    }
    if summary.already_present > 0 && summary.total() == summary.already_present {
        return format!(
            "No new fonts were installed because {} peer font(s) are already present here.",
            summary.already_present
        );
    }
    fallback.to_string()
}

fn append_installed_review_lines(lines: &mut Vec<String>, installed: &[PathBuf]) {
    if installed.is_empty() {
        lines.push("- Installed: none".to_string());
    } else {
        for path in installed {
            lines.push(format!("- Installed: {}", path.display()));
        }
    }
}

fn append_skipped_review_lines(lines: &mut Vec<String>, skipped: &[String]) {
    if skipped.is_empty() {
        lines.push("- Skipped: none".to_string());
        return;
    }

    for item in skipped {
        lines.push(format!("- {}", readable_skip_line(item)));
    }
}

fn readable_skip_line(item: &str) -> String {
    if let Some(name) = item.strip_prefix("would install ") {
        return format!("Would install: {name}");
    }
    if item.contains("already present") {
        return format!("Already here: {item}");
    }
    if item.contains("unsupported format") || item.contains("unsupported-format") {
        return format!("Unsupported format: {item}");
    }
    if item.contains("system-font-conflict") {
        return format!("System font conflict, not installed: {item}");
    }
    format!("Skipped: {item}")
}

fn append_skip_context(next_step: &mut String, summary: &SkipSummary) {
    let mut details = Vec::new();
    if summary.system_conflicts > 0 {
        details.push(format!(
            "{} system-font conflict(s) were safely skipped",
            summary.system_conflicts
        ));
    }
    if summary.unsupported > 0 {
        details.push(format!(
            "{} unsupported font file(s) were skipped",
            summary.unsupported
        ));
    }
    if !details.is_empty() {
        next_step.push(' ');
        next_step.push_str(&details.join("; "));
        next_step.push('.');
    }
}

fn append_peer_error_context(next_step: &mut String, failed: usize) {
    if failed > 0 {
        next_step.push_str(&format!(
            " {failed} saved peer(s) could not be reached; check sharing and LAN access on those computers."
        ));
    }
}

fn redacted_peer_config(peer: &LanPeerConfig) -> RedactedPeer {
    RedactedPeer {
        name: peer.name.clone(),
        url: peer.url.clone(),
        has_lan_key: lan_peer_has_key(peer),
        key_storage: lan_peer_key_storage(peer),
    }
}

fn redacted_lan_peers() -> Result<Vec<RedactedPeer>> {
    Ok(load_app_config()?
        .peers
        .iter()
        .map(redacted_peer_config)
        .collect())
}

fn lan_peer_key_storage(peer: &LanPeerConfig) -> &'static str {
    if peer.lan_key_secret_id.is_some() {
        "native-credential-store"
    } else if peer
        .lan_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        "portable-config-fallback"
    } else {
        "none"
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

fn platform_pre_share_guidance() -> &'static str {
    #[cfg(target_os = "windows")]
    {
        "Only click Share Fonts On This Network when this Windows PC has fonts another computer needs. Receiving fonts from another computer does not require an inbound firewall prompt."
    }
    #[cfg(target_os = "macos")]
    {
        "Only click Share Fonts On This Network when this Mac has fonts another computer needs. Receiving fonts from another computer can use Find LAN Peers or a pasted LAN URL."
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        "Only click Share Fonts On This Network when this computer has fonts another computer needs. Receiving fonts can use Find LAN Peers or a pasted LAN URL."
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

fn peer_key_label() -> &'static str {
    "Shared Key (optional)"
}

fn share_key_label() -> &'static str {
    "Shared Key (optional)"
}

fn pairing_instructions_copied_next_step() -> &'static str {
    "Pairing instructions copied. Paste them on the other computer, then pair, preview, and use Get Missing Fonts From Peer."
}

fn previewed_peer_from_parts(url: &str, lan_key: Option<&str>) -> PreviewedPeer {
    PreviewedPeer {
        url: url.trim().to_string(),
        key_fingerprint: lan_key
            .map(str::trim)
            .filter(|key| !key.is_empty())
            .map(lan_key_fingerprint),
    }
}

fn lan_key_fingerprint(key: &str) -> String {
    hex::encode(Sha256::digest(format!("syncmyfonts-lan-preview:{key}")))
}

fn should_auto_sync_saved_peers(
    enabled: bool,
    task_running: bool,
    saved_peer_sync_ready: bool,
    last_sync_at: Option<Instant>,
    interval_minutes: u64,
    now: Instant,
) -> bool {
    if !enabled || task_running || !saved_peer_sync_ready {
        return false;
    }
    let interval = Duration::from_secs(interval_minutes.max(1) * 60);
    last_sync_at
        .map(|last_sync_at| now.duration_since(last_sync_at) >= interval)
        .unwrap_or(true)
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
            preferences: AppPreferences::default(),
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

fn saved_peer_repeat_sync_ready() -> Result<bool> {
    let config = load_app_config()?;
    Ok(!config.peers.is_empty() && saved_lan_key_count(&config) == config.peers.len())
}

fn saved_peer_summary_text(config: &AppConfig) -> String {
    if config.peers.is_empty() {
        return "Saved peers: none yet.".to_string();
    }

    let saved = config.peers.len();
    let paired = saved_lan_key_count(config);
    let names = config
        .peers
        .iter()
        .map(|peer| peer.name.as_str())
        .collect::<Vec<_>>()
        .join(", ");

    if paired == saved {
        format!("Saved peers: {saved} paired ({names})")
    } else {
        format!("Saved peers: {saved} saved, {paired} paired ({names})")
    }
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
    let normalized_interval = config.preferences.auto_sync_interval_minutes.clamp(1, 1440);
    if normalized_interval != config.preferences.auto_sync_interval_minutes {
        config.preferences.auto_sync_interval_minutes = normalized_interval;
        changed = true;
    }
    let normalized_listen = normalize_lan_listen_address(&config.preferences.lan_listen_address);
    if normalized_listen != config.preferences.lan_listen_address {
        config.preferences.lan_listen_address = normalized_listen;
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

fn set_friendly_device_name(name: String) -> Result<AppConfig> {
    let mut config = load_app_config()?;
    config.friendly_device_name = normalize_friendly_device_name(&name);
    save_app_config(&config)?;
    Ok(config)
}

fn set_app_preferences(preferences: AppPreferences) -> Result<AppConfig> {
    let mut config = load_app_config()?;
    config.preferences = AppPreferences {
        auto_sync_saved_peers: preferences.auto_sync_saved_peers,
        auto_sync_interval_minutes: preferences.auto_sync_interval_minutes.clamp(1, 1440),
        lan_listen_address: normalize_lan_listen_address(&preferences.lan_listen_address),
    };
    save_app_config(&config)?;
    Ok(config)
}

fn set_lan_listen_preference(listen: SocketAddr) -> Result<AppConfig> {
    let mut config = load_app_config()?;
    config.preferences.lan_listen_address = listen.to_string();
    save_app_config(&config)?;
    Ok(config)
}

fn normalize_lan_listen_address(value: &str) -> String {
    value
        .trim()
        .parse::<SocketAddr>()
        .map(|listen| listen.to_string())
        .unwrap_or_else(|_| default_lan_listen_address())
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
        format!(
            "Auto sync saved peers: {} every {} minute(s)",
            if report.preferences.auto_sync_saved_peers {
                "on"
            } else {
                "off"
            },
            report.preferences.auto_sync_interval_minutes
        ),
        format!(
            "LAN listen address: {}",
            report.preferences.lan_listen_address
        ),
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
        let saved_key_count = report
            .saved_peers
            .iter()
            .filter(|peer| peer.has_lan_key)
            .count();
        let portable_key_count = report
            .saved_peers
            .iter()
            .filter(|peer| peer.key_storage == "portable-config-fallback")
            .count();
        let native_key_count = report
            .saved_peers
            .iter()
            .filter(|peer| peer.key_storage == "native-credential-store")
            .count();
        if saved_key_count > 0 {
            lines.push(format!(
                "Secret storage: {saved_key_count} saved LAN token(s) are redacted here; {native_key_count} native credential-store reference(s), {portable_key_count} portable config fallback token(s)."
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

fn sign_in_sync_installed() -> Result<bool> {
    let helper_path = startup_sync_helper_path()?;
    #[cfg(target_os = "macos")]
    {
        Ok(helper_path.exists() && macos_startup_sync_plist_path()?.exists())
    }
    #[cfg(target_os = "windows")]
    {
        Ok(helper_path.exists() && windows_startup_sync_shortcut_path()?.exists())
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Ok(helper_path.exists())
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
            helper_removed: false,
            registration_removed: false,
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
            helper_removed: false,
            registration_removed: false,
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
            helper_removed: false,
            registration_removed: false,
            saved_peer_count,
            message: "Wrote a saved-peer sync helper, but automatic sign-in registration is not supported on this platform yet."
                .to_string(),
        })
    }
}

fn uninstall_startup_sync() -> Result<StartupSyncReport> {
    let agent_path = agent_command_exe()?;
    let config = load_app_config()?;
    let saved_peer_count = config.peers.len();
    let helper_path = startup_sync_helper_path()?;

    #[cfg(target_os = "macos")]
    {
        let registration_path = macos_startup_sync_plist_path()?;
        if std::env::var("SYNCMYFONTS_SKIP_STARTUP_REGISTRATION").as_deref() != Ok("1")
            && registration_path.exists()
        {
            let _ = Command::new("launchctl")
                .args(["bootout", &format!("gui/{}", unsafe { libc_uid() })])
                .arg(&registration_path)
                .status();
        }
        let registration_removed = remove_file_if_exists(&registration_path)?;
        let helper_removed = remove_file_if_exists(&helper_path)?;
        return Ok(StartupSyncReport {
            installed: false,
            platform: platform_name(),
            agent_path,
            helper_path,
            registration_path,
            helper_removed,
            registration_removed,
            saved_peer_count,
            message: "Removed the per-user LaunchAgent sign-in sync helper.".to_string(),
        });
    }

    #[cfg(target_os = "windows")]
    {
        let registration_path = windows_startup_sync_shortcut_path()?;
        let registration_removed = remove_file_if_exists(&registration_path)?;
        let helper_removed = remove_file_if_exists(&helper_path)?;
        return Ok(StartupSyncReport {
            installed: false,
            platform: platform_name(),
            agent_path,
            helper_path,
            registration_path,
            helper_removed,
            registration_removed,
            saved_peer_count,
            message: "Removed the per-user Startup folder sign-in sync helper.".to_string(),
        });
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let registration_path = helper_path.clone();
        let helper_removed = remove_file_if_exists(&helper_path)?;
        Ok(StartupSyncReport {
            installed: false,
            platform: platform_name(),
            agent_path,
            helper_path,
            registration_path,
            helper_removed,
            registration_removed: helper_removed,
            saved_peer_count,
            message: "Removed the saved-peer sync helper for this platform.".to_string(),
        })
    }
}

fn remove_file_if_exists(path: &Path) -> Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
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
        registration_issues: Vec::new(),
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

        if !skip_platform_font_registration()
            && let Err(error) = verify_platform_registration(&record)
        {
            report.registration_issues.push(managed_verify_issue(
                &record,
                &format!("platform registration check failed: {error}"),
            ));
            continue;
        }

        report.ok += 1;
    }

    Ok(report)
}

fn repair_managed_fonts() -> Result<ManagedRepairReport> {
    let manifest_path = managed_manifest_path()?;
    let manifest = load_managed_manifest()?;
    let total = manifest.installed.len();
    let mut report = ManagedRepairReport {
        manifest_path,
        total,
        repaired: Vec::new(),
        skipped: Vec::new(),
        failed: Vec::new(),
    };

    for record in manifest.installed {
        match managed_record_file_is_intact(&record) {
            Ok(()) => {}
            Err(error) => {
                report.skipped.push(managed_verify_issue(
                    &record,
                    &format!("managed font was not repaired because it is not intact: {error}"),
                ));
                continue;
            }
        }

        match platform_post_install(&record.path) {
            Ok(()) => report.repaired.push(ManagedRepairEntry {
                sha256: record.sha256,
                file_name: record.file_name,
                path: record.path,
                message: platform_repair_message(),
            }),
            Err(error) => report.failed.push(managed_verify_issue(
                &record,
                &format!("platform registration repair failed: {error}"),
            )),
        }
    }

    Ok(report)
}

fn install_validation_font(url: &str) -> Result<ValidationFontInstallReport> {
    let bytes = http_client()?
        .get(url)
        .send()
        .with_context(|| format!("downloading validation font from {url}"))?
        .error_for_status()
        .with_context(|| format!("validation font download was rejected by {url}"))?
        .bytes()
        .context("reading validation font bytes")?;
    install_validation_font_bytes(url, bytes.as_ref())
}

fn install_validation_font_bytes(url: &str, bytes: &[u8]) -> Result<ValidationFontInstallReport> {
    let sha256 = hex::encode(Sha256::digest(bytes));
    let safe_name = safe_file_name(VALIDATION_FONT_FILE_NAME, &sha256);
    if let Some(conflict_path) = system_font_filename_conflict(&safe_name)? {
        bail!(
            "system-font-conflict: {} conflicts with {}",
            safe_name,
            conflict_path.display()
        );
    }

    let install_dir = user_font_dir()?;
    fs::create_dir_all(&install_dir)
        .with_context(|| format!("creating {}", install_dir.display()))?;
    let destination = unique_destination(&install_dir, &safe_name, &sha256)?;
    let already_present = destination.exists();
    if !already_present {
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
    }

    if let Err(error) = platform_post_install(&destination) {
        if !already_present {
            fs::remove_file(&destination).with_context(|| {
                format!(
                    "validation font registration failed and cleanup could not remove {}",
                    destination.display()
                )
            })?;
        }
        return Err(error).with_context(|| {
            format!(
                "validation font registration failed after installing {}",
                destination.display()
            )
        });
    }

    Ok(ValidationFontInstallReport {
        source_url: url.to_string(),
        file_name: safe_name,
        path: destination,
        sha256,
        size_bytes: bytes.len() as u64,
        already_present,
        message: "Installed a known OFL validation font for this user. Share this computer, then sync from the other computer to prove LAN font sync.".to_string(),
    })
}

fn managed_record_file_is_intact(record: &ManagedFontRecord) -> Result<()> {
    if !record.path.exists() {
        bail!("managed font file is missing");
    }
    let bytes =
        fs::read(&record.path).with_context(|| format!("reading {}", record.path.display()))?;
    let actual_sha256 = hex::encode(Sha256::digest(&bytes));
    if actual_sha256 != record.sha256 {
        bail!(
            "managed font hash changed from {} to {}",
            record.sha256,
            actual_sha256
        );
    }
    if bytes.len() as u64 != record.size_bytes {
        bail!(
            "managed font size changed from {} to {} bytes",
            record.size_bytes,
            bytes.len()
        );
    }
    Ok(())
}

fn managed_verify_issue(record: &ManagedFontRecord, message: &str) -> ManagedVerifyIssue {
    ManagedVerifyIssue {
        sha256: record.sha256.clone(),
        file_name: record.file_name.clone(),
        path: record.path.clone(),
        message: message.to_string(),
    }
}

fn platform_repair_message() -> String {
    #[cfg(target_os = "windows")]
    {
        "Re-registered the font in the current user's Windows font table.".to_string()
    }
    #[cfg(target_os = "macos")]
    {
        "Confirmed the intact managed font is in the current user's macOS font folder and can be parsed by CoreText.".to_string()
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "Confirmed the intact managed font record.".to_string()
    }
}

#[cfg(target_os = "windows")]
fn windows_registry_value_name_for_font_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("SyncMyFonts Font");
    format!("{stem} (SyncMyFonts)")
}

#[cfg(target_os = "windows")]
fn verify_platform_registration(record: &ManagedFontRecord) -> Result<()> {
    let value_name = windows_registry_value_name_for_font_path(&record.path);
    let output = Command::new("reg")
        .args([
            "query",
            r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
            "/v",
            &value_name,
        ])
        .output()
        .context("querying font registration in HKCU")?;
    if !output.status.success() {
        bail!("Windows registry value {value_name:?} is missing");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected_path = record.path.to_string_lossy();
    if !stdout.contains(expected_path.as_ref()) {
        bail!(
            "Windows registry value {value_name:?} does not point to {}",
            record.path.display()
        );
    }
    add_windows_font_resource(&record.path).context("probing Windows font loadability")?;
    remove_windows_font_resource(&record.path);
    notify_windows_font_change();
    Ok(())
}

#[cfg(target_os = "macos")]
fn verify_platform_registration(record: &ManagedFontRecord) -> Result<()> {
    let managed_dir = managed_font_dir()?;
    if !record.path.starts_with(&managed_dir) {
        bail!(
            "managed macOS font is outside {}; clean-machine testing must confirm app visibility",
            managed_dir.display()
        );
    }
    macos_font_loadability_summary(&record.path)?;
    Ok(())
}

#[cfg(target_os = "macos")]
fn macos_font_loadability_summary(path: &Path) -> Result<String> {
    let font = font_kit::font::Font::from_path(path, 0)
        .with_context(|| format!("loading {} through CoreText", path.display()))?;
    let family_name = font.family_name();
    let full_name = font.full_name();
    let postscript_name = font
        .postscript_name()
        .unwrap_or_else(|| "unknown PostScript name".to_string());
    if family_name.trim().is_empty() || full_name.trim().is_empty() {
        bail!(
            "CoreText loaded {} but returned an empty family or full name",
            path.display()
        );
    }
    Ok(format!(
        "CoreText loaded {family_name} / {full_name} / {postscript_name}"
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn verify_platform_registration(_record: &ManagedFontRecord) -> Result<()> {
    Ok(())
}

fn diagnostics_warnings(
    config: &AppConfig,
    mut warnings: Vec<String>,
    manifest_result: Result<ManagedManifest>,
) -> Vec<String> {
    if let Err(error) = manifest_result {
        warnings.push(format!("managed manifest unavailable: {error}"));
    }
    let portable_key_count = portable_lan_key_count(config);
    if portable_key_count > 0 {
        warnings.push(format!(
            "{portable_key_count} saved LAN token(s) are still stored in the per-user config fallback; diagnostics redact them, but native credential-store migration is recommended."
        ));
    }
    warnings
}

fn normalize_peer_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

fn peer_url_is_ready(url: &str) -> bool {
    let Ok(url) = Url::parse(url.trim()) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
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
    let destination_existed_before_install = destination.exists();
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
    if let Err(error) = platform_post_install(&destination) {
        if !destination_existed_before_install {
            fs::remove_file(&destination).with_context(|| {
                format!(
                    "platform registration failed and cleanup could not remove {}",
                    destination.display()
                )
            })?;
        }
        return Err(error).with_context(|| {
            format!(
                "platform registration failed after installing {}; rolled back newly installed file",
                destination.display()
            )
        });
    }
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

fn is_reportable_install_skip(error: &anyhow::Error) -> bool {
    let message = error.to_string();
    message.contains("system-font-conflict") || message.contains("unsupported-format")
}

fn managed_install_dir() -> Result<PathBuf> {
    managed_font_dir()
}

fn skip_platform_font_registration() -> bool {
    std::env::var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION").as_deref() == Ok("1")
}

fn platform_post_install(path: &Path) -> Result<()> {
    if skip_platform_font_registration() {
        let _ = path;
        return Ok(());
    }
    #[cfg(test)]
    if std::env::var("SYNCMYFONTS_FAIL_PLATFORM_POST_INSTALL").as_deref() == Ok("1") {
        let _ = path;
        bail!("simulated-platform-post-install-failure");
    }

    #[cfg(target_os = "windows")]
    {
        let value_name = windows_registry_value_name_for_font_path(path);
        let registry_path = path.to_string_lossy().to_string();
        add_windows_font_resource(path)?;
        let status = std::process::Command::new("reg")
            .args([
                "add",
                r"HKCU\Software\Microsoft\Windows NT\CurrentVersion\Fonts",
                "/v",
                &value_name,
                "/t",
                "REG_SZ",
                "/d",
                &registry_path,
                "/f",
            ])
            .status()
            .context("registering font in HKCU")?;
        if !status.success() {
            remove_windows_font_resource(path);
            notify_windows_font_change();
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
fn add_windows_font_resource(path: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Graphics::Gdi::AddFontResourceW;

    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide_path.push(0);
    let loaded_count = unsafe { AddFontResourceW(wide_path.as_ptr()) };
    if loaded_count == 0 {
        bail!("WindowsFontLoadFailed: AddFontResourceW loaded zero fonts");
    }
    Ok(())
}

#[cfg(target_os = "windows")]
fn remove_windows_font_resource(path: &Path) {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Graphics::Gdi::RemoveFontResourceW;

    let mut wide_path = path.as_os_str().encode_wide().collect::<Vec<_>>();
    wide_path.push(0);
    unsafe {
        RemoveFontResourceW(wide_path.as_ptr());
    }
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

fn normalized_pairing_code_is_ready(code: &str) -> bool {
    normalize_pairing_code(code).len() == 8
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

fn print_json_to_stderr<T: Serialize>(value: &T) -> Result<()> {
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{}", serde_json::to_string_pretty(value)?)?;
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
        <button onclick="openAppSupportFolder()">Open App Support</button>
        <button class="primary" onclick="syncAll(false)">Sync Saved Peers</button>
        <button onclick="syncAll(true)">Dry Run</button>
      </div>
    </section>

    <section>
      <h2>Saved LAN Peer</h2>
      <div class="grid">
        <label>Name <input id="peerName" placeholder="Workshop PC"></label>
        <label>URL <input id="peerUrl" placeholder="http://192.168.1.50:7370"></label>
        <label>Shared Key (optional) <input id="peerKey" type="password" placeholder="saved after pairing"></label>
        <label>Pairing Code <input id="pairingCode" placeholder="8 digits from sharing computer"></label>
      </div>
      <p class="row">
        <button onclick="discoverPeers()">Find LAN Peers</button>
        <button class="primary" onclick="pairPeer()">Pair Peer</button>
        <button onclick="testPeer()">Test Connection</button>
        <button onclick="syncPeer(true)">Preview From Peer</button>
        <button onclick="syncPeer(false)">Get Missing Fonts From Peer</button>
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
      <label>Shared Key (optional) <input id="shareKey" type="password" placeholder="blank creates pairing code"></label>
      </div>
      <p class="row">
        <button class="primary" onclick="startShare()">Share Fonts On This Network</button>
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
    async function openAppSupportFolder() {
      try {
        const result = await request('/api/support/open', { method: 'POST' });
        showResult(result);
        setNextStep('This folder contains SyncMyFonts config, saved peers, preferences, and managed manifest files.');
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
        setNextStep(`Connected. This peer reports ${result.peer_fonts} fonts. Use Preview From Peer to see what would happen, or Get Missing Fonts From Peer to install missing fonts.`);
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
        setNextStep(`${peer.name} is paired and saved. Click Test Connection or Preview From Peer next.`);
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
            ? `${wouldInstall} fonts are missing locally. Click Get Missing Fonts From Peer to install them.`
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

    fn spawn_short_lived_child_for_tests() -> Child {
        Command::new(std::env::current_exe().unwrap())
            .arg("--help")
            .spawn()
            .unwrap()
    }

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
    fn peer_url_ready_requires_absolute_http_url() {
        assert!(peer_url_is_ready("http://192.168.1.50:7370"));
        assert!(peer_url_is_ready(" https://syncmyfonts.local:7370 "));
        assert!(!peer_url_is_ready(""));
        assert!(!peer_url_is_ready("shop-pc:7370"));
        assert!(!peer_url_is_ready("/api/lan/v1/manifest"));
        assert!(!peer_url_is_ready("file:///tmp/font.ttf"));
    }

    #[test]
    fn normalized_peer_name_uses_url_when_name_is_blank() {
        assert_eq!(
            normalized_peer_name("  ", "http://192.168.1.50:7370"),
            "Peer 192.168.1.50:7370"
        );
        assert_eq!(
            normalized_peer_name("  Shop PC  ", "http://192.168.1.50:7370"),
            "Shop PC"
        );
    }

    #[test]
    fn add_lan_peer_replaces_existing_peer_by_normalized_url() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        add_lan_peer(
            "Shop PC".to_string(),
            "http://192.168.1.50:7370/".to_string(),
            Some("old-key".to_string()),
        )
        .unwrap();
        let peer = add_lan_peer(
            "Workshop PC".to_string(),
            " http://192.168.1.50:7370/// ".to_string(),
            Some("new-key".to_string()),
        )
        .unwrap();
        let config = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(peer.name, "Workshop PC");
        assert_eq!(peer.url, "http://192.168.1.50:7370");
        assert_eq!(config.peers.len(), 1);
        assert_eq!(config.peers[0].name, "Workshop PC");
        assert_eq!(config.peers[0].lan_key.as_deref(), Some("new-key"));
    }

    #[test]
    fn pairing_code_normalization_keeps_only_digits() {
        assert_eq!(normalize_pairing_code(" 1234-56 78 "), "12345678");
    }

    #[test]
    fn normalized_pairing_code_ready_requires_exactly_eight_digits() {
        assert!(normalized_pairing_code_is_ready("12345678"));
        assert!(normalized_pairing_code_is_ready("1234-5678"));
        assert!(!normalized_pairing_code_is_ready(""));
        assert!(!normalized_pairing_code_is_ready("abcd"));
        assert!(!normalized_pairing_code_is_ready("1234567"));
        assert!(!normalized_pairing_code_is_ready("123456789"));
    }

    #[test]
    fn pairing_code_validity_text_rounds_up_to_minutes() {
        assert_eq!(
            pairing_code_validity_text(Some(10 * 60)),
            "valid for about 10 minutes"
        );
        assert_eq!(
            pairing_code_validity_text(Some(61)),
            "valid for about 2 minutes"
        );
        assert_eq!(
            pairing_code_validity_text(Some(60)),
            "valid for about 1 minute"
        );
        assert_eq!(
            pairing_code_validity_text(None),
            "valid for about 10 minutes"
        );
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
    fn app_preferences_persist_and_clamp_auto_sync_interval() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        set_app_preferences(AppPreferences {
            auto_sync_saved_peers: true,
            auto_sync_interval_minutes: 10_000,
            lan_listen_address: " 0.0.0.0:7474 ".to_string(),
        })
        .unwrap();
        let loaded = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert!(loaded.preferences.auto_sync_saved_peers);
        assert_eq!(loaded.preferences.auto_sync_interval_minutes, 1440);
        assert_eq!(loaded.preferences.lan_listen_address, "0.0.0.0:7474");
    }

    #[test]
    fn lan_listen_preference_persists_after_valid_share_address() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        set_lan_listen_preference("127.0.0.1:7475".parse().unwrap()).unwrap();
        let loaded = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(loaded.preferences.lan_listen_address, "127.0.0.1:7475");
    }

    #[test]
    fn legacy_app_config_without_preferences_gets_defaults() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{
  "schema": 1,
  "device_id": null,
  "friendly_device_name": "Shop PC",
  "peers": []
}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        let loaded = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert!(!loaded.preferences.auto_sync_saved_peers);
        assert_eq!(loaded.preferences.auto_sync_interval_minutes, 15);
        assert_eq!(loaded.preferences.lan_listen_address, "0.0.0.0:7370");
    }

    #[test]
    fn partial_app_preferences_get_default_lan_listen_address() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        fs::create_dir_all(&config_dir).unwrap();
        fs::write(
            config_dir.join("config.json"),
            r#"{
  "schema": 1,
  "device_id": null,
  "friendly_device_name": "Shop PC",
  "preferences": {
    "auto_sync_saved_peers": true,
    "auto_sync_interval_minutes": 20
  },
  "peers": []
}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }

        let loaded = load_app_config().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert!(loaded.preferences.auto_sync_saved_peers);
        assert_eq!(loaded.preferences.auto_sync_interval_minutes, 20);
        assert_eq!(loaded.preferences.lan_listen_address, "0.0.0.0:7370");
    }

    #[test]
    fn gui_loads_selected_saved_peer_instead_of_first_peer() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::remove_var("SYNCMYFONTS_DEVICE_NAME");
        }
        save_app_config(&AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            friendly_device_name: None,
            preferences: AppPreferences::default(),
            peers: vec![
                LanPeerConfig {
                    name: "Office MacBook".to_string(),
                    url: "http://192.168.1.10:7370".to_string(),
                    lan_key_secret_id: None,
                    lan_key: Some("office-key".to_string()),
                },
                LanPeerConfig {
                    name: "Shop PC".to_string(),
                    url: "http://192.168.1.20:7370".to_string(),
                    lan_key_secret_id: None,
                    lan_key: Some("shop-key".to_string()),
                },
            ],
        })
        .unwrap();

        let mut app = SyncMyFontsGui::new();
        app.selected_peer_name = "Shop PC".to_string();
        app.load_selected_saved_peer_into_form();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(app.saved_peer_names, vec!["Office MacBook", "Shop PC"]);
        assert_eq!(app.peer_name, "Shop PC");
        assert_eq!(app.peer_url, "http://192.168.1.20:7370");
        assert_eq!(app.peer_key, "shop-key");
        assert!(!app.discovered_peer_requires_lan_key);
        assert!(app.next_step.contains("Test Connection"));
        assert!(
            app.next_step
                .contains("Preview From Peer before Get Missing Fonts From Peer")
        );
        assert!(app.next_step.contains("Loaded paired peer Shop PC"));
    }

    #[test]
    fn gui_loading_unpaired_saved_peer_prompts_pairing() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::remove_var("SYNCMYFONTS_DEVICE_NAME");
        }
        save_app_config(&AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            friendly_device_name: None,
            preferences: AppPreferences::default(),
            peers: vec![LanPeerConfig {
                name: "Shop PC".to_string(),
                url: "http://192.168.1.20:7370".to_string(),
                lan_key_secret_id: None,
                lan_key: None,
            }],
        })
        .unwrap();

        let mut app = SyncMyFontsGui::new();
        app.selected_peer_name = "Shop PC".to_string();
        app.load_selected_saved_peer_into_form();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(app.peer_name, "Shop PC");
        assert_eq!(app.peer_url, "http://192.168.1.20:7370");
        assert!(app.peer_key.is_empty());
        assert!(app.discovered_peer_requires_lan_key);
        assert!(app.next_step.contains("without a saved LAN token"));
        assert!(app.next_step.contains("click Pair Peer"));
        assert!(
            app.peer_pairing_detail()
                .contains("was discovered and requires pairing")
        );
    }

    #[test]
    fn gui_save_peer_next_step_distinguishes_pairing_state() {
        let paired = LanPeerConfig {
            name: "Shop PC".to_string(),
            url: "http://192.168.1.20:7370".to_string(),
            lan_key_secret_id: None,
            lan_key: Some("shop-key".to_string()),
        };
        let unpaired = LanPeerConfig {
            name: "Office MacBook".to_string(),
            url: "http://192.168.1.10:7370".to_string(),
            lan_key_secret_id: None,
            lan_key: None,
        };

        let paired_step = gui_save_peer_next_step(&paired);
        let unpaired_step = gui_save_peer_next_step(&unpaired);

        assert!(paired_step.contains("saved with a LAN token"));
        assert!(paired_step.contains("Use Sync Saved Peers later"));
        assert!(unpaired_step.contains("saved as a peer URL"));
        assert!(unpaired_step.contains("click Pair Peer"));
        assert!(unpaired_step.contains("before using saved-peer sync"));
    }

    #[test]
    fn saved_peer_summary_text_counts_paired_peers() {
        let empty = AppConfig::default();
        assert_eq!(saved_peer_summary_text(&empty), "Saved peers: none yet.");

        let mixed = AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            friendly_device_name: None,
            preferences: AppPreferences::default(),
            peers: vec![
                LanPeerConfig {
                    name: "Shop PC".to_string(),
                    url: "http://192.168.1.20:7370".to_string(),
                    lan_key_secret_id: None,
                    lan_key: Some("shop-key".to_string()),
                },
                LanPeerConfig {
                    name: "Office Mac".to_string(),
                    url: "http://192.168.1.10:7370".to_string(),
                    lan_key_secret_id: None,
                    lan_key: None,
                },
            ],
        };
        assert_eq!(
            saved_peer_summary_text(&mixed),
            "Saved peers: 2 saved, 1 paired (Shop PC, Office Mac)"
        );

        let paired = AppConfig {
            peers: vec![LanPeerConfig {
                name: "Shop PC".to_string(),
                url: "http://192.168.1.20:7370".to_string(),
                lan_key_secret_id: None,
                lan_key: Some("shop-key".to_string()),
            }],
            ..AppConfig::default()
        };
        assert_eq!(
            saved_peer_summary_text(&paired),
            "Saved peers: 1 paired (Shop PC)"
        );
    }

    #[test]
    fn gui_load_saved_peer_control_requires_saved_peers() {
        let mut app = SyncMyFontsGui::new();
        app.saved_peer_names.clear();
        assert!(!app.can_load_saved_peer());

        app.saved_peer_names.push("Shop PC".to_string());
        assert!(app.can_load_saved_peer());
    }

    #[test]
    fn gui_forget_peer_targets_selected_saved_peer_first() {
        let mut app = SyncMyFontsGui::new();
        app.saved_peer_names = vec!["Office MacBook".to_string(), "Shop PC".to_string()];
        app.selected_peer_name = "Shop PC".to_string();
        app.peer_name = "Office MacBook".to_string();

        assert_eq!(app.peer_to_forget_name(), "Shop PC");
        assert!(app.can_forget_peer());
        assert_eq!(app.forget_peer_button_label(), "Forget Shop PC");
    }

    #[test]
    fn gui_forget_peer_can_fall_back_to_typed_name() {
        let mut app = SyncMyFontsGui::new();
        app.saved_peer_names.clear();
        app.selected_peer_name.clear();
        app.peer_name = "Workshop laptop".to_string();

        assert_eq!(app.peer_to_forget_name(), "Workshop laptop");
        assert!(app.can_forget_peer());
        assert_eq!(app.forget_peer_button_label(), "Forget Workshop laptop");
    }

    #[test]
    fn gui_saved_peer_automation_requires_saved_peers_but_can_turn_off() {
        let mut app = SyncMyFontsGui::new();
        app.saved_peer_names.clear();
        app.saved_peer_key_count = 0;
        app.auto_sync_enabled = false;

        assert!(!app.can_enable_saved_peer_automation());
        assert!(!app.can_change_auto_sync_preference());
        assert!(app.saved_peer_sync_hint().is_some());

        app.auto_sync_enabled = true;
        assert!(app.can_change_auto_sync_preference());
        app.save_auto_sync_preferences();
        assert!(!app.auto_sync_enabled);
        assert!(app.next_step.contains("Pair a LAN peer"));

        app.saved_peer_names.push("Shop PC".to_string());
        assert!(!app.can_enable_saved_peer_automation());
        assert!(!app.can_change_auto_sync_preference());
        assert_eq!(
            app.saved_peer_sync_hint(),
            Some("Pair 1 saved peer(s) before using saved-peer sync or automation.".to_string())
        );

        app.saved_peer_key_count = 1;
        assert!(app.can_enable_saved_peer_automation());
        assert!(app.can_change_auto_sync_preference());
        assert!(app.saved_peer_sync_hint().is_none());
    }

    #[test]
    fn gui_last_action_guidance_respects_saved_peer_readiness() {
        let mut app = SyncMyFontsGui::new();
        app.saved_peer_names.clear();
        app.saved_peer_key_count = 0;
        let no_peers = app.last_action_success_next_step();
        assert!(no_peers.contains("Preview From Peer"));
        assert!(no_peers.contains("Pair a LAN peer"));
        assert!(!no_peers.contains("Sync Saved Peers"));

        app.saved_peer_names = vec!["Shop PC".to_string()];
        app.saved_peer_key_count = 0;
        let unpaired = app.last_action_success_next_step();
        assert!(unpaired.contains("Preview From Peer"));
        assert!(unpaired.contains("Pair 1 saved peer"));
        assert!(!unpaired.contains("Sync Saved Peers"));

        app.saved_peer_key_count = 1;
        let paired = app.last_action_success_next_step();
        assert!(paired.contains("Preview From Peer"));
        assert!(paired.contains("Sync Saved Peers"));
    }

    #[test]
    fn gui_self_test_initializes_native_gui_state_without_secrets() {
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

        let report = gui_self_test().unwrap();
        let json = serde_json::to_string(&report).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert!(report.ok);
        assert!(report.setup_phase.contains("Pairing mode"));
        assert!(report.role_card_text.contains("This computer"));
        assert!(report.role_card_text.contains("Other computer"));
        assert!(
            report
                .role_card_text
                .contains("Share Fonts On This Network")
        );
        assert_eq!(report.saved_peer_count, 0);
        assert_eq!(report.saved_peer_summary, "Saved peers: none yet.");
        assert!(!report.saved_peer_sync_ready);
        assert_eq!(
            report.saved_peer_sync_hint,
            Some("Pair a LAN peer before enabling saved-peer sync.".to_string())
        );
        assert!(!report.sign_in_sync_installed);
        assert_eq!(report.selected_peer_name, "");
        assert_eq!(report.listen, AppPreferences::default().lan_listen_address);
        assert!(report.listen_address_ready);
        assert!(report.listen_address_detail.contains("0.0.0.0:7370"));
        assert!(!report.peer_url_ready);
        assert!(!report.peer_pairing_ready);
        assert!(!report.peer_sync_ready);
        assert!(!report.peer_install_ready);
        assert!(report.can_find_lan_peers);
        assert!(!report.can_pair_peer);
        assert!(!report.can_test_peer);
        assert!(!report.can_preview_peer);
        assert!(!report.can_get_missing_fonts_from_peer);
        assert!(!report.can_save_peer);
        assert!(!report.can_load_saved_peer);
        assert!(!report.can_enable_saved_peer_automation);
        assert!(!report.can_change_auto_sync_preference);
        assert!(report.can_start_sharing);
        assert!(!report.can_stop_sharing);
        assert!(!report.can_forget_peer);
        assert!(report.peer_action_hint.contains("Find a LAN peer or paste"));
        assert!(report.peer_pairing_detail.contains("No peer selected yet"));
        assert_eq!(report.peer_key_label, "Shared Key (optional)");
        assert_eq!(report.share_key_label, "Shared Key (optional)");
        assert!(
            report
                .pairing_instructions_next_step
                .contains("Get Missing Fonts From Peer")
        );
        assert!(report.message.contains("Native GUI state initialized"));
        assert!(report.lan_sharing_guidance.contains("No port forwarding"));
        assert!(
            report
                .pre_share_guidance
                .contains("Only click Share Fonts On This Network")
        );
        assert!(
            report
                .manual_peer_fallback_guidance
                .contains("paste the sharing computer")
        );
        assert!(
            report
                .first_run_steps
                .iter()
                .any(|step| step.contains("Install Validation Font"))
        );
        assert!(
            report
                .first_run_steps
                .iter()
                .any(|step| step.contains("Preview From Peer"))
        );
        assert!(
            report
                .lan_readiness
                .iter()
                .any(|line| line.contains("Sharing: off"))
        );
        assert!(
            report
                .lan_readiness
                .iter()
                .any(|line| line.contains("Saved peers: none yet"))
        );
        assert!(
            report
                .lan_readiness
                .iter()
                .any(|line| line.contains("system fonts are excluded"))
        );
        assert!(
            report
                .lan_readiness
                .iter()
                .any(|line| line.contains("Sign-in sync: off"))
        );
        assert_eq!(report.sync_validation_matrix.len(), 2);
        assert!(
            report
                .sync_validation_matrix
                .iter()
                .any(|direction| direction.name == "macOS to Windows")
        );
        assert!(
            report
                .sync_validation_matrix
                .iter()
                .any(|direction| direction.name == "Windows to macOS")
        );
        assert!(
            report
                .validation_checklist_text
                .contains("SyncMyFonts LAN MVP validation")
        );
        assert!(
            report
                .validation_checklist_text
                .contains("macOS to Windows")
        );
        assert!(
            report
                .validation_checklist_text
                .contains("Windows to macOS")
        );
        assert!(
            report
                .setup_packet_text
                .contains("SyncMyFonts LAN setup packet")
        );
        assert!(report.setup_packet_text.contains("Proof checklist"));
        assert!(!json.contains("super-secret-lan-key"));
    }

    #[test]
    fn validation_checklist_text_summarizes_real_clean_machine_proof() {
        let checklist = validation_checklist_text();

        assert!(checklist.contains("current user"));
        assert!(checklist.contains("system fonts were not offered"));
        assert!(checklist.contains("macOS to Windows"));
        assert!(checklist.contains("Windows to macOS"));
        assert!(checklist.contains("Reopen the design app"));
    }

    #[test]
    fn pre_share_guidance_distinguishes_receiving_from_hosting() {
        let guidance = platform_pre_share_guidance();

        assert!(guidance.contains("Only click Share Fonts On This Network"));
        assert!(guidance.contains("Receiving fonts"));
        assert!(!guidance.contains("port forwarding"));
    }

    #[test]
    fn gui_first_run_steps_react_to_pairing_state() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }
        let mut app = SyncMyFontsGui::new();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        let initial_steps = app.first_run_steps();
        assert!(
            initial_steps
                .iter()
                .any(|step| step.contains("Shared Key blank"))
        );
        assert!(
            initial_steps
                .iter()
                .any(|step| step.contains("Find LAN Peers"))
        );
        assert!(platform_manual_peer_fallback_guidance().contains("manually"));
        assert!(app.setup_phase().contains("Pairing mode"));
        assert!(app.role_card_text().contains("Share Fonts On This Network"));
        assert_eq!(
            app.peer_action_hint(),
            "Find a LAN peer or paste the sharing computer's URL first."
        );
        assert!(app.peer_pairing_detail().contains("No peer selected yet"));
        assert!(!app.peer_pairing_ready());
        assert!(!app.peer_sync_ready());

        app.peer_name = "Shop PC".to_string();
        app.peer_url = "shop-pc".to_string();
        app.discovered_peer_requires_lan_key = true;
        app.pairing_code = "12345678".to_string();
        assert!(!app.peer_url_ready());
        assert!(!app.peer_pairing_ready());
        assert_eq!(
            app.peer_action_hint(),
            "Find a LAN peer or paste the sharing computer's URL first."
        );
        app.pairing_code.clear();

        app.peer_url = "http://192.168.1.25:7370".to_string();
        assert!(app.peer_url_ready());
        assert!(app.setup_phase().contains("enter the code"));
        assert!(app.role_card_text().contains("Pair Peer"));
        assert!(app.peer_action_hint().contains("Enter the pairing code"));
        assert!(
            app.peer_pairing_detail()
                .contains("was discovered and requires pairing")
        );
        assert!(!app.peer_pairing_ready());
        assert!(!app.peer_sync_ready());
        let loaded_peer_steps = app.first_run_steps();
        assert!(
            loaded_peer_steps
                .iter()
                .any(|step| step.contains("Enter its pairing code"))
        );

        app.pairing_code = "abcd".to_string();
        assert!(!app.peer_pairing_ready());
        assert!(app.peer_pairing_detail().contains("0/8 digit(s) entered"));

        app.pairing_code = "1234567".to_string();
        assert!(!app.peer_pairing_ready());
        assert!(app.peer_pairing_detail().contains("7/8 digit(s) entered"));

        app.pairing_code = "1234-5678".to_string();
        assert!(app.peer_pairing_ready());
        assert!(!app.peer_sync_ready());
        assert!(app.peer_action_hint().contains("Pair this peer"));
        assert!(
            app.peer_pairing_detail()
                .contains("Pairing saves a redacted LAN token")
        );
        app.pairing_code.clear();

        app.peer_key = "saved-token".to_string();
        assert!(app.setup_phase().contains("Preview mode"));
        assert!(app.role_card_text().contains("Get Missing Fonts From Peer"));
        assert!(app.peer_sync_ready());
        assert!(!app.peer_install_ready());
        assert!(app.peer_action_hint().contains("Preview this peer first"));
        assert!(app.peer_pairing_detail().contains("has a saved LAN token"));
        let keyed_steps = app.first_run_steps();
        assert!(
            keyed_steps
                .iter()
                .any(|step| step.contains("Peer details are filled in"))
        );

        app.last_previewed_peer = Some(previewed_peer_from_parts(
            "http://192.168.1.25:7370",
            Some("saved-token"),
        ));
        assert!(app.peer_install_ready());
        assert!(app.peer_action_hint().contains("Peer preview is current"));

        app.peer_key = "changed-token".to_string();
        assert!(!app.peer_install_ready());
        app.peer_key = "saved-token".to_string();
        assert!(app.peer_install_ready());

        app.peer_url = "http://192.168.1.26:7370".to_string();
        assert!(!app.peer_install_ready());

        app.saved_peer_names = vec!["Shop PC".to_string(), "Office Mac".to_string()];
        app.saved_peer_key_count = 1;
        let partially_paired_steps = app.first_run_steps();
        assert!(
            partially_paired_steps
                .iter()
                .any(|step| step.contains("Saved peer URLs"))
        );
        assert!(
            partially_paired_steps
                .iter()
                .any(|step| step.contains("Pair each saved peer"))
        );
        assert!(app.setup_phase().contains("Preview mode"));
        assert!(app.role_card_text().contains("Get Missing Fonts From Peer"));

        app.saved_peer_names = vec!["Shop PC".to_string()];
        app.saved_peer_key_count = 1;
        assert!(app.setup_phase().contains("Sync mode"));
        assert!(app.role_card_text().contains("opposite direction"));
    }

    #[test]
    fn gui_peer_install_requires_current_preview() {
        let mut app = SyncMyFontsGui::new();
        app.peer_url = "http://192.168.1.25:7370".to_string();
        app.peer_key = "saved-token".to_string();

        assert!(app.peer_sync_ready());
        assert!(!app.peer_install_ready());

        app.last_previewed_peer = Some(previewed_peer_from_parts(
            "http://192.168.1.25:7370",
            Some("saved-token"),
        ));
        assert!(app.peer_install_ready());

        app.peer_key = "different-token".to_string();
        assert!(!app.peer_install_ready());
        app.peer_key = "saved-token".to_string();
        assert!(app.peer_install_ready());

        app.peer_url = "http://192.168.1.26:7370".to_string();
        assert!(!app.peer_install_ready());
    }

    #[test]
    fn gui_lan_readiness_lines_track_share_pair_and_saved_peer_state() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }
        let mut app = SyncMyFontsGui::new();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        let first_run = app.lan_readiness_lines();
        assert!(first_run.iter().any(|line| line.contains("Sharing: off")));
        assert!(
            first_run
                .iter()
                .any(|line| line.contains("find a LAN peer"))
        );
        assert!(
            first_run
                .iter()
                .any(|line| line.contains("Saved peers: none yet"))
        );
        assert!(
            first_run
                .iter()
                .any(|line| line.contains("Automation: available after pairing a peer"))
        );
        assert!(
            first_run
                .iter()
                .any(|line| line.contains("Secrets: no saved LAN tokens yet"))
        );

        app.peer_url = "http://192.168.1.25:7370".to_string();
        app.pairing_code = "12345678".to_string();
        let ready_to_pair = app.lan_readiness_lines();
        assert!(
            ready_to_pair
                .iter()
                .any(|line| line.contains("Pair Peer is ready"))
        );

        app.saved_peer_names = vec!["Shop PC".to_string(), "Office Mac".to_string()];
        app.saved_peer_key_count = 1;
        let partially_paired = app.lan_readiness_lines();
        assert!(
            partially_paired
                .iter()
                .any(|line| line.contains("2 saved, 1 paired"))
        );
        assert!(
            partially_paired
                .iter()
                .any(|line| line.contains("pair saved peers before enabling repeat sync"))
        );

        app.pairing_code.clear();
        app.peer_key = "saved-token".to_string();
        app.saved_peer_names = vec!["Shop PC".to_string()];
        app.saved_peer_key_count = 1;
        let ready_to_sync = app.lan_readiness_lines();
        assert!(
            ready_to_sync
                .iter()
                .any(|line| line.contains("saved token is ready"))
        );
        assert!(
            ready_to_sync
                .iter()
                .any(|line| line.contains("1 ready (Shop PC)"))
        );
        assert!(
            ready_to_sync
                .iter()
                .any(|line| line.contains("1 saved LAN token"))
        );

        app.share = Some(RunningShare {
            child: spawn_short_lived_child_for_tests(),
            listen: "127.0.0.1:7370".parse().unwrap(),
        });
        app.share_urls = vec!["http://127.0.0.1:7370".to_string()];
        app.last_pairing_code = Some("87654321".to_string());
        let sharing = app.lan_readiness_lines();
        assert!(
            sharing
                .iter()
                .any(|line| line.contains("Sharing: on at http://127.0.0.1:7370"))
        );
        assert!(
            sharing
                .iter()
                .any(|line| line.contains("code 87654321 is ready"))
        );
    }

    #[test]
    fn gui_lan_readiness_text_is_copyable_summary() {
        let mut app = SyncMyFontsGui::new();
        app.peer_url = "http://192.168.1.25:7370".to_string();
        app.peer_key = "saved-token".to_string();
        app.saved_peer_names = vec!["Shop PC".to_string(), "Office Mac".to_string()];
        app.saved_peer_key_count = 2;
        app.auto_sync_enabled = true;
        app.auto_sync_interval_minutes = 30;

        let readiness = app.lan_readiness_text();

        assert!(readiness.contains("Sharing: off; no port forwarding is required."));
        assert!(readiness.contains("Pairing: saved token is ready; preview can run."));
        assert!(readiness.contains("Scope: current-user fonts only; system fonts are excluded."));
        assert!(readiness.contains("Saved peers: 2 ready (Shop PC, Office Mac)"));
        assert!(
            readiness
                .contains("Sign-in sync: off; enable it after a successful saved-peer preview.")
        );
        assert!(readiness.contains("Automation: auto-sync while app is open every 30 minute(s)."));
        assert!(readiness.contains("Secrets: 2 saved LAN token(s) are redacted"));
        assert_eq!(readiness.lines().count(), 7);
    }

    #[test]
    fn gui_setup_packet_bundles_role_readiness_and_validation() {
        let mut app = SyncMyFontsGui::new();
        app.device_name_input = "Office Mac".to_string();
        app.peer_url = "http://192.168.1.25:7370".to_string();
        app.peer_key = "saved-token".to_string();
        app.saved_peer_names = vec!["Shop PC".to_string()];
        app.saved_peer_key_count = 1;

        let packet = app.setup_packet_text();

        assert!(packet.contains("SyncMyFonts LAN setup packet"));
        assert!(packet.contains("Device: Office Mac"));
        assert!(packet.contains("Role card:"));
        assert!(packet.contains("Readiness:"));
        assert!(packet.contains("Saved peers: 1 ready (Shop PC)"));
        assert!(packet.contains("First sync steps:"));
        assert!(packet.contains("Proof checklist:"));
        assert!(packet.contains("SyncMyFonts LAN MVP validation"));
        assert!(!packet.contains("saved-token"));
    }

    #[test]
    fn gui_share_copy_receipts_are_visible_and_persisted() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }
        let mut app = SyncMyFontsGui::new();

        app.record_copy_url_receipt("http://192.168.1.10:7370");
        let url_history = load_app_history().unwrap();
        let url_action = url_history.last_action.unwrap();
        assert_eq!(url_action.action, "Copy LAN URL");
        assert_eq!(url_action.status, "success");
        assert_eq!(url_action.warning_count, 0);
        assert!(app.output.contains("http://192.168.1.10:7370"));
        assert!(app.next_step.contains("Paste it on the other computer"));

        app.record_copy_pairing_code_receipt("valid for about 10 minutes");
        let code_history = load_app_history().unwrap();
        let code_action = code_history.last_action.unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert_eq!(code_action.action, "Copy Pairing Code");
        assert_eq!(code_action.status, "success");
        assert_eq!(code_action.warning_count, 0);
        assert!(app.output.contains("Copied pairing code"));
        assert!(app.next_step.contains("valid for about 10 minutes"));
        assert!(!code_action.result.contains("12345678"));
    }

    #[test]
    fn gui_share_invitation_copies_pairing_instructions_without_custom_key() {
        let mut app = SyncMyFontsGui::new();
        app.device_name_input = "Office Mac".to_string();
        app.share_urls = vec!["http://192.168.1.10:7370".to_string()];
        app.last_pairing_code = Some("12345678".to_string());
        app.last_pairing_expires_seconds = Some(600);

        let invitation = app.share_invitation_text().unwrap();

        assert!(invitation.contains("SyncMyFonts LAN pairing"));
        assert!(invitation.contains("Sharing computer: Office Mac"));
        assert!(invitation.contains("URL: http://192.168.1.10:7370"));
        assert!(invitation.contains("Pairing code: 12345678"));
        assert!(invitation.contains("click Pair Peer"));
        assert!(invitation.contains("Preview From Peer"));
        assert!(invitation.contains("Get Missing Fonts From Peer"));
        assert!(!invitation.contains("Shared key:"));

        app.last_pairing_code = None;
        app.share_key = "super-secret-lan-key".to_string();
        let invitation = app.share_invitation_text().unwrap();
        assert!(invitation.contains("Shared key: use the key entered"));
        assert!(invitation.contains("Get Missing Fonts From Peer"));
        assert!(!invitation.contains("super-secret-lan-key"));
    }

    #[test]
    fn gui_role_card_explains_sharing_mode_for_the_other_computer() {
        let mut app = SyncMyFontsGui::new();
        app.share = Some(RunningShare {
            child: spawn_short_lived_child_for_tests(),
            listen: "127.0.0.1:7370".parse().unwrap(),
        });
        app.share_urls = vec!["http://127.0.0.1:7370".to_string()];
        app.last_pairing_code = Some("12345678".to_string());

        let role_card = app.role_card_text();
        assert!(role_card.contains("keep sharing on"));
        assert!(role_card.contains("http://127.0.0.1:7370"));
        assert!(role_card.contains("pairing code 12345678"));
        assert!(role_card.contains("Preview From Peer"));
    }

    #[test]
    fn gui_share_controls_follow_running_share_state() {
        let mut app = SyncMyFontsGui::new();
        assert!(app.can_start_sharing());
        assert!(!app.can_stop_sharing());
        assert!(app.listen_address_ready());
        assert!(app.listen_address_detail().contains("Use 0.0.0.0:7370"));

        app.listen = "not-a-socket".to_string();
        assert!(!app.can_start_sharing());
        assert!(!app.listen_address_ready());
        assert!(
            app.listen_address_detail()
                .contains("must look like 0.0.0.0:7370")
        );
        app.listen = "127.0.0.1:7370".to_string();
        assert!(app.can_start_sharing());

        app.share = Some(RunningShare {
            child: spawn_short_lived_child_for_tests(),
            listen: "127.0.0.1:7370".parse().unwrap(),
        });

        assert!(!app.can_start_sharing());
        assert!(app.can_stop_sharing());
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
    fn auto_sync_waits_for_saved_peers_and_idle_gui() {
        let now = Instant::now();

        assert!(!should_auto_sync_saved_peers(
            false, false, true, None, 15, now
        ));
        assert!(!should_auto_sync_saved_peers(
            true, true, true, None, 15, now
        ));
        assert!(!should_auto_sync_saved_peers(
            true, false, false, None, 15, now
        ));
        assert!(should_auto_sync_saved_peers(
            true, false, true, None, 15, now
        ));
    }

    #[test]
    fn auto_sync_respects_interval() {
        let now = Instant::now();
        assert!(!should_auto_sync_saved_peers(
            true,
            false,
            true,
            Some(now - Duration::from_secs(30)),
            1,
            now
        ));
        assert!(should_auto_sync_saved_peers(
            true,
            false,
            true,
            Some(now - Duration::from_secs(60)),
            1,
            now
        ));
    }

    #[test]
    fn gui_error_guidance_distinguishes_pairing_key_and_connectivity_failures() {
        assert!(
            gui_error_next_step(
                "LAN peer rejected pairing request: HTTP status client error (401 Unauthorized)"
            )
            .contains("pairing code was rejected")
        );
        assert!(
            gui_error_next_step(
                "LAN peer rejected manifest request: HTTP status client error (401 Unauthorized)"
            )
            .contains("shared key did not match")
        );
        assert!(
            gui_error_next_step(
                "fetching LAN peer manifest: error trying to connect: connection refused"
            )
            .contains("could not reach that peer")
        );
    }

    #[test]
    fn pairing_code_remaining_seconds_expires_after_ttl() {
        let started_at = Instant::now();
        assert_eq!(
            pairing_code_remaining_seconds(started_at, started_at + Duration::from_secs(1)),
            Some(PAIRING_CODE_TTL.as_secs() - 1)
        );
        assert_eq!(
            pairing_code_remaining_seconds(started_at, started_at + PAIRING_CODE_TTL),
            None
        );
        assert_eq!(
            pairing_code_remaining_seconds(
                started_at,
                started_at + PAIRING_CODE_TTL + Duration::from_secs(30)
            ),
            None
        );
    }

    #[test]
    fn gui_error_guidance_distinguishes_invalid_urls_and_share_ports() {
        assert!(
            gui_error_next_step("builder error: relative URL without a base")
                .contains("peer URL is invalid")
        );
        assert!(
            gui_error_next_step("LAN share did not answer at 127.0.0.1:7370")
                .contains("could not start on that port")
        );
        assert!(
            gui_error_next_step("invalid listen address: invalid socket address syntax")
                .contains("listen address is invalid")
        );
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
    fn validation_font_installs_as_user_source_font_not_managed() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let config_dir = root.join("config");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let bytes = b"syncmyfonts validation font bytes";
        let report = install_validation_font_bytes("https://example.test/font.ttf", bytes).unwrap();
        let default_scan = scan(false).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert!(report.path.starts_with(&user_dir));
        assert_eq!(fs::read(&report.path).unwrap(), bytes);
        assert!(report.file_name.starts_with("SyncMyFontsValidation"));
        assert!(
            default_scan
                .fonts
                .iter()
                .any(|font| font.path == report.path)
        );
        assert!(!config_dir.join("managed-fonts.json").exists());
    }

    #[test]
    fn install_font_rolls_back_new_file_when_platform_registration_fails() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let user_dir = root.join("user-fonts");
        let config_dir = root.join("config");
        let system_dir = root.join("system-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_dir);
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_SYSTEM_FONT_DIRS", &system_dir);
            std::env::set_var("SYNCMYFONTS_FAIL_PLATFORM_POST_INSTALL", "1");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        let bytes = b"syncmyfonts rollback font bytes";
        let sha256 = hex::encode(Sha256::digest(bytes));
        let error = install_font("Rollback.ttf", &sha256, bytes).unwrap_err();
        let managed_dir = managed_font_dir().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_SYSTEM_FONT_DIRS");
            std::env::remove_var("SYNCMYFONTS_FAIL_PLATFORM_POST_INSTALL");
        }

        let error_text = error.to_string();
        assert!(error_text.contains("platform registration failed"));
        assert!(error_text.contains("rolled back"));
        assert!(!managed_dir.join("Rollback.ttf").exists());
        assert!(!config_dir.join("managed-fonts.json").exists());
    }

    #[test]
    fn format_error_chain_includes_inner_causes() {
        let error = anyhow!("inner failure")
            .context("middle failure")
            .context("outer failure");

        assert_eq!(
            format_error_chain(&error),
            "outer failure: middle failure: inner failure"
        );
    }

    #[test]
    fn cli_error_report_is_machine_readable_and_guided() {
        let error = anyhow!("connection refused").context("contacting LAN peer http://127.0.0.1");

        let report = cli_error_report("lan-sync", &error);

        assert!(!report.ok);
        assert_eq!(report.command, "lan-sync");
        assert_eq!(report.message, "contacting LAN peer http://127.0.0.1");
        assert_eq!(report.causes.len(), 2);
        assert!(report.next_step.contains("could not reach that peer"));
    }

    #[test]
    fn cli_error_report_redacts_obvious_secret_tokens() {
        let error = anyhow!("invalid LAN key smf-super-secret")
            .context("request failed with api-key=abc123");

        let report = cli_error_report("lan-sync", &error);
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("[redacted]"));
        assert!(!json.contains("smf-super-secret"));
        assert!(!json.contains("api-key=abc123"));
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
    fn remove_file_if_exists_is_idempotent() {
        let path = std::env::temp_dir().join(format!("syncmyfonts-remove-{}", Uuid::new_v4()));
        fs::write(&path, b"temporary helper").unwrap();

        assert!(remove_file_if_exists(&path).unwrap());
        assert!(!remove_file_if_exists(&path).unwrap());
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
        let sign_in_helper = report
            .checks
            .iter()
            .find(|check| check.name == "sign-in-sync-helper")
            .expect("doctor should include sign-in sync helper guidance");
        assert!(sign_in_helper.ok);
        assert!(
            sign_in_helper
                .message
                .contains("Optional sign-in sync helper")
        );
        assert!(report.next_step.contains("Pair saved LAN peers"));
    }

    #[test]
    fn doctor_reports_saved_peers_without_pairing_tokens() {
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
            None,
        )
        .unwrap();

        let report = doctor().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert!(!report.ok);
        let saved_peers = report
            .checks
            .iter()
            .find(|check| check.name == "saved-peers")
            .expect("doctor should include saved-peer readiness");
        assert!(!saved_peers.ok);
        assert!(saved_peers.message.contains("still need pairing"));
        assert!(report.next_step.contains("Pair saved LAN peers"));
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
    fn doctor_includes_font_sync_scope_guidance() {
        let check = font_sync_scope_check();

        assert_eq!(check.name, "font-sync-scope");
        assert!(check.ok);
        assert!(check.message.contains("current-user fonts only"));
        assert!(
            check
                .message
                .contains("system font directories are excluded")
        );
    }

    #[test]
    fn sign_in_sync_readiness_distinguishes_complete_missing_and_partial_installs() {
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let helper = root.join("run-sign-in-sync.sh");
        let registration = root.join("com.syncmyfonts.signin-sync.plist");

        let missing = sign_in_sync_readiness_check_from_paths(&helper, &registration);
        assert_eq!(missing.name, "sign-in-sync-helper");
        assert!(missing.ok);
        assert!(
            missing
                .message
                .contains("Optional sign-in sync helper is not installed")
        );

        fs::create_dir_all(&root).unwrap();
        fs::write(&helper, b"helper").unwrap();
        let helper_only = sign_in_sync_readiness_check_from_paths(&helper, &registration);
        assert!(!helper_only.ok);
        assert!(helper_only.message.contains("registration is missing"));
        assert!(helper_only.message.contains("Disable Sign-In Sync"));

        fs::write(&registration, b"registration").unwrap();
        let complete = sign_in_sync_readiness_check_from_paths(&helper, &registration);
        assert!(complete.ok);
        assert!(complete.message.contains("Sign-in sync is installed"));

        fs::remove_file(&helper).unwrap();
        let registration_only = sign_in_sync_readiness_check_from_paths(&helper, &registration);
        assert!(!registration_only.ok);
        assert!(registration_only.message.contains("helper is missing"));
        assert!(registration_only.message.contains("Enable Sign-In Sync"));
    }

    #[test]
    fn windows_network_profile_check_warns_on_public_networks() {
        let categories =
            parse_windows_network_profile_categories("Public\r\nPrivate\r\npublic\r\n");
        let check = windows_network_profile_check_from_categories(&categories);

        assert_eq!(
            categories,
            vec!["Public".to_string(), "Private".to_string()]
        );
        assert_eq!(check.name, "windows-network-profile");
        assert!(!check.ok);
        assert!(check.message.contains("Public"));
        assert!(check.message.contains("switch the trusted LAN to Private"));
    }

    #[test]
    fn windows_network_profile_check_allows_private_or_domain_networks() {
        let categories = parse_windows_network_profile_categories("Private\nDomainAuthenticated\n");
        let check = windows_network_profile_check_from_categories(&categories);

        assert!(check.ok);
        assert!(check.message.contains("Private"));
        assert!(check.message.contains("DomainAuthenticated"));
        assert!(check.message.contains("trusted Private or domain networks"));
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
        assert_eq!(report.sync_validation_matrix.len(), 2);
        assert!(report.sync_validation_matrix.iter().any(|direction| {
            direction.name == "macOS to Windows"
                && direction
                    .target_evidence
                    .iter()
                    .any(|evidence| evidence.contains("Preview From Peer"))
        }));
        assert!(report.sync_validation_matrix.iter().any(|direction| {
            direction.name == "Windows to macOS"
                && direction
                    .pass_criteria
                    .iter()
                    .any(|criterion| criterion.contains("managed user font folder"))
        }));
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
    fn write_validation_report_saves_timestamped_json_file() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", root.join("config"));
            std::env::set_var("SYNCMYFONTS_LOG_DIR", root.join("logs"));
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", root.join("fonts"));
        }

        let saved = write_validation_report().unwrap();
        let file = fs::read_to_string(&saved.path).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert!(saved.path.starts_with(root.join("logs")));
        assert!(
            saved
                .path
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("validation-report-")
        );
        assert!(file.contains("\"manual_validation_steps\""));
        assert!(file.contains("\"sync_validation_matrix\""));
        assert!(file.contains("macOS to Windows"));
        assert!(file.contains("Windows to macOS"));
        assert!(file.contains("\"pass_criteria\""));
        assert!(saved.message.contains("Validation report saved"));
    }

    #[test]
    fn gui_network_errors_include_manual_peer_fallback() {
        let result = gui_error(anyhow!("simulated network failure"));

        assert!(result.next_step.contains("paste the sharing computer"));
        assert!(result.next_step.contains("manually"));
    }

    #[test]
    fn gui_single_peer_sync_explains_system_font_conflict_skips() {
        let report = LanSyncReport {
            known_local: 1,
            peer_fonts: 1,
            installed: Vec::new(),
            skipped: vec![
                "Existing.ttf system-font-conflict: Existing.ttf conflicts with /System/Library/Fonts/Existing.ttf"
                    .to_string(),
            ],
            dry_run: false,
        };

        let next_step = gui_single_peer_sync_next_step(&report, false);

        assert!(next_step.contains("system fonts"));
        assert!(next_step.contains("not installed"));
    }

    #[test]
    fn gui_saved_peer_sync_summarizes_installs_skips_and_peer_errors() {
        let report = LanSyncAllReport {
            dry_run: false,
            peers: vec![
                LanPeerSyncReport {
                    name: "Shop PC".to_string(),
                    url: "http://127.0.0.1:7370".to_string(),
                    ok: true,
                    installed: vec![PathBuf::from("Installed.ttf")],
                    skipped: vec!["WebFont.woff2 unsupported format".to_string()],
                    error: None,
                },
                LanPeerSyncReport {
                    name: "Offline MacBook".to_string(),
                    url: "http://127.0.0.1:7371".to_string(),
                    ok: false,
                    installed: Vec::new(),
                    skipped: Vec::new(),
                    error: Some("connection refused".to_string()),
                },
            ],
        };

        let next_step = gui_saved_peer_sync_next_step(&report, false);

        assert!(next_step.contains("Installed 1 font"));
        assert!(next_step.contains("unsupported font"));
        assert!(next_step.contains("could not be reached"));
    }

    #[test]
    fn gui_saved_peer_dry_run_reports_missing_installable_fonts() {
        let report = LanSyncAllReport {
            dry_run: true,
            peers: vec![LanPeerSyncReport {
                name: "Shop PC".to_string(),
                url: "http://127.0.0.1:7370".to_string(),
                ok: true,
                installed: Vec::new(),
                skipped: vec![
                    "would install Script.ttf".to_string(),
                    "Already.ttf already present".to_string(),
                ],
                error: None,
            }],
        };

        let next_step = gui_saved_peer_sync_next_step(&report, true);

        assert!(next_step.contains("1 missing installable font"));
    }

    #[test]
    fn gui_single_peer_result_summary_separates_preview_counts() {
        let report = LanSyncReport {
            known_local: 3,
            peer_fonts: 4,
            installed: Vec::new(),
            skipped: vec![
                "would install Script.ttf".to_string(),
                "Already.ttf already present".to_string(),
                "WebFont.woff2 unsupported format".to_string(),
            ],
            dry_run: true,
        };

        let summary = gui_single_peer_sync_result_summary(&report);

        assert!(summary.contains("1 missing installable"));
        assert!(summary.contains("1 already here"));
        assert!(summary.contains("1 skipped"));
        assert!(summary.contains("4 peer font"));
    }

    #[test]
    fn gui_saved_peer_result_summary_includes_installs_skips_and_failures() {
        let report = LanSyncAllReport {
            dry_run: false,
            peers: vec![
                LanPeerSyncReport {
                    name: "Shop PC".to_string(),
                    url: "http://127.0.0.1:7370".to_string(),
                    ok: true,
                    installed: vec![PathBuf::from("Installed.ttf")],
                    skipped: vec![
                        "Already.ttf already present".to_string(),
                        "System.ttf system-font-conflict: conflicts with system font".to_string(),
                    ],
                    error: None,
                },
                LanPeerSyncReport {
                    name: "Offline MacBook".to_string(),
                    url: "http://127.0.0.1:7371".to_string(),
                    ok: false,
                    installed: Vec::new(),
                    skipped: Vec::new(),
                    error: Some("connection refused".to_string()),
                },
            ],
        };

        let summary = gui_saved_peer_sync_result_summary(&report);

        assert!(summary.contains("1 installed"));
        assert!(summary.contains("1 already here"));
        assert!(summary.contains("1 skipped"));
        assert!(summary.contains("1 failed peer"));
        assert!(summary.contains("2 peer(s) checked"));
    }

    #[test]
    fn gui_single_peer_review_explains_skipped_font_reasons() {
        let report = LanSyncReport {
            known_local: 2,
            peer_fonts: 4,
            installed: vec![PathBuf::from("/tmp/Installed.ttf")],
            skipped: vec![
                "would install PreviewOnly.ttf".to_string(),
                "Already.ttf already present".to_string(),
                "WebFont.woff2 unsupported format".to_string(),
                "System.ttf system-font-conflict: conflicts with system font".to_string(),
            ],
            dry_run: false,
        };

        let review = gui_single_peer_sync_review(&report);

        assert!(review.contains("Get Missing Fonts From Peer review"));
        assert!(review.contains("Installed: /tmp/Installed.ttf"));
        assert!(review.contains("Would install: PreviewOnly.ttf"));
        assert!(review.contains("Already here"));
        assert!(review.contains("Unsupported format"));
        assert!(review.contains("System font conflict, not installed"));
    }

    #[test]
    fn gui_saved_peer_review_lists_peer_failures() {
        let report = LanSyncAllReport {
            dry_run: true,
            peers: vec![
                LanPeerSyncReport {
                    name: "Shop PC".to_string(),
                    url: "http://127.0.0.1:7370".to_string(),
                    ok: true,
                    installed: Vec::new(),
                    skipped: vec!["would install Script.ttf".to_string()],
                    error: None,
                },
                LanPeerSyncReport {
                    name: "Offline MacBook".to_string(),
                    url: "http://127.0.0.1:7371".to_string(),
                    ok: false,
                    installed: Vec::new(),
                    skipped: Vec::new(),
                    error: Some("connection refused".to_string()),
                },
            ],
        };

        let review = gui_saved_peer_sync_review(&report);

        assert!(review.contains("Saved peer preview review"));
        assert!(review.contains("Peer: Shop PC"));
        assert!(review.contains("Would install: Script.ttf"));
        assert!(review.contains("Peer: Offline MacBook"));
        assert!(review.contains("Failed: connection refused"));
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
            lan_key_secret_id: None,
            lan_key: Some("super-secret".to_string()),
        };

        let redacted = redacted_peer_config(&peer);
        let json = serde_json::to_string(&redacted).unwrap();

        assert!(json.contains("\"has_lan_key\":true"));
        assert!(!json.contains("super-secret"));
    }

    #[test]
    fn lan_peers_output_redacts_saved_key_material() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
        }
        save_app_config(&AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            friendly_device_name: None,
            preferences: AppPreferences::default(),
            peers: vec![LanPeerConfig {
                name: "Workshop".to_string(),
                url: "http://192.168.1.50:7370".to_string(),
                lan_key_secret_id: None,
                lan_key: Some("super-secret".to_string()),
            }],
        })
        .unwrap();

        let peers = redacted_lan_peers().unwrap();
        let json = serde_json::to_string(&peers).unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
        }

        assert_eq!(peers[0].name, "Workshop");
        assert!(peers[0].has_lan_key);
        assert_eq!(peers[0].key_storage, "portable-config-fallback");
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
                .warnings
                .iter()
                .any(|warning| warning.contains("saved LAN token"))
        );
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
        assert!(
            report
                .support_report_text
                .contains("Secret storage: 1 saved LAN token")
        );
        assert!(!report_json.contains("12345678"));
        assert!(!report.support_report_text.contains("12345678"));
        assert!(!report_json.contains("super-secret-lan-key"));
        assert!(!report.support_report_text.contains("super-secret-lan-key"));
    }

    #[test]
    fn gui_loads_last_action_summary_on_startup() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let config_dir = root.join("config");
        let log_dir = root.join("logs");
        let font_dir = root.join("fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_LOG_DIR", &log_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &font_dir);
        }
        record_action(
            "Preview From Peer",
            "success",
            1,
            "Preview found 2 missing installable fonts.",
        )
        .unwrap();

        let app = SyncMyFontsGui::new();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert!(
            app.last_result
                .contains("Last action: Preview From Peer success")
        );
        assert!(app.last_result.contains("warnings: 1"));
        assert_eq!(app.warning_count, 1);
        assert_eq!(app.output, "Preview found 2 missing installable fonts.");
        assert!(app.next_step.contains("Last action loaded"));
    }

    #[test]
    fn doctor_reports_portable_secret_storage_without_blocking_readiness() {
        let config = AppConfig {
            schema: 1,
            device_id: Some(Uuid::new_v4()),
            friendly_device_name: None,
            preferences: AppPreferences::default(),
            peers: vec![
                LanPeerConfig {
                    name: "Shop PC".to_string(),
                    url: "http://127.0.0.1:7370".to_string(),
                    lan_key_secret_id: None,
                    lan_key: Some("super-secret-lan-key".to_string()),
                },
                LanPeerConfig {
                    name: "Workshop Laptop".to_string(),
                    url: "http://127.0.0.1:7371".to_string(),
                    lan_key_secret_id: None,
                    lan_key: None,
                },
            ],
        };

        let check = secret_storage_check(&config);

        assert_eq!(check.name, "secret-storage");
        assert!(check.ok);
        assert!(check.message.contains("1 saved LAN token"));
        assert!(
            check
                .message
                .contains("per-user SyncMyFonts config fallback")
        );
        assert!(check.message.contains("native credential-store"));
        assert!(!check.message.contains("super-secret-lan-key"));
    }

    #[test]
    fn gui_diagnostics_result_carries_redacted_support_report() {
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
            "Secret Peer".to_string(),
            "http://127.0.0.1:7370".to_string(),
            Some("super-secret-lan-key".to_string()),
        )
        .unwrap();

        let report = diagnostics().unwrap();
        let gui = gui_diagnostics_result(&report, report.warnings.len());
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_LOG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        let support_report = gui.support_report.unwrap();
        assert!(support_report.contains("SyncMyFonts Support Report"));
        assert!(support_report.contains("Saved peer summary:"));
        assert!(!support_report.contains("super-secret-lan-key"));
        assert!(gui.output.contains("\"support_report_text\""));
        assert!(!gui.output.contains("super-secret-lan-key"));
    }

    #[test]
    fn gui_readiness_review_summarizes_passed_and_failed_checks() {
        let report = DoctorReport {
            ok: false,
            checks: vec![
                doctor_check("agent-binary", true, "Agent helper is available."),
                doctor_check(
                    "saved-peers",
                    false,
                    "No saved peers yet. Pair another computer.",
                ),
            ],
            next_step: "Pair saved LAN peers, then run Readiness Check again.".to_string(),
        };

        let summary = gui_readiness_result_summary(&report);
        let review = gui_readiness_review(&report);

        assert_eq!(
            summary,
            "Readiness: 1 check(s) passed; 1 check(s) need attention."
        );
        assert!(review.contains("SyncMyFonts readiness review"));
        assert!(review.contains("Next step: Pair saved LAN peers"));
        assert!(review.contains("- OK: agent-binary"));
        assert!(review.contains("- Needs attention: saved-peers"));
    }

    #[test]
    fn browser_surface_exposes_app_support_folder_action() {
        assert!(APP_HTML.contains("Open App Support"));
        assert!(APP_HTML.contains("openAppSupportFolder()"));
        assert!(APP_HTML.contains("/api/support/open"));
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
        let fonts_dir = config_dir.join("fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &fonts_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let managed_dir = managed_font_dir().unwrap();
        fs::create_dir_all(&fonts_dir).unwrap();
        fs::create_dir_all(&managed_dir).unwrap();
        let ok_path = managed_dir.join("ok.ttf");
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
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert_eq!(report.total, 3);
        assert_eq!(report.ok, 1);
        assert_eq!(report.missing.len(), 1);
        assert_eq!(report.modified.len(), 1);
        assert!(report.unreadable.is_empty());
        assert!(report.registration_issues.is_empty());
        assert_eq!(report.missing[0].file_name, "Missing.ttf");
        assert_eq!(report.modified[0].file_name, "Modified.ttf");
    }

    #[test]
    fn repair_managed_fonts_repairs_only_intact_records() {
        let _guard = ENV_LOCK.lock().unwrap();
        let config_dir = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let fonts_dir = config_dir.join("fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &fonts_dir);
            std::env::set_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION", "1");
        }

        let managed_dir = managed_font_dir().unwrap();
        fs::create_dir_all(&managed_dir).unwrap();
        let intact_path = managed_dir.join("intact.ttf");
        let intact_bytes = b"intact managed font";
        let intact_sha = hex::encode(Sha256::digest(intact_bytes));
        fs::write(&intact_path, intact_bytes).unwrap();
        let modified_path = managed_dir.join("modified.ttf");
        let modified_original_bytes = b"original repair font";
        let modified_sha = hex::encode(Sha256::digest(modified_original_bytes));
        fs::write(&modified_path, b"changed repair font").unwrap();
        let missing_path = managed_dir.join("missing.ttf");
        let manifest = ManagedManifest {
            schema: 1,
            installed: vec![
                ManagedFontRecord {
                    sha256: intact_sha,
                    file_name: "Intact.ttf".to_string(),
                    path: intact_path,
                    source: "lan:http://127.0.0.1:7370".to_string(),
                    installed_at: Utc::now().to_rfc3339(),
                    size_bytes: intact_bytes.len() as u64,
                },
                ManagedFontRecord {
                    sha256: modified_sha,
                    file_name: "Modified.ttf".to_string(),
                    path: modified_path,
                    source: "lan:http://127.0.0.1:7370".to_string(),
                    installed_at: Utc::now().to_rfc3339(),
                    size_bytes: modified_original_bytes.len() as u64,
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

        let report = repair_managed_fonts().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
            std::env::remove_var("SYNCMYFONTS_SKIP_PLATFORM_FONT_REGISTRATION");
        }

        assert_eq!(report.total, 3);
        assert_eq!(report.repaired.len(), 1);
        assert_eq!(report.repaired[0].file_name, "Intact.ttf");
        assert_eq!(report.skipped.len(), 2);
        assert!(report.failed.is_empty());
        assert!(
            report
                .skipped
                .iter()
                .any(|issue| issue.file_name == "Modified.ttf")
        );
        assert!(
            report
                .skipped
                .iter()
                .any(|issue| issue.file_name == "Missing.ttf")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_managed_fonts_reports_macos_records_outside_managed_folder() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let config_dir = root.join("config");
        let user_font_dir = root.join("user-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_font_dir);
        }

        let outside_path = user_font_dir.join("OutsideManaged.ttf");
        let bytes = b"outside managed folder";
        let sha = hex::encode(Sha256::digest(bytes));
        fs::create_dir_all(&user_font_dir).unwrap();
        fs::write(&outside_path, bytes).unwrap();
        let manifest = ManagedManifest {
            schema: 1,
            installed: vec![ManagedFontRecord {
                sha256: sha,
                file_name: "OutsideManaged.ttf".to_string(),
                path: outside_path,
                source: "lan:http://127.0.0.1:7370".to_string(),
                installed_at: Utc::now().to_rfc3339(),
                size_bytes: bytes.len() as u64,
            }],
        };
        save_managed_manifest(&manifest).unwrap();

        let report = verify_managed_fonts().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert_eq!(report.total, 1);
        assert_eq!(report.ok, 0);
        assert!(report.missing.is_empty());
        assert!(report.modified.is_empty());
        assert!(report.unreadable.is_empty());
        assert_eq!(report.registration_issues.len(), 1);
        assert!(report.registration_issues[0].message.contains("outside"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn verify_managed_fonts_reports_macos_unloadable_managed_font() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = std::env::temp_dir().join(format!("syncmyfonts-test-{}", Uuid::new_v4()));
        let config_dir = root.join("config");
        let user_font_dir = root.join("user-fonts");
        unsafe {
            std::env::set_var("SYNCMYFONTS_CONFIG_DIR", &config_dir);
            std::env::set_var("SYNCMYFONTS_USER_FONT_DIR", &user_font_dir);
        }

        let managed_dir = managed_font_dir().unwrap();
        let unloadable_path = managed_dir.join("Unloadable.ttf");
        let bytes = b"not actually a font";
        let sha = hex::encode(Sha256::digest(bytes));
        fs::create_dir_all(&managed_dir).unwrap();
        fs::write(&unloadable_path, bytes).unwrap();
        let manifest = ManagedManifest {
            schema: 1,
            installed: vec![ManagedFontRecord {
                sha256: sha,
                file_name: "Unloadable.ttf".to_string(),
                path: unloadable_path,
                source: "lan:http://127.0.0.1:7370".to_string(),
                installed_at: Utc::now().to_rfc3339(),
                size_bytes: bytes.len() as u64,
            }],
        };
        save_managed_manifest(&manifest).unwrap();

        let report = verify_managed_fonts().unwrap();
        unsafe {
            std::env::remove_var("SYNCMYFONTS_CONFIG_DIR");
            std::env::remove_var("SYNCMYFONTS_USER_FONT_DIR");
        }

        assert_eq!(report.total, 1);
        assert_eq!(report.ok, 0);
        assert!(report.missing.is_empty());
        assert!(report.modified.is_empty());
        assert!(report.unreadable.is_empty());
        assert_eq!(report.registration_issues.len(), 1);
        assert!(report.registration_issues[0].message.contains("CoreText"));
    }
}
