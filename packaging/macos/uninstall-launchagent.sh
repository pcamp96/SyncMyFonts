#!/bin/sh
set -eu

label="${1:-}"
case "$label" in
  serve)
    label="com.syncmyfonts.lan-serve"
    ;;
  sync)
    label="com.syncmyfonts.lan-sync"
    ;;
  com.syncmyfonts.lan-serve|com.syncmyfonts.lan-sync)
    ;;
  *)
    echo "Usage: uninstall-launchagent.sh serve|sync" >&2
    exit 2
    ;;
esac

plist_path="$HOME/Library/LaunchAgents/$label.plist"

if [ -f "$plist_path" ]; then
  launchctl bootout "gui/$UID" "$plist_path" >/dev/null 2>&1 || true
  rm -f "$plist_path"
else
  launchctl bootout "gui/$UID/$label" >/dev/null 2>&1 || true
fi

echo "Removed $label"
