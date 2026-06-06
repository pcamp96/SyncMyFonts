param(
    [string]$Configuration = "release"
)

$ErrorActionPreference = "Stop"

$RepoRoot = Resolve-Path (Join-Path $PSScriptRoot "../..")
$Version = "0.1.0"
$Metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
$AgentPackage = $Metadata.packages | Where-Object { $_.name -eq "syncmyfonts-agent" } | Select-Object -First 1
if ($AgentPackage -and $AgentPackage.version) {
    $Version = $AgentPackage.version
}

$DistRoot = Join-Path $RepoRoot "dist"
$DistDir = Join-Path $DistRoot "syncmyfonts-windows-$Version"
Remove-Item -Recurse -Force $DistDir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Join-Path $DistDir "bin"), (Join-Path $DistDir "packaging"), (Join-Path $DistDir "docs") | Out-Null

Push-Location $RepoRoot
try {
    cargo build --release -p syncmyfonts-agent --bins
} finally {
    Pop-Location
}

Copy-Item (Join-Path $RepoRoot "target/release/syncmyfonts-agent.exe") (Join-Path $DistDir "bin/")
Copy-Item (Join-Path $RepoRoot "target/release/syncmyfonts-gui.exe") (Join-Path $DistDir "bin/")
Copy-Item -Recurse (Join-Path $RepoRoot "packaging/windows") (Join-Path $DistDir "packaging/")
Copy-Item (Join-Path $RepoRoot "packaging/windows/Start-SyncMyFonts.cmd") $DistDir
Copy-Item (Join-Path $RepoRoot "README.md") $DistDir
Copy-Item (Join-Path $RepoRoot "docs/app-install.md") (Join-Path $DistDir "docs/")
Copy-Item (Join-Path $RepoRoot "docs/manual-clean-machine-validation.md") (Join-Path $DistDir "docs/")
if (Test-Path (Join-Path $RepoRoot "docs/desktop-app-surface.md")) {
    Copy-Item (Join-Path $RepoRoot "docs/desktop-app-surface.md") (Join-Path $DistDir "docs/")
}

Set-Content -Path (Join-Path $DistDir "START-HERE.txt") -Encoding UTF8 -Value @"
SyncMyFonts Windows MVP

1. Double-click:
   bin\syncmyfonts-gui.exe

   If Windows asks whether to run the app, choose to run it for this MVP build.
   You can also use:
   Start-SyncMyFonts.cmd

2. The native SyncMyFonts window should open. If it does not, run:
   .\bin\syncmyfonts-agent.exe gui

3. Click Readiness Check. The managed font folder should be under your user
   account, and no administrator prompt should appear.

4. Click Validation Report before and after a real two-computer sync test to
   save clean-machine evidence in the log folder.

5. If you need a safe non-system font for testing, click Install Validation
   Font. SyncMyFonts installs an OFL test font into your normal user font
   folder so the other computer has something legitimate to pull.

6. On the computer with fonts, click Share Fonts On LAN. Leave Shared Key blank
   for the easiest setup and copy the pairing code.

7. On the other computer, click Find LAN Peers, select the sharing computer,
   enter the pairing code, and click Pair Peer. Then use Preview From Peer or
   Get Missing Fonts.

8. Reopen your design apps if an installed font does not appear immediately.

9. To install startup helpers, click Enable Sign-In Sync after pairing peers,
   or see:
   packaging\windows\README.md

10. To add Start Menu launchers for SyncMyFonts, saved-peer sync, dry-run
   preview, diagnostics, and readiness, click Install App Shortcuts.

Validation:
- For a full Mac-to-Windows and Windows-to-Mac test pass, see:
  docs\app-install.md
  docs\manual-clean-machine-validation.md

Troubleshooting:
- Both computers must be on the same trusted LAN/VPN.
- If this Windows computer is sharing fonts, allow SyncMyFonts on Private
  networks when Windows Firewall asks.
- No port forwarding is needed.
- SyncMyFonts only syncs current-user fonts and fonts it installed itself. It
  does not copy system font folders.
"@

$Archive = Join-Path $DistRoot "syncmyfonts-windows-$Version.zip"
Remove-Item -Force $Archive -ErrorAction SilentlyContinue
Compress-Archive -Path $DistDir -DestinationPath $Archive
Write-Host "Created $Archive"
