# macOS Launcher Artifacts

These files provide per-user LaunchAgent wrappers for the current
`syncmyfonts-agent` CLI. They do not install a system daemon and do not require
sudo.

## Install LAN Sharing

```sh
packaging/macos/install-launchagent.sh serve \
  --agent-path "$PWD/target/release/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key"
```

This starts `syncmyfonts-agent lan-serve --listen 0.0.0.0:7370` when the user
signs in and restarts it if the listener exits unexpectedly.

## Install Scheduled LAN Pull

```sh
packaging/macos/install-launchagent.sh sync \
  --agent-path "$PWD/target/release/syncmyfonts-agent" \
  --lan-key "choose-a-shared-key" \
  --peer "http://192.168.1.50:7370" \
  --interval 14400
```

This runs `lan-sync` at sign-in and every 4 hours.

## Uninstall

```sh
packaging/macos/uninstall-launchagent.sh serve
packaging/macos/uninstall-launchagent.sh sync
```

Logs are written to `~/Library/Logs/SyncMyFonts`. The generated plist is written
to `~/Library/LaunchAgents`. The helper stores the LAN key in
`~/Library/Application Support/SyncMyFonts/lan.env` with user-only permissions
and points the plist at a generated wrapper script.

For a production app, store the LAN key in Keychain and generate the plist from
the app settings flow instead of accepting it on the command line.
