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

2. Your browser should open automatically. If it does not, run:
   ./bin/syncmyfonts-agent app
   Then open the printed
   localhost URL manually.

3. Use the app to Share Fonts On LAN, Test Peer, Preview From Peer, and Get
   Missing Fonts.

4. To install launch-at-login helpers, see:
   packaging/macos/README.md
EOF

archive="$repo_root/dist/syncmyfonts-macos-$version.tar.gz"
tar -C "$repo_root/dist" -czf "$archive" "syncmyfonts-macos-$version"
echo "Created $archive"
