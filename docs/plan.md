# Controlz — Plan

> Source of truth for *what to do*. Only the coordinator edits this file.
> Task format: see `.claude/skills/bootstrap/templates/task.md`. Facts and decisions live in `knowledge_base.md`.

## Status legend
`todo` → `in_progress` → `in_review` → `done` · `blocked` (reason in notes) · `reopened` (link to B#/correction)

## Open questions
- [x] OK for the Mac to join the AC's `LG…` setup network briefly to read the module type (ThinQ1/2)? — needed by T1.1 → yes, read-only probe (user, 2026-11-26)
- [x] Is send-only IR acceptable as a fallback; is buying an IR blaster OK? — needed by T1.1 → IR OK as fallback, evaluated on paper only, no purchase (user, 2026-11-26)
- [x] Does the AC show in ThinQ/Alexa via Matter? (low priority) — needed by T1.1 → user doesn't know; T1.1 checks sources (2026-11-26)

---

## P0 — Setup
Goal: a working toolchain, a compiling workspace and a CLI to test devices with.

### T0.1 — Rust toolchain via rustup
- status: done
- deps: -
- owner: implementer
- files: rust-toolchain.toml, core/Cargo.toml
- kb: Environment; Gotchas
- design: -
- approach: Install rustup (`rustup-init -y`), pin an exact stable version + `rustfmt`/`clippy` in `rust-toolchain.toml`; make sure `~/.cargo/bin` precedes Homebrew's cargo on PATH (Homebrew rust can stay installed but unused). Bump to current majors (uniffi 0.33, tokio-tungstenite 0.30), move to edition 2024, drop `reqwest` (and `uuid`/`base64` unless still used). `cargo build -p controlz_core` must pass.
- acceptance:
  - [x] rustup stable installed and pinned in `rust-toolchain.toml`
  - [x] Dependency versions in `core/Cargo.toml` bumped to current releases that resolve and build on it; `reqwest` removed (no ThinQ, D4)
  - [x] MSRV of every core dependency recorded in the knowledge base (Environment); the pinned toolchain satisfies all of them
- notes: rustup install approved by the user (2026-11-25). Blocked twice by the coordinator path guard (fixed in 5c05f6f, session restarted). Implemented: rust-toolchain.toml (1.100.0, rustfmt+clippy), core/Cargo.toml (edition 2024; uniffi 0.33.0, tokio 1.54.1, tokio-tungstenite 0.30.0, native-tls 0.2.18, futures-util 0.3.34, serde 1.0.229, serde_json 1.0.154, thiserror 2.0.21; reqwest/uuid/base64 removed), core/Cargo.lock, KB Environment MSRV table. **Reviewer: PASS (2026-11-25)** — all 3 criteria verified; only E0583 thinq/webos errors remain (T0.2). Closed with the user's OK (2026-11-25).

### T0.2 — Workspace + make core/ compile
- status: done
- deps: T0.1
- owner: implementer
- files: Cargo.toml, core/Cargo.toml, core/src/lib.rs, core/src/wol.rs, core/src/webos.rs, scripts/check.sh, .gitignore
- kb: Conventions; Decisions (D7, D9); Gotchas
- design: -
- approach: Root `Cargo.toml` with `[workspace] members = ["core","cli"]`, `resolver = "3"`, `[workspace.package]` (edition, rust-version) and `[workspace.dependencies]`; build output moves to root `target/`. `lib.rs`: drop `thinq`, keep `wol`, add an empty `webos` module and `error.rs` with `ControlError`; keep `uniffi::setup_scaffolding!()`. First `scripts/check.sh` = fmt + clippy + test (T0.4 extends it).
- acceptance:
  - [x] Root Cargo workspace with members `core` and `cli` (cli may be a stub `main`); `.gitignore` covers the root `target/`
  - [x] `lib.rs` declares only modules that exist (`webos` stub, `wol`); the `thinq` module is removed
  - [x] `scripts/check.sh` runs `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test`, all green
  - [x] Existing Wake-on-LAN tests pass
- notes: T0.2 review PASS (2026-11-25); open: 5 "unused workspace dependency" warnings (futures-util, native-tls, serde, serde_json, tokio-tungstenite) — decide accept / drop pins / allow lint; follow-ups: align .claude/scripts/rust-check.sh with scripts/check.sh (--all-features) in T0.4; re-check MSRVs when T1.x re-adds the pruned crates. Earlier, from T0.1 review: (1) put uniffi's `cli` feature behind a crate feature (e.g. `bindgen = ["uniffi/cli"]` + `required-features` on the `uniffi-bindgen` bin) so the bindgen stack stays out of the shipped staticlib; (2) fix the lib.rs crate doc that still mentions ThinQ cloud (D4); (3) move core/Cargo.lock to the workspace root; (4) set `rust-version` in `[workspace.package]` (≥ 1.95 for uniffi; 1.100 matches the pin); (5) decide how to handle cargo 1.100's `unused_dependencies` lint for deps not used yet.

### T0.3 — CLI skeleton, config and Keychain secrets
- status: done
- deps: T0.2
- owner: implementer
- files: cli/Cargo.toml, cli/src/main.rs, cli/src/config.rs, cli/src/secrets.rs
- kb: Decisions (D9, D10); Conventions; Gotchas
- design: -
- approach: `clap` derive with subcommand groups `ac`, `tv`, `config`. Config: `serde` + `toml`, struct with optional sections per device, stored at `~/Library/Application Support/Controlz/config.toml` (core owns read/write so the app reuses it). Secrets: `security-framework` generic passwords, one service name (proposal `dev.controlz`), wrapped in a `Secret` newtype whose `Debug`/`Display` print `***`. `tracing-subscriber` behind `-v`.
- acceptance:
  - [x] `controlz --help` lists `ac`, `tv` and `config` command groups (ac/tv may be stubs)
  - [x] `controlz config set <key> <value>` writes `~/Library/Application Support/Controlz/config.toml`; nothing is read from the repo
  - [x] Secrets are read/written in the macOS Keychain under one service name recorded in the knowledge base (live Keychain round trip not run, user choice)
  - [x] A test proves secret values never appear in `Debug` output or logs
- notes: from T0.2: remove `[workspace.dependencies]` entries no member uses (they print "unused workspace dependency" warnings; at T0.2 close: futures-util, native-tls, serde, serde_json, tokio-tungstenite); keep any this task starts using; later tasks re-add theirs.

### T0.4 — Rust quality tooling
- status: done
- deps: T0.2
- owner: implementer
- files: Cargo.toml, deny.toml, .config/nextest.toml, .editorconfig, scripts/check.sh, .githooks/pre-commit
- kb: Tooling; Decisions (D13); Gotchas
- design: -
- approach: Add `[workspace.lints]` (see KB Tooling), `.config/nextest.toml`, `deny.toml` (license allow-list, advisories, sources), `insta` dev-dependency, `.editorconfig`, and a fast `cargo fmt --check` step in `.githooks/pre-commit` after the privacy check. Extend `scripts/check.sh` to run fmt, clippy `-D warnings`, nextest, doctests, cargo-deny, cargo-machete, failing on the first error with a clear message; it installs nothing silently — it prints the `cargo install` line if a tool is missing.
- acceptance:
  - [x] `[workspace.lints]` configured per KB Tooling; the workspace is clean under clippy `-D warnings`
  - [x] `scripts/check.sh` runs fmt check, clippy, nextest, doctests, cargo-deny and cargo-machete, all green
  - [x] `deny.toml` rejects GPL licenses and RustSec advisories (verified by a temporary failing case, then reverted)
  - [x] `insta` available as a dev-dependency; the pre-commit hook adds a fast fmt check without weakening the privacy check
- notes: user installs the cargo tools once (check.sh prints the exact command)

### T0.5 — CI
- status: done
- deps: T0.4
- owner: implementer
- files: .github/workflows/ci.yml
- kb: Tooling; Decisions (D13, D19)
- design: -
- approach: `.github/workflows/ci.yml`: on push/PR to `main`, macOS runner, `permissions: contents: read`, actions pinned by full SHA, `Swatinem/rust-cache`, tools via `taiki-e/install-action` (nextest, cargo-deny, cargo-machete), then `scripts/check.sh`. No secrets needed.
- acceptance:
  - [x] CI runs `scripts/check.sh` on push and PR to `main` on a macOS runner
  - [x] Every action pinned by full commit SHA; `permissions: contents: read`
- notes: green run verified after the user pushes; Dependabot dropped (D19)

## P1 — AC route + core + CLI
Goal: pick a local route for the S4NQ09WA51A and control it from the CLI. Route (T1.1): clean-room Rust local ThinQ2 cloud (D20).

### T1.1 — Choose a local AC route
- status: done
- deps: -
- owner: implementer
- files: docs/knowledge_base.md
- kb: AC — local control options; Devices (AC); Decisions (D4, D11)
- design: -
- approach: Desk research plus, with your OK, a read-only probe of the AC's setup network (connect TLS to port 5500 and read the module's device-info reply; no Wi-Fi credentials sent). Sources: rethink wiki/code (read for facts only — GPL-2.0, no copying), LG/CSA Matter listings, ESPHome LG projects, IRremoteESP8266's LG protocol. Output: a KB section with the chosen route's protocol notes, a DNS plan, certificate/TLS needs, and a proposed T1.2+ task list for plan-adjust.
- acceptance:
  - [x] Confirms with sources whether S4NQ09WA51A / its LCW-003 module supports Matter (now or via firmware)
  - [x] Evaluates the `rethink` local-cloud route in depth: ThinQ1 vs ThinQ2 for LCW-003, model support, DNS/server requirements, and run-rethink-as-a-service vs clean-room Rust reimplementation (GPL-2.0 implications)
  - [x] Checks ESP32/ESPHome module-replacement projects for this model; notes hardware, safety and warranty impact
  - [x] Covers the IR route: LG AC IR protocol support, recommended blaster and how it's driven locally (e.g. Broadlink protocol)
  - [x] For the rethink route, picks a DNS strategy that fits the router (KB: Home network): own hostname via public wildcard DNS (ThinQ2 only, no router change) vs own resolver + router "specific DNS server" (whole-home DNS dependency); with the server on the Mac (D12)
  - [x] Compares routes on effort, extra hardware, two-way state and macOS feasibility; ends with a recommendation and a proposed task list — no product code
  - [x] Lists the Rust libraries the chosen route needs (e.g. TLS, MQTT) with their MSRVs, added to the knowledge base
