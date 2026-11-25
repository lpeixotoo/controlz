# Controlz — Knowledge Base

> Source of truth for *what is true*. Add facts with a source. Supersede decisions instead of deleting them.
> Status: bootstrapped 2026-11-25.

## Project
- **Goal:** a macOS menu-bar app that acts as a control center for the user's LG TV and LG air conditioner, **controlling every device locally — no cloud services** (D4).
- **Scope (v1):** TV power, volume/mute, input, apps, media keys, D-pad navigation. AC (power, mode, target temperature, fan) follows once a local route is chosen (T5.1).
- **Non-goals (v1):** other brands, iOS, automations/schedules, cloud APIs (LG ThinQ), App Store distribution.

## Environment
- Machine: Apple Silicon Mac, macOS 26 (Darwin 25.5). (source: `uname`/`swift --version`, 2026-11-25)
- Rust: `cargo 1.85.1` from Homebrew (`/opt/homebrew/bin/cargo`). **This may be too old for current crates** (uniffi, tokio-tungstenite, reqwest MSRVs move fast). P0 should check MSRVs and likely switch to `rustup` stable.
- Swift 6.1, Xcode 16.3. No `xcodegen`/`tuist` installed.
- **Minimum supported macOS: 26** (D6). This Mac runs macOS 26.5.2 (source: `sw_vers`, 2026-11-25).
- `jq` available at `/usr/bin/jq` (used by hooks).

## Devices
- **All appliances are LG** (source: user, 2026-11-25).
- **Private details** (IPs, MACs, serials, ThinQ device IDs, account email) live in `docs/local.md`, which is gitignored. This repo is public: never copy them here, into the plan, fixtures or commits.
- **TV:** LG **OLED55C1PSA** (2021 C1 series, shipped with webOS 6.0). Source: UPnP description at `http://<tv>:1926/` (`friendlyName "[LG] webOS TV OLED55C1PSA"`, `Server: WebOS/4.0.0 UPnP/1.0`), 2026-11-25.
  - Found by LAN sweep: ports 3000 (ws, answers "Hello world") and 3001 (wss) open; also 1926, 9080 (Netflix NRDP), 18181, 36866. MAC vendor prefix is LG Innotek. IP and MAC → `docs/local.md`.
  - "Turn on via Wi-Fi" is **enabled** (user, 2026-11-25), so Wake-on-LAN should work.
  - Bluetooth is not a control channel for the TV (remote/audio only); LAN WebSocket is.
- **AC:** LG, model **unknown**. Registered in the LG ThinQ app; account country BR (user, 2026-11-25). ThinQ is not used (D4). Bluetooth on LG ACs is only used by the ThinQ app for Wi-Fi provisioning.
- Two commissioned Matter nodes (`_matter._tcp`) are on the LAN; what they are is unknown (identifiers in `docs/local.md`). No `_matterc._udp` (commissionable) devices seen.

