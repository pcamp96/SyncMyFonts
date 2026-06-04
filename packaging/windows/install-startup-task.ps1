param(
    [Parameter(Mandatory = $true)]
    [ValidateSet("Sync", "Serve")]
    [string]$Mode,

    [Parameter(Mandatory = $true)]
    [string]$AgentPath,

    [Parameter(Mandatory = $true)]
    [string]$LanKey,

    [string]$Peer,
    [string]$Listen = "0.0.0.0:7370",
    [int]$RepeatHours = 4,
    [string]$TaskName
)

$ErrorActionPreference = "Stop"

if (-not (Test-Path $AgentPath)) {
    throw "AgentPath does not exist: $AgentPath"
}

if (-not $TaskName) {
    $TaskName = "SyncMyFonts LAN $Mode"
}

$configRoot = Join-Path $env:LOCALAPPDATA "SyncMyFonts"
$logRoot = Join-Path $configRoot "logs"
New-Item -ItemType Directory -Force -Path $configRoot, $logRoot | Out-Null

$envFile = Join-Path $configRoot "lan-startup.env.ps1"
Set-Content -Path $envFile -Encoding UTF8 -Value @"
`$env:SYNCMYFONTS_LAN_KEY = '$($LanKey.Replace("'", "''"))'
"@

$wrapperPath = Join-Path $configRoot "run-lan-$($Mode.ToLowerInvariant()).ps1"
$stdoutPath = Join-Path $logRoot "lan-$($Mode.ToLowerInvariant()).log"
$stderrPath = Join-Path $logRoot "lan-$($Mode.ToLowerInvariant()).err.log"

if ($Mode -eq "Serve") {
    $agentArgs = @("lan-serve", "--listen", $Listen)
} else {
    if (-not [string]::IsNullOrWhiteSpace($Peer)) {
        & $AgentPath lan-add-peer --name "Scheduled Peer" --url $Peer --lan-key $LanKey | Out-Null
    }
    $agentArgs = @("lan-sync-all")
}

$quotedArgs = ($agentArgs | ForEach-Object { "'" + ($_.Replace("'", "''")) + "'" }) -join ", "

Set-Content -Path $wrapperPath -Encoding UTF8 -Value @"
`$ErrorActionPreference = "Stop"
. '$($envFile.Replace("'", "''"))'
`$agent = '$($AgentPath.Replace("'", "''"))'
`$agentArgs = @($quotedArgs)
& `$agent @agentArgs >> '$($stdoutPath.Replace("'", "''"))' 2>> '$($stderrPath.Replace("'", "''"))'
"@

$taskAction = New-ScheduledTaskAction `
    -Execute "powershell.exe" `
    -Argument "-NoProfile -ExecutionPolicy Bypass -File `"$wrapperPath`""

$logonTrigger = New-ScheduledTaskTrigger -AtLogOn -User $env:USERNAME

$triggers = @($logonTrigger)
if ($Mode -eq "Sync" -and $RepeatHours -gt 0) {
    $repeatTrigger = New-ScheduledTaskTrigger -Once -At (Get-Date).Date.AddMinutes(5) `
        -RepetitionInterval (New-TimeSpan -Hours $RepeatHours) `
        -RepetitionDuration (New-TimeSpan -Days 3650)
    $triggers += $repeatTrigger
}

$settings = New-ScheduledTaskSettingsSet `
    -AllowStartIfOnBatteries `
    -DontStopIfGoingOnBatteries `
    -StartWhenAvailable `
    -MultipleInstances IgnoreNew

Register-ScheduledTask `
    -TaskName $TaskName `
    -Action $taskAction `
    -Trigger $triggers `
    -Settings $settings `
    -Description "Runs SyncMyFonts $Mode for the signed-in user." `
    -Force | Out-Null

Write-Host "Installed scheduled task: $TaskName"
Write-Host "Wrapper: $wrapperPath"
Write-Host "Logs: $logRoot"
