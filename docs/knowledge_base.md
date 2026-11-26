# Controlz — Knowledge Base

> Source of truth for *what is true*. Add facts with a source. Supersede decisions instead of deleting them.
> Status: bootstrapped 2026-11-25.

## Project
- **Goal:** a macOS menu-bar app that acts as a control center for the user's LG TV and LG air conditioner, **controlling every device locally — no cloud services** (D4).
- **Scope (v1):** TV power, volume/mute, input, apps, media keys, D-pad navigation. AC (power, mode, target temperature, fan) follows once a local route is chosen (T1.1).
- **Non-goals (v1):** other brands, iOS, automations/schedules, cloud APIs (LG ThinQ), App Store distribution.

## Environment
- Machine: Apple Silicon Mac, macOS 26 (Darwin 25.5). (source: `uname`/`swift --version`, 2026-11-25)
- Rust: ~~`cargo 1.85.1` from Homebrew is the toolchain~~ (corrected 2026-11-25: Homebrew cargo 1.85.1 is still installed but unused; the project uses rustup 1.100.0, see below).
- Swift 6.1, Xcode 16.3. No `xcodegen`/`tuist` installed.
- **Minimum supported macOS: 26** (D6). Installing rustup approved by the user (2026-11-25). This Mac runs macOS 26.5.2 (source: `sw_vers`, 2026-11-25).
- `jq` available at `/usr/bin/jq` (used by hooks).
- **Rust toolchain (T0.1):** rustup 1.29.3 (official `sh.rustup.rs` installer, `--no-modify-path`), toolchain **1.100.0** (`rustc 1.100.0 (0ef1bc386 2026-11-09)`, `cargo 1.100.0`), current stable on 2026-11-25. Pinned in `rust-toolchain.toml` (`channel = "1.100.0"`, `rustfmt` + `clippy`). (source: `rustc --version`, `static.rust-lang.org/dist/channel-rust-stable.toml`, 2026-11-25)
  - PATH: `~/.zshrc` ends with `export PATH="$HOME/.cargo/bin:$PATH"`. It has to be the **last** PATH change, because `.zshrc` prepends `/opt/homebrew/bin` near the top. Homebrew `rust` 1.85.1 stays installed but is shadowed. Homebrew's `cargo` ignores `rust-toolchain.toml` (only the rustup proxies in `~/.cargo/bin` honor it). Check with `which cargo` → `~/.cargo/bin/cargo`. GUI processes (Xcode build phases) don't read `.zshrc`, so scripts they run should call `~/.cargo/bin/cargo` or set PATH themselves. (source: `zsh -i -c 'which -a cargo'`, 2026-11-25)
  - Claude Code agent Bash shells and hooks (and formerly the rust-check hook; now `.claude/scripts/rust-check.sh`, which forces `~/.cargo/bin` and `--locked`) do **not** get `~/.cargo/bin` on PATH, so they run Homebrew cargo 1.85.1. That cargo fails on the 1.85+ MSRV deps ("rustc 1.85.1 is not supported by the following packages"). Worse, its MSRV-aware resolver writes a `Cargo.lock` pinned to 1.85-compatible versions (it chose `cargo-platform` 0.3.0 and `textwrap` 0.16.2 instead of 0.3.3 and 0.16.4). Prefix agent commands with `export PATH="$HOME/.cargo/bin:$PATH"`. If Homebrew cargo wrote the lockfile, run `cargo generate-lockfile` again with 1.100. (source: hook output plus diffing the lockfiles, 2026-11-25)
  - cargo 1.100 has a built-in manifest lint `cargo::unused_dependencies` (warn by default). It flags declared-but-unused deps at build time. (source: build output, 2026-11-25)
  - The same lint also covers the workspace: an entry in `[workspace.dependencies]` that no member uses prints "warning: unused workspace dependency `<name>`". It is only a warning, so `-D warnings` (a rustc flag) does not turn it into an error and `scripts/check.sh` still exits 0. (source: `scripts/check.sh` output after T0.2, 2026-11-25)