- notes: Probe (read-only, user-approved 2026-11-26): module is ThinQ2, TLS 1.2 CBC only. Findings in KB "T1.1 findings". Route chosen: clean-room Rust local ThinQ2 cloud; OpenSSL 3.6 vendored (D20); ongoing hostname decided later (KB Open questions). Closed with the user's OK (2026-11-26). Originally: research only; user should first create DHCP reservations for the TV, the AC and the server host. Needs the open questions answered (AC network location, remote model, IR acceptable?). After the user picks a route, the coordinator adds T1.2+ (AC core + CLI) via plan-adjust.

### T1.2 — Legacy TLS + PKI module
- status: done
- deps: T1.1
- owner: implementer
- files: Cargo.toml, Cargo.lock, core/Cargo.toml, core/src/lib.rs, core/src/ac/mod.rs, core/src/ac/tls.rs, core/src/ac/pki.rs
- kb: AC — local control options (T1.1 findings: TLS / certificates, Crates); Decisions (D13, D20); Environment; Gotchas
- design: -
- approach: `openssl` crate with `vendored` on openssl-src 300.x (OpenSSL 3.6, D20) plus `tokio-openssl`. TLS 1.2-only acceptor and connector profiles (`ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA:AES256-SHA:AES128-SHA@SECLEVEL=0`, legacy renegotiation flags); an RSA-2048 CA and a server cert with the hostname as SAN; CSR signing. Where the private keys live (Keychain vs 0600 file under Application Support) is decided in the brief: they are secrets.
- acceptance:
  - [x] A client restricted to `ECDHE-RSA-AES256-SHA` handshakes with the acceptor; a TLS 1.3-only client is refused (tests)
  - [x] A test CSR is signed and the cert verifies against the CA; server cert carries the configured SAN
  - [x] Private keys are never logged or written into the repo; storage location documented in the KB
  - [ ] `scripts/check.sh` (incl. `cargo deny check`) and CI stay green with the vendored OpenSSL build
