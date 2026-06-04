param(
    [string]$TaskName = "SyncMyFonts LAN Sync",
    [switch]$RemoveGeneratedFiles
)

$ErrorActionPreference = "Stop"

if (Get-ScheduledTask -TaskName $TaskName -ErrorAction SilentlyContinue) {
    Unregister-ScheduledTask -TaskName $TaskName -Confirm:$false
    Write-Host "Removed scheduled task: $TaskName"
} else {
    Write-Host "Scheduled task not found: $TaskName"
}

if ($RemoveGeneratedFiles) {
    $configRoot = Join-Path $env:LOCALAPPDATA "SyncMyFonts"
    Remove-Item -Force -ErrorAction SilentlyContinue `
        (Join-Path $configRoot "lan-startup.env.ps1"),
        (Join-Path $configRoot "run-lan-sync.ps1"),
        (Join-Path $configRoot "run-lan-serve.ps1")
    Write-Host "Removed generated SyncMyFonts startup helper files."
}
