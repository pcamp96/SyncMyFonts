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

2. Your browser should open automatically. If it does not, run:
   .\bin\syncmyfonts-agent.exe app
   Then open the printed
   localhost URL manually.

3. Use the app to Share Fonts On LAN, Test Peer, Preview From Peer, and Get
   Missing Fonts.

4. To install startup helpers, see:
   packaging\windows\README.md
"@

$Archive = Join-Path $DistRoot "syncmyfonts-windows-$Version.zip"
Remove-Item -Force $Archive -ErrorAction SilentlyContinue
Compress-Archive -Path $DistDir -DestinationPath $Archive
Write-Host "Created $Archive"
