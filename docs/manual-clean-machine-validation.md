# Manual Clean-Machine Validation

Use this checklist to prove the portable app works on real macOS and Windows
machines, not only in CI. The goal is to validate the native GUI, LAN pairing,
system-font exclusion, and current-user installs on two normal computers.

## Test Setup

- macOS computer:
- Windows computer:
- Shared network or VPN:
- Release artifact or GitHub Actions run:
- macOS archive:
- Windows archive:
- Tester:
- Date:

Install or extract the matching release archive on each computer. Do not run
with administrator privileges.

## Before Sync

On both computers:

1. Launch the native app.
   - macOS: open `SyncMyFonts.app`.
   - Windows: open `bin\syncmyfonts-gui.exe`.
2. Click `Diagnostics` and save or paste the support report path/output.
3. Click `Readiness Check`.
4. Click `Validation Report` and keep the saved JSON as the before-sync
   evidence.
5. Confirm the managed font folder is a per-user path.
   - macOS should use `~/Library/Fonts/SyncMyFonts`.
   - Windows should use `%LOCALAPPDATA%\Microsoft\Windows\Fonts`.
6. Confirm the app does not ask for administrator permission.

Evidence:

```text
macOS diagnostics:
Windows diagnostics:
macOS readiness:
Windows readiness:
macOS validation report:
Windows validation report:
```

## macOS To Windows

1. On macOS, install or place a non-system test font in the current user's font
   folder. Do not use a font that shipped with macOS or Windows.
2. On macOS, click `Share Fonts On LAN`.
3. Leave `Shared Key` blank and use `Copy URL` plus `Copy Code` for the
   displayed peer URL and pairing code.
4. On Windows, click `Find LAN Peers`.
5. Select the Mac, enter the pairing code, and click `Pair Peer`.
6. Click `Preview From Peer`.
7. Confirm the test font appears as missing and no system fonts are offered as
   missing.
8. Click `Get Missing Fonts`.
9. Confirm the font installs for the current Windows user.
10. Run the same sync again and confirm it skips the already installed font.

Evidence:

```text
macOS peer URL:
Windows preview result:
Windows install result:
Windows second sync result:
Installed Windows font path:
```

## Windows To macOS

1. On Windows, install or place a different non-system test font in the current
   user's font folder.
2. On Windows, click `Share Fonts On LAN`.
3. If Windows Firewall prompts, allow Private networks only.
4. Leave `Shared Key` blank and use `Copy URL` plus `Copy Code` for the
   displayed peer URL and pairing code.
5. On macOS, click `Find LAN Peers`.
6. Select the Windows computer, enter the pairing code, and click `Pair Peer`.
7. Click `Preview From Peer`.
8. Confirm the test font appears as missing and no system fonts are offered as
   missing.
9. Click `Get Missing Fonts`.
10. Confirm the font installs under `~/Library/Fonts/SyncMyFonts`.
11. Run the same sync again and confirm it skips the already installed font.

Evidence:

```text
Windows peer URL:
macOS preview result:
macOS install result:
macOS second sync result:
Installed macOS font path:
```

## System-Font Exclusion

On both computers:

1. Click `Scan Local Fonts`.
2. Confirm fonts from known system folders are not listed as sync candidates.
3. Confirm fonts installed by SyncMyFonts are tracked as managed fonts.
4. Click `Verify Managed Fonts` and confirm the report is clean.
5. Click `Validation Report` and keep the saved JSON as the after-sync
   evidence.

Evidence:

```text
macOS scan notes:
Windows scan notes:
macOS managed verification:
Windows managed verification:
macOS after-sync validation report:
Windows after-sync validation report:
```

## Sign-In Sync

After pairing at least one peer:

1. Click `Enable Sign-In Sync` on macOS.
2. Click `Enable Sign-In Sync` on Windows.
3. Sign out and back in, or reboot each machine.
4. Confirm saved-peer sync runs without putting LAN tokens in visible shortcut
   or plist arguments.

Evidence:

```text
macOS sign-in sync result:
Windows sign-in sync result:
```

## Pass Criteria

- Native GUI launches on both platforms.
- Pairing code flow works in both directions.
- Missing fonts install only into current-user or SyncMyFonts-managed folders.
- System fonts are not offered for sync.
- Re-running sync skips already installed fonts.
- Diagnostics redact secrets.
- No administrator privileges are required.
- No port forwarding is required.
