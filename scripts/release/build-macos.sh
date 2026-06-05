#!/bin/sh
set -eu

repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
version=$(cd "$repo_root" && cargo metadata --no-deps --format-version 1 | sed -n 's/.*"name":"syncmyfonts-agent","version":"\([^"]*\)".*/\1/p')
if [ "$version" = "" ]; then
  version="0.1.0"
fi

dist_dir="$repo_root/dist/syncmyfonts-macos-$version"
rm -rf "$dist_dir"
mkdir -p "$dist_dir/bin" "$dist_dir/packaging" "$dist_dir/docs"

cd "$repo_root"
cargo build --release -p syncmyfonts-agent --bins

cp "$repo_root/target/release/syncmyfonts-agent" "$dist_dir/bin/"
cp "$repo_root/target/release/syncmyfonts-gui" "$dist_dir/bin/"
cp -R "$repo_root/packaging/macos" "$dist_dir/packaging/"
cp "$repo_root/packaging/macos/Start-SyncMyFonts.command" "$dist_dir/"
cp "$repo_root/README.md" "$dist_dir/"
cp "$repo_root/docs/app-install.md" "$dist_dir/docs/"
cp "$repo_root/docs/manual-clean-machine-validation.md" "$dist_dir/docs/"
cp "$repo_root/docs/desktop-app-surface.md" "$dist_dir/docs/" 2>/dev/null || true
chmod +x "$dist_dir/Start-SyncMyFonts.command"

app_dir="$dist_dir/SyncMyFonts.app"
mkdir -p "$app_dir/Contents/MacOS" "$app_dir/Contents/Resources"
cp "$repo_root/target/release/syncmyfonts-gui" "$app_dir/Contents/MacOS/SyncMyFonts"
cp "$repo_root/target/release/syncmyfonts-agent" "$app_dir/Contents/MacOS/syncmyfonts-agent"
chmod +x "$app_dir/Contents/MacOS/SyncMyFonts" "$app_dir/Contents/MacOS/syncmyfonts-agent"
cat > "$app_dir/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleDevelopmentRegion</key>
  <string>en</string>
  <key>CFBundleExecutable</key>
  <string>SyncMyFonts</string>
  <key>CFBundleIdentifier</key>
  <string>com.syncmyfonts.app</string>
  <key>CFBundleName</key>
  <string>SyncMyFonts</string>
  <key>CFBundleDisplayName</key>
  <string>SyncMyFonts</string>
  <key>CFBundlePackageType</key>
  <string>APPL</string>
  <key>CFBundleShortVersionString</key>
  <string>$version</string>
  <key>CFBundleVersion</key>
  <string>$version</string>
  <key>LSMinimumSystemVersion</key>
  <string>12.0</string>
  <key>NSLocalNetworkUsageDescription</key>
  <string>SyncMyFonts uses your local network to find and sync fonts with your other computers.</string>
</dict>
</plist>
EOF

cat > "$dist_dir/START-HERE.txt" <<'EOF'
SyncMyFonts macOS MVP

1. Double-click:
   SyncMyFonts.app

   If macOS blocks the unsigned MVP app, use:
   Start-SyncMyFonts.command

2. The native SyncMyFonts window should open. If it does not, run:
   ./bin/syncmyfonts-agent gui

3. Click Readiness Check. The managed font folder should be under your user
   account, and no administrator prompt should appear.

4. On the computer with fonts, click Share Fonts On LAN. Leave Shared Key blank
   for the easiest setup and copy the pairing code.

5. On the other computer, click Find LAN Peers, select the sharing computer,
   enter the pairing code, and click Pair Peer. Then use Preview From Peer or
   Get Missing Fonts.

6. To install launch-at-login helpers, click Enable Sign-In Sync after pairing
   peers, or see:
   packaging/macos/README.md

Validation:
- For a full Mac-to-Windows and Windows-to-Mac test pass, see:
  docs/app-install.md
  docs/manual-clean-machine-validation.md

Troubleshooting:
- Both computers must be on the same trusted LAN/VPN.
- If a Windows computer is sharing fonts, allow SyncMyFonts on Private networks
  when Windows Firewall asks.
- No port forwarding is needed.
- SyncMyFonts only syncs current-user fonts and fonts it installed itself. It
  does not copy system font folders.
EOF

archive="$repo_root/dist/syncmyfonts-macos-$version.tar.gz"
tar -C "$repo_root/dist" -czf "$archive" "syncmyfonts-macos-$version"
echo "Created $archive"
