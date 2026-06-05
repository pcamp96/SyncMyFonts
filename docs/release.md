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
syncmyfonts-agent app
```

That command launches the local browser-based control surface. It can scan
fonts, start LAN sharing, show a copyable LAN URL and pairing code, discover
sharing LAN peers, pair with a peer, test a LAN peer, preview missing fonts,
install missing fonts, save LAN peers, sync all saved peers, stop LAN sharing,
and produce a redacted diagnostics report.