- **Core dependency MSRVs** (`rust_version` from `https://crates.io/api/v1/crates/<name>`; 2026-11-25). The pin 1.100.0 satisfies all of them:
  | crate | version | MSRV |
  |---|---|---|
  | uniffi (`tokio`; `cli` only via `controlz_core/bindgen` since T0.2) | 0.33.0 | 1.95.0, not declared in Cargo.toml (`rust_version: null`); taken from uniffi CI `prepare-rust-min-version: rustup override set 1.95.0` at tag `v0.33.0` (`.circleci/config.yml`); uniffi itself builds with 1.99.0 (`rust-toolchain.toml`); policy `docs/policies/rust-versions.md` |
  | tokio | 1.54.1 | 1.85 |
  | tokio-tungstenite (`native-tls`) | 0.30.0 | 1.85 |
  | native-tls | 0.2.18 | 1.80 |
  | futures-util | 0.3.34 | 1.71 |
  | serde | 1.0.229 | 1.56 |
  | serde_json | 1.0.154 | 1.71 |
  | thiserror | 2.0.21 | 1.77 |
  - Highest declared MSRV in the whole resolved graph (133 locked dependency packages plus `controlz_core`, `cargo metadata`): **1.91** (`cargo-platform` 0.3.3, pulled in by uniffi's `cli` feature via `cargo_metadata`; since T0.2 only with `--features bindgen`, not in the shipped staticlib); then 1.88 (`askama` 0.16.x, `textwrap` 0.16.4). 29 packages declare none (all `uniffi_*` among them).
  - Removed in T0.1: `reqwest` (ThinQ only, D4), `uuid` and `base64` (no users in `core/src`).
  - Added in T0.3 (`rust_version` from crates.io API, 2026-11-25): core `toml` 1.2.3 (MSRV 1.85), core `tracing` 0.1.44 (1.65.0); CLI only (D9) `clap` 4.6.10 with `derive` (1.85), `security-framework` 3.7.0 (1.85), `tracing-subscriber` 0.3.23 with `default-features = false`, features `fmt`, `std`, `ansi` (1.65.0; also a core dev-dependency for the log-redaction test). Highest declared MSRV in the resolved graph (117 packages) is still 1.91 (`cargo-platform`).
  - Removed in T0.3 from `[workspace.dependencies]` (no member used them; re-add when TV work starts): `tokio-tungstenite`, `native-tls`, `futures-util`, `serde_json`. Their table rows above are kept as the versions chosen in T0.1.

## Devices
- **All appliances are LG** (source: user, 2026-11-25).
- **Private details** (IPs, MACs, serials, ThinQ device IDs, account email) live in `docs/local.md`, which is gitignored. This repo is public: never copy them here, into the plan, fixtures or commits.
- **TV:** LG **OLED55C1PSA** (2021 C1 series, shipped with webOS 6.0). Source: UPnP description at `http://<tv>:1926/` (`friendlyName "[LG] webOS TV OLED55C1PSA"`, `Server: WebOS/4.0.0 UPnP/1.0`), 2026-11-25.
  - Found by LAN sweep: ports 3000 (ws, answers "Hello world") and 3001 (wss) open; also 1926, 9080 (Netflix NRDP), 18181, 36866. MAC vendor prefix is LG Innotek. IP and MAC → `docs/local.md`.
  - "Turn on via Wi-Fi" is **enabled** (user, 2026-11-25), so Wake-on-LAN should work.
  - Bluetooth is not a control channel for the TV (remote/audio only); LAN WebSocket is.
- **AC:** LG split, indoor unit **S4NQ09WA51A** — cooling only (heating N/A), 9,000 BTU/h (2,638 W nominal; 2,010–10,000 BTU/h range), 220 V 1φ 60 Hz, R410A, built-in Wi-Fi module **LCW-003**, made in Manaus; label date code suggests **Sept 2020**. Serial and part numbers → `docs/local.md`. (source: indoor-unit label photo, user, 2026-11-25)
  - Registered in the LG ThinQ app; account country BR (user, 2026-11-25). ThinQ is not used (D4). Bluetooth on LG ACs is only used by the ThinQ app for Wi-Fi provisioning.
  - **Not visible on the Mac's subnet** in ARP/ping sweeps (2026-11-25). The user then put it into Wi-Fi setup mode on purpose; it broadcasts its own **`LG…` Wi-Fi network** (SoftAP) (user, 2026-11-25).
  - IR remote: **AKB75215401** (source: remote label photo, user, 2026-11-25). The IR remote talks to the indoor unit directly and keeps working whatever happens to the Wi-Fi module.
  - User accepts that app control of the AC depends on a local server running, since the IR remote always works (user, 2026-11-25).
- The two commissioned Matter nodes (`_matter._tcp`) on the LAN are **Amazon devices** (MAC vendor Amazon), not the AC. (corrected 2026-11-25: was "unknown devices"). No `_matterc._udp` (commissionable) devices seen.

## Architecture
```
SwiftUI menu-bar app (app/)                 Rust core (core/, staticlib via uniffi)
  MenuBarExtra panel + Settings   ─uniffi─▶  AcController ─ local AC server (TLS/MQTT, route per T1.1) ◀── LG AC Wi-Fi module
  @Observable @MainActor models   ◀─callbacks─ (state pushes)      TvClient ─ WebSocket (LAN) ──▶ LG webOS TV
  Keychain (Security.framework)              wol ─ UDP broadcast ──▶ TV power-on
                                            config (TOML) · ControlError · one tokio runtime
controlz CLI (cli/) ── links controlz_core directly (no FFI), same config file + Keychain service
```
- **Ownership:** Rust owns every device protocol, the AC server, config-file I/O and the domain types. Swift owns UI, the Keychain on the app side, and app lifecycle. The CLI is a thin Rust front-end over the same core, used to verify each protocol on real hardware before any UI uses it.
- **Repo layout (target):**
  - `Cargo.toml` workspace (`resolver = "3"`, shared `[workspace.package]`, `[workspace.dependencies]`, `[workspace.lints]`)
  - `core/src/`: `lib.rs`, `error.rs` (`ControlError`), `runtime.rs`, `ffi.rs` (uniffi exports), `ac/` (route-specific, shaped by T1.1), `webos/` (`transport`, `pairing`, `manifest`, `commands`, `apps`, `pointer`), `wol.rs`, `discovery.rs`; integration tests in `core/tests/` with in-process mock peers.
  - `cli/src/`: `main.rs` (clap), `config.rs`, `secrets.rs`, `ac.rs`, `tv.rs`.
  - `app/`: SwiftPM package (`Package.swift`, `Sources/Controlz/…`, `Tests/ControlzTests/`), `Generated/` (gitignored uniffi output).
  - `scripts/`: `check.sh` (all quality gates), `build-core.sh`, `bundle-app.sh`.
- **Async + threading:** the core owns one multi-thread tokio runtime (`OnceLock`). uniffi async exports (`#[uniffi::export(async_runtime = "tokio")]`) surface as Swift `async throws`. Long-lived things are uniffi Objects (`Arc`): `AcController`, `TvClient`. State changes are pushed to Swift through a foreign-implemented callback trait (e.g. `AcObserver::on_state`), which Swift hops onto `@MainActor` models.
- **Errors:** one `ControlError` enum (`thiserror` + `uniffi::Error`) with user-presentable messages. Secrets (TV client-key, any AC credentials) never appear in errors, `Debug` output or logs (`Secret` newtype with redacted `Debug`).
- **Config + secrets (D10):** non-secret config in `~/Library/Application Support/Controlz/config.toml` (read/written by the core for both CLI and app). Secrets in the Keychain under one service name (set in T0.3), accessed from Rust via `security-framework` (CLI) and from Swift via Security.framework (app).
- **AC server (if the local-cloud route wins, D12):** runs inside the app process (and `controlz ac serve` for the CLI), only while it is running. Listens on the ports the AC module expects (e.g. 443). Unprivileged processes **can bind port 443 on macOS 26** (tested 2026-11-25, uid 501). Details from T1.1.
- **Logging:** `tracing` in Rust (CLI: `tracing-subscriber`, `-v` flags), `os.Logger` in Swift; never log secrets.
- **Testing strategy:** unit tests next to the code; protocol tests against in-process mock peers (a fake webOS WebSocket server, a fake AC module client); `insta` snapshots for wire messages; fixtures use fake values only (privacy hook). Swift unit tests (Swift Testing) target view models against a fake core protocol; UI is verified by designer screenshot review.

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

## Home network
- DHCP hands out the **router as the DNS server** (`ipconfig getpacket en0`, 2026-11-25), so the AC will resolve names through the router. The Mac itself is manually set to 8.8.8.8.
- The router is an ISP-supplied box with a web admin page (user has access). Its identifiers are deliberately **not recorded anywhere** (user request, 2026-11-25).
- **Router capabilities** (source: user screenshots of the admin UI, 2026-11-25):
  - DHCP server on; a **"specific DNS server" option** that changes the DNS server handed out by DHCP (currently off). It is network-wide, not per device.
  - **DHCP reservations** (hostname/MAC → IP): use them to pin the TV, the AC and whichever machine hosts the local AC server.
  - Port forwarding, DMZ, UPnP, DDNS pages exist. **No local DNS host-override / static-DNS page** was seen.
- Consequence for the AC: overriding one hostname (needed by ThinQ1 modules, which always call an LG domain) requires running **our own DNS resolver** on the LAN and pointing the router's "specific DNS server" at it — then the whole home's DNS depends on that box being up. A ThinQ2 module can be given our own hostname instead (see below), which may avoid any router change.
- The earlier "router doesn't answer DNS/HTTP" test was invalid: macOS Local Network permission was blocking the terminal (EHOSTUNREACH on every LAN address, internet fine). (corrected 2026-11-25)
- Public wildcard DNS (e.g. `<ip-with-dashes>.sslip.io`) resolves to LAN addresses via 8.8.8.8 (tested 2026-11-25). **The router's DNS forwards these answers too (no DNS-rebind filtering; `dig @router`, 2026-11-25)** → a ThinQ2 module can be pointed at a server on the Mac **without any router change**.

## AC — local control options (to be decided in T1.1)
- **Matter (local IP, two-way):** only some newer LG ACs ship Matter support — check the model. A Matter device can join several fabrics (multi-admin), so it could stay in ThinQ/Apple Home and also be commissioned by Controlz. On macOS, candidates are Apple's `Matter.framework` (`MTRDeviceController`, own fabric) or HomeKit (available to Mac Catalyst apps only); a Rust controller (`rs-matter`) is less mature. **All unverified — T1.1 confirms.**
- **IR blaster (local, one-way):** a Wi-Fi IR blaster (e.g. Broadlink RM4 mini, local UDP protocol, see `python-broadlink`) replays the AC remote's codes. Works with any model; no state readback (Controlz keeps its own assumed state). Needs extra hardware.
- **Not available:** a documented local Wi-Fi API on LG ACs; Bluetooth control.
- **For S4NQ09WA51A (2020, LCW-003 module): Matter support is unlikely** — Matter 1.0 shipped in late 2022, after this unit. Unverified; T1.1 confirms.
- **Local ThinQ cloud replacement — `rethink` (most promising, partly verified):** https://github.com/anszom/rethink (GPL-2.0, ~400★, active Nov 2026). Source: its README, wiki and `rethink-setup.ts`, read 2026-11-25.
  - How: while the AC is in SoftAP mode, a computer joins the `LG…` network and talks TLS to port 5500 on the module's fixed SoftAP gateway address (LG default, listed in `rethink-setup.ts` usage text). ThinQ1 modules: XML `mTosp` messages (`deviceinfo`, then `apinfo` with base64 SSID/password and a fake `regionalCode`). ThinQ2 modules: JSON `setDeviceInit` … plus an RSA public key. The AC then joins the home Wi-Fi and connects to a **local server pretending to be LG's cloud** (MQTT over TLS) instead of LG.
  - Two-way: state readback and control, no extra hardware, no cloud. Wall-mounted LG ACs (ThinQ model `RAC_056905_WW`, "DualCool family IDUs") have its highest support level; whether S4NQ09WA51A reports that model, and whether LCW-003 is ThinQ1 or ThinQ2, is **unverified** (the SoftAP `deviceinfo` reply would tell).
  - Requirements: an always-on server on the LAN (rethink is Node.js; listens on 443), **DNS on the LAN redirecting the AC's cloud hostname to that server** (the Wi-Fi module has no mDNS), its own MQTT broker. After provisioning, the AC no longer talks to the official ThinQ app (re-provisioning with the ThinQ app undoes it).
  - For Controlz: either run rethink as a separate service and talk MQTT to it, or reimplement the needed server side in the Rust core (clean-room from the wiki; copying rethink code would pull in GPL-2.0). To decide in T1.1.
- **Possible fourth route (unverified):** community ESP32/ESPHome projects that replace or tap an LG AC's Wi-Fi module / indoor-unit serial bus for local two-way control. Hardware modification of a 220 V appliance (warranty and safety risk). T1.1 checks whether any project supports this model.

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
- Keychain layout (D10, T0.3): generic passwords with service **`dev.controlz`** and account = secret name (today only `tv.client-key`). Tests use service `dev.controlz.test`. Written via `security_framework::passwords::{set,get,delete}_generic_password`; `errSecItemNotFound` (-25300) means "not set". Code: `cli/src/secrets.rs`. (source: T0.3 implementation, 2026-11-25)
- Config file (D10, T0.3): `$HOME/Library/Application Support/Controlz/config.toml`, path from `std::env::home_dir()`; read/written by `controlz_core::Config` (`core/src/config.rs`) so the app reuses it. Keys `tv.host`, `tv.mac` (MAC normalised to lowercase colon form); `config set` rejects unknown keys; unknown keys/tables in the file are kept on load → save and logged as a warning (names only), so older builds keep newer settings; saves go through a pid-unique temp file + rename (temp removed on failure; concurrent writers: last one wins, no lock). `config secret set` reads only from piped stdin and refuses a terminal (it would echo). (source: T0.3 implementation, 2026-11-25)

## Decisions
- **D1** · 2026-11-25 · Rust core + thin SwiftUI shell, bridged with uniffi · native look and a small menu-bar footprint for the UI, Rust for the protocol logic; pure-Rust AppKit (objc2) is too tedious, Tauri carries a WebView · status: active
- **D2** · 2026-11-25 · TV controlled locally over webOS SSAP; AC through the official ThinQ Connect cloud API with a PAT · LG ACs have no local API; ThinQ Connect is LG's official, documented route · status: TV part active; AC part superseded by D4
- **D3** · 2026-11-25 · Development workflow: coordinator main-thread agent + implementer/reviewer/designer subagents, `docs/plan.md` + `docs/knowledge_base.md` as the sources of truth, one task per cycle · status: active
- **D4** · 2026-11-25 · Every device is controlled locally; no cloud services (LG ThinQ rejected). AC route is chosen in T1.1 among: local stand-in for LG's cloud (rethink-style, leading), IR blaster, Matter, ESP32 module swap (corrected 2026-11-25: was "Matter vs IR blaster") · user wants local-only control; accepts that AC support depends on the route (a local server on the Mac, the model, or extra hardware) · status: active
- **D5** · 2026-11-25 · Build the TV end to end first (core → CLI → Swift bridge → app); AC work starts after · TV has a known local protocol and is already reachable; AC route still open · status: superseded by D11
- **D6** · 2026-11-25 · Minimum macOS 26 · this Mac runs 26.5; use current SwiftUI APIs without fallbacks · status: active
- **D7** · 2026-11-25 · Keep the existing partial `core/` code as a starting point; T0.2 reviews it and makes it compile · Wake-on-LAN + tests are reusable · status: active
- **D8** · 2026-11-25 · One git commit per finished task; designer reviews screenshots after UI tasks · small reviewable history; catch drift from specs early · status: commit/close part superseded by D14; screenshot review still active
- **D9** · 2026-11-25 · Cargo workspace with `core` (library, `controlz_core`) and `cli` (binary `controlz`) · keeps CLI-only deps (clap) out of the library shipped in the app · status: active
- **D10** · 2026-11-25 · Secrets (TV client-key) in the macOS Keychain under one service shared by CLI and app; non-secret device config (TV IP/MAC) in `~/Library/Application Support/Controlz/` · pair once from the CLI, the app reuses it; nothing private in the repo · status: active
- **D11** · 2026-11-25 · Build the AC end to end first (choose local route → core + CLI → Swift bridge → app); TV follows · user priority · status: active
- **D12** · 2026-11-25 · The local AC server (stand-in for LG's cloud) runs on this Mac · user choice; app control of the AC only while the Mac is awake is acceptable because the IR remote always works · status: active
- **D13** · 2026-11-25 · Standard tooling: rustfmt, clippy (workspace lints, pedantic warn), cargo-nextest, cargo-deny, cargo-machete, insta; swift-format, SwiftLint, Swift Testing, Swift 6 strict concurrency; one `scripts/check.sh` gate; GitHub Actions CI + Dependabot (see Tooling) · community-standard, maintained tools; cargo-deny's license check also guards the GPL boundary · status: active
- **D14** · 2026-11-25 · The coordinator never commits a task or changes its status past `in_review` until the user says so; after review it gives the user an overview and waits. Non-task changes are committed only when the user asks · user wants to approve each task before it is closed · status: active
- **D15** · 2026-11-25 · No agent changes anything outside the repo (installs, shell profiles, global config, `~/Library`, system settings) unless the user approved that exact change beforehand. Claude's session scratchpad is exempt · T0.1 edited `~/.zshrc` and installed rustup without a heads-up · status: active
- **D16** · 2026-11-25 · Every agent that edits files (implementer, designer) first returns a read-only brief (plan, repo files, outside-repo changes, new deps, side effects, questions); the coordinator shows it to the user and dispatches the real work only after approval · user wants to see and approve the plan before any edits · status: active
- **D17** · 2026-11-25 · The coordinator thread stays lean: no large reference skills or docs loaded into it (delegate the lookup or read only the needed lines), targeted `awk`/`grep`/`sed -n` reads of `docs/plan.md` and `docs/knowledge_base.md` instead of whole files, short command output · cost evaluation 2026-11-25: the coordinator thread is ~90% of token spend because every call re-reads its whole context; loading the `claude-api` skill for one pricing lookup added ~37k tokens · status: active
- **D18** · 2026-11-25 · The plan's Change log records only finished tasks (`done`); status moves, task notes (incl. review results), plan reshuffles and KB edits get no Change log line; review results are reported to the user, not written into plan.md. The done entry (status, ticks, Session log line without a hash) goes into the task's own commit, never a separate log-only commit. Overrides the coordinator rule "every plan/KB change gets a Change log entry" for those changes · user wants a lean log · status: active

## Conventions
- Cargo workspace at the repo root: `core/` (crate `controlz_core`) and `cli/` (binary `controlz`) (D9). Swift package in `app/`.
- Every device protocol is verified against the real device from the `controlz` CLI before UI work uses it.
- Generic standards: `.claude/skills/rust-standards`, `.claude/skills/swift-shell`, `.claude/skills/macos-design`. Project-specific overrides go here.
- **Tasks carry an `approach` line** in `docs/plan.md` with the intended technical design; implementers may deviate with a reason in their report.

## Tooling (D13)
Versions checked 2026-11-25 (crates.io / Homebrew / GitHub).
- **Rust**
  - Toolchain via rustup, pinned in `rust-toolchain.toml` with `rustfmt` + `clippy` components. Edition 2024. The pin must satisfy the MSRV of every core dependency (recorded under Environment; new libraries add theirs when introduced).
  - `rustfmt` (default style + `rustfmt.toml` only if needed).
  - `clippy` with `[workspace.lints]`: `clippy::all` = deny, `clippy::pedantic` = warn (noisy lints allowed explicitly with a comment), `rust.unsafe_code` = deny (the uniffi scaffolding is exempt if it needs it), `rust.missing_debug_implementations` = warn. CI runs clippy with `-D warnings`.
  - **cargo-nextest** 0.9.x as the test runner (+ `cargo test --doc` for doctests).
  - **cargo-deny** 0.22.x: RustSec advisories, license allow-list (MIT, Apache-2.0, BSD-2/3, ISC, Unicode-3.0, Zlib, MPL-2.0; **no GPL** — keeps any rethink-derived work clean-room), banned/duplicate crates, allowed sources.
  - **cargo-machete** 0.10.x: unused dependencies.
  - **insta** 1.x: snapshot tests for protocol messages (`cargo insta review`).
  - **cargo-llvm-cov** 0.9.x: coverage report on demand (not a gate).
- **Swift**
  - Swift 6 language mode (complete strict concurrency), warnings as errors in CI.
  - **swift-format** (Apple; bundled with Xcode at `xcrun swift-format`, no install): formatting + `lint --strict`, config in `.swift-format`.
  - **SwiftLint** 0.65.x (Homebrew): extra lint rules; formatting-related rules disabled so it doesn't fight swift-format. Config `.swiftlint.yml`.
  - **Swift Testing** (`import Testing`) for unit tests, run with `swift test`.
- **Shared**
  - `scripts/check.sh` is the single local + CI gate: fmt check, clippy, nextest, doctests, cargo-deny, cargo-machete; plus swift-format lint, SwiftLint, `swift build`, `swift test` once `app/` exists.
  - `.editorconfig` for whitespace basics.
  - Git hooks: `.githooks/pre-commit` keeps the privacy check and adds a fast `cargo fmt --check` (no clippy/tests, to stay fast).
  - **CI:** GitHub Actions on a macOS runner (repo is public → free), actions pinned by commit SHA, `permissions: contents: read`, Rust cache; runs `scripts/check.sh`. **Dependabot** weekly for Cargo and GitHub Actions.
- Considered, not adopted: SwiftFormat (nicklockwood) — swift-format is bundled and sufficient; cargo-udeps — needs nightly, cargo-machete covers it; cargo-audit — cargo-deny covers advisories.

## Gotchas
- An app that **listens** for connections (the AC server) triggers the macOS application-firewall "accept incoming connections" prompt when the firewall is on. Ad-hoc signed builds change identity every build → the prompt can repeat during development. Stable signing (T7.3) fixes it.
- The GitHub repo is public. A pre-commit hook (`.githooks/pre-commit`, enabled with `git config core.hooksPath .githooks`) blocks LAN IPs, MAC addresses, personal emails and `thinqpat_` tokens; mark obvious fakes with `privacy:allow`.
- ~~`core/` contains a **partial, uncompiled** start from before the workflow existed: `Cargo.toml`, `uniffi-bindgen.rs`, `src/lib.rs` (declares `thinq`/`webos` modules that don't exist yet), `src/wol.rs` (Wake-on-LAN + tests). Kept (D7); `reqwest` and the `thinq` module are no longer needed (D4).~~ (corrected 2026-11-25: T0.2 made `core/` compile inside the root workspace; `thinq` removed, `webos` is a stub, `ControlError` lives in `src/error.rs`.)
- The TV client-key is a secret: never in code, fixtures, logs or docs.
- Keychain items written by the unsigned/ad-hoc-signed `controlz` CLI land in the login keychain with an access list tied to that binary. A rebuilt binary (new identity) or the app reading the same item may trigger a "wants to use your confidential information" prompt until signing is stable (T7.3). The data-protection keychain would need entitlements, which a plain CLI does not have. (source: Apple Keychain Services docs on legacy keychain ACLs; not yet observed here, the Keychain test is `#[ignore]`d, 2026-11-25)
- The TV gets its IP by DHCP; the router supports DHCP reservations — the user should reserve the TV's (and later the AC's and the AC-server host's) IP so configured addresses stay valid (until SSDP discovery, T7.1).
- macOS Local Network permission applies to every app that reaches the LAN — including the terminal running the CLI and the user's browser. Denied → instant "No route to host" while the internet still works.

## Roadmap sketch
See `docs/plan.md`. AC first (D11): P0 Setup · P1 AC route + core + CLI · P2 Swift bridge · P3 Design specs · P4 Mac app (AC) · P5 TV core + CLI · P6 Mac app (TV) · P7 Later.

## Open questions
- Is the LCW-003 module ThinQ1 or ThinQ2, and which ThinQ model does it report? Needs the Mac to join the AC's `LG…` network briefly (user OK required) — needed by T1.1
- Is send-only (IR) acceptable as a fallback, and is buying an IR blaster OK? — needed by T1.1 (only if the rethink route fails)
- Does the ThinQ or Alexa app show this AC via Matter? (low priority; Matter unlikely) — needed by T1.1

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
- rethink (local ThinQ cloud replacement) — https://github.com/anszom/rethink · wiki https://github.com/anszom/rethink/wiki
