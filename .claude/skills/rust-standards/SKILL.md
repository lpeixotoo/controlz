---
name: rust-standards
description: Staff-level Rust standards for the Controlz core crate - error handling, async/tokio, uniffi FFI surface, networking, testing and dependency rules. Used by the implementer when writing Rust and by the reviewer when judging it.
user-invocable: false
paths: core/**
---

# Rust standards (core/)

Project-specific conventions in `docs/knowledge_base.md` → **Conventions** override anything here.

## Shape of the crate
- One module per device protocol (`webos`, `thinq`, `wol`, …). Protocol details stay private; only uniffi types are exported.
- The FFI surface is small, stable and boring: `#[derive(uniffi::Object)]` clients with `async` methods, `#[derive(uniffi::Record)]` plain data, `#[derive(uniffi::Enum)]` for closed sets, one `#[derive(uniffi::Error)]` error enum.
- No generics, lifetimes or trait objects across the FFI boundary. Use owned `String`, `Vec`, `Option`, primitives.
- Changing an exported signature is a **decision**: report it under `decisions_needed` unless the task explicitly asks for it.

## Errors
- No `unwrap()`/`expect()`/`panic!` outside tests and provably infallible spots (comment why). A panic across FFI kills the app.
- Library errors use `thiserror`; map lower-level errors at module boundaries into meaningful variants with context (which device, which call).
- Device/API error codes become typed variants or carry the code; never discard them.

## Async & networking
- tokio only. Exported async functions run on the uniffi tokio runtime (`#[uniffi::export(async_runtime = "tokio")]`).
- Every network call has a timeout. No unbounded waits on a socket.
- Shared mutable state: `tokio::sync::Mutex` held for the shortest scope; never hold a lock across an `.await` that does network I/O unless the lock *is* the connection.
- Reconnect logic is explicit and bounded (attempt count, backoff).
- TLS: the webOS TV uses a self-signed cert. Accepting invalid certs is allowed **only** for the TV connection and must be scoped to it.

## Data & parsing
- serde structs for payloads you rely on; `serde_json::Value` only for genuinely open-ended blobs, and never leaked across FFI.
- Unknown enum values from devices must not fail parsing: keep them as `Other(String)` or a raw string.

## Code quality
- `cargo fmt` clean; `cargo clippy --all-targets -- -D warnings` clean.
- Small functions, intention-revealing names, doc comments on every exported item. Comments explain *why*, not *what*.
- No `unsafe`.
- Logging through `log` macros at boundaries only; never log tokens or client keys.

## Tests
- Unit tests next to the code (`#[cfg(test)] mod tests`).
- Protocol and API code is tested against JSON fixtures in `core/tests/fixtures/` (real shapes, fake secrets). Builders and parsers must be covered both ways (request JSON produced, response JSON parsed).
- Tests must not need a real device or network. Real-device checks go through the CLI and are reported as manual verification.
- A test that cannot fail is not a test: assert on exact values.

## Dependencies
- Prefer well-maintained crates already in the tree. Check the crate's MSRV against the toolchain noted in the knowledge base.
- Minimal features (`default-features = false` where sensible). Justify each new dependency in the report.