- notes: clean-room rule (KB T1.1 findings): work from the rethink wiki, our captures and MIT/0BSD references only — never the rethink or rethink-rs source. OpenSSL build needs perl/make/Xcode CLT; check CI time. Keys: 0600 PEM files under Application Support/Controlz/pki/ (user choice). Local gate green, cold OpenSSL build 38 s; CI box ticked after the user's push. Closed with the user's OK (2026-11-26).

### T1.3 — SoftAP setup client + `controlz ac setup`
- status: todo
- deps: T1.2
- owner: implementer
- files: core/src/ac/setup.rs, core/src/ac/mod.rs, cli/src/main.rs, cli/src/ac.rs, core/tests/fixtures/
- kb: AC — local control options (T1.1 findings: setup protocol); Devices (AC); Decisions (D20); Gotchas
- design: -
- approach: Client to the module's SoftAP gateway on 5500 over the T1.2 connector. Streaming splitter for concatenated JSON objects; typed messages (setDeviceInit, getDeviceInfo with optional apiServer/mqttServer, setCertInfo, setApInfo, releaseDev, ping/pong). CLI: `controlz ac setup --info-only` for now; the Wi-Fi password is read from piped stdin like `config secret set` (only used later, by T1.9).
- acceptance:
  - [ ] Fixture tests (fake values) for every message, and for split and merged frames
  - [ ] A mock-module test runs the whole sequence end to end
  - [ ] **Device step limited to `setDeviceInit` + `getDeviceInfo` without server fields**; `setCertInfo`/`setApInfo`/`releaseDev` are never sent to the real AC in this task
  - [ ] On the device (user present, AC in setup mode): `modelName` recorded in the KB, uuid and other identifiers only in docs/local.md
- notes: the module stores a `registered` flag after provisioning that only a Wi-Fi reset clears — so no provisioning before T1.4/T1.5 exist. Whether `getDeviceInfo` persists anything is unverified: note what the brief plans to send and get the user's OK first. Clean-room rule as in T1.2.

