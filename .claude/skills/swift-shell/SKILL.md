---
name: swift-shell
description: Conventions for the Controlz SwiftUI menu-bar shell - app structure, calling the Rust core over uniffi, state management, Keychain, Info.plist and bundling. Used by the implementer when writing Swift and by the reviewer when judging it.
user-invocable: false
paths: app/**
---

# Swift shell standards (app/)

The shell is deliberately thin: **all device logic lives in Rust.** Swift does presentation, persistence of settings and secrets, and calling the core.

## Structure
- SwiftPM package built from the command line (`swift build`), assembled into `Controlz.app` by a script. No Xcode project unless a decision says otherwise.
- `@main` App with a `MenuBarExtra(..., content:)` using `.menuBarExtraStyle(.window)` and a `Settings` scene.
- `LSUIElement = YES` (no Dock icon). `NSLocalNetworkUsageDescription` set (required for reaching the TV on the LAN).
- Minimum macOS version is recorded in the knowledge base; don't use newer APIs without `if #available`.

## State
- One `@Observable` (Observation framework) model per device, e.g. `TVModel`, `ACModel`, owned by the App and passed via environment.
- Views never call the Rust core directly; models do.
- Every async call to the core is wrapped in a `Task`, updates state on `@MainActor`, and maps `ControlError` to a user-facing message.
- Optimistic UI is allowed for instant controls (volume, power) but must roll back on error.

## Calling Rust (uniffi)
- The generated Swift bindings are build output: never edit them by hand, and don't commit them unless a decision says so.
- Rust clients (`TvClient`, `ThinqClient`) are created once per configuration change, not per call.
- Rust errors arrive as Swift `throws`; catch them in the model layer, never `try!`.

## Secrets & settings
- ThinQ personal access token and TV client-key go in the **Keychain** (`kSecClassGenericPassword`, service `app.controlz`). Never in `UserDefaults`, logs or files.
- Non-secret settings (TV IP, MAC, country, selected AC device) go in `UserDefaults` via `@AppStorage`.

## Quality
- No force unwraps (`!`) outside constants that cannot fail.
- Accessibility labels on every icon-only control.
- Follow the active design spec in `docs/design/` exactly; deviations are reported, not improvised.
- `swift build` must pass with no warnings in files you touched.
