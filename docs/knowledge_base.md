# Controlz — Knowledge Base

> Source of truth for *what is true*. Add facts with a source. Supersede decisions instead of deleting them.
> Status: **seeded from research on 2026-11-25, before /bootstrap.** Device details and workflow answers are still open.

## Project
- **Goal:** a macOS menu-bar app that acts as a control center for the user's LG TV and LG air conditioner.
- **Scope (v1):** TV power, volume/mute, input, apps, media keys, D-pad navigation; AC power, mode, target temperature, fan speed, current temperature.
- **Non-goals (v1):** other brands, iOS, automations/schedules, Apple Home/Matter integration, App Store distribution.

## Environment
- Machine: Apple Silicon Mac, macOS 26 (Darwin 25.5). (source: `uname`/`swift --version`, 2026-11-25)
- Rust: `cargo 1.85.1` from Homebrew (`/opt/homebrew/bin/cargo`). **This may be too old for current crates** (uniffi, tokio-tungstenite, reqwest MSRVs move fast). P0 should check MSRVs and likely switch to `rustup` stable.
- Swift 6.1, Xcode 16.3. No `xcodegen`/`tuist` installed.
- `jq` available at `/usr/bin/jq` (used by hooks).

## Devices
- **All appliances are LG** (source: user, 2026-11-25).
- TV: model / year / webOS version / IP / MAC — **unknown**.
- AC: model / ThinQ registration — **unknown**.
- LG account country (selects the ThinQ region) — **unknown**.

## Architecture
```
SwiftUI menu-bar app (app/)          Rust core (core/, staticlib + uniffi)
  MenuBarExtra + Settings   ──uniffi──▶  TvClient    ──WebSocket (LAN)──▶  LG webOS TV
  @Observable models                    ThinqClient ──HTTPS (cloud)───▶   LG ThinQ Connect API ─▶ AC
  Keychain (secrets)                    wol         ──UDP broadcast──▶    TV power-on
```
- Rust owns every protocol; Swift owns UI, settings and secrets.
- uniffi generates Swift bindings; async Rust functions become Swift `async throws`.

