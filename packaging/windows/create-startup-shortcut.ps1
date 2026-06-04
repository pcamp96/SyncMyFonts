param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Sync", "Serve")]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$AgentPath,

    [Parameter(Mandatory = $true)]
    [string]$LanKey,

    [string]$Peer,
    [string]$Listen = "0.0.0.0:7370"
)

$ErrorActionPreference = "Stop"

$startup = [Environment]::GetFolderPath("Startup")
$configRoot = Join-Path $env:LOCALAPPDATA "SyncMyFonts"
$logRoot = Join-Path $configRoot "logs"
New-Item -ItemType Directory -Force -Path $configRoot, $logRoot | Out-Null

$envFile = Join-Path $configRoot "lan-startup.env.ps1"
Set-Content -Path $envFile -Encoding UTF8 -Value @"
`$env:SYNCMYFONTS_LAN_KEY = '$($LanKey.Replace("'", "''"))'
"@

$wrapperPath = Join-Path $configRoot "run-lan-$($Mode.ToLowerInvariant()).ps1"
if ($Mode -eq "Serve") {
    $agentArgs = @("lan-serve", "--listen", $Listen)
} else {
    if (-not [string]::IsNullOrWhiteSpace($Peer)) {
        & $AgentPath lan-add-peer --name "Startup Peer" --url $Peer --lan-key $LanKey | Out-Null
    }
    $agentArgs = @("lan-sync-all")
}
$quotedArgs = ($agentArgs | ForEach-Object { "'" + ($_.Replace("'", "''")) + "'" }) -join ", "
$stdoutPath = Join-Path $logRoot "lan-$($Mode.ToLowerInvariant()).log"
$stderrPath = Join-Path $logRoot "lan-$($Mode.ToLowerInvariant()).err.log"

Set-Content -Path $wrapperPath -Encoding UTF8 -Value @"
`$ErrorActionPreference = "Stop"
. '$($envFile.Replace("'", "''"))'
`$agent = '$($AgentPath.Replace("'", "''"))'
`$agentArgs = @($quotedArgs)
& `$agent @agentArgs >> '$($stdoutPath.Replace("'", "''"))' 2>> '$($stderrPath.Replace("'", "''"))'
"@

$shortcutPath = Join-Path $startup "SyncMyFonts LAN $Mode.lnk"
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = "powershell.exe"
$shortcut.Arguments = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$wrapperPath`""
$shortcut.WorkingDirectory = $configRoot
$shortcut.Save()

Write-Host "Created startup shortcut: $shortcutPath"
Write-Host "Wrapper: $wrapperPath"
Write-Host "Logs: $logRoot"
