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
fonts, save LAN peers, dry-run syncs, sync all saved peers, start LAN sharing,
stop LAN sharing, and produce a redacted diagnostics report.
