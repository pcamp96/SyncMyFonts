# Release Build Notes

The MVP release artifact is a portable archive containing the
`syncmyfonts-agent` binary, a native GUI launcher, platform launcher helpers,
and app install docs. It is not yet a signed installer, notarized app, DMG,
MSI, or MSIX.

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

The portable releases include app-style entry points:

```text
macOS:   SyncMyFonts.app
Windows: bin\syncmyfonts-gui.exe
```

The fallback command remains:

```text
syncmyfonts-agent gui
```

Both paths launch the native SyncMyFonts GUI. It can scan fonts, start LAN
sharing, show a copyable LAN URL and pairing code, discover sharing LAN peers,
pair with a peer, test a LAN peer, preview missing fonts, install missing
fonts, save LAN peers, sync all saved peers, stop LAN sharing, verify managed
font installs, set a friendly device name, open managed/log folders, and
produce a redacted diagnostics report. It can also install per-user sign-in
sync for saved peers and run a local readiness check before live two-machine
testing. If more than one peer is saved, the GUI lets the user choose which
saved computer to load before testing, previewing, syncing, or forgetting it.
The result panel can copy the current result and the latest redacted support
report for clean-machine validation or troubleshooting.

The browser control surface remains available through `syncmyfonts-agent app`
for development and future self-hosted/server-adjacent workflows.

The release folder's `START-HERE.txt` also calls out the common LAN setup
checks: both computers need to be on the same trusted LAN/VPN, Windows sharing
hosts should allow SyncMyFonts on Private networks if Firewall prompts, and no
port forwarding is needed.

## Trust And Installer Status

Current artifacts are meant for MVP testing:

- macOS: portable `.tar.gz`; no notarized `.app` or DMG yet.
- Windows: portable `.zip`; no MSI/MSIX or code-signed installer yet.

The GitHub Actions build proves that the Rust workspace builds/tests on macOS
and Windows, that portable archives are produced, that the app-style launchers
are present, and that the packaged agent inside each archive can run
`diagnostics`, `doctor`, and a loopback LAN pairing plus saved-peer sync smoke
with isolated per-user paths. It does not replace a clean-machine GUI launch
test, code signing, notarization, or installer QA.
