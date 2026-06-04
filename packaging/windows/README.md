# Windows Launcher Artifacts

These PowerShell helpers run `syncmyfonts-agent` for the signed-in user. They do
not install a Windows service and do not require administrator rights.

## Scheduled Task

Use this path when there is no tray app yet and you want sync at sign-in plus a
repeat interval.

```powershell
.\packaging\windows\install-startup-task.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\target\release\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370" `
  -RepeatHours 4
```

To host fonts from this Windows account:

```powershell
.\packaging\windows\install-startup-task.ps1 `
  -Mode Serve `
  -AgentPath "$PWD\target\release\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key"
```

## Startup Folder Shortcut

Use this path for the lightest MVP shape:

```powershell
.\packaging\windows\create-startup-shortcut.ps1 `
  -Mode Sync `
  -AgentPath "$PWD\target\release\syncmyfonts-agent.exe" `
  -LanKey "choose-a-shared-key" `
  -Peer "http://192.168.1.50:7370"
```

## Uninstall

```powershell
.\packaging\windows\uninstall-startup-task.ps1 -TaskName "SyncMyFonts LAN Sync" -RemoveGeneratedFiles
.\packaging\windows\uninstall-startup-task.ps1 -TaskName "SyncMyFonts LAN Serve" -RemoveGeneratedFiles
```

Startup shortcuts can be removed from:

```text
%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup
```

Generated wrappers and logs live under `%LOCALAPPDATA%\SyncMyFonts`.

For a production app, store the LAN key in Windows Credential Manager and let a
tray/settings UI create the task or shortcut.
