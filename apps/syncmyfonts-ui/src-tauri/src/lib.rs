use serde::Serialize;

#[derive(Serialize)]
struct AppSnapshot {
    device_name: String,
    platform: String,
    sharing: bool,
    saved_peers: u8,
    warnings: u8,
    system_fonts_excluded: bool,
    mode: String,
}

#[tauri::command]
fn app_snapshot() -> AppSnapshot {
    AppSnapshot {
        device_name: hostname(),
        platform: std::env::consts::OS.to_string(),
        sharing: false,
        saved_peers: 0,
        warnings: 0,
        system_fonts_excluded: true,
        mode: "LAN sync".to_string(),
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "This computer".to_string())
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .invoke_handler(tauri::generate_handler![app_snapshot])
        .run(tauri::generate_context!())
        .expect("error while running SyncMyFonts UI");
}
