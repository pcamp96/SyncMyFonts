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
cargo build --release -p syncmyfonts-agent

cp "$repo_root/target/release/syncmyfonts-agent" "$dist_dir/bin/"
cp -R "$repo_root/packaging/macos" "$dist_dir/packaging/"
cp "$repo_root/packaging/macos/Start-SyncMyFonts.command" "$dist_dir/"
cp "$repo_root/README.md" "$dist_dir/"
cp "$repo_root/docs/app-install.md" "$dist_dir/docs/"
cp "$repo_root/docs/desktop-app-surface.md" "$dist_dir/docs/" 2>/dev/null || true
chmod +x "$dist_dir/Start-SyncMyFonts.command"

cat > "$dist_dir/START-HERE.txt" <<'EOF'
SyncMyFonts macOS MVP

1. Double-click:
   Start-SyncMyFonts.command

2. The native SyncMyFonts window should open. If it does not, run:
   ./bin/syncmyfonts-agent gui

3. On the computer with fonts, click Share Fonts On LAN. Leave Shared Key blank
   for the easiest setup and copy the pairing code.

4. On the other computer, click Find LAN Peers, select the sharing computer,
   enter the pairing code, and click Pair Peer. Then use Preview From Peer or
   Get Missing Fonts.

5. To install launch-at-login helpers, see:
   packaging/macos/README.md

Troubleshooting:
- Both computers must be on the same trusted LAN/VPN.
- If a Windows computer is sharing fonts, allow SyncMyFonts on Private networks
  when Windows Firewall asks.
- No port forwarding is needed.
EOF

archive="$repo_root/dist/syncmyfonts-macos-$version.tar.gz"
tar -C "$repo_root/dist" -czf "$archive" "syncmyfonts-macos-$version"
echo "Created $archive"