### T1.4 — Local cloud HTTPS server
- status: todo
- deps: T1.2
- owner: implementer
- files: core/Cargo.toml, core/src/ac/cloud.rs, core/src/ac/mod.rs, core/src/ac/tls.rs, core/src/ac/pki.rs, core/tests/fixtures/
- kb: AC — local control options (T1.1 findings: local cloud side, DNS plan); Decisions (D12, D20); Gotchas (firewall prompt, AC server security)
- design: -
- approach: hyper 1.x over tokio-openssl on 443 (unprivileged bind works on macOS 26). `/route` returns apiServer/mqttServer built from a configured host (IP or hostname, so T1.9 can try each strategy), `/route/certificate[?name=…]`, `POST /device/<uuid>/certificate` signs the CSR with the T1.2 CA. The server is started for one provisioning session with the expected uuid (from T1.3's `getDeviceInfo`); certificate signing is tied to that uuid.
- acceptance:
  - [ ] Snapshot tests of each response
  - [ ] A mock-device test fetches route → certs → submits a CSR → gets a cert chaining to our CA
  - [ ] The advertised host is configurable (plain IP or DNS name); nothing device-specific is hard-coded
  - [ ] Hardening: the HTTPS server runs only for a provisioning session (started/stopped explicitly, not always on) and binds to the configured LAN interface address, not 0.0.0.0
  - [ ] Our servers use an ECDHE-only suite list (`ECDHE-RSA-AES256-SHA:ECDHE-RSA-AES128-SHA`), separate from the module-client list; the full list stays available as a documented fallback until T1.9 confirms on the device
  - [ ] `pki::server_is_current` treats a `verify()` error as "not current" (re-issue) instead of a hard error (T1.2 review nit)
  - [ ] CSR signing is tied to the session: `POST /device/<uuid>/certificate` signs only when `<uuid>` equals the session's expected uuid and the CSR's CN equals it too, at most once per session; any other request gets an error and is logged without the CSR body (tests: wrong uuid, CN mismatch, second CSR)
  - [ ] `sign_csr` caps validity (≤ 825 days) and accepts only RSA ≥ 2048 or EC P-256 keys (tests)
  - [ ] Client certs are signed by a separate client-issuing CA that `/route/certificate` never returns, so a client cert can't pass as our server cert even if the module ignores EKU; if the device rejects this in T1.9, fall back to one CA and record why in the KB
  - [ ] Our CA has `pathlen:0` in its basic constraints (test); name constraints are tried on the real module in T1.9, not assumed
  - [ ] `server_acceptor` drops `ALLOW_UNSAFE_LEGACY_RENEGOTIATION` (test: a client renegotiation attempt is refused); the flag stays a documented fallback until T1.9
- notes: clean-room rule as in T1.2. Hardening items from the T1.2 security discussion (2026-11-26). CSR binding, client CA, `pathlen:0` and renegotiation items from the 2026-11-26 security review (findings 1–3); no module is enrolled yet, so changing the CA layout orphans nothing.

### T1.5 — Single-device MQTT broker + provisioning
- status: todo
- deps: T1.2
- owner: implementer
- files: core/src/ac/mqtt.rs, core/src/ac/mod.rs, core/tests/fixtures/
- kb: AC — local control options (T1.1 findings: local cloud side, Crates); Decisions (D20); Gotchas (AC server security)
- design: -
- approach: In-crate MQTT 3.1.1 subset (CONNECT/SUBSCRIBE/PUBLISH QoS 0–1/PING) over TLS on 8883 with client-cert check, one client (clientId = uuid). Handles preDeploy/deploy → completeProvisioning with the default topic names, and the ack; `req_timesync` reply decided from captures.
- acceptance:
  - [ ] A mock-device test completes provisioning and a redeploy
  - [ ] Unknown packets are logged without payload secrets
  - [ ] Reconnect after a dropped connection works (test)
  - [ ] Hardening: a client without a cert from our client CA is rejected in the TLS handshake, before any MQTT byte is parsed (test); CONNECT is rejected unless cert CN == clientId == the session's expected uuid (test with a valid cert for another uuid); reads and writes after the handshake have timeouts; the listener binds to the configured LAN interface address
- notes: default topic names only (a non-default `topic_prov` is stored in the module's flash). Clean-room rule as in T1.2.

### T1.6 — RAC TLV codec
- status: todo
- deps: -
- owner: implementer
- files: core/src/ac/tlv.rs, core/src/ac/mod.rs, core/tests/fixtures/
- kb: AC — local control options (T1.1 findings: model); Devices (AC)
- design: -
- approach: Parse `device_packet` and build `packet` hex TLV for power, mode (cooling-only set; Auto = 3), target temperature (half degrees), fan, light; state query 0x1f5. Typed `AcState` / `AcCommand`.
- acceptance:
  - [ ] Round-trip unit tests on fixture packets (fake ids)
  - [ ] Modes the unit doesn't have (heat) are rejected with a clear error
  - [ ] Unknown TLV fields are kept/logged, not fatal
- notes: fields and values from the rethink wiki and our own captures only (clean-room rule as in T1.2); re-check against live packets in T1.9.

### T1.7 — Re-check the router for local DNS
- status: todo
- deps: T1.1
- owner: implementer
- files: docs/knowledge_base.md, docs/local.md
- kb: Home network; AC — local control options (T1.1 findings: DNS plan); Open questions (hostname)
- design: -
- approach: Walk every page of the router's admin UI with the user (fresh screenshots from the user, or the user's own browser with their approval) looking for a DNS host override, static DNS / local domain, or a hostname-from-DHCP-reservation feature that the AC could resolve. Read-only: change no router setting. Record the result in KB Home network (router identifiers stay out of the repo, as before).
- acceptance:
  - [ ] Every admin section checked and listed in the KB (DNS, DHCP, LAN, advanced), with yes/no for a local-name option
  - [ ] If a local-name option exists: how it would serve our hostname, and whether DHCP-reservation hostnames resolve (`dig @router <name>`)
  - [ ] Hostname open question updated with the result
- notes: user asked to double-check the 2026-11-25 screenshots (2026-11-26). Needs the user present.

### T1.8 — Provisioning DNS fallback
- status: todo
- deps: T1.3, T1.7
- owner: implementer
- files: core/Cargo.toml, core/src/ac/dns.rs, cli/src/ac.rs
- kb: AC — local control options (T1.1 findings: DNS plan); Home network; Gotchas
- design: -
- approach: Only needed if passing apiServer/mqttServer in `getDeviceInfo` does not stop the one-time `common.lgthinq.com` call. hickory-server resolver on the Mac answering `common.lgthinq.com` (and our hostname, if T1.7 found no router option) and forwarding the rest, run only during `controlz ac setup --dns`.
- acceptance:
  - [ ] Tests: the resolver answers the overridden names and forwards others
  - [ ] Whether UDP 53 binds unprivileged on macOS 26 is checked and recorded in the KB
  - [ ] User-guided router steps (set and then revert "specific DNS server") documented in the KB
- notes: may be dropped (with the user's OK) if T1.9 shows it is not needed.

### T1.9 — Provision the real AC + `controlz ac status|set`
- status: todo
- deps: T1.3, T1.4, T1.5, T1.6, T1.7
- owner: implementer
- files: core/src/ac/mod.rs, core/src/ac/controller.rs, cli/src/ac.rs, docs/knowledge_base.md
- kb: AC — local control options (T1.1 findings); Home network; Devices (AC); Decisions (D4, D12, D20); Open questions (hostname)
- design: -
- approach: `controlz ac serve` (HTTPS + MQTT) plus the full setup on the device with the user present, then `controlz ac status|set` over the running server. Hostname order, stopping at the first that works: (1) plain IP in `/route`; (2) a router local name, if T1.7 found one; (3) `<mac-ip>.sslip.io`; (4) user-owned domain; (5) own resolver (T1.8). The user picks before (3)–(5), since they touch D4 or the whole-home DNS. The uuid from setup's `getDeviceInfo` is passed to the HTTPS and MQTT servers as the session's expected uuid.
- acceptance:
  - [ ] The AC completes provisioning to the Mac
  - [ ] `controlz ac status` shows live power, mode and temperature; a `set` changes the unit and the change is read back
  - [ ] The hostname strategy that worked is recorded as a decision; the procedure and quirks (ping/pong, power cycle after releaseDev) in the KB
  - [ ] The ECDHE-only server suites (T1.4) work with the real AC; if not, the fallback list is used and the reason recorded in the KB
  - [ ] The T1.4 security-review hardening (separate client CA, no unsafe renegotiation, name constraints if tried) works with the real AC; anything that fell back is recorded in the KB with the reason
- notes: needs a DHCP reservation for the Mac (and the AC) first. After provisioning the official ThinQ app stops working with the AC until its Wi-Fi is reset. Add T1.8 to deps if option 1 fails on the device.

## P2 — Swift bridge
Goal: expose the Rust core to Swift with uniffi and a one-command build.

### T2.1 — uniffi exports (AC)
- status: todo
- deps: T1.9, T0.4
- owner: implementer
- files: core/src/ffi.rs, core/src/lib.rs
- kb: Architecture; Decisions (D1)
- design: -
- approach: Expose `AcController` as a `#[derive(uniffi::Object)]` with `#[uniffi::export(async_runtime = "tokio")]` async methods; state pushes through a foreign trait `AcObserver` (`#[uniffi::export(with_foreign)]`). Records/enums for `AcState`, `AcMode`, `FanSpeed`. Runtime in `runtime.rs` (`OnceLock<tokio::runtime::Runtime>`). `ControlError` derives `uniffi::Error`.
- acceptance:
  - [ ] AC functions exported as Swift `async throws`
  - [ ] One tokio runtime owned by the core
  - [ ] Errors map to a single error enum with readable messages; no secrets in error text
- notes: depends on T1.9 (AC core + CLI verified on the device)

### T2.2 — Core build script
- status: todo
- deps: T2.1
- owner: implementer
- files: scripts/build-core.sh
- kb: Architecture; Environment
- design: -
- approach: `cargo build --release -p controlz_core --target aarch64-apple-darwin` → `libcontrolz_core.a`; `cargo run -p controlz_core --bin uniffi-bindgen -- generate --library <lib> --language swift --out-dir app/Generated` → `controlz_core.swift` + `controlz_coreFFI.h` + modulemap. Package the header, modulemap and lib into `app/Generated/ControlzCoreFFI.xcframework` (`xcodebuild -create-xcframework`) so SwiftPM consumes it as a `binaryTarget`; the generated `.swift` goes into its own Swift target.
- acceptance:
  - [ ] Builds the arm64 release static library
  - [ ] Generates Swift bindings and module map into `app/Generated/`
  - [ ] Idempotent; usage recorded in the knowledge base
- notes:

## P3 — Design specs
Goal: designer specs for the menu-bar panel, Settings and first-run setup — AC first, TV included.

### T3.1 — Menu-bar panel spec
- status: todo
- deps: T1.6
- owner: designer
- files: docs/design/panel.md
- kb: Project; macOS platform
- design: -
- approach: Designer follows the `macos-design` skill. `MenuBarExtra` window style, ~320 pt wide panel. AC block: power toggle, mode segmented control, target temperature stepper (range/step from the route), fan picker; current temperature as secondary text if readable. TV block below (or a device switcher). Output: layout sketch, component choices, every state, exact copy, accessibility notes.
- acceptance:
  - [ ] AC section first: controls the chosen route supports (power, mode, target temperature, fan; current temperature only if readable)
  - [ ] States: loading, unreachable, not set up; for a send-only route, how assumed state is shown
  - [ ] TV section: power, volume/mute, input, apps, D-pad, media keys; TV off (with Turn On), not paired
- notes: depends on T1.6 because the AC controls depend on what the codec supports

### T3.2 — Settings and first-run spec
- status: todo
- deps: T1.9
- owner: designer
- files: docs/design/settings.md
- kb: Project; macOS platform; TV / webOS protocol (Registration)
- design: -
- approach: Settings window with one pane per device. First-run as a short sheet flow: Local Network explainer → AC setup (route-specific: e.g. put the AC in Wi-Fi setup mode, join its network, hand over Wi-Fi details) → TV address + pairing prompt. Spec covers every step's copy, errors and retry.
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
- approach: SwiftPM executable target `Controlz` (Swift 6 mode, macOS 26). `@main struct ControlzApp: App` with `MenuBarExtra { PanelView() }.menuBarExtraStyle(.window)` and a `Settings` scene. `scripts/bundle-app.sh`: `swift build -c release`, assemble `Controlz.app/Contents/{MacOS,Resources,Info.plist}` (LSUIElement, NSLocalNetworkUsageDescription, bundle id), then ad-hoc sign (`codesign -s -`) so Local Network and Keychain prompts work. Smoke call: show the core's version string.
- acceptance:
  - [ ] `scripts/bundle-app.sh` builds a `.app` with `LSUIElement` and `NSLocalNetworkUsageDescription`
  - [ ] Menu-bar icon opens a `MenuBarExtra` panel (window style), macOS 26 minimum
  - [ ] Calls into the Rust core successfully
- notes:

### T4.6 — Swift quality tooling
- status: todo
- deps: T4.1, T0.5
- owner: implementer
- files: app/.swift-format, app/.swiftlint.yml, app/Package.swift, app/Tests/ControlzTests/, scripts/check.sh, .github/workflows/ci.yml
- kb: Tooling; Decisions (D13)
- design: -
- approach: `.swift-format` config (line length 120, 4-space indent), `.swiftlint.yml` (default rules + a few opt-ins, formatting rules disabled), `Tests/ControlzTests` with Swift Testing, `-warnings-as-errors` for the release/CI build. Extend `scripts/check.sh` (swift-format lint --strict, swiftlint, swift build, swift test — skipped with a notice if `app/` is absent) and the CI workflow (install SwiftLint via Homebrew).
- acceptance:
  - [ ] Swift 6 language mode; release/CI build treats warnings as errors
  - [ ] `swift-format lint --strict` and SwiftLint pass with no formatting-rule conflicts
  - [ ] At least one Swift Testing test runs via `swift test`
  - [ ] `scripts/check.sh` and CI run the Swift checks
- notes: user installs SwiftLint (`brew install swiftlint`)

### T4.2 — Settings and Keychain
- status: todo
- deps: T4.1, T4.6, T3.2
- owner: implementer
- files: app/Sources/Settings/, app/Sources/Keychain.swift
- kb: Decisions (D10); macOS platform
- design: docs/design/settings.md
- approach: SwiftUI `Settings` scene with a `TabView` per device. Keychain wrapper over `SecItemAdd`/`SecItemCopyMatching`/`SecItemUpdate`/`SecItemDelete` using the same service name as the CLI. Config read/write goes through core FFI functions (one TOML implementation).
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
- approach: `@Observable @MainActor SetupModel` drives a step enum; each step calls the core (async) and maps `ControlError` to copy from the spec. Exact steps depend on T1.1's route.
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
- approach: `@Observable @MainActor AcModel` owns the core `AcController` and implements `AcObserver` (callbacks hop to the main actor). Controls send commands and show a pending state until the AC reports the new state (or show assumed state for a send-only route). The AC server starts with the app.
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
- approach: Model a connection state machine in `AcModel` (unknown → connecting → online / unreachable / notSetUp / permissionDenied), with backoff reconnects and `NWPathMonitor` for network changes. Each state maps to a spec'd view.
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
- approach: Existing `wol.rs` builds the magic packet (6×0xFF + 16×MAC) and sends it via a broadcast-enabled `UdpSocket` to 255.255.255.255 and the subnet broadcast on ports 9 and 7. CLI reads `tv.mac` from config.
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
- approach: `tokio-tungstenite` with a `native-tls` connector that accepts the TV's self-signed cert (scoped to this connector only). One reader task routes incoming frames by `id` to a `HashMap<String, oneshot::Sender<Value>>`; requests use an atomic counter id and `tokio::time::timeout`. Mock server = a tokio-tungstenite server in the test.
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
- approach: Explicit handshake state machine (connect → getSystemInfo → register → prompt? → registered). Signed manifest copied from aiowebostv (Apache-2.0) into `manifest.rs` with attribution in a `NOTICE`; unsigned variant derived in code. Client-key stored via the Keychain service as a `Secret`.
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
- approach: Thin typed wrappers over SSAP requests (`audio/*`, `system/turnOff`, `system.notifications/createToast`). Power off sends without awaiting a reply. Payloads snapshot-tested with `insta`.
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
- approach: Typed `TvApp` / `TvInput` records parsed from `listLaunchPoints` and `getExternalInputList`; `launch {id}`, `switchInput {inputId}`, `getForegroundAppInfo`.
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
- approach: Request `getPointerInputSocket` → open a second WebSocket to `socketPath` (same TLS rules) and send `type:button\nname:<NAME>\n\n` frames; `Button` enum maps to names. Media keys via `media.controls/*` on the main socket.
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
- approach: Export `TvClient` as a uniffi Object (async methods) and a `TvObserver` foreign trait for power/volume changes; regenerate bindings with `build-core.sh`.
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
- approach: Add a TV branch to the setup flow: address/MAC entry → pairing step that waits for the on-TV prompt with a cancel button and a timeout.
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
- approach: `@Observable @MainActor TvModel` over `TvClient`; D-pad as a 3×3 button grid with keyboard shortcuts (arrow keys, Return, Esc) while the panel has focus.
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
- approach: Same state machine pattern as `AcModel` (T4.5); TV-off is a normal state that offers Wake-on-LAN, not an error.
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
- approach: UDP multicast `M-SEARCH` with `ST: urn:lge-com:service:webos-second-screen:1`, collect `LOCATION`s, fetch the UPnP description on port 1926 and parse `modelName`/`friendlyName`.
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
- approach: `SMAppService.mainApp.register()` / `unregister()` behind a Settings toggle; status read back from `SMAppService.mainApp.status`. Keeps the AC server available after login.
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
- approach: Developer ID Application cert, hardened runtime, `codesign --options runtime`, `xcrun notarytool submit --wait`, `xcrun stapler staple`. Stable signing also stops repeated firewall prompts.
- acceptance:
  - [ ] Signed with Developer ID and notarized
  - [ ] Opens on another Mac without Gatekeeper warnings
- notes: needs an Apple Developer account

### T7.4 — Sandbox the AC server
- status: todo
- deps: T7.3
- owner: implementer
- files: app/, scripts/bundle-app.sh, core/src/ac/pki.rs, core/src/config.rs
- kb: macOS platform; Gotchas (AC server security); Decisions (D10, D12)
- design: -
- approach: Run the in-app AC server under the macOS App Sandbox (entitlements `com.apple.security.network.server` + `network.client`), so a compromised server can't read the user's files. The sandbox moves `~/Library/Application Support` into the app's container, which breaks the CLI/app shared config and PKI files (D10): solve with an app-group container both use, or have the app own the server and the CLI talk to it. Decide in the brief.
- acceptance:
  - [ ] The signed app runs sandboxed (`codesign -d --entitlements -` shows app-sandbox) and the AC server still provisions and controls the AC
  - [ ] CLI and app still share config and PKI (or the chosen alternative is recorded as a decision)
  - [ ] A sandboxed process can't read a file outside its container (manual check, noted in the KB)
- notes: from the T1.2 security discussion (2026-11-26); may touch D10, so the brief asks the user first.

---

## Bugs
<!-- B# tasks use the same format, plus: severity (critical|major|minor), repro, affects (T#) -->

---

## Session log
<!-- one line per finished task/session: date · task · outcome · commit -->
- 2026-11-25 · bootstrap · plan + knowledge base written; TV found on the LAN · -
- 2026-11-25 · bootstrap · plan v4 approved by the user (incl. D9, D10) · -
- 2026-11-25 · T0.1 · done · rustup + Rust 1.100.0 pinned; core deps bumped to latest (edition 2024, reqwest/uuid/base64 removed); MSRV table in KB · 95ede0e
- 2026-11-25 · T0.2 · done · root workspace (core + cli stub), core trimmed to uniffi/tokio/thiserror, bindgen feature-gated, error.rs, webos stub, scripts/check.sh · 8713341
- 2026-11-25 · T0.3 · done · controlz CLI (clap: ac/tv stubs, config set/get/show/path, config secret set/status/delete via piped stdin), Config + Secret in core, Keychain service dev.controlz, unknown config keys kept, -v tracing
- 2026-11-26 · T0.4 · done · workspace lints (clippy all deny, pedantic warn, unsafe_code deny), deny.toml, nextest config, .editorconfig, insta + Config snapshot, check.sh full gate (fmt/clippy/nextest/doctests/deny/machete), pre-commit fmt check after privacy check, rust-check.sh delegates to check.sh
- 2026-11-26 · T0.5 · done · GitHub Actions CI on macos-latest runs scripts/check.sh on push/PR to main; actions pinned by SHA, contents: read; Dependabot dropped (D19)
- 2026-11-26 · T1.1 · done · AC module probed (ThinQ2, TLS 1.2 CBC only); routes compared; chosen: clean-room Rust local ThinQ2 cloud with vendored OpenSSL 3.6 (D20); T1.2+ proposed in KB
- 2026-11-26 · T1.2 · done · legacy TLS 1.2 CBC profiles (10 s handshake timeout) + PKI (RSA-2048 CA, server cert with SAN, CSR signing, 0600 key files) on vendored OpenSSL 3.6

## Change log
<!-- one line per plan/knowledge-base change: date · what · why -->
- 2026-11-25 — bootstrap — initial plan
- 2026-11-25 — KB: TV model, AC/account facts, D4–D10, AC local options section; ThinQ section kept as reference only — interview answers + LAN scan
- 2026-11-25 — D2 AC part superseded by D4 (local-only, no ThinQ); plan reordered TV end to end first (D5) — user decision
- 2026-11-25 · kb: AC model S4NQ09WA51A (cooling-only, LCW-003 Wi-Fi, ~2020), Matter nodes are Amazon devices, D11 supersedes D5 · user photo + LAN scan + user priority
- 2026-11-25 · plan reordered AC first (D11): AC research → T1.1; bridge/design/app retargeted to AC; TV core → T5.1–T5.6 + new T5.7 (TV uniffi); TV app → T6.1–T6.3 (T6.1 new); later → T7.x. IDs renumbered (nothing started) · user request
- 2026-11-25 · kb: AC is in SoftAP setup mode (not on home Wi-Fi); added `rethink` local-cloud route; T1.1 acceptance + open questions updated · user observation + rethink README/wiki
- 2026-11-25 · kb: remote AKB75215401; setup mode user-initiated; server-dependent AC control accepted (IR remote always works); Home network section (DNS facts); stale T5.1/T6.1 refs fixed · user answers + DNS probe
- 2026-11-25 · kb: router capabilities (DHCP DNS option, reservations, no host override); Local Network permission gotcha; T1.1 acceptance adds DNS strategy + server host; router identifiers intentionally not recorded · user screenshots
- 2026-11-25 · kb: D12 AC server on the Mac; router DNS passes LAN-resolving public names (no router change for ThinQ2); rustup approved; open questions closed · user answers + dig test
- 2026-11-25 · added T0.4 (Rust tooling), T0.5 (CI + Dependabot), T4.6 (Swift tooling); T4.2 deps + T4.6, T2.1 deps + T0.4; `approach` line on every task; KB Architecture + Tooling (D13) · user asked for standard tooling and more technical detail
- 2026-11-25 · T0.1: record MSRV of every core dependency (not just uniffi/tokio-tungstenite); T1.1: list the route's libraries + MSRVs · AC-first means the AC server's libraries may set the minimum toolchain
- 2026-11-25 · plan approved by the user (D9 and D10 confirmed) · bootstrap complete
- 2026-11-25 · T0.1 blocked: coordinator path-guard hook applies to subagents · found during the first implementer run
- 2026-11-25 · coordinator path guard now `--main-only` (skips tool calls carrying `agent_id`); subagents keep their own guards · user-approved fix for the T0.1 block
- 2026-11-25 · T0.1 blocked again: hook fix only takes effect after a session restart · second implementer run
- 2026-11-25 · KB: removed the hook-reload gotcha (user: not needed); T0.1 resumed after session restart · user request
- 2026-11-25 · D14: no task commits or status moves past in_review without the user's OK; implement-task skill updated; rust-check hook → reviewer script (b2b0492); KB Gotchas core/ line corrected; T0.2 notes get T0.1 review follow-ups · user instruction + T0.1 review
- 2026-11-25 · D15 (no outside-repo changes without prior approval) and D16 (editing agents return a brief for approval before working); task-contract, implementer, designer, implement-task and CLAUDE.md updated · user instruction after T0.1 touched ~/.zshrc
- 2026-11-25 · KB: D4 wording updated to the current AC route options; D11 moved back into numeric order · user OK
- 2026-11-25 · agents' effort high → medium; lean briefs (coordinator pastes KB excerpts; brief runs return `findings` for the implement run) · user request to cut token usage
- 2026-11-25 · KB: T0.2 Environment line on unused workspace deps (implementer); core/ Gotcha struck as corrected; uniffi `cli` MSRV rows annotated as bindgen-only · T0.2 review kb_conflicts
- 2026-11-25 · T0.2 done; T0.3 notes: drop unused [workspace.dependencies] pins · user approved close; user chose to fold the warning cleanup into the next task
- 2026-11-25 · kb: D17 (coordinator thread stays lean: no large skills/docs, targeted reads, short output) · cost evaluation; user chose this option only
- 2026-11-25 · T0.3 done · user approved close
- 2026-11-26 · T0.4 done · user approved close
- 2026-11-26 · T1.1 done · user approved close
