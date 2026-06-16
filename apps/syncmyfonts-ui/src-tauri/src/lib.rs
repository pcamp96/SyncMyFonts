use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Serialize)]
struct AppSnapshot {
    device_name: String,
    platform: String,
    sharing: bool,
    saved_peers: usize,
    paired_peers: usize,
    warnings: usize,
    system_fonts_excluded: bool,
    mode: String,
    auto_sync_saved_peers: bool,
    auto_sync_interval_minutes: u64,
    lan_listen_address: String,
    config_path: String,
    log_dir: String,
    user_font_dir: String,
    managed_font_dir: String,
    managed_manifest_count: usize,
    user_font_count: usize,
    peers: Vec<PeerSnapshot>,
}

#[derive(Serialize)]
struct PeerSnapshot {
    name: String,
    url: String,
    paired: bool,
}

#[derive(Default, Deserialize)]
struct AppConfig {
    friendly_device_name: Option<String>,
    #[serde(default)]
    preferences: AppPreferences,
    #[serde(default)]
    peers: Vec<LanPeerConfig>,
}

#[derive(Deserialize)]
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

#[derive(Deserialize)]
struct LanPeerConfig {
    name: String,
    url: String,
    #[serde(default)]
    lan_key_secret_id: Option<String>,
    #[serde(default)]
    lan_key: Option<String>,
}

#[derive(Default, Deserialize)]
struct ManagedManifest {
    #[serde(default)]
    installed: Vec<serde_json::Value>,
}

#[tauri::command]
fn app_snapshot() -> AppSnapshot {
    let config = load_app_config().unwrap_or_default();
    let peers = config
        .peers
        .iter()
        .map(|peer| PeerSnapshot {
            name: peer.name.clone(),
            url: peer.url.clone(),
            paired: peer.lan_key.is_some() || peer.lan_key_secret_id.is_some(),
        })
        .collect::<Vec<_>>();
    let paired_peers = peers.iter().filter(|peer| peer.paired).count();
    let managed_manifest = load_managed_manifest().unwrap_or_default();
    let app_data_dir = app_data_dir();
    let log_dir = app_log_dir();
    let user_font_dir = user_font_dir();
    let managed_font_dir = app_data_dir
        .clone()
        .map(|path| path.join("fonts"))
        .or_else(|| user_font_dir.clone());
    let device_name = config
        .friendly_device_name
        .clone()
        .unwrap_or_else(hostname);

    AppSnapshot {
        device_name,
        platform: std::env::consts::OS.to_string(),
        sharing: false,
        saved_peers: peers.len(),
        paired_peers,
        warnings: 0,
        system_fonts_excluded: true,
        mode: "LAN sync".to_string(),
        auto_sync_saved_peers: config.preferences.auto_sync_saved_peers,
        auto_sync_interval_minutes: config.preferences.auto_sync_interval_minutes.clamp(1, 1440),
        lan_listen_address: config.preferences.lan_listen_address,
        config_path: display_path(app_data_dir.clone().map(|path| path.join("config.json"))),
        log_dir: display_path(log_dir),
        user_font_dir: display_path(user_font_dir.clone()),
        managed_font_dir: display_path(managed_font_dir),
        managed_manifest_count: managed_manifest.installed.len(),
        user_font_count: user_font_dir
            .as_ref()
            .map(|path| count_font_files(path))
            .unwrap_or(0),
        peers,
    }
}

fn default_auto_sync_interval_minutes() -> u64 {
    15
}

fn default_lan_listen_address() -> String {
    "0.0.0.0:7370".to_string()
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "This computer".to_string())
}

fn load_app_config() -> Option<AppConfig> {
    let path = app_data_dir()?.join("config.json");
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn load_managed_manifest() -> Option<ManagedManifest> {
    let path = app_data_dir()?.join("managed-fonts.json");
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn app_data_dir() -> Option<PathBuf> {
    if let Ok(config_dir) = std::env::var("SYNCMYFONTS_CONFIG_DIR") {
        return Some(PathBuf::from(config_dir));
    }

    #[cfg(target_os = "macos")]
    {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        return Some(home.join("Library/Application Support/SyncMyFonts"));
    }

    #[cfg(target_os = "windows")]
    {
        let base = directories::BaseDirs::new()?;
        return Some(base.data_local_dir().join("SyncMyFonts"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let base = directories::BaseDirs::new()?;
        Some(base.config_dir().join("syncmyfonts"))
    }
}

fn app_log_dir() -> Option<PathBuf> {
    if let Ok(log_dir) = std::env::var("SYNCMYFONTS_LOG_DIR") {
        return Some(PathBuf::from(log_dir));
    }

    #[cfg(target_os = "macos")]
    {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        return Some(home.join("Library/Logs/SyncMyFonts"));
    }

    #[cfg(target_os = "windows")]
    {
        return Some(app_data_dir()?.join("logs"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        Some(app_data_dir()?.join("logs"))
    }
}

fn user_font_dir() -> Option<PathBuf> {
    if let Ok(font_dir) = std::env::var("SYNCMYFONTS_USER_FONT_DIR") {
        return Some(PathBuf::from(font_dir));
    }

    #[cfg(target_os = "macos")]
    {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        return Some(home.join("Library/Fonts"));
    }

    #[cfg(target_os = "windows")]
    {
        let local_app_data = std::env::var("LOCALAPPDATA").ok().map(PathBuf::from)?;
        return Some(local_app_data.join("Microsoft/Windows/Fonts"));
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let home = directories::UserDirs::new()?.home_dir().to_path_buf();
        Some(home.join(".local/share/fonts"))
    }
}

fn count_font_files(path: &PathBuf) -> usize {
    let Ok(entries) = fs::read_dir(path) else {
        return 0;
    };

    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .map(|path| {
            if path.is_dir() {
                count_font_files(&path)
            } else if is_font_path(&path) {
                1
            } else {
                0
            }
        })
        .sum()
}

fn is_font_path(path: &PathBuf) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "otf" | "ttf" | "ttc" | "otc"
            )
        })
        .unwrap_or(false)
}

fn display_path(path: Option<PathBuf>) -> String {
    path.map(|path| path.display().to_string())
        .unwrap_or_else(|| "Unavailable".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_snapshot])
        .run(tauri::generate_context!())
        .expect("error while running SyncMyFonts UI");
}
