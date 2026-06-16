# SyncMyFonts Tauri UI Direction

This pass moves the app shell away from the egui control wall and toward a polished desktop UI that can match the Stitch direction while keeping the Rust LAN sync engine.

## Product Position

SyncMyFonts is a local-first desktop utility for designers and shop operators who move between a design machine and a production machine. The main job of the UI is to make LAN font transfer feel calm, obvious, and safe.

## UI Rules

- LAN sync is the primary mode.
- System fonts are excluded by default and the UI must say so clearly.
- The first screen should guide the user through Share, Pair, Preview, and Install.
- Advanced diagnostics should be available, but not visible as the main product surface.
- The app is a desktop app. Browser UI is only for optional self-hosted server administration.
- macOS and Windows should share the same layout language, with small platform-specific packaging later.

## Current Migration Shape

The new Tauri shell lives in `apps/syncmyfonts-ui`. It is intentionally separate from the current `syncmyfonts-gui` binary until the new UI is visually and operationally ready to replace it in packaging.

The first Tauri milestone is UI-only:

1. Build the polished shell.
2. Prove the Rust command bridge works.
3. Add screenshots and runtime checks.
4. Wire existing LAN actions into the shell.
5. Switch release packaging from egui to Tauri.

## Visual Language

Palette:

- Ink: `#172033`
- Muted ink: `#68758c`
- Canvas: `#f5f7fb`
- Surface: `#ffffff`
- Line: `#dde4ee`
- Action blue: `#2d6cdf`
- Success green: `#2f9c67`
- Warning amber: `#c98214`

Signature element:

The sync workspace uses a device-to-device rail as the central mental model: this computer on the left, paired computer on the right, and the Share, Pair, Preview, Install sequence between them.
