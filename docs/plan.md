# Controlz — Plan

> Source of truth for *what to do*. Only the coordinator edits this file.
> Task format: see `.claude/skills/bootstrap/templates/task.md`. Facts and decisions live in `knowledge_base.md`.

## Status legend
`todo` → `in_progress` → `in_review` → `done` · `blocked` (reason in notes) · `reopened` (link to B#/correction)

## Open questions
- [ ] OK to install `rustup`? Homebrew cargo 1.85 is likely too old — needed by T0.1
- [ ] Can the router override DNS for one hostname (or can we run our own DNS)? — needed by T1.1
- [ ] OK that the AC is only controllable while a local server runs (the Mac or an always-on box)? — needed by T1.1
- [ ] Did you put the AC into Wi-Fi setup mode, or did it fall back to it on its own? — needed by T1.1
- [ ] Model printed on the AC's original IR remote (often AKB…) — needed by T1.1
- [ ] Is AC state readback a must, or is send-only (IR) OK? Is buying an IR blaster (~US$25–35) OK? — needed by T1.1
- [ ] Does the AC show in the ThinQ/Alexa app via Matter or only via the LG skill? — needed by T1.1

---

## P0 — Setup
Goal: a working toolchain, a compiling workspace and a CLI to test devices with.

### T0.1 — Rust toolchain via rustup
- status: todo
- deps: -
- owner: implementer
- files: rust-toolchain.toml, core/Cargo.toml
- kb: Environment; Gotchas
- design: -
- acceptance:
  - [ ] rustup stable installed and pinned in `rust-toolchain.toml`
  - [ ] Dependency versions in `core/Cargo.toml` bumped to current releases that resolve and build on it; `reqwest` removed (no ThinQ, D4)
  - [ ] MSRVs of uniffi and tokio-tungstenite recorded in the knowledge base (Environment)
- notes: needs the user's OK to install rustup

### T0.2 — Workspace + make core/ compile
- status: todo
- deps: T0.1
- owner: implementer
- files: Cargo.toml, core/Cargo.toml, core/src/lib.rs, core/src/wol.rs, core/src/webos.rs, scripts/check.sh, .gitignore
- kb: Conventions; Decisions (D7, D9); Gotchas
- design: -
- acceptance:
  - [ ] Root Cargo workspace with members `core` and `cli` (cli may be a stub `main`); `.gitignore` covers the root `target/`
  - [ ] `lib.rs` declares only modules that exist (`webos` stub, `wol`); the `thinq` module is removed
  - [ ] `scripts/check.sh` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, all green
  - [ ] Existing Wake-on-LAN tests pass
- notes:

### T0.3 — CLI skeleton, config and Keychain secrets
- status: todo
- deps: T0.2
- owner: implementer
- files: cli/Cargo.toml, cli/src/main.rs, cli/src/config.rs, cli/src/secrets.rs
- kb: Decisions (D9, D10); Conventions; Gotchas
- design: -
- acceptance:
  - [ ] `controlz --help` lists `ac`, `tv` and `config` command groups (ac/tv may be stubs)
  - [ ] `controlz config set <key> <value>` writes `~/Library/Application Support/Controlz/config.toml`; nothing is read from the repo
  - [ ] Secrets are read/written in the macOS Keychain under one service name recorded in the knowledge base
  - [ ] A test proves secret values never appear in `Debug` output or logs
- notes:

## P1 — AC route + core + CLI
Goal: pick a local route for the S4NQ09WA51A and control it from the CLI. Route-specific tasks (T1.2+) are added after T1.1.

### T1.1 — Choose a local AC route
- status: todo
- deps: -
- owner: implementer
- files: docs/knowledge_base.md
- kb: AC — local control options; Devices (AC); Decisions (D4, D11)
- design: -
- acceptance:
  - [ ] Confirms with sources whether S4NQ09WA51A / its LCW-003 module supports Matter (now or via firmware)
  - [ ] Evaluates the `rethink` local-cloud route in depth: ThinQ1 vs ThinQ2 for LCW-003, model support, DNS/server requirements, and run-rethink-as-a-service vs clean-room Rust reimplementation (GPL-2.0 implications)
  - [ ] Checks ESP32/ESPHome module-replacement projects for this model; notes hardware, safety and warranty impact
  - [ ] Covers the IR route: LG AC IR protocol support, recommended blaster and how it's driven locally (e.g. Broadlink protocol)
  - [ ] Compares routes on effort, extra hardware, two-way state and macOS feasibility; ends with a recommendation and a proposed task list — no product code
- notes: research only; needs the open questions answered (AC network location, remote model, IR acceptable?). After the user picks a route, the coordinator adds T1.2+ (AC core + CLI) via plan-adjust.

## P2 — Swift bridge
Goal: expose the Rust core to Swift with uniffi and a one-command build.

### T2.1 — uniffi exports (AC)
- status: todo
- deps: T1.1
- owner: implementer
- files: core/src/ffi.rs, core/src/lib.rs
- kb: Architecture; Decisions (D1)
- design: -
- acceptance:
  - [ ] AC functions exported as Swift `async throws`
  - [ ] One tokio runtime owned by the core
  - [ ] Errors map to a single error enum with readable messages; no secrets in error text
- notes: deps get extended to the AC core tasks once they exist (after T1.1)

### T2.2 — Core build script
- status: todo
- deps: T2.1
- owner: implementer
- files: scripts/build-core.sh
- kb: Architecture; Environment
- design: -
- acceptance:
  - [ ] Builds the arm64 release static library
  - [ ] Generates Swift bindings and module map into `app/Generated/`
  - [ ] Idempotent; usage recorded in the knowledge base
- notes:

## P3 — Design specs
Goal: designer specs for the menu-bar panel, Settings and first-run setup — AC first, TV included.

### T3.1 — Menu-bar panel spec
- status: todo
- deps: T1.1
- owner: designer
- files: docs/design/panel.md
- kb: Project; macOS platform
- design: -
- acceptance:
  - [ ] AC section first: controls the chosen route supports (power, mode, target temperature, fan; current temperature only if readable)
  - [ ] States: loading, unreachable, not set up; for a send-only route, how assumed state is shown
  - [ ] TV section: power, volume/mute, input, apps, D-pad, media keys; TV off (with Turn On), not paired
- notes: depends on T1.1 because the AC controls depend on the route

### T3.2 — Settings and first-run spec
- status: todo
- deps: T1.1
- owner: designer
- files: docs/design/settings.md
- kb: Project; macOS platform; TV / webOS protocol (Registration)
- design: -
- acceptance:
  - [ ] AC setup flow for the chosen route
  - [ ] TV setup: enter IP/MAC, pairing flow including the on-TV prompt
  - [ ] Explains the Local Network permission before macOS asks for it
  - [ ] Settings layout per device (re-pair/re-setup, forget)
- notes:

## P4 — Mac app (AC)
Goal: the menu-bar app controlling the AC, built to the specs and reviewed by screenshot.

### T4.1 — App skeleton and bundling
- status: todo
- deps: T2.2
- owner: implementer
- files: app/Package.swift, app/Sources/, app/Info.plist, scripts/bundle-app.sh
- kb: macOS platform; Decisions (D6)
- design: -
- acceptance:
  - [ ] `scripts/bundle-app.sh` builds a `.app` with `LSUIElement` and `NSLocalNetworkUsageDescription`
  - [ ] Menu-bar icon opens a `MenuBarExtra` panel (window style), macOS 26 minimum
  - [ ] Calls into the Rust core successfully
- notes:

### T4.2 — Settings and Keychain
- status: todo
- deps: T4.1, T3.2
- owner: implementer
- files: app/Sources/Settings/, app/Sources/Keychain.swift
- kb: Decisions (D10); macOS platform
- design: docs/design/settings.md
- acceptance:
  - [ ] Settings window per spec (AC section; TV section can be a placeholder)
  - [ ] Shares the Keychain service and config file with the CLI
  - [ ] No secret shown or logged after entry
- notes:

### T4.3 — First-run AC setup
- status: todo
- deps: T4.2
- owner: implementer
- files: app/Sources/Setup/
- kb: AC — local control options; macOS platform
- design: docs/design/settings.md
- acceptance:
  - [ ] Guides AC setup end to end for the chosen route
  - [ ] Shows a clear, actionable error for each failure (unreachable, no Local Network permission, …)
  - [ ] Designer screenshot review passes
- notes: user sets up the AC from the app

### T4.4 — AC controls in the panel
- status: todo
- deps: T4.3, T3.1
- owner: implementer
- files: app/Sources/AC/
- kb: AC — local control options
- design: docs/design/panel.md
- acceptance:
  - [ ] All AC controls from the spec work on the real AC
  - [ ] Shows pending state until the AC confirms (or assumed state for a send-only route)
  - [ ] Designer screenshot review passes
- notes: user tries it on the AC

### T4.5 — Error and offline states (AC)
- status: todo
- deps: T4.4
- owner: implementer
- files: app/Sources/
- kb: AC — local control options; macOS platform
- design: docs/design/panel.md
- acceptance:
  - [ ] Unreachable, not set up and permission-denied states match the spec
  - [ ] Reconnects when the AC or network comes back
  - [ ] Designer screenshot review passes
- notes:

## P5 — TV core + CLI
Goal: full TV control over the LAN, verified on the user's OLED C1 through the CLI.

### T5.1 — Turn the TV on (Wake-on-LAN)
- status: todo
- deps: T0.3
- owner: implementer
- files: core/src/wol.rs, cli/src/tv.rs
- kb: TV / webOS protocol (Power on); Devices
- design: -
- acceptance:
  - [ ] `controlz tv on` sends the magic packet to the configured MAC on UDP broadcast ports 9 and 7
  - [ ] MAC parsing accepts `:` and `-` separators and rejects malformed input (tests, fake MACs only)
  - [ ] Returns immediately without waiting for the TV
- notes: user verifies the TV wakes from standby

### T5.2 — webOS connection
- status: todo
- deps: T0.2
- owner: implementer
- files: core/src/webos/mod.rs, core/src/webos/transport.rs, core/tests/webos_transport.rs
- kb: TV / webOS protocol (Transport, Requests)
- design: -
- acceptance:
  - [ ] Tries `wss://<ip>:3001` (self-signed cert accepted for this connection only), falls back to `ws://<ip>:3000`
  - [ ] Remembers which transport worked for the next connect
  - [ ] Requests are matched to responses by `id`, with a timeout
  - [ ] Tested against an in-process mock WebSocket server
- notes:

### T5.3 — Pair with the TV
- status: todo
- deps: T5.2, T0.3
- owner: implementer
- files: core/src/webos/pairing.rs, core/src/webos/manifest.rs, cli/src/tv.rs
- kb: TV / webOS protocol (Handshake, Registration, Manifest); Gotchas
- design: -
- acceptance:
  - [ ] Requests `ssap://system/getSystemInfo` before `register`
  - [ ] Handles the `PROMPT` response then `registered`, returning the client-key
  - [ ] Retries once with the unsigned manifest if the signed one is rejected
  - [ ] A stored key reconnects without a prompt; `controlz tv pair` saves the key to the Keychain (fixture tests for both paths)
- notes: user must accept the pairing prompt on the TV. Manifest copied from aiowebostv (Apache-2.0) with attribution.

### T5.4 — Volume, mute, power off, toast
- status: todo
- deps: T5.3
- owner: implementer
- files: core/src/webos/commands.rs, cli/src/tv.rs
- kb: TV / webOS protocol (Endpoints, Power off)
- design: -
- acceptance:
  - [ ] Get/set volume, volume up/down and mute
  - [ ] Power off does not wait for a reply
  - [ ] `controlz tv vol | mute | off | toast` work against the real TV
  - [ ] Fixture tests for each request payload
- notes: user confirms on the TV

### T5.5 — Apps and inputs
- status: todo
- deps: T5.3
- owner: implementer
- files: core/src/webos/apps.rs, cli/src/tv.rs
- kb: TV / webOS protocol (Endpoints)
- design: -
- acceptance:
  - [ ] List and launch apps; show the foreground app
  - [ ] List and switch external inputs
  - [ ] Fixture tests for list parsing; `controlz tv apps | launch | inputs | input` work
- notes: user confirms on the TV

### T5.6 — Remote buttons and media keys
- status: todo
- deps: T5.3
- owner: implementer
- files: core/src/webos/pointer.rs, cli/src/tv.rs
- kb: TV / webOS protocol (Buttons, Endpoints)
- design: -
- acceptance:
  - [ ] Opens the pointer-input socket from `getPointerInputSocket`
  - [ ] Sends D-pad, ENTER, BACK, HOME and MENU button frames
  - [ ] Play, pause, stop, rewind, fast-forward via `media.controls`
  - [ ] `controlz tv key <name>` works against the real TV
- notes: user confirms on the TV

### T5.7 — uniffi exports (TV)
- status: todo
- deps: T2.2, T5.1, T5.4, T5.5, T5.6
- owner: implementer
- files: core/src/ffi.rs
- kb: Architecture; TV / webOS protocol
- design: -
- acceptance:
  - [ ] TV and wake functions exported as Swift `async throws`, reusing the runtime and error enum from T2.1
  - [ ] The client-key never appears in error text
  - [ ] `scripts/build-core.sh` regenerates bindings including the TV API
- notes:

## P6 — Mac app (TV)
Goal: add the TV to the app.

### T6.1 — First-run TV setup
- status: todo
- deps: T4.3, T5.7
- owner: implementer
- files: app/Sources/Setup/
- kb: TV / webOS protocol (Registration); macOS platform
- design: docs/design/settings.md
- acceptance:
  - [ ] Guides TV setup and pairing end to end
  - [ ] Shows a clear, actionable error for each failure (unreachable, rejected, no Local Network permission)
  - [ ] Designer screenshot review passes
- notes: user pairs the TV from the app

### T6.2 — TV controls in the panel
- status: todo
- deps: T6.1, T3.1
- owner: implementer
- files: app/Sources/TV/
- kb: TV / webOS protocol
- design: docs/design/panel.md
- acceptance:
  - [ ] All TV controls from the spec work on the real TV
  - [ ] TV-off state offers Turn On (Wake-on-LAN)
  - [ ] Designer screenshot review passes
- notes: user tries it on the TV

### T6.3 — Error and offline states (TV)
- status: todo
- deps: T6.2
- owner: implementer
- files: app/Sources/
- kb: TV / webOS protocol; macOS platform
- design: docs/design/panel.md
- acceptance:
  - [ ] Unreachable, not paired and permission-denied states match the spec
  - [ ] Reconnects when the TV or network comes back
  - [ ] Designer screenshot review passes
- notes:

## P7 — Later
Goal: nice-to-haves.

### T7.1 — Find the TV automatically (SSDP)
- status: todo
- deps: T5.2
- owner: implementer
- files: core/src/discovery.rs, cli/src/tv.rs
- kb: Devices; TV / webOS protocol
- design: -
- acceptance:
  - [ ] SSDP search finds LG webOS TVs on the LAN
  - [ ] Reads the model name from the UPnP description on port 1926
- notes:

### T7.2 — Launch at login
- status: todo
- deps: T4.1
- owner: implementer
- files: app/Sources/Settings/
- kb: macOS platform
- design: docs/design/settings.md
- acceptance:
  - [ ] Settings toggle using `SMAppService`
  - [ ] State survives app restarts
- notes:

### T7.3 — Signing and notarization
- status: todo
- deps: T6.3
- owner: implementer
- files: scripts/bundle-app.sh
- kb: macOS platform
- design: -
- acceptance:
  - [ ] Signed with Developer ID and notarized
  - [ ] Opens on another Mac without Gatekeeper warnings
- notes: needs an Apple Developer account

---

## Bugs
<!-- B# tasks use the same format, plus: severity (critical|major|minor), repro, affects (T#) -->

---

## Session log
<!-- one line per finished task/session: date · task · outcome · commit -->
- 2026-11-25 · bootstrap · plan + knowledge base written; TV found on the LAN · -

## Change log
<!-- one line per plan/knowledge-base change: date · what · why -->
- 2026-11-25 — bootstrap — initial plan
- 2026-11-25 — KB: TV model, AC/account facts, D4–D10, AC local options section; ThinQ section kept as reference only — interview answers + LAN scan
- 2026-11-25 — D2 AC part superseded by D4 (local-only, no ThinQ); plan reordered TV end to end first (D5) — user decision
- 2026-11-25 · kb: AC model S4NQ09WA51A (cooling-only, LCW-003 Wi-Fi, ~2020), Matter nodes are Amazon devices, D11 supersedes D5 · user photo + LAN scan + user priority
- 2026-11-25 · plan reordered AC first (D11): AC research → T1.1; bridge/design/app retargeted to AC; TV core → T5.1–T5.6 + new T5.7 (TV uniffi); TV app → T6.1–T6.3 (T6.1 new); later → T7.x. IDs renumbered (nothing started) · user request
- 2026-11-25 · kb: AC is in SoftAP setup mode (not on home Wi-Fi); added `rethink` local-cloud route; T1.1 acceptance + open questions updated · user observation + rethink README/wiki
