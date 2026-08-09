# Changelog

All notable changes will be documented in this file.

## 1.0.0 - 2026-08-09

### Changed
- Publishing now uses crates.io **Trusted Publishing** (OIDC) instead of a long-lived
  API token secret; the release workflow exchanges a short-lived token via
  `rust-lang/crates-io-auth-action`.
- First stable release: the typed `SeatError` API and high-level `ComputerUse`
  harness are now considered stable.

## 0.2.0 - 2026-08-09

### Added
- `COMPATIBILITY.md`: compositor/toolkit support tiers and validation guide.
- `docs/FFI.md`: cross-language (C/Python/Node/Go/JVM) integration guide with a C-ABI shim sketch.
- `RELEASE.md` + `.github/workflows/release.yml`: crates.io release checklist and tag-driven publish workflow.
- `examples/agent_loop.rs`: perceive -> act -> perceive demonstration loop.

### Changed
- Low-level API now returns a typed `thiserror`-based `SeatError` instead of `String`,
  with the high-level `Error` wrapping it and exposing `source()`.
- Complete `///` documentation across the public low-level API (`missing_docs` clean).

## 0.1.0 - 2026-08-08

- Initial standalone extraction from Chorus.
- Private same-UID Wayland proxy with interface filtering.
- App-scoped `wl_shm` frame capture.
- Pointer, keyboard, text, scroll, and drag injection.
- High-level framework-neutral `ComputerUse` harness.
- Automatic authenticated XWayland compatibility fallback.
- Owned child, socket, credential, thread, and bridge lifecycle.
