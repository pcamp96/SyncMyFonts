#!/bin/sh
set -eu

script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)

if [ -x "$script_dir/bin/syncmyfonts-agent" ]; then
  agent="$script_dir/bin/syncmyfonts-agent"
elif [ -x "$script_dir/../../bin/syncmyfonts-agent" ]; then
  agent="$script_dir/../../bin/syncmyfonts-agent"
else
  echo "Could not find bin/syncmyfonts-agent next to this helper."
  echo "Move this helper back into the SyncMyFonts release folder and try again."
  read -r _
  exit 1
fi

if [ -x "$script_dir/bin/syncmyfonts-gui" ]; then
  gui="$script_dir/bin/syncmyfonts-gui"
elif [ -x "$script_dir/../../bin/syncmyfonts-gui" ]; then
  gui="$script_dir/../../bin/syncmyfonts-gui"
else
  gui=""
fi

timestamp=$(date -u +"%Y%m%d-%H%M%SZ")
evidence_dir="$HOME/Desktop/SyncMyFonts-Evidence-$timestamp"
mkdir -p "$evidence_dir"

echo "Collecting SyncMyFonts launch and readiness evidence..."
"$agent" diagnostics > "$evidence_dir/diagnostics.json"
"$agent" doctor > "$evidence_dir/readiness-check.json"
"$agent" validation-report --write > "$evidence_dir/validation-report-path.json"
if [ "$gui" != "" ]; then
  "$gui" --self-test > "$evidence_dir/gui-self-test.json"
fi

cat > "$evidence_dir/README.txt" <<EOF
SyncMyFonts validation evidence

Collected: $timestamp

Files:
- diagnostics.json: redacted support report and local paths.
- readiness-check.json: local app readiness checks.
- validation-report-path.json: path to the saved full validation report.
- gui-self-test.json: native GUI first-run state check, if the GUI binary was present.

Next:
1. Confirm the SyncMyFonts window opens.
2. Run Preview From Peer before installing fonts.
3. Keep this folder with the before/after clean-machine validation notes.
EOF

echo "Evidence saved to:"
echo "$evidence_dir"
echo
echo "Launching SyncMyFonts..."

if [ -d "$script_dir/SyncMyFonts.app" ]; then
  open "$script_dir/SyncMyFonts.app"
elif [ "$gui" != "" ]; then
  "$gui" &
else
  "$agent" gui &
fi

echo "Press Return to close this helper."
read -r _
