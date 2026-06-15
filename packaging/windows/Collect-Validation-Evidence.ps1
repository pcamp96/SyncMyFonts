param()

$ErrorActionPreference = "Stop"

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path

$Agent = Join-Path $ScriptDir "..\..\bin\syncmyfonts-agent.exe"
if (-not (Test-Path $Agent)) {
    $Agent = Join-Path $ScriptDir "bin\syncmyfonts-agent.exe"
}
if (-not (Test-Path $Agent)) {
    throw "Could not find bin\syncmyfonts-agent.exe next to this helper. Move it back into the SyncMyFonts release folder and try again."
}

$Gui = Join-Path $ScriptDir "..\..\bin\syncmyfonts-gui.exe"
if (-not (Test-Path $Gui)) {
    $Gui = Join-Path $ScriptDir "bin\syncmyfonts-gui.exe"
}

$Timestamp = (Get-Date).ToUniversalTime().ToString("yyyyMMdd-HHmmssZ")
$Desktop = [Environment]::GetFolderPath("Desktop")
if ([string]::IsNullOrWhiteSpace($Desktop)) {
    $Desktop = $env:USERPROFILE
}
$EvidenceDir = Join-Path $Desktop "SyncMyFonts-Evidence-$Timestamp"
New-Item -ItemType Directory -Force -Path $EvidenceDir | Out-Null

function Invoke-SyncMyFontsReport {
    param(
        [Parameter(Mandatory = $true)]
        [string]$Executable,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments,

        [Parameter(Mandatory = $true)]
        [string]$OutputPath
    )

    & $Executable @Arguments | Set-Content -Encoding UTF8 $OutputPath
    if ($LASTEXITCODE -ne 0) {
        throw "SyncMyFonts command failed: $Executable $($Arguments -join ' ')"
    }
}

Write-Host "Collecting SyncMyFonts launch and readiness evidence..."
Invoke-SyncMyFontsReport -Executable $Agent -Arguments @("diagnostics") -OutputPath (Join-Path $EvidenceDir "diagnostics.json")
Invoke-SyncMyFontsReport -Executable $Agent -Arguments @("doctor") -OutputPath (Join-Path $EvidenceDir "readiness-check.json")
Invoke-SyncMyFontsReport -Executable $Agent -Arguments @("validation-report", "--write") -OutputPath (Join-Path $EvidenceDir "validation-report-path.json")
if (Test-Path $Gui) {
    Invoke-SyncMyFontsReport -Executable $Gui -Arguments @("--self-test") -OutputPath (Join-Path $EvidenceDir "gui-self-test.json")
}

Set-Content -Path (Join-Path $EvidenceDir "README.txt") -Encoding UTF8 -Value @"
SyncMyFonts validation evidence

Collected: $Timestamp

Files:
- diagnostics.json: redacted support report and local paths.
- readiness-check.json: local app readiness checks.
- validation-report-path.json: path to the saved full validation report.
- gui-self-test.json: native GUI first-run state check, if the GUI binary was present.

Next:
1. Confirm the SyncMyFonts window opens.
2. Run Preview From Peer before Get Missing Fonts From Peer.
3. Keep this folder with the before/after clean-machine validation notes.
"@

Write-Host "Evidence saved to:"
Write-Host $EvidenceDir
Write-Host
Write-Host "Launching SyncMyFonts..."

if (Test-Path $Gui) {
    Start-Process -FilePath $Gui
} else {
    Start-Process -FilePath $Agent -ArgumentList @("gui")
}
