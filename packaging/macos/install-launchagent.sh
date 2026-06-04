#!/bin/sh
set -eu

usage() {
  cat <<'USAGE'
Usage:
  install-launchagent.sh serve --agent-path /path/to/syncmyfonts-agent --lan-key KEY [--listen 0.0.0.0:7370]
  install-launchagent.sh sync --agent-path /path/to/syncmyfonts-agent --lan-key KEY [--peer http://HOST:7370] [--interval 14400]

Installs a per-user LaunchAgent under ~/Library/LaunchAgents.
USAGE
}

shell_quote() {
  printf "%s" "$1" | sed "s/'/'\\\\''/g; 1s/^/'/; \$s/\$/'/"
}

mode="${1:-}"
if [ "$mode" = "" ]; then
  usage
  exit 2
fi
shift

agent_path=""
lan_key=""
listen="0.0.0.0:7370"
peer=""
interval="14400"

while [ "$#" -gt 0 ]; do
  case "$1" in
    --agent-path)
      agent_path="${2:-}"
      shift 2
      ;;
    --lan-key)
      lan_key="${2:-}"
      shift 2
      ;;
    --listen)
      listen="${2:-}"
      shift 2
      ;;
    --peer)
      peer="${2:-}"
      shift 2
      ;;
    --interval)
      interval="${2:-}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "Unknown argument: $1" >&2
      usage
      exit 2
      ;;
  esac
done

if [ "$agent_path" = "" ] || [ "$lan_key" = "" ]; then
  echo "--agent-path and --lan-key are required." >&2
  exit 2
fi

if [ ! -x "$agent_path" ]; then
  echo "Agent path is not executable: $agent_path" >&2
  exit 1
fi

case "$mode" in
  serve)
    template_name="com.syncmyfonts.lan-serve.plist.template"
    label="com.syncmyfonts.lan-serve"
    ;;
  sync)
    template_name="com.syncmyfonts.lan-sync.plist.template"
    label="com.syncmyfonts.lan-sync"
    ;;
  *)
    echo "Mode must be serve or sync." >&2
    usage
    exit 2
    ;;
esac

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
launch_agents_dir="$HOME/Library/LaunchAgents"
log_dir="$HOME/Library/Logs/SyncMyFonts"
support_dir="$HOME/Library/Application Support/SyncMyFonts"
plist_path="$launch_agents_dir/$label.plist"
env_path="$support_dir/lan.env"
runner_path="$support_dir/run-$mode.sh"

mkdir -p "$launch_agents_dir" "$log_dir" "$support_dir"
chmod 700 "$support_dir"

quoted_lan_key=$(shell_quote "$lan_key")
cat > "$env_path" <<EOF
SYNCMYFONTS_LAN_KEY=$quoted_lan_key
EOF
chmod 600 "$env_path"

case "$mode" in
  serve)
    cat > "$runner_path" <<EOF
#!/bin/sh
set -eu
. "$env_path"
exec "$agent_path" lan-serve --listen "$listen"
EOF
    ;;
  sync)
    if [ "$peer" != "" ]; then
      "$agent_path" lan-add-peer --name "LaunchAgent Peer" --url "$peer" --lan-key "$lan_key" >/dev/null
    fi
    cat > "$runner_path" <<EOF
#!/bin/sh
set -eu
. "$env_path"
exec "$agent_path" lan-sync-all
EOF
    ;;
esac
chmod 700 "$runner_path"

sed \
  -e "s|{{SYNCMYFONTS_RUNNER_PATH}}|$runner_path|g" \
  -e "s|{{SYNCMYFONTS_SYNC_INTERVAL_SECONDS}}|$interval|g" \
  -e "s|{{SYNCMYFONTS_LOG_DIR}}|$log_dir|g" \
  -e "s|{{SYNCMYFONTS_WORKING_DIR}}|$support_dir|g" \
  "$script_dir/$template_name" > "$plist_path"

chmod 600 "$plist_path"

if launchctl print "gui/$UID/$label" >/dev/null 2>&1; then
  launchctl bootout "gui/$UID" "$plist_path" >/dev/null 2>&1 || true
fi

launchctl bootstrap "gui/$UID" "$plist_path"
launchctl enable "gui/$UID/$label"
launchctl kickstart -k "gui/$UID/$label"

echo "Installed $label at $plist_path"
