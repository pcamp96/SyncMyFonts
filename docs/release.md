# Release Build Notes

The MVP release artifact is a folder containing the `syncmyfonts-agent` binary,
platform launcher helpers, and app install docs.

## macOS

Build on macOS:

```sh
scripts/release/build-macos.sh
```

Output:

```text
dist/syncmyfonts-macos-<version>.tar.gz
```

## Windows

Build on Windows PowerShell:

```powershell
.\scripts\release\build-windows.ps1
```

Output:

```text
dist\syncmyfonts-windows-<version>.zip
```

## MVP App Entry Point

Both release folders start from the same command:

```text
syncmyfonts-agent gui
```

That command launches the native SyncMyFonts GUI. It can scan fonts, start LAN
sharing, show a copyable LAN URL and pairing code, discover sharing LAN peers,
pair with a peer, test a LAN peer, preview missing fonts, install missing
fonts, save LAN peers, sync all saved peers, stop LAN sharing, verify managed
font installs, and produce a redacted diagnostics report.

The browser control surface remains available through `syncmyfonts-agent app`
for development and future self-hosted/server-adjacent workflows.

The release folder's `START-HERE.txt` also calls out the common LAN setup
checks: both computers need to be on the same trusted LAN/VPN, Windows sharing
hosts should allow SyncMyFonts on Private networks if Firewall prompts, and no
port forwarding is needed.