## TV / webOS protocol
Source: `aiowebostv` (Home Assistant's library, Apache-2.0) — https://github.com/home-assistant-libs/aiowebostv ; Rust reference: https://github.com/Zakarialabib/tv-rs
- **Transport:** WebSocket. Try `wss://<ip>:3001` first (newer firmware may reject or hang on ws), fall back to `ws://<ip>:3000` (older webOS). Remember which one worked. TLS is a self-signed cert → certificate validation must be disabled for this connection only.
- **Handshake order (newer webOS):** optionally send `{"id":"hello","type":"hello","payload":{}}`; newer versions want `ssap://system/getSystemInfo` requested **before** registration.
- **Registration:** send `{"type":"register","id":"register_0","payload":{forcePairing:false, pairingType:"PROMPT", manifest:{…}, "client-key": <stored or absent>}}`.
  - First response may be `type:"response"` with `payload.pairingType:"PROMPT"` → the TV shows a prompt; wait for the next message.
  - Success: `type:"registered"`, `payload["client-key"]` → persist it (Keychain). Keep the stored key if the TV sends none.
  - `type:"error"` with a stored key while the TV is booting is transient → retry on next connect.
- **Manifest:** the well-known signed "LG Remote App" manifest (appId `com.lge.test`, signature + `signed` block, permission list). Copy it from `aiowebostv/handshake.py` (Apache-2.0, keep attribution). **Newer firmware rejects the signature:** on `type:"error"` retry once with an *unsigned* manifest (remove `signatures`, merge `signed.permissions` into `permissions`, drop `signed`).
- **Requests:** `{"id":"<unique>","type":"request","uri":"ssap://<endpoint>","payload":{…}}`; match responses by `id`.
- **Endpoints (ssap://…):** `audio/volumeUp`, `audio/volumeDown`, `audio/setVolume {volume}`, `audio/getVolume`, `audio/setMute {mute}`, `audio/getStatus`, `system/turnOff`, `system/turnOn`, `system/getSystemInfo`, `com.webos.applicationManager/listLaunchPoints`, `com.webos.applicationManager/launch {id}`, `com.webos.applicationManager/getForegroundAppInfo`, `tv/getExternalInputList`, `tv/switchInput {inputId}`, `tv/channelUp`, `tv/channelDown`, `media.controls/play|pause|stop|rewind|fastForward`, `com.webos.service.tvpower/power/getPowerState`, `com.webos.service.tvpower/power/turnOffScreen|turnOnScreen`, `system.notifications/createToast {message}`.
- **Buttons (D-pad, OK, Back, Home…):** request `ssap://com.webos.service.networkinput/getPointerInputSocket` → `payload.socketPath` (a ws/wss URL); open a second WebSocket to it and send text frames `type:button\nname:<NAME>\n\n` (names: `UP`, `DOWN`, `LEFT`, `RIGHT`, `ENTER`, `BACK`, `HOME`, `MENU`, `EXIT`, …).
- **Power on:** the socket is down when the TV is off → use Wake-on-LAN (magic packet to the TV's MAC, UDP broadcast ports 9/7). Requires the TV setting "Turn on via Wi-Fi" / "Mobile TV On".
- **Power off:** `system/turnOff`; the response may never arrive while shutting down → don't wait for it.

## AC / ThinQ Connect API
Source: LG's official SDK `pythinqconnect` (Apache-2.0) — https://github.com/thinq-connect/pythinqconnect ; PAT docs — https://thinq.developer.lge.com/en/cloud/docs/thinq-connect/PAT-en/
- **Cloud only.** There is no documented local API for LG ACs. Expect seconds of latency.
- **Auth:** Personal Access Token created by the user at https://connect-pat.lgthinq.com (sign in with the ThinQ account → Add new token → choose scopes). Stored in Keychain.
- **Base URL:** `https://api-<region>.lgthinq.com/` where region is `kic` / `aic` / `eic` chosen by the account's country (mapping table in `thinqconnect/country.py`; e.g. US and BR → `aic`; KR, AU → `kic`; most of Europe/Middle East/Africa → `eic`). Verify the user's country against the table.
- **Headers on every request:**
  - `Authorization: Bearer <PAT>`
  - `x-country: <ISO country code>`
  - `x-message-id: <base64url(uuid4 bytes) without padding, 22 chars>`
  - `x-client-id: <uuid, stable across restarts — generate once and persist>`
  - `x-api-key: v6GFvkweNo7DK7yD3ylIZ9w52aKBU0eJ7wLXkSR3` (public program key shipped in LG's SDK; identifies the API, not the user)
  - `x-service-phase: OP`
  - control calls also send `x-conditional-control: true`
- **Endpoints:** `GET devices` (list), `GET devices/{id}/profile` (capabilities), `GET devices/{id}/state`, `POST devices/{id}/control` (JSON body). Push/events via `POST event/{id}/subscribe` + MQTT (later phase).
- **Responses:** success → JSON with the payload under `response`. Error → `{"error":{"code":"<4 digits>","message":…}}`. Useful codes: `1103` invalid token, `1218` invalid token again, `1306` exceeded API calls, `1222` device not connected, `2301`–`2305` command not supported in current state (e.g. `2304` when powered off), `2209` device response delay, `2210` retry request.
- **Device type** for ACs: `DEVICE_AIR_CONDITIONER`.
- **State/control resources (AC):**
  - power: `{"operation": {"airConOperationMode": "POWER_ON" | "POWER_OFF"}}`
  - mode: `{"airConJobMode": {"currentJobMode": "COOL" | "HEAT" | "AIR_DRY" | "FAN" | "AUTO" | …}}`
  - fan: `{"airFlow": {"windStrength": "LOW" | "MID" | "HIGH" | "AUTO" | …}}` (some models use `windStrengthDetail`; check the profile)
  - temperature — **two shapes exist, pick by what the device's state/profile contains:**
    - `temperature` (object): `{currentTemperature, targetTemperature, unit}`; control `{"temperature": {"targetTemperature": 22, "unit": "C"}}`
    - `temperatureInUnits` (array, one entry per unit): `[{unit:"C", currentTemperature, targetTemperature, …}, {unit:"F", …}]`; control `{"temperatureInUnits": {"targetTemperature": 22, "unit": "C"}}`
  - others available: `powerSave.powerSaveEnabled`, `display.light`, `windDirection.rotateUpDown`, `timer`, `sleepTimer`, `airQualitySensor`, `filterInfo`.
- **Profile format:** `profile.property.<resource>.<key> = {"type": "enum"|"range"|"boolean", "mode": ["r","w"], "value": {"r": [...], "w": [...]}}` (range values carry `min`/`max`/`step`). Writable mode/fan lists and temperature limits come from here; don't hardcode them. Multi-unit devices may have `property` as a list keyed by location.

## macOS platform
- Local network access: apps that reach LAN devices need `NSLocalNetworkUsageDescription` in Info.plist, and the user grants "Local Network" in Privacy & Security. Without it, connections and discovery fail silently. (Apple TN3179: https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy.md)
- The multicast entitlement is not required on macOS.
- Menu-bar-only app: `LSUIElement = YES`; SwiftUI `MenuBarExtra` with `.menuBarExtraStyle(.window)` gives a popover-like panel.
- Secrets in Keychain (`kSecClassGenericPassword`).

## Decisions
- **D1** · 2026-11-25 · Rust core + thin SwiftUI shell, bridged with uniffi · native look and a small menu-bar footprint for the UI, Rust for the protocol logic; pure-Rust AppKit (objc2) is too tedious, Tauri carries a WebView · status: active
- **D2** · 2026-11-25 · TV controlled locally over webOS SSAP; AC through the official ThinQ Connect cloud API with a PAT · LG ACs have no local API; ThinQ Connect is LG's official, documented route · status: active
- **D3** · 2026-11-25 · Development workflow: coordinator main-thread agent + implementer/reviewer/designer subagents, `docs/plan.md` + `docs/knowledge_base.md` as the sources of truth, one task per cycle · status: active

## Conventions
- Rust crate lives in `core/` (crate name `controlz_core`), Swift package in `app/`.
- Generic standards: `.claude/skills/rust-standards`, `.claude/skills/swift-shell`, `.claude/skills/macos-design`. Project-specific overrides go here.

## Gotchas
- `core/` contains a **partial, uncompiled** start from before the workflow existed: `Cargo.toml` (uniffi 0.29, tokio-tungstenite 0.26, reqwest 0.12; versions unverified against the toolchain), `uniffi-bindgen.rs`, `src/lib.rs` (declares `thinq`/`webos` modules that don't exist yet), `src/wol.rs` (Wake-on-LAN + tests). `/bootstrap` decides whether to keep it.
- ThinQ PAT and TV client-key are secrets: never in code, fixtures, logs or docs.

## Roadmap sketch
- **P0 Setup:** toolchain (rustup + MSRV check), crate + workspace layout, build script skeleton, decide on the existing `core/` code.
- **P1 TV (local):** Wake-on-LAN; webOS connect + pairing (wss→ws, signed→unsigned); commands (volume, mute, apps, inputs, media, power off); pointer-socket buttons; a `controlz-cli` to verify against the real TV.
- **P2 AC (cloud):** ThinQ HTTP client (headers, regions, errors); device list + profile parsing; state parsing (both temperature shapes); control commands; CLI commands; fixture tests.
- **P3 FFI:** uniffi exports + tokio runtime; script that builds the static lib and generates Swift bindings.
- **P4 Design:** designer specs for the menu-bar popover, Settings, and first-run setup (TV pairing, ThinQ token entry).
- **P5 Swift app:** SwiftPM skeleton + `.app` bundling + Info.plist; Settings + Keychain; TV pairing flow; TV controls; AC controls; error/offline states.
- **P6 Later:** ThinQ MQTT push for live AC state; SSDP TV discovery; launch at login; signing/notarization.

## Open questions
- TV model/year, IP, MAC; is "Turn on via Wi-Fi" enabled?
- AC model; is it registered in the LG ThinQ app?
- LG account country.
- Commit policy: one commit per finished task?
- Keep or delete the partial `core/` code?
- Minimum macOS version to support.

## Sources
- Anthropic, Effective harnesses for long-running agents — https://anthropic.com/engineering/effective-harnesses-for-long-running-agents
- Claude Code docs: subagents https://code.claude.com/docs/en/sub-agents · skills https://code.claude.com/docs/en/skills · settings https://code.claude.com/docs/en/settings-reference
- aiowebostv — https://github.com/home-assistant-libs/aiowebostv
- pythinqconnect — https://github.com/thinq-connect/pythinqconnect
- tv-rs (Rust webOS/Tizen) — https://github.com/Zakarialabib/tv-rs
- Rust core + Swift shell write-up — https://hackmd.io/@Nyanako0129/tokenbar-rust-swift-ffi-en
- Apple TN3179 Local Network Privacy — https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy.md