## Architecture
```
SwiftUI menu-bar app (app/)          Rust core (core/, staticlib + uniffi)
  MenuBarExtra + Settings   ──uniffi──▶  TvClient    ──WebSocket (LAN)──▶  LG webOS TV
  @Observable models                    AcClient    ──local (TBD, T5.1)──▶ LG AC (Matter or IR blaster)
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

## AC — local control options (to be decided in T5.1)
- **Matter (local IP, two-way):** only some newer LG ACs ship Matter support — check the model. A Matter device can join several fabrics (multi-admin), so it could stay in ThinQ/Apple Home and also be commissioned by Controlz. On macOS, candidates are Apple's `Matter.framework` (`MTRDeviceController`, own fabric) or HomeKit (available to Mac Catalyst apps only); a Rust controller (`rs-matter`) is less mature. **All unverified — T5.1 confirms.**
- **IR blaster (local, one-way):** a Wi-Fi IR blaster (e.g. Broadlink RM4 mini, local UDP protocol, see `python-broadlink`) replays the AC remote's codes. Works with any model; no state readback (Controlz keeps its own assumed state). Needs extra hardware.
- **Not available:** a documented local Wi-Fi API on LG ACs; Bluetooth control.

## AC / ThinQ Connect API (reference only — not used, see D4)
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
- **D2** · 2026-11-25 · TV controlled locally over webOS SSAP; AC through the official ThinQ Connect cloud API with a PAT · LG ACs have no local API; ThinQ Connect is LG's official, documented route · status: TV part active; AC part superseded by D4
- **D3** · 2026-11-25 · Development workflow: coordinator main-thread agent + implementer/reviewer/designer subagents, `docs/plan.md` + `docs/knowledge_base.md` as the sources of truth, one task per cycle · status: active
- **D4** · 2026-11-25 · Every device is controlled locally; no cloud services (LG ThinQ rejected). AC route (Matter vs IR blaster) is chosen in T5.1 · user wants local-only control; accepts that AC support depends on the model (Matter) or extra hardware (IR) · status: active
- **D5** · 2026-11-25 · Build the TV end to end first (core → CLI → Swift bridge → app); AC work starts after · TV has a known local protocol and is already reachable; AC route still open · status: active
- **D6** · 2026-11-25 · Minimum macOS 26 · this Mac runs 26.5; use current SwiftUI APIs without fallbacks · status: active
- **D7** · 2026-11-25 · Keep the existing partial `core/` code as a starting point; T0.2 reviews it and makes it compile · Wake-on-LAN + tests are reusable · status: active
- **D8** · 2026-11-25 · One git commit per finished task; designer reviews screenshots after UI tasks · small reviewable history; catch drift from specs early · status: active
- **D9** · 2026-11-25 · Cargo workspace with `core` (library, `controlz_core`) and `cli` (binary `controlz`) · keeps CLI-only deps (clap) out of the library shipped in the app · status: active
- **D10** · 2026-11-25 · Secrets (TV client-key) in the macOS Keychain under one service shared by CLI and app; non-secret device config (TV IP/MAC) in `~/Library/Application Support/Controlz/` · pair once from the CLI, the app reuses it; nothing private in the repo · status: active

## Conventions
- Cargo workspace at the repo root: `core/` (crate `controlz_core`) and `cli/` (binary `controlz`) (D9). Swift package in `app/`.
- Every device protocol is verified against the real device from the `controlz` CLI before UI work uses it.
- Generic standards: `.claude/skills/rust-standards`, `.claude/skills/swift-shell`, `.claude/skills/macos-design`. Project-specific overrides go here.

## Gotchas
- The GitHub repo is public. A pre-commit hook (`.githooks/pre-commit`, enabled with `git config core.hooksPath .githooks`) blocks LAN IPs, MAC addresses, personal emails and `thinqpat_` tokens; mark obvious fakes with `privacy:allow`.
- `core/` contains a **partial, uncompiled** start from before the workflow existed: `Cargo.toml` (uniffi 0.29, tokio-tungstenite 0.26, reqwest 0.12; versions unverified against the toolchain), `uniffi-bindgen.rs`, `src/lib.rs` (declares `thinq`/`webos` modules that don't exist yet), `src/wol.rs` (Wake-on-LAN + tests). Kept (D7); `reqwest` and the `thinq` module are no longer needed (D4).
- The TV client-key is a secret: never in code, fixtures, logs or docs.
- The TV gets its IP by DHCP; suggest a router reservation so the configured IP stays valid (until SSDP discovery, T6.1).

## Roadmap sketch
See `docs/plan.md`. Phases: P0 Setup · P1 TV core + CLI · P2 Swift bridge · P3 Design specs · P4 Mac app (TV) · P5 AC (local, route decided in T5.1) · P6 Later (SSDP discovery, launch at login, signing).

## Open questions
- AC model number — needed by T5.1.
- OK to install `rustup` (Homebrew cargo 1.85 likely too old) — needed by T0.1.

## Sources
- Anthropic, Effective harnesses for long-running agents — https://anthropic.com/engineering/effective-harnesses-for-long-running-agents
- Claude Code docs: subagents https://code.claude.com/docs/en/sub-agents · skills https://code.claude.com/docs/en/skills · settings https://code.claude.com/docs/en/settings-reference
- aiowebostv — https://github.com/home-assistant-libs/aiowebostv
- pythinqconnect — https://github.com/thinq-connect/pythinqconnect
- tv-rs (Rust webOS/Tizen) — https://github.com/Zakarialabib/tv-rs
- Rust core + Swift shell write-up — https://hackmd.io/@Nyanako0129/tokenbar-rust-swift-ffi-en
- Apple TN3179 Local Network Privacy — https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy.md
- LG ThinQ Connect country→region table — https://github.com/thinq-connect/pythinqconnect/blob/main/thinqconnect/country.py (BR → aic; checked 2026-11-25)
- python-broadlink (IR blaster local protocol) — https://github.com/mjg59/python-broadlink
- Matter.framework — https://developer.apple.com/documentation/matter
