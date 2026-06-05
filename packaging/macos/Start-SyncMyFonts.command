#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if [ -x "$script_dir/../../bin/syncmyfonts-agent" ]; then
  agent="$script_dir/../../bin/syncmyfonts-agent"
elif [ -x "$script_dir/bin/syncmyfonts-agent" ]; then
  agent="$script_dir/bin/syncmyfonts-agent"
else
  echo "Could not find bin/syncmyfonts-agent next to this launcher."
  echo "Move this launcher back into the SyncMyFonts release folder and try again."
  read -r _
  exit 1
fi

"$agent" app
