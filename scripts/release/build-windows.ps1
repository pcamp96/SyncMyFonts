param(
    [string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "../..")
$CargoToml = Join-Path $RepoRoot "crates/syncmyfonts-agent/Cargo.toml"
$VersionLine = Select-String -Path $CargoToml -Pattern '^version' -ErrorAction SilentlyContinue | Select-Object -First 1
$Version = "0.1.0"
if ($VersionLine) {
    $Version = ($VersionLine.Line -replace '.*"([^"]+)".*', '$1')
}

$DistRoot = Join-Path $RepoRoot "dist"
$DistDir = Join-Path $DistRoot "syncmyfonts-windows-$Version"
Remove-Item -Recurse -Force $DistDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $DistDir "bin"), (Join-Path $DistDir "packaging"), (Join-Path $DistDir "docs") | Out-Null

Push-Location $RepoRoot
try {
    cargo build --release -p syncmyfonts-agent
} finally {
    Pop-Location
}

Copy-Item (Join-Path $RepoRoot "target/release/syncmyfonts-agent.exe") (Join-Path $DistDir "bin/")
Copy-Item -Recurse (Join-Path $RepoRoot "packaging/windows") (Join-Path $DistDir "packaging/")
Copy-Item (Join-Path $RepoRoot "packaging/windows/Start-SyncMyFonts.cmd") $DistDir
Copy-Item (Join-Path $RepoRoot "README.md") $DistDir
Copy-Item (Join-Path $RepoRoot "docs/app-install.md") (Join-Path $DistDir "docs/")
if (Test-Path (Join-Path $RepoRoot "docs/desktop-app-surface.md")) {
    Copy-Item (Join-Path $RepoRoot "docs/desktop-app-surface.md") (Join-Path $DistDir "docs/")
}

Set-Content -Path (Join-Path $DistDir "START-HERE.txt") -Encoding UTF8 -Value @"
SyncMyFonts Windows MVP

1. Double-click:
   Start-SyncMyFonts.cmd

2. The native SyncMyFonts window should open. If it does not, run:
   .\bin\syncmyfonts-agent.exe gui

3. On the computer with fonts, click Share Fonts On LAN. Leave Shared Key blank
   for the easiest setup and copy the pairing code.

4. On the other computer, click Find LAN Peers, select the sharing computer,
   enter the pairing code, and click Pair Peer. Then use Preview From Peer or
   Get Missing Fonts.

5. To install startup helpers, see:
   packaging\windows\README.md

Troubleshooting:
- Both computers must be on the same trusted LAN/VPN.
- If this Windows computer is sharing fonts, allow SyncMyFonts on Private
  networks when Windows Firewall asks.
- No port forwarding is needed.
"@

$Archive = Join-Path $DistRoot "syncmyfonts-windows-$Version.zip"
Remove-Item -Force $Archive -ErrorAction SilentlyContinue
Compress-Archive -Path $DistDir -DestinationPath $Archive
Write-Host "Created $Archive"
