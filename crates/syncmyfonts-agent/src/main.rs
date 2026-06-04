use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Parser, Subcommand};
use reqwest::blocking::{Client, multipart};
use serde::Serialize;
use sha2::{Digest, Sha256};
use syncmyfonts_core::{
    DEFAULT_API_KEY_HEADER, DeviceCheckInRequest, FontFormat, ManifestResponse,
    RegisterFontRequest, RegisterFontResponse,
};
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
    let client = Client::new();
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
    let client = Client::new();
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

fn authed(
    builder: reqwest::blocking::RequestBuilder,
    api_key: Option<&str>,
) -> reqwest::blocking::RequestBuilder {
    match api_key {
        Some(api_key) => builder.header(DEFAULT_API_KEY_HEADER, api_key),
        None => builder,
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
